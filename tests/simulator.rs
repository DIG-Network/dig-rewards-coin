//! Simulator acceptance tests — `SPEC.md` §15 clauses 9 and 9a.
//!
//! These run the real CHIP-0051 puzzles against `chia-sdk-test`'s simulator. They are not mocks:
//! every assertion below is about what the compiled CLVM actually did.
//!
//! ## The one substitution, and why
//!
//! A DIG distributor's `reserve_asset_id` is `dig_constants::DIG_ASSET_ID` and is deliberately not
//! a runtime parameter (§9.1 clause 3). A simulator cannot mint a CAT with that asset id — an asset
//! id is the hash of its TAIL, so it is fixed by the issuance. These tests therefore build the
//! constants table with the simulator's own asset id and **assert field-by-field that the table is
//! otherwise identical to `dig_distributor_constants`** ([`the_test_table_differs_only_in_the_asset_id`]).
//! That keeps the substitution visible instead of quietly forking the table, and it means no hex
//! literal for the $DIG asset id appears anywhere here.

use chia_protocol::{Bytes32, Coin, SpendBundle};
use chia_puzzle_types::singleton::{SingletonArgs, SingletonSolution};
use chia_puzzle_types::{CoinProof, Memos};
use chia_puzzle_types::{EveProof, LineageProof, Proof};
use chia_puzzles::{SETTLEMENT_PAYMENT_HASH, SINGLETON_LAUNCHER_HASH};
use chia_sdk_driver::{
    sign_standard_transaction, ActionLayer, Cat, CatSpend, HashedPtr, Launcher, Layer, Offer,
    RewardDistributor, RewardDistributorConstants, RewardDistributorState, RewardDistributorType,
    SingleCatSpend, Slot, Spend, SpendContext, SpendWithConditions, StandardLayer,
};
use chia_sdk_test::Simulator;
use chia_sdk_types::puzzles::{
    RawActionLayerSolution, RewardDistributorCommitIncentivesActionSolution,
    RewardDistributorCommitmentSlotValue, RewardDistributorRewardSlotValue,
    RewardDistributorSlotNonce,
};
use chia_sdk_types::{Conditions, TESTNET11_CONSTANTS};
use clvm_traits::{clvm_quote, ToClvm};
use clvmr::NodePtr;
use dig_rewards_coin::clawback::{
    clawback_authority, commitment_distributor_epoch_start, withdraw_committed_incentives,
};
use dig_rewards_coin::comment::LaunchComment;
use dig_rewards_coin::constants::{
    dig_distributor_constants, DistributorLaunchTerms, ENTRY_SHARES, MAX_SECONDS_OFFSET,
    PAYOUT_THRESHOLD_BASE_UNITS, WITHDRAWAL_SHARE_BPS,
};
use dig_rewards_coin::discovery::{discover_distributor, discovered_distributors_in_spend};
use dig_rewards_coin::eligibility::{
    judge_candidate, EligibilityQuestion, EligiblePayoutHash, MirrorCoinFacts,
};
use dig_rewards_coin::entries::{add_entry, remove_entry, ManagerAuthority};
use dig_rewards_coin::epoch::{
    current_distributor_epoch_end, last_update, start_next_distributor_epoch, sync_distributor,
};
use dig_rewards_coin::fund::commit_incentives_for_distributor_epoch;
use dig_rewards_coin::launch::launch_dig_distributor;
use dig_rewards_coin::manager::{launch_manager_singleton, ManagerInnerPuzzle};
use dig_rewards_coin::payout::{initiate_payout, EntrySlotSource, PayoutOutcome};
use dig_rewards_coin::state::MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS;
use dig_rewards_coin::{
    read_distributor, DistributorSnapshot, RewardsError, MAX_REPORTABLE_COMMITMENT_BASE_UNITS,
    STALE_ENTRY_SET_SECONDS,
};

/// The first distributor epoch starts here. Small on purpose: the simulator's clock starts at zero,
/// so a mainnet-shaped timestamp would mean passing decades of simulated time.
const FIRST_EPOCH_START: u64 = 1_234;

/// A short distributor epoch, so a test can let a whole one elapse.
///
/// The DIG value is a week; these tests are about where value goes, not about how long an epoch is,
/// and `epoch_seconds` is a launch-time choice by design (§8.1).
const TEST_EPOCH_SECONDS: u64 = 1_000;

/// $DIG the funder mints for itself before doing anything.
const MINTED_BASE_UNITS: u64 = 10_000_000_000;

/// What the funder commits to one distributor epoch.
const COMMITTED_BASE_UNITS: u64 = 1_000_000;

/// The mirror-collateral epoch these tests judge candidates against. Any ordinal will do; what
/// matters is that the same one is asked and advertised (§4.3 clause 1).
const TEST_MIRROR_COLLATERAL_EPOCH: u32 = 7;

/// A mirror coin that passes every §4.3 check and pays out to one hash.
///
/// `add_entry` accepts only an [`EligiblePayoutHash`], which nothing outside `judge_candidate` can
/// mint, so these tests reach the entry set exactly the way `dig-node` does: judge a candidate
/// against a coin, then add the verdict. There is no test-only back door, because a back door here
/// would test a path production does not have.
struct EligibleMirrorCoin {
    payout_puzzle_hash: Bytes32,
}

impl MirrorCoinFacts for EligibleMirrorCoin {
    fn advertises(&self, _store: Bytes32, _root: Bytes32, mirror_collateral_epoch: u32) -> bool {
        mirror_collateral_epoch == TEST_MIRROR_COLLATERAL_EPOCH
    }

    fn declares_peer(&self, _peer_id: Bytes32) -> bool {
        true
    }

    fn owner_puzzle_hash(&self) -> Bytes32 {
        self.payout_puzzle_hash
    }
}

/// Judge a candidate whose mirror coin pays out to `payout_puzzle_hash`, and take the verdict.
///
/// The eligible outcome is the only one that yields an [`EligiblePayoutHash`], so an
/// `add_entry` call site in these tests is by construction downstream of a real verdict.
fn verdict_for(payout_puzzle_hash: Bytes32) -> EligiblePayoutHash {
    let question = EligibilityQuestion {
        store_launcher_id: Bytes32::new([0xaa; 32]),
        root_hash: Bytes32::new([0xbb; 32]),
        mirror_collateral_epoch: TEST_MIRROR_COLLATERAL_EPOCH,
    };
    let coin = EligibleMirrorCoin { payout_puzzle_hash };

    judge_candidate(question, Bytes32::new([0xcc; 32]), Some(&coin))
        .expect("the epoch is established")
        .expect("every §4.3 check passes")
}

/// A test-only slot source that hands back the slot it was given.
///
/// `initiate_payout` will not accept a slot value, only a source, so a test that wants to claim
/// has to present one. Wrapping a slot here is the test standing in for a chain read.
struct StubSlotSource(Slot<chia_sdk_types::puzzles::RewardDistributorEntrySlotValue>);

impl EntrySlotSource for StubSlotSource {
    fn read_entry_slot(
        &self,
        payout_puzzle_hash: Bytes32,
    ) -> Result<Option<Slot<chia_sdk_types::puzzles::RewardDistributorEntrySlotValue>>, RewardsError>
    {
        if self.0.info.value.payout_puzzle_hash == payout_puzzle_hash {
            Ok(Some(self.0.clone()))
        } else {
            Ok(None)
        }
    }
}

/// A slot source that finds nothing, for the absent-entry outcome.
struct EmptySlotSource;

impl EntrySlotSource for EmptySlotSource {
    fn read_entry_slot(
        &self,
        _payout_puzzle_hash: Bytes32,
    ) -> Result<Option<Slot<chia_sdk_types::puzzles::RewardDistributorEntrySlotValue>>, RewardsError>
    {
        Ok(None)
    }
}

/// A test manager singleton with an inner puzzle of `1`.
///
/// Real DIG managers use a recovery-capable inner puzzle (§15 clause 3a); this is the cheapest
/// singleton that can deliver conditions, and nothing about the assertions below depends on which
/// inner puzzle it is.
struct TestSingleton {
    launcher_id: Bytes32,
    coin: Coin,
    proof: Proof,
    inner_puzzle_hash: Bytes32,
    puzzle: NodePtr,
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
    let (_, coin) = launcher.spend(ctx, inner_puzzle_hash.into(), ())?;

    let puzzle = ctx.curry(SingletonArgs::new(launcher_id, inner_puzzle))?;
    let proof = Proof::Eve(EveProof {
        parent_parent_coin_info: launcher_coin.parent_coin_info,
        parent_amount: launcher_coin.amount,
    });

    Ok(TestSingleton {
        launcher_id,
        coin,
        proof,
        inner_puzzle_hash: inner_puzzle_hash.into(),
        puzzle,
    })
}

/// Deliver `output_conditions` from the manager singleton, recreating it for the next spend.
fn spend_manager_singleton(
    ctx: &mut SpendContext,
    singleton: &TestSingleton,
    output_conditions: Conditions<NodePtr>,
) -> anyhow::Result<(Coin, Proof)> {
    let inner_puzzle = ctx.alloc(&1)?;
    let inner_puzzle_hash: Bytes32 = ctx.tree_hash(inner_puzzle).into();

    let inner_solution = output_conditions
        .create_coin(inner_puzzle_hash, 1, Memos::None)
        .to_clvm(ctx)?;
    let solution = ctx.alloc(&SingletonSolution {
        lineage_proof: singleton.proof,
        amount: 1,
        inner_solution,
    })?;

    ctx.spend(singleton.coin, Spend::new(singleton.puzzle, solution))?;

    let next_proof = Proof::Lineage(LineageProof {
        parent_parent_coin_info: singleton.coin.parent_coin_info,
        parent_inner_puzzle_hash: inner_puzzle_hash,
        parent_amount: singleton.coin.amount,
    });
    let next_coin = Coin::new(singleton.coin.coin_id(), singleton.coin.puzzle_hash, 1);

    Ok((next_coin, next_proof))
}

/// Assert an optional condition set, for the `Sync` an entry-set write may or may not need.
fn ensure_optional_conditions_met(
    ctx: &mut SpendContext,
    sim: &mut Simulator,
    conditions: Option<Conditions<NodePtr>>,
) -> anyhow::Result<()> {
    match conditions {
        Some(conditions) => ensure_conditions_met(ctx, sim, conditions),
        None => Ok(()),
    }
}

/// Make a coin whose whole puzzle is "these conditions must hold", so a permissionless action's
/// conditions get asserted by something.
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

/// The DIG constants table with the simulator's asset id substituted for $DIG's.
///
/// Every other row is read from this crate's own published constants, so a drift in the table shows
/// up in [`the_test_table_differs_only_in_the_asset_id`] rather than silently here.
fn test_constants(
    manager_singleton_launcher_id: Bytes32,
    funder_refund_puzzle_hash: Bytes32,
    simulator_asset_id: Bytes32,
) -> RewardDistributorConstants {
    test_constants_with_bps(
        manager_singleton_launcher_id,
        funder_refund_puzzle_hash,
        simulator_asset_id,
        WITHDRAWAL_SHARE_BPS,
    )
}

/// As [`test_constants`], but with `withdrawal_share_bps` supplied by the caller.
///
/// `RewardDistributorConstants` takes a raw `u64` there, and nothing upstream or in
/// `launch_dig_distributor` narrows it -- which is exactly why `read_distributor` has to reject an
/// out-of-domain value itself (SPEC.md §0.1 clause 5d). This exists so a test can launch the
/// hostile distributor an attacker can launch, rather than assert about one it cannot.
fn test_constants_with_bps(
    manager_singleton_launcher_id: Bytes32,
    funder_refund_puzzle_hash: Bytes32,
    simulator_asset_id: Bytes32,
    withdrawal_share_bps: u64,
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
        withdrawal_share_bps,
        simulator_asset_id,
    )
}

/// As [`test_constants_with_bps`], but overrides `epoch_seconds` instead of the withdrawal share
/// -- builds the fixture `RewardsError::UnreadableEpochSeconds`'s domain check needs a distributor
/// to actually carry. `dig_distributor_constants` refuses a zero epoch length at launch, so the
/// only way this reaches the reader at all is a hostile launcher assembling the table directly,
/// which is exactly the threat model `read_distributor` is written for.
fn test_constants_with_epoch_seconds(
    manager_singleton_launcher_id: Bytes32,
    funder_refund_puzzle_hash: Bytes32,
    simulator_asset_id: Bytes32,
    epoch_seconds: u64,
) -> RewardDistributorConstants {
    RewardDistributorConstants::without_launcher_id(
        RewardDistributorType::Managed {
            manager_singleton_launcher_id,
        },
        funder_refund_puzzle_hash,
        epoch_seconds,
        u64::MAX,
        MAX_SECONDS_OFFSET,
        PAYOUT_THRESHOLD_BASE_UNITS,
        false,
        0,
        0,
        simulator_asset_id,
    )
}

/// Everything a launched test distributor needs to keep being driven.
struct Harness {
    sim: Simulator,
    distributor: RewardDistributor,
    first_epoch_slot: Slot<RewardDistributorRewardSlotValue>,
    source_cat: Cat,
    funder: chia_sdk_test::BlsPairWithCoin,
    manager: TestSingleton,
    entry: chia_sdk_test::BlsPairWithCoin,

    /// The commitment slot the most recent [`commit_to_epoch`] created.
    ///
    /// Captured by the helper because it can only be derived from the distributor coin being
    /// spent: re-deriving it later would name a slot coin that never existed.
    last_commitment_slot: Option<Slot<RewardDistributorCommitmentSlotValue>>,
}

/// Mint $DIG, launch a manager singleton, build the launch offer, and launch the distributor.
fn launch_harness(ctx: &mut SpendContext) -> anyhow::Result<Harness> {
    launch_harness_with(ctx, MINTED_BASE_UNITS, WITHDRAWAL_SHARE_BPS)
}

/// As [`launch_harness`], but mints `minted_base_units` of the reward CAT and curries
/// `withdrawal_share_bps` into the distributor's constants.
///
/// Both are parameters rather than constants because the #3286 guards are about scale and domain:
/// a fixture that can only mint `MINTED_BASE_UNITS` at `WITHDRAWAL_SHARE_BPS` cannot reach either
/// bound, and a guard no fixture can reach is a claim rather than a proof.
fn launch_harness_with(
    ctx: &mut SpendContext,
    minted_base_units: u64,
    withdrawal_share_bps: u64,
) -> anyhow::Result<Harness> {
    launch_harness_with_constants_builder(ctx, minted_base_units, |manager, funder, asset_id| {
        test_constants_with_bps(manager, funder, asset_id, withdrawal_share_bps)
    })
}

/// As [`launch_harness_with`], but overrides `epoch_seconds` -- the launch constant
/// `RewardsError::UnreadableEpochSeconds` domain-checks, and the one whose zero value makes
/// upstream's backfill loop non-terminating.
fn launch_harness_with_epoch_seconds(
    ctx: &mut SpendContext,
    minted_base_units: u64,
    epoch_seconds: u64,
) -> anyhow::Result<Harness> {
    launch_harness_with_constants_builder(ctx, minted_base_units, |manager, funder, asset_id| {
        test_constants_with_epoch_seconds(manager, funder, asset_id, epoch_seconds)
    })
}

