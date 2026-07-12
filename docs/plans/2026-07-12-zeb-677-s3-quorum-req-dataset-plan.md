# ZEB-677 S3: `owner-quorum-req-v1` dataset + quorum revocation ceremony — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A master-less device can request a sibling co-sign to revoke a device; the request replicates over a new `FleetSyncEngine` dataset, a sibling approves with one click, and the assembled quorum `RevocationCert` lands fleet-wide through the existing trust pipeline.

**Architecture:** New fleet dataset `owner-quorum-req-v1` (donor: `owner_trust_sync.rs`) carries pending co-sign requests as a CRDT (grow-only signature union, LWW arm cells). Three IPCs drive the ceremony (`request_quorum_revocation` / `cosign_quorum_request` / `decline_quorum_request`); an applied-task sweep on the initiator assembles the cert via S1's `RevocationCert::assemble_quorum` and applies it through `mutate_trust_state` → the crate's validating `add_revocation` quorum arm. DevicesPanel gains the co-sign banner and quorum-visible Remove; `OwnerStateView` gains `selfIsMaster` / `quorumRequests` / `quorumArmedUntilMs`.

**Tech stack:** Rust (tokio, ciborium, ed25519-dalek, harmony-owner rev `1ecb4160`), Svelte 5, vitest.

**Spec:** `docs/specs/2026-07-12-zeb-677-quorum-wiring-design.md` §3/§4/§7(interim)/§8/§9/§10. Branch `zeb-677-s3-quorum-req-dataset` off main `9b9847fc`.

## Global Constraints

- Gates per task: `cd src-tauri && cargo fmt --all` then `scripts/test-select --context task` (repo root; paste the printed `round=… bucket=…` line into the task report). This slice does NOT touch `Cargo.toml`/`Cargo.lock`, so test-select stays usable. Final sweep before PR: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo fmt --all -- --check`, `npx tsc --noEmit`, `npx vitest run`.
- Tauri IPC: Rust params `snake_case`, JS callers `camelCase` (auto-converted at the boundary). Frontend error extraction: `e instanceof Error ? e.message : String(e)`.
- ZEB-428: keychain access ONLY through `KeychainFactory` seams (`prod_keychain` in prod, `|| None` in tests). This slice never writes the keychain — the quorum doc persists to its own file.
- K=2 fixed. Depth-1: quorum signers must hold Master-issued enrollment certs. No social recovery. Honesty-rule copy per spec §8.
- **S1 payload constraint (load-bearing):** `RevocationCert::quorum_signing_payload_bytes(owner_id, target, issued_at, reason, signers)` binds the SIGNER SET into the signed payload. Since any eligible sibling may co-sign, every signature is over the payload for the sorted pair `[initiator, cosigner]`. The initiator pre-signs one payload per eligible cosigner at request creation (`initiator_sigs` map) — this also authenticates the request to each candidate. The initiator's own assembly-time part is minted fresh; **spec §3 deviation** (its "signatures (its own included)" single-payload shape) — document in PR body.
- No epoch bump in this slice (S5). A quorum revoke leaves `fleetEpochStale` true — the existing banner is the honest interim surface (spec §7).
- Quorum IPCs require the node running (resident docs); otherwise `nodeNotRunning:` error. No FileOnly fallback — a co-sign request without replication is meaningless.
- Events: new `owner-quorum-updated` (doc changed); existing `owner-devices-updated` fires when the assembled revocation lands.
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`.

---

### Task 1: `owner_quorum_sync.rs` — doc types, merge, prune, pair payload, persistence

**Files:**
- Create: `src-tauri/src/owner_quorum_sync.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod owner_quorum_sync;` in the module list near `pub mod owner_trust_sync;`)

