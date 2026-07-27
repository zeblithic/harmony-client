# ZEB-811 Vine Relay Fan-out Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A follow acquires a network path: creators advertise a relay set in a new public pkarr `vines` slot, relays (v1: the creator's own devices) serve descriptors + video over a new public-read iroh ALPN, and followers pull per cadence — closing the cross-WAN follow-only delivery gap.

**Architecture:** Three new surfaces mirroring proven siblings: a sixth pkarr publisher (`pkarr_vines_publisher.rs`, modeled on the identity slot but derived from the creator *address*, which is all a follower holds), a public-read `harmony/vine-relay/v1` ALPN (modeled on the community-relay pull acceptor plus the tunnel acceptor's admission semaphore, because this endpoint is anonymous), and a follower pull driver (`vine_pull_driver.rs`, modeled on `community_relay_pull_driver`). Relay-arrived descriptors enter through the byte-identical trust path the mesh uses (`VineFeedCache::on_descriptor_sample` with a synthesized canonical key) so no second trust path exists.

**Tech Stack:** Rust (src-tauri), iroh QUIC ALPNs, ciborium CBOR frames, HKDF-SHA256 slot derivation (core `harmony-pkarr` crate), tokio, e2e-harness.

**Spec:** `docs/superpowers/specs/2026-07-26-zeb-811-vine-relay-fanout-design.md`. Branch: `zeb-811-vine-relay-fanout` (off main @ 2ddf0625).

## Global Constraints

- Bounds, verbatim from the spec (every one MUST exist — the endpoint is public/unauthenticated): `VINE_RELAY_SET_MAX = 4`; descriptor page `limit ≤ 256`; `VINE_QUERY_MAX_FRAME_BYTES = 64 * 1024`; `VINE_CONTENT_MAX_FRAME_BYTES = 16 * 1024 * 1024`; `VINE_RELAY_MAX_CONCURRENT_SESSIONS = 8` (accept-then-close at capacity); `VINE_RELAY_SESSION_BYTE_BUDGET = 256 * 1024 * 1024`; per-exchange io deadline 30 s (`VINE_RELAY_IO_DEADLINE_MS = 30_000`, precedent `DEFAULT_RELAY_IO_DEADLINE_MS`, `iroh_community_relay_acceptor.rs:695`); `VINE_PULL_SKIP_MAX_CONSECUTIVE = 4`.
- Pull cadence is an alias, not a literal: `pub const VINE_PULL_INTERVAL_MS: u64 = crate::community_relay_announce::COMMUNITY_RELAY_AD_REFRESH_MS;` (the community driver does exactly this, `community_relay_pull_driver.rs:58`).
- **Single trust path:** every relay-arrived descriptor is ingested via `VineFeedCache::on_descriptor_sample(&format!("harmony/vines/{creator}"), bytes, followed_set, now_ms)` — never a parallel validation path, never `populate_from_disk`.
- The pull cursor is the lossless tuple `(created_at: u64, id: String)` with strictly-greater ordering, ascending pages (`created_at` is epoch **seconds** and collides; ids are `String` — there is no `VineId` type in this codebase).
- Descriptors travel as their original **JSON bytes** (`VineDescriptorPayload`, `lib.rs:15604`) inside CBOR frames — the puller feeds those exact bytes to ingest. The pkarr record payload is CBOR with 2-char keys (reachability conventions).
- Vine addresses are hex `String`s (`creator_address`); slot derivation ikm = `hex::decode(creator_address)` (HKDF accepts any ikm length).
- Cross-repo: Task 1 lands `PkarrCase::Vines` in `~/work/zeblithic/harmony` (own PR). The client pins `harmony-pkarr` at its **own** rev (src-tauri/Cargo.toml lines 145 and 262 — both must move together; the 13-crate `b904b0b9` lockstep set is untouched).
- Gates per task: `cargo fmt --all -- --check` and `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` from `src-tauri/`. `scripts/test-select --context task` for local iteration — paste its printed `round=… bucket=…` summary line into task reports. The `harmony-pkarr` rev bump (Task 1) is a dependency-graph change, so any round touching it needs `scripts/test-select --full` instead of the selective mapping (this is what actually ran for this PR). The full-workspace `cargo nextest run --locked --workspace --all-targets --features test-fixtures` CI-parity sweep is the final gate regardless. Before ANY e2e run: `cargo build --bin harmony-app` (the harness freshness gate hard-fails on a stale binary, `bin_resolver.rs:66`).
- RPC arg structs use `#[serde(rename_all = "camelCase", deny_unknown_fields)]`; adding RPC verbs requires updating the `registry_has_exactly_the_curated_v1_surface` pin test in `src-tauri/src/api/rpc.rs`.
- No worktrees. Commit at every task boundary at minimum.

## v1 scope guards (from the spec — do NOT build these)

- No volunteer-relay ingestion/hold machinery; the ad list and ALPN stay provenance-agnostic.
- No reverse channel (follower reactions do not reach the creator).
- No tombstone/follow-list propagation over the relay path (descriptors + video only).
- No live push — pull cadence only.

---

### Task 1: Core — `PkarrCase::Vines` (repo `~/work/zeblithic/harmony`)

**Files:**
- Modify: `crates/harmony-pkarr/src/derive.rs`

**Interfaces:**
- Consumes: nothing (first task).
- Produces: `PkarrCase::Vines` variant + salt `b"harmony.pkarr.v1.vines"`, consumed by client Tasks 2/4 via the rev bump. The pushed commit SHA is recorded in the ledger for Task 2.

**Context for the implementer:** `harmony-pkarr` derives per-flavor ephemeral Ed25519 keys via `derive_ephemeral_key(case, ikm, info)` with domain separation by a per-case HKDF salt (`derive.rs:46-56`). The vines slot must be derivable by anyone holding only the creator's public hex *address* (the follow stores addresses, not identity pubs — so `Identity`'s 64-byte-pub ikm is unusable). This is an additive change; changing existing salts is forbidden (reference vectors pin them).

- [ ] **Step 1: Branch off latest core main**

```bash
cd ~/work/zeblithic/harmony && git fetch origin && git checkout -b zeb-811-pkarr-vines-case origin/main
```

- [ ] **Step 2: Add the variant** — in `derive.rs`, after the `Friend` variant (`:42`):

```rust
    /// Case E: vine relay discovery (ZEB-811). `ikm` = the creator's public
    /// hex address, hex-decoded to raw bytes; `info` = `epoch_be`.
    ///
    /// Like Case B (`Identity`), this derives from **public** input by
    /// design: anyone who follows a creator holds their address and must be
    /// able to find the relay-set record. The follower does NOT hold the
    /// creator's 64-byte identity pub (addresses are one-way hashes of it),
    /// which is why Case B's ikm is unusable here. Authenticity comes from
    /// the record's inner `#3` identity signature plus the embedded
    /// identity-pub→address binding checked by the client.
    Vines,
```

and in `salt()` (`:47-55`):

```rust
            Self::Vines => b"harmony.pkarr.v1.vines",
```

- [ ] **Step 3: Add the reference vector + extend the separation test** — in the `tests` module, after `reference_vector_case_friend`:

```rust
    /// Reference vector — pins the Case-E (vines) keying scheme.
    /// Same DO-NOT-REGENERATE warning as the other reference vectors applies.
    #[test]
    fn reference_vector_case_vines() {
        // ikm = 16 zero bytes (placeholder hex-decoded creator address)
        // info = epoch_id 12345 in big-endian
        let ikm = [0u8; 16];
        let info = 12345u64.to_be_bytes();
        let key = derive_ephemeral_key(PkarrCase::Vines, &ikm, &info);
        let vk_hex = hex::encode(key.verifying_key().to_bytes());
        // Pin: compute once, paste here. Regenerating breaks v1 records.
        let expected = "PASTE-FROM-FIRST-RUN";
        assert_eq!(vk_hex, expected, "case-vines v1 keying must not drift");
    }
