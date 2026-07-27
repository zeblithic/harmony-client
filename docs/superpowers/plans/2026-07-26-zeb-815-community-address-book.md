# ZEB-815 Community Address Book Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move Reachability/CommunityRelay announces out of the membership event log into a bounded, sealed, per-community address book with live pub + snapshot sync (spec: `docs/superpowers/specs/2026-07-26-zeb-815-community-address-book-design.md`).

**Architecture:** A new store module (`community_address_book.rs`) holds LWW rows keyed like ZEB-813's supersession keys, with TTL/bounds/eviction and a CBOR sidecar. A sync module (`address_book_sync.rs`) seals records with a membership-derived key (presence pattern), publishes them on `harmony/addrbook/{hex}/records`, serves/requests snapshots on `harmony/addrbook/{hex}/snapshot`, and funnels every arrival through one ingest gate that feeds the existing resolvers (supervisor kicks preserved). The two publishers swap from minting membership events to upserting + publishing records (flag-day); the delta-consumer announce arms and boot replay are removed; kick/leave eviction extends the existing arms.

**Tech Stack:** Rust (src-tauri), zenoh pub/sub + queryable, ciborium CBOR, HKDF-SHA256 sealing via `encrypt_voice_packet`/`decrypt_voice_packet`, ed25519-dalek, cargo-nextest.

## Global Constraints