**Interfaces produced:**
- `QuorumReqDoc { requests: BTreeMap<String, QuorumRequest>, enroll_arms: BTreeMap<String, EnrollArm> }`
- `QuorumRequest { kind, initiator_hex, issued_at, created_at: Hlc, expires_at_ms, initiator_sigs: BTreeMap<String,String>, signatures: BTreeMap<String,QuorumRequestSigs>, declined_by: BTreeSet<String> }`
- `QuorumRequestKind::Revocation { target_hex, reason }` (serde variant `"r"`)
- `QuorumRequestSigs { primary_sig_hex, epoch_doc_sig_hex: Option<String> }`
- `EnrollArm { armed_until_ms, set_at: Hlc }`
- `merge_quorum_remote_into_local(&mut QuorumReqDoc, QuorumReqDoc) -> MergeOutcome`, `quorum_merger() -> Merger<QuorumReqDoc>`
- `prune_settled_requests(&mut QuorumReqDoc, trust: &OwnerState, now_ms: u64) -> bool`
- `revocation_pair_payload(owner_id, target, issued_at, &RevocationReason, a: [u8;16], b: [u8;16]) -> Result<Vec<u8>, String>` (sorts the pair)
- `QuorumPersist { doc_path, replay_path }` impl `FleetPersist<QuorumReqDoc>`; `load_quorum_doc_or_recover`, `load_quorum_replay_or_recover` (schema byte + quarantine, donor recipe)
- Constants: `OWNER_QUORUM_DATASET = "owner-quorum-req-v1"`, `OWNER_QUORUM_LOOKUP_TAG`, `OWNER_QUORUM_DATASET_MAX_BYTES = 256*1024`, `OWNER_QUORUM_DOC_FILENAME = "owner_quorum_req.cbor"`, `OWNER_QUORUM_REPLAY_FILENAME = "owner_quorum_replay.cbor"`, `QUORUM_REVOCATION_TTL_MS = 86_400_000`, `MAX_QUORUM_REQUESTS = 32`, `MAX_QUORUM_SIG_ENTRIES = 16`

**Steps:**

- [ ] **1.1 Types.** Serde codes 1-char per struct: doc `r`/`e`; request `k`/`i`/`u`(issued_at)/`c`/`x`/`p`(initiator_sigs)/`s`/`d`; sigs `p`/`e`; arm `u`/`a`. All maps/sets `#[serde(default, skip_serializing_if = "…::is_empty")]`; `epoch_doc_sig_hex` `#[serde(default, skip_serializing_if = "Option::is_none")]` (S5 slot, always `None` this slice). Derive `Debug, Clone, Serialize, Deserialize, PartialEq`; `Default` on the doc. Manual `CanonicalPayloadSealed`/`CanonicalPayload` impls for `QuorumReqDoc` (donor: `owner_trust_sync.rs:47-48`).
- [ ] **1.2 Merge.** Requests: unknown id → structural caps check (`initiator_sigs`/`signatures`/`declined_by` each ≤ `MAX_QUORUM_SIG_ENTRIES`, else drop + `warn`), insert only while `local.requests.len() < MAX_QUORUM_REQUESTS` (else drop + `warn`); known id → identity fields (`kind`, `initiator_hex`, `issued_at`, `expires_at_ms`) must match or drop + `warn` (tamper signal; id is 16-byte random), then grow-only union of `initiator_sigs`, `signatures`, `declined_by` — existing entries always win (never overwrite). `enroll_arms`: per-cell LWW on `set_at` (strictly-newer wins; `Hlc` already has lexicographic `Ord`). `changed` = canonical-encode compare (donor pattern, docs are tiny). NO pruning inside merge.
- [ ] **1.3 Prune.** `prune_settled_requests` removes a request when: `now_ms > expires_at_ms`, OR (kind Revocation and `trust.is_revoked(target)` — completion converges on every device without an explicit tombstone), OR (`!declined_by.is_empty()` AND `now_ms > expires_at_ms`) — i.e. declined requests are retained (dead, UI-hidden) until expiry so the decline tombstone cannot be resurrected by a union re-merge from a device that never saw it. Also prune expired `enroll_arms` cells (`now_ms > armed_until_ms`). Returns `true` if anything was removed. Bad `target_hex` (non-hex/wrong length) → treat as settled and prune (defensive; cannot ever complete).
- [ ] **1.4 Pair payload.** Decode/sort pair, call `RevocationCert::quorum_signing_payload_bytes(owner_id, target, issued_at, reason, &[lo, hi])`, map err to String.
- [ ] **1.5 Persistence.** Schema byte `1` + canonical CBOR for BOTH doc and replay files; `save_atomically`; load-or-recover with quarantine rename (donor: `owner_trust_sync.rs:136-192`, `fleet_key_epoch` doc recipe). `QuorumPersist::persist` writes doc then replay (no cross-file lock needed — this doc has no second writer path like owner_state.cbor).
- [ ] **1.6 Unit tests** (in-module): serde round-trip incl. empty-maps omission and unknown-request tolerance; merge unions sigs from disjoint remotes; merge never overwrites an existing sig entry; merge drops identity-mutated duplicate; merge drops over-cap inserts; arm LWW newer-wins both directions; prune expiry / revoked-target / declined-retained-until-expiry / bad-target; persist round-trip + corrupt-file quarantine (donor test shapes).
- [ ] **1.7 Gate + commit.** `cargo fmt --all`; `scripts/test-select --context task`; commit `ZEB-677 S3 Task 1: owner-quorum-req-v1 doc, merge, prune, persistence`.

