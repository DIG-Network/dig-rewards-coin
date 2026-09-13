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
    (`UnreadableDistributorConstants`), and refuses a generation as soon as it CREATES a commitment
    slot whose own recorded `rewards` exceeds `MAX_REPORTABLE_COMMITMENT_BASE_UNITS`
    (`u64::MAX / 10_000`, `CommitmentRewardsTooLargeToRead`) -- bounding the committed value
    directly, not the reserve coin's amount, because upstream can batch a `CommitIncentives` with
    other reserve-affecting actions into one distributor-coin spend, so the reserve amount after a
    generation reflects only that generation's net effect. Both are **errors**, never `Ok(None)`.
    Without them a read panicked in a checked build -- a remote denial of service on every caller
    of the public reader, reachable from unauthenticated chain input with DIG's own 9_000 bps and a
    large enough commitment -- or, in release, reconstructed a fabricated
    `created_reward_slot.rewards` as authenticated distributor state.
- `read_distributor` now domain-checks two further launch constants, on the constants and before
  a single generation is parsed, for the same reason B1 is checked there (dig_ecosystem#3313):
  - `max_seconds_offset` above `MAX_SANE_SECONDS_OFFSET` (`u32::MAX` seconds, about 136 years)
    is refused (`UnreadableMaxSecondsOffset`). Nothing in `chia-sdk-driver` 0.36.0 bounded it, so
    an attacker-launched distributor could declare `u64::MAX` as a clock-skew tolerance.
  - `epoch_seconds == 0` is refused (`UnreadableEpochSeconds`). Upstream's reward-slot backfill
    loop never advances at that value, and being a pure non-yielding CPU loop no caller-side
    `tokio::time::timeout` can cancel it -- a refusal by this reader is the only defence that can
    work, so it runs as early as this reader can run it.
- `read_distributor` now runs a fail-closed pre-screen, `refuse_unrepresentable_action_arithmetic`,
  over every generation's action-layer solution BEFORE calling `chia-sdk-driver`'s
  `RewardDistributor::from_spend`, closing seven further unchecked-arithmetic and
  non-termination hazards inside upstream's `get_log` methods that B1/B2 above do not reach
  (dig_ecosystem#3313): `withdraw_incentives.rs:71,89`'s multiply and subtract,
  `commit_incentives.rs:85,101`'s two adds plus its `commit_incentives.rs:103-112` backfill loop
  (bounded by `MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS`, an ABSOLUTE constant this crate chooses --
  never a ratio of the distributor's own declared constants, and never a bare decimal literal),
  `unstake.rs:235`'s subtract (closed by actually running the action's own unlock puzzle to
  recover `removed_shares`, since it is not a static solution field), and `stake.rs:326,329`'s
  counter increment (`i128`, panics at `i128::MAX`) and add (closed the same way, via the action's
  own lock puzzle). New `RewardsError` variants: `ActionArithmeticNotRepresentable` (extended to
  name all four affected actions), `UnreadableEpochSeconds`, `UnreadableMaxSecondsOffset`,
  `CommitIncentivesBackfillBoundExceeded`. The pre-screen also fails closed on any action puzzle
  hash it does not recognise as one of the eleven reward-distributor actions
  `chia-sdk-driver` 0.36.0 defines (`UnrecognisedActionPuzzle`) -- a future pin bump that changes
  an action puzzle makes every read refuse loudly rather than silently skip an unscreened hazard.

### Documentation
- Corrected `src/clawback.rs`'s claims that the driver returns "the puzzle's own figure, never a
  recomputation" and that it "cannot build the spend at that scale". Both were false: the returned
  `u64` is an independent Rust re-derivation, and above the bound the driver builds a spend the
  chain honours while misreporting what it pays.
- SPEC.md 0.1 clause 1's exception paragraph corrected, and clause 5 added.

### Miscellaneous
- New public `MAX_REPORTABLE_COMMITMENT_BASE_UNITS` (`u64::MAX / 10_000`), derived rather than
  spelled.
- New public `state::MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS` (`1_000_000`) and
  `state::MAX_SANE_SECONDS_OFFSET` (`u32::MAX` seconds). Both are absolute figures this reader
  chooses, deliberately independent of any distributor's own declared constants: a bound derived
  from parameters an attacker picks is not a bound. At DIG's own one-week epoch the backfill cap
  is roughly nineteen thousand years of backfilled epochs, so no honest commitment approaches it.
- `clvm-traits` and `clvmr` moved from `[dev-dependencies]` to `[dependencies]`: the pre-screen
  runs the `unstake`/`stake` unlock and lock puzzles from the library itself, so they must be
  nameable outside the test harness. Both were already resolved at these exact versions
  transitively; `Cargo.lock` is unchanged.
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


