# ZEB-916 (R6b) — Summary-first exchange with reverse delta: study & go/no-go

**Date:** 2026-08-14 · **Ticket:** ZEB-916 (study), parent epic ZEB-909 · **Branch:** landed direct to `main` (doc + comment-only).
**Method:** three parallel code-verified audits over `harmony-client` (`src-tauri/src`), plus a deep voting sub-audit. Every claim below is cited to `file:line`; the load-bearing mechanism (per-segment random key ⇒ non-convergent `root_cid`) was re-verified by hand at `community_state_sync.rs:3527-3569`.
**Reference:** `docs/research/2026-08-11-freenet-architecture-review.md` §1.4, §2.4, §5 (R6b); Freenet incident #4857 (~20M spurious full-state heals from nondeterministic summary bytes).

---

## Verdict in one paragraph

The determinism half of this ticket (Q3) is a **clean pass**: there are **zero at-risk staleness paths** in the community/voting/channel planes. The Freenet #4857 failure class is *structurally absent*, not merely mitigated — the one plane that could exhibit it (membership state-root) never byte-compares roots across peers, and the one plane that does carry a fingerprint (channel-log RBSR) uses a provably order-independent accumulator over signature-covered bytes. The only real findings are **two stale comments** that assert determinism properties the code no longer guarantees — both corrected in this change as cheap insurance against a future optimizer re-introducing #4857. On the adoption half (Q2), the summary/reverse-delta interface is a **NATURAL fit for community membership state** and a **PARTIAL fit for the voting log** — but with a sharp caveat that reframes the go/no-go: the *forward* half of the mechanism already exists (channel-log RBSR is in-tree and pull-only), and the ZEB-814 segment layer already gives blob-level dedup of sealed history, so the marginal win of adopting RBSR for community state is concentrated in the *live tail and cold/partial-sync peers* — exactly the volume Q1 would measure, and Q1 is fleet-blocked. **Recommendation: NO-GO on community-state adoption pending Q1 fleet measurement; GO (separate ticket) on bringing RBSR to the voting log, which is the one plane paying a real, unbounded full-dump cost today.**

---

## Q3 — Determinism audit (the "while we're in there" pass)

**Result: 3 staleness paths in the membership plane, 0 at-risk. No `HashMap`/`HashSet` iteration, unsorted `Vec`, or float reaches any equality-bearing hash on any cross-peer path.**

### The membership state-root plane carries no set-content bytes

The wire payload (`CommunityRootPublishPayload`, `community_state_sync.rs:224`) is a `root_cid` + publisher addr + HLC + signature + epoch + format tag — **zero information about set contents**. "Am I stale?" is never answered by comparing bytes. It is answered by:

1. **HLC replay admission** (`community_state_sync.rs:4649-4659` → `harmony-crdt-sync/replay_admission.rs:240`) — `tracker.admit(&(publisher_addr, device_id), &payload.at)`. Structured `Hlc` compare (`Ord` on `(u64, u32, String)`, `owner_state_types.rs:347`), backed by `BTreeMap`. **CANONICAL.**
2. **Unconditional post-admit fetch-and-merge** (`community_state_sync.rs:4668-4674`) — on `Accept`, always fetch `root_cid` from CAS and merge every event; never gated on "does this root differ from mine". The engine retains no local root to compare. Merge is idempotent (`contains_event` → skip, `:5020`) and the inbound path never calls `notify_dirty()` — an inbound publish cannot trigger an outbound one, so **no two-peer publish loop can exist regardless of what either side computes.**
3. **Timer/epoch/presence-driven anti-entropy re-query** (`channel_backfill.rs:774+`, latch `harmony-crdt-sync/backfill_latch.rs:294`) — satisfaction = "≥1 responder replied"; inspects no state, compares no digest. **CANONICAL (n/a).**

`root_cid` derivation is itself canonical (`encode_root_packet`, `community_state_sync.rs:3450`): snapshot → `sort_by(event_sort_key)` (`:3529`, a total order `(wall_ms, logical, device_id, EventId, sig)` per `community_membership.rs:2273`) → segment → seal. The `CommunityState.log` serde shim (`community_state_crdt.rs:52`) deliberately collects into `BTreeMap<EventId, &Event>` for byte-transparency — the exact pattern the voting persist path does *not* follow (see below).

### Channel-log RBSR is order-independent by construction (confirmed, not assumed)

`RangeFingerprint` (`channel_rbsr.rs:53-118`): `fold`/`combine` both delegate to `add_mod_256` (LE 256-bit modular add discarding carry); `finalize` = `SHA256(raw_sum ‖ leb128(count))[..16]`. A commutative + associative sum → ranges aggregate from sub-range summaries in any order. Per-element input is `event_element_hash = SHA256(signed_set_canonical_cbor(event))` (`community_channel_log.rs:619`) — **the exact bytes the signature covers**, so any peer that verified an event necessarily agrees on its fingerprint input; determinism is enforced by signature verification, not convention. Pinned by `fingerprint_is_order_independent_and_associative` (`channel_rbsr.rs:462`); count-fold defeats hash-cancellation forgery (`:489`). The chunk index (`channel_chunk_index.rs`) is structurally immune — boundaries never cross the wire, only fingerprint *results*. **No raw-serialized-bytes staleness compare exists on the channel-log path.**

