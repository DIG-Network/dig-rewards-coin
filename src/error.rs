//! [`RewardsError`] — why a reward-distributor operation could not be completed.
//!
//! Scaffolding for the error surface the driver logic (a follow-up ticket) will grow into. The
//! split that matters, matching the sibling `dig-mirror-coin`, is between **"the chain says
//! no"** and **"the chain did not say"**: a read that could not be established must fail closed,
//! never degrade into an empty or default answer.

use chia_sdk_driver::DriverError;
use thiserror::Error;

/// The reason a reward-distributor operation failed.
///
/// `#[non_exhaustive]`: new failure modes will arrive as driver logic lands in a minor release,
/// so consumers MUST include a wildcard match arm.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RewardsError {
    /// A chain source could not reliably answer a read.
    ///
    /// **This is never an absence.** It means the question went unanswered — a transport
    /// failure, a timeout, an unsupported query, a malformed response. A caller MUST NOT
    /// degrade this into an empty result.
    #[error("chain source could not answer: {0}")]
    ChainUnavailable(String),

    /// On-chain data was read but could not be interpreted (an undecodable memo, a puzzle that
    /// did not run). The read is untrustworthy, so the operation fails closed.
    #[error("malformed chain data: {0}")]
    Malformed(String),

    /// The launch terms a caller supplied cannot produce a valid DIG distributor.
    ///
    /// These are the values curried at launch and immutable afterwards, so refusing here is the
    /// only chance to refuse at all: a distributor launched with a zero fee-payout hash or a zero
    /// epoch length carries that mistake for its whole life (`SPEC.md` §7.3, §8.1).
    #[error("invalid launch terms: {0}")]
    InvalidLaunchTerms(String),

    /// The entry set is at `SPEC.md` §15 clause 7's cap and cannot take another entry.
    ///
    /// A named refusal rather than a silent stop: an operator whose adds quietly stopped landing
    /// would keep paying network fees to add nobody, and would read the frozen set as a prover
    /// fault.
    #[error("the entry set is full at its cap of {cap} entries")]
    EntrySetFull {
        /// The cap that was reached.
        cap: u32,
    },

    /// An entry-set write cannot be brought inside its validity window.
    ///
    /// The write asserts `ASSERT_BEFORE_SECONDS_ABSOLUTE(last_update + max_seconds_offset)`, and a
    /// `Sync` can only move `last_update` strictly forward and never past the current epoch's end
    /// (`SPEC.md` §8.2 clause 1). Once `last_update` has reached `epoch_end` the distributor's
    /// clock cannot advance at all until someone rolls the epoch, so the remedy is `NewEpoch`
    /// first -- which is permissionless, and therefore something the caller can do itself.
    #[error(
        "entry-set write window is closed: last_update {last_update} has reached epoch_end {epoch_end} (now {now_unix_seconds}); roll the distributor epoch first"
    )]
    EntrySetWriteWindowClosed {
        /// The distributor's `last_update`, in Unix seconds.
        last_update: u64,
        /// The current distributor epoch's end, in Unix seconds.
        epoch_end: u64,
        /// The time the caller supplied, in Unix seconds.
        now_unix_seconds: u64,
    },

    /// The caller is not the authority recorded in the commitment slot being withdrawn.
    ///
    /// Authority for a clawback is the slot's own `clawback_ph` and nothing else — not the manager
    /// singleton, not the launcher (`SPEC.md` §7.5). Refused here rather than on chain, because the
    /// operator pays the network fee for a spend the puzzle then rejects.
    #[error("not the clawback authority recorded in the commitment slot")]
    NotTheClawbackAuthority,

    /// A puzzle construction or spend-building step failed inside the Chia driver layer.
    ///
    /// Boxed because `DriverError` is large and would otherwise bloat every `Result` in the
    /// crate.
    #[error("chia driver error: {0}")]
    Driver(#[from] Box<DriverError>),
}

impl From<DriverError> for RewardsError {
    fn from(error: DriverError) -> Self {
        Self::Driver(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chia_sdk_driver::DriverError;

    #[test]
    fn chain_unavailable_displays_the_reason() {
        let err = RewardsError::ChainUnavailable("peer timed out".to_string());
        assert_eq!(
            err.to_string(),
            "chain source could not answer: peer timed out"
        );
    }

    #[test]
    fn malformed_displays_the_reason() {
        let err = RewardsError::Malformed("undecodable memo".to_string());
        assert_eq!(err.to_string(), "malformed chain data: undecodable memo");
    }

    #[test]
    fn driver_error_from_conversion_boxes_and_displays() {
        let driver_err = DriverError::Custom("boom".to_string());
        let wrapped: RewardsError = driver_err.into();
        assert!(matches!(wrapped, RewardsError::Driver(_)));
        assert!(wrapped.to_string().starts_with("chia driver error: "));
    }

    #[test]
    fn from_box_driver_error_variant_constructs_directly() {
        let driver_err = DriverError::Custom("boom".to_string());
        let err = RewardsError::Driver(Box::new(driver_err));
        assert!(err.to_string().starts_with("chia driver error: "));
    }
}