- Branch: `zeb-815-community-address-book` (exists, off `main @ cb580cd4`; both specs committed).
- Constants (exact values from spec): `ADDRBOOK_MAX_NODES_PER_MEMBER = 8`, `ADDRBOOK_MAX_ROWS = 4096` (hard cap per community), `ADDRBOOK_SNAPSHOT_COOLDOWN_MS = 60_000`, `ADDRBOOK_SKEW_TOLERANCE_MS = 300_000` (5 min, mirrors resolver), reachability row TTL `ADDRBOOK_REACHABILITY_TTL_MS = 86_400_000` (24 h), relay row TTL = existing `community_relay_announce::COMMUNITY_RELAY_AD_FRESHNESS_MS` (15 min).
- Topics: `harmony/addrbook/{community_id_hex}/records` (live), `harmony/addrbook/{community_id_hex}/snapshot` (queryable). Seal AAD: `b"harmony-addrbook-v1"`. Sentinel channel: `ChannelId([1u8; 16])`. HKDF info: `b"addrbook:"`.
- Old `MembershipEventKind::ReachabilityAnnounce`("a") / `CommunityRelayAnnounce`("b") events: **decode + verify_event arms stay forever** (history must verify); minting and consumption are removed. Flag-day rollout — no dual-write.
- All resolver/consumer contracts unchanged: `ReachabilityResolver::update` supervisor kicks, `CommunityRelayResolver` read semantics (self entries stay — ZEB-524/ZEB-806), `remove_advertiser`/`remove_owner` eviction shape.
- Cargo from `src-tauri/`; gates: `cargo fmt --all -- --check`, `cargo clippy --all-targets --features test-fixtures --locked -- -D warnings`, `cargo nextest run --locked --features test-fixtures` scoped per task (`-E` filters — the documented ZEB-631 iterative-selection exception; lib change relinks ~97 integration binaries, so never `--all-targets` per-task), `scripts/test-select --context round` at the end as the *local* convergence gate. Final pre-merge validation is CI's full-workspace `--all-targets` nextest suite (3 shards + roll-up gate) — test-select is never the final gate.
- Commit after each task; trailer: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`.

---

### Task 1: Address book store (`community_address_book.rs`)

**Files:**
- Create: `src-tauri/src/community_address_book.rs`
- Modify: `src-tauri/src/lib.rs` (one `mod community_address_book;` line next to `mod community_relay_resolver;`)

**Interfaces:**
- Produces (later tasks rely on these exact names):
  - `pub enum AddressBookKey { Reachability(OwnerAddr, [u8; 32]), Relay(OwnerAddr, [u8; 16]) }` (derives `Clone, Debug, PartialEq, Eq, PartialOrd, Ord`)
  - `pub enum AddressBookEntry { Reachability(reachability_record::ReachabilityAnnouncePayload), Relay(community_relay_announce::CommunityRelayAnnouncePayload) }` (derives `Clone, Debug, serde::Serialize, serde::Deserialize`)
  - `pub struct AddressBookRow { pub entry: AddressBookEntry, pub actor: OwnerAddr, pub device: [u8; 32], pub at: Hlc, pub stamped_at_ms: u64 }` (same derives as entry)
  - `pub enum UpsertOutcome { Inserted, Replaced, IgnoredOlder, IgnoredCapped }`
  - `pub struct CommunityAddressBook { .. }` with:
    - `pub fn new() -> Self`
    - `pub fn upsert(&self, community: SpaceId, row: AddressBookRow, now_ms: u64) -> UpsertOutcome`
    - `pub fn rows_for_community(&self, community: &SpaceId, now_ms: u64) -> Vec<AddressBookRow>` (TTL-filtered)
    - `pub fn remove_owner(&self, community: &SpaceId, owner: &OwnerAddr) -> usize`
    - `pub fn remove_community(&self, community: &SpaceId) -> usize`
    - `pub fn sweep_expired(&self, now_ms: u64) -> usize`
  - `pub fn key_for_row(row: &AddressBookRow) -> AddressBookKey`
  - `pub fn row_ttl_ms(entry: &AddressBookEntry) -> u64`
  - Consts: `ADDRBOOK_MAX_NODES_PER_MEMBER`, `ADDRBOOK_MAX_ROWS`, `ADDRBOOK_SKEW_TOLERANCE_MS`, `ADDRBOOK_REACHABILITY_TTL_MS` (values in Global Constraints).

Storage: `Mutex<BTreeMap<(SpaceId, AddressBookKey), AddressBookRow>>` — the same shape as `CommunityRelayResolver` (`community_relay_resolver.rs:16`).

Semantics to implement exactly:
- `key_for_row`: Reachability → `(actor, payload.iroh_node_id)`; Relay → `(actor, payload.relay.relay_device_id)`.
- `row_ttl_ms`: Reachability → `ADDRBOOK_REACHABILITY_TTL_MS`; Relay → `COMMUNITY_RELAY_AD_FRESHNESS_MS`.
- `upsert`: effective stamp = `min(row.stamped_at_ms, now_ms + ADDRBOOK_SKEW_TOLERANCE_MS)` stored back into the row; existing row with `stamped_at_ms >= effective` → `IgnoredOlder`. New key: count community rows — `>= ADDRBOOK_MAX_ROWS` → `IgnoredCapped`; for a Reachability key, if the actor already has `>= ADDRBOOK_MAX_NODES_PER_MEMBER` reachability rows in this community, evict that actor's oldest-stamped reachability row, then insert (`Inserted`).
- `rows_for_community`: filter `now_ms.saturating_sub(stamped_at_ms) <= row_ttl_ms(entry)`.

- [ ] **Step 1: Write the failing tests** — new file with the impl skeleton (types + `todo!()` bodies is fine) and a `#[cfg(test)] mod tests` copied in style from `community_relay_resolver.rs:105` (local `fn hlc(ms) -> Hlc`, `fn reach_payload(seed: u8, ts: u64) -> ReachabilityAnnouncePayload` built via `reachability_record` struct literal with `identity_signature: [0;64]`, `fn relay_payload(seed: u8, ad_at: u64)` like `community_relay_resolver.rs:118`). Tests:
  - `upsert_then_read_returns_row`
  - `newer_stamp_replaces_older_ignored` (Replaced / IgnoredOlder)
  - `future_stamp_clamped_to_skew_tolerance` (a `now + 10min` stamp stores as `now + 5min` and a later `now + 6min` row replaces it)
  - `ttl_filters_on_read_reachability_24h_relay_15m`
  - `per_member_node_cap_evicts_oldest` (9 reachability rows for one actor → oldest gone, 8 remain)
  - `hard_cap_ignores_new_keys` (fill 4096 synthetic keys → next distinct key `IgnoredCapped`; reuse one loop with seeded owners)
  - `remove_owner_drops_both_kinds_scoped_to_community`
  - `remove_community_drops_all_and_sweep_expired_counts`
- [ ] **Step 2: Run to verify failure:** `cargo nextest run --locked --features test-fixtures -E 'test(community_address_book)'` — expect FAIL/todo panics.
- [ ] **Step 3: Implement the store** per the semantics block above.
- [ ] **Step 4: Run to green** (same command). Expected: all 8 pass.
- [ ] **Step 5: Commit** `feat(zeb-815): address book store — LWW rows, TTL, bounds, eviction`

