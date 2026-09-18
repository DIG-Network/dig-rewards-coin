//! `InitiatePayout` — a mirror claiming its own rewards, permissionlessly (`SPEC.md` §12.5).
//!
//! No authority argument, and none is possible: `require_payout_approval` is `false` for every DIG
//! distributor (§7.1), so a mirror's claim is never gated on an operator being alive and willing.
//!
//! Two properties are structural rather than documented:
//!
//! 1. **The payee is the slot's recorded `payout_puzzle_hash`, never a spender-chosen one.** The
//!    upstream action reads it off the slot; this module never takes a destination argument, so
//!    there is no parameter for an attacker to substitute. Anyone may *pay* an entry; nobody may
//!    redirect the payment.
//! 2. **The slot is read fresh on every claim.** `counter` is the slot's replay guard, and
//!    `InitiatePayout` writes `counter + 1` — so a cached slot value produces a spend that the
//!    chain rejects. [`initiate_payout`] therefore does not accept a slot: it accepts an
//!    [`EntrySlotSource`] and consults it inside the call (§10.2 clause 3, §12.5 clause 3).
//!
//! "No entry slot for that payout puzzle hash" is [`PayoutOutcome::EntrySlotAbsent`], a **terminal
//! non-error**: the peer is simply not in the set right now, which is the normal state for most
//! peers most of the time (§12.5 clause 1).

use chia_protocol::Bytes32;
use chia_sdk_driver::{
    RewardDistributor, RewardDistributorConstants, RewardDistributorInitiatePayoutAction, Slot,
    RewardDistributorState, SpendContext,
};
use chia_sdk_types::puzzles::RewardDistributorEntrySlotValue;
use chia_sdk_types::Conditions;

use crate::RewardsError;

/// Where a claim reads the entry slot from, freshly, once per claim.
///
/// [`crate::state::read_distributor`] (#3267) returns a full snapshot, which is the wrong shape
/// for "read one slot, right before I spend against it" — a claim needs the freshest possible
/// single read, not last cycle's whole-distributor walk. Implement this trait over a
/// `ChainSource`-backed lookup for that narrower read. The trait exists so that
/// [`initiate_payout`] cannot be handed a slot value at all — a caller with a stale one in a
/// variable has nowhere to put it.
pub trait EntrySlotSource {
    /// Read the current entry slot for `payout_puzzle_hash`.
    ///
    /// `Ok(None)` means the entry is genuinely not in the set. An error means the question went
    /// unanswered, which MUST NOT be degraded into `None`.
    ///
    /// # Errors
    ///
    /// [`RewardsError::ChainUnavailable`] when the read could not be established.
    fn read_entry_slot(
        &self,
        payout_puzzle_hash: Bytes32,
    ) -> Result<Option<Slot<RewardDistributorEntrySlotValue>>, RewardsError>;
}

/// What a claim attempt produced.
#[must_use]
#[derive(Debug)]
pub enum PayoutOutcome {
    /// The claim was built.
    Paid {
        /// Conditions some coin in the same bundle must assert.
        conditions: Conditions,

        /// What the puzzle paid, in **$DIG base units**, to the slot's recorded
        /// `payout_puzzle_hash`.
        ///
        /// The puzzle's own figure. This module performs no payout division (§0.1 clause 1).
        amount_base_units: u64,

        /// The replay-guard counter the slot carried when it was read.
        ///
        /// Surfaced so a caller can tell "my claim landed" from "someone else's claim landed
        /// first"; a re-read showing a higher counter means the latter.
        counter: u64,
    },

    /// There is no entry slot for that payout puzzle hash.
    ///
    /// A terminal non-error. It means the peer is not in the set right now — most often because it
    /// has not passed an evaluation yet, or because it was removed. It is not a failure and carries
    /// no accusation.
    EntrySlotAbsent,
}

/// Claim whatever the entry keyed by `payout_puzzle_hash` has accrued.
///
/// The slot is read from `source` inside this call, so every claim reads fresh state.
///
/// # Errors
///
/// - [`RewardsError::ChainUnavailable`] if `source` could not answer.
/// - [`RewardsError::Driver`] if the upstream action could not be built.
pub fn initiate_payout(
    ctx: &mut SpendContext,
    distributor: &mut RewardDistributor,
    source: &impl EntrySlotSource,
    payout_puzzle_hash: Bytes32,
) -> Result<PayoutOutcome, RewardsError> {
    let Some(entry_slot) = source.read_entry_slot(payout_puzzle_hash)? else {
        return Ok(PayoutOutcome::EntrySlotAbsent);
    };

    let counter = entry_slot.info.value.counter;

    let (conditions, amount_base_units) = distributor
        .new_action::<RewardDistributorInitiatePayoutAction>()
        .spend(ctx, distributor, entry_slot)?;

    Ok(PayoutOutcome::Paid {
        conditions,
        amount_base_units,
        counter,
    })
}

