//! The state a UI needs from a distributor, and the reader that rebuilds it from the chain.
//!
//! # A failed read is never an empty answer
//!
//! Every `ChainSource` error MUST become [`crate::RewardsError::ChainUnavailable`]. A distributor
//! whose read failed MUST NOT render as "no entries" or "nothing accrued" — those are claims about
//! money, and the honest answer is that the question went unanswered. [`Ok(None)`] is reserved for
//! exactly one fact: the launcher id was never spent, i.e. no distributor was ever launched there.
//!
//! # The landmine this reader must never touch
//!
//! `RewardDistributor::from_parent_spend` substitutes an all-zero dummy `LineageProof` for the
//! reserve. A snapshot built that way reads perfectly and is **unspendable** — the operator pays a
//! network fee for a chain rejection. This module never calls it; see [`read_distributor`]'s doc
//! for the recipe it uses instead (`SPEC.md` §12.1 clause 1, #3267).

use chia_protocol::Bytes32;
use chia_sdk_driver::{RewardDistributor, RewardDistributorType, SpendContext};
use chia_sdk_types::puzzles::{
    RewardDistributorCommitmentSlotValue, RewardDistributorEntrySlotValue,
    RewardDistributorRewardSlotValue,
};
use dig_chainsource_interface::ChainSource;

use crate::RewardsError;

/// A distributor's entry set has not changed in this long, while its reserve is non-zero, is
/// reported `EntrySetStale` to anyone reading it (`SPEC.md` §12.4).
pub const STALE_ENTRY_SET_SECONDS: u64 = 172_800;

/// Turns a `ChainSource` error into the one `RewardsError` variant a failed read may ever produce.
fn chain_unavailable<E: core::fmt::Display>(error: E) -> RewardsError {
    RewardsError::ChainUnavailable(error.to_string())
}

fn malformed(reason: impl Into<String>) -> RewardsError {
    RewardsError::Malformed(reason.into())
}

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

impl DistributorSlots {
    /// Applies one generation's created/spent deltas: removes exactly one matching instance of
    /// each spent slot value, then appends the created ones.
    ///
    /// Slot values are plain data (no coin id inside), so identity is by VALUE — which is exactly
    /// what the puzzle itself treats as the slot, and exactly why an entry is removed rather than
    /// decremented: there is no other handle to remove by.
    fn apply_generation(
        &mut self,
        spent_entries: &[RewardDistributorEntrySlotValue],
        created_entries: &[RewardDistributorEntrySlotValue],
        spent_commitments: &[RewardDistributorCommitmentSlotValue],
        created_commitments: &[RewardDistributorCommitmentSlotValue],
        spent_rewards: &[RewardDistributorRewardSlotValue],
        created_rewards: &[RewardDistributorRewardSlotValue],
    ) {
        remove_one_each(&mut self.entries, spent_entries);
        self.entries.extend_from_slice(created_entries);

        remove_one_each(&mut self.commitments, spent_commitments);
        self.commitments.extend_from_slice(created_commitments);

        remove_one_each(&mut self.rewards, spent_rewards);
        self.rewards.extend_from_slice(created_rewards);
    }
}

/// Removes, from `set`, one occurrence of each value in `spent` — never all occurrences, since two
/// outstanding slots can be equal by value.
fn remove_one_each<T: PartialEq + Copy>(set: &mut Vec<T>, spent: &[T]) {
    for value in spent {
        if let Some(index) = set.iter().position(|existing| existing == value) {
            set.remove(index);
        }
    }
}

/// The chain view a snapshot was taken against. Every field mandatory: a snapshot without
/// provenance is the stale-read hazard itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainObservation {
    /// The fully-synced block height as of the read.
    pub peak_height: u32,
    /// `block_timestamp(peak_height)` — chain-derived "now", never the wall clock.
    pub peak_timestamp: u64,
    /// The tip coin id AT THE TIME OF THE READ. `peak_height` alone is insufficient: two different
    /// states can share a height. This is what makes "is this the same state I read?" checkable.
    pub tip_coin_id: Bytes32,
    /// The Unix time of the most recent entry-set write this walk observed, if any (`SPEC.md`
    /// §12.4). `None` means the entry set has never been written since launch.
    pub last_entry_write_unix: Option<u64>,
}

