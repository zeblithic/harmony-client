# ZEB-668 S2 — Device Revoke IPC + DevicesPanel UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire `RevocationCert` end-to-end — a `revoke_device` IPC (master + self issuers) riding the S1 trust-replication substrate, DevicesPanel Remove affordances with typed confirmation and a Removed-devices section, and a revoked-self terminal state that also fixes the S1 boot trap (revoked device currently classifies as `missing` → un-mintable WelcomeModal dead-end).

**Architecture:** Backend splits a pure `plan_revocation` (guards + cert construction, fully unit-testable from mint fixtures) from `revoke_device_inner` (NodeState snapshot → key load → apply via `mutate_trust_state` → `flush_now` → events → self-halt). Frontend adds a `RemoveDeviceDialog` (reason picker + typed device name), row affordances gated by `canBackUp`, a collapsed Removed section rendered from real `RevocationSet` data, and a `'revoked'` owner-gate state driven by a new `StartNodeResponse.selfRevoked` flag plus the live `device-revoked-self` event.

**Tech Stack:** Rust (tauri, harmony-owner rev 8b870ae0, FleetSyncEngine from S1), Svelte 5 runes + TS, vitest + testing-library, cargo-nextest.

## Global Constraints

- Error prefixes EXACT: `notMaster:` (cert-only device targeting a sibling), `lastDevice:` (refuse revoking the last active device). Additional prefixes introduced here: `invalidReason:`, `badDeviceVk:`, `unknownDevice:`, `noOwner:`.
- `reason` wire values EXACT: `"decommissioned" | "lost" | "compromised"` (spec §3; `RevocationReason::Other` unused in UI).
- Self-revoke ordering (spec §3, load-bearing): sign → add → persist → **publish and flush the trust doc** (`FleetSyncEngine::flush_now`) → only then latch/emit terminal state and stop engines. Initiating device does NOT wait for its own merge callback.
- Keychain access only via injectable seams (ZEB-428): never construct `KeychainStore::new()` in code reachable from tests; this plan introduces `KeychainFactory` for that.
- DTOs `#[serde(rename_all = "camelCase")]`; JS invokes with camelCase args, Rust declares snake_case params.
- Style-token guard: DevicesPanel + dialogs have raw-color budget **0** — `var(--…)` tokens only (`--danger`, `--danger-muted`, `--overlay` available).
- Honesty rule (ZEB-610 §0): confirm-dialog copy enumerates what IS severed (community posting, fleet sync, deposits/relay) and what is NOT (existing DMs/vines/storage records — spec §8); terminal copy says local data is not wiped.
- No persisted-file version bumps; additive fields get `#[serde(default)]`.
- Gates: `scripts/test-select --context task` per task (paste `round=…/bucket=…`), `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` in EVERY round gate, full `--workspace --all-targets --features test-fixtures` nextest + `npx tsc --noEmit` + `npx vitest run` before PR.
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`.

## File Structure

- `src-tauri/src/owner_commands.rs` — parse/label helpers, `plan_revocation`, `KeychainFactory` seam, `get_owner_state_inner`, `revoke_device` command + `_impl` + `_inner`, in-module tests.
- `src-tauri/src/owner_state.rs` — `DeviceView` gains `revoked`/`revoked_at`/`revoked_reason`.
- `src-tauri/src/lib.rs` — `StartNodeResponse.self_revoked`, command registration (:56104 cluster).
- `src-tauri/src/api/rpc.rs` — `RevokeDeviceArgs` + `rpc!` entry (butler-rung cluster ~:920).
- `src/lib/owner-service.ts` — TS `DeviceView` fields, `RevokeReason`, `OwnerService.revoke()`.
- `src/lib/types/onboarding.ts` — `StartNodeResponse.selfRevoked?`.
- `src/lib/owner-gate.ts` — `'revoked'` state in `OwnerIdentityState` + classifier.
- `src/lib/components/RemoveDeviceDialog.svelte` (new) — reason picker + typed-confirm (modeled on `TypeToConfirmDialog.svelte`).
- `src/lib/components/DevicesPanel.svelte` — Remove buttons, Removed section, `owner-devices-updated` listener.
- `src/App.svelte` — `'revoked'` overlay + `device-revoked-self` listener.
- Tests: in-module Rust tests; `src/lib/components/__tests__/RemoveDeviceDialog.test.ts` (new); `DevicesPanel.test.ts`, `owner-service.test.ts`, `owner-gate.test.ts` (new if absent) updates.

---

### Task 1: Backend core — reason parsing + `plan_revocation` (pure)

**Files:**
- Modify: `src-tauri/src/owner_commands.rs` (helpers + `#[cfg(test)]` module)

**Interfaces:**
- Produces: `pub(crate) fn parse_revoke_reason(&str) -> Result<RevocationReason, String>`; `pub(crate) fn revoke_reason_label(&RevocationReason) -> String`; `pub(crate) struct PlannedRevocation { pub cert: RevocationCert, pub is_self: bool }`; `pub(crate) fn plan_revocation(state: &OwnerState, device_signing_key: &SigningKey, master_seed: Option<&[u8; 32]>, device_vk_hex: &str, reason_str: &str, now: u64) -> Result<Option<PlannedRevocation>, String>` (`Ok(None)` = target already revoked, idempotent no-op).
- Consumes: `harmony_owner::certs::{RevocationCert, RevocationReason}`, `harmony_owner::lifecycle::RecoveryArtifact`, `harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS`, `crate::owner_state::device_id_from_signing_key`.

- [ ] **Step 1: Write failing tests** (append to owner_commands.rs `#[cfg(test)] mod tests`; create the module if absent). Fixtures mirror `owner_trust_sync.rs:380` (`mint_owner`, `enroll_via_master`, `SigningKey::generate(&mut rand::rngs::OsRng)`):

