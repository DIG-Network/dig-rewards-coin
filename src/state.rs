//! The state a UI needs from a distributor, and the reader that rebuilds it from the chain.
//!
//! # A failed read is never an empty answer
//!
//! Every `ChainSource` error MUST become [`crate::RewardsError::ChainUnavailable`]. A distributor
//! whose read failed MUST NOT render as "no entries" or "nothing accrued" — those are claims about
//! money, and the honest answer is that the question went unanswered. `Ok(None)` is reserved for
//! exactly one fact: the launcher id was never spent, i.e. no distributor was ever launched there.
//!
//! # The landmine this reader must never touch
//!
//! `RewardDistributor::from_parent_spend` substitutes an all-zero dummy `LineageProof` for the
//! reserve. A snapshot built that way reads perfectly and is **unspendable** — the operator pays a
//! network fee for a chain rejection. This module never calls it; see [`read_distributor`]'s doc
//! for the recipe it uses instead (`SPEC.md` §12.1 clause 1, #3267).

use chia_protocol::{Bytes32, CoinSpend};
use chia_puzzle_types::singleton::SingletonSolution;
use chia_sdk_driver::{
    ActionLayer, HashedPtr, Layer, RewardDistributor, RewardDistributorActionLog,
    RewardDistributorAddEntryAction, RewardDistributorAddIncentivesAction,
    RewardDistributorCommitIncentivesAction, RewardDistributorConstants,
    RewardDistributorInitiatePayoutAction, RewardDistributorNewEpochAction,
    RewardDistributorRefreshAction, RewardDistributorRemoveEntryAction,
    RewardDistributorStakeAction, RewardDistributorState, RewardDistributorType,
    RewardDistributorUnstakeAction, RewardDistributorWithdrawIncentivesAction, SingletonAction,
    Slot, SpendContext,
};
use chia_sdk_types::puzzles::{
    RewardDistributorAddIncentivesActionArgs, RewardDistributorCommitmentSlotValue,
    RewardDistributorEntrySlotValue, RewardDistributorInitiatePayoutWithApprovalActionArgs,
    RewardDistributorInitiatePayoutWithoutApprovalActionArgs, RewardDistributorRewardSlotValue,
    RewardDistributorSlotNonce, RewardDistributorSyncActionArgs,
};
use chia_sdk_types::Mod;
use clvm_traits::clvm_tuple;
use clvmr::NodePtr;
use dig_chainsource_interface::ChainSource;

use crate::RewardsError;

/// A distributor's entry set has not changed in this long, while its reserve is non-zero, is
/// reported `EntrySetStale` to anyone reading it (`SPEC.md` §12.4).
pub const STALE_ENTRY_SET_SECONDS: u64 = 172_800;

/// The largest number of reward slots `refuse_unrepresentable_action_arithmetic` will let a
/// single `commit_incentives` action backfill, in its non-adjacent-epoch branch
/// (`chia-sdk-driver-0.36.0`'s `commit_incentives.rs:101-112`), before refusing the generation.
///
/// Fixed and independent of any distributor's own declared constants — never a ratio of
/// `max_seconds_offset` to `epoch_seconds`, which produced `0` at DIG's own real launch constants
/// (`300` / `604_800`) while an attacker's own `epoch_seconds = 1, max_seconds_offset = u64::MAX`
/// inflated the same ratio past `1.8e19`, a bound in name only. See
/// `RewardsError::CommitIncentivesBackfillBoundExceeded`'s doc for both failures.
///
/// One million is comfortably beyond any plausible historical gap in real incentive commitments
/// — at DIG's own one-week epoch it is roughly nineteen thousand years of backfilled epochs --
/// while still bounding what this reader must hold in memory for one action to a few tens of
/// megabytes of `RewardDistributorRewardSlotValue` structs (~40 bytes each), never the unbounded
/// count an attacker's own `epoch_seconds` could otherwise demand.
pub const MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS: u64 = 1_000_000;

/// The largest `max_seconds_offset` a distributor's own launch constants may declare before
/// `read_distributor` refuses to read it at all. See
/// `RewardsError::UnreadableMaxSecondsOffset`'s doc for why: nothing in `chia-sdk-driver` 0.36.0
/// or elsewhere in this crate bounds this value on its own, so an unbounded reader would trust an
/// attacker's own launch constant as a real clock-skew tolerance.
pub const MAX_SANE_SECONDS_OFFSET: u64 = u32::MAX as u64;

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
    ///
    /// # Errors
    ///
    /// If a generation spends a slot this walk never saw created. That means the reader's model of
    /// the chain disagrees with the chain, and per this module's own doctrine a diverged read is an
    /// error rather than a silently smaller answer.
    fn apply_generation(
        &mut self,
        spent_entries: &[RewardDistributorEntrySlotValue],
        created_entries: &[RewardDistributorEntrySlotValue],
        spent_commitments: &[RewardDistributorCommitmentSlotValue],
        created_commitments: &[RewardDistributorCommitmentSlotValue],
        spent_rewards: &[RewardDistributorRewardSlotValue],
        created_rewards: &[RewardDistributorRewardSlotValue],
    ) -> Result<(), RewardsError> {
        remove_one_each(&mut self.entries, spent_entries, "entry")?;
        self.entries.extend_from_slice(created_entries);

        remove_one_each(&mut self.commitments, spent_commitments, "commitment")?;
        self.commitments.extend_from_slice(created_commitments);

        remove_one_each(&mut self.rewards, spent_rewards, "reward")?;
        self.rewards.extend_from_slice(created_rewards);

        Ok(())
    }
}

/// Removes, from `set`, one occurrence of each value in `spent` — never all occurrences, since two
/// outstanding slots can be equal by value.
///
/// # Errors
///
/// If a value in `spent` is not in `set`. The chain cannot spend a slot that was never created, so
/// a spend the walk cannot account for means the walk's model is WRONG — and absorbing it would
/// render a diverged read as a merely smaller one, which for a slot set is a claim about money.
fn remove_one_each<T: PartialEq + Copy>(
    set: &mut Vec<T>,
    spent: &[T],
    slot_kind: &str,
) -> Result<(), RewardsError> {
    for value in spent {
        let Some(index) = set.iter().position(|existing| existing == value) else {
            return Err(malformed(format!(
                "a generation spends a {slot_kind} slot this walk never saw created -- the \
                 reader's model of the chain disagrees with the chain"
            )));
        };
        set.remove(index);
    }

    Ok(())
}

/// The chain view a snapshot was taken against. Every field mandatory: a snapshot without
/// provenance is the stale-read hazard itself.
///
/// Read-only by construction: the fields are private, there is no public constructor, and no
/// setter. Only [`read_distributor`] can mint one, so an observation always describes a read that
/// actually happened. That is what makes the pairing inside a [`DistributorSnapshot`] unforgeable
/// — a caller cannot attach a fresh observation to stale data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainObservation {
    peak_height: u32,
    peak_timestamp: u64,
    tip_coin_id: Bytes32,
    last_entry_write_unix: Option<u64>,
}

impl ChainObservation {
    /// The fully-synced block height as of the read.
    #[must_use]
    pub fn peak_height(&self) -> u32 {
        self.peak_height
    }

    /// `block_timestamp(peak_height)` — chain-derived "now", never the wall clock.
    #[must_use]
    pub fn peak_timestamp(&self) -> u64 {
        self.peak_timestamp
    }

    /// The tip coin id AT THE TIME OF THE READ. `peak_height` alone is insufficient: two different
    /// states can share a height. This is what makes "is this the same state I read?" checkable.
    #[must_use]
    pub fn tip_coin_id(&self) -> Bytes32 {
        self.tip_coin_id
    }

    /// The Unix time of the most recent entry-set WRITE (`AddEntry` / `RemoveEntry`) this walk
    /// observed, if any (`SPEC.md` §12.4).
    ///
    /// `None` is a positive fact, not missing information: the walk covers every generation from
    /// the eve coin to the tip or it fails, so `None` means no entry-set write has ever happened
    /// since launch.
    pub fn last_entry_write_unix(&self) -> Option<u64> {
        self.last_entry_write_unix
    }
}

/// A distributor as it stands on chain, with everything a UI needs to answer "is anyone being
/// paid?".
///
/// # What this type guarantees, and what it does not
///
/// The fields are private and there is no public constructor, so the pairing of chain data with
/// the [`ChainObservation`] it was read under is **unforgeable**: only [`read_distributor`] can
/// produce a snapshot, and it always pairs the data with the observation of that same read. A
/// caller therefore cannot present stale data under a fresh observation, which is the one
/// guarantee this type needs to make [`Self::is_current`] mean anything.
///
/// It does NOT prevent a caller keeping a copy of the CONTENTS. `RewardDistributor` and
/// [`DistributorSlots`] are `Clone` upstream, so anything reachable through [`Self::distributor`]
/// or [`Self::slots`] can be copied out and held indefinitely — and once copied out it carries no
/// observation at all. `#[derive(Debug)]`-only makes the aggregate awkward to duplicate wholesale
/// and nothing more.
///
/// So: **a caller about to spend MUST re-read**, either with [`read_distributor`] or by asking
/// [`Self::is_current`], rather than trusting a snapshot it has been holding. A type documented as
/// stronger than it is would be worse than this residual.
#[derive(Debug)]
pub struct DistributorSnapshot {
    distributor: RewardDistributor,
    slots: DistributorSlots,
    observed: ChainObservation,
}

impl DistributorSnapshot {
    /// The live singleton, ready for its next action.
    ///
    /// The contents are `Clone`: see the type's own docs for why a copy taken from here must not be
    /// trusted as current.
    pub fn distributor(&self) -> &RewardDistributor {
        &self.distributor
    }

    /// The outstanding slots.
    #[must_use]
    pub fn slots(&self) -> &DistributorSlots {
        &self.slots
    }

    /// The chain view this snapshot was read against.
    #[must_use]
    pub fn observed(&self) -> &ChainObservation {
        &self.observed
    }

