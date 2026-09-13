//! `Sync` and `NewEpoch` — **permissionless** (`SPEC.md` §12.5, §8).
//!
//! Neither builder here takes a [`crate::entries::ManagerAuthority`], and that absence is the
//! point: anyone may advance a distributor's clock and anyone may roll it into its next epoch. A
//! distributor whose operator has gone away still works, which is what makes the mirrors'
//! entitlement independent of the funder's liveness.
//!
//! Neither builder here restates any arithmetic of its OWN -- both surface a figure the upstream
//! driver already computed (§0.1 clause 1) rather than a second, independent computation of this
//! crate's own. That is not the same claim as "never recomputed": `NewEpoch`'s
//! `epoch_total_rewards * fee_bps / 10000` (`new_epoch.rs:124`) is itself a plain `u64` multiply
//! inside the driver, the same #3286-shaped hazard `withdraw_incentives.rs:105-107` has for a
//! withdraw's share. It is inert for every DIG distributor because `fee_bps` is `0` there, but
//! [`crate::state::read_distributor`] is deliberately distributor-agnostic and replays this same
//! multiply for `NewEpoch` actions in ANY launcher's history, including a hostile one with a
//! nonzero `fee_bps` -- tracked as a follow-up (dig-rewards-coin#10), out of this PR's scope,
//! which is about `WithdrawIncentives` specifically. `Sync` moves `cumulative_payout` and
//! `remaining_rewards` with no multiply of any kind (`sync.rs`).
//!
//! `Refresh` is deliberately **not exposed**. §13.3 measures it as the NFT/DataLayer refresh, which
//! has no meaning in `Managed` mode; offering it would be offering an action that cannot succeed.

use chia_sdk_driver::{
    RewardDistributor, RewardDistributorNewEpochAction, RewardDistributorSyncAction, Slot,
    SpendContext,
};
use chia_sdk_types::puzzles::RewardDistributorRewardSlotValue;
use chia_sdk_types::Conditions;

use crate::RewardsError;

/// A completed `NewEpoch`: the conditions to deliver, and the fee the puzzle skimmed.
#[derive(Debug)]
pub struct DistributorEpochRoll {
    /// Conditions some coin in the same bundle must assert.
    pub conditions: Conditions,

    /// The epoch fee the puzzle paid to `fee_payout_puzzle_hash`, in **$DIG base units**.
    ///
    /// Zero for a DIG distributor, because `fee_bps` is zero. Surfaced anyway: a caller reading a
    /// distributor it did not launch may find a non-zero one, and with the recipient set to the
    /// funder's own hash a non-zero fee is a funder self-skim taken off the mirrors, not an
    /// ecosystem fee (§7.3 clause 4).
    pub fee_base_units: u64,
}

/// Advance the distributor's clock to `update_time_unix_seconds`.
///
/// Permissionless: no authority argument, because none is required.
///
/// # Errors
///
/// [`RewardsError::Driver`] if the upstream action could not be built.
pub fn sync_distributor(
    ctx: &mut SpendContext,
    distributor: &mut RewardDistributor,
    update_time_unix_seconds: u64,
) -> Result<Conditions, RewardsError> {
    let conditions = distributor
        .new_action::<RewardDistributorSyncAction>()
        .spend(ctx, distributor, update_time_unix_seconds)?;

    Ok(conditions)
}

/// Roll the distributor into its next epoch, consuming that epoch's reward slot.
///
/// Permissionless: no authority argument, because none is required.
///
/// # Errors
///
/// [`RewardsError::Driver`] if the upstream action could not be built.
pub fn start_next_distributor_epoch(
    ctx: &mut SpendContext,
    distributor: &mut RewardDistributor,
    reward_slot: Slot<RewardDistributorRewardSlotValue>,
) -> Result<DistributorEpochRoll, RewardsError> {
    let (conditions, fee_base_units) = distributor
        .new_action::<RewardDistributorNewEpochAction>()
        .spend(ctx, distributor, reward_slot)?;

    Ok(DistributorEpochRoll {
        conditions,
        fee_base_units,
    })
}

/// When the distributor's current epoch ends, in Unix seconds.
#[must_use]
pub fn current_distributor_epoch_end(distributor: &RewardDistributor) -> u64 {
    distributor
        .pending_spend
        .latest_state
        .1
        .round_time_info
        .epoch_end
}

/// The moment the distributor's state was last advanced, in Unix seconds.
///
/// An entry-set write is only valid while this is within `max_seconds_offset` of the spend's
/// asserted time, which is why [`crate::entries`] emits a `Sync` in the same bundle.
#[must_use]
pub fn last_update(distributor: &RewardDistributor) -> u64 {
    distributor
        .pending_spend
        .latest_state
        .1
        .round_time_info
        .last_update
}
