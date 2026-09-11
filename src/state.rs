//! The state a UI needs from a distributor, and the reader that rebuilds it from the chain.
//!
//! # A failed read is never an empty answer
//!
//! Every `ChainSource` error MUST become [`crate::RewardsError::ChainUnavailable`]. A distributor
//! whose read failed MUST NOT render as "no entries" or "nothing accrued" — those are claims about
//! money, and the honest answer is that the question went unanswered. [`Ok(None)`] is reserved for
//! exactly one fact: the launcher id was never spent, i.e. no distributor was ever launched there.
//!
//! # The landmine this reader must never touch
//!
//! `RewardDistributor::from_parent_spend` substitutes an all-zero dummy `LineageProof` for the
//! reserve. A snapshot built that way reads perfectly and is **unspendable** — the operator pays a
//! network fee for a chain rejection. This module never calls it; see [`read_distributor`]'s doc
//! for the recipe it uses instead (`SPEC.md` §12.1 clause 1, #3267).

use chia_protocol::Bytes32;
use chia_sdk_driver::{
    RewardDistributor, RewardDistributorActionLog, RewardDistributorType, SpendContext,
};
use chia_sdk_types::puzzles::{
    RewardDistributorCommitmentSlotValue, RewardDistributorEntrySlotValue,
    RewardDistributorRewardSlotValue,
};
use dig_chainsource_interface::ChainSource;

use crate::RewardsError;

/// A distributor's entry set has not changed in this long, while its reserve is non-zero, is
/// reported `EntrySetStale` to anyone reading it (`SPEC.md` §12.4).
pub const STALE_ENTRY_SET_SECONDS: u64 = 172_800;

/// Turns a `ChainSource` error into the one `RewardsError` variant a failed read may ever produce.
fn chain_unavailable<E: core::fmt::Display>(error: E) -> RewardsError {
    RewardsError::ChainUnavailable(error.to_string())
}

fn malformed(reason: impl Into<String>) -> RewardsError {
    RewardsError::Malformed(reason.into())
}

/// The slots a distributor currently has outstanding, as observed by the walk.
#[derive(Debug, Clone, Default)]
pub struct DistributorSlots {
    /// One per entry in the entry set: who gets paid, and the replay guard.
    pub entries: Vec<RewardDistributorEntrySlotValue>,

    /// One per outstanding commitment: which distributor epoch, whose clawback, how much.
    pub commitments: Vec<RewardDistributorCommitmentSlotValue>,

    /// One per distributor epoch with rewards attached.
    pub rewards: Vec<RewardDistributorRewardSlotValue>,
}

impl DistributorSlots {
    /// Applies one generation's created/spent deltas: removes exactly one matching instance of
    /// each spent slot value, then appends the created ones.
    ///
    /// Slot values are plain data (no coin id inside), so identity is by VALUE — which is exactly
    /// what the puzzle itself treats as the slot, and exactly why an entry is removed rather than
    /// decremented: there is no other handle to remove by.
    ///
    /// # Errors
    ///
    /// If a generation spends a slot this walk never saw created. That means the reader's model of
    /// the chain disagrees with the chain, and per this module's own doctrine a diverged read is an
    /// error rather than a silently smaller answer.
    fn apply_generation(
        &mut self,
        spent_entries: &[RewardDistributorEntrySlotValue],
        created_entries: &[RewardDistributorEntrySlotValue],
        spent_commitments: &[RewardDistributorCommitmentSlotValue],
        created_commitments: &[RewardDistributorCommitmentSlotValue],
        spent_rewards: &[RewardDistributorRewardSlotValue],
        created_rewards: &[RewardDistributorRewardSlotValue],
    ) -> Result<(), RewardsError> {
        remove_one_each(&mut self.entries, spent_entries, "entry")?;
        self.entries.extend_from_slice(created_entries);

        remove_one_each(&mut self.commitments, spent_commitments, "commitment")?;
        self.commitments.extend_from_slice(created_commitments);

        remove_one_each(&mut self.rewards, spent_rewards, "reward")?;
        self.rewards.extend_from_slice(created_rewards);

        Ok(())
    }
}