/// Shared body behind [`launch_harness_with`] and [`launch_harness_with_epoch_seconds`]:
/// mint the reward CAT, launch a manager singleton, build the launch offer, then hand the
/// manager/funder/asset identities `launch_dig_distributor` needs to whatever constants table the
/// caller's closure builds from them, and launch.
fn launch_harness_with_constants_builder(
    ctx: &mut SpendContext,
    minted_base_units: u64,
    build_constants: impl FnOnce(Bytes32, Bytes32, Bytes32) -> RewardDistributorConstants,
) -> anyhow::Result<Harness> {
    let mut sim = Simulator::new();

    // Mint the reward CAT.
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

    // Distinct keys, explicitly: the entry's payout key must not be the funder's, or a one-owner
    // fixture would silently model two owners.
    let entry = sim.bls(0);
    assert_ne!(
        entry.puzzle_hash, funder.puzzle_hash,
        "the entry and the funder must be different owners"
    );

    // Build the launch offer: one XCH mojo plus the whole reward CAT, both to the settlement puzzle.
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

    let constants = build_constants(
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
        // The simulator's clock starts at zero, so FIRST_EPOCH_START is in the future.
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

    // The change CAT went to the hash the constants table carries, which is the only hash the
    // funder supplied: SPEC.md §15 clause 3 by construction, asserted on real launch output.
    assert_eq!(
        launched.refund_cat.info.p2_puzzle_hash,
        constants.fee_payout_puzzle_hash
    );

    // The launch curries the constants table and nothing else: the manager singleton and
    // `epoch_seconds` on the launched distributor come from `constants`, which is the single place
    // a caller can set them. There is no second copy for them to disagree with.
    assert_eq!(
        launched.distributor.info.constants.reward_distributor_type,
        RewardDistributorType::Managed {
            manager_singleton_launcher_id: manager.launcher_id,
        },
        "the manager singleton is curried from the constants table"
    );
    assert_eq!(
        launched.distributor.info.constants.epoch_seconds, constants.epoch_seconds,
        "epoch_seconds is curried from the constants table the caller's builder produced -- \
         compared against THAT table, not against TEST_EPOCH_SECONDS, so a fixture that \
         deliberately launches a hostile epoch length is not refused by its own harness"
    );

    Ok(Harness {
        sim,
        distributor: launched.distributor,
        first_epoch_slot: launched.first_distributor_epoch_slot,
        source_cat: launched.refund_cat,
        funder,
        manager,
        entry,
        last_commitment_slot: None,
    })
}

/// Commit `COMMITTED_BASE_UNITS` to the distributor epoch starting at `epoch_start`.
fn commit_to_epoch(
    ctx: &mut SpendContext,
    harness: &mut Harness,
    reward_slot: Slot<RewardDistributorRewardSlotValue>,
    epoch_start: u64,
    amount_base_units: u64,
) -> anyhow::Result<Vec<Slot<RewardDistributorRewardSlotValue>>> {
    let secure_conditions = commit_incentives_for_distributor_epoch(
        ctx,
        &mut harness.distributor,
        reward_slot,
        epoch_start,
        harness.funder.puzzle_hash,
        amount_base_units,
    )?;

    let hint = ctx.hint(harness.funder.puzzle_hash)?;
    let change = harness.source_cat.coin.amount - amount_base_units;
    let source_cat_spend = CatSpend::new(
        harness.source_cat,
        StandardLayer::new(harness.funder.pk).spend_with_conditions(
            ctx,
            secure_conditions.create_coin(harness.funder.puzzle_hash, change, hint),
        )?,
    );

    let reward_slots = harness
        .distributor
        .pending_spend
        .created_reward_slots
        .iter()
        .map(|value| {
            harness
                .distributor
                .created_slot_value_to_slot(*value, RewardDistributorSlotNonce::REWARD)
        })
        .collect::<Vec<_>>();

    harness.last_commitment_slot = harness
        .distributor
        .pending_spend
        .created_commitment_slots
        .first()
        .copied()
        .map(|value| {
            harness
                .distributor
                .created_slot_value_to_slot(value, RewardDistributorSlotNonce::COMMITMENT)
        });

    harness.distributor = harness
        .distributor
        .clone()
        .finish_spend(ctx, vec![source_cat_spend])?
        .0;
    harness
        .sim
        .spend_coins(ctx.take(), std::slice::from_ref(&harness.funder.sk))?;
    harness.source_cat = harness.source_cat.child(harness.funder.puzzle_hash, change);

    Ok(reward_slots)
}

#[test]
fn the_test_table_differs_only_in_the_asset_id() {
    let manager = Bytes32::new([7; 32]);
    let refund = Bytes32::new([9; 32]);
    let simulated_asset = Bytes32::new([3; 32]);

    let terms = DistributorLaunchTerms {
        manager_singleton_launcher_id: manager,
        distributor_epoch_seconds: TEST_EPOCH_SECONDS,
    };
    let dig = dig_distributor_constants(terms, refund).expect("valid terms");
    let simulated = test_constants(manager, refund, simulated_asset);

    assert_eq!(
        simulated.reward_distributor_type,
        dig.reward_distributor_type
    );
    assert_eq!(simulated.fee_payout_puzzle_hash, dig.fee_payout_puzzle_hash);
    assert_eq!(simulated.epoch_seconds, dig.epoch_seconds);
    assert_eq!(simulated.precision, dig.precision);
    assert_eq!(simulated.max_seconds_offset, dig.max_seconds_offset);
    assert_eq!(simulated.payout_threshold, dig.payout_threshold);
    assert_eq!(
        simulated.require_payout_approval,
        dig.require_payout_approval
    );
    assert_eq!(simulated.fee_bps, dig.fee_bps);
    assert_eq!(simulated.withdrawal_share_bps, dig.withdrawal_share_bps);

    assert_ne!(
        simulated.reserve_asset_id, dig.reserve_asset_id,
        "the substitution is the only difference, and it is real"
    );
}

#[test]
fn managed_dig_distributor_end_to_end() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let mut harness = launch_harness(ctx)?;

    assert_eq!(harness.distributor.info.state.total_reserves, 0);
    assert!(!harness.distributor.info.constants.require_payout_approval);

    // Fund the first distributor epoch with a revocable commitment.
    let first_epoch_slot = harness.first_epoch_slot.clone();
    let reward_slots = commit_to_epoch(
        ctx,
        &mut harness,
        first_epoch_slot,
        FIRST_EPOCH_START,
        COMMITTED_BASE_UNITS,
    )?;
    assert_eq!(
        harness.distributor.info.state.total_reserves, COMMITTED_BASE_UNITS,
        "the commitment landed in the reserve"
    );

    // Add the entry while the clock is still inside the launch's validity window, so the builder
    // correctly decides that no Sync is needed: a Sync must move `last_update` strictly forward,
    // and before the first epoch has started it cannot.
    let authority = ManagerAuthority::new(harness.manager.inner_puzzle_hash)?;
    let write = add_entry(
        ctx,
        &mut harness.distributor,
        authority,
        verdict_for(harness.entry.puzzle_hash),
        0,
    )?;

    assert!(
        write.sync_conditions.is_none(),
        "inside the window, no Sync is emitted"
    );

    let entry_slot_value = harness.distributor.pending_spend.created_entry_slots[0];
    let entry_slot = harness
        .distributor
        .created_slot_value_to_slot(entry_slot_value, RewardDistributorSlotNonce::ENTRY);

    assert_eq!(
        entry_slot.info.value.payout_puzzle_hash, harness.entry.puzzle_hash,
        "the entry is keyed by a payout puzzle hash"
    );
    assert_eq!(entry_slot.info.value.shares, ENTRY_SHARES);
    assert_eq!(entry_slot.info.value.counter, 0);

    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    ensure_optional_conditions_met(ctx, &mut harness.sim, write.sync_conditions)?;
    let (next_manager_coin, next_manager_proof) =
        spend_manager_singleton(ctx, &harness.manager, write.manager_conditions)?;
    harness.sim.spend_coins(ctx.take(), &[])?;
    harness.manager.coin = next_manager_coin;
    harness.manager.proof = next_manager_proof;

    assert_eq!(harness.distributor.info.state.active_shares, ENTRY_SHARES);

    // Roll into the first epoch, which is what makes the commitment start accruing.
    harness.sim.set_next_timestamp(FIRST_EPOCH_START)?;
    let first_reward_slot = reward_slots
        .iter()
        .find(|slot| slot.info.value.epoch_start == FIRST_EPOCH_START)
        .expect("a reward slot for the first epoch")
        .clone();
    let roll = start_next_distributor_epoch(ctx, &mut harness.distributor, first_reward_slot)?;
    assert_eq!(roll.fee_base_units, 0, "SPEC.md §7.3: fee_bps is zero");

    ensure_conditions_met(ctx, &mut harness.sim, roll.conditions)?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    harness.sim.spend_coins(ctx.take(), &[])?;

    // Let most of the epoch elapse, then sync so the accrual is visible.
    let sync_time = FIRST_EPOCH_START + TEST_EPOCH_SECONDS / 2;
    harness.sim.set_next_timestamp(sync_time)?;
    let sync_conditions = sync_distributor(ctx, &mut harness.distributor, sync_time)?;
    ensure_conditions_met(ctx, &mut harness.sim, sync_conditions)?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    harness.sim.spend_coins(ctx.take(), &[])?;

    assert!(
        harness
            .distributor
            .info
            .state
            .round_reward_info
            .cumulative_payout
            > 0,
        "half an epoch with one entry must have accrued something"
    );

    // The entry claims for itself, permissionlessly. The slot object must be the one the add
    // created: `created_slot_value_to_slot` derives the slot coin from the distributor coin being
    // spent, so re-deriving it later would name a coin that never existed.
    let source = StubSlotSource(entry_slot.clone());
    let outcome = initiate_payout(
        ctx,
        &mut harness.distributor,
        &source,
        harness.entry.puzzle_hash,
    )?;

    let PayoutOutcome::Paid {
        conditions,
        amount_base_units,
        counter,
    } = outcome
    else {
        panic!("the entry is in the set, so the claim must build");
    };

    assert!(
        amount_base_units >= PAYOUT_THRESHOLD_BASE_UNITS,
        "a claim below the threshold would not be payable: {amount_base_units}"
    );
    assert_eq!(counter, 0, "the slot's replay guard, as read");

    // The claim spent the entry slot and created its replacement, with `counter` advanced. That
    // replacement is what a later write must reference.
    let paid_entry_slot = harness.distributor.created_slot_value_to_slot(
        harness.distributor.pending_spend.created_entry_slots[0],
        RewardDistributorSlotNonce::ENTRY,
    );
    assert_eq!(
        paid_entry_slot.info.value.counter,
        counter + 1,
        "InitiatePayout advances the replay guard"
    );

    ensure_conditions_met(ctx, &mut harness.sim, conditions)?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    harness.sim.spend_coins(ctx.take(), &[])?;

    // An absent entry is a terminal non-error, not a failure.
    assert!(matches!(
        initiate_payout(
            ctx,
            &mut harness.distributor,
            &EmptySlotSource,
            harness.entry.puzzle_hash
        )?,
        PayoutOutcome::EntrySlotAbsent
    ));

    // Remove the entry, which settles whatever it had accrued since the claim.
    let now = harness.distributor.info.state.round_time_info.last_update;
    let removal = remove_entry(
        ctx,
        &mut harness.distributor,
        authority,
        paid_entry_slot,
        now,
    )?;

    // §6.4: the removal settles the last payment, with the threshold NOT applied, and this crate
    // surfaces the figure rather than discarding it.
    let settled = removal.settled_base_units;
    ensure_optional_conditions_met(ctx, &mut harness.sim, removal.write.sync_conditions)?;
    let (_, _) = spend_manager_singleton(ctx, &harness.manager, removal.write.manager_conditions)?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    harness.sim.spend_coins(ctx.take(), &[])?;

    assert_eq!(
        harness.distributor.info.state.active_shares, 0,
        "the entry set is empty again"
    );
    println!("§6.4 settlement amount on removal: {settled} base units");

    Ok(())
}

#[test]
fn empty_first_epoch_settles_where() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let mut harness = launch_harness(ctx)?;

    // Two DIFFERENT amounts, so the claim below can only match one arithmetic. If the empty
    // epoch's value carries forward, a half-epoch claim in the second epoch is worth about half of
    // both commitments; if it is stranded, about half of the second alone.
    let first_epoch_commitment = COMMITTED_BASE_UNITS;
    let second_epoch_commitment = COMMITTED_BASE_UNITS * 2 / 5;
    let second_epoch_start = FIRST_EPOCH_START + TEST_EPOCH_SECONDS;

    let first_epoch_slot = harness.first_epoch_slot.clone();
    let mut reward_slots = commit_to_epoch(
        ctx,
        &mut harness,
        first_epoch_slot,
        FIRST_EPOCH_START,
        first_epoch_commitment,
    )?;

    // Fund the second epoch too, from whichever reward slot covers it.
    let slot_for_second = pick_reward_slot(&reward_slots, second_epoch_start);
    reward_slots = merge_reward_slots(
        reward_slots,
        commit_to_epoch(
            ctx,
            &mut harness,
            slot_for_second,
            second_epoch_start,
            second_epoch_commitment,
        )?,
    );

    assert_eq!(
        harness.distributor.info.state.total_reserves,
        first_epoch_commitment + second_epoch_commitment
    );

    // Roll into the first epoch with ZERO entries.
    harness.sim.set_next_timestamp(FIRST_EPOCH_START)?;
    let first_reward_slot = reward_slots
        .iter()
        .find(|slot| slot.info.value.epoch_start == FIRST_EPOCH_START)
        .expect("a reward slot for the first epoch")
        .clone();
    let roll = start_next_distributor_epoch(ctx, &mut harness.distributor, first_reward_slot)?;
    ensure_conditions_met(ctx, &mut harness.sim, roll.conditions)?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    harness.sim.spend_coins(ctx.take(), &[])?;

    assert_eq!(
        harness.distributor.info.state.active_shares, 0,
        "the first epoch runs with nobody in the set"
    );

    // Let the whole empty epoch elapse, syncing to its end.
    let epoch_end = harness.distributor.info.state.round_time_info.epoch_end;
    harness.sim.set_next_timestamp(epoch_end)?;
    let sync_conditions = sync_distributor(ctx, &mut harness.distributor, epoch_end)?;
    ensure_conditions_met(ctx, &mut harness.sim, sync_conditions)?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    harness.sim.spend_coins(ctx.take(), &[])?;

    let after_empty = harness.distributor.info.state.round_reward_info;
    println!(
        "after an EMPTY distributor epoch: cumulative_payout={} remaining_rewards={} \
         total_reserves={}",
        after_empty.cumulative_payout,
        after_empty.remaining_rewards,
        harness.distributor.info.state.total_reserves
    );

    assert_eq!(
        after_empty.cumulative_payout, 0,
        "with nobody in the set, nothing accrued to anybody"
    );
    assert_eq!(
        harness.distributor.info.state.total_reserves,
        first_epoch_commitment + second_epoch_commitment,
        "the reserve is untouched: an empty epoch pays nothing out"
    );

    // Now add an entry. `last_update` has just reached `epoch_end`, so the window is still open and
    // no Sync is emitted.
    let authority = ManagerAuthority::new(harness.manager.inner_puzzle_hash)?;
    let write = add_entry(
        ctx,
        &mut harness.distributor,
        authority,
        verdict_for(harness.entry.puzzle_hash),
        epoch_end,
    )?;

    // Asserted, not assumed. `ensure_optional_conditions_met` below has a silent `None` arm, so
    // without this line either decision of `sync_if_the_window_needs_it` would satisfy the test
    // and the window logic would be untested here.
    assert!(
        write.sync_conditions.is_none(),
        "at last_update == epoch_end the window is still open, so no Sync is emitted"
    );

    let entry_slot = harness.distributor.created_slot_value_to_slot(
        harness.distributor.pending_spend.created_entry_slots[0],
        RewardDistributorSlotNonce::ENTRY,
    );
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    ensure_optional_conditions_met(ctx, &mut harness.sim, write.sync_conditions)?;
    let (next_manager_coin, next_manager_proof) =
        spend_manager_singleton(ctx, &harness.manager, write.manager_conditions)?;
    harness.sim.spend_coins(ctx.take(), &[])?;
    harness.manager.coin = next_manager_coin;
    harness.manager.proof = next_manager_proof;

    // Roll into the second epoch, which is the moment that decides whether the empty epoch's value
    // is still reachable.
    let second_reward_slot = reward_slots
        .iter()
        .find(|slot| slot.info.value.epoch_start == second_epoch_start)
        .expect("a reward slot for the second epoch")
        .clone();
    let roll = start_next_distributor_epoch(ctx, &mut harness.distributor, second_reward_slot)?;
    ensure_conditions_met(ctx, &mut harness.sim, roll.conditions)?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    harness.sim.spend_coins(ctx.take(), &[])?;

    // ---- The second epoch's roll is proven to have HAPPENED, by its post-state ----
    //
    // Everything below this point is the answer to §15 clause 9a, so a silently skipped roll must
    // not be able to reach it. These two assertions are the guard, and they are deliberately the
    // FIRST thing after the roll: without them a missing roll surfaces much later as a bare
    // `clvm raise` out of the next `sync_distributor` — a red, but one that says nothing about
    // where the money went, and one that a future edit to the tail of this test could mask.
    //
    // `epoch_end` is the load-bearing witness: only `NewEpoch` moves it, and it moves to a value
    // no earlier state of this distributor ever holds.
    let rolled = harness.distributor.info.state;
    assert_eq!(
        rolled.round_time_info.epoch_end,
        second_epoch_start + TEST_EPOCH_SECONDS,
        "the distributor is in the SECOND epoch — only NewEpoch moves epoch_end, so this \
         failing means the roll never happened and the claim below would prove nothing"
    );
    assert_eq!(roll.fee_base_units, 0, "SPEC.md §7.3: fee_bps is zero");

    // And this is §15 clause 9a settled at the STATE level, independently of the claim
    // arithmetic below: `remaining_rewards` is precision-scaled, and after the roll it holds
    // precision × BOTH commitments. `NewEpoch` added the second epoch's commitment ON TOP OF the
    // empty epoch's untouched balance rather than replacing or discarding it — which is the whole
    // question, and the reason two independent measurements of it are better than one.
    let precision = u128::from(harness.distributor.info.constants.precision);
    assert_eq!(
        rolled.round_reward_info.remaining_rewards,
        precision * u128::from(first_epoch_commitment + second_epoch_commitment),
        "the empty epoch's value is still in the distributable pool, with the second epoch's \
         commitment added on top of it"
    );
    assert_eq!(
        rolled.round_reward_info.cumulative_payout, 0,
        "still nothing accrued to anybody: the second epoch has only just started"
    );

    // Half of the second epoch elapses, then the entry claims.
    let mid_second = second_epoch_start + TEST_EPOCH_SECONDS / 2;
    harness.sim.set_next_timestamp(mid_second)?;
    let sync_conditions = sync_distributor(ctx, &mut harness.distributor, mid_second)?;
    ensure_conditions_met(ctx, &mut harness.sim, sync_conditions)?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    harness.sim.spend_coins(ctx.take(), &[])?;

    let outcome = initiate_payout(
        ctx,
        &mut harness.distributor,
        &StubSlotSource(entry_slot),
        harness.entry.puzzle_hash,
    )?;
    let PayoutOutcome::Paid {
        conditions,
        amount_base_units,
        ..
    } = outcome
    else {
        panic!("the entry was added, so the slot must be present");
    };
    ensure_conditions_met(ctx, &mut harness.sim, conditions)?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    harness.sim.spend_coins(ctx.take(), &[])?;
    let reserve_after_claim = harness.distributor.info.state.total_reserves;

    println!(
        "the sole entry, added AFTER the empty epoch, claimed {amount_base_units} base units at \
         the halfway point of the second epoch, leaving {reserve_after_claim} in the reserve. \
         first_epoch_commitment={first_epoch_commitment} second_epoch_commitment={second_epoch_commitment}"
    );

    // SPEC.md §15 clause 9a's open money question, settled by measurement rather than inference:
    // the value committed to an epoch that ran with nobody in the set is NOT stranded. It stays in
    // `remaining_rewards` untouched -- an empty epoch distributes nothing and pays nothing out --
    // and `NewEpoch` adds the next epoch's commitment on top of it, so the whole balance accrues
    // to whoever is in the set once somebody is. A funder who launches with a `first_epoch_start`
    // before its prover is ready loses nothing; the money simply waits.
    //
    // Clause 9a bans an approximate assertion, a value the test recomputes from the same
    // commitments the puzzle's own arithmetic consumes, and a bare `>` inequality standing in for
    // a value -- so (a) and (b) below are EXACT base-unit LITERALS, measured once by running this
    // harness (`first_epoch_commitment = 1_000_000`, `second_epoch_commitment = 400_000`,
    // `TEST_EPOCH_SECONDS = 1_000`) and printed above, not derived here from those commitments. A
    // literal can only match one arithmetic: if the empty epoch's value were stranded, `(a)` would
    // read `200_000` and this assertion would fail; if it carried forward wrongly (e.g. double
    // counted), `(a)` would read something other than `700_000` and still fail. Only the actual,
    // correct carry-forward arithmetic passes.
    assert_eq!(
        amount_base_units, 700_000,
        "(a) SPEC.md §15 clause 9a: the entry's claim at the second epoch's halfway point must \
         pay out the empty epoch's value carried forward (700_000 = half of both commitments), \
         never the stranded reading (200_000 = half of the second commitment alone): \
         got {amount_base_units}"
    );

    // (b) the reserve's remaining base units after that claim -- clause 9a requires this figure
    // as a literal too, independently of (a).
    assert_eq!(
        reserve_after_claim, 700_000,
        "(b) SPEC.md §15 clause 9a: the reserve must hold exactly the funded total minus what was \
         just claimed (1_400_000 funded - 700_000 claimed = 700_000): got {reserve_after_claim}"
    );

    // (c) conservation identity: the claim, the remaining reserve, and every base unit already
    // paid out or skimmed as fee must together equal every base unit funded. Nothing had been
    // fee'd or paid out anywhere earlier in this test (both rolls asserted `fee_base_units == 0`
    // above, SPEC.md §7.3, and this is the entry's first claim), so that third term is 0 here --
    // it is named rather than omitted so a future fee or an earlier payout in this test would be
    // caught by the identity instead of silently passing under it.
    let already_paid_out_or_skimmed_as_fee: u64 = 0;
    let funded_total = first_epoch_commitment + second_epoch_commitment;
    assert_eq!(
        funded_total, 1_400_000,
        "the harness commits 1_000_000 + 400_000 = 1_400_000 base units across the two epochs"
    );
    assert_eq!(
        amount_base_units + reserve_after_claim + already_paid_out_or_skimmed_as_fee,
        funded_total,
        "(c) SPEC.md §15 clause 9a conservation: claimed + reserve remainder + already paid out \
         or skimmed as fee must equal every base unit funded"
    );

    Ok(())
}

