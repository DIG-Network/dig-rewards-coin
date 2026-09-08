# dig-rewards-coin — normative specification

## 0. Scope

A **rewards distributor** is a CHIP-0051 reward distributor, in its **`Managed`** mode, that pays
$DIG to the peers that actually mirror one DIG generation — one `storeId:root`. Anyone may mint one,
anyone may fund it, the funder's node continuously proves which peers really serve those bytes, and a
mirroring peer collects its own rewards without asking anyone.

This crate defines: the DIG-shaped constants a DIG distributor MUST carry, the spend builders that
launch it and mutate its entry set, the eligibility rule that decides who is in that set, and the
observable state that makes "is anyone being paid?" answerable. It is the document every
implementation child of DIG-Network/dig_ecosystem#3246 reads **instead of** the SDK.

### 0.1 What this crate owns, and what it MUST NOT

The on-chain mechanism is **not ours**. It is CHIP-0051, implemented upstream in `chia-wallet-sdk`
0.36 (`chia-sdk-driver` 0.36.0 + `chia-sdk-types` 0.36.0).

1. This crate MUST be a driver over the upstream reward distributor. It MUST NOT reimplement, fork,
   or transcribe any reward-distributor puzzle, and MUST NOT restate any arithmetic the puzzle owns:
   the per-share accrual, the payout division, the epoch fee, and the withdrawal share are the
   puzzle's, and this document names the function that performs each rather than repeating a formula
   that would then drift.
2. This crate MUST perform no socket I/O, MUST hold no keys, and MUST NOT broadcast. Chain reads
   arrive through a caller-supplied chain source (`dig-chainsource-interface`); spend builders return
   unsigned coin spends. This is the same rule the sibling `dig-mirror-coin` states as its invariant
   6, and it is what keeps the crate testable against a simulator and keeps signing in the wallet
   layer where the operator's key already lives.
