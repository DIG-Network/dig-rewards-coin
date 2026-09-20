//! [`RewardsError`] — why a reward-distributor operation could not be completed.
//!
//! The split that matters, matching the sibling `dig-mirror-coin`, is between **"the chain says
//! no"** and **"the chain did not say"**: a read that could not be established must fail closed,
//! never degrade into an empty or default answer.

use chia_protocol::Bytes32;
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

    /// No distributor was ever launched at this launcher id (`SPEC.md` §12.5 clause 3a).
    ///
    /// [`crate::state::read_distributor`] answering `Ok(None)` means exactly this — the launcher
    /// coin was never spent — and it MUST NOT be degraded into "this peer holds no entry slot",
    /// which is a different fact with a different remedy (clause 1's "keep observing, spend
    /// nothing" is correct only for a real distributor a peer has not yet been admitted to).
    /// [`crate::payout::ChainEntrySlotSource`]'s `read_entry_slot` surfaces this as an error rather
    /// than `Ok(None)` for exactly that reason: reported as an absence, a mistyped or
    /// non-existent launcher id would be indistinguishable from a peer patiently waiting to be
    /// admitted, forever.
    #[error("no distributor was ever launched at launcher id {launcher_id}")]
    NoDistributorAtLauncherId {
        /// The launcher id a chain-backed entry-slot read was asked to resolve.
        launcher_id: Bytes32,
    },

    /// A puzzle construction or spend-building step failed inside the Chia driver layer.
    ///
    /// Boxed because `DriverError` is large and would otherwise bloat every `Result` in the
    /// crate.
    #[error("chia driver error: {0}")]
    Driver(#[from] Box<DriverError>),

    /// A withdrawal's share cannot be computed by `chia-sdk-driver` 0.36.0 at all: its own
    /// `rewards * withdrawal_share_bps` (`withdraw_incentives.rs:105-107`) is a plain `u64`
    /// multiply and this pair overflows it (#3286).
    ///
    /// Refused BEFORE the upstream call, because a checked-arithmetic build would otherwise panic
    /// inside the driver with no chance to report anything at all.
    #[error(
        "the withdrawal share for {rewards_base_units} base units at {withdrawal_share_bps} bps \
         cannot be represented by the chia-sdk-driver 0.36.0 multiply (tracked upstream as #3286); \
         refusing rather than paying a network fee for a spend the driver cannot build"
    )]
    DriverShareNotRepresentable {
        /// The full committed amount the share would be computed from.
        rewards_base_units: u64,
        /// The distributor's own withdrawal-share basis points.
        withdrawal_share_bps: u64,
    },

    /// The driver's returned withdrawal share disagrees with this crate's own restatement
    /// ([`crate::recoverable_base_units`]), or no restatement could even be computed.
    ///
    /// `restated` is [`None`] rather than a defaulted `0` when `withdrawal_share_bps` itself is
    /// out of the `u16` range [`crate::recoverable_base_units`] takes (reachable for any bps in
    /// `10_001..=65_535`): a fabricated `0` would read as "this crate's own restatement computes
    /// zero", which is a different -- and false -- claim from "no restatement exists". Either way
    /// the driver's whole returned tuple is untrustworthy, not just the share, so this refuses the
    /// completed spend rather than return it.
    #[error(
        "the driver reported a withdrawal share of {driver_reported} base units but this \
         crate's own restatement computes {restated:?} -- the driver's u64 multiply wrapped, or \
         withdrawal_share_bps is out of the u16 range restated (#3286)"
    )]
    DriverShareDisagrees {
        /// What `chia-sdk-driver` 0.36.0 returned.
        driver_reported: u64,
        /// What [`crate::recoverable_base_units`] computes independently, or [`None`] if
        /// `withdrawal_share_bps` could not even be narrowed to a `u16` to compute one.
        restated: Option<u64>,
    },

    /// A distributor's own `withdrawal_share_bps` constant is outside the legitimate `0..=10_000`
    /// domain, so no honest share can be quoted for it at all.
    ///
    /// [`crate::state::read_distributor`] is deliberately distributor-agnostic and reads a
    /// launcher any caller could have created, so a hostile or corrupt constant reaches this
    /// check from unauthenticated chain input rather than only from DIG's own launches.
    #[error(
        "distributor constants carry withdrawal_share_bps={withdrawal_share_bps}, outside the \
         legitimate 0..=10_000 domain -- refusing to read rather than reporting a fabricated share"
    )]
    UnreadableDistributorConstants {
        /// The out-of-domain basis points read off the distributor's own constants.
        withdrawal_share_bps: u64,
    },

    /// A commitment slot's own recorded `rewards` value is too large for
    /// [`crate::state::read_distributor`] to ever safely reconstruct a later generation that
    /// withdraws it: `rewards * withdrawal_share_bps` could exceed `u64::MAX` inside the upstream
    /// driver's plain `u64` multiply (#3286) before this crate ever sees a returned value to
    /// check.
    ///
    /// Refused as soon as the generation that CREATES the slot is reconstructed — before any
    /// later generation could reach the unchecked multiply. This bounds the quantity a withdraw
    /// can name directly, rather than a proxy for it (a reserve-coin high-water mark, this
    /// error's predecessor): the action layer can batch a `CommitIncentives` with a
    /// same-generation reserve outflow into one distributor-coin spend, so the reserve coin's
    /// *amount* after any given generation reflects only the net of that generation's actions and
    /// can never be trusted to have seen a transient peak. A commitment slot's own `rewards`
    /// field has no such blind spot: `CommitIncentives::get_log` performs no multiply, so it
    /// parses safely at any scale.
    ///
    /// **This check runs at the CREATING generation regardless of whether a withdraw of the same
    /// slot shares it.** An earlier version of this doc claimed a slot could never be created and
    /// withdrawn in the same singleton spend; that is false (see `src/state.rs`'s
    /// `read_distributor` docs and `tests/simulator.rs`'s
    /// `a_commitment_above_the_bound_batched_with_another_action_is_refused_by_the_reader`) — the two actions can
    /// share one spend, and this check still catches it because it reads
    /// `created_commitment_slots` on `from_spend`'s return, before this walk advances to any later
    /// generation, independent of what else shared the spend. What this bound does NOT close is a
    /// same-generation withdraw whose multiply overflows the driver's OWN `u64` arithmetic before
    /// `from_spend` returns at all — see `RewardsError::UnrecognisedActionPuzzle` and
    /// `RewardsError::ActionArithmeticNotRepresentable`, and DIG-Network/dig_ecosystem#3313.
    #[error(
        "commitment slot rewards of {rewards_base_units} base units exceeds the \
         {max_readable_base_units} base units this reader can safely carry through to a later \
         withdraw -- refusing rather than risk the driver's u64 share multiply (#3286)"
    )]
    CommitmentRewardsTooLargeToRead {
        /// The commitment slot's own recorded `rewards`, read off the generation that created it.
        rewards_base_units: u64,
        /// The largest commitment-slot `rewards` this reader will carry through to a withdraw.
        max_readable_base_units: u64,
    },

    /// A generation's inner solution names an action-layer puzzle this reader does not
    /// recognise as one of the 11 reward-distributor actions `chia-sdk-driver` 0.36.0 defines.
    ///
    /// This is a fail-closed pre-screen, not a fail-open skip (DIG-Network/dig_ecosystem#3313): a
    /// future pin bump that changes, adds or removes an action puzzle makes every read refuse
    /// loudly here, rather than silently walking an action this reader never checked for an
    /// unchecked-arithmetic hazard. Refusing on drift is the only direction a refuse-don't-serve
    /// reader may fail in.
    #[error(
        "action-layer puzzle at generation carries hash {action_puzzle_hash} which is not one of \
         the 11 reward-distributor actions this reader enumerates -- refusing rather than walking \
         an action this reader never screened for an unchecked-arithmetic hazard (#3313)"
    )]
    UnrecognisedActionPuzzle {
        /// The tree hash of the action puzzle this reader could not match to a known action.
        action_puzzle_hash: Bytes32,
    },

    /// One of the unchecked `u64` (or `i128`) arithmetic sites inside `chia-sdk-driver` 0.36.0's
    /// action `get_log` methods (`withdraw_incentives.rs:71`, `:89`, `commit_incentives.rs:85`,
    /// `:101`, `:111`, `unstake.rs:235`, `stake.rs:326`, `:329`) would overflow on the operands this
    /// generation's own action solution carries (or, for `unstake`/`stake`, on a value the
    /// solution's own locked/unlocked CAT or NFT commitment authenticates), before
    /// `RewardDistributor::from_spend` ever returns to let this crate's own B1/B2 guards run.
    ///
    /// Refused BEFORE calling `from_spend` on this generation, because a checked-arithmetic build
    /// (`dig-node` and `dig-relay` both ship `overflow-checks = true` in release) panics inside the
    /// driver with no chance to report anything at all, and a wrapping build would hand back a
    /// fabricated figure as authenticated distributor state (DIG-Network/dig_ecosystem#3313).
    #[error(
        "action {action} solution's {operation} would overflow chia-sdk-driver 0.36.0's unchecked \
         arithmetic before RewardDistributor::from_spend can return -- refusing rather than \
         risking a panic (checked build) or a fabricated figure (wrapping build) (#3313)"
    )]
    ActionArithmeticNotRepresentable {
        /// Which screened action this came from (`"withdraw_incentives"`, `"commit_incentives"`,
        /// `"unstake"` or `"stake"`).
        action: &'static str,
        /// Which operation would overflow (`"committed_value * withdrawal_share_bps"`,
        /// `"reward_slot_total_rewards - withdrawal_share"`, `"slot_total_rewards +
        /// rewards_to_add"`, `"slot_epoch_time + epoch_seconds"`, `"start_epoch_time +
        /// iterations * epoch_seconds"`, `"entry_slot.shares - removed_shares"`,
        /// `"existing_slot_counter + 1"` or `"existing_slot_shares + new_shares"`).
        operation: &'static str,
    },

    /// A distributor's own `epoch_seconds` launch constant is zero, which makes
    /// `chia-sdk-driver` 0.36.0's reward-slot backfill loop non-terminating.
    ///
    /// `commit_incentives`'s non-adjacent-epoch branch (`chia-sdk-driver-0.36.0`'s
    /// `commit_incentives.rs:101-112`) backfills one empty
    /// [`chia_sdk_types::puzzles::RewardDistributorRewardSlotValue`] per epoch between the spent
    /// reward slot's `slot_epoch_time` and the new commitment's `epoch_start`, stepping by
    /// `epoch_seconds` — a value curried into the action puzzle from the distributor's own launch
    /// constants ([`chia_sdk_driver::RewardDistributorConstants::epoch_seconds`]), not a per-spend
    /// solution field. An attacker who launches their own distributor picks it freely, and at
    /// `epoch_seconds == 0` the loop's own `start_epoch_time += epoch_seconds` never advances: an
    /// unconditional, profile-independent non-terminating loop, reachable from the public reader,
    /// that a `checked_add` cannot close because the addition never overflows — it just never
    /// progresses.
    ///
    /// That loop is also **pure and non-yielding** (`grep -c await` over `commit_incentives.rs`
    /// is `0`), so a consumer cannot rescue itself with a `tokio::time::timeout`: with no await
    /// point the timeout future is never polled and the worker thread hangs regardless. A refusal
    /// by this reader is the only defence that can work, which is why
    /// [`crate::state::read_distributor`] takes it on the **launch constants**, beside
    /// [`RewardsError::UnreadableDistributorConstants`], before a single generation is parsed
    /// -- and not only per action inside the replay walk. Matching one of this reader's recognised
    /// eleven action hashes proves only that the distributor's constants are well-formed, never
    /// that they are benign.
    #[error(
        "distributor constants carry epoch_seconds == 0 -- the driver's own reward-slot backfill \
         loop would never terminate on this value, and being a pure CPU loop no caller-side \
         timeout can cancel it; refusing to read rather than hanging (#3313)"
    )]
    UnreadableEpochSeconds,

    /// An observed spend's serialized `puzzle_reveal` or `solution` exceeds
    /// [`crate::discovery::DECODE_MAX_SERIALIZED_BYTES`], refused before any byte of it is
    /// deserialised into the allocator (DIG-Network/dig_ecosystem#3333).
    ///
    /// Deliberately its own variant rather than [`RewardsError::Malformed`]:
    /// [`RewardsError::Malformed`] means "the read was attempted and could not be interpreted",
    /// while this is a policy refusal **before any read is attempted at all** -- a caller scanning
    /// the chain has a legitimate reason to distinguish "this spend is junk" from "this spend is
    /// too big to be worth decoding", and a `#[non_exhaustive]` enum can carry the new arm without
    /// breaking any caller that already matches on an else/wildcard arm.
    #[error(
        "observed spend's {field} is {actual_len} bytes, exceeding the \
         {limit_bytes}-byte decode limit -- refusing before any allocation"
    )]
    ObservedSpendFieldTooLarge {
        /// Which field was too large: `"puzzle_reveal"` or `"solution"`.
        field: &'static str,
        /// The field's actual serialized length, in bytes.
        actual_len: usize,
        /// The limit that was exceeded:
        /// [`crate::discovery::DECODE_MAX_SERIALIZED_BYTES`].
        limit_bytes: usize,
    },

    /// `commit_incentives`'s non-adjacent-epoch backfill loop (see
    /// [`RewardsError::UnreadableEpochSeconds`]) pushes one reward slot per iteration of
    /// `(epoch_start - slot_epoch_time) / epoch_seconds`, both `epoch_start` and `slot_epoch_time`
    /// being attacker-supplied action-solution fields with `epoch_seconds > 0` but otherwise
    /// unbounded in magnitude — `read_distributor` is deliberately distributor-agnostic, so for an
    /// attacker's own launcher `epoch_seconds` is also attacker-chosen, all the way down to `1`.
    ///
    /// An earlier version of this bound divided the distributor's own declared
    /// `max_seconds_offset` tolerance by `epoch_seconds` and refused past the quotient. That
    /// derivation was wrong in **both** directions at once, which is why it is gone: at DIG's own
    /// real launch constants (`epoch_seconds = 604_800`, `max_seconds_offset = 300`) the quotient
    /// is `0`, so it refused every honest non-adjacent-epoch commit outright; and because nothing
    /// in this crate bounded `max_seconds_offset`, an attacker's own `epoch_seconds = 1,
    /// max_seconds_offset = u64::MAX` inflated the same quotient to roughly `1.8e19`, which is no
    /// bound at all. A ratio of two attacker-reachable parameters cannot be a safety bound in
    /// either direction.
    ///
    /// [`MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS`](crate::state::MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS)
    /// replaces it: a fixed count this reader chose, independent of anything either an honest or a
    /// hostile launcher supplies, large enough that no plausible honest gap in real incentive
    /// commitments comes close (at DIG's own one-week epoch it is roughly nineteen thousand
    /// years), while still bounding the `RewardDistributorRewardSlotValue` structs this reader's
    /// own memory must hold to a few tens of megabytes at most.
    ///
    /// **It is a budget for a whole READ, not a ceiling for one action or one generation.**
    /// [`crate::state::read_distributor`] declares the accumulator outside its own walk loop,
    /// because that is the frame that owns the data the budget bounds: every reward slot a
    /// backfill creates is retained in the walk's `DistributorSlots::rewards` until the read
    /// returns, and nothing prunes it -- backfilled slots carry `counter: 0, rewards: 0`, and an
    /// attacker's own distributor need never spend them. A budget that reset each generation
    /// would bound nothing, since nothing bounds a walk's generation count: N cheaply-mined
    /// generations would multiply this reader's retained memory by N. Within one generation the
    /// same reasoning applies action by action. A generation's
    /// `ActionLayerSolution::action_spends` is a plain `Vec<Spend>`
    /// (`chia-sdk-driver-0.36.0/src/layers/action_layer/action_layer.rs:42`) whose length nothing
    /// bounds, here or upstream, and `ActionLayer::parse_solution` resolves repeated selectors
    /// through one CACHED Merkle proof (`action_layer.rs:255-272`), so a single leaf can be spent
    /// arbitrarily many times in one generation. A `commit_incentives` action's on-chain CLVM cost
    /// does not scale with its backfill gap -- only the off-chain `get_log` reconstruction does --
    /// so a per-action ceiling would let one cheaply-mined spend force every reader to materialise
    /// `action_spends.len()` times the cap. This reader therefore CONSUMES the budget as it walks
    /// the read's generations and their actions, and refuses when an action asks for more than is
    /// left, which is why the refusal names the two operands of what remains rather than a single
    /// count.
    ///
    /// `action_spends.len()` itself is deliberately NOT refused separately: an action that
    /// backfills nothing allocates nothing, so a length bound would name no hazard this budget
    /// does not already cover, and would refuse honest callers for nothing.
    #[error(
        "a commit_incentives action would backfill {iterations} reward slots on top of the \
         {already_committed} this read's earlier actions already committed, exceeding \
         the {max_backfill_slots} this reader will construct across one read -- refusing \
         rather than risking unbounded CPU/memory (#3313)"
    )]
    CommitIncentivesBackfillBoundExceeded {
        /// The real backfill iteration count upstream's loop would run for THIS action: the
        /// CEILING of `epoch_start - (slot_epoch_time + epoch_seconds)` over `epoch_seconds`.
        iterations: u64,
        /// How much of the budget this read's EARLIER `commit_incentives` actions have already
        /// consumed -- across every generation walked so far, not only this one. Zero when this
        /// action is the read's first backfilling one, so a single-action refusal reads exactly
        /// as it did when the cap was per-action.
        already_committed: u64,
        /// The fixed per-read budget this reader enforces, independent of any distributor's
        /// own constants:
        /// [`MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS`](crate::state::MAX_COMMIT_INCENTIVES_BACKFILL_SLOTS).
        max_backfill_slots: u64,
    },
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
