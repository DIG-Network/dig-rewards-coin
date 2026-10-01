//! #3425 -- is a commitment to an already-STARTED distributor epoch actually clawback-able on
//! chain, or does the puzzle refuse it?
//!
//! `commitment_slots()` still lists these commitments after their epoch has started. Before 0.11.0
//! `withdraw_committed_incentives` still BUILT a withdraw for one -- a prior lane measured a build
//! recovering 900,000 base units against a started epoch; since dig_ecosystem#3444 it refuses with
//! `RewardsError::CommitmentEpochStarted` before building anything (SPEC.md §7.4 clause 7), and the
//! started-epoch tests below reach the chain through [`submit_withdraw_bypassing_the_guard`]
//! instead, so they still measure what the chain itself does. No test in
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
//! ## Verdict: REFUSED, on chain, by design -- not a fixture artifact
//!
//! A started-epoch commitment's withdraw builds and signs cleanly and is then refused at
//! `chia_sdk_test::Simulator::spend_coins` (the real `chia_consensus` validator) with
//! `Validation error: AssertBeforeSecondsAbsoluteFailed`. Four tests below triangulate this to a
//! single mechanism and rule out every confound this epic has previously been burned by:
//!
//! - The bound is **wall-clock time versus the commitment's own `epoch_start`**, strict-before --
//!   not `last_update`, not `MAX_SECONDS_OFFSET`, and not whether the distributor's own on-chain
//!   generation has actually rolled into that epoch (`NewEpoch`) yet.
//! - [`a_wall_clock_reaching_epoch_start_refuses_the_withdraw_even_without_an_epoch_roll`] jumps
//!   the simulator's clock to exactly `FIRST_EPOCH_START` **without** ever calling
//!   `start_next_distributor_epoch`, and still gets refused with the identical error. This rules
//!   out epoch-roll state entirely: the distributor's own `round_time_info.epoch_end` never moves
//!   in this test, so whatever is asserting cannot be reading it.
//! - [`a_withdraw_one_second_before_epoch_start_still_succeeds_pinning_the_exact_boundary`] jumps
//!   the clock to `FIRST_EPOCH_START - 1` and gets `Ok(900_000)` -- one second earlier, same
//!   commitment, same fixture. This pins the boundary to `epoch_start` exactly (not
//!   `epoch_start - MAX_SECONDS_OFFSET`, which sits 300 seconds further back and would still have
//!   refused a jump to `epoch_start - 1`).
//!
//! Rust-visible provenance for the bound: `withdraw_committed_incentives`'s solution carries
//! `reward_slot_epoch_time: reward_slot.info.value.epoch_start`
//! (`chia-sdk-driver-0.36.0/src/layers/action_layer/actions/reward_distributor/withdraw_incentives.rs:125`,
//! matching solution field at
//! `chia-sdk-types-0.36.0/src/puzzles/action_layer/actions/reward_distributor/withdraw_incentives.rs:60`).
//! No Rust-level `.assert_before_seconds_absolute()` call exists anywhere in this crate's
//! `clawback.rs`, `commit.rs`, `epoch.rs`, or in `chia-sdk-driver`'s `Slot::spend` /
//! `RewardDistributor::finish_spend` / `insert_action_spend` -- the assertion is compiled into the
//! withdraw-incentives puzzle itself, downstream of that curried value, and only observable by
//! running the chain's own validator against it, which is what the two isolation tests do.
//!
//! **This is legitimate design, not a bug in the reward distributor's own logic.** Once an epoch's
//! wall-clock time arrives, that epoch's committed incentives are what fund its payouts to
//! entries; a funder cannot unilaterally claw a commitment back out from under payouts that are
//! about to be computed against it. That money-honesty gap is closed in 0.10.0 (dig_ecosystem#3439): the only recoverable figure this crate reports is `Commitment::recoverable_base_units()`, computed against the chain clock of the read, and the cases below assert it agrees with what the chain pays or refuses.
//!
//! ## Four tests, each isolating one variable
//!
//! - [`a_not_yet_started_commitment_is_clawed_back_on_chain`]: commits to the first distributor
//!   epoch and claws it back immediately -- no `Sync`, no `NewEpoch` -- so the epoch is provably
//!   NOT started (`round_time_info.epoch_end` is still `FIRST_EPOCH_START` throughout). This is
//!   the harness proving it CAN submit a real withdraw at all: without it, a failure in the other
//!   tests would be measuring this harness, not the chain.
//! - [`a_started_epoch_commitment_is_clawed_back_on_chain`]: commits to the same epoch, then rolls
//!   the distributor into it -- `sim.set_next_timestamp` + `start_next_distributor_epoch`, the
//!   same sequence `tests/simulator.rs::managed_dig_distributor_end_to_end` uses to make an epoch
//!   "current" -- and only THEN attempts the SAME commitment's withdraw. This is the ticket's own
//!   question, and it measures REFUSED.
//! - [`a_wall_clock_reaching_epoch_start_refuses_the_withdraw_even_without_an_epoch_roll`] and
//!   [`a_withdraw_one_second_before_epoch_start_still_succeeds_pinning_the_exact_boundary`]: the
//!   isolation pair described above.
//!
//! All four use the identical fixture shape (same launch, same commitment amount, same withdraw
//! wiring) via the shared [`commit_to_first_epoch`] / [`submit_clawback_conditions`] helpers,
//! differing only in the simulator clock and whether `start_next_distributor_epoch` runs, so the
//! epoch/clock state is the only variable under test in each. The two refused cases also assert
//! that the guarded builder, handed the same observation, refuses before building -- guard and
//! chain pinned together in one test.
//!
//! ## The build-path guard (dig_ecosystem#3444)
//!
//! [`withdraw_refuses_a_started_epoch_commitment_before_building`],
//! [`withdraw_refuses_at_exactly_epoch_start`] and [`withdraw_builds_one_second_before_epoch_start`]
//! pin `withdraw_committed_incentives`' own refusal to the same one-second boundary the chain
//! enforces.
//!
//! This is a new file rather than an addition to `tests/simulator.rs` or
//! `tests/recoverable_share.rs` -- both carry live work elsewhere in this epic -- per
//! `recoverable_share.rs`'s own precedent ("this is a separate file ... so this ticket does not
//! collide with the concurrent #3267 work landing in that file").

