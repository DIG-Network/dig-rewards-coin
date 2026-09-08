//! `InitiatePayout` — a mirror claiming its own rewards, permissionlessly (`SPEC.md` §12.5).
//!
//! No authority argument, and none is possible: `require_payout_approval` is `false` for every DIG
//! distributor (§7.1), so a mirror's claim is never gated on an operator being alive and willing.
//!
//! Two properties are structural rather than documented:
//!
//! 1. **The payee is the slot's recorded `payout_puzzle_hash`, never a spender-chosen one.** The
//!    upstream action reads it off the slot; this module never takes a destination argument, so
//!    there is no parameter for an attacker to substitute. Anyone may *pay* an entry; nobody may
//!    redirect the payment.
//! 2. **The slot is read fresh on every claim.** `counter` is the slot's replay guard, and
//!    `InitiatePayout` writes `counter + 1` — so a cached slot value produces a spend that the
//!    chain rejects. [`initiate_payout`] therefore does not accept a slot: it accepts an
//!    [`EntrySlotSource`] and consults it inside the call (§10.2 clause 3, §12.5 clause 3).
//!
//! "No entry slot for that payout puzzle hash" is [`PayoutOutcome::EntrySlotAbsent`], a **terminal
//! non-error**: the peer is simply not in the set right now, which is the normal state for most
//! peers most of the time (§12.5 clause 1).

use chia_protocol::Bytes32;
use chia_sdk_driver::{
    RewardDistributor, RewardDistributorInitiatePayoutAction, Slot, SpendContext,
};
use chia_sdk_types::puzzles::RewardDistributorEntrySlotValue;
use chia_sdk_types::Conditions;

use crate::RewardsError;

/// Where a claim reads the entry slot from, freshly, once per claim.
///
/// Implement this over a `ChainSource`-backed reader (see [`crate::state`]). The trait exists so
/// that [`initiate_payout`] cannot be handed a slot value at all — a caller with a stale one in a
/// variable has nowhere to put it.
pub trait EntrySlotSource {
    /// Read the current entry slot for `payout_puzzle_hash`.
    ///
    /// `Ok(None)` means the entry is genuinely not in the set. An error means the question went
    /// unanswered, which MUST NOT be degraded into `None`.
    ///
    /// # Errors
    ///
    /// [`RewardsError::ChainUnavailable`] when the read could not be established.
    fn read_entry_slot(
        &self,
        payout_puzzle_hash: Bytes32,
    ) -> Result<Option<Slot<RewardDistributorEntrySlotValue>>, RewardsError>;
}

/// What a claim attempt produced.
#[must_use]
pub enum PayoutOutcome {
    /// The claim was built.
    Paid {
        /// Conditions some coin in the same bundle must assert.
        conditions: Conditions,

        /// What the puzzle paid, in **$DIG base units**, to the slot's recorded
        /// `payout_puzzle_hash`.
        ///
        /// The puzzle's own figure. This module performs no payout division (§0.1 clause 1).
        amount_base_units: u64,

        /// The replay-guard counter the slot carried when it was read.
        ///
        /// Surfaced so a caller can tell "my claim landed" from "someone else's claim landed
        /// first"; a re-read showing a higher counter means the latter.
        counter: u64,
    },

    /// There is no entry slot for that payout puzzle hash.
    ///
    /// A terminal non-error. It means the peer is not in the set right now — most often because it
    /// has not passed an evaluation yet, or because it was removed. It is not a failure and carries
    /// no accusation.
    EntrySlotAbsent,
}

/// Claim whatever the entry keyed by `payout_puzzle_hash` has accrued.
///
/// The slot is read from `source` inside this call, so every claim reads fresh state.
///
/// # Errors
///
/// - [`RewardsError::ChainUnavailable`] if `source` could not answer.
/// - [`RewardsError::Driver`] if the upstream action could not be built.
pub fn initiate_payout(
    ctx: &mut SpendContext,
    distributor: &mut RewardDistributor,
    source: &impl EntrySlotSource,
    payout_puzzle_hash: Bytes32,
) -> Result<PayoutOutcome, RewardsError> {
    let Some(entry_slot) = source.read_entry_slot(payout_puzzle_hash)? else {
        return Ok(PayoutOutcome::EntrySlotAbsent);
    };

    let counter = entry_slot.info.value.counter;

    let (conditions, amount_base_units) = distributor
        .new_action::<RewardDistributorInitiatePayoutAction>()
        .spend(ctx, distributor, entry_slot)?;

    Ok(PayoutOutcome::Paid {
        conditions,
        amount_base_units,
        counter,
    })
}

/// The payout puzzle hash an entry slot pays — read off the slot, never supplied.
#[must_use]
pub fn slot_payout_puzzle_hash(entry_slot: &Slot<RewardDistributorEntrySlotValue>) -> Bytes32 {
    entry_slot.info.value.payout_puzzle_hash
}

/// The minimum a claim must have accrued before the puzzle will pay it.
///
/// Below it, `InitiatePayout` fails; the value simply stays accrued until the next attempt. A
/// removal settles whatever has accrued regardless of this threshold ([`crate::entries`], §6.4).
#[must_use]
pub fn payout_threshold_base_units(distributor: &RewardDistributor) -> u64 {
    distributor.info.constants.payout_threshold
}
