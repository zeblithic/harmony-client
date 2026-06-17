# ZEB-489 Headless Butler-Rung Deposit→Recover Tooling — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add headless control + observability for the recipient's own **butler** deposit rung (designate a butler; inspect what it's holding), mirroring the merged ZEB-487 relay tooling, so the offline-at-create → deposit → recover DM durability path is testable via a butler.

**Architecture:** Three curated headless RPCs, each routed through an extracted `*_impl(&Mutex<NodeState>)` core the GUI Tauri command also calls (the `connectivity_redeem_invite_iroh_impl` / ZEB-487 pattern): `set_butler_pin` (promote), `get_butler_pin` (new status), `get_butler_held` (new observability over `NodeState.dm_inbox_doc`). A new `butler_held_dto.rs` holds the serialization DTOs + a pure mapper. No transport/verify/deposit/recovery logic changes. No co-located harness scenario; the cross-WAN proof is a playbook Scenario D3.

**Tech Stack:** Rust (tokio, serde, `hex`), Tauri command layer, the curated `serve` RPC registry (`api/rpc.rs`).

**Spec:** `docs/specs/2026-06-17-headless-butler-rung-deposit-recover-tooling-design.md` (commit `5e44a416`).

**Gates (run from `src-tauri/`):**
- `cargo fmt --all -- --check`
- `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
- `cargo nextest run --locked -p harmony-app --lib --features test-fixtures`
- Final sweep (once, before PR): `cargo clippy --locked -p harmony-app --all-targets --features test-fixtures --no-deps -- -D warnings`

---

## File Structure

- **Create** `src-tauri/src/butler_held_dto.rs` — `ButlerHeldEntryDto`, `ButlerHeldResponse`, `ButlerPinStatus`, pure `map_butler_held(&DmInboxDoc)`, unit test. (Mirrors `relay_held_dto.rs`.)
- **Modify** `src-tauri/src/lib.rs` — `pub mod butler_held_dto;`; extract `set_butler_pin_impl`; thin `set_butler_pin` wrapper; add `get_butler_pin_impl` + `get_butler_pin` wrapper; add `get_butler_held_impl` + `get_butler_held` wrapper.
- **Modify** `src-tauri/src/api/rpc.rs` — `SetButlerPinArgs`; three `rpc!` registrations; three names in the allowlist test; `build_registry` doc-comment count `46`→`49`.
- **Modify** `docs/playbooks/e2e-two-agent-suite.md` — append Scenario D3.

---

## Task 1: `butler_held_dto.rs` — DTOs + pure mapper (TDD)

**Files:**
- Create: `src-tauri/src/butler_held_dto.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod butler_held_dto;` next to `pub mod relay_held_dto;` at line ~135)

- [ ] **Step 1: Create the module with the DTOs, mapper, and a failing unit test**

Create `src-tauri/src/butler_held_dto.rs` with the full contents:

```rust
//! ZEB-489: read-only DTOs + mapper for the headless butler observability RPCs
//! (`get_butler_held`, `get_butler_pin`). The butler is the recipient's OWN
//! fleet device, so — unlike the third-party relay — the inbox key exposes the
//! DM `space_id` + `message_cid`, and `ingested_by` is the built-in
//! "recovered/cleared" signal. The sealed/bulky payload (`cidnotify_packet`,
//! `storage_blob`, `invite_packet`) is NEVER exposed.

use crate::dm_inbox_crdt::DmInboxDoc;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ButlerHeldEntryDto {
    pub sender_owner_hex: String,
    pub space_id_hex: String,
    pub message_cid_hex: String,
    pub deposited_at_ms: u64,
    pub deposited_by_device: String,
    pub ingested_by_devices: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ButlerHeldResponse {
    pub held: Vec<ButlerHeldEntryDto>,
}

/// ZEB-489: status of this fleet's pinned butler device.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ButlerPinStatus {
    pub pinned_device_id: Option<String>,
    pub pinned_at_ms: u64,
}