### Task 2: planner + the three IPCs (`owner_quorum_commands.rs`)

**Files:**
- Create: `src-tauri/src/owner_quorum_commands.rs`
- Modify: `src-tauri/src/lib.rs` (module decl; `generate_handler` list near `owner_commands::revoke_device` at ~:57026: add the three commands)
- Modify: `src-tauri/src/api/rpc.rs` (args structs near `RevokeDeviceArgs` :376; `rpc!` entries in the "Device management" block :956-975)

**Interfaces produced:**
- `plan_quorum_revocation_request(trust: &OwnerState, device_sk: &SigningKey, master_seed_present: bool, device_vk_hex: &str, reason_str: &str, now_secs: u64, now_ms: u64, request_id: [u8;16]) -> Result<(String, QuorumRequest), String>` (pure; returns id-hex + request)
- `request_quorum_revocation_impl(state, sink, device_vk_hex, reason) -> Result<String, String>`; `cosign_quorum_request_impl(state, sink, request_id) -> Result<(), String>`; `decline_quorum_request_impl(state, sink, request_id) -> Result<(), String>` + `#[tauri::command]` wrappers (each taking `tauri::AppHandle` + `State<Mutex<NodeState>>`, donor `revoke_device` :869)
- Helper `is_master_issued(cert) -> bool` = `matches!(cert.issuer, EnrollmentIssuer::Master { .. })`
- Helper `eligible_cosigners(trust, now_secs, self_id, target) -> Vec<[u8;16]>` = `trust.active_devices(now, DEFAULT_ACTIVE_WINDOW_SECS)` ∩ master-certed − `{self, target}`

**Steps:**

