//! #3425 -- is a commitment to an already-STARTED distributor epoch actually clawback-able on
//! chain, or does the puzzle refuse it?
//!
//! `commitment_slots()` still lists these commitments after their epoch has started, and
//! `withdraw_committed_incentives` (`src/clawback.rs:191`) still BUILDS a withdraw for one -- a
//! prior lane measured a build recovering 900,000 base units against a started epoch. No test in
//! this crate had ever SUBMITTED *that* withdraw -- a STARTED-epoch one. Clawback submission
//! itself is not new territory: #3295 (CLOSED SATISFIED, shipped 0.7.0, PR dig-rewards-coin#12)
//! established that
//! `tests/recoverable_share.rs::clawback_pays_the_funder_the_amount_actually_observed_on_chain`
//! already submits a clawback spend via `sim.spend_coins` and asserts the funder's on-chain CAT
//! amount -- but only for a commitment whose epoch has NOT started. The one prior attempt at a
//! started-epoch submission failed on `MessageNotSentOrReceived`, which is a missing funder-side
//! message in that attempt's own fixture (nothing had wired the clawback authority's own coin to
//! carry `Clawback::into_conditions()`, so the puzzle's `SEND_MESSAGE` was never met by a matching
//! `RECEIVE_MESSAGE`) -- a fixture bug, not a chain refusal, so it answered nothing about the
//! epoch question.
//!
//! ## The evidence ladder
//!
//! BUILT and SIGNED prove nothing about admission -- this epic already paid for that once, where a
//! slot derived from a distributor rebuilt at an earlier generation curried cleanly and signed
//! cleanly, and the network refused it as `UnknownUnspent` at push time, after a user had watched
//! a ceremony succeed. Both tests below go all the way to
//! `chia_sdk_test::Simulator::spend_coins`, which runs the real `chia_consensus` validator
//! (`validate_conditions` / `validate_relative_conditions`) and only returns `Ok` after calling
//! `create_block` -- CONFIRMED AT DEPTH, never a lower rung reported as this one.
//!
//! ## Two tests, one the other's control
//!
//! - [`a_not_yet_started_commitment_is_clawed_back_on_chain`]: commits to the first distributor
//!   epoch and claws it back immediately -- no `Sync`, no `NewEpoch` -- so the epoch is provably
//!   NOT started (`round_time_info.epoch_end` is still `FIRST_EPOCH_START` throughout). This is
//!   the harness proving it CAN submit a real withdraw at all: without it, a failure in the other
//!   test would be measuring this harness, not the chain.
//! - [`a_started_epoch_commitment_is_clawed_back_on_chain`]: commits to the same epoch, then rolls
//!   the distributor into it -- `sim.set_next_timestamp` + `start_next_distributor_epoch`, the
//!   same sequence `tests/simulator.rs::managed_dig_distributor_end_to_end` uses to make an epoch
//!   "current" -- and only THEN attempts the SAME commitment's withdraw.
//!
//! Both use the identical fixture shape (same launch, same commitment amount, same withdraw
//! wiring), differing only in whether `start_next_distributor_epoch` runs first, so the epoch
//! question is the only variable between them.
//!
//! This is a new file rather than an addition to `tests/simulator.rs` or
//! `tests/recoverable_share.rs` -- both carry live work elsewhere in this epic -- per
//! `recoverable_share.rs`'s own precedent ("this is a separate file ... so this ticket does not
//! collide with the concurrent #3267 work landing in that file").

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
    RewardDistributorSlotNonce,
};
use chia_sdk_types::{Conditions, TESTNET11_CONSTANTS};
use clvm_traits::{clvm_quote, ToClvm};
use clvmr::NodePtr;
use dig_rewards_coin::clawback::withdraw_committed_incentives;
use dig_rewards_coin::comment::LaunchComment;
use dig_rewards_coin::constants::{
    MAX_SECONDS_OFFSET, PAYOUT_THRESHOLD_BASE_UNITS, WITHDRAWAL_SHARE_BPS,
};
use dig_rewards_coin::epoch::{current_distributor_epoch_end, start_next_distributor_epoch};
use dig_rewards_coin::fund::commit_incentives_for_distributor_epoch;
use dig_rewards_coin::launch::launch_dig_distributor;
use dig_rewards_coin::recoverable_base_units;

/// The first distributor epoch starts here -- small, because the simulator's clock starts at zero.
const FIRST_EPOCH_START: u64 = 1_234;

/// A short distributor epoch; these tests are about whether value can leave, not how long an
/// epoch is.
const TEST_EPOCH_SECONDS: u64 = 1_000;

/// What the funder commits. `9_000` bps of this is `900_000` -- the same figure a prior lane
/// measured a BUILD recover against a started epoch; this file measures whether it also SUBMITS.
const COMMITTED_BASE_UNITS: u64 = 1_000_000;

