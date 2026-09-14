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
- Bound `discovered_distributors_in_spend`'s CLVM run of an observed (attacker-chosen) spend to an
  explicit `DECODE_MAX_COST`, rather than relying on `ctx.run`'s implicit full-block-cost ceiling
  (`src/discovery.rs`) (#11)
- Mark `ManagerInnerPuzzle` `#[non_exhaustive]`, matching `RewardsError` and this crate's other
  enums, since a `MultisigBuiltHere` arm is the most likely future addition and this is a published
  crate consumers pin (`src/manager.rs`) (#11)

### Documentation
- Qualify SPEC.md's §13.1 status: only clauses 4-10 (the decode) ship at 0.6.0; clauses 1-3 (the
  scan) remain #3250's, corrected everywhere the row previously read as "ships" without that split
  (§14, §15.2) (#11)
- Document `launch_manager_singleton`'s permanent-freeze risk and the one-bundle requirement for
  the parent spend and the distributor launch (SPEC.md §7.2 clause 3, §7.2a clause 9) (#11)

### Tests
- End-to-end mint + discovery coverage through the simulator, plus #3309's discovery negatives
- A cost-bound regression for the discovery decode: a puzzle whose charged cost would exceed
  `DECODE_MAX_COST` is refused (#11)

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


