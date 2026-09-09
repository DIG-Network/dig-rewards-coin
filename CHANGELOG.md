# Changelog

All notable changes to this project are documented here.
This project adheres to [Semantic Versioning](https://semver.org) and
[Conventional Commits](https://www.conventionalcommits.org).

## [0.4.0] - 2026-09-10

### Features
- Chain reader `state::read_distributor` — rebuilds a full `DistributorSnapshot` from a
  distributor's launcher id alone, over a caller-supplied `ChainSource`; never calls
  `RewardDistributor::from_parent_spend`, which fabricates a zero `LineageProof` for the reserve
  (#3267)

### Fixed
- SPEC.md §0.5 clause 1 corrected: the exact-pin (`=`) requirement now names only the byte-bearing
  cohort crates that curry puzzle hashes (`chia-puzzle-types`, `chia-sdk-driver`,
  `chia-sdk-types`), with a version table and a `tests/cohort_lock_guard.rs` guard; `clvmr`'s
  documented pin corrected `0.16.2` → `0.16.4` to match what actually resolves (#3272)

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


