//! `recoverable_base_units` proved equal to what a real clawback actually pays, for the amounts a
//! real clawback can pay on at all.
//!
//! This binds `src/clawback.rs`'s one authoritative restatement of the withdrawal-share
//! arithmetic to the paying puzzle code
//! (`chia-sdk-driver-0.36.0/src/layers/action_layer/actions/reward_distributor/withdraw_incentives.rs:105-107`):
//! if a future upstream bump changes that arithmetic, [`Clawback::recovered_base_units`] and
//! [`recoverable_base_units`] diverge and this test goes red. A simulator test proving equality on
//! a round number would pass under a rounding bug (round numbers do not exercise truncation) and
//! under a lost precision bug (small numbers never overflow), so no case here is round.
//!
//! The equality proof stops where upstream stops being defined —
//! `rewards <= u64::MAX / withdrawal_share_bps` — and the overflow-scale case above that bound is
//! deliberately arithmetic-only. See that test's own doc comment for why.
//!
//! This is a separate file from `tests/simulator.rs` rather than an addition to it, so this ticket
//! does not collide with the concurrent #3267 work landing in that file.

use chia_protocol::{Bytes32, CoinState, SpendBundle};
use chia_puzzle_types::cat::CatArgs;
use chia_puzzle_types::{CoinProof, Memos};
use chia_puzzles::{SETTLEMENT_PAYMENT_HASH, SINGLETON_LAUNCHER_HASH};
use chia_sdk_driver::{
    sign_standard_transaction, Cat, CatSpend, Launcher, Offer, RewardDistributor,
    RewardDistributorConstants, RewardDistributorType, RewardDistributorWithdrawIncentivesAction,
    SingleCatSpend, Slot, Spend, SpendContext, SpendWithConditions, StandardLayer,
};
use chia_sdk_test::Simulator;
use chia_sdk_types::puzzles::{
    RewardDistributorCommitmentSlotValue, RewardDistributorRewardSlotValue,
    RewardDistributorWithdrawIncentivesActionSolution,
};
use chia_sdk_types::{Conditions, TESTNET11_CONSTANTS};
use clvm_traits::{clvm_quote, ToClvm};
use clvmr::NodePtr;
use dig_rewards_coin::clawback::withdraw_committed_incentives;
use dig_rewards_coin::comment::LaunchComment;
use dig_rewards_coin::constants::{
    MAX_SECONDS_OFFSET, PAYOUT_THRESHOLD_BASE_UNITS, WITHDRAWAL_SHARE_BPS,
};
use dig_rewards_coin::fund::commit_incentives_for_distributor_epoch;
use dig_rewards_coin::launch::launch_dig_distributor;
use dig_rewards_coin::recoverable_base_units;

/// The first distributor epoch starts here — small, because the simulator's clock starts at zero.
const FIRST_EPOCH_START: u64 = 1_234;

/// A short distributor epoch; these tests are about where value goes, not how long an epoch is.
const TEST_EPOCH_SECONDS: u64 = 1_000;

/// A cheap manager singleton for a test distributor, mirroring `tests/simulator.rs`'s own.
struct TestSingleton {
    launcher_id: Bytes32,
}

fn launch_test_singleton(
    ctx: &mut SpendContext,
    sim: &mut Simulator,
) -> anyhow::Result<TestSingleton> {
    let launcher_coin = sim.new_coin(SINGLETON_LAUNCHER_HASH.into(), 1);
    let launcher = Launcher::new(launcher_coin.parent_coin_info, 1);
    let launcher_id = launcher.coin().coin_id();

    let inner_puzzle = ctx.alloc(&1)?;
    let inner_puzzle_hash = ctx.tree_hash(inner_puzzle);
    // The launcher's own conditions are not needed: nothing else in this fixture asserts them.
    let (_conditions, _eve_coin) = launcher.spend(ctx, inner_puzzle_hash.into(), ())?;

    Ok(TestSingleton { launcher_id })
}

