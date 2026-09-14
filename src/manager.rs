//! The manager singleton — the entry set's custody boundary (`SPEC.md` §7.2, §7.2a).
//!
//! At 0.5.0 this crate shipped no way to mint one: `DistributorLaunchTerms` *requires* a launcher
//! id (`src/constants.rs`) and refuses a zero one, while nothing in `src/` produced one at all — so
//! no distributor could be minted through this crate. This module is that launch.
//!
//! Two things this module refuses to do, because §7.2a clause 3 forbids them:
//!
//! - it never accepts a launcher id, a launcher coin or a singleton coin from a caller — every
//!   value [`LaunchedManagerSingleton`] exposes is derived from the launch spend built here;
//! - it never claims a caller-supplied inner puzzle hash is recovery-capable, multisig or
//!   rekeyable. It cannot see the puzzle 32 opaque bytes commit to, so [`ManagerInnerPuzzle`]
//!   names its two arms by **provenance** — built here from a key, or supplied by the caller as a
//!   hash — never by a capability.

use chia_bls::PublicKey;
use chia_protocol::{Bytes32, Coin};
use chia_puzzle_types::{EveProof, Proof};
use chia_sdk_driver::{Launcher, SpendContext};
use chia_sdk_types::Conditions;

use crate::constants::DistributorLaunchTerms;
use crate::launch::standard_puzzle_hash;
use crate::RewardsError;

/// The manager singleton's fixed launch amount, in XCH mojos (`SPEC.md` §7.2a clause 5).
///
/// A singleton's amount must be odd or it can never be spent; upstream's own reward-distributor
/// launch uses `1` (`chia-sdk-driver-0.36.0`'s `launch_drivers.rs:644,725`). This is fixed here and
/// is never a caller parameter — a caller MUST NOT be given the opportunity to pick an even one.
pub const MANAGER_SINGLETON_AMOUNT_MOJOS: u64 = 1;

/// The manager singleton's inner puzzle, named by **provenance** rather than by capability
/// (`SPEC.md` §7.2a clause 3).
///
/// There is deliberately no `Default` impl (§7.2a clause 2): a caller that supplies nothing MUST
/// fail to compile, never receive a silent default for the one choice that is permanent after
/// launch (§7.2 clause 1a).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerInnerPuzzle {
    /// A `p2_delegated_puzzle_or_hidden_puzzle` this crate curries to one public key.
    ///
    /// This crate built the puzzle, so it knows the puzzle has exactly one key and therefore **no
    /// recovery path** — the case §7.2 clause 3's permanent freeze is about.
    SingleKeyBuiltHere(PublicKey),

    /// An inner puzzle hash the caller supplies directly.
    ///
    /// This crate receives 32 opaque bytes. It cannot run them, cannot see the puzzle they commit
    /// to, and cannot tell a 2-of-3 from a single key from a hash of nothing at all — so nothing in
    /// this crate may describe a value carried here as recovery-capable, multisig, k-of-n, vault,
    /// rekeyable or time-delayed (§7.2a clause 3).
    HashSuppliedByCaller(Bytes32),
}

/// The manager singleton, as launched — every field private, no public constructor.
///
/// The only way to obtain one is [`launch_manager_singleton`], so every value here is derived from
/// a real launch spend rather than echoed back from a caller (§7.2a clause 1).
#[derive(Debug)]
pub struct LaunchedManagerSingleton {
    launcher_id: Bytes32,
    inner_puzzle_hash: Bytes32,
    singleton_coin: Coin,
    eve_proof: Proof,
    parent_conditions: Conditions,
}

impl LaunchedManagerSingleton {
    /// The manager singleton's launcher id, **derived** from the launch spend this crate built.
    #[must_use]
    pub const fn launcher_id(&self) -> Bytes32 {
        self.launcher_id
    }

    /// The inner puzzle hash the singleton was launched with.
    #[must_use]
    pub const fn inner_puzzle_hash(&self) -> Bytes32 {
        self.inner_puzzle_hash
    }

    /// The eve singleton coin created by the launch.
    #[must_use]
    pub const fn singleton_coin(&self) -> Coin {
        self.singleton_coin
    }

    /// The eve singleton's own lineage proof (`Proof::Eve`).
    #[must_use]
    pub const fn eve_proof(&self) -> Proof {
        self.eve_proof
    }

    /// The conditions the **parent** coin's spend must carry — a `CREATE_COIN` of the launcher
    /// coin plus an `ASSERT_COIN_ANNOUNCEMENT` over the launcher solution (§7.2a clause 6). This
    /// crate never signs or spends the parent; the caller's own key does.
    #[must_use]
    pub const fn parent_conditions(&self) -> &Conditions {
        &self.parent_conditions
    }

