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
//! `payout_puzzle_hash: Bytes32`, and nothing else. It comes from
//! [`crate::eligibility::judge_candidate`], which derives it from the mirror coin's lineage proof.
//! A public key, a BLS key, a peer id or an address string in that position is a defect (§10.2
//! clause 1) — and note that the distributor knows nothing about peer identity at all: every
//! statement tying a payment to a peer lives in the mirror coin.
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
/// `payout_puzzle_hash` must be the value [`crate::eligibility::judge_candidate`] returned.
/// `now_unix_seconds` is the caller's current time, from which the validity window is settled.
///
/// # Errors
///
/// - [`RewardsError::EntrySetFull`] if the distributor already holds
///   [`MAX_ENTRIES_PER_DISTRIBUTOR`] entries. A named refusal, never a silent stop (§15 clause 7).
/// - [`RewardsError::InvalidLaunchTerms`] if `payout_puzzle_hash` is the zero hash.
/// - [`RewardsError::EntrySetWriteWindowClosed`] if no `Sync` could bring the write inside its
///   window, because the distributor's epoch has ended.
/// - [`RewardsError::Driver`] if either upstream action could not be built.
pub fn add_entry(
    ctx: &mut SpendContext,
    distributor: &mut RewardDistributor,
    authority: ManagerAuthority,
    payout_puzzle_hash: Bytes32,
    now_unix_seconds: u64,
) -> Result<EntrySetWrite, RewardsError> {
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
fn sync_if_the_window_needs_it(
    ctx: &mut SpendContext,
    distributor: &mut RewardDistributor,
    now_unix_seconds: u64,
) -> Result<Option<Conditions>, RewardsError> {
    let state = distributor.pending_spend.latest_state.1;
    let last_update = state.round_time_info.last_update;
    let epoch_end = state.round_time_info.epoch_end;
    let window_closes_at =
        last_update.saturating_add(distributor.info.constants.max_seconds_offset);

    if now_unix_seconds < window_closes_at {
        return Ok(None);
    }

    // Sync as far as the caller's clock allows, but never past the epoch the puzzle is in.
    let sync_to = now_unix_seconds.min(epoch_end);

    if sync_to <= last_update {
        return Err(RewardsError::EntrySetWriteWindowClosed {
            last_update,
            epoch_end,
            now_unix_seconds,
        });
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
}