/// The payout puzzle hash an entry slot pays — read off the slot, never supplied.
#[must_use]
pub fn slot_payout_puzzle_hash(entry_slot: &Slot<RewardDistributorEntrySlotValue>) -> Bytes32 {
    entry_slot.info.value.payout_puzzle_hash
}

/// The minimum a claim must have accrued before the puzzle will pay it.
///
/// Below it, `InitiatePayout` fails; the value simply stays accrued until the next attempt. A
/// removal settles whatever has accrued regardless of this threshold ([`crate::entries`], §6.4).
#[must_use]
pub fn payout_threshold_base_units(distributor: &RewardDistributor) -> u64 {
    distributor.info.constants.payout_threshold
}

/// What `InitiatePayout` would pay `entry`, mirroring `RewardDistributorInitiatePayoutAction::spend`'s
/// arithmetic exactly (`SPEC.md` §12.5 clause 3b) -- a restatement of puzzle arithmetic, permitted
/// only because it is bound to the real paying code by an equality test (§0.1 clause 1).
///
/// Deliberately does **not** apply [`payout_threshold_base_units`]: the threshold gates whether
/// `InitiatePayout` succeeds, not what it would pay if it did, and conflating the two would make a
/// caller under the threshold see `0` rather than "not yet, but accruing".
///
/// # `None`
///
/// Returned rather than a saturated or wrapped value on:
/// - `state`'s `cumulative_payout` behind `entry`'s `initial_cumulative_payout` (a diverged read,
///   never a valid distributor state for this entry);
/// - the payout narrowing past `u64` (the same failure `spend` reports via `TryFromIntError`).
#[must_use]
pub fn accrued_base_units(
    constants: &RewardDistributorConstants,
    state: &RewardDistributorState,
    entry: &RewardDistributorEntrySlotValue,
) -> Option<u64> {
    let elapsed_cumulative_payout = state
        .round_reward_info
        .cumulative_payout
        .checked_sub(entry.initial_cumulative_payout)?;

    let withdrawal_amount_precision = u128::from(entry.shares) * elapsed_cumulative_payout;

    let withdrawal_amount = withdrawal_amount_precision.checked_div(u128::from(constants.precision))?;

    u64::try_from(withdrawal_amount).ok()
}

/// The chain-backed [`EntrySlotSource`] (`SPEC.md` §12.5 clause 3a): re-walks the whole
/// distributor, from the eve coin to the tip, on **every** call.
///
/// A narrower read is FORBIDDEN. `dig_chainsource_interface::ChainSource` has no hint index and a
/// slot's puzzle hash depends on the slot *value* (§12.1 clause 1), so the authenticated walk
/// [`crate::state::read_distributor`] performs is the only thing that establishes a slot coin
/// exists and what proof it carries — the walk **is** the authentication. A shortcut that
/// returned a `Slot` without it would return exactly §12.1 clause 1c's phantom, with none of the
/// warning signs.
///
/// The cost is stated rather than optimised away: one claim is one walk, which a loop claiming on
/// `SPEC.md` §8.6's `CLAIM_CADENCE_SECONDS` pays once per cadence period.
pub struct ChainEntrySlotSource<'a, S> {
    source: &'a S,
    launcher_id: Bytes32,
}

impl<'a, S> ChainEntrySlotSource<'a, S> {
    /// Reads entry slots for the distributor launched at `launcher_id`, through `source`.
    pub fn new(source: &'a S, launcher_id: Bytes32) -> Self {
        Self {
            source,
            launcher_id,
        }
    }
}