/// The DIG constants table with the simulator's asset id substituted for $DIG's, otherwise
/// matching `tests/simulator.rs`'s own `test_constants`.
fn test_constants(
    manager_singleton_launcher_id: Bytes32,
    funder_refund_puzzle_hash: Bytes32,
    simulator_asset_id: Bytes32,
) -> RewardDistributorConstants {
    RewardDistributorConstants::without_launcher_id(
        RewardDistributorType::Managed {
            manager_singleton_launcher_id,
        },
        funder_refund_puzzle_hash,
        TEST_EPOCH_SECONDS,
        u64::MAX,
        MAX_SECONDS_OFFSET,
        PAYOUT_THRESHOLD_BASE_UNITS,
        false,
        0,
        WITHDRAWAL_SHARE_BPS,
        simulator_asset_id,
    )
}

/// Launch a distributor whose funder holds `minted_base_units` of $DIG and nothing has been
/// committed yet.
fn launch_harness(
    ctx: &mut SpendContext,
    minted_base_units: u64,
) -> anyhow::Result<(
    Simulator,
    RewardDistributor,
    Slot<RewardDistributorRewardSlotValue>,
    Cat,
    chia_sdk_test::BlsPairWithCoin,
)> {
    let mut sim = Simulator::new();

    let funder = sim.bls(minted_base_units);
    let funder_p2 = StandardLayer::new(funder.pk);
    let (issue_cat, source_cats) = Cat::single_issuance(
        ctx,
        funder.coin.coin_id(),
        None,
        minted_base_units,
        Conditions::new().create_coin(funder.puzzle_hash, minted_base_units, Memos::None),
    )?;
    funder_p2.spend(ctx, funder.coin, issue_cat)?;
    let source_cat = source_cats[0];
    sim.spend_coins(ctx.take(), std::slice::from_ref(&funder.sk))?;

    let manager = launch_test_singleton(ctx, &mut sim)?;

    let offer_amount = 1;
    let launcher_bls = sim.bls(offer_amount);
    let offer_spend = StandardLayer::new(launcher_bls.pk).spend_with_conditions(
        ctx,
        Conditions::new().create_coin(SETTLEMENT_PAYMENT_HASH.into(), offer_amount, Memos::None),
    )?;
    let puzzle_reveal = ctx.serialize(&offer_spend.puzzle)?;
    let solution = ctx.serialize(&offer_spend.solution)?;

    let cat_inner_puzzle = clvm_quote!(Conditions::new().create_coin(
        SETTLEMENT_PAYMENT_HASH.into(),
        source_cat.coin.amount,
        Memos::None
    ))
    .to_clvm(ctx)?;
    let cat_inner_spend = funder_p2.delegated_inner_spend(
        ctx,
        Spend {
            puzzle: cat_inner_puzzle,
            solution: NodePtr::NIL,
        },
    )?;
    source_cat.spend(
        ctx,
        SingleCatSpend {
            prev_coin_id: source_cat.coin.coin_id(),
            next_coin_proof: CoinProof {
                parent_coin_info: source_cat.coin.parent_coin_info,
                inner_puzzle_hash: funder.puzzle_hash,
                amount: source_cat.coin.amount,
            },
            prev_subtotal: 0,
            extra_delta: 0,
            p2_spend: cat_inner_spend,
            revoke: false,
        },
    )?;

    let spends = ctx.take();
    let cat_offer_spend = spends
        .iter()
        .find(|spend| spend.coin.coin_id() == source_cat.coin.coin_id())
        .expect("the CAT offer spend")
        .clone();
    for spend in spends {
        if spend.coin.coin_id() != source_cat.coin.coin_id() {
            ctx.insert(spend);
        }
    }

    let signature = sign_standard_transaction(
        ctx,
        launcher_bls.coin,
        offer_spend,
        &launcher_bls.sk,
        &TESTNET11_CONSTANTS,
    )?;
    let offer = Offer::from_spend_bundle(
        ctx,
        &SpendBundle {
            coin_spends: vec![
                chia_protocol::CoinSpend::new(launcher_bls.coin, puzzle_reveal, solution),
                cat_offer_spend,
            ],
            aggregated_signature: signature,
        },
    )?;

    let constants = test_constants(
        manager.launcher_id,
        funder.puzzle_hash,
        source_cat.info.asset_id,
    );
    let launched = launch_dig_distributor(
        ctx,
        &offer,
        FIRST_EPOCH_START,
        constants,
        &TESTNET11_CONSTANTS,
        LaunchComment::new(Bytes32::new([0xaa; 32]), Bytes32::new([0xbb; 32])),
        0,
    )?;

    sim.spend_coins(
        ctx.take(),
        &[
            launcher_bls.sk.clone(),
            launched.security_coin_secret_key.clone(),
            funder.sk.clone(),
        ],
    )?;

    Ok((
        sim,
        launched.distributor,
        launched.first_distributor_epoch_slot,
        launched.refund_cat,
        funder,
    ))
}

