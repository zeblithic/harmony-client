# ZEB-418 SP2 Phase 1: Butler inbound deposit (1:1 DMs) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A recipient-owner's online device (the butler) accepts a sealed 1:1 DM deposit for an offline sibling, persists it into a new `dm-inbox-v1` fleet dataset, acks, and every sibling ingests it through the normal DM receive path — sender's existing "delivered" state fires on the butler ack.

**Architecture:** Five seams, all in harmony-client: (1) a `DmInboxDoc` CRDT + persistence on the SP1 `FleetSyncEngine`; (2) a `butler_deposit` wire module (frame + sealed envelope, reusing `dm_signing`'s sealed-ECDH with a new info string); (3) butler-set advertisement inside the existing pkarr routing blob; (4) an iroh ALPN acceptor that admission-gates, decrypts, persists-then-acks; (5) a sender-side deposit rung in the `DmOutbox` drain that fires on direct-delivery failure and marks delivered via the existing `mark_ack_delivered`.

**Spec:** `docs/specs/2026-06-09-zeb-418-sp2-butler-design.md` (approved). **Branch:** `zeb-418-sp2-butler` (this branch). **Dependencies:** SP1 (merged, #218). ZEB-372 is NOT a hard P1 dependency — the client already ships `ed25519_pub_to_x25519`/`ed25519_priv_to_x25519` (`dm_signing.rs:142/164`); the sender seals to `birational(vk)` from the butler-set entry.

**Per-task gates** (from `src-tauri/`, per the relink-cost rule — `--all-targets` only in the final task):
`cargo fmt --all -- --check && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(dm_inbox)+test(butler)'`

**Pinned constants (spec open items 3 resolved):**
```rust
pub const BUTLER_SET_MAX_ENTRIES: usize = 2;          // spec §3
pub const BUTLER_SET_FRESHNESS_MS: u64 = 15 * 60 * 1_000;  // §3 ~15 min
pub const DEPOSIT_MAX_FRAME_BYTES: usize = 256 * 1024;     // length-prefix cap
pub const INBOX_PER_SENDER_CAP: usize = 64;           // pending (un-ingested) per sender
pub const INBOX_GLOBAL_CAP: usize = 1024;             // pending total
pub const INBOX_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1_000;  // §5, = dm_outbox EXPIRATION_MS
```

**Ground truth (recon 2026-06-09; verify anchors at impl time, locate by signature if lines drift):**
- DM pipeline: `send_dm` IPC `lib.rs:6765`; `DmOutbox` `dm_outbox.rs:399`; drain phases A/B/C `dm_outbox.rs:837/918/1882`; transient-failure backoff `dm_outbox.rs:301-305,1065-1070`; ack→`mark_ack_delivered`→`newly_delivered`→`dm-delivered` event via `handle_ack` `dm_outbox.rs:1718`; inbound `handle_cidnotify_lifted` `dm_outbox.rs:1331`.
- Dedupe: `InboxKey { space_id, message_cid }`, idempotent `apply_inbox` `owner_state_crdt.rs:412`.
- Wire: `DmCidNotifySigned` `dm_envelope.rs:124`; packet layout `[discriminant][CBOR body][64B sig]`; storage blob `[ver(1)][nonce(12)][ct][tag(16)]` encrypted to the Space key; `verify_dm_packet_signature` `dm_signing.rs:282`.
- Sealing: `seal_to_owner`/`open_from_owner` `dm_signing.rs:66/104`; `derive_seal_key` info `b"harmony-zeb-249-epoch-key-seal"` at `dm_signing.rs:184-189`.
- SP1: `FleetSyncConfig` `fleet_sync.rs:118-152`; `list_online_devices()` `fleet_sync.rs:299-307`; notes wiring template `lib.rs:850-863,2928-2934,3264-3312` + `NotesSyncHandles` `event_loop.rs:58-66`; topic `harmony/owner/{addr_hex}/ds/notes-v1`.
- pkarr: `PkarrRoutingRecord{rd,ip,at,sg}` `harmony/crates/harmony-pkarr/src/record.rs:14-33`; client publisher + `blob_builder()` `pkarr_identity_publisher.rs:10-70`; size headroom ~620B/1000B with 2 butler entries.
- ALPNs: `iroh_endpoint.rs:46-61` (5 registered at bind, `iroh_endpoint.rs:96-110`); acceptor worked example `iroh_friend_acceptor.rs:195-600`.
- Friends: `FriendGraph.friends: BTreeMap<OwnerAddr, FriendEntry{master_ed25519, status, ...}>` `friend_graph.rs:134-189`; DM-thread check via `SpaceKind::Dm` + `DedupeKey::DmMembers` `owner_state_types.rs:1619-1704`.
- Devices: enrollments `BTreeMap<[u8;16], EnrollmentCert>` in owner state; SP1 `device_id` = 64-hex of device ed25519 verify key `lib.rs:2928-2934`.

---

### Task 1: `DmInboxDoc` CRDT

**Files:** Create `src-tauri/src/dm_inbox_crdt.rs`; modify `src-tauri/src/lib.rs` (mod decl).

- [ ] **Step 1: Write failing tests** (in-module `#[cfg(test)]`): `merge_inserts_new_entry_and_is_idempotent`, `ingested_by_merges_by_union_no_lww_race` (A adds dev-1, B adds dev-2 concurrently → merged entry has both), `concurrent_insert_same_key_converges` (both replicas same final entry), `visible_change_flag_only_on_new_entries_or_ig_growth`, `cbor_round_trips_canonically`.
- [ ] **Step 2: Run** `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(dm_inbox)'` → compile fail.
- [ ] **Step 3: Implement:**

```rust
//! Butler dm-inbox CRDT (ZEB-418 P1): deposited-but-not-yet-ingested DM
//! deliveries, replicated across the owner's fleet via FleetSyncEngine.
//! NOT a migration of DM history (spec D6).

use crate::fleet_sync::MergeOutcome;
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::Hlc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Key = "{space_id_hex}:{message_cid_hex}" — mirrors InboxKey, string-keyed
/// for canonical CBOR map encoding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmInboxEntry {
    #[serde(rename = "so")]
    pub sender_owner: [u8; 16],
    #[serde(rename = "cn")]
    pub cidnotify_packet: Vec<u8>, // full signed CidNotify packet bytes (discriminant+body+sig)
    #[serde(rename = "pl")]
    pub storage_blob: Vec<u8>,     // the CAS storage blob ([ver][nonce][ct][tag])
    #[serde(rename = "da")]
    pub deposited_at: Hlc,
    #[serde(rename = "db")]
    pub deposited_by: String,      // SP1 device_id (64-hex)
    #[serde(rename = "ig", default, skip_serializing_if = "BTreeSet::is_empty")]
    pub ingested_by: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmInboxDoc {
    #[serde(rename = "en")]
    pub entries: BTreeMap<String, DmInboxEntry>,
}

impl CanonicalPayloadSealed for DmInboxEntry {}
impl CanonicalPayload for DmInboxEntry {}
impl CanonicalPayloadSealed for DmInboxDoc {}
impl CanonicalPayload for DmInboxDoc {}

impl DmInboxDoc {
    pub fn key(space_id: &[u8; 16], message_cid: &[u8]) -> String {
        format!("{}:{}", hex::encode(space_id), hex::encode(message_cid))
    }

    /// Insert-once + ig-union merge. Same key redeposited carries identical
    /// payload (same CidNotify + blob), so first-writer-wins by `da` is safe;
    /// `ingested_by` always merges by union (grow-only set — concurrent
    /// ingestion by siblings can never race).
    pub fn merge_from(&mut self, remote: DmInboxDoc) -> MergeOutcome {
        let mut changed = false;
        for (k, r) in remote.entries {
            match self.entries.get_mut(&k) {
                None => {
                    changed |= true;
                    self.entries.insert(k, r);
                }
                Some(l) => {
                    let before = l.ingested_by.len();
                    l.ingested_by.extend(r.ingested_by);
                    if r.deposited_at.is_strictly_newer_than(&l.deposited_at) {
                        // keep earliest deposit metadata (first-writer-wins);
                        // nothing to do — l already older or equal
                    } else if l.deposited_at.is_strictly_newer_than(&r.deposited_at) {
                        l.deposited_at = r.deposited_at;
                        l.deposited_by = r.deposited_by;
                    }
                    changed |= l.ingested_by.len() != before;
                }
            }
        }
        MergeOutcome { changed }
    }
}
```

(Exact `MergeOutcome{changed}` semantics drive `on_applied` → ingestion wakeups, so ig-growth MUST count as changed: a sibling's ack must propagate for GC.)
- [ ] **Step 4: Run tests → pass.** `cargo nextest run ... -E 'test(dm_inbox)'`
- [ ] **Step 5: Commit.** `git add -A && git commit -m "feat(zeb-418-p1): DmInboxDoc CRDT — insert-once entries + grow-only ingested_by"`

### Task 2: `dm_inbox_persist.rs`

**Files:** Create `src-tauri/src/dm_inbox_persist.rs` (+ mod decl). Mirror `notes_persist.rs` exactly: version byte `0x01`, plaintext canonical CBOR, `owner_state_persist::save_atomically` (parent-fsync), reject trailing bytes, `load_doc_or_recover`/`load_replay_or_recover` quarantine-to-`*.corrupt-<ms>`-and-default, `DmInboxPersist { doc_path, replay_path }` implementing `FleetPersist<DmInboxDoc>`.

- [ ] **Step 1:** Write failing tests: round-trip, corrupt-file-quarantines-and-defaults, trailing-bytes-rejected, missing-file-defaults. (Copy the notes_persist test shapes, adjusted types.)
- [ ] **Step 2:** Run → fail. **Step 3:** Implement by direct analogy to `notes_persist.rs` (read it first; keep helper names parallel: `save`, `load_doc_or_recover`, `save_replay`, `load_replay_or_recover`). **Step 4:** Run → pass. **Step 5:** Commit `feat(zeb-418-p1): dm-inbox persistence (atomic, quarantine-on-corrupt)`.

### Task 3: `butler_deposit.rs` wire module (frame + envelope + fixtures)

**Files:** Create `src-tauri/src/butler_deposit.rs` (+ mod decl); modify `src-tauri/src/dm_signing.rs` (parameterized info string).

- [ ] **Step 1: Failing tests:** `deposit_frame_cbor_round_trips`, `deposit_frame_wire_fixture_pinned` (deterministic fields → hex-pinned canonical CBOR, regeneration-gated comment), `sealed_envelope_round_trips_with_butler_info_string`, `sig_payload_is_domain_separated` (prefix changes ⇒ verify fails), `ack_round_trips`.
- [ ] **Step 2:** Run → fail. **Step 3: Implement:**

```rust
pub const BUTLER_DEPOSIT_SIG_DOMAIN: &[u8] = b"harmony-zeb-418-deposit-sig-v1";
pub const BUTLER_DEPOSIT_SEAL_INFO: &[u8] = b"harmony-zeb-418-butler-deposit-v1";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DepositFrame {
    #[serde(rename = "ro")] pub recipient_owner: [u8; 16],
    #[serde(rename = "so")] pub sender_owner: [u8; 16],
    #[serde(rename = "ec")] pub sender_enrollment_cert: Vec<u8>, // canonical CBOR of EnrollmentCert
    #[serde(rename = "sg")] pub sig: Vec<u8>, // 64B device ed25519 over BUTLER_DEPOSIT_SIG_DOMAIN ‖ ro ‖ sealed_blob
    #[serde(rename = "sb")] pub sealed_blob: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DepositAck {
    #[serde(rename = "sp")] pub space_id: [u8; 16],
    #[serde(rename = "mc")] pub message_cid: Vec<u8>,
}

/// Plaintext inside sealed_blob (sealed to birational(vk) of the butler device):
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DepositPayload {
    #[serde(rename = "cn")] pub cidnotify_packet: Vec<u8>,
    #[serde(rename = "pl")] pub storage_blob: Vec<u8>,
}
```

In `dm_signing.rs`: refactor `derive_seal_key(shared)` → `derive_seal_key_with_info(shared, info)`; keep `derive_seal_key` calling it with the ZEB-249 constant (existing wire byte-identical — the existing seal/open tests pin this); add `seal_to_owner_with_info` / `open_from_owner_with_info` thin variants (same envelope layout). Framing: u32-LE length prefix, cap `DEPOSIT_MAX_FRAME_BYTES`, reject oversize before read.
- [ ] **Step 4:** Run → pass (fill the pinned fixture hex from first verified run, frozen comment). **Step 5:** Commit `feat(zeb-418-p1): butler deposit wire types + domain-separated sealed envelope`.

### Task 4: Butler-set in the pkarr routing blob

**Files:** Modify the client-side routing-blob struct that `pkarr_identity_publisher.rs`'s `blob_builder()` encodes (locate the exact struct at impl time — it is client-side, NOT `PkarrRoutingRecord` in the harmony crate; likely the reachability/routing payload near `reachability_record.rs:20-52`). Add:

```rust
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ButlerSetEntry {
    #[serde(rename = "d")]  pub device_id: [u8; 16],
    #[serde(rename = "ep")] pub iroh_endpoint_id: [u8; 32],
    #[serde(rename = "vk")] pub device_ed25519_verify: [u8; 32],
    #[serde(rename = "hr")] pub home_relay: String,
    #[serde(rename = "pn")] pub pinned: bool,
}
// On the routing blob struct:
//   #[serde(rename = "bs", default, skip_serializing_if = "Vec::is_empty")]
//   pub butler_set: Vec<ButlerSetEntry>,
//   #[serde(rename = "ba", default, skip_serializing_if = "is_zero_u64")]
//   pub bs_at: u64,
```

- [ ] **Step 1: Failing tests:** `routing_blob_without_butler_set_is_wire_identical_to_legacy` (THE compat pin — encode with empty set, byte-compare to a pre-change pinned hex), `routing_blob_with_butler_set_round_trips`, `butler_set_capped_at_two`, `stale_bs_at_is_filtered_by_reader` (helper `fresh_butler_set(blob, now_ms) -> Vec<ButlerSetEntry>` returns empty past `BUTLER_SET_FRESHNESS_MS`), `encoded_size_with_two_entries_under_bep44_budget` (assert < 900 bytes with realistic relay URL).
- [ ] **Step 2:** Run → fail. **Step 3:** Implement struct fields + builder population: in `blob_builder()`, primary entry = SELF (this device's id-hash, endpoint id, own cert verify key, current home relay, `pinned:false`), secondary = best-effort first sibling from `list_online_devices()` whose endpoint info is resolvable locally (if none resolvable, publish self only — document); `bs_at = now_ms`. Republish trigger: the existing per-publish re-derive path already rebuilds the blob each publish; additionally call the publisher's re-register when `list_online_devices()` output changes (hook where the engine handle is available; a 60s-debounced check task is acceptable v1). **Step 4:** Run → pass. **Step 5:** Commit `feat(zeb-418-p1): butler-set advertisement in pkarr routing blob (wire-compat when absent)`.

### Task 5: Inbound acceptor (`iroh_butler_acceptor.rs`)

**Files:** Create `src-tauri/src/iroh_butler_acceptor.rs`; modify `iroh_endpoint.rs` (const + bind registration), the accept-loop dispatcher (where the 5 existing ALPNs route), `lib.rs` (handler state handle).

- [ ] **Step 1: Failing tests** (Tauri-free core, mirror `notes_commands` core style — handler logic takes injected state, no real iroh):
  - `deposit_from_active_friend_is_accepted_persisted_then_acked` (assert dataset entry exists BEFORE ack returned; use a probe persist sink recording order),
  - `deposit_from_non_friend_rejected_before_any_crypto` (probe: decrypt closure never called),
  - `deposit_with_bad_cert_or_bad_sig_rejected`,
  - `per_sender_and_global_caps_enforced`,
  - `duplicate_deposit_same_key_acks_without_second_entry` (idempotent),
  - `inner_cidnotify_failing_verification_rejected_not_persisted`.
- [ ] **Step 2:** Run → fail. **Step 3: Implement** `handle_deposit_core(frame, ctx) -> Result<DepositAck, DepositReject>` with the spec §4 order: (1) friend lookup by `sender_owner` → must be `FriendStatus::Active` (gives `master_ed25519`); (2) decode + verify `EnrollmentCert` against that master key, extract device verify key; (3) verify `sig` over `BUTLER_DEPOSIT_SIG_DOMAIN ‖ ro ‖ sealed_blob`; (4) caps; (5) `open_from_owner_with_info(ed25519_priv_to_x25519(own_device_sk), …)`; (6) decode `DepositPayload`, verify `cidnotify_packet` via `verify_dm_packet_signature` + space/cid consistency with the storage blob CID (`ContentId::for_book`); (7) insert into `DmInboxDoc` under doc lock + `engine.flush_now().await` (persist-before-ack, D7); (8) return ack. Then the thin iroh shell: ALPN const `pub const HARMONY_BUTLER_DEPOSIT_V1: &[u8] = b"harmony/butler-deposit/v1";`, register at bind, route in the accept dispatcher, length-prefixed read (cap), call core, write ack, close. **Step 4:** Run core tests → pass; `cargo check --locked -p harmony-app --lib --features test-fixtures` for the shell. **Step 5:** Commit `feat(zeb-418-p1): butler deposit acceptor — admission-gated, persist-then-ack`.

### Task 6: Ingestion hook

**Files:** Create ingestion fn in `dm_inbox_crdt.rs` or new `dm_inbox_ingest.rs`; modify `lib.rs` (wire `on_applied` → mpsc nudge → ingestion task), `dm_outbox.rs` only if a helper must be made `pub(crate)`.

- [ ] **Step 1: Failing tests:** `ingest_puts_blob_verifies_and_applies_inbox` (entry → CAS contains blob, `apply_inbox` recorded, self added to `ig`), `ingest_is_idempotent_for_already_ingested`, `startup_sweep_ingests_preexisting_entries`, `gc_removes_when_ig_covers_enrolled_set_or_ttl` (+ re-GC after resurrection-by-merge converges — document the churn-tolerant model).
- [ ] **Step 2:** Run → fail. **Step 3: Implement** `ingest_pending(doc, ctx)`: for each entry where `!entry.ingested_by.contains(self_device_id)`: CAS-put `storage_blob`; verify `cidnotify_packet` (reuse the existing inbound verification helpers — refactor the narrowest piece of `handle_cidnotify_lifted` Phase A/C into a `pub(crate)` helper rather than duplicating; keep the refactor mechanical and covered by existing tests); `apply_inbox(InboxEntry{space_id, message_cid, from: sender_owner, received_at: deposited_at})`; emit the same UI event the normal path emits; add self to `ig`; `notify_dirty()`. Then GC pass: remove entries where `ig ⊇ hex-encoded enrolled-device verify keys` or `deposited_at.wall_ms + INBOX_TTL_MS < now`. Trigger: `on_applied` callback sends on an mpsc; a small task debounces and runs the sweep; run once at startup. **Step 4:** Run → pass. **Step 5:** Commit `feat(zeb-418-p1): dm-inbox ingestion through the normal DM receive path + coverage/TTL GC`.

### Task 7: NodeState + start_node + event_loop wiring

**Files:** Modify `src-tauri/src/lib.rs` (NodeState fields `dm_inbox_doc/tracker/sync/device_id`, Default init, load via `load_doc_or_recover`/`load_replay_or_recover` at the notes template site ~3264, engine construction ~3279 with `lookup_key_tag: b"dm-inbox-v1"`, `publish_seen: true`, `on_applied: Some(ingestion nudge)`, failure-path shutdown like notes R2, stop_inner shutdown), `src-tauri/src/event_loop.rs` (`DmInboxSyncHandles` mirroring `NotesSyncHandles`, topic `harmony/owner/{addr_hex}/ds/dm-inbox-v1`, backoff-resubscribe adapter, shutdown).

- [ ] **Step 1:** Engine-wiring proof test, exactly the `notes_engine_publishes_on_local_write` shape (`notes_commands.rs:331-384`): construct `FleetSyncEngine<DmInboxDoc>` as start_node does, insert an entry, `notify_dirty` + `flush_now`, assert a publish frame on the outbound channel.
- [ ] **Step 2:** Run → fail. **Step 3:** Implement the wiring per the 15-site template (ground truth list above). **Step 4:** Run task gate (`-E 'test(dm_inbox)+test(butler)'`) + `cargo check --locked -p harmony-app --lib --features test-fixtures`. **Step 5:** Commit `feat(zeb-418-p1): wire dm-inbox FleetSyncEngine into NodeState/start_node/event_loop`.

### Task 8: Sender-side deposit rung in DmOutbox

**Files:** Modify `src-tauri/src/dm_outbox.rs` (drain Phase B/C), new `pub(crate)` deposit client fn (in `butler_deposit.rs`), `lib.rs` (inject resolver/endpoint handles into DmOutbox construction).

- [ ] **Step 1: Failing tests** (mock `DmTransport` + mock deposit channel, existing outbox test harness):
  - `transient_direct_failure_with_fresh_butler_set_attempts_deposit`,
  - `deposit_ack_marks_owner_delivered_and_emits_dm_delivered` (via `mark_ack_delivered`, assert `newly_delivered`),
  - `stale_or_missing_butler_set_skips_rung_falls_back_to_retry`,
  - `deposit_failure_leaves_entry_pending_with_backoff` (shared `AttemptState`),
  - `late_direct_ack_after_deposit_ack_is_idempotent`.
- [ ] **Step 2:** Run → fail. **Step 3: Implement:** a `ButlerDepositClient` trait (prod: pkarr-resolve recipient record → `fresh_butler_set` → for each entry in priority order: `EndpointAddr::new(ep).with_relay_url(hr)` → connect ALPN → send frame sealed to `dm_signing::ed25519_pub_to_x25519(&entry.device_ed25519_verify)` → await ack; test: mock). Hook into drain Phase B: when a destination's direct send fails transiently and the entry has been pending ≥ one backoff cycle, attempt deposit once per backoff window; Phase C: on ack, `mark_ack_delivered(entry_id, recipient_owner)`. Frame `sig` signed with the device signing key already on `DmOutbox` (`signing_key`, ZEB-339). **Step 4:** Run → pass. **Step 5:** Commit `feat(zeb-418-p1): sender deposit rung — direct-fail → butler deposit → delivered on ack`.

### Task 9: Two-engine integration test

**Files:** Create `src-tauri/tests/butler_deposit_integration.rs`.

- [ ] **Step 1:** Test: two `FleetSyncEngine<DmInboxDoc>` instances (same owner KeyTree, devices A/B) bridged by in-memory channels (the SP1 integration harness pattern); run `handle_deposit_core` on A with a real sealed frame from a third "sender" identity; assert: entry persisted on A before ack; B receives via fan-out; B's ingestion sweep applies `apply_inbox` + adds B to `ig`; A observes B's `ig` ack; GC fires when coverage complete. Gate with `--test butler_deposit_integration` (scoped, per relink rule).
- [ ] **Step 2:** Run → pass (this is an integration assembly of tested parts; failures here are wiring bugs). **Step 3:** Commit `test(zeb-418-p1): two-engine butler deposit → fan-out → ingestion integration`.

### Task 10: Final sweep + docs

- [ ] **Step 1:** `cargo fmt --all -- --check` && `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` && `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (full; budget ~30 min; known-unrelated: rename_content port-4242 ZEB-420 family). `npx tsc --noEmit && npx vitest run` (frontend untouched — must stay green unchanged).
- [ ] **Step 2:** Add the plain-string `"superseded"` rejection case to `src/lib/notes-migrate.test.ts` (PR #218 CodeRabbit carry-over; Jake-approved rollup).
- [ ] **Step 3:** Commit `chore(zeb-418-p1): final sweep + carry PR#218 string-rejection test nitpick`.

---

**Out of scope for P1 (spec §8):** outbound fleet hold (P2), group DMs/community backfill (P3), relay + proof-carrying admission + UCAN/PoW (P4), pin UI (P2), Koya/Ildwyn manual cross-WAN session (post-merge validation with Jake).

---

## Implementation notes (post-plan corrections + deviations, 2026-06-10 — kept so this doc matches what shipped)

1. **Wire fields are bstr-packed (Task 3, controller fix).** All seven `Vec<u8>` wire fields across `DepositFrame`/`DepositAck`/`DepositPayload` use `serde_bytes` (CBOR bstr) per the dm_envelope convention — int-array encoding would have ~2×'d frame size. The pinned `DepositFrame` fixture was regenerated ONCE pre-ship for this (2026-06-09, nothing had shipped); it is frozen from then on.
2. **Butler-set freshness vs the epoch publish schedule (Task 4, P2 deferral).** `PkarrPublisher` rebuilds the routing blob on every scheduled publish, but the schedule is epoch-based — `bs_at` exceeds `BUTLER_SET_FRESHNESS_MS` between slots, so butler discovery is effectively fresh only ~15 min after boot/opt-in/epoch publishes. Senders fall through to the existing retry chain when stale (spec §3: never worse). Periodic refresh + a sibling secondary entry (needs a device-id → endpoint/relay map that doesn't exist locally yet) + fleet-change re-register are ONE P2 follow-up — see the `// ZEB-418 P2:` comment in the lib.rs blob builder.
3. **Identity-pub trust source (Tasks 5/7).** The acceptor's inner-CidNotify verification looks up the sender device's identity pub in `owner_device_cache` via `dm_outbox::lookup_pubkey_for_device` — the SAME source the normal receive path uses. A deposit from a sender device the butler has never seen rejects at `InnerVerifyFailed`, exactly as the normal path would drop it. Do not introduce a second trust source.
4. **`persist_entry` hardening (Task 7).** Production deposit persist = insert-once under the doc lock → `notify_dirty` BEFORE `flush_now` (a failed publish leg leaves the dirty latch armed for retry) → flush even on duplicate keys (a redelivery after a failed first flush must not ack non-durable state, D7) → best-effort weak-sender self-nudge (the butler is itself a recipient device; without it, its own UI delivery would wait for a sibling ig-ack that never comes when the rest of the fleet is offline).
5. **Sender-rung trigger gap (Task 8) → ZEB-422.** The rung fires on `TransportError::Transient` + pending ≥ one backoff window (plan-faithful). Cached-but-offline recipients produce Ok-enqueued sends that never ack — no transient failure, rung never fires. Follow-up filed as ZEB-422 (no-ack-after-N-windows candidacy); must land before/with the cross-WAN butler proof.
6. **Coverage-GC one-sweep deferral (Task 9 — real bug found by the integration test).** `ingest_pending` originally GC'd on the POST-ingest `ig`, so the last-ingesting device deleted the entry in the same sweep and the covering `ig ⊇ enrolled` state never replicated — siblings pinned the entry until TTL. Fixed: coverage-GC removes only entries covered at sweep START (`covered_at_start` snapshot), deferring removal one sweep so the covering state publishes first. Residual (accepted): a fully-covered entry lingers on a replica until that replica's next sweep trigger (inbound nudge or startup sweep) — bounded, removable state.
7. **Task 10 step 2 (plain-string "superseded" test) was carried by the ZEB-372 Phase 2 re-pin PR (#220)** instead of this branch — skipped here to avoid a cross-branch duplicate.
8. **PR #221 round-1 review fixes (2026-06-10).** Three confirmed findings. (a) *Device→owner binding (Qodo, correctness):* the acceptor verified the inner CidNotify signature but never bound the signing DEVICE to `frame.sender_owner` — a deposit the normal receive path would refuse could be persisted+acked, showing the sender "delivered" while ingestion (which reuses the normal path) rejects it until TTL. `lookup_identity_pub` became `resolve_sender_device` → `(owner, identity_pub)` via `resolve_signed_origin_owner` + `lookup_pubkey_for_device` under ONE `owner_device_cache` lock (Unknown AND Ambiguous → reject), with an explicit `resolved_owner == frame.sender_owner` check. (b) *Caps atomic-at-persist with duplicate exemption (Cursor High + CodeAnt race):* the standalone pre-decrypt `caps_snapshot` both raced (snapshot-then-insert under separate lock acquisitions could overshoot the quotas) and wrongly `CapExceeded`-rejected a lost-ack REDELIVERY of an already-stored entry at a full inbox. Cap enforcement moved INSIDE `persist_entry`'s doc-lock critical section (`DepositPersistVerdict::{Inserted, Duplicate, CapExceeded}`); occupied keys bypass the caps and re-flush+re-ack; `CapExceeded` inserts and flushes nothing. New step order: recipient bind → friend-Active → cert → sig → decrypt → inner decode + device-owner binding + packet sig + CID bind → atomic persist-with-caps → ack. (c) *Future `bs_at` bound (Qodo, reliability):* `fresh_butler_set` treated any future stamp as fresh-forever via `saturating_sub`; now stamps more than one freshness window ahead of the reader's clock are stale (bounded forward-skew tolerance), so a maliciously future-stamped record can't pin the deposit rung onto dead butlers.
