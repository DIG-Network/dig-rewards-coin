# Changelog

All notable changes to this project are documented here.
This project adheres to [Semantic Versioning](https://semver.org) and
[Conventional Commits](https://www.conventionalcommits.org).

## [0.7.0] - 2026-09-16

### Security
- Bound `puzzle_reveal`/`solution` size before decode in on-chain discovery, independent of the
  existing CLVM cost bound (#3333, `DECODE_MAX_SERIALIZED_BYTES = 65_536`)
- Report an entry set as stale, not fresh, when its last-write timestamp is inverted relative to the
  observed peak (#3304 item 1)
- Authenticate every zero-amount eve-era reserve candidate as a genuine CAT child of the reserve
  asset id before selecting one, closing the **refusal** shape of a decoy read-DoS on
  `find_eve_reserve_provenance` (#3304 item 2). The per-candidate authentication loop this
  introduces reuses `DECODE_MAX_SERIALIZED_BYTES` to bound each candidate's parent-spend
  allocation; before that reuse, the loop itself was an unbounded **work-amplification** DoS this
  same change had introduced, worse than the read-DoS it was closing.
- Distinguish a chain source `Err` from a genuine `Ok(None)` decoy on
  `find_eve_reserve_provenance`'s parent-spend lookup: a transient read failure on the one
  authenticating candidate no longer reports as "no candidate authenticates" (malformed chain
  data) -- it now propagates `ChainUnavailable`, the honest "retry" answer, matching #3304 item
  1's absence-vs-failure discipline on this neighbouring path.

### Documentation
- Cross-reference the two same-named "base units of reward" quantities in `recoverable_base_units`
  and `rewards_per_distributor_epoch` (#3304 item 3)
- `RUSTDOCFLAGS=-D warnings` on the doc gate, `--locked` on every resolving CI step (#3335, #3304
  item 4)

### Tests
- Assert the real launch spend's discarded CLVM decode cost as a measured literal, `55_338` (#3334)

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