/// Launch, fund the first two distributor epochs, and roll into the first one.
///
/// The two window tests below both need a distributor that is *inside* a running epoch, with the
/// next epoch already funded so that `NewEpoch` — the remedy
/// [`RewardsError::EntrySetWriteWindowClosed`] names — has a reward slot to consume.
fn launch_funded_and_inside_the_first_epoch(
    ctx: &mut SpendContext,
) -> anyhow::Result<(Harness, Vec<Slot<RewardDistributorRewardSlotValue>>)> {
    let mut harness = launch_harness(ctx)?;
    let second_epoch_start = FIRST_EPOCH_START + TEST_EPOCH_SECONDS;

    let first_epoch_slot = harness.first_epoch_slot.clone();
    let mut reward_slots = commit_to_epoch(
        ctx,
        &mut harness,
        first_epoch_slot,
        FIRST_EPOCH_START,
        COMMITTED_BASE_UNITS,
    )?;
    let slot_for_second = pick_reward_slot(&reward_slots, second_epoch_start);
    reward_slots = merge_reward_slots(
        reward_slots,
        commit_to_epoch(
            ctx,
            &mut harness,
            slot_for_second,
            second_epoch_start,
            COMMITTED_BASE_UNITS,
        )?,
    );

    harness.sim.set_next_timestamp(FIRST_EPOCH_START)?;
    let first_reward_slot = pick_reward_slot(&reward_slots, FIRST_EPOCH_START);
    let roll = start_next_distributor_epoch(ctx, &mut harness.distributor, first_reward_slot)?;
    ensure_conditions_met(ctx, &mut harness.sim, roll.conditions)?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    harness.sim.spend_coins(ctx.take(), &[])?;

    assert_eq!(
        harness.distributor.info.state.round_time_info.last_update, FIRST_EPOCH_START,
        "the roll set last_update to the epoch it started"
    );
    assert_eq!(
        last_update(&harness.distributor),
        FIRST_EPOCH_START,
        "the public accessor agrees with the state field it reads"
    );
    assert_eq!(
        current_distributor_epoch_end(&harness.distributor),
        FIRST_EPOCH_START + TEST_EPOCH_SECONDS,
        "and the epoch runs one epoch_seconds from its start"
    );

    Ok((harness, reward_slots))
}

/// `SPEC.md` §8.2 clause 1, past the window: the `Sync` rides in the **same bundle**, and the
/// chain accepts the pair.
///
/// This is the `sync_conditions == Some` branch of the decision `crate::entries` computes. Its
/// counterpart — the `None` branch, inside the window — is asserted in
/// [`managed_dig_distributor_end_to_end`] and [`empty_first_epoch_settles_where`].
#[test]
fn past_the_window_an_entry_set_write_carries_a_sync_in_the_same_bundle() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let (mut harness, _reward_slots) = launch_funded_and_inside_the_first_epoch(ctx)?;

    // Move well past `last_update + max_seconds_offset`, but stay inside the epoch so a Sync is
    // still able to move the clock forward.
    let past_the_window = FIRST_EPOCH_START + MAX_SECONDS_OFFSET + 100;
    assert!(
        past_the_window < harness.distributor.info.state.round_time_info.epoch_end,
        "the fixture must sit past the window but inside the epoch, or it tests the wrong branch"
    );
    harness.sim.set_next_timestamp(past_the_window)?;

    let authority = ManagerAuthority::new(harness.manager.inner_puzzle_hash)?;
    let write = add_entry(
        ctx,
        &mut harness.distributor,
        authority,
        verdict_for(harness.entry.puzzle_hash),
        past_the_window,
    )?;

    let sync_conditions = write
        .sync_conditions
        .clone()
        .expect("past the window, a Sync must ride along or the write is invalid on chain");

    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    ensure_conditions_met(ctx, &mut harness.sim, sync_conditions)?;
    let (_, _) = spend_manager_singleton(ctx, &harness.manager, write.manager_conditions)?;

    // The chain accepting this is the actual claim: the bundled Sync is what keeps the
    // `ASSERT_BEFORE_SECONDS_ABSOLUTE` the write carries satisfiable.
    harness.sim.spend_coins(ctx.take(), &[])?;

    assert_eq!(
        harness.distributor.info.state.round_time_info.last_update, past_the_window,
        "the bundled Sync moved the distributor's clock forward"
    );
    assert_eq!(
        harness.distributor.info.state.active_shares, ENTRY_SHARES,
        "and the entry-set write it protected landed"
    );

    Ok(())
}

/// `SPEC.md` §8.2 clause 1, when no `Sync` can help: the write is refused by name, and the remedy
/// the error names actually works.
///
/// Once `last_update` has reached `epoch_end` a `Sync` cannot move the clock at all — it may only
/// move strictly forward, and never past the epoch's end. So the write is impossible until someone
/// rolls the epoch. The second half of this test spends that remedy and re-attempts, because an
/// error message naming a remedy nobody has executed is a claim, not a fact.
#[test]
fn the_entry_set_write_window_closes_at_the_end_of_an_epoch() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let (mut harness, reward_slots) = launch_funded_and_inside_the_first_epoch(ctx)?;

    // Run the clock to the epoch's end, so `last_update == epoch_end`.
    let epoch_end = harness.distributor.info.state.round_time_info.epoch_end;
    harness.sim.set_next_timestamp(epoch_end)?;
    let sync_conditions = sync_distributor(ctx, &mut harness.distributor, epoch_end)?;
    ensure_conditions_met(ctx, &mut harness.sim, sync_conditions)?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    harness.sim.spend_coins(ctx.take(), &[])?;

    assert_eq!(
        harness.distributor.info.state.round_time_info.last_update, epoch_end,
        "the clock has reached the epoch's end, which is as far as a Sync may take it"
    );

    // Past the window now, with no Sync able to fix it.
    let too_late = epoch_end + MAX_SECONDS_OFFSET;
    let authority = ManagerAuthority::new(harness.manager.inner_puzzle_hash)?;
    // Matched rather than `expect_err`: `EntrySetWrite` carries `Conditions` and is not `Debug`,
    // and a refusal test should not be the reason a public type grows a derive.
    let refusal = match add_entry(
        ctx,
        &mut harness.distributor,
        authority,
        verdict_for(harness.entry.puzzle_hash),
        too_late,
    ) {
        Ok(_) => panic!("no Sync can bring this write inside its window, so it must be refused"),
        Err(error) => error,
    };

    let RewardsError::EntrySetWriteWindowClosed {
        last_update,
        epoch_end: refused_epoch_end,
        now_unix_seconds,
    } = &refusal
    else {
        panic!("expected EntrySetWriteWindowClosed, got: {refusal}");
    };
    assert_eq!(*last_update, epoch_end);
    assert_eq!(*refused_epoch_end, epoch_end);
    assert_eq!(*now_unix_seconds, too_late);
    assert!(
        refusal
            .to_string()
            .contains("roll the distributor epoch first"),
        "the refusal must name its remedy: {refusal}"
    );

    // The named remedy, spent. `NewEpoch` is permissionless, so the caller can do this itself.
    let second_epoch_start = FIRST_EPOCH_START + TEST_EPOCH_SECONDS;
    let second_reward_slot = reward_slots
        .iter()
        .find(|slot| slot.info.value.epoch_start == second_epoch_start)
        .expect("a reward slot for the second epoch")
        .clone();
    let roll = start_next_distributor_epoch(ctx, &mut harness.distributor, second_reward_slot)?;
    ensure_conditions_met(ctx, &mut harness.sim, roll.conditions)?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    harness.sim.spend_coins(ctx.take(), &[])?;

    // And the very same write now builds, which is what makes the remedy a fact rather than a
    // sentence in an error message. The roll set `last_update` to the new epoch's start, so this
    // write is once again past its window — a Sync rides along, and the chain has to actually be
    // at that time for the Sync's own `ASSERT_SECONDS_ABSOLUTE` to hold.
    harness.sim.set_next_timestamp(too_late)?;
    let write = add_entry(
        ctx,
        &mut harness.distributor,
        authority,
        verdict_for(harness.entry.puzzle_hash),
        too_late,
    )?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    ensure_optional_conditions_met(ctx, &mut harness.sim, write.sync_conditions)?;
    let (_, _) = spend_manager_singleton(ctx, &harness.manager, write.manager_conditions)?;
    harness.sim.spend_coins(ctx.take(), &[])?;

    assert_eq!(
        harness.distributor.info.state.active_shares, ENTRY_SHARES,
        "after the roll the refused write lands"
    );

    Ok(())
}

/// `SPEC.md` §8.2, the fourth state: the epoch is still running, so a `Sync` *can* move the clock,
/// but not far enough — and the write it would carry is already doomed.
///
/// This is the state the window guard used to miss. It asked "can the clock move forward?" when the
/// question is "does the clock reach?". With `last_update < epoch_end` a `Sync` to `epoch_end` is
/// legal, so the write was built and returned `Ok`; but the write asserts
/// `ASSERT_BEFORE_SECONDS_ABSOLUTE(epoch_end + max_seconds_offset)`, and once the caller's clock has
/// passed that moment the chain rejects the bundle and the operator pays the fee for it.
#[test]
fn a_sync_that_cannot_reach_the_window_is_refused_before_the_operator_pays() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let (mut harness, _reward_slots) = launch_funded_and_inside_the_first_epoch(ctx)?;

    let epoch_end = harness.distributor.info.state.round_time_info.epoch_end;
    let last_update = harness.distributor.info.state.round_time_info.last_update;
    let authority = ManagerAuthority::new(harness.manager.inner_puzzle_hash)?;
    let payout_hash = verdict_for(harness.entry.puzzle_hash);

    // The fixture is the fourth state and not one of the three already covered: the epoch has NOT
    // ended, so a Sync is still able to move the clock strictly forward.
    assert!(
        last_update < epoch_end,
        "a Sync must still be able to move the clock, or this is the already-covered third state"
    );

    // The furthest a Sync may take `last_update` is `epoch_end`, so the write's own
    // ASSERT_BEFORE_SECONDS_ABSOLUTE can be no later than this.
    let furthest_the_write_can_be_valid = epoch_end + MAX_SECONDS_OFFSET;

    let refusal = match add_entry(
        ctx,
        &mut harness.distributor,
        authority,
        payout_hash,
        furthest_the_write_can_be_valid,
    ) {
        Ok(_) => panic!(
            "the best Sync available reaches only {epoch_end}, whose window closes at {furthest_the_write_can_be_valid}: the write is already invalid, so it must be refused here rather than rejected on chain at the operator's expense"
        ),
        Err(error) => error,
    };

    let RewardsError::EntrySetWriteWindowClosed {
        last_update: refused_last_update,
        epoch_end: refused_epoch_end,
        now_unix_seconds,
    } = &refusal
    else {
        panic!("expected EntrySetWriteWindowClosed, got: {refusal}");
    };
    assert_eq!(*refused_last_update, last_update);
    assert_eq!(*refused_epoch_end, epoch_end);
    assert_eq!(*now_unix_seconds, furthest_the_write_can_be_valid);

    // And the guard is not simply refusing everything past the window: one second earlier the best
    // available Sync still lands the write inside its validity window, and the chain accepts the
    // pair. Without this the fix above could be a blanket refusal and the test would not notice.
    let still_reachable = furthest_the_write_can_be_valid - 1;
    harness.sim.set_next_timestamp(still_reachable)?;
    let write = add_entry(
        ctx,
        &mut harness.distributor,
        authority,
        payout_hash,
        still_reachable,
    )?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    ensure_optional_conditions_met(ctx, &mut harness.sim, write.sync_conditions)?;
    let (_, _) = spend_manager_singleton(ctx, &harness.manager, write.manager_conditions)?;
    harness.sim.spend_coins(ctx.take(), &[])?;

    assert_eq!(
        harness.distributor.info.state.active_shares, ENTRY_SHARES,
        "one second inside the reachable window the write still lands on chain"
    );

    Ok(())
}

/// `SPEC.md` §7.4 clauses 3-5 and §7.5: a clawback is authorized by the commitment slot's own
/// recorded hash and by nothing else.
///
/// Not the manager singleton, not the launcher, not the distributor's operator. The refusal is
/// made here rather than on chain because the operator pays the network fee for a spend the puzzle
/// then rejects.
#[test]
fn a_clawback_is_authorized_by_the_commitment_slot_and_nothing_else() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let mut harness = launch_harness(ctx)?;

    // Commit to the SECOND epoch: a commitment is withdrawable while its epoch is still in the
    // future.
    let second_epoch_start = FIRST_EPOCH_START + TEST_EPOCH_SECONDS;
    let first_epoch_slot = harness.first_epoch_slot.clone();
    let reward_slots = commit_to_epoch(
        ctx,
        &mut harness,
        first_epoch_slot,
        second_epoch_start,
        COMMITTED_BASE_UNITS,
    )?;
    let commitment_slot = harness
        .last_commitment_slot
        .clone()
        .expect("committing created a commitment slot");

    assert_eq!(
        clawback_authority(&commitment_slot),
        harness.funder.puzzle_hash,
        "the recorded authority is the funder that committed"
    );
    assert_eq!(
        commitment_distributor_epoch_start(&commitment_slot),
        second_epoch_start,
        "and the commitment is for the epoch it was committed to"
    );

    let reward_slot = pick_reward_slot(&reward_slots, second_epoch_start);

    // A stranger is refused before any spend is built.
    let stranger = Bytes32::new([0x7e; 32]);
    assert_ne!(stranger, harness.funder.puzzle_hash);
    let refusal = match withdraw_committed_incentives(
        ctx,
        &mut harness.distributor,
        commitment_slot.clone(),
        reward_slot.clone(),
        stranger,
    ) {
        Ok(_) => panic!("a stranger must not be able to withdraw the funder's commitment"),
        Err(error) => error,
    };
    assert!(
        matches!(refusal, RewardsError::NotTheClawbackAuthority),
        "a wrong authority is NotTheClawbackAuthority, got: {refusal}"
    );

    // The recorded authority succeeds, and the guards cross-check the figure returned.
    let clawback = withdraw_committed_incentives(
        ctx,
        &mut harness.distributor,
        commitment_slot,
        reward_slot,
        harness.funder.puzzle_hash,
    )?;
    assert_eq!(
        clawback.recovered_base_units(),
        COMMITTED_BASE_UNITS * WITHDRAWAL_SHARE_BPS / 10_000,
        "§7.5: the funder recovers withdrawal_share_bps of the commitment, and the rest stays \
         in the reserve"
    );
    assert!(
        clawback.recovered_base_units() < COMMITTED_BASE_UNITS,
        "a clawback is never the whole commitment: the forfeit is the deterrent"
    );

    Ok(())
}

/// Pick the reward slot that covers `epoch_start`: an exact match if one exists, otherwise the
/// latest slot at or before it, which is the one `CommitIncentives` will split.
fn pick_reward_slot(
    slots: &[Slot<RewardDistributorRewardSlotValue>],
    epoch_start: u64,
) -> Slot<RewardDistributorRewardSlotValue> {
    if let Some(exact) = slots
        .iter()
        .find(|slot| slot.info.value.epoch_start == epoch_start)
    {
        return exact.clone();
    }

    slots
        .iter()
        .filter(|slot| slot.info.value.epoch_start <= epoch_start)
        .max_by_key(|slot| slot.info.value.epoch_start)
        .unwrap_or_else(|| {
            slots
                .iter()
                .min_by_key(|slot| slot.info.value.epoch_start)
                .expect("at least one reward slot")
        })
        .clone()
}