### Voting is immune by *omission*

The voting plane has **no fingerprint at all**. It re-ships the entire event log every `backfill_interval` (300s, `lib.rs:57921`) via a pull-based Zenoh full-dump (`read_backfill_frames`, `community_voting_log_engine.rs:543` — one CBOR frame per event, no watermark), and the requester dedups by exact coordinate against a `HashSet<VotingEventCoord>` (order-independent). There is no staleness hash to mismatch. The high-stakes sortition-electorate derivation reads `HashMap.keys()` but is correctly canonicalized by `canonical_electorate_order` (`community_voting_sortition.rs:45`, sort before Fisher-Yates), pinned by `eligible_electorate_snapshot_is_deterministically_sorted_regardless_of_hashmap_order`. Conviction math uses `i128` Q32 fixed-point specifically to avoid f64 cross-arch drift (`community_voting_conviction.rs:10`).

### Cross-peer digests that DO exist — all canonical

`recovery_config_digest` = `blake3(canonical_cbor(RecoveryDesignates))` (`community_membership.rs:1998`, fed by a `Vec` copied verbatim off the *signed* event, order fixed by wire bytes); the `Space` guarded-immutable-field merge check (`owner_state_crdt.rs:333`, structured compares, `old_epoch_keys` is `BTreeMap`). Both CANONICAL.

### The two real findings — stale comments (corrected in this change)

| # | Site | The false claim | Why it's stale | Risk if trusted |
|---|---|---|---|---|
| **1** | `community_state_sync.rs:3437-3441` | "two devices encrypting the same `CommunityState` derive the same ContentId … replica convergence on `root_cid`" | Since ZEB-814, `plan_segments` mints a fresh random `K_s` per segment (`OsRng`, `:3549`) and the manifest embeds every `K_s` — so two publishers' `root_cid`s **differ** for byte-identical logical state | An optimizer that skips the fetch on "their root == my last root" would fail **open in the worst direction**: roots never match → full-state exchange every publish forever = the #4857 storm |
| **2** | `community_voting_log.rs:58` | "events, ordered by (hlc, event_hash) at insert time" | `apply_with_snapshot` **pushes** in arrival order; canonical order is (re)established by `rebuild_from_events` (`sort_by_cached_key(canonical_key)`, `community_voting_tier3.rs:544`), which is what keeps live == boot-restore | Anyone trusting the comment to byte-compare/order the raw `events` vec would be wrong; mitigated today only because nothing does |

### Latent traps (documented, not fixed here)

- **Voting persist serializes a `HashMap` to CBOR** (`community_voting_persist.rs:111` — `poll_restore: HashMap<PollId, PollRestore>` → `ciborium::into_writer`). `voting.cbor` is therefore **not byte-reproducible** across processes (Rust `RandomState` reseeds). Safe *only* because nothing hashes or byte-diffs that file — it's local disk, decoded structurally. Any future "has voting.cbor changed?" byte-hash optimization would be wrong on day one. Cheap hardening: switch the persist maps to `BTreeMap` (Freenet's own "BTree everything" rule, §2.4). Deferred as it's a serialization-touching code change deserving its own test, not a comment fix.
- **`canonical_cbor_encode` has no type-level determinism guard** (`owner_state_crypto.rs:753`) — the "`BTreeMap` not `HashMap`, no floats" contract is enforced only by the sealed `CanonicalPayload` trait + a review checklist. Every community payload satisfies it today; nothing structurally prevents a future `impl CanonicalPayload for X` where `X` holds a `HashMap`. Pre-existing follow-up: **ZEB-220**.

---

## Q2 — Does a summary/reverse-delta interface exist naturally?

### Community config/membership state: **NATURAL**

Over the wire, `CommunityState` (`community_state_crdt.rs:93`) is a **pure event set** under a 16-byte `EventId` with an already-shared canonical total order (`event_sort_key`). The register-shaped fields (`forked_from`, `parent_lineage`, `fork_reason`, `admin_quorum`) are **never synced** — the receive path takes `remote.into_events()` and discards the rest (`community_state_sync.rs:4835`); the segmented path carries only `Vec<SignedMembershipEvent>` (`:4728`). So **no per-field version vectors are needed** — there are no replicated register fields. An id-set / range-fingerprint summary is derivable from accessors that already ship (`events()`/`get_event()`/`contains_event()`/`event_count()`, `community_state_crdt.rs:762`).

