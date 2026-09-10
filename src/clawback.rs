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
//! This module carries exactly **one** authoritative restatement of the puzzle's share
//! arithmetic — [`recoverable_base_units`] — bound to the paying code
//! (`withdraw_incentives.rs:105-107`) by a simulator equality test, so a future upstream drift
//! arrives as a red build rather than a silent mismatch. §0.1 clause 1 forbids an *untested* copy
//! scattered into a consumer that can drift unnoticed; a single tested restatement, kept here in
//! the crate that owns the domain and proven equal to what the puzzle actually pays for every
//! amount the puzzle can pay on, is not that drift — it is the fix for it. That qualifier is
//! load-bearing: see [`recoverable_base_units`] for the bound, and for why nothing is equal to
//! the puzzle above it. [`withdraw_committed_incentives`] itself still returns the
//! puzzle's own figure, never a recomputation: [`recoverable_base_units`] exists so a caller (such
//! as `dig.listRewardDistributorCommitments`) can preview the amount *before* paying for a spend.

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
#[derive(Debug)]
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

/// The share of a commitment the puzzle pays out on clawback, mirroring
/// `chia-sdk-driver-0.36.0/src/layers/action_layer/actions/reward_distributor/withdraw_incentives.rs:105-107`
/// byte-for-byte: multiply then divide, truncating, never rounded or saturated.
///
/// This is the **one** authoritative restatement this crate carries (see the module doc). A
/// simulator equality test binds it to the real paid amount, so an upstream change to that
/// arithmetic arrives here as a red test rather than a silent mismatch.
///
/// # Where this equals what the puzzle pays — and where nothing does
///
/// The simulator equality proof holds for
/// `rewards_base_units <= u64::MAX / withdrawal_share_bps`: about `2.05e15` base units at this
/// crate's own 9_000 bps. That bound is not a test limitation, it is the whole range in which
/// upstream is defined. Its `rewards * withdrawal_share_bps` (`withdraw_incentives.rs:105-107`)
/// is a plain `u64` multiply, so above the bound a real clawback panics under overflow checks and
/// silently wraps without them. There is no amount it *should* have paid for this function to be
/// equal to, which is why the overflow-scale test asserts arithmetic only and claims nothing
/// about upstream.
///
/// Above the bound this function still returns the mathematically correct share, because the
/// multiply runs in a `u128` intermediate. That is deliberate rather than defensive: callers here
/// (such as `dig.listRewardDistributorCommitments` previewing a commitment) carry no chain-level
/// amount limit of their own, and a preview that wrapped into a plausible-looking small number
/// would be worse than one that refused. It remains a preview of the arithmetic, never a promise
/// that a clawback spend at that scale would succeed — it would not.
///
/// `withdrawal_share_bps` is deliberately `u16`, narrower than the puzzle constant's own `u64`.
/// That narrowing is the point: a caller reading `withdrawal_share_bps` off a chain constant must
/// face the cast rather than have it silently wrap into a plausible-looking small share. Rejecting
/// or clamping a constant above `u16::MAX` is the **caller's** responsibility, not this
/// function's — it must never be widened to make that check disappear.
#[must_use]
pub fn recoverable_base_units(rewards_base_units: u64, withdrawal_share_bps: u16) -> u64 {
    let share = u128::from(rewards_base_units) * u128::from(withdrawal_share_bps) / 10_000;

    // `withdrawal_share_bps` is at most `u16::MAX` (65_535) and the divisor is 10_000, so the
    // quotient can never exceed `rewards_base_units` and always fits back in a u64.
    u64::try_from(share).expect("share of a u64 amount by a bps fraction fits in u64")
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
