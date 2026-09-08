//! Minting a DIG rewards distributor.
//!
//! One thin wrapper over the upstream `launch_reward_distributor`, which owns every byte of the
//! launch spend. What this module adds is the two refusals a funder cannot recover from:
//!
//! - `first_epoch_start` in the past, which launches a distributor whose first epoch can never be
//!   started cleanly (`SPEC.md` §8.5 clause 1); and
//! - a free-form comment. The comment is built from a [`LaunchComment`], so a caller cannot pass a
//!   string that fails §1.3 and thereby mint a distributor no DIG client will ever recognise.
//!
//! The mode is always `Managed`. Staking is hard-refused in `Managed` mode upstream, and the DIG
//! entry set is maintained by a prover rather than by stakers.

use chia_bls::{SecretKey, Signature};
use chia_consensus::consensus_constants::ConsensusConstants;
use chia_protocol::Bytes32;
use chia_puzzle_types::standard::StandardArgs;
use chia_sdk_driver::{
    launch_reward_distributor, Cat, Offer, RewardDistributor, RewardDistributorConstants, Slot,
    SpendContext,
};
use chia_sdk_types::puzzles::RewardDistributorRewardSlotValue;

use crate::{comment::LaunchComment, constants::DistributorLaunchTerms, RewardsError};

/// Everything the launch produced, named rather than positional.
///
/// The upstream call returns a five-tuple; naming the parts here is what stops a caller from
/// swapping the security key and the launch signature at a call site where both are opaque bytes.
pub struct LaunchedDistributor {
    /// Aggregated signature over the launch bundle's security coin.
    pub signature: Signature,

    /// The ephemeral security-coin secret key the launch created. The caller must sign with it and
    /// then discard it; it controls nothing afterwards.
    pub security_coin_secret_key: SecretKey,

    /// The distributor singleton, ready for its first action.
    pub distributor: RewardDistributor,

    /// The reward slot for the first distributor epoch.
    pub first_distributor_epoch_slot: Slot<RewardDistributorRewardSlotValue>,

    /// The change CAT returned to the funder's refund puzzle hash.
    pub refund_cat: Cat,
}

/// Mint a DIG rewards distributor for one generation.
///
/// `terms` carries the three launch-time-only choices; `funder_refund_puzzle_hash` receives the CAT
/// change and is also the (inert, because `fee_bps` is 0) fee payout hash. `now_unix_seconds` is
/// the caller's current time, used only for the past-`first_epoch_start` refusal — this crate reads
/// no clock of its own.
///
/// # Errors
///
/// - [`RewardsError::InvalidLaunchTerms`] if `terms.first_epoch_start` is not strictly in the
///   future. A distributor whose first epoch has already begun cannot have that epoch started, so
///   its reserve accrues to nobody.
/// - [`RewardsError::Driver`] if the upstream launch spend could not be built.
pub fn launch_dig_distributor(
    ctx: &mut SpendContext,
    offer: &Offer,
    terms: DistributorLaunchTerms,
    constants: RewardDistributorConstants,
    consensus_constants: &ConsensusConstants,
    generation: LaunchComment,
    funder_refund_puzzle_hash: Bytes32,
    now_unix_seconds: u64,
) -> Result<LaunchedDistributor, RewardsError> {
    require_future_first_epoch_start(terms.first_epoch_start, now_unix_seconds)?;

    let (
        signature,
        security_coin_secret_key,
        distributor,
        first_distributor_epoch_slot,
        refund_cat,
    ) = launch_reward_distributor(
        ctx,
        offer,
        terms.first_epoch_start,
        funder_refund_puzzle_hash,
        constants,
        consensus_constants,
        // §1.3: the comment is rendered from a parsed value, never taken as free-form text.
        &generation.to_string(),
    )?;

    Ok(LaunchedDistributor {
        signature,
        security_coin_secret_key,
        distributor,
        first_distributor_epoch_slot,
        refund_cat,
    })
}

/// Refuse a `first_epoch_start` that is not strictly in the future (`SPEC.md` §8.5 clause 1).
///
/// Separated from [`launch_dig_distributor`] because it is the whole of the launch-time judgement
/// and deserves to be checkable without building a spend.
///
/// # Errors
///
/// [`RewardsError::InvalidLaunchTerms`] if `first_epoch_start` is at or before `now_unix_seconds`.
pub fn require_future_first_epoch_start(
    first_epoch_start: u64,
    now_unix_seconds: u64,
) -> Result<(), RewardsError> {
    if first_epoch_start <= now_unix_seconds {
        return Err(RewardsError::InvalidLaunchTerms(format!(
            "first_epoch_start {first_epoch_start} is not in the future (now {now_unix_seconds})"
        )));
    }

    Ok(())
}

/// The standard-puzzle hash for a public key, for callers assembling a funder refund hash.
///
/// Offered because getting this wrong sends the CAT change somewhere unrecoverable, and because the
/// alternative is every caller hand-rolling the same currying.
#[must_use]
pub fn standard_puzzle_hash(public_key: chia_bls::PublicKey) -> Bytes32 {
    StandardArgs::curry_tree_hash(public_key).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::FIRST_EPOCH_START_LEAD_SECONDS;

    #[test]
    fn a_first_epoch_start_in_the_past_is_refused() {
        let now = 1_800_000_000;

        assert!(matches!(
            require_future_first_epoch_start(now - 1, now),
            Err(RewardsError::InvalidLaunchTerms(_))
        ));
        assert!(
            matches!(
                require_future_first_epoch_start(now, now),
                Err(RewardsError::InvalidLaunchTerms(_))
            ),
            "the boundary itself is already too late: the epoch has begun"
        );
        assert!(
            require_future_first_epoch_start(now + FIRST_EPOCH_START_LEAD_SECONDS, now).is_ok()
        );
    }
}