/// Merge newly created reward slots into the known set, dropping any the new ones replace.
fn merge_reward_slots(
    previous: Vec<Slot<RewardDistributorRewardSlotValue>>,
    created: Vec<Slot<RewardDistributorRewardSlotValue>>,
) -> Vec<Slot<RewardDistributorRewardSlotValue>> {
    let mut merged: Vec<Slot<RewardDistributorRewardSlotValue>> = previous
        .into_iter()
        .filter(|slot| {
            !created
                .iter()
                .any(|new| new.info.value.epoch_start == slot.info.value.epoch_start)
        })
        .collect();

    merged.extend(created);
    merged
}

// ---------------------------------------------------------------------------------------------
// `read_distributor` (#3267) -- rebuilding a snapshot from the chain alone.
//
// `MockChainSource` (dig-chainsource-interface, `testing` feature) is loaded from real simulator
// coin records/spends rather than hand-built fixtures, so these tests exercise the actual trait
// boundary `read_distributor` reads through, not a shortcut that only looks like it.
// ---------------------------------------------------------------------------------------------

/// Loads `sim`'s own records for every id in `singleton_members` and `extra_coin_ids` into a
/// fresh `dig_chainsource_interface::MockChainSource`, plus the singleton's lineage and the
/// current peak/timestamp -- everything `read_distributor` needs to answer for `launcher_id`.
fn mock_chain_source(
    sim: &Simulator,
    launcher_id: Bytes32,
    singleton_members: &[Bytes32],
    extra_coin_ids: &[Bytes32],
) -> dig_chainsource_interface::MockChainSource {
    mock_chain_source_missing_timestamps(sim, launcher_id, singleton_members, extra_coin_ids, &[])
}

/// As [`mock_chain_source`], but answers `None` for every height in `missing_heights` rather than
/// loading a timestamp for it -- what a pruning RPC that indexes only the peak looks like.
/// `block_timestamp`'s own contract (`dig-chainsource-interface` 0.3.3, `source.rs:102-105`) makes
/// `Ok(None)` mean "no such block OR no timestamp index", so this is a reachable production
/// answer, not a hostile fixture (F5).
fn mock_chain_source_missing_timestamps(
    sim: &Simulator,
    launcher_id: Bytes32,
    singleton_members: &[Bytes32],
    extra_coin_ids: &[Bytes32],
    missing_heights: &[u32],
) -> dig_chainsource_interface::MockChainSource {
    chain_source_with_gaps(
        sim,
        launcher_id,
        singleton_members,
        extra_coin_ids,
        missing_heights,
        &[],
    )
}

/// As [`mock_chain_source`], but answers `Ok(None)` from `coin_record` for every id in
/// `unrecorded_coin_ids` while still serving that coin's SPEND -- a source that knows a coin was
/// spent but not at which height. `CoinRecord::spent_height` is documented as the spend height
/// "if it has been spent AND THE SOURCE KNOWS IT" (`dig-chainsource-interface` 0.3.3,
/// `record.rs:17`), and a pruning source answers this for an old generation, so it is a
/// sanctioned production answer rather than a hostile fixture (F6).
fn mock_chain_source_unrecorded_coins(
    sim: &Simulator,
    launcher_id: Bytes32,
    singleton_members: &[Bytes32],
    extra_coin_ids: &[Bytes32],
    unrecorded_coin_ids: &[Bytes32],
) -> dig_chainsource_interface::MockChainSource {
    chain_source_with_gaps(
        sim,
        launcher_id,
        singleton_members,
        extra_coin_ids,
        &[],
        unrecorded_coin_ids,
    )
}

/// The one builder both gap-injecting helpers above delegate to, so a fixture can only ever
/// differ from [`mock_chain_source`] by the gaps it names.
fn chain_source_with_gaps(
    sim: &Simulator,
    launcher_id: Bytes32,
    singleton_members: &[Bytes32],
    extra_coin_ids: &[Bytes32],
    missing_heights: &[u32],
    unrecorded_coin_ids: &[Bytes32],
) -> dig_chainsource_interface::MockChainSource {
    let tip = *singleton_members
        .last()
        .expect("a singleton chain always has at least the launcher");

    let mut source = dig_chainsource_interface::MockChainSource::new();

    // The eve coin itself is never `harness.distributor.coin` at any point the test observes --
    // by the time `launch_harness` returns, the launch bundle has already spent it to produce
    // the first post-eve generation. `read_distributor` needs the SPEND that consumed it (what
    // `from_eve_coin_spend` parses), so it must be loaded even though no `singleton_members`
    // entry names it.
    let eve_coin_id = sim
        .children(launcher_id)
        .first()
        .map(|state| state.coin.coin_id());

    let ids = singleton_members
        .iter()
        .copied()
        .chain(extra_coin_ids.iter().copied())
        .chain(eve_coin_id);

    for id in ids {
        // The SPEND is always loaded; only the RECORD is withheld for an unrecorded id. That is
        // what makes the resulting source self-inconsistent in the way F6 is about: it hands over
        // the spend that proves the coin is spent, and no height for it.
        if !unrecorded_coin_ids.contains(&id) {
            if let Some(state) = sim.coin_state(id) {
                source = source.with_coin(
                    id,
                    dig_chainsource_interface::CoinRecord::from_coin_state(state),
                );
            }
        }
        if let Some(spend) = sim.coin_spend(id) {
            source = source.with_spend(id, spend);
        }
    }

    source = source.with_lineage(
        launcher_id,
        dig_chainsource_interface::SingletonLineage::new(tip, singleton_members.iter().copied()),
    );

    let peak = sim.height();
    // Synthetic but monotonic: nothing here asserts these equal any real chain clock, only that
    // every height read_distributor asks about resolves to SOME timestamp -- except a height
    // named in `missing_heights`, which resolves to none at all.
    for height in 0..=peak {
        if missing_heights.contains(&height) {
            continue;
        }
        source = source.with_timestamp(height, mock_timestamp(height));
    }
    source.with_peak(peak)
}

/// The timestamp [`mock_chain_source`] assigns to `height`. Named so a test that needs to reason
/// about elapsed chain time can compute it instead of re-deriving the formula.
fn mock_timestamp(height: u32) -> u64 {
    u64::from(height) * 1_000 + 1
}

/// The all-zero `LineageProof` `RewardDistributor::from_parent_spend` fabricates for the reserve.
/// A genuine read never produces one; asserting against it is the landmine's revert-proof.
fn zero_lineage_proof() -> LineageProof {
    LineageProof {
        parent_parent_coin_info: Bytes32::default(),
        parent_inner_puzzle_hash: Bytes32::default(),
        parent_amount: 0,
    }
}

/// Asserts a read failed as `Malformed` **with the specific reason** -- never merely "some error",
/// which is satisfied by a failure elsewhere in the recipe.
fn assert_malformed_because(
    result: Result<Option<DistributorSnapshot>, RewardsError>,
    reason: &str,
) {
    match result {
        Err(RewardsError::Malformed(message)) => assert!(
            message.contains(reason),
            "Malformed, but for the wrong reason: expected {reason:?}, got {message:?}"
        ),
        other => panic!("expected Malformed({reason:?}), got {other:?}"),
    }
}

/// Launch, then commit to the first distributor epoch.
///
/// Two things this establishes that a bare launch does not: the reserve holds value, and the tip
/// reserve coin is no longer the eve-era one. Both are what the step-11 money cross-check is about.
fn launch_and_commit(
    ctx: &mut SpendContext,
) -> anyhow::Result<(Harness, Vec<Bytes32>, Vec<Bytes32>)> {
    let mut harness = launch_harness(ctx)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;
    let mut members = vec![launcher_id, harness.distributor.coin.coin_id()];
    let reserve_launch_id = harness.distributor.reserve.coin.coin_id();
    let reserve_parent_id = harness.distributor.reserve.coin.parent_coin_info;

    let first_epoch_slot = harness.first_epoch_slot.clone();
    commit_to_epoch(
        ctx,
        &mut harness,
        first_epoch_slot,
        FIRST_EPOCH_START,
        COMMITTED_BASE_UNITS,
    )?;
    members.push(harness.distributor.coin.coin_id());

    let extras = vec![
        reserve_launch_id,
        reserve_parent_id,
        harness.distributor.reserve.coin.coin_id(),
    ];
    Ok((harness, members, extras))
}

/// A `CoinRecord` for `coin`, confirmed at `confirmed_height` and spent at `spent_height`.
fn record(
    coin: Coin,
    confirmed_height: u32,
    spent_height: Option<u32>,
) -> dig_chainsource_interface::CoinRecord {
    dig_chainsource_interface::CoinRecord {
        coin,
        confirmed_height: Some(confirmed_height),
        spent_height,
        timestamp: None,
        coinbase: false,
    }
}

#[test]
fn state_rebuilt_from_chain_matches_what_was_driven() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let mut harness = launch_harness(ctx)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;

    let mut singleton_members = vec![launcher_id, harness.distributor.coin.coin_id()];

    let reserve_launch_id = harness.distributor.reserve.coin.coin_id();
    let reserve_parent_id = harness.distributor.reserve.coin.parent_coin_info;

    let first_epoch_slot = harness.first_epoch_slot.clone();
    let reward_slots = commit_to_epoch(
        ctx,
        &mut harness,
        first_epoch_slot,
        FIRST_EPOCH_START,
        COMMITTED_BASE_UNITS,
    )?;
    singleton_members.push(harness.distributor.coin.coin_id());

    let authority = ManagerAuthority::new(harness.manager.inner_puzzle_hash)?;
    // The generation whose spend carries the AddEntry action: the ONE entry-set write in this
    // drive, and the timestamp §12.4's staleness signal must report (F1, asserted below).
    let entry_write_generation = harness.distributor.coin.coin_id();
    let write = add_entry(
        ctx,
        &mut harness.distributor,
        authority,
        verdict_for(harness.entry.puzzle_hash),
        0,
    )?;
    let entry_slot_value = harness.distributor.pending_spend.created_entry_slots[0];
    let entry_slot = harness
        .distributor
        .created_slot_value_to_slot(entry_slot_value, RewardDistributorSlotNonce::ENTRY);

    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    ensure_optional_conditions_met(ctx, &mut harness.sim, write.sync_conditions)?;
    let (next_manager_coin, next_manager_proof) =
        spend_manager_singleton(ctx, &harness.manager, write.manager_conditions)?;
    harness.sim.spend_coins(ctx.take(), &[])?;
    harness.manager.coin = next_manager_coin;
    harness.manager.proof = next_manager_proof;
    singleton_members.push(harness.distributor.coin.coin_id());

    harness.sim.set_next_timestamp(FIRST_EPOCH_START)?;
    let first_reward_slot = reward_slots
        .iter()
        .find(|slot| slot.info.value.epoch_start == FIRST_EPOCH_START)
        .expect("a reward slot for the first epoch")
        .clone();
    let roll = start_next_distributor_epoch(ctx, &mut harness.distributor, first_reward_slot)?;
    ensure_conditions_met(ctx, &mut harness.sim, roll.conditions)?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    harness.sim.spend_coins(ctx.take(), &[])?;
    singleton_members.push(harness.distributor.coin.coin_id());

    let sync_time = FIRST_EPOCH_START + TEST_EPOCH_SECONDS / 2;
    harness.sim.set_next_timestamp(sync_time)?;
    let sync_conditions = sync_distributor(ctx, &mut harness.distributor, sync_time)?;
    ensure_conditions_met(ctx, &mut harness.sim, sync_conditions)?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    harness.sim.spend_coins(ctx.take(), &[])?;
    singleton_members.push(harness.distributor.coin.coin_id());

    let source = StubSlotSource(entry_slot.clone());
    let PayoutOutcome::Paid { conditions, .. } = initiate_payout(
        ctx,
        &mut harness.distributor,
        &source,
        harness.entry.puzzle_hash,
    )?
    else {
        panic!("the entry is in the set, so the claim must build");
    };
    ensure_conditions_met(ctx, &mut harness.sim, conditions)?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    harness.sim.spend_coins(ctx.take(), &[])?;
    singleton_members.push(harness.distributor.coin.coin_id());

    let reserve_tip_id = harness.distributor.reserve.coin.coin_id();

    let chain = mock_chain_source(
        &harness.sim,
        launcher_id,
        &singleton_members,
        &[reserve_launch_id, reserve_parent_id, reserve_tip_id],
    );

    let snapshot = read_distributor(&chain, launcher_id)?
        .expect("a distributor was launched at this launcher id");

    assert_eq!(
        snapshot.reserve_base_units(),
        harness.distributor.info.state.total_reserves,
        "the rebuilt reserve balance must match what was actually driven"
    );
    assert_eq!(
        snapshot.payout_puzzle_hashes(),
        vec![harness.entry.puzzle_hash],
        "the entry set rebuilt from the chain must match the one entry actually added"
    );
    assert_eq!(
        snapshot.observed().tip_coin_id(),
        harness.distributor.coin.coin_id(),
        "the observed tip must be the real tip"
    );
    assert_ne!(
        snapshot.distributor().reserve.proof,
        zero_lineage_proof(),
        "the reserve proof reaching a snapshot must not be the all-zero one from_parent_spend          fabricates -- but NOTE: this assertion is NOT a revert-proof for that authentication.          This walk drives several generations past eve, and each `.child()` re-derives the proof          via `child_lineage_proof()`, so a landmine at steps 4-5 heals before the snapshot is          returned and this assertion still passes. The load-bearing one is in          `a_nonzero_amount_decoy_at_the_reserve_puzzle_hash_does_not_confuse_the_selector`,          which reads with no generation driven past eve"
    );

    // ---- §12.4, and why the signal cannot be the slot deltas (F1) ----------------------------
    // The LAST generation driven above is a payout, which spends the claimer's entry slot and
    // re-creates it. So a signal derived from created/spent entry slots reports the PAYOUT's
    // timestamp -- a value the party being paid controls -- and a distributor whose prover died
    // reads healthy forever as long as one holder keeps claiming. The signal must name the
    // AddEntry above, which is the only write the operator performed.
    let entry_write_height = harness
        .sim
        .coin_state(entry_write_generation)
        .expect("the entry-writing generation is a real coin")
        .spent_height
        .expect("it was spent by the AddEntry bundle");
    assert_eq!(
        snapshot.observed().last_entry_write_unix(),
        Some(mock_timestamp(entry_write_height)),
        "the liveness signal must name the AddEntry generation, never the later payout"
    );

    // And therefore: threshold-and-more of chain time after that write, with a payout in between,
    // the entry set reports STALE. A payee cannot reset it.
    let stale_peak = harness.sim.height() + 1;
    let much_later = chain.clone().with_peak(stale_peak).with_timestamp(
        stale_peak,
        mock_timestamp(entry_write_height) + STALE_ENTRY_SET_SECONDS,
    );
    let stale = read_distributor(&much_later, launcher_id)?.expect("still launched");
    assert!(
        stale.reserve_base_units() > 0,
        "the §12.4 signal is only meaningful while value is at stake"
    );
    assert!(
        stale.entry_set_stale(),
        "a claim is not an entry-set write: §12.4 must report stale once the threshold has passed \
         since the last AddEntry/RemoveEntry, however recently someone was paid"
    );
    assert!(
        !stale.entry_set_frozen(),
        "this distributor has a real manager singleton, so its entry set is not frozen"
    );

    Ok(())
}

// ---------------------------------------------------------------------------------------------
// F5: an observed entry-set write whose generation has no resolvable chain timestamp must refuse
// the read, never report a snapshot with a stale/absent `last_entry_write_unix` -- a pruning RPC
// answers `Some` for the peak and `None` for an old height, so this is a reachable production
// answer (`block_timestamp`'s own contract, `dig-chainsource-interface` 0.3.3, source.rs:102-105),
// not a hostile fixture.
// ---------------------------------------------------------------------------------------------

#[test]
fn an_entry_write_generation_with_no_resolvable_timestamp_is_refused() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let mut harness = launch_harness(ctx)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;

    let mut singleton_members = vec![launcher_id, harness.distributor.coin.coin_id()];

    // Captured BEFORE any further generation: `find_eve_reserve_provenance` authenticates the
    // EVE-ERA reserve coin against its parent spend, so it is the launch-time reserve and its
    // parent -- not the tip reserve -- that the chain source must be able to answer for.
    let reserve_launch_id = harness.distributor.reserve.coin.coin_id();
    let reserve_parent_id = harness.distributor.reserve.coin.parent_coin_info;

    let authority = ManagerAuthority::new(harness.manager.inner_puzzle_hash)?;
    // The generation whose spend carries the AddEntry action -- the one entry-set write this
    // walk observes, and whose SPENT HEIGHT will be made timestamp-less below.
    let entry_write_generation = harness.distributor.coin.coin_id();
    let write = add_entry(
        ctx,
        &mut harness.distributor,
        authority,
        verdict_for(harness.entry.puzzle_hash),
        0,
    )?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    ensure_optional_conditions_met(ctx, &mut harness.sim, write.sync_conditions)?;
    let (next_manager_coin, next_manager_proof) =
        spend_manager_singleton(ctx, &harness.manager, write.manager_conditions)?;
    harness.sim.spend_coins(ctx.take(), &[])?;
    harness.manager.coin = next_manager_coin;
    harness.manager.proof = next_manager_proof;
    singleton_members.push(harness.distributor.coin.coin_id());

    let entry_write_height = harness
        .sim
        .coin_state(entry_write_generation)
        .expect("the entry-writing generation is a real coin")
        .spent_height
        .expect("it was spent by the AddEntry bundle");

    let reserve_tip_id = harness.distributor.reserve.coin.coin_id();

    let chain = mock_chain_source_missing_timestamps(
        &harness.sim,
        launcher_id,
        &singleton_members,
        &[reserve_launch_id, reserve_parent_id, reserve_tip_id],
        &[entry_write_height],
    );

    assert_malformed_because(
        read_distributor(&chain, launcher_id),
        "an observed entry-set write has no resolvable chain timestamp",
    );

    Ok(())
}