use chia_consensus::validation_error::ErrorCode;
use chia_protocol::{Bytes32, CoinState, SpendBundle};
use chia_puzzle_types::cat::CatArgs;
use chia_puzzle_types::{CoinProof, Memos};
use chia_puzzles::{SETTLEMENT_PAYMENT_HASH, SINGLETON_LAUNCHER_HASH};
use chia_sdk_driver::{
    sign_standard_transaction, Cat, CatSpend, Launcher, Offer, RewardDistributor,
    RewardDistributorConstants, RewardDistributorType, RewardDistributorWithdrawIncentivesAction,
    SingleCatSpend, Slot, Spend, SpendContext, SpendWithConditions, StandardLayer,
};
use chia_sdk_test::{Simulator, SimulatorError};
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
use dig_rewards_coin::state::{read_distributor, ChainObservation, DistributorSnapshot};
use dig_rewards_coin::RewardsError;

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

    /// The distributor's own singleton coin id at every post-eve generation so far, launcher
    /// first -- what [`read_snapshot`] needs to rebuild a [`dig_chainsource_interface::MockChainSource`]
    /// wide enough for [`read_distributor`] to walk, mirroring `tests/simulator.rs::mock_chain_source`'s
    /// own convention (that file cannot be reused here: each `tests/*.rs` file is its own binary).
    singleton_members: Vec<Bytes32>,
    launcher_id: Bytes32,
    reserve_launch_id: Bytes32,
    reserve_parent_id: Bytes32,
}

