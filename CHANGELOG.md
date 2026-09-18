# Changelog

All notable changes to this project are documented here.
This project adheres to [Semantic Versioning](https://semver.org) and
[Conventional Commits](https://www.conventionalcommits.org).

## [0.8.0] - 2026-09-18

### Features
- Chain-backed spendable entry slot and real lineage proofs (§12.1, §12.5, #3356):
  `read_distributor`'s walk now bookkeeps slots over `Slot<V>` in one place, so
  every slot carries the `LineageProof` of the generation that actually created
  it, never one derived from the tip. `DistributorSnapshot::entry_slot`,
  `commitment_slots` and `reward_slots` expose that Slot set directly.
  `ChainEntrySlotSource` implements `EntrySlotSource` by re-walking the whole
  distributor from chain on every read; a never-launched launcher id is now
  `RewardsError::NoDistributorAtLauncherId`, never `Ok(None)`.
  `accrued_base_units` is a pure restatement of `InitiatePayout`'s own payout
  arithmetic, bound to it by a simulator equality test.

## [0.7.0] - 2026-09-17

### Features
- 0.7.0 hardening — size bound, reader hardening, doc gate (#12)

## [0.6.0] - 2026-09-14

### Features
- Manager-singleton launcher, on-chain discovery, window fail-closed (0.6.0) (#11)

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


