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
///
/// This bound limits the summed *charged* cost of the operations `run_puzzle_with_cost` completes,
/// not the work performed by any single one of them: `clvmr` 0.16.4 charges most operators (for
/// example `op_multiply`) only after they finish, so one pair of very large atoms can still cost a
/// decoding peer real CPU before the accumulated charge trips this ceiling.
const DECODE_MAX_COST: u64 = 10_000_000;

/// The largest serialized `puzzle_reveal` or `solution` this decode will allocate at all, checked
/// **independently** against each of the two -- never against their sum.
///
/// `DECODE_MAX_COST` bounds the CLVM cost `run_puzzle_with_cost` charges, but `ctx.alloc` at
/// [`discovered_distributors_in_spend`]'s first two lines deserialises `observed`'s bytes into the
/// allocator **before any cost accounting exists at all**, and `clvmr` 0.16.4 charges several
/// operators (`op_multiply` among them) only after they finish, so a cost bound alone is
/// post-hoc accounting, not mid-run interruption: one pair of very large atoms burns real CPU and
/// RAM before the ceiling trips. This is a bound on bytes received, checked before the first byte
/// is allocated.
///
/// **Independent, not summed.** A sum bound over `puzzle_reveal.len() + solution.len()` admits a
/// degenerate split -- a 1-byte puzzle paired with an (N-1)-byte solution passes it exactly as
/// easily as a balanced split -- so it gives an attacker one knob to spend however they like,
/// where two independent bounds give none. Checking each field on its own also lets the refusal
/// name *which* input exceeded, rather than a combined figure neither field alone explains.
///
/// **The literal, derived from a real measurement, not picked round.** `tests/simulator.rs`'s
/// `launch_manager_and_distributor_in_one_bundle` fixture bundles a manager-singleton launch and a
/// DIG-distributor launch together and submits every produced spend; measuring
/// `puzzle_reveal.len()` and `solution.len()` across all of them (2026-09-16, this crate at
/// 0.6.0) gave a largest observed `puzzle_reveal` of **2,060 bytes** and a largest observed
/// `solution` of **387 bytes** -- both on the order of 1-2 KB, as an ordinary standard-puzzle or
/// singleton-launcher spend is. Sixteen times the larger of the two is 32,960 bytes, and the
/// smallest power of two at or above that is 65,536 (64 KiB) -- comfortable room for a real spend
/// while still refusing the multi-megabyte blob DIG-Network/dig_ecosystem#3333 is about. This
/// derivation is pinned by `tests/simulator.rs`'s
/// `the_real_launch_spends_observed_sizes_are_measured_literals`, the same way the neighbouring
/// `DECODE_MAX_COST` literal is pinned by `the_real_launch_spends_decode_cost_is_a_measured_literal`
/// -- an unpinned "measured" number is silent drift the moment the fixture it was measured from
/// changes shape.
///
/// # What this bound does NOT cover
///
/// Scoped to [`discovered_distributors_in_spend`] only -- [`crate::state::find_eve_reserve_provenance`]
/// performs its own, separate per-candidate size check against this same constant (see that
/// function's doc), and is not covered by the list below.
///
/// A screen that reads as complete and is not is worse than no screen at all -- 0.5.0 shipped
/// exactly that shape once already (`commit_incentives.rs` hanging at `epoch_seconds == 0` while
/// the hanging spend's hash stayed a legitimately recognised cohort member). This bound is
/// narrower than it may look:
///
/// 1. **An under-bound spend can still be expensive to evaluate.** This is a *size* bound; the
///    *evaluation cost* of a spend under 64 KiB is bounded only by `DECODE_MAX_COST`, which is
///    itself charged post-hoc (see that constant's own doc) -- a small puzzle can still be a slow
///    one to run.
/// 2. **Everything the run produces is unbounded by this check.** `ctx.extract::<Conditions<..>>`
///    and the per-`CREATE_COIN` memo extraction execute *after* this gate and are bounded only by
///    however many conditions the run produced -- a small serialized input can still unpack into a
///    large condition list. (The hint `tree_hash` is computed once, over the fixed literal
///    `"Reward Distributor v1"`, and does not scale with the run's output at all.)
/// 3. **In-memory amplification is untouched.** This bounds bytes *received*, not bytes *held*: a
///    small serialized atom can still expand into a far larger `NodePtr` tree once allocated.
/// 4. **This is a per-spend bound, not a per-scan one.** N spends each one byte under the limit
///    cost N times the work; nothing here rate-limits how many spends a caller decodes.
/// 5. **It authenticates nothing.** A well-formed, under-bound, cheap spend from an attacker
///    decodes in full, and the resulting [`DiscoveredDistributor`] proves only that *some* spend
///    advertised `storeId:root` (module docs above, §13.1 clause 9) -- never that the distributor
///    is this reader's own, still exists, or is funded. §9.3 is still required before a caller
///    treats a discovered distributor as its own.
pub const DECODE_MAX_SERIALIZED_BYTES: usize = 65_536;

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
    // Bounded before any allocation (§ `DECODE_MAX_SERIALIZED_BYTES`'s doc): `ctx.alloc` below
    // deserialises attacker bytes into the allocator, which must not happen at all for a spend
    // this oversized, let alone before `DECODE_MAX_COST`'s accounting exists.
    if observed.puzzle_reveal.len() > DECODE_MAX_SERIALIZED_BYTES {
        return Err(RewardsError::ObservedSpendFieldTooLarge {
            field: "puzzle_reveal",
            actual_len: observed.puzzle_reveal.len(),
            limit_bytes: DECODE_MAX_SERIALIZED_BYTES,
        });
    }
    if observed.solution.len() > DECODE_MAX_SERIALIZED_BYTES {
        return Err(RewardsError::ObservedSpendFieldTooLarge {
            field: "solution",
            actual_len: observed.solution.len(),
            limit_bytes: DECODE_MAX_SERIALIZED_BYTES,
        });
    }

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
    /// refuse a puzzle whose *charged* cost would exceed the declared bound. `(18 (1 . N) (1 . N))`
    /// -- opcode 18 is `*` -- multiplies two ~60 KB atoms against each other; `clvmr`'s own cost
    /// model for `*` is roughly `len(N)^2 / 128` (`more_ops.rs`'s `MUL_SQUARE_COST_PER_BYTE_DIVIDER`),
    /// so this run costs on the order of 28,000,000 -- comfortably past `DECODE_MAX_COST`'s
    /// 10,000,000 -- while the two atoms themselves are cheap to allocate (two flat byte buffers).
    /// `clvmr` 0.16.4 completes this multiply in full and charges its cost afterward, so the
    /// refusal here comes from the accumulated charge crossing the bound, not from the operation
    /// being interrupted mid-run. This is deliberately NOT a 500,000-scale fixture: the cost wall
    /// this crate must respect is a property of the numbers' BYTE LENGTH, not of a loop count, so a
    /// cheap, small-in-wall-time construction is enough to cross it.
    #[test]
    fn a_puzzle_over_the_decode_cost_bound_is_refused() {
        // `op_multiply` is variadic (`more_ops.rs`'s `op_multiply` loops `a.next(input)`), so one
        // call can take thousands of small quoted arguments rather than two huge atoms: each
        // argument charges `MUL_COST_PER_OP` (885) on top of its size-dependent terms, so 4,200
        // one-byte arguments alone cross `DECODE_MAX_COST` (measured cost ~10,464,848) while the
        // whole serialized puzzle stays at ~16.8 KB -- comfortably UNDER
        // `DECODE_MAX_SERIALIZED_BYTES` (65,536), so this test exercises the cost gate in
        // isolation from the size gate above it, rather than tripping the size gate first.
        const ARG_COUNT: usize = 4_200;

        let mut ctx = SpendContext::new();
        let quote_op = ctx.new_small_number(1).unwrap();
        let multiply_op = ctx.new_small_number(18).unwrap();
        let mut arg_list = ctx.nil();
        for _ in 0..ARG_COUNT {
            let atom = ctx.new_small_number(2).unwrap();
            let quoted = ctx.new_pair(quote_op, atom).unwrap();
            arg_list = ctx.new_pair(quoted, arg_list).unwrap();
        }
        let expensive_puzzle = ctx.new_pair(multiply_op, arg_list).unwrap();

        let puzzle_reveal = ctx.serialize(&expensive_puzzle).unwrap();
        assert!(
            puzzle_reveal.len() < DECODE_MAX_SERIALIZED_BYTES,
            "this fixture must stay under the size gate to test the cost gate in isolation, got \
             {} bytes",
            puzzle_reveal.len()
        );
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

    /// `DECODE_MAX_SERIALIZED_BYTES` must be checked BEFORE any decode work runs -- provable
    /// without executing an expensive puzzle by declaring an over-budget shape and asserting on
    /// the refusal payload alone (the crate's established pattern for an unrunnable-scale
    /// fixture): this gate must fire before any run is even attempted, so the puzzle bytes here
    /// need not be valid CLVM at all.
    #[test]
    fn a_puzzle_reveal_one_byte_over_the_size_bound_is_refused_before_any_decode() {
        let oversized = vec![0u8; DECODE_MAX_SERIALIZED_BYTES + 1];
        let coin = Coin::new(Bytes32::new([1; 32]), Bytes32::new([2; 32]), 0);
        let observed = CoinSpend::new(coin, Program::from(oversized), Program::from(vec![0x80]));

        match discovered_distributors_in_spend(&observed) {
            Err(RewardsError::ObservedSpendFieldTooLarge {
                field,
                actual_len,
                limit_bytes,
            }) => {
                assert_eq!(field, "puzzle_reveal");
                assert_eq!(actual_len, DECODE_MAX_SERIALIZED_BYTES + 1);
                assert_eq!(limit_bytes, DECODE_MAX_SERIALIZED_BYTES);
            }
            other => {
                panic!("expected ObservedSpendFieldTooLarge naming puzzle_reveal, got: {other:?}")
            }
        }
    }

    #[test]
    fn a_solution_one_byte_over_the_size_bound_is_refused_before_any_decode() {
        // A well-formed, in-bound puzzle reveal -- the refusal must come from the SOLUTION arm,
        // independently of the puzzle_reveal arm covered above.
        let mut ctx = SpendContext::new();
        let puzzle_ptr = clvm_quote!(Conditions::<NodePtr>::new())
            .to_clvm(&mut ctx)
            .unwrap();
        let puzzle_reveal = ctx.serialize(&puzzle_ptr).unwrap();
        let coin = Coin::new(Bytes32::new([1; 32]), ctx.tree_hash(puzzle_ptr).into(), 0);

        let oversized_solution = vec![0u8; DECODE_MAX_SERIALIZED_BYTES + 1];
        let observed = CoinSpend::new(coin, puzzle_reveal, Program::from(oversized_solution));

        match discovered_distributors_in_spend(&observed) {
            Err(RewardsError::ObservedSpendFieldTooLarge {
                field,
                actual_len,
                limit_bytes,
            }) => {
                assert_eq!(field, "solution");
                assert_eq!(actual_len, DECODE_MAX_SERIALIZED_BYTES + 1);
                assert_eq!(limit_bytes, DECODE_MAX_SERIALIZED_BYTES);
            }
            other => panic!("expected ObservedSpendFieldTooLarge naming solution, got: {other:?}"),
        }
    }

    /// The size gate is `>`, not `>=`: a `puzzle_reveal` of exactly `DECODE_MAX_SERIALIZED_BYTES`
    /// must not be refused by [`RewardsError::ObservedSpendFieldTooLarge`]. This declares a
    /// boundary-sized shape (deliberately not a valid CLVM program -- only the length matters to
    /// this gate, and it runs before any deserialisation is attempted) and asserts on the
    /// resulting error's IDENTITY: it must be the deserialise-failure arm
    /// ([`RewardsError::Malformed`]), never the size-refusal arm, proving the length gate itself
    /// let this exact byte count through.
    #[test]
    fn a_puzzle_reveal_exactly_at_the_size_bound_is_not_refused_by_the_size_gate() {
        let at_bound = vec![0xffu8; DECODE_MAX_SERIALIZED_BYTES];
        let coin = Coin::new(Bytes32::new([1; 32]), Bytes32::new([2; 32]), 0);
        let observed = CoinSpend::new(coin, Program::from(at_bound), Program::from(vec![0x80]));

        // Malformed (not valid CLVM) or Ok -- either proves the size gate let it through.
        if let Err(RewardsError::ObservedSpendFieldTooLarge { .. }) =
            discovered_distributors_in_spend(&observed)
        {
            panic!("a puzzle_reveal of exactly the bound must pass the size gate");
        }
    }

    /// Same boundary, the `solution` arm: exactly at the bound must not trip the size gate.
    #[test]
    fn a_solution_exactly_at_the_size_bound_is_not_refused_by_the_size_gate() {
        let mut ctx = SpendContext::new();
        let puzzle_ptr = clvm_quote!(Conditions::<NodePtr>::new())
            .to_clvm(&mut ctx)
            .unwrap();
        let puzzle_reveal = ctx.serialize(&puzzle_ptr).unwrap();
        let coin = Coin::new(Bytes32::new([1; 32]), ctx.tree_hash(puzzle_ptr).into(), 0);

        let at_bound = vec![0xffu8; DECODE_MAX_SERIALIZED_BYTES];
        let observed = CoinSpend::new(coin, puzzle_reveal, Program::from(at_bound));

        // Malformed (not valid CLVM) or Ok -- either proves the size gate let it through.
        if let Err(RewardsError::ObservedSpendFieldTooLarge { .. }) =
            discovered_distributors_in_spend(&observed)
        {
            panic!("a solution of exactly the bound must pass the size gate");
        }
    }
}