impl<'a, S: dig_chainsource_interface::ChainSource> EntrySlotSource for ChainEntrySlotSource<'a, S> {
    /// Performs a full [`crate::state::read_distributor`] and takes the slot from that snapshot's
    /// [`crate::state::DistributorSnapshot::entry_slot`] accessor.
    ///
    /// # Errors
    ///
    /// - [`RewardsError::NoDistributorAtLauncherId`] when `read_distributor` answers `Ok(None)` —
    ///   no distributor was ever launched at this launcher id, which MUST NOT be degraded into
    ///   `Ok(None)` here: "this distributor does not exist" and "this peer holds no entry in it"
    ///   are different facts with different remedies (`SPEC.md` §12.5 clause 3a).
    /// - [`RewardsError::ChainUnavailable`] when the underlying read could not be established.
    fn read_entry_slot(
        &self,
        payout_puzzle_hash: Bytes32,
    ) -> Result<Option<Slot<RewardDistributorEntrySlotValue>>, RewardsError> {
        let snapshot = crate::state::read_distributor(self.source, self.launcher_id)?
            .ok_or(RewardsError::NoDistributorAtLauncherId {
                launcher_id: self.launcher_id,
            })?;

        Ok(snapshot.entry_slot(payout_puzzle_hash)?.cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chia_sdk_driver::{RewardDistributorType, RoundRewardInfo, RoundTimeInfo};

    fn some_constants(precision: u64) -> RewardDistributorConstants {
        RewardDistributorConstants {
            launcher_id: Bytes32::new([1; 32]),
            reward_distributor_type: RewardDistributorType::Managed {
                manager_singleton_launcher_id: Bytes32::new([7; 32]),
            },
            fee_payout_puzzle_hash: Bytes32::new([2; 32]),
            epoch_seconds: 1,
            precision,
            max_seconds_offset: 0,
            payout_threshold: 0,
            require_payout_approval: false,
            fee_bps: 0,
            withdrawal_share_bps: 0,
            reserve_asset_id: Bytes32::new([3; 32]),
            reserve_inner_puzzle_hash: Bytes32::new([4; 32]),
            reserve_full_puzzle_hash: Bytes32::new([5; 32]),
        }
    }

    fn some_state(cumulative_payout: u128) -> RewardDistributorState {
        RewardDistributorState {
            total_reserves: 0,
            active_shares: 0,
            round_reward_info: RoundRewardInfo {
                cumulative_payout,
                remaining_rewards: 0,
            },
            round_time_info: RoundTimeInfo {
                last_update: 0,
                epoch_end: 0,
            },
        }
    }

    fn some_entry(initial_cumulative_payout: u128, shares: u64) -> RewardDistributorEntrySlotValue {
        RewardDistributorEntrySlotValue {
            counter: 0,
            payout_puzzle_hash: Bytes32::new([6; 32]),
            initial_cumulative_payout,
            shares,
        }
    }

    /// The ordinary case: matches `withdrawal_amount_precision / precision` from
    /// `RewardDistributorInitiatePayoutAction::spend` by hand.
    #[test]
    fn accrues_shares_times_elapsed_payout_divided_by_precision() {
        let constants = some_constants(100);
        let state = some_state(1_000);
        let entry = some_entry(200, 10);

        // (1_000 - 200) * 10 / 100 = 80
        assert_eq!(accrued_base_units(&constants, &state, &entry), Some(80));
    }

    /// `state`'s `cumulative_payout` behind `entry`'s `initial_cumulative_payout` is a diverged
    /// read (never valid for this entry) -- `None`, never a wrapped/saturated figure.
    #[test]
    fn a_cumulative_payout_behind_the_entrys_initial_value_is_none_not_wrapped() {
        let constants = some_constants(100);
        let state = some_state(50);
        let entry = some_entry(200, 10);

        assert_eq!(accrued_base_units(&constants, &state, &entry), None);
    }

    /// A payout too large for `u64` is `None`, mirroring `spend`'s `u64::try_from` failing.
    #[test]
    fn a_payout_that_does_not_fit_u64_is_none() {
        let constants = some_constants(1);
        let state = some_state(u128::from(u64::MAX) + 1);
        let entry = some_entry(0, 1);

        assert_eq!(accrued_base_units(&constants, &state, &entry), None);
    }

    /// A zero precision would divide by zero in the puzzle's own arithmetic too; this must return
    /// `None` rather than panic.
    #[test]
    fn a_zero_precision_is_none_not_a_panic() {
        let constants = some_constants(0);
        let state = some_state(1_000);
        let entry = some_entry(0, 10);

        assert_eq!(accrued_base_units(&constants, &state, &entry), None);
    }

    /// `payout_threshold` must never be applied here -- it gates whether `InitiatePayout`
    /// succeeds, not what it would pay.
    #[test]
    fn the_payout_threshold_is_never_applied() {
        let mut constants = some_constants(1);
        constants.payout_threshold = 1_000_000;
        let state = some_state(10);
        let entry = some_entry(0, 1);

        assert_eq!(
            accrued_base_units(&constants, &state, &entry),
            Some(10),
            "a figure below the threshold is still the accrued figure, not zero"
        );
    }
}