/// Everything a committed-then-withdrawable distributor needs, so the three tests that reach the
/// #3286 bound do not each re-derive the fixture (and drift apart while doing it).
struct Committed {
    sim: Simulator,
    distributor: RewardDistributor,
    commitment_slot: Slot<RewardDistributorCommitmentSlotValue>,
    reward_slot: Slot<RewardDistributorRewardSlotValue>,
    funder: chia_sdk_test::BlsPairWithCoin,

    /// The reward CAT's asset id, needed to recognise the coin the clawback pays out.
    asset_id: Bytes32,

    /// `total_reserves` as it stood BEFORE the commitment was made, and the reserve coin's own
    /// amount after it. `read_distributor`'s B2 guard rests on a commitment entering the reserve
    /// in its own generation, and `committing_deposits_the_full_committed_value_into_the_reserve`
    /// is what pins that on chain rather than assuming it.
    reserves_before_commit: u64,
    reserve_amount_after_commit: u64,
}

/// Launch, then commit `amount_base_units` to [`FIRST_EPOCH_START`], leaving the commitment
/// withdrawable and every handle a withdraw needs in hand.
fn commit_to_first_epoch(
    ctx: &mut SpendContext,
    amount_base_units: u64,
) -> anyhow::Result<Committed> {
    let (mut sim, mut distributor, reward_slot, source_cat, funder) =
        launch_harness(ctx, amount_base_units + FUNDING_HEADROOM)?;
    let asset_id = source_cat.info.asset_id;
    let reserves_before_commit = distributor.info.state.total_reserves;

    let secure_conditions = commit_incentives_for_distributor_epoch(
        ctx,
        &mut distributor,
        reward_slot,
        FIRST_EPOCH_START,
        funder.puzzle_hash,
        amount_base_units,
    )?;

    let hint = ctx.hint(funder.puzzle_hash)?;
    let change = source_cat.coin.amount - amount_base_units;
    let source_cat_spend = CatSpend::new(
        source_cat,
        StandardLayer::new(funder.pk).spend_with_conditions(
            ctx,
            secure_conditions.create_coin(funder.puzzle_hash, change, hint),
        )?,
    );

    let reward_slots: Vec<Slot<RewardDistributorRewardSlotValue>> = distributor
        .pending_spend
        .created_reward_slots
        .iter()
        .map(|value| {
            distributor.created_slot_value_to_slot(
                *value,
                chia_sdk_types::puzzles::RewardDistributorSlotNonce::REWARD,
            )
        })
        .collect();

    let commitment_slot: Slot<RewardDistributorCommitmentSlotValue> = distributor
        .pending_spend
        .created_commitment_slots
        .first()
        .copied()
        .map(|value| {
            distributor.created_slot_value_to_slot(
                value,
                chia_sdk_types::puzzles::RewardDistributorSlotNonce::COMMITMENT,
            )
        })
        .expect("committing created a commitment slot");

    let (distributor, _) = distributor.finish_spend(ctx, vec![source_cat_spend])?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&funder.sk))?;

    let reward_slot = reward_slots
        .into_iter()
        .find(|slot| slot.info.value.epoch_start == FIRST_EPOCH_START)
        .expect("the split created a slot covering the first epoch");

    Ok(Committed {
        sim,
        reserve_amount_after_commit: distributor.reserve.coin.amount,
        distributor,
        commitment_slot,
        reward_slot,
        funder,
        asset_id,
        reserves_before_commit,
    })
}

