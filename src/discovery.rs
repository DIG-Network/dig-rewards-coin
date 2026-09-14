//! On-chain discovery — the decode half of `SPEC.md` §1.3's launch comment (§13.1 clauses 4-10).
//!
//! At 0.5.0 `LaunchComment` was rendered into a launch and parsed from text, but nothing in `src/`
//! recovered one from an observed spend, so §13.1 clause 2 ("a peer MUST be able to find, evaluate
//! and claim from a distributor using nothing but a chain source") was unsatisfiable by any
//! consumer of this crate. This module is that decode.
//!
//! ## Where the comment actually is
//!
//! It is **not** in the launcher coin's own solution -- that carries `(first_epoch_start .
//! constants)`, which [`crate::state::read_distributor`] already decodes and which never carries
//! the generation. The comment is carried in the memos of the `CREATE_COIN` condition that creates
//! the launcher coin, emitted by that coin's **parent** spend. A decoder is therefore given the
//! parent spend, and obtains the conditions by running that spend's puzzle against its solution.
//!
//! ## What a decoded result proves, and what it does not
//!
//! It proves exactly that the spend which created this launcher coin advertised, in a memo, that
//! the distributor it launched is about `storeId:root`. It does **not** prove the distributor is
//! ours, that it still exists or is funded, or that it is the only distributor advertising that
//! generation (§13.1 clause 9). A reader MUST still apply §9.3 before treating a discovered
//! distributor as its own.

use chia_protocol::{Bytes32, CoinSpend};
use chia_puzzle_types::Memos;
use chia_sdk_driver::{Launcher, SpendContext};
use chia_sdk_types::{Condition, Conditions};
use clvmr::NodePtr;
use dig_chainsource_interface::ChainSource;

use crate::comment::LaunchComment;
use crate::RewardsError;

/// A distributor discovered from an observed spend -- private fields, no public constructor.
///
/// Every value here is derived from the spend that was decoded, never from caller input (§13.1
/// clause 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveredDistributor {
    launcher_id: Bytes32,
    generation: LaunchComment,
}

impl DiscoveredDistributor {
    /// The launcher id, derived as the coin id of the `CREATE_COIN` this decode matched -- never
    /// caller input.
    #[must_use]
    pub const fn launcher_id(&self) -> Bytes32 {
        self.launcher_id
    }

    /// The generation this distributor's launch comment advertised. `store_id` and `root` travel
    /// together and are never separable (§1.3, `crate::comment`'s own discipline).
    #[must_use]
    pub const fn generation(&self) -> LaunchComment {
        self.generation
    }
}

/// Decode every DIG rewards distributor launched by one observed spend.
///
/// `observed` MUST be the spend that **created** the launcher coin -- the launcher's parent's
/// spend -- never the launcher's own spend, whose solution carries no comment at all (§13.1 clause
/// 5). A spend may create more than one launcher; this returns one result per launcher-creating
/// `CREATE_COIN` that carries a well-formed DIG rewards comment, in condition order (§13.1 clause
/// 7). An absent, malformed or non-parsing comment contributes no result for that `CREATE_COIN`
/// rather than a defaulted one (§13.1 clause 8).
///
/// # Errors
///
/// - [`RewardsError::Malformed`] if `observed`'s puzzle reveal or solution cannot be deserialised,
///   or if the puzzle does not run. The read was attempted and could not be interpreted, so this
///   fails closed rather than reading as "no distributor found".
pub fn discovered_distributors_in_spend(
    observed: &CoinSpend,
) -> Result<Vec<DiscoveredDistributor>, RewardsError> {
    let mut ctx = SpendContext::new();

    let puzzle_ptr = ctx.alloc(&observed.puzzle_reveal).map_err(|error| {
        RewardsError::Malformed(format!(
            "observed spend's puzzle reveal could not be deserialised: {error}"
        ))
    })?;
    let solution_ptr = ctx.alloc(&observed.solution).map_err(|error| {
        RewardsError::Malformed(format!(
            "observed spend's solution could not be deserialised: {error}"
        ))
    })?;
    let output_ptr = ctx.run(puzzle_ptr, solution_ptr).map_err(|error| {
        RewardsError::Malformed(format!("observed spend's puzzle did not run: {error}"))
    })?;
    let conditions = ctx
        .extract::<Conditions<NodePtr>>(output_ptr)
        .map_err(|error| {
            RewardsError::Malformed(format!(
                "observed spend's output is not a condition list: {error}"
            ))
        })?;

    // The hint atom, recomputed rather than written as a hash literal (§13.1 clause 5).
    let hint_ptr = ctx
        .alloc(&"Reward Distributor v1")
        .map_err(RewardsError::from)?;
    let hint: Bytes32 = ctx.tree_hash(hint_ptr).into();

    let mut discoveries = Vec::new();

    for condition in conditions {
        let Condition::CreateCoin(create_coin) = condition else {
            continue;
        };

        // The launcher puzzle hash enters via `Launcher::new`, never a hash literal and never
        // `chia-puzzles` (§13.1 clause 6): a CREATE_COIN whose puzzle hash is not the launcher
        // puzzle hash must not contribute a result, however well-formed its memos are.
        let launcher = Launcher::new(observed.coin.coin_id(), create_coin.amount);
        if launcher.coin().puzzle_hash != create_coin.puzzle_hash {
            continue;
        }

        let Memos::Some(memos_ptr) = create_coin.memos else {
            continue;
        };

        let Ok((memo_hint, (comment, ()))) = ctx.extract::<(Bytes32, (String, ()))>(memos_ptr)
        else {
            continue;
        };

        if memo_hint != hint {
            continue;
        }

        let Some(generation) = LaunchComment::parse(&comment) else {
            continue;
        };

        discoveries.push(DiscoveredDistributor {
            launcher_id: launcher.coin().coin_id(),
            generation,
        });
    }

    Ok(discoveries)
}