/// A distributor as it stands on chain, with everything a UI needs to answer "is anyone being
/// paid?".
///
/// Deliberately `#[derive(Debug)]` only — no `Clone`, no `Copy`, no serde — so a snapshot cannot be
/// cheaply duplicated and stored past the read that produced it. Re-derive with
/// [`read_distributor`] instead of holding on to one.
#[derive(Debug)]
pub struct DistributorSnapshot {
    /// The live singleton, ready for its next action.
    pub distributor: RewardDistributor,

    /// The outstanding slots.
    pub slots: DistributorSlots,

    /// The chain view this snapshot was read against.
    pub observed: ChainObservation,
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

    /// Whether the entry set has gone stale (`SPEC.md` §12.4): unchanged for
    /// [`STALE_ENTRY_SET_SECONDS`] while the reserve is non-zero. `false` while the reserve is
    /// zero — nothing is at stake to go stale.
    #[must_use]
    pub fn entry_set_stale(&self) -> bool {
        let Some(last_write) = self.observed.last_entry_write_unix else {
            return self.reserve_base_units() > 0;
        };

        self.reserve_base_units() > 0
            && self.observed.peak_timestamp.saturating_sub(last_write) >= STALE_ENTRY_SET_SECONDS
    }

    /// Whether the entry set is frozen for this distributor's LIFE: the `Managed` manager
    /// singleton's launcher id is the zero hash (`SPEC.md` §7.2 clause 3). A frozen entry set MUST
    /// be an observable fact, not a silence.
    #[must_use]
    pub fn entry_set_frozen(&self) -> bool {
        matches!(
            self.distributor.info.constants().reward_distributor_type,
            RewardDistributorType::Managed {
                manager_singleton_launcher_id
            } if manager_singleton_launcher_id == Bytes32::default()
        )
    }

    /// Re-reads the chain and reports whether this snapshot is still the current state: the
    /// authenticated tip coin id and the peak height must both match. Turns staleness into
    /// something ANSWERABLE rather than asserted.
    pub fn is_current(&self, source: &impl ChainSource) -> Result<bool, RewardsError> {
        let launcher_id = self.distributor.info.constants().launcher_id;
        let current = read_distributor(source, launcher_id)?;
        let Some(current) = current else {
            return Ok(false);
        };

        Ok(current.observed.tip_coin_id == self.observed.tip_coin_id
            && current.observed.peak_height == self.observed.peak_height)
    }
}

// `RewardDistributorInfo::constants` isn't a real accessor upstream (the field is named
// `constants` and is already public) -- this local extension exists only so the doc comments
// above can call it as a method without repeating `.info.constants` everywhere. Kept private.
trait ConstantsAccess {
    fn constants(&self) -> chia_sdk_driver::RewardDistributorConstants;
}

impl ConstantsAccess for chia_sdk_driver::RewardDistributorInfo {
    fn constants(&self) -> chia_sdk_driver::RewardDistributorConstants {
        self.constants
    }
}