---

### Task 2: Sidecar persistence (same module)

**Files:**
- Modify: `src-tauri/src/community_address_book.rs`

**Interfaces:**
- Produces:
  - `pub fn addrbook_path(identity_dir: &std::path::Path, community: &SpaceId) -> std::path::PathBuf` → `identity_dir/communities/{hex}/addrbook.cbor` (mirror `community_state_sync.rs:5234 paths_for`)
  - `pub fn save_addrbook(path: &std::path::Path, rows: &[AddressBookRow]) -> Result<(), String>` (create parent dirs; write via temp file + rename, same durability posture as `community_state_persist::save_crdt`)
  - `pub fn load_addrbook(path: &std::path::Path, now_ms: u64) -> Vec<AddressBookRow>` (missing/corrupt file → empty vec — loss-safe per spec §2; TTL-filter on load)

Wire format: `ciborium` CBOR of `Vec<AddressBookRow>` (serde derives from Task 1).

- [ ] **Step 1: Failing tests** (`tempfile::tempdir()` — already a dev-dependency):
  - `sidecar_round_trip_preserves_rows`
  - `load_filters_expired_rows`
  - `load_missing_or_corrupt_returns_empty` (nonexistent path; then write garbage bytes and load)
- [ ] **Step 2: Run to verify failure** (same nextest filter).
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to green.**
- [ ] **Step 5: Commit** `feat(zeb-815): addrbook.cbor sidecar — TTL-filtered load, loss-safe`

---

### Task 3: Sealed wire codec (`address_book_sync.rs`)

**Files:**
- Create: `src-tauri/src/address_book_sync.rs` (+ `mod address_book_sync;` in lib.rs)

**Interfaces:**
- Produces:
  - `pub const ADDRBOOK_AAD: &[u8] = b"harmony-addrbook-v1";`
  - `pub const ADDRBOOK_SENTINEL_CHANNEL: ChannelId = ChannelId([1u8; 16]);`
  - `pub const ADDRBOOK_SNAPSHOT_COOLDOWN_MS: u64 = 60_000;`
  - `pub const ADDRBOOK_SNAPSHOT_MAX_BYTES: usize = 1_048_576;`
  - `pub fn derive_addrbook_key(mk: &EpochKey, community_id: &SpaceId) -> ChannelKey` — HKDF-SHA256, salt = `community_id.0`, info = `b"addrbook:"` (copy `community_channel_log.rs:90 derive_presence_key` body, changing only the info string)
  - `pub fn seal_records(key: &ChannelKey, community: &SpaceId, rows: &[AddressBookRow]) -> Result<Vec<u8>, String>` — ciborium-encode `Vec<AddressBookRow>` then `encrypt_voice_packet(key, community, &ADDRBOOK_SENTINEL_CHANNEL, ADDRBOOK_AAD, &plain)` (mirror `community_presence.rs:115 seal_presence_beacon`)
  - `pub fn open_records(key: &ChannelKey, community: &SpaceId, packet: &[u8]) -> Option<Vec<AddressBookRow>>` (mirror `community_presence.rs:132 open_presence_beacon`; enforce `packet.len() <= ADDRBOOK_SNAPSHOT_MAX_BYTES` before decrypt)
- Consumes: Task 1 `AddressBookRow`.

A live record publish is `seal_records` with a 1-element slice; a snapshot is the same codec with the full row set — one codec, no second format.

- [ ] **Step 1: Failing tests** (fixture key: `ChannelKey` from `[7u8; 32]` the way `community_presence.rs` tests build theirs — copy the fixture pattern from its test module):
  - `seal_open_round_trip_single_and_many`
  - `wrong_key_fails_open` (epoch-rotation stand-in: different key bytes → `None`)
  - `tampered_packet_fails_open` (flip one ciphertext byte)
  - `oversize_packet_rejected_before_decrypt` (`vec![0u8; ADDRBOOK_SNAPSHOT_MAX_BYTES + 1]` → `None`)
  - `distinct_from_presence_seal` (presence key ≠ addrbook key for same `mk`/community — assert `derive_presence_key(..) != derive_addrbook_key(..)`)
