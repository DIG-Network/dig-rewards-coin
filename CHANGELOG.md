# Changelog

All notable changes to this project are documented here.
This project adheres to [Semantic Versioning](https://semver.org) and
[Conventional Commits](https://www.conventionalcommits.org).

## [0.1.3] - 2026-09-09

### Documentation
- SPEC amendment 3 (section 12.5): "entry slot absent" is terminal for the claim **attempt**,
  never for the distributor. Every guarantee the old clause 1 carried is kept — no spend, no
  chain fault, no report of a lost payment, because section 6.4 clause 1 already settled
  everything accrued. New clause 1a requires continued observation on section 8.6's cadence;
  clause 4 reconciles it with clause 3 (an absence MUST NOT be cached any more than a value);
  clause 5 bans a permanent per-distributor exclusion set; clause 6 requires the absence be
  surfaced in section 2.3/2.4's existing vocabulary; clause 7 forbids guessing "never admitted"
  apart from "evicted after settlement". The withdrawn wording is recorded in place and section
  15.4 row A3 carries the correction. Clauses 2 and 3 are unchanged; no other section is touched.

## [0.1.2] - 2026-09-09

### Documentation
- Reconcile SPEC 2.2 clause 1 with 2.1, and make the commitment-slot RPC normative (#4)

## [0.1.1] - 2026-09-08

### Documentation
- Normative SPEC.md for the managed reward distributor (v0.1.1)

## [0.1.0] - 2026-09-08

### Chores
- Bootstrap repo (README, licenses, gitignore)- Scaffold dig-rewards-coin (CI, release model, dep ceiling, error type)


