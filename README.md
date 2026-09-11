# dig-rewards-coin

Reward-distributor coin driver for the DIG Network, on Chia.

The driver ships: the DIG-shaped constants table, the launch comment, the launch / fund /
clawback / entry-set / epoch / payout spend builders, the eligibility rule, the observable state,
and — as of 0.4.0 — the chain reader (`state::read_distributor`,
[#3267](https://github.com/DIG-Network/dig_ecosystem/issues/3267)) that rebuilds a distributor's
state from its launcher id alone. `SPEC.md` at the repository root is normative. See the parent
epic [#3246](https://github.com/DIG-Network/dig_ecosystem/issues/3246).

## Uptime — what a stopped prover does and does not do (SPEC §2.2)

This crate performs no I/O and runs no loop itself; the **prover loop** that reads `SPEC.md` §2.2
against runs in `dig-node`. Anyone integrating that prover, or a funder deciding whether to launch
a distributor, should hold these five facts (normative text: `SPEC.md` §2.2):

1. The prover determines **who** is paid, not **whether** anyone is paid. Accrual and payouts are
   permissionless and continue while the prover is stopped — they need no permission from anyone,
   including from the funder's machine.
2. Funds are not lost when the prover stops: they stay in the reserve, and future commitments
   remain clawback-eligible (§7.4).
3. While the prover is stopped, the entry set — the paid list — is frozen: peers that stopped
   mirroring keep earning, and peers that started mirroring cannot be added.
4. Losing the manager singleton key freezes the entry set **permanently** — unless the manager's
   inner puzzle was chosen recovery-capable **at launch** (§7.2). That choice exists only on the
   creation screen and is fixed for the life of the distributor once the launch spend is signed.
5. The clause-3 loss is bounded by the funder's commitment depth, `COMMITMENT_DEPTH_EPOCHS = 2`
   future epochs (§7.4) — the risk has a stated bound, not just an alarm.

**Downtime does not pause payment. It hands payment to a list that has stopped being true.**

## Shape, copied from `dig-mirror-coin`

Like its `10-primitives` sibling
[`dig-mirror-coin`](https://github.com/DIG-Network/dig-mirror-coin), this crate owns the
reward-distributor driver outright: no `datalayer-driver` dependency, no re-export layer over
anything else.

## Layering

This crate sits at `10-primitives`. It performs no socket I/O itself: chain reads arrive through
the canonical caller-supplied `ChainSource` trait (`dig-chainsource-interface`, `00-foundation`),
which the caller constructs and this crate only reads through — a `10-primitives` crate never
pulls a network stack down into itself. Two same-level edges are illegal from here and
deliberately not taken:
`chia-query` (also `10-primitives`, despite being "the canonical coinset access layer") and
`dig-mirror-coin` (also `10-primitives` — that gate belongs to a `dig-node`-level consumer, not
to this crate).

## Dependency ceiling

Pinned to the `chia-wallet-sdk` 0.36 ceiling, not to crates.io latest — `chia-sdk-driver` 0.36.0
pins a `0.36.1` cohort across `chia-bls`/`chia-protocol`/`chia-puzzle-types`/`clvm-traits`/
`clvm-utils`, `chia-puzzles` 0.20.3 and `clvmr` 0.16.4. Taking the primitives' newer 0.48.x
releases would link two incompatible `chia-protocol` versions and `Bytes32` stops being one type.

Only the crates the library itself names are `[dependencies]`. `chia-puzzles`, `clvm-traits`,
`clvm-utils` and `clvmr` are named solely by the tests and are `[dev-dependencies]`: the library
reaches them transitively through `chia-consensus`/`chia-sdk-driver` anyway, and a published
version is immutable, so declaring them would pin four extra versions on every consumer for
nothing.
`chia-sdk-driver` and `chia-sdk-types` both carry the `action-layer` feature — it is what exposes
the reward distributor at all.

## Release model

Copied verbatim from `dig-mirror-coin`: merge to `main` runs `release.yml`, which regenerates
`CHANGELOG.md` with `git-cliff`, commits it, and tags `vX.Y.Z` with a `RELEASE_TOKEN` PAT (a tag
pushed by the default `GITHUB_TOKEN` does not trigger downstream workflows). The tag triggers
`publish.yml`, which publishes to crates.io.

## License

MIT OR Apache-2.0.