/// Removes, from `set`, one occurrence of each value in `spent` — never all occurrences, since two
/// outstanding slots can be equal by value.
///
/// # Errors
///
/// If a value in `spent` is not in `set`. The chain cannot spend a slot that was never created, so
/// a spend the walk cannot account for means the walk's model is WRONG — and absorbing it would
/// render a diverged read as a merely smaller one, which for a slot set is a claim about money.
fn remove_one_each<T: PartialEq + Copy>(
    set: &mut Vec<T>,
    spent: &[T],
    slot_kind: &str,
) -> Result<(), RewardsError> {
    for value in spent {
        let Some(index) = set.iter().position(|existing| existing == value) else {
            return Err(malformed(format!(
                "a generation spends a {slot_kind} slot this walk never saw created -- the \
                 reader's model of the chain disagrees with the chain"
            )));
        };
        set.remove(index);
    }

    Ok(())
}

/// The chain view a snapshot was taken against. Every field mandatory: a snapshot without
/// provenance is the stale-read hazard itself.
///
/// Read-only by construction: the fields are private, there is no public constructor, and no
/// setter. Only [`read_distributor`] can mint one, so an observation always describes a read that
/// actually happened. That is what makes the pairing inside a [`DistributorSnapshot`] unforgeable
/// — a caller cannot attach a fresh observation to stale data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainObservation {
    peak_height: u32,
    peak_timestamp: u64,
    tip_coin_id: Bytes32,
    last_entry_write_unix: Option<u64>,
}

impl ChainObservation {
    /// The fully-synced block height as of the read.
    #[must_use]
    pub fn peak_height(&self) -> u32 {
        self.peak_height
    }

    /// `block_timestamp(peak_height)` — chain-derived "now", never the wall clock.
    #[must_use]
    pub fn peak_timestamp(&self) -> u64 {
        self.peak_timestamp
    }

    /// The tip coin id AT THE TIME OF THE READ. `peak_height` alone is insufficient: two different
    /// states can share a height. This is what makes "is this the same state I read?" checkable.
    #[must_use]
    pub fn tip_coin_id(&self) -> Bytes32 {
        self.tip_coin_id
    }

    /// The Unix time of the most recent entry-set WRITE (`AddEntry` / `RemoveEntry`) this walk
    /// observed, if any (`SPEC.md` §12.4).
    ///
    /// `None` is a positive fact, not missing information: the walk covers every generation from
    /// the eve coin to the tip or it fails, so `None` means no entry-set write has ever happened
    /// since launch.
    pub fn last_entry_write_unix(&self) -> Option<u64> {
        self.last_entry_write_unix
    }
}

/// A distributor as it stands on chain, with everything a UI needs to answer "is anyone being
/// paid?".
///
/// # What this type guarantees, and what it does not
///
/// The fields are private and there is no public constructor, so the pairing of chain data with
/// the [`ChainObservation`] it was read under is **unforgeable**: only [`read_distributor`] can
/// produce a snapshot, and it always pairs the data with the observation of that same read. A
/// caller therefore cannot present stale data under a fresh observation, which is the one
/// guarantee this type needs to make [`Self::is_current`] mean anything.
///
/// It does NOT prevent a caller keeping a copy of the CONTENTS. `RewardDistributor` and
/// [`DistributorSlots`] are `Clone` upstream, so anything reachable through [`Self::distributor`]
/// or [`Self::slots`] can be copied out and held indefinitely — and once copied out it carries no
/// observation at all. `#[derive(Debug)]`-only makes the aggregate awkward to duplicate wholesale
/// and nothing more.
///
/// So: **a caller about to spend MUST re-read**, either with [`read_distributor`] or by asking
/// [`Self::is_current`], rather than trusting a snapshot it has been holding. A type documented as
/// stronger than it is would be worse than this residual.
#[derive(Debug)]
pub struct DistributorSnapshot {
    distributor: RewardDistributor,
    slots: DistributorSlots,
    observed: ChainObservation,
}