/// Map the butler dm-inbox doc into DTOs. Pure (no NodeState / no I/O) so it is
/// unit-testable in isolation.
pub fn map_butler_held(doc: &DmInboxDoc) -> Vec<ButlerHeldEntryDto> {
    doc.entries
        .iter()
        .map(|(key, e)| {
            // key = "{space_id_hex}:{message_cid_hex}" (DmInboxDoc::key). Both
            // halves are pure hex, so the FIRST ':' is the unambiguous separator.
            let (space_id_hex, message_cid_hex) = key
                .split_once(':')
                .map(|(s, c)| (s.to_string(), c.to_string()))
                .unwrap_or_else(|| (key.clone(), String::new()));
            ButlerHeldEntryDto {
                sender_owner_hex: hex::encode(e.sender_owner),
                space_id_hex,
                message_cid_hex,
                deposited_at_ms: e.deposited_at.wall_ms,
                deposited_by_device: e.deposited_by.clone(),
                ingested_by_devices: e.ingested_by.iter().cloned().collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dm_inbox_crdt::{DmInboxDoc, DmInboxEntry};
    use crate::owner_state_types::Hlc;
    use std::collections::BTreeSet;

    fn entry(sender_owner: u8, dev: &str, ingested: &[&str]) -> DmInboxEntry {
        DmInboxEntry {
            sender_owner: [sender_owner; 16],
            cidnotify_packet: vec![1, 2, 3],
            storage_blob: vec![4, 5, 6],
            invite_packet: None,
            deposited_at: Hlc {
                wall_ms: 4242,
                logical: 0,
                device_id: dev.into(),
            },
            deposited_by: dev.into(),
            ingested_by: ingested.iter().map(|s| s.to_string()).collect::<BTreeSet<String>>(),
        }
    }

    #[test]
    fn maps_inbox_entries_with_key_split_and_ingested_set() {
        let space = [0xAB; 16];
        let cid = [0xCD; 20];
        let space2 = [0x22; 16];
        let cid2 = [0x33; 20];

        let mut doc = DmInboxDoc::default();
        doc.entries
            .insert(DmInboxDoc::key(&space, &cid), entry(0x11, "butlerdev", &["primarydev"]));
        doc.entries
            .insert(DmInboxDoc::key(&space2, &cid2), entry(0x44, "butlerdev", &[]));

        let held = map_butler_held(&doc);
        assert_eq!(held.len(), 2);

        let d = held.iter().find(|d| d.space_id_hex == hex::encode(space)).unwrap();
        assert_eq!(d.sender_owner_hex, hex::encode([0x11u8; 16]));
        assert_eq!(d.space_id_hex, hex::encode(space));
        assert_eq!(d.message_cid_hex, hex::encode(cid));
        assert_eq!(d.deposited_at_ms, 4242);
        assert_eq!(d.deposited_by_device, "butlerdev");
        assert_eq!(d.ingested_by_devices, vec!["primarydev".to_string()]);

        let d2 = held.iter().find(|d| d.space_id_hex == hex::encode(space2)).unwrap();
        assert!(d2.ingested_by_devices.is_empty());

        assert!(map_butler_held(&DmInboxDoc::default()).is_empty());
    }
}
```

- [ ] **Step 2: Register the module**

In `src-tauri/src/lib.rs`, immediately after the existing `pub mod relay_held_dto;` (line ~135), add:

```rust
pub mod butler_held_dto;
```

- [ ] **Step 3: Run the test to verify it passes**

Run (from `src-tauri/`): `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(butler_held)'`
Expected: PASS (`maps_inbox_entries_with_key_split_and_ingested_set`). If `DmInboxEntry` field names differ, fix the fixture to match `src-tauri/src/dm_inbox_crdt.rs:14-41` (do NOT change the struct).

- [ ] **Step 4: fmt + clippy**

Run (from `src-tauri/`):
```bash
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
```
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/butler_held_dto.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-489): butler_held_dto — ButlerHeldEntryDto + map_butler_held"
```

---

## Task 2: Extract `set_butler_pin_impl` (behavior-preserving refactor)

**Files:**
- Modify: `src-tauri/src/lib.rs:43828-43915` (the `set_butler_pin` Tauri command)

The existing command snapshots handles from `NodeState`, computes `now_ms`, calls `set_butler_pin_inner` (untouched, at `lib.rs:43797`), refreshes the sync snapshot, notifies + flushes the engine, and fires the routing-republish trigger. Extract that whole body into a `*_impl(&Mutex<NodeState>, …)` and make the command a one-line delegate (the relay pattern at `lib.rs:44009-44015`).

- [ ] **Step 1: Replace the command with `_impl` + thin wrapper**

In `src-tauri/src/lib.rs`, replace the entire `#[tauri::command] async fn set_butler_pin(…) { … }` (lines ~43827-43915, the `#[tauri::command]` attribute line through the closing `}`) with:

```rust
/// ZEB-489: NodeState-level core of `set_butler_pin`, shared by the GUI Tauri
/// command and the headless RPC. Snapshots the fleet-net handles, applies the
/// pin via `set_butler_pin_inner` (LWW-correct stamp), refreshes the sync
/// snapshot, notifies + flushes the engine, and fires the routing-republish
/// trigger so the pkarr-advertised butler set updates immediately.
pub(crate) async fn set_butler_pin_impl(
    state: &Mutex<NodeState>,
    device_id: Option<String>,
) -> Result<(), String> {
    // Snapshot the handles needed from the NodeState lock — drop the lock
    // before the async doc lock acquisition.
    let (
        fleet_net_doc_arc,
        fleet_net_sync_arc,
        fleet_net_snapshot_arc,
        enrolled,
        self_device_id,
        routing_republish,
    ) = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        let doc = g.fleet_net_doc.clone().ok_or_else(|| {
            "set_butler_pin: fleet-net not running (node not started)".to_string()
        })?;
        let sync = g
            .fleet_net_sync
            .clone()
            .ok_or_else(|| "set_butler_pin: fleet-net engine not running".to_string())?;
        let snapshot = g
            .fleet_net_snapshot
            .clone()
            .ok_or_else(|| "set_butler_pin: fleet-net snapshot not available".to_string())?;
        let enrolled = g.fleet_net_enrolled.clone().unwrap_or_default();
        let self_device_id = g.fleet_net_device_id.clone().unwrap_or_default();
        let routing_republish = g.routing_republish.clone();
        (
            doc,
            sync,
            snapshot,
            enrolled,
            self_device_id,
            routing_republish,
        )
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    set_butler_pin_inner(
        &fleet_net_doc_arc,
        &enrolled,
        device_id,
        &self_device_id,
        now_ms,
    )
    .await?;

    // Refresh the sync snapshot (clone under the doc lock so the RwLock write and
    // the tokio Mutex lock don't nest). Local writes don't fire `on_applied`.
    {
        let cloned_doc = fleet_net_doc_arc.lock().await.clone();
        *fleet_net_snapshot_arc
            .write()
            .unwrap_or_else(|p| p.into_inner()) = cloned_doc;
    }

    fleet_net_sync_arc.notify_dirty();
    if let Err(e) = fleet_net_sync_arc.flush_now().await {
        tracing::warn!(
            error = %e,
            "set_butler_pin: fleet-net flush failed; dirty latch will retry on next cycle"
        );
    }

    // A pin change is a deliberate user action — republish the pkarr routing
    // record immediately (local writes never fire `on_applied`). Fire-and-forget.
    if let Some(rp) = routing_republish {
        rp();
    }
    Ok(())
}

/// ZEB-418 P2 D17: IPC to set or clear the pinned butler device. Thin wrapper
/// over `set_butler_pin_impl`. `device_id`: `Some(hex)` → pin; `null`/`None` →
/// clear. Validates the id is in the current enrolled set.
#[tauri::command]
async fn set_butler_pin(
    device_id: Option<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    set_butler_pin_impl(state.inner(), device_id).await
}
```

- [ ] **Step 2: Build + run the existing butler-pin tests**

Run (from `src-tauri/`): `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(set_butler_pin)'`
Expected: PASS — `set_butler_pin_rejects_unknown_device` (lib.rs:50370), `set_butler_pin_roundtrip` (lib.rs:50408), and `device_vk_hex_round_trips_through_set_butler_pin` still green (they exercise `set_butler_pin_inner`, which is unchanged).

- [ ] **Step 3: fmt + clippy**

Run (from `src-tauri/`):
```bash
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
```
Expected: clean (watch for `await_holding_lock` — the std-`Mutex` guard `g` is dropped at the end of the snapshot block, before any `.await`).

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "refactor(zeb-489): extract set_butler_pin_impl(&Mutex<NodeState>)"
```

---

## Task 3: `get_butler_pin_impl` + thin command

**Files:**
- Modify: `src-tauri/src/lib.rs` (add directly after the `set_butler_pin` wrapper from Task 2)

`FleetNetDoc.pinned: Option<String>` and `FleetNetDoc.pinned_at: Hlc` are the fields `set_butler_pin_inner` writes (`lib.rs:43813-43814`); `Hlc` has a `wall_ms: u64` field. `ButlerPinStatus` is defined in Task 1 (`butler_held_dto.rs`).

- [ ] **Step 1: Add the `_impl` + command**

In `src-tauri/src/lib.rs`, immediately after the `set_butler_pin` Tauri command added in Task 2, insert:

```rust
/// ZEB-489: NodeState-level core of `get_butler_pin`, shared by the GUI command
/// and the headless RPC. Read-only report of this fleet's currently pinned
/// butler device (none → `pinned_device_id: None`).
pub(crate) async fn get_butler_pin_impl(
    state: &Mutex<NodeState>,
) -> Result<crate::butler_held_dto::ButlerPinStatus, String> {
    let doc = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.fleet_net_doc.clone().ok_or_else(|| {
            "get_butler_pin: fleet-net not running (node not started)".to_string()
        })?
    };
    let (pinned_device_id, pinned_at_ms) = {
        let g = doc.lock().await;
        (g.pinned.clone(), g.pinned_at.wall_ms)
    };
    Ok(crate::butler_held_dto::ButlerPinStatus {
        pinned_device_id,
        pinned_at_ms,
    })
}

/// ZEB-489: read-only IPC reporting this fleet's pinned butler device. Thin
/// wrapper over `get_butler_pin_impl`.
#[tauri::command]
async fn get_butler_pin(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<crate::butler_held_dto::ButlerPinStatus, String> {
    get_butler_pin_impl(state.inner()).await
}
```

- [ ] **Step 2: Build + clippy**

Run (from `src-tauri/`):
```bash
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
```
Expected: clean (compiles; `g` std-guard dropped before `doc.lock().await`). If `FleetNetDoc.pinned`/`pinned_at` are named differently, match the actual fields written in `set_butler_pin_inner` (`lib.rs:43813-43814`).

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-489): get_butler_pin_impl — pinned butler status reader"
```

---

## Task 4: `get_butler_held_impl` + thin command

**Files:**
- Modify: `src-tauri/src/lib.rs` (add directly after the `get_butler_pin` command from Task 3)

Mirrors `get_relay_held_impl` (`lib.rs:44052`): snapshot the `dm_inbox_doc` Arc (`NodeState.dm_inbox_doc: Option<Arc<tokio::sync::Mutex<DmInboxDoc>>>`, `lib.rs:1025`), drop the `NodeState` std-guard, then `lock().await` the inbox and map from the guard (sync map → `await_holding_lock`-safe).

- [ ] **Step 1: Add the `_impl` + command**

In `src-tauri/src/lib.rs`, immediately after the `get_butler_pin` Tauri command, insert:

```rust
/// ZEB-489: read-only observability over the butler dm-inbox doc. Reports the
/// deposits this node (as a fleet butler) is holding for offline fleet-mates as
/// routing metadata only — the sealed/bulky payload (cidnotify/storage/invite)
/// is never exposed.
pub(crate) async fn get_butler_held_impl(
    state: &Mutex<NodeState>,
) -> Result<crate::butler_held_dto::ButlerHeldResponse, String> {
    let doc = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.dm_inbox_doc.clone().ok_or_else(|| {
            "get_butler_held: dm-inbox not running (node not started)".to_string()
        })?
    };
    // Map directly from the guard — do NOT clone the whole DmInboxDoc (which
    // would deep-copy every entry's sealed payload). The map is sync (no .await
    // while the lock is held), so the hold is brief.
    let held = {
        let guard = doc.lock().await;
        crate::butler_held_dto::map_butler_held(&guard)
    };
    Ok(crate::butler_held_dto::ButlerHeldResponse { held })
}

/// ZEB-489: read-only IPC over the butler dm-inbox. Thin wrapper over
/// `get_butler_held_impl`.
#[tauri::command]
async fn get_butler_held(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<crate::butler_held_dto::ButlerHeldResponse, String> {
    get_butler_held_impl(state.inner()).await
}
```

- [ ] **Step 2: Build + clippy**

Run (from `src-tauri/`):
```bash
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
```
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-489): get_butler_held_impl — butler dm-inbox observability"
```

---

## Task 5: Register the three RPCs in the curated surface (allowlist TDD)

**Files:**
- Modify: `src-tauri/src/api/rpc.rs` (args struct ~line 218; registrations after the relay block at line 469; allowlist test at lines 860-862; doc-comment at line 260)

The allowlist test `registry_has_exactly_the_curated_v1_surface` asserts the exact command set; adding the 3 expected names first makes it RED (registry still has 46), then the registrations make it GREEN (49).

- [ ] **Step 1: Add the 3 expected names to the allowlist test (RED)**

In `src-tauri/src/api/rpc.rs`, in `registry_has_exactly_the_curated_v1_surface`, immediately after the relay-rung block (after `"get_relay_held",` at line ~862), add:

```rust
            // butler rung (ZEB-489)
            "set_butler_pin",
            "get_butler_pin",
            "get_butler_held",
```

- [ ] **Step 2: Run the allowlist test to verify it FAILS**

Run (from `src-tauri/`): `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(registry_has_exactly_the_curated_v1_surface)'`
Expected: FAIL — `curated v1 surface drifted` (expected has 49, registry has 46).

- [ ] **Step 3: Add the `SetButlerPinArgs` struct**

In `src-tauri/src/api/rpc.rs`, immediately after `GetRelayHeldArgs` (ends at line ~218), add:

```rust
/// ZEB-489: butler-pin control arg shape. `deviceId` omitted/null → clear.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetButlerPinArgs {
    #[serde(default)]
    device_id: Option<String>,
}
```

- [ ] **Step 4: Add the 3 registrations**

In `src-tauri/src/api/rpc.rs`, immediately after the relay-rung `get_relay_held` registration (closes at line ~469, before `// Connectivity.`), add:

```rust

    // Butler rung (ZEB-489).
    rpc!(
        m,
        "set_butler_pin",
        SetButlerPinArgs,
        |state, _sink, a| async move { crate::set_butler_pin_impl(state, a.device_id).await }
    );
    rpc!(
        m,
        "get_butler_pin",
        EmptyArgs,
        |state, _sink, _a| async move { crate::get_butler_pin_impl(state).await }
    );
    rpc!(
        m,
        "get_butler_held",
        EmptyArgs,
        |state, _sink, _a| async move { crate::get_butler_held_impl(state).await }
    );
```

- [ ] **Step 5: Bump the `build_registry` doc-comment count**

In `src-tauri/src/api/rpc.rs`, line ~260, change:

```rust
/// Build the curated v1 RPC surface (46 commands). Every handler calls
```
to:
```rust
/// Build the curated v1 RPC surface (49 commands). Every handler calls
```

- [ ] **Step 6: Run the allowlist test to verify it PASSES (GREEN)**

Run (from `src-tauri/`): `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(registry_has_exactly_the_curated_v1_surface)'`
Expected: PASS.

- [ ] **Step 7: fmt + clippy + full lib test run**

Run (from `src-tauri/`):
```bash
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```
Expected: clean fmt, zero clippy warnings, all lib tests pass.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/api/rpc.rs
git commit -m "feat(zeb-489): register set_butler_pin/get_butler_pin/get_butler_held (49 surface)"
```

---

## Task 6: Playbook Scenario D3 (cross-WAN butler deposit→recover)

**Files:**
- Modify: `docs/playbooks/e2e-two-agent-suite.md` (append after the existing Scenario D2)

- [ ] **Step 1: Append Scenario D3**

Add this section at the end of the Scenario D-series in `docs/playbooks/e2e-two-agent-suite.md`:

```markdown
## Scenario D3 — cross-WAN butler deposit→recover (ZEB-489)

**Roles:** A = **Ildwyn** (sender, 1 device). R = **AVALON** (recipient), running **two local profiles** in one fleet: primary `P` + butler `B2`. (Baseline keeps both recipient devices on AVALON via local pairing — sidesteps cross-WAN pairing. Optional 3-machine variant: run `B2` on Koya.)

Proves the offline-at-create durability path via the recipient's own **butler** rung (ZEB-418), not a community relay (that's D2). HELD ∧ RECV ∧ CLEARED by construction: P is killed before the send, so the tunnel cannot carry it; the deposit lands on B2, and P recovers it on reconnect.