```rust
#[cfg(test)]
mod revoke_tests {
    use super::*;
    use harmony_owner::certs::{RevocationCert, RevocationReason};
    use harmony_owner::lifecycle::{enroll_via_master, mint_owner, MintResult};
    use harmony_owner::state::OwnerState;

    // Mint an owner (device A holds the seed) and enroll a second device B.
    // Returns (state, a_sk, seed, b_sk, b_vk_hex).
    fn two_device_fixture() -> (
        OwnerState,
        ed25519_dalek::SigningKey,
        [u8; 32],
        ed25519_dalek::SigningKey,
        String,
    ) {
        let now = 1_700_000_000u64;
        let MintResult {
            mut state,
            recovery_artifact,
            device_signing_key: a_sk,
        } = mint_owner(now).expect("mint");
        let seed = *recovery_artifact.as_bytes();
        let b_sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let b_vk = b_sk.verifying_key().to_bytes();
        let enroll = enroll_via_master(
            &state,
            &recovery_artifact,
            &b_sk,
            harmony_owner::pubkey_bundle::PubKeyBundle::classical_only(b_vk),
            now,
            harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS,
        )
        .expect("enroll");
        state
            .add_enrollment(enroll.enrollment_cert, now, harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS)
            .expect("add enrollment");
        for c in enroll.auto_vouch_certs {
            let _ = state.add_vouching(c);
        }
        (state, a_sk, seed, b_sk, hex::encode(b_vk))
    }

    #[test]
    fn parse_revoke_reason_maps_wire_values() {
        assert_eq!(parse_revoke_reason("decommissioned").unwrap(), RevocationReason::Decommissioned);
        assert_eq!(parse_revoke_reason("lost").unwrap(), RevocationReason::Lost);
        assert_eq!(parse_revoke_reason("compromised").unwrap(), RevocationReason::Compromised);
        let err = parse_revoke_reason("banana").unwrap_err();
        assert!(err.starts_with("invalidReason:"), "{err}");
    }

    #[test]
    fn plan_master_revoke_of_sibling_produces_master_cert() {
        let (state, a_sk, seed, _b_sk, b_vk_hex) = two_device_fixture();
        let now = 1_700_000_100u64;
        let planned = plan_revocation(&state, &a_sk, Some(&seed), &b_vk_hex, "lost", now)
            .expect("plan ok")
            .expect("some plan");
        assert!(!planned.is_self);
        assert!(matches!(
            planned.cert.issuer,
            harmony_owner::certs::RevocationIssuer::Master { .. }
        ));
        assert_eq!(planned.cert.reason, RevocationReason::Lost);
        // The cert must be acceptable to the CRDT.
        let mut s2 = state.clone();
        s2.add_revocation(planned.cert.clone()).expect("cert verifies");
        assert!(s2.is_revoked(planned.cert.target));
    }

    #[test]
    fn plan_self_revoke_produces_self_cert_without_seed() {
        let (state, _a_sk, _seed, b_sk, b_vk_hex) = two_device_fixture();
        let planned = plan_revocation(&state, &b_sk, None, &b_vk_hex, "decommissioned", 1_700_000_100)
            .expect("plan ok")
            .expect("some plan");
        assert!(planned.is_self);
        assert!(matches!(
            planned.cert.issuer,
            harmony_owner::certs::RevocationIssuer::SelfDevice
        ));
        let mut s2 = state.clone();
        s2.add_revocation(planned.cert).expect("self cert verifies");
    }

    #[test]
    fn plan_sibling_revoke_without_seed_is_not_master() {
        let (state, _a, _seed, b_sk, _bhex) = two_device_fixture();
        // Device B (no seed) targets device A: find A's vk from its enrollment.
        let b_id = crate::owner_state::device_id_from_signing_key(&b_sk);
        let a_vk_hex = state
            .enrollments
            .values()
            .find(|c| c.device_id != b_id)
            .map(|c| hex::encode(c.device_pubkeys.classical.ed25519_verify))
            .expect("A enrolled");
        let err = plan_revocation(&state, &b_sk, None, &a_vk_hex, "lost", 1_700_000_100).unwrap_err();
        assert!(err.starts_with("notMaster:"), "{err}");
    }

    #[test]
    fn plan_refuses_revoking_last_active_device() {
        let now = 1_700_000_000u64;
        let MintResult { state, recovery_artifact, device_signing_key } =
            mint_owner(now).expect("mint");
        let seed = *recovery_artifact.as_bytes();
        let self_vk_hex = hex::encode(device_signing_key.verifying_key().to_bytes());
        let err = plan_revocation(&state, &device_signing_key, Some(&seed), &self_vk_hex, "decommissioned", now + 10)
            .unwrap_err();
        assert!(err.starts_with("lastDevice:"), "{err}");
    }

    #[test]
    fn plan_unknown_target_and_bad_hex_error() {
        let (state, a_sk, seed, _b, _bhex) = two_device_fixture();
        let unknown_vk = hex::encode([9u8; 32]);
        let err = plan_revocation(&state, &a_sk, Some(&seed), &unknown_vk, "lost", 1_700_000_100).unwrap_err();
        assert!(err.starts_with("unknownDevice:"), "{err}");
        let err = plan_revocation(&state, &a_sk, Some(&seed), "zz", "lost", 1_700_000_100).unwrap_err();
        assert!(err.starts_with("badDeviceVk:"), "{err}");
    }

    #[test]
    fn plan_already_revoked_target_is_noop() {
        let (mut state, a_sk, seed, _b, b_vk_hex) = two_device_fixture();
        let now = 1_700_000_100u64;
        let planned = plan_revocation(&state, &a_sk, Some(&seed), &b_vk_hex, "lost", now)
            .unwrap()
            .unwrap();
        state.add_revocation(planned.cert).unwrap();
        let second = plan_revocation(&state, &a_sk, Some(&seed), &b_vk_hex, "lost", now + 1).unwrap();
        assert!(second.is_none(), "idempotent no-op");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(revoke_tests)'`
Expected: compile FAIL — `parse_revoke_reason` / `plan_revocation` not found.

- [ ] **Step 3: Implement the helpers** (owner_commands.rs, after `now_unix`):