/// Headroom above the commitment so the funder can pay the commit spend's own CAT change output.
const FUNDING_HEADROOM: u64 = 1_000;

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

/// The DIG constants table with the simulator's asset id substituted for $DIG's, matching
/// `tests/recoverable_share.rs`'s own `test_constants`.
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

/// Make a coin whose whole puzzle is "these conditions must hold", so a permissionless action's
/// conditions get asserted by something -- copied from `tests/simulator.rs`'s own helper of the
/// same name.
fn ensure_conditions_met(
    ctx: &mut SpendContext,
    sim: &mut Simulator,
    conditions: Conditions<NodePtr>,
) -> anyhow::Result<()> {
    let checker_puzzle = clvm_quote!(conditions).to_clvm(ctx)?;
    let checker_coin = sim.new_coin(ctx.tree_hash(checker_puzzle).into(), 0);
    ctx.spend(checker_coin, Spend::new(checker_puzzle, NodePtr::NIL))?;

    Ok(())
}

/// Launch a distributor whose funder holds `minted_base_units` of $DIG and nothing has been
/// committed yet. Identical in shape to `tests/recoverable_share.rs`'s own `launch_harness`.
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

/// Everything a committed distributor needs before a withdraw attempt.
struct Committed {
    sim: Simulator,
    distributor: RewardDistributor,
    commitment_slot: Slot<RewardDistributorCommitmentSlotValue>,
    reward_slot: Slot<RewardDistributorRewardSlotValue>,
    funder: chia_sdk_test::BlsPairWithCoin,
    asset_id: Bytes32,
}

/// Launch, then commit `amount_base_units` to [`FIRST_EPOCH_START`], leaving the commitment
/// withdrawable and every handle a withdraw needs in hand. Identical in shape to
/// `tests/recoverable_share.rs::commit_to_first_epoch`.
fn commit_to_first_epoch(
    ctx: &mut SpendContext,
    amount_base_units: u64,
) -> anyhow::Result<Committed> {
    let (mut sim, mut distributor, reward_slot, source_cat, funder) =
        launch_harness(ctx, amount_base_units + FUNDING_HEADROOM)?;
    let asset_id = source_cat.info.asset_id;

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
            distributor.created_slot_value_to_slot(*value, RewardDistributorSlotNonce::REWARD)
        })
        .collect();

    let commitment_slot: Slot<RewardDistributorCommitmentSlotValue> = distributor
        .pending_spend
        .created_commitment_slots
        .first()
        .copied()
        .map(|value| {
            distributor.created_slot_value_to_slot(value, RewardDistributorSlotNonce::COMMITMENT)
        })
        .expect("committing created a commitment slot");

    let (distributor, _) = distributor.finish_spend(ctx, vec![source_cat_spend])?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&funder.sk))?;

    let reward_slot = reward_slots
        .into_iter()
        .find(|slot| slot.info.value.epoch_start == FIRST_EPOCH_START)
        .expect("the commit created a slot covering the first epoch");

    Ok(Committed {
        sim,
        distributor,
        commitment_slot,
        reward_slot,
        funder,
        asset_id,
    })
}

/// Roll `committed.distributor` into [`FIRST_EPOCH_START`] -- the SAME sequence
/// `tests/simulator.rs::managed_dig_distributor_end_to_end` uses to make an epoch "current":
/// advance the simulator's clock, spend `NewEpoch` against the epoch's own reward slot, submit.
///
/// Returns the reward slot `NewEpoch` re-created, which is what a withdraw after this point must
/// use: `RewardDistributor::actual_reward_slot_value` only substitutes within the SAME
/// `pending_spend` generation, and this call crosses a `finish_spend` boundary, so the caller
/// cannot rely on it to find the post-roll slot automatically.
fn roll_into_first_epoch(
    ctx: &mut SpendContext,
    committed: &mut Committed,
) -> anyhow::Result<Slot<RewardDistributorRewardSlotValue>> {
    committed.sim.set_next_timestamp(FIRST_EPOCH_START)?;

    let roll = start_next_distributor_epoch(ctx, &mut committed.distributor, committed.reward_slot.clone())?;
    assert_eq!(
        roll.fee_base_units, 0,
        "SPEC.md §7.3: fee_bps is zero for a DIG distributor"
    );

    let rolled_reward_slot = committed
        .distributor
        .pending_spend
        .created_reward_slots
        .iter()
        .find(|value| value.epoch_start == FIRST_EPOCH_START)
        .copied()
        .map(|value| {
            committed
                .distributor
                .created_slot_value_to_slot(value, RewardDistributorSlotNonce::REWARD)
        })
        .expect("NewEpoch recreates the reward slot it spent, at the same epoch_start");

    ensure_conditions_met(ctx, &mut committed.sim, roll.conditions)?;
    let (distributor, _) = committed.distributor.clone().finish_spend(ctx, vec![])?;
    committed.distributor = distributor;
    committed.sim.spend_coins(ctx.take(), &[])?;

    Ok(rolled_reward_slot)
}