- [ ] **Step 2: Run to verify failure:** `-E 'test(address_book_sync)'`.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to green.**
- [ ] **Step 5: Commit** `feat(zeb-815): sealed addrbook wire codec (presence-pattern HKDF + AEAD)`

---

### Task 4: Ingest gate

**Files:**
- Modify: `src-tauri/src/address_book_sync.rs`

**Interfaces:**
- Produces:
  - `pub enum IngestOutcome { Applied(UpsertOutcome), BadSignature, NotMember, Malformed }`
  - Pure core (sync, unit-testable — membership already checked by caller):
    ```rust
    pub fn ingest_verified_row(
        book: &CommunityAddressBook,
        reachability_resolver: &ReachabilityResolver,
        community_relay_resolver: &CommunityRelayResolver,
        community: SpaceId,
        row: AddressBookRow,
        now_ms: u64,
    ) -> IngestOutcome
    ```
    Steps: dispatch on `row.entry` → verify inner signature with `ed25519_dalek::VerifyingKey::from_bytes(&row.device)`:
    `reachability_record::verify_inner_signature(&p, &row.actor, &row.at, &vk)` (`reachability_record.rs:241`) or `community_relay_announce::verify_inner_signature(&p, &row.actor, &row.at, &vk)` (`community_relay_announce.rs:125`) → on failure `BadSignature`. Then `book.upsert(community, row.clone(), now_ms)`; on `Inserted | Replaced`, fan out exactly what the old delta arms did (lib.rs:7251/7269):
    Reachability → `reachability_resolver.update(row.actor, p, row.at)`; Relay → `community_relay_resolver.update(community, row.actor, p, row.at)`. Return `Applied(outcome)`.
  - Async wrapper (membership gate + unseal; used by Tasks 5/6):
    ```rust
    pub async fn ingest_sealed_packet(
        registry: &Arc<CommunitySyncRegistry>,
        book: &CommunityAddressBook,
        reachability_resolver: &ReachabilityResolver,
        community_relay_resolver: &CommunityRelayResolver,
        community: SpaceId,
        packet: &[u8],
        now_ms: u64,
    ) -> Vec<IngestOutcome>
    ```
    `engine_arc` → `engine.membership_key()` → `derive_addrbook_key` → `open_records` (else one `Malformed`) → per row: `voice_presence::beacon_signer_is_member(registry, &community, &row.actor, &row.device).await` (`voice_presence.rs:524`) — false → `NotMember`, skip; else `ingest_verified_row`.
- Consumes: Tasks 1–3 exports; existing resolvers.

Note: `beacon_signer_is_member` is the same gate presence ingest uses (community_presence.rs:576-612 order: unseal → sig → membership). We check membership *before* signature here only in the sense that the wrapper batches it per-row before the pure call — same net gate set.

