# Changelog

All notable changes to this project are documented here.
This project adheres to [Semantic Versioning](https://semver.org) and
[Conventional Commits](https://www.conventionalcommits.org).

## [0.5.0] - 2026-09-11

### BREAKING CHANGES
- `Clawback`'s fields are private. `RewardsError` is `#[non_exhaustive]`, so the new refusal
  variants below are invisible to a consumer at compile time -- the field privacy is the only part
  of this fix a consumer is forced to see, and it is deliberate: a public `recovered_base_units`
  field let anyone construct a `Clawback` carrying any figure they liked in a money field the
  documentation called the puzzle's own, which is the very defect this release closes.

  **Migration** -- replace field access with the accessors:

  | before | after |
  | --- | --- |
  | `clawback.conditions` | `clawback.conditions()` (borrows) or `clawback.into_conditions()` (moves) |
  | `clawback.recovered_base_units` | `clawback.recovered_base_units()` |

  `Clawback { .. }` can no longer be constructed outside this crate. Only
  `withdraw_committed_incentives` produces one, so a `Clawback` now always describes a spend this
  crate actually built and cross-checked (SPEC.md 0.1 clause 5c).

### Bug Fixes
- Both public paths that reach `chia-sdk-driver` 0.36.0's `WithdrawIncentives` share multiply now
  refuse **before** the upstream call rather than let it misreport (dig_ecosystem#3286, #3303,
  #3305). Upstream's multiply is a plain `u64` in upstream's crate, so under `overflow-checks` it
  panics before returning and no post-hoc check can run:
  - `withdraw_committed_incentives` refuses when `rewards * withdrawal_share_bps` is not
    representable (`DriverShareNotRepresentable`), and below that bound cross-checks the driver's
    returned figure against `recoverable_base_units`, refusing on disagreement
    (`DriverShareDisagrees`).
  - `read_distributor` refuses `withdrawal_share_bps > 10_000` read off the launch constants
    (`UnreadableDistributorConstants`) and refuses a generation whose reserve high-water mark
    exceeds `u64::MAX / 10_000` (`DistributorReserveTooLargeToRead`). Both are **errors**, never
    `Ok(None)`. Without them a read panicked in a checked build -- a remote denial of service on
    every caller of the public reader, reachable from unauthenticated chain input with DIG's own
    9_000 bps and a large enough commitment -- or, in release, reconstructed a fabricated
    `created_reward_slot.rewards` as authenticated distributor state.

### Documentation
- Corrected `src/clawback.rs`'s claims that the driver returns "the puzzle's own figure, never a
  recomputation" and that it "cannot build the spend at that scale". Both were false: the returned
  `u64` is an independent Rust re-derivation, and above the bound the driver builds a spend the
  chain honours while misreporting what it pays.
- SPEC.md 0.1 clause 1's exception paragraph corrected, and clause 5 added.

### Miscellaneous
- New public `MAX_REPORTABLE_COMMITMENT_BASE_UNITS` (`u64::MAX / 10_000`), derived rather than
  spelled.
- New CI job `Tests (release profile)`: `cargo build --release` ran no tests, so the wrap half of
  #3286 (overflow-checks off) was never measured.
## [0.4.1] - 2026-09-11

### Documentation
- Uptime warning, version-claim fix, SPEC status-row correction (#9)

## [0.4.0] - 2026-09-11

### Features
- Chain reader + SPEC 0.5 cohort-pin correction (0.4.0) (#6)

## [0.3.0] - 2026-09-10

### Features
- Recoverable_base_units — bound clawback share to the paying code (#7)

## [0.2.0] - 2026-09-09

### Features
- Reward-distributor driver — mint, fund, clawback, entries, epoch, payout (#3)

## [0.1.3] - 2026-09-09

### Documentation
- Scope SPEC 12.5's absent entry slot to the claim attempt, not the distributor (#5)

## [0.1.2] - 2026-09-09

### Documentation
- Reconcile SPEC 2.2 clause 1 with 2.1, and make the commitment-slot RPC normative (#4)

## [0.1.1] - 2026-09-08

### Documentation
- Normative SPEC.md for the managed reward distributor (v0.1.1)

## [0.1.0] - 2026-09-08

### Chores
- Bootstrap repo (README, licenses, gitignore)- Scaffold dig-rewards-coin (CI, release model, dep ceiling, error type)