/// Build the withdraw for `committed`'s commitment against `reward_slot`, wire the funder-side
/// message the puzzle's `SEND_MESSAGE` needs answered (`Clawback::into_conditions()` attached to a
/// fresh coin the funder controls -- the step the prior failed attempt in this epic's history left
/// out, which is what actually produced `MessageNotSentOrReceived` there), and submit all the way
/// through the simulator's real consensus validator.
///
/// Tags each rung distinctly in the returned error's context, per the evidence ladder: a failure
/// here is either BUILD (the driver refused before touching the chain), SIGN (the authority coin's
/// spend could not be constructed), or SUBMIT (the simulator's validator refused the bundle) --
/// never conflated.
fn submit_withdraw(
    ctx: &mut SpendContext,
    committed: &mut Committed,
    reward_slot: Slot<RewardDistributorRewardSlotValue>,
) -> anyhow::Result<u64> {
    use anyhow::Context;

    let funder_puzzle_hash = committed.funder.puzzle_hash;

    let clawback = withdraw_committed_incentives(
        ctx,
        &mut committed.distributor,
        committed.commitment_slot.clone(),
        reward_slot,
        funder_puzzle_hash,
    )
    .context("BUILD: withdraw_committed_incentives refused before reaching the chain")?;
    let driver_reported = clawback.recovered_base_units();

    let authority_coin = committed.sim.new_coin(funder_puzzle_hash, 1);
    StandardLayer::new(committed.funder.pk)
        .spend(ctx, authority_coin, clawback.into_conditions())
        .context("SIGN: the clawback authority's own coin spend could not be constructed")?;

    let (distributor, _signature) = committed
        .distributor
        .clone()
        .finish_spend(ctx, vec![])
        .context("BUILD: the distributor's own generation spend could not be constructed")?;
    committed.distributor = distributor;

    committed
        .sim
        .spend_coins(ctx.take(), std::slice::from_ref(&committed.funder.sk))
        .map(|_| driver_reported)
        .context("SUBMIT: the simulator's consensus validator refused the bundle")
}

/// The reward-CAT coin(s) that appeared hinted to `funder_puzzle_hash` during the submission --
/// never selected by amount, which would make a comparison against it circular. Copied from
/// `tests/recoverable_share.rs::clawback_pays_the_funder_the_amount_actually_observed_on_chain`'s
/// own selection logic.
fn paid_on_chain(
    sim: &Simulator,
    asset_id: Bytes32,
    funder_puzzle_hash: Bytes32,
    hinted_before: &[Bytes32],
) -> Vec<CoinState> {
    let expected_paid_puzzle_hash: Bytes32 =
        CatArgs::curry_tree_hash(asset_id, funder_puzzle_hash.into()).into();

    sim.hinted_coins(funder_puzzle_hash)
        .into_iter()
        .filter(|coin_id| !hinted_before.contains(coin_id))
        .filter_map(|coin_id| sim.coin_state(coin_id))
        .filter(|coin_state| coin_state.coin.puzzle_hash == expected_paid_puzzle_hash)
        .collect()
}