- [ ] **Step 1: Failing tests** for the pure core (resolvers are plain structs — construct real ones; build a *correctly signed* reachability row via `reachability_record::build_signed_payload_with_key` (`reachability_record.rs:204`) and a relay row via `build_signed_community_relay_announce` (`community_relay_announce.rs:109`) with a throwaway `ed25519_dalek::SigningKey`; `row.device` = that key's verifying bytes):
  - `verified_reachability_row_lands_in_book_and_resolver` (assert `resolver.resolve(&actor)` non-empty after)
  - `verified_relay_row_lands_in_book_and_relay_resolver` (assert `relays_for_community` non-empty)
  - `bad_signature_rejected_nothing_stored` (corrupt `identity_signature`)
  - `older_row_ignored_resolver_not_double_fed` (second ingest with older stamp → `Applied(IgnoredOlder)`)
- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement both functions.**
- [ ] **Step 4: Run to green.**
- [ ] **Step 5: Commit** `feat(zeb-815): addrbook ingest gate — verify, upsert, resolver fan-out`

---

### Task 5: Event-loop wiring (subscriber + snapshot queryable + requester)

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (request enum + task pool, mirroring the presence pool at `event_loop.rs:3960-4090`)
- Modify: `src-tauri/src/address_book_sync.rs` (the three spawn fns)
- Modify: `src-tauri/src/lib.rs` (send Subscribe/Unsubscribe wherever `CommunityPresenceRequest::Subscribe`/`Unsubscribe` is sent today — find all send sites with `grep -n "CommunityPresenceRequest::" src-tauri/src/lib.rs src-tauri/src/event_loop.rs` and add the sibling send at each)
- Modify: `src-tauri/src/community_presence.rs` (one line in `on_presence_roster_change`, `community_presence.rs:522-535`: alongside `supervisor.kick_sweep()`, bump an optional `addrbook_resync: Option<Arc<tokio::sync::Notify>>` field threaded like `supervisor` is)

**Interfaces:**
- Produces in `event_loop.rs`:
  - `pub enum AddressBookRequest { Subscribe { community_id: [u8; 16] }, Unsubscribe { community_id: [u8; 16] } }` (mirror `event_loop.rs:462`)
- Produces in `address_book_sync.rs` (each returns `tokio::task::JoinHandle<()>`):
  - `pub fn spawn_addrbook_subscriber(session: zenoh::Session, registry: Arc<CommunitySyncRegistry>, book: Arc<CommunityAddressBook>, rr: Arc<ReachabilityResolver>, crr: Arc<CommunityRelayResolver>, community: SpaceId, dirty: Arc<Notify>) -> JoinHandle<()>` — `session.declare_subscriber(format!("harmony/addrbook/{}/records", hex))`, each sample → `ingest_sealed_packet`; any `Applied(Inserted|Replaced)` → `dirty.notify_one()`.
  - `pub fn spawn_addrbook_snapshot_queryable(session: zenoh::Session, registry, book, community) -> JoinHandle<()>` — declare queryable on `harmony/addrbook/{hex}/snapshot` **before** spawning the loop (copy the declare-then-spawn shape from `event_loop.rs:10904`); on query: `rows_for_community(now)` → `membership_key` → `seal_records` → `query.reply(query.key_expr(), packet).await`.
  - `pub fn spawn_addrbook_snapshot_requester(session: zenoh::Session, registry, book, rr, crr, community: SpaceId, resync: Arc<Notify>, dirty: Arc<Notify>) -> JoinHandle<()>` — loop: fire a `session.get(&format!("harmony/addrbook/{}/snapshot", hex))` immediately on spawn, then wait on `resync.notified()` OR a 30-min idle re-query tick, with `ADDRBOOK_SNAPSHOT_COOLDOWN_MS` between fires; every reply payload → `ingest_sealed_packet`. No-responder → `tracing::info!` with community context (spec §6 — INFO, not WARN) and continue.
  - `pub fn spawn_addrbook_persist_task(book: Arc<CommunityAddressBook>, path: PathBuf, community: SpaceId, dirty: Arc<Notify>) -> JoinHandle<()>` — on notify: 2 s debounce sleep, `rows_for_community`, `tokio::task::spawn_blocking(move || save_addrbook(..))` (mirror `persist_crdt_only`'s snapshot-then-blocking shape, `community_state_sync.rs:4369`).
- The event-loop pool owns `HashMap<[u8;16], Vec<JoinHandle<()>>>` (4 handles per community), self-healing retain like `event_loop.rs:4004-4012`; `Unsubscribe` aborts all four. The pool task gets `session`, `registry`, the three Arcs, `identity_dir`, and per-community builds `dirty`/`resync` Notifies; `resync` is registered with the presence subscriber's new `addrbook_resync` field for that community.

- [ ] **Step 1: Failing test** (in `address_book_sync.rs` tests — no zenoh needed): `requester_cooldown_gates_rapid_notifies` — factor the cooldown decision into `pub fn cooldown_elapsed(last_fire_ms: u64, now_ms: u64) -> bool` and unit-test both sides of the boundary; plus `snapshot_reply_and_ingest_round_trip` — seal N rows with a key, run them through `open_records` + `ingest_verified_row` chain, assert book+resolver state (this is the queryable↔requester contract minus the wire).
- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement** the spawn fns + event-loop pool + request plumbing + the presence one-liner. Wire `AddressBookRequest` channel creation next to the presence channel and route sends at every `CommunityPresenceRequest` send site found by the grep.
- [ ] **Step 4: Green + compile gates:** the nextest filter above, then `cargo clippy --all-targets --features test-fixtures --locked -- -D warnings` (event_loop.rs touched — clippy early here saves a late surprise).
- [ ] **Step 5: Commit** `feat(zeb-815): addrbook sync tasks — live sub, snapshot serve/request, persist`

---

### Task 6: Publisher swap (flag-day begins)

**Files:**
- Modify: `src-tauri/src/lib.rs:8463-8712` (reachability PublishFn closure) and `src-tauri/src/lib.rs:10595-10700` (relay PublishFn closure); their capture blocks gain `book: Arc<CommunityAddressBook>`, `publish_tx: tokio::sync::mpsc::Sender<event_loop::PublishRequest>` (created at `lib.rs:3392`), and per-community `dirty` notify access (simplest: a `Arc<CommunityAddressBook>`-adjacent `Arc<Notify>` map owned by the pool — expose a `pub fn dirty_handle(&self, community: &SpaceId) -> Arc<Notify>` registry on the pool's shared state, or pass one global dirty Notify per node and let the persist tasks all wake; choose the global Notify — simpler, persist tasks snapshot per-community anyway).
- Modify: `src-tauri/src/address_book_sync.rs` — add the extraction both closures call:
  ```rust
  pub async fn publish_own_rows(
      registry: &Arc<CommunitySyncRegistry>,
      book: &CommunityAddressBook,
      rr: &ReachabilityResolver,
      crr: &CommunityRelayResolver,
      publish_tx: &tokio::sync::mpsc::Sender<crate::event_loop::PublishRequest>,
      rows: Vec<(SpaceId, AddressBookRow)>,
      dirty: &Notify,
  )
  ```
  For each `(community, row)`: `ingest_verified_row(..)` (own rows go through the same gate — uniform trust path, self lands in own resolvers exactly as the old `insert_local_event → delta hook` did), then `engine.membership_key()` → `derive_addrbook_key` → `seal_records(&key, &community, &[row])` → `publish_tx.send(PublishRequest { key_expr: format!("harmony/addrbook/{}/records", hex), payload, reply })` with the oneshot reply awaited-and-ignored on error (pattern: `event_loop.rs:338/4417`); `dirty.notify_one()`.

**The swap, in each closure:**
- Reachability (lib.rs:8489-8712): keep everything up to and including `build_signed_payload_with_key(..)` per community (unchanged — same signature, same HLC reservation via `reserve_next_hlc_for_device`). **Delete** the `EventPayload`/`sign_event`/`insert_local_event` block. Instead accumulate `(community_id, AddressBookRow { entry: AddressBookEntry::Reachability(payload), actor, device: community_signing_key.verifying_key().to_bytes(), at: hlc, stamped_at_ms: announced_at_ms })` and call `publish_own_rows` once after the loop. Keep the `sync_case_d_handles` tail untouched.
- Relay (lib.rs:10595): same surgery — keep `build_signed_community_relay_announce` + the `membership.is_joined` gate + the `rendezvous_publisher.refresh_slot(..)` tail (it reads `advertiser_addrs_for_community` from the resolver, which `publish_own_rows` has just fed — call `refresh_slot` *after* `publish_own_rows` so the freshly-upserted self ad is visible, preserving today's ordering where `insert_local_event`'s delta fed the resolver before `refresh_slot` ran).

**Interfaces:** Consumes Tasks 1–5. Produces: zero new public API beyond `publish_own_rows`.

- [ ] **Step 1: Failing test** — in `address_book_sync.rs`: `publish_own_rows_feeds_book_resolvers_and_wire` — build a real `(publish_tx, publish_rx)` mpsc pair, real resolvers/book, a signed reachability row + relay row; call `publish_own_rows`; assert (a) book has both rows, (b) both resolvers resolve them, (c) `publish_rx` yielded two `PublishRequest`s whose `key_expr`s match the two communities' record topics and whose payloads `open_records` back to the rows. (Registry/engine dependency: for the seal key, accept a `key_fn: impl Fn(&SpaceId) -> Option<ChannelKey>` parameter instead of the registry in `publish_own_rows` — the closures pass `|c| registry.engine_arc-blocking membership_key` via a small async-adapted lookup; the test passes a fixed key. Lock this signature choice in now: `key_fn: &(dyn Fn(&SpaceId) -> Option<ChannelKey> + Send + Sync)` and make `publish_own_rows` take it instead of `registry`.)
- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement** `publish_own_rows` + both closure surgeries + captures. Grep-proof the flag-day: `grep -n "MembershipEventKind::ReachabilityAnnounce {" src-tauri/src/lib.rs` and the relay equivalent must show **zero minting sites** (only the delta/verify/boot arms remain until Task 7).
- [ ] **Step 4: Green:** `-E 'test(address_book_sync)'`, then `cargo build --locked --lib` (closure capture changes are where lib compile breaks surface).
- [ ] **Step 5: Adapt the in-tree publisher test** at `lib.rs:76803-76920` (drives a real `ReachabilityPublisher` with a counter `PublishFn`) — it should still pass untouched (it counts callback invocations, not events); run its filter to confirm.
- [ ] **Step 6: Commit** `feat(zeb-815): publishers write addrbook records — announce events no longer minted`

---

### Task 7: Consumption removal, boot swap, eviction wiring

**Files:**
- Modify: `src-tauri/src/lib.rs`

**Changes:**
1. **Delta arms** (lib.rs:7251-7332): replace both announce arms' bodies with a no-op comment (`// ZEB-815: routing data now arrives via the address book; old events are ignored (decode+verify retained for history)`) — keep the match arms so the enum stays exhaustively handled.
2. **Kick/leave arms** (lib.rs:7333-7424): alongside the existing `community_relay_resolver.remove_advertiser(..)` + `resolver.remove_owner(..)` calls, add `book.remove_owner(&community_id, &event.actor)` (Leave) / `book.remove_owner(&community_id, target)` (Kick) + `dirty.notify_one()`. Capture `book` + `dirty` into the delta hook closure (capture block lib.rs:7131-7202).
3. **Boot replay block** (lib.rs:8237-8357): delete the two announce replay collectors; in their place, per community: `let rows = community_address_book::load_addrbook(&addrbook_path(&identity_dir, cid), now_ms);` then for each row, re-run the membership filter the old replay used (materialized `is_joined` on `row.actor`) and call `ingest_verified_row(..)` (trusted-load still re-verifies signatures — cheap at these row counts and removes a second trust path). Keep the `membership_projection` seeding lines untouched.
4. **Own leave:** in `leave_community_impl` (find via `grep -n "fn leave_community_impl" src-tauri/src/lib.rs`), after the existing community teardown: `book.remove_community(&community_id);` + delete the sidecar file (`std::fs::remove_file(addrbook_path(..)).ok()`), and send `AddressBookRequest::Unsubscribe`.
5. **start_node wiring:** construct `let address_book = Arc::new(CommunityAddressBook::new());` + the global `dirty: Arc<Notify>` early (next to `community_relay_resolver` construction, lib.rs:~10266); clone into: delta hook, both publisher closures, boot block, event-loop pool creation, leave path.

**Interfaces:** Consumes everything prior. Produces: the flag-day is complete — no announce events minted, consumed, or replayed.

- [ ] **Step 1: Failing integration test** — extend the delta-hook coverage where kick/leave eviction is already tested (locate with `grep -rn "remove_advertiser" src-tauri/tests/ src-tauri/src/lib.rs | grep -i test`; if only unit-level exists, add to `community_address_book.rs` tests a direct simulation): `kick_evicts_book_rows_and_leave_evicts_book_rows` — seed book via `ingest_verified_row`, run the eviction calls the arms make, assert empty. (The arm wiring itself is compile-checked + covered by Task 8's integration run.)
- [ ] **Step 2: Run to verify failure → implement all five changes → green.**
- [ ] **Step 3: Flag-day greps** (all must be true): `MembershipEventKind::ReachabilityAnnounce {` appears ONLY in: enum def, verify_event arms (community_membership.rs), the no-op delta arms, and materialize's no-op. Zero hits in the publishers and zero in the boot block. Same for `CommunityRelayAnnounce {`.
- [ ] **Step 4: Compile + scoped tests:** `cargo build --locked --lib`; `cargo nextest run --locked --features test-fixtures -E 'test(community_address_book) or test(address_book_sync)'`.
- [ ] **Step 5: Commit** `feat(zeb-815): flag-day — announces consumed from addrbook only; boot seeds from sidecar; kick/leave evict`

---

### Task 8: Integration + e2e coverage

**Files:**
- Modify: `src-tauri/tests/community_sync_tests.rs` (or the matching module dir `community_sync/` — follow the existing file's registration pattern)
- Modify: `e2e-harness/tests/e2e_two_node.rs`

**Coverage:**
1. **Integration (in-process, two engines):** `addrbook_replaces_announce_events_end_to_end` — build two community engines from the existing fixtures in `community_sync/` (copy the two-engine join fixture the file already uses), run `publish_own_rows` for node A with a loopback: feed A's produced `PublishRequest` payloads into `ingest_sealed_packet` on B (same membership key — both engines share the community). Assert: B's `reachability_resolver.resolve(a_actor)` non-empty; **both engines' `st.events()` contain zero `"a"`/`"b"` events**; B's book row survives a `save_addrbook`/`load_addrbook` round trip.
2. **e2e (new test `s6_addrbook_join_message_delivery`):** copy the `s_vines_publish_feed_view_reshare` preamble shape (`e2e_two_node.rs:2375-2392`: `create_community` → `generate_invite` → `poll_join_iroh` → roster poll) then alice posts a channel message and bob polls `list_channel_messages` for it (camelCase keys — assert on the DTO's `messageId`/`body` fields, `poll_until` 120 s). This exercises the full join → snapshot → resolver → dial → deliver chain on the real binary with zero announce events available. Build the spawned binary first: `cargo build --locked --bin harmony-app` from `src-tauri/`, run with `HARMONY_APP_BIN` pinned (nextest never rebuilds the spawned binary).
- [ ] **Step 1: Write the integration test; run to failure** (it fails before Task 6/7 are merged in-branch only if run standalone — here it should PASS if Tasks 1–7 are correct; treat a failure as a real defect, not a red-green formality).
- [ ] **Step 2: Write the e2e; build the app binary; run it:** from `e2e-harness/`: `HARMONY_APP_BIN=$PWD/../src-tauri/target/debug/harmony-app cargo nextest run --features e2e -E 'test(s6_addrbook_join_message_delivery)'`. Expected: PASS. (Known context: `s5d`/`s3` are red on main — ZEB-810, unrelated; do not fold fixes in.)
- [ ] **Step 3: Commit** `test(zeb-815): integration loopback + e2e join-delivery on the addrbook path`

---

### Task 9: Final gates + docs

- [ ] **Step 1:** `cargo fmt --all` then `cargo fmt --all -- --check`.
- [ ] **Step 2:** `cargo clippy --all-targets --features test-fixtures --locked -- -D warnings` (use `${pipestatus[1]}` if piping).
- [ ] **Step 3:** `scripts/test-select --context round` (budget ~50 min relink; run ONCE here, not per-task). Paste the printed `round=… bucket=…` summary line into the ledger/task report so the selection is auditable. This is the *local* gate only — final pre-merge validation is CI's full-workspace `--all-targets` suite (3 shards + roll-up), which must be green on the PR.
- [ ] **Step 4:** Spec conformance sweep — reread `docs/superpowers/specs/2026-07-26-zeb-815-community-address-book-design.md` §1–§7 against the diff; add an as-implemented note to the spec for any shipped deltas (banner pattern from the ZEB-813 spec).
- [ ] **Step 5:** Fleet-validation notes into the PR body: post-deploy assertions (membership log growth ≈ 0 events/day on the fleet community; root blob flat; addrbook.cbor present and small; join bootstrap works with a fresh profile).
- [ ] **Step 6: Commit + open PR** (`Fixes ZEB-815` in body; PR footer per convention; ONE CodeRabbit trigger at open, then zero `@`).

## Self-Review (run before handoff)

1. **Spec coverage:** §1 data model → Task 1; TTL-at-storage → Tasks 1/2; §2 live+snapshot+persist → Tasks 3/5; sealing → Task 3; snapshot-through-same-gate → Task 4/5; §3 publisher swap + flag-day + variants-stay-decodable → Task 6 (+Task 7 grep); watermark-stays → untouched (no task removes it); §4 resolver contracts + bootstrap → Tasks 4/7/8; §5 eviction → Tasks 1/7; §6 failure modes → INFO no-responder (Task 5), skew clamp (Task 1), flood caps (Task 1), snapshot row-verification (Task 4); §7 tests → Tasks 1–8; fleet validation → Task 9. ✔ No gaps found.
2. **Placeholder scan:** no TBD/TODO; the one deliberately-deferred value (persist debounce 2 s) is stated inline. ✔
3. **Type consistency:** `AddressBookRow`/`UpsertOutcome`/`ingest_verified_row`/`publish_own_rows` signatures used identically in Tasks 4/5/6/7/8; `key_fn` signature decision locked in Task 6 Step 1. ✔
