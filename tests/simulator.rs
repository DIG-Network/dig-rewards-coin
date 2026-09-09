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
use dig_rewards_coin::clawback::{
    clawback_authority, commitment_distributor_epoch_start, withdraw_committed_incentives,
};
use dig_rewards_coin::comment::LaunchComment;
use dig_rewards_coin::constants::{
    dig_distributor_constants, DistributorLaunchTerms, ENTRY_SHARES, MAX_SECONDS_OFFSET,
    PAYOUT_THRESHOLD_BASE_UNITS, WITHDRAWAL_SHARE_BPS,
};
use dig_rewards_coin::eligibility::{
    judge_candidate, EligibilityQuestion, EligiblePayoutHash, MirrorCoinFacts,
};
use dig_rewards_coin::entries::{add_entry, remove_entry, ManagerAuthority};
use dig_rewards_coin::epoch::{
    current_distributor_epoch_end, last_update, start_next_distributor_epoch, sync_distributor,
};
use dig_rewards_coin::fund::commit_incentives_for_distributor_epoch;
use dig_rewards_coin::launch::launch_dig_distributor;
use dig_rewards_coin::payout::{initiate_payout, EntrySlotSource, PayoutOutcome};
use dig_rewards_coin::{
    read_distributor, ChainObservation, DistributorSlots, DistributorSnapshot, RewardsError,
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
    let mut sim = Simulator::new();

    // Mint the reward CAT.
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
        launched.distributor.info.constants.epoch_seconds, TEST_EPOCH_SECONDS,
        "epoch_seconds is curried from the constants table"
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

    // These accessors are exercised directly against an assembled snapshot here (rather than one
    // read_distributor (#3267) produced) precisely because they must work no matter how the
    // snapshot was built -- `observed` is a synthetic-but-well-formed value since nothing in this
    // test is about chain provenance.
    let snapshot = DistributorSnapshot {
        distributor: harness.distributor.clone(),
        slots: DistributorSlots {
            entries: vec![entry_slot_value],
            commitments: vec![],
            // Deliberately out of order, so the accessor's sort is what produces the ordering
            // asserted below rather than the order they were pushed in.
            rewards: vec![
                RewardDistributorRewardSlotValue {
                    counter: 0,
                    epoch_start: FIRST_EPOCH_START + TEST_EPOCH_SECONDS,
                    next_epoch_initialized: false,
                    rewards: 17,
                },
                harness.first_epoch_slot.info.value,
            ],
        },
        observed: ChainObservation {
            peak_height: 1,
            peak_timestamp: FIRST_EPOCH_START,
            tip_coin_id: harness.distributor.coin.coin_id(),
            last_entry_write_unix: None,
        },
    };

    assert_eq!(snapshot.entry_count(), 1);
    assert_eq!(
        snapshot.payout_puzzle_hashes(),
        vec![harness.entry.puzzle_hash],
        "the entry set is reported as payout puzzle hashes, never peer identities (§10.2)"
    );
    assert_eq!(
        snapshot.reserve_base_units(),
        harness.distributor.info.state.total_reserves,
        "the reserve figure is the puzzle's own balance, not a recomputation"
    );
    assert_eq!(
        snapshot.rewards_per_distributor_epoch(),
        vec![
            (
                harness.first_epoch_slot.info.value.epoch_start,
                harness.first_epoch_slot.info.value.rewards
            ),
            (FIRST_EPOCH_START + TEST_EPOCH_SECONDS, 17),
        ],
        "sorted by epoch start, with the figures taken from the reward slots unchanged"
    );

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

    let if_carried_forward = (first_epoch_commitment + second_epoch_commitment) / 2;
    let if_stranded = second_epoch_commitment / 2;
    println!(
        "the sole entry, added AFTER the empty epoch, claimed {amount_base_units} base units \
         at the halfway point of the second epoch. \
         Half of BOTH commitments would be {if_carried_forward}; \
         half of the SECOND alone would be {if_stranded}."
    );

    // This is SPEC.md §15 clause 9a's open money question, and this is its answer: the value
    // committed to an epoch that ran with nobody in the set is NOT stranded. It stays in
    // `remaining_rewards` untouched -- an empty epoch distributes nothing and pays nothing out --
    // and `NewEpoch` adds the next epoch's commitment on top of it, so the whole balance accrues
    // to whoever is in the set once somebody is. A funder who launches with a `first_epoch_start`
    // before its prover is ready loses nothing; the money simply waits.
    let tolerance = if_carried_forward / 100;
    assert!(
        amount_base_units.abs_diff(if_carried_forward) <= tolerance,
        "the empty epoch's value carried forward: expected about {if_carried_forward}, \
         got {amount_base_units} (stranded would have been about {if_stranded})"
    );
    assert!(
        amount_base_units > if_stranded * 2,
        "the claim is far more than the second epoch alone could account for, so the first \
         epoch's value was included"
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

    // The recorded authority succeeds, and the figure returned is the puzzle's own.
    let clawback = withdraw_committed_incentives(
        ctx,
        &mut harness.distributor,
        commitment_slot,
        reward_slot,
        harness.funder.puzzle_hash,
    )?;
    assert_eq!(
        clawback.recovered_base_units,
        COMMITTED_BASE_UNITS * WITHDRAWAL_SHARE_BPS / 10_000,
        "§7.5: the funder recovers withdrawal_share_bps of the commitment, and the rest stays \
         in the reserve"
    );
    assert!(
        clawback.recovered_base_units < COMMITTED_BASE_UNITS,
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
    let tip = *singleton_members
        .last()
        .expect("a singleton chain always has at least the launcher");

    let mut source = dig_chainsource_interface::MockChainSource::new();

    for &id in singleton_members.iter().chain(extra_coin_ids) {
        if let Some(state) = sim.coin_state(id) {
            source = source.with_coin(
                id,
                dig_chainsource_interface::CoinRecord::from_coin_state(state),
            );
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
    // every height read_distributor asks about resolves to SOME timestamp.
    for height in 0..=peak {
        source = source.with_timestamp(height, u64::from(height) * 1_000 + 1);
    }
    source.with_peak(peak)
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
        snapshot.observed.tip_coin_id,
        harness.distributor.coin.coin_id(),
        "the observed tip must be the real tip"
    );
    let zero_lineage_proof = chia_puzzle_types::LineageProof {
        parent_parent_coin_info: Bytes32::default(),
        parent_inner_puzzle_hash: Bytes32::default(),
        parent_amount: 0,
    };
    assert_ne!(
        snapshot.distributor.reserve.child_lineage_proof(),
        zero_lineage_proof,
        "REVERT-PROOF: from_parent_spend fabricates an all-zero LineageProof for the reserve; a genuine read must never produce one"
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

    assert_ne!(
        s1.observed.tip_coin_id, s2.observed.tip_coin_id,
        "a write must move the observed tip -- this is what is_current relies on"
    );
    assert!(
        !s1.is_current(&chain_s2)?,
        "S1 was read before the removal; against the post-removal chain it must be stale"
    );
    assert!(
        s2.is_current(&chain_s2)?,
        "S2 was read after the removal; against the same chain it must still be current"
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