    /// How many entries the set holds.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.slots.entries.len()
    }

    /// Base units of the distributor's own reserve CAT, as the puzzle itself records them.
    ///
    /// **Not necessarily $DIG.** This is a generic reward-distributor reader: `reserve_asset_id`
    /// is whatever the launch curried, and nothing here checks it against
    /// `dig_constants::DIG_ASSET_ID`. A caller that needs $DIG specifically MUST compare
    /// `distributor().info.constants.reserve_asset_id` itself.
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

    /// Whether the entry set has gone stale (`SPEC.md` §12.4): no entry-set WRITE for
    /// [`STALE_ENTRY_SET_SECONDS`] of chain time while the reserve is non-zero.
    ///
    /// Three cases, all of them chain-derived, never a wall clock:
    /// - reserve zero → `false`. Nothing is at stake, so nothing can go stale.
    /// - reserve non-zero and a write observed → stale once
    ///   `peak_timestamp - last_entry_write_unix` reaches the threshold.
    /// - reserve non-zero and NO write ever observed → `true`. This is not an absence of
    ///   information: the walk covers every generation from the eve coin to the tip or it fails,
    ///   so `None` is the positive fact "the entry set has never been written since launch", and
    ///   a funded distributor that has never had an entry set is exactly what §12.4 warns about.
    #[must_use]
    pub fn entry_set_stale(&self) -> bool {
        entry_set_is_stale(
            self.reserve_base_units(),
            self.observed.peak_timestamp,
            self.observed.last_entry_write_unix,
        )
    }

    /// Whether the entry set is frozen for this distributor's LIFE, because the authority that
    /// would write it is the zero hash (`SPEC.md` §7.2 clause 3). A frozen entry set MUST be an
    /// observable fact, not a silence.
    ///
    /// Checked for EVERY `RewardDistributorType`, not just `Managed`: each variant names the one
    /// identity entries can ever come from, and a zero there is unreachable by construction, so no
    /// entry can ever be added or removed again.
    #[must_use]
    pub fn entry_set_frozen(&self) -> bool {
        entry_set_is_frozen(self.distributor.info.constants().reward_distributor_type)
    }

    /// Re-reads the chain and reports whether this snapshot is still the current state.
    ///
    /// Compares the authenticated tip coin id ALONE, which is the question a caller about to spend
    /// actually has: *is the state I read still the state I would be spending against?* It
    /// deliberately does not compare [`ChainObservation::peak_height`] — a block arriving without
    /// spending the distributor changes the peak and changes nothing this caller depends on, and
    /// conjoining the height would make every snapshot report `false` within seconds of any
    /// mainnet read, which is a check consumers drop rather than obey.
    ///
    /// What it proves: the distributor singleton has not been spent since the read, so the entry
    /// set, the reserve and the slot set in this snapshot are the chain's current ones. What it
    /// does NOT prove: that the chain source is honest, that a spend is not already in the mempool,
    /// or anything about wall-clock freshness — read [`ChainObservation::peak_height`] and
    /// [`ChainObservation::peak_timestamp`] through [`Self::observed`] for those questions.
    ///
    /// `Ok(false)` also covers a launcher whose record has vanished from the source entirely.
    pub fn is_current(&self, source: &impl ChainSource) -> Result<bool, RewardsError> {
        let launcher_id = self.distributor.info.constants().launcher_id;
        let current = read_distributor(source, launcher_id)?;
        let Some(current) = current else {
            return Ok(false);
        };

        Ok(current.observed.tip_coin_id == self.observed.tip_coin_id)
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

/// [`DistributorSnapshot::entry_set_stale`]'s decision, over plain inputs so every branch and its
/// inversion can be tested directly.
fn entry_set_is_stale(
    reserve_base_units: u64,
    peak_timestamp: u64,
    last_entry_write_unix: Option<u64>,
) -> bool {
    if reserve_base_units == 0 {
        return false;
    }

    let Some(last_write) = last_entry_write_unix else {
        // The walk is complete or it errors, so this is "never written since launch" -- a funded
        // distributor with no entry set is the §12.4 case, not missing information.
        return true;
    };

    peak_timestamp.saturating_sub(last_write) >= STALE_ENTRY_SET_SECONDS
}

/// [`DistributorSnapshot::entry_set_frozen`]'s decision, over the type alone.
///
/// Every variant carries exactly one identity that entries can originate from. A zero there is an
/// unset field that no spend can ever satisfy, so the entry set can never change again.
fn entry_set_is_frozen(kind: RewardDistributorType) -> bool {
    let identity = match kind {
        // Only this singleton can authorize AddEntry / RemoveEntry.
        RewardDistributorType::Managed {
            manager_singleton_launcher_id,
        } => manager_singleton_launcher_id,
        // Entries come from NFTs of this collection.
        RewardDistributorType::NftCollection {
            collection_did_launcher_id,
        } => collection_did_launcher_id,
        // Entries come from the DL store this names.
        RewardDistributorType::CuratedNft {
            store_launcher_id, ..
        } => store_launcher_id,
        // Entries come from stakes of this CAT.
        RewardDistributorType::Cat { asset_id, .. } => asset_id,
    };

    identity == Bytes32::default()
}

/// A fail-closed pre-screen ahead of `chia-sdk-driver` 0.36.0's unchecked action arithmetic
/// (DIG-Network/dig_ecosystem#3313).
///
/// **This is a shim with an exit, not a durable fix.** The durable fix belongs upstream, in
/// `chia-sdk-driver`'s own `get_log` methods
/// (<https://github.com/xch-dev/chia-wallet-sdk/issues/436>); this function exists only because
/// this crate cannot patch a pinned dependency and must not ship a reader that panics or
/// fabricates a figure in the meantime. Delete it the day the pin cohort carries the fix.
///
/// Four of the eleven reward-distributor actions perform unchecked arithmetic on their own
/// solution's fields (or on a value their own solution's locked/unlocked commitment
/// authenticates), inside `get_log`, which runs INSIDE `RewardDistributor::from_spend` -- before
/// this crate's own B1/B2 guards ever get a chance to run on `from_spend`'s return. Every one of
/// the eleven `get_log` bodies was read for this round (DIG-Network/dig_ecosystem#3313's
/// re-gate); this is the complete hazard set, not the dispatch enumeration:
/// - `withdraw_incentives.rs:71` -- `committed_value * withdrawal_share_bps`
/// - `withdraw_incentives.rs:89` -- `reward_slot_total_rewards - withdrawal_share`
/// - `commit_incentives.rs:85` -- `slot_total_rewards + rewards_to_add` (only when
///   `slot_epoch_time == epoch_start`; the adjacent-epoch branch)
/// - `commit_incentives.rs:101` -- `slot_epoch_time + epoch_seconds` (the non-adjacent-epoch
///   branch's backfill loop init)
/// - `commit_incentives.rs:103-112` -- the backfill loop itself: not an overflow hazard but a
///   non-termination / unbounded-iteration one (see [`RewardsError::UnreadableEpochSeconds`]
///   and [`RewardsError::CommitIncentivesBackfillBoundExceeded`])
/// - `unstake.rs:235` -- `entry_slot.shares - removed_shares`, where `removed_shares` is the
///   output of running the unstake action's own unlock puzzle (`unstake.rs:228`) against the
///   solution's `unlock_puzzle_solution` -- there is no static bound on it in the solution, so
///   closing this site means running that puzzle ourselves, exactly as `get_log` does
/// - `stake.rs:326` -- `u64::try_from(existing_slot_counter + 1)` where `existing_slot_counter`
///   is `i128`: the `+ 1` is unchecked `i128` arithmetic and panics at `i128::MAX` before
///   `try_from` ever runs
/// - `stake.rs:329` -- `existing_slot_shares + new_shares`, where `new_shares` is likewise the
///   output of running the stake action's own lock puzzle (`stake.rs:322`) -- both operands
///   attacker-influenced, one of them only knowable by running attacker-supplied CLVM
///
/// **Six counter-increment sites are deliberately NOT checked here**, stated once: each is bounded
/// by the slot's own real on-chain counter (a small, monotonically-incrementing value copied
/// forward from a genuine prior slot), never by an attacker-supplied magnitude, so none is a
/// vector this pre-screen needs to close --
/// `commit_incentives.rs:82`, `commit_incentives.rs:89`, `withdraw_incentives.rs:86`,
/// `new_epoch.rs:98`, `initiate_payout.rs:107`, `refresh.rs:136`.
/// `new_epoch.rs:124`'s `epoch_total_rewards * fee_bps / 10000` is also NOT checked here: that
/// multiply lives only in `NewEpochAction::spend` (the write-side builder), never in `get_log`,
/// and `from_spend`/this reader's walk calls only `get_log` -- see dig-rewards-coin#10, which
/// stays deferred on that ground and is not closed by this pre-screen. `refresh.rs:143-144`
/// already uses the safe wide-int pattern this crate's own
/// [`crate::clawback::recoverable_base_units`] follows (`i128::from(x) + i128::from(y)` then
/// `u64::try_from`) -- the contrast that shows the unchecked sites above are oversights, not a
/// deliberate design upstream chose.
///
/// Two fail-closed rules, checked for every action spend in the generation's inner solution,
/// BEFORE `from_spend` is called on that generation at all:
///
/// 1. **Refuse on any action solution this pre-screen cannot parse** (an unparseable inner
///    solution, or an unparseable solution for one of the three screened actions once its puzzle
///    hash is recognised).
/// 2. **Refuse on any action puzzle hash this pre-screen does not recognise as one of the eleven
///    reward-distributor actions `chia-sdk-driver` 0.36.0 defines**
///    (`chia-sdk-driver-0.36.0/src/primitives/action_layer/reward_distributor.rs:131-166`
///    enumerates the same eleven). This is deliberately NOT "skip unknown": a future pin bump that
///    changes, adds or removes an action puzzle makes every read refuse here, loudly, on the
///    first test that exercises it -- instead of silently walking past an action this pre-screen
///    was never taught to screen for its own unchecked-arithmetic hazard. Over-refusing on drift
///    is the only direction a refuse-don't-serve reader may fail in.
fn refuse_unrepresentable_action_arithmetic(
    ctx: &mut SpendContext,
    spend: &CoinSpend,
    constants: RewardDistributorConstants,
) -> Result<(), RewardsError> {
    let solution_ptr = ctx.alloc(&spend.solution).map_err(RewardsError::from)?;
    let singleton_solution = ctx
        .extract::<SingletonSolution<_>>(solution_ptr)
        .map_err(RewardsError::from)?;

    let action_layer_solution = ActionLayer::<RewardDistributorState, HashedPtr>::parse_solution(
        ctx,
        singleton_solution.inner_solution,
    )
    .map_err(RewardsError::from)?;

    // The same eleven action puzzle hashes upstream itself precomputes for its own dispatch
    // (`reward_distributor.rs:131-166`), derived the same way it derives them: curry the action's
    // own constants-derived arguments and hash, never running any CLVM.
    let withdraw_incentives_hash = RewardDistributorWithdrawIncentivesAction::new_args(
        constants.launcher_id,
        constants.withdrawal_share_bps,
    )
    .curry_tree_hash();
    let commit_incentives_hash = RewardDistributorCommitIncentivesAction::new_args(
        constants.launcher_id,
        constants.epoch_seconds,
    )
    .curry_tree_hash();
    let new_epoch_action = RewardDistributorNewEpochAction::from_constants(&constants);
    let new_epoch_hash = RewardDistributorNewEpochAction::new_args(
        new_epoch_action.launcher_id,
        new_epoch_action.fee_payout_puzzle_hash,
        new_epoch_action.fee_bps,
        new_epoch_action.epoch_seconds,
        new_epoch_action.precision,
    )
    .curry_tree_hash();
    let add_entry_action = RewardDistributorAddEntryAction::from_constants(&constants);
    let add_entry_hash = RewardDistributorAddEntryAction::new_args(
        add_entry_action.launcher_id,
        add_entry_action.manager_launcher_id,
        add_entry_action.max_second_offset,
    )
    .curry_tree_hash();
    let remove_entry_action = RewardDistributorRemoveEntryAction::from_constants(&constants);
    let remove_entry_hash = RewardDistributorRemoveEntryAction::new_args(
        remove_entry_action.launcher_id,
        remove_entry_action.manager_launcher_id,
        remove_entry_action.max_seconds_offset,
        remove_entry_action.precision,
    )
    .curry_tree_hash();
    let stake_action = RewardDistributorStakeAction::from_constants(&constants);
    let stake_hash = RewardDistributorStakeAction::new_args_treehash(
        stake_action.launcher_id,
        stake_action.max_second_offset,
        stake_action.distributor_type,
    )
    .curry_tree_hash();
    let unstake_action = RewardDistributorUnstakeAction::from_constants(&constants);
    let unstake_hash = RewardDistributorUnstakeAction::new_args_treehash(
        unstake_action.launcher_id,
        unstake_action.max_second_offset,
        unstake_action.precision,
        unstake_action.distributor_type,
    )
    .curry_tree_hash();
    let initiate_payout_action = RewardDistributorInitiatePayoutAction::from_constants(&constants);
    let entry_slot_1st_curry_hash: Bytes32 = Slot::<()>::first_curry_hash(
        initiate_payout_action.launcher_id,
        RewardDistributorSlotNonce::ENTRY.to_u64(),
    )
    .into();
    let initiate_payout_hash = if initiate_payout_action.require_approval {
        RewardDistributorInitiatePayoutWithApprovalActionArgs {
            entry_slot_1st_curry_hash,
            payout_threshold: initiate_payout_action.payout_threshold,
            precision: initiate_payout_action.precision,
        }
        .curry_tree_hash()
    } else {
        RewardDistributorInitiatePayoutWithoutApprovalActionArgs {
            entry_slot_1st_curry_hash,
            payout_threshold: initiate_payout_action.payout_threshold,
            precision: initiate_payout_action.precision,
        }
        .curry_tree_hash()
    };
    let add_incentives_action = RewardDistributorAddIncentivesAction::from_constants(&constants);
    let add_incentives_hash = RewardDistributorAddIncentivesActionArgs {
        fee_payout_puzzle_hash: add_incentives_action.fee_payout_puzzle_hash,
        fee_bps: add_incentives_action.fee_bps,
        precision: add_incentives_action.precision,
    }
    .curry_tree_hash();
    let sync_hash = RewardDistributorSyncActionArgs::curry_tree_hash();
    // Refresh is only a legal action puzzle for a refreshable curated-NFT distributor; for every
    // other distributor type its own `new_args` refuses to build one, so there is no puzzle hash
    // to recognise -- narrowing rule 2 correctly rather than weakening it.
    let refresh_action = RewardDistributorRefreshAction::from_constants(&constants);
    let refresh_hash = RewardDistributorRefreshAction::new_args(
        refresh_action.launcher_id,
        refresh_action.max_second_offset,
        refresh_action.distributor_type,
        refresh_action.precision,
    )
    .ok()
    .map(|args| args.curry_tree_hash());

    for action_spend in &action_layer_solution.action_spends {
        let raw_action_hash = ctx.tree_hash(action_spend.puzzle);

        if raw_action_hash == withdraw_incentives_hash {
            let params = ctx
                .extract::<chia_sdk_types::puzzles::RewardDistributorWithdrawIncentivesActionSolution>(
                    action_spend.solution,
                )
                .map_err(RewardsError::from)?;

            let raw_share = params
                .committed_value
                .checked_mul(constants.withdrawal_share_bps)
                .ok_or(RewardsError::ActionArithmeticNotRepresentable {
                    action: "withdraw_incentives",
                    operation: "committed_value * withdrawal_share_bps",
                })?;
            let withdrawal_share = raw_share / 10_000;

            params
                .reward_slot_total_rewards
                .checked_sub(withdrawal_share)
                .ok_or(RewardsError::ActionArithmeticNotRepresentable {
                    action: "withdraw_incentives",
                    operation: "reward_slot_total_rewards - withdrawal_share",
                })?;
        } else if raw_action_hash == commit_incentives_hash {
            let params = ctx
                .extract::<chia_sdk_types::puzzles::RewardDistributorCommitIncentivesActionSolution>(
                    action_spend.solution,
                )
                .map_err(RewardsError::from)?;

            if params.slot_epoch_time == params.epoch_start {
                params
                    .slot_total_rewards
                    .checked_add(params.rewards_to_add)
                    .ok_or(RewardsError::ActionArithmeticNotRepresentable {
                        action: "commit_incentives",
                        operation: "slot_total_rewards + rewards_to_add",
                    })?;
            } else {
                // The non-adjacent-epoch branch: `commit_incentives.rs:88-112` backfills one
                // empty reward slot per epoch between `params.slot_epoch_time` and
                // `params.epoch_start`, stepping by the distributor's own `constants.epoch_seconds`
                // (curried into the action puzzle at launch, not a solution field -- see
                // `RewardsError::UnreadableEpochSeconds`'s doc).
                // `read_distributor` already refused this distributor outright on its launch
                // constants, before any generation was parsed. Repeated here because this
                // function is also reachable directly (its own unit tests call it), and a screen
                // that divides by `epoch_seconds` below must be TOTAL rather than rely on a
                // caller's discipline: an `epoch_seconds` of zero would make `div_ceil` panic.
                if constants.epoch_seconds == 0 {
                    return Err(RewardsError::UnreadableEpochSeconds);
                }

                let start_epoch_time = params
                    .slot_epoch_time
                    .checked_add(constants.epoch_seconds)
                    .ok_or(RewardsError::ActionArithmeticNotRepresentable {
                        action: "commit_incentives",
                        operation: "slot_epoch_time + epoch_seconds",
                    })?;

                // A FIXED cap, `MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS` -- never a ratio of the
                // distributor's own declared constants. An earlier version of this bound divided
                // `max_seconds_offset` by `epoch_seconds`, which was wrong in both directions at
                // once: at DIG's own real launch constants (`300` / `604_800`) the quotient is
                // `0`, refusing every honest non-adjacent-epoch commit outright, while an
                // attacker's own `epoch_seconds = 1, max_seconds_offset = u64::MAX` inflated the
                // same quotient past `1.8e19`, no bound at all. `read_distributor` is
                // deliberately distributor-agnostic, so for an attacker's own launcher every
                // operand of that ratio was attacker-chosen -- a ratio of two attacker-reachable
                // parameters cannot be a safety bound in either direction. See
                // `RewardsError::CommitIncentivesBackfillBoundExceeded`'s doc.
                let max_backfill_slots = MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS;

                if params.epoch_start > start_epoch_time {
                    // Upstream's loop is `while end_epoch_time > start_epoch_time { ...;
                    // start_epoch_time += epoch_seconds }`, so the count is the CEILING of the gap
                    // over the step, not the floor plus one -- floor-plus-one over-counts by
                    // exactly one whenever the gap divides evenly, which would make the refusal's
                    // own `iterations` figure a count no loop ever runs.
                    let iterations =
                        (params.epoch_start - start_epoch_time).div_ceil(constants.epoch_seconds);
                    if iterations > max_backfill_slots {
                        return Err(RewardsError::CommitIncentivesBackfillBoundExceeded {
                            iterations,
                            max_backfill_slots,
                        });
                    }
                }
            }
        } else if raw_action_hash == unstake_hash {
            let params = ctx
                .extract::<chia_sdk_types::puzzles::RewardDistributorUnstakeActionSolution<NodePtr>>(
                    action_spend.solution,
                )
                .map_err(RewardsError::from)?;

            // `unstake.rs:228-229`: `removed_shares` is the output of running the unstake
            // action's own unlock puzzle -- there is no static bound on it in the solution, so
            // closing `unstake.rs:235` means running that puzzle ourselves, exactly as `get_log`
            // does. `ctx.run` bounds CPU by `chia-sdk-types::MAINNET_CONSTANTS.max_block_cost_clvm`
            // and memory by the allocator's atom/pair caps, and upstream itself already runs
            // attacker CLVM unconditionally on every action before hash dispatch
            // (`reward_distributor.rs:168`) -- so this adds no new class of exposure, only a
            // second bounded, already-tolerated run.
            //
            // The unlock puzzle's solution also carries an `ephemeral_state` NodePtr threaded
            // forward from whichever action ran immediately before this one in the SAME
            // generation (`RewardDistributorPendingSpendInfo::new` seeds it to `NodePtr::NIL` for
            // a generation's first action; every action after that receives the prior action's
            // own full-puzzle output). This pre-screen does not replay prior actions' full
            // puzzles -- doing so would duplicate `from_spend` itself -- so it always supplies
            // `NodePtr::NIL` here. For an unstake that is its generation's first (or only) action
            // this is the exact correct value. For a later action, a wrong `ephemeral_state`
            // makes the unlock puzzle's own internal checks fail, which this pre-screen maps to a
            // refusal (`RewardsError::Driver`/`Malformed`) rather than a wrong `removed_shares` --
            // an over-refusal on a legitimate multi-action generation, the only direction this
            // reader may fail in, never an under-refusal.
            let unlock_puzzle = RewardDistributorUnstakeAction::unlock_puzzle(
                ctx,
                constants.launcher_id,
                constants.reward_distributor_type,
            )
            .map_err(RewardsError::from)?;
            let actual_unlock_solution = ctx
                .alloc(&clvm_tuple!(
                    NodePtr::NIL,
                    clvm_tuple!(
                        params.entry_slot.payout_puzzle_hash,
                        params.unlock_puzzle_solution
                    )
                ))
                .map_err(RewardsError::from)?;
            let unlock_puzzle_result = ctx
                .run(unlock_puzzle, actual_unlock_solution)
                .map_err(RewardsError::from)?;
            let removed_shares = ctx
                .extract::<(u64, NodePtr)>(unlock_puzzle_result)
                .map_err(RewardsError::from)?
                .0;

            params.entry_slot.shares.checked_sub(removed_shares).ok_or(
                RewardsError::ActionArithmeticNotRepresentable {
                    action: "unstake",
                    operation: "entry_slot.shares - removed_shares",
                },
            )?;
        } else if raw_action_hash == stake_hash {
            let params = ctx
                .extract::<chia_sdk_types::puzzles::RewardDistributorStakeActionSolution<NodePtr>>(
                    action_spend.solution,
                )
                .map_err(RewardsError::from)?;

            // `stake.rs:326`: `u64::try_from(existing_slot_counter + 1)` -- the `+ 1` is
            // unchecked `i128` arithmetic and panics at `i128::MAX` before `try_from` ever runs.
            params.existing_slot_counter.checked_add(1).ok_or(
                RewardsError::ActionArithmeticNotRepresentable {
                    action: "stake",
                    operation: "existing_slot_counter + 1",
                },
            )?;

            // `stake.rs:322,329`: `new_shares` is the output of running the stake action's own
            // lock puzzle against `lock_puzzle_solution` -- no static bound on it in the solution,
            // so closing `stake.rs:329` means running that puzzle ourselves, exactly as
            // `created_slot_value` does (same `Bytes32::default()`/`1` placeholders upstream
            // itself uses to build `lock_puzzle` here -- that construction only depends on
            // `distributor_type`, never on `launcher_id`/`max_second_offset`, for the numeric
            // output this screen re-derives).
            let lock_puzzle = RewardDistributorStakeAction::new_args(
                ctx,
                Bytes32::default(),
                1,
                constants.reward_distributor_type,
            )
            .map_err(RewardsError::from)?
            .lock_puzzle;
            let actual_lock_solution = ctx
                .alloc(&(
                    1,
                    (
                        params.entry_custody_puzzle_hash,
                        params.lock_puzzle_solution,
                    ),
                ))
                .map_err(RewardsError::from)?;
            let lock_puzzle_output = ctx
                .run(lock_puzzle, actual_lock_solution)
                .map_err(RewardsError::from)?;
            let (new_shares, _conditions): (u64, NodePtr) = ctx
                .extract(lock_puzzle_output)
                .map_err(RewardsError::from)?;

            params.existing_slot_shares.checked_add(new_shares).ok_or(
                RewardsError::ActionArithmeticNotRepresentable {
                    action: "stake",
                    operation: "existing_slot_shares + new_shares",
                },
            )?;
        } else if raw_action_hash == new_epoch_hash
            || raw_action_hash == add_entry_hash
            || raw_action_hash == remove_entry_hash
            || raw_action_hash == initiate_payout_hash
            || raw_action_hash == add_incentives_hash
            || raw_action_hash == sync_hash
            || refresh_hash == Some(raw_action_hash)
        {
            // Recognised, and no unchecked-arithmetic hazard reachable via `get_log` for this
            // action (rule 2 satisfied; nothing to screen further).
        } else {
            return Err(RewardsError::UnrecognisedActionPuzzle {
                action_puzzle_hash: raw_action_hash.into(),
            });
        }
    }

    Ok(())
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
///
/// # Two pre-guards against #3286, ahead of the withdraw path this walk can reconstruct
///
/// `chia-sdk-driver` 0.36.0's `WithdrawIncentives` action re-derives the withdrawal share with a
/// plain `u64` multiply (`withdraw_incentives.rs:71,89`) and this walk calls into that code
/// (`RewardDistributor::from_spend`, step 7) for every generation, so an unrepresentable share
/// would panic (checked arithmetic) or silently corrupt `RewardDistributorRewardSlotValue::rewards`
/// (release) with no chance for this function to intervene afterwards -- a post-hoc check cannot
/// run inside a call that never returns. Both guards therefore run BEFORE the call they protect:
///
/// - **B1** (constants domain): refuses if the distributor's own `withdrawal_share_bps` is outside
///   `0..=10_000`, immediately after `from_launcher_solution`.
/// - **B2** (scale domain): refuses, as soon as the generation that CREATES a commitment slot is
///   reconstructed, if that slot's own recorded `rewards` exceeds
///   [`crate::MAX_REPORTABLE_COMMITMENT_BASE_UNITS`] (`u64::MAX / 10_000`).
///
/// Together they keep a wrong figure from ever being RETURNED to a caller of this walk, because:
///
/// 1. **B2 bounds the quantity itself, not a proxy for it.** An earlier version of this guard
///    bounded a high-water mark of the reserve COIN's amount instead, on the premise that a
///    commitment's value is always reflected in the reserve by the time it could be withdrawn.
///    That premise is false: `chia-sdk-driver` 0.36.0's action layer can batch a `CommitIncentives`
///    together with any number of other reserve-affecting actions into ONE distributor-coin spend
///    (`reward_distributor.rs:699-747`), so the reserve coin's amount after a generation reflects
///    only that generation's NET effect -- a large commitment netted against a same-generation
///    outflow is never visible to a reader that only ever samples the reserve amount at generation
///    boundaries. `CommitIncentives::get_log` performs no multiply at all, so the generation that
///    creates a commitment slot parses safely at any scale and hands this walk the slot's `rewards`
///    directly, with no risk of the panic B2 exists to avoid.
/// 2. **B2 checks at the CREATING generation, so batching cannot hide a commitment from it.**
///    B2 reads `created_commitment_slots[].rewards` as soon as a generation is reconstructed, on
///    the RETURN path of `from_spend` and before this walk moves on, so it is indifferent to how
///    many other actions share that generation. `tests/simulator.rs`'s
///    `a_commitment_above_the_bound_batched_with_another_action_is_refused_by_the_reader` proves
///    exactly that and nothing weaker: its generation carries the over-bound commit together with
///    an entry-set write, spends no slot it also creates, and with B2 commented out the reader
///    returns `Ok(Some(..))` carrying the unrepresentable commitment.
///
///    An earlier version of this doc-comment claimed a slot could never be created and withdrawn
///    in the same distributor spend (reasoning from `assert_concurrent_puzzle`,
///    `withdraw_incentives.rs:120`, requiring the commitment slot to already exist as a spendable
///    coin). That claim is false: the composition IS constructible through this crate's own public
///    API and IS accepted by the simulator --
///    `a_same_generation_commit_and_withdraw_above_the_driver_bound_is_refused_before_from_spend`
///    builds and submits one. This reader refuses it either way, but note WHICH check does the
///    refusing: `DistributorSlots::apply_generation` removes a generation's spent slots before
///    extending with its created ones, so a create-then-spend of one commitment slot inside one
///    generation is refused as `Malformed` by slot bookkeeping alone, at any magnitude. That
///    backstop is orthogonal to B2's bound, which is why no test asserts B2's value on that
///    shape -- a test in that shape could only ever discriminate which error came back.
///
/// **Closed for the reachable sites (DIG-Network/dig_ecosystem#3313):** B2 only runs once
/// `from_spend` RETURNS. `chia-sdk-driver` 0.36.0's withdraw action re-derives the share with a
/// plain, unchecked `u64` multiply (`withdraw_incentives.rs:71`) that runs INSIDE `from_spend`,
/// ahead of B2. For a one-spend commit+withdraw composition whose `committed_value` exceeds the
/// driver's OWN overflow bound (`u64::MAX / withdrawal_share_bps` -- a narrower bound than
/// [`crate::MAX_REPORTABLE_COMMITMENT_BASE_UNITS`] whenever `withdrawal_share_bps > 10_000` is not
/// in play, and reachable well below it too for large `withdrawal_share_bps`), that multiply would
/// overflow before B2 ever runs. `refuse_unrepresentable_action_arithmetic` closes exactly this
/// gap: it screens every generation's action-layer solution BEFORE `from_spend` is called on it,
/// at the three sites `chia-sdk-driver` 0.36.0 computes from an action solution's own fields ahead
/// of B1/B2 (`withdraw_incentives.rs:71,89`, `commit_incentives.rs:85`), and refuses outright on an
/// action puzzle hash it does not recognise. It is a shim with an exit, not a permanent restatement
/// of upstream's arithmetic (see its own doc comment, and
/// <https://github.com/xch-dev/chia-wallet-sdk/issues/436> for the durable fix), and it makes no
/// claim about `commit_incentives.rs:82` or `new_epoch.rs:124` -- see that doc comment for why.
/// Both `dig-node` and `dig-relay` ship `overflow-checks = true` in their release profile, so before
/// this pre-screen existed, a generation at this scale was a live remote denial of service on
/// `read_distributor` in production, not merely in `cargo test`.
///
/// Batching cannot hide a commitment's value from B2 the way it could from a reserve-amount proxy:
/// the bound is checked against the slot bookkeeping this walk already reconstructs
/// (`DistributorSlots::apply_generation`'s `created_commitments`), never against the reserve
/// coin, so nothing about how many other actions share the generation changes what B2 sees.
///
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

    // B1 (constants domain): the same 0..=10_000 domain rule `recoverable_base_units` already
    // enforces, checked here against the distributor's OWN constants rather than DIG's -- this
    // reader is deliberately distributor-agnostic (see this function's own docs), so an equality
    // check against WITHDRAWAL_SHARE_BPS would wrongly refuse an honest self-skim variant. A
    // hostile or corrupt launcher reaches this from unauthenticated chain input, and refusing here
    // -- before B2, before any `from_spend` -- is the only chance to refuse at all: once inside
    // the walk, `chia-sdk-driver` 0.36.0's own share multiply (#3286) has no domain check of its
    // own.
    if constants.withdrawal_share_bps > 10_000 {
        return Err(RewardsError::UnreadableDistributorConstants {
            withdrawal_share_bps: constants.withdrawal_share_bps,
        });
    }

    // Same shape as B1 immediately above: `max_seconds_offset` is curried into `add_entry`,
    // `stake`, `unstake` and `refresh` as a clock-skew tolerance, and nothing in
    // `chia-sdk-driver` 0.36.0 or elsewhere in this crate bounds it. Left unchecked, it also
    // silently controlled `refuse_unrepresentable_action_arithmetic`'s old (now-removed)
    // backfill-iteration ratio -- an attacker's own `max_seconds_offset = u64::MAX` inflated that
    // ratio past any real bound (DIG_ecosystem#3313's security-leg finding).
    if constants.max_seconds_offset > MAX_SANE_SECONDS_OFFSET {
        return Err(RewardsError::UnreadableMaxSecondsOffset {
            max_seconds_offset: constants.max_seconds_offset,
        });
    }

    // The third constants-domain check, and the earliest one this reader can possibly make:
    // `epoch_seconds` is curried into the action puzzles at LAUNCH, never supplied per spend, so
    // it is knowable here -- before one generation is parsed -- rather than only per action once
    // the walk is already running. At zero, upstream's backfill loop
    // (`commit_incentives.rs:101-111`) never advances and never terminates; it is a pure,
    // non-yielding CPU loop, so a consumer's own `tokio::time::timeout` cannot cancel it either.
    // A refusal is the only defence that works, which is why it runs as early as possible rather
    // than at the generation that happens to carry the offending action. See
    // `RewardsError::UnreadableEpochSeconds`.
    if constants.epoch_seconds == 0 {
        return Err(RewardsError::UnreadableEpochSeconds);
    }

    let Some(eve_spend) = source
        .coin_spend(eve_coin.coin_id())
        .map_err(chain_unavailable)?
    else {
        return Err(malformed("eve coin is recorded but has no recorded spend"));
    };

    let (reserve_parent_id, reserve_lineage_proof) =
        find_eve_reserve_provenance(&mut ctx, source, &constants)?;

    let Some((mut distributor, launch_reward_slot)) = RewardDistributor::from_eve_coin_spend(
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

    // The launch creates the first distributor epoch's reward slot, and no later generation
    // reports it as created -- so a walk that starts from the eve spend must seed it here or a
    // chain-rebuilt prover cannot roll the first epoch (an in-process launcher gets the same
    // handle as `LaunchedDistributor::first_distributor_epoch_slot`).
    let mut slots = DistributorSlots {
        rewards: vec![launch_reward_slot.info.value],
        ..DistributorSlots::default()
    };
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

        // Fail-closed pre-screen (DIG-Network/dig_ecosystem#3313): a shim with an exit, not a
        // durable fix -- the durable fix is upstream
        // (https://github.com/xch-dev/chia-wallet-sdk/issues/436). Runs BEFORE `from_spend`
        // because three of `chia-sdk-driver` 0.36.0's action `get_log` methods do unchecked
        // `u64` arithmetic on solution fields inside that call, ahead of this crate's own B1/B2
        // guards, which only run once `from_spend` returns. See
        // `refuse_unrepresentable_action_arithmetic`'s own doc for the two fail-closed rules.
        refuse_unrepresentable_action_arithmetic(&mut ctx, &spend, constants)?;

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

        // B2 (scale domain), composed with B1 above: refuse as soon as a commitment slot's own
        // `rewards` is observed being created -- whether a withdraw of that same slot shares this
        // generation or waits for a later one (see this function's docs, point 2: a same-generation
        // commit+withdraw IS constructible, and this check still catches it because it reads
        // `created_commitment_slots` on the RETURN path, before this walk advances). Reading
        // `rewards` here risks no panic of its own, because `CommitIncentives::get_log` performs no
        // multiply -- but a same-generation WITHDRAW's own multiply can still run, and overflow,
        // BEFORE this check gets a chance to refuse, if `rewards` exceeds the driver's own bound
        // (`u64::MAX / withdrawal_share_bps`). That residual is open, not closed here: see this
        // function's docs and DIG-Network/dig_ecosystem#3313.
        for created_commitment in &reconstructed.pending_spend.created_commitment_slots {
            if created_commitment.rewards > crate::MAX_REPORTABLE_COMMITMENT_BASE_UNITS {
                return Err(RewardsError::CommitmentRewardsTooLargeToRead {
                    rewards_base_units: created_commitment.rewards,
                    max_readable_base_units: crate::MAX_REPORTABLE_COMMITMENT_BASE_UNITS,
                });
            }
        }

        // §12.4 is a signal a counterparty judges the OPERATOR by, so it must count only actions
        // the operator can take. `InitiatePayout` spends an entry slot and re-creates it
        // (upstream `action_log.rs:140,171`), so deriving this from created/spent entry slots let
        // one holder claiming every 47 h report a distributor healthy forever while its prover was
        // dead. The action log distinguishes them; nothing else in the pending spend does.
        let entry_write_happened = reconstructed.pending_spend.logs.iter().any(|log| {
            matches!(
                log,
                RewardDistributorActionLog::AddEntry(_)
                    | RewardDistributorActionLog::RemoveEntry(_)
            )
        });

        slots.apply_generation(
            &reconstructed.pending_spend.spent_entry_slots,
            &reconstructed.pending_spend.created_entry_slots,
            &reconstructed.pending_spend.spent_commitment_slots,
            &reconstructed.pending_spend.created_commitment_slots,
            &reconstructed.pending_spend.spent_reward_slots,
            &reconstructed.pending_spend.created_reward_slots,
        )?;

        let next_state = reconstructed.pending_spend.latest_state.1;
        distributor = reconstructed.child(next_state);

        if entry_write_happened {
            let spent_height = source
                .coin_record(spend.coin.coin_id())
                .map_err(chain_unavailable)?
                .and_then(|record| record.spent_height);
            // Refuse rather than silently drop the write. BOTH halves of the read above can be
            // absent from a source that is not lying: `coin_record` answers `Ok(None)` for a
            // pruned generation, and `spent_height` is documented as the height "if it has been
            // spent and the source knows it" (`dig-chainsource-interface` 0.3.3,
            // `record.rs:17`). Either collapses here, and leaving `None` would assert the
            // positive fact `ChainObservation::last_entry_write_unix` documents ("no entry-set
            // write has ever happened") about a write this walk just observed. It is also
            // self-inconsistent source data: we are only in this branch because `coin_spend`
            // returned this generation's spend, so the coin is known-spent -- the same
            // contradiction step 8 above already refuses.
            let height = spent_height.ok_or_else(|| {
                malformed("an observed entry-set write's generation has no resolvable spent height")
            })?;
            // Refuse rather than fall back. `block_timestamp`'s contract makes `Ok(None)`
            // mean "no such block OR no timestamp index", which a pruning RPC answers for an
            // old height while answering the peak fine -- so this is reachable in production.
            // Keeping the previous value would report an OLDER write as the most recent one,
            // and leaving `None` would assert the positive fact `ChainObservation::
            // last_entry_write_unix` documents ("no entry-set write has ever happened") about
            // a write this walk just observed. Both silently drop it; the peak timestamp
            // below already takes this same posture.
            last_entry_write_unix = Some(
                source
                    .block_timestamp(height)
                    .map_err(chain_unavailable)?
                    .ok_or_else(|| {
                        malformed("an observed entry-set write has no resolvable chain timestamp")
                    })?,
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A non-zero identity, so a test asserting `false` is asserting something.
    fn some_identity() -> Bytes32 {
        Bytes32::new([7; 32])
    }

    #[test]
    fn a_slot_spent_without_a_matching_creation_is_a_diverged_read() {
        let mut slots = DistributorSlots::default();
        let never_created = RewardDistributorRewardSlotValue {
            counter: 0,
            epoch_start: 1_234,
            next_epoch_initialized: false,
            rewards: 5,
        };

        let result = slots.apply_generation(&[], &[], &[], &[], &[never_created], &[]);

        match result {
            Err(RewardsError::Malformed(message)) => assert!(
                message.contains("never saw created"),
                "wrong reason: {message}"
            ),
            other => panic!("a spend the walk cannot account for must be an error, got {other:?}"),
        }
    }

    #[test]
    fn a_slot_spent_after_it_was_created_is_removed_exactly_once() {
        let value = RewardDistributorRewardSlotValue {
            counter: 0,
            epoch_start: 1_234,
            next_epoch_initialized: false,
            rewards: 5,
        };
        let mut slots = DistributorSlots::default();

        slots
            .apply_generation(&[], &[], &[], &[], &[], &[value, value])
            .expect("creations alone never diverge");
        slots
            .apply_generation(&[], &[], &[], &[], &[value], &[])
            .expect("one of the two outstanding copies is spent");

        assert_eq!(
            slots.rewards,
            vec![value],
            "two outstanding slots can be equal by value; spending one must not remove both"
        );
    }

    #[test]
    fn a_zero_reserve_is_never_stale_and_a_funded_never_written_set_always_is() {
        assert!(
            !entry_set_is_stale(0, u64::MAX, None),
            "nothing is at stake, so nothing can go stale"
        );
        assert!(
            entry_set_is_stale(1, 0, None),
            "a funded distributor whose entry set has never been written is the §12.4 case"
        );
    }

    #[test]
    fn staleness_turns_over_exactly_at_the_threshold_in_both_directions() {
        let last_write = 1_000_000;

        assert!(
            !entry_set_is_stale(
                1,
                last_write + STALE_ENTRY_SET_SECONDS - 1,
                Some(last_write)
            ),
            "one second short of the threshold is not stale"
        );
        assert!(
            entry_set_is_stale(1, last_write + STALE_ENTRY_SET_SECONDS, Some(last_write)),
            "the threshold itself is stale (§12.4 says `>=`)"
        );
    }

    #[test]
    fn a_zero_identity_freezes_the_entry_set_for_every_distributor_type() {
        let zero = Bytes32::default();

        assert!(entry_set_is_frozen(RewardDistributorType::Managed {
            manager_singleton_launcher_id: zero
        }));
        assert!(entry_set_is_frozen(RewardDistributorType::NftCollection {
            collection_did_launcher_id: zero
        }));
        assert!(entry_set_is_frozen(RewardDistributorType::CuratedNft {
            store_launcher_id: zero,
            refreshable: true
        }));
        assert!(entry_set_is_frozen(RewardDistributorType::Cat {
            asset_id: zero,
            hidden_puzzle_hash: None
        }));
    }

    #[test]
    fn a_real_identity_leaves_the_entry_set_writable_for_every_distributor_type() {
        // The inversion of the test above: flip the `==` in `entry_set_is_frozen` and one of these
        // four fails, which is what makes the guard's direction tested rather than merely present.
        assert!(!entry_set_is_frozen(RewardDistributorType::Managed {
            manager_singleton_launcher_id: some_identity()
        }));
        assert!(!entry_set_is_frozen(RewardDistributorType::NftCollection {
            collection_did_launcher_id: some_identity()
        }));
        assert!(!entry_set_is_frozen(RewardDistributorType::CuratedNft {
            store_launcher_id: some_identity(),
            refreshable: false
        }));
        assert!(!entry_set_is_frozen(RewardDistributorType::Cat {
            asset_id: some_identity(),
            hidden_puzzle_hash: None
        }));
    }

    /// Fail-closed rule 2: an action-layer solution naming a puzzle this reader does not recognise
    /// as one of the eleven known reward-distributor actions must refuse the read, never be
    /// silently skipped -- exactly the case a future upstream pin bump that changes an action
    /// puzzle would produce.
    ///
    /// Built directly against `refuse_unrepresentable_action_arithmetic`, at the unit level: a
    /// real, valid `ActionLayerSolution` (via `RawActionLayerSolution`, the same shape
    /// `ActionLayer::parse_solution` decodes) whose one action puzzle is an ordinary CLVM atom --
    /// guaranteed to match none of the eleven curried action puzzle hashes this pre-screen
    /// computes from `constants`, since none of those eleven is ever a bare atom.
    #[test]
    fn an_unrecognised_action_puzzle_hash_is_refused() {
        let ctx = &mut SpendContext::new();

        let constants = RewardDistributorConstants::without_launcher_id(
            RewardDistributorType::Managed {
                manager_singleton_launcher_id: some_identity(),
            },
            some_identity(),
            1_000,
            10_000,
            1_000,
            0,
            false,
            0,
            9_000,
            some_identity(),
        )
        .with_launcher_id(some_identity());

        // An ordinary atom: not a curried puzzle, so it cannot possibly equal any of the eleven
        // curried action-puzzle hashes the pre-screen enumerates from `constants`.
        let unknown_action_puzzle = ctx.alloc(&42u64).expect("an atom always allocates");
        let unknown_action_solution = ctx.alloc(&()).expect("the empty solution always allocates");

        let raw_action_layer_solution = chia_sdk_types::puzzles::RawActionLayerSolution {
            puzzles: vec![unknown_action_puzzle],
            selectors_and_proofs: vec![(2, Some(chia_sdk_types::MerkleProof::new(0, vec![])))],
            solutions: vec![unknown_action_solution],
            finalizer_solution: ctx
                .alloc(&())
                .expect("the empty finalizer solution allocates"),
        };

        let singleton_solution = SingletonSolution {
            lineage_proof: chia_puzzle_types::Proof::Eve(chia_puzzle_types::EveProof {
                parent_parent_coin_info: some_identity(),
                parent_amount: 1,
            }),
            amount: 1,
            inner_solution: raw_action_layer_solution,
        };

        let solution = ctx
            .serialize(&singleton_solution)
            .expect("a well-formed singleton solution always serializes");

        let spend = CoinSpend::new(
            chia_protocol::Coin::new(some_identity(), some_identity(), 1),
            solution.clone(),
            solution,
        );

        match refuse_unrepresentable_action_arithmetic(ctx, &spend, constants) {
            Err(RewardsError::UnrecognisedActionPuzzle { action_puzzle_hash }) => {
                assert_eq!(
                    action_puzzle_hash,
                    ctx.tree_hash(unknown_action_puzzle).into(),
                    "the refusal must name the actual unrecognised hash, not a placeholder"
                );
            }
            Ok(()) => panic!(
                "rule 2 must refuse an action puzzle hash outside the eleven known actions, not \
                 silently skip it"
            ),
            Err(other) => panic!("expected UnrecognisedActionPuzzle, got: {other}"),
        }
    }

    /// Site 2 of 3 (`withdraw_incentives.rs:89`): `reward_slot_total_rewards - withdrawal_share`
    /// underflows when a withdraw action's solution names a `reward_slot_total_rewards` smaller
    /// than the share its own `committed_value` computes -- a shape the multiply guard above
    /// (site 1) does not catch, since the multiply itself stays in range here.
    #[test]
    fn a_withdraw_subtract_that_would_underflow_is_refused() {
        let ctx = &mut SpendContext::new();
        let launcher_id = some_identity();
        let withdrawal_share_bps = 9_000;

        let constants = RewardDistributorConstants::without_launcher_id(
            RewardDistributorType::Managed {
                manager_singleton_launcher_id: some_identity(),
            },
            some_identity(),
            1_000,
            10_000,
            1_000,
            0,
            false,
            0,
            withdrawal_share_bps,
            some_identity(),
        )
        .with_launcher_id(launcher_id);

        let committed_value = 1_000u64;
        // `withdrawal_share = committed_value * bps / 10_000` = 900 here -- larger than the
        // slot's own recorded total, so the subtract underflows even though the multiply above it
        // stayed in range.
        let reward_slot_total_rewards = 1u64;

        let action_puzzle = ctx
            .curry(
                chia_sdk_driver::RewardDistributorWithdrawIncentivesAction::new_args(
                    launcher_id,
                    withdrawal_share_bps,
                ),
            )
            .expect("withdraw args always curry");
        let action_solution = ctx
            .alloc(
                &chia_sdk_types::puzzles::RewardDistributorWithdrawIncentivesActionSolution {
                    reward_slot_counter: 0,
                    reward_slot_epoch_time: 0,
                    clawback_ph: some_identity(),
                    committed_value,
                    reward_slot_total_rewards,
                    reward_slot_next_epoch_initialized: false,
                },
            )
            .expect("a well-formed withdraw solution always allocates");

        let raw_action_layer_solution = chia_sdk_types::puzzles::RawActionLayerSolution {
            puzzles: vec![action_puzzle],
            selectors_and_proofs: vec![(2, Some(chia_sdk_types::MerkleProof::new(0, vec![])))],
            solutions: vec![action_solution],
            finalizer_solution: ctx
                .alloc(&())
                .expect("the empty finalizer solution allocates"),
        };
        let singleton_solution = SingletonSolution {
            lineage_proof: chia_puzzle_types::Proof::Eve(chia_puzzle_types::EveProof {
                parent_parent_coin_info: some_identity(),
                parent_amount: 1,
            }),
            amount: 1,
            inner_solution: raw_action_layer_solution,
        };
        let solution = ctx
            .serialize(&singleton_solution)
            .expect("a well-formed singleton solution always serializes");
        let spend = CoinSpend::new(
            chia_protocol::Coin::new(some_identity(), some_identity(), 1),
            solution.clone(),
            solution,
        );

        match refuse_unrepresentable_action_arithmetic(ctx, &spend, constants) {
            Err(RewardsError::ActionArithmeticNotRepresentable { action, operation }) => {
                assert_eq!(action, "withdraw_incentives");
                assert_eq!(operation, "reward_slot_total_rewards - withdrawal_share");
            }
            Ok(()) => {
                panic!("the pre-screen let a withdraw solution through whose subtract underflows")
            }
            Err(other) => panic!("expected ActionArithmeticNotRepresentable, got: {other}"),
        }
    }

    /// Site 3 of 3 (`commit_incentives.rs:85`): `slot_total_rewards + rewards_to_add` overflows
    /// when `slot_epoch_time == epoch_start` (the branch upstream takes when the commitment adds
    /// to an already-initialized slot for the SAME epoch it targets) -- never screened when the
    /// two differ, matching upstream's own guard.
    #[test]
    fn a_commit_add_that_would_overflow_is_refused() {
        let ctx = &mut SpendContext::new();
        let launcher_id = some_identity();
        let epoch_seconds = 1_000;

        let constants = RewardDistributorConstants::without_launcher_id(
            RewardDistributorType::Managed {
                manager_singleton_launcher_id: some_identity(),
            },
            some_identity(),
            epoch_seconds,
            10_000,
            1_000,
            0,
            false,
            0,
            9_000,
            some_identity(),
        )
        .with_launcher_id(launcher_id);

        let epoch_start = 5_000u64;

        let action_puzzle = ctx
            .curry(
                chia_sdk_driver::RewardDistributorCommitIncentivesAction::new_args(
                    launcher_id,
                    epoch_seconds,
                ),
            )
            .expect("commit args always curry");
        let action_solution = ctx
            .alloc(
                &chia_sdk_types::puzzles::RewardDistributorCommitIncentivesActionSolution {
                    slot_counter: 0,
                    slot_epoch_time: epoch_start,
                    slot_next_epoch_initialized: false,
                    slot_total_rewards: u64::MAX,
                    epoch_start,
                    clawback_ph: some_identity(),
                    rewards_to_add: 1,
                },
            )
            .expect("a well-formed commit solution always allocates");

        let raw_action_layer_solution = chia_sdk_types::puzzles::RawActionLayerSolution {
            puzzles: vec![action_puzzle],
            selectors_and_proofs: vec![(2, Some(chia_sdk_types::MerkleProof::new(0, vec![])))],
            solutions: vec![action_solution],
            finalizer_solution: ctx
                .alloc(&())
                .expect("the empty finalizer solution allocates"),
        };
        let singleton_solution = SingletonSolution {
            lineage_proof: chia_puzzle_types::Proof::Eve(chia_puzzle_types::EveProof {
                parent_parent_coin_info: some_identity(),
                parent_amount: 1,
            }),
            amount: 1,
            inner_solution: raw_action_layer_solution,
        };
        let solution = ctx
            .serialize(&singleton_solution)
            .expect("a well-formed singleton solution always serializes");
        let spend = CoinSpend::new(
            chia_protocol::Coin::new(some_identity(), some_identity(), 1),
            solution.clone(),
            solution,
        );

        match refuse_unrepresentable_action_arithmetic(ctx, &spend, constants) {
            Err(RewardsError::ActionArithmeticNotRepresentable { action, operation }) => {
                assert_eq!(action, "commit_incentives");
                assert_eq!(operation, "slot_total_rewards + rewards_to_add");
            }
            Ok(()) => panic!("the pre-screen let a commit solution through whose add overflows"),
            Err(other) => panic!("expected ActionArithmeticNotRepresentable, got: {other}"),
        }
    }

    /// Wraps one action puzzle+solution pair in the same `SingletonSolution` /
    /// `RawActionLayerSolution` scaffolding every pre-screen test above builds by hand, so the
    /// four hazard tests below don't have to repeat it a fourth and fifth time.
    fn single_action_spend(
        ctx: &mut SpendContext,
        action_puzzle: NodePtr,
        action_solution: NodePtr,
    ) -> CoinSpend {
        let raw_action_layer_solution = chia_sdk_types::puzzles::RawActionLayerSolution {
            puzzles: vec![action_puzzle],
            selectors_and_proofs: vec![(2, Some(chia_sdk_types::MerkleProof::new(0, vec![])))],
            solutions: vec![action_solution],
            finalizer_solution: ctx
                .alloc(&())
                .expect("the empty finalizer solution allocates"),
        };
        let singleton_solution = SingletonSolution {
            lineage_proof: chia_puzzle_types::Proof::Eve(chia_puzzle_types::EveProof {
                parent_parent_coin_info: some_identity(),
                parent_amount: 1,
            }),
            amount: 1,
            inner_solution: raw_action_layer_solution,
        };
        let solution = ctx
            .serialize(&singleton_solution)
            .expect("a well-formed singleton solution always serializes");
        CoinSpend::new(
            chia_protocol::Coin::new(some_identity(), some_identity(), 1),
            solution.clone(),
            solution,
        )
    }

    /// `commit_incentives.rs:101`'s non-adjacent-epoch branch never terminates its backfill loop
    /// if the distributor's own `epoch_seconds` constant is zero, since the loop steps by
    /// `constants.epoch_seconds` -- refusing outright is the only sound response, not a checked
    /// add on a step of zero (which would still loop forever, just without overflowing).
    #[test]
    fn a_commit_incentives_epoch_seconds_of_zero_is_refused_before_it_can_hang() {
        let ctx = &mut SpendContext::new();
        let launcher_id = some_identity();
        // The hazard: a distributor launched with `epoch_seconds == 0`. `epoch_seconds` is
        // curried into the action puzzle at launch (never a solution field), so an attacker who
        // launches their own distributor can pick it freely.
        let epoch_seconds = 0u64;

        let constants = RewardDistributorConstants::without_launcher_id(
            RewardDistributorType::Managed {
                manager_singleton_launcher_id: some_identity(),
            },
            some_identity(),
            epoch_seconds,
            10_000,
            1_000,
            0,
            false,
            0,
            9_000,
            some_identity(),
        )
        .with_launcher_id(launcher_id);

        let action_puzzle = ctx
            .curry(
                chia_sdk_driver::RewardDistributorCommitIncentivesAction::new_args(
                    launcher_id,
                    epoch_seconds,
                ),
            )
            .expect("commit args always curry even at epoch_seconds == 0");
        let action_solution = ctx
            .alloc(
                &chia_sdk_types::puzzles::RewardDistributorCommitIncentivesActionSolution {
                    slot_counter: 0,
                    // Not equal to `epoch_start`, so the non-adjacent-epoch (backfill) branch is
                    // the one this pre-screen must refuse in.
                    slot_epoch_time: 0,
                    slot_next_epoch_initialized: false,
                    slot_total_rewards: 0,
                    epoch_start: 5_000,
                    clawback_ph: some_identity(),
                    rewards_to_add: 1,
                },
            )
            .expect("a well-formed commit solution always allocates");

        let spend = single_action_spend(ctx, action_puzzle, action_solution);

        match refuse_unrepresentable_action_arithmetic(ctx, &spend, constants) {
            Err(RewardsError::UnreadableEpochSeconds) => {}
            Ok(()) => panic!(
                "the pre-screen let a commit_incentives backfill through with epoch_seconds == 0, \
                 which is a non-terminating loop upstream, not merely an overflow"
            ),
            Err(other) => panic!("expected UnreadableEpochSeconds, got: {other}"),
        }
    }

    /// `commit_incentives.rs:103-112`'s backfill loop is bounded here by a FIXED, named cap
    /// (`MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS`), never a ratio of the distributor's own declared
    /// constants (see `refuse_unrepresentable_action_arithmetic`'s comment on why the old ratio
    /// failed in both directions) and never a bare decimal literal -- a solution whose
    /// `epoch_start` is far enough past `slot_epoch_time` to need more iterations than that fixed
    /// cap must be refused before the (unbounded, from this reader's point of view) loop runs.
    #[test]
    fn a_commit_incentives_backfill_past_the_fixed_cap_is_refused() {
        let ctx = &mut SpendContext::new();
        let launcher_id = some_identity();
        // `epoch_seconds = 1` is not itself hostile (the domain check on `max_seconds_offset`
        // guards the attacker-controlled shape); it is chosen here only to make the backfill span
        // needed to exceed a MILLION-slot fixed cap reachable with small solution fields.
        let epoch_seconds = 1u64;
        let max_seconds_offset = 300u64;

        let constants = RewardDistributorConstants::without_launcher_id(
            RewardDistributorType::Managed {
                manager_singleton_launcher_id: some_identity(),
            },
            some_identity(),
            epoch_seconds,
            10_000,
            max_seconds_offset,
            0,
            false,
            0,
            9_000,
            some_identity(),
        )
        .with_launcher_id(launcher_id);

        let slot_epoch_time = 0u64;
        // `start_epoch_time = slot_epoch_time + epoch_seconds = 1`; picking `epoch_start` one past
        // `MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS` iterations out puts the real iteration count one
        // above the fixed cap.
        let epoch_start = MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS + 2;

        let action_puzzle = ctx
            .curry(
                chia_sdk_driver::RewardDistributorCommitIncentivesAction::new_args(
                    launcher_id,
                    epoch_seconds,
                ),
            )
            .expect("commit args always curry");
        let action_solution = ctx
            .alloc(
                &chia_sdk_types::puzzles::RewardDistributorCommitIncentivesActionSolution {
                    slot_counter: 0,
                    slot_epoch_time,
                    slot_next_epoch_initialized: false,
                    slot_total_rewards: 0,
                    epoch_start,
                    clawback_ph: some_identity(),
                    rewards_to_add: 1,
                },
            )
            .expect("a well-formed commit solution always allocates");

        let spend = single_action_spend(ctx, action_puzzle, action_solution);

        match refuse_unrepresentable_action_arithmetic(ctx, &spend, constants) {
            Err(RewardsError::CommitIncentivesBackfillBoundExceeded {
                iterations,
                max_backfill_slots,
            }) => {
                assert_eq!(
                    max_backfill_slots, MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS,
                    "the cap must be the fixed, named constant, not a re-derived value"
                );
                assert_eq!(
                    iterations,
                    MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS + 1,
                    "the refusal must name the real iteration count"
                );
            }
            Ok(()) => panic!(
                "the pre-screen let a commit_incentives backfill through past its own fixed cap"
            ),
            Err(other) => panic!("expected CommitIncentivesBackfillBoundExceeded, got: {other}"),
        }
    }

    /// The revised cap must not merely be uncrossable -- it must still ADMIT the honest case it
    /// exists to serve: DIG's own real launch constants (`epoch_seconds = 604_800`,
    /// `max_seconds_offset = 300`) backfilling exactly two empty epochs. The retired ratio-based
    /// cap evaluated to `0` at these exact constants, refusing this every time; this is the red
    /// test that would have caught it.
    #[test]
    fn an_honest_two_epoch_backfill_succeeds_under_dig_distributor_constants() {
        let ctx = &mut SpendContext::new();
        let launcher_id = some_identity();
        let refund_hash = some_identity();
        // DIG's OWN published launch terms, not a test-shaped stand-in: the retired ratio cap
        // `max_seconds_offset / epoch_seconds` floors to zero at exactly these values
        // (`300 / 604_800`), so the honest case has to be built from the real constructor for the
        // test to be evidence about production.
        let terms = crate::constants::DistributorLaunchTerms {
            manager_singleton_launcher_id: some_identity(),
            distributor_epoch_seconds: crate::constants::DEFAULT_DISTRIBUTOR_EPOCH_SECONDS,
        };
        let constants = crate::constants::dig_distributor_constants(terms, refund_hash)
            .expect("DIG's own canonical constants must build")
            .with_launcher_id(launcher_id);

        let epoch_seconds = constants.epoch_seconds;
        let slot_epoch_time = 0u64;
        // Exactly two epochs past `slot_epoch_time + epoch_seconds`: an honest backfill of two
        // empty epochs, well inside `MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS`.
        let epoch_start = epoch_seconds
            .checked_mul(3)
            .expect("small multiple of DIG's own epoch_seconds must be representable");

        let action_puzzle = ctx
            .curry(
                chia_sdk_driver::RewardDistributorCommitIncentivesAction::new_args(
                    launcher_id,
                    epoch_seconds,
                ),
            )
            .expect("commit args always curry");
        let action_solution = ctx
            .alloc(
                &chia_sdk_types::puzzles::RewardDistributorCommitIncentivesActionSolution {
                    slot_counter: 0,
                    slot_epoch_time,
                    slot_next_epoch_initialized: false,
                    slot_total_rewards: 0,
                    epoch_start,
                    clawback_ph: some_identity(),
                    rewards_to_add: 1,
                },
            )
            .expect("a well-formed commit solution always allocates");

        let spend = single_action_spend(ctx, action_puzzle, action_solution);

        // `RewardsError` is deliberately not `PartialEq` (it carries a `String`), so the admission
        // is asserted by matching rather than comparing.
        if let Err(refusal) = refuse_unrepresentable_action_arithmetic(ctx, &spend, constants) {
            panic!(
                "an honest two-epoch backfill under DIG's own real constants must be admitted, \
                 not refused by a cap that evaluates to zero at those exact constants: {refusal}"
            );
        }
    }

    /// `unstake.rs:235`: `entry_slot.shares - removed_shares` underflows when a solution names an
    /// `entry_slot.shares` smaller than what the unstake action's own unlock puzzle actually
    /// returns for `removed_shares` -- a shape only reachable by RUNNING that puzzle (its output
    /// is not a static solution field), which is exactly what this test proves the pre-screen
    /// does: a genuine, successfully-run CAT unlock puzzle whose real `cat_shares` output (5)
    /// exceeds the solution's own claimed `entry_slot.shares` (1).
    #[test]
    fn an_unstake_subtract_that_would_underflow_is_refused() {
        let ctx = &mut SpendContext::new();
        let launcher_id = some_identity();
        let max_second_offset = 1_000u64;
        let precision = 1_000u64;
        let asset_id = some_identity();
        let distributor_type = RewardDistributorType::Cat {
            asset_id,
            hidden_puzzle_hash: None,
        };

        let constants = RewardDistributorConstants::without_launcher_id(
            distributor_type,
            some_identity(),
            1_000,
            precision,
            max_second_offset,
            0,
            false,
            0,
            9_000,
            some_identity(),
        )
        .with_launcher_id(launcher_id);

        let entry_slot = chia_sdk_types::puzzles::RewardDistributorEntrySlotValue {
            counter: 0,
            payout_puzzle_hash: some_identity(),
            initial_cumulative_payout: 0,
            // Smaller than the unlock puzzle's real output below -- the underflow.
            shares: 1,
        };
        let removed_shares = 5u64;

        let action_solution_value =
            chia_sdk_types::puzzles::RewardDistributorUnstakeActionSolution {
                entry_slot,
                entry_payout_info: chia_sdk_types::puzzles::RewardDistributorEntryPayoutInfo {
                    payout_amount: 0,
                    payout_rounding_error: 0,
                },
                unlock_puzzle_solution:
                    chia_sdk_types::puzzles::RewardDistributorCatUnlockingPuzzleSolution {
                        cat_parent_id: some_identity(),
                        cat_amount: removed_shares,
                        cat_shares: removed_shares,
                        cat_maker_solution_rest: (),
                    },
            };

        let unstake_args = chia_sdk_driver::RewardDistributorUnstakeAction::new_args(
            ctx,
            launcher_id,
            max_second_offset,
            precision,
            distributor_type,
        )
        .expect("unstake args always build for a Cat distributor");
        let action_puzzle = ctx.curry(unstake_args).expect("unstake args always curry");
        let action_solution = ctx
            .alloc(&action_solution_value)
            .expect("a well-formed unstake solution always allocates");

        let spend = single_action_spend(ctx, action_puzzle, action_solution);

        match refuse_unrepresentable_action_arithmetic(ctx, &spend, constants) {
            Err(RewardsError::ActionArithmeticNotRepresentable { action, operation }) => {
                assert_eq!(action, "unstake");
                assert_eq!(operation, "entry_slot.shares - removed_shares");
            }
            Ok(()) => panic!(
                "the pre-screen let an unstake solution through whose subtract underflows, \
                 after actually running its unlock puzzle"
            ),
            Err(other) => panic!("expected ActionArithmeticNotRepresentable, got: {other}"),
        }
    }

    /// `stake.rs:329`: `existing_slot_shares + new_shares` overflows when `new_shares` -- the
    /// real output of running the stake action's own lock puzzle -- pushes the sum past
    /// `u64::MAX`, a shape only reachable by running that puzzle, exactly as production's own
    /// `created_slot_value` does.
    #[test]
    fn a_stake_add_that_would_overflow_is_refused() {
        let ctx = &mut SpendContext::new();
        let launcher_id = some_identity();
        let max_second_offset = 1_000u64;
        let asset_id = some_identity();
        let distributor_type = RewardDistributorType::Cat {
            asset_id,
            hidden_puzzle_hash: None,
        };

        let constants = RewardDistributorConstants::without_launcher_id(
            distributor_type,
            some_identity(),
            1_000,
            1_000,
            max_second_offset,
            0,
            false,
            0,
            9_000,
            some_identity(),
        )
        .with_launcher_id(launcher_id);

        let new_shares = 5u64;
        let action_solution_value = chia_sdk_types::puzzles::RewardDistributorStakeActionSolution {
            lock_puzzle_solution:
                chia_sdk_types::puzzles::RewardDistributorCatLockingPuzzleSolution {
                    my_id: some_identity(),
                    cat_amount: new_shares,
                    cat_maker_solution_rest: (),
                },
            existing_slot_counter: 0,
            entry_custody_puzzle_hash: some_identity(),
            existing_slot_cumulative_payout: 0,
            // `u64::MAX - (new_shares - 1)`, so `+ new_shares` overflows by exactly one.
            existing_slot_shares: u64::MAX - (new_shares - 1),
        };

        let stake_args = chia_sdk_driver::RewardDistributorStakeAction::new_args(
            ctx,
            launcher_id,
            max_second_offset,
            distributor_type,
        )
        .expect("stake args always build for a Cat distributor");
        let action_puzzle = ctx.curry(stake_args).expect("stake args always curry");
        let action_solution = ctx
            .alloc(&action_solution_value)
            .expect("a well-formed stake solution always allocates");

        let spend = single_action_spend(ctx, action_puzzle, action_solution);

        match refuse_unrepresentable_action_arithmetic(ctx, &spend, constants) {
            Err(RewardsError::ActionArithmeticNotRepresentable { action, operation }) => {
                assert_eq!(action, "stake");
                assert_eq!(operation, "existing_slot_shares + new_shares");
            }
            Ok(()) => panic!(
                "the pre-screen let a stake solution through whose add overflows, after actually \
                 running its lock puzzle"
            ),
            Err(other) => panic!("expected ActionArithmeticNotRepresentable, got: {other}"),
        }
    }

    /// `stake.rs:326`: `u64::try_from(existing_slot_counter + 1)` where `existing_slot_counter`
    /// is `i128` -- the `+ 1` is unchecked `i128` arithmetic and panics at `i128::MAX` before
    /// `try_from` ever runs, independent of the `existing_slot_shares + new_shares` site above.
    #[test]
    fn a_stake_counter_at_i128_max_is_refused() {
        let ctx = &mut SpendContext::new();
        let launcher_id = some_identity();
        let max_second_offset = 1_000u64;
        let asset_id = some_identity();
        let distributor_type = RewardDistributorType::Cat {
            asset_id,
            hidden_puzzle_hash: None,
        };

        let constants = RewardDistributorConstants::without_launcher_id(
            distributor_type,
            some_identity(),
            1_000,
            1_000,
            max_second_offset,
            0,
            false,
            0,
            9_000,
            some_identity(),
        )
        .with_launcher_id(launcher_id);

        let action_solution_value = chia_sdk_types::puzzles::RewardDistributorStakeActionSolution {
            lock_puzzle_solution:
                chia_sdk_types::puzzles::RewardDistributorCatLockingPuzzleSolution {
                    my_id: some_identity(),
                    cat_amount: 1,
                    cat_maker_solution_rest: (),
                },
            existing_slot_counter: i128::MAX,
            entry_custody_puzzle_hash: some_identity(),
            existing_slot_cumulative_payout: 0,
            existing_slot_shares: 0,
        };

        let stake_args = chia_sdk_driver::RewardDistributorStakeAction::new_args(
            ctx,
            launcher_id,
            max_second_offset,
            distributor_type,
        )
        .expect("stake args always build for a Cat distributor");
        let action_puzzle = ctx.curry(stake_args).expect("stake args always curry");
        let action_solution = ctx
            .alloc(&action_solution_value)
            .expect("a well-formed stake solution always allocates");

        let spend = single_action_spend(ctx, action_puzzle, action_solution);

        match refuse_unrepresentable_action_arithmetic(ctx, &spend, constants) {
            Err(RewardsError::ActionArithmeticNotRepresentable { action, operation }) => {
                assert_eq!(action, "stake");
                assert_eq!(operation, "existing_slot_counter + 1");
            }
            Ok(()) => panic!(
                "the pre-screen let a stake solution through whose counter increment panics at \
                 i128::MAX"
            ),
            Err(other) => panic!("expected ActionArithmeticNotRepresentable, got: {other}"),
        }
    }
}