```

In `different_cases_produce_different_keys`, add `let k5 = derive_ephemeral_key(PkarrCase::Vines, &ikm, &info);` and `assert_ne!` of `k5` against `k1`–`k4`.

- [ ] **Step 4: Pin the vector.** Run `cargo test -p harmony-pkarr reference_vector_case_vines` once — it fails printing the actual hex; paste that hex into `expected` (this is how every existing vector in this file was minted — see the comment at `derive.rs:96`). Re-run: PASS.

- [ ] **Step 5: Full crate gates**

```bash
cargo test -p harmony-pkarr && cargo fmt -p harmony-pkarr -- --check && cargo clippy -p harmony-pkarr --all-targets -- -D warnings
```

- [ ] **Step 6: Commit, push, open the core PR**

```bash
git add crates/harmony-pkarr/src/derive.rs
git commit -m "pkarr: add Vines case for address-derived relay-set slots (ZEB-811)"
git push -u origin zeb-811-pkarr-vines-case
gh pr create --repo zeblithic/harmony --title "pkarr: PkarrCase::Vines — address-derived relay-set slots (ZEB-811)" --body "Adds the sixth slot flavor for ZEB-811 vine relay fan-out: salt \`harmony.pkarr.v1.vines\`, ikm = hex-decoded creator address, info = epoch_be. Additive only; existing salts and reference vectors untouched. Consumed by the harmony-client ZEB-811 branch via a harmony-pkarr rev bump."
```

Record `git rev-parse HEAD` in the ledger — Task 2 pins it. (The client can build against this branch SHA immediately; the pin moves to the merged-main SHA during client-PR convergence, after the core PR merges.)

---

### Task 2: Client — vines record codec + resolve (`pkarr_vines.rs`)

**Files:**
- Modify: `src-tauri/Cargo.toml:145` and `src-tauri/Cargo.toml:262` (harmony-pkarr rev → Task 1's SHA; both lines, same SHA)
- Create: `src-tauri/src/pkarr_vines.rs`
- Modify: `src-tauri/src/lib.rs` (one `pub mod pkarr_vines;` line beside `pub mod pkarr_identity_publisher;`)
- Modify: `src-tauri/src/vine_signing.rs` (extract one helper, see Step 3)

**Interfaces:**
- Consumes: `harmony_pkarr::{PkarrCase, derive_ephemeral_key, epoch_tolerance_window, current_epoch_id, PkarrRoutingRecord, PkarrResolver}`; `crate::reachability_record::REACHABILITY_RECORD_TTL_MS` (7 d).
- Produces (used by Tasks 4, 7):
  - `pub const VINE_RELAY_SET_MAX: usize = 4;`
  - `pub struct VineRelayEntry { pub iroh_endpoint_id: [u8; 32], pub home_relay: String }` (serde renames `ep`/`hr`; copy the byte-array serde attrs from `CommunityRelayEntry` in `community_relay_announce.rs` verbatim)
  - `pub struct VineRelayRecordPayload { pub relay_set: Vec<VineRelayEntry>, pub issued_at_ms: u64 }` (renames `rs`/`ts`)
  - `pub fn vines_ikm(creator_addr_hex: &str) -> Result<Vec<u8>, String>` (hex-decode; error on invalid hex)
  - `pub fn vines_key_for_epoch(creator_addr_hex: &str, epoch_id: u64) -> Result<ed25519_dalek::SigningKey, String>`
  - `pub fn build_vines_record_blob(payload: &VineRelayRecordPayload) -> Result<Vec<u8>, String>` (ciborium encode; error if `relay_set.len() > VINE_RELAY_SET_MAX` or encoded blob > 700 bytes — headroom under pkarr's 1104-byte packet budget, which otherwise fails as an eternal silent 60 s retry loop, `harmony-pkarr/src/publisher.rs:204-208`)
  - `pub fn verify_vines_record(rec: &harmony_pkarr::PkarrRoutingRecord, creator_addr_hex: &str, now_ms: u64) -> Result<VineRelayRecordPayload, String>`
  - `pub async fn resolve_vine_relays(resolver: &harmony_pkarr::PkarrResolver, creator_addr_hex: &str, now_ms: u64) -> Result<Vec<VineRelayEntry>, String>`

**Design note (spec deviation, deliberate):** the spec's §1 payload table lists an inner `sg identity_signature`. The pkarr envelope (`PkarrRoutingRecord`) already carries the `#3` identity signature over the blob plus the embedded 64-byte identity pub (`harmony-pkarr/src/record.rs:58-64`), and the reachability flavor zero-fills its inner signature on the pkarr path for exactly this reason (`lib.rs:9227-9228`). The vines payload therefore carries only `rs`/`ts`; authenticity = `verify_inner_sig()` + freshness + the identity-pub→address binding. Task 11 records this in the spec's as-implemented notes.

- [ ] **Step 1: Bump the pin.** Replace `80f6d80858f283d4f4094d483d548e50b8c4e107` with Task 1's SHA at Cargo.toml:145 and :262. Run `cargo fetch` from `src-tauri/` to confirm the rev resolves.

- [ ] **Step 2: Write failing unit tests** in `pkarr_vines.rs` `#[cfg(test)]`:

```rust
#[test]
fn payload_round_trips_via_cbor() {
    let p = VineRelayRecordPayload {
        relay_set: vec![VineRelayEntry { iroh_endpoint_id: [7u8; 32], home_relay: "https://relay.example".into() }],
        issued_at_ms: 1_000,
    };
    let blob = build_vines_record_blob(&p).unwrap();
    let back: VineRelayRecordPayload = ciborium::from_reader(blob.as_slice()).unwrap();
    assert_eq!(back.relay_set.len(), 1);
    assert_eq!(back.relay_set[0].iroh_endpoint_id, [7u8; 32]);
    assert_eq!(back.issued_at_ms, 1_000);
}

#[test]
fn oversize_relay_set_is_rejected_at_build() {
    let entry = VineRelayEntry { iroh_endpoint_id: [1u8; 32], home_relay: "https://r".into() };
    let p = VineRelayRecordPayload { relay_set: vec![entry; VINE_RELAY_SET_MAX + 1], issued_at_ms: 0 };
    assert!(build_vines_record_blob(&p).is_err());
}

#[test]
fn slot_derivation_is_stable_and_address_scoped() {
    let k1 = vines_key_for_epoch("aabbccdd00112233aabbccdd00112233", 42).unwrap();
    let k2 = vines_key_for_epoch("aabbccdd00112233aabbccdd00112233", 42).unwrap();
    let k3 = vines_key_for_epoch("ffeeddcc00112233aabbccdd00112233", 42).unwrap();
    assert_eq!(k1.verifying_key(), k2.verifying_key());
    assert_ne!(k1.verifying_key(), k3.verifying_key());
    assert!(vines_key_for_epoch("not-hex", 42).is_err());
}

#[test]
fn record_verification_binds_identity_to_address() {
    // Build a real record signed by a real identity, then verify against the
    // right and wrong addresses. Use vine_signing's test identity helpers if
    // present; otherwise mint an Identity the way vine_signing's own tests do.
    let identity = crate::vine_signing::test_identity(); // reuse/mirror vine_signing test helper
    let addr = crate::vine_signing::signer_address(&identity);
    let payload = VineRelayRecordPayload { relay_set: vec![], issued_at_ms: 5_000 };
    let blob = build_vines_record_blob(&payload).unwrap();
    let rec = harmony_pkarr::PkarrRoutingRecord::sign_new(
        blob, identity_pub_64(&identity), 5_000, 5_000 + crate::reachability_record::REACHABILITY_RECORD_TTL_MS,
        identity_signing_key(&identity),
    ).unwrap();
    assert!(verify_vines_record(&rec, &addr, 6_000).is_ok());
    assert!(verify_vines_record(&rec, "00112233445566770011223344556677", 6_000).is_err(), "wrong address must fail the binding");
}
```