impl DistributorSnapshot {
    /// The live singleton, ready for its next action.
    ///
    /// The contents are `Clone`: see the type's own docs for why a copy taken from here must not be
    /// trusted as current.
    pub fn distributor(&self) -> &RewardDistributor {
        &self.distributor
    }

    /// The outstanding slots.
    #[must_use]
    pub fn slots(&self) -> &DistributorSlots {
        &self.slots
    }

    /// The chain view this snapshot was read against.
    #[must_use]
    pub fn observed(&self) -> &ChainObservation {
        &self.observed
    }

    /// How many entries the set holds.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.slots.entries.len()
    }

    /// Base units of the distributor's own reserve CAT, as the puzzle itself records them.
    ///
    /// **Not necessarily $DIG.** This is a generic reward-distributor reader: `reserve_asset_id`
    /// is whatever the launch curried, and nothing here checks it against
    /// `dig_constants::DIG_ASSET_ID`. A caller that needs $DIG specifically MUST compare
    /// `distributor().info.constants.reserve_asset_id` itself.
    ///
    /// A **balance**, and nothing more. It is not evidence anyone is being paid the right amount:
    /// who receives it depends on an entry set that only a live prover maintains (§0.4 clause 3).
    #[must_use]
    pub fn reserve_base_units(&self) -> u64 {
        self.distributor.info.state.total_reserves
    }

    /// Committed rewards per distributor epoch, as `(distributor_epoch_start, base_units)`, sorted
    /// by epoch start.
    ///
    /// Derived from the reward slots, which is where the puzzle keeps them; this performs no
    /// accrual arithmetic of its own.
    #[must_use]
    pub fn rewards_per_distributor_epoch(&self) -> Vec<(u64, u64)> {
        let mut totals: Vec<(u64, u64)> = self
            .slots
            .rewards
            .iter()
            .map(|slot| (slot.epoch_start, slot.rewards))
            .collect();

        totals.sort_unstable_by_key(|(epoch_start, _)| *epoch_start);
        totals
    }

    /// The payout puzzle hashes currently in the entry set.
    ///
    /// Puzzle hashes, never peer identities: the distributor knows nothing about peer identity, and
    /// every statement tying a payment to a peer lives in the mirror coin (§10.2 clause 4).
    #[must_use]
    pub fn payout_puzzle_hashes(&self) -> Vec<Bytes32> {
        self.slots
            .entries
            .iter()
            .map(|slot| slot.payout_puzzle_hash)
            .collect()
    }

    /// Whether the entry set has gone stale (`SPEC.md` §12.4): no entry-set WRITE for
    /// [`STALE_ENTRY_SET_SECONDS`] of chain time while the reserve is non-zero.
    ///
    /// Three cases, all of them chain-derived, never a wall clock:
    /// - reserve zero → `false`. Nothing is at stake, so nothing can go stale.
    /// - reserve non-zero and a write observed → stale once
    ///   `peak_timestamp - last_entry_write_unix` reaches the threshold.
    /// - reserve non-zero and NO write ever observed → `true`. This is not an absence of
    ///   information: the walk covers every generation from the eve coin to the tip or it fails,
    ///   so `None` is the positive fact "the entry set has never been written since launch", and
    ///   a funded distributor that has never had an entry set is exactly what §12.4 warns about.
    #[must_use]
    pub fn entry_set_stale(&self) -> bool {
        entry_set_is_stale(
            self.reserve_base_units(),
            self.observed.peak_timestamp,
            self.observed.last_entry_write_unix,
        )
    }

    /// Whether the entry set is frozen for this distributor's LIFE, because the authority that
    /// would write it is the zero hash (`SPEC.md` §7.2 clause 3). A frozen entry set MUST be an
    /// observable fact, not a silence.
    ///
    /// Checked for EVERY `RewardDistributorType`, not just `Managed`: each variant names the one
    /// identity entries can ever come from, and a zero there is unreachable by construction, so no
    /// entry can ever be added or removed again.
    #[must_use]
    pub fn entry_set_frozen(&self) -> bool {
        entry_set_is_frozen(self.distributor.info.constants().reward_distributor_type)
    }

    /// Re-reads the chain and reports whether this snapshot is still the current state.
    ///
    /// Compares the authenticated tip coin id ALONE, which is the question a caller about to spend
    /// actually has: *is the state I read still the state I would be spending against?* It
    /// deliberately does not compare [`ChainObservation::peak_height`] — a block arriving without
    /// spending the distributor changes the peak and changes nothing this caller depends on, and
    /// conjoining the height would make every snapshot report `false` within seconds of any
    /// mainnet read, which is a check consumers drop rather than obey.
    ///
    /// What it proves: the distributor singleton has not been spent since the read, so the entry
    /// set, the reserve and the slot set in this snapshot are the chain's current ones. What it
    /// does NOT prove: that the chain source is honest, that a spend is not already in the mempool,
    /// or anything about wall-clock freshness — read [`ChainObservation::peak_height`] and
    /// [`ChainObservation::peak_timestamp`] through [`Self::observed`] for those questions.
    ///
    /// `Ok(false)` also covers a launcher whose record has vanished from the source entirely.
    pub fn is_current(&self, source: &impl ChainSource) -> Result<bool, RewardsError> {
        let launcher_id = self.distributor.info.constants().launcher_id;
        let current = read_distributor(source, launcher_id)?;
        let Some(current) = current else {
            return Ok(false);
        };

        Ok(current.observed.tip_coin_id == self.observed.tip_coin_id)
    }
}

