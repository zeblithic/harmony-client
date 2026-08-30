# ZEB-1030 D-FROST Catch-up Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Evidence-based catch-up for the dfrost committee log — a `dfrost/catchup` queryable serving the current epoch's signed `dk` quorum + recent `vb` beacons, with three verify-from-evidence adopt entry points, so member stragglers re-enter signing after missed refreshes and post-promotion joiners acquire committee state.

**Architecture:** Mirror the voting plane's layering: a sans-I/O protocol/selection module (`community_dfrost_catchup.rs`), adopt methods on `DfrostLog`, engine halves on `DfrostLogEngine` bridged by type-erased hooks (shape of `VotingRbsrHooks`), and a queryable + periodic requester task in `event_loop.rs`. NOT RBSR — the needed item set is tiny and targeted; the retained history is not replayable into state.

**Tech Stack:** Rust (src-tauri workspace), ciborium CBOR, frost-ristretto255, Zenoh queryable/GET, tokio.

**Spec:** `docs/superpowers/specs/2026-08-29-zeb1030-dfrost-catchup-design.md` — read it first; every trust decision below is argued there.

## Global Constraints

- Cargo commands run from `src-tauri/`; always `--locked --features test-fixtures`; clippy gate is `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`.
- All new top-level CBOR maps use 2-character keys (same-length-keys invariant, `community_dfrost_types.rs` module doc). `Vec<u8>` fields use `#[serde(with = "serde_bytes")]`.
- Caps: `MAX_DFROST_CATCHUP_FRAME_BYTES = 64 * 1024`, `MAX_DFROST_CATCHUP_ROUND_BYTES = 16 * 1024 * 1024`, `MAX_CATCHUP_BEACONS_PER_ROUND = 64`, `DFROST_CATCHUP_INTERVAL = Duration::from_secs(300)`.
- New AAD: `DFROST_CATCHUP_AAD = b"harmony-dfrost-catchup-v1"` (distinct from `DFROST_TOPIC_AAD = b"harmony-dfrost-v1"`).
- The responder never re-signs or re-mints — verbatim retained `SignedCommitteeEvent` bytes only. Every adopted event passes `verify_signed_committee_event` (engine layer) before any adopt method sees it.
- Adopt rejections must leave NO partial state (assert in every reject test).
- Never `.await` engine work inside a Zenoh reply arm — drain the whole reply stream into a `Vec` first (channel-log "pattern B", `event_loop.rs:13385-13439`).
- Test names end in `_zeb1030`. Inner loop: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(zeb1030)'`.
- Commit after every green task with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc` trailers.

---

### Task 1: Wire types + pure selection (`community_dfrost_catchup.rs`)

**Files:**
- Create: `src-tauri/src/community_dfrost_catchup.rs`
- Modify: `src-tauri/src/lib.rs` (one line: `pub mod community_dfrost_catchup;` next to the other `community_dfrost_*` mod decls)

**Interfaces (Produces — later tasks rely on these exact names):**

```rust
pub const CATCHUP_VERSION: u8 = 1;
pub const MAX_DFROST_CATCHUP_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_DFROST_CATCHUP_ROUND_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CATCHUP_BEACONS_PER_ROUND: usize = 64;

/// Envelope HLC of the newest `vb` event the requester holds. 2-char keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeaconWatermark {
    #[serde(rename = "wm")] pub wall_ms: u64,
    #[serde(rename = "lg")] pub logical: u32,
    #[serde(rename = "dv")] pub device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchupRequest {
    #[serde(rename = "vr")] pub version: u8,
    /// Requester's committee epoch (0 + active=false ⇒ no state).
    #[serde(rename = "ep")] pub epoch: u64,
    #[serde(rename = "ac")] pub active: bool,
    #[serde(rename = "bw", skip_serializing_if = "Option::is_none", default)]
    pub beacon_watermark: Option<BeaconWatermark>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchupStatus {
    #[serde(rename = "ep")] pub epoch: u64,
    #[serde(rename = "ac")] pub active: bool,
}

/// Externally-tagged enum — encodes as a 1-entry map {"st"|"dk"|"vb": ...}.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CatchupBody {
    #[serde(rename = "st")] Status(CatchupStatus),
    /// Verbatim ciborium-encoded `SignedCommitteeEvent` (kind `dk`).
    #[serde(rename = "dk")] DkEvidence(#[serde(with = "serde_bytes")] Vec<u8>),
    /// Verbatim ciborium-encoded `SignedCommitteeEvent` (kind `vb`).
    #[serde(rename = "vb")] Beacon(#[serde(with = "serde_bytes")] Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchupFrame {
    #[serde(rename = "vr")] pub version: u8,
    /// Per-round random responder id — frames group by this on the
    /// requester (Zenoh reply order/attribution is not load-bearing).
    #[serde(rename = "ri", with = "serde_bytes")] pub responder_id: [u8; 8],
    #[serde(rename = "bd")] pub body: CatchupBody,
}

pub fn encode_request(req: &CatchupRequest) -> Result<Vec<u8>, String>;
pub fn decode_request(bytes: &[u8]) -> Result<CatchupRequest, String>; // cap + version gate
pub fn encode_frame(frame: &CatchupFrame) -> Result<Vec<u8>, String>;  // cap gate post-encode
pub fn decode_frame(bytes: &[u8]) -> Result<CatchupFrame, String>;     // cap + version gate