// ---------------------------------------------------------------------------------------------
// F6: the same walk, one branch earlier. `read_distributor` resolves an observed entry-set
// write's timestamp from `coin_record(generation).spent_height`, and BOTH halves of that can be
// absent from a source that is not lying: `spent_height` is documented as the height "if it has
// been spent and the source knows it" (`dig-chainsource-interface` 0.3.3, `record.rs:17`), and a
// pruning source answers `Ok(None)` for an old generation's record outright. Either way the walk
// reaches the tip and reports `last_entry_write_unix: None` -- which
// `ChainObservation::last_entry_write_unix` documents as the POSITIVE fact "no entry-set write
// has ever happened since launch", about a write this very walk observed.
//
// The fixture drives the `coin_record(..) == Ok(None)` sub-path: the source serves the write
// generation's SPEND (so the walk parses the AddEntry and the branch is entered) and withholds
// only its record. That is the same contradiction step 8 already refuses -- a source that hands
// over a spend it cannot place on a chain.
// ---------------------------------------------------------------------------------------------

#[test]
fn an_entry_write_generation_with_no_resolvable_spent_height_is_refused() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let mut harness = launch_harness(ctx)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;

    let mut singleton_members = vec![launcher_id, harness.distributor.coin.coin_id()];

    // Captured BEFORE the write generation is driven: `find_eve_reserve_provenance`
    // authenticates the EVE-ERA reserve against its parent spend, so capturing these after the
    // write would make the read die on the eve-era reserve long before it reaches the branch
    // under test, and the test would be red for the wrong reason.
    let reserve_launch_id = harness.distributor.reserve.coin.coin_id();
    let reserve_parent_id = harness.distributor.reserve.coin.parent_coin_info;

    let authority = ManagerAuthority::new(harness.manager.inner_puzzle_hash)?;
    // The generation whose spend carries the AddEntry action -- the one entry-set write this walk
    // observes, and the one whose coin RECORD is withheld below.
    let entry_write_generation = harness.distributor.coin.coin_id();
    let write = add_entry(
        ctx,
        &mut harness.distributor,
        authority,
        verdict_for(harness.entry.puzzle_hash),
        0,
    )?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    ensure_optional_conditions_met(ctx, &mut harness.sim, write.sync_conditions)?;
    let (next_manager_coin, next_manager_proof) =
        spend_manager_singleton(ctx, &harness.manager, write.manager_conditions)?;
    harness.sim.spend_coins(ctx.take(), &[])?;
    harness.manager.coin = next_manager_coin;
    harness.manager.proof = next_manager_proof;
    singleton_members.push(harness.distributor.coin.coin_id());

    let reserve_tip_id = harness.distributor.reserve.coin.coin_id();

    let chain = mock_chain_source_unrecorded_coins(
        &harness.sim,
        launcher_id,
        &singleton_members,
        &[reserve_launch_id, reserve_parent_id, reserve_tip_id],
        &[entry_write_generation],
    );

    assert_malformed_because(
        read_distributor(&chain, launcher_id),
        "an observed entry-set write's generation has no resolvable spent height",
    );

    Ok(())
}

#[test]
fn a_snapshot_from_before_a_write_is_not_current() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let mut harness = launch_harness(ctx)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;

    let mut singleton_members = vec![launcher_id, harness.distributor.coin.coin_id()];
    let reserve_launch_id = harness.distributor.reserve.coin.coin_id();
    let reserve_parent_id = harness.distributor.reserve.coin.parent_coin_info;

    let authority = ManagerAuthority::new(harness.manager.inner_puzzle_hash)?;
    let write = add_entry(
        ctx,
        &mut harness.distributor,
        authority,
        verdict_for(harness.entry.puzzle_hash),
        0,
    )?;
    let entry_slot_value = harness.distributor.pending_spend.created_entry_slots[0];
    let entry_slot = harness
        .distributor
        .created_slot_value_to_slot(entry_slot_value, RewardDistributorSlotNonce::ENTRY);

    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    ensure_optional_conditions_met(ctx, &mut harness.sim, write.sync_conditions)?;
    let (next_manager_coin, next_manager_proof) =
        spend_manager_singleton(ctx, &harness.manager, write.manager_conditions)?;
    harness.sim.spend_coins(ctx.take(), &[])?;
    harness.manager.coin = next_manager_coin;
    harness.manager.proof = next_manager_proof;
    singleton_members.push(harness.distributor.coin.coin_id());
    let reserve_tip_id = harness.distributor.reserve.coin.coin_id();

    let chain_s1 = mock_chain_source(
        &harness.sim,
        launcher_id,
        &singleton_members,
        &[reserve_launch_id, reserve_parent_id, reserve_tip_id],
    );
    let s1 = read_distributor(&chain_s1, launcher_id)?.expect("launched");

    let now = harness.distributor.info.state.round_time_info.last_update;
    let removal = remove_entry(ctx, &mut harness.distributor, authority, entry_slot, now)?;
    ensure_optional_conditions_met(ctx, &mut harness.sim, removal.write.sync_conditions)?;
    let (_, _) = spend_manager_singleton(ctx, &harness.manager, removal.write.manager_conditions)?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    harness.sim.spend_coins(ctx.take(), &[])?;
    singleton_members.push(harness.distributor.coin.coin_id());
    let reserve_tip_id = harness.distributor.reserve.coin.coin_id();

    let chain_s2 = mock_chain_source(
        &harness.sim,
        launcher_id,
        &singleton_members,
        &[reserve_launch_id, reserve_parent_id, reserve_tip_id],
    );
    let s2 = read_distributor(&chain_s2, launcher_id)?.expect("still launched");

    // A new block arrives and the distributor is not spent in it: the state S2 read is still the
    // current state, but the peak has moved. This is the case mainnet is ALWAYS in within seconds
    // of any read, and a frozen-peak fixture cannot express it (S2 findings, notes 6d/A3).
    let later_peak = harness.sim.height() + 1;
    let chain_after_a_block = chain_s2
        .clone()
        .with_peak(later_peak)
        .with_timestamp(later_peak, mock_timestamp(later_peak));

    assert_ne!(
        s1.observed().tip_coin_id(),
        s2.observed().tip_coin_id(),
        "a write must move the observed tip -- this is what is_current relies on"
    );
    assert!(
        !s1.is_current(&chain_after_a_block)?,
        "S1 was read before the removal; against the post-removal chain it must be stale"
    );
    assert!(
        s2.is_current(&chain_after_a_block)?,
        "S2 is the current state and a bare new block does not change that -- is_current must          answer 'is this still the current state', not 'has any block arrived since'"
    );

    Ok(())
}

// The `MockChainSource` fixtures above only ever answer with coins the test enumerated, so the
// eve-reserve selector's decoy-rejection and ambiguity branches (`state.rs`'s
// `find_eve_reserve_provenance`) are never exercised by a round trip alone. These two tests inject
// synthetic candidates directly at the real `reserve_full_puzzle_hash`, on top of a genuine launch,
// to drive those branches deliberately.
#[test]
fn a_nonzero_amount_decoy_at_the_reserve_puzzle_hash_does_not_confuse_the_selector(
) -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let harness = launch_harness(ctx)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;
    let reserve_full_puzzle_hash = harness.distributor.info.constants.reserve_full_puzzle_hash;

    let singleton_members = vec![launcher_id, harness.distributor.coin.coin_id()];
    let reserve_launch_id = harness.distributor.reserve.coin.coin_id();
    let reserve_parent_id = harness.distributor.reserve.coin.parent_coin_info;

    let chain = mock_chain_source(
        &harness.sim,
        launcher_id,
        &singleton_members,
        &[reserve_launch_id, reserve_parent_id],
    );

    // A decoy sitting at the SAME puzzle hash, funded (amount != 0), so it is never a candidate --
    // an attacker cannot buy their way into the selection by parking a paid coin at this hash.
    let decoy = Coin::new(Bytes32::new([0x77; 32]), reserve_full_puzzle_hash, 999);
    let chain = chain.with_coin(
        decoy.coin_id(),
        dig_chainsource_interface::CoinRecord {
            coin: decoy,
            confirmed_height: Some(0),
            spent_height: None,
            timestamp: None,
            coinbase: false,
        },
    );

    let snapshot = read_distributor(&chain, launcher_id)?
        .expect("a distributor was launched at this launcher id");
    assert_eq!(
        snapshot.reserve_base_units(),
        harness.distributor.info.state.total_reserves,
        "a funded decoy at the reserve puzzle hash must never be picked -- the real, zero-amount \
         eve-era candidate must still be the one authenticated"
    );

    // `harness.distributor.coin` is still unspent at this point (no generation past eve has been
    // driven), so `snapshot.distributor().reserve.proof` is the eve authentication's OWN proof,
    // un-healed by any later `.child()` call -- this is the one place in the walk where a landmine
    // at steps 4-5 (skipping `find_eve_reserve_provenance`'s authentication) would actually surface
    // in the returned snapshot, rather than being masked by a subsequent generation's re-derivation.
    assert_ne!(
        snapshot.distributor().reserve.proof,
        zero_lineage_proof(),
        "REVERT-PROOF: an unauthenticated (zero) eve-reserve proof must never reach a snapshot \
         that has driven no generation past eve"
    );

    Ok(())
}

/// #3304 item 2: a zero-amount decoy at the reserve puzzle hash that has no resolvable CAT
/// lineage (no registered parent spend) must be discarded during authentication, not treated as
/// an ambiguous sibling of the real candidate -- otherwise landing such a decoy is a standing
/// read-DoS requiring no signature and no genuine spend. This replaces the previous version of
/// this test, which asserted the OLD (buggy) selects-before-authenticating behaviour: it expected
/// this exact decoy to make the read refuse with "ambiguous ...". Under the fix the decoy fails
/// authentication and is silently discarded, and the real candidate alone is picked.
#[test]
fn a_zero_amount_decoy_that_fails_lineage_does_not_prevent_the_read() -> anyhow::Result<()> {
    use dig_chainsource_interface::ChainSource;

    let ctx = &mut SpendContext::new();
    let harness = launch_harness(ctx)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;
    let reserve_full_puzzle_hash = harness.distributor.info.constants.reserve_full_puzzle_hash;

    let singleton_members = vec![launcher_id, harness.distributor.coin.coin_id()];
    let reserve_launch_id = harness.distributor.reserve.coin.coin_id();
    let reserve_parent_id = harness.distributor.reserve.coin.parent_coin_info;

    let chain = mock_chain_source(
        &harness.sim,
        launcher_id,
        &singleton_members,
        &[reserve_launch_id, reserve_parent_id],
    );

    let real_candidate = chain
        .coin_records_by_puzzle_hash(reserve_full_puzzle_hash, true)
        .expect("mock reads never fail")
        .into_iter()
        .find(|record| record.coin.amount == 0)
        .expect("the genuine eve-era reserve candidate is loaded");

    // A second, distinct coin at the identical puzzle hash, also zero-amount, confirmed at the
    // identical height -- but with NO registered parent spend, so it can never authenticate as a
    // genuine CAT of the reserve asset id. A decoy this cheap to plant must not brick the read.
    let twin = Coin::new(Bytes32::new([0x88; 32]), reserve_full_puzzle_hash, 0);
    let chain = chain.with_coin(
        twin.coin_id(),
        dig_chainsource_interface::CoinRecord {
            coin: twin,
            confirmed_height: real_candidate.confirmed_height,
            spent_height: None,
            timestamp: None,
            coinbase: false,
        },
    );

    let snapshot = read_distributor(&chain, launcher_id)?
        .expect("a distributor was launched at this launcher id");
    assert_eq!(
        snapshot.reserve_base_units(),
        harness.distributor.info.state.total_reserves,
        "a decoy that fails CAT lineage must never be picked -- the real, authenticating \
         candidate must still be the one used"
    );

    Ok(())
}

/// #3304 item 2: even after fixing decoys-that-fail-authentication, two candidates that BOTH
/// authenticate as genuine CATs of the reserve asset id at the same lowest confirmed height must
/// still refuse -- there is no principled tiebreak, and picking one arbitrarily would be a wrong
/// read, which is worse than a refusal.
#[test]
fn two_authenticating_reserve_candidates_at_the_same_height_still_refuses() -> anyhow::Result<()> {
    use dig_chainsource_interface::ChainSource;

    let ctx = &mut SpendContext::new();
    let harness = launch_harness(ctx)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;
    let reserve_full_puzzle_hash = harness.distributor.info.constants.reserve_full_puzzle_hash;
    let reserve_inner_puzzle_hash = harness.distributor.info.constants.reserve_inner_puzzle_hash;

    let singleton_members = vec![launcher_id, harness.distributor.coin.coin_id()];
    let reserve_launch_id = harness.distributor.reserve.coin.coin_id();
    let reserve_parent_id = harness.distributor.reserve.coin.parent_coin_info;

    let chain = mock_chain_source(
        &harness.sim,
        launcher_id,
        &singleton_members,
        &[reserve_launch_id, reserve_parent_id],
    );

    // Build a genuine second CAT spend of the SAME asset id as the distributor's reserve asset,
    // landing a zero-amount output at the SAME `reserve_inner_puzzle_hash` -- which the CAT layer
    // wraps to the identical `reserve_full_puzzle_hash` the real eve reserve sits at, since both
    // share the same asset id. This is not a decoy: `Cat::parse_children` authenticates it exactly
    // as it would the real candidate.
    let funder_p2 = StandardLayer::new(harness.funder.pk);
    let second_source_cat = harness.source_cat;
    let leftover = second_source_cat.coin.amount;
    let cat_inner_puzzle = clvm_quote!(Conditions::new()
        .create_coin(reserve_inner_puzzle_hash, 0, Memos::None)
        .create_coin(harness.funder.puzzle_hash, leftover, Memos::None))
    .to_clvm(ctx)?;
    let cat_inner_spend = funder_p2.delegated_inner_spend(
        ctx,
        Spend {
            puzzle: cat_inner_puzzle,
            solution: NodePtr::NIL,
        },
    )?;
    second_source_cat.spend(
        ctx,
        SingleCatSpend {
            prev_coin_id: second_source_cat.coin.coin_id(),
            next_coin_proof: CoinProof {
                parent_coin_info: second_source_cat.coin.parent_coin_info,
                inner_puzzle_hash: harness.funder.puzzle_hash,
                amount: second_source_cat.coin.amount,
            },
            prev_subtotal: 0,
            extra_delta: 0,
            p2_spend: cat_inner_spend,
            revoke: false,
        },
    )?;
    let spends = ctx.take();
    let second_parent_spend = spends
        .into_iter()
        .find(|spend| spend.coin.coin_id() == second_source_cat.coin.coin_id())
        .expect("the second CAT spend was produced");

    let twin = Coin::new(second_source_cat.coin.coin_id(), reserve_full_puzzle_hash, 0);
    assert_eq!(
        twin.puzzle_hash, reserve_full_puzzle_hash,
        "the CAT layer must wrap the same asset id + inner puzzle hash to the identical full \
         puzzle hash the real eve reserve sits at, or this fixture is not testing what it claims"
    );

    let real_confirmed_height = chain
        .coin_records_by_puzzle_hash(reserve_full_puzzle_hash, true)
        .expect("mock reads never fail")
        .into_iter()
        .find(|record| record.coin.amount == 0)
        .expect("the genuine eve-era reserve candidate is loaded")
        .confirmed_height;

    let chain = chain
        .with_coin(
            twin.coin_id(),
            dig_chainsource_interface::CoinRecord {
                coin: twin,
                confirmed_height: real_confirmed_height,
                spent_height: None,
                timestamp: None,
                coinbase: false,
            },
        )
        .with_spend(second_source_cat.coin.coin_id(), second_parent_spend);

    assert_malformed_because(
        read_distributor(&chain, launcher_id),
        "ambiguous eve-era reserve candidates at the lowest confirmed height",
    );

    Ok(())
}

// ---------------------------------------------------------------------------------------------
// `read_distributor` error paths, over a bare `MockChainSource` -- no simulator, since these
// are about what `read_distributor` does with an incomplete or hostile answer, not about
// puzzle behaviour.
// ---------------------------------------------------------------------------------------------

#[test]
fn zero_launcher_id_is_refused_before_any_chain_read() {
    let chain = dig_chainsource_interface::MockChainSource::new();
    let result = read_distributor(&chain, Bytes32::default());
    assert!(
        matches!(result, Err(RewardsError::Malformed(_))),
        "the zero hash is never a real launcher id, and this must be caught before any read"
    );
}

#[test]
fn an_unspent_launcher_reads_as_never_launched() {
    let chain = dig_chainsource_interface::MockChainSource::new();
    let launcher_id = Bytes32::new([0x42; 32]);
    let result = read_distributor(&chain, launcher_id);
    assert!(
        result.unwrap().is_none(),
        "no coin record for the launcher id -- Ok(None) is the ONLY case this reserves"
    );
}

// ---------------------------------------------------------------------------------------------
// The launch-created reward slot (F2), the step-11 money cross-check (F3) and the fail-closed
// rule (F4) -- every one of these was invisible to a fully green suite.
// ---------------------------------------------------------------------------------------------

