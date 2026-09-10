//! `recoverable_base_units` proved equal to what a real clawback actually pays.
//!
//! This binds `src/clawback.rs`'s one authoritative restatement of the withdrawal-share
//! arithmetic to the paying puzzle code
//! (`chia-sdk-driver-0.36.0/src/layers/action_layer/actions/reward_distributor/withdraw_incentives.rs:105-107`):
//! if a future upstream bump changes that arithmetic, [`Clawback::recovered_base_units`] and
//! [`recoverable_base_units`] diverge and this test goes red. A simulator test proving equality on
//! a round number would pass under a rounding bug (round numbers do not exercise truncation) and
//! under a lost precision bug (small numbers never overflow), so neither case here is round or
//! small.
//!
//! This is a separate file from `tests/simulator.rs` rather than an addition to it, so this ticket
//! does not collide with the concurrent #3267 work landing in that file.

use chia_protocol::{Bytes32, Coin, SpendBundle};
use chia_puzzle_types::singleton::SingletonArgs;
use chia_puzzle_types::{CoinProof, EveProof, LineageProof, Memos, Proof};
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
    launcher.spend(ctx, inner_puzzle_hash.into(), ())?;

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

/// `1_001 @ 9_000 bps` is `900.9` truncated to `900` — a round number would pass under a rounding
/// bug, so this is deliberately not one.
#[test]
fn recoverable_base_units_matches_a_real_clawback_at_an_odd_amount() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let (mut sim, distributor, first_epoch_slot, source_cat, funder) =
        launch_harness(ctx, 1_000_000)?;

    const REWARDS: u64 = 1_001;
    assert_eq!(
        WITHDRAWAL_SHARE_BPS, 9_000,
        "the fixture assumes the crate's own launch bps"
    );

    let paid = commit_then_clawback(
        ctx,
        &mut sim,
        distributor,
        first_epoch_slot,
        source_cat,
        &funder,
        REWARDS,
    )?;

    assert_eq!(
        paid, 900,
        "the puzzle truncates 1_001 * 9_000 / 10_000 = 900.9 down to 900"
    );
    assert_eq!(
        recoverable_base_units(REWARDS, u16::try_from(WITHDRAWAL_SHARE_BPS).unwrap()),
        paid,
        "recoverable_base_units must equal what the puzzle actually paid"
    );

    Ok(())
}

/// A value large enough that `rewards_base_units * withdrawal_share_bps` overflows a plain u64
/// multiply (`2_000_000_000_000_000_000 * 9_000` is roughly `1.8e22`, far past `u64::MAX`'s
/// `~1.8e19`), proving the u128 intermediate in `recoverable_base_units` is load-bearing and not
/// merely defensive.
///
/// This is deliberately **not** driven through a real clawback spend: at this scale the puzzle's
/// own `withdrawal_share` line (`withdraw_incentives.rs:105`) panics on the same u64 overflow —
/// verified by first running this case through `commit_then_clawback` and watching it panic
/// exactly there (see the mutation-probe report). That panic is itself the proof the u128
/// intermediate matters: production code calling the real SDK at this scale would already be
/// broken, and `recoverable_base_units` must not import that ceiling into a function whose caller
/// (`dig.listRewardDistributorCommitments`) has no such bound of its own.
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