// `RewardDistributorInfo::constants` isn't a real accessor upstream (the field is named
// `constants` and is already public) -- this local extension exists only so the doc comments
// above can call it as a method without repeating `.info.constants` everywhere. Kept private.
trait ConstantsAccess {
    fn constants(&self) -> chia_sdk_driver::RewardDistributorConstants;
}

impl ConstantsAccess for chia_sdk_driver::RewardDistributorInfo {
    fn constants(&self) -> chia_sdk_driver::RewardDistributorConstants {
        self.constants
    }
}

/// [`DistributorSnapshot::entry_set_stale`]'s decision, over plain inputs so every branch and its
/// inversion can be tested directly.
fn entry_set_is_stale(
    reserve_base_units: u64,
    peak_timestamp: u64,
    last_entry_write_unix: Option<u64>,
) -> bool {
    if reserve_base_units == 0 {
        return false;
    }

    let Some(last_write) = last_entry_write_unix else {
        // The walk is complete or it errors, so this is "never written since launch" -- a funded
        // distributor with no entry set is the §12.4 case, not missing information.
        return true;
    };

    peak_timestamp.saturating_sub(last_write) >= STALE_ENTRY_SET_SECONDS
}

/// [`DistributorSnapshot::entry_set_frozen`]'s decision, over the type alone.
///
/// Every variant carries exactly one identity that entries can originate from. A zero there is an
/// unset field that no spend can ever satisfy, so the entry set can never change again.
fn entry_set_is_frozen(kind: RewardDistributorType) -> bool {
    let identity = match kind {
        // Only this singleton can authorize AddEntry / RemoveEntry.
        RewardDistributorType::Managed {
            manager_singleton_launcher_id,
        } => manager_singleton_launcher_id,
        // Entries come from NFTs of this collection.
        RewardDistributorType::NftCollection {
            collection_did_launcher_id,
        } => collection_did_launcher_id,
        // Entries come from the DL store this names.
        RewardDistributorType::CuratedNft {
            store_launcher_id, ..
        } => store_launcher_id,
        // Entries come from stakes of this CAT.
        RewardDistributorType::Cat { asset_id, .. } => asset_id,
    };

    identity == Bytes32::default()
}

