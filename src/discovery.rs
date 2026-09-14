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
use chia_sdk_types::{run_puzzle_with_cost, Condition, Conditions};
use clvmr::NodePtr;
use dig_chainsource_interface::ChainSource;

use crate::comment::LaunchComment;
use crate::RewardsError;

/// The CLVM cost budget this decode allows itself to spend running `observed`'s puzzle.
///
/// `observed` is a spend this crate did not build -- a peer scanning the chain for distributors
/// hands this function whatever parent spend a `launcher_id` names, and that spend's puzzle is
/// attacker-chosen (SPEC.md §13.1 clause 2, §1.3). `SpendContext::run` alone bounds cost only to
/// `chia_sdk_types::MAINNET_CONSTANTS.max_block_cost_clvm` (`11_000_000_000`) -- the cost of an
/// entire block, not of one coin spend -- so calling it directly here would let a single crafted
/// spend force every peer that decodes it to burn close to a full block's CPU budget on one
/// decode. `10_000_000` is three orders of magnitude below that ceiling while remaining three
/// orders of magnitude above what an ordinary standard-puzzle or singleton-launcher spend costs
/// to run (the spends this crate's own `launch_dig_distributor` / `launch_manager_singleton`
/// produce, and the only shapes a well-formed launcher-creating parent spend takes), so it
/// comfortably admits every real spend this crate needs to decode while refusing one shaped to
/// exhaust the block's whole budget. This is a bound this crate chose, not one upstream imposes;
/// nothing in `chia-sdk-driver` 0.36.0 supplies a smaller default for a spend read off the chain.
const DECODE_MAX_COST: u64 = 10_000_000;

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
    // Bounded explicitly to `DECODE_MAX_COST` rather than via `ctx.run` (§13.1's decode runs
    // attacker-chosen CLVM -- see `DECODE_MAX_COST`'s doc for why `ctx.run`'s own implicit
    // full-block bound is not tight enough here).
    let clvmr::reduction::Reduction(_cost, output_ptr) =
        run_puzzle_with_cost(&mut ctx, puzzle_ptr, solution_ptr, DECODE_MAX_COST, false).map_err(
            |error| {
                RewardsError::Malformed(format!(
                    "observed spend's puzzle did not run within the decode's {DECODE_MAX_COST} \
                     cost bound: {error}"
                ))
            },
        )?;
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

    #[test]
    fn an_unparseable_solution_is_malformed_not_absent() {
        let mut ctx = SpendContext::new();

        // A well-formed puzzle reveal (quotes an empty condition list), but a solution that is
        // not valid CLVM at all -- the solution-deserialise arm, distinct from the puzzle-reveal
        // arm covered above.
        let puzzle_ptr = clvm_quote!(Conditions::<NodePtr>::new())
            .to_clvm(&mut ctx)
            .unwrap();
        let puzzle_reveal = ctx.serialize(&puzzle_ptr).unwrap();
        let coin = Coin::new(Bytes32::new([1; 32]), ctx.tree_hash(puzzle_ptr).into(), 0);

        let observed = CoinSpend::new(coin, puzzle_reveal, Program::from(vec![0xff]));

        let result = discovered_distributors_in_spend(&observed);
        assert!(matches!(result, Err(RewardsError::Malformed(_))));
    }

    #[test]
    fn a_puzzle_that_does_not_run_is_malformed_not_absent() {
        // `(x)` -- CLVM opcode 8 applied to no arguments -- is valid CLVM that always raises when
        // run, distinct from the puzzle-reveal and solution deserialise arms covered above.
        let mut ctx = SpendContext::new();
        let raising_puzzle = ctx.alloc(&(8, ())).unwrap();
        let raising_reveal = ctx.serialize(&raising_puzzle).unwrap();

        let coin = Coin::new(
            Bytes32::new([1; 32]),
            ctx.tree_hash(raising_puzzle).into(),
            0,
        );
        let solution = ctx.serialize(&NodePtr::NIL).unwrap();
        let observed = CoinSpend::new(coin, raising_reveal, solution);

        let result = discovered_distributors_in_spend(&observed);
        assert!(matches!(result, Err(RewardsError::Malformed(_))));
    }

    #[test]
    fn a_hint_that_does_not_match_the_dig_rewards_hint_yields_nothing() {
        let mut ctx = SpendContext::new();

        // Well-formed memos -- a hint atom and a comment string -- but the hint is some other
        // string, not "Reward Distributor v1" (§13.1 clause 8: an absent/mismatched hint
        // contributes no result rather than a defaulted one).
        let other_hint_ptr = ctx.alloc(&"Some Other Hint").unwrap();
        let comment_ptr = ctx.alloc(&"storeId:root").unwrap();
        let memos = ctx.memos(&(other_hint_ptr, (comment_ptr, ()))).unwrap();

        let parent_coin_id = Bytes32::new([3; 32]);
        let launcher = Launcher::new(parent_coin_id, 1);
        let conditions = Conditions::new().create_coin(launcher.coin().puzzle_hash, 1, memos);
        let observed = quoted_puzzle_spend(&mut ctx, conditions);
        let observed = CoinSpend::new(
            Coin::new(
                parent_coin_id,
                observed.coin.puzzle_hash,
                observed.coin.amount,
            ),
            observed.puzzle_reveal,
            observed.solution,
        );

        let discoveries = discovered_distributors_in_spend(&observed).unwrap();
        assert!(discoveries.is_empty());
    }

    #[test]
    fn a_comment_that_does_not_parse_as_a_generation_yields_nothing() {
        let mut ctx = SpendContext::new();

        // The real DIG rewards hint, but a comment that is not a `storeId:root` pair -- §13.1
        // clause 8's "malformed comment contributes no result" arm.
        let hint_ptr = ctx.alloc(&"Reward Distributor v1").unwrap();
        let comment_ptr = ctx.alloc(&"not-a-generation").unwrap();
        let memos = ctx.memos(&(hint_ptr, (comment_ptr, ()))).unwrap();

        let parent_coin_id = Bytes32::new([4; 32]);
        let launcher = Launcher::new(parent_coin_id, 1);
        let conditions = Conditions::new().create_coin(launcher.coin().puzzle_hash, 1, memos);
        let observed = quoted_puzzle_spend(&mut ctx, conditions);
        let observed = CoinSpend::new(
            Coin::new(
                parent_coin_id,
                observed.coin.puzzle_hash,
                observed.coin.amount,
            ),
            observed.puzzle_reveal,
            observed.solution,
        );

        let discoveries = discovered_distributors_in_spend(&observed).unwrap();
        assert!(discoveries.is_empty());
    }

    #[test]
    fn discover_distributor_returns_none_when_the_chain_has_no_parent_spend() {
        let launcher_id = Bytes32::new([5; 32]);
        let chain = dig_chainsource_interface::MockChainSource::new();

        let result = discover_distributor(&chain, launcher_id).unwrap();
        assert!(result.is_none());
    }

    /// `observed`'s puzzle is attacker-chosen (`DECODE_MAX_COST`'s doc), so this decode MUST
    /// refuse a puzzle whose run would exceed the declared cost bound rather than execute it to
    /// completion. `(18 (1 . N) (1 . N))` -- opcode 18 is `*` -- multiplies two ~60 KB atoms
    /// against each other; `clvmr`'s own cost model for `*` is roughly `len(N)^2 / 128`
    /// (`more_ops.rs`'s `MUL_SQUARE_COST_PER_BYTE_DIVIDER`), so this run costs on the order of
    /// 28,000,000 -- comfortably past `DECODE_MAX_COST`'s 10,000,000 -- while the two atoms
    /// themselves are cheap to allocate (two flat byte buffers) and the interpreter itself stops
    /// as soon as the declared bound is crossed, never actually finishing the multiplication.
    /// This is deliberately NOT a 500,000-scale fixture: the cost wall this crate must respect is
    /// a property of the numbers' BYTE LENGTH, not of a loop count, so a cheap, small-in-wall-time
    /// construction is enough to cross it.
    #[test]
    fn a_puzzle_over_the_decode_cost_bound_is_refused_not_run_to_completion() {
        let mut ctx = SpendContext::new();

        let quote_op = ctx.new_small_number(1).unwrap();
        let multiply_op = ctx.new_small_number(18).unwrap();
        let big_atom = ctx.new_atom(&vec![0x7f_u8; 60_000]).unwrap();
        let quoted_big_atom = ctx.new_pair(quote_op, big_atom).unwrap();
        let nil = ctx.nil();
        let second_arg = ctx.new_pair(quoted_big_atom, nil).unwrap();
        let arg_list = ctx.new_pair(quoted_big_atom, second_arg).unwrap();
        let expensive_puzzle = ctx.new_pair(multiply_op, arg_list).unwrap();

        let puzzle_reveal = ctx.serialize(&expensive_puzzle).unwrap();
        let solution = ctx.serialize(&NodePtr::NIL).unwrap();
        let coin = Coin::new(
            Bytes32::new([1; 32]),
            ctx.tree_hash(expensive_puzzle).into(),
            0,
        );
        let observed = CoinSpend::new(coin, puzzle_reveal, solution);

        match discovered_distributors_in_spend(&observed) {
            Err(RewardsError::Malformed(message)) => assert!(
                message.contains(&DECODE_MAX_COST.to_string()),
                "the refusal must name the cost bound it enforced, got: {message}"
            ),
            other => panic!(
                "expected a Malformed refusal naming the decode's cost bound, got: {other:?}"
            ),
        }
    }
}
