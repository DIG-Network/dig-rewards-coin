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
- `state::read_distributor` — §12.4's staleness signal is derived from each generation's action log
  and counts only `AddEntry`/`RemoveEntry`. It was derived from the created/spent entry-slot deltas,
  which `InitiatePayout` also writes, so a single entry holder claiming inside every 48 h window
  reset the signal for a distributor whose prover was dead (#3267)
- `state::read_distributor` — the reward slot the launch creates is carried into the snapshot; it
  was discarded, so a chain-rebuilt prover could not roll the first distributor epoch and
  `rewards_per_distributor_epoch()` under-reported (#3267)
- `state::read_distributor` — a generation that spends a slot the walk never saw created is now
  `Malformed` rather than a silent no-op: a diverged read is an error, not a smaller answer (#3267)
- `DistributorSnapshot` / `ChainObservation` — fields are private behind accessors with no public
  constructor, so the pairing of chain data with the observation it was read under is unforgeable.
  With `pub` fields a stale snapshot's observation could be overwritten with a fresh one's and
  `is_current()` then returned `true` for a state generations old (#3267)
- `DistributorSnapshot::is_current` compares the tip coin id alone. Conjoining `peak_height` made it
  report `false` within seconds of every mainnet read even for a byte-identical state (#3267)
- `DistributorSnapshot::entry_set_frozen` covers every `RewardDistributorType`, not only `Managed`
  (#3267)
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