/// CONTROL for the epoch question -- mirrors #3295's already-established, already-passing
/// `tests/recoverable_share.rs::clawback_pays_the_funder_the_amount_actually_observed_on_chain`
/// (same construction, reused here via the shared [`submit_withdraw`] helper) so this file's own
/// harness is not itself a new, unproven code path.
///
/// Commits to [`FIRST_EPOCH_START`] and claws it back with no `Sync`/`NewEpoch` in between, so
/// `round_time_info.epoch_end` is still `FIRST_EPOCH_START` throughout: the epoch is provably NOT
/// started. The assertions below require `chia_sdk_test::Simulator::spend_coins` to accept the
/// bundle and the resulting coin state at the funder's puzzle hash to pay exactly
/// `recoverable_base_units`'s figure -- running this test is what measures the rung reached, not
/// this comment. Without this test passing, a failure in
/// [`a_started_epoch_commitment_is_clawed_back_on_chain`] would answer nothing about the epoch
/// question -- it would just mean this harness cannot submit a withdraw at all.
#[test]
fn a_not_yet_started_commitment_is_clawed_back_on_chain() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let mut committed = commit_to_first_epoch(ctx, COMMITTED_BASE_UNITS)?;
    let funder_puzzle_hash = committed.funder.puzzle_hash;
    let asset_id = committed.asset_id;

    assert_eq!(
        current_distributor_epoch_end(&committed.distributor),
        FIRST_EPOCH_START,
        "control precondition: the epoch must NOT have started yet"
    );

    let hinted_before = committed.sim.hinted_coins(funder_puzzle_hash);
    let reward_slot = committed.reward_slot.clone();

    let driver_reported = submit_withdraw(ctx, &mut committed, reward_slot)
        .expect("CONFIRMED: a not-yet-started commitment's withdraw was accepted on chain");

    let paid = paid_on_chain(&committed.sim, asset_id, funder_puzzle_hash, &hinted_before);
    assert_eq!(
        paid.len(),
        1,
        "the withdraw must create exactly one reward-CAT coin hinted to the funder; found {}: {paid:?}",
        paid.len()
    );

    let expected = recoverable_base_units(
        COMMITTED_BASE_UNITS,
        u16::try_from(WITHDRAWAL_SHARE_BPS).unwrap(),
    )
    .expect("9_000 bps is inside the legitimate domain");

    assert_eq!(
        paid[0].coin.amount, expected,
        "the simulator's own coin record at the funder's puzzle hash paid {} base units, but the \
         expectation was {expected}",
        paid[0].coin.amount
    );
    assert_eq!(
        paid[0].coin.amount, driver_reported,
        "the on-chain payout ({}) must agree with what Clawback::recovered_base_units() reported \
         ({driver_reported})",
        paid[0].coin.amount
    );

    Ok(())
}

/// #3425 -- the question itself.
///
/// Commits to [`FIRST_EPOCH_START`], rolls the distributor into that epoch (`NewEpoch`), and only
/// THEN attempts the SAME commitment's withdraw -- the started-epoch case this ticket asks about.
/// Built via the identical [`submit_withdraw`] helper
/// [`a_not_yet_started_commitment_is_clawed_back_on_chain`] uses, differing only in the
/// [`roll_into_first_epoch`] call before the withdraw -- the epoch state is the single variable
/// under test.
///
/// This comment asserts no result. Whether the simulator's real consensus validator accepts or
/// refuses a started-epoch withdraw is exactly what running this test measures, not something to
/// claim in advance of running it -- BUILT and SIGNED prove nothing about admission either way;
/// this epic already paid for that lesson once, where a slot derived from a distributor rebuilt at
/// an earlier generation curried cleanly and signed cleanly, and the network refused it as
/// `UnknownUnspent` at push time, after a user had watched a ceremony succeed. Only this test's own
/// `spend_coins` call -- ADMITTED or CONFIRMED, never a lower rung dressed up as this one -- answers
/// the question; see the ticket (#3425) for the rung actually reached and the chain's actual
/// response.
#[test]
fn a_started_epoch_commitment_is_clawed_back_on_chain() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let mut committed = commit_to_first_epoch(ctx, COMMITTED_BASE_UNITS)?;
    let funder_puzzle_hash = committed.funder.puzzle_hash;
    let asset_id = committed.asset_id;

    let rolled_reward_slot = roll_into_first_epoch(ctx, &mut committed)?;
    assert!(
        current_distributor_epoch_end(&committed.distributor) > FIRST_EPOCH_START,
        "the epoch must actually have rolled forward before the withdraw is attempted"
    );

    let hinted_before = committed.sim.hinted_coins(funder_puzzle_hash);

    let driver_reported = submit_withdraw(ctx, &mut committed, rolled_reward_slot)
        .expect("CONFIRMED: a started-epoch commitment's withdraw was accepted on chain");

    let paid = paid_on_chain(&committed.sim, asset_id, funder_puzzle_hash, &hinted_before);
    assert_eq!(
        paid.len(),
        1,
        "the withdraw must create exactly one reward-CAT coin hinted to the funder; found {}: {paid:?}",
        paid.len()
    );

    let expected = recoverable_base_units(
        COMMITTED_BASE_UNITS,
        u16::try_from(WITHDRAWAL_SHARE_BPS).unwrap(),
    )
    .expect("9_000 bps is inside the legitimate domain");
    assert_eq!(
        expected, 900_000,
        "pinned to the same figure a prior lane's BUILD-only measurement reported"
    );

    assert_eq!(
        paid[0].coin.amount, expected,
        "the simulator's own coin record at the funder's puzzle hash paid {} base units, but the \
         expectation was {expected}",
        paid[0].coin.amount
    );
    assert_eq!(
        paid[0].coin.amount, driver_reported,
        "the on-chain payout ({}) must agree with what Clawback::recovered_base_units() reported \
         ({driver_reported})",
        paid[0].coin.amount
    );

    Ok(())
}