/// Commit `amount_base_units` to [`FIRST_EPOCH_START`], then immediately claw it back through this
/// crate's own guarded entry point, returning the figure it reports.
fn commit_then_clawback(ctx: &mut SpendContext, amount_base_units: u64) -> anyhow::Result<u64> {
    let mut committed = commit_to_first_epoch(ctx, amount_base_units)?;

    let clawback = withdraw_committed_incentives(
        ctx,
        &mut committed.distributor,
        committed.commitment_slot,
        committed.reward_slot,
        committed.funder.puzzle_hash,
    )?;

    Ok(clawback.recovered_base_units())
}

/// The largest commitment upstream can compute a share of at all: one more base unit and
/// `rewards * withdrawal_share_bps` (`withdraw_incentives.rs:105-107`) no longer fits in a `u64`.
///
/// Written as the derivation, never as the decimal it evaluates to. A spelled literal here would
/// still pass after a mutation of the bound in `src/`, which is the one failure this constant
/// exists to catch (SPEC.md 0.1 clause 5e).
const LARGEST_PAYABLE_COMMITMENT: u64 = u64::MAX / WITHDRAWAL_SHARE_BPS;

/// The first commitment upstream cannot compute a share of.
const FIRST_UNPAYABLE_COMMITMENT: u64 = LARGEST_PAYABLE_COMMITMENT + 1;

/// Funding headroom above the commitment, so the funder's CAT has a non-zero change output.
const FUNDING_HEADROOM: u64 = 1_000;

/// One in-range case: commit `rewards_base_units`, claw it back, and require both that the figure
/// this crate returns is `expected_paid` and that [`recoverable_base_units`] agrees.
fn assert_matches_a_real_clawback(
    rewards_base_units: u64,
    expected_paid: u64,
) -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let paid = commit_then_clawback(ctx, rewards_base_units)?;

    assert_eq!(
        paid, expected_paid,
        "the puzzle truncates {rewards_base_units} * 9_000 / 10_000 down to {expected_paid}"
    );
    assert_eq!(
        recoverable_base_units(
            rewards_base_units,
            u16::try_from(WITHDRAWAL_SHARE_BPS).unwrap()
        ),
        Some(paid),
        "recoverable_base_units must equal the driver's returned figure"
    );

    Ok(())
}

/// Equality with the real paid amount, across the range where upstream is defined -- including its
/// last base unit.
///
/// Every case truncates -- `1_001 @ 9_000 bps` is `900.9`, `7_777` is `6_999.3` and the bound case
/// is `...954.8` -- so none would pass under a rounding bug, and none would pass if the restatement
/// divided before it multiplied (`1_001 / 10_000` is `0`).
///
/// The third case sits exactly at [`LARGEST_PAYABLE_COMMITMENT`]. A documented boundary that no
/// test touches is a claim rather than a proof, so this pins it: equality holds at the very last
/// amount a real clawback can pay on, and one base unit further up upstream stops having an answer
/// at all (`the_driver_panics_one_base_unit_above_the_bound`).
///
/// The expected figures are decimal literals ON PURPOSE, and are not bounds: each was computed
/// independently of the code under test, which is what makes the comparison evidence rather than
/// an echo. `WITHDRAWAL_SHARE_BPS` is asserted first because they are only correct at 9_000 bps.
#[test]
fn recoverable_base_units_matches_a_real_clawback_at_odd_amounts() -> anyhow::Result<()> {
    assert_eq!(
        WITHDRAWAL_SHARE_BPS, 9_000,
        "the expected figures below were computed at 9_000 bps"
    );

    assert_matches_a_real_clawback(1_001, 900)?;
    assert_matches_a_real_clawback(7_777, 6_999)?;
    assert_matches_a_real_clawback(LARGEST_PAYABLE_COMMITMENT, 1_844_674_407_370_954)?;

    Ok(())
}

