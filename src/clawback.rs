//! Clawing a commitment back — `SPEC.md` §7.4 clauses 3-5 and §7.5.
//!
//! A funder who committed $DIG to a future distributor epoch can withdraw that commitment and
//! recover `withdrawal_share_bps / 10_000` of it. The rest stays in the reserve.
//!
//! Two things about this are easy to get wrong and are structural here rather than advisory:
//!
//! 1. **Authority is the commitment slot's own recorded `clawback_ph` and nothing else.** Not the
//!    manager singleton, not the launcher, not the distributor's operator. [`clawback_authority`]
//!    reads it off the slot so a caller can check they hold it before paying for a spend that the
//!    chain will reject.
//! 2. **Withdrawal is per commitment slot, never against an aggregate balance.** There is no
//!    "distributor balance" to withdraw from; each commitment is its own object with its own
//!    authority and its own epoch.
//!
//! This module computes no share. The `withdrawal_share_bps` division belongs to the puzzle
//! (`withdraw_incentives.rs:105-107`), and [`withdraw_committed_incentives`] returns the amount the
//! puzzle arrived at. A restated formula here would drift (§0.1 clause 1).

use chia_protocol::Bytes32;
use chia_sdk_driver::{
    RewardDistributor, RewardDistributorWithdrawIncentivesAction, Slot, SpendContext,
};
use chia_sdk_types::puzzles::{
    RewardDistributorCommitmentSlotValue, RewardDistributorRewardSlotValue,
};
use chia_sdk_types::Conditions;

use crate::RewardsError;

/// A completed clawback: the conditions to deliver, and what the puzzle returned.
pub struct Clawback {
    /// Conditions the clawbacker's own coin must assert in the same bundle.
    pub conditions: Conditions,

    /// The amount, in **$DIG base units**, the puzzle paid to the slot's recorded `clawback_ph`.
    ///
    /// This is the puzzle's own figure, not a recomputation of it.
    pub recovered_base_units: u64,
}

/// Who — and only who — may withdraw this commitment.
///
/// Read off the commitment slot the funder created. Comparing your own puzzle hash to this before
/// building a spend is cheaper than discovering the mismatch as a rejected transaction.
#[must_use]
pub fn clawback_authority(commitment_slot: &Slot<RewardDistributorCommitmentSlotValue>) -> Bytes32 {
    commitment_slot.info.value.clawback_ph
}

/// Which distributor epoch this commitment is for.
///
/// Named `distributor_epoch_start` rather than `epoch_start` because a bare `epoch` in this crate's
/// public API is a defect (§0.3 clause 3).
#[must_use]
pub fn commitment_distributor_epoch_start(
    commitment_slot: &Slot<RewardDistributorCommitmentSlotValue>,
) -> u64 {
    commitment_slot.info.value.epoch_start
}

/// Withdraw one commitment, recovering the puzzle's withdrawal share of it.
///
/// `expected_clawback_puzzle_hash` is the caller's own hash. It is checked against the slot's
/// recorded authority first: a mismatch is refused here rather than on chain, because the operator
/// pays the network fee for a spend the puzzle then rejects.
///
/// # Errors
///
/// - [`RewardsError::NotTheClawbackAuthority`] if the slot records a different authority.
/// - [`RewardsError::Driver`] if the upstream action could not be built.
pub fn withdraw_committed_incentives(
    ctx: &mut SpendContext,
    distributor: &mut RewardDistributor,
    commitment_slot: Slot<RewardDistributorCommitmentSlotValue>,
    reward_slot: Slot<RewardDistributorRewardSlotValue>,
    expected_clawback_puzzle_hash: Bytes32,
) -> Result<Clawback, RewardsError> {
    let recorded = clawback_authority(&commitment_slot);

    if recorded != expected_clawback_puzzle_hash {
        return Err(RewardsError::NotTheClawbackAuthority);
    }

    let (conditions, recovered_base_units) = distributor
        .new_action::<RewardDistributorWithdrawIncentivesAction>()
        .spend(ctx, distributor, commitment_slot, reward_slot)?;

    Ok(Clawback {
        conditions,
        recovered_base_units,
    })
}
