//! The entry set — the only writes that need the manager singleton (`SPEC.md` §11).
//!
//! `AddEntry` and `RemoveEntry` are manager-authorized; **everything else this crate builds is
//! permissionless** ([`crate::epoch`], [`crate::payout`]). That distinction is carried by a type
//! rather than by a comment: [`ManagerAuthority`] is a required argument of exactly these two
//! builders, and no builder outside this module takes one.
//!
//! ## An entry-set write cannot be built outside its validity window
//!
//! §8.2 clause 1 is a puzzle-level fact: an entry-set write asserts
//! `ASSERT_BEFORE_SECONDS_ABSOLUTE(last_update + max_seconds_offset)`, so it is invalid unless the
//! distributor's `last_update` is fresh. Both builders here take the caller's current time and
//! settle that themselves: inside the window the write goes on its own; past the window a `Sync`
//! rides in the **same bundle** and its conditions come back beside the manager's; and when no
//! `Sync` could fix it the write is refused with
//! [`RewardsError::EntrySetWriteWindowClosed`], which names the remedy.
//!
//! A `Sync` must move the clock **strictly forward**, so emitting one unconditionally would make a
//! perfectly valid write impossible. That is why the decision is computed rather than hard-coded.
//! Either way there is no path through this module that assembles a bundle the chain will reject
//! and the operator will pay for.
//!
//! ## What an entry is keyed by
//!
//! A payout puzzle hash, and nothing else. It arrives as an [`EligiblePayoutHash`], which only
//! [`crate::eligibility::judge_candidate`] can mint from the mirror coin's lineage proof — so
//! [`add_entry`] cannot be handed a bare hash at all, and the manager authority alone does not let
//! a caller choose where DIG goes. A public key, a BLS key, a peer id or an address string in that
//! position is a defect (§10.2 clause 1) — and note that the distributor knows nothing about peer
//! identity at all: every statement tying a payment to a peer lives in the mirror coin.
//!
//! `shares` is not a caller parameter. Every mirror of one generation is worth the same, so the DIG
//! path always passes [`ENTRY_SHARES`] (§11.1, §11.3).

use chia_protocol::Bytes32;
use chia_sdk_driver::{
    RewardDistributor, RewardDistributorAddEntryAction, RewardDistributorRemoveEntryAction,
    RewardDistributorSyncAction, Slot, SpendContext,
};
use chia_sdk_types::puzzles::RewardDistributorEntrySlotValue;
use chia_sdk_types::Conditions;

use crate::constants::{ENTRY_SHARES, MAX_ENTRIES_PER_DISTRIBUTOR};
use crate::eligibility::EligiblePayoutHash;
use crate::RewardsError;

/// Proof that the caller is acting as the distributor's manager singleton.
///
/// Holding one of these does not grant anything by itself — the manager singleton still has to
/// deliver the returned conditions. Its job is to make the authority requirement visible in every
/// signature that needs it, and absent from every signature that does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagerAuthority {
    manager_singleton_inner_puzzle_hash: Bytes32,
}

impl ManagerAuthority {
    /// Name the manager singleton's inner puzzle hash.
    ///
    /// # Errors
    ///
    /// [`RewardsError::InvalidLaunchTerms`] if the hash is zero. A zero inner puzzle hash cannot
    /// authorize anything, and passing one is a sign the caller has not resolved its singleton.
    pub fn new(manager_singleton_inner_puzzle_hash: Bytes32) -> Result<Self, RewardsError> {
        if manager_singleton_inner_puzzle_hash == Bytes32::default() {
            return Err(RewardsError::InvalidLaunchTerms(
                "manager singleton inner puzzle hash must not be the zero hash".to_string(),
            ));
        }

        Ok(Self {
            manager_singleton_inner_puzzle_hash,
        })
    }

    /// The inner puzzle hash this authority carries.
    #[must_use]
    pub const fn inner_puzzle_hash(self) -> Bytes32 {
        self.manager_singleton_inner_puzzle_hash
    }
}