### Setup (AVALON)
1. Mint `P`: `harmony-app --profile p serve --api-port 7421` then `... api mint_owner_identity`; `get_owner_state` → `OWNER-P`.
2. Pair `B2` into P's fleet (second profile, second `serve`): drive the ZEB-446 pairing RPCs — `start_inviter_pairing` on P / `start_joiner_pairing` on B2, `select_pairing_peer`, `confirm_pairing_sas` (SAS match), poll `get_pairing_state` until both report enrolled. `B2` is now a second enrolled device under P's owner.
3. `set_butler_pin '{"deviceId":"<B2 device id>"}'` on P (the device id is B2's 64-hex enrolled key, from P's device view). `get_butler_pin` → confirms `pinnedDeviceId == <B2>`.

### Run
4. **A (Ildwyn):** friend P (`generate_friend_token` → P `redeem_friend_token`; both `list_friends` → active). `add_space` (DM) with P → `SPACE`.
5. **Kill P** (real PID kill — `kill <pid>` / `Stop-Process -Force`, never just close a window).
6. **A:** `send_dm` to P while P is offline → the deposit rung fires after `DEPOSIT_NOACK_WINDOWS=2`; it lands on **B2** (P's online butler).
7. **B2:** poll `get_butler_held` until the entry appears — **HELD** (`senderOwnerHex == OWNER-A`, `spaceIdHex`/`messageCidHex` present, `ingestedByDevices` does NOT yet contain P).
8. **Relaunch P** (same `--profile p serve`). P auto-recovers (startup inbox sweep + fleet merge → `apply_deposited_invite` bootstrap).
9. **B2:** `get_butler_held` now shows `ingestedByDevices` containing P's device id (or the entry GC'd) — **CLEARED**. **P:** `read_dm_thread` shows A's plaintext — **RECV**.