/// Max envelope HLC among retained `vb` events (for the request).
pub fn beacon_watermark_of(log: &DfrostLog) -> Option<BeaconWatermark>;

pub struct CatchupSelection {
    pub status: CatchupStatus,
    /// Current epoch's `dk` events, one per distinct actor (newest per
    /// actor in synthesized-id order). May be sub-threshold — the
    /// requester decides adoptability.
    pub dk_events: Vec<SignedCommitteeEvent>,
    /// `vb` events with envelope HLC strictly above the watermark,
    /// OLDEST-first, capped at `max_beacons`.
    pub beacons: Vec<SignedCommitteeEvent>,
}

/// Pure responder selection. `None` ⇒ nothing to serve (inactive
/// responder, or requester already fully current) — transport answers
/// with silence.
pub fn select_catchup(
    log: &DfrostLog,
    req: &CatchupRequest,
    max_beacons: usize,
) -> Option<CatchupSelection>;

/// Group frames by responder_id, DISCARDING any group without exactly
/// one Status frame. Order: groups in first-seen order.
pub fn group_frames(frames: Vec<CatchupFrame>) -> Vec<(CatchupStatus, Vec<CatchupFrame>)>;
```

**Selection rules for `select_catchup` (implement exactly):**
1. `log.committee_state.active == false` → `None`.
2. Requester fully current (`req.active && req.epoch == current_epoch` && no beacon above watermark) → `None`.
3. `dk_events`: include only when `!req.active || req.epoch < current_epoch`. Scan `log.events()` (already synthesized-id/HLC order) for `kind == DfrostEventKind::DkgComplete`, decode `DkgCompletePayload`, keep those with `payload.epoch == current_epoch`, collapse into `BTreeMap<OwnerAddr, SignedCommitteeEvent>` keyed by `event.actor` (later insert wins = newest per actor), emit values.
4. `beacons`: scan `log.events()` for `kind == DfrostEventKind::VrfBeacon` with `(hlc.wall_ms, hlc.logical, device_id) > watermark` (no watermark ⇒ all), take the FIRST `max_beacons` (oldest-first so repeated rounds converge contiguously).

**Steps:**

- [ ] **Step 1: Write the failing tests** (in-module `#[cfg(test)]`). Test list, each with concrete assertions:
  - `catchup_request_and_frame_round_trip_zeb1030` — encode→decode equality for a request with and without watermark, and for one frame of each body variant; assert every top-level map key is 2 chars (mirror `signed_committee_event_envelope_has_8_two_char_keys` in `community_dfrost_types.rs:588`).
  - `decode_rejects_bad_version_and_oversize_zeb1030` — version 0 and version 2 rejected for both request and frame; a `vec![0u8; MAX_DFROST_CATCHUP_FRAME_BYTES + 1]` input rejected before decode; garbage bytes rejected.
  - `select_catchup_serves_dk_quorum_and_beacons_zeb1030` — build a log via the `committee_log_from_material` pattern (copy the fixture shape from `community_dfrost_log.rs:5592` — it is `#[cfg(test)]` in another module, so re-declare a local `test_active_log()` helper that sets `active=true, current_epoch=1, members/threshold=2/max_signers=3, joint_verifying_key, verifying_shares` from `dkg_2of3_material`-style dealer output, or construct with arbitrary 32-byte vk/share values since selection never verifies crypto). Insert via `log.apply(...)`? — NO: apply requires pending slots. Instead add a `#[cfg(any(test, feature = "test-fixtures"))] pub fn insert_event_for_test(&mut self, ev: SignedCommitteeEvent)` on `DfrostLog` that calls `insert_applied` (document: test-only seeding of retained history). Seed: 2 `dk` events at epoch 1 from actors A/B, 1 stale `dk` at epoch 0, one re-minted duplicate `dk` from A with higher HLC, 3 `vb` events at ascending HLCs. Assert: requester `{epoch:0, active:false, bw:None}` gets status `{epoch:1, active:true}`, exactly 2 dk events (newest-per-actor — the re-mint won for A), all 3 beacons oldest-first. Requester at `{epoch:1, active:true, bw:Some(<2nd beacon hlc>)}` gets `None`... only if no 3rd beacon above — use `bw` of the LAST beacon → `None`; `bw` of the first → dk empty, beacons = [2nd, 3rd].
  - `select_catchup_inactive_responder_serves_nothing_zeb1030` — fresh `DfrostLog::new()` → `None` for any request.
  - `select_catchup_caps_beacons_oldest_first_zeb1030` — seed `MAX_CATCHUP_BEACONS_PER_ROUND + 3`... use `max_beacons = 2` param directly: 5 beacons, watermark none → exactly the 2 OLDEST returned.
  - `group_frames_discards_statusless_groups_zeb1030` — frames from rid X (status + 1 dk) and rid Y (dk only) → one group (X); a rid with TWO status frames is also discarded.