/// Rebuilds a distributor's full state from the chain, from its launcher id alone.
///
/// `SPEC.md` §12.1 clause 1's recipe:
/// 1. `launcher_id == Bytes32::default()` — refuse; that is never a real launcher.
/// 2. [`ChainSource::coin_record`] on the launcher. `None` means no distributor was ever launched
///    there — the only case this function returns `Ok(None)` for.
/// 3. [`ChainSource::coin_spend`] on the launcher, then
///    `RewardDistributor::from_launcher_solution` for the launch constants, the initial state and
///    the eve coin.
/// 4. The eve coin's own spend, plus the eve-era reserve: the zero-amount coin at
///    `constants.reserve_full_puzzle_hash`, selected unambiguously by lowest confirmed height —
///    never picked arbitrarily.
/// 5. That candidate's `parent_spend`, parsed with `Cat::parse_children`, to recover
///    `reserve_parent_id` / `reserve_lineage_proof` and AUTHENTICATE the candidate as a genuine CAT
///    of `constants.reserve_asset_id` rather than an attacker-paid coin sitting at the same puzzle
///    hash.
/// 6. `RewardDistributor::from_eve_coin_spend` with that provenance — **never**
///    `from_parent_spend`, which fabricates a zero `LineageProof` for the reserve.
/// 7. A forward walk, generation by generation, each hop authenticated against the chain source's
///    own [`ChainSource::resolve_singleton_lineage`] membership set rather than trusted by
///    puzzle-hash equality.
/// 8. A money cross-check: the tip reserve coin must be unspent and its amount must equal
///    `state.total_reserves`.
/// 9. `peak_height` and `block_timestamp(peak_height)`, both mandatory.
pub fn read_distributor(
    source: &impl ChainSource,
    launcher_id: Bytes32,
) -> Result<Option<DistributorSnapshot>, RewardsError> {
    if launcher_id == Bytes32::default() {
        return Err(malformed("launcher id is the zero hash"));
    }

    if source
        .coin_record(launcher_id)
        .map_err(chain_unavailable)?
        .is_none()
    {
        return Ok(None);
    }

    let Some(launcher_spend) = source.coin_spend(launcher_id).map_err(chain_unavailable)? else {
        return Err(malformed(
            "launcher coin is recorded but has no recorded spend",
        ));
    };

    let mut ctx = SpendContext::new();
    let launcher_solution_ptr = ctx
        .alloc(&launcher_spend.solution)
        .map_err(RewardsError::from)?;

    let Some((constants, initial_state, eve_coin)) = RewardDistributor::from_launcher_solution(
        &mut ctx,
        launcher_spend.coin,
        launcher_solution_ptr,
    )
    .map_err(RewardsError::from)?
    else {
        return Err(malformed(
            "launcher solution is not a reward-distributor launch",
        ));
    };

    let Some(eve_spend) = source
        .coin_spend(eve_coin.coin_id())
        .map_err(chain_unavailable)?
    else {
        return Err(malformed("eve coin is recorded but has no recorded spend"));
    };

    let (reserve_parent_id, reserve_lineage_proof) =
        find_eve_reserve_provenance(&mut ctx, source, &constants)?;

    let Some((mut distributor, _reward_slot)) = RewardDistributor::from_eve_coin_spend(
        &mut ctx,
        constants,
        initial_state,
        &eve_spend,
        reserve_parent_id,
        reserve_lineage_proof,
    )
    .map_err(RewardsError::from)?
    else {
        return Err(malformed("eve coin spend did not produce a distributor"));
    };

    let lineage = source
        .resolve_singleton_lineage(launcher_id)
        .map_err(chain_unavailable)?
        .ok_or_else(|| malformed("launcher has no resolvable singleton lineage"))?;

    let mut slots = DistributorSlots::default();
    let mut last_entry_write_unix: Option<u64> = None;

    loop {
        if !lineage.contains(distributor.coin.coin_id()) {
            return Err(malformed(
                "a coin in the reader's own walk is not a member of the chain source's \
                 authenticated singleton lineage",
            ));
        }

        let Some(spend) = source
            .coin_spend(distributor.coin.coin_id())
            .map_err(chain_unavailable)?
        else {
            // Unspent: this generation is the tip.
            break;
        };

        let reserve_lineage_proof = distributor.reserve.child_lineage_proof();
        let Some(reconstructed) = chia_sdk_driver::RewardDistributor::from_spend(
            &mut ctx,
            &spend,
            Some(reserve_lineage_proof),
            constants,
            chia_bls::Signature::default(),
        )
        .map_err(RewardsError::from)?
        else {
            return Err(malformed(
                "a spend mid-walk did not parse as this distributor",
            ));
        };

        let entry_write_happened = !reconstructed.pending_spend.created_entry_slots.is_empty()
            || !reconstructed.pending_spend.spent_entry_slots.is_empty();

        slots.apply_generation(
            &reconstructed.pending_spend.spent_entry_slots,
            &reconstructed.pending_spend.created_entry_slots,
            &reconstructed.pending_spend.spent_commitment_slots,
            &reconstructed.pending_spend.created_commitment_slots,
            &reconstructed.pending_spend.spent_reward_slots,
            &reconstructed.pending_spend.created_reward_slots,
        );

        let next_state = reconstructed.pending_spend.latest_state.1;
        distributor = reconstructed.child(next_state);

        if entry_write_happened {
            let spent_height = source
                .coin_record(spend.coin.coin_id())
                .map_err(chain_unavailable)?
                .and_then(|record| record.spent_height);
            if let Some(height) = spent_height {
                last_entry_write_unix = source
                    .block_timestamp(height)
                    .map_err(chain_unavailable)?
                    .or(last_entry_write_unix);
            }
        }
    }

    let tip_coin_id = distributor.coin.coin_id();

    let tip_reserve_record = source
        .coin_record(distributor.reserve.coin.coin_id())
        .map_err(chain_unavailable)?
        .ok_or_else(|| malformed("tip reserve coin is not present on chain"))?;
    if tip_reserve_record.is_spent() {
        return Err(malformed("tip reserve coin is already spent"));
    }
    if tip_reserve_record.coin.amount != distributor.info.state.total_reserves {
        return Err(malformed(
            "tip reserve amount does not match the distributor's own total_reserves",
        ));
    }

    let peak_height = source
        .peak_height()
        .map_err(chain_unavailable)?
        .ok_or_else(|| malformed("chain source exposes no peak height"))?;
    let peak_timestamp = source
        .block_timestamp(peak_height)
        .map_err(chain_unavailable)?
        .ok_or_else(|| malformed("chain source has no timestamp for its own peak"))?;

    Ok(Some(DistributorSnapshot {
        distributor,
        slots,
        observed: ChainObservation {
            peak_height,
            peak_timestamp,
            tip_coin_id,
            last_entry_write_unix,
        },
    }))
}