/// Rebuilds a distributor's full state from the chain, from its launcher id alone.
///
/// `SPEC.md` §12.1 clause 1's recipe:
/// 1. `launcher_id == Bytes32::default()` — refuse; that is never a real launcher.
/// 2. [`ChainSource::coin_record`] on the launcher. `None` means no distributor was ever launched
///    there — the only case this function returns `Ok(None)` for.
/// 3. [`ChainSource::coin_spend`] on the launcher, then
///    `RewardDistributor::from_launcher_solution` for the launch constants, the initial state and
///    the eve coin.
/// 4. The eve coin's own spend, plus the eve-era reserve: the zero-amount coin at
///    `constants.reserve_full_puzzle_hash`, selected unambiguously by lowest confirmed height —
///    never picked arbitrarily.
/// 5. That candidate's `parent_spend`, parsed with `Cat::parse_children`, to recover
///    `reserve_parent_id` / `reserve_lineage_proof` and AUTHENTICATE the candidate as a genuine CAT
///    of `constants.reserve_asset_id` rather than an attacker-paid coin sitting at the same puzzle
///    hash.
/// 6. `RewardDistributor::from_eve_coin_spend` with that provenance — **never**
///    `from_parent_spend`, which fabricates a zero `LineageProof` for the reserve.
/// 7. A forward walk, generation by generation, each hop authenticated against the chain source's
///    own [`ChainSource::resolve_singleton_lineage`] membership set rather than trusted by
///    puzzle-hash equality.
/// 8. A money cross-check: the tip reserve coin must be unspent and its amount must equal
///    `state.total_reserves`.
/// 9. `peak_height` and `block_timestamp(peak_height)`, both mandatory.
pub fn read_distributor(
    source: &impl ChainSource,
    launcher_id: Bytes32,
) -> Result<Option<DistributorSnapshot>, RewardsError> {
    if launcher_id == Bytes32::default() {
        return Err(malformed("launcher id is the zero hash"));
    }

    if source
        .coin_record(launcher_id)
        .map_err(chain_unavailable)?
        .is_none()
    {
        return Ok(None);
    }

    let Some(launcher_spend) = source.coin_spend(launcher_id).map_err(chain_unavailable)? else {
        return Err(malformed(
            "launcher coin is recorded but has no recorded spend",
        ));
    };

    let mut ctx = SpendContext::new();
    let launcher_solution_ptr = ctx
        .alloc(&launcher_spend.solution)
        .map_err(RewardsError::from)?;

    let Some((constants, initial_state, eve_coin)) = RewardDistributor::from_launcher_solution(
        &mut ctx,
        launcher_spend.coin,
        launcher_solution_ptr,
    )
    .map_err(RewardsError::from)?
    else {
        return Err(malformed(
            "launcher solution is not a reward-distributor launch",
        ));
    };

    let Some(eve_spend) = source
        .coin_spend(eve_coin.coin_id())
        .map_err(chain_unavailable)?
    else {
        return Err(malformed("eve coin is recorded but has no recorded spend"));
    };

    let (reserve_parent_id, reserve_lineage_proof) =
        find_eve_reserve_provenance(&mut ctx, source, &constants)?;

    let Some((mut distributor, launch_reward_slot)) = RewardDistributor::from_eve_coin_spend(
        &mut ctx,
        constants,
        initial_state,
        &eve_spend,
        reserve_parent_id,
        reserve_lineage_proof,
    )
    .map_err(RewardsError::from)?
    else {
        return Err(malformed("eve coin spend did not produce a distributor"));
    };

    let lineage = source
        .resolve_singleton_lineage(launcher_id)
        .map_err(chain_unavailable)?
        .ok_or_else(|| malformed("launcher has no resolvable singleton lineage"))?;

    // The launch creates the first distributor epoch's reward slot, and no later generation
    // reports it as created -- so a walk that starts from the eve spend must seed it here or a
    // chain-rebuilt prover cannot roll the first epoch (an in-process launcher gets the same
    // handle as `LaunchedDistributor::first_distributor_epoch_slot`).
    let mut slots = DistributorSlots {
        rewards: vec![launch_reward_slot.info.value],
        ..DistributorSlots::default()
    };
    let mut last_entry_write_unix: Option<u64> = None;

    loop {
        if !lineage.contains(distributor.coin.coin_id()) {
            return Err(malformed(
                "a coin in the reader's own walk is not a member of the chain source's \
                 authenticated singleton lineage",
            ));
        }

        let Some(spend) = source
            .coin_spend(distributor.coin.coin_id())
            .map_err(chain_unavailable)?
        else {
            // Unspent: this generation is the tip.
            break;
        };

        let reserve_lineage_proof = distributor.reserve.child_lineage_proof();
        let Some(reconstructed) = chia_sdk_driver::RewardDistributor::from_spend(
            &mut ctx,
            &spend,
            Some(reserve_lineage_proof),
            constants,
            chia_bls::Signature::default(),
        )
        .map_err(RewardsError::from)?
        else {
            return Err(malformed(
                "a spend mid-walk did not parse as this distributor",
            ));
        };

        // §12.4 is a signal a counterparty judges the OPERATOR by, so it must count only actions
        // the operator can take. `InitiatePayout` spends an entry slot and re-creates it
        // (upstream `action_log.rs:140,171`), so deriving this from created/spent entry slots let
        // one holder claiming every 47 h report a distributor healthy forever while its prover was
        // dead. The action log distinguishes them; nothing else in the pending spend does.
        let entry_write_happened = reconstructed.pending_spend.logs.iter().any(|log| {
            matches!(
                log,
                RewardDistributorActionLog::AddEntry(_)
                    | RewardDistributorActionLog::RemoveEntry(_)
            )
        });

        slots.apply_generation(
            &reconstructed.pending_spend.spent_entry_slots,
            &reconstructed.pending_spend.created_entry_slots,
            &reconstructed.pending_spend.spent_commitment_slots,
            &reconstructed.pending_spend.created_commitment_slots,
            &reconstructed.pending_spend.spent_reward_slots,
            &reconstructed.pending_spend.created_reward_slots,
        )?;

        let next_state = reconstructed.pending_spend.latest_state.1;
        distributor = reconstructed.child(next_state);

        if entry_write_happened {
            let spent_height = source
                .coin_record(spend.coin.coin_id())
                .map_err(chain_unavailable)?
                .and_then(|record| record.spent_height);
            // Refuse rather than silently drop the write. BOTH halves of the read above can be
            // absent from a source that is not lying: `coin_record` answers `Ok(None)` for a
            // pruned generation, and `spent_height` is documented as the height "if it has been
            // spent and the source knows it" (`dig-chainsource-interface` 0.3.3,
            // `record.rs:17`). Either collapses here, and leaving `None` would assert the
            // positive fact `ChainObservation::last_entry_write_unix` documents ("no entry-set
            // write has ever happened") about a write this walk just observed. It is also
            // self-inconsistent source data: we are only in this branch because `coin_spend`
            // returned this generation's spend, so the coin is known-spent -- the same
            // contradiction step 8 above already refuses.
            let height = spent_height.ok_or_else(|| {
                malformed("an observed entry-set write's generation has no resolvable spent height")
            })?;
            // Refuse rather than fall back. `block_timestamp`'s contract makes `Ok(None)`
            // mean "no such block OR no timestamp index", which a pruning RPC answers for an
            // old height while answering the peak fine -- so this is reachable in production.
            // Keeping the previous value would report an OLDER write as the most recent one,
            // and leaving `None` would assert the positive fact `ChainObservation::
            // last_entry_write_unix` documents ("no entry-set write has ever happened") about
            // a write this walk just observed. Both silently drop it; the peak timestamp
            // below already takes this same posture.
            last_entry_write_unix = Some(
                source
                    .block_timestamp(height)
                    .map_err(chain_unavailable)?
                    .ok_or_else(|| {
                        malformed("an observed entry-set write has no resolvable chain timestamp")
                    })?,
            );
        }
    }

    let tip_coin_id = distributor.coin.coin_id();

    let tip_reserve_record = source
        .coin_record(distributor.reserve.coin.coin_id())
        .map_err(chain_unavailable)?
        .ok_or_else(|| malformed("tip reserve coin is not present on chain"))?;
    if tip_reserve_record.is_spent() {
        return Err(malformed("tip reserve coin is already spent"));
    }
    if tip_reserve_record.coin.amount != distributor.info.state.total_reserves {
        return Err(malformed(
            "tip reserve amount does not match the distributor's own total_reserves",
        ));
    }

    let peak_height = source
        .peak_height()
        .map_err(chain_unavailable)?
        .ok_or_else(|| malformed("chain source exposes no peak height"))?;
    let peak_timestamp = source
        .block_timestamp(peak_height)
        .map_err(chain_unavailable)?
        .ok_or_else(|| malformed("chain source has no timestamp for its own peak"))?;

    Ok(Some(DistributorSnapshot {
        distributor,
        slots,
        observed: ChainObservation {
            peak_height,
            peak_timestamp,
            tip_coin_id,
            last_entry_write_unix,
        },
    }))
}

