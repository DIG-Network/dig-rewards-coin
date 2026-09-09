//! `SPEC.md` §0.5 clause 2 — the guard that turns an upstream puzzle change into a red build.
//!
//! `RewardDistributorConstants` are curried into the action puzzles at launch, so a distributor's
//! on-chain identity is a function of the exact upstream puzzle bytes. A client built on different
//! bytes computes different curried hashes and can therefore no longer reconstruct or spend a
//! distributor launched under the old ones. The funder's reserve is not lost — it is invisible to
//! that client, which is worse than an error, because the client's own view simply shows nothing
//! there.
//!
//! So every reward-distributor action hash this crate depends on is pinned here and compared to the
//! `chia-sdk-types` constant. An upstream bump that moves any of them fails this test, and §0.5
//! clause 3 then requires a migration story for already-launched distributors before it may merge.

use chia_sdk_types::puzzles::{
    REWARD_DISTRIBUTOR_ADD_ENTRY_PUZZLE_HASH, REWARD_DISTRIBUTOR_ADD_INCENTIVES_PUZZLE_HASH,
    REWARD_DISTRIBUTOR_COMMIT_INCENTIVES_PUZZLE_HASH,
    REWARD_DISTRIBUTOR_INITIATE_PAYOUT_WITHOUT_APPROVAL_PUZZLE_HASH,
    REWARD_DISTRIBUTOR_NEW_EPOCH_PUZZLE_HASH, REWARD_DISTRIBUTOR_REMOVE_ENTRY_PUZZLE_HASH,
    REWARD_DISTRIBUTOR_SYNC_PUZZLE_HASH, REWARD_DISTRIBUTOR_WITHDRAW_INCENTIVES_PUZZLE_HASH,
};
use clvm_utils::TreeHash;

/// The pinned hash of every reward-distributor action this crate uses, at the 0.36 cohort.
///
/// Measured from `chia-sdk-types` 0.36.0. Two of them — `AddEntry` and `Sync` — are also written
/// out in `SPEC.md` §0.5, which is what makes this table checkable against the specification
/// without reading the registry.
const PINNED_ACTION_PUZZLE_HASHES: &[(&str, TreeHash, &str)] = &[
    (
        "AddEntry",
        REWARD_DISTRIBUTOR_ADD_ENTRY_PUZZLE_HASH,
        "9a25633bc5b34abc08bf75b62ad5d44caa37270065161c1800189aabe2ae45ec",
    ),
    (
        "RemoveEntry",
        REWARD_DISTRIBUTOR_REMOVE_ENTRY_PUZZLE_HASH,
        "6cdf7feefe369fa694e71ee9c40a383bfda5ef43eab52034195990d20086d2b9",
    ),
    (
        "InitiatePayout (without approval)",
        REWARD_DISTRIBUTOR_INITIATE_PAYOUT_WITHOUT_APPROVAL_PUZZLE_HASH,
        "3ac00fa8db24e15d425af0624502a9ff7c588eeb2726bd5d7f83b39897484b66",
    ),
    (
        "NewEpoch",
        REWARD_DISTRIBUTOR_NEW_EPOCH_PUZZLE_HASH,
        "1b2c758b5a4da560bf177ab23b9500dcdb302bc7724b39c82b641c214d13332f",
    ),
    (
        "Sync",
        REWARD_DISTRIBUTOR_SYNC_PUZZLE_HASH,
        "1a4d3e443be05a124980741db509657d5b49a0405d9646179e8a498ae2fe4343",
    ),
    (
        "AddIncentives",
        REWARD_DISTRIBUTOR_ADD_INCENTIVES_PUZZLE_HASH,
        "01146475a5ece9f0625beb4a82298e37dc864a3c162e7533585967517b37bee7",
    ),
    (
        "CommitIncentives",
        REWARD_DISTRIBUTOR_COMMIT_INCENTIVES_PUZZLE_HASH,
        "dd092298c7331f56f00b34cb68425a4f34bac28729a60daff7384a517087d3ec",
    ),
    (
        "WithdrawIncentives",
        REWARD_DISTRIBUTOR_WITHDRAW_INCENTIVES_PUZZLE_HASH,
        "3bc68bef318e3d6a1c2a6002e9f6f56cba0fe2a81404adcec99055b467df05a0",
    ),
];

#[test]
fn puzzle_hash_guard() {
    for (action, upstream, pinned) in PINNED_ACTION_PUZZLE_HASHES {
        assert_eq!(
            hex::encode(upstream.to_bytes()),
            *pinned,
            "the {action} action puzzle hash moved upstream. \
             SPEC.md §0.5 clause 3: this is a wire-breaking event — every already-launched \
             distributor becomes unreadable to a client built on the new bytes, so a migration \
             story is required before the bump may merge. Do not simply update this table."
        );
    }

    // A table that silently lost a row would pass every assertion above, so the count is pinned
    // too: all eight actions §0.5 clause 2 enumerates must be present.
    assert_eq!(
        PINNED_ACTION_PUZZLE_HASHES.len(),
        8,
        "SPEC.md §0.5 clause 2 requires all eight actions to be guarded"
    );
}