/// An entry-set write, ready to be delivered.
///
/// Both condition sets belong in the **same** bundle. `manager_conditions` are delivered by the
/// manager singleton's spend; `sync_conditions`, when present, must be asserted by some coin in
/// the bundle, which is what keeps `last_update` inside `max_seconds_offset` and the write valid.
#[derive(Debug)]
pub struct EntrySetWrite {
    /// Conditions the manager singleton's spend must carry.
    pub manager_conditions: Conditions,

    /// Conditions from the `Sync` that rides in the same bundle, when one was needed.
    ///
    /// `None` means the write was already inside its validity window, so no `Sync` was emitted and
    /// none is required.
    pub sync_conditions: Option<Conditions>,
}

/// A removal, which also settles what the entry had accrued.
#[derive(Debug)]
pub struct EntryRemoval {
    /// The entry-set write itself.
    pub write: EntrySetWrite,

    /// What the puzzle paid the departing entry, in **$DIG base units** — §6.4's settlement amount.
    ///
    /// This is the entry's last payment, and the `payout_threshold` is **not** applied to it: a
    /// removal settles whatever had accrued, however small. That is why there is no separate
    /// dust-flush path in this crate (§6.4 clause 1). It is the puzzle's own figure
    /// (`remove_entry.rs:95-133`), surfaced rather than discarded, because a caller that drops it
    /// cannot tell an operator what the eviction actually paid out.
    pub settled_base_units: u64,
}

/// Add one eligible mirror to the entry set.
///
/// `payout` is the verdict [`crate::eligibility::judge_candidate`] returned, and there is no other
/// way to obtain one — the hash an entry carries is bound to the eligibility decision in the type,
/// not by a docs sentence a buggy caller can miss. `now_unix_seconds` is the caller's current time,
/// from which the validity window is settled.
///
/// # Errors
///
/// - [`RewardsError::EntrySetFull`] if the distributor already holds
///   [`MAX_ENTRIES_PER_DISTRIBUTOR`] entries. A named refusal, never a silent stop (§15 clause 7).
/// - [`RewardsError::InvalidLaunchTerms`] if the verdict's payout puzzle hash is the zero hash. It
///   cannot be an arbitrary caller value, but a coin whose lineage proof yielded nothing would
///   otherwise send every DIG payment to a puzzle nobody can spend.
/// - [`RewardsError::EntrySetWriteWindowClosed`] if no `Sync` could bring the write inside its
///   window, because the distributor's epoch has ended.
/// - [`RewardsError::Driver`] if either upstream action could not be built.
pub fn add_entry(
    ctx: &mut SpendContext,
    distributor: &mut RewardDistributor,
    authority: ManagerAuthority,
    payout: EligiblePayoutHash,
    now_unix_seconds: u64,
) -> Result<EntrySetWrite, RewardsError> {
    let payout_puzzle_hash = payout.payout_puzzle_hash();
    if payout_puzzle_hash == Bytes32::default() {
        return Err(RewardsError::InvalidLaunchTerms(
            "payout puzzle hash must not be the zero hash".to_string(),
        ));
    }

    let entries_now = entry_count(distributor);
    if entries_now >= u64::from(MAX_ENTRIES_PER_DISTRIBUTOR) {
        return Err(RewardsError::EntrySetFull {
            cap: MAX_ENTRIES_PER_DISTRIBUTOR,
        });
    }

    // The sync, when one is needed, goes first so that the entry-set write it protects is built
    // against the state the sync just established.
    let sync_conditions = sync_if_the_window_needs_it(ctx, distributor, now_unix_seconds)?;

    let manager_conditions = distributor
        .new_action::<RewardDistributorAddEntryAction>()
        .spend(
            ctx,
            distributor,
            payout_puzzle_hash,
            ENTRY_SHARES,
            authority.inner_puzzle_hash(),
        )?;

    Ok(EntrySetWrite {
        manager_conditions,
        sync_conditions,
    })
}