Today there is **no mismatch detection at all** — it's publish-whole-state (`community_state_sync.rs:3520`) / fetch-whole-state (`:4728`, `channel_backfill.rs:773` with no `since`/fingerprint). Content-addressing gives *blob-layer* dedup (`GetOrFetch`, sealed segments are local hits) but not a *protocol-layer* delta: every round still pays a full manifest + full tail + full decode + full re-insert sweep.

One caveat, not a blocker: membership is not *purely* grow-only — `ReachabilityAnnounce`/`CommunityRelayAnnounce` live in LWW lineages (`community_state_crdt.rs:425`) and the log physically compacts superseded entries. Compaction is order-independent and convergent, but a peer's offered event can be legitimately refused as `Superseded`, so "I offered it and you didn't take it" must not be treated as an error.

### Voting log: **PARTIAL** (with a hard prerequisite)

`VotingLog` (`community_voting_log.rs:57`) is log-shaped (`events: Vec<SignedVotingEvent>` + derived `polls`/`delegation_graph`), but:
- `SignedVotingEvent` (`community_voting_core.rs:906`) has **no id field** — set identity today is the coordinate `(actor, device_id, wall_ms, logical)`. A content hash would have to be introduced for a fingerprint. (`canonical_key`, `community_voting_tier3.rs:456`, is already byte-shape-identical to `ReconcileKey` — the material exists.)
- **The real blocker:** `archive_finalized_polls` (`community_voting_log.rs:1342`) prunes ballots 90 days after finalize on each node's **local wall clock**, so two honest peers' event sets legitimately and permanently differ. Naive set reconciliation over the whole universe would never converge and would refetch archived ballots forever — exactly River's `RetentionHorizon` bug (§2.3). A voting RBSR would first need an archive-boundary-aware reconciliation universe (reconcile only over the non-archived range, or make the archive cut a replicated fact).

---

## Q1 — Full-state exchange volume (fleet-blocked → analytical substitute)

Cannot be measured today: the fleet agents (AVALON/Ildwyn) are stopped. Analytical read from the code structure:

- **Community state:** the ZEB-814 segment layer already makes sealed history a local CAS hit; only newly-sealed segments are `put`, and the publisher's cost is already O(delta). The *receiver* still pays a full manifest+tail decode each round, but the sealed-segment blobs are dedup'd. So RBSR's marginal win over today's segmented root is concentrated in the **live tail** and in **cold/partially-synced peers**, not in sealed history. If the observed pain is manifest-and-tail churn on large communities, RBSR pays; if it's cold-join cost, ZEB-814 is already doing most of the work and the delta is smaller than it looks.
- **Voting:** unconditionally re-ships O(all events) per community every 300s, forever — the inverse of the channel-log design. This is the one plane with a real, unbounded, measurable full-dump cost, and it grows with poll history.

---

## Recommendation (go/no-go)

1. **Community config/membership state adoption — NO-GO / DEFER pending Q1 fleet measurement.** The interface is a natural fit and mostly *assembly* (mirror `community_channel_log.rs`'s `RangeReconcileSource` impl, seal under the epoch key), but the only genuinely novel piece is Freenet's *reverse-delta bidirectionality* (Harmony RBSR is pull-only, `channel_rbsr.rs:301`) — a real ~300-500 LOC protocol extension, scope-comparable to ZEB-592/593. Given ZEB-814 already dedups sealed history, committing that on spec is premature; the win lives precisely in the volume Q1 would quantify. Revisit when fleet telemetry exists.
2. **Voting-log RBSR — GO, as a separate ticket.** This is the concrete, actionable win the audit surfaced: the voting plane pays an unbounded full-dump cost every interval, the fix (`RangeReconcileSource` over the event log) is in-tree with a clean trait boundary, and `canonical_key` is already the right shape. Prerequisite: introduce a content-hash id for `SignedVotingEvent` and an archive-boundary-aware reconciliation universe (else it refetches archived ballots forever). Filed as a follow-up.
3. **Determinism — no action required beyond the two comment corrections in this change.** The codebase is clean. Two optional hardenings are documented above (voting persist `HashMap`→`BTreeMap`; the pre-existing type-level gap ZEB-220) for whoever next touches those paths.

### If we ever say "go" on the reverse-delta half

The version-gate + unconditional full-state fall-through pattern from Freenet's implementation is the template — and Harmony already *has* the fall-through (full-state fetch is the current default), so that safety net is free. The build is: a `membership_element_hash` (mirror of `community_channel_log.rs:619`), a maintained sorted side-index + chunk index on `CommunityState`, an `impl RangeReconcileSource`, a state-topic `rbsr/**` queryable/driver cloned from `community_channel_log_engine.rs:1998-2130` (sealed under the epoch key), and one new `RbsrMode` variant carrying the reverse `Want`/forward-push — the only piece not already in the tree.
