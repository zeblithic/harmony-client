# ZEB-170 Track B v1 — Devices panel + harmony-owner wiring

> Linear: [ZEB-170 Track B: Device administration UI in harmony-client](https://linear.app/zeblith/issue/ZEB-170)
> Parent umbrella: ZEB-169 Track A — Multi-device unified identity
> Date: 2026-04-28
> Status: Design

## Goal

Wire `harmony-owner` (shipped via ZEB-173, harmony PR #261) into `harmony-client` and surface a read-only "My Devices" panel under Settings → Identity. Pairing flow and revocation UI are explicit follow-ups, deferred to separate tickets after v1 lands.

After this v1 ships, a user can:
1. Open Settings → Identity → Devices for the first time and create a new owner identity through a confirm-modal flow.
2. See their owner identity (16-byte fingerprint, "Back up" CTA) and the single device they just bound (this device).
3. Back up the owner recovery artifact through the existing PR-61 backup wizard, with passphrase-encrypted file output.
4. Rename the device locally.
5. On subsequent launches, see the populated panel directly without the bootstrap modal.

## Background

`harmony-owner` (in the upstream `harmony` repo, `crates/harmony-owner/`) provides the owner→device binding primitive:
- `mint_owner(now)` mints a master signing key + device #1 keypair atomically; returns `OwnerState`, a 32-byte recovery artifact (the master seed), and a fresh `device_signing_key`. The master is wiped from RAM after mint per upstream design intent.
- `enroll_via_master(state, artifact, device_sk, device_pubkey, ...)` enrolls device N≥2 by transiently reconstructing the master from the recovery artifact.
- `enroll_via_quorum(...)` enrolls device N≥3 via K=2 active-sibling signatures, no master needed.
- `trust::evaluate_trust(state, target_id, now, ...)` returns `Full | Provisional | Refused` per device.
- `OwnerState` carries `enrollments`, `vouching` CRDT, `revocations` CRDT, `liveness` heartbeats. All certs are CBOR-canonicalized + Ed25519-signed with domain-separation tags.

`harmony-client` already has:
- `harmony-owner` in its Cargo.toml (`recovery` feature enabled, used by PR-59 CLI and PR-61 GUI).
- Per-device identity (Ed25519 + ML-DSA + ML-KEM) in keychain via PR-58 (ZEB-174). The 16-byte `IdentityHash` is what peers route to.
- PR-61 backup/restore GUI wizard, the `PreviewedRecovery` token-cache pattern (single-use, TTL-bounded, `Zeroizing`-wrapped), and the `write_atomic_0600` file primitive.

What's missing — and what this v1 introduces:
- No `OwnerState` is persisted anywhere in harmony-client; `mint_owner` has never been called.
- The `device_signing_key` (Ed25519, owner-scoped, distinct from per-device transport keys) has no keychain slot.
- No UI for any of this.

## Scope

### In scope

1. Persist `OwnerState`, `device_signing_key`, and the 32-byte master seed at rest.
2. Tauri commands: `get_owner_state`, `mint_owner_identity`, `export_owner_recovery_file_to_path`. (Preview/restore flows are deferred along with the pairing UX that would consume them.)
3. Settings → Identity → Devices Svelte panel:
   - Empty state with bootstrap CTA.
   - Bootstrap confirm-modal.
   - Populated state with owner header + single device row + educational "Add another device" footer.
   - Back-up CTA chained into a parallel-to-PR-61 wizard variant.
   - Local-device rename via existing `profile-service.ts`.
4. Test coverage: Rust unit tests for state persistence + token cache + degraded states + concurrent-mint guard, Rust integration test for end-to-end mint→export→decrypt round-trip, Vitest tests for empty/bootstrap/populated/error UI paths.

### Out of scope (deferred follow-ups)

1. **Pairing UX** (`enroll_via_master` / `enroll_via_quorum` GUI flows). Two devices coexisting under one owner identity requires a follow-up. The v1 panel's "Add another device" footer educates the user that this is coming.
2. **Revocation UI**. Marking a device stolen/lost and propagating a `RevocationCert` to siblings.
3. **Vouching gossip transport**. Publishing/subscribing to `LivenessCert` / `VouchingCert` / `RevocationCert` updates over Zenoh — needed for live cross-device state in later versions.
4. **Hardware label** ("KRILE 4090/22GB"). Free-form vs derived-from-system-info is out of v1.
5. **Capabilities display per device**. Comes with the gossip transport.
6. **Last-seen** annotation. Requires liveness gossip; v1 shows "Added <date>" from `EnrollmentCert.issued_at` only.
7. **Multi-device cross-device-rename**. v1 only renames *this* device locally.
8. **"Wipe master from device" UX**. The master-seed-missing degraded panel state must render correctly because future tickets will introduce the wipe action; v1 itself does not ship the action.

## Architecture

### Two-address world

Each device now carries two distinct 16-byte hashes:

- **Transport address** (existing): `SHA256(transport_ed25519_verify || transport_ml_dsa_verify)[:16]` — derived from the harmony-identity bundle. This is what peers route to over Zenoh / Iroh / Reticulum. Unchanged by this work.
- **Owner-scoped device_id** (new): `SHA256(owner_ed25519_verify)[:16]` — derived from the fresh Ed25519 key that `mint_owner()` generates. This is the key inside `OwnerState.enrollments` and inside enrollment / vouching / liveness / revocation certs.

These are intentionally separate. The bridge between them — profile-update gossip carrying both addresses, so a peer can correlate a transport-layer message with an owner-scope enrollment — is part of the deferred gossip transport, not v1.

For v1's read-only panel, the user sees the owner-scoped `device_id` as a fingerprint (first 8 hex characters, formatted `xxxx·xxxx`); the transport address is internal plumbing that doesn't surface.

### Layering

`harmony-owner` integration is purely additive. `identity.rs` and the existing per-device identity flow are untouched. New modules sit alongside.

```
harmony-client/src-tauri/src/
├── identity.rs              (unchanged — per-device transport identity)
├── identity_commands.rs     (unchanged — PR-61 backup/restore wizard)
├── recovery_cli.rs          (unchanged — ZEB-176 CLI)
├── owner_state.rs           (NEW — load/save OwnerState + keys + token cache)
├── owner_commands.rs        (NEW — Tauri commands)
└── lib.rs                   (modify — register new commands; pub-export NodeState already done)
```

### Persistence

Three new artifacts at rest, sibling to `identity.enc`:

1. **Keychain entry `harmony.owner.device_signing_key`** — Ed25519 secret bytes (32B). Falls back to encrypted file using the existing `HARMONY_PASSPHRASE` resolution chain (same chain as the per-device seed). Same `KeychainStore` injection pattern PR-61 established.
2. **Keychain entry `harmony.owner.master_seed`** — 32-byte master seed. Same fallback chain. *See "Documented divergence from upstream" below.*
3. **File `owner_state.cbor`** at the same dir as `identity.enc`, mode 0600 — canonical CBOR encoding of `OwnerState` via `harmony_owner::cbor::to_canonical`. All certs inside are public-key signed material, so plaintext at the file layer is fine. Written via `write_atomic_0600` (the helper PR-61 already established).

The `.cbor` file's presence is the *minted-marker* for `get_owner_state()` — if the file is absent, the panel renders empty state regardless of keychain contents.

### Documented divergence from upstream

`harmony_owner::lifecycle::mint_owner` documents:
> IMPORTANT: After this returns, the master key is reconstructible only from the recovery artifact. Callers must never persist the master key outside that artifact.

We are deliberately diverging — we persist the 32-byte master seed (encrypted, under the existing at-rest `HARMONY_PASSPHRASE`) so the user can re-issue backups anytime. Rationale:

- harmony-client already persists the per-device seed under the same at-rest passphrase. The master is no worse-protected than the per-device transport seed already is.
- Without persistence, "dismiss-with-warning" becomes a one-shot footgun: a user who declines to back up at mint time cannot back up later, and their recovery artifact is lost forever — a worse outcome than the protocol's strict-no-persist intent was trying to prevent.
- A future "Wipe master from device" UX action, opt-in by users who want the strict harmony-owner posture (master cannot be re-extracted from the device), can land as a follow-up. The v1 panel already handles the master-seed-missing degraded state correctly.

This divergence is local to harmony-client. We may want to raise it with the upstream `harmony-owner` maintainers (the same project, but separate spec): either soften the doc to acknowledge persist-with-warning as an acceptable mode, add a configuration toggle, or keep the divergence as a documented harmony-client policy.

### Bootstrap flow

On first launch nothing is minted. The user explicitly triggers minting by visiting Settings → Identity → Devices:

1. Panel mounts → calls `get_owner_state()` → returns `null` (no `owner_state.cbor`).
2. Panel renders empty state with one CTA: "Bind this device to a new owner identity →"
3. Click → confirm-modal explaining what will happen (an owner identity will be created; this device will be bound; a recovery file should be backed up).
4. User confirms → invoke `mint_owner_identity()`.
5. Backend:
   a. Acquires the process-wide mint mutex (refuses if another mint is in flight).
   b. Refuses if `node_state.is_running()` (mirrors PR-61's restore guard via `require_node_stopped(state)` — re-uses the helper).
   c. Refuses if `owner_state.cbor` already exists (idempotent failure, like PR-61 restore-without-force).
   d. Calls `harmony_owner::lifecycle::mint_owner(now)`. Holds `MintResult { state, recovery_artifact, device_signing_key }` in memory.
   e. Persists keychain entries (`device_signing_key`, then `master_seed`).
   f. Atomically writes `owner_state.cbor` with `write_atomic_0600`.
   g. Inserts the master seed into the token cache (single-use, TTL-bounded, `Zeroizing`-wrapped — same pattern as PR-61's `PreviewedRecovery`); receives a UUID-based `recoveryToken`.
   h. Returns `{ state: OwnerStateView, recoveryToken: string }`.
6. Frontend: panel transitions to populated state. The "Back up your owner identity now" CTA in the header glows.
7. Two paths:
   - **Back up immediately:** click → opens a parallel-to-PR-61 wizard variant. Wizard collects passphrase → invokes `export_owner_recovery_file_to_path(token, path, passphrase)` → token consumed → encrypted file written.
   - **Dismiss-with-warning:** clicking outside the highlighted CTA shows a toast: "Your owner recovery is unbacked. Anyone losing this device cannot recover it." The header CTA stays highlighted across sessions; user can come back later.

### Subsequent launches

`get_owner_state()` reads `owner_state.cbor` and the keychain → returns populated `OwnerStateView`. Panel skips the bootstrap modal entirely and renders the populated state directly.

### Rename

`profile-service.ts` already maintains a per-device `displayName` in localStorage. The Devices panel's Rename action wires through the existing service — no new persistence layer in v1. Cross-device names (other devices' displayNames as gossiped via profile-update) are out of v1 scope.

## Components

### Backend Rust modules

#### `src-tauri/src/owner_state.rs` (new)

Public API:

- `pub fn load_owner_state(plaintext_path: &Path, keychain: Option<KeychainStore>) -> Result<Option<OwnerState>, String>` — returns `Ok(None)` when `owner_state.cbor` is absent (empty state, the natural un-minted condition); returns `Err(...)` for corrupt CBOR or for the inconsistent-state cases (state present but `device_signing_key` missing).
- `pub fn save_owner_state_atomic(...)` — encapsulates the atomicity contract: keychain writes first, `.cbor` last.
- `pub fn load_device_signing_key(...) -> Result<SigningKey, String>` — keychain primary, encrypted-file fallback.
- `pub fn load_master_seed(...) -> Result<Option<[u8; 32]>, String>` — `Ok(None)` when seed missing but state+signing_key present (degraded "cannot back up" state).

Internal: token cache for the recovery-artifact bytes (`Mutex<HashMap<Uuid, PreviewEntry>>`, single-use, TTL via background-LRU eviction). Mirrors PR-61's `PreviewedRecovery` shape exactly.

Public types (serde-camelCase, mirrored to TS):

- `OwnerStateView { ownerId: String, ownerDisplayName: String, devices: Vec<DeviceView>, canBackUp: bool }`
- `DeviceView { deviceId: String, displayName: String, isThisDevice: bool, trustDecision: TrustDecisionView, enrolledAt: u64, fingerprint: String }`
- `TrustDecisionView { kind: "full" | "provisional" | "refused", reason: Option<String> }`

#### `src-tauri/src/owner_commands.rs` (new)

Tauri commands (all `pub async fn`, all using `tauri::State<'_, Mutex<NodeState>>` where state mutation is involved):

- `#[tauri::command] pub async fn get_owner_state() -> Result<Option<OwnerStateView>, String>` — load-and-render, returns `null` for empty state (un-minted is normal; not an error).
- `#[tauri::command] pub async fn mint_owner_identity(state: tauri::State<'_, Mutex<NodeState>>) -> Result<MintResult, String>` — bootstrap. `MintResult { state: OwnerStateView, recoveryToken: String }`.
- `#[tauri::command] pub async fn export_owner_recovery_file_to_path(recoveryToken: String, path: String, passphrase: String, comment: Option<String>) -> Result<ExportInfo, String>` — single-use token; consumes on success or hard-failure (matches PR-61 single-use semantics; user re-CTAs to mint a fresh token).

All long-running operations (Argon2id KDF, file I/O) use the `run_blocking` adapter PR-61 introduced (`tokio::task::spawn_blocking`), so the async executor is never stalled.

#### `src-tauri/src/lib.rs` (modify)

- Add the four new commands to `tauri::generate_handler!`.
- The `pub` visibility on `NodeState` is already in place from PR-61 round 5.

### Frontend (`src/lib/`)

#### `owner-service.ts` (new)

Service-class pattern mirroring `notification-service.ts`:

- `class OwnerService { state: OwnerStateView | null; onChange?: () => void; ... }`
- Methods: `refresh()`, `mint()`, `exportRecoveryFile(token, path, passphrase, comment)`.
- Wraps Tauri invokes; converts camelCase JSON. Error extraction uses `e instanceof Error ? e.message : String(e)` (memory rule).

#### `components/DevicesPanel.svelte` (new)

Mounts inside Settings → Identity (a new sub-tab next to "Backup & Restore" from PR-61).

States:
1. **Empty** — `OwnerStateView === null`. Renders "Bind this device to a new owner identity →" CTA.
2. **Bootstrap modal** — confirm dialog explaining the mint action.
3. **Populated** — owner header (display name, fingerprint, Back-up CTA) + device list (one row in v1) + educational footer.
4. **Degraded (canBackUp=false)** — populated panel renders normally but Back-up CTA is disabled with explanatory tooltip.

Reuses the existing PR-61 backup wizard for the artifact-export step, but with separate copy to distinguish:
- "Back up **owner** identity" — new flow, uses `export_owner_recovery_file_to_path`.
- "Back up **device** identity" — existing flow, unchanged.

The two flows coexist; the v1 panel and the chained wizard make the distinction unambiguous.

#### `components/__tests__/DevicesPanel.test.ts` (new)

Vitest coverage detailed in Testing section.

## Data flow

### Mint sequence — atomicity contract

```
mint_owner_identity():
  1. Acquire process-wide Mutex<MintGuard>.
  2. Verify NodeState.is_running() == false. Else ERR_NODE_RUNNING.
  3. Verify owner_state.cbor does not exist. Else "already minted" error.
  4. Call harmony_owner::lifecycle::mint_owner(now). [In-memory MintResult.]
  5. keychain.set("harmony.owner.device_signing_key", device_sk_bytes).
  6. keychain.set("harmony.owner.master_seed", recovery_artifact.as_bytes()).
  7. write_atomic_0600(owner_state.cbor, canonical_cbor(state)).  // minted-marker
  8. token_cache.insert(uuid, master_seed_bytes_zeroizing).  // single-use
  9. Return { state: view(state), recoveryToken: uuid }.
```

If any step 5-7 fails, error propagates; partial keychain entries remain but are tolerated (overwritten on next mint attempt). The `.cbor` file is the minted-marker — its absence on next launch yields the natural empty state.

If step 4 fails (extremely unlikely), nothing is persisted.

### Backup export

Mirrors PR-61's preview→commit pattern:

```
export_owner_recovery_file_to_path(token, path, passphrase, comment):
  1. token_cache.take(token) → Option<Zeroizing<[u8; 32]>>.
     If None: "expired or invalid token". (Single-use semantics.)
  2. RecoveryArtifact::from_seed(*seed)
       .to_encrypted_file(&SecretString::from(passphrase), &RecoveryMetadata{...})
     → encrypted bytes.
  3. write_atomic_0600(path, bytes).
  4. Return ExportInfo { identityHash, byteLen }.
```

Token is consumed on the `take()` call in step 1, regardless of subsequent success — single-use means the user re-CTAs to retry on disk-write failure. (Exactly matches PR-61's behavior.)

### Read flow

```
get_owner_state():
  1. cbor_path = identity_dir.join("owner_state.cbor")
  2. If !cbor_path.exists(): return Ok(None).  // un-minted, natural empty state
  3. Read + canonical-CBOR-decode into OwnerState.  Err on corrupt.
  4. load_device_signing_key() → check presence.  Err if missing (inconsistent state).
  5. load_master_seed() → Option<[u8; 32]>.  None means degraded "cannot back up".
  6. Build OwnerStateView:
       canBackUp: master_seed.is_some()
       devices: state.enrollments.values().map(view) — single row in v1
  7. Return Ok(Some(view)).
```

### Rename

```
DevicesPanel.svelte rename action:
  → owner-service.ts rename(deviceId, newName)
    → if deviceId === currentDevice: profile-service saveProfile({ displayName: newName })
    → emit onChange  // panel re-renders
```

v1 only handles the *current* device; the rename for other devices (when they exist) is handled by the deferred gossip transport.

## Error handling

### Bootstrap (mint)

| Failure | UX | Persistence outcome |
|---|---|---|
| Concurrent mint in flight | Toast "Another mint is in progress, retry shortly" | No partial state. |
| Node running | Toast `ERR_NODE_RUNNING` ("Stop the node before minting...") | No partial state. |
| Already minted | Inline error in modal | No state change. |
| `mint_owner` upstream error | Inline error with the upstream message | No partial state. |
| Keychain write fails | Inline error, surfaced upstream message | Possibly orphan keychain entry (previous step succeeded). Tolerated; overwritten on retry. |
| `.cbor` write fails | Inline error | Keychain entries present but no minted-marker — treated as un-minted on next launch. Cleanup on failure is nice-to-have, not v1. |

### Read

| Failure | UX |
|---|---|
| `.cbor` corrupt | Panel shows "Inconsistent owner state — wipe and re-mint" admin path. v1 does *not* auto-wipe. |
| `.cbor` present, `device_signing_key` missing | Same admin path. |
| `.cbor` + `device_signing_key` present, `master_seed` missing | Panel renders normally. Back-up CTA disabled with tooltip "Master seed not on this device — backup is no longer possible." (Future "Wipe master" path produces this state.) |
| `.cbor` absent | Empty state (natural un-minted condition; not an error). |

### Backup export

| Failure | UX |
|---|---|
| Token expired/invalid/double-use | Inline error in wizard "Backup token expired, please retry from Devices panel." Token already consumed by the failed `take()`. |
| Weak passphrase | Inline validation error in wizard, same minimum rules as PR-61. Token *not* consumed (validation precedes `take()`). |
| Disk write fails | Wizard inline error, atomic write means the partial file is cleaned up. Token consumed (single-use). |

### Tauri error extraction in tests

Production rejections are strings; test rejections are Error objects. Memory rule applies — all error-path tests use `e instanceof Error ? e.message : String(e)`.

## Testing strategy

### Rust unit (`src-tauri/src/owner_state.rs#tests`, `owner_commands.rs#tests`)

- Mint → save → load round-trip preserves owner_id, device_id, enrollment cert.
- Token cache invariants: insert+take is single-use; double-take fails; TTL eviction.
- Concurrent mint guard: two parallel `mint_owner_identity()` calls — exactly one succeeds.
- Mint refuses when node is running (uses `require_node_stopped(state)` from PR-61).
- Mint refuses when `.cbor` already exists.
- Read: corrupt `.cbor` → typed error.
- Read: state present but `device_signing_key` missing → "inconsistent state" error.
- Read: state + signing_key present but master_seed missing → returns view with `canBackUp: false`.
- Export: token-cache invariants observable through the command surface.
- All cache-mutating tests use `#[serial]` + `clear_token_cache()` at entry (PR-61 pattern).
- Inject keychain via `Option<KeychainStore>` for hermeticity (no developer-keychain pollution).

### Rust integration (`src-tauri/tests/owner_integration.rs`)

- End-to-end: mint → `export_owner_recovery_file_to_path` → file-on-disk → `RecoveryArtifact::from_encrypted_file` round-trips to the same 32-byte master seed. Validates the single-use token, the encrypted-file format, and the at-rest CBOR encoding hold together.
- Reuses the `TempDir`-based isolation pattern PR-61 established.

### Vitest (`src/lib/components/__tests__/DevicesPanel.test.ts`)

- Empty state renders bootstrap CTA when `get_owner_state` returns null.
- Bootstrap modal → confirm → `mint_owner_identity` invoked → populated panel.
- Dismiss-with-warning: post-mint dismiss shows toast, header CTA stays highlighted.
- Populated panel renders device row with display name, fingerprint, trust badge, enrolled-date copy.
- Rename: inline edit → save → `profile-service.saveProfile` called.
- Back-up CTA: passes `recoveryToken` to wizard via the chained flow.
- Degraded (`canBackUp: false`): backup CTA disabled with tooltip.
- All error-path tests use `e instanceof Error ? e.message : String(e)`.

### CI

Reuses existing gates from PR-61: `cargo test --workspace --all-targets`, vitest, fmt, clippy with CI flags (`--locked --all-targets --no-deps`), MSRV 1.88 check. No new pipeline work needed.

## Open questions / upstream feedback

1. **Documented divergence on master-seed persistence.** The harmony-owner `mint_owner` doc says callers must not persist the master key outside the recovery artifact. We are diverging. Worth raising with upstream after v1 lands: should the doc soften, should there be a configuration toggle, or should the divergence stay local to harmony-client policy?

2. **Two-recovery-file labeling.** Users now have two distinct backups (per-device transport seed via PR-61, owner master seed via this v1). The wizard copy needs to be unambiguous. Inline copy proposed during brainstorm; final copy will be reviewed during implementation.

3. **`enroll_via_master` on the v1 device.** v1 deliberately does not wire the enrollment-from-recovery path. The v1 panel's "Add another device" footer promises a future capability rather than a current one — copy is deliberately framed as forward-looking ("coming") rather than instructional.

## Future work (deferred follow-ups)

To be filed as separate tickets after v1 ships:

1. **Pairing UX** — `enroll_via_master` flow (recovery-artifact transit between two devices) and `enroll_via_quorum` flow (K=2 active-sibling signing). Two devices coexisting under one owner.
2. **Vouching gossip transport** — Zenoh subscription for `LivenessCert` / `VouchingCert` / `RevocationCert`. Enables live cross-device state, last-seen, and per-device trust evolution.
3. **Revocation UI** — mark device stolen/lost, propagate `RevocationCert`.
4. **"Wipe master from device" UX** — opt-in action for users who want the strict harmony-owner posture (master not re-extractable from device). v1 already handles the resulting degraded state.
5. **Hardware label per device** — free-form vs derived-from-system-info — separate UX brainstorm.
6. **Capabilities display per device** — joins gossip transport.
7. **Cross-device rename** — propagate display-name updates via profile-update gossip.
8. **Track C (inference RPC) integration** — when Track C lands, refactor `identity.rs` + `owner_state.rs` into a unified identity bundle (the eventual approach B from the brainstorm). v1's component boundaries are designed to make this refactor mechanical.