/// Steps 4-5 of the recipe: selects the eve-era reserve candidate unambiguously and authenticates
/// it as a genuine CAT of `constants.reserve_asset_id`, returning the provenance
/// `from_eve_coin_spend` needs.
fn find_eve_reserve_provenance(
    ctx: &mut SpendContext,
    source: &impl ChainSource,
    constants: &chia_sdk_driver::RewardDistributorConstants,
) -> Result<(Bytes32, chia_puzzle_types::LineageProof), RewardsError> {
    let candidates = source
        .coin_records_by_puzzle_hash(constants.reserve_full_puzzle_hash, true)
        .map_err(chain_unavailable)?;

    let zero_amount: Vec<_> = candidates
        .into_iter()
        .filter(|record| record.coin.amount == 0)
        .collect();

    if zero_amount.is_empty() {
        return Err(malformed(
            "no zero-amount eve-era reserve candidate at the reserve puzzle hash",
        ));
    }

    let min_height = zero_amount
        .iter()
        .filter_map(|record| record.confirmed_height)
        .min()
        .ok_or_else(|| malformed("eve-era reserve candidate(s) have no confirmed height"))?;

    let mut at_min_height: Vec<_> = zero_amount
        .into_iter()
        .filter(|record| record.confirmed_height == Some(min_height))
        .collect();

    let candidate = match (at_min_height.pop(), at_min_height.is_empty()) {
        (Some(only), true) => only,
        _ => {
            return Err(malformed(
                "ambiguous eve-era reserve candidates at the lowest confirmed height",
            ));
        }
    };

    let candidate_coin_id = candidate.coin.coin_id();
    let Some(parent_spend) = source
        .parent_spend(candidate_coin_id)
        .map_err(chain_unavailable)?
    else {
        return Err(malformed(
            "eve-era reserve candidate's parent spend could not be resolved",
        ));
    };

    let parent_puzzle_ptr = ctx
        .alloc(&parent_spend.puzzle_reveal)
        .map_err(RewardsError::from)?;
    let parent_puzzle = chia_sdk_driver::Puzzle::parse(ctx, parent_puzzle_ptr);
    let parent_solution_ptr = ctx
        .alloc(&parent_spend.solution)
        .map_err(RewardsError::from)?;

    let children = chia_sdk_driver::Cat::parse_children(
        ctx,
        parent_spend.coin,
        parent_puzzle,
        parent_solution_ptr,
    )
    .map_err(RewardsError::from)?
    .ok_or_else(|| malformed("eve-era reserve candidate's parent spend is not a CAT spend"))?;

    let authenticated = children
        .into_iter()
        .find(|child| child.coin.coin_id() == candidate_coin_id)
        .ok_or_else(|| {
            malformed(
                "eve-era reserve candidate is not among its claimed parent's children -- \
                 not a genuine CAT of the reserve asset id",
            )
        })?;

    if authenticated.info.asset_id != constants.reserve_asset_id {
        return Err(malformed(
            "eve-era reserve candidate's authenticated asset id does not match the distributor's",
        ));
    }

    let reserve_lineage_proof = authenticated
        .lineage_proof
        .ok_or_else(|| malformed("authenticated eve-era reserve candidate has no lineage proof"))?;

    Ok((candidate.coin.parent_coin_info, reserve_lineage_proof))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A non-zero identity, so a test asserting `false` is asserting something.
    fn some_identity() -> Bytes32 {
        Bytes32::new([7; 32])
    }

    #[test]
    fn a_slot_spent_without_a_matching_creation_is_a_diverged_read() {
        let mut slots = DistributorSlots::default();
        let never_created = RewardDistributorRewardSlotValue {
            counter: 0,
            epoch_start: 1_234,
            next_epoch_initialized: false,
            rewards: 5,
        };

        let result = slots.apply_generation(&[], &[], &[], &[], &[never_created], &[]);

        match result {
            Err(RewardsError::Malformed(message)) => assert!(
                message.contains("never saw created"),
                "wrong reason: {message}"
            ),
            other => panic!("a spend the walk cannot account for must be an error, got {other:?}"),
        }
    }

    #[test]
    fn a_slot_spent_after_it_was_created_is_removed_exactly_once() {
        let value = RewardDistributorRewardSlotValue {
            counter: 0,
            epoch_start: 1_234,
            next_epoch_initialized: false,
            rewards: 5,
        };
        let mut slots = DistributorSlots::default();

        slots
            .apply_generation(&[], &[], &[], &[], &[], &[value, value])
            .expect("creations alone never diverge");
        slots
            .apply_generation(&[], &[], &[], &[], &[value], &[])
            .expect("one of the two outstanding copies is spent");

        assert_eq!(
            slots.rewards,
            vec![value],
            "two outstanding slots can be equal by value; spending one must not remove both"
        );
    }

    #[test]
    fn a_zero_reserve_is_never_stale_and_a_funded_never_written_set_always_is() {
        assert!(
            !entry_set_is_stale(0, u64::MAX, None),
            "nothing is at stake, so nothing can go stale"
        );
        assert!(
            entry_set_is_stale(1, 0, None),
            "a funded distributor whose entry set has never been written is the §12.4 case"
        );
    }

    #[test]
    fn staleness_turns_over_exactly_at_the_threshold_in_both_directions() {
        let last_write = 1_000_000;

        assert!(
            !entry_set_is_stale(
                1,
                last_write + STALE_ENTRY_SET_SECONDS - 1,
                Some(last_write)
            ),
            "one second short of the threshold is not stale"
        );
        assert!(
            entry_set_is_stale(1, last_write + STALE_ENTRY_SET_SECONDS, Some(last_write)),
            "the threshold itself is stale (§12.4 says `>=`)"
        );
    }

    #[test]
    fn a_zero_identity_freezes_the_entry_set_for_every_distributor_type() {
        let zero = Bytes32::default();

        assert!(entry_set_is_frozen(RewardDistributorType::Managed {
            manager_singleton_launcher_id: zero
        }));
        assert!(entry_set_is_frozen(RewardDistributorType::NftCollection {
            collection_did_launcher_id: zero
        }));
        assert!(entry_set_is_frozen(RewardDistributorType::CuratedNft {
            store_launcher_id: zero,
            refreshable: true
        }));
        assert!(entry_set_is_frozen(RewardDistributorType::Cat {
            asset_id: zero,
            hidden_puzzle_hash: None
        }));
    }

    #[test]
    fn a_real_identity_leaves_the_entry_set_writable_for_every_distributor_type() {
        // The inversion of the test above: flip the `==` in `entry_set_is_frozen` and one of these
        // four fails, which is what makes the guard's direction tested rather than merely present.
        assert!(!entry_set_is_frozen(RewardDistributorType::Managed {
            manager_singleton_launcher_id: some_identity()
        }));
        assert!(!entry_set_is_frozen(RewardDistributorType::NftCollection {
            collection_did_launcher_id: some_identity()
        }));
        assert!(!entry_set_is_frozen(RewardDistributorType::CuratedNft {
            store_launcher_id: some_identity(),
            refreshable: false
        }));
        assert!(!entry_set_is_frozen(RewardDistributorType::Cat {
            asset_id: some_identity(),
            hidden_puzzle_hash: None
        }));
    }
}
