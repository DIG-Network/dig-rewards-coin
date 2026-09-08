//! # dig-rewards-coin — the reward-distributor coin driver, on Chia
//!
//! This crate will own the reward-distributor coin driver for the DIG Network outright, the same
//! way its `10-primitives` sibling `dig-mirror-coin` owns the mirror-coin driver: no
//! `datalayer-driver` dependency, no re-export layer over anything else.
//!
//! **This is scaffolding only.** No driver logic has landed yet — this commit exists so the
//! crate is publishable, gated (CI, commitlint, version-increment) and releasable (tag-driven
//! `crates.io` publish) before any behaviour does. See the parent epic
//! <https://github.com/DIG-Network/dig_ecosystem/issues/3246> and the scaffolding ticket
//! <https://github.com/DIG-Network/dig_ecosystem/issues/3247>.
//!
//! ## Layering
//!
//! This crate sits at `10-primitives`. Chain reads will arrive through the canonical
//! `ChainSource` trait (`dig-chainsource-interface`, `00-foundation`) — a `10-primitives` crate
//! never pulls a network stack down into itself, and it may never depend on a same-level crate
//! (`chia-query`, `dig-mirror-coin`) or anything above its own level.
//!
//! ## `action-layer`
//!
//! The reward distributor is exposed by `chia-sdk-driver`'s `action-layer` feature; without that
//! feature enabled the crate cannot see it at all. See `Cargo.toml` for the pinned dependency
//! table this crate builds against.

#![warn(missing_docs)]

/// Placeholder module for the reward-distributor coin driver.
///
/// Empty until the driver logic in a follow-up ticket lands. Kept as a named module (rather than
/// leaving `lib.rs` bare) so the crate's public shape is visible from commit one.
pub mod distributor {
    // Intentionally empty: no driver logic yet (see the ticket referenced in the crate docs).
}