/// `WITHDRAWAL_SHARE_BPS` fits a `u16`, which is what makes the `u16::try_from` on the cross-check
/// path in `withdraw_committed_incentives` provably infallible for DIG's own constant.
///
/// The fallible conversion stays, because `read_distributor` is distributor-agnostic and an
/// attacker's bps does NOT fit. This test is about DIG's own table only.
#[test]
fn dig_s_own_withdrawal_share_bps_fits_the_u16_recoverable_base_units_takes() {
    assert!(
        u16::try_from(WITHDRAWAL_SHARE_BPS).is_ok(),
        "WITHDRAWAL_SHARE_BPS ({WITHDRAWAL_SHARE_BPS}) must fit the u16 recoverable_base_units \
         takes, or this crate's own launches could not be quoted at all"
    );
}

/// A value large enough that `rewards_base_units * withdrawal_share_bps` overflows a plain `u64`
/// multiply, proving the `u128` intermediate in `recoverable_base_units` is load-bearing rather
/// than decorative: drop the intermediate and this test panics or returns a wrapped value.
///
/// **This case deliberately asserts nothing about what a real clawback pays**, because above the
/// bound there is no driver-produced amount for our figure to be equal to -- the driver misreports
/// instead of refusing (#3286). The on-chain puzzle pays correctly at any scale (CLVM arithmetic is
/// bignum). Equality below the bound is
/// `recoverable_base_units_matches_a_real_clawback_at_odd_amounts`; what the driver does above it
/// is `the_driver_panics_one_base_unit_above_the_bound` and
/// `the_driver_reports_a_wrapped_share_where_the_puzzle_pays_correctly`.
#[test]
fn recoverable_base_units_does_not_overflow_where_a_plain_u64_multiply_would() {
    const REWARDS: u64 = 2_000_000_000_000_000_000;
    const BPS: u16 = 9_000;

    // The expected value, computed independently in u128 so this assertion does not simply
    // restate the function under test.
    let expected = u64::try_from(u128::from(REWARDS) * u128::from(BPS) / 10_000)
        .expect("fits back in u64: the quotient never exceeds rewards_base_units");

    assert_eq!(recoverable_base_units(REWARDS, BPS), Some(expected));

    // Sanity check the overflow premise: `REWARDS * (BPS as u64)` alone cannot fit in a u64.
    assert!(REWARDS.checked_mul(u64::from(BPS)).is_none());
}

/// Red test for #3269 C1: `withdrawal_share_bps` above the legitimate `0..=10_000` range made the
/// `u128 -> u64` narrowing fail, and the `.expect()` that assumed it never could panicked --
/// confirmed against `53a73ff1`:
/// `panicked at src\clawback.rs:109:26: share of a u64 amount by a bps fraction fits in u64:
/// TryFromIntError(PosOverflow)`. `withdrawal_share_bps` is `u16`, so an attacker-controlled
/// chain-read value up to `65_535` reaches this -- not just theoretically past `u64::MAX / bps`.
///
/// `(1_001, 10_001)` pins the boundary one bps above the legitimate range, distinct from the
/// `u64::MAX` case so this cannot pass merely by refusing anything huge.
#[test]
fn recoverable_base_units_rejects_bps_above_10_000_instead_of_panicking() {
    assert_eq!(recoverable_base_units(u64::MAX, 65_535), None);
    assert_eq!(recoverable_base_units(1_001, 10_001), None);
}

/// Selectivity: exactly at the boundary (`10_000` bps, the top of the legitimate range) the guard
/// must NOT reject -- proving `recoverable_base_units_rejects_bps_above_10_000_instead_of_panicking`
/// added a selective guard rather than a blanket one.
#[test]
fn recoverable_base_units_accepts_bps_at_the_10_000_boundary() {
    assert_eq!(recoverable_base_units(1_001, 10_000), Some(1_001));
}