/// Discover the generation one specific launcher id advertised, using nothing but a chain source
/// (`SPEC.md` §13.1 clause 2).
///
/// Reads the spend that created `launcher_id`'s coin and selects the result, if any, whose
/// **derived** launcher id equals `launcher_id` -- never by taking the caller's id on faith (§13.1
/// clause 6: a check that takes both of its sides from the caller is forged in one line).
///
/// # Errors
///
/// - [`RewardsError::ChainUnavailable`] if the chain source could not answer.
/// - [`RewardsError::Malformed`] via [`discovered_distributors_in_spend`].
///
/// Returns `Ok(None)` when `launcher_id` is unknown to `source` (never launched, or the source
/// simply has not seen it) or when the spend that created it advertised no DIG rewards
/// distributor -- both terminal, non-error outcomes.
pub fn discover_distributor(
    source: &impl ChainSource,
    launcher_id: Bytes32,
) -> Result<Option<DiscoveredDistributor>, RewardsError> {
    let observed = source
        .parent_spend(launcher_id)
        .map_err(|error| RewardsError::ChainUnavailable(error.to_string()))?;

    let Some(observed) = observed else {
        return Ok(None);
    };

    let discoveries = discovered_distributors_in_spend(&observed)?;

    Ok(discoveries
        .into_iter()
        .find(|discovered| discovered.launcher_id() == launcher_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chia_protocol::{Coin, Program};
    use clvm_traits::{clvm_quote, ToClvm};

    /// A puzzle that ignores its solution and always outputs `conditions` -- the standard
    /// quote-puzzle idiom this crate's own `tests/simulator.rs` uses.
    fn quoted_puzzle_spend(ctx: &mut SpendContext, conditions: Conditions<NodePtr>) -> CoinSpend {
        let puzzle_ptr = clvm_quote!(conditions).to_clvm(ctx).unwrap();
        let puzzle_reveal = ctx.serialize(&puzzle_ptr).unwrap();
        let solution = ctx.serialize(&NodePtr::NIL).unwrap();
        let coin = Coin::new(Bytes32::new([1; 32]), ctx.tree_hash(puzzle_ptr).into(), 0);

        CoinSpend::new(coin, puzzle_reveal, solution)
    }

    #[test]
    fn a_spend_that_creates_no_launcher_yields_nothing() {
        let mut ctx = SpendContext::new();

        // A CREATE_COIN to some ordinary puzzle hash -- not the launcher puzzle Launcher::new
        // would derive for this parent and amount.
        let conditions = Conditions::new().create_coin(Bytes32::new([9; 32]), 1_000, Memos::None);
        let observed = quoted_puzzle_spend(&mut ctx, conditions);

        let discoveries = discovered_distributors_in_spend(&observed).unwrap();
        assert!(discoveries.is_empty());
    }

    #[test]
    fn an_unparseable_puzzle_reveal_is_malformed_not_absent() {
        let coin = Coin::new(Bytes32::new([1; 32]), Bytes32::new([2; 32]), 1);
        let observed = CoinSpend::new(
            coin,
            Program::from(vec![0xff]), // truncated CLVM: not a valid program
            Program::from(vec![0x80]),
        );

        let result = discovered_distributors_in_spend(&observed);
        assert!(matches!(result, Err(RewardsError::Malformed(_))));
    }
}
