# Changelog

All notable changes to this project are documented here.
This project adheres to [Semantic Versioning](https://semver.org) and
[Conventional Commits](https://www.conventionalcommits.org).

## [0.3.0] - 2026-09-10

### Features
- `clawback::recoverable_base_units` — the one authoritative, tested restatement of the puzzle's
  withdrawal-share arithmetic, bound to `withdraw_incentives.rs:105-107` by a simulator equality
  test for every amount a real clawback can pay on, i.e. up to
  `u64::MAX / withdrawal_share_bps`. Above that bound the `chia-sdk-driver` 0.36.0 Rust driver's
  own `u64` multiply overflows and cannot build the spend (#3286) — the on-chain puzzle itself
  still pays correctly at any scale — so the `u128` intermediate is proven there arithmetically
  instead (#3269)

### Fixes
- `recoverable_base_units` now returns `Option<u64>`, `None` for `withdrawal_share_bps > 10_000`:
  the prior `.expect()` panicked reachably on an attacker-controlled bps constant above 10_000
  (e.g. `u64::MAX` rewards at `65_535` bps), and its safety comment's claim that the quotient could
  never exceed `rewards_base_units` was false above 10_000 bps (#3269)

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


