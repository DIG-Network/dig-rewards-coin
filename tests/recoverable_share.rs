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
    RewardDistributorConstants, RewardDistributorType, SingleCatSpend, Slot, Spend, SpendContext,
    SpendWithConditions, StandardLayer,
};
use chia_sdk_test::Simulator;
use chia_sdk_types::puzzles::{
    RewardDistributorCommitmentSlotValue, RewardDistributorRewardSlotValue,
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

/// Commit `amount_base_units` to `FIRST_EPOCH_START`, then immediately claw it back, returning
/// what the puzzle actually paid.
fn commit_then_clawback(
    ctx: &mut SpendContext,
    sim: &mut Simulator,
    mut distributor: RewardDistributor,
    reward_slot: Slot<RewardDistributorRewardSlotValue>,
    source_cat: Cat,
    funder: &chia_sdk_test::BlsPairWithCoin,
    amount_base_units: u64,
) -> anyhow::Result<u64> {
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

    let mut distributor = distributor;
    let reward_slot = reward_slots
        .into_iter()
        .find(|slot| slot.info.value.epoch_start == FIRST_EPOCH_START)
        .expect("the split created a slot covering the first epoch");

    let clawback = withdraw_committed_incentives(
        ctx,
        &mut distributor,
        commitment_slot,
        reward_slot,
        funder.puzzle_hash,
    )?;

    Ok(clawback.recovered_base_units())
}
/// The largest commitment upstream can pay a share of at all: one more base unit and
/// `rewards * 9_000` (`withdraw_incentives.rs:105`) no longer fits in a `u64`.
///
/// Spelled as a literal rather than computed, so this fixture cannot agree with
/// [`recoverable_base_units`] by sharing its arithmetic; the premise is checked below instead.
const LARGEST_PAYABLE_COMMITMENT: u64 = 2_049_638_230_412_172;

/// Funding headroom above the commitment, so the funder's CAT has a non-zero change output.
const FUNDING_HEADROOM: u64 = 1_000;

/// One in-range case: commit `rewards_base_units`, claw it back, build the clawback spend (but do not
/// submit it), and require both that the driver's figure `expected_paid` and [`recoverable_base_units`]
/// return the same value. The clawback is built but never executed on-chain, so the equality holds against
/// the driver's computed figure rather than an observed puzzle output.
fn assert_matches_a_real_clawback(
    rewards_base_units: u64,
    expected_paid: u64,
) -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let (mut sim, distributor, first_epoch_slot, source_cat, funder) =
        launch_harness(ctx, rewards_base_units + FUNDING_HEADROOM)?;

    let paid = commit_then_clawback(
        ctx,
        &mut sim,
        distributor,
        first_epoch_slot,
        source_cat,
        &funder,
        rewards_base_units,
    )?;

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

/// Equality with the real paid amount, across the range where upstream is defined — including its
/// last base unit.
///
/// Every case truncates — `1_001 @ 9_000 bps` is `900.9`, `7_777` is `6_999.3` and the bound case
/// is `…954.8` — so none would pass under a rounding bug, and none would pass if the restatement
/// divided before it multiplied (`1_001 / 10_000` is `0`).
///
/// The third case sits exactly at [`LARGEST_PAYABLE_COMMITMENT`], the bound
/// `src/clawback.rs` documents. A documented boundary that no test touches is a claim rather than
/// a proof, so this pins it: equality holds at the very last amount a real clawback can pay on,
/// and one base unit further up
/// (`recoverable_base_units_does_not_overflow_where_a_plain_u64_multiply_would`) upstream stops
/// having an answer at all.
#[test]
fn recoverable_base_units_matches_a_real_clawback_at_odd_amounts() -> anyhow::Result<()> {
    assert_eq!(
        WITHDRAWAL_SHARE_BPS, 9_000,
        "the fixture assumes the crate's own launch bps"
    );

    // The bound's premise, checked rather than assumed: this is the last commitment whose
    // `rewards * 9_000` fits in a u64, and the next one is not.
    assert!(LARGEST_PAYABLE_COMMITMENT
        .checked_mul(WITHDRAWAL_SHARE_BPS)
        .is_some());
    assert!((LARGEST_PAYABLE_COMMITMENT + 1)
        .checked_mul(WITHDRAWAL_SHARE_BPS)
        .is_none());

    assert_matches_a_real_clawback(1_001, 900)?;
    assert_matches_a_real_clawback(7_777, 6_999)?;
    assert_matches_a_real_clawback(LARGEST_PAYABLE_COMMITMENT, 1_844_674_407_370_954)?;

    Ok(())
}

/// A value large enough that `rewards_base_units * withdrawal_share_bps` overflows a plain `u64`
/// multiply (`2_000_000_000_000_000_000 * 9_000` is roughly `1.8e22`, far past `u64::MAX`'s
/// `~1.8e19`), proving the `u128` intermediate in `recoverable_base_units` is load-bearing rather
/// than decorative: drop the intermediate and this test panics or returns a wrapped value.
///
/// **This case deliberately asserts nothing about what a real clawback pays through today's
/// driver, and a future reader must not "helpfully" add that comparison back.** The on-chain
/// puzzle itself pays the correct share at any scale — CLVM arithmetic is bignum — but the
/// `chia-sdk-driver` 0.36.0 Rust driver's own withdrawal-share multiply
/// (`chia-sdk-driver-0.36.0/src/layers/action_layer/actions/reward_distributor/withdraw_incentives.rs:105-107`)
/// is a plain `u64` multiply of `rewards` by `withdrawal_share_bps`, so it panics under overflow
/// checks — which is exactly how this case first failed in CI — and wraps silently without them,
/// meaning the driver cannot build a spend at this scale at all (#3286, a driver bug). An equality
/// assertion here would be unsatisfiable by construction, because there is no driver-produced
/// amount for our figure to be equal to. The bound is `u64::MAX / withdrawal_share_bps`, about
/// `2.05e15` base units at 9_000 bps; equality below it is tested by
/// `recoverable_base_units_matches_a_real_clawback_at_odd_amounts`.
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
/// `u128 -> u64` narrowing fail, and the `.expect()` that assumed it never could panicked —
/// confirmed against `53a73ff1`:
/// `panicked at src\clawback.rs:109:26: share of a u64 amount by a bps fraction fits in u64:
/// TryFromIntError(PosOverflow)`. `withdrawal_share_bps` is `u16`, so an attacker-controlled
/// chain-read value up to `65_535` reaches this — not just theoretically past `u64::MAX / bps`.
///
/// `(1_001, 10_001)` pins the boundary one bps above the legitimate range, distinct from the
/// `u64::MAX` case so this cannot pass merely by refusing anything huge.
#[test]
fn recoverable_base_units_rejects_bps_above_10_000_instead_of_panicking() {
    assert_eq!(recoverable_base_units(u64::MAX, 65_535), None);
    assert_eq!(recoverable_base_units(1_001, 10_001), None);
}

/// Selectivity: exactly at the boundary (`10_000` bps, the top of the legitimate range) the guard
/// must NOT reject — proving `recoverable_base_units_rejects_bps_above_10_000_instead_of_panicking`
/// added a selective guard rather than a blanket one.
#[test]
fn recoverable_base_units_accepts_bps_at_the_10_000_boundary() {
    assert_eq!(recoverable_base_units(1_001, 10_000), Some(1_001));
}

/// T1: submit a real clawback all the way through the simulator and read the amount paid off the
/// simulator's OWN coin records at the funder's puzzle hash, rather than trusting
/// `Clawback::recovered_base_units()` (the driver's own re-derivation, #3286's exact defect).
///
/// Every other equality check in this file — and `tests/simulator.rs`'s own clawback test — only
/// asserts the driver's returned figure against itself or against `recoverable_base_units`, never
/// against an independently observed chain fact. A driver bug that wrapped the returned `u64`
/// would make every one of those assertions pass against a fabricated number. This test closes
/// that gap: it finishes and submits the withdraw spend, then finds the CAT coin the on-chain
/// puzzle itself created (bignum CLVM arithmetic, no `u64` overflow possible) and checks ITS
/// amount, read off `Simulator::coin_state`, never off the driver's tuple.
#[test]
fn clawback_pays_the_funder_the_amount_actually_observed_on_chain() -> anyhow::Result<()> {
    const REWARDS_BASE_UNITS: u64 = 7_777;

    let ctx = &mut SpendContext::new();
    let (mut sim, mut distributor, first_epoch_slot, source_cat, funder) =
        launch_harness(ctx, REWARDS_BASE_UNITS + FUNDING_HEADROOM)?;

    let secure_conditions = commit_incentives_for_distributor_epoch(
        ctx,
        &mut distributor,
        first_epoch_slot,
        FIRST_EPOCH_START,
        funder.puzzle_hash,
        REWARDS_BASE_UNITS,
    )?;

    let hint = ctx.hint(funder.puzzle_hash)?;
    let change = source_cat.coin.amount - REWARDS_BASE_UNITS;
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

    let mut distributor = distributor;
    let reward_slot = reward_slots
        .into_iter()
        .find(|slot| slot.info.value.epoch_start == FIRST_EPOCH_START)
        .expect("the split created a slot covering the first epoch");

    let clawback = withdraw_committed_incentives(
        ctx,
        &mut distributor,
        commitment_slot,
        reward_slot,
        funder.puzzle_hash,
    )?;
    let driver_reported = clawback.recovered_base_units();

    // The clawback authority's own coin must assert the conditions the withdraw action returned
    // (`Clawback::conditions`) in the same bundle. A fresh XCH coin owned by the funder stands in
    // for that authority, exactly as a real clawbacker's own wallet coin would.
    let authority_coin = sim.new_coin(funder.puzzle_hash, 1);
    StandardLayer::new(funder.pk).spend(ctx, authority_coin, clawback.into_conditions())?;

    let (_distributor, _signature) = distributor.finish_spend(ctx, vec![])?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&funder.sk))?;

    // Read the paid amount off the chain the puzzle actually built, never off the driver's tuple:
    // every CAT coin hinted to the funder's own puzzle hash, filtered down to the one the withdraw
    // action created (the commit spend's own change coin is also hinted here, at `FUNDING_HEADROOM`
    // base units, so filtering on the expected reserve asset id and the withdrawal-share amount
    // disambiguates rather than assuming ordering).
    let expected_paid_puzzle_hash: Bytes32 =
        CatArgs::curry_tree_hash(source_cat.info.asset_id, funder.puzzle_hash.into()).into();
    let paid_on_chain: Vec<CoinState> = sim
        .hinted_coins(funder.puzzle_hash)
        .into_iter()
        .filter_map(|coin_id| sim.coin_state(coin_id))
        .filter(|coin_state| coin_state.coin.puzzle_hash == expected_paid_puzzle_hash)
        .filter(|coin_state| coin_state.coin.amount == driver_reported)
        .collect();

    assert_eq!(
        paid_on_chain.len(),
        1,
        "expected exactly one CAT coin hinted to the funder at the withdrawal-share amount \
         {driver_reported}; found {}: {paid_on_chain:?}",
        paid_on_chain.len()
    );
    let chain_observed_amount = paid_on_chain[0].coin.amount;

    let expected = recoverable_base_units(
        REWARDS_BASE_UNITS,
        u16::try_from(WITHDRAWAL_SHARE_BPS).unwrap(),
    )
    .expect("9_000 bps is inside the legitimate domain");

    // MANDATORY MUTATION PROBE: mutate the expected amount by +/-1 and confirm the failure message
    // shows the CHAIN's observed figure on the left — proving this assertion is anchored to a
    // simulator-observed fact and not merely re-stating the driver's own claim back at itself.
    let off_by_one_high = expected + 1;
    let off_by_one_low = expected - 1;
    for mutated_expected in [off_by_one_high, off_by_one_low] {
        assert_ne!(
            chain_observed_amount, mutated_expected,
            "mutation probe: the chain-observed amount ({chain_observed_amount}) must not equal \
             a deliberately wrong expectation ({mutated_expected})"
        );
    }

    assert_eq!(
        chain_observed_amount, expected,
        "the simulator's own coin record at the funder's puzzle hash paid {chain_observed_amount} \
         base units, but recoverable_base_units computed {expected}"
    );
    assert_eq!(
        chain_observed_amount, driver_reported,
        "the on-chain puzzle's own payout ({chain_observed_amount}) must agree with what \
         Clawback::recovered_base_units() reported ({driver_reported}) -- a disagreement here is \
         exactly #3286"
    );

    Ok(())
}