3. The **prover loop** and the **claim loop** are NOT in this crate. They are `dig-node`
   (DIG-Network/dig_ecosystem#3250, #3251). This crate supplies the types, the eligibility predicate,
   and the spend builders those loops call. Every clause below that says "the prover MUST" is a
   requirement on #3250 that this crate MUST make expressible — where the crate cannot express a rule,
   the rule cannot be enforced, and §15 lists which side each clause lands on.
4. This crate MUST NOT depend on `dig-epoch`. See §0.3; a Cargo dependency on `dig-epoch` from this
   crate is a defect, not a style preference.

### 0.2 Units — named once, never converted silently

- **$DIG amounts** are always **DIG CAT base units**. $DIG carries **3 decimals**, so
  `1 $DIG = 1_000 base units`. Every amount in this specification, in `reserve` state, in
  `payout_threshold`, and in every API on this crate is base units, as an integer. A float MUST NOT
  appear anywhere in the money path. A display layer MAY render `1_000` as `1.000 $DIG`; it MUST NOT
  hand a rendered value back into an API.
- **Network fees** are **XCH mojos**, `1 XCH = 1_000_000_000_000 mojos`. A fee field is never $DIG,
  and the two MUST NOT share a type. The fee an operator pays for an entry-set write (§6) and the
  reward a mirror earns (§8) are denominated in different assets; an implementation that adds them,
  compares them, or displays them in one column is wrong.
- **`fee_bps` and `withdrawal_share_bps`** are basis points out of `10_000`. The puzzle divides by
  `10000` itself (`new_epoch.rs:124`, `withdraw_incentives.rs:71`). An implementation MUST carry them
  as basis points end to end and MUST NOT convert them to a percentage, a ratio, or a float at any
  layer including the UI's own arithmetic — a percentage round-trip is how a 420 becomes a 4.
- **`precision`** is a dimensionless scale factor on the `u128` accumulators `cumulative_payout` and
  `remaining_rewards`. It is not a currency, not a count, and not a rounding mode. See §8.4.
- **Times** are Unix seconds. **Heights** are block heights. The two MUST NOT be interconverted by
  multiplying by a block time; where finality is meant, §12 names the blocks.

### 0.3 Three clocks share the word "epoch" — they are unrelated

This has already cost confusion once. All three exist in the same process, at the same time, and none
of them may be derived from another.

| name in this document | what it is | who owns it |
|---|---|---|
| **distributor epoch** | `RewardDistributorConstants::epoch_seconds` — the CHIP-0051 reward-accrual window, curried at launch, immutable | this crate (§8) |
| **mirror-collateral epoch** | the `epoch` ordinal in a mirror advertisement `(store, root, owner, epoch)`; a coin qualifies for the census of epoch `n` only by declaring `n-1` exactly | `dig-mirror-coin` / `dig-mirror-collateral`; the calendar is an INPUT neither crate owns (`dig-mirror-coin/SPEC.md` §8.2 C4 and :371-372) |
| **`dig-epoch` L2 epoch** | DIG L2 epoch geometry — phases, checkpoint competition, `EPOCH_L1_BLOCKS = 32` (`modules/crates/30-network/dig-epoch/src/constants.rs:174`) | `dig-epoch` |

Normatively:

1. An implementation MUST NOT derive `epoch_seconds` from `dig-epoch`, from the mirror-collateral
   calendar, or from the claim cadence.
2. An implementation MUST NOT derive the mirror-collateral epoch ordinal from `epoch_seconds`, from
   `dig_epoch::EpochManager::current_epoch()`
   (`modules/crates/30-network/dig-epoch/src/manager.rs:88`), or from wall-clock arithmetic of its
   own. §4.6 states where it comes from.
3. Prose, log lines, field names and RPC fields MUST qualify the word: `distributor_epoch_seconds`,
   `mirror_collateral_epoch`. A bare `epoch` in this crate's public API is a defect.

The two lengths coincide by choice and not by wiring: §8.1 sets `epoch_seconds` to seven days, which
is also the mirror-collateral epoch length (`dig-mirror-collateral/src/constants.rs:216-217`). The
**phases** are independent — `first_epoch_start` is chosen per distributor at launch — and an
implementation MUST NOT align them, assume they are aligned, or read a distributor boundary as a
collateral boundary.

### 0.4 What a reader may NOT conclude

Every statement here is a limit on what this mechanism proves. Each exists because the opposite
reading is the natural one.

1. **A passing challenge is not evidence that a peer stores the bytes.** It is evidence the peer
   returned the exact bytes for windows it could not predict, within a deadline (§3). A peer that
   fetches each window from another holder on demand passes. The deadline makes that expensive; it
   does not make it impossible. This mechanism buys **availability under a bounded latency**, which
   is what a mirror is for, and it MUST NOT be described as a proof of storage or a proof of
   replication.
2. **An entry in the set is not evidence that a peer is mirroring now.** It is evidence it passed at
   its last completed evaluation. Between evaluations the set is a claim about the past, and while
   the prover is not running the set is frozen and the claim keeps aging (§2.1).
3. **A funded distributor is not evidence anyone is being paid the right amount.** A reserve is a
   balance. Who receives it depends on an entry set that only a live prover maintains.
4. **A mirror coin is not evidence of availability.** It proves $DIG is locked
   (`dig-mirror-coin/SPEC.md` §3 invariant 3). The challenge, not the coin, is the availability
   evidence.
5. **Being paid by a distributor is not a statement by the DIG network about the store.** Anyone may
   mint and fund a distributor for any `storeId:root`. A payment stream is one funder's private
   incentive; it MUST NOT be surfaced as an endorsement, a ranking input, a trust signal, or a
   content warranty.
6. **The entry set is not an access-control list.** Peers not in the set may still serve the store,
   and peers in it may refuse to. The set decides money, nothing else.

### 0.5 How a wrong version fails

`RewardDistributorConstants` are **curried into the action puzzles** at launch, so a distributor's
on-chain identity is a function of the exact upstream puzzle bytes. Two measured examples of those
bytes at the pinned cohort:

```
REWARD_DISTRIBUTOR_ADD_ENTRY_PUZZLE_HASH
    = 9a25633bc5b34abc08bf75b62ad5d44caa37270065161c1800189aabe2ae45ec
REWARD_DISTRIBUTOR_SYNC_PUZZLE_HASH
    = 1a4d3e443be05a124980741db509657d5b49a0405d9646179e8a498ae2fe4343
```

If an upstream release changes any reward-distributor puzzle, a client built on the new bytes computes
different curried hashes and can therefore **no longer reconstruct or spend a distributor launched
under the old bytes**. The funder's reserve is not lost — it is unspendable by that client, which is
worse than an error, because the client's own view simply shows nothing there.

Therefore:

1. This crate MUST pin the `chia-sdk-driver` 0.36 cohort (`chia-bls` / `chia-protocol` /
   `chia-puzzle-types` / `clvm-traits` / `clvm-utils` at `0.36.1`, `chia-puzzles 0.20.3`,
   `clvmr 0.16.2`). The ceiling is the cohort, never crates.io latest.
2. This crate MUST carry a guard test asserting the puzzle hash of **every** reward-distributor
   action it uses — `AddEntry`, `RemoveEntry`, `InitiatePayout` (without-approval variant),
   `NewEpoch`, `Sync`, `AddIncentives`, `CommitIncentives`, `WithdrawIncentives` — against a value
   pinned in this crate, each read from the corresponding `chia-sdk-types` constant at the pinned
   version. An upstream bump MUST fail that test, so the break arrives as a red build and not as a
   funder whose distributor has vanished.
3. A cohort bump that moves any of those hashes MUST be treated as a **wire-breaking event**: it
   requires a migration story for already-launched distributors before it may merge, exactly as
   `dig-mirror-coin/SPEC.md` §3 invariant 1 requires for its CAT outer hash.

---

## 1. Local-holding precondition

### 1.1 The rule

A distributor MUST NOT be created for a `(store_id, root)` that the creating node does not hold, in
full, **at that exact root**.

The reason is mechanical, not policy: the challenge (§3) decides pass/fail by comparing a peer's
returned bytes against local bytes. Without local bytes there is no comparison, and a prover with no
comparison must either pay every claimant or none — the first is a theft primitive, the second is a
distributor that cannot work.

### 1.2 What proves the precondition, and what does not

1. The check MUST be **at the generation**, not at the store. Holding `store_id` at some other root
   is not holding `(store_id, root)`: a different generation is different ciphertext, and every byte
   comparison against it fails.
2. The creation path MUST perform at least one **real local range read** at `root` — the same read
   the challenge will perform — and MUST refuse creation if it fails.
3. An inventory row, a config entry, or a directory's existence MUST NOT be accepted as proof. An
   inventory row outlives the bytes it describes; that is precisely the state §1.4 exists for.

### 1.3 The distributor names its generation on-chain

`launch_reward_distributor` takes `comment: &str`
(`chia-sdk-driver-0.36.0/src/primitives/action_layer/launch_drivers.rs:616,622`). That comment is the
only place the money is tied to content, so it is normative:

```
dig-rewards:v1:<store_id_hex>:<root_hex>
```

- `store_id_hex` and `root_hex` MUST each be exactly 64 lowercase hexadecimal characters.
- A writer MUST emit lowercase; a reader MUST accept either case and MUST compare the 32 BYTES the
  hex denotes, never the text. The two halves are stated together for the same reason
  `dig-mirror-coin/SPEC.md` §5.1 states them together: splitting them is an authorization difference
  between two implementations.
- A reader MUST treat a comment that does not parse as **not a DIG rewards distributor** and MUST NOT
  guess a store from any other field. It is not an error; CHIP-0051 distributors exist for other
  purposes (see §9.3).
- The comment MUST NOT be read as evidence that the named generation exists, is valid, or is held by
  anyone. It states which generation this distributor's money is *about*.

### 1.4 When the local copy is later lost

The prover MUST enter the named state `LocalCopyMissing` for that distributor and, while in it:

1. MUST stop issuing challenges,
2. MUST NOT add entries,
3. **MUST NOT remove entries**, and
4. MUST surface the state through §2's status surface.

Clause 3 is the single most important asymmetry in this document. An eviction caused by the
operator's own missing bytes is a false accusation that costs an honest mirror its stream and costs
the operator a chain write to inflict it. **Fail-closed here means "stop deciding", not "evict":
absence of evidence evicts nobody.** Every later "fails closed" in this document inherits that
reading, and §3.7 and §12.2 restate it where it is easy to get wrong.

Recovery is automatic: when the local read at `root` succeeds again the prover MUST resume, and MUST
NOT carry forward any strike (§3.6) accrued while in `LocalCopyMissing`.

### 1.5 When the store's root advances

A distributor names one `(store_id, root)` for its entire life, and:

1. This crate MUST NOT provide any operation that re-points a distributor at a different root. There
   is no such action in CHIP-0051 and there MUST NOT be a DIG-level emulation of one.
2. When the store's head advances past `root`, the distributor keeps paying mirrors of the named root.
   That is correct — those mirrors still hold and serve exactly the bytes the funder paid for.
3. A funder who wants the new generation mirrored MUST launch a **new** distributor for the new root,
   and MAY clawback the old one's future commitments (§7.4) to fund it.
4. The creation flow MUST state 2 and 3 before the launch spend is signed. A funder who believes a
   distributor follows the store's head will fund the wrong generation indefinitely.
5. A node MUST be permitted to hold and prove several generations of one store at once, each with its
   own distributor. Nothing in this document couples them.

---

## 2. Liveness honesty

> "Anytime the process isn't running then rewards are not being distributed."

That sentence is the requirement's, and **as written it is half true — the false half is the
dangerous one.** This section states what is actually true, then specifies the observable state that
makes it visible.

### 2.1 What a dead prover actually does — measured

In `Managed` mode the authority split is not what an operator assumes:

| action | authority required | measured at |
|---|---|---|
| `Sync` | **none** — permissionless | `chia-sdk-types-0.36.0/src/puzzles/action_layer/actions/reward_distributor/sync.rs:38-42` (solution is `update_time`; the args struct is empty) |
| `NewEpoch` | **none** — permissionless | `chia-sdk-driver-0.36.0/.../new_epoch.rs:19-25` (args carry no manager singleton struct hash) |
| `InitiatePayout` | **none**, with `require_payout_approval = false` | `.../initiate_payout.rs:118` |
| `AddIncentives`, `CommitIncentives` | none beyond funding the CAT | `.../add_incentives.rs`, `.../commit_incentives.rs` |
| `WithdrawIncentives` | the committer, via the commitment slot's `clawback_ph` | `chia-sdk-types-0.36.0/.../slot_values.rs:420-425` |
| **`AddEntry`, `RemoveEntry`** | **the manager singleton, by mode-18 message** | `.../add_entry.rs:110-117`, `.../remove_entry.rs:100-110` |

So while the prover is dead:

- accrual **continues** (`Sync` is permissionless, and any claimant may spend it),
- payouts **continue** (`InitiatePayout` is permissionless — §7.1), and
- **only the entry set freezes.**

The harm is therefore not "nothing happens". It is: **peers that stopped mirroring keep earning, and
peers that started mirroring cannot begin.** A frozen set drains the reserve to the wrong parties.
This is worse than a stall and the operator MUST be told so in exactly these terms.

### 2.2 The creation-time uptime warning

Before the launch spend is signed, the creation flow MUST state all four:

1. Rewards are distributed only while **this node's** prover runs.
2. The funds are not lost when it stops — they stay in the reserve, and future commitments can be
   clawed back (§7.4).
3. **While the prover is stopped the entry set is frozen: peers that have stopped mirroring continue
   to earn, and peers that begin mirroring cannot start.** Distribution to the frozen set continues.
4. Losing the manager singleton key freezes the entry set **permanently** (§7.3).

The warning MUST NOT be reducible to "requires consistent uptime". That phrasing invites the reader
to conclude that downtime merely pauses payment, which is the false half of §2.1.

### 2.3 The status surface

The prover MUST maintain, per distributor, a record carrying at least:

```
launcher_id                      Bytes32
store_id, root                   Bytes32, Bytes32          (from §1.3)
prover_state                     one of the named states below
prover_state_since               Unix seconds
last_cycle_started_at            Option<Unix seconds>
last_cycle_completed_at          Option<Unix seconds>
next_cycle_due_at                Option<Unix seconds>
last_entry_write_at              Option<Unix seconds>
consecutive_cycle_failures       u32
pending_entry_writes             u32                       (decided, not yet on chain — §6.3)
observed_at                      Unix seconds              (chain view this record reflects)
counters                         mirrors_seen, challenges_issued, challenges_passed,
                                 challenges_failed, entries_added, entries_removed,
                                 entry_count, reserve_base_units, total_paid_out_base_units
```

The named states are exactly: `Idle`, `Running`, `LocalCopyMissing`, `ChainSourceUnavailable`,
`Unfunded`, `FeeBudgetExhausted`, `EntrySetFull`, `Paused`, `Stopped`. An implementation MUST use this
closed set, MUST NOT add a state without adding it here, and MUST NOT collapse two of them into one
message: each maps to a different operator action, and `LocalCopyMissing` in particular must be
distinguishable from `Stopped` because only one of them is the operator's mistake.

### 2.4 The status MUST NOT contain a health boolean

An implementation MUST NOT expose a `healthy`, `ok`, `up`, or `running` boolean, and MUST NOT expose
a pre-computed staleness or "seconds since last run".

A wedged loop cannot report its own wedging. Whatever it last wrote stays there, so a boolean
computed by the writer reads `true` forever after the failure it exists to reveal. The reader MUST
derive staleness itself from `last_cycle_completed_at` against `observed_at` and its own clock, which
is a computation a stalled writer cannot influence.

For the same reason:

1. Absence of a status record for a funded distributor MUST render as **"not distributing"**, never as
   blank, "unknown", or a spinner that never resolves. Silence is not an acceptable representation of
   "not distributing".
2. `total_paid_out_base_units = 0` MUST NOT be rendered without `last_cycle_completed_at`. A zero
   beside no timestamp means **never ran**, and the surface MUST say that. A reassuring zero is the
   same lie as a stale `true`.
3. `entry_count` MUST be rendered with `last_entry_write_at`. An entry count with no write timestamp
   is a claim about the past presented as the present.

### 2.5 Heartbeat and cycle deadline

1. The prover MUST refresh `observed_at` at least every `PROVER_HEARTBEAT_SECONDS = 60`, including
   while `Idle`. A heartbeat is what makes "the process is gone" distinguishable from "the process is
   between cycles".
2. A cycle that exceeds `PROVER_CYCLE_DEADLINE_SECONDS = 900` MUST be abandoned, counted in
   `consecutive_cycle_failures`, and reported. It MUST NOT be left pending. A task awaiting a socket
   forever is the stall mode this whole section exists to expose, and a deadline converts it into an
   observable failure.
3. An abandoned cycle MUST NOT produce a challenge strike against any peer (§3.6): the failure was
   the prover's.

### 2.6 The RPC surface

Three methods, added to `modules/crates/00-foundation/dig-rpc-protocol/src/method.rs` (the enum,
`name()`, `tier()`, and the OpenRPC description), all at **`Tier::Control`**:

| method | answers |
|---|---|
| `dig.listRewardDistributors` | the distributors this node funds, and the distributors this node has a claim to as a mirror |
| `dig.getRewardProverStatus` | the §2.3 record, for one distributor or all |
| `dig.getRewardDistributor` | the chain-derived state of one distributor by launcher id: constants, reserve, entry count, current distributor epoch, last entry-write time |

`Tier::Control` is loopback / in-process only
(`modules/crates/00-foundation/dig-rpc-protocol/src/tier.rs:29-36`). All three are Control in the MVP
because the prover surface reveals which stores the operator funds and which peers it is currently
evaluating, which is the operator's business and not a peer's. `dig.getRewardDistributor` returns
nothing but public chain data and MAY be promoted to `PublicRead` or peer-reachable later — but that
promotion is a deliberate security decision taken at the allowlist, in the terms `method.rs`'s own
`is_peer_reachable` documentation sets, and MUST NOT be done as a convenience.

**Control is the correct default because the two directions are not symmetric.** Promoting a method
later is **additive** — no existing caller breaks. Demoting one is **breaking**, and it breaks exactly
the anonymous callers nobody can enumerate or notify. So an entry set and a payout history stay
operator data until someone argues otherwise on the record, and the cost of having been too strict is
one additive change while the cost of having been too loose is a withdrawal of access that is already
being relied on.

A mirror MUST NOT depend on any of these to decide whether a distributor is worth chasing: an
operator's self-report about its own liveness is worthless to a counterparty. §12.4 specifies the
chain-derived signal a mirror uses instead.

---

## 3. Challenge soundness

### 3.1 The wire is `dig.fetchRange`, and `capsule` MUST be false

The challenge MUST be issued as `dig.fetchRange` over the mTLS peer surface
(`modules/crates/00-foundation/dig-rpc-protocol/src/method.rs:135`, `Tier::Peer`), with

```
FetchRangeParams::resource(store_id, root, retrieval_key, length)
    .with_offset(offset)
    .with_skip_layout(true)
```

(`modules/crates/00-foundation/dig-rpc-protocol/src/types.rs:685-748`, setters at `:751-810`).

1. `capsule` MUST be absent or `false`. **Capsule range fetch is not served** and a `true` yields
   `-32004 ResourceUnavailable` (`types.rs:719-721` and the `with_capsule` doc). The requirement's
   phrase is "random ranged data from the capsule"; the served form of that is a random range of a
   **resource within** the capsule at `root`. A prover that requests capsule mode fails every honest
   mirror for a reason that has nothing to do with the mirror, and evicts the entire set on its first
   cycle.
2. `skip_layout` MUST be `true`. The prover already holds the layout for `root` by §1, and a layout
   prologue is resource-scaling — roughly 7.3 MB for a 1,048,576-chunk resource (`types.rs:724-747`).
   Re-requesting it per challenge turns a bounded probe into an unbounded one and hands a peer a
   bandwidth amplifier pointed at the operator.
3. The identity fields `root`, `total_length`, `chunk_count` and `chunk_index` ride every frame and
   are **not** suppressed by `skip_layout` (`types.rs:738-747`). The prover MUST check them against
   its local layout before comparing bytes: a mismatch identifies a wrong-generation responder in one
   comparison instead of a full byte diff.

### 3.2 Window selection — unpredictable per challenge

Per cycle, per candidate peer, the prover MUST select `CHALLENGE_WINDOWS_PER_CYCLE = 4` windows:

1. **Resource choice MUST be length-proportional.** Choose a `retrieval_key` from the resources at
   `root` with probability proportional to that resource's ciphertext length. Uniform-over-resources
   is wrong and exploitable: it lets a peer discard the large resources — most of the bytes — and
   still pass, because a store's byte mass is usually concentrated in a few resources.
2. **Offset MUST be uniform** in `[0, total_length - length]` for the chosen resource.
3. `length = CHALLENGE_WINDOW_BYTES = 65_536` (64 KiB), clamped down to `total_length` for a resource
   smaller than that. 4 x 64 KiB = 256 KiB per peer per cycle, so 100 mirrors cost about 25 MiB of
   inbound per cycle — a number the operator can be shown, and comfortably under the RPC window cap
   (`types.rs:716`). Smaller windows are cheaper but approach the size at which a response could be
   reconstructed from metadata a peer can obtain without the bytes; 64 KiB is not reconstructible from
   any chunk-hash list.
4. **Randomness MUST come from a CSPRNG** (`getrandom` / `OsRng`). It MUST NOT be derived from a
   counter, a timestamp, the peer id, the store id, the root, the cycle index, or any hash of those.
   A range derived from stable inputs is the same range every cycle — that is a precompute oracle and
   a replay in one.
5. The prover MUST remember the windows it asked each peer for the last
   `CHALLENGE_NO_REPEAT_CYCLES = 8` cycles and MUST NOT reuse a window for the same
   `(peer_id, launcher_id)` within it. Caching one answer is the cheapest attack on any
   challenge-response scheme, and a bounded memory closes it at bounded cost.

### 3.3 The comparison is byte-for-byte on ciphertext, and that is sound

The prover MUST base64-decode `RangeFrame::bytes` and compare it **byte-for-byte** with its own
ciphertext for the same `(root, retrieval_key, offset, length)`.

This is sound because DIG capsule encryption is deterministic: AES-256-GCM-SIV under a fixed nonce,
with the key derived by HKDF from the canonical URN
(`modules/crates/10-primitives/dig-capsule/src/imp/core/crypto.rs:37-45,73-79`), whose own comment
records why — "holding the nonce fixed keeps encryption deterministic so the ciphertext-committed
merkle root is reproducible (the committed root is taken over the ciphertext bytes)". Two honest
holders of the same `root` therefore hold **identical bytes**, and any difference at all is evidence.

The prover MUST NOT decrypt in order to compare. Decryption is unnecessary work on the hot path, it
requires the content key on a machine that only needs to compare, and it discards the very property
that makes the comparison exact.

### 3.4 The inclusion proof is not evidence of possession

`RangeFrame::inclusion_proof` is a whole-resource merkle proof against `root`
(`types.rs:893-901`). It is **public and relayable**: a peer holding none of the bytes can obtain a
proof and forward it verbatim.

1. The prover MUST NOT accept an inclusion proof as evidence of possession, of availability, or of
   anything about the responder.
2. It MAY be used only as a cheap pre-check that the responder is answering about the right
   generation, before the byte comparison.
3. A valid inclusion proof accompanying wrong bytes MUST fail exactly as loudly as no proof at all.

### 3.5 Fail-closed, and what "closed" means here

A challenge window FAILS on any of: transport failure; a TLS peer-id mismatch (§4.5); a timeout; any
JSON-RPC error including `-32004`; a frame shorter or longer than `length`; a frame whose `offset`
does not echo the request; a `total_length`, `chunk_count` or `chunk_index` inconsistent with the
local layout; a base64 that does not decode; or any byte difference.

A cycle PASSES only if **all four** windows match. Partial credit is not offered, and the reason is
§3.2's sampling: with length-proportional windows a peer holding a fraction `f` of the bytes passes a
cycle with probability about `f^4`, so "all of 4" already *is* the graded test. A pass threshold below
4 would grade the same evidence twice and admit a peer that stores three quarters of what the funder
is paying for.

**A failed cycle is not an eviction.** §3.6 is the eviction rule; §1.4's asymmetry applies here too.

### 3.6 Eviction is three consecutive strikes, not one failure

1. A completed cycle that fails increments `consecutive_failures` for that
   `(peer_id, launcher_id)`.
2. A completed cycle that passes MUST reset it to zero.
3. At `CHALLENGE_STRIKES_TO_EVICT = 3` the prover MUST schedule a `RemoveEntry` (§6).
4. A cycle **not completed because of the prover's own fault** — `LocalCopyMissing`,
   `ChainSourceUnavailable`, the prover's own cycle deadline (§2.5), a reorg (§12.2) — MUST NOT
   increment anything.
5. Strikes MUST reset to zero on prover restart (§12.1).

Single-failure eviction is wrong on both money axes. It evicts an honest mirror for one dropped
packet, one restart, or one NAT rebind; and because every eviction and every re-add is a chain write
the **operator** pays for (§6), a hair-trigger is also a drain on the funder. Three consecutive
failures at the §6.3 cycle period is roughly three hours of continuous unavailability before a
mirror's stream stops — long enough to be real, short enough that a dead mirror is not paid for a
day.

### 3.7 Bounds that protect the peer being challenged

1. `CHALLENGE_DEADLINE_SECONDS = 30` per window; `CHALLENGE_PEER_DEADLINE_SECONDS = 120` for a
   peer's four windows. The deadline is what makes on-demand proxying expensive rather than free. It
   is **not** a bandwidth SLA and MUST NOT be described as one — 64 KiB in 30 s is a floor almost any
   real host clears.
2. `CHALLENGE_MIN_INTERVAL_SECONDS = 900` per peer, **summed across every distributor this node
   funds**. Without the cross-distributor sum, an operator funding 50 distributors for stores that one
   peer mirrors turns its own prover into a request amplifier aimed at that peer.
3. The prover MUST NOT challenge more than `CHALLENGE_MAX_PEERS_PER_CYCLE = 64` peers per cycle per
   distributor, and MUST rotate deterministically through the candidate list across cycles so that a
   large candidate set is covered rather than truncated.
4. Every string a peer supplies — an error message, a URL term (§4.4) — MUST be treated as
   attacker-controlled: bounded in length before logging, never interpolated into a shell, a path, or
   a UI without escaping.

---

## 4. Mirror-coin gate

**§4 and §10 are one mechanism.** The gate that decides eligibility is the same three-call chain that
binds a peer identity to a payout address; specifying them apart is what produced the superseded
recommendation to invent a signing scheme. §10 states the binding and its fail-closed rule; this
section states the procedure.

### 4.1 Candidates come from the DHT

The prover MUST obtain candidates from
`DhtService::find_providers(&ContentId::root(store_id, root))`
(`modules/crates/20-domain/dig-dht/src/service.rs:204`,
`modules/crates/20-domain/dig-dht/src/content.rs:36-80`). Each `ProviderRecord`
(`.../dig-dht/src/record.rs:379-433`) carries `provider_peer_id`, a capped and ranked `addresses`
list, `expires_at`, and `unverified_mirror_coin_id`.

A record is a claim by an untrusted peer. Nothing in it is evidence, and the prover MUST NOT rank,
prefer, or admit on the basis of any field before §4.3 has closed.

### 4.2 The mirror coin is fetched through the untrusted pointer

`ProviderRecord::unverified_mirror_coin_id` exists so a verifier can fetch **one** coin instead of
scanning by hint (`record.rs:393-427`). The prover MUST use it, and MUST use it exactly as that field's
own documentation requires — as a pointer that proves nothing.

Against the prover's **own** chain source:

1. Fetch the coin and verify it sits at `dig_mirror_coin::mirror_coin_puzzle_hash()`.
2. Verify it is $DIG, with the asset id re-derived from the creating spend and equal to
   `dig_constants::DIG_ASSET_ID` (§9).
3. Verify it carries the full collateral requirement for its epoch (`dig-mirror-collateral`). An
   under-collateralised coin contributes to nothing (`dig-mirror-coin/SPEC.md` §8.3) and MUST NOT be
   read here as partially bonding anything.
4. Verify it is **unspent** as of the prover's chain view.
5. Reconstruct it with `MirrorCoin::from_creating_spend`
   (`modules/crates/10-primitives/dig-mirror-coin/src/coin.rs:227`), then apply §4.3.

A pointer that is absent, unresolvable, or fails any step means the candidate is **not eligible** this
cycle. It MUST NOT be treated as evidence of bad faith: absence is normal and fully supported —
publishers mid-epoch-rollover, publishers with no coin yet, and republished records with a stale
pointer all legitimately lack one (`record.rs:411-421`). A mismatch is indistinguishable from an
epoch rollover, so it MUST NOT produce a strike, a blocklist entry, or a retry loop. One chain read,
no retry.

### 4.3 The three calls that close the loop

All three MUST pass. Each is a call into `dig-mirror-coin`; a caller MUST NOT reimplement any of them.

| call | what it establishes | measured at |
|---|---|---|
| `MirrorCoin::advertises(store_launcher_id, root_hash, epoch)` | the `storeId:root` **and** mirror-collateral-epoch binding | `dig-mirror-coin/src/coin.rs:148` |
| `MirrorCoin::declares_peer(peer_id)` | the owner **declared this `peer_id`** — the authenticated binding | `.../coin.rs:207` |
| `MirrorCoin::owner_puzzle_hash()` | the **`payout_puzzle_hash`** the entry will carry | `.../coin.rs:93` |

1. `advertises` performs **two** checks — the declared tuple and the namespace hint — and both are
   required. The crate performs both; a caller MUST NOT substitute either one alone. Check 1 without
   check 2 accepts a coin that declares one thing and is indexed as another; check 2 without check 1
   accepts a coin bonding an entirely different store, because the epoch term is free and its author
   can solve for a hint collision (`coin.rs:140-147`).
2. `declares_peer` is trustworthy for exactly the question it answers, and `dig-mirror-coin` says why:
   the declaration was written by the spend that created the coin, and *"only the owner's key could
   produce the spend that wrote it"* (`coin.rs:186-191`). It is the only memo-derived value in that
   specification an implementation may rely on against an adversary.
   - `peer_id` MUST be the DHT record's `provider_peer_id`, and MUST be compared as the 32 BYTES it
     denotes, never as text (`dig-mirror-coin/SPEC.md` §5.1 rule 2). `PeerDeclaration` is exactly
     `SHA-256(TLS SubjectPublicKeyInfo DER)` — the same value
     `dig_tls::peer_id_from_tls_spki_der` produces
     (`modules/crates/00-foundation/dig-tls/src/identity.rs:71`), so the DHT's identity space, the
     mirror coin's, and the TLS transport's are already **one** space. Nothing needs bridging.
   - A coin carrying two or more declaration terms declares **nobody**
     (`dig-mirror-coin/SPEC.md` §5.1 rule 3), so it fails this gate. That is an economic rule: one
     coin standing behind several peers would make each claim cost a fraction while every one still
     read as fully bonded.
3. `owner_puzzle_hash()` derives from the **lineage proof** — executed on-chain code — not from a
   memo. It is the value the entry MUST carry (§10.2).

### 4.4 `urls()` is not a list of URLs

`MirrorCoin::urls()` returns the owner's whole free memo tail (`coin.rs:182-194`): *"unverified
strings straight off the chain... not been contacted, parsed for scheme, or bounded in number by
anything but the block that carried them."*

A prover MUST:

1. consider at most `MAX_MIRROR_URL_TERMS = 8` terms and ignore the rest — the count is not bounded on
   chain, so it MUST be bounded here;
2. skip any term beginning `dig_mirror_coin::PEER_DECLARATION_PREFIX` (`dig-peer:`,
   `.../declaration.rs:56`) — it is a declaration, not a URL, and it is read by `declares_peer`, not
   by a fetcher;
3. skip any term that is not a well-formed absolute URL with an allowed scheme;
4. refuse to dial a loopback, link-local, or private-range address obtained from a term — the terms are
   attacker-chosen, so dialling them unfiltered is an SSRF primitive pointed at the operator's own
   network;
5. never log a term unescaped or unbounded.

The prover MUST prefer `ProviderRecord::addresses`, which `dig-dht` has already capped
(`MAX_ADDRESSES_PER_RECORD`) and ranked, and MUST fall back to `urls()` terms only when those are
exhausted, within a bounded total dial budget per candidate per cycle.

### 4.5 "URL validated" means the transport pinned the peer id

The declaration binds a **coin to a peer id and nothing further**. `dig-mirror-coin` states the gap
verbatim: *"It does not establish that the addresses alongside a claimed peer id reach that peer"*
(`coin.rs:205-206`), and its SPEC §5.1 states the closure: *"a provider record carrying an honest
holder's peer id, that holder's real coin id, and an attacker's addresses satisfies this check
completely... Closing that requires the consumer's transport to pin the dialled peer id against the
presented certificate, which is a property of the transport and not of this crate."*

Therefore:

1. The prover MUST dial the candidate over the DIG mTLS peer transport and the transport MUST pin
   `dig_tls::peer_id_from_tls_spki_der(<presented leaf SPKI DER>)` against the `peer_id` the mirror
   coin declared. A mismatch is a failed validation, and MUST fail the same way a wrong byte does.
2. **The challenge IS the URL validation.** Because §3 dials the pinned peer and requires exact bytes
   back, a separate reachability probe adds nothing and MUST NOT be substituted for it. A prover MUST
   NOT admit a candidate on reachability alone, and MUST NOT record a bare TCP or TLS handshake as
   evidence of anything but reachability.
3. An implementation whose transport does not pin MUST NOT run this prover at all. Without pinning,
   the entry set is decided by whoever wrote the addresses.

### 4.6 "Live for that epoch"

The `epoch` argument to `advertises` is the **mirror-collateral epoch ordinal** (§0.3), and:

1. A coin qualifies for the census of mirror-collateral epoch `n` only by declaring `n-1` **exactly**;
   a coin declaring an earlier or a later ordinal MUST be excluded
   (`dig-mirror-coin/SPEC.md` §8.2 C4). The prover MUST use that same offset, because any other
   offset admits a coin the census itself excludes.
2. The ordinal MUST be supplied to the prover as an **input**, by the component that owns the
   mirror-collateral calendar. `dig-mirror-coin` explicitly does not own it — *"the epoch calendar...
   is not defined by this crate. An implementation MUST take the epoch start as an input"*
   (`dig-mirror-coin/SPEC.md:371-372`) — and neither does this crate. The prover MUST NOT compute
   it (§0.3 clause 2).
3. During `MIRROR_EPOCH_GRACE_SECONDS = 21_600` (6 h) after a rollover the prover MUST also accept a
   coin declaring the previous ordinal, and MUST NOT strike a peer for a rollover mismatch. Without
   the grace window every mirror in the network becomes ineligible simultaneously at every boundary,
   through no fault of its own, and a strict prover would evict its entire set every epoch.
4. Liveness of the coin itself is §4.2 clause 4: unspent as of the prover's chain view. A coin spent
   after the prover's view was taken is not retroactively disqualifying — the prover re-evaluates
   every cycle.

### 4.7 The gate is per cycle, and the census is not on the hot path

The prover MUST re-run §4.2-§4.6 for every candidate every cycle. Eligibility is not cached across
cycles, because a coin can be reclaimed at any time and a cached "eligible" would keep paying a peer
whose collateral has gone.

The prover MUST NOT run a full `dig_mirror_coin::census` per cycle. The census is an epoch-wide,
finality-gated computation (`dig-mirror-coin/SPEC.md` §8.6-§8.7) whose cost is the whole mirror
puzzle-hash population; running it per cycle per distributor is an unbounded chain read on a loop
that must complete inside `PROVER_CYCLE_DEADLINE_SECONDS`. The pointer path reads one coin.

---

## 5. Self-exclusion

### 5.1 The rule is absolute

The operator's own node MUST NOT be an entry in a distributor it provers.

Reasons, in order of weight:

1. The operator holds the generation by §1, so it passes its own challenge by construction. The
   challenge would compare local bytes with themselves.
2. Shares are equal (§11), so a self-entry takes `1/(n+1)` of every epoch's rewards from the honest
   mirrors the funder is trying to attract. The funder pays itself with its own money and dilutes
   everyone who did the work.
3. It converts an incentive into a fee-free round trip, which makes the distributor's published
   metrics a lie about how many independent mirrors exist.

### 5.2 It is enforced on BOTH coordinates

A candidate MUST be rejected if **either**:

- its `peer_id` equals this node's own `peer_id`, **or**
- the `payout_puzzle_hash` derived in §4.3 is one this node's own wallet controls.

Excluding only `peer_id` is bypassed by running a second node whose mirror coin pays the operator's
puzzle hash. Excluding only the puzzle hash is bypassed by paying a fresh address. Both coordinates or
neither.

### 5.3 It is enforced on EVERY path, at ONE place

DIG-Network/dig-node#261 is the analogous defect and its lesson is the rule here: *"An invariant
enforced on some paths is not an invariant; it is a habit."* There, an absolute SPEC self-exclusion was
honoured by the DHT leg and bypassed by the forwarded leg.

Therefore:

1. Self-exclusion MUST be a single check at the **single admission point** where a candidate becomes
   an entry decision — not one check per discovery path.
2. Every path MUST pass through it: the DHT walk, this node's locally-held provider set, the
   discovered cache, any operator-supplied allowlist or manual add, and the deferred off-chain hint
   (§13.2, #3252).
3. The check MUST be a **refusal at admission**, never a filter at display. A filtered display leaves
   the entry on chain.
4. The test that proves it MUST include a control: the identical candidate arriving by a second path
   must also be refused, so the test distinguishes "excluded self" from "dropped everything".

### 5.4 What this rule does not do

It excludes the operator's own identity. It MUST NOT be extended to co-location, a shared subnet, a
shared host, or a heuristic about "related" peers: those are unknowable from this vantage and the
extension would exclude honest mirrors that happen to share a data centre. This is not a Sybil
defence and MUST NOT be described as one — the Sybil cost is the collateral (§6.2).

---

## 6. Sybil and eviction economics

### 6.1 Who pays what

| act | payer | asset |
|---|---|---|
| `AddEntry` / `RemoveEntry` bundle | the **operator** (the funder running the prover; holder of the manager singleton) | XCH mojos, network fee |
| the mirror coin's collateral | the **mirror** | $DIG base units, locked |
| `InitiatePayout` (a claim) | the **claiming peer** | XCH mojos, network fee |
| `Sync` / `NewEpoch` | whoever spends it — in practice the prover or a claimant | XCH mojos, network fee |
| the reward itself | the **funder's reserve** | $DIG base units |

A mirror MUST NOT be charged anything by this mechanism to be added, evaluated, or removed.

### 6.2 Do not double-charge a cost the chain already levies

A Sybil mirror must already create a mirror coin locking the epoch's **full** collateral requirement,
per `(owner, store, root)` triple, and one coin may declare at most one peer:

- the counted unit is the triple, not the coin (`dig-mirror-coin/SPEC.md` §8.2 C7),
- an under-collateralised coin contributes to nothing (§8.3), and
- *"an owner who wants two peers bonded MUST create two coins and lock the collateral twice"*
  (§5.1 rule 3).

So `N` Sybil identities cost `N x` the collateral requirement, in $DIG, on chain, before any of them
is even a candidate.

**This specification MUST NOT add a bond, a stake, an application fee, or a deposit for entry.** A
second charge would fall on honest mirrors too, and it would duplicate a cost the chain levies
already. The correct Sybil lever, if one is ever needed, is the collateral requirement in
`dig-mirror-collateral`, which is not this crate's to set.

### 6.3 Four bounds on entry-set writes

Every add and every remove is a distributor singleton spend concurrent with a manager singleton spend,
and the operator pays its fee. An unbounded churn loop drains the funder in XCH while the reserve is
untouched — a failure mode that looks like nothing is wrong with the distributor.

1. **Batch.** Entry-set writes MUST be batched. One prover cycle produces **at most one** distributor
   spend bundle, carrying up to `MAX_ENTRY_WRITES_PER_BUNDLE = 8` add/remove actions. The action layer
   accepts several actions in one spend (`RewardDistributor::new_action`, `insert_action_spend`,
   `finish_spend`), so eight mutations cost one fee instead of eight.
2. **Rate.** At most one entry-set bundle per distributor per
   `ENTRY_WRITE_MIN_INTERVAL_SECONDS = 3_600`. A decision reached sooner waits, and MUST appear in
   `pending_entry_writes` (§2.3) so the surface shows "3 adds pending" rather than nothing.
3. **Cap.** A per-distributor fee budget, `ENTRY_WRITE_FEE_BUDGET_PER_DAY_MOJOS`, denominated in **XCH
   mojos**, defaulting to 24 x the operator's configured standard fee — one bundle per hour. When it
   is exhausted the prover MUST stop writing, MUST NOT discard the decisions, and MUST report the
   named state `FeeBudgetExhausted` (§2.3). It MUST NOT stall silently and MUST NOT keep spending.
4. **Hysteresis.** An add requires one passing cycle; a removal requires
   `CHALLENGE_STRIKES_TO_EVICT` consecutive failures (§3.6); and a removed entry MUST NOT be re-added
   for `REENTRY_COOLDOWN_SECONDS = 21_600` (6 h). Without a cooldown a flapping peer alternates
   add/remove forever at two chain writes per flap. The cooldown MUST be keyed on
   `(payout_puzzle_hash, launcher_id)`, not on `peer_id`, because the puzzle hash is what the chain
   writes and a peer can present a new `peer_id` for the same payout address.

### 6.4 Eviction settles; it does not confiscate

Measured: `RewardDistributorRemoveEntryAction::spend` computes the entry's accrued amount and pays it
out as part of the removal, returning it (`chia-sdk-driver-0.36.0/.../remove_entry.rs:95-133`; the
signature's own comment is `u64 = last payment amount`), and it does so **without applying
`payout_threshold`**.

Consequences that MUST be stated, because both are counter-intuitive:

1. A removed mirror is paid **everything it accrued up to the removal**, including a remainder below
   `payout_threshold` that it could never have claimed itself. No dust is stranded by eviction, and an
   implementation MUST NOT add a separate flush path for it.
2. An eviction therefore **costs the reserve**, not only a fee. An operator MUST NOT be told that
   removing an entry is free, and the prover MUST NOT treat eviction as a cheap default.

### 6.5 The entry set is capped

`MAX_ENTRIES_PER_DISTRIBUTOR = 250`.

The per-mirror stream is the epoch's rewards divided across `active_shares`, and `payout_threshold`
(§8.3) is enforced by the puzzle. So an unbounded set drives every mirror's claimable amount below the
threshold and **nobody can claim at all** — the set grows until the distributor stops working. With
equal shares (§11) the funder's floor is arithmetic they can be shown before they fund:

```
minimum daily funding (base units) = entry_count x payout_threshold x (86400 / claim_cadence_seconds)
```

At the §8.3 threshold, a 24 h cadence and 250 entries that is `250 x 1_000` base units = **250 $DIG
per day**.

**This floor MUST be displayed, not merely documented.** The creation and refill surfaces MUST show
it for the distributor's current entry count, at the moment the funder chooses an amount (#3253). A
funder who cannot see the floor discovers it when mirrors stop being paid — the payout falls below
`payout_threshold`, the puzzle refuses every claim, and nothing anywhere reports an error, because
nothing is broken. That silence is why documenting the arithmetic is insufficient: this is the one
number whose absence looks exactly like normal operation.

When the set is full:

1. The prover MUST report the named state `EntrySetFull` (§2.3).
2. It MUST prefer existing entries: no churn to make room.
3. It **MUST NOT** evict a passing entry to admit a new candidate. A displacement rule is a griefing
   primitive — a candidate could unseat an incumbent by merely appearing — and it would spend the
   operator's fees to do it.

---

## 7. Custody

### 7.1 `require_payout_approval = false`

Measured: `RewardDistributorInitiatePayoutAction::spend(ctx, distributor, entry_slot)`
(`chia-sdk-driver-0.36.0/.../initiate_payout.rs:118`) carries no manager authority, and the funds go
to the `payout_puzzle_hash` recorded in the slot — not to a hash the spender chooses.

So `false` is the correct value and MUST be used:

1. It is what makes **peer self-claim** possible, which the requirement demands ("automatically
   collect the rewards at a configurable cadence").
2. `true` would put the manager singleton in the path of every claim, making the operator's uptime a
   precondition for a mirror being **paid** and not merely **evaluated** — a far larger dependency
   than §2 already describes.
3. `true` would also make the operator a censor over an individual mirror's money, which no clause of
   the requirement asks for.
4. Neither value is a theft vector: the destination is the slot's recorded hash either way. The choice
   is about who must be online, not about who can be robbed.

### 7.2 The distributor is `Managed`, and the manager singleton is the custody boundary

`RewardDistributorType::Managed { manager_singleton_launcher_id }`
(`.../reward_distributor_info.rs:85`). Add and remove are authorized only by a mode-18 message from
that singleton (`.../add_entry.rs:110-117`, `.../remove_entry.rs:100-110`). Staking is unavailable in
this mode — `.../stake.rs:177` returns *"Stake action not available in managed mode"* — so the
manager singleton's key **is** the entry set's custody boundary, and there is no second path.

1. The manager singleton MUST be controlled by the funding node's **operator wallet**. It MUST NOT be
   held by a DIG network service, a hub backend, a relay, or any shared key. This crate MUST NOT
   provide a mode in which a third party holds it.
2. The manager singleton launcher id is curried into the action puzzles and is therefore **immutable**
   for the life of the distributor. There is no key rotation.
3. **Losing the manager key freezes the entry set permanently**: no add, no remove, ever — while
   accrual and payouts to the frozen set continue permissionlessly (§2.1). This is the worst
   irreversible outcome in the design, it cannot be recovered by anyone including the funder, and the
   creation flow MUST state it (§2.2 clause 4). The funder's only remedy afterwards is
   `WithdrawIncentives` on future commitments (§7.4) and launching a new distributor.

### 7.3 `fee_bps = 0` is the MVP default, and `fee_payout_puzzle_hash` is the funder's own

The epoch fee is skimmed at `NewEpoch`: `fee = epoch_total_rewards * fee_bps / 10000`, paid to
`fee_payout_puzzle_hash` (`.../new_epoch.rs:124`). Both are curried at launch and immutable.

1. `fee_bps` MUST default to **0** in the MVP.

   **Immutability is per distributor, not per product.** Because the value is curried at launch, a
   distributor keeps the terms it was launched with for its whole life — so a later $DIG treasury
   policy would apply to distributors launched *after* it and would leave existing ones alone.
   Choosing 0 now therefore **forecloses nothing**, while a non-zero default would silently tax every
   MVP mirror before anyone had decided to levy anything, in favour of whoever's puzzle hash happened
   to be curried in. Of the two, the untaken tax is the easy one to undo and the taken one is not.

   This is an **MVP default, not a permanent property of the mechanism.** A treasury fee, if one is
   ever levied, arrives as the §7.3a policy hook and MUST NOT be introduced by changing this default
   under existing distributors, which is impossible anyway.
2. The builder MUST require an explicit opt-in to set it non-zero, and the MVP creation surface MUST
   NOT offer the field.
3. `fee_payout_puzzle_hash` MUST be the funder's own refund/change puzzle hash — the same one passed
   as `cat_refund_puzzle_hash`. It MUST NOT be a zero hash (a later non-zero fee would burn to it) and
   MUST NOT be the DIG treasury (with `fee_bps = 0` that is inert, and it misleads a reader into
   believing the treasury takes a cut).

### 7.3a The policy hook, named

If a protocol-level fee is ever levied, this is the only shape it may take, and stating it here is
what keeps §7.3's default from being mistaken for a decision nobody may revisit.

1. The launch builder MUST read `fee_bps` and `fee_payout_puzzle_hash` from a single named policy
   input with the MVP default of `(0, <the funder's own refund puzzle hash>)`. An implementation MUST
   NOT scatter the two values across call sites, because a policy that cannot be changed in one place
   will be changed in two and disagree.
2. A change to that policy MUST apply **only to distributors launched after it**. An implementation
   MUST NOT attempt to apply a new fee to an existing distributor — it is curried, so the attempt can
   only produce an unspendable construction — and MUST NOT present a fee change as retroactive.
3. A non-zero policy MUST be surfaced at creation before the launch spend is signed, in the same
   place §2.2's warning appears, stating the rate and that it is permanent for this distributor.
4. This crate MUST NOT hard-code the DIG treasury puzzle hash for this purpose. The treasury is not a
   party to a distributor anyone may mint (§7.3 clause 3), and a hard-coded recipient is a policy
   decision smuggled in as a constant.

### 7.4 Clawback: `CommitIncentives` is the funding path, `AddIncentives` is a donation

There are two ways to put $DIG into a distributor and only one is recoverable.

- `AddIncentives` adds to the **current** epoch's rewards. It creates no commitment slot, so there is
  nothing to withdraw against: it is an **irrevocable donation**.
- `CommitIncentives` creates a commitment slot
  `RewardDistributorCommitmentSlotValue { epoch_start, clawback_ph, rewards }`
  (`chia-sdk-types-0.36.0/.../slot_values.rs:420-425`), keyed by the epoch it funds and recording the
  puzzle hash entitled to claw it back.

Therefore:

1. The default "fund" action MUST use `CommitIncentives`, per future distributor epoch. Only this
   makes the requirement's "ability to clawback funds from the distributor" true.
2. `AddIncentives` MUST be exposed under a different, clearly irrevocable label, and MUST NOT be the
   default. An implementation that funds with `AddIncentives` and offers a clawback button is lying.
3. **Who may claw back:** the holder of the key for the commitment slot's recorded `clawback_ph`, and
   nobody else. That is the proof — a recorded puzzle hash from the commit spend, not an operator
   role, not the manager singleton, and not the launcher.
4. Clawback returns `withdrawal_share_bps / 10000` of the committed value
   (`withdraw_incentives.rs:71`); the remainder stays in the reserve for the mirrors.
5. A clawback is per commitment slot, so the UI MUST present the funder's commitments **per epoch**
   with the recoverable amount computed per slot. A single "balance" figure cannot express which part
   is recoverable, and presenting one is the money-honesty failure of this section.

### 7.5 `withdrawal_share_bps = 9000`

The committer recovers 90% of a withdrawn commitment and forfeits 10% to the reserve.

1. Not `10000`: a costless retraction lets a funder advertise a large reward, induce mirrors to lock
   real $DIG collateral and spend real bandwidth, then withdraw everything. The mirrors' costs are
   unrecoverable; the funder's would not be. 10% is the smallest retraction cost that is not zero.
2. Not lower: this is a funder's own money, and a large penalty deters funding — the behaviour the
   whole epic exists to encourage.
3. `9000` is also the value the upstream reference flow exercises
   (`launch_drivers.rs:2932`, commented *"90% of the amount deposited will be returned"*), so the
   chosen path is the tested one.

---

## 8. Epoch mechanics

All four values are curried at launch and immutable. The creation surface MUST say so, per field.

### 8.1 `epoch_seconds = 604_800` (7 days), per distributor, defaulted

1. **Funding and operational cost scale with the epoch COUNT, not with epoch length.**
   `CommitIncentives` creates one commitment slot per epoch funded, and `NewEpoch` is one spend per
   rollover. Funding a quarter at 7-day epochs costs 13 commitment slots; at 1-day epochs, 91.
2. **A long epoch does not delay any mirror's earnings.** Accrual inside an epoch is continuous — the
   `Sync` action advances `cumulative_payout` for elapsed time
   (`chia-sdk-types-0.36.0/.../sync.rs:38-42`; the arithmetic is the puzzle's) — and a mirror joining
   mid-epoch starts accruing from its join time, because `AddEntry` snapshots
   `initial_cumulative_payout` from the current state (`.../add_entry.rs:88-100`). So epoch length is
   not a payout latency.
3. Seven days matches the mirror-collateral epoch length
   (`modules/crates/00-foundation/dig-mirror-collateral/src/constants.rs:216-217`), so a mirror coin's
   useful life and a distributor epoch have the same duration and eligibility does not churn against
   the reward accounting on a different beat. **Only the length matches; the phases are independent
   and MUST NOT be aligned or assumed aligned** (§0.3).
4. Seven days is short enough that a funder can change the funding rate within a week, and long enough
   that a rollover is a weekly event rather than background noise.

`epoch_seconds` MUST be a settable parameter with this default; it MUST NOT be hard-coded, because a
funder with a different funding rhythm has no other lever.

**`epoch_seconds` and the claim cadence measure different things, and MUST NOT be reconciled.** The
24 h figure in §8.6 is the *peer's* polling interval — how often a mirror asks to be paid. The
604,800 here is the *distributor's* reward-accrual window — how the funder's commitments are
partitioned in time. They are not two settings for one quantity, and making them equal buys nothing:

1. Accrual is continuous inside an epoch (clause 2), so a mirror does not wait for an epoch boundary
   to earn, and a shorter epoch would not pay it sooner.
2. A mirror claiming daily against a 7-day epoch is the *normal* case, not a mismatch. It collects
   whatever accrued since its last claim, seven times per epoch.
3. Setting `epoch_seconds = 86_400` "to match the cadence" multiplies the funder's commitment slots
   and `NewEpoch` spends by seven for no change in what any mirror receives.

An implementation MUST NOT derive either value from the other, and a later change to §8.6's cadence
MUST NOT propagate here. This clause exists because "these two numbers should agree" is the plausible
wrong fix, and it is the one a reader arrives at without reading clause 2.

### 8.2 `max_seconds_offset = 300`

Measured: the add-entry and remove-entry puzzles assert
`ASSERT_BEFORE_SECONDS_ABSOLUTE(last_update + max_second_offset)`
(`chia-sdk-types-0.36.0/.../add_entry.rs:41-46` currying `max_second_offset`; the same field at
`.../remove_entry.rs:53`). So the distributor state must have been `Sync`ed within that window for an
entry-set write to be valid at all.

1. Every entry-set bundle MUST therefore include a `Sync` (or a `NewEpoch`) in the same bundle. An
   implementation that omits it produces a bundle the chain rejects, and the operator pays for the
   attempt.
2. Not larger: a new entry's `initial_cumulative_payout` is snapshotted from the last-synced state, so
   a stale state pays a brand-new entry for time before it joined, at every existing entry's expense.
3. Not smaller: a bundle that misses the window is rejected and the fee is wasted. 300 s is a
   realistic mainnet mempool window, and is the value the upstream reference flow uses
   (`launch_drivers.rs:2926`).

### 8.3 `payout_threshold = 1_000` base units = 1.000 $DIG

`payout_threshold` is curried into both `InitiatePayout` variants
(`.../initiate_payout.rs:23,60`) and the puzzle refuses a payout below it.

1. Not `0`: a zero threshold lets a peer spend `InitiatePayout` for dust, and every claim spends the
   entry slot and re-creates it (`initiate_payout.rs:101,107`). The claim's network fee would exceed
   its value, so a zero threshold is a way for a peer to lose money on purpose and to bloat the chain
   doing it.
2. Not larger: a high threshold silently withholds a small mirror's earnings until they accumulate,
   which the mirror experiences as not being paid. 1 $DIG is above any plausible fee and small enough
   that a mirror earning at the §6.5 floor clears it daily.
3. It sets the funder's floor, which §6.5 makes explicit and which #3253 MUST display.
4. Eviction bypasses it (§6.4), so a below-threshold remainder is never stranded.

### 8.4 `precision = u64::MAX`

`cumulative_payout` and `remaining_rewards` are `u128` accumulators **scaled by `precision`**. A
payout is `shares x (cumulative_payout - initial_cumulative_payout) / precision`, and the modulus is
carried as `payout_rounding_error` and left in the reserve
(`.../initiate_payout.rs:118-128`, `.../remove_entry.rs:113-124`).

1. `precision` exists so the per-share rate does not truncate to zero when `active_shares` is large
   relative to the epoch's rewards. A small value silently pays nobody.
2. `u64::MAX` is the largest value the field can carry and is what the upstream reference flow uses
   (`launch_drivers.rs:2926,4764`), so the arithmetic follows the tested path.
3. **This value is safe only because §11 fixes `shares = 1`.** The product
   `shares x (cumulative_payout - initial_cumulative_payout)` is computed in `u128`; with `shares = 1`
   and the §6.5 entry cap it stays far inside the type. A weighted-shares policy would multiply that
   product by the weight and MUST re-derive this bound before changing §11. An implementation MUST NOT
   change one of §8.4 and §11 without the other.
4. `precision` MUST NOT be exposed as a user-editable field. It is not a preference; a wrong value
   breaks payouts arithmetically and immutably.

### 8.5 `first_epoch_start`

1. MUST NOT be in the past at broadcast. `RewardDistributorState::initial(first_epoch_start)` sets
   both `last_update` and `epoch_end` to it (`.../reward_distributor_info.rs:61-76`), and the `Sync`
   puzzle requires `update_time > last_update` and `update_time <= epoch_end`
   (`chia-sdk-types-0.36.0/.../sync.rs` — the puzzle raises otherwise), so a start in the past means
   the first epoch is already partly elapsed before any entry exists.
2. MUST default to `now + FIRST_EPOCH_START_LEAD_SECONDS = 600` — ten minutes of lead, so the launch
   spend can confirm before the first epoch is running.
3. The value used MUST be recorded and displayed; it is the origin of every epoch boundary for the
   life of the distributor.

### 8.6 The claim cadence is the peer's, not the distributor's

The 24 h default claim cadence (#3251) is a **peer-side** setting and is not curried anywhere. It MUST
be configurable per peer, MUST default to `CLAIM_CADENCE_SECONDS = 86_400`, and MUST be jittered by at
least `CLAIM_JITTER_SECONDS = 3_600` so that a network of peers sharing a default does not converge on
one minute of the day and self-congest. A claim MUST be skipped, not failed, when the accrued amount
is below `payout_threshold`.

---

## 9. Reserve asset

### 9.1 The constant, never a literal

`reserve_asset_id` MUST be `dig_constants::DIG_ASSET_ID`
(`modules/crates/00-foundation/dig-constants/src/lib.rs:353`).

1. A typed hex literal for the $DIG asset id anywhere in this crate, its tests, its fixtures, or its
   consumers is a **defect**. The constant carries a byte-identical contract with
   `chip35_dl_coin::DIG_ASSET_ID`, digstore-chain and DataLayer-Driver, with a guard test at
   `dig-constants/src/lib.rs:858`.
2. This crate MUST NOT re-export a second copy of the value under its own name. `dig-mirror-coin`
   re-exports one for its own callers (`dig-mirror-coin/src/lib.rs:93`); a third alias is how the
   contract drifts.
3. The asset id MUST NOT be a runtime parameter of a DIG rewards distributor. There is exactly one
   reward asset.

### 9.2 The derived reserve puzzle hashes

`reserve_inner_puzzle_hash` and `reserve_full_puzzle_hash` MUST be produced by
`RewardDistributorConstants::with_launcher_id`, which derives them from the launcher id and the asset
id (`.../reward_distributor_info.rs:207-215`). They MUST NOT be set by hand, computed locally, or
carried in configuration. `without_launcher_id(..)` then `.with_launcher_id(..)` is the only
construction path.

### 9.3 A non-$DIG distributor is not ours

A CHIP-0051 distributor whose on-chain `reserve_asset_id` is not `DIG_ASSET_ID`, or whose launch
comment does not parse per §1.3, MUST be treated as **not a DIG rewards distributor**: excluded from
discovery (§13), absent from the UI, and never claimed against by the peer claim loop. Other
distributors legitimately exist — the DIG Alpha Test Rewards distributor behind hub.dig.net's
`/quest?tab=stake` surface is one (`SYSTEM.md:484`) — and showing a foreign asset's balance to a
funder as though it were $DIG is a money lie.

---

## 10. Identity binding

### 10.1 The binding already exists on chain

**No new signing scheme.** The `peer_id` -> `payout_puzzle_hash` binding is the §4.3 three-call chain
over `dig-mirror-coin`:

1. the DHT names a `peer_id` supplying this `storeId:root` (§4.1);
2. the mirror coin `advertises(store, root, mirror_collateral_epoch)` **and**
   `declares_peer(peer_id)` (§4.3);
3. the entry pays `MirrorCoin::owner_puzzle_hash()` (§4.3, §10.2).

An earlier draft of the epic recommended building this binding out of `dig-gossip`'s holdings-announce
leaf-key signature. **That recommendation is superseded and MUST NOT be implemented.** The mirror-coin
declaration is already an owner attestation carried by executed on-chain code, already collateralised,
and already epoch-bound; adding a signature beside it would be a redundant, weaker mechanism with its
own key handling and its own failure modes. `dig-gossip` holdings-announce reuse remains correct for
**distributor discovery** (§13.2, #3252), which is a different job.

### 10.2 An entry is keyed by a PUZZLE HASH, never a pubkey

The requirement's guess — "adding and removing pubkeys of the peers" — is wrong, and it is written
here explicitly so that no implementer reintroduces it.

An entry is `RewardDistributorEntrySlotValue { counter, payout_puzzle_hash: Bytes32,
initial_cumulative_payout, shares }`
(`chia-sdk-types-0.36.0/src/puzzles/action_layer/slot_values.rs:431`), and `AddEntry` takes
`entry_payout_puzzle_hash: Bytes32` (`chia-sdk-types-0.36.0/.../add_entry.rs:50-56`).

1. The entry key is a **`Bytes32` payout puzzle hash**. It is not a public key, not a BLS pubkey, not
   a peer id, and not an address string. An implementation MUST NOT accept, store, or display a pubkey
   in this position.
2. Its value MUST be `MirrorCoin::owner_puzzle_hash()` (`dig-mirror-coin/src/coin.rs:93`), which
   derives from the coin's **lineage proof** — executed on-chain code — and not from a memo an owner
   wrote. This is why there is no separate `payout_puzzle_hash` message for an attacker to substitute:
   the prover never receives a payout address from anyone. It derives one.
3. `counter` is the slot's replay guard; `InitiatePayout` writes `counter + 1`
   (`.../initiate_payout.rs:101,107`). A caller MUST NOT cache a slot value across claims (§12.5).
4. The distributor knows nothing about peer identity. Every statement tying a payment to a peer lives
   in the mirror coin, and if that coin is gone so is the tie.

### 10.3 An unmatched or absent declaration is ineligibility, fail-closed

1. No mirror coin found, no `unverified_mirror_coin_id` pointer, a coin that fails any §4.2 check,
   `advertises` false, or `declares_peer` false, in any combination: the candidate is **not eligible**.
2. There is **no fallback to a weaker path.** Not a nomination message, not a self-declared payout
   address, not a signature, not an operator override, not "eligible with reduced shares", not
   "provisionally eligible". An implementation MUST NOT provide any of these, and MUST NOT add a
   configuration flag that admits a candidate without the chain binding.
3. Ineligibility is **not** an accusation: it produces no strike, no blocklist entry, and no log line
   that reads as misconduct (§4.2). It most often means the peer has not created its coin yet.
4. The absence of a declaration MUST NOT be read as evidence of anything at all — every mirror coin
   created before the declaration format existed carries none
   (`dig-mirror-coin/SPEC.md` §5.1 rule 8).

---

## 11. Shares policy

### 11.1 Every passing mirror gets exactly one share

`entry_shares = 1`, for every entry, always. The add-entry puzzle requires `shares > 0` — it raises
otherwise — so `0` is not available; and a DIG driver MUST pass `1` and MUST refuse any other value.

The reasons, in order:

1. **Nothing a prover could weight by is verifiable.** Claimed bandwidth, claimed storage, claimed
   uptime and self-reported region are all peer-supplied. Weighting by a lie the prover cannot check
   is worse than not weighting.
2. **Weighting by collateral turns a mirror reward into a yield on locked $DIG.** The richest mirror
   would take most of the reserve without serving more bytes, which inverts the incentive the funder
   is paying for.
3. **The evidence is binary, so the share must be.** §3 answers exactly one question: did the peer
   return the exact bytes for windows it could not predict? A binary test can only justify a binary
   share; a graded share would be a number with no measurement behind it.
4. **Equal shares make the funder's arithmetic knowable in advance.** The per-mirror stream, the
   §6.5 funding floor, and the §8.3 threshold interaction are all computable before funding. Under
   weighting, the floor depends on a distribution the funder cannot see at funding time.
5. **It pins the `precision` overflow bound** (§8.4).

### 11.2 What equal shares cost, stated plainly

A peer serving a 10 TB generation and a peer serving a 1 KB generation earn the same from their
respective distributors, and two peers mirroring the *same* generation earn the same regardless of how
much bandwidth each actually serves.

That is correct here because the unit of work is **"mirror this generation"**, which §3.2's
length-proportional sampling makes all-or-nothing: a peer holding only part of the generation fails.
There is no partial mirror to pay partially. Serving *volume* is a different service and is not what
this distributor buys.

### 11.3 Changing this is a spec change, not a config change

`entry_shares` MUST remain in the crate's API because the SDK requires it, but the DIG path MUST pass
`1`. A weighted policy would change who gets the reserve, so it MUST arrive as a revision of this
section together with §8.4, and MUST NOT be introduced as a configuration flag, an experiment, or a
per-distributor option.

---

## 12. Recovery

### 12.1 Node restart

1. The prover MUST rebuild every distributor's state **from the chain**, never from local cache alone:
   `RewardDistributor::{from_launcher_solution, from_parent_spend, from_spend}`, with slots found by
   hint, then the parent's `get_puzzle_and_solution`, then `RewardDistributor::from_spend`, then the
   `pending_created_*_slots` accessors. **This recipe is already in production** for a CHIP-0051
   distributor in this ecosystem — hub.dig.net's `/quest?tab=stake` surface reads distributor state,
   reward/commitment/entry slots and locked NFTs exactly this way (`SYSTEM.md:484`,
   `apps/web/features/staking/config.ts`, `lib/rewards-distributor.ts`). An implementation MUST reuse
   that shape rather than invent a second slot-discovery path.
2. Local prover state is a **cache and advisory**. On restart:
   - challenge **strikes MUST reset to zero** — a strike is evidence about a specific recent window
     the prover no longer holds, and carrying it forward evicts on evidence nobody can re-examine;
   - **cooldowns and fee budgets MUST persist** — they exist to bound chain writes, and losing them
     re-opens the churn drain (§6.3) at exactly the moment a restart loop would hit it hardest.

   Lose the evidence, keep the bounds. An implementation that persists strikes, or that drops
   cooldowns, has the asymmetry backwards.
3. On restart the prover MUST publish a status record (§2.3) before its first cycle completes, in the
   `Running` state with `last_cycle_completed_at` absent. An absent record and a never-completed cycle
   are different facts and MUST render differently (§2.4).

### 12.2 Chain reorg

1. The prover MUST treat its own submitted bundle as unconfirmed until it is buried by
   `dig_mirror_collateral::CENSUS_FINALITY_DEPTH_BLOCKS = 32`
   (`modules/crates/00-foundation/dig-mirror-collateral/src/constants.rs:216-218`). That constant is
   the ecosystem's finality depth; this crate MUST reuse the name and MUST NOT introduce a second
   finality number.
2. On a reorg that unwinds an entry write, the prover MUST re-derive the entry set from the new chain
   view and decide again. It MUST NOT replay the old bundle. A replayed spend is invalid anyway — the
   slot `counter` is the guard (§10.2 clause 3) — so a blind replay only wastes a fee.
3. A reorg MUST NOT produce a challenge strike (§3.6 clause 4) and MUST NOT trigger an eviction.
4. A reorg MUST be surfaced as `ChainSourceUnavailable` or a `consecutive_cycle_failures` increment,
   never as a peer-side failure.

### 12.3 A distributor funded with no live prover

This is §2.1's state and it is fully observable on chain. The remedies are: run the prover, or
`WithdrawIncentives` the future commitments (§7.4). Discovery (§13) MUST NOT present a distributor as
"active" on the basis of a non-zero reserve alone.

### 12.4 The stale-entry-set signal is computed from the chain, not reported by the operator

A distributor whose entry set has not changed in `STALE_ENTRY_SET_SECONDS = 172_800` (48 h) while its
reserve is non-zero MUST be reported as `EntrySetStale` to anyone reading it — funder or mirror.

The last entry-write time is derivable from the singleton's own spend history, so this signal is
**not** a self-report and cannot be faked by a wedged prover. That is the point: liveness that matters
to a counterparty MUST be computed from the chain. A mirror deciding whether to lock $DIG collateral
to chase a reward needs a signal the funder cannot flatter, and the operator's RPC (§2.6) is not that
signal.

### 12.5 A peer claiming after eviction

`RemoveEntry` spends the entry slot and settles the accrued amount (§6.4). After eviction there is no
slot to claim against, so `InitiatePayout` cannot be built — correctly, and with nothing owed.

1. A claim loop MUST treat "entry slot absent" as a **terminal, non-error** outcome for that
   distributor: stop retrying, do not report a chain fault, do not report a lost payment.
2. Re-entry requires passing the challenge again and waiting out `REENTRY_COOLDOWN_SECONDS` (§6.3).
3. A claim loop MUST re-read the entry slot before every claim and MUST NOT cache a slot value across
   cycles: `counter` increments on each payout, so a cached value produces an invalid spend and a
   wasted fee.

### 12.6 Reserve exhausted

`remaining_rewards` reaching zero is **not an error**. Payout amounts go to zero and entries stay.

1. The prover MUST report the named state `Unfunded` (§2.3).
2. It MUST keep the entry set. Evicting 250 entries because the money ran out would cost the operator
   250 chain writes' worth of fees to punish nobody, and would force every honest mirror through the
   re-entry cooldown when the distributor is refilled.
3. A refill MUST resume accrual with the existing set intact.

---

## 13. Discovery

### 13.1 On-chain discovery is sufficient and is the MVP path

1. A distributor is discoverable from its launch: the singleton's launch comment carries
   `dig-rewards:v1:<store_id_hex>:<root_hex>` (§1.3), so any party can map a launcher id to the
   generation it rewards, and a peer can decide whether it has a claim.
2. A peer MUST be able to find, evaluate and claim from a distributor using nothing but a chain
   source. No off-chain component may be a precondition for being paid.
3. A reader MUST apply §9.3 before treating a discovered distributor as ours.

### 13.2 Off-chain discovery is an optimisation, and it is deferred (#3252)

The peer network SHOULD help peers discover distributors faster, by extending `dig-gossip`'s
holdings-announce (opcode 222,
`modules/crates/00-foundation/dig-peer-protocol/src/opcodes.rs:48`) with a distributor hint.

The invariant that makes this deferral safe, and which MUST hold whenever it does ship:

1. A gossip hint is an **untrusted pointer**, exactly like `unverified_mirror_coin_id`. Every property
   MUST be re-derived from the chain. A hint MUST NOT admit an entry, MUST NOT rank a candidate, and
   MUST NOT be a claim's authority.
2. A peer that never hears a hint MUST still find and claim from the distributor via §13.1. The hint
   reduces discovery **latency**; it MUST NOT become a gate on getting paid.
3. Self-exclusion (§5.3) applies to this path like every other.

### 13.3 `Refresh` is not used, and in this mode it has no meaning

The MVP note defers `Refresh` support. Measured, the deferral costs nothing:
`RewardDistributorRefreshAction` is the NFT/DataLayer refresh
(`chia-sdk-driver-0.36.0/.../refresh.rs:28`, built from `RefreshNftsFromDl*` types), which belongs to
the `CuratedNft { refreshable }` mode and has no semantics for a `Managed` distributor. This crate MUST
NOT expose it.

---

## 14. MVP subset versus deferred

Every section above is normative in full. This section governs **shipping order only**. Nothing here
removes a clause from the specification; a clause silently left in scope and then not built, and a
clause quietly deleted to make the MVP look complete, are both the failure this table prevents.

| section | MVP | note |
|---|---|---|
| §1 Local-holding precondition | **ships** | including §1.4 `LocalCopyMissing` and the §1.5 root-advance refusal |
| §2 Liveness honesty | **ships** | not polish. §2.2's warning and §2.3-§2.6's surface are money-honesty clauses and are **not deferrable** |
| §3 Challenge soundness | **ships** | full sampling, deadlines, strike rule |
| §4 Mirror-coin gate | **ships** | via the §4.2 pointer path. The hint-scan / census fallback for a mirror that publishes no pointer is **specified, deferred — #3258**; until it lands, a mirror that does not announce with collateral cannot be paid (§14.1) |
| §5 Self-exclusion | **ships** | both coordinates, every path, with the §5.3 clause-4 control test |
| §6 Sybil and eviction economics | **ships** | all four bounds, the §6.4 settlement statement, the §6.5 cap |
| §7 Custody | **ships** | `require_payout_approval = false`, `fee_bps = 0`, `CommitIncentives` as the default fund action, per-epoch clawback presentation |
| §8 Epoch mechanics | **ships** | all four constants at their stated defaults |
| §9 Reserve asset | **ships** | |
| §10 Identity binding | **ships** | it is §4's mechanism; there is nothing separate to build |
| §11 Shares policy | **ships** | `shares = 1`, refused otherwise |
| §12 Recovery | **ships** | §12.4's chain-derived `EntrySetStale` included; it is what a mirror reads instead of trusting an operator |
| §13.1 On-chain discovery | **ships** | |
| §13.2 Off-chain discovery | **specified, deferred** | **#3252**. Safe to defer only because §13.2 clause 2 holds |
| §13.3 `Refresh` | **specified, not used** | inapplicable in `Managed` mode; nothing to defer |
| metrics presentation beyond the §2.3 counters | **specified, deferred** | **#3253** ships create-with-warning, refill, clawback and prover health; per-epoch payout history and charting follow |

Driver action coverage for the MVP (#3249): `launch_reward_distributor`, `AddIncentives`,
`CommitIncentives`, `WithdrawIncentives`, `AddEntry`, `RemoveEntry`, `NewEpoch`, `Sync`,
`InitiatePayout`, and sync-from-chain. `Stake` / `Unstake` are unavailable in `Managed` mode (§7.2) and
`Refresh` is §13.3.

### 14.1 The one MVP limitation a reader must not discover later

Because §4.2 is the only eligibility path in the MVP, **a mirror whose provider record carries no
`unverified_mirror_coin_id` cannot be paid in the MVP**, even though it holds the bytes, holds a valid
mirror coin, and would pass the challenge.

This is a fail-closed limitation that withholds reward rather than granting it, so it is safe — but it
is not invisible, and it MUST be handled rather than left to be found:

1. A mirror MUST announce with `DhtService::announce_provider_with_collateral(content, Some(coin_id))`
   (`modules/crates/20-domain/dig-dht/src/service.rs:308`) and MUST re-announce with the new coin id
   across each mirror-collateral epoch rollover. #3254's mirror-operator documentation MUST say so.
2. The funder-facing surface MUST distinguish "no eligible mirrors" from "no mirrors found", so an
   operator is not left concluding nobody is mirroring their store when the real state is that nobody
   published the pointer.
3. The hint-scan / census fallback that removes the limitation is
   **DIG-Network/dig_ecosystem#3258** — specified as deferred, not dropped. Until it lands, clauses 1
   and 2 are what stop this limitation from reading to a funder as "nobody is mirroring my store".

---

## 15. Conformance

An implementation conforms when all of the following hold.

1. **Layering and purity.** No socket I/O, no keys, no broadcast, chain reads through a
   caller-supplied chain source, unsigned spends out (§0.1). No dependency on `dig-epoch` (§0.3).
2. **Version pinning.** The 0.36 cohort is pinned and the §0.5 puzzle-hash guard test covers every
   action used.
3. **Constants.** A launched DIG distributor carries exactly:

   | field | value | section |
   |---|---|---|
   | `reward_distributor_type` | `Managed { manager_singleton_launcher_id }` | §7.2 |
   | `fee_payout_puzzle_hash` | the funder's own refund puzzle hash | §7.3 |
   | `epoch_seconds` | `604_800` (default; settable) | §8.1 |
   | `precision` | `u64::MAX` | §8.4 |
   | `max_seconds_offset` | `300` | §8.2 |
   | `payout_threshold` | `1_000` base units | §8.3 |
   | `require_payout_approval` | `false` | §7.1 |
   | `fee_bps` | `0` | §7.3 |
   | `withdrawal_share_bps` | `9_000` | §7.5 |
   | `reserve_asset_id` | `dig_constants::DIG_ASSET_ID` | §9.1 |
   | `reserve_inner_puzzle_hash`, `reserve_full_puzzle_hash` | derived by `with_launcher_id` | §9.2 |
   | `launcher_id` | set by `with_launcher_id` | §9.2 |

   built `without_launcher_id(..)` then `.with_launcher_id(..)`, in that order and no other
   (`.../reward_distributor_info.rs:180-215`).
4. **Eligibility.** Every entry admitted passed §3's challenge, §4's three-call chain, §4.5's
   transport pinning, and §5's self-exclusion — with no path that bypasses any of them (§5.3, §10.3).
5. **Entry shape.** Every entry carries `payout_puzzle_hash = MirrorCoin::owner_puzzle_hash()` and
   `shares = 1`. No pubkey appears in an entry position (§10.2, §11.1).
6. **Observability.** The §2.3 record exists per distributor, contains no health boolean (§2.4), uses
   only the closed state set, and heartbeats at least every `PROVER_HEARTBEAT_SECONDS`.
7. **Bounds.** Every named bound in §3.2, §3.7, §4.4, §6.3 and §6.5 is enforced, and each exhaustion
   is a named, reported state rather than a silent stop.
8. **Asymmetry.** Absence of evidence never evicts: §1.4, §3.6 clause 4, §12.2 clause 3.
9. **Evidence bar.** The crate's correctness is demonstrated by a **simulator** test modelled on
   `chia-sdk-driver-0.36.0/src/primitives/action_layer/launch_drivers.rs:2738`
   `test_managed_reward_distributor()` — launch, fund, add entry, roll epoch, self-claim payout,
   remove entry and observe the settlement of §6.4 — not by a mock.

### 15.1 Which side each clause lands on

- **This crate (#3249):** §0.1, §0.2, §0.5, §1.3, §4.3's call sequence as a reusable predicate, §6.4's
  settlement amount, §7, §8, §9, §10.2, §11, §12.1 clause 1, §12.5 clause 3, §15.3.
- **`dig-node` prover (#3250):** §1.1-§1.2, §1.4-§1.5, §2, §3, §4.1-§4.2, §4.4-§4.7, §5, §6.3, §6.5,
  §12.1-§12.4, §12.6, §13.1.
- **`dig-node` claim loop (#3251):** §8.6, §12.5.
- **`dig-gossip` (#3252):** §13.2.
- **`dig-app` (#3253):** §1.5 clause 4, §2.2, §2.4 clauses 1-3, §6.5's funding floor **as a displayed
  value**, §7.3a clause 3, §7.4 clause 5, §14.1 clause 2.
- **Docs (#3254):** §2.2, §14.1 clause 1.
- **`dig-rpc-protocol`:** the three §2.6 methods.

### 15.2 Status of this document

Every clause above is **specified, not yet implemented**: at the time of writing, `main` of
`dig-rewards-coin` holds only the repository bootstrap and the crate scaffold
(DIG-Network/dig_ecosystem#3247). Every `file:line` citation in this document is to the **pinned
upstream SDK** (`chia-sdk-driver-0.36.0`, `chia-sdk-types-0.36.0`) or to an **existing sibling crate**
in `dig_ecosystem`, measured on 2026-09-08. No citation is to code this specification introduces.

### 15.3 Open item — the mirror-collateral epoch calendar has no owner (#3259)

The **mirror-collateral epoch calendar** (§4.6 clause 2) has no owner in the tree: `dig-mirror-coin`
takes the epoch start as an input (`dig-mirror-coin/SPEC.md:371-372`) and `dig-mirror-collateral`
computes requirements from an ordinal it is given. The prover needs a supplier for that ordinal before
§4.6 can be enforced.

1. **This crate MUST NOT become that supplier.** A rewards crate deciding the collateral calendar is
   exactly the wiring §0.3 forbids, and it would make every consumer of the calendar depend on a
   rewards crate to learn what epoch it is.
2. **The interim rule**, which holds until an owner exists: an implementation MUST take the ordinal
   from configuration, and MUST report `ChainSourceUnavailable` rather than guessing when it is
   absent. Guessing here silently shifts the acceptable epoch window, which admits coins the census
   excludes or excludes coins it counts — in either direction the money goes to the wrong set.
3. **DIG-Network/dig_ecosystem#3259** carries the ownership question. When it lands, the named owner
   replaces clause 2's configuration input; clause 1 is unaffected by its outcome and stays.