/// Remove one entry from the set, settling what it had accrued.
///
/// Removal is not an accusation and carries no strike: it is how the set stops naming a peer that
/// no longer passes. The returned [`EntryRemoval::settled_base_units`] is §6.4's settlement amount.
///
/// # Errors
///
/// - [`RewardsError::EntrySetWriteWindowClosed`] if no `Sync` could bring the write inside its
///   window.
/// - [`RewardsError::Driver`] if either upstream action could not be built.
pub fn remove_entry(
    ctx: &mut SpendContext,
    distributor: &mut RewardDistributor,
    authority: ManagerAuthority,
    entry_slot: Slot<RewardDistributorEntrySlotValue>,
    now_unix_seconds: u64,
) -> Result<EntryRemoval, RewardsError> {
    let sync_conditions = sync_if_the_window_needs_it(ctx, distributor, now_unix_seconds)?;

    let (manager_conditions, settled_base_units) = distributor
        .new_action::<RewardDistributorRemoveEntryAction>()
        .spend(ctx, distributor, entry_slot, authority.inner_puzzle_hash())?;

    Ok(EntryRemoval {
        write: EntrySetWrite {
            manager_conditions,
            sync_conditions,
        },
        settled_base_units,
    })
}

/// How many entries the distributor currently holds.
///
/// Every DIG entry carries [`ENTRY_SHARES`] = 1 share, so `active_shares` **is** the entry count on
/// this path. The division is written out rather than assumed so that a future non-unit share
/// weight would show up here as an obviously wrong number instead of silently shifting the cap.
#[must_use]
pub fn entry_count(distributor: &RewardDistributor) -> u64 {
    distributor.pending_spend.latest_state.1.active_shares / ENTRY_SHARES
}

