//! Who belongs in the entry set — `SPEC.md` §4.3's three calls as one reusable predicate.
//!
//! A candidate peer is eligible only if a mirror coin establishes, on chain, all three of:
//!
//! 1. the `storeId:root` **and** mirror-collateral-epoch binding ([`MirrorCoinFacts::advertises`]),
//! 2. that the coin's owner declared **this** `peer_id` ([`MirrorCoinFacts::declares_peer`]), and
//! 3. the payout puzzle hash the entry will carry ([`MirrorCoinFacts::owner_puzzle_hash`]).
//!
//! All three MUST pass. There is no weaker path: no nomination message, no self-declared payout
//! address, no operator override, no "eligible with reduced shares", and no configuration flag that
//! admits a candidate without the chain binding (§10.3 clause 2).
//!
//! ## Ineligibility is not an accusation
//!
//! [`Ineligible`] produces no strike, no blocklist entry, and no wording that reads as misconduct.
//! It most often means the peer has not created its coin yet (§10.3 clause 3), and the absence of a
//! declaration is evidence of nothing at all (§10.3 clause 4).
//!
//! ## Why the mirror coin arrives as a trait
//!
//! `dig-mirror-coin` sits at `10-primitives`, the same level as this crate, so a dependency on it
//! would be a forbidden same-level edge. The facts this predicate needs are therefore declared as
//! [`MirrorCoinFacts`], which this crate owns and the caller — `dig-node`, which may depend on both
//! — implements over its own `MirrorCoin`. The three method names and their order match §4.3's
//! table exactly so that the mapping is checkable by eye.
//!
//! An implementor MUST NOT reimplement any of the three checks. In particular `advertises`
//! performs **two** checks upstream (the declared tuple *and* the namespace hint) and both are
//! required: check 1 alone accepts a coin that declares one thing and is indexed as another, and
//! check 2 alone accepts a coin bonding an entirely different store.

use chia_protocol::Bytes32;

use crate::RewardsError;

/// The facts a mirror coin establishes about a candidate peer, as this crate needs them.
///
/// Implement this over `dig_mirror_coin::MirrorCoin` in a layer that may depend on both crates.
/// Every method MUST delegate to the upstream call named in its documentation; a reimplementation
/// is a defect, not an optimisation.
pub trait MirrorCoinFacts {
    /// Does this coin advertise the given generation for the given mirror-collateral epoch?
    ///
    /// Delegate to `MirrorCoin::advertises(store_launcher_id, root_hash, epoch)`, which performs
    /// both the declared-tuple check and the namespace-hint check. Substituting either one alone is
    /// a defect.
    ///
    /// `mirror_collateral_epoch` is the `epoch` ordinal in a mirror advertisement. It is **not** the
    /// distributor epoch and MUST NOT be derived from `distributor_epoch_seconds`, from the
    /// `dig-epoch` L2 epoch, or from wall-clock arithmetic (§0.3).
    fn advertises(
        &self,
        store_launcher_id: Bytes32,
        root_hash: Bytes32,
        mirror_collateral_epoch: u32,
    ) -> bool;

    /// Did the coin's owner declare this `peer_id`?
    ///
    /// Delegate to `MirrorCoin::declares_peer(peer_id)`. `peer_id` is 32 bytes —
    /// `SHA-256(TLS SubjectPublicKeyInfo DER)`, the same value `dig_tls::peer_id_from_tls_spki_der`
    /// produces — and MUST be compared as those bytes, never as text.
    ///
    /// A coin carrying two or more declaration terms declares **nobody** and so fails this gate.
    fn declares_peer(&self, peer_id: Bytes32) -> bool;

    /// The payout puzzle hash the entry will carry.
    ///
    /// Delegate to `MirrorCoin::owner_puzzle_hash()`, which derives from the coin's **lineage
    /// proof** — executed on-chain code — and not from a memo an owner wrote. This is why the
    /// prover never receives a payout address from anyone: it derives one (§10.2 clause 2).
    fn owner_puzzle_hash(&self) -> Bytes32;
}

/// The generation and mirror-collateral epoch a candidate is being judged against.
///
/// The three values travel together because judging against a partial tuple is the mistake §4.3
/// clause 1 describes: the epoch term is free, and its author can solve for a hint collision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EligibilityQuestion {
    /// The DIG store's launcher id.
    pub store_launcher_id: Bytes32,

    /// The exact generation root the distributor pays mirrors of.
    pub root_hash: Bytes32,

    /// The mirror-collateral epoch ordinal the census is for.
    ///
    /// **Required, with no default and no derivation.** Its calendar is an input this crate does not
    /// own (`dig_ecosystem#3259`, §15.3 clause 2); a caller that cannot supply it gets
    /// [`RewardsError::ChainUnavailable`] from [`judge_candidate`], never a guess.
    pub mirror_collateral_epoch: u32,
}

