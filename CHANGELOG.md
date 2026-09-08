# Changelog

All notable changes to this project are documented here.
This project adheres to [Semantic Versioning](https://semver.org) and
[Conventional Commits](https://www.conventionalcommits.org).

## [0.2.0] - 2026-09-08

### Features
- DIG-shaped `RewardDistributorConstants` builder: the SPEC.md §15 clause 3 table as one
  construction path (`without_launcher_id` then `with_launcher_id`), with the three launch-time-only
  choices as required fields of `DistributorLaunchTerms` and a separate opt-in constructor for a
  non-zero epoch fee.
- `LaunchComment`: the normative `dig-rewards:v1:<store_id_hex>:<root_hex>` launch comment
  (SPEC.md §1.3) — emitted lowercase, parsed in either case, compared as 32 bytes, with a
  non-parsing comment classified as "not a DIG rewards distributor" rather than an error.

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