- [ ] **Step 2: Run to verify failure:** `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(zeb1030)'` → compile errors (module absent), then test failures.
- [ ] **Step 3: Implement** the module per the interface block above. `encode_*` = `ciborium::ser::into_writer` + frame-size check; `decode_*` = length gate FIRST, then `ciborium::de::from_reader`, then version gate. Building `vb`/`dk` test events: copy the envelope shape from `community_dfrost_types.rs:589-602` (`tag: 'd', version: 1, committee_tier: 0`, fabricated 64-byte sigs — the log's policy verify checks envelope shape only, not signatures).
- [ ] **Step 4: Run tests to green**, same command.
- [ ] **Step 5: Commit** `feat(app): dfrost catch-up wire types + responder selection (ZEB-1030 task 1)`.

---

### Task 2: Adopt entry points on `DfrostLog`

**Files:**
- Modify: `src-tauri/src/community_dfrost_log.rs` (new methods after `install_restored_share`, ~line 953; tests in the existing `#[cfg(test)]` module reusing `dkg_2of3_material` (line 5528), `committee_log_from_material` (line 5592), `share_scalar_bytes` (line 5620))

**Interfaces (Produces):**

```rust
/// ZEB-1030: adopt a later epoch from ≥ threshold verbatim signed `dk`
/// events (vk-anchored — see spec §2). Caller (engine) has already
/// envelope-verified every event. Returns the adopted epoch.
pub fn adopt_refresh_quorum(&mut self, events: &[SignedCommitteeEvent]) -> Result<u64, String>;

/// ZEB-1030: first-time committee-state adoption for a node with NO
/// active state (fresh joiner/observer). Caller has envelope-verified
/// AND membership-verified (at each event's own HLC) every event.
pub fn adopt_initial_quorum(&mut self, events: &[SignedCommitteeEvent]) -> Result<u64, String>;

/// ZEB-1030: adopt self-certifying beacons. Per-event failures skip
/// that event (each is independent). Returns newly-indexed count.
pub fn adopt_beacons(&mut self, events: &[SignedCommitteeEvent]) -> usize;
```

**`adopt_refresh_quorum` checks, in order (each its own error string; the state writes happen only after ALL checks):**
1. `active` must be true; held vk must exist (`joint_verifying_key`).
2. Every event: `kind == DkgComplete`, decode `DkgCompletePayload` (error on any decode failure).
3. All payloads must agree exactly on: `ceremony_id`, `epoch`, `joint_verifying_key`, `members`, `threshold`, `max_signers`, and the full `verifying_shares` vec (compare against the first payload).
4. `payload.joint_verifying_key == held vk`; `payload.epoch > current_epoch`; `payload.members == self.committee_state.members` (refresh preserves the member set); `payload.threshold == self.committee_state.threshold`; `payload.max_signers == self.committee_state.max_signers`.
5. Distinct actors: collect `BTreeSet<OwnerAddr>` of `event.actor`; every actor ∈ held `members`; `set.len() >= threshold as usize`.
6. Shares map 1:1: build `BTreeMap<OwnerAddr, [u8;32]>` from `payload.verifying_shares` with the same member-set/duplicate/missing checks as `apply_dkg_complete` (`community_dfrost_log.rs:1302-1322`).
7. **Commit (mirror the promotion discipline at 1369-1470):** `current_epoch = payload.epoch`; `verifying_shares = <map from 6>`; `pending_dkg = None`; `pending_refresh = None`; `pending_repair = None`; `pending_sign.clear()` (sessions cannot complete across the epoch move); `local_dkg_secret = None`; `local_dkg_secret2 = None`; staged `pending_rotated` is installed iff it matches the adopted consensus for its identifier (mirroring live promotion), else dropped with a warn; then run the SAME `matches_consensus` check as lines 1447-1470 on `local_key_package` and drop kp + pub pkg on mismatch (after a refresh every share rotates, so a held stale kp always drops — un-suppressing `has_key_package` so ZEB-1027 auto-repair recovers the share). `members`/`threshold`/`max_signers`/`identifier_map` unchanged. Finally insert each event not already `self.log.contains(&dfrost_event_id(ev))` via `insert_applied` (fires `dirty`).

**`adopt_initial_quorum` checks:** (1) `!active` and `pending_dkg.is_none()`; (2)-(3) as above; (4′) `payload.epoch >= 1`; `payload.members` sorted-ascending + deduplicated (reject otherwise — identifier assignment depends on it); `payload.max_signers as usize == payload.members.len()`; `1 <= payload.threshold && payload.threshold <= payload.max_signers`; (5′) distinct actors ⊆ `payload.members`, count ≥ `payload.threshold`; (6) shares 1:1 over `payload.members`. Commit: full promotion (`active = true`, epoch, vk, shares, members, threshold, max_signers, `identifier_map = CommitteeState::build_identifier_map(&payload.members)`); do NOT touch `local_key_package` (a joiner has none); insert events as above.

**`adopt_beacons` per event:** skip unless `kind == VrfBeacon` and `active` with a held vk; decode `VrfBeaconPayload`; skip unless `payload.signature.len() == 64`, `derive_vrf_output(&sig[..32]) == payload.vrf_output`, and `crate::community_dfrost_crypto::verify_schnorr_signature(&held_vk, &payload.message_hash, &payload.signature).is_ok()` (exactly the `apply_vrf_beacon` crypto at 1624-1650, MINUS the `pending_sign` session requirement — that is the whole point); then `beacon_index.entry(payload.message_hash).or_insert(payload.vrf_output)` (count only fresh inserts) and insert the event into the log if absent. Never touches `pending_sign`.

**Steps:**

- [ ] **Step 1: Write failing tests** (all in `community_dfrost_log.rs` tests mod; build dk/vb events with a local helper `fn signed_dk(actor: OwnerAddr, wall: u64, dev: &str, payload: &DkgCompletePayload) -> SignedCommitteeEvent` that ciborium-encodes the payload and fabricates `sig: vec![0u8; 64]`):
  - `adopt_refresh_quorum_happy_path_zeb1030` — held state from `committee_log_from_material` (epoch 1) with alice's real kp installed; quorum of 2 dk events (actors alice+bob) at epoch 2, same held vk, NEW arbitrary shares map (`[0x41;32]`-style per member, internally consistent). Assert `Ok(2)`; `current_epoch == 2`; `verifying_shares` == new map; `local_key_package.is_none()` and `local_pub_key_package.is_none()` (stale share dropped); `pending_sign` empty (pre-seed one session before adopting and assert it cleared); both events retained (`event_count() == 2`); members/threshold unchanged.
  - `adopt_refresh_quorum_reject_matrix_zeb1030` — one loop over named cases, each starting from a fresh held log and asserting `Err` AND full no-partial-state (`current_epoch` still 1, shares unchanged, kp still present if seeded, `event_count() == 0`): sub-threshold (1 event); non-member actor (`OwnerAddr([0x99;16])`); vk mismatch (`[0xde;32]`); `epoch == 1` (not >); `epoch 0`; members list differing from held; disagreeing shares between the two payloads; duplicate share entry for one member; missing member in shares; wrong kind (a `vb` event in the slice); inactive log (fresh `DfrostLog::new()`).
  - `adopt_initial_quorum_happy_path_zeb1030` — fresh `DfrostLog::new()`; quorum of 2 dk events at epoch 1 (vk + shares from `dkg_2of3_material`'s real `pub_pkg`, members sorted). Assert `Ok(1)`, `active`, vk/shares/members/threshold/max_signers/identifier_map all set, `local_key_package.is_none()`, events retained. Then: a LIVE `dk` claiming a different vk applied via `log.apply` still fails (vk-immutability pin — construct with a `pending_dkg`-less log: assert `apply` returns `Err` and vk unchanged); a duplicate of an adopted event via `insert_event_for_test`... instead assert `log.apply(<same adopted event>)` returns `Ok(())` as a structural no-op (`event_count()` unchanged — the ZEB-753 dedup at line 775).
  - `adopt_initial_quorum_reject_matrix_zeb1030` — active log rejects; unsorted members reject; `threshold 0` and `threshold > max_signers` reject; `max_signers != members.len()` rejects; epoch 0 rejects; disagreeing payloads reject; no-partial-state asserted (still `!active`, `event_count() == 0`).
  - `adopt_beacons_self_certifying_zeb1030` — held active log (real vk from `dkg_2of3_material`); produce a REAL beacon: run the FROST sign flow the way `threshold_sign_two_engine_vrf_beacon_verifies` (tests/community_voting/community_dfrost_integration.rs:471) aggregates one, OR simpler in-unit: `frost_ristretto255` two-party sign using `key_packages` from `dkg_2of3_material` — commit with `frost_ristretto255::round1::commit`, sign `round2::sign`, `aggregate`, then build the `VrfBeaconPayload { ceremony_id: [0xcc;32], message_hash: <32B msg>, signature: sig.serialize() (64B), vrf_output: derive_vrf_output(sig[..32]) }`. Assert: adopt on a log WITHOUT any `pending_sign` session returns 1, `find_vrf_beacon_output_by_seed`-style lookup via `beacon_index[message_hash]` yields the output, event retained; re-adopt returns 0 (idempotent); a tampered signature (flip one byte) adopts 0; a wrong `vrf_output` adopts 0; batch of [good, bad] adopts exactly the good one.
  - `adopt_epoch_heals_stale_beacon_lookup_zeb1030` — the spec §5.3 mis-key self-heal pin: held log at epoch 1; insert into `beacon_index` a beacon keyed by `derive_vrf_seed(&seed, 2)` (as the straggler's live path would have — the TRUE hash from the sender's epoch-2 traffic); assert `find_vrf_beacon_output_by_seed(&seed, 1)` (stale-epoch lookup) is `None`; run `adopt_refresh_quorum` to epoch 2; assert `find_vrf_beacon_output_by_seed(&seed, 2)` — which the oracle now derives from the adopted `current_epoch` — returns the output.
- [ ] **Step 2: Verify failure** (`-E 'test(zeb1030)'`).
- [ ] **Step 3: Implement** the three methods + the `insert_event_for_test` fixture helper from Task 1.
- [ ] **Step 4: Green**, then run the surrounding battery: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(dfrost)'` — the existing dfrost suite must stay green untouched.
- [ ] **Step 5: Commit** `feat(app): DfrostLog evidence-based adopt entry points (ZEB-1030 task 2)`.

---

### Task 3: Engine halves + catch-up hint

**Files:**
- Modify: `src-tauri/src/community_dfrost_log_engine.rs` — new fields on `DfrostLogEngine` (retain `identity_resolver: Arc<dyn IdentityResolver + Send + Sync>` and the already-constructed `orchestrator: Arc<OrchestratorHandle>` as struct fields — currently both move into the receive task; clone before the move in `new`), new fields on `OrchestratorHandle` (struct at line 592): `pub(crate) catchup_hint: Arc<tokio::sync::Notify>` + `pub(crate) catchup_hint_last: std::sync::Mutex<Option<std::time::Instant>>`; three public engine methods; hint firing in `process_inbound`'s apply-failure arm (line 1340-1358).

**Interfaces (Produces):**

```rust
/// Outcome of one requester round, for logging + cadence decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatchupOutcome {
    AdoptedRefresh { epoch: u64, beacons: usize },
    AdoptedInitial { epoch: u64, beacons: usize },
    BeaconsOnly(usize),
    /// Local state already current and no usable frames beyond status.
    UpToDate,
    /// Joiner path: responder groups disagree on the joint vk.
    Disagreement,
    /// No group survived validation / nothing adoptable.
    NothingUsable,
}

impl<R: tauri::Runtime> DfrostLogEngine<R> {
    pub async fn catchup_build_request(&self) -> CatchupRequest;      // snapshot epoch/active/watermark under one log lock
    pub async fn catchup_respond(&self, req: CatchupRequest) -> Option<Vec<CatchupFrame>>;
    pub async fn catchup_ingest(&self, frames: Vec<CatchupFrame>) -> CatchupOutcome;
    pub fn catchup_hint(&self) -> Arc<tokio::sync::Notify>;           // clone of orchestrator.catchup_hint
}
```

**`catchup_respond`:** one fresh `responder_id: [u8; 8]` per call via `rand::random()` (harmony-app already depends on `rand` — verify with `grep -rn "rand::" src-tauri/src/lib.rs | head -3`; if the dep is `rand 0.8` use `rand::random::<[u8; 8]>()`); lock the log once, run `select_catchup(&log, &req, MAX_CATCHUP_BEACONS_PER_ROUND)`; map the selection into frames: one `Status`, then one `DkEvidence(encoded event)` per dk event, one `Beacon(...)` per beacon — encoding each event with `ciborium::ser::into_writer`; skip (with a warn) any single event whose encoding exceeds `MAX_DFROST_CATCHUP_FRAME_BYTES`.

**`catchup_ingest` (implement exactly this flow):**
1. `group_frames(frames)`; empty → `NothingUsable`.
2. Decode every `DkEvidence`/`Beacon` body into `SignedCommitteeEvent` (drop undecodable with a warn); **envelope-verify each** via `verify_signed_committee_event(&event, self.identity_resolver.as_ref()).await` (drop failures with a warn) — trust invariant #2, no adopt method ever sees an unverified event. Additionally require `DkEvidence` events to have `kind == DkgComplete` and `Beacon` events `kind == VrfBeacon` (drop otherwise).
3. Snapshot `(local_active, local_epoch)` under a short log lock.
4. **Straggler path** (`local_active`): iterate groups with `status.epoch > local_epoch` in DESCENDING `status.epoch` order; for each, lock the log and try `adopt_refresh_quorum(&group_dk_events)`; on the first `Ok(epoch)`: record every adopted dk in the replay tracker (`tracker.lock().await.record(&ev)` — same as a live apply, step 5 of `process_inbound`), then `adopt_beacons(&group_beacons)` (recording those too), return `AdoptedRefresh { epoch, beacons }`. On `Err`: warn with the reason, try the next group. If no group adopts: fall through to beacons — `adopt_beacons` over ALL groups' beacons; return `BeaconsOnly(n)` if `n > 0` else `UpToDate` (when some group's status matched our epoch) else `NothingUsable`.
5. **Joiner path** (`!local_active`): collect the joint vk from each group that carries ≥1 dk event (decode the first dk's `DkgCompletePayload`); if ≥2 distinct vks → `tracing::warn!("dfrost catchup: responders disagree on joint vk — adopting nothing")`, return `Disagreement`. If zero groups have dk evidence → `NothingUsable`. Otherwise take the dk-bearing group with the highest `status.epoch`; **membership gate** (spec §5.3, at each event's OWN envelope HLC — `dk` has no payload mint stamp, so the at-event-HLC rule applies; Task 5 amends the spec wording): when `self.orchestrator.membership_resolver` is `Some(resolver)`, for each dk event resolve `resolver.snapshot_at(community_id, &event.hlc).await` and require `event.actor` AND every `payload.members` entry to be present in `snapshot.members` (mirror the `di` gate at lines 877-908); any failure drops the whole group with a warn → `NothingUsable`. `None` resolver ⇒ skip (test engines), exactly like the `di` gate. Then `adopt_initial_quorum`, record tracker, `adopt_beacons` from the same group → `AdoptedInitial`.

**Hint firing** — in `process_inbound` right after the `apply failed` warn (before `maybe_heal_straggler`), add:

```rust
// ZEB-1030: an apply failure that smells like "the committee moved
// without us" (or "a committee exists we never saw") pulls the next
// catch-up attempt forward. Rate-limited; never fires for di/dk
// invariant rejections (those are live-ceremony races, not lag).
let hint_worthy = matches!(apply_err, ApplyError::UnknownCeremony)
    || (matches!(apply_err, ApplyError::InvariantViolation)
        && matches!(
            event.kind,
            DfrostEventKind::ThresholdSign
                | DfrostEventKind::VrfBeacon
                | DfrostEventKind::ProactiveRefresh
                | DfrostEventKind::RepairShare
        ));
if hint_worthy {
    let mut last = orchestrator.catchup_hint_last.lock().expect("hint clock");
    let due = last
        .map(|t| t.elapsed() >= orchestrator.config.rebroadcast_interval)
        .unwrap_or(true);
    if due {
        *last = Some(std::time::Instant::now());
        orchestrator.catchup_hint.notify_one();
    }
}
```

(The apply-failure arm currently binds the error as `e` inside `if let Err(e)` — capture it as `apply_err` for the match.)

**Steps:**

- [ ] **Step 1: Write failing engine tests** (in `community_dfrost_log_engine.rs` tests mod; construct engines exactly the way the existing `persist_funnel_embeds_share_and_self_cleans_zeb1029` test does — copy its params shape: channels via `mpsc::channel`, `app_handle: None`, resolver fixture, `driver: None`, `membership_resolver: None`, `persist: None`):
  - `catchup_respond_then_ingest_straggler_adopts_zeb1030` — engine A holds a dealer-consistent active log at epoch 2 with retained dk events (seed via the Task 1 `insert_event_for_test` helper with events signed by the test-identity fixture the engine's resolver knows — reuse the engine tests' existing identity fixture; envelope verification is LIVE in ingest, so sigs must be real: mint them with the same signing helper the existing engine tests use to build inbound events). Engine B holds the same committee at epoch 1. Drive: `let req = b.catchup_build_request().await; let frames = a.catchup_respond(req).await.unwrap(); let out = b.catchup_ingest(frames).await;` assert `AdoptedRefresh { epoch: 2, .. }`, B's log epoch 2, B's shares == A's, B's kp dropped if one was seeded.
  - `catchup_ingest_joiner_adopts_and_disagreement_aborts_zeb1030` — fresh engine C ingests A's frames → `AdoptedInitial`, active, vk == A's. Then fresh engine D ingests a frame set containing TWO responder groups (call `a.catchup_respond` twice — two rids — and hand-tamper the second group's dk payload vk... simpler: build the second group's frames manually with a different vk and valid signatures) → `Disagreement`, D still `!active`.
  - `catchup_ingest_drops_unverified_events_zeb1030` — flip a byte in one dk event's sig inside the frames; ingest must not adopt from a now-sub-threshold group (`NothingUsable` for a 2-dk threshold-2 group with one corrupted).
  - `catchup_build_request_reports_watermark_zeb1030` — engine with two retained vb events: request's `beacon_watermark` equals the newest one's envelope HLC; fresh engine: `None` + `epoch 0` + `active false`.
  - `catchup_hint_fires_rate_limited_zeb1030` — call the extracted hint-decision helper... implement the hint block as a free fn `pub(crate) fn maybe_fire_catchup_hint(orchestrator: &OrchestratorHandle, kind: DfrostEventKind, err: &ApplyError)` so the test drives it directly: `UnknownCeremony` fires (notified within a `tokio::time::timeout` on `hint.notified()`); a second call within `rebroadcast_interval` does NOT re-arm (`catchup_hint_last` unchanged); `InvariantViolation` + `CeremonyInit` does not fire; `InvariantViolation` + `ThresholdSign` fires after the interval.
- [ ] **Step 2: Verify failure.**
- [ ] **Step 3: Implement** (fields, methods, `maybe_fire_catchup_hint` called from `process_inbound`).
- [ ] **Step 4: Green** on `-E 'test(zeb1030)'`, then `-E 'test(dfrost)'` for the battery.
- [ ] **Step 5: Commit** `feat(app): dfrost engine catch-up halves + epoch-lag hint (ZEB-1030 task 3)`.

---

### Task 4: Transport — AAD, seal/open, queryable, requester task, wiring

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` — add `pub const DFROST_CATCHUP_AAD: &[u8] = b"harmony-dfrost-catchup-v1";` beside `VOTING_RBSR_AAD` (line ~423) + a two-direction domain-separation test mirroring `voting_rbsr_aad_is_domain_separated` (line 15912) against `DFROST_TOPIC_AAD`.
- Modify: `src-tauri/src/event_loop.rs` — `DfrostCatchupHooks` struct beside `VotingRbsrHooks` (line 189); `catchup_hooks: Option<DfrostCatchupHooks>` + docs on `DfrostLogAdapterRequest` (line 275, replacing/annotating the "no backfill" design note at 267-274); seal/open helpers + responder queryable + requester task inside `spawn_dfrost_log_zenoh_adapter` (line 11936), topic `harmony/community/{id_hex}/dfrost/catchup`, mirroring the voting responder (11504-11578) and requester (11580-11713) but with channel-log pattern-B full-drain (13385-13439) and the hint select-arm; `DFROST_CATCHUP_INTERVAL: Duration = Duration::from_secs(300)`.
- Modify: `src-tauri/src/lib.rs` — build the hooks as type-erased closures over the engine Arc where the dfrost adapter request is assembled in `ensure_dfrost_engine_for` (the `DfrostLogAdapterRequest` construction — grep `DfrostLogAdapterRequest {` in lib.rs), mirroring the voting hook construction at lib.rs:61315-61328.

**Interfaces (Produces):**

```rust
// event_loop.rs
#[derive(Clone)]
pub struct DfrostCatchupHooks {
    pub build_request: std::sync::Arc<dyn Fn() -> Pin<Box<dyn Future<Output = crate::community_dfrost_catchup::CatchupRequest> + Send>> + Send + Sync>,
    pub respond: std::sync::Arc<dyn Fn(crate::community_dfrost_catchup::CatchupRequest) -> Pin<Box<dyn Future<Output = Option<Vec<crate::community_dfrost_catchup::CatchupFrame>>> + Send>> + Send + Sync>,
    pub ingest: std::sync::Arc<dyn Fn(Vec<crate::community_dfrost_catchup::CatchupFrame>) -> Pin<Box<dyn Future<Output = crate::community_dfrost_log_engine::CatchupOutcome> + Send>> + Send + Sync>,
    /// Engine-owned; the requester task selects on it to pull the next
    /// attempt forward (epoch-lag hint).
    pub hint: std::sync::Arc<tokio::sync::Notify>,
}
```

**Transport rules (each mirrors a cited proven site):**
- Seal/open under the community epoch key + `DFROST_CATCHUP_AAD`, cap-checked before alloc on open and after encode on seal — mirror `voting_rbsr_seal`/`voting_rbsr_open` (11026/11050). ONE epoch snapshot seals the entire reply frame set (ZEB-920 rule — mirror `voting_rbsr_seal_reply_and_bodies`, 11087-11118). Opening: strict current-epoch, matching the dfrost inbound cut (12145-12155).
- Responder: queryable on the catchup topic; payload-less GET, oversize, unopenable, undecodable, or `respond → None` ⇒ reply NOTHING (silence — mirror 11548-11559).
- Requester loop per community (spawned beside the existing subscriber task, same `closing` lifecycle, joined at teardown like the voting requester at 11905): immediate first attempt, then wait `DFROST_CATCHUP_INTERVAL` in 1 s `closing`-poll slices with a `select!` arm on `hooks.hint.notified()` that breaks the wait early. Each attempt: `build_request` → seal → `session.get` with `ConsolidationMode::None` + `Locality::Remote` + 10 s timeout (mirror 11196-11202) → **pattern-B drain**: collect raw payloads into `Vec<Vec<u8>>` with `MAX_DFROST_CATCHUP_ROUND_BYTES` + per-frame cap enforcement, ONLY THEN open/decode each frame and call `ingest` (never await engine work in the reply arm — the channel-log discipline at 13385-13439, spec invariant #7). Log the outcome at `info` for adopts, `debug` otherwise.
- No requester/responder when `catchup_hooks` is `None` (ingest-only test adapters keep working).

**Steps:**

- [ ] **Step 1: Failing tests.**
  - `dfrost_catchup_aad_is_domain_separated_zeb1030` (community_state_sync.rs) — seal under `DFROST_CATCHUP_AAD`, assert open under `DFROST_TOPIC_AAD` fails and vice versa (copy the voting test's shape at 15912).
  - In event_loop.rs tests (mirror `zeb932_voting_rbsr_cadence_tests`, 15484): `catchup_wait_slices_break_on_hint_zeb1030` — extract the wait logic as `pub(crate) async fn catchup_wait(interval_secs: u64, closing: &AtomicBool, hint: &Notify) -> WaitEnd { Interval | Hint | Closing }` and pin: hint notify ends the wait early; closing set ends it; otherwise it runs the full interval (test with 2 s + tokio time pause if the harness allows, else 1 s real).
  - Seal/open round-trip + caps: `dfrost_catchup_seal_open_round_trip_and_caps_zeb1030` — a frame seals and opens under the same epoch key; an over-cap payload is rejected before decode.
- [ ] **Step 2: Verify failure.**
- [ ] **Step 3: Implement** (hooks struct, adapter field, seal/open, responder, requester, lib.rs hook construction + wiring). The lib.rs closures capture the `Arc<DfrostLogEngine>` exactly the way the voting rbsr hooks capture theirs (61315-61328); `hint` comes from `engine.catchup_hint()`.
- [ ] **Step 4: Green** on `-E 'test(zeb1030)'`; then compile-check the workspace: `cargo check --locked --all-targets --features test-fixtures`.
- [ ] **Step 5: Commit** `feat(app): dfrost catch-up queryable + requester transport (ZEB-1030 task 4)`.

---

### Task 5: Integration tests + spec amendment

**Files:**
- Modify: `src-tauri/tests/community_voting/community_dfrost_integration.rs` (reuse `dkg_2of2_setup` (line 211), `build_rf_event`/`build_dk_event`/`build_vb_event`/`build_ts_event`, and the refresh flow shape from `refresh_two_engine_preserves_joint_vk` (line 768))
- Modify: `docs/superpowers/specs/2026-08-29-zeb1030-dfrost-catchup-design.md` — §5.3 `adopt_initial_quorum`: change "membership snapshot at the payload mint HLC" to "membership snapshot at each event's own envelope HLC (`dk` carries no payload mint stamp; served events are verbatim originals, so their envelope HLCs are the promotion-time ones)". §5.4 joiner path gains the same parenthetical.

**Two tests (engine-level wire crossing — frames encode → decode across engines, the arbiter is bytes crossing the catch-up protocol boundary; Zenoh-session-level coverage stays out, matching the voting plane's test posture):**

- [ ] **Step 1: Write `straggler_catches_up_after_missed_refresh_zeb1030` (failing).** Flow: (a) `dkg_2of2_setup()` → alice-log + bob-log both active epoch 1, both holding real key packages (whatever the fixture provides — read it first); (b) drive a full refresh **on alice's log only** (both members' `rf` rn=1/rn=2 + both `dk` events at epoch 2, exactly the `refresh_two_engine_preserves_joint_vk` event sequence but applied only to alice) — bob is the partitioned straggler at epoch 1; (c) build engines over both logs (the engine-construction pattern already used by this file's two-engine tests — if this file drives logs directly without engines, do the same here and call the ENGINE halves via a pair of engines constructed the way Task 3's tests do; whichever this file supports with least new scaffolding, keeping the frames-cross-a-byte-boundary property: `encode_frame`/`decode_frame` every frame between respond and ingest); (d) bob requests catch-up from alice → assert `AdoptedRefresh { epoch: 2, .. }`; (e) assert bob: epoch 2, verifying_shares == alice's, `local_key_package.is_none()` (his epoch-1 share was dropped as provably stale), pending_sign empty; (f) **repair admissibility at the new epoch**: assert bob's log accepts a repair request now — `check_repair_request_admissible`-equivalent via applying a valid `rp` rn=1 built at epoch 2 with alice as helper (mirror how the ZEB-1027 repair tests in this file build one) succeeds where at the stale epoch it would have `InvariantViolation`'d; (g) beacon: mint a real epoch-2 beacon on alice (the `threshold_sign_two_engine_vrf_beacon_verifies` flow, line 471), re-run catch-up, assert `BeaconsOnly(1)` and bob's `beacon_index` contains it.
- [ ] **Step 2: Write `fresh_joiner_adopts_committee_state_zeb1030` (failing).** Fresh log/engine C (never saw the DKG): catch-up from alice → `AdoptedInitial { epoch, .. }`; assert active, vk == alice's, identifier_map built; C can now VERIFY an alice-minted beacon via `adopt_beacons`/lookup; and a hostile second responder group with a different vk (hand-built frames, valid test-identity signatures) makes a fresh D return `Disagreement` and stay inactive.
- [ ] **Step 3: Run both to failure, implement any missing glue, run to green:** `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_dfrost_integration)'` (integration target — no `--lib`).
- [ ] **Step 4: Amend the spec** (two wording changes above) — the plan/spec must not disagree with the shipped membership-HLC rule.
- [ ] **Step 5: Commit** `test(app): dfrost catch-up integration — straggler + joiner wire-crossing (ZEB-1030 task 5)`.

---

### Task 6: Gates, ticket, PR

- [ ] **Step 1:** `cd src-tauri && cargo fmt --all`
- [ ] **Step 2:** Full dfrost battery: `cargo nextest run --locked --all-targets -p harmony-app --features test-fixtures -E 'test(dfrost)'` → all green.
- [ ] **Step 3:** `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` → clean.
- [ ] **Step 4:** From repo root: `scripts/test-select --context task` → green (paste the `round=… bucket=…` line into the task report).
- [ ] **Step 5:** Commit any gate fixups, push branch, open the PR (body: problem → spec pointer → the three adopt paths + trust anchors → test inventory → gates), fire `@coderabbitai review` ONCE per the standing rule, start the full `--workspace --all-targets` sweep in the background.

## Self-review notes (already applied)

- Spec §5.3 said "membership at the payload mint HLC" — `dk` has no payload mint stamp; Task 5 amends the spec to the at-event-HLC rule Task 3 implements.
- `pending_sign.clear()` on refresh-adopt is deliberate and spec'd (§5.3); `adopt_beacons` never touches `pending_sign` (adoption has no session).
- `select_catchup` may serve a sub-threshold dk set (spec §5.2) — the requester's `adopt_refresh_quorum` threshold check is the gate; Task 3's corrupted-sig test covers the sub-threshold-after-drop path.
- Type-name consistency check: `CatchupRequest`/`CatchupFrame`/`CatchupBody`/`CatchupStatus`/`BeaconWatermark`/`CatchupSelection`/`CatchupOutcome`/`DfrostCatchupHooks` are used with identical spelling across Tasks 1/3/4; adopt methods `adopt_refresh_quorum`/`adopt_initial_quorum`/`adopt_beacons` across Tasks 2/3/5.
