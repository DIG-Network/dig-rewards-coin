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

use chia_protocol::{Bytes32, SpendBundle};
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

    Ok(clawback.recovered_base_units)
}
/// One in-range case: commit `rewards_base_units`, claw it back, and require both that the puzzle
/// paid `expected_paid` and that [`recoverable_base_units`] returns the same figure.
fn assert_matches_a_real_clawback(
    rewards_base_units: u64,
    expected_paid: u64,
) -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let (mut sim, distributor, first_epoch_slot, source_cat, funder) =
        launch_harness(ctx, 1_000_000)?;

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
        paid,
        "recoverable_base_units must equal what the puzzle actually paid"
    );

    Ok(())
}

/// Equality with the real paid amount, for amounts inside the range where upstream is defined.
///
/// Both cases truncate — `1_001 @ 9_000 bps` is `900.9` and `7_777 @ 9_000 bps` is `6_999.3` — so
/// neither would pass under a rounding bug, and neither would pass if the restatement divided
/// before it multiplied (`1_001 / 10_000` is `0`).
#[test]
fn recoverable_base_units_matches_a_real_clawback_at_odd_amounts() -> anyhow::Result<()> {
    assert_eq!(
        WITHDRAWAL_SHARE_BPS, 9_000,
        "the fixture assumes the crate's own launch bps"
    );

    assert_matches_a_real_clawback(1_001, 900)?;
    assert_matches_a_real_clawback(7_777, 6_999)?;

    Ok(())
}

/// A value large enough that `rewards_base_units * withdrawal_share_bps` overflows a plain `u64`
/// multiply (`2_000_000_000_000_000_000 * 9_000` is roughly `1.8e22`, far past `u64::MAX`'s
/// `~1.8e19`), proving the `u128` intermediate in `recoverable_base_units` is load-bearing rather
/// than decorative: drop the intermediate and this test panics or returns a wrapped value.
///
/// **This case deliberately asserts nothing about what a real clawback pays, and a future reader
/// must not "helpfully" add that comparison back.** At this scale a real clawback pays nothing at
/// all: upstream's own withdrawal share
/// (`chia-sdk-driver-0.36.0/src/layers/action_layer/actions/reward_distributor/withdraw_incentives.rs:105-107`)
/// is a plain `u64` multiply of `rewards` by `withdrawal_share_bps`, so it panics under overflow
/// checks — which is exactly how this case first failed in CI — and wraps silently without them.
/// An equality assertion here would be unsatisfiable by construction, because there is no correct
/// amount for our figure to be equal to. The bound is `u64::MAX / withdrawal_share_bps`, about
/// `2.05e15` base units at 9_000 bps; equality above it is tested by
/// `recoverable_base_units_matches_a_real_clawback_at_odd_amounts`, below it.
#[test]
fn recoverable_base_units_does_not_overflow_where_a_plain_u64_multiply_would() {
    const REWARDS: u64 = 2_000_000_000_000_000_000;
    const BPS: u16 = 9_000;

    // The expected value, computed independently in u128 so this assertion does not simply
    // restate the function under test.
    let expected = u64::try_from(u128::from(REWARDS) * u128::from(BPS) / 10_000)
        .expect("fits back in u64: the quotient never exceeds rewards_base_units");

    assert_eq!(recoverable_base_units(REWARDS, BPS), expected);

    // Sanity check the overflow premise: `REWARDS * (BPS as u64)` alone cannot fit in a u64.
    assert!(REWARDS.checked_mul(u64::from(BPS)).is_none());
}
