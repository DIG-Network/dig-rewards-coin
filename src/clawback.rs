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
//! at `withdraw_incentives.rs:105-107` for every amount the driver can pay on, is not that drift — it is the fix for it.
//!
//! **[`withdraw_committed_incentives`] does not simply return the puzzle's own figure.** The
//! driver's `(Conditions, u64)` return is a Rust **re-derivation** of the share — a second,
//! independent `u64` multiply, never curried into the puzzle solution and never present in the
//! returned conditions — so above `u64::MAX / withdrawal_share_bps` it disagrees with (or, without
//! checked arithmetic, silently misreports) what the on-chain puzzle actually pays (#3286). This
//! function therefore refuses BEFORE calling the upstream driver when the share cannot be
//! represented at all, and cross-checks the driver's returned figure against
//! [`recoverable_base_units`] afterwards, refusing rather than returning a tuple that may not
//! describe the real spend.
//!
//! The puzzle itself pays the correct share at **any** scale — CLVM arithmetic is bignum, so there
//! is no on-chain overflow and it never "cannot build the spend at that scale". What fails above
//! `u64::MAX / withdrawal_share_bps` is only the `chia-sdk-driver` 0.36.0 Rust driver's own plain
//! `u64` multiply (`withdraw_incentives.rs:105-107`), which misreports rather than refuses — a
//! driver bug tracked as #3286, not a property of the reward system. See
//! [`recoverable_base_units`] for the `bps` domain rule and that bound.

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
///
/// Fields are private with accessors. `RewardsError` is `#[non_exhaustive]`, so a new failure
/// variant is invisible to a consumer at compile time -- but a `pub conditions` /
/// `pub recovered_base_units` pair lets a consumer construct `Clawback { .. }` with any number of
/// their own choosing, which repeats the exact trap #3286 is: a figure nothing downstream can
/// verify against the puzzle. Only [`withdraw_committed_incentives`] can produce one, so a
/// `Clawback` always describes a spend this crate actually built and cross-checked.
#[derive(Debug)]
pub struct Clawback {
    conditions: Conditions,
    recovered_base_units: u64,
}

impl Clawback {
    /// Conditions the clawbacker's own coin must assert in the same bundle.
    ///
    /// `Conditions` is itself `#[must_use]`, so no attribute is needed here.
    pub fn conditions(&self) -> &Conditions {
        &self.conditions
    }

    /// Consumes `self`, returning the conditions to deliver.
    pub fn into_conditions(self) -> Conditions {
        self.conditions
    }

    /// The amount, in **$DIG base units**, this crate cross-checked against the puzzle's own
    /// arithmetic before returning it. See the module doc for why this is a checked re-derivation
    /// rather than a value read straight off the puzzle.
    #[must_use]
    pub fn recovered_base_units(&self) -> u64 {
        self.recovered_base_units
    }
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
///
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

/// The largest reserve, in **$DIG base units**, [`crate::state::read_distributor`] will attempt to
/// reconstruct a generation through: above this, `reserve_base_units * 10_000` -- the widest a
/// `withdrawal_share_bps` reading can validly be, per [`RewardsError::UnreadableDistributorConstants`]'s
/// own domain check -- could exceed `u64::MAX`, which is the scale at which `chia-sdk-driver`
/// 0.36.0's plain `u64` share multiply can misreport (#3286). Derived, never spelled as a decimal
/// literal, so the bound cannot silently drift from the domain rule it is paired with.
pub const MAX_REPORTABLE_COMMITMENT_BASE_UNITS: u64 = u64::MAX / 10_000;

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

    let withdrawal_share_bps = distributor.info.constants.withdrawal_share_bps;

    // Pre-guard, BEFORE the upstream call: `chia-sdk-driver` 0.36.0's own share multiply
    // (`withdraw_incentives.rs:105-107`) is a plain `u64` multiply and panics under checked
    // arithmetic with no chance to return anything at all (#3286). A post-hoc check cannot run
    // inside a call that never returns.
    //
    // The guarded value must be the one upstream will ACTUALLY multiply, not the caller-supplied
    // `commitment_slot` as passed in: `RewardDistributor::actual_commitment_slot_value`
    // (`reward_distributor.rs:833-849`) silently substitutes any pending commitment slot created
    // earlier in this same spend that matches on `epoch_start` alone -- ignoring `clawback_ph`
    // and `rewards` -- and `.spend()` below re-derives from that substituted value
    // (`withdraw_incentives.rs:103`), not from the slot this function was handed. A caller who
    // funds a future epoch (`crate::fund::commit_incentives_for_distributor_epoch`) and claws
    // back an earlier commitment for the same `epoch_start` in one bundle would otherwise have
    // this guard check one number while upstream multiplies another.
    let actual_commitment_slot = distributor.actual_commitment_slot_value(commitment_slot);
    let rewards_base_units = actual_commitment_slot.info.value.rewards;

    if rewards_base_units
        .checked_mul(withdrawal_share_bps)
        .is_none()
    {
        return Err(RewardsError::DriverShareNotRepresentable {
            rewards_base_units,
            withdrawal_share_bps,
        });
    }

    let (conditions, recovered_base_units) = distributor
        .new_action::<RewardDistributorWithdrawIncentivesAction>()
        .spend(ctx, distributor, actual_commitment_slot, reward_slot)?;

    // Cross-check the driver's returned figure against this crate's own restatement. A
    // disagreement means the pre-guard above was not tight enough to catch every way the
    // driver's arithmetic can misreport (for example `withdrawal_share_bps` too wide for the
    // `u16` `recoverable_base_units` takes) -- and either way, a disagreement makes the whole
    // returned tuple untrustworthy, not just the share.
    let restated = u16::try_from(withdrawal_share_bps)
        .ok()
        .and_then(|bps| recoverable_base_units(rewards_base_units, bps));

    if restated != Some(recovered_base_units) {
        return Err(RewardsError::DriverShareDisagrees {
            driver_reported: recovered_base_units,
            restated,
        });
    }

    Ok(Clawback {
        conditions,
        recovered_base_units,
    })
}