/// T1: submit a real clawback all the way through the simulator and read the amount paid off the
/// simulator's OWN coin records, rather than trusting `Clawback::recovered_base_units()` (the
/// driver's own re-derivation, which is #3286's exact defect).
///
/// Every other equality check in this file -- and `tests/simulator.rs`'s own clawback test -- only
/// compares the driver's returned figure against `recoverable_base_units`, never against an
/// independently observed chain fact. A driver bug that wrapped the returned `u64` would make every
/// one of those pass against a fabricated number.
///
/// The paid coin is selected by **what appeared on chain during the withdraw**, never by its
/// amount: selecting on `driver_reported` would make the comparison circular, because
/// `withdraw_committed_incentives` has already refused unless `driver_reported` equals the
/// restatement. The set difference of hinted coins across the submission is independent of both.
///
/// The +/-1 mutation probe demanded by #3303 was run against this test rather than encoded in it
/// (an `assert_ne!` against a wrong expectation inside the test would be satisfied by the same
/// circularity it is meant to detect). Mutating `expected` by +1 produced, in the debug profile:
///
/// ```text
/// assertion `left == right` failed: the simulator's own coin record at the funder's puzzle hash
/// paid 6999 base units, but the expectation was 7000
///   left: 6999
///  right: 7000
/// ```
///
/// and by -1 the same message with `right: 6998`. The chain's own figure is on the left in both,
/// which is what shows the assertion is anchored to an observed fact.
#[test]
fn clawback_pays_the_funder_the_amount_actually_observed_on_chain() -> anyhow::Result<()> {
    const REWARDS_BASE_UNITS: u64 = 7_777;

    let ctx = &mut SpendContext::new();
    let mut committed = commit_to_first_epoch(ctx, REWARDS_BASE_UNITS)?;
    let funder_puzzle_hash = committed.funder.puzzle_hash;

    let clawback = withdraw_committed_incentives(
        ctx,
        &mut committed.distributor,
        committed.commitment_slot.clone(),
        committed.reward_slot.clone(),
        funder_puzzle_hash,
    )?;
    let driver_reported = clawback.recovered_base_units();

    // Every coin already hinted to the funder BEFORE the withdraw is submitted -- among them the
    // commit spend's own change coin. Anything hinted to the funder afterwards was created by the
    // withdraw, and that is the selector: a fact about WHEN the coin appeared, not about its value.
    let hinted_before: Vec<Bytes32> = committed.sim.hinted_coins(funder_puzzle_hash);

    // The clawback authority's own coin must assert the conditions the withdraw action returned in
    // the same bundle. A fresh XCH coin owned by the funder stands in for that authority, exactly
    // as a real clawbacker's wallet coin would.
    let authority_coin = committed.sim.new_coin(funder_puzzle_hash, 1);
    StandardLayer::new(committed.funder.pk).spend(
        ctx,
        authority_coin,
        clawback.into_conditions(),
    )?;

    let (_distributor, _signature) = committed.distributor.finish_spend(ctx, vec![])?;
    committed
        .sim
        .spend_coins(ctx.take(), std::slice::from_ref(&committed.funder.sk))?;

    let expected_paid_puzzle_hash: Bytes32 =
        CatArgs::curry_tree_hash(committed.asset_id, funder_puzzle_hash.into()).into();
    let paid_on_chain: Vec<CoinState> = committed
        .sim
        .hinted_coins(funder_puzzle_hash)
        .into_iter()
        .filter(|coin_id| !hinted_before.contains(coin_id))
        .filter_map(|coin_id| committed.sim.coin_state(coin_id))
        .filter(|coin_state| coin_state.coin.puzzle_hash == expected_paid_puzzle_hash)
        .collect();

    assert_eq!(
        paid_on_chain.len(),
        1,
        "the withdraw must create exactly one reward-CAT coin hinted to the funder; found {}: \
         {paid_on_chain:?}",
        paid_on_chain.len()
    );
    let chain_observed_amount = paid_on_chain[0].coin.amount;

    let expected = recoverable_base_units(
        REWARDS_BASE_UNITS,
        u16::try_from(WITHDRAWAL_SHARE_BPS).unwrap(),
    )
    .expect("9_000 bps is inside the legitimate domain");

    assert_eq!(
        chain_observed_amount, expected,
        "the simulator's own coin record at the funder's puzzle hash paid {chain_observed_amount} \
         base units, but the expectation was {expected}"
    );
    assert_eq!(
        chain_observed_amount, driver_reported,
        "the on-chain puzzle's own payout ({chain_observed_amount}) must agree with what \
         Clawback::recovered_base_units() reported ({driver_reported}) -- a disagreement here is \
         exactly #3286"
    );

    Ok(())
}

