//! The state a UI needs from a distributor, and the reader that is **not** in 0.2.0.
//!
//! # The chain reader is withheld from this release — see [issue #3267]
//!
//! `SPEC.md` §12.1 clause 1 specifies a `read_distributor` that walks a distributor's singleton
//! generations from its launcher id. **This release does not publish one, deliberately.** The
//! implementation that existed failed at its first hop for *every* distributor: it applied
//! `RewardDistributor::from_parent_spend` to the eve coin's spend, which carries the launch inner
//! puzzle rather than the action-layer one, so the call returned `None` and every read reported
//! `Malformed`. The correct hop is upstream's `from_eve_coin_spend`, which additionally needs the
//! reserve CAT's `reserve_parent_id` and `reserve_lineage_proof` — provenance a reader starting
//! from a launcher id cannot currently discover. That is real design work, and it is tracked at
//! [issue #3267].
//!
//! A crates.io version is immutable, so a present-but-broken public function is a worse lie than an
//! absent one: every consumer who found it in the docs would write code against a function that
//! cannot work, and the release could never be corrected in place. Re-adding a public item later is
//! purely additive, so omitting it now forecloses nothing.
//!
//! What this module does publish is the state shape itself — [`DistributorSlots`] and
//! [`DistributorSnapshot`] with its accessors — which is useful to any caller that obtained a
//! [`RewardDistributor`] by other means, including straight out of a launch.
//!
//! # A failed read is never an empty answer
//!
//! The rule the missing reader will have to honour, recorded here because it is where the next
//! reader arrives: every `ChainSource` error MUST become [`crate::RewardsError::ChainUnavailable`]. A
//! distributor whose read failed MUST NOT render as "no entries" or "nothing accrued" — those are
//! claims about money, and the honest answer is that the question went unanswered.
//!
//! [issue #3267]: https://github.com/DIG-Network/dig_ecosystem/issues/3267

use chia_protocol::Bytes32;
use chia_sdk_driver::RewardDistributor;
use chia_sdk_types::puzzles::{
    RewardDistributorCommitmentSlotValue, RewardDistributorEntrySlotValue,
    RewardDistributorRewardSlotValue,
};

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

/// A distributor as it stands on chain, with everything a UI needs to answer "is anyone being
/// paid?".
#[derive(Debug)]
pub struct DistributorSnapshot {
    /// The live singleton, ready for its next action.
    pub distributor: RewardDistributor,

    /// The outstanding slots.
    pub slots: DistributorSlots,
}

impl DistributorSnapshot {
    /// How many entries the set holds.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.slots.entries.len()
    }

    /// The $DIG base units held in the reserve.
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
}
