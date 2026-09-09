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
- Launch, fund, clawback, entry-set, epoch and payout spend builders, plus the observable-state
  accessors over a `RewardDistributor` a caller already holds. No chain reader ships — see
  **Changed** below. `AddEntry`/`RemoveEntry`
  require a `ManagerAuthority`; `Sync`/`NewEpoch`/`InitiatePayout` are permissionless and take none.
- Entry-set writes settle their own validity window (SPEC.md §8.2 clause 1) instead of emitting a
  `Sync` unconditionally. A `Sync` may only move the distributor's clock **strictly forward**, so
  bundling one with a write that is already inside `max_seconds_offset` would make a valid write
  impossible. `EntrySetWrite::sync_conditions` is therefore an `Option<Conditions>`: `None` inside
  the window, `Some` in the same bundle past it, and a new
  `RewardsError::EntrySetWriteWindowClosed` when no `Sync` could help because `last_update` has
  reached `epoch_end` — naming the permissionless `NewEpoch` as the remedy the caller can spend
  itself.
- `EligiblePayoutHash`: the payout puzzle hash an entry carries is bound to the eligibility
  verdict in the type. Only `eligibility::judge_candidate` can mint one — the field is private to
  its module, and there is no public constructor, no `From<Bytes32>` and no public field — and
  `entries::add_entry` accepts nothing else. A caller holding a legitimate `ManagerAuthority` can
  therefore no longer route a DIG payout to a hash of its own choosing, which is the one thing the
  manager authority was never meant to grant. Tightening a published parameter is breaking, so it
  ships in 0.2.0 or never.
- Simulator acceptance suite against the real CHIP-0051 puzzles (SPEC.md §15 clause 9), modelled on
  upstream `test_managed_reward_distributor()`: launch, fund, add entry, roll epoch, self-claim
  payout, remove entry and observe §6.4's settlement.

### Changed
- **No chain reader ships in 0.2.0.** `state::read_distributor` is removed from the public surface.
  It failed at its first hop for every distributor: it applied `from_parent_spend` to the eve coin's
  spend, which carries the launch inner puzzle rather than the action-layer one, so the call
  returned `None` and every read reported `Malformed`. The correct hop is upstream's
  `from_eve_coin_spend`, which additionally needs the reserve CAT's `reserve_parent_id` and
  `reserve_lineage_proof` — provenance a reader starting from a launcher id cannot currently
  discover — so SPEC.md §12.1 clause 1 is deferred to
  [#3267](https://github.com/DIG-Network/dig_ecosystem/issues/3267). A crates.io version is
  immutable and a present-but-broken public function is a worse lie than an absent one: every
  consumer who found it in the docs would write code against a function that cannot work. Re-adding
  a public item later is purely additive, so nothing is foreclosed. `DistributorSlots`,
  `DistributorSnapshot` and its accessors stay public — they work, and they are useful to any caller
  holding a `RewardDistributor` obtained another way, including straight out of a launch.
- `launch_dig_distributor` takes `first_epoch_start: u64` rather than a whole
  `DistributorLaunchTerms`. Two of the terms — the manager singleton launcher id and
  `distributor_epoch_seconds` — are already curried into `constants`, which is what the launch
  spend reads, so the `terms` argument carried a second copy that was silently ignored and a caller
  could pass terms disagreeing with the constants. `DistributorLaunchTerms` is unchanged as the
  required-fields input to `dig_distributor_constants`, where the three launch-time choices are
  made once. `first_epoch_start` is the one launch value the constants table does not hold.
- `launch_dig_distributor` no longer takes a `funder_refund_puzzle_hash` parameter. The CAT change
  destination is now derived from `constants.fee_payout_puzzle_hash` through the single
  `funder_refund_puzzle_hash(constants)` accessor, so SPEC.md §15 clause 3's requirement that the
  two be equal holds by construction instead of resting on a check a caller can forget. Two sources
  of truth for where the change CAT goes is a money bug waiting on a mismatch. The constants
  builder's existing zero-hash refusal now covers both uses at once, since there is only one field.

### Fixed
- `cargo clippy --all-targets -- -D warnings` and `cargo doc --no-deps` were already failing before
  this change: `launch_dig_distributor` tripped `too_many_arguments` at 8/7 (dropping the redundant
  refund-hash parameter takes it to 7, so the suppression is gone rather than silenced), and a
  public doc comment in `state.rs` linked to the private `MAX_GENERATIONS_PER_READ`, rendering a
  broken link.

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