/// T2: the puzzle is handed the **full** committed amount; the share exists only in the driver's
/// returned `u64`.
///
/// This is what makes SPEC.md 0.1 clause 1's "the returned figure is an independent Rust
/// re-derivation" a checked statement rather than an assertion about upstream's intent. The action
/// solution that goes on chain carries `committed_value`, and the second tuple element carries the
/// share -- two different numbers, from the same call. If the share were ever curried into the
/// solution, the puzzle and the driver could not disagree and #3286 would be a non-issue; this test
/// goes red the moment that changes.
///
/// No overflow is involved: the amounts here are small and every figure is exact.
#[test]
fn the_action_solution_carries_the_full_commitment_while_the_return_carries_the_share(
) -> anyhow::Result<()> {
    const REWARDS_BASE_UNITS: u64 = 7_777;

    let ctx = &mut SpendContext::new();
    let mut committed = commit_to_first_epoch(ctx, REWARDS_BASE_UNITS)?;

    let actions_before = committed.distributor.pending_spend.actions.len();
    let clawback = withdraw_committed_incentives(
        ctx,
        &mut committed.distributor,
        committed.commitment_slot,
        committed.reward_slot,
        committed.funder.puzzle_hash,
    )?;
    let share = clawback.recovered_base_units();

    let action_spends = &committed.distributor.pending_spend.actions;
    assert_eq!(
        action_spends.len(),
        actions_before + 1,
        "the withdraw inserted exactly one action spend"
    );
    let params = ctx.extract::<RewardDistributorWithdrawIncentivesActionSolution>(
        action_spends[actions_before].solution,
    )?;

    assert_eq!(
        params.committed_value, REWARDS_BASE_UNITS,
        "the solution the puzzle evaluates carries the FULL commitment, never the share"
    );
    assert!(
        share < params.committed_value,
        "and the returned figure is the share of it ({share} of {})",
        params.committed_value
    );
    assert_eq!(
        share,
        recoverable_base_units(
            params.committed_value,
            u16::try_from(WITHDRAWAL_SHARE_BPS).unwrap()
        )
        .expect("9_000 bps is inside the legitimate domain"),
        "the share is a re-derivation OF the solution's committed_value, computed in Rust"
    );

    Ok(())
}

/// T3: upstream panics one base unit above the bound, in an overflow-checked build, **before it
/// returns anything at all**.
///
/// This is the whole reason every guard in this crate precedes the upstream call: a post-hoc check
/// on a returned value cannot run inside a call that never returns. The upstream action is invoked
/// directly here, bypassing `withdraw_committed_incentives`' pre-guard, because the pre-guard is
/// exactly what this test is justifying.
///
/// Entitled to say nothing about a returned value, and it does not.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "attempt to multiply with overflow")]
fn the_driver_panics_one_base_unit_above_the_bound() {
    let ctx = &mut SpendContext::new();
    let mut committed = commit_to_first_epoch(ctx, FIRST_UNPAYABLE_COMMITMENT)
        .expect("the fixture itself must build");

    let mut distributor = committed.distributor.clone();
    let _ = distributor
        .new_action::<RewardDistributorWithdrawIncentivesAction>()
        .spend(
            ctx,
            &mut distributor,
            committed.commitment_slot.clone(),
            committed.reward_slot.clone(),
        );
    committed.distributor = distributor;
}