(`test_identity` / `identity_pub_64` / `identity_signing_key`: reuse the exact helpers `vine_signing.rs`'s own test module uses to mint a signing identity — read that module first and mirror it; if a helper is `#[cfg(test)]`-private there, add a `pub(crate) #[cfg(any(test, feature = "test-fixtures"))]` re-export rather than duplicating key-minting code.)

- [ ] **Step 3: The address-binding helper.** `verify_vines_record` must compute a creator address from the record's embedded 64-byte identity pub and compare it to `creator_addr_hex`. `vine_signing.rs` already performs exactly this pubkey→address binding inside `verify_signed` (used by `verify_descriptor`, `vine_signing.rs:189`). Extract that derivation into `pub(crate) fn address_for_identity_pub_hex(identity_pub_hex: &str) -> Result<String, String>` in `vine_signing.rs`, make `verify_signed` call it (no behavior change — its existing tests hold), and call it from `verify_vines_record`.

- [ ] **Step 4: Implement the module.** `verify_vines_record` in order: `rec.verify_inner_sig()` → `rec.verify_freshness(now_ms)` → `address_for_identity_pub_hex(hex::encode(rec.identity_pub)) == creator_addr_hex` (use the record struct's actual identity-pub accessor/field per `harmony-pkarr/src/record.rs:15-42`) → decode blob to `VineRelayRecordPayload` → reject `relay_set.len() > VINE_RELAY_SET_MAX`. `resolve_vine_relays`: derive the 3-epoch verifying-key window exactly like the identity resolve at `lib.rs:63178-63190` (`harmony_pkarr::epoch_tolerance_window(now_ms)` → `vines_key_for_epoch(...).verifying_key()` per epoch) → `resolver.resolve_window_freshest(&keys).await` → `verify_vines_record` → `Ok(payload.relay_set)`.

- [ ] **Step 5: Run the tests**

```bash
cd src-tauri && cargo test --lib pkarr_vines
```
Expected: all 4 pass.

- [ ] **Step 6: Gates + commit**

```bash
cargo fmt --all && cargo clippy --lib -- -D warnings
git add -A && git commit -m "vines pkarr slot: record codec, derivation, windowed resolve (ZEB-811)"
```

---

### Task 3: `share_vines_publicly` setting + vine-settings RPC surface

**Files:**
- Modify: `src-tauri/src/vine_settings.rs`
- Modify: `src-tauri/src/lib.rs` (NodeState field ~`:786`, boot apply ~`:11773-11780`, `VineSettingsDto` `:19147`, both `_impl`s `:19157`/`:19175`, both tauri commands)
- Modify: `src-tauri/src/api/rpc.rs` (two new verbs + curated-surface pin test)
- Modify: `src/App.svelte` (~`:4157-4163`), `src/lib/components/VineFeed.svelte` (~`:64-99`)

**Interfaces:**
- Produces: `VineSettings.share_vines_publicly: bool` (default **true**), `NodeState.vine_share_publicly: bool`, `set_vine_settings_impl(state, share_follows: bool, share_vines_publicly: bool)`, RPC verbs `get_vine_settings` / `set_vine_settings`. Task 4 reads the flag; Task 10's e2e flips it over RPC.

**Serde trap (this is the reason this task exists as its own reviewable unit):** `#[serde(default)]` on a `bool` yields `false`. A legacy `vine_settings.json` without the new field must default **true** (vines are public by intent — spec product decision 4). Use `#[serde(default = "default_true")]` with `fn default_true() -> bool { true }`. The pinning test `legacy_file_without_floor_field_defaults_to_zero` (`vine_settings.rs:135`) is the template.

- [ ] **Step 1: Failing test** in `vine_settings.rs`:

```rust
#[test]
fn legacy_file_without_share_vines_publicly_defaults_true() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vine_settings.json");
    std::fs::write(&path, r#"{"version":1,"share_follows":false,"last_published_updated_at":7}"#).unwrap();
    let s = load_or_default(&path);
    assert!(!s.share_follows);
    assert!(s.share_vines_publicly, "legacy files must default the new gate ON");
}
```

- [ ] **Step 2: Run it** — `cargo test --lib vine_settings` — FAIL (no such field).

- [ ] **Step 3: Add the field** with `#[serde(default = "default_true")]`, update `Default` (`true`), add `fn default_true() -> bool { true }`. Re-run: PASS (plus existing tests).

- [ ] **Step 4: Thread it through lib.rs.** `NodeState.vine_share_publicly: bool` (test default `true`, mirroring `vine_share_follows` at `:786`/`:1865`); boot apply beside `vine_share_follows`; `VineSettingsDto { share_follows, share_vines_publicly }`; `set_vine_settings_impl` gains the second bool — keep the existing transactional-disable ordering for `share_follows` untouched, and persist-first for the new flag (the live publisher toggle arrives in Task 4; leave a `// ZEB-811 Task 4 wires the publisher enable/disable here` seam comment). The `save` call site reconstructs the full `VineSettings` literal (`:19195-19205`) — the compiler forces the update.

- [ ] **Step 5: RPC verbs.** In `rpc.rs`, register `get_vine_settings` (EmptyArgs → `VineSettingsDto`) and `set_vine_settings` (args struct `SetVineSettingsArgs { share_follows: bool, share_vines_publicly: bool }`, camelCase + deny_unknown_fields) beside the vine cluster (`:1168-1234`), delegating to the `_impl`s. Add both names to the `registry_has_exactly_the_curated_v1_surface` pin list.

- [ ] **Step 6: Frontend.** In `App.svelte`, extend the settings load/save to carry `shareVinesPublicly`; in `VineFeed.svelte`, add a second toggle following the `shareFollows` pattern including the `shareFollowsLoaded` disabled-until-read gate (`:96-99`). Copy: label "Share my vines publicly", help text "Publishes a relay record so followers outside your communities can fetch your vines."

- [ ] **Step 7: Gates + commit**

```bash
cargo test --lib vine_settings && cargo test --lib rpc && cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add -A && git commit -m "vine settings: share_vines_publicly gate + headless vine-settings RPC (ZEB-811)"
```

---

### Task 4: `pkarr_vines_publisher.rs` + boot wiring

**Files:**
- Create: `src-tauri/src/pkarr_vines_publisher.rs` (model: `pkarr_identity_publisher.rs`, 141 lines — read it first, it is the smallest flavor)
- Modify: `src-tauri/src/lib.rs` (module decl; construct beside the five flavor managers at `:9243-9296`; NodeState stash mirroring `pkarr_identity_publisher` at `:1633`/`:9836-9845`/`:12143-12180`; `stop_inner` clear at `:1768-1797`; enable/disable calls in `set_vine_settings_impl`'s Task 3 seam; force-republish after `publish_vine_descriptor` success)

**Interfaces:**
- Consumes: Task 2 (`vines_key_for_epoch` via an `EphemeralKeyBuilder` closure, `build_vines_record_blob`, `VineRelayEntry`, `VINE_RELAY_SET_MAX`), Task 3 (`vine_share_publicly`), `harmony_pkarr::PkarrPublisher::{register, unregister}`, `PkarrRoutingRecord::sign_new` with the `#3` identity signing key + `REACHABILITY_RECORD_TTL_MS`.
- Produces: `PkarrVinesPublisher` with `pub(crate) const HANDLE: &str = "vines";`, `pub async fn enable(&self)`, `pub async fn disable(&self)`, `pub async fn republish(&self)` (= re-register, which wakes the core publish loop — `publisher.rs:97`). Task 10's e2e relies on publish-on-first-vine.

**Behavior contract (spec §1):**
- Registered iff `share_vines_publicly` AND the node has at least one own published vine (the blob builder checks both at build time — the registry cadence re-invokes it each publish, so fresh data flows without re-registration).
- The relay entry is **self**: own iroh endpoint id + `home_relay()` read **fresh at build time** (never boot-frozen — the ZEB-521 lesson, `reference_transport_architecture_truth`; the reachability blob builder at `lib.rs:9152-9240` shows the fresh-read pattern).
- Key builder: `derive_ephemeral_key(PkarrCase::Vines, &vines_ikm(&own_addr)?, &current_epoch_id(at_ms).to_be_bytes())` — re-derived **every publish** (epoch-boundary rule, `publisher.rs:234`).
- Triggers: registered at boot when the gate passes (startup publish comes from the core loop); `enable()`/`disable()` from `set_vine_settings_impl` using the detached persist-first pattern of `set_identity_discoverable_detached` (`lib.rs:59479-59501`); `republish()` after every successful `publish_vine_descriptor` (covers "first vine ever" flipping the has-vines gate; cheap no-op otherwise). Idle/epoch republish cadence is the core `PkarrPublisher`'s (`compute_next_publish_at`); no separate network-change watcher in v1 — the record carries an endpoint id + relay URL, both stable across address churn (iroh re-resolves), unlike reachability's direct-address list. Note this as a deliberate v1 simplification in the module doc.

- [ ] **Step 1: Failing unit tests** (in the new module; construct the publisher with closure seams so no network is needed):

```rust
#[test]
fn blob_absent_when_gate_off_or_no_vines() {
    // builder returns None (skip publish) when share flag is off or own vine count is 0
    let b = test_builder(/*share=*/ false, /*own_vines=*/ 3);
    assert!(b().is_none());
    let b = test_builder(true, 0);
    assert!(b().is_none());
}

#[test]
fn blob_contains_self_entry_when_enabled() {
    let b = test_builder(true, 3);
    let blob = b().expect("enabled with vines publishes");
    let p: crate::pkarr_vines::VineRelayRecordPayload = ciborium::from_reader(blob.as_slice()).unwrap();
    assert_eq!(p.relay_set.len(), 1);
    assert_eq!(p.relay_set[0].iroh_endpoint_id, TEST_SELF_ENDPOINT);
}
```

Structure the module so the blob-builder logic is a pure `fn build_blob(share: bool, own_vine_count: usize, endpoint_id: [u8;32], home_relay: String, now_ms: u64) -> Option<Vec<u8>>` that both the tests and the registered closure call — the closure supplies live values from its captured `Arc`s (endpoint, `VineFeedCache` count via a `has_own_vines: Arc<dyn Fn() -> usize + Send + Sync>` seam, settings flag via `Arc<AtomicBool>` kept in sync by `set_vine_settings_impl`).

- [ ] **Step 2: Run tests** — FAIL (module absent) → implement → PASS.

- [ ] **Step 3: Wire boot + toggle + republish call sites** per the Files list. The publish-site hook: in `publish_vine_descriptor`, after the successful `reply_rx.await`, `if let Some(p) = vines_publisher { tokio::spawn(async move { p.republish().await }) }` — spawned, never inline-awaited (start_node is not on this path, but the IPC should not block on pkarr I/O either).

- [ ] **Step 4: Gates + commit**

```bash
cargo test --lib pkarr_vines && cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add -A && git commit -m "vines pkarr publisher: self relay-set record behind share_vines_publicly (ZEB-811)"
```

---

### Task 5: `VineFeedCache` — signature retention, per-creator pages, self-ingest at publish

**Files:**
- Modify: `src-tauri/src/vine_feed_cache.rs`
- Modify: `src-tauri/src/lib.rs` (`publish_vine_descriptor` tail — self-ingest)

**Interfaces:**
- Consumes: existing `DescriptorOnDisk` (`vine_feed_cache.rs:97-113`), `CachedVine` (`:286`), `on_descriptor_sample` (`:573`).
- Produces (Tasks 6, 7 depend on these exact signatures):
  - `pub fn descriptors_for_creator_page(&self, creator: &str, after: &(u64, String), limit: usize) -> Vec<VineDescriptorPayload>` — ascending `(created_at, id)` tuple order, strictly greater than `after`, only rows whose `sig` (or `device_sig`) is present (a relay must not serve unverifiable rows), `limit` already clamped by the caller.
  - `pub fn last_received_ms_for_creator(&self, creator: &str) -> Option<u64>` — max `received_at_ms` over that creator's cached rows.

**Why signature retention:** wire descriptors carry `identity_pub`/`sig`/`device_sig` as `Option<String>`, but `DescriptorOnDisk` drops all three (verify-once-at-ingest, documented at `lib.rs:15622-15629`) — so today a relay could not serve verifiable descriptors after a restart. Retain the three fields on disk (same principle as ZEB-815's "the book keeps the signed original"). Old files deserialize with `None` (serde default) and are simply not served. Update the `lib.rs:15622-15629` doc comment: sigs are now retained for relay serving; verification still happens once at ingest.

- [ ] **Step 1: Failing tests** (in `vine_feed_cache.rs`'s test module, using its existing signed-descriptor test helpers — read neighboring tests first and reuse their fixture builders):

```rust
#[test]
fn disk_round_trip_retains_wire_signatures() {
    // ingest a properly signed descriptor via on_descriptor_sample, save,
    // load into a fresh cache, and assert sig/identity_pub survive.
}

#[test]
fn legacy_disk_rows_without_sigs_still_load_and_are_not_served() {
    // write a DescriptorOnDisk JSON without the new fields, load, assert the
    // row lists in list_descriptors() but is absent from descriptors_for_creator_page.
}

#[test]
fn creator_page_orders_by_tuple_and_breaks_created_at_ties_by_id() {
    // three signed rows for one creator: created_at 10/"b", 10/"a", 11/"c".
    // page after (0,"") limit 2 => ids ["a","b"]; after (10,"b") => ["c"].
    // rows for a second creator never appear.
}

#[test]
fn last_received_ms_tracks_the_freshest_row_per_creator() { /* two rows, max wins; unknown creator => None */ }
```

- [ ] **Step 2: Run** — FAIL → implement: add the three `Option<String>` fields to `DescriptorOnDisk` with `#[serde(default, skip_serializing_if = "Option::is_none")]`, populate in the save path, restore in load; add the two accessors. Tie-break comparison: `(a.created_at, a.id.as_str()) > (after.0, after.1.as_str())` with `sort_by` on the same tuple. → PASS.

- [ ] **Step 3: Self-ingest at publish.** In `publish_vine_descriptor` after the successful zenoh reply: serialize the signed descriptor once, then feed those bytes through the standard path so the publisher's own cache holds a serveable copy deterministically (today it depends on zenoh local loopback):

```rust
// ZEB-811: the vine-relay serve source is this cache. Own descriptors must
// be present with their signatures regardless of zenoh loopback semantics;
// on_descriptor_sample is id-keyed first-write-wins, so a loopback copy
// dedupes to a no-op.
let key = format!("harmony/vines/{}", descriptor.creator_address);
let bytes = serde_json::to_vec(&descriptor).map_err(|e| format!("serialize: {e}"))?;
if let (Ok(mut cache), Ok(set)) = (vine_feed_cache.lock(), followed_set.lock()) {
    let now_ms = /* same clock the event loop uses */;
    let _ = cache.on_descriptor_sample(&key, &bytes, &set, now_ms);
}
```

(Fetch the two `Arc` handles from `NodeState` in the same lock that fetches `publish_tx`; if either is absent — tests construct partial states — skip silently.)

- [ ] **Step 4: Gates + commit**

```bash
cargo test --lib vine_feed_cache && cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add -A && git commit -m "vine feed cache: retain wire sigs on disk, per-creator tuple pages, publish self-ingest (ZEB-811)"
```

---

### Task 6: `harmony/vine-relay/v1` — wire protocol + public serve acceptor

**Files:**
- Create: `src-tauri/src/vine_relay.rs`
- Modify: `src-tauri/src/iroh_endpoint.rs` (`pub mod alpn` `:37-89` — add `HARMONY_VINE_RELAY_V1`; `all_client_alpns()` `:95-108`)
- Modify: `src-tauri/src/zenoh_iroh_transport.rs` (OnceLock field beside `:200-202`, init beside `:277`, installer beside `install_community_relay_pull_acceptor` `:446-451`, dispatch branch immediately after the community-relay-pull branch at `:760-772`)
- Modify: `src-tauri/src/lib.rs` (boot install beside the community-relay acceptor install at `:10528-10548`)
- Modify: `src-tauri/src/network_health.rs` (serving-side telemetry writer + wire struct, modeled on `CommunityRelayServingTelemetry` `:581` / `CommunityRelayServingHealth` `:316` — the service-field/snapshot wiring is Task 8)

**Interfaces:**
- Consumes: Task 5's `descriptors_for_creator_page`; `crate::iroh_framing::{read_len_prefixed, write_len_prefixed}` (LE); `ContentStore::get_local` (`content_store.rs:75`) via the serve ctx; `ContentId::verify_hash`.
- Produces (Task 7's client side and Task 9 depend on these):
  - `pub const VINE_RELAY_ALPN: &[u8] = b"harmony/vine-relay/v1";` (re-exported from `iroh_endpoint::alpn` as `HARMONY_VINE_RELAY_V1`)
  - Constants from Global Constraints: `VINE_QUERY_MAX_FRAME_BYTES`, `VINE_CONTENT_MAX_FRAME_BYTES`, `VINE_RELAY_MAX_CONCURRENT_SESSIONS`, `VINE_RELAY_SESSION_BYTE_BUDGET`, `VINE_RELAY_IO_DEADLINE_MS`, `VINE_PULL_PAGE_LIMIT_MAX: u16 = 256`, `VINE_CONTENT_CHUNK_BYTES: usize = 4 * 1024 * 1024`
  - Frames (CBOR, 2-char keys, all length-prefixed LE):
    - `VinePullRequest` — tagged enum with two variants so one session can interleave descriptor pages and content fetches: `Query(VinePullQuery)` / `Content(VineContentRequest)`; encode as `{ "q": VinePullQuery }` or `{ "c": VineContentRequest }` (single-key map = cheap discriminant)
    - `pub struct VinePullQuery { pub creator_addr: String /* ca */, pub after_created_at: u64 /* at */, pub after_id: String /* ai */, pub limit: u16 /* lm */ }`
    - `pub struct VinePullResponse { pub descriptors: Vec<serde_bytes::ByteBuf> /* ds — each element is the descriptor's original JSON bytes */ }`
    - `pub struct VineContentRequest { pub cid_hex: String /* cd */ }`
    - `pub struct VineContentMeta { pub ok: bool /* ok */, pub size: u64 /* sz */ }` — followed, when `ok`, by `ceil(size / VINE_CONTENT_CHUNK_BYTES)` raw chunk frames
  - `pub trait VineRelayServeCtx: Send + Sync` with `fn descriptors_page(&self, creator: &str, after: &(u64, String), limit: usize) -> Vec<Vec<u8>>` (JSON bytes, sig-retained rows only), `fn video_cid_is_served(&self, cid_hex: &str) -> bool` (cid ∈ video_cids of sig-retained cached descriptors — the allowlist is resolved from the node's own store, never the requester's claims), `async fn video_bytes(&self, cid_hex: &str) -> Option<Vec<u8>>` (get_local + `verify_hash` before serving; **never** a network fetch — an anonymous requester must not be able to make this node dial the mesh)
  - `pub struct VineRelayAcceptor` with `pub async fn handle_connection(&self, conn: iroh::endpoint::Connection)` and a `with_telemetry` builder
- Admission: model on `iroh_tunnel_acceptor.rs`'s `InboundAdmission` (`:44-94`) — `Semaphore(VINE_RELAY_MAX_CONCURRENT_SESSIONS)`, `try_admit() -> Option<OwnedSemaphorePermit>`, at capacity accept-then-`conn.close(1u32.into(), b"busy")`, reject-warn throttled to one log per 30 s (`REJECT_WARN_INTERVAL_MS` precedent).
- Session loop: repeat { read request frame (cap `VINE_QUERY_MAX_FRAME_BYTES`; oversize prefix → close without allocating — the codec's bound-check-before-alloc does this, `iroh_framing.rs:136-138`) → dispatch → write response } until client EOF / `VINE_RELAY_IO_DEADLINE_MS` idle timeout per exchange / session byte budget exceeded (count every response byte; on breach, close — the follower resumes from its cursor). Clamp `limit` to `VINE_PULL_PAGE_LIMIT_MAX`. Every failure path closes uniformly (anti-oracle posture, `iroh_community_relay_acceptor.rs:963`).

- [ ] **Step 1: Failing codec + handler unit tests** (pure, via a mock `VineRelayServeCtx` and `tokio::io::duplex` streams — the community acceptor's tests show the duplex pattern):

```text
frame_round_trip_query_and_content        — encode/decode both request variants + response
oversize_query_frame_closes_without_reply — write a frame with a 128 KiB length prefix; handler closes
page_serves_ascending_tuples_with_limit_clamp — ctx with 300 rows; limit 500 → 256 returned, ascending
content_refused_for_unlisted_cid          — video_cid_is_served=false → VineContentMeta{ok:false}
content_streams_chunks_and_counts_budget  — 9 MiB blob → meta{ok,9MiB} + 3 chunks; session budget forced to 8 MiB → close mid-stream
admission_cap_rejects_ninth_session       — 8 permits held → try_admit None
```

- [ ] **Step 2: Run → FAIL → implement the module → PASS.**

- [ ] **Step 3: Registration wiring.** Add the ALPN constant + `all_client_alpns()` entry; OnceLock + installer + boot call + dispatch branch, copying the community-relay-pull branch shape verbatim (`zenoh_iroh_transport.rs:760-772`): `tokio::spawn(async move { acceptor.handle_connection(conn).await })` per connection, with `InboundAdmission` inside `handle_connection` deciding serve-vs-close. Production `VineRelayServeCtx` impl: holds `Arc<Mutex<VineFeedCache>>` + `Arc<dyn ContentStore>`; boot-install it beside the community relay acceptor (`lib.rs:10528-10548`). Serving telemetry: `VineRelayServingTelemetry { sessions_served, sessions_rejected, sessions_failed, bytes_served, last_served_ms }` (AtomicU64s + `summary() -> VineRelayServingHealth`, camelCase wire struct) in `network_health.rs`, recorded from the acceptor.

- [ ] **Step 4: Gates + commit**

```bash
cargo test --lib vine_relay && cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add -A && git commit -m "vine relay ALPN: public-read descriptor+content serve with admission caps (ZEB-811)"
```

---

### Task 7: `vine_pull_driver.rs` — follower pull driver

**Files:**
- Create: `src-tauri/src/vine_pull_driver.rs` (model: `community_relay_pull_driver.rs` — read `:194-388` and `:502-525` first)
- Modify: `src-tauri/src/network_health.rs` (pull-side telemetry writer + wire structs, modeled on `CommunityRelayPullTelemetry` `:661-756`)

**Interfaces:**
- Consumes: Task 2 `resolve_vine_relays` + `VineRelayEntry`; Task 5 `last_received_ms_for_creator` + `on_descriptor_sample` (via ctx trait); Task 6 frames + `VINE_RELAY_ALPN`.
- Produces (Task 8 wires these):
  - `pub const VINE_PULL_INTERVAL_MS: u64 = crate::community_relay_announce::COMMUNITY_RELAY_AD_REFRESH_MS;`
  - `pub const VINE_PKARR_RESOLVE_COOLDOWN_MS: u64 = 15 * 60 * 1000;` (mirrors `PKARR_REFRESH_COOLDOWN`, `reachability_resolver.rs:37` — that constant is `pub(crate)` to another module tree; alias with a doc comment naming the source)
  - `pub type FollowedCreatorsFn = Arc<dyn Fn() -> Vec<String> + Send + Sync>;` (sync closure over `NodeState.followed_set`'s own mutex — unlike the community driver, NO refresher task is needed; `lib.rs:800` is already a sync `Arc<Mutex<HashSet<String>>>`)
  - `pub trait VinePullTransport: Send + Sync { async fn pull_pages(&self, relay: &VineRelayEntry, creator: &str, cursor: (u64, String), ingest: &dyn VineIngestCtx) -> Result<PullSessionResult, String>; }`
  - `pub trait VineIngestCtx: Send + Sync { fn ingest_descriptor(&self, creator: &str, json_bytes: &[u8], now_ms: u64) -> IngestVerdict; }` with `pub enum IngestVerdict { Advance, SkipInvalid, Halt }`
  - `pub struct PullSessionResult { pub cursor: (u64, String), pub ingested: u32, pub skipped_invalid: u32 }`
  - `pub struct VinePullDriver` with `new(...)`, builder `with_telemetry`, `pub fn wake_handle(&self) -> Arc<Notify>`, `pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()>`
  - Sidecar: `vine_pull.cbor` in `app_data_dir` beside `follows.json` — `pub struct VinePullSidecar { pub per_creator: BTreeMap<String, CreatorPullState> }`, `pub struct CreatorPullState { pub cursor: (u64, String), pub last_pull_attempt_ms: u64, pub consecutive_skips: u32, pub relay_set: Vec<VineRelayEntry>, pub relays_fetched_at_ms: u64 }`; `save_vine_pull(path, &sidecar)` / `load_vine_pull(path) -> VinePullSidecar` copying the addrbook helper shape exactly (`community_address_book.rs:307`/`:323`): tmp+rename, no fsync (fully peer-recoverable data), load infallible (missing/corrupt → default). The cached `relay_set` is a **dialing hint only** — descriptors and content are independently verified on arrival, so a tampered sidecar cannot inject state (note this in the struct doc, citing the ZEB-815 boot-seed rule at `lib.rs:8313-8320`).
  - Pull-side telemetry `VinePullTelemetry` mirroring `CommunityRelayPullTelemetry`: `passes_run, last_pass_ms, sessions_ok, sessions_failed, descriptors_ingested, last_ingest_ms, passes_no_relay, recent` ring (`creator_short` = first 4 bytes hex, `relay_endpoint_short`, `outcome`, `ingested`, `captured_at_ms`). Copy the three load-bearing conventions verbatim: `record_pass_start()` first and unconditional in the pass; no-relay is a counter only (never a ring row); `ingested == 0` is success.

**Per-pass algorithm (the core of the module — implement exactly this):**

```text
record_pass_start()
prune sidecar entries for creators no longer in followed()
for creator in followed():
    st = sidecar entry (default: cursor (0,""), skips 0)
    # bounded mesh-live skip — recency is not completeness:
    if st.cursor != (0,"")                       # first follow always pulls (backfills history the mesh never carried)
       and last_received_ms_for_creator(creator) > st.last_pull_attempt_ms   # mesh delivered since we last tried
       and st.consecutive_skips < VINE_PULL_SKIP_MAX_CONSECUTIVE:
        st.consecutive_skips += 1; continue
    st.consecutive_skips = 0
    st.last_pull_attempt_ms = now_ms
    # relay set (cooldown-gated resolve, cached set as fallback):
    if now_ms - st.relays_fetched_at_ms >= VINE_PKARR_RESOLVE_COOLDOWN_MS:
        match resolve_vine_relays(...): Ok(rs) => { st.relay_set = rs; st.relays_fetched_at_ms = now_ms }
                                        Err(_) => {}   # keep cached hint
    candidates = st.relay_set minus any entry whose iroh_endpoint_id == self_endpoint_id   # ZEB-806 lesson, day one
    if candidates.is_empty(): telemetry.record_no_relay(); continue
    relay = candidates[0]                         # freshest-first: record order as published
    match transport.pull_pages(relay, creator, st.cursor, ingest):
        Ok(res) => { st.cursor = res.cursor; telemetry.record_session_ok(...) }
        Err(_)  => telemetry.record_session_failed(...)
save sidecar (snapshot → spawn_blocking write, lock never held across the write)
```

`pull_pages` (production impl): connect via `iroh::EndpointAddr::new(ep_id).with_relay_url(home_relay)` + `VINE_RELAY_ALPN` exactly like `IrohRelayPullTransport::pull_session` (`community_relay_pull_driver.rs:99-174`), whole exchange under `tokio::time::timeout(VINE_RELAY_IO_DEADLINE_MS)`; loop: send `Query{cursor, limit: 256}` → read response → for each descriptor-bytes: `ingest.ingest_descriptor(...)` — `Advance` and `SkipInvalid` (bad signature: log + count, each descriptor is independently dual-signed so skipping cannot forge or hide later ones) both advance the cursor to that row's `(created_at, id)` (parse the two fields from the JSON before ingest; a row whose JSON does not even parse is `SkipInvalid` but CANNOT advance the cursor — use the last good tuple); `Halt` (infrastructure failure: cache lock poisoned — `on_descriptor_sample` returning `None`) stops the session with the cursor at the last durable row so the next session retries. Stop paging when a page returns fewer than `limit` rows. Sessions are sequential; one bad relay/creator is logged and never aborts the pass. Production `VineIngestCtx`: locks `vine_feed_cache` + `followed_set`, calls `on_descriptor_sample(&format!("harmony/vines/{creator}"), bytes, &set, now_ms)`, maps `Some(Inserted|AlreadyPresent)` → `Advance`, `Some(Rejected)` → `SkipInvalid`, `None` → `Halt`.

- [ ] **Step 1: Failing unit tests** (mock transport + mock ingest; `tokio::time::pause` for cadence-free testing):

```text
first_follow_always_pulls_and_backfills          — cursor (0,"") never skips
mesh_live_skip_is_bounded_at_four                — recency newer than attempt → skips 1..4, 5th pass pulls (repair) and resets
self_relay_entry_is_never_dialed                 — relay set = [self, other] → only other dialed
cursor_advances_past_invalid_but_not_past_halt   — page [good, bad-sig, good, HALT-row]: cursor lands on row 3's tuple, session ends
mesh_delivered_duplicate_is_a_cheap_no_op        — ingest returns AlreadyPresent (mesh got there first) → Advance, ingested count 0, session ok
unparseable_row_does_not_advance_cursor          — garbage JSON at page end → cursor = last good tuple
resolve_cooldown_uses_cached_relay_hint          — resolver errors inside cooldown → cached set dialed
unfollowed_creator_state_is_pruned               — followed() shrinks → sidecar entry gone after pass
sidecar_round_trip_and_corrupt_file_loads_empty  — save/load; truncated file → default
telemetry_pass_counter_beats_before_target_read  — record_pass_start fires even with zero followed creators
```

- [ ] **Step 2: Run → FAIL → implement → PASS.**

- [ ] **Step 3: Gates + commit**

```bash
cargo test --lib vine_pull && cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add -A && git commit -m "vine pull driver: cadenced relay pulls with tuple cursor and bounded mesh-live skip (ZEB-811)"
```

---

### Task 8: Wiring — spawn, wake-on-follow, network-health section

**Files:**
- Modify: `src-tauri/src/lib.rs` (start_node spawn; NodeState handle + wake fields; stop_node/stop_inner abort; error-path cleanup; follow/unfollow IPC wake+prune at `:19240-19247` / `:19284-19291`)
- Modify: `src-tauri/src/network_health.rs` (service fields, installers, snapshot assembly beside `:1654-1663`; shape-pin test)
- Modify: `src-tauri/src/event_loop.rs` (`:5142-5148` — comment only: the Follow/Unfollow no-op arm's comment now points at the driver wake living in the IPC impls)

**Interfaces:**
- Consumes: Task 7's driver + telemetry; Task 6's serving telemetry.
- Produces: `NetworkHealthSnapshot.vine_relay: Option<VineRelayHealth>` where `pub struct VineRelayHealth { pub serving: VineRelayServingHealth, pub pulling: VinePullingHealth }` — assembled with the same `(None, None) => None, (s, p) => Some(...unwrap_or_default())` shape as `community_relay` (`network_health.rs:1654-1663`; the "unwired side reports zeroed defaults rather than suppressing the section" rule at `:1648-1653` applies). Task 10's e2e polls `vineRelay.pulling`.

- [ ] **Step 1: Spawn wiring in start_node**, copying the community pull driver's row-by-row pattern (the table in the ledger recon; anchors `lib.rs:4125`, `:10621-10724`, `:12033-12034`, `:12339-12341`, `:2751-2753`, error-path `:12536`/`:12565`/`:12746-12748`): local `Option<JoinHandle>`, build production transport/ingest/serve ctxs, `VinePullDriver::new(...).with_telemetry(...)`, `driver.spawn()` — **spawned, never inline-awaited** (the start_node inline-await hazard, `lib.rs:6015-6022`). Sidecar loaded before spawn (`load_vine_pull`), saved by the driver at pass boundaries. Stash `driver.wake_handle()` on `NodeState` as `vine_pull_wake: Option<Arc<Notify>>`.

- [ ] **Step 2: Wake-on-follow.** In `follow_vine_creator_impl` (after the `followed_set` insert + `FollowRequest` send, `lib.rs:19240-19247`): `if let Some(w) = &guard.vine_pull_wake { w.notify_one(); }` — a fresh follow pulls within seconds instead of waiting out the ~7.5 min cadence (the driver's first action for it is a full backfill from cursor (0,"")). In `unfollow_vine_creator_impl`: also `notify_one()` — the next pass prunes the sidecar entry. Update the stale event-loop comment (`event_loop.rs:5142-5147`) to name this seam.

- [ ] **Step 3: Snapshot assembly + shape test.** Add the service fields + `set_vine_relay_serving_source` / `set_vine_pull_source` installers (model `:1393-1398`), assemble `vine_relay` in `snapshot()`, install both at boot (model `:12339-12341`). Shape-pin test modeled on `network_health.rs:5132-5163`: assert the `vineRelay.pulling` camelCase key set (`passesRun`, `lastPassMs`, `sessionsOk`, `sessionsFailed`, `descriptorsIngested`, `lastIngestMs`, `passesNoRelay`, `recent`) and `serving` (`sessionsServed`, `sessionsRejected`, `sessionsFailed`, `bytesServed`, `lastServedMs`); extend `network_health_snapshot_empty_is_well_formed` (`:3211`).

- [ ] **Step 4: Gates + commit**

```bash
cargo test --lib network_health && cargo test --lib vine_pull && cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add -A && git commit -m "vine pull wiring: spawn at boot, wake on follow, vineRelay health section (ZEB-811)"
```

---

### Task 9: Video fetch fallback — `fetch_vine_video`

**Files:**
- Modify: `src-tauri/src/lib.rs` (new `fetch_vine_video_impl` + tauri command beside `fetch_content` `:24987` / `fetch_avatar` `:25031`; `generate_handler!` registration `:67716` region)
- Modify: `src-tauri/src/api/rpc.rs` (RPC verb `fetch_vine_video` + pin-test update — the e2e asserts the relay content leg through it)
- Modify: `src/App.svelte:2334-2339` (resolveVideoFn), `src/lib/components/VineFeed.svelte` / `VineCard.svelte` (thread `creatorAddress` into the resolve call — it is already in scope on the card)

**Interfaces:**
- Consumes: Task 6 content frames + `VINE_RELAY_ALPN`; Task 7's sidecar relay-set cache (read via a small `pub fn cached_relays_for(&self, creator: &str) -> Vec<VineRelayEntry>` accessor on the driver, exposed through `NodeState`); `ContentStore::put` for admission.
- Produces: `fetch_vine_video_impl(state, cid_hex: String, creator_address: String) -> Result<Vec<u8>, String>`; RPC args `FetchVineVideoArgs { cid: String, creatorAddress: String }` (camelCase, deny_unknown_fields).

**Behavior (spec §3 step 5 — no change to the happy path):**
1. Mesh first: send the same `FetchRequest` `fetch_content` sends (`event_loop.rs:345`); on `Ok`, return.
2. On `Err` AND `creator_address ∈ followed_set`: take the creator's cached relay set (skip self-entries), open one `VINE_RELAY_ALPN` session, send `Content(VineContentRequest{cid})`, read meta + chunks under the io deadline, `ContentId::verify_hash` the assembled bytes (reject on mismatch — the relay is untrusted), admit via `ContentStore::put` (mirrors what mesh-fetch admission does, so subsequent plays are local), return the bytes.
3. Any fallback failure returns the ORIGINAL mesh error string (the fallback is best-effort; surfacing "relay dial failed" for a CID that simply doesn't exist anywhere would mislead).

- [ ] **Step 1: Failing unit test** for the pure decision layer (factor `enum VideoFetchPlan { MeshOnly, MeshThenRelay(Vec<VineRelayEntry>) }` + `fn plan_video_fetch(followed: bool, cached_relays: Vec<VineRelayEntry>, self_ep: [u8;32]) -> VideoFetchPlan`):

```rust
#[test]
fn relay_fallback_only_for_followed_creators_and_never_self() {
    assert!(matches!(plan_video_fetch(false, vec![other()], SELF_EP), VideoFetchPlan::MeshOnly));
    assert!(matches!(plan_video_fetch(true, vec![], SELF_EP), VideoFetchPlan::MeshOnly));
    let VideoFetchPlan::MeshThenRelay(r) = plan_video_fetch(true, vec![self_entry(), other()], SELF_EP) else { panic!() };
    assert_eq!(r.len(), 1, "self entry filtered");
}
```

- [ ] **Step 2: Run → FAIL → implement impl + command + RPC verb (+ pin test) → PASS.**

- [ ] **Step 3: Frontend.** `resolveVideoFn` becomes `adapter.invoke('fetch_vine_video', { cid, creatorAddress })`; `VineCard`/`VineFeed` pass the card's `creatorAddress` through. Keep `fetch_content` untouched for every other caller.

- [ ] **Step 4: Gates + commit**

```bash
cargo test --lib fetch_vine && cargo test --lib rpc && cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add -A && git commit -m "vine video fetch: relay fallback behind the mesh path (ZEB-811)"
```

---

### Task 10: e2e — `s_vines_follow_only` + driver wrappers

**Files:**
- Modify: `e2e-harness/src/driver.rs` (new wrappers in the vine section `:406-475`)
- Modify: `e2e-harness/tests/e2e_two_node.rs` (new scenario after `s_vines_publish_feed_view_reshare` `:2345-2508`)
- Modify: `e2e-harness/README.md:31+` (scenario table row — the table currently omits even the existing vines scenario; add both)

**Interfaces:**
- Consumes: RPC verbs from Tasks 3 (`get_vine_settings`/`set_vine_settings`) and 9 (`fetch_vine_video`), existing `follow_vine_creator`/`list_vine_videos`/`mark_vine_viewed`/`reshare_vine` RPCs (`rpc.rs:1168-1234`), `two_minted_nodes` (`e2e_two_node.rs:91-118`), `poll_until` (`driver.rs:12-27`).
- Produces: the regression guard the ticket asked for.

**Scenario contract (spec §4):** two nodes, **no community, no friendship, no LAN scouting** (the harness already strips `HARMONY_ZENOH_ENABLE_LAN_SCOUTING`, `node.rs:110`). Alice publishes; her vines pkarr record publishes (real pkarr relays — `pkarr.q8.fyi` is ours; publish→resolve latency is seconds, and this test is a local guard, not a CI shard). Bob follows by address → wake → resolve → pull → descriptors + video arrive over the relay path. View + reshare legs assert on Bob's pulled copies only — v1 has no reverse channel, Alice never sees the reshare (spec §6).

- [ ] **Step 1: Driver wrappers** (signatures mirror the existing vine section):

```rust
pub async fn follow_vine_creator(node: &NodeHandle, address: &str) -> anyhow::Result<bool>      // rpc follow_vine_creator {address} → followed
pub async fn get_vine_settings(node: &NodeHandle) -> anyhow::Result<serde_json::Value>          // → {shareFollows, shareVinesPublicly}
pub async fn set_vine_settings(node: &NodeHandle, share_follows: bool, share_vines_publicly: bool) -> anyhow::Result<()>
pub async fn fetch_vine_video(node: &NodeHandle, cid: &str, creator: &str) -> anyhow::Result<Vec<u8>>  // rpc fetch_vine_video {cid, creatorAddress}
```

- [ ] **Step 2: The scenario** (`#[tokio::test(flavor = "multi_thread", worker_threads = 4)]`, gated by the file's `#![cfg(feature = "e2e")]`):

```rust
/// ZEB-811: follow-only cross-node delivery over the vine relay path.
/// Two nodes with NO relationship of any kind — the exact gap the ticket
/// documents (the old vines test needed a community join to pass without
/// LAN scouting). Alice's own device is her v1 relay; Bob discovers it via
/// the public pkarr `vines` slot and pulls.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s_vines_follow_only() {
    let (run, _ah, _bh, alice, bob) = two_minted_nodes("vines-follow").await;
    let alice_addr = node_vine_address(&alice).await;      // helper: read the node's own creator_address (see Step 3)
    // Alice: confirm the gate defaults ON, publish one vine.
    let settings = get_vine_settings(&alice).await.unwrap();
    assert_eq!(settings["shareVinesPublicly"], true, "public-by-default (spec product decision 4)");
    let title = format!("e2e-vine-follow-{}", &alice_addr[..8]);
    let (vine_id, video_cid) = publish_vine(&alice, &title, "alice").await.unwrap();
    // Bob: follow by address — no community, no friendship, no multicast.
    assert!(follow_vine_creator(&bob, &alice_addr).await.unwrap());
    // Descriptor leg: bob's feed gains the vine via the pull driver
    // (pkarr publish + resolve + pull; generous deadline, poll only).
    poll_until(Duration::from_secs(240), || async {
        Ok(list_vine_videos(&bob).await?.iter().any(|v| v["id"] == vine_id.as_str()))
    }).await.expect("descriptor must arrive over the relay path");
    // Video leg: relay content fetch (mesh GET cannot succeed — no shared mesh).
    let bytes = fetch_vine_video(&bob, &video_cid, &alice_addr).await.expect("video over vine-relay");
    assert!(!bytes.is_empty());
    // View + reshare legs on the PULLED copies (v1 has no reverse channel).
    assert!(mark_vine_viewed(&bob, &vine_id).await.unwrap());
    let reshare_of = reshare_vine(&bob, &vine_id, "bob").await.unwrap();
    assert_eq!(reshare_of, vine_id);
    assert!(list_vine_videos(&bob).await.unwrap().iter().any(|v| v["reshareOf"] == vine_id.as_str() && v["viewed"] == false));
    run.mark_success();
}
```

- [ ] **Step 3: `node_vine_address` helper.** Bob follows Alice's `creator_address` (= `vine_signing::signer_address` of her identity, embedded as `creatorAddress` in her published descriptors). Cleanest source available over RPC today: publish first, read `creatorAddress` from Alice's OWN `list_vine_videos` entry (self-ingest from Task 5 guarantees it is present). Implement the helper that way — no new RPC needed; note the ordering (publish before follow) in the scenario comment.

- [ ] **Step 4: Build + run** (the harness hard-fails on a stale binary):

```bash
cd src-tauri && cargo build --bin harmony-app
cd ../e2e-harness && cargo test --features e2e --test e2e_two_node s_vines_follow_only -- --nocapture
```
Expected: PASS. Also re-run the mesh-path sibling to prove no regression: `cargo test --features e2e --test e2e_two_node s_vines_publish_feed_view_reshare`.

- [ ] **Step 5: README scenario table** — add rows for `s_vines_publish_feed_view_reshare` (mesh path, community preamble) and `s_vines_follow_only` (relay path, no relationship).

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "e2e: follow-only vine delivery over the relay path (ZEB-811)"
```

---

### Task 11: Final gates + spec as-implemented notes

**Files:**
- Modify: `docs/superpowers/specs/2026-07-26-zeb-811-vine-relay-fanout-design.md` (append an "As-implemented notes" section)

- [ ] **Step 1: Full local gate sweep** — CI-parity commands, not the
      iterative `scripts/test-select` runner: this is the final gate, and
      the `harmony-pkarr` rev bump (Task 1) is a dependency-graph change
      that makes selective test-mapping unreliable anyway.

```bash
cd src-tauri && cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

- [ ] **Step 2: e2e sweep** (fresh binary first): `s_vines_follow_only` + `s_vines_publish_feed_view_reshare`.

- [ ] **Step 3: Spec as-implemented notes**, covering at minimum: (1) the inner-`sg` deviation (envelope signature per reachability precedent — Task 2 note); (2) `created_at` is seconds and ids are plain `String`s, so the cursor is `(u64, String)`; (3) wire-signature retention on disk as the serve-source prerequisite (spec §2 "hold is serving what is already there" implicitly assumed restart-surviving signatures); (4) self-ingest at publish; (5) no network-change republish trigger in v1 (endpoint-id + relay URL are churn-stable, unlike reachability's direct addresses); (6) the wake-on-follow seam living in the IPC impls, not the event-loop `FollowRequest` arm.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "ZEB-811: final gates + spec as-implemented notes"
```

PR opening, review convergence, and the core-pin bump to the merged-main SHA (after the Task 1 core PR merges) follow via the standard SDD finishing flow — the client PR body must name the core PR as a merge-order dependency.
