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
//! the crate that owns the domain and proven equal to `chia-sdk-driver` 0.36.0's returned figure
//! at `withdraw_incentives.rs:105-107` for every amount the driver can pay on, is not that drift — it is the fix for it. [`withdraw_committed_incentives`]
//! itself still returns the puzzle's own figure, never a recomputation:
//! [`recoverable_base_units`] exists so a caller (such as `dig.listRewardDistributorCommitments`)
//! can preview the amount *before* paying for a spend.
//!
//! The puzzle itself pays the correct share at **any** scale — CLVM arithmetic is bignum, so there
//! is no on-chain overflow. What fails above `u64::MAX / withdrawal_share_bps` is the
//! `chia-sdk-driver` 0.36.0 Rust driver, whose own multiply is a plain `u64` (`withdraw_incentives.rs:105-107`)
//! and cannot build the spend at that scale — a driver bug tracked as #3286, not a property of the
//! reward system. See [`recoverable_base_units`] for the `bps` domain rule and that bound.

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
/// `rewards_base_units <= u64::MAX / withdrawal_share_bps`: `2_049_638_230_412_172` base units at
/// this crate's own 9_000 bps, and a simulator case sits on exactly that last base unit, so the
/// bound is pinned rather than merely asserted here. That bound is **not** where the puzzle stops
/// being able to pay — CLVM arithmetic is bignum, so the on-chain puzzle pays the correct share at
/// any scale. It is where the `chia-sdk-driver` 0.36.0 Rust driver stops being able to build the
/// spend at all: its own `rewards * withdrawal_share_bps` (`withdraw_incentives.rs:105-107`) is a
/// plain `u64` multiply, so above the bound it panics under overflow checks and silently wraps
/// without them — a driver bug tracked as #3286, not a property of the reward system. There is no
/// amount the *driver* would have produced for this function to be equal to above that bound,
/// which is why the overflow-scale test asserts arithmetic only and claims nothing about the
/// driver's output there.
///
/// Above the bound this function still returns the mathematically correct share, because the
/// multiply runs in a `u128` intermediate. That is deliberate rather than defensive: callers here
/// (such as `dig.listRewardDistributorCommitments` previewing a commitment) carry no chain-level
/// amount limit of their own, and a preview that wrapped into a plausible-looking small number
/// would be worse than one that refused. It remains a preview of the arithmetic, never a promise
/// that a clawback spend at that scale would succeed with today's driver — it would not, until
/// #3286 lands.
///
/// `withdrawal_share_bps` is deliberately `u16`, narrower than the puzzle constant's own `u64`.
/// That narrowing is the point: a caller reading `withdrawal_share_bps` off a chain constant must
/// face the cast rather than have it silently wrap into a plausible-looking small share.
///
/// The legitimate domain is `0..=10_000`: above 10_000 bps there is no honest share to quote — a
/// distributor cannot pay back more than it holds — so this returns [`None`] rather than a
/// confident number for a constant that is already nonsense. It does not clamp: clamping would
/// report a plausible figure for a distributor whose real constant is hostile or corrupt, which is
/// worse than refusing. A `u16` can carry values up to `65_535`, and that range is reachable — a
/// caller such as `dig.listRewardDistributorCommitments` reads `withdrawal_share_bps` off the
/// chain, and an attacker who launches a distributor with a hostile bps constant reaches this
/// function with it.
#[must_use]
pub fn recoverable_base_units(rewards_base_units: u64, withdrawal_share_bps: u16) -> Option<u64> {
    if withdrawal_share_bps > 10_000 {
        return None;
    }

    let share = u128::from(rewards_base_units) * u128::from(withdrawal_share_bps) / 10_000;

    // `withdrawal_share_bps` is now bounded to `0..=10_000` (checked above) and the divisor is
    // 10_000, so the quotient can never exceed `rewards_base_units` and always fits back in a u64.
    // This is what the pre-guard comment claimed of the full `u16` range, which is false above
    // 10_000 bps — `u64::MAX * 65_535 / 10_000` does not fit in a u64.
    Some(u64::try_from(share).expect("share bounded to 0..=10_000 bps fits in u64"))
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
