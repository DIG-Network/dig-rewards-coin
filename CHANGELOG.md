# Changelog

All notable changes to this project are documented here.
This project adheres to [Semantic Versioning](https://semver.org) and
[Conventional Commits](https://www.conventionalcommits.org).

## [0.6.0] - 2026-09-13

### Features
- Launch the manager singleton (`launch_manager_singleton`, SPEC.md §7.2a) (#3308)
- On-chain discovery: decode a launch comment from an observed spend (`discover_distributor`,
  `discovered_distributors_in_spend`, SPEC.md §13.1) (#3308, #3249)

### Bug Fixes
- Fail closed on a saturating `max_seconds_offset` instead of leaving the entry-set write window
  permanently open (#3321)

### Tests
- End-to-end mint + discovery coverage through the simulator, plus #3309's discovery negatives

## [0.5.0] - 2026-09-13

### Bug Fixes
- Make #3286's wrapping u64 share arithmetic unreachable from both public paths (#8)

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


