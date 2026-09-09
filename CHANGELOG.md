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
- Launch, fund, clawback, entry-set, epoch, payout and chain-read builders. `AddEntry`/`RemoveEntry`
  require a `ManagerAuthority`; `Sync`/`NewEpoch`/`InitiatePayout` are permissionless and take none.
- Entry-set writes settle their own validity window (SPEC.md §8.2 clause 1) instead of emitting a
  `Sync` unconditionally. A `Sync` may only move the distributor's clock **strictly forward**, so
  bundling one with a write that is already inside `max_seconds_offset` would make a valid write
  impossible. `EntrySetWrite::sync_conditions` is therefore an `Option<Conditions>`: `None` inside
  the window, `Some` in the same bundle past it, and a new
  `RewardsError::EntrySetWriteWindowClosed` when no `Sync` could help because `last_update` has
  reached `epoch_end` — naming the permissionless `NewEpoch` as the remedy the caller can spend
  itself.
- Simulator acceptance suite against the real CHIP-0051 puzzles (SPEC.md §15 clause 9), modelled on
  upstream `test_managed_reward_distributor()`: launch, fund, add entry, roll epoch, self-claim
  payout, remove entry and observe §6.4's settlement.

### Fixed
- `cargo clippy --all-targets -- -D warnings` and `cargo doc --no-deps` were already failing before
  this change: `launch_dig_distributor` trips `too_many_arguments` at 8/7, and a public doc comment
  in `state.rs` linked to the private `MAX_GENERATIONS_PER_READ`, rendering a broken link.

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