```rust
use harmony_owner::certs::{RevocationCert, RevocationReason};
use harmony_owner::lifecycle::RecoveryArtifact;
use harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS;

/// Wire → crate reason mapping (spec §3: three UI reasons; Other unused).
pub(crate) fn parse_revoke_reason(reason: &str) -> Result<RevocationReason, String> {
    match reason {
        "decommissioned" => Ok(RevocationReason::Decommissioned),
        "lost" => Ok(RevocationReason::Lost),
        "compromised" => Ok(RevocationReason::Compromised),
        other => Err(format!(
            "invalidReason: expected decommissioned|lost|compromised, got {other:?}"
        )),
    }
}

/// Crate → wire label (for DeviceView.revoked_reason).
pub(crate) fn revoke_reason_label(reason: &RevocationReason) -> String {
    match reason {
        RevocationReason::Decommissioned => "decommissioned".to_string(),
        RevocationReason::Lost => "lost".to_string(),
        RevocationReason::Compromised => "compromised".to_string(),
        RevocationReason::Other(s) => s.clone(),
    }
}

pub(crate) struct PlannedRevocation {
    pub cert: RevocationCert,
    pub is_self: bool,
}

/// Pure revocation planner: validates the request against a trust-state
/// snapshot and constructs the signed cert. No I/O, no locks — the whole
/// guard surface is unit-testable from mint fixtures.
///
/// `Ok(None)` = target already revoked (idempotent no-op).
pub(crate) fn plan_revocation(
    state: &harmony_owner::state::OwnerState,
    device_signing_key: &ed25519_dalek::SigningKey,
    master_seed: Option<&[u8; 32]>,
    device_vk_hex: &str,
    reason_str: &str,
    now: u64,
) -> Result<Option<PlannedRevocation>, String> {
    let reason = parse_revoke_reason(reason_str)?;
    let vk_bytes: [u8; 32] = hex::decode(device_vk_hex)
        .map_err(|e| format!("badDeviceVk: {e}"))?
        .try_into()
        .map_err(|_| "badDeviceVk: expected 32 bytes".to_string())?;
    // Resolve the target through its enrollment — revocation of a device the
    // owner never enrolled is meaningless (and SelfDevice certs cannot verify
    // without the enrolled vk).
    let target = state
        .enrollments
        .values()
        .find(|c| c.device_pubkeys.classical.ed25519_verify == vk_bytes)
        .map(|c| c.device_id)
        .ok_or_else(|| "unknownDevice: no enrollment matches that key".to_string())?;
    if state.is_revoked(target) {
        return Ok(None);
    }
    let active = state.active_devices(now, DEFAULT_ACTIVE_WINDOW_SECS);
    if active.len() == 1 && active[0] == target {
        return Err(
            "lastDevice: refusing to revoke the only active device on this account".to_string(),
        );
    }
    let self_id = crate::owner_state::device_id_from_signing_key(device_signing_key);
    let is_self = target == self_id;
    let cert = if is_self {
        RevocationCert::sign_self(device_signing_key, state.owner_id, target, now, reason)
            .map_err(|e| format!("failed to sign self-revocation: {e}"))?
    } else {
        let seed = master_seed.ok_or_else(|| {
            "notMaster: this device does not hold the master key; only the \
             device with your master key can remove other devices"
                .to_string()
        })?;
        // Transient master reconstruct — same shape as pairing/cert.rs
        // sign_enrollment_for_joiner: derive, sign, drop (RecoveryArtifact
        // zeroizes its seed on drop).
        let artifact = RecoveryArtifact::from_seed(*seed);
        let master_pubkey = artifact.master_pubkey_bundle();
        if master_pubkey.identity_hash() != state.owner_id {
            return Err("master seed does not match this owner".to_string());
        }
        let master_sk = artifact.master_signing_key();
        let cert = RevocationCert::sign_master(&master_sk, master_pubkey, target, now, reason)
            .map_err(|e| format!("failed to sign revocation: {e}"))?;
        drop(master_sk);
        drop(artifact);
        cert
    };
    Ok(Some(PlannedRevocation { cert, is_self }))
}
```

