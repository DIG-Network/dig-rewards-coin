//! The DIG-shaped [`RewardDistributorConstants`] — the constants table a DIG rewards distributor
//! MUST carry, as one builder.
//!
//! `SPEC.md` §15 clause 3 fixes every row of this table, and §9.2 fixes the only construction path:
//! `RewardDistributorConstants::without_launcher_id` and then
//! `RewardDistributorConstants::with_launcher_id`, never a hand-assembled struct. The two reserve
//! puzzle hashes are *derived* from the launcher id and the asset id, so setting either by hand
//! produces a distributor whose reserve nobody can spend.
//!
//! ## The three launch-time-only choices live in the type
//!
//! `epoch_seconds`, `first_epoch_start` and the manager singleton are curried into the action
//! puzzles at launch and are immutable for the distributor's whole life (§8.1). They are therefore
//! required fields of [`DistributorLaunchTerms`], which deliberately has **no `Default` impl**: a
//! creation surface can only offer a choice this API forces it to supply. The DIG defaults are
//! published as the named constants below so a caller opts into them explicitly.

use chia_protocol::Bytes32;
use chia_sdk_driver::{RewardDistributorConstants, RewardDistributorType};
use dig_constants::{DIG_ASSET_ID, DIG_TREASURY_INNER_PUZZLE_HASH};

use crate::RewardsError;

/// The DIG distributor epoch length, in seconds: seven days (`SPEC.md` §8.1).
///
/// This is the **distributor** epoch — the CHIP-0051 reward-accrual window. It is unrelated to the
/// mirror-collateral epoch and to the `dig-epoch` L2 epoch, and none of the three may be derived
/// from another (§0.3).
pub const DEFAULT_DISTRIBUTOR_EPOCH_SECONDS: u64 = 604_800;

/// How far a spend's asserted time may sit from the distributor's `last_update`, in seconds
/// (`SPEC.md` §8.2). The puzzle asserts `ASSERT_BEFORE_SECONDS_ABSOLUTE(last_update + this)`.
pub const MAX_SECONDS_OFFSET: u64 = 300;

/// The minimum accrued amount `InitiatePayout` will pay out, in **$DIG base units** — one whole
/// $DIG, since $DIG carries three decimals (`SPEC.md` §0.2, §15 clause 3).
pub const PAYOUT_THRESHOLD_BASE_UNITS: u64 = 1_000;

/// The share of a withdrawn commitment the clawbacker recovers, in basis points out of `10_000`
/// (`SPEC.md` §7.5). Carried as basis points end to end; never a percentage and never a float.
pub const WITHDRAWAL_SHARE_BPS: u64 = 9_000;

/// The MVP epoch fee, in basis points out of `10_000`: **zero** (`SPEC.md` §7.3 clause 1).
///
/// A non-zero fee is a funder self-skim taken off the mirrors, not an ecosystem fee (§7.3 clause 4),
/// so it needs the explicit opt-in constructor
/// [`dig_distributor_constants_with_funder_self_skim`].
pub const DEFAULT_FEE_BPS: u64 = 0;

/// How far in the future a launch should place `first_epoch_start`, in seconds, when the caller has
/// no reason to prefer another moment (`SPEC.md` §8.5).
pub const FIRST_EPOCH_START_LEAD_SECONDS: u64 = 600;

/// How many future distributor epochs the default `CommitIncentives` fund path commits to
/// (`SPEC.md` §7.4 clause 1).
pub const COMMITMENT_DEPTH_EPOCHS: u32 = 2;

/// The cap on entries in one distributor's entry set (`SPEC.md` §15 clause 7). An add beyond it is
/// refused with a named state, never a silent stop.
pub const MAX_ENTRIES_PER_DISTRIBUTOR: u32 = 250;

/// The share weight every DIG entry carries. Every mirror of one generation is worth the same, so
/// `shares` is never a caller parameter on the DIG path (`SPEC.md` §11.1, §11.3).
pub const ENTRY_SHARES: u64 = 1;

/// The dimensionless scale factor on the puzzle's `u128` accumulators (`SPEC.md` §8.4).
///
/// Deliberately **not** a public settable field: it is neither a currency, a count, nor a rounding
/// mode, and a caller who lowers it silently coarsens every accrual.
const ACCUMULATOR_PRECISION: u64 = u64::MAX;

/// The largest representable basis-point denominator; the puzzle divides by it itself.
const BPS_DENOMINATOR: u64 = 10_000;