/// Emit a `Sync` only if the entry-set write would otherwise fall outside its validity window.
///
/// The window closes at `last_update + max_seconds_offset`, because the write asserts
/// `ASSERT_BEFORE_SECONDS_ABSOLUTE` of that moment. A `Sync` can only move `last_update` strictly
/// forward and never past the current epoch's end, so once `last_update` has reached `epoch_end`
/// there is no sync to emit and the epoch has to be rolled first.
///
/// `max_seconds_offset` is a distributor-supplied constant with no domain bound this crate
/// enforces (`read_distributor` is deliberately distributor-agnostic), so the two window
/// boundaries below are computed with `checked_add` and fail **closed** on overflow: at a
/// saturating value the write window is CLOSED, never permanently open (#3321). A `saturating_add`
/// here previously made `now_unix_seconds < window_closes_at` true for every representable clock
/// once `max_seconds_offset` saturated, which read as "the window never closes" -- exactly the
/// opposite of a fail-closed refusal, and it made
/// [`RewardsError::EntrySetWriteWindowClosed`] unreachable from either caller.
fn sync_if_the_window_needs_it(
    ctx: &mut SpendContext,
    distributor: &mut RewardDistributor,
    now_unix_seconds: u64,
) -> Result<Option<Conditions>, RewardsError> {
    let state = distributor.pending_spend.latest_state.1;
    let last_update = state.round_time_info.last_update;
    let epoch_end = state.round_time_info.epoch_end;
    let max_seconds_offset = distributor.info.constants.max_seconds_offset;

    let window_closed_err = || RewardsError::EntrySetWriteWindowClosed {
        last_update,
        epoch_end,
        now_unix_seconds,
    };

    let Some(window_closes_at) = last_update.checked_add(max_seconds_offset) else {
        return Err(window_closed_err());
    };

    if now_unix_seconds < window_closes_at {
        return Ok(None);
    }

    // Sync as far as the caller's clock allows, but never past the epoch the puzzle is in.
    let sync_to = now_unix_seconds.min(epoch_end);

    // The question is not "can the clock move?" but "does it REACH?". After the sync, the write's
    // window closes at `sync_to + max_seconds_offset`; if the caller's clock has already passed
    // that moment the write is invalid no matter what, and a `Sync` that moved the clock would only
    // buy a bundle the chain rejects at the operator's expense. This one predicate covers both
    // refusals: when `sync_to <= last_update` (the epoch has ended, so no forward sync exists) the
    // window already closed at `last_update + max_seconds_offset`, which is at or before
    // `sync_to + max_seconds_offset`, so it fires there too.
    let Some(post_sync_closes_at) = sync_to.checked_add(max_seconds_offset) else {
        return Err(window_closed_err());
    };

    if post_sync_closes_at <= now_unix_seconds {
        return Err(window_closed_err());
    }

    let conditions = distributor
        .new_action::<RewardDistributorSyncAction>()
        .spend(ctx, distributor, sync_to)?;

    Ok(Some(conditions))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manager_authority_cannot_be_the_zero_hash() {
        assert!(ManagerAuthority::new(Bytes32::default()).is_err());

        let authority = ManagerAuthority::new(Bytes32::new([5; 32])).unwrap();
        assert_eq!(authority.inner_puzzle_hash(), Bytes32::new([5; 32]));
    }

    /// A distributor's own `max_seconds_offset` has no domain bound this reader enforces, so a
    /// hostile or corrupt launch can set it to a saturating value. Before the fix,
    /// `saturating_add` made `last_update + max_seconds_offset` read as `u64::MAX`, which is
    /// greater than every representable `now_unix_seconds` -- so the write window read as
    /// PERMANENTLY OPEN instead of closed, and `RewardsError::EntrySetWriteWindowClosed` could
    /// never fire. This test fails under that defect and passes once overflow fails closed (#3321).
    #[test]
    fn a_saturating_max_seconds_offset_closes_the_window_instead_of_leaving_it_open_forever() {
        use chia_puzzle_types::{EveProof, LineageProof, Proof};
        use chia_sdk_driver::{
            Reserve, RewardDistributorConstants, RewardDistributorInfo, RewardDistributorState,
            RewardDistributorType,
        };
        use chia_protocol::Coin;

        let constants = RewardDistributorConstants::without_launcher_id(
            RewardDistributorType::Managed {
                manager_singleton_launcher_id: Bytes32::new([1; 32]),
            },
            Bytes32::new([2; 32]),
            604_800,
            u64::MAX,
            u64::MAX, // max_seconds_offset: a distributor-supplied constant, no domain bound here
            1_000,
            false,
            0,
            9_000,
            Bytes32::new([3; 32]),
        )
        .with_launcher_id(Bytes32::new([4; 32]));

        let mut state = RewardDistributorState::initial(1);
        state.round_time_info.last_update = 1;
        state.round_time_info.epoch_end = 2;

        let info = RewardDistributorInfo::new(state, constants);
        let mut distributor = RewardDistributor::new(
            Coin::new(Bytes32::default(), Bytes32::default(), 1),
            Proof::Eve(EveProof {
                parent_parent_coin_info: Bytes32::default(),
                parent_amount: 1,
            }),
            info,
            Reserve::new(
                Bytes32::default(),
                LineageProof {
                    parent_parent_coin_info: Bytes32::default(),
                    parent_inner_puzzle_hash: Bytes32::default(),
                    parent_amount: 0,
                },
                Bytes32::new([3; 32]),
                Bytes32::default(),
                0,
                0,
            ),
        );

        let mut ctx = SpendContext::new();

        let result = sync_if_the_window_needs_it(&mut ctx, &mut distributor, 100);

        assert!(
            matches!(result, Err(RewardsError::EntrySetWriteWindowClosed { .. })),
            "a saturating max_seconds_offset must close the window, not leave it open forever: \
             got {result:?}"
        );
    }
}