/// Why a candidate is not eligible.
///
/// Each variant names a chain fact that did not hold. None of them is an accusation, and none MUST
/// be rendered as one: no strike, no blocklist entry, no log line that reads as misconduct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ineligible {
    /// No mirror coin was found for the candidate.
    ///
    /// The commonest outcome by far, and it means only that: the peer has most likely not created
    /// its coin yet.
    NoMirrorCoin,

    /// A coin exists but does not advertise this generation for this mirror-collateral epoch.
    DoesNotAdvertiseGeneration,

    /// A coin exists and advertises the generation, but its owner did not declare this `peer_id`.
    ///
    /// Also the outcome when a coin carries two or more declaration terms, because such a coin
    /// declares nobody.
    DoesNotDeclarePeer,
}

/// Judge one candidate peer against one generation, in `SPEC.md` §4.3's order.
///
/// `facts` is `None` when no mirror coin was found for the candidate — the absent-coin case, which
/// is ineligibility and not an error.
///
/// On success the returned [`Bytes32`] is the `payout_puzzle_hash` the entry MUST carry. It is a
/// puzzle hash and never a public key, a BLS key, a peer id or an address string (§10.2 clause 1).
///
/// Fails closed: any one of the three checks failing, or the coin being absent, is ineligibility
/// with no weaker fallback.
///
/// # Errors
///
/// [`RewardsError::ChainUnavailable`] when `question.mirror_collateral_epoch` could not be
/// established — the question went unanswered, which is not the same as an answer of "no".
pub fn judge_candidate(
    question: EligibilityQuestion,
    peer_id: Bytes32,
    facts: Option<&impl MirrorCoinFacts>,
) -> Result<Result<Bytes32, Ineligible>, RewardsError> {
    let Some(facts) = facts else {
        return Ok(Err(Ineligible::NoMirrorCoin));
    };

    if !facts.advertises(
        question.store_launcher_id,
        question.root_hash,
        question.mirror_collateral_epoch,
    ) {
        return Ok(Err(Ineligible::DoesNotAdvertiseGeneration));
    }

    if !facts.declares_peer(peer_id) {
        return Ok(Err(Ineligible::DoesNotDeclarePeer));
    }

    Ok(Ok(facts.owner_puzzle_hash()))
}

