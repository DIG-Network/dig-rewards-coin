//! Reading a distributor off the chain, and the state a UI needs — `SPEC.md` §12.1 clause 1.
//!
//! Every read arrives through the caller's [`ChainSource`]. This module opens no socket, holds no
//! key, broadcasts nothing, and returns no signed spend (§0.1 clause 2). What it returns is a
//! reconstructed [`RewardDistributor`] plus the slots the walk observed.
//!
//! ## The walk
//!
//! 1. `RewardDistributor::from_launcher_solution` on the launcher's spend gives the constants, the
//!    initial state and the eve coin. That call also **re-derives** the constants from the launcher
//!    id and rejects a launcher whose curried constants do not match, so a spoofed launcher fails
//!    here rather than later.
//! 2. From there, each singleton generation is resolved by reading the spend that spent the current
//!    coin (`ChainSource::coin_spend`) and applying `RewardDistributor::from_parent_spend`. The last
//!    unspent generation is the live distributor.
//! 3. Slots are collected from each spend as it is walked, out of the `pending_spend`
//!    `created_*_slots` / `spent_*_slots` accessors, and a created slot is retired when a later
//!    spend consumes it.
//!
//! Collecting slots from the walk rather than from a hint index is deliberate. It is the shape
//! already in production behind `hub.dig.net`'s `/quest?tab=stake` surface, and a second
//! slot-discovery path would be a second place for the entry set to be wrong.
//!
//! ## A failed read is never an empty answer
//!
//! Every `ChainSource` error becomes [`RewardsError::ChainUnavailable`]. A distributor whose read
//! failed MUST NOT render as "no entries" or "nothing accrued": those are claims about money, and
//! the honest answer is that the question went unanswered.

use chia_protocol::{Bytes32, CoinSpend};
use chia_sdk_driver::{RewardDistributor, RewardDistributorConstants, SpendContext};
use chia_sdk_types::puzzles::{
    RewardDistributorCommitmentSlotValue, RewardDistributorEntrySlotValue,
    RewardDistributorRewardSlotValue,
};
use dig_chainsource_interface::ChainSource;

use crate::RewardsError;

/// The slots a distributor currently has outstanding, as observed by the walk.
#[derive(Debug, Clone, Default)]
pub struct DistributorSlots {
    /// One per entry in the entry set: who gets paid, and the replay guard.
    pub entries: Vec<RewardDistributorEntrySlotValue>,

    /// One per outstanding commitment: which distributor epoch, whose clawback, how much.
    pub commitments: Vec<RewardDistributorCommitmentSlotValue>,

    /// One per distributor epoch with rewards attached.
    pub rewards: Vec<RewardDistributorRewardSlotValue>,
}

/// A distributor as it stands on chain, with everything a UI needs to answer "is anyone being
/// paid?".
pub struct DistributorSnapshot {
    /// The live singleton, ready for its next action.
    pub distributor: RewardDistributor,

    /// The outstanding slots.
    pub slots: DistributorSlots,
}

impl DistributorSnapshot {
    /// How many entries the set holds.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.slots.entries.len()
    }

    /// The $DIG base units held in the reserve.
    ///
    /// A **balance**, and nothing more. It is not evidence anyone is being paid the right amount:
    /// who receives it depends on an entry set that only a live prover maintains (§0.4 clause 3).
    #[must_use]
    pub fn reserve_base_units(&self) -> u64 {
        self.distributor.info.state.total_reserves
    }

    /// Committed rewards per distributor epoch, as `(distributor_epoch_start, base_units)`, sorted
    /// by epoch start.
    ///
    /// Derived from the reward slots, which is where the puzzle keeps them; this performs no
    /// accrual arithmetic of its own.
    #[must_use]
    pub fn rewards_per_distributor_epoch(&self) -> Vec<(u64, u64)> {
        let mut totals: Vec<(u64, u64)> = self
            .slots
            .rewards
            .iter()
            .map(|slot| (slot.epoch_start, slot.rewards))
            .collect();

        totals.sort_unstable_by_key(|(epoch_start, _)| *epoch_start);
        totals
    }

    /// The payout puzzle hashes currently in the entry set.
    ///
    /// Puzzle hashes, never peer identities: the distributor knows nothing about peer identity, and
    /// every statement tying a payment to a peer lives in the mirror coin (§10.2 clause 4).
    #[must_use]
    pub fn payout_puzzle_hashes(&self) -> Vec<Bytes32> {
        self.slots
            .entries
            .iter()
            .map(|slot| slot.payout_puzzle_hash)
            .collect()
    }
}