/// T4: with overflow checks OFF, upstream returns a **wrapped** share where the puzzle pays the
/// right one -- the release-profile half of #3286, which no `cargo build --release` can detect.
///
/// At `FIRST_UNPAYABLE_COMMITMENT` the wrap lands the product `5_384` above `2^64`, so the integer
/// division by `10_000` reports **zero**: the driver would tell a caller a clawback recovered
/// nothing while the puzzle paid out `1_844_674_407_370_955` base units. Both figures are asserted,
/// because "the driver is wrong" is only half the claim; the other half is that
/// `recoverable_base_units` is right.
///
/// Run by the `Tests (release profile)` CI job. `cargo test` alone never reaches it.
#[cfg(not(debug_assertions))]
#[test]
fn the_driver_reports_a_wrapped_share_where_the_puzzle_pays_correctly() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let mut committed = commit_to_first_epoch(ctx, FIRST_UNPAYABLE_COMMITMENT)?;

    let mut distributor = committed.distributor.clone();
    let (_conditions, driver_reported) = distributor
        .new_action::<RewardDistributorWithdrawIncentivesAction>()
        .spend(
            ctx,
            &mut distributor,
            committed.commitment_slot.clone(),
            committed.reward_slot.clone(),
        )?;
    committed.distributor = distributor;

    assert_eq!(
        driver_reported, 0,
        "the wrapped product is 5_384, and 5_384 / 10_000 is 0 -- the driver reports a clawback \
         of nothing"
    );

    // What the puzzle actually pays, computed in u128 so this figure does not share the arithmetic
    // it is judging.
    let true_share = u64::try_from(
        u128::from(FIRST_UNPAYABLE_COMMITMENT) * u128::from(WITHDRAWAL_SHARE_BPS) / 10_000,
    )
    .expect("the share never exceeds the commitment");
    assert_eq!(
        true_share, 1_844_674_407_370_955,
        "the puzzle's bignum arithmetic pays this, whatever the driver says"
    );
    assert_eq!(
        recoverable_base_units(
            FIRST_UNPAYABLE_COMMITMENT,
            u16::try_from(WITHDRAWAL_SHARE_BPS).unwrap()
        ),
        Some(true_share),
        "this crate's restatement agrees with the puzzle, not with the driver"
    );

    Ok(())
}

/// `read_distributor`'s B2 guard bounds `committed_value` by the reserve HIGH-WATER MARK, and the
/// step that makes that sound is: a commitment enters the reserve in its own generation.
///
/// That step is enforced by the reserve finalizer inside compiled puzzle bytes -- there is no
/// chialisp source in the dependency tree to read it out of -- so it is pinned on chain here
/// instead. Committing `X` must grow `total_reserves` by exactly `X`, and the physical reserve coin
/// must hold the result. If either could be less than `X`, B2 would bound the wrong quantity and
/// `src/state.rs`'s stated invariant would be false.
#[test]
fn committing_deposits_the_full_committed_value_into_the_reserve() -> anyhow::Result<()> {
    const REWARDS_BASE_UNITS: u64 = 7_777;

    let ctx = &mut SpendContext::new();
    let committed = commit_to_first_epoch(ctx, REWARDS_BASE_UNITS)?;

    assert_eq!(
        committed.distributor.info.state.total_reserves,
        committed.reserves_before_commit + REWARDS_BASE_UNITS,
        "committing must deposit the FULL committed value into the reserve, not a share of it"
    );
    assert_eq!(
        committed.reserve_amount_after_commit, committed.distributor.info.state.total_reserves,
        "and the physical reserve coin must hold what the state claims (CAT conservation)"
    );
    assert!(
        committed.reserve_amount_after_commit >= REWARDS_BASE_UNITS,
        "which is the step src/state.rs's B2 invariant rests on"
    );

    Ok(())
}