/// Rebuild a [`DistributorSnapshot`] from `committed`'s own simulator state, at chain time
/// `peak_timestamp` -- the SAME `t` the caller passes to `committed.sim.set_next_timestamp(t)`
/// before submitting. `mock_chain_source` (`tests/simulator.rs:1624-1633`) stamps every height with
/// a SYNTHETIC timestamp; feeding a different clock here than the one the simulator validates the
/// submit against would compare two unrelated numbers and prove nothing (dig_ecosystem#3439, the
/// decision's F4). The assertion below is what turns that mismatch into a loud failure instead of a
/// silent one.
fn read_snapshot(
    committed: &Committed,
    peak_timestamp: u64,
) -> anyhow::Result<DistributorSnapshot> {
    let mut source = dig_chainsource_interface::MockChainSource::new();

    // The eve coin is a child of the launcher, spent to produce the first post-eve generation.
    // `read_distributor` needs that spend (`from_eve_coin_spend` parses it) even though it is
    // never itself a member of `singleton_members` -- the same exception
    // `tests/simulator.rs::mock_chain_source` makes.
    let eve_coin_id = committed
        .sim
        .children(committed.launcher_id)
        .first()
        .map(|state| state.coin.coin_id());

    let reserve_tip_id = committed.distributor.reserve.coin.coin_id();

    let ids = committed
        .singleton_members
        .iter()
        .copied()
        .chain(eve_coin_id)
        .chain([
            committed.reserve_launch_id,
            committed.reserve_parent_id,
            reserve_tip_id,
        ]);

    for id in ids {
        if let Some(state) = committed.sim.coin_state(id) {
            source = source.with_coin(
                id,
                dig_chainsource_interface::CoinRecord::from_coin_state(state),
            );
        }
        if let Some(spend) = committed.sim.coin_spend(id) {
            source = source.with_spend(id, spend);
        }
    }

    let tip = *committed
        .singleton_members
        .last()
        .expect("a singleton chain always has at least the launcher and one generation");
    source = source.with_lineage(
        committed.launcher_id,
        dig_chainsource_interface::SingletonLineage::new(
            tip,
            committed.singleton_members.iter().copied(),
        ),
    );

    let peak = committed.sim.height();
    let source = source.with_timestamp(peak, peak_timestamp).with_peak(peak);

    let snapshot = read_distributor(&source, committed.launcher_id)?
        .expect("the distributor's launcher coin is on chain");

    assert_eq!(
        snapshot.observed().peak_timestamp(),
        peak_timestamp,
        "the report must be read against the SAME clock the chain validates the submit against, \
         or the two sides of assert_report_agrees_with_chain compare unrelated numbers"
    );

    Ok(snapshot)
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

    let launcher_id = distributor.info.constants.launcher_id;
    let mut singleton_members = vec![launcher_id, distributor.coin.coin_id()];
    let reserve_launch_id = distributor.reserve.coin.coin_id();
    let reserve_parent_id = distributor.reserve.coin.parent_coin_info;

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
    singleton_members.push(distributor.coin.coin_id());

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
        singleton_members,
        launcher_id,
        reserve_launch_id,
        reserve_parent_id,
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

    let roll = start_next_distributor_epoch(
        ctx,
        &mut committed.distributor,
        committed.reward_slot.clone(),
    )?;
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
    committed
        .singleton_members
        .push(committed.distributor.coin.coin_id());

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
///
/// This is the guarded public path: `observed` is what `withdraw_committed_incentives` judges the
/// commitment's epoch against (SPEC.md §7.4 clause 7).
fn submit_withdraw(
    ctx: &mut SpendContext,
    committed: &mut Committed,
    reward_slot: Slot<RewardDistributorRewardSlotValue>,
    observed: &ChainObservation,
) -> anyhow::Result<u64> {
    use anyhow::Context;

    let clawback = withdraw_committed_incentives(
        ctx,
        &mut committed.distributor,
        committed.commitment_slot.clone(),
        reward_slot,
        committed.funder.puzzle_hash,
        observed,
    )
    .context("BUILD: withdraw_committed_incentives refused before reaching the chain")?;
    let driver_reported = clawback.recovered_base_units();

    submit_clawback_conditions(ctx, committed, clawback.into_conditions(), driver_reported)
}

/// Build the SAME withdraw [`submit_withdraw`] builds, but straight through `chia-sdk-driver`'s
/// own action, skipping this crate's guard, and submit it.
///
/// Exists only to prove the chain refuses what the guard refuses: since dig_ecosystem#3444 the
/// guarded path refuses a started epoch before building, so without this the chain's own
/// `AssertBeforeSecondsAbsoluteFailed` could no longer be observed at all.
fn submit_withdraw_bypassing_the_guard(
    ctx: &mut SpendContext,
    committed: &mut Committed,
    reward_slot: Slot<RewardDistributorRewardSlotValue>,
) -> anyhow::Result<u64> {
    use anyhow::Context;

    let actual_commitment_slot = committed
        .distributor
        .actual_commitment_slot_value(committed.commitment_slot.clone());
    let (conditions, driver_reported) = committed
        .distributor
        .new_action::<RewardDistributorWithdrawIncentivesAction>()
        .spend(
            ctx,
            &mut committed.distributor,
            actual_commitment_slot,
            reward_slot,
        )
        .context("BUILD: the driver's own withdraw action could not be built")?;

    submit_clawback_conditions(ctx, committed, conditions, driver_reported)
}

/// The shared tail of both withdraw paths: answer the puzzle's `SEND_MESSAGE` from a fresh coin the
/// funder controls, finish the distributor's generation and submit, returning `driver_reported` iff
/// the chain accepted the bundle.
fn submit_clawback_conditions(
    ctx: &mut SpendContext,
    committed: &mut Committed,
    conditions: Conditions,
    driver_reported: u64,
) -> anyhow::Result<u64> {
    use anyhow::Context;

    let funder_puzzle_hash = committed.funder.puzzle_hash;
    let authority_coin = committed.sim.new_coin(funder_puzzle_hash, 1);
    StandardLayer::new(committed.funder.pk)
        .spend(ctx, authority_coin, conditions)
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

    let reported = reported_recoverable(&committed, committed.sim.next_timestamp())?;
    let observed = observed_at(&committed, committed.sim.next_timestamp())?;

    let submit_result = submit_withdraw(ctx, &mut committed, reward_slot, &observed);
    let driver_reported = match &submit_result {
        Ok(amount) => *amount,
        Err(error) => panic!(
            "CONFIRMED: a not-yet-started commitment's withdraw was accepted on chain, got {error:#}"
        ),
    };
    assert_report_agrees_with_chain(reported, submit_result);

    let paid = paid_on_chain(&committed.sim, asset_id, funder_puzzle_hash, &hinted_before);
    assert_eq!(
        paid.len(),
        1,
        "the withdraw must create exactly one reward-CAT coin hinted to the funder; found {}: {paid:?}",
        paid.len()
    );

    let expected = reported.expect(
        "commitments() must have reported a figure for a not-yet-started commitment -- \
         assert_report_agrees_with_chain above would already have failed otherwise",
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

/// The figure `commitments()` reports for `committed`'s own commitment slot, read against the
/// chain clock `peak_timestamp` -- the same clock `submit_withdraw` is about to be validated
/// against, so a caller can compare the two honestly.
fn reported_recoverable(committed: &Committed, peak_timestamp: u64) -> anyhow::Result<Option<u64>> {
    let snapshot = read_snapshot(committed, peak_timestamp)?;
    let commitment = snapshot
        .commitments()
        .iter()
        .find(|commitment| commitment.slot().info.value == committed.commitment_slot.info.value)
        .expect("read_distributor must report the commitment slot this test just created");
    Ok(commitment.recoverable_base_units())
}

/// A real [`ChainObservation`] of `committed`'s distributor at chain time `peak_timestamp`, minted
/// by `read_distributor` itself -- the only way one can exist -- so the guard is judged against the
/// same clock [`reported_recoverable`] and the simulator use.
fn observed_at(committed: &Committed, peak_timestamp: u64) -> anyhow::Result<ChainObservation> {
    Ok(*read_snapshot(committed, peak_timestamp)?.observed())
}

/// Require the guarded builder to refuse `committed`'s withdraw with the exact typed
/// [`RewardsError::CommitmentEpochStarted`], and to have built nothing doing so: SPEC.md §7.4
/// clause 7 puts the refusal before any spend, so the distributor's pending actions and the spend
/// context must be exactly as they were.
fn assert_guard_refuses_before_building(
    ctx: &mut SpendContext,
    committed: &mut Committed,
    reward_slot: Slot<RewardDistributorRewardSlotValue>,
    observed: &ChainObservation,
) {
    let actions_before = committed.distributor.pending_spend.actions.len();

    let result = withdraw_committed_incentives(
        ctx,
        &mut committed.distributor,
        committed.commitment_slot.clone(),
        reward_slot,
        committed.funder.puzzle_hash,
        observed,
    );

    match result {
        Err(RewardsError::CommitmentEpochStarted {
            distributor_epoch_start,
            peak_timestamp,
        }) => {
            assert_eq!(distributor_epoch_start, FIRST_EPOCH_START);
            assert_eq!(peak_timestamp, observed.peak_timestamp());
        }
        Ok(clawback) => panic!(
            "withdraw_committed_incentives built a withdraw recovering {} base units at chain \
             clock {} for a commitment whose epoch starts at {FIRST_EPOCH_START} -- the chain \
             refuses exactly this (dig_ecosystem#3444)",
            clawback.recovered_base_units(),
            observed.peak_timestamp()
        ),
        Err(other) => panic!("expected CommitmentEpochStarted, got: {other}"),
    }

    assert_eq!(
        committed.distributor.pending_spend.actions.len(),
        actions_before,
        "the refusal must come before any action is added to the distributor's pending spend"
    );
    assert!(
        ctx.take().is_empty(),
        "the refusal must come before any coin spend is added to the spend context"
    );
}

/// The money-honesty invariant dig_ecosystem#3439 is about: what `commitments()` REPORTED before a
/// withdraw was attempted must agree with what the chain actually did with it.
///
/// - `Some(amount)` reported + accepted: the chain must have paid exactly `amount`.
/// - `None` reported + refused: the refusal must be `AssertBeforeSecondsAbsoluteFailed` -- a
///   correct withheld figure, not a coincidence.
/// - Any other pairing is the false report this whole change exists to prevent, and panics naming
///   which side lied.
fn assert_report_agrees_with_chain(reported: Option<u64>, submit_result: anyhow::Result<u64>) {
    match (reported, submit_result) {
        (Some(amount), Ok(paid)) => assert_eq!(
            amount, paid,
            "commitments() reported {amount} recoverable but the chain paid {paid}"
        ),
        (None, Ok(paid)) => panic!(
            "commitments() reported nothing recoverable, but the chain accepted the withdraw and \
             paid {paid} -- a false negative"
        ),
        (Some(amount), Err(refusal)) => panic!(
            "commitments() reported {amount} recoverable, but the chain refused the withdraw: \
             {refusal:#} -- exactly the false-positive dig_ecosystem#3439 is about"
        ),
        (None, Err(refusal)) => assert_eq!(
            validation_error_code(&refusal),
            ErrorCode::AssertBeforeSecondsAbsoluteFailed,
            "commitments() correctly withheld a figure, and the chain refused, but for the wrong \
             reason: {refusal:#}"
        ),
    }
}

/// Extract the `chia_consensus` validator's [`ErrorCode`] out of a [`submit_withdraw`] refusal, so
/// each assertion below names the exact refusal rather than accepting any error as confirmation --
/// matching `tests/simulator.rs::the_entry_set_write_window_closes_at_the_end_of_an_epoch`'s own
/// convention of matching a precise error shape instead of a loose string check.
fn validation_error_code(error: &anyhow::Error) -> ErrorCode {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<SimulatorError>())
        .and_then(|sim_error| match sim_error {
            SimulatorError::Validation(code) => Some(*code),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a SimulatorError::Validation refusal, got: {error:#}"))
}

/// #3425 -- the question itself. **Measures REFUSED.**
///
/// Commits to [`FIRST_EPOCH_START`], rolls the distributor into that epoch (`NewEpoch`), and only
/// THEN attempts the SAME commitment's withdraw -- the started-epoch case this ticket asks about.
/// Submitted via [`submit_withdraw_bypassing_the_guard`], which shares its whole submit tail with
/// the [`submit_withdraw`] helper [`a_not_yet_started_commitment_is_clawed_back_on_chain`] uses,
/// differing only in the [`roll_into_first_epoch`] call before the withdraw -- the epoch state is
/// the single variable under test. The guarded builder is asserted first to refuse the same
/// withdraw before building it (dig_ecosystem#3444).
///
/// The withdraw builds and signs cleanly and is refused at `Simulator::spend_coins` -- the real
/// `chia_consensus` validator -- with `Validation error: AssertBeforeSecondsAbsoluteFailed`. This
/// is CONFIRMED AT DEPTH, the top rung of the evidence ladder this file's module doc describes,
/// and the two isolation tests below pin the exact mechanism: a wall-clock bound against the
/// commitment's own `epoch_start`, not an epoch-roll state check. See the module doc for how
/// `Commitment::recoverable_base_units()` reports this boundary.
#[test]
fn a_started_epoch_commitment_is_clawed_back_on_chain() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let mut committed = commit_to_first_epoch(ctx, COMMITTED_BASE_UNITS)?;

    let rolled_reward_slot = roll_into_first_epoch(ctx, &mut committed)?;
    assert!(
        current_distributor_epoch_end(&committed.distributor) > FIRST_EPOCH_START,
        "the epoch must actually have rolled forward before the withdraw is attempted"
    );

    let reported = reported_recoverable(&committed, committed.sim.next_timestamp())?;
    assert_eq!(
        reported, None,
        "dig_ecosystem#3439: commitments() must withhold a figure once the epoch has started, \
         since the chain is about to refuse this withdraw"
    );

    let observed = observed_at(&committed, committed.sim.next_timestamp())?;
    assert_guard_refuses_before_building(
        ctx,
        &mut committed,
        rolled_reward_slot.clone(),
        &observed,
    );

    let submit_result =
        submit_withdraw_bypassing_the_guard(ctx, &mut committed, rolled_reward_slot);
    let refusal = match &submit_result {
        Ok(amount) => panic!(
            "a started-epoch commitment's withdraw was accepted on chain and paid {amount} base \
             units -- this ticket's premise (that this might be refused) no longer holds; update \
             this test's expectation and the module doc together"
        ),
        Err(error) => error,
    };

    assert_eq!(
        validation_error_code(refusal),
        ErrorCode::AssertBeforeSecondsAbsoluteFailed,
        "expected the puzzle's own epoch_start bound to refuse this, got: {refusal:#}"
    );
    assert_report_agrees_with_chain(reported, submit_result);

    Ok(())
}

/// Isolates whether the started-epoch refusal above is caused by the epoch actually having rolled
/// (`NewEpoch`), or merely by the simulator's clock having reached `epoch_start` --
/// `roll_into_first_epoch` necessarily does both in one step, so without this test the two are
/// confounded. Commits, then jumps the clock to exactly [`FIRST_EPOCH_START`] WITHOUT ever calling
/// `start_next_distributor_epoch`, and attempts the withdraw against the still-not-rolled
/// commitment and reward slot (`round_time_info.epoch_end` never moves in this test).
///
/// Measures the identical refusal as the started-epoch test, with zero `NewEpoch` involvement --
/// ruling out epoch-roll state as the mechanism. The bound is wall-clock time against the
/// commitment's own `epoch_start`, independent of whether the distributor's own on-chain
/// generation has been rolled into that epoch yet.
#[test]
fn a_wall_clock_reaching_epoch_start_refuses_the_withdraw_even_without_an_epoch_roll(
) -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let mut committed = commit_to_first_epoch(ctx, COMMITTED_BASE_UNITS)?;
    let reward_slot = committed.reward_slot.clone();

    committed.sim.set_next_timestamp(FIRST_EPOCH_START)?;
    assert_eq!(
        current_distributor_epoch_end(&committed.distributor),
        FIRST_EPOCH_START,
        "precondition: no NewEpoch has run, so the distributor's own state must not have rolled"
    );

    let reported = reported_recoverable(&committed, committed.sim.next_timestamp())?;
    assert_eq!(
        reported, None,
        "dig_ecosystem#3439: commitments() must withhold a figure once the chain clock has \
         reached epoch_start, even with no NewEpoch involved"
    );

    let observed = observed_at(&committed, committed.sim.next_timestamp())?;
    assert_guard_refuses_before_building(ctx, &mut committed, reward_slot.clone(), &observed);

    let submit_result = submit_withdraw_bypassing_the_guard(ctx, &mut committed, reward_slot);
    let refusal = match &submit_result {
        Ok(amount) => panic!(
            "a withdraw at the exact epoch_start instant was accepted and paid {amount} base \
             units with no epoch roll involved -- the wall-clock-bound hypothesis is wrong; \
             update this test and the module doc together"
        ),
        Err(error) => error,
    };

    assert_eq!(
        validation_error_code(refusal),
        ErrorCode::AssertBeforeSecondsAbsoluteFailed,
        "expected the identical refusal as the rolled-epoch case, got: {refusal:#}"
    );
    assert_report_agrees_with_chain(reported, submit_result);

    Ok(())
}

/// Pins the exact boundary. Same fixture as
/// [`a_wall_clock_reaching_epoch_start_refuses_the_withdraw_even_without_an_epoch_roll`], but jumps
/// to `FIRST_EPOCH_START - 1` instead of `FIRST_EPOCH_START` exactly, to tell
/// `ASSERT_BEFORE_SECONDS_ABSOLUTE(epoch_start)` (succeeds here) apart from a bound anchored
/// `MAX_SECONDS_OFFSET` seconds earlier, e.g. `epoch_start - 300` (would still refuse here, since
/// `933 < 1233`). Succeeding one second before the failing test above's identical instant proves
/// the bound is `epoch_start` itself, strict-before, at one-second granularity.
#[test]
fn a_withdraw_one_second_before_epoch_start_still_succeeds_pinning_the_exact_boundary(
) -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let mut committed = commit_to_first_epoch(ctx, COMMITTED_BASE_UNITS)?;
    let funder_puzzle_hash = committed.funder.puzzle_hash;
    let asset_id = committed.asset_id;
    let reward_slot = committed.reward_slot.clone();

    committed.sim.set_next_timestamp(FIRST_EPOCH_START - 1)?;

    let hinted_before = committed.sim.hinted_coins(funder_puzzle_hash);

    let reported = reported_recoverable(&committed, committed.sim.next_timestamp())?;
    let observed = observed_at(&committed, committed.sim.next_timestamp())?;

    let submit_result = submit_withdraw(ctx, &mut committed, reward_slot, &observed);
    let driver_reported = match &submit_result {
        Ok(amount) => *amount,
        Err(error) => panic!(
            "one second before epoch_start must still succeed -- if this fails, the boundary \
             moved: {error:#}"
        ),
    };

    let expected = 900_000;
    assert_eq!(
        driver_reported, expected,
        "Clawback::recovered_base_units() reported {driver_reported}, expected {expected} -- \
         pinned to the same figure a prior lane's BUILD-only measurement reported"
    );
    assert_eq!(
        reported,
        Some(expected),
        "commitments() must report the same figure the chain is about to pay"
    );
    assert_report_agrees_with_chain(reported, submit_result);

    let paid = paid_on_chain(&committed.sim, asset_id, funder_puzzle_hash, &hinted_before);
    assert_eq!(
        paid.len(),
        1,
        "the withdraw must create exactly one reward-CAT coin hinted to the funder; found {}: {paid:?}",
        paid.len()
    );
    assert_eq!(
        paid[0].coin.amount, expected,
        "the simulator's own coin record at the funder's puzzle hash paid {} base units, but the \
         expectation was {expected}",
        paid[0].coin.amount
    );

    Ok(())
}

/// dig_ecosystem#3444 -- the build-path defect itself. Before 0.11.0 the guarded builder returned
/// `Ok(Clawback)` recovering 900,000 base units for this commitment, a figure the chain then refused
/// to pay. Rolls into the first distributor epoch, reads at the clock the simulator will validate the
/// next block at, and requires the typed refusal before anything is built.
#[test]
fn withdraw_refuses_a_started_epoch_commitment_before_building() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let mut committed = commit_to_first_epoch(ctx, COMMITTED_BASE_UNITS)?;
    let rolled_reward_slot = roll_into_first_epoch(ctx, &mut committed)?;

    let observed = observed_at(&committed, committed.sim.next_timestamp())?;
    assert!(
        observed.peak_timestamp() >= FIRST_EPOCH_START,
        "precondition: the read must be at or after the commitment's epoch_start"
    );

    assert_guard_refuses_before_building(ctx, &mut committed, rolled_reward_slot, &observed);

    Ok(())
}

/// The guard's boundary is inclusive at `epoch_start`, matching the chain's strict-before
/// `ASSERT_BEFORE_SECONDS_ABSOLUTE(epoch_start)`: an observation exactly at [`FIRST_EPOCH_START`],
/// with no epoch roll, is refused.
#[test]
fn withdraw_refuses_at_exactly_epoch_start() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let mut committed = commit_to_first_epoch(ctx, COMMITTED_BASE_UNITS)?;
    let reward_slot = committed.reward_slot.clone();

    let observed = observed_at(&committed, FIRST_EPOCH_START)?;
    assert_guard_refuses_before_building(ctx, &mut committed, reward_slot, &observed);

    Ok(())
}

/// The other side of the boundary: one second before [`FIRST_EPOCH_START`] the guarded builder
/// builds, the chain accepts the bundle, and it pays exactly what `commitments()` reported -- so
/// the guard refuses nothing the chain would pay.
#[test]
fn withdraw_builds_one_second_before_epoch_start() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let mut committed = commit_to_first_epoch(ctx, COMMITTED_BASE_UNITS)?;
    let reward_slot = committed.reward_slot.clone();
    let one_second_before = FIRST_EPOCH_START - 1;

    committed.sim.set_next_timestamp(one_second_before)?;
    let reported = reported_recoverable(&committed, one_second_before)?;
    let observed = observed_at(&committed, one_second_before)?;
    assert!(
        reported.is_some(),
        "commitments() must report a figure one second before epoch_start"
    );

    let submit_result = submit_withdraw(ctx, &mut committed, reward_slot, &observed);
    assert_report_agrees_with_chain(reported, submit_result);

    Ok(())
}
