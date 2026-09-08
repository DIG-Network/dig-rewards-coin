//! Funding a distributor — `SPEC.md` §7.4.
//!
//! There are two ways to put $DIG into a distributor and they differ in exactly one respect that
//! matters to a funder: whether the money can ever come back.
//!
//! - [`commit_incentives_for_distributor_epoch`] (`CommitIncentives`) commits to a **named future**
//!   distributor epoch and leaves a commitment slot recording the funder's clawback hash. It is the
//!   default because it is the only one a funder can undo (§7.5, [`crate::clawback`]).
//! - [`donate_irrevocably_to_current_epoch`] (`AddIncentives`) adds to the epoch already running.
//!   There is no slot, no clawback hash, and no way back. Its name says so, and it is deliberately
//!   not the default (§7.4 clause 2).
//!
//! Every amount here is **$DIG base units** as an integer, never a rendered value and never a
//! float.

use chia_protocol::Bytes32;
use chia_sdk_driver::{
    RewardDistributor, RewardDistributorAddIncentivesAction,
    RewardDistributorCommitIncentivesAction, Slot, SpendContext,
};
use chia_sdk_types::puzzles::RewardDistributorRewardSlotValue;
use chia_sdk_types::Conditions;

use crate::constants::COMMITMENT_DEPTH_EPOCHS;
use crate::RewardsError;

/// How many future distributor epochs a funder is committing to.
///
/// [`CommitmentDepth::default_depth`] is [`COMMITMENT_DEPTH_EPOCHS`]. Going deeper needs
/// [`CommitmentDepth::deeper`], which exists so that "I am locking money up for the next N weeks"
/// is a sentence the caller had to write, not a number that drifted upward in a config file
/// (§7.4 clause 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CommitmentDepth(u32);

impl CommitmentDepth {
    /// The DIG default: [`COMMITMENT_DEPTH_EPOCHS`] future distributor epochs.
    #[must_use]
    pub const fn default_depth() -> Self {
        Self(COMMITMENT_DEPTH_EPOCHS)
    }

    /// Commit deeper than the default.
    ///
    /// # Errors
    ///
    /// [`RewardsError::InvalidLaunchTerms`] if `epochs` is zero or not deeper than the default —
    /// use [`CommitmentDepth::default_depth`] for those, so that a call to `deeper` always means
    /// what it says.
    pub fn deeper(epochs: u32) -> Result<Self, RewardsError> {
        if epochs <= COMMITMENT_DEPTH_EPOCHS {
            return Err(RewardsError::InvalidLaunchTerms(format!(
                "{epochs} is not deeper than the default {COMMITMENT_DEPTH_EPOCHS}; \
                 use CommitmentDepth::default_depth()"
            )));
        }

        Ok(Self(epochs))
    }

    /// How many epochs this depth covers.
    #[must_use]
    pub const fn epochs(self) -> u32 {
        self.0
    }
}

impl Default for CommitmentDepth {
    fn default() -> Self {
        Self::default_depth()
    }
}

/// The distributor-epoch start times a commitment of the given depth covers.
///
/// Every start is a real epoch boundary derived from `first_epoch_start` and
/// `distributor_epoch_seconds`, so a caller cannot commit to a moment mid-epoch, which the puzzle
/// would reject after the operator had already paid the fee.
///
/// The first returned start is the earliest boundary strictly after `now_unix_seconds`:
/// `CommitIncentives` is for **future** epochs only.
///
/// # Errors
///
/// [`RewardsError::InvalidLaunchTerms`] if `distributor_epoch_seconds` is zero, or if the epoch
/// schedule overflows.
pub fn plan_commitment_epochs(
    first_epoch_start: u64,
    distributor_epoch_seconds: u64,
    now_unix_seconds: u64,
    depth: CommitmentDepth,
) -> Result<Vec<u64>, RewardsError> {
    if distributor_epoch_seconds == 0 {
        return Err(RewardsError::InvalidLaunchTerms(
            "distributor_epoch_seconds must not be zero".to_string(),
        ));
    }

    let elapsed = now_unix_seconds.saturating_sub(first_epoch_start);
    let epochs_gone = elapsed / distributor_epoch_seconds;

    (0..u64::from(depth.epochs()))
        .map(|offset| {
            let index = epochs_gone
                .checked_add(1)
                .and_then(|next| next.checked_add(offset))
                .ok_or_else(overflowed_schedule)?;

            index
                .checked_mul(distributor_epoch_seconds)
                .and_then(|delta| first_epoch_start.checked_add(delta))
                .ok_or_else(overflowed_schedule)
        })
        .collect()
}