/// Judge one candidate when the mirror-collateral epoch may not have been established.
///
/// This is the entry point for a caller reading the epoch ordinal from a source that can fail. A
/// `None` epoch is **not** a licence to guess one: it yields [`RewardsError::ChainUnavailable`], so
/// the census simply does not run rather than running against a fabricated calendar (§15.3
/// clause 2).
///
/// # Errors
///
/// [`RewardsError::ChainUnavailable`] when `mirror_collateral_epoch` is `None`.
pub fn judge_candidate_for_epoch(
    store_launcher_id: Bytes32,
    root_hash: Bytes32,
    mirror_collateral_epoch: Option<u32>,
    peer_id: Bytes32,
    facts: Option<&impl MirrorCoinFacts>,
) -> Result<Result<Bytes32, Ineligible>, RewardsError> {
    let Some(mirror_collateral_epoch) = mirror_collateral_epoch else {
        return Err(RewardsError::ChainUnavailable(
            "mirror-collateral epoch not established; the census cannot run".to_string(),
        ));
    };

    judge_candidate(
        EligibilityQuestion {
            store_launcher_id,
            root_hash,
            mirror_collateral_epoch,
        },
        peer_id,
        facts,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const STORE: Bytes32 = Bytes32::new([0x11; 32]);
    const ROOT: Bytes32 = Bytes32::new([0x22; 32]);
    const PEER: Bytes32 = Bytes32::new([0x33; 32]);
    const OWNER_PH: Bytes32 = Bytes32::new([0x44; 32]);
    const MIRROR_COLLATERAL_EPOCH: u32 = 41;

    /// A stand-in for the caller's `MirrorCoin`, with each of the three facts switchable so a test
    /// can fail exactly one of them.
    struct StubCoin {
        advertises: bool,
        declares_peer: bool,

        /// The one mirror-collateral epoch this coin advertises. A real coin advertises a specific
        /// ordinal, so asking it about any other must come back `false` rather than panicking.
        advertised_epoch: u32,
    }

    impl StubCoin {
        fn all_passing() -> Self {
            Self {
                advertises: true,
                declares_peer: true,
                advertised_epoch: MIRROR_COLLATERAL_EPOCH,
            }
        }
    }

    impl MirrorCoinFacts for StubCoin {
        fn advertises(
            &self,
            store_launcher_id: Bytes32,
            root_hash: Bytes32,
            mirror_collateral_epoch: u32,
        ) -> bool {
            assert_eq!(store_launcher_id, STORE);
            assert_eq!(root_hash, ROOT);
            self.advertises && mirror_collateral_epoch == self.advertised_epoch
        }

        fn declares_peer(&self, peer_id: Bytes32) -> bool {
            // §4.3 clause 2: the comparison is over the 32 bytes, never over text.
            assert_eq!(peer_id.as_ref(), PEER.as_ref());
            self.declares_peer
        }

        fn owner_puzzle_hash(&self) -> Bytes32 {
            OWNER_PH
        }
    }

    fn question() -> EligibilityQuestion {
        EligibilityQuestion {
            store_launcher_id: STORE,
            root_hash: ROOT,
            mirror_collateral_epoch: MIRROR_COLLATERAL_EPOCH,
        }
    }

    #[test]
    fn eligibility_all_three_passing_yields_the_owner_puzzle_hash() {
        let verdict = judge_candidate(question(), PEER, Some(&StubCoin::all_passing())).unwrap();
        assert_eq!(verdict, Ok(OWNER_PH));
    }

    #[test]
    fn eligibility_absent_coin_is_ineligible_not_an_error() {
        let verdict = judge_candidate(question(), PEER, None::<&StubCoin>).unwrap();
        assert_eq!(verdict, Err(Ineligible::NoMirrorCoin));
    }

    #[test]
    fn eligibility_advertises_failing_alone_is_ineligible() {
        let coin = StubCoin {
            advertises: false,
            ..StubCoin::all_passing()
        };
        let verdict = judge_candidate(question(), PEER, Some(&coin)).unwrap();
        assert_eq!(verdict, Err(Ineligible::DoesNotAdvertiseGeneration));
    }

    #[test]
    fn eligibility_declares_peer_failing_alone_is_ineligible() {
        let coin = StubCoin {
            declares_peer: false,
            ..StubCoin::all_passing()
        };
        let verdict = judge_candidate(question(), PEER, Some(&coin)).unwrap();
        assert_eq!(verdict, Err(Ineligible::DoesNotDeclarePeer));
    }

    #[test]
    fn eligibility_a_different_mirror_collateral_epoch_is_a_different_question() {
        // §4.3 clause 1: the mirror-collateral epoch term is free, and its author can solve for a
        // hint collision, so the ordinal must reach the coin unaltered and decide the verdict. The
        // stub advertises exactly `MIRROR_COLLATERAL_EPOCH`.
        let coin = StubCoin::all_passing();

        // Asked about the census the coin does not advertise, a candidate that passes every other
        // check must still fail closed. Nothing else in the question changes.
        let wrong_census = EligibilityQuestion {
            mirror_collateral_epoch: MIRROR_COLLATERAL_EPOCH + 1,
            ..question()
        };
        assert_eq!(
            judge_candidate(wrong_census, PEER, Some(&coin)).unwrap(),
            Err(Ineligible::DoesNotAdvertiseGeneration),
            "a wrong-census question must be ineligible, not eligible on the coin's own answer"
        );

        // And the refusal is the epoch's doing rather than a blanket one: the same coin, the same
        // peer and the advertised ordinal is eligible. The pair together is only satisfiable if
        // `question.mirror_collateral_epoch` is what reaches `advertises`.
        assert_eq!(
            judge_candidate(question(), PEER, Some(&coin)).unwrap(),
            Ok(OWNER_PH),
            "the advertised census must still be eligible, or the refusal above proves nothing"
        );
    }

    #[test]
    fn eligibility_an_unestablished_epoch_is_chain_unavailable_never_a_guess() {
        let err =
            judge_candidate_for_epoch(STORE, ROOT, None, PEER, Some(&StubCoin::all_passing()))
                .unwrap_err();
        assert!(matches!(err, RewardsError::ChainUnavailable(_)));
    }

    #[test]
    fn eligibility_an_established_epoch_runs_the_same_predicate() {
        let verdict = judge_candidate_for_epoch(
            STORE,
            ROOT,
            Some(MIRROR_COLLATERAL_EPOCH),
            PEER,
            Some(&StubCoin::all_passing()),
        )
        .unwrap();
        assert_eq!(verdict, Ok(OWNER_PH));
    }
}
