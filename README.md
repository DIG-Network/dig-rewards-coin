# dig-rewards-coin

Reward-distributor coin driver for the DIG Network, on Chia.

The driver ships as of 0.2.0: the DIG-shaped constants table, the launch comment, the launch /
fund / clawback / entry-set / epoch / payout spend builders, the eligibility rule and the
observable state. `SPEC.md` at the repository root is normative. See the parent epic
[#3246](https://github.com/DIG-Network/dig_ecosystem/issues/3246).

**0.2.0 publishes no chain reader.** `read_distributor` was non-functional and is withheld rather
than shipped broken — [#3267](https://github.com/DIG-Network/dig_ecosystem/issues/3267) tracks it,
and re-adding a public item later is purely additive.

## Shape, copied from `dig-mirror-coin`

Like its `10-primitives` sibling
[`dig-mirror-coin`](https://github.com/DIG-Network/dig-mirror-coin), this crate owns the
reward-distributor driver outright: no `datalayer-driver` dependency, no re-export layer over
anything else.

## Layering

This crate sits at `10-primitives`. It performs no socket I/O: chain reads will arrive through the
canonical caller-supplied `ChainSource` trait (`dig-chainsource-interface`, `00-foundation`) when
the reader lands with #3267 — a `10-primitives` crate never pulls a network stack down into
itself. Two same-level edges are illegal from here and deliberately not taken:
`chia-query` (also `10-primitives`, despite being "the canonical coinset access layer") and
`dig-mirror-coin` (also `10-primitives` — that gate belongs to a `dig-node`-level consumer, not
to this crate).

## Dependency ceiling

Pinned to the `chia-wallet-sdk` 0.36 ceiling, not to crates.io latest — `chia-sdk-driver` 0.36.0
pins a `0.36.1` cohort across `chia-bls`/`chia-protocol`/`chia-puzzle-types`/`clvm-traits`/
`clvm-utils`, `chia-puzzles` 0.20.3 and `clvmr` 0.16.2. Taking the primitives' newer 0.48.x
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
