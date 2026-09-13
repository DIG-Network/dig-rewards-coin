//! # dig-rewards-coin — the reward-distributor coin driver, on Chia
//!
//! A **rewards distributor** is a CHIP-0051 reward distributor, in its `Managed` mode, that pays
//! $DIG to the peers that actually mirror one DIG generation — one `storeId:root`. Anyone may mint
//! one, anyone may fund it, the funder's node continuously proves which peers really serve those
//! bytes, and a mirroring peer collects its own rewards without asking anyone.
//!
//! This crate is the **driver** over that mechanism: the DIG-shaped constants a DIG distributor
//! MUST carry ([`constants`]), the comment that ties its money to a generation ([`comment`]), the
//! spend builders that launch it and mutate its entry set, the eligibility rule that decides who is
//! in that set, and the observable state that makes "is anyone being paid?" answerable.
//!
//! `SPEC.md` at the repository root is normative and is what every implementation reads **instead
//! of** the SDK.
//!
//! ## What this crate is not
//!
//! The on-chain mechanism is not ours. It is CHIP-0051, implemented upstream in `chia-wallet-sdk`
//! 0.36 (`chia-sdk-driver` + `chia-sdk-types`). This crate therefore:
//!
//! - never reimplements or restates puzzle arithmetic — the per-share accrual, the payout division,
//!   the epoch fee and the withdrawal share belong to the puzzle, except for [`recoverable_base_units`]
//!   which is tested equal to the paying code (§0.1 clause 1);
//! - performs no socket I/O, holds no keys, and never broadcasts; spend builders return unsigned
//!   spends (§0.1 clause 2). Chain reads arrive through a caller-supplied `ChainSource`
//!   (`dig-chainsource-interface`) passed by reference into [`state::read_distributor`] — see
//!   [`state`] and <https://github.com/DIG-Network/dig_ecosystem/issues/3267>;
//! - contains neither the prover loop nor the claim loop. Those are `dig-node`
//!   (<https://github.com/DIG-Network/dig_ecosystem/issues/3250>,
//!   <https://github.com/DIG-Network/dig_ecosystem/issues/3251>); this crate supplies what they
//!   call.
//!
//! ## Units, named once
//!
//! $DIG amounts are always **DIG CAT base units** ($DIG carries three decimals, so
//! `1 $DIG = 1_000` base units). Network fees are **XCH mojos**. The two never share a type and are
//! never added, compared or displayed in one column. `fee_bps` and `withdrawal_share_bps` are basis
//! points out of `10_000`, carried as basis points end to end — a percentage round-trip is how a
//! `420` becomes a `4`. **No float appears anywhere in the money path** (§0.2).
//!
//! ## Three clocks share the word "epoch"
//!
//! The distributor epoch, the mirror-collateral epoch and the `dig-epoch` L2 epoch are unrelated,
//! and none may be derived from another. A bare `epoch` in this crate's public API is a defect, so
//! every identifier here reads `distributor_epoch_*` or `mirror_collateral_epoch` (§0.3).
//!
//! ## Version pinning is a money question
//!
//! `RewardDistributorConstants` are curried into the action puzzles at launch, so a distributor's
//! on-chain identity is a function of the exact upstream puzzle bytes. A client built on different
//! bytes cannot reconstruct or spend a distributor launched under the old ones — the reserve is not
//! lost, it is invisible, which is worse than an error. The `chia-sdk-driver` 0.36 cohort is
//! therefore pinned, and a guard test asserts every action puzzle hash so an upstream bump arrives
//! as a red build (§0.5).

#![warn(missing_docs)]

pub mod clawback;
pub mod comment;
pub mod constants;
pub mod eligibility;
pub mod entries;
pub mod epoch;
mod error;
pub mod fund;
pub mod launch;
pub mod payout;
pub mod state;

pub use clawback::{recoverable_base_units, Clawback, MAX_REPORTABLE_COMMITMENT_BASE_UNITS};
pub use comment::LaunchComment;
pub use constants::{
    dig_distributor_constants, dig_distributor_constants_with_funder_self_skim,
    with_dig_launcher_id, DistributorLaunchTerms, COMMITMENT_DEPTH_EPOCHS,
    DEFAULT_DISTRIBUTOR_EPOCH_SECONDS, DEFAULT_FEE_BPS, ENTRY_SHARES,
    FIRST_EPOCH_START_LEAD_SECONDS, MAX_ENTRIES_PER_DISTRIBUTOR, MAX_SECONDS_OFFSET,
    PAYOUT_THRESHOLD_BASE_UNITS, WITHDRAWAL_SHARE_BPS,
};
pub use eligibility::{
    judge_candidate, judge_candidate_for_epoch, EligibilityQuestion, EligiblePayoutHash,
    Ineligible, MirrorCoinFacts,
};
pub use entries::ManagerAuthority;
pub use error::RewardsError;
pub use launch::{funder_refund_puzzle_hash, launch_dig_distributor, LaunchedDistributor};
pub use state::{
    read_distributor, ChainObservation, DistributorSlots, DistributorSnapshot,
    STALE_ENTRY_SET_SECONDS,
};