/// Commit $DIG to one named future distributor epoch — the default fund path.
///
/// `clawback_puzzle_hash` is recorded in the commitment slot and is the **only** authority that can
/// later withdraw this commitment (§7.5). It is not the manager singleton and not the launcher.
///
/// The returned [`Conditions`] must be delivered by the funder's own CAT spend in the same bundle,
/// exactly as the upstream flow requires: the reserve and the source CAT are spent together so the
/// deltas add up.
///
/// # Errors
///
/// - [`RewardsError::InvalidLaunchTerms`] if `clawback_puzzle_hash` is the zero hash — a zero hash
///   makes the commitment unrecoverable, which turns the default revocable path into the
///   irrevocable one without saying so — or if `rewards_base_units` is zero.
/// - [`RewardsError::Driver`] if the upstream action could not be built.
pub fn commit_incentives_for_distributor_epoch(
    ctx: &mut SpendContext,
    distributor: &mut RewardDistributor,
    reward_slot: Slot<RewardDistributorRewardSlotValue>,
    distributor_epoch_start: u64,
    clawback_puzzle_hash: Bytes32,
    rewards_base_units: u64,
) -> Result<Conditions, RewardsError> {
    if clawback_puzzle_hash == Bytes32::default() {
        return Err(RewardsError::InvalidLaunchTerms(
            "clawback puzzle hash must not be the zero hash; the commitment would be \
             unrecoverable"
                .to_string(),
        ));
    }

    if rewards_base_units == 0 {
        return Err(RewardsError::InvalidLaunchTerms(
            "a commitment of zero base units funds nothing".to_string(),
        ));
    }

    let conditions = distributor
        .new_action::<RewardDistributorCommitIncentivesAction>()
        .spend(
            ctx,
            distributor,
            reward_slot,
            distributor_epoch_start,
            clawback_puzzle_hash,
            rewards_base_units,
        )?;

    Ok(conditions)
}

/// Add $DIG to the distributor epoch **already running**, irrevocably.
///
/// This is `AddIncentives`. It creates no commitment slot, records no clawback hash, and cannot be
/// withdrawn by anyone under any condition. The money is gone from the funder's control the moment
/// the spend confirms.
///
/// It exists because donating to a live epoch is a legitimate thing to want. It is not the default,
/// and the name is the warning (§7.4 clause 2).
///
/// # Errors
///
/// - [`RewardsError::InvalidLaunchTerms`] if `amount_base_units` is zero.
/// - [`RewardsError::Driver`] if the upstream action could not be built.
pub fn donate_irrevocably_to_current_epoch(
    ctx: &mut SpendContext,
    distributor: &mut RewardDistributor,
    amount_base_units: u64,
) -> Result<Conditions, RewardsError> {
    if amount_base_units == 0 {
        return Err(RewardsError::InvalidLaunchTerms(
            "a donation of zero base units funds nothing".to_string(),
        ));
    }

    let conditions = distributor
        .new_action::<RewardDistributorAddIncentivesAction>()
        .spend(ctx, distributor, amount_base_units)?;

    Ok(conditions)
}

/// The schedule arithmetic overflowed `u64` seconds.
fn overflowed_schedule() -> RewardsError {
    RewardsError::InvalidLaunchTerms("the distributor epoch schedule overflows".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::DEFAULT_DISTRIBUTOR_EPOCH_SECONDS;

    const FIRST: u64 = 1_800_000_000;

    #[test]
    fn the_default_commitment_depth_is_two_future_epochs() {
        assert_eq!(CommitmentDepth::default_depth().epochs(), 2);
        assert_eq!(CommitmentDepth::default().epochs(), 2);

        let plan = plan_commitment_epochs(
            FIRST,
            DEFAULT_DISTRIBUTOR_EPOCH_SECONDS,
            FIRST,
            CommitmentDepth::default_depth(),
        )
        .unwrap();

        assert_eq!(
            plan,
            vec![
                FIRST + DEFAULT_DISTRIBUTOR_EPOCH_SECONDS,
                FIRST + 2 * DEFAULT_DISTRIBUTOR_EPOCH_SECONDS,
            ]
        );
    }

    #[test]
    fn a_plan_only_ever_names_future_epoch_boundaries() {
        let now = FIRST + 3 * DEFAULT_DISTRIBUTOR_EPOCH_SECONDS + 42;
        let plan = plan_commitment_epochs(
            FIRST,
            DEFAULT_DISTRIBUTOR_EPOCH_SECONDS,
            now,
            CommitmentDepth::default_depth(),
        )
        .unwrap();

        for start in &plan {
            assert!(*start > now, "{start} is not in the future");
            assert_eq!((start - FIRST) % DEFAULT_DISTRIBUTOR_EPOCH_SECONDS, 0);
        }
        assert_eq!(plan.len(), 2);
    }

    #[test]
    fn a_deeper_commitment_needs_an_explicit_argument() {
        assert!(CommitmentDepth::deeper(2).is_err());
        assert!(CommitmentDepth::deeper(0).is_err());

        let deep = CommitmentDepth::deeper(6).unwrap();
        assert_eq!(deep.epochs(), 6);

        let plan =
            plan_commitment_epochs(FIRST, DEFAULT_DISTRIBUTOR_EPOCH_SECONDS, FIRST, deep).unwrap();
        assert_eq!(plan.len(), 6);
    }

    #[test]
    fn a_zero_epoch_length_has_no_schedule() {
        assert!(plan_commitment_epochs(FIRST, 0, FIRST, CommitmentDepth::default_depth()).is_err());
    }
}