- [ ] **Step 4: Run tests to verify pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(revoke_tests)'`
Expected: 7/7 PASS. (If `enroll_via_master`'s exact arity differs, mirror the working call in `owner_trust_sync.rs` tests verbatim.)

- [ ] **Step 5: Gate + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd .. && scripts/test-select --context task
git add -A && git commit -m "ZEB-668 S2 T1: pure revocation planner (guards + cert construction)"
```

---

### Task 2: `DeviceView` revoked fields + keychain-factory seam for `get_owner_state`

**Files:**
- Modify: `src-tauri/src/owner_state.rs:23-44` (DeviceView)
- Modify: `src-tauri/src/owner_commands.rs` (`build_owner_state_view`, `get_owner_state_impl` → `_inner`)

**Interfaces:**
- Produces: `DeviceView { …, revoked: bool, revoked_at: Option<u64>, revoked_reason: Option<String> }` (camelCase: `revoked`/`revokedAt`/`revokedReason`); `pub(crate) type KeychainFactory = fn() -> Option<crate::identity::KeychainStore>`; `pub(crate) async fn get_owner_state_inner(state: &Mutex<NodeState>, keychain: KeychainFactory) -> Result<Option<OwnerStateView>, String>`.
- Consumes: `revoke_reason_label` (Task 1), `state.revocations.cert_for(device_id)`.

- [ ] **Step 1: Write failing test** (owner_commands.rs `revoke_tests`):

```rust
    #[test]
    fn view_marks_revoked_device_with_reason_and_date() {
        let (mut state, a_sk, seed, _b, b_vk_hex) = two_device_fixture();
        let now = 1_700_000_100u64;
        let planned = plan_revocation(&state, &a_sk, Some(&seed), &b_vk_hex, "lost", now)
            .unwrap()
            .unwrap();
        let target = planned.cert.target;
        state.add_revocation(planned.cert).unwrap();
        let loaded = crate::owner_state::LoadedOwnerState {
            state,
            device_signing_key: a_sk,
            master_seed: Some(zeroize::Zeroizing::new(seed)),
            fleet_keytree: None,
        };
        let view = build_owner_state_view(&loaded, "Test Device".to_string(), None);
        let revoked_row = view
            .devices
            .iter()
            .find(|d| d.device_id == hex::encode(target))
            .expect("revoked device still in view");
        assert!(revoked_row.revoked);
        assert_eq!(revoked_row.revoked_at, Some(now));
        assert_eq!(revoked_row.revoked_reason.as_deref(), Some("lost"));
        let self_row = view.devices.iter().find(|d| d.is_this_device).unwrap();
        assert!(!self_row.revoked);
        assert_eq!(self_row.revoked_at, None);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(view_marks_revoked_device)'`
Expected: compile FAIL — no `revoked` field.

- [ ] **Step 3: Add fields + populate.** In `owner_state.rs` `DeviceView` (after `device_vk_hex`):

```rust
    /// ZEB-668 S2: revocation surface for the Removed-devices section.
    #[serde(default)]
    pub revoked: bool,
    #[serde(default)]
    pub revoked_at: Option<u64>,
    #[serde(default)]
    pub revoked_reason: Option<String>,
```

In `build_owner_state_view`'s per-device map (owner_commands.rs:73-149), alongside the existing field population:

```rust
            let rev_cert = loaded.state.revocations.cert_for(cert.device_id);
            // …inside the DeviceView literal:
                revoked: rev_cert.is_some(),
                revoked_at: rev_cert.map(|c| c.issued_at),
                revoked_reason: rev_cert.map(|c| revoke_reason_label(&c.reason)),
```

Fix any other `DeviceView { … }` literal in the workspace the compiler flags (struct literals must gain the three fields or use `..Default::default()` only if the struct derives it — it does not; spell them out).

- [ ] **Step 4: Keychain-factory seam** (the refactor promised in the PR #451 review reply). In owner_commands.rs:

```rust
/// ZEB-428: keychain construction is injected as a factory so tests pass
/// `|| None` explicitly instead of relying on the constructor's test-build
/// refusal. A fn pointer (not a closure) keeps the blocking-task move 'static.
pub(crate) type KeychainFactory = fn() -> Option<crate::identity::KeychainStore>;

pub(crate) fn prod_keychain() -> Option<crate::identity::KeychainStore> {
    crate::identity::KeychainStore::new().ok()
}
```

Rename the body of `get_owner_state_impl` to `get_owner_state_inner(state, keychain: KeychainFactory)`; replace both `KeychainStore::new().ok()` call sites (`owner_commands.rs:212` and `:246`) with `keychain()` (construction stays inside the `run_blocking` closures — pass the fn pointer in). Re-add a thin `get_owner_state_impl(state)` that calls `get_owner_state_inner(state, prod_keychain)` so the Tauri command and `api/rpc.rs:437` keep compiling unchanged.

- [ ] **Step 5: Run tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(revoke_tests) or test(get_owner_state)'`
Expected: PASS (new test + any existing get_owner_state tests).

- [ ] **Step 6: Gate + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd .. && scripts/test-select --context task
git add -A && git commit -m "ZEB-668 S2 T2: DeviceView revocation fields + keychain-factory seam"
```

---

### Task 3: `revoke_device` command — inner/impl/wrappers, registration, self-halt, `StartNodeResponse.self_revoked`

**Files:**
- Modify: `src-tauri/src/owner_commands.rs` (inner/impl/command + tests)
- Modify: `src-tauri/src/lib.rs` (register at :56104 cluster; `StartNodeResponse` field ~:1927 + construction sites; thread `self_revoked_at_boot` from :3591)
- Modify: `src-tauri/src/api/rpc.rs` (args + `rpc!` near :920)

**Interfaces:**
- Produces: `pub(crate) async fn revoke_device_inner(state: &Mutex<NodeState>, keychain: KeychainFactory, emit: Arc<dyn Fn(&str) + Send + Sync>, device_vk_hex: String, reason: String) -> Result<(), String>`; `pub(crate) async fn revoke_device_impl(state, sink: Arc<dyn NodeEventSink>, device_vk_hex, reason)`; `#[tauri::command] revoke_device`; RPC verb `"revoke_device"`; `StartNodeResponse.self_revoked: bool` (JSON `selfRevoked`).
- Consumes: Task 1 `plan_revocation`, Task 2 `KeychainFactory`/`prod_keychain`, S1 `TrustStateAccess`/`mutate_trust_state`, `FleetSyncEngine::{flush_now, shutdown}`, NodeState fields `owner_trust_doc`/`owner_trust_sync`/`owner_trust_revoked_self`/`sync_engine`/`fleet_net_sync`/`identity_dir`.

- [ ] **Step 1: Write failing integration test** (owner_commands.rs `revoke_tests`; persists a full identity with `save_owner_state_atomic(dir, …, keychain: None)` under `HARMONY_PASSPHRASE` — same fallback the ZEB-428 gates force; nextest = process-per-test so `set_var` is safe):

```rust
    #[tokio::test]
    async fn revoke_device_inner_master_revokes_sibling_file_only() {
        std::env::set_var("HARMONY_PASSPHRASE", "test-passphrase");
        let (state, a_sk, seed, _b, b_vk_hex) = two_device_fixture();
        let dir = tempfile::tempdir().unwrap();
        crate::owner_state::save_owner_state_atomic(dir.path(), &state, &a_sk, Some(&seed), None)
            .expect("persist identity");
        let node = std::sync::Mutex::new(crate::NodeState {
            identity_dir: Some(dir.path().to_path_buf()),
            ..crate::NodeState::default()
        });
        let events: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let ev = events.clone();
        let emit: std::sync::Arc<dyn Fn(&str) + Send + Sync> =
            std::sync::Arc::new(move |name: &str| ev.lock().unwrap().push(name.to_string()));

        revoke_device_inner(&node, || None, emit.clone(), b_vk_hex.clone(), "lost".into())
            .await
            .expect("revoke ok");

        // Durable: the revocation is on disk.
        let disk = crate::owner_state::load_owner_state_cbor(dir.path()).expect("disk state");
        let b_id = disk
            .enrollments
            .values()
            .find(|c| hex::encode(c.device_pubkeys.classical.ed25519_verify) == b_vk_hex)
            .map(|c| c.device_id)
            .unwrap();
        assert!(disk.is_revoked(b_id));
        assert_eq!(events.lock().unwrap().as_slice(), ["owner-devices-updated"]);
        // Sibling revoke must NOT latch the self-revoked flag.
        assert!(!node
            .lock()
            .unwrap()
            .owner_trust_revoked_self
            .load(std::sync::atomic::Ordering::Acquire));

        // Idempotent second call: no error, no duplicate event.
        revoke_device_inner(&node, || None, emit, b_vk_hex, "lost".into())
            .await
            .expect("noop ok");
        assert_eq!(events.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn revoke_device_inner_self_revoke_latches_and_emits_terminal_event() {
        std::env::set_var("HARMONY_PASSPHRASE", "test-passphrase");
        let (state, a_sk, seed, b_sk, _b_vk_hex) = two_device_fixture();
        // Persist device B's identity (no seed) — B removes itself.
        let dir = tempfile::tempdir().unwrap();
        crate::owner_state::save_owner_state_atomic(dir.path(), &state, &b_sk, None, None)
            .expect("persist identity");
        let _ = (a_sk, seed);
        let self_vk_hex = hex::encode(b_sk.verifying_key().to_bytes());
        let node = std::sync::Mutex::new(crate::NodeState {
            identity_dir: Some(dir.path().to_path_buf()),
            ..crate::NodeState::default()
        });
        let events: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let ev = events.clone();
        let emit: std::sync::Arc<dyn Fn(&str) + Send + Sync> =
            std::sync::Arc::new(move |name: &str| ev.lock().unwrap().push(name.to_string()));

        revoke_device_inner(&node, || None, emit, self_vk_hex, "decommissioned".into())
            .await
            .expect("self-revoke ok");

        let disk = crate::owner_state::load_owner_state_cbor(dir.path()).unwrap();
        assert!(disk.is_revoked(crate::owner_state::device_id_from_signing_key(&b_sk)));
        assert!(node
            .lock()
            .unwrap()
            .owner_trust_revoked_self
            .load(std::sync::atomic::Ordering::Acquire));
        let got = events.lock().unwrap().clone();
        assert_eq!(got, ["owner-devices-updated", "device-revoked-self"]);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(revoke_device_inner)'`
Expected: compile FAIL — `revoke_device_inner` not found.

- [ ] **Step 3: Implement inner + impl + command** (owner_commands.rs). Flow comments carry the spec-ordering rationale:

```rust
/// ZEB-668 S2. Self-revoke ordering is load-bearing (spec §3): sign → add →
/// persist → publish+flush → terminal state + engine halt. The initiating
/// device must not wait for its own merge callback.
pub(crate) async fn revoke_device_inner(
    state: &std::sync::Mutex<crate::NodeState>,
    keychain: KeychainFactory,
    emit: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    device_vk_hex: String,
    reason: String,
) -> Result<(), String> {
    // Snapshot handles under the std lock; drop before any await.
    let (trust_doc, trust_engine, identity_dir, revoked_flag, owner_sync, fleet_net) = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.owner_trust_doc.clone(),
            g.owner_trust_sync.clone(),
            g.identity_dir.clone(),
            std::sync::Arc::clone(&g.owner_trust_revoked_self),
            g.sync_engine.clone(),
            g.fleet_net_sync.clone(),
        )
    };
    let dir = identity_dir.ok_or_else(|| "noOwner: identity dir not resolved".to_string())?;

    // Keys always come from disk/keychain (device sk + optional master seed).
    let dir_for_load = dir.clone();
    let loaded = crate::identity_commands::run_blocking(move || {
        let _guard = OWNER_STATE_WRITE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::owner_state::load_owner_state(&dir_for_load, keychain())
    })
    .await??
    .ok_or_else(|| "noOwner: no owner identity on this device".to_string())?;

    // Guard against the freshest trust state we have: resident doc when the
    // node is running, disk snapshot otherwise.
    let trust_snapshot = match &trust_doc {
        Some(doc) => doc.lock().await.clone(),
        None => loaded.state.clone(),
    };
    let planned = match plan_revocation(
        &trust_snapshot,
        &loaded.device_signing_key,
        loaded.master_seed.as_ref().map(|z| &**z),
        &device_vk_hex,
        &reason,
        now_unix(),
    )? {
        Some(p) => p,
        None => return Ok(()), // already revoked — idempotent
    };
    let is_self = planned.is_self;
    let cert = planned.cert;

    // Apply through the S1 substrate: resident doc + notify_dirty, or the
    // locked load→mutate→save path when the node is down.
    let access = match (&trust_doc, &trust_engine) {
        (Some(doc), Some(engine)) => crate::owner_trust_sync::TrustStateAccess::Resident {
            doc: std::sync::Arc::clone(doc),
            engine: std::sync::Arc::clone(engine),
        },
        _ => crate::owner_trust_sync::TrustStateAccess::FileOnly {
            identity_dir: dir.clone(),
        },
    };
    crate::owner_trust_sync::mutate_trust_state(access, move |s| s.add_revocation(cert))
        .await?
        .map_err(|e| format!("revocation rejected: {e}"))?;

    // Durability + propagation. Resident: force the publish+persist now.
    // Sibling revokes tolerate a flush failure (dirty latch retries);
    // a SELF revoke must not reach the terminal state unpublished.
    if let Some(engine) = &trust_engine {
        if let Err(e) = engine.flush_now().await {
            if is_self {
                return Err(format!(
                    "self-revocation staged but not yet published (will retry): {e}"
                ));
            }
            tracing::warn!(error = %e, "revoke_device: trust flush failed; dirty latch will retry");
        }
    }

    emit("owner-devices-updated");

    if is_self {
        // Terminal state: latch once, tell the UI, then stop fleet engines
        // (hygiene — enforcement is receiver-side; matches the S1 halt set).
        if !revoked_flag.swap(true, std::sync::atomic::Ordering::AcqRel) {
            emit("device-revoked-self");
        }
        if let Some(engine) = owner_sync {
            if let Err(e) = engine.shutdown().await {
                tracing::error!(error = %e, "revoke_device: owner-state engine shutdown failed");
            }
        }
        if let Some(engine) = fleet_net {
            if let Err(e) = engine.shutdown().await {
                tracing::error!(error = %e, "revoke_device: fleet-net engine shutdown failed");
            }
        }
        if let Some(engine) = trust_engine {
            if let Err(e) = engine.shutdown().await {
                tracing::error!(error = %e, "revoke_device: trust engine shutdown failed");
            }
        }
    }
    Ok(())
}

pub(crate) async fn revoke_device_impl(
    state: &std::sync::Mutex<crate::NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    device_vk_hex: String,
    reason: String,
) -> Result<(), String> {
    let emit: std::sync::Arc<dyn Fn(&str) + Send + Sync> = std::sync::Arc::new(move |name: &str| {
        crate::node_event_sink::emit_ser(&*sink, name, &serde_json::Value::Null);
    });
    revoke_device_inner(state, prod_keychain, emit, device_vk_hex, reason).await
}

#[tauri::command]
pub async fn revoke_device(
    app: tauri::AppHandle,
    device_vk_hex: String,
    reason: String,
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
) -> Result<(), String> {
    revoke_device_impl(state.inner(), std::sync::Arc::new(app), device_vk_hex, reason).await
}
```

Note the `.await??` double-unwrap (run_blocking Result + load Result) and that `mutate_trust_state` returns `Result<R, String>` with `R = Result<(), OwnerError>`. If `NodeEventSink` lacks `Send + Sync` supertraits the `Arc<dyn …>` in rpc.rs proves the bound exists — mirror whatever `api/rpc.rs:37` compiles with.

- [ ] **Step 4: Register — Tauri + RPC + StartNodeResponse.**
  1. lib.rs `generate_handler!` owner cluster (after `owner_commands::restore_owner_mnemonic_from_words,` at :56104): add `owner_commands::revoke_device,`. Check the second test-only handler list at :56306 — if owner commands appear there, add it there too.
  2. api/rpc.rs, butler-rung cluster (~:920), plus args struct beside `SetButlerPinArgs` (:368):

```rust
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevokeDeviceArgs {
    device_vk_hex: String,
    reason: String,
}
```

```rust
    // Device management (ZEB-668 S2).
    rpc!(
        m,
        "revoke_device",
        RevokeDeviceArgs,
        |state, sink, a| async move {
            crate::owner_commands::revoke_device_impl(state, sink, a.device_vk_hex, a.reason).await
        }
    );
```

  3. `StartNodeResponse` (lib.rs ~:1927): add

```rust
    /// ZEB-668 S2: true when this device's own enrollment is revoked in the
    /// trust state — the frontend renders the terminal "removed" screen
    /// instead of the mint gate (has_owner_identity is false in this case,
    /// which would otherwise misclassify as first-run `missing`).
    #[serde(default)]
    pub self_revoked: bool,
```

Thread `self_revoked_at_boot` (computed at lib.rs:3591) into every `StartNodeResponse { … }` construction site the compiler flags (:10521, :10549, :11155 and the test literals near :63845) — `self_revoked: self_revoked_at_boot` where in scope, `false` in the error-path/test literals. Extend the camelCase serialization test at :63877 to pin `"selfRevoked"`.

- [ ] **Step 5: Run tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(revoke)'`
Expected: all revoke tests PASS.

- [ ] **Step 6: Gate + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd .. && scripts/test-select --context task
git add -A && git commit -m "ZEB-668 S2 T3: revoke_device IPC (master+self), self-halt ordering, selfRevoked boot flag"
```

---

### Task 4: Frontend service + owner-gate

**Files:**
- Modify: `src/lib/owner-service.ts` (DeviceView fields, `RevokeReason`, `revoke()`)
- Modify: `src/lib/types/onboarding.ts` (`selfRevoked?`)
- Modify: `src/lib/owner-gate.ts` (+ `'revoked'`)
- Test: `src/lib/owner-service.test.ts`, `src/lib/owner-gate.test.ts` (create if absent)

**Interfaces:**
- Produces: `DeviceView { …, revoked: boolean, revokedAt: number | null, revokedReason: string | null }`; `export type RevokeReason = 'decommissioned' | 'lost' | 'compromised'`; `OwnerService.revoke(deviceVkHex: string, reason: RevokeReason): Promise<void>`; `OwnerIdentityState` includes `'revoked'`; `classifyOwnerIdentity` returns `'revoked'` when `resp.selfRevoked === true`.

- [ ] **Step 1: Write failing tests.** In `owner-service.test.ts` (mirror the existing invoke-args-verbatim case):

```ts
it('revoke passes camelCase args verbatim', async () => {
  mockedInvoke.mockResolvedValueOnce(undefined);
  const svc = new OwnerService();
  await svc.revoke('ab'.repeat(32), 'lost');
  expect(mockedInvoke).toHaveBeenCalledWith('revoke_device', {
    deviceVkHex: 'ab'.repeat(32),
    reason: 'lost',
  });
});
```

In `owner-gate.test.ts` (create with vitest imports if absent):

```ts
import { describe, expect, it } from 'vitest';
import { classifyOwnerIdentity } from './owner-gate';

describe('classifyOwnerIdentity', () => {
  it('classifies selfRevoked before present/missing', () => {
    expect(
      classifyOwnerIdentity({ hasOwnerIdentity: false, selfRevoked: true } as never, false),
    ).toBe('revoked');
  });
  it('error still wins over revoked-shaped nulls', () => {
    expect(classifyOwnerIdentity(null, true)).toBe('error');
  });
  it('existing states unchanged', () => {
    expect(classifyOwnerIdentity({ hasOwnerIdentity: true } as never, false)).toBe('present');
    expect(classifyOwnerIdentity({ hasOwnerIdentity: false } as never, false)).toBe('missing');
  });
});
```

- [ ] **Step 2: Run to verify failure**: `npx vitest run src/lib/owner-service.test.ts src/lib/owner-gate.test.ts` — FAIL (no `revoke`, no `'revoked'`).

- [ ] **Step 3: Implement.** owner-service.ts — extend `DeviceView`:

```ts
  revoked: boolean;
  revokedAt: number | null;
  revokedReason: string | null;
```

and the service:

```ts
export type RevokeReason = 'decommissioned' | 'lost' | 'compromised';

  /** ZEB-668 S2. Errors surface backend prefixes (notMaster:, lastDevice:). */
  async revoke(deviceVkHex: string, reason: RevokeReason): Promise<void> {
    await invoke('revoke_device', { deviceVkHex, reason });
  }
```

onboarding.ts `StartNodeResponse`: add `selfRevoked?: boolean;`. owner-gate.ts:

```ts
export type OwnerIdentityState = 'unknown' | 'present' | 'missing' | 'error' | 'revoked';
```

and in `classifyOwnerIdentity`, before the present/missing return:

```ts
  if (resp.selfRevoked === true) return 'revoked';
```

(Extend the doc comment: `revoked` — start_node succeeded but this device's enrollment is revoked; terminal screen, never the mint gate.)

- [ ] **Step 4: Run tests**: `npx vitest run src/lib/owner-service.test.ts src/lib/owner-gate.test.ts` — PASS. Also `npx tsc --noEmit` (DeviceView consumers may need the new fields in test fixtures — update `DevicesPanel.test.ts` fixture objects with `revoked: false, revokedAt: null, revokedReason: null`).

- [ ] **Step 5: Commit**: `git add -A && git commit -m "ZEB-668 S2 T4: revoke service method, DeviceView revocation fields, 'revoked' owner-gate state"`

---

### Task 5: `RemoveDeviceDialog` + DevicesPanel wiring

**Files:**
- Create: `src/lib/components/RemoveDeviceDialog.svelte`
- Modify: `src/lib/components/DevicesPanel.svelte`
- Test: `src/lib/components/__tests__/RemoveDeviceDialog.test.ts` (new), `src/lib/components/__tests__/DevicesPanel.test.ts`

**Interfaces:**
- Produces: `RemoveDeviceDialog` props `{ deviceName: string, isSelf: boolean, isSeedHolder: boolean, busy: boolean, error: string | null, onConfirm: (reason: RevokeReason) => void, onCancel: () => void }`.
- Consumes: `OwnerService.revoke` (Task 4), `Modal` (same import as `TypeToConfirmDialog.svelte`), `listen('owner-devices-updated')`.

Design notes (from spec §3 + §8, honesty rule):
- The dialog is `TypeToConfirmDialog`'s structure (Modal + typed input + destructive confirm, exact-match `typed === deviceName`) with a reason radio group added — `TypeToConfirmDialog` itself has no content slot, so this is a sibling component, deliberately reusing its classes/tokens.
- Copy block (static, in the dialog):
  - severed: "Removing this device cuts it off from posting in your communities, fleet sync between your devices, and message deposits and relay."
  - not severed: "Existing direct-message and feed publishing from that device is not blocked yet."
  - `isSelf && isSeedHolder` extra line: "This device holds your master key. Afterwards you will need your recovery phrase to manage devices."
  - `isSelf` extra line: "Your local data on this device is not deleted."
- Reasons render as three radios with labels: "Decommissioned — retiring this device deliberately", "Lost — I can't find this device", "Compromised — someone else may control this device". Default `decommissioned`.
- Error display: map backend prefixes to friendly text (`notMaster:` → "Only the device holding your master key can remove other devices."; `lastDevice:` → "You can't remove your only active device."; otherwise show the raw message).

- [ ] **Step 1: Write failing dialog tests** (`RemoveDeviceDialog.test.ts`, cribbing `TypeToConfirmDialog.test.ts` setup):

```ts
import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, expect, it, vi } from 'vitest';
import RemoveDeviceDialog from '../RemoveDeviceDialog.svelte';

function props(overrides: Record<string, unknown> = {}) {
  return {
    deviceName: 'Study Mac',
    isSelf: false,
    isSeedHolder: false,
    busy: false,
    error: null,
    onConfirm: vi.fn(),
    onCancel: vi.fn(),
    ...overrides,
  };
}

describe('RemoveDeviceDialog', () => {
  it('disables confirm until the exact device name is typed', async () => {
    const p = props();
    render(RemoveDeviceDialog, { props: p });
    const confirm = screen.getByRole('button', { name: /remove device/i });
    expect(confirm).toBeDisabled();
    await fireEvent.input(screen.getByRole('textbox'), { target: { value: 'Study Mac' } });
    expect(confirm).toBeEnabled();
    await fireEvent.click(confirm);
    expect(p.onConfirm).toHaveBeenCalledWith('decommissioned');
  });

  it('passes the selected reason to onConfirm', async () => {
    const p = props();
    render(RemoveDeviceDialog, { props: p });
    await fireEvent.click(screen.getByRole('radio', { name: /compromised/i }));
    await fireEvent.input(screen.getByRole('textbox'), { target: { value: 'Study Mac' } });
    await fireEvent.click(screen.getByRole('button', { name: /remove device/i }));
    expect(p.onConfirm).toHaveBeenCalledWith('compromised');
  });

  it('shows the seed-holder warning only for the seed-holding self device', () => {
    render(RemoveDeviceDialog, { props: props({ isSelf: true, isSeedHolder: true }) });
    expect(screen.getByText(/holds your master key/i)).toBeInTheDocument();
  });

  it('maps notMaster errors to friendly copy', () => {
    render(RemoveDeviceDialog, { props: props({ error: 'notMaster: nope' }) });
    expect(screen.getByText(/only the device holding your master key/i)).toBeInTheDocument();
  });
});
```

- [ ] **Step 2: Run to verify failure**: `npx vitest run src/lib/components/__tests__/RemoveDeviceDialog.test.ts` — FAIL (component missing).

- [ ] **Step 3: Build the dialog.** Structure (full file; styles reuse the token set `TypeToConfirmDialog.svelte` uses — copy its `<style>` classes and extend):

```svelte
<script lang="ts">
  import Modal from './Modal.svelte';
  import type { RevokeReason } from '../owner-service';

  let {
    deviceName,
    isSelf,
    isSeedHolder,
    busy,
    error,
    onConfirm,
    onCancel,
  }: {
    deviceName: string;
    isSelf: boolean;
    isSeedHolder: boolean;
    busy: boolean;
    error: string | null;
    onConfirm: (reason: RevokeReason) => void;
    onCancel: () => void;
  } = $props();

  let typed = $state('');
  let reason = $state<RevokeReason>('decommissioned');
  let matches = $derived(typed === deviceName);

  const friendlyError = $derived.by(() => {
    if (!error) return null;
    if (error.startsWith('notMaster:'))
      return 'Only the device holding your master key can remove other devices.';
    if (error.startsWith('lastDevice:')) return "You can't remove your only active device.";
    return error;
  });
</script>
```

(markup: Modal with title `Remove {deviceName}?`; the severed/not-severed copy paragraphs; the two conditional `isSelf`/`isSeedHolder` lines; radio group `role=radiogroup` with the three reasons; labeled text input "Type the device name to confirm"; Cancel + destructive confirm button labeled "Remove device" `disabled={!matches || busy}` calling `onConfirm(reason)`; `{#if friendlyError}<p class="error">{friendlyError}</p>{/if}`. Match `Modal` usage — `onClose={onCancel}` or whatever prop `TypeToConfirmDialog.svelte` passes — copy it exactly.)

- [ ] **Step 4: Run dialog tests**: PASS expected.

- [ ] **Step 5: Wire DevicesPanel.** Changes to `DevicesPanel.svelte`:
  1. Derived splits: `const activeDevices = $derived((state?.devices ?? []).filter((d) => !d.revoked));` and `removedDevices` for `d.revoked`, sorted by `revokedAt` desc. The `{#each}` at :494 iterates `activeDevices`.
  2. Row affordance: in the row actions, `{#if device.isThisDevice}` → button "Remove this device"; `{:else if state.canBackUp}` → button "Remove…" (honesty rule: sibling affordance renders only where the IPC can succeed). Both set `removeTarget = device`.
  3. Dialog state + handler (mirrors the butler-pin in-flight shape at :189):

```ts
let removeTarget = $state<DeviceView | null>(null);
let removeInFlight = $state(false);
let removeError = $state<string | null>(null);

async function handleRemoveConfirm(reason: RevokeReason) {
  if (!removeTarget || removeInFlight) return;
  removeError = null;
  removeInFlight = true;
  try {
    await svc.revoke(removeTarget.deviceVkHex, reason);
    await svc.refresh();
    removeTarget = null;
  } catch (e) {
    removeError = extractError(e);
  } finally {
    removeInFlight = false;
  }
}
```

  4. Render `{#if removeTarget}<RemoveDeviceDialog deviceName={removeTarget.displayName} isSelf={removeTarget.isThisDevice} isSeedHolder={removeTarget.isThisDevice && state?.canBackUp === true} busy={removeInFlight} error={removeError} onConfirm={handleRemoveConfirm} onCancel={() => { removeTarget = null; removeError = null; }} />{/if}`.
  5. Removed section (after the devices list): only when `removedDevices.length > 0` — disclosure button `aria-expanded={removedOpen}` labeled `Removed devices ({removedDevices.length})`, toggling a list of rows: `displayName`, reason label, `removed {formatEnrolledAt(device.revokedAt ?? 0)}`. No butler checkbox / rename on removed rows.
  6. Live refresh listener (simple variant of the `PendingAdminProposalsPanel.svelte` pattern): in `onMount`, `listen('owner-devices-updated', () => { void svc.refresh(); })`, store the unlisten, call it in `onDestroy` (guard the resolved-after-unmount race with a `cancelled` flag).
  7. Styles: tokens only — removed rows get `opacity: 0.75`, reason chip `color: var(--danger)`, section spacing consistent with `.devices-list`.

- [ ] **Step 6: Update DevicesPanel tests.**
  - REWRITE `'shows a self-sovereign badge but no rotation/revoke/danger chrome'` (:132): the honesty premise changed — backing IPC now exists. Replace with: seed-holder fixture (`canBackUp: true`) shows "Remove…" on sibling rows and "Remove this device" on self; non-seed fixture (`canBackUp: false`) hides sibling "Remove…" but keeps self-remove.
  - New: revoke flow test (crib butler-toggle at :1032): click Remove on a sibling → dialog appears → type name, confirm → `invoke` called with `('revoke_device', { deviceVkHex, reason: 'decommissioned' })` → `get_owner_state` re-fetched.
  - New: removed-section test: fixture with one `revoked: true, revokedAt: 1_700_000_000, revokedReason: 'lost'` device renders the disclosure with count 1, expands to show name + reason; revoked device absent from the active list.
  - Update all device fixtures with the three new fields.

- [ ] **Step 7: Run**: `npx vitest run src/lib/components/__tests__/RemoveDeviceDialog.test.ts src/lib/components/__tests__/DevicesPanel.test.ts src/style-token-guard.test.ts` — PASS; `npx tsc --noEmit` clean.

- [ ] **Step 8: Commit**: `git add -A && git commit -m "ZEB-668 S2 T5: RemoveDeviceDialog + DevicesPanel remove/removed-section wiring"`

---

### Task 6: App terminal state + full gates

**Files:**
- Modify: `src/App.svelte` (~:912 state, ~:2058 classify path, ~:4364 overlay block, listener near other boot `listen` calls)

**Interfaces:**
- Consumes: `'revoked'` owner-gate state (Task 4), `device-revoked-self` event (backend, S1+T3).

- [ ] **Step 1: Terminal overlay.** Clone the startup-error block (App.svelte:4364-4395) as a sibling:

```svelte
{#if ownerIdentityState === 'revoked'}
  <div class="modal-overlay" data-testid="device-revoked-backdrop" role="presentation">
    <div
      bind:this={revokedModalEl}
      class="modal-content startup-error-modal"
      data-testid="device-revoked-modal"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="device-revoked-title"
      tabindex="-1"
    >
      <h2 id="device-revoked-title">This device was removed from your account</h2>
      <p>
        Another of your devices (or this one) revoked this device's enrollment. It no longer
        syncs with your other devices or posts to your communities as you.
      </p>
      <p>
        Your local data on this device has not been deleted. To use Harmony here again, pair
        this device from another of your devices, or restore an identity from a recovery
        phrase.
      </p>
    </div>
  </div>
{/if}
```

Add `let revokedModalEl = $state<HTMLDivElement | null>(null);` beside `startupErrorModalEl` and extend the focus-trap effect (App.svelte:916-924) to also trap when `ownerIdentityState === 'revoked'` (same `trapFocus` call with `revokedModalEl`).

- [ ] **Step 2: Live listener.** Where App.svelte registers boot listeners, add:

```ts
listen('device-revoked-self', () => {
  ownerIdentityState = 'revoked';
})
```

with the same unlisten bookkeeping as the adjacent listeners. The boot path needs no extra code — `classifyOwnerIdentity` (Task 4) already maps `selfRevoked: true` → `'revoked'`. Verify no `ownerIdentityState === 'present'`-gated code assumes only four states (grep `ownerIdentityState` in App.svelte; `'revoked'` must not fall into the mint-gate branch at :2098 — it returns before, since classify runs once at :2058).

- [ ] **Step 3: Full gate sweep (pre-PR):**

```bash
npx tsc --noEmit && npx vitest run
cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: all clean/green (paste totals).

- [ ] **Step 4: Commit + PR**

```bash
git add -A && git commit -m "ZEB-668 S2 T6: revoked-self terminal overlay + boot classification"
git push -u origin zeb-668-s2-revoke
```

Open the PR (body: what/why, S1-boot-trap fix called out, honesty-ledger copy decisions, gates, deviations), fire `@coderabbitai review` once at open, converge per standing loop.

---

## Self-Review (done at write time)

- **Spec coverage:** §3 IPC (T1+T3), issuer selection + `notMaster:` (T1), `lastDevice:` guard (T1), self-revoke ordering incl. flush + halt (T3), retire-announce queue hand-off — S3's slice, explicitly NOT here (spec assigns it to the retire-announce slice); DevicesPanel affordances + honesty gating (T5), TypeToConfirmDialog-style typed confirm + reason picker (T5 — sibling component since the donor has no content slot; deviation documented), Removed section from real RevocationSet data (T2+T5), DeviceView additions (T2), terminal state (T6) + the S1 boot-trap fix via `selfRevoked` (T3+T4; not in spec text — discovered during planning, spec's terminal-state intent requires it).
- **Placeholder scan:** clean — every code step carries real code; the two "mirror the working call / copy it exactly" notes point at specific existing lines, not unwritten designs.
- **Type consistency:** `RevokeReason` string union == backend `parse_revoke_reason` wire values; `revoked`/`revokedAt`/`revokedReason` camelCase == serde rename; `revoke_device` args `deviceVkHex`/`reason` == snake_case params.