/// How many singleton generations one read will walk before giving up.
///
/// A bound, not a policy: without one, a source that returns a cycle would spin forever. Hitting it
/// is reported as [`RewardsError::ChainUnavailable`] — an unanswered question — and never as a
/// snapshot of whatever had been walked so far, which would be a stale entry set presented as
/// current.
const MAX_GENERATIONS_PER_READ: usize = 100_000;

/// Read a distributor from its launcher id.
///
/// # Errors
///
/// - [`RewardsError::ChainUnavailable`] if any read could not be answered, or if the walk exceeded
///   [`MAX_GENERATIONS_PER_READ`] generations.
/// - [`RewardsError::Malformed`] if the launcher exists but is not a reward distributor launcher, or
///   if a spend along the chain could not be interpreted as one.
/// - [`RewardsError::Driver`] if the upstream parser rejected the launcher's curried constants.
pub fn read_distributor(
    ctx: &mut SpendContext,
    source: &impl ChainSource,
    launcher_id: Bytes32,
) -> Result<DistributorSnapshot, RewardsError> {
    let launcher_spend = require_spend(source, launcher_id, "launcher")?;
    let launcher_solution = ctx.alloc(&launcher_spend.solution)?;

    let Some((constants, _initial_state, eve_coin)) =
        RewardDistributor::from_launcher_solution(ctx, launcher_spend.coin, launcher_solution)?
    else {
        return Err(RewardsError::Malformed(
            "launcher solution is not a reward distributor launch".to_string(),
        ));
    };

    let mut slots = DistributorSlots::default();
    let mut current_coin_id = eve_coin.coin_id();
    let mut distributor: Option<RewardDistributor> = None;

    for _ in 0..MAX_GENERATIONS_PER_READ {
        let Some(spend) = read_spend(source, current_coin_id)? else {
            // The current coin is unspent, so the distributor we last reconstructed is live.
            return match distributor {
                Some(distributor) => Ok(DistributorSnapshot { distributor, slots }),
                None => Err(RewardsError::Malformed(
                    "the eve coin is unspent, so no distributor exists yet".to_string(),
                )),
            };
        };

        let Some(child) = RewardDistributor::from_parent_spend(ctx, &spend, constants)? else {
            return Err(RewardsError::Malformed(format!(
                "spend of {current_coin_id} is not a reward distributor spend"
            )));
        };

        absorb_slot_changes(&spend, ctx, constants, &mut slots)?;

        current_coin_id = child.coin.coin_id();
        distributor = Some(child);
    }

    Err(RewardsError::ChainUnavailable(format!(
        "distributor {launcher_id} did not resolve within {MAX_GENERATIONS_PER_READ} generations"
    )))
}

/// Fold one spend's slot creations and consumptions into the running slot set.
///
/// A spent slot is removed before the created ones are added, because an action that replaces a
/// slot spends and creates one with the same identity but a new `counter`.
fn absorb_slot_changes(
    spend: &CoinSpend,
    ctx: &mut SpendContext,
    constants: RewardDistributorConstants,
    slots: &mut DistributorSlots,
) -> Result<(), RewardsError> {
    let Some(spent) =
        RewardDistributor::from_spend(ctx, spend, None, constants, chia_bls::Signature::default())?
    else {
        return Ok(());
    };

    let pending = &spent.pending_spend;

    slots
        .entries
        .retain(|entry| !pending.spent_entry_slots.contains(entry));
    slots
        .commitments
        .retain(|commitment| !pending.spent_commitment_slots.contains(commitment));
    slots
        .rewards
        .retain(|reward| !pending.spent_reward_slots.contains(reward));

    slots.entries.extend(pending.created_entry_slots.iter());
    slots
        .commitments
        .extend(pending.created_commitment_slots.iter());
    slots.rewards.extend(pending.created_reward_slots.iter());

    Ok(())
}

/// Read the spend that spent `coin_id`, mapping a source failure to `ChainUnavailable`.
fn read_spend(
    source: &impl ChainSource,
    coin_id: Bytes32,
) -> Result<Option<CoinSpend>, RewardsError> {
    source.coin_spend(coin_id).map_err(|error| {
        RewardsError::ChainUnavailable(format!("could not read the spend of {coin_id}: {error}"))
    })
}

/// Read a spend that must exist, naming what was being resolved.
fn require_spend(
    source: &impl ChainSource,
    coin_id: Bytes32,
    what: &str,
) -> Result<CoinSpend, RewardsError> {
    read_spend(source, coin_id)?.ok_or_else(|| {
        RewardsError::Malformed(format!("the {what} coin {coin_id} has not been spent"))
    })
}