/// The three choices that can only ever be made at launch, because they are curried into the action
/// puzzles and immutable afterwards (`SPEC.md` §8.1, §15 clause 3a).
///
/// There is intentionally no `Default` impl. A creation surface can only offer a choice this type
/// forces it to supply, and each of these three is a decision no library should make silently on a
/// funder's behalf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DistributorLaunchTerms {
    /// Launcher id of the **manager singleton** that authorizes entry-set writes (§11).
    ///
    /// Its inner puzzle is the caller's choice and MAY be recovery-capable — a multisig, a vault, a
    /// rekeyable layer. This crate never assumes, and never hard-codes, a bare single-key p2:
    /// losing that one key would strand the entry set for the distributor's whole life.
    pub manager_singleton_launcher_id: Bytes32,

    /// The distributor epoch length in seconds. [`DEFAULT_DISTRIBUTOR_EPOCH_SECONDS`] is the DIG
    /// value; it MUST NOT be derived from the mirror-collateral calendar or from `dig-epoch`
    /// (§0.3).
    pub distributor_epoch_seconds: u64,

    /// Unix seconds at which the first distributor epoch begins. Must be in the future at launch
    /// (§8.5 clause 1); [`FIRST_EPOCH_START_LEAD_SECONDS`] is a reasonable lead.
    pub first_epoch_start: u64,
}

/// The DIG constants table for a rewards distributor, with a zero epoch fee.
///
/// `funder_refund_puzzle_hash` is the funder's own refund/change puzzle hash — the same hash passed
/// to the launch as `cat_refund_puzzle_hash` — and it is also the `fee_payout_puzzle_hash` (§7.3
/// clause 3). A zero hash is refused: a later non-zero fee would burn to it.
///
/// The returned value carries the **pre-launch** form: `launcher_id` and the two reserve puzzle
/// hashes are still default, because the launcher id does not exist until the launch spend builds
/// it. [`with_dig_launcher_id`] completes the table once it does.
///
/// # Errors
///
/// [`RewardsError::InvalidLaunchTerms`] if `funder_refund_puzzle_hash` is the zero hash or the DIG
/// treasury (§7.3 clause 3), if `terms.manager_singleton_launcher_id` is the zero hash, or if
/// `terms.distributor_epoch_seconds` is zero.
pub fn dig_distributor_constants(
    terms: DistributorLaunchTerms,
    funder_refund_puzzle_hash: Bytes32,
) -> Result<RewardDistributorConstants, RewardsError> {
    dig_constants_with_fee(terms, funder_refund_puzzle_hash, DEFAULT_FEE_BPS)
}

/// The DIG constants table with a **non-zero epoch fee**, which is a funder self-skim.
///
/// Every basis point here is a basis point the mirrors do not receive, paid to the funder that set
/// it (§7.3 clause 4). It is not an ecosystem, protocol or infrastructure fee and MUST NOT be
/// labelled as one; a surface that exposes it MUST show the mirror-facing net rate beside the
/// gross. This separate constructor exists so that the plain [`dig_distributor_constants`] path
/// *cannot* express a non-zero fee (§7.3 clause 2).
///
/// # Errors
///
/// [`RewardsError::InvalidLaunchTerms`] if `fee_bps` is zero (use
/// [`dig_distributor_constants`]), if `fee_bps` is not below `10_000`, or for any reason
/// [`dig_distributor_constants`] itself refuses the terms.
pub fn dig_distributor_constants_with_funder_self_skim(
    terms: DistributorLaunchTerms,
    funder_refund_puzzle_hash: Bytes32,
    fee_bps: u64,
) -> Result<RewardDistributorConstants, RewardsError> {
    if fee_bps == 0 {
        return Err(RewardsError::InvalidLaunchTerms(
            "fee_bps is zero; use dig_distributor_constants for the DIG default".to_string(),
        ));
    }

    if fee_bps >= BPS_DENOMINATOR {
        return Err(RewardsError::InvalidLaunchTerms(format!(
            "fee_bps {fee_bps} must be below {BPS_DENOMINATOR} basis points"
        )));
    }

    dig_constants_with_fee(terms, funder_refund_puzzle_hash, fee_bps)
}

/// Complete a constants table with the launcher id the launch spend produced.
///
/// This is the second and only other half of §9.2's construction path: it derives
/// `reserve_inner_puzzle_hash` and `reserve_full_puzzle_hash` from the launcher id and the $DIG
/// asset id. Never compute either by hand.
pub fn with_dig_launcher_id(
    constants: RewardDistributorConstants,
    launcher_id: Bytes32,
) -> RewardDistributorConstants {
    constants.with_launcher_id(launcher_id)
}

