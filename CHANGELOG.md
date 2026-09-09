# Changelog

All notable changes to this project are documented here.
This project adheres to [Semantic Versioning](https://semver.org) and
[Conventional Commits](https://www.conventionalcommits.org).

## [0.1.2] - 2026-09-08

### Documentation
- SPEC amendment 1 (section 2.2 clause 1): this node's prover governs *who* is paid, not
  *whether* anyone is paid. The withdrawn wording is recorded in place, the closing ban is
  widened to any paraphrase making payment conditional on this node, and section 15.4 row
  A1 carries the correction. Section 2.2 clauses 2-5 are unchanged.
- SPEC amendment 2 (section 2.6): a fourth normative method,
  `dig.listRewardDistributorCommitments` at `Tier::Control`, matching the shipped
  `dig-rpc-protocol` v0.11.0 wire field for field. The responder MUST compute
  `recoverable_base_units` as `rewards_base_units * withdrawal_share_bps / 10_000` with
  integer arithmetic in that order, truncated; the echoed `withdrawal_share_bps` and
  `epoch_seconds` MUST be used instead of compiled-in constants; the chain's `clawback_ph`
  and the wire's `clawback_puzzle_hash` are named as one value; an empty commitment list is
  legitimate. Section 15.1's `dig-rpc-protocol` line, section 15.2, section 14's MVP table
  and section 15.4 row A2 are updated to match.
- Documentation only: no constant, no default and no driver shape changed.

## [0.1.1] - 2026-09-08

### Documentation
- Normative SPEC.md for the managed reward distributor (v0.1.1)

## [0.1.0] - 2026-09-08

### Chores
- Bootstrap repo (README, licenses, gitignore)- Scaffold dig-rewards-coin (CI, release model, dep ceiling, error type)