/// A prover rebuilt from the chain must be able to roll the FIRST distributor epoch, which means
/// the reward slot the launch created has to reach the snapshot. An in-process launcher gets it as
/// `LaunchedDistributor::first_distributor_epoch_slot`; SPEC §12.1 clause 1 exists so that a
/// reader with nothing but the launcher id gets the same thing.
#[test]
fn the_launch_created_reward_slot_reaches_a_chain_rebuilt_snapshot() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let harness = launch_harness(ctx)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;

    let members = vec![launcher_id, harness.distributor.coin.coin_id()];
    let extras = vec![
        harness.distributor.reserve.coin.coin_id(),
        harness.distributor.reserve.coin.parent_coin_info,
    ];
    let chain = mock_chain_source(&harness.sim, launcher_id, &members, &extras);

    let snapshot = read_distributor(&chain, launcher_id)?.expect("launched");
    let launch_slot = harness.first_epoch_slot.info.value;

    assert_eq!(
        snapshot.slots().rewards,
        vec![launch_slot],
        "the reward slot the launch created is the only outstanding one, and a chain-rebuilt \
         prover cannot roll the first epoch without it"
    );
    assert_eq!(
        snapshot.rewards_per_distributor_epoch(),
        vec![(launch_slot.epoch_start, launch_slot.rewards)],
        "rewards_per_distributor_epoch under-reports whenever a reward slot is dropped"
    );

    Ok(())
}

#[test]
fn a_tip_reserve_absent_from_the_chain_is_refused() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let (harness, members, extras) = launch_and_commit(ctx)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;
    let tip_reserve_id = harness.distributor.reserve.coin.coin_id();

    // Everything the reader needs EXCEPT the tip reserve coin's own record.
    let extras: Vec<Bytes32> = extras
        .into_iter()
        .filter(|id| *id != tip_reserve_id)
        .collect();
    let chain = mock_chain_source(&harness.sim, launcher_id, &members, &extras);

    assert_malformed_because(
        read_distributor(&chain, launcher_id),
        "tip reserve coin is not present on chain",
    );
    Ok(())
}

#[test]
fn an_already_spent_tip_reserve_is_refused() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let (harness, members, extras) = launch_and_commit(ctx)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;
    let tip_reserve = harness.distributor.reserve.coin;

    let chain = mock_chain_source(&harness.sim, launcher_id, &members, &extras).with_coin(
        tip_reserve.coin_id(),
        record(tip_reserve, 0, Some(harness.sim.height())),
    );

    assert_malformed_because(
        read_distributor(&chain, launcher_id),
        "tip reserve coin is already spent",
    );
    Ok(())
}

#[test]
fn a_tip_reserve_amount_disagreeing_with_total_reserves_is_refused() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let (harness, members, extras) = launch_and_commit(ctx)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;
    let tip_reserve = harness.distributor.reserve.coin;

    // The same coin id carrying a different amount: what a wrong eve-reserve selection produces,
    // and the reason the cross-check compares the amount rather than mere existence.
    let lying = Coin::new(
        tip_reserve.parent_coin_info,
        tip_reserve.puzzle_hash,
        tip_reserve.amount + 1,
    );
    let chain = mock_chain_source(&harness.sim, launcher_id, &members, &extras)
        .with_coin(tip_reserve.coin_id(), record(lying, 0, None));

    assert_malformed_because(
        read_distributor(&chain, launcher_id),
        "tip reserve amount does not match",
    );
    Ok(())
}