/// The one construction path, shared by both public constructors so neither can drift from §15's
/// table.
fn dig_constants_with_fee(
    terms: DistributorLaunchTerms,
    funder_refund_puzzle_hash: Bytes32,
    fee_bps: u64,
) -> Result<RewardDistributorConstants, RewardsError> {
    if funder_refund_puzzle_hash == Bytes32::default() {
        return Err(RewardsError::InvalidLaunchTerms(
            "funder refund puzzle hash must not be the zero hash".to_string(),
        ));
    }

    // §7.3 clause 3: the fee payout hash MUST NOT be the DIG treasury. Refusing the treasury as a
    // *destination* is the opposite of §7.3a's prohibition on hard-coding it as the recipient — a
    // non-zero epoch fee is a funder self-skim (§7.3 clause 4), and paying it to the treasury would
    // dress that skim up as an ecosystem fee. The constant is read from `dig-constants`, so no
    // policy value is restated here.
    if funder_refund_puzzle_hash == DIG_TREASURY_INNER_PUZZLE_HASH {
        return Err(RewardsError::InvalidLaunchTerms(
            "fee_payout_puzzle_hash must not be the DIG treasury (SPEC.md §7.3 clause 3): the \
             epoch fee is a funder self-skim, not an ecosystem fee"
                .to_string(),
        ));
    }

    // §7.2 clause 3: the manager singleton launcher id is curried into the action puzzles and is
    // immutable for the distributor's whole life. A zero id names no singleton, so no spend can
    // ever satisfy the entry-set write authority — the set is PERMANENTLY unwritable, which the
    // SPEC calls the worst irreversible outcome in the design. It is refused here because it cannot
    // be refused anywhere later.
    if terms.manager_singleton_launcher_id == Bytes32::default() {
        return Err(RewardsError::InvalidLaunchTerms(
            "manager_singleton_launcher_id must not be the zero hash: it is curried in at launch \
             and a zero id freezes the entry set for the distributor's life"
                .to_string(),
        ));
    }

    if terms.distributor_epoch_seconds == 0 {
        return Err(RewardsError::InvalidLaunchTerms(
            "distributor_epoch_seconds must not be zero".to_string(),
        ));
    }

    Ok(RewardDistributorConstants::without_launcher_id(
        RewardDistributorType::Managed {
            manager_singleton_launcher_id: terms.manager_singleton_launcher_id,
        },
        funder_refund_puzzle_hash,
        terms.distributor_epoch_seconds,
        ACCUMULATOR_PRECISION,
        MAX_SECONDS_OFFSET,
        PAYOUT_THRESHOLD_BASE_UNITS,
        // §7.1: DIG never gates a mirror's own claim on an operator approval.
        false,
        fee_bps,
        WITHDRAWAL_SHARE_BPS,
        DIG_ASSET_ID,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms() -> DistributorLaunchTerms {
        DistributorLaunchTerms {
            manager_singleton_launcher_id: Bytes32::new([7; 32]),
            distributor_epoch_seconds: DEFAULT_DISTRIBUTOR_EPOCH_SECONDS,
            first_epoch_start: 1_800_000_000,
        }
    }

    fn refund_hash() -> Bytes32 {
        Bytes32::new([9; 32])
    }

    #[test]
    fn dig_constants_match_the_spec_table() {
        let constants = dig_distributor_constants(terms(), refund_hash()).unwrap();

        assert_eq!(constants.epoch_seconds, 604_800);
        assert_eq!(constants.max_seconds_offset, 300);
        assert_eq!(constants.payout_threshold, 1_000);
        assert_eq!(constants.withdrawal_share_bps, 9_000);
        assert_eq!(constants.fee_bps, 0);
        assert!(!constants.require_payout_approval);
        assert_eq!(constants.precision, u64::MAX);
        assert_eq!(constants.reserve_asset_id, DIG_ASSET_ID);
        assert_eq!(constants.fee_payout_puzzle_hash, refund_hash());
        assert_eq!(
            constants.reward_distributor_type,
            RewardDistributorType::Managed {
                manager_singleton_launcher_id: Bytes32::new([7; 32]),
            }
        );

        // §9.2: `without_launcher_id` leaves all three derived fields default, and only
        // `with_launcher_id` fills them. Asserting both halves is what proves the order.
        assert_eq!(constants.launcher_id, Bytes32::default());
        assert_eq!(constants.reserve_inner_puzzle_hash, Bytes32::default());
        assert_eq!(constants.reserve_full_puzzle_hash, Bytes32::default());

        let launcher_id = Bytes32::new([3; 32]);
        let completed = with_dig_launcher_id(constants, launcher_id);

        assert_eq!(completed.launcher_id, launcher_id);
        assert_ne!(completed.reserve_inner_puzzle_hash, Bytes32::default());
        assert_ne!(completed.reserve_full_puzzle_hash, Bytes32::default());
        assert_eq!(
            completed.reserve_inner_puzzle_hash,
            constants
                .with_launcher_id(launcher_id)
                .reserve_inner_puzzle_hash
        );
    }

    #[test]
    fn the_named_defaults_are_the_spec_values() {
        assert_eq!(DEFAULT_DISTRIBUTOR_EPOCH_SECONDS, 604_800);
        assert_eq!(MAX_SECONDS_OFFSET, 300);
        assert_eq!(PAYOUT_THRESHOLD_BASE_UNITS, 1_000);
        assert_eq!(WITHDRAWAL_SHARE_BPS, 9_000);
        assert_eq!(DEFAULT_FEE_BPS, 0);
        assert_eq!(FIRST_EPOCH_START_LEAD_SECONDS, 600);
        assert_eq!(COMMITMENT_DEPTH_EPOCHS, 2);
        assert_eq!(MAX_ENTRIES_PER_DISTRIBUTOR, 250);
        assert_eq!(ENTRY_SHARES, 1);
    }

    #[test]
    fn a_zero_refund_hash_is_refused() {
        let err = dig_distributor_constants(terms(), Bytes32::default()).unwrap_err();
        assert!(matches!(err, RewardsError::InvalidLaunchTerms(_)));
    }

    #[test]
    fn a_zero_manager_singleton_launcher_id_is_refused() {
        // The launcher id is curried into the action puzzles at launch, so a zero one freezes the
        // entry set for the distributor's whole life (SPEC.md §7.2 clause 3). Nothing downstream
        // can refuse it, which is why it is refused here.
        let mut terms = terms();
        terms.manager_singleton_launcher_id = Bytes32::default();

        let err = dig_distributor_constants(terms, refund_hash()).unwrap_err();
        assert!(matches!(err, RewardsError::InvalidLaunchTerms(_)));

        // The self-skim constructor shares the one construction path, so it refuses it too.
        assert!(
            dig_distributor_constants_with_funder_self_skim(terms, refund_hash(), 250).is_err()
        );
    }

    #[test]
    fn the_dig_treasury_is_refused_as_the_fee_payout_hash() {
        // SPEC.md §7.3 clause 3. The epoch fee is a funder self-skim (§7.3 clause 4), so sending it
        // to the treasury would present it as an ecosystem fee. This is the opposite of §7.3a,
        // which forbids hard-coding the treasury as the RECIPIENT.
        let err = dig_distributor_constants(terms(), DIG_TREASURY_INNER_PUZZLE_HASH).unwrap_err();
        assert!(matches!(err, RewardsError::InvalidLaunchTerms(_)));

        assert!(
            dig_distributor_constants_with_funder_self_skim(
                terms(),
                DIG_TREASURY_INNER_PUZZLE_HASH,
                250
            )
            .is_err(),
            "the fee-bearing path is the one that would actually pay the treasury"
        );

        // And the refusal is about the treasury specifically, not about non-funder hashes at large.
        assert!(dig_distributor_constants(terms(), Bytes32::new([9; 32])).is_ok());
    }

    #[test]
    fn a_zero_distributor_epoch_is_refused() {
        let mut terms = terms();
        terms.distributor_epoch_seconds = 0;
        let err = dig_distributor_constants(terms, refund_hash()).unwrap_err();
        assert!(matches!(err, RewardsError::InvalidLaunchTerms(_)));
    }

    #[test]
    fn a_non_zero_fee_needs_the_opt_in_constructor() {
        let skimmed =
            dig_distributor_constants_with_funder_self_skim(terms(), refund_hash(), 250).unwrap();
        assert_eq!(skimmed.fee_bps, 250);

        // The opt-in constructor refuses the values the plain one already covers, and refuses a
        // fee that would take the whole epoch.
        assert!(
            dig_distributor_constants_with_funder_self_skim(terms(), refund_hash(), 0).is_err()
        );
        assert!(
            dig_distributor_constants_with_funder_self_skim(terms(), refund_hash(), 10_000)
                .is_err()
        );
    }
}