/// Steps 4-5 of the recipe: selects the eve-era reserve candidate unambiguously and authenticates
/// it as a genuine CAT of `constants.reserve_asset_id`, returning the provenance
/// `from_eve_coin_spend` needs.
fn find_eve_reserve_provenance(
    ctx: &mut SpendContext,
    source: &impl ChainSource,
    constants: &chia_sdk_driver::RewardDistributorConstants,
) -> Result<(Bytes32, chia_puzzle_types::LineageProof), RewardsError> {
    let candidates = source
        .coin_records_by_puzzle_hash(constants.reserve_full_puzzle_hash, true)
        .map_err(chain_unavailable)?;

    let zero_amount: Vec<_> = candidates
        .into_iter()
        .filter(|record| record.coin.amount == 0)
        .collect();

    if zero_amount.is_empty() {
        return Err(malformed(
            "no zero-amount eve-era reserve candidate at the reserve puzzle hash",
        ));
    }

    let min_height = zero_amount
        .iter()
        .filter_map(|record| record.confirmed_height)
        .min()
        .ok_or_else(|| malformed("eve-era reserve candidate(s) have no confirmed height"))?;

    let mut at_min_height: Vec<_> = zero_amount
        .into_iter()
        .filter(|record| record.confirmed_height == Some(min_height))
        .collect();

    let candidate = match (at_min_height.pop(), at_min_height.is_empty()) {
        (Some(only), true) => only,
        _ => {
            return Err(malformed(
                "ambiguous eve-era reserve candidates at the lowest confirmed height",
            ));
        }
    };

    let candidate_coin_id = candidate.coin.coin_id();
    let Some(parent_spend) = source
        .parent_spend(candidate_coin_id)
        .map_err(chain_unavailable)?
    else {
        return Err(malformed(
            "eve-era reserve candidate's parent spend could not be resolved",
        ));
    };

    let parent_puzzle_ptr = ctx
        .alloc(&parent_spend.puzzle_reveal)
        .map_err(RewardsError::from)?;
    let parent_puzzle = chia_sdk_driver::Puzzle::parse(ctx, parent_puzzle_ptr);
    let parent_solution_ptr = ctx
        .alloc(&parent_spend.solution)
        .map_err(RewardsError::from)?;

    let children = chia_sdk_driver::Cat::parse_children(
        ctx,
        parent_spend.coin,
        parent_puzzle,
        parent_solution_ptr,
    )
    .map_err(RewardsError::from)?
    .ok_or_else(|| malformed("eve-era reserve candidate's parent spend is not a CAT spend"))?;

    let authenticated = children
        .into_iter()
        .find(|child| child.coin.coin_id() == candidate_coin_id)
        .ok_or_else(|| {
            malformed(
                "eve-era reserve candidate is not among its claimed parent's children -- \
                 not a genuine CAT of the reserve asset id",
            )
        })?;

    if authenticated.info.asset_id != constants.reserve_asset_id {
        return Err(malformed(
            "eve-era reserve candidate's authenticated asset id does not match the distributor's",
        ));
    }

    let reserve_lineage_proof = authenticated
        .lineage_proof
        .ok_or_else(|| malformed("authenticated eve-era reserve candidate has no lineage proof"))?;

    Ok((candidate.coin.parent_coin_info, reserve_lineage_proof))
}