#[test]
fn a_chain_source_that_errors_never_renders_as_an_empty_distributor() {
    let chain = dig_chainsource_interface::MockChainSource::new()
        .fail_with(dig_chainsource_interface::ChainSourceError::Timeout);
    let result = read_distributor(&chain, Bytes32::new([0x42; 32]));

    match result {
        Err(RewardsError::ChainUnavailable(_)) => {}
        other => panic!(
            "a source that cannot answer must be ChainUnavailable -- never Ok(None), which would \
             render a failed read as 'no distributor was ever launched'; got {other:?}"
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// #3303 / #3305: read_distributor's two pre-guards against chia-sdk-driver 0.36.0's unchecked u64
// share multiply (#3286). SPEC.md 0.1 clause 5d.
//
// Both tests reach the guards the way an attacker does -- through the PUBLIC reader, over chain
// input nobody authenticated -- rather than by calling the guard's own predicate.
// ---------------------------------------------------------------------------------------------

/// B1, end to end: a distributor launched with a `withdrawal_share_bps` outside `0..=10_000` is
/// refused by `read_distributor` as an **error**, not reported as state and not `Ok(None)`.
///
/// `RewardDistributorConstants` takes `withdrawal_share_bps` as a raw `u64` and neither upstream
/// nor `launch_dig_distributor` narrows it, so anyone can launch this distributor and any caller
/// of the public reader then reads it. Before the guard (commit `933b2ea`) this call returned
/// `Ok(Some(snapshot))` whose `withdrawal_share_bps` was `u64::MAX / 2`, handed on as
/// authenticated distributor state -- the match below is what distinguishes the two.
///
/// The bps is written as `u64::MAX / 2`, never as a decimal literal: a literal here would still
/// pass if the guard's own bound were mutated.
#[test]
fn a_distributor_launched_with_an_out_of_domain_bps_is_refused_by_the_reader() -> anyhow::Result<()>
{
    const HOSTILE_BPS: u64 = u64::MAX / 2;

    let ctx = &mut SpendContext::new();
    let harness = launch_harness_with(ctx, MINTED_BASE_UNITS, HOSTILE_BPS)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;

    let members = vec![launcher_id, harness.distributor.coin.coin_id()];
    let extras = vec![
        harness.distributor.reserve.coin.coin_id(),
        harness.distributor.reserve.coin.parent_coin_info,
    ];
    let chain = mock_chain_source(&harness.sim, launcher_id, &members, &extras);

    match read_distributor(&chain, launcher_id) {
        Err(RewardsError::UnreadableDistributorConstants {
            withdrawal_share_bps,
        }) => assert_eq!(
            withdrawal_share_bps, HOSTILE_BPS,
            "the refusal must name the bps it read off the chain"
        ),
        Ok(Some(snapshot)) => panic!(
            "the reader handed on an out-of-domain bps as authenticated state: {:?}",
            snapshot.rewards_per_distributor_epoch()
        ),
        Ok(None) => panic!(
            "Ok(None) asserts the positive fact that no such distributor exists, and one does \
             (SPEC.md 0.1 clause 5d)"
        ),
        Err(other) => panic!("expected UnreadableDistributorConstants, got: {other}"),
    }

    Ok(())
}

/// B1 sibling for `epoch_seconds`, and a POSITION test as much as a domain test.
///
/// A distributor launched with `epoch_seconds = 0` makes upstream's reward-slot backfill loop
/// (`commit_incentives.rs:101-111`) non-terminating, and being a pure non-yielding CPU loop no
/// consumer-side `tokio::time::timeout` can cancel it -- so this reader's refusal is the only
/// defence that can work, and it must therefore run as early as it possibly can.
///
/// What makes this discriminating rather than merely a type assertion: the history read back here
/// contains **no `commit_incentives` action at all** -- only the launch and the eve spend. The
/// per-action screen (`refuse_unrepresentable_action_arithmetic`) inspects `epoch_seconds` only
/// inside its `commit_incentives` branch, so it can never fire on this history. A refusal here can
/// only have come from the launch-constants check, which runs before a single generation is
/// parsed. Moving that check back down into the walk turns this test red while an
/// error-variant-only assertion would stay green.
#[test]
fn a_distributor_launched_with_zero_epoch_seconds_is_refused_before_any_generation_is_parsed(
) -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let harness = launch_harness_with_epoch_seconds(ctx, MINTED_BASE_UNITS, 0)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;

    let members = vec![launcher_id, harness.distributor.coin.coin_id()];
    let extras = vec![
        harness.distributor.reserve.coin.coin_id(),
        harness.distributor.reserve.coin.parent_coin_info,
    ];
    let chain = mock_chain_source(&harness.sim, launcher_id, &members, &extras);

    match read_distributor(&chain, launcher_id) {
        Err(RewardsError::UnreadableEpochSeconds) => {}
        Ok(Some(snapshot)) => panic!(
            "the reader handed on a zero-epoch_seconds distributor as authenticated state: {:?}",
            snapshot.rewards_per_distributor_epoch()
        ),
        Ok(None) => panic!(
            "Ok(None) asserts the positive fact that no such distributor exists, and one does \
             (SPEC.md 0.1 clause 5d)"
        ),
        Err(other) => panic!("expected UnreadableEpochSeconds, got: {other}"),
    }

    Ok(())
}

/// B2, end to end, with **DIG's own** `WITHDRAWAL_SHARE_BPS`: a commitment one base unit above
/// `u64::MAX / WITHDRAWAL_SHARE_BPS`, withdrawn on chain, makes upstream's `get_log` multiply
/// (`withdraw_incentives.rs:71`) wrap while the puzzle itself pays correctly -- so the reader must
/// refuse the generation rather than reconstruct a fabricated `created_reward_slot.rewards`.
///
/// This is the reachable shape of #3286 through the public reader, and it needs no hostile
/// constants at all: bps is DIG's 9_000, the table is DIG's own, and the only unusual thing is the
/// SIZE of the commitment. B1 cannot catch it; B2 is what does.
///
/// **Release-only, by necessity.** Building the fixture means calling upstream's withdraw action
/// directly, and upstream's own share multiply (`withdraw_incentives.rs:105-107`) is the same
/// unchecked `u64`: under `debug_assertions` it panics before returning, so the on-chain spend
/// this test reads back cannot be constructed in a checked profile at all. `cargo test --release`
/// covers it (see `.github/workflows/ci.yml`); the debug profile covers the panic itself in
/// `tests/recoverable_share.rs`.
///
/// Before the guard (commit `933b2ea`) this read returned `Ok(Some(snapshot))` built from a
/// wrapped multiply.
#[cfg(not(debug_assertions))]
#[test]
fn a_commitment_above_the_driver_bound_is_refused_by_the_reader() -> anyhow::Result<()> {
    // One base unit past the largest commitment whose share upstream can compute in a u64.
    // Derived from the bound, never spelled: a decimal literal here would survive a mutation of
    // MAX_REPORTABLE_COMMITMENT_BASE_UNITS.
    let committed_base_units = u64::MAX / WITHDRAWAL_SHARE_BPS + 1;
    let headroom = 1_000;

    let ctx = &mut SpendContext::new();
    let mut harness =
        launch_harness_with(ctx, committed_base_units + headroom, WITHDRAWAL_SHARE_BPS)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;
    let mut members = vec![launcher_id, harness.distributor.coin.coin_id()];
    let mut extras = vec![
        harness.distributor.reserve.coin.coin_id(),
        harness.distributor.reserve.coin.parent_coin_info,
    ];

    // A commitment is withdrawable while its epoch is still in the future.
    let second_epoch_start = FIRST_EPOCH_START + TEST_EPOCH_SECONDS;
    let first_epoch_slot = harness.first_epoch_slot.clone();
    let reward_slots = commit_to_epoch(
        ctx,
        &mut harness,
        first_epoch_slot,
        second_epoch_start,
        committed_base_units,
    )?;
    members.push(harness.distributor.coin.coin_id());
    extras.push(harness.distributor.reserve.coin.coin_id());

    let commitment_slot = harness
        .last_commitment_slot
        .clone()
        .expect("committing created a commitment slot");
    let reward_slot = pick_reward_slot(&reward_slots, second_epoch_start);

    // Deliberately NOT through `withdraw_committed_incentives`: its own pre-guard refuses exactly
    // this pair, and the question here is what the READER does with a spend already on chain.
    let mut distributor = harness.distributor.clone();
    let (conditions, driver_reported) = distributor
        .new_action::<chia_sdk_driver::RewardDistributorWithdrawIncentivesAction>()
        .spend(ctx, &mut distributor, commitment_slot, reward_slot)?;
    harness.distributor = distributor;

    // The fixture's own premise: the driver's returned figure is ALREADY wrong here. The correct
    // share is `committed * 9_000 / 10_000`, computed in u128 so this comparison does not share
    // the arithmetic it is judging.
    let true_share =
        u64::try_from(u128::from(committed_base_units) * u128::from(WITHDRAWAL_SHARE_BPS) / 10_000)
            .expect("the share never exceeds the commitment");
    assert_ne!(
        driver_reported, true_share,
        "this fixture only proves something if the driver has already misreported"
    );

    let authority_coin = harness.sim.new_coin(harness.funder.puzzle_hash, 1);
    StandardLayer::new(harness.funder.pk).spend(ctx, authority_coin, conditions)?;
    harness.distributor = harness.distributor.clone().finish_spend(ctx, vec![])?.0;
    harness
        .sim
        .spend_coins(ctx.take(), std::slice::from_ref(&harness.funder.sk))?;
    members.push(harness.distributor.coin.coin_id());
    extras.push(harness.distributor.reserve.coin.coin_id());

    let chain = mock_chain_source(&harness.sim, launcher_id, &members, &extras);

    match read_distributor(&chain, launcher_id) {
        Err(RewardsError::CommitmentRewardsTooLargeToRead {
            rewards_base_units,
            max_readable_base_units,
        }) => {
            assert_eq!(
                max_readable_base_units, MAX_REPORTABLE_COMMITMENT_BASE_UNITS,
                "the refusal must name the derived bound"
            );
            assert_eq!(
                rewards_base_units, committed_base_units,
                "the refusal must name the commitment slot's own recorded rewards, caught at the \
                 generation that CREATED it -- strictly earlier than the withdraw generation this \
                 fixture goes on to build"
            );
        }
        Ok(Some(snapshot)) => panic!(
            "the reader reconstructed a generation through a wrapped u64 multiply: {:?}",
            snapshot.rewards_per_distributor_epoch()
        ),
        Ok(None) => panic!("Ok(None) is forbidden here (SPEC.md 0.1 clause 5d)"),
        Err(other) => panic!("expected CommitmentRewardsTooLargeToRead, got: {other}"),
    }

    Ok(())
}

/// One-spend BATCH composition, and the discriminating regression the triple gate required at
/// PR #8: B2 must refuse a commitment above `MAX_REPORTABLE_COMMITMENT_BASE_UNITS` even when that
/// commitment is created in the SAME distributor-coin spend as another reserve-affecting action --
/// not only across two separate, single-action generations, which is the shape
/// `a_commitment_above_the_driver_bound_is_refused_by_the_reader` above uses and the shape that was
/// never at risk.
///
/// **Why the second action is an `AddEntry`, and why that choice is the whole point.** An earlier
/// version of this test batched the commit with a withdraw of that same just-created commitment
/// slot, and could not discriminate the guard at all: this crate's own slot bookkeeping removes a
/// generation's SPENT slots before extending with its CREATED ones
/// (`DistributorSlots::apply_generation`, `src/state.rs`), so ANY same-generation
/// create-then-spend of one slot is refused as `RewardsError::Malformed` by that ordering alone,
/// at any magnitude, with or without B2. A silently loosened B2 still passed it, so the assertion
/// was only ever about which error came back, never about accept-versus-refuse.
///
/// The same trap catches a second commit chained onto the first one's pending reward slot --
/// measured, not assumed: that shape fails with "a generation spends a reward slot this walk never
/// saw created". `AddEntry` avoids it structurally: it CREATES an entry slot and spends none, so
/// the only slot this generation spends is the first epoch's reward slot, which predates the
/// generation. The bookkeeping backstop cannot fire, and the accept/refuse decision belongs to B2
/// alone. Commenting out B2's loop in `src/state.rs` turns this test red with `Ok(Some(..))` -- a
/// reader that HANDED ON the unrepresentable commitment as authenticated state -- rather than
/// merely changing which error is returned. Verified by doing exactly that.
///
/// The same-generation commit-and-withdraw-of-the-same-slot composition is still built, for the
/// separate premise that upstream accepts it and keeps the created slot in
/// `created_commitment_slots`, by
/// `a_same_generation_commit_and_withdraw_above_the_driver_bound_is_refused_before_from_spend`
/// below.
///
/// `chia-sdk-driver` 0.36.0's action layer batches any number of actions into one distributor-coin
/// spend (`reward_distributor.rs:699-747`), and this crate's own
/// `commit_incentives_for_distributor_epoch` and `add_entry` both leave `finish_spend` to the
/// caller, so the composition below is directly constructible with this repo's own public API.
///
/// Debug-safe on purpose: `committed_base_units` sits just above the READ bound
/// (`MAX_REPORTABLE_COMMITMENT_BASE_UNITS`, `u64::MAX / 10_000`) but stays under the DRIVER's own
/// overflow bound (`u64::MAX / WITHDRAWAL_SHARE_BPS`, i.e. `/ 9_000`), so nothing in this fixture
/// panics without `--release` -- proving the generic B2 bound catches a commitment DIG's own
/// 9_000 bps table could still have paid out, not merely one already broken by the driver's own
/// overflow.
#[test]
fn a_commitment_above_the_bound_batched_with_another_action_is_refused_by_the_reader(
) -> anyhow::Result<()> {
    // One base unit above the read bound; derived, never spelled, so a mutation of
    // MAX_REPORTABLE_COMMITMENT_BASE_UNITS in src/ cannot survive unnoticed here.
    let committed_base_units = MAX_REPORTABLE_COMMITMENT_BASE_UNITS + 1;
    let headroom = 1_000;

    let ctx = &mut SpendContext::new();
    let mut harness =
        launch_harness_with(ctx, committed_base_units + headroom, WITHDRAWAL_SHARE_BPS)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;
    let mut members = vec![launcher_id, harness.distributor.coin.coin_id()];
    let mut extras = vec![
        harness.distributor.reserve.coin.coin_id(),
        harness.distributor.reserve.coin.parent_coin_info,
    ];

    let second_epoch_start = FIRST_EPOCH_START + TEST_EPOCH_SECONDS;

    // Action 1 of the batch: the commitment above the read bound. Deliberately NOT finished --
    // action 2 must land in the SAME pending spend.
    let commit_conditions = commit_incentives_for_distributor_epoch(
        ctx,
        &mut harness.distributor,
        harness.first_epoch_slot.clone(),
        second_epoch_start,
        harness.funder.puzzle_hash,
        committed_base_units,
    )?;

    // Action 2: an entry-set write, in the SAME pending spend. It creates an entry slot and
    // spends none, which is exactly what keeps the slot-bookkeeping backstop out of the case
    // under test.
    let authority = ManagerAuthority::new(harness.manager.inner_puzzle_hash)?;
    let write_time = last_update(&harness.distributor);
    let write = add_entry(
        ctx,
        &mut harness.distributor,
        authority,
        verdict_for(harness.entry.puzzle_hash),
        write_time,
    )?;

    assert_eq!(
        harness.distributor.pending_spend.logs.len(),
        2,
        "the fixture only tests batching if BOTH actions landed in one pending spend"
    );
    assert!(
        harness
            .distributor
            .pending_spend
            .spent_commitment_slots
            .is_empty()
            && harness
                .distributor
                .pending_spend
                .spent_entry_slots
                .is_empty(),
        "this generation must spend no slot it also creates, or the slot-bookkeeping backstop -- \
         not B2 -- would be what refuses this read"
    );

    let hint = ctx.hint(harness.funder.puzzle_hash)?;
    let change = harness.source_cat.coin.amount - committed_base_units;
    let source_cat_spend = CatSpend::new(
        harness.source_cat,
        StandardLayer::new(harness.funder.pk).spend_with_conditions(
            ctx,
            commit_conditions.create_coin(harness.funder.puzzle_hash, change, hint),
        )?,
    );
    harness.source_cat = harness.source_cat.child(harness.funder.puzzle_hash, change);

    harness.distributor = harness
        .distributor
        .clone()
        .finish_spend(ctx, vec![source_cat_spend])?
        .0;
    ensure_optional_conditions_met(ctx, &mut harness.sim, write.sync_conditions)?;
    let (next_manager_coin, next_manager_proof) =
        spend_manager_singleton(ctx, &harness.manager, write.manager_conditions)?;
    harness
        .sim
        .spend_coins(ctx.take(), std::slice::from_ref(&harness.funder.sk))?;
    harness.manager.coin = next_manager_coin;
    harness.manager.proof = next_manager_proof;
    members.push(harness.distributor.coin.coin_id());
    extras.push(harness.distributor.reserve.coin.coin_id());

    let chain = mock_chain_source(&harness.sim, launcher_id, &members, &extras);

    match read_distributor(&chain, launcher_id) {
        Err(RewardsError::CommitmentRewardsTooLargeToRead {
            rewards_base_units,
            max_readable_base_units,
        }) => {
            assert_eq!(
                max_readable_base_units, MAX_REPORTABLE_COMMITMENT_BASE_UNITS,
                "the refusal must name the derived bound"
            );
            assert_eq!(
                rewards_base_units, committed_base_units,
                "the refusal must name the commitment slot's own recorded rewards, read off the \
                 generation that created it even though another action shares that generation"
            );
        }
        Ok(Some(snapshot)) => panic!(
            "B2 let an out-of-bounds commitment through when it shared a generation with a \
             second action: {:?}",
            snapshot.rewards_per_distributor_epoch()
        ),
        Ok(None) => panic!("Ok(None) is forbidden here (SPEC.md 0.1 clause 5d)"),
        Err(other) => panic!("expected CommitmentRewardsTooLargeToRead, got: {other}"),
    }

    Ok(())
}

/// Discriminating regression for dig_ecosystem#3313's remedy: a one-spend commit+withdraw
/// composition whose `committed_value` is above the DRIVER's OWN overflow bound
/// (`u64::MAX / WITHDRAWAL_SHARE_BPS`) -- not merely the READ bound
/// (`MAX_REPORTABLE_COMMITMENT_BASE_UNITS`) that
/// `a_commitment_above_the_bound_batched_with_another_action_is_refused_by_the_reader`
/// above already proves B2 catches. At THIS scale, `chia-sdk-driver` 0.36.0's own
/// `committed_value * withdrawal_share_bps` multiply (`withdraw_incentives.rs:71`) overflows
/// INSIDE `RewardDistributor::from_spend`, before B2 -- which only runs once `from_spend`
/// RETURNS -- ever gets a chance to refuse. The fail-closed pre-screen
/// (`refuse_unrepresentable_action_arithmetic`) must catch it first, from the action's own
/// solution fields alone, without ever calling `from_spend` on this generation.
///
/// **Release-only, by necessity** (same reason as `a_commitment_above_the_driver_bound_is_refused_by_the_reader`
/// above): building this fixture calls upstream's withdraw action `.spend()` directly, which
/// performs the same unchecked multiply while constructing the spend -- panicking under
/// `debug_assertions` before the coin ever reaches simulated chain state to read back.
///
/// Before the pre-screen (this PR), this generation would have reached `from_spend`, and the
/// wrapped multiply would have flowed through B2 unguarded (B2 only checks the recorded
/// commitment `rewards`, never re-derives the withdraw share itself).
#[cfg(not(debug_assertions))]
#[test]
fn a_same_generation_commit_and_withdraw_above_the_driver_bound_is_refused_before_from_spend(
) -> anyhow::Result<()> {
    // One base unit above the driver's own overflow bound; derived, never spelled.
    let committed_base_units = u64::MAX / WITHDRAWAL_SHARE_BPS + 1;
    let headroom = 1_000;

    let ctx = &mut SpendContext::new();
    let mut harness =
        launch_harness_with(ctx, committed_base_units + headroom, WITHDRAWAL_SHARE_BPS)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;
    let mut members = vec![launcher_id, harness.distributor.coin.coin_id()];
    let mut extras = vec![
        harness.distributor.reserve.coin.coin_id(),
        harness.distributor.reserve.coin.parent_coin_info,
    ];

    let second_epoch_start = FIRST_EPOCH_START + TEST_EPOCH_SECONDS;

    // CommitIncentives -- above the driver's own overflow bound -- deliberately NOT finished yet:
    // the withdraw below must land in the SAME pending spend.
    let secure_conditions = commit_incentives_for_distributor_epoch(
        ctx,
        &mut harness.distributor,
        harness.first_epoch_slot.clone(),
        second_epoch_start,
        harness.funder.puzzle_hash,
        committed_base_units,
    )?;

    let hint = ctx.hint(harness.funder.puzzle_hash)?;
    let change = harness.source_cat.coin.amount - committed_base_units;
    let source_cat_spend = CatSpend::new(
        harness.source_cat,
        StandardLayer::new(harness.funder.pk).spend_with_conditions(
            ctx,
            secure_conditions.create_coin(harness.funder.puzzle_hash, change, hint),
        )?,
    );
    harness.source_cat = harness.source_cat.child(harness.funder.puzzle_hash, change);

    let reward_slots: Vec<Slot<RewardDistributorRewardSlotValue>> = harness
        .distributor
        .pending_spend
        .created_reward_slots
        .iter()
        .map(|value| {
            harness
                .distributor
                .created_slot_value_to_slot(*value, RewardDistributorSlotNonce::REWARD)
        })
        .collect();
    let reward_slot = pick_reward_slot(&reward_slots, second_epoch_start);

    let commitment_slot: Slot<RewardDistributorCommitmentSlotValue> = harness
        .distributor
        .pending_spend
        .created_commitment_slots
        .first()
        .copied()
        .map(|value| {
            harness
                .distributor
                .created_slot_value_to_slot(value, RewardDistributorSlotNonce::COMMITMENT)
        })
        .expect("the commit above created a commitment slot");

    // WithdrawIncentives of that SAME just-created slot, built directly against the raw driver
    // action -- deliberately bypassing this crate's own pre-guard, because the question here is
    // what the PRE-SCREEN does with a spend already on chain, not whether
    // `withdraw_committed_incentives`'s own entry-point guard would have refused first.
    let mut distributor = harness.distributor.clone();
    let (withdraw_conditions, _driver_reported) = distributor
        .new_action::<chia_sdk_driver::RewardDistributorWithdrawIncentivesAction>()
        .spend(ctx, &mut distributor, commitment_slot, reward_slot)?;
    harness.distributor = distributor;

    let authority_coin = harness.sim.new_coin(harness.funder.puzzle_hash, 1);
    StandardLayer::new(harness.funder.pk).spend(ctx, authority_coin, withdraw_conditions)?;

    harness.distributor = harness
        .distributor
        .clone()
        .finish_spend(ctx, vec![source_cat_spend])?
        .0;
    harness
        .sim
        .spend_coins(ctx.take(), std::slice::from_ref(&harness.funder.sk))?;
    members.push(harness.distributor.coin.coin_id());
    extras.push(harness.distributor.reserve.coin.coin_id());

    let chain = mock_chain_source(&harness.sim, launcher_id, &members, &extras);

    match read_distributor(&chain, launcher_id) {
        Err(RewardsError::ActionArithmeticNotRepresentable { action, operation }) => {
            assert_eq!(
                action, "withdraw_incentives",
                "the multiply that overflows here belongs to the withdraw action, not the commit"
            );
            assert_eq!(
                operation, "committed_value * withdrawal_share_bps",
                "this composition overflows the multiply itself, before any subtraction runs"
            );
        }
        Ok(Some(snapshot)) => panic!(
            "the pre-screen let a generation through whose withdraw multiply overflows the \
             driver's own u64 bound: {:?}",
            snapshot.rewards_per_distributor_epoch()
        ),
        Ok(None) => panic!("Ok(None) is forbidden here (SPEC.md 0.1 clause 5d)"),
        Err(other) => {
            panic!("expected ActionArithmeticNotRepresentable from the pre-screen, got: {other}")
        }
    }

    Ok(())
}

/// Path A (PR #8 review at `state.rs:433`): `withdraw_committed_incentives`'s pre-guard must judge
/// the value upstream will ACTUALLY multiply -- the commitment slot `actual_commitment_slot_value`
/// substitutes in, matching on `epoch_start` ALONE (`reward_distributor.rs:833-849`) -- never the
/// stale slot the caller happened to pass in.
///
/// Constructed exactly as the review described: a small, real, ALREADY-ON-CHAIN commitment to
/// `second_epoch_start` is clawed back in the SAME generation that commits a SECOND, much larger
/// amount to that same `epoch_start` -- the caller passes the stale, small, safe slot, and the
/// guard must still refuse on the large, substituted one.
///
/// The revert probe for this test is recorded on its own doc line at the bottom rather than run
/// automatically: reverting `src/clawback.rs`'s `distributor.actual_commitment_slot_value(...)`
/// call to a bare `commitment_slot` makes the pre-guard see the caller's stale, SAFE small figure
/// and let the withdraw through -- at which point the driver's OWN internal resolution (
/// `withdraw_incentives.rs:103`, unconditional) still substitutes the large value and the same
/// multiply this crate's guard exists to pre-empt now runs unguarded, panicking under
/// `debug_assertions` instead of returning a controlled `Err`. That is what "must go red" means
/// here: not a different Err, but the test failing to reach its `assert_eq!` at all.
#[test]
fn withdraw_guards_the_substituted_commitment_not_the_callers_stale_one() -> anyhow::Result<()> {
    const SMALL_COMMITMENT: u64 = 1_000;
    // One base unit above the driver's own overflow bound, so the guard's `checked_mul` is what
    // catches it -- derived, never spelled.
    let large_commitment = u64::MAX / WITHDRAWAL_SHARE_BPS + 1;
    let headroom = 1_000;

    let ctx = &mut SpendContext::new();
    let mut harness = launch_harness_with(
        ctx,
        large_commitment + SMALL_COMMITMENT + headroom,
        WITHDRAWAL_SHARE_BPS,
    )?;

    let second_epoch_start = FIRST_EPOCH_START + TEST_EPOCH_SECONDS;

    // A small, real commitment to `second_epoch_start`, finished onto chain in its OWN generation
    // -- this is the stale slot the caller below will pass to the guard.
    let first_epoch_slot = harness.first_epoch_slot.clone();
    let reward_slots = commit_to_epoch(
        ctx,
        &mut harness,
        first_epoch_slot,
        second_epoch_start,
        SMALL_COMMITMENT,
    )?;
    let stale_commitment_slot = harness
        .last_commitment_slot
        .clone()
        .expect("the small commit created a commitment slot");
    let reward_slot_for_second_epoch = pick_reward_slot(&reward_slots, second_epoch_start);

    // A second, much larger commitment to the SAME epoch_start, in a NEW generation deliberately
    // left open: the withdraw below must land in this same pending spend.
    let secure_conditions = commit_incentives_for_distributor_epoch(
        ctx,
        &mut harness.distributor,
        reward_slot_for_second_epoch,
        second_epoch_start,
        harness.funder.puzzle_hash,
        large_commitment,
    )?;
    let hint = ctx.hint(harness.funder.puzzle_hash)?;
    let change = harness.source_cat.coin.amount - large_commitment;
    let _source_cat_spend = CatSpend::new(
        harness.source_cat,
        StandardLayer::new(harness.funder.pk).spend_with_conditions(
            ctx,
            secure_conditions.create_coin(harness.funder.puzzle_hash, change, hint),
        )?,
    );
    harness.source_cat = harness.source_cat.child(harness.funder.puzzle_hash, change);

    let reward_slot_after_second_commit = harness
        .distributor
        .pending_spend
        .created_reward_slots
        .iter()
        .find(|value| value.epoch_start == second_epoch_start)
        .copied()
        .map(|value| {
            harness
                .distributor
                .created_slot_value_to_slot(value, RewardDistributorSlotNonce::REWARD)
        })
        .expect("the second commit re-created a reward slot covering second_epoch_start");

    // The caller passes the STALE, small, already-on-chain slot -- `epoch_start` is all
    // `actual_commitment_slot_value` looks at when it substitutes.
    let clawback = withdraw_committed_incentives(
        ctx,
        &mut harness.distributor,
        stale_commitment_slot,
        reward_slot_after_second_commit,
        harness.funder.puzzle_hash,
    );

    match clawback {
        Err(RewardsError::DriverShareNotRepresentable {
            rewards_base_units,
            withdrawal_share_bps,
        }) => {
            assert_eq!(
                rewards_base_units, large_commitment,
                "the guard must judge the SUBSTITUTED commitment upstream will actually multiply \
                 ({large_commitment}), not the caller's stale slot ({SMALL_COMMITMENT})"
            );
            assert_eq!(withdrawal_share_bps, WITHDRAWAL_SHARE_BPS);
        }
        Ok(paid) => panic!(
            "the guard used the caller's stale, safe slot instead of the substituted one: \
             {paid:?}"
        ),
        Err(other) => panic!("expected DriverShareNotRepresentable, got: {other}"),
    }

    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The backfill budget (SPEC.md 0.1 clause 5d) is a READ-WIDE accumulator, hoisted into
// `read_distributor` above its walk loop rather than declared inside the per-generation
// pre-screen. The discriminator: a two-generation history whose first generation genuinely
// backfills 3 epochs, and whose second DECLARES a backfill far past the whole budget. Hoisted,
// the second generation's refusal reports `already_committed == 3` (gen 1's real spend carried
// into gen 2's refusal); un-hoisted, the accumulator resets per generation and reports 0.
// ---------------------------------------------------------------------------------------------

/// Rewrites a real, recorded `commit_incentives` `CoinSpend`'s solution to declare
/// `forged_epoch_start` instead of whatever epoch it was really built with -- leaving the coin,
/// the puzzle reveal, the lineage proof and the Merkle proof exactly as the real spend built
/// them. Only the one field `refuse_unrepresentable_action_arithmetic` reads is forged, which is
/// what makes a refusal here evidence about the PRE-SCREEN, not about a hand-built fixture: the
/// spend genuinely came out of the driver, and every other field genuinely authenticates.
///
/// Never used to build a spend whose declared backfill is real (that is exactly the closed
/// approach: a real spend backfilling anywhere near this crate's budget runs into
/// `chia-sdk-driver`'s own `CostExceeded` long before it is useful as a test fixture).
fn commit_incentives_spend_with_forged_epoch_start(
    ctx: &mut SpendContext,
    real_spend: &chia_protocol::CoinSpend,
    forged_epoch_start: u64,
) -> chia_protocol::CoinSpend {
    let solution_ptr = ctx
        .alloc(&real_spend.solution)
        .expect("a real recorded solution always allocates");
    let singleton_solution = ctx
        .extract::<SingletonSolution<NodePtr>>(solution_ptr)
        .expect("a real singleton spend's solution always parses as SingletonSolution");
    let action_layer_solution = ActionLayer::<RewardDistributorState, HashedPtr>::parse_solution(
        ctx,
        singleton_solution.inner_solution,
    )
    .expect("a real action-layer solution always parses");

    assert_eq!(
        action_layer_solution.action_spends.len(),
        1,
        "this fixture only ever forges a single-action generation"
    );
    let action_spend = &action_layer_solution.action_spends[0];
    let mut params = ctx
        .extract::<RewardDistributorCommitIncentivesActionSolution>(action_spend.solution)
        .expect("this fixture only ever forges a commit_incentives generation");
    params.epoch_start = forged_epoch_start;
    let forged_action_solution = ctx
        .alloc(&params)
        .expect("the forged commit_incentives solution always allocates");

    let raw_action_layer_solution = RawActionLayerSolution {
        puzzles: vec![action_spend.puzzle],
        selectors_and_proofs: vec![(2, Some(action_layer_solution.proofs[0].clone()))],
        solutions: vec![forged_action_solution],
        finalizer_solution: action_layer_solution.finalizer_solution,
    };
    let forged_inner_solution = ctx
        .alloc(&raw_action_layer_solution)
        .expect("the rebuilt action-layer solution always allocates");

    let forged_singleton_solution = SingletonSolution {
        lineage_proof: singleton_solution.lineage_proof,
        amount: singleton_solution.amount,
        inner_solution: forged_inner_solution,
    };
    let forged_solution = ctx
        .serialize(&forged_singleton_solution)
        .expect("the rebuilt singleton solution always serializes");

    chia_protocol::CoinSpend::new(
        real_spend.coin,
        real_spend.puzzle_reveal.clone(),
        forged_solution,
    )
}

/// Discriminating regression for the backfill budget's FRAME (SPEC.md 0.1 clause 5d, rule (1)):
/// the accumulator MUST live in `read_distributor`, one frame above its walk loop, never inside
/// `refuse_unrepresentable_action_arithmetic` itself -- because the reward slots a backfill
/// creates are retained in `DistributorSlots::rewards` for the WHOLE walk, so a per-generation
/// accumulator bounds nothing (an attacker mines N cheap generations and multiplies the reader's
/// retained memory by N).
///
/// Gen 1 is a genuine, cheap, real `commit_incentives` spend backfilling 3 epochs -- real puzzle,
/// real `from_spend`, milliseconds. Gen 2 is a second genuine, cheap real spend whose solution's
/// `epoch_start` is then rewritten (see `commit_incentives_spend_with_forged_epoch_start`) to
/// declare a backfill of `MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS + 1` -- one past the WHOLE budget,
/// not merely past what gen 1 already spent, so an un-hoisted (per-generation) accumulator would
/// still refuse it on the count alone. What discriminates hoisted from un-hoisted is the
/// `already_committed` figure the refusal carries: hoisted, it is `3` (gen 1's real backfill,
/// carried forward); un-hoisted, it is `0` (gen 2 read as if it were the read's first
/// backfilling action).
///
/// This is deliberately debug-profile-safe: gen 2's declared backfill is refused at the
/// pre-screen, `refuse_unrepresentable_action_arithmetic`, which runs on the action's solution
/// fields alone and never calls `from_spend` -- and never runs the real backfill loop -- on the
/// generation it refuses.
///
/// **This test's validity depends on the pre-screen running BEFORE `from_spend` on every
/// generation** (`src/state.rs`: `refuse_unrepresentable_action_arithmetic`'s call site inside
/// `read_distributor`'s walk loop precedes `RewardDistributor::from_spend`'s call there). If that
/// ordering is ever reversed, gen 2's forged, giant declared backfill would instead reach the
/// driver's own loop, and this test HANGS (or times out under `CostExceeded`) rather than failing
/// cleanly -- it does not fail loudly on its own.
#[test]
fn a_backfill_forged_past_the_whole_budget_is_refused_with_the_earlier_generations_count(
) -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let mut harness = launch_harness(ctx)?;
    let launcher_id = harness.distributor.info.constants.launcher_id;

    let mut members = vec![launcher_id, harness.distributor.coin.coin_id()];
    let reserve_launch_id = harness.distributor.reserve.coin.coin_id();
    let reserve_parent_id = harness.distributor.reserve.coin.parent_coin_info;

    // Gen 1: a genuine backfill of 3 epochs. `commit_to_epoch`'s `epoch_start` is 4 epochs past
    // `first_epoch_slot`'s own `epoch_start` (an adjacent commit would be the `slot_epoch_time ==
    // epoch_start` branch, which never backfills at all), so the real gap is 3 epochs --
    // `already_committed` after this generation must be exactly 3.
    let gen1_epoch_start = FIRST_EPOCH_START + 4 * TEST_EPOCH_SECONDS;
    let first_epoch_slot = harness.first_epoch_slot.clone();
    let reward_slots_after_gen1 = commit_to_epoch(
        ctx,
        &mut harness,
        first_epoch_slot,
        gen1_epoch_start,
        COMMITTED_BASE_UNITS,
    )?;
    members.push(harness.distributor.coin.coin_id());
    let gen2_coin_id = harness.distributor.coin.coin_id();

    let reserve_tip_id = harness.distributor.reserve.coin.coin_id();

    // Gen 2's raw material: a second genuine, CHEAP real commit (a one-epoch gap) spending the
    // gen-1 tip coin. Its only purpose is to produce a well-formed `commit_incentives` CoinSpend
    // for that coin -- real puzzle reveal, real lineage proof, real Merkle proof -- whose
    // solution is then forged below. Building it with a real huge gap instead is the closed
    // approach: `chia-sdk-driver` hits `CostExceeded` long before N reaches this crate's budget.
    let gen2_reward_slot = pick_reward_slot(&reward_slots_after_gen1, gen1_epoch_start);
    let gen2_slot_epoch_time = gen2_reward_slot.info.value.epoch_start;
    commit_to_epoch(
        ctx,
        &mut harness,
        gen2_reward_slot,
        gen1_epoch_start + 2 * TEST_EPOCH_SECONDS,
        COMMITTED_BASE_UNITS,
    )?;

    let real_gen2_spend = harness
        .sim
        .coin_spend(gen2_coin_id)
        .expect("gen 2's real spend was just recorded by the simulator");

    // One past the WHOLE budget -- never `MAX - 1`, which an un-hoisted (per-generation)
    // accumulator would still admit (0 already committed + `MAX - 1` <= `MAX`), passing for the
    // wrong reason and, worse, proceeding into the real backfill loop and its 290s `CostExceeded`
    // wall. `start_epoch_time` mirrors `refuse_unrepresentable_action_arithmetic`
    // (`slot_epoch_time + epoch_seconds`) so the forged gap divides evenly by `epoch_seconds`,
    // matching upstream's own ceiling-of-gap-over-step arithmetic exactly.
    let start_epoch_time = gen2_slot_epoch_time + TEST_EPOCH_SECONDS;
    let forged_iterations = MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS + 1;
    let forged_epoch_start = start_epoch_time + forged_iterations * TEST_EPOCH_SECONDS;

    let forged_gen2_spend =
        commit_incentives_spend_with_forged_epoch_start(ctx, &real_gen2_spend, forged_epoch_start);

    let chain = mock_chain_source(
        &harness.sim,
        launcher_id,
        &members,
        &[reserve_launch_id, reserve_parent_id, reserve_tip_id],
    )
    .with_spend(gen2_coin_id, forged_gen2_spend);

    match read_distributor(&chain, launcher_id) {
        Err(RewardsError::CommitIncentivesBackfillBoundExceeded {
            iterations,
            already_committed,
            max_backfill_slots,
        }) => {
            assert_eq!(
                already_committed, 3,
                "the accumulator must be HOISTED above the walk loop: gen 1's real 3-epoch \
                 backfill must be carried into gen 2's refusal. An un-hoisted (per-generation) \
                 accumulator reports 0 here instead -- the discriminator this test exists for"
            );
            assert_eq!(
                max_backfill_slots, MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS,
                "the refusal must name the fixed, named budget, not a re-derived value"
            );
            assert_eq!(
                iterations, forged_iterations,
                "the refusal must name the real iteration count the forged declaration implies"
            );
        }
        Ok(Some(snapshot)) => panic!(
            "the reader accepted a generation whose declared backfill is forged past the whole \
             budget: {:?}",
            snapshot.rewards_per_distributor_epoch()
        ),
        Ok(None) => panic!("Ok(None) is forbidden here (SPEC.md 0.1 clause 5d)"),
        Err(other) => panic!("expected CommitIncentivesBackfillBoundExceeded, got: {other}"),
    }

    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Manager-singleton launch (§7.2a) and on-chain discovery (§13.1) -- both driven end to end
// through the real simulator, so a decode here is a decode of the actual CLVM output rather than
// a hand-built `CoinSpend`.
// ---------------------------------------------------------------------------------------------

/// Launch a *real* manager singleton via [`launch_manager_singleton`] (§7.2a) and, in the SAME
/// spend bundle, a DIG distributor naming it (§13.1's launch comment lives in the distributor's
/// own launcher-creating spend, never the manager's). Returns the manager's derived launcher id,
/// the distributor's launcher id, the generation the launch advertised, and every `CoinSpend` the
/// bundle produced -- captured before `Simulator::spend_coins` consumes them, so a caller can
/// decode the bundle exactly as a chain reader would see it and still finalise it afterwards.
#[allow(clippy::type_complexity)]
fn launch_manager_and_distributor_in_one_bundle(
    ctx: &mut SpendContext,
) -> anyhow::Result<(
    Simulator,
    Bytes32,
    Bytes32,
    LaunchComment,
    Vec<chia_protocol::CoinSpend>,
)> {
    let mut sim = Simulator::new();

    // §7.2a: the manager singleton, launched for real -- never `sim.new_coin`'s synthetic coin,
    // which would leave no parent spend for discovery to decode.
    let manager_parent = sim.bls(1);
    let manager_p2 = StandardLayer::new(manager_parent.pk);
    let launched_manager = launch_manager_singleton(
        ctx,
        manager_parent.coin.coin_id(),
        ManagerInnerPuzzle::HashSuppliedByCaller(Bytes32::new([0x42; 32])),
    )?;
    manager_p2.spend(
        ctx,
        manager_parent.coin,
        launched_manager.parent_conditions().clone(),
    )?;

    // Mint the reward CAT and build the launch offer, exactly as `launch_harness` does.
    let funder = sim.bls(MINTED_BASE_UNITS);
    let funder_p2 = StandardLayer::new(funder.pk);
    let (issue_cat, source_cats) = Cat::single_issuance(
        ctx,
        funder.coin.coin_id(),
        None,
        MINTED_BASE_UNITS,
        Conditions::new().create_coin(funder.puzzle_hash, MINTED_BASE_UNITS, Memos::None),
    )?;
    funder_p2.spend(ctx, funder.coin, issue_cat)?;
    let source_cat = source_cats[0];

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
        launched_manager.launcher_id(),
        funder.puzzle_hash,
        source_cat.info.asset_id,
    );
    let generation = LaunchComment::new(Bytes32::new([0x11; 32]), Bytes32::new([0x22; 32]));
    let launched = launch_dig_distributor(
        ctx,
        &offer,
        FIRST_EPOCH_START,
        constants,
        &TESTNET11_CONSTANTS,
        generation,
        0,
    )?;

    let distributor_launcher_id = launched.distributor.info.constants.launcher_id;

    // Captured before `spend_coins` drains `ctx` -- this is the whole bundle: the manager's
    // launch, the CAT issuance, the offer settlement, and the distributor's launch, all in one.
    let all_spends = ctx.take();

    sim.spend_coins(
        all_spends.clone(),
        &[
            manager_parent.sk.clone(),
            launcher_bls.sk.clone(),
            launched.security_coin_secret_key.clone(),
            funder.sk.clone(),
        ],
    )?;

    Ok((
        sim,
        launched_manager.launcher_id(),
        distributor_launcher_id,
        generation,
        all_spends,
    ))
}

/// §7.2a + §13.1 end to end: mint a manager singleton and a DIG distributor in one bundle, then
/// recover the distributor's generation from nothing but the bundle's own `CoinSpend`s -- the
/// same shape a chain reader decodes from.
#[test]
fn mint_end_to_end_is_recoverable_by_discovery() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let (_sim, manager_launcher_id, distributor_launcher_id, generation, all_spends) =
        launch_manager_and_distributor_in_one_bundle(ctx)?;

    // Every spend in the bundle is a candidate parent -- decode each and keep whatever a real
    // `discover_distributor` walk over `parent_spend(distributor_launcher_id)` would find.
    let mut discovered = Vec::new();
    for spend in &all_spends {
        discovered.extend(discovered_distributors_in_spend(spend)?);
    }

    assert_eq!(
        discovered.len(),
        1,
        "exactly one CREATE_COIN in the whole bundle carries a well-formed DIG rewards comment \
         -- the manager singleton's own launcher-creating CREATE_COIN has no memo at all \
         (`Launcher::new`, not `Launcher::with_memos`) and must not contribute a result"
    );
    assert_eq!(
        discovered[0].launcher_id(),
        distributor_launcher_id,
        "the decoded launcher id must be the DISTRIBUTOR's, never the manager's"
    );
    assert_ne!(
        discovered[0].launcher_id(),
        manager_launcher_id,
        "the manager singleton and the distributor are different launches with different ids"
    );
    assert_eq!(
        discovered[0].generation(),
        generation,
        "the decoded generation must be exactly what the launch advertised"
    );

    Ok(())
}

/// §13.1 clause 6: a `CREATE_COIN` to some OTHER puzzle hash, even carrying memos shaped exactly
/// like a real DIG rewards comment, must not be mistaken for a launcher creation.
#[test]
fn a_create_coin_with_dig_shaped_memos_but_the_wrong_puzzle_hash_yields_nothing(
) -> anyhow::Result<()> {
    let mut ctx = SpendContext::new();

    let hint_ptr = ctx.alloc(&"Reward Distributor v1")?;
    let hint: Bytes32 = ctx.tree_hash(hint_ptr).into();
    let generation = LaunchComment::new(Bytes32::new([0x33; 32]), Bytes32::new([0x44; 32]));
    let memos = ctx.memos(&(hint, (generation.to_string(), ())))?;

    // Not the launcher puzzle hash `Launcher::new(coin_id, amount)` would derive for this parent
    // and amount -- an ordinary puzzle hash that happens to receive well-formed memos.
    let conditions = Conditions::new().create_coin(Bytes32::new([0x99; 32]), 1, memos);
    let puzzle_ptr = clvm_quote!(conditions).to_clvm(&mut ctx)?;
    let puzzle_reveal = ctx.serialize(&puzzle_ptr)?;
    let solution = ctx.serialize(&NodePtr::NIL)?;
    let coin = Coin::new(
        Bytes32::new([0x01; 32]),
        ctx.tree_hash(puzzle_ptr).into(),
        0,
    );
    let observed = chia_protocol::CoinSpend::new(coin, puzzle_reveal, solution);

    let discoveries = discovered_distributors_in_spend(&observed)?;
    assert!(
        discoveries.is_empty(),
        "well-formed memos on the wrong puzzle hash must not be mistaken for a launcher creation"
    );

    Ok(())
}

/// §13.1 clause 7: a spend that creates TWO launchers, each with its own well-formed comment,
/// yields two distinct results -- not one, and not a merge of the two.
#[test]
fn two_launchers_in_one_spend_yield_two_distinct_results() -> anyhow::Result<()> {
    let mut ctx = SpendContext::new();

    let parent_coin_id = Bytes32::new([0x01; 32]);
    let hint_ptr = ctx.alloc(&"Reward Distributor v1")?;
    let hint: Bytes32 = ctx.tree_hash(hint_ptr).into();

    let generation_a = LaunchComment::new(Bytes32::new([0xa1; 32]), Bytes32::new([0xa2; 32]));
    let generation_b = LaunchComment::new(Bytes32::new([0xb1; 32]), Bytes32::new([0xb2; 32]));

    let launcher_a = Launcher::new(parent_coin_id, 7);
    let launcher_b = Launcher::new(parent_coin_id, 9);

    let memos_a = ctx.memos(&(hint, (generation_a.to_string(), ())))?;
    let memos_b = ctx.memos(&(hint, (generation_b.to_string(), ())))?;

    let conditions = Conditions::new()
        .create_coin(launcher_a.coin().puzzle_hash, 7, memos_a)
        .create_coin(launcher_b.coin().puzzle_hash, 9, memos_b);
    let puzzle_ptr = clvm_quote!(conditions).to_clvm(&mut ctx)?;
    let puzzle_reveal = ctx.serialize(&puzzle_ptr)?;
    let solution = ctx.serialize(&NodePtr::NIL)?;
    let coin = Coin::new(parent_coin_id, ctx.tree_hash(puzzle_ptr).into(), 0);
    let observed = chia_protocol::CoinSpend::new(coin, puzzle_reveal, solution);

    // The discovered launcher id is re-derived from the SPENT coin's own id (`observed.coin`),
    // never from `parent_coin_id` -- the spent coin, not its parent, is the launcher's actual
    // parent on chain.
    let expected_launcher_a_id = Launcher::new(coin.coin_id(), 7).coin().coin_id();
    let expected_launcher_b_id = Launcher::new(coin.coin_id(), 9).coin().coin_id();
    let _ = (launcher_a, launcher_b);

    let discoveries = discovered_distributors_in_spend(&observed)?;

    assert_eq!(
        discoveries.len(),
        2,
        "both launcher creations must contribute a result"
    );
    assert!(discoveries
        .iter()
        .any(|d| d.launcher_id() == expected_launcher_a_id && d.generation() == generation_a));
    assert!(discoveries
        .iter()
        .any(|d| d.launcher_id() == expected_launcher_b_id && d.generation() == generation_b));
    assert_ne!(
        discoveries[0].launcher_id(),
        discoveries[1].launcher_id(),
        "two distinct launches must never collapse into one result"
    );

    Ok(())
}

/// [`discover_distributor`] over a real `ChainSource`: given the distributor's launcher id alone,
/// it must recover the same generation the launch advertised -- §13.1 clause 2's whole claim,
/// exercised through the trait boundary a real caller uses rather than the pure decode directly.
#[test]
fn discover_distributor_recovers_the_generation_via_a_real_chain_source() -> anyhow::Result<()> {
    let ctx = &mut SpendContext::new();
    let (sim, _manager_launcher_id, distributor_launcher_id, generation, _all_spends) =
        launch_manager_and_distributor_in_one_bundle(ctx)?;

    // `parent_spend` resolves the launcher coin's OWN record to find its parent (the security
    // coin), then reads that parent's spend -- so both the launcher's record and the security
    // coin's spend must be loaded, not just the launcher's.
    let security_coin_id = sim
        .coin_state(distributor_launcher_id)
        .expect("the launcher coin was recorded")
        .coin
        .parent_coin_info;
    let chain = mock_chain_source(
        &sim,
        distributor_launcher_id,
        &[distributor_launcher_id],
        &[security_coin_id],
    );

    let discovered = discover_distributor(&chain, distributor_launcher_id)?
        .expect("the distributor's launch is in the chain source");

    assert_eq!(discovered.launcher_id(), distributor_launcher_id);
    assert_eq!(discovered.generation(), generation);

    Ok(())
}