- [ ] **2.1 Planner.** Guard order + error vocabulary (donor `plan_revocation` :112): `hasMaster:` when `master_seed_present` ("this device holds the master key — use Remove directly"); `parse_revoke_reason`; `badDeviceVk:`/`unknownDevice:` (same target resolution by ed25519 vk bytes); `selfTarget:` when target == self ("use Remove this device — self-removal needs no co-sign"); `alreadyRevoked:` when `trust.is_revoked(target)`; `notEligible:` when self's enrollment is not Master-issued (depth-1: "this device's enrollment is not master-issued, so it cannot sign a co-sign request"); `noQuorum:` when `eligible_cosigners` is empty ("no other active device with a master-issued enrollment can co-sign"). Happy path: `issued_at = now_secs`, `expires_at_ms = now_ms + QUORUM_REVOCATION_TTL_MS`, `created_at = Hlc { wall_ms: now_ms, logical: 0, device_id: self_id_hex }`, and for each eligible cosigner C: `initiator_sigs[hex(C)] = hex(RevocationCert::sign_quorum_part(device_sk, &revocation_pair_payload(owner_id, target, issued_at, &reason, self_id, C)?))`. `signatures`/`declined_by` empty.
- [ ] **2.2 request impl.** Snapshot NodeState handles (quorum doc+engine, trust doc, identity_dir) under the std lock; `nodeNotRunning:` if quorum doc/engine or trust doc absent. Load keys via `run_blocking` + `OWNER_STATE_WRITE_LOCK` + `load_owner_state(&dir, keychain())` (donor `revoke_device_inner` :637). Trust snapshot from resident doc. `request_id` = 16 random bytes (`rand::rngs::OsRng` via `rand::RngCore::fill_bytes`). Plan → insert into quorum doc (`doc.lock().await`, reject if `requests.len() >= MAX_QUORUM_REQUESTS` with `tooManyRequests:`) → `engine.notify_dirty()` + `flush_now().await` (warn-only on flush error — dirty latch retries; the request is durable in the doc) → `emit_ser(sink, "owner-quorum-updated", Null)` → return id hex.
- [ ] **2.3 cosign impl.** Same handle snapshot + key load. Validations in order, exact error prefixes: `unknownRequest:`; kind must be Revocation (`unsupportedKind:`); `expired:` (`now_ms > expires_at_ms`); `declined:` (self in `declined_by`); idempotent `Ok(())` if self already in `signatures`; `ownRequest:` (self == initiator); `selfTarget:` (self == target); `notEligible:` (self's cert not Master-issued); initiator checks against trust doc — enrolled (`unknownInitiator:`), Master-issued (`initiatorNotEligible:`), not revoked (`initiatorRevoked:`); target enrolled + not revoked (`alreadyRevoked:` benign-error); `notAddressed:` when `initiator_sigs` lacks self's entry; verify the initiator part with `harmony_owner::signing::verify_with_tag(&initiator_vk, tags::REVOCATION, &pair_payload, &sig, "Revocation-Quorum-Part")` (`badInitiatorSig:` on failure). Then sign own part over the SAME pair payload, insert `signatures[self_hex] = QuorumRequestSigs { primary_sig_hex, epoch_doc_sig_hex: None }`, `notify_dirty` + `flush_now` (warn-only), emit `owner-quorum-updated`.
- [ ] **2.4 decline impl.** `unknownRequest:`; `ownRequest:` (initiator cancels by letting it expire — v1 keeps one verb per role); insert self into `declined_by` (idempotent), flush (warn-only), emit `owner-quorum-updated`.
- [ ] **2.5 Registration.** `generate_handler`: `owner_quorum_commands::request_quorum_revocation, owner_quorum_commands::cosign_quorum_request, owner_quorum_commands::decline_quorum_request`. `api/rpc.rs`: `RequestQuorumRevocationArgs { device_vk_hex, reason }`, `RequestIdArgs { request_id }` + three `rpc!` entries delegating to the `_impl`s (donor `revoke_device` block :956).
- [ ] **2.6 Unit tests.** Planner guard matrix (every error prefix above + happy path asserting one `initiator_sigs` entry per eligible cosigner, each verifying via `verify_with_tag` against the initiator's enrolled vk over the recomputed pair payload; quorum-certed initiator → `notEligible:`). Impl-level tests with NodeState fixtures (donor: `owner_commands::tests::revoke_device_inner_master_revokes_sibling_file_only` fixture style, extended with quorum doc/engine handles): request→doc entry present + event emitted; cosign happy → signature entry lands; cosign rejects wrong-cases (expired, declined, own request, non-addressed, tampered initiator sig); decline → tombstone present; second cosign call idempotent.
- [ ] **2.7 Gate + commit.** fmt; `scripts/test-select --context task`; commit `ZEB-677 S3 Task 2: quorum revocation planner + request/cosign/decline IPCs`.

### Task 3: applied task — completion sweep + assembly on the initiator

**Files:**
- Modify: `src-tauri/src/owner_quorum_sync.rs`

**Interfaces produced:**
- `spawn_quorum_applied_task(nudge_rx: mpsc::Receiver<()>, quorum_doc, quorum_engine, trust_doc, trust_engine, device_signing_key: SigningKey, self_device_id: [u8;16], emit: Arc<dyn Fn(&str) + Send + Sync>, retire_nudge: Option<mpsc::Sender<()>>) -> JoinHandle<()>`
- `run_quorum_sweep(…) -> SweepOutcome { doc_changed: bool, revocations_applied: usize }` (extracted async fn so tests drive it directly without the task loop)

**Steps:**

- [ ] **3.1 Sweep body.** (a) Prune: lock quorum doc + trust snapshot, `prune_settled_requests`; if pruned → `quorum_engine.notify_dirty()`. (b) Complete: for each request where `initiator_hex == self`, kind Revocation, unexpired, `declined_by` empty-or-not (a decline only kills the request for the UI; a valid second signature may still exist — v1 rule: ANY decline tombstones, so skip requests with non-empty `declined_by`, matching spec §3): iterate `signatures` in BTreeMap order; for each cosigner C validate against the CURRENT trust doc — C ≠ self, C ≠ target, enrolled, Master-issued, not revoked, and `verify_with_tag` over the pair payload; first valid C wins. Mint own part (`sign_quorum_part`), `parts = [(lo_id, lo_sig), (hi_id, hi_sig)]` sorted by device id, `RevocationCert::assemble_quorum(owner_id, target, issued_at, reason, parts)`. Apply through `mutate_trust_state(Resident { trust_doc, trust_engine }, |s| if s.is_revoked(target) { Ok(()) } else { s.add_revocation(cert, now_secs, DEFAULT_ACTIVE_WINDOW_SECS) })` — the crate's quorum arm does full policy verification (≥2 distinct enrolled non-revoked active-window signers, not backdated). On `Ok`: `trust_engine.flush_now()` (warn-only), `emit("owner-devices-updated")`, retire-nudge `try_send`, remove the request from the quorum doc + `notify_dirty` + `flush_now` (warn-only). On `Err`: `warn` and leave the request (retry next nudge; expiry bounds it). (c) If the quorum doc changed at all (fingerprint compare, donor `device_set_fingerprint` idiom — here canonical-encode compare taken before/after) → `emit("owner-quorum-updated")`.
- [ ] **3.2 Task loop.** Donor `spawn_trust_applied_task` shape: `while nudge_rx.recv().await.is_some() { run_quorum_sweep(…).await }`.
- [ ] **3.3 Tests** (in-module, `collecting_emit` donor): initiator + valid cosigner sig in doc → sweep applies revocation to trust doc, prunes request, emits both events, retire nudge fired; sweep is idempotent (second run: no events, no change); invalid cosigner sig skipped + request retained; declined request never assembles; non-initiator device never assembles someone else's request; expired request pruned without assembly; `add_revocation` rejection (e.g. cosigner cert quorum-issued — build via `enrollment_verify::quorum_fixtures::mint_quorum_world`) leaves request intact and applies nothing.
- [ ] **3.4 Gate + commit.** fmt; `scripts/test-select --context task`; commit `ZEB-677 S3 Task 3: initiator completion sweep — assemble + apply quorum revocation`.

### Task 4: boot wiring — engine, NodeState, adapter, halt

**Files:**
- Modify: `src-tauri/src/lib.rs` — NodeState fields (near `owner_trust_doc` :1375): `owner_quorum_doc: Option<Arc<tokio::sync::Mutex<QuorumReqDoc>>>`, `owner_quorum_sync: Option<Arc<FleetSyncEngine<QuorumReqDoc>>>`; init `None` (:1868 block), clear at stop_node (:2428 block), assign into NodeState (:10696 block); engine construction after the fleet-keys block (:5648-…) mirroring the trust block :5524-5647; thread `quorum_sync_handles_for_loop` (:10262 block) into `event_loop::run` (:10379 call)
- Modify: `src-tauri/src/event_loop.rs` — `run` param `mut quorum_sync_handles: Option<DatasetSyncHandles>` (next to `trust_sync_handles` :1037); adapter block after the fleet-keys one (:1951-1962): `spawn_dataset_sync_zenoh_adapter(…, OWNER_QUORUM_DATASET, "owner-quorum-sync-degraded", OWNER_QUORUM_DATASET_MAX_BYTES)`

**Steps:**

- [ ] **4.1 Engine block.** Donor trust block: doc from `load_quorum_doc_or_recover(&identity_dir.join(OWNER_QUORUM_DOC_FILENAME))` (missing file → `QuorumReqDoc::default()`), tracker from `load_quorum_replay_or_recover`, channels (out/in 32, nudge 8), `FleetSyncConfig { keys: keys.clone() /* swappable set, same as trust */, device_id, state, merger: quorum_merger(), replay_tracker, content_store, publisher_tx, subscriber_rx, persist: QuorumPersist { doc_path, replay_path }, lookup_key_tag: OWNER_QUORUM_LOOKUP_TAG, debounce_ms: DEFAULT_DEBOUNCE_MS, publish_seen: true, on_applied: Some(ingest_nudge_on_applied(quorum_nudge_tx.clone())), sibling_acks: fresh }`. Spawn the applied task with `loaded.device_signing_key.clone()`, self device id, trust doc+engine Arcs, the same `emit` closure shape as the trust detector (:5611-5619), and `retire_deposit_nudge_tx.clone()` (in scope :5547). **Boot nudge:** `let _ = quorum_nudge_tx.try_send(());` after spawning (completions that accumulated while this device was offline must not wait for the first remote merge). Add the quorum engine to the revoked-self halt closure (:5623-5635) as a fourth shutdown.
- [ ] **4.2 Adapter + threading.** Event-loop param + block; lib.rs handle bundle + `_for_loop` binding + call-site arg (grep `trust_sync_handles_for_loop` for all three touch points).
- [ ] **4.3 Compile check** `cargo check -p harmony-app --features test-fixtures` (boot block has no direct unit test; the two-engine test in Task 6 covers the engine config).
- [ ] **4.4 Gate + commit.** fmt; `scripts/test-select --context task`; commit `ZEB-677 S3 Task 4: boot the owner-quorum-req-v1 engine + zenoh adapter + halt wiring`.

### Task 5: view fields — `selfIsMaster`, `quorumRequests`, `quorumArmedUntilMs`, `quorumRemovable`

**Files:**
- Modify: `src-tauri/src/owner_state.rs` — `OwnerStateView` += `#[serde(default)] pub self_is_master: bool`, `#[serde(default)] pub quorum_requests: Vec<QuorumRequestView>`, `#[serde(default)] pub quorum_armed_until_ms: Option<u64>`; `DeviceView` += `#[serde(default)] pub quorum_removable: bool`; new `QuorumRequestView { request_id, kind: String ("revocation"), target_device_id, initiator_device_id, reason, expires_at_ms, initiated_by_me, signed_by_me, declined_by_me, cosigner_signed, can_cosign }` (camelCase serde like siblings)
- Modify: `src-tauri/src/owner_commands.rs` — `get_owner_state_inner` snapshots `NodeState.owner_quorum_doc` (alongside the trust-resident snapshot :517-525) into a `QuorumJoin { requests: Vec<(String, QuorumRequest)>, armed_until_ms: Option<u64> }` passed to `build_owner_state_view`; view build computes the new fields

**Steps:**

- [ ] **5.1 Snapshot.** In `get_owner_state_inner`, clone `owner_quorum_doc` under the std lock with the other handles; `if let Some(arc) = …` lock (async) and materialize `QuorumJoin`: unexpired requests only; `armed_until_ms` from `enroll_arms[self_id_hex]` if unexpired (self id known only later in the blocking closure — resolve by passing the raw map snapshot and joining inside `build_owner_state_view`, mirroring how `FleetJoin` carries raw rows). Both call sites of `build_owner_state_view` (resident :556 and file-only :586) pass the same join (file-only: `QuorumJoin::default()` — node down ⇒ no resident doc ⇒ no pending ceremony surface; honest).
- [ ] **5.2 View computation.** `self_is_master = loaded.master_seed.is_some()` (identical to `can_back_up` TODAY by design — distinct field because their future semantics diverge, spec §5). Per request → `QuorumRequestView`: `initiated_by_me` / `signed_by_me` / `declined_by_me` by self id hex; `cosigner_signed` = `!signatures.is_empty()`; `can_cosign` = `!initiated_by_me && !signed_by_me && !declined_by_me && self-not-target && self-master-certed && initiator_sigs.contains_key(self)` (server-computed so the panel stays dumb). Per device row: `quorum_removable` = `!is_this_device && !revoked && !self_is_master && self-master-certed && ∃ other sibling (≠ self, ≠ row, non-revoked, Master-issued cert, in active_devices(now, DEFAULT_ACTIVE_WINDOW_SECS))` (spec §4.1 visibility rule; initiator's own activity is implied — it is making the call).
- [ ] **5.3 Tests.** Extend the existing `build_owner_state_view`/`get_owner_state` test group: `self_is_master` mirrors seed presence; `quorum_removable` matrix (master-less 3-device fleet → true on sibling rows; false when: seed present / row is self / row revoked / no second master-certed active sibling / self quorum-certed); `QuorumRequestView` flag matrix incl. `can_cosign` gating; `quorum_armed_until_ms` surfaces only an unexpired own-cell arm.
- [ ] **5.4 Gate + commit.** fmt; `scripts/test-select --context task`; commit `ZEB-677 S3 Task 5: OwnerStateView quorum fields + quorum_removable`.

### Task 6: two-engine integration test — request → co-sign → revoke lands fleet-wide

**Files:**
- Modify: `src-tauri/src/owner_quorum_sync.rs` (tests module)

**Steps:**

- [ ] **6.1 Harness.** `QuorumPair`: donor `TrustPair` (:565-637) extended to run BOTH datasets per device — trust engines A/B crossed over in-memory channels AND quorum engines A/B crossed over a second channel pair, sharing one `InMemoryStub` CAS and one KeyTree. Seed: mint owner on A, master-enroll B + C (C = the revocation target, offline — no engine), fold enrollments + a liveness cert per signer into the seeded trust doc so `active_devices` sees A and B.
- [ ] **6.2 Happy path.** A: `plan_quorum_revocation_request` + insert into A's quorum doc + `notify_dirty` (IPC body minus NodeState). Await B's quorum doc containing the request (poll ≤5s, donor loop). B: run the cosign core against B's docs (validate + sign + union — call the extracted seam, not the `#[tauri::command]`). Await A's merge; drive A's `run_quorum_sweep`. Assert: A's trust doc `is_revoked(C)`; B's trust doc converges to `is_revoked(C)` via trust replication (poll); the request pruned from A's doc; A emitted `owner-devices-updated` + `owner-quorum-updated`; B's doc prunes after its next sweep (revoked-target predicate).
- [ ] **6.3 Decline path.** B declines instead → tombstone reaches A; A's sweep never assembles; C stays unrevoked both sides; request survives (UI-dead) until a time-warped expiry sweep (`now_ms` param makes expiry testable without waiting — pass `expires_at_ms + 1`).
- [ ] **6.4 Crash-retry idempotency.** After the happy-path revocation lands, re-insert a duplicate pending request for the same target (simulates initiator crash after co-sign but before prune, then re-request); sweep prunes it via the revoked-target predicate without a second `add_revocation`.
- [ ] **6.5 Gate + commit.** fmt; `scripts/test-select --context task`; commit `ZEB-677 S3 Task 6: two-engine quorum ceremony integration tests`.

### Task 7: frontend — service, banner, Remove gating, dialog copy, vitest

**Files:**
- Modify: `src/lib/owner-service.ts` — `OwnerStateView` += `selfIsMaster: boolean; quorumRequests: QuorumRequestView[]; quorumArmedUntilMs: number | null;` `DeviceView` += `quorumRemovable: boolean;` new `QuorumRequestView` interface; `OwnerService` += `requestQuorumRevocation(deviceVkHex, reason)`, `cosignQuorumRequest(requestId)`, `declineQuorumRequest(requestId)` (invoke + refresh, same shape as `revoke`)
- Modify: `src/lib/components/DevicesPanel.svelte`
- Modify: `src/lib/components/RemoveDeviceDialog.svelte` — new optional prop `quorum = false`; when set, body gains the spec §4 line
- Modify: `src/lib/components/__tests__/DevicesPanel.test.ts`, `src/lib/components/__tests__/RemoveDeviceDialog.test.ts`

**Steps:**

- [ ] **7.1 Service.** Types + three methods; errors extracted per convention. Note `quorumRequests` may be `undefined` on stale backends — normalize with `?? []` at the read site (additive-field tolerance, donor `lastSeenMs` typeof guard).
- [ ] **7.2 Co-sign banner.** New block after the epoch banner (:752-777, same `.epoch-banner` styling class family — add `.quorum-banner` reusing the CSS shape, `data-testid="quorum-cosign-banner"`): for each request with `canCosign`: text `Co-sign request from {deviceName(initiatorDeviceId)}: remove {deviceName(targetDeviceId)} ({reason})` — `deviceName` = petname/displayName join against `state.devices` by `deviceId`, falling back to the 8-hex prefix; buttons `Approve` (click-confirm tier — the typed-confirm already happened on the initiator, spec §4.2) → `svc.cosignQuorumRequest(r.requestId)` and `Decline` → `svc.declineQuorumRequest(r.requestId)`; per-banner in-flight + error state. For requests with `initiatedByMe`: passive line `Waiting for another device to co-sign removal of {target} — expires {relative time}` (`data-testid="quorum-pending-note"`), plus `Request sent — co-signed` variant when `cosignerSigned` (sweep will complete momentarily).
- [ ] **7.3 Remove gating + honesty copy.** Sibling-row affordance block (:817-841): `{#if state.canBackUp}` → `{#if state.canBackUp || device.quorumRemovable}` with `Replace…` still master-only (`state.canBackUp`) — replace re-pairs, which needs the S4 arm flow. `handleRemoveConfirm`: branch — master path unchanged (`svc.revoke`); quorum path (`!state.canBackUp && removeTarget.quorumRemovable`) → `svc.requestQuorumRevocation(removeTarget.deviceVkHex, reason)`, close dialog on success (banner takes over as the pending surface). Dialog invocation passes `quorum={!state?.canBackUp && removeTarget.quorumRemovable === true}`. Master-less fleet with NO quorum (≥2 master-certed actives absent): the §8 floor copy — in the add-another-device footer's `{:else}` explainer (:941-944) is master-wipe copy already; leave it, and add nothing new (the Remove button simply doesn't render — spec §8 "arm affordance hidden").
- [ ] **7.4 Dialog copy.** `RemoveDeviceDialog` `quorum` prop: append paragraph `This device doesn't hold your master key. Your other devices will be asked to co-sign.` (`data-testid="remove-quorum-copy"`); keep the typed-confirm tier untouched.
- [ ] **7.5 Listener.** `listen('owner-quorum-updated', () => svc.refresh().catch(() => {}))` alongside the `owner-devices-updated` listener (:433-449), same unlisten hygiene.
- [ ] **7.6 Vitest.** DevicesPanel: banner renders from a `canCosign` fixture with joined names; Approve/Decline invoke the right IPCs with camelCase args; banner absent when `canCosign` false (signed/declined/initiator); pending note renders for `initiatedByMe`; sibling Remove visible with `quorumRemovable` when `canBackUp` false and hidden when both false; quorum remove confirm calls `request_quorum_revocation` not `revoke_device`; `quorumRequests` undefined tolerated. RemoveDeviceDialog: quorum copy shown/hidden by prop; typed-confirm still enforced.
- [ ] **7.7 Gate + commit.** `npx tsc --noEmit && npx vitest run` + fmt/test-select untouched-Rust sanity; commit `ZEB-677 S3 Task 7: co-sign banner, quorum Remove flow, service + dialog copy`.

### Task 8: final sweep + PR

- [ ] **8.1** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; full `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (background with wall-clock net); `npx tsc --noEmit`; `npx vitest run`.
- [ ] **8.2** Self-review diff (second-order check: new invariants — grow-only union vs prune interplay, no keychain writes, no `Date.now` in Rust tests without control).
- [ ] **8.3** PR to `zeblithic/harmony-client` titled `ZEB-677 S3: owner-quorum-req-v1 dataset + quorum revocation ceremony (request/co-sign/assemble) + DevicesPanel co-sign surface`; body: slices context, §3 deviation note (per-pair initiator sigs), §7 interim (no epoch bump — fleetEpochStale banner), test evidence. Fire `@coderabbitai review` once at open.

## Self-review notes (writing time)

- Spec §3 `QuorumRequest.signatures` "its own included" → replaced by `initiator_sigs` per-pair map (S1 payload binds signer set); deviation documented in Global Constraints + PR body.
- Spec §3 "Expired and declined requests are pruned" → declined retained-until-expiry to prevent union resurrection of the decline tombstone; UI treats declined as gone immediately. Documented in Task 1.3.
- Spec §3 `epoch_doc` in kind / `QuorumRequestSigs.epoch_doc_sig_hex` → slot present, always `None` (S5).
- Arm IPCs (`arm_quorum_enrollment`/`disarm`) are S4 per §9; `EnrollArm` struct + `quorumArmedUntilMs` view land now for schema stability.
- Completion is initiator-driven (§3): only the initiator assembles; other devices prune via the revoked-target predicate.