PASS = HELD observed on B2 while P offline, RECV on P after reconnect, CLEARED on B2 after recovery. This is a bring-up/discovery run, not a regression gate — capture both AVALON profiles' logs + the `api` transcript on any failure and file under ZEB-489 / ZEB-321.
```

- [ ] **Step 2: Commit**

```bash
git add docs/playbooks/e2e-two-agent-suite.md
git commit -m "docs(zeb-489): playbook Scenario D3 — cross-WAN butler deposit→recover"
```

---

## Task 7: Final gates + PR

**Files:** none (verification + ship).

- [ ] **Step 1: Full lib gate**

Run (from `src-tauri/`):
```bash
cargo fmt --all -- --check && \
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings && \
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```
Expected: clean fmt, zero clippy warnings, all lib tests pass.

- [ ] **Step 2: Full `--all-targets` clippy sweep (the expensive one — once)**

Run (from `src-tauri/`): `cargo clippy --locked -p harmony-app --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: zero warnings.

- [ ] **Step 3: Push + open the PR**

```bash
git push -u origin zeb-489-headless-butler-rung-deposit-recover
```
PR title: `ZEB-489: headless butler-rung deposit→recover tooling`. Body: summarize the 3 RPCs + the `butler_held_dto` mapper + Scenario D3; reference the spec path; **closes ZEB-489 only** (keep parent ZEB-321 and refs ZEB-418/483/487/488 out of the close-trigger format — Linear auto-close cascade). Then run the bot-review loop (Qodo/CodeAnt first pass → address → one CodeRabbit round) and pushover Jake at the ready-to-merge gate. Do NOT self-merge.

---

## Self-Review

**Spec coverage:** §3.1 → Task 2; §3.2 → Task 3; §3.3 → Task 4; §4 (DTO + mapper) → Task 1; §5 (registration + allowlist + count) → Task 5; §6 (Scenario D3) → Task 6; §7 (testing/gates) → Tasks 1 & 7; §8 (file-touch map) → all tasks. No gaps.

**Type consistency:** `ButlerHeldEntryDto` / `ButlerHeldResponse` / `ButlerPinStatus` / `map_butler_held` defined in Task 1 and referenced verbatim in Tasks 3-5. `set_butler_pin_impl` / `get_butler_pin_impl` / `get_butler_held_impl` signatures match their Task-5 registration call sites (`a.device_id`, no-arg). Surface count `46`→`49` consistent between the allowlist (Task 5 Step 1) and the doc-comment (Task 5 Step 5).

**Placeholder scan:** none — every code step has complete code; every run step has an exact command + expected result.