    /// The two launch-time-only choices [`DistributorLaunchTerms`] needs from this launch: the
    /// **derived** launcher id and the caller's chosen distributor epoch length.
    ///
    /// This is §7.2a clause 8's step 3 — the path from a launch result to the terms, so a caller
    /// never re-types the launcher id by hand.
    #[must_use]
    pub const fn distributor_launch_terms(
        &self,
        distributor_epoch_seconds: u64,
    ) -> DistributorLaunchTerms {
        DistributorLaunchTerms {
            manager_singleton_launcher_id: self.launcher_id,
            distributor_epoch_seconds,
        }
    }
}

/// Launch the manager singleton that will authorize this distributor's entry-set writes.
///
/// The caller chooses the inner puzzle (`SPEC.md` §7.2a clause 2) and supplies the parent coin id
/// whose spend will create the launcher coin. This function inserts the launcher coin's own spend
/// into `ctx` and returns the conditions the **parent** spend must carry (§7.2a clause 6); it signs
/// and spends nothing itself (§0.1 clause 2).
///
/// # Errors
///
/// - [`RewardsError::InvalidLaunchTerms`] if `parent_coin_id` is the zero hash, or if the resolved
///   inner puzzle hash is the zero hash — a singleton with a zero inner puzzle hash can never be
///   spent, so the entry set would be frozen from the first block (§7.2a clause 4).
/// - [`RewardsError::Driver`] if the upstream launcher spend could not be built.
pub fn launch_manager_singleton(
    ctx: &mut SpendContext,
    parent_coin_id: Bytes32,
    inner_puzzle: ManagerInnerPuzzle,
) -> Result<LaunchedManagerSingleton, RewardsError> {
    if parent_coin_id == Bytes32::default() {
        return Err(RewardsError::InvalidLaunchTerms(
            "manager singleton parent_coin_id must not be the zero hash".to_string(),
        ));
    }

    let inner_puzzle_hash = match inner_puzzle {
        ManagerInnerPuzzle::SingleKeyBuiltHere(public_key) => standard_puzzle_hash(public_key),
        ManagerInnerPuzzle::HashSuppliedByCaller(hash) => hash,
    };

    if inner_puzzle_hash == Bytes32::default() {
        return Err(RewardsError::InvalidLaunchTerms(
            "manager singleton inner puzzle hash must not be the zero hash: a singleton with a \
             zero inner puzzle hash can never be spent, freezing the entry set from the first \
             block"
                .to_string(),
        ));
    }

    let launcher = Launcher::new(parent_coin_id, MANAGER_SINGLETON_AMOUNT_MOJOS);
    let launcher_coin = launcher.coin();
    let launcher_id = launcher_coin.coin_id();

    // §7.2a clause 7: the launcher solution's key_value_list MUST be nil. Nothing needs to
    // discover this singleton independently -- the distributor's own constants already carry its
    // launcher id.
    let (parent_conditions, singleton_coin) = launcher.spend(ctx, inner_puzzle_hash, ())?;

    let eve_proof = Proof::Eve(EveProof {
        parent_parent_coin_info: launcher_coin.parent_coin_info,
        parent_amount: launcher_coin.amount,
    });

    Ok(LaunchedManagerSingleton {
        launcher_id,
        inner_puzzle_hash,
        singleton_coin,
        eve_proof,
        parent_conditions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_parent_coin_id_is_refused() {
        let mut ctx = SpendContext::new();

        let result = launch_manager_singleton(
            &mut ctx,
            Bytes32::default(),
            ManagerInnerPuzzle::HashSuppliedByCaller(Bytes32::new([1; 32])),
        );

        assert!(matches!(result, Err(RewardsError::InvalidLaunchTerms(_))));
    }

    #[test]
    fn a_zero_inner_puzzle_hash_is_refused() {
        let mut ctx = SpendContext::new();

        let result = launch_manager_singleton(
            &mut ctx,
            Bytes32::new([1; 32]),
            ManagerInnerPuzzle::HashSuppliedByCaller(Bytes32::default()),
        );

        assert!(matches!(result, Err(RewardsError::InvalidLaunchTerms(_))));
    }

    #[test]
    fn a_hash_supplied_launch_derives_a_nonzero_launcher_id() {
        let mut ctx = SpendContext::new();

        let launched = launch_manager_singleton(
            &mut ctx,
            Bytes32::new([7; 32]),
            ManagerInnerPuzzle::HashSuppliedByCaller(Bytes32::new([9; 32])),
        )
        .unwrap();

        assert_ne!(launched.launcher_id(), Bytes32::default());
        assert_eq!(launched.inner_puzzle_hash(), Bytes32::new([9; 32]));
        assert_eq!(launched.singleton_coin().amount, MANAGER_SINGLETON_AMOUNT_MOJOS);
        assert!(matches!(launched.eve_proof(), Proof::Eve(_)));

        let terms = launched.distributor_launch_terms(604_800);
        assert_eq!(terms.manager_singleton_launcher_id, launched.launcher_id());
        assert_eq!(terms.distributor_epoch_seconds, 604_800);
    }
}
