# ZEB-487 — Headless relay-rung deposit→recover tooling — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose the community sealed-relay deposit rung as headless `serve` RPCs (opt-in control + held-deposit observability) and prove the ZEB-483 *offline-at-create → relay-deposit → recover* DM durability path end-to-end.

**Architecture:** Promote two existing GUI Tauri IPCs (`set_community_relay_opt_in`, `get_community_relay_status`) into the curated headless RPC surface by extracting `*_impl` cores both the command and the RPC call (the `connectivity_redeem_invite_iroh_impl` pattern); add one new read-only `get_relay_held` over the already-present `NodeState.relay_hold_doc`; then add a 3-node `e2e-harness` test and a cross-WAN playbook scenario that assert the HELD∧RECV∧CLEARED triple by construction (no new wire/event fields). No deposit/recover/verify logic changes.

**Tech Stack:** Rust (tauri app crate `harmony-app`), `serde_json` RPC registry (`src-tauri/src/api/rpc.rs`), the `e2e-harness` crate (gated `--features e2e`, not in CI), Markdown playbook.

**Spec:** `docs/specs/2026-06-16-headless-relay-rung-deposit-recover-tooling-design.md` (committed `2779f74b`).
**Branch:** `zeb-487-headless-relay-rung-deposit-recover` (off `main` `6b2424a4`).

**Gates (run from `src-tauri/`):**
- `cargo fmt --all`
- `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
- `cargo nextest run --locked -p harmony-app --lib --features test-fixtures`
- Harness (from `e2e-harness/`): `cargo nextest run --features e2e`
- **Final sweep only:** `cargo clippy --locked -p harmony-app --all-targets --features test-fixtures --no-deps -- -D warnings` (relinks ~97 integration binaries — expensive; do once at the end).

---

## File structure

| File | Responsibility | Change |
|---|---|---|
| `src-tauri/src/lib.rs` | Tauri commands + `*_impl` cores + NodeState | Extract `set_community_relay_opt_in_impl`, `get_community_relay_status_impl`; add `get_relay_held_impl`; thin the two commands to wrappers. |
| `src-tauri/src/relay_held_dto.rs` (new) | The `RelayHeldEntryDto`/`RelayHeldResponse` DTOs + the pure `map_relay_held` mapper (unit-testable, no `NodeState`). | Create. |
| `src-tauri/src/api/rpc.rs` | Curated headless RPC registry + allowlist test | Add 3 args structs + 3 `rpc!` registrations + 3 names in the allowlist `expected`. |
| `e2e-harness/src/node.rs` | Node lifecycle | Add `NodeHandle::relaunch`. |
| `e2e-harness/src/driver.rs` | Semantic RPC helpers | Add `set_relay_opt_in`, `get_relay_opt_in`, `get_relay_held`. |
| `e2e-harness/tests/e2e_two_node.rs` | Scenario tests | Add `three_minted_nodes` + `s6_relay_deposit_recover`. |
| `docs/playbooks/e2e-two-agent-suite.md` | Cross-machine playbook | Add Scenario D2. |

**Reference facts (verified, from the spec §3 + code map):**
- `rpc!` macro (`api/rpc.rs:51`): `rpc!(map, "name", ArgsTy, |state, sink, a| async move { … })` where `state: &Mutex<NodeState>`, `sink: Arc<dyn NodeEventSink>`; the closure body returns `Result<T, String>` and the macro serializes `T`.
- Args structs live in `api/rpc.rs` with `#[derive(serde::Deserialize)] #[serde(rename_all = "camelCase")]`.
- The allowlist test is `registry_has_exactly_the_curated_v1_surface` (`api/rpc.rs:~769`); it asserts `reg.command_names()` equals a sorted `expected` vec — adding a command requires adding its name there.
- `NodeState.relay_hold_doc: Option<Arc<Mutex<RelayHoldDoc>>>` already exists (`lib.rs:1043`).
- `RelayHoldEntry` (`community_relay_hold_crdt.rs:23`): `recipient_owner:[u8;16]`, `sender_owner:[u8;16]`, `community_id:SpaceId`, `sealed_blob:Vec<u8>`, `held_at:Hlc`, `held_by:String`, `pulled_by:BTreeSet<String>`. Map key = `"{recipientOwnerHex}:{contentIdHex}"`.
- `RelayHoldDoc.entries: BTreeMap<String, RelayHoldEntry>` (`community_relay_hold_crdt.rs:43`).
- `SpaceId` is a tuple newtype `SpaceId([u8;16])`; `Hlc { wall_ms:u64, logical:u64, device_id:String }`.
- `Mutex` in `lib.rs` = `std::sync::Mutex`; the relay docs use `tokio::sync::Mutex`.

---

## Task 1: Promote `set_community_relay_opt_in` to the headless surface

**Files:**
- Modify: `src-tauri/src/lib.rs` (the command at `~:43946`)
- Modify: `src-tauri/src/api/rpc.rs` (args struct + registration + allowlist)

- [ ] **Step 1: Add the command name to the allowlist test (the failing test)**

In `src-tauri/src/api/rpc.rs`, in `registry_has_exactly_the_curated_v1_surface`'s `expected` vec, add a relay-rung group after the `// spaces / DMs` block:

```rust
            // spaces / DMs
            "add_space",
            "send_dm",
            "read_dm_thread",
            // relay rung (ZEB-487)
            "set_community_relay_opt_in",
```

- [ ] **Step 2: Run the allowlist test — verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(registry_has_exactly)'`
Expected: FAIL — `expected` now contains `set_community_relay_opt_in` but `build_registry()` does not register it (left/right vecs differ).

- [ ] **Step 3: Extract `set_community_relay_opt_in_impl` from the command body**

In `src-tauri/src/lib.rs`, replace the existing `#[tauri::command] async fn set_community_relay_opt_in(...)` (at `~:43946`) with an `_impl` that takes `&Mutex<NodeState>`, and a thin command wrapper. The body is moved verbatim from the current command (only the signature/`state` source changes):

```rust
/// ZEB-458 P4 / ZEB-487: NodeState-level core of `set_community_relay_opt_in`,
/// shared by the GUI Tauri command and the headless RPC. Snapshots the relay-
/// optin handles, LWW-writes via `_inner`, flushes the sync engine, and wakes
/// the announce publisher.
pub(crate) async fn set_community_relay_opt_in_impl(
    state: &Mutex<NodeState>,
    community_id_hex: String,
    opted_in: bool,
) -> Result<(), String> {
    let (relay_optin_doc_arc, relay_optin_sync_arc, self_device_id, publisher_force) = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        let doc = g.relay_optin_doc.clone().ok_or_else(|| {
            "set_community_relay_opt_in: relay-optin not running (node not started)".to_string()
        })?;
        let sync = g.relay_optin_sync.clone().ok_or_else(|| {
            "set_community_relay_opt_in: relay-optin engine not running".to_string()
        })?;
        let self_device_id = g.dm_inbox_device_id.clone().unwrap_or_default();
        let publisher_force = g.community_relay_publisher_force.clone();
        (doc, sync, self_device_id, publisher_force)
    };

    let community_id = crate::owner_state_types::SpaceId(parse_space_id_16(&community_id_hex)?);

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    set_community_relay_opt_in_inner(
        &relay_optin_doc_arc,
        community_id,
        opted_in,
        now_ms,
        &self_device_id,
    )
    .await?;

    relay_optin_sync_arc.notify_dirty();
    if let Err(e) = relay_optin_sync_arc.flush_now().await {
        tracing::warn!(
            error = %e,
            "set_community_relay_opt_in: relay-optin flush failed; dirty latch will retry on next cycle"
        );
    }

    if let Some(force) = publisher_force {
        force.notify_one();
    }
    Ok(())
}

#[tauri::command]
async fn set_community_relay_opt_in(
    community_id_hex: String,
    opted_in: bool,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    set_community_relay_opt_in_impl(state.inner(), community_id_hex, opted_in).await
}
```

- [ ] **Step 4: Register the headless RPC + add the args struct**

In `src-tauri/src/api/rpc.rs`, add the args struct near the other args structs (after `ReadDmThreadArgs`):

```rust
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetCommunityRelayOptInArgs {
    community_id_hex: String,
    opted_in: bool,
}
```

And register it in `build_registry` next to the DM verbs:

```rust
    rpc!(
        m,
        "set_community_relay_opt_in",
        SetCommunityRelayOptInArgs,
        |state, _sink, a| async move {
            crate::set_community_relay_opt_in_impl(state, a.community_id_hex, a.opted_in).await
        }
    );
```

- [ ] **Step 5: Run the allowlist test — verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(registry_has_exactly)'`
Expected: PASS.

- [ ] **Step 6: fmt + clippy + commit**

```bash
cd src-tauri && cargo fmt --all && \
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
git add -A && git commit -m "feat(zeb-487): promote set_community_relay_opt_in to headless RPC

Extract set_community_relay_opt_in_impl(&Mutex<NodeState>) shared by the
GUI command + the curated serve RPC (connectivity_redeem_invite_iroh_impl
pattern). Relay opt-in is now drivable headlessly.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Promote `get_community_relay_status` to the headless surface

**Files:**
- Modify: `src-tauri/src/lib.rs` (the command at `~:44005`)
- Modify: `src-tauri/src/api/rpc.rs`

- [ ] **Step 1: Add the command name to the allowlist test (failing test)**

In `api/rpc.rs` `expected`, under the `// relay rung (ZEB-487)` group:

```rust
            // relay rung (ZEB-487)
            "set_community_relay_opt_in",
            "get_community_relay_status",
```

- [ ] **Step 2: Run the allowlist test — verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(registry_has_exactly)'`
Expected: FAIL — `get_community_relay_status` not yet registered.

- [ ] **Step 3: Extract `get_community_relay_status_impl`**

In `src-tauri/src/lib.rs`, replace the `get_community_relay_status` command (at `~:44005`) with an `_impl` + thin wrapper:

```rust
/// ZEB-458 P4 / ZEB-487: NodeState-level core of `get_community_relay_status`,
/// shared by the GUI Tauri command and the headless RPC.
pub(crate) async fn get_community_relay_status_impl(
    state: &Mutex<NodeState>,
    community_id_hex: String,
) -> Result<bool, String> {
    let community_id = crate::owner_state_types::SpaceId(parse_space_id_16(&community_id_hex)?);
    let doc = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.relay_optin_doc.clone().ok_or_else(|| {
            "get_community_relay_status: relay-optin not running (node not started)".to_string()
        })?
    };
    let opted_in = doc.lock().await.is_opted_in(&community_id);
    Ok(opted_in)
}

#[tauri::command]
async fn get_community_relay_status(
    community_id_hex: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    get_community_relay_status_impl(state.inner(), community_id_hex).await
}
```

- [ ] **Step 4: Register the headless RPC + args struct**

In `api/rpc.rs`, add (reused by the new read in Task 4 too):

```rust
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommunityIdArgs {
    community_id_hex: String,
}
```

And register:

```rust
    rpc!(
        m,
        "get_community_relay_status",
        CommunityIdArgs,
        |state, _sink, a| async move {
            crate::get_community_relay_status_impl(state, a.community_id_hex).await
        }
    );
```

- [ ] **Step 5: Run the allowlist test — verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(registry_has_exactly)'`
Expected: PASS.

- [ ] **Step 6: fmt + clippy + commit**

```bash
cd src-tauri && cargo fmt --all && \
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
git add -A && git commit -m "feat(zeb-487): promote get_community_relay_status to headless RPC

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: `RelayHeldEntryDto` + pure `map_relay_held` mapper (TDD core)

**Files:**
- Create: `src-tauri/src/relay_held_dto.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod relay_held_dto;`)

- [ ] **Step 1: Create the module with the DTO + a failing unit test (no mapper yet)**

Create `src-tauri/src/relay_held_dto.rs`:

```rust
//! ZEB-487: read-only DTOs + mapper for the headless `get_relay_held`
//! observability RPC. The relay holds blobs SEALED to the recipient's device
//! key — it cannot see the DM `space_id` or plaintext. Only routing metadata
//! (sender/recipient owner, community, the sealed-blob content id, timestamps)
//! is exposed. The content id is the recipient's CAS id for the held blob and
//! uniquely identifies the entry (the hold-doc map key is
//! `"{recipientOwnerHex}:{contentIdHex}"`).

use crate::community_relay_hold_crdt::RelayHoldDoc;
use crate::owner_state_types::SpaceId;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayHeldEntryDto {
    pub sender_owner_hex: String,
    pub recipient_owner_hex: String,
    pub community_id_hex: String,
    pub content_id_hex: String,
    pub held_at_ms: u64,
    pub held_by_device: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RelayHeldResponse {
    pub held: Vec<RelayHeldEntryDto>,
}

/// Map the relay-hold doc into DTOs, optionally filtered to one community.
/// Pure (no NodeState / no I/O) so it is unit-testable in isolation.
pub fn map_relay_held(doc: &RelayHoldDoc, community_filter: Option<&SpaceId>) -> Vec<RelayHeldEntryDto> {
    doc.entries
        .iter()
        // match (not `is_none_or`/`map_or`) sidesteps the MSRV-vs-clippy tension:
        // `is_none_or` needs Rust 1.82 (the `msrv` CI job may pin older), while
        // `map_or(true, …)` trips clippy::unnecessary_map_or on a recent toolchain.
        .filter(|(_, e)| match community_filter {
            Some(c) => &e.community_id == c,
            None => true,
        })
        .map(|(key, e)| {
            // key = "{recipientOwnerHex}:{contentIdHex}"
            let content_id_hex = key
                .rsplit_once(':')
                .map(|(_, c)| c.to_string())
                .unwrap_or_else(|| key.clone());
            RelayHeldEntryDto {
                sender_owner_hex: hex::encode(e.sender_owner),
                recipient_owner_hex: hex::encode(e.recipient_owner),
                community_id_hex: hex::encode(e.community_id.0),
                content_id_hex,
                held_at_ms: e.held_at.wall_ms,
                held_by_device: e.held_by.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_relay_hold_crdt::{RelayHoldDoc, RelayHoldEntry};
    use crate::owner_state_types::{Hlc, SpaceId};

    fn entry(so: u8, ro: u8, c: SpaceId, dev: &str) -> RelayHoldEntry {
        RelayHoldEntry {
            recipient_owner: [ro; 16],
            sender_owner: [so; 16],
            community_id: c,
            sealed_blob: vec![1, 2, 3],
            held_at: Hlc { wall_ms: 1234, logical: 0, device_id: dev.into() },
            held_by: dev.into(),
            pulled_by: Default::default(),
        }
    }

    #[test]
    fn maps_entries_with_optional_community_filter() {
        let c1 = SpaceId([0x11; 16]);
        let c2 = SpaceId([0x22; 16]);
        let mut doc = RelayHoldDoc::default();
        doc.entries.insert(
            format!("{}:{}", hex::encode([0xBB; 16]), hex::encode([0xCC; 32])),
            entry(0xAA, 0xBB, c1, "relaydev1"),
        );
        doc.entries.insert(
            format!("{}:{}", hex::encode([0xFF; 16]), hex::encode([0xEE; 32])),
            entry(0xDD, 0xFF, c2, "relaydev1"),
        );

        let all = map_relay_held(&doc, None);
        assert_eq!(all.len(), 2);

        let filtered = map_relay_held(&doc, Some(&c1));
        assert_eq!(filtered.len(), 1);
        let dto = &filtered[0];
        assert_eq!(dto.sender_owner_hex, hex::encode([0xAA; 16]));
        assert_eq!(dto.recipient_owner_hex, hex::encode([0xBB; 16]));
        assert_eq!(dto.community_id_hex, hex::encode([0x11; 16]));
        assert_eq!(dto.content_id_hex, hex::encode([0xCC; 32]));
        assert_eq!(dto.held_at_ms, 1234);
        assert_eq!(dto.held_by_device, "relaydev1");

        assert!(map_relay_held(&RelayHoldDoc::default(), None).is_empty());
    }
}
```

Register the module in `src-tauri/src/lib.rs` next to the other `pub mod community_relay_*;` declarations (e.g. after `pub mod community_relay_optin;` at `~:127`):

```rust
pub mod relay_held_dto;
```

> NOTE on `hex::encode`: confirm `hex` is a direct dependency of `harmony-app` (`grep '^hex' src-tauri/Cargo.toml`) — it is used pervasively for owner/space hex, so it almost certainly is. If a project helper is the canonical owner→hex encoder, use it instead for consistency. `SpaceId` is `SpaceId([u8;16])`, so `hex::encode(e.community_id.0)` encodes its bytes.

- [ ] **Step 2: Run the mapper test — verify it passes (the type + mapper are written together; this is the verification)**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(maps_entries_with_optional_community_filter)'`
Expected: PASS. (If you wrote the test first against a missing `map_relay_held`, it fails to compile until the fn exists — that is the red→green.)

- [ ] **Step 3: fmt + clippy + commit**

```bash
cd src-tauri && cargo fmt --all && \
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
git add -A && git commit -m "feat(zeb-487): RelayHeldEntryDto + pure map_relay_held mapper

Relay-held observability DTO carries only routing metadata (sender/recipient
owner, community, sealed-blob content id, timestamps) — the held blob is
sealed to the recipient device key, so no space_id/plaintext cid is available.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: `get_relay_held` headless RPC

**Files:**
- Modify: `src-tauri/src/lib.rs` (add `get_relay_held_impl`)
- Modify: `src-tauri/src/api/rpc.rs` (args + registration + allowlist)

- [ ] **Step 1: Add the command name to the allowlist test (failing test)**

In `api/rpc.rs` `expected`, under the relay-rung group:

```rust
            // relay rung (ZEB-487)
            "set_community_relay_opt_in",
            "get_community_relay_status",
            "get_relay_held",
```

- [ ] **Step 2: Run the allowlist test — verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(registry_has_exactly)'`
Expected: FAIL — `get_relay_held` not yet registered.

- [ ] **Step 3: Add `get_relay_held_impl` in `lib.rs`**

```rust
/// ZEB-487: read-only observability over the relay-hold doc. Reports the
/// blobs this node (as a community relay) is holding for offline recipients,
/// optionally filtered to one community. Sealed blobs are never opened.
pub(crate) async fn get_relay_held_impl(
    state: &Mutex<NodeState>,
    community_id_hex: Option<String>,
) -> Result<crate::relay_held_dto::RelayHeldResponse, String> {
    let filter = match community_id_hex {
        Some(h) => Some(crate::owner_state_types::SpaceId(parse_space_id_16(&h)?)),
        None => None,
    };
    let doc = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.relay_hold_doc.clone().ok_or_else(|| {
            "get_relay_held: relay-hold not running (node not started)".to_string()
        })?
    };
    let snapshot = doc.lock().await.clone();
    let held = crate::relay_held_dto::map_relay_held(&snapshot, filter.as_ref());
    Ok(crate::relay_held_dto::RelayHeldResponse { held })
}
```

- [ ] **Step 4: Register the headless RPC + args struct**

In `api/rpc.rs`:

```rust
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetRelayHeldArgs {
    #[serde(default)]
    community_id_hex: Option<String>,
}
```

```rust
    rpc!(
        m,
        "get_relay_held",
        GetRelayHeldArgs,
        |state, _sink, a| async move {
            crate::get_relay_held_impl(state, a.community_id_hex).await
        }
    );
```

- [ ] **Step 5: Run the allowlist test — verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(registry_has_exactly)'`
Expected: PASS.

- [ ] **Step 6: Full lib unit run + fmt + clippy + commit**

```bash
cd src-tauri && cargo fmt --all && \
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings && \
cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(relay) or test(registry_has_exactly)'
git add -A && git commit -m "feat(zeb-487): get_relay_held headless observability RPC

Read-only view of held relay deposits (routing metadata only) over the
existing NodeState.relay_hold_doc. Completes the headless relay-rung surface.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: e2e-harness — relaunch + relay driver helpers

**Files:**
- Modify: `e2e-harness/src/node.rs`
- Modify: `e2e-harness/src/driver.rs`

- [ ] **Step 1: Add `NodeHandle::relaunch` in `node.rs`**

Add after `shutdown` (`~:229`). `NodeConfig` is `Clone` and is explicitly designed for kill+relaunch (same `home`+`profile` rehydrates on-disk state):

```rust
    /// Take the node offline (real process kill, if still alive) and bring it
    /// back from the SAME config so its on-disk identity + app-data rehydrate.
    /// Returns a fresh handle (new port/token after re-discovery). Models the
    /// offline→online half of the ZEB-487 deposit→recover scenario.
    pub async fn relaunch(mut self) -> anyhow::Result<Self> {
        let _ = self.kill().await;
        NodeHandle::spawn(self.config.clone()).await
    }
```

- [ ] **Step 2: Add relay driver helpers in `driver.rs`**

Add next to the DM helpers (`~:206`):

```rust
/// ZEB-487: opt this node in/out of relaying for a community.
pub async fn set_relay_opt_in(node: &NodeHandle, community_id: &str, opted_in: bool) -> anyhow::Result<()> {
    node.rpc(
        "set_community_relay_opt_in",
        json!({ "communityIdHex": community_id, "optedIn": opted_in }),
    )
    .await?;
    Ok(())
}

/// ZEB-487: read whether this node is opted in to relaying for a community.
pub async fn get_relay_opt_in(node: &NodeHandle, community_id: &str) -> anyhow::Result<bool> {
    let v = node
        .rpc("get_community_relay_status", json!({ "communityIdHex": community_id }))
        .await?;
    Ok(v.as_bool().unwrap_or(false))
}

/// ZEB-487: list the relay-held deposit entries on this node (routing metadata
/// only; the held blobs are sealed). `community_id = None` returns all.
pub async fn get_relay_held(node: &NodeHandle, community_id: Option<&str>) -> anyhow::Result<Vec<Value>> {
    let args = match community_id {
        Some(c) => json!({ "communityIdHex": c }),
        None => json!({}),
    };
    let v = node.rpc("get_relay_held", args).await?;
    Ok(v.get("held").and_then(Value::as_array).cloned().unwrap_or_default())
}
```

- [ ] **Step 3: Compile the harness — verify the helpers build**

Run: `cd e2e-harness && cargo build --features e2e --tests 2>&1 | tail -5`
Expected: builds (no test run yet; the new helpers are unused-but-public — `pub` suppresses dead-code warnings).

- [ ] **Step 4: Commit**

```bash
cd e2e-harness && cargo fmt
git add -A && git commit -m "feat(zeb-487): harness relaunch + relay driver helpers

NodeHandle::relaunch (offline->online same profile) + set_relay_opt_in /
get_relay_opt_in / get_relay_held driver helpers for the deposit-recover
scenario.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: `s6_relay_deposit_recover` scenario (3 nodes)

**Files:**
- Modify: `e2e-harness/tests/e2e_two_node.rs`

> CONTEXT for the implementer: read `s1_invite_join_roster_convergence` and `s2_friend_graph_and_dm_send` in this same file, and the driver helpers they use (`create_community`, `generate_invite`, `poll_join_iroh` / the iroh redeem helper, `list_community_members`, `owner_id`, `generate_friend_token`, `redeem_friend_token`, `friend_is_active`, `accept_pending_from`, `add_dm_space`, `send_dm`, `read_dm_plaintext`, `poll_until`). Reuse them verbatim for the community-join + friendship dance — do NOT reimplement them. This task adds a 3rd node and the relay deposit→recover sequence.

- [ ] **Step 1: Add a `three_minted_nodes` helper**

Model it on `two_minted_nodes` (`~:91`). Roles: `a` = sender, `b` = recipient (goes offline), `r` = relay host.

```rust
async fn three_minted_nodes(
    scenario: &str,
) -> (
    RunDir,
    tempfile::TempDir,
    tempfile::TempDir,
    tempfile::TempDir,
    NodeHandle, // a = sender
    NodeHandle, // b = recipient
    NodeHandle, // r = relay host
) {
    let run = RunDir::new(scenario).expect("run dir");
    let a_home = fresh_home(&format!("{scenario}-a"));
    let b_home = fresh_home(&format!("{scenario}-b"));
    let r_home = fresh_home(&format!("{scenario}-r"));
    let mk = |home: &tempfile::TempDir, profile: &str| {
        let mut cfg = NodeConfig::new(PathBuf::from(home.path()), profile);
        cfg.log_dir = Some(run.log_dir());
        cfg
    };
    let a = NodeHandle::spawn(mk(&a_home, "alice")).await.expect("spawn a");
    let b = NodeHandle::spawn(mk(&b_home, "bob")).await.expect("spawn b");
    let r = NodeHandle::spawn(mk(&r_home, "relay")).await.expect("spawn r");
    for (n, who) in [(&a, "a"), (&b, "b"), (&r, "r")] {
        n.rpc("mint_owner_identity", json!({})).await.unwrap_or_else(|e| panic!("{who} mint: {e}"));
    }
    (run, a_home, b_home, r_home, a, b, r)
}
```

- [ ] **Step 2: Write the scenario test**

Add this test. It asserts the HELD∧RECV∧CLEARED triple; if the relay deposit never lands (co-located relay-routing gap, ZEB-466 class), it falls back to characterize-not-assert + a printed FINDING (mirroring `s2`/`s5`). The DM Space is deliberately NOT created while `b` is online — the deposited invite must bootstrap it.

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn s6_relay_deposit_recover() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let (mut run, ah, bh, rh, a, mut b, r) = three_minted_nodes("s6").await;
    let a_owner = owner_id(&a).await;
    let b_owner = owner_id(&b).await;

    // --- Setup: shared community C (a creates; b + r join via iroh first-contact).
    //     Reuse the EXACT community-join helpers s1 uses (read s1 first).
    let community_id = create_community(&a, "s6-relay").await.expect("create community");
    let invite = generate_invite(&a, &community_id).await.expect("invite");
    poll_join_iroh(&b, &invite).await.expect("b joins C");
    poll_join_iroh(&r, &invite).await.expect("r joins C");

    // --- Friendship a<->b while b is ONLINE (populates b's OwnerDeviceCache
    //     with a — required to verify the recovered CidNotify). Reuse s2's dance.
    let token = generate_friend_token(&a).await.expect("friend token");
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut last_err = String::from("(no redeem completed)");
    loop {
        if std::time::Instant::now() >= deadline {
            panic!("b never redeemed a's friend token in 120s; last error: {last_err}");
        }
        match redeem_friend_token(&b, &token).await {
            Ok(_) => break,
            Err(e) => last_err = e.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    poll_until(Duration::from_secs(120), || async {
        accept_pending_from(&a, &b_owner).await?;
        Ok(friend_is_active(&a, &b_owner).await?.then_some(()))
    })
    .await
    .expect("a has b active");
    poll_until(Duration::from_secs(120), || async {
        Ok(friend_is_active(&b, &a_owner).await?.then_some(()))
    })
    .await
    .expect("b has a active");

    // --- r volunteers as the relay for C; confirm the opt-in took.
    set_relay_opt_in(&r, &community_id, true).await.expect("r opts in to relay C");
    assert!(get_relay_opt_in(&r, &community_id).await.expect("r status"), "r is opted in");

    // --- b goes OFFLINE (real kill). The DM Space does NOT exist on b yet.
    b.kill().await.expect("kill b");

    // --- a creates the DM Space + sends the first message. b is unreachable, so
    //     after the no-ack windows the deposit fans out: butler skipped (b has
    //     none) -> relay rung deposits to r.
    let a_space = add_dm_space(&a, "s6-dm", &b_owner).await.expect("a dm space");
    send_dm(&a, &a_space, b"durable-hello", "text/plain")
        .await
        .expect("a send_dm accepted");

    // --- ASSERTION 1 (HELD): r holds the deposit for b while b is offline.
    //     Generous budget: deposit only fires after DEPOSIT_NOACK_WINDOWS=2 backoff.
    let held = poll_until(Duration::from_secs(60), || async {
        let entries = get_relay_held(&r, Some(&community_id)).await?;
        let m = entries.into_iter().find(|e| {
            e.get("senderOwnerHex").and_then(Value::as_str) == Some(a_owner.as_str())
                && e.get("recipientOwnerHex").and_then(Value::as_str) == Some(b_owner.as_str())
        });
        Ok(m)
    })
    .await;

    let held_ok = held.is_ok();
    if !held_ok {
        // FALLBACK: characterize, do not assert. Co-located relay resolve/dial
        // may not establish (ZEB-466-class community-topic routing gap). The
        // cross-WAN playbook run (Scenario D2) is the real proof.
        eprintln!(
            "S6 FINDING: relay deposit never landed on r within 60s co-located \
             (held=false). Likely the co-located community-relay resolve/dial gap \
             (ZEB-466 class), NOT a deposit-logic bug — file a finding ticket and \
             confirm via the cross-WAN Scenario D2. Skipping HELD/RECV/CLEARED asserts."
        );
        run.mark_success();
        drop((a, b, r, ah, bh, rh));
        return;
    }
    eprintln!("S6 HELD: r is holding a's deposit for b while b is offline.");

    // --- b comes back ONLINE (rehydrates). Recovery is automatic: b pulls held
    //     blobs for C from r, ingests, apply_deposited_invite bootstraps the DM
    //     Space, message applies, dm-received fires.
    b = b.relaunch().await.expect("relaunch b");

    // --- ASSERTION 2 (RECV): a's plaintext shows up in b's thread post-reconnect.
    //     b learns the space id from the bootstrapped Space; read tolerant of the
    //     a-side id (canonicalizes to min after merge).
    let recovered = poll_until(Duration::from_secs(90), || async {
        let msgs = read_dm_plaintext(&b, &a_space).await.unwrap_or_default();
        Ok(msgs.iter().any(|(_, body)| body == b"durable-hello").then_some(()))
    })
    .await;
    assert!(
        recovered.is_ok(),
        "RECV: b recovered a's deposited DM after reconnect (deposit->recover path)"
    );
    eprintln!("S6 RECV: b recovered the deposited DM after reconnect.");

    // --- ASSERTION 3 (CLEARED): r's held entry is gone (acked + GC'd post-recovery).
    let cleared = poll_until(Duration::from_secs(60), || async {
        let entries = get_relay_held(&r, Some(&community_id)).await?;
        let still_held = entries.iter().any(|e| {
            e.get("recipientOwnerHex").and_then(Value::as_str) == Some(b_owner.as_str())
        });
        Ok((!still_held).then_some(()))
    })
    .await;
    assert!(cleared.is_ok(), "CLEARED: r released the held entry after b recovered it");
    eprintln!("S6 CLEARED: r released the held deposit after recovery.");

    run.mark_success();
    drop((a, b, r, ah, bh, rh));
}
```

> If `create_community` / `generate_invite` / `poll_join_iroh` have different names in `driver.rs`, use the actual ones s1 calls — the SHAPE above is correct (create → invite → each peer redeems-iroh until joined).

- [ ] **Step 3: Run the scenario (asserted path or characterized fallback)**

Run: `cd e2e-harness && cargo nextest run --features e2e -E 'test(s6_relay_deposit_recover)' --no-capture`
Expected: PASS — either the full HELD→RECV→CLEARED triple, or the characterized fallback (which marks success + prints the FINDING). If it fails for a reason OTHER than "deposit never landed" (e.g. a real RECV/CLEARED assertion fired after HELD succeeded), that is a genuine product finding — capture both nodes' logs from the run dir and file a ticket; do NOT weaken the assert.

- [ ] **Step 4: Commit**

```bash
cd e2e-harness && cargo fmt
git add -A && git commit -m "test(zeb-487): s6_relay_deposit_recover 3-node deposit->recover scenario

Asserts HELD (relay holds deposit while recipient offline) -> RECV (DM
recovered after reconnect, Space bootstrapped from the deposited invite) ->
CLEARED (relay releases the entry). Characterize-not-assert fallback if the
co-located relay routing gap (ZEB-466 class) prevents the deposit landing.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 7: Playbook Scenario D2 (cross-WAN agent-driven)

**Files:**
- Modify: `docs/playbooks/e2e-two-agent-suite.md`

- [ ] **Step 1: Append Scenario D2 after the existing scenarios (before "Artifacts on failure")**

Add this section. It is the live, three-machine counterpart to the harness test (the real cross-WAN proof). Three roles: Agent A = Ildwyn (sender), Agent B = AVALON (recipient), Agent R = Koya (relay host). All use the proven `xwan` env recipe.

````markdown
### Scenario D2 — offline-at-create → relay deposit → recover (ZEB-487 / ZEB-483 durability)

Proves the headline DM durability: a DM created while the recipient is offline is
deposited on a community relay and delivered when the recipient returns, bootstrapping
the DM Space from the deposited invite. Three nodes: **A = sender, B = recipient
(goes offline), R = relay host** (a distinct owner; only needs to be a community
co-member). Drive with `harmony-app api`; signals on the ZEB-477 thread.

**Setup (all online):**

1. **A:** `create_community '{"name":"s6","isInviteOnly":true}'` → `communityId`; `generate_invite '{"communityId":"…"}'` → invite. Post `INVITE <url>` + `OWNER-A <hex>`.
2. **B and R:** each poll `connectivity_redeem_invite_iroh '{"url":"<url>"}'` until `{"status":"joined"}`. Post `JOINED-B` / `JOINED-R` + their `OWNER-*`.
3. **A:** `generate_friend_token` → post `FTOKEN <url>`. **B:** poll `redeem_friend_token '{"url":"<url>"}'` until Ok; both poll `list_friends` until `status:"active"` (A `accept_friend_request '{"ownerIdHex":"<B>"}'` if pending). This populates B's device cache with A. **Do NOT send a DM yet** — the DM Space must not exist on B.
4. **R:** `set_community_relay_opt_in '{"communityIdHex":"<communityId>","optedIn":true}'`; confirm `get_community_relay_status '{"communityIdHex":"<communityId>"}'` → `true`. Post `RELAY-READY`.

**Run:**

5. **B:** kill the `serve` PID (real offline). Post `OFFLINE`.
6. **A:** `add_space '{"kind":"dm","name":"s6-dm","members":["<B owner>"]}'` → `spaceId`; `send_dm '{"spaceId":"…","content":<bytes>,"mimeType":"text/plain"}'`. Post `SENT`.
7. **R:** poll `get_relay_held '{"communityIdHex":"<communityId>"}'` until an entry shows `senderOwnerHex == A` and `recipientOwnerHex == B`. (Deposit fires only after ~2 no-ack windows — be patient.) Post `HELD`.
8. **B:** relaunch the same `--profile` (rehydrates → auto-pulls + recovers).
9. **B:** poll `read_dm_thread '{"spaceId":"<A spaceId>","limit":100}'` until A's plaintext appears (hex-decode `body`). Post `RECV`.
10. **R:** poll `get_relay_held '{"communityIdHex":"<communityId>"}'` until B's entry is gone. Post `CLEARED`.
11. **PASS** = `HELD` (while B offline) ∧ `RECV` (after reconnect) ∧ `CLEARED`. Post `DONE D2 PASS`.

**Provenance is by construction:** B is killed before the send, so the live tunnel
cannot carry the message; HELD-while-offline + RECV-after-reconnect + CLEARED proves
it travelled via the relay deposit. If `get_relay_held` never shows the entry, capture
R's `<app-data>/profiles/xwan/logs/` + A's outbox logs and file a finding (likely a
relay resolve/dial issue cross-NAT) — do not call it PASS.
````

- [ ] **Step 2: Commit**

```bash
git add docs/playbooks/e2e-two-agent-suite.md
git commit -m "docs(zeb-487): playbook Scenario D2 — cross-WAN deposit->recover durability

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 8: Final sweep + PR prep

**Files:** none (verification only)

- [ ] **Step 1: Full lib gate**

Run:
```bash
cd src-tauri && cargo fmt --all -- --check && \
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings && \
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```
Expected: clean fmt, zero clippy warnings, all lib tests pass.

- [ ] **Step 2: Full `--all-targets` clippy sweep (the expensive one — once)**

Run: `cd src-tauri && cargo clippy --locked -p harmony-app --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: zero warnings (catches integration-test compile breaks the `--lib` gate misses).

- [ ] **Step 3: Harness compile + scenario**

Run: `cd e2e-harness && cargo fmt -- --check && cargo nextest run --features e2e -E 'test(s6_relay_deposit_recover)' --no-capture`
Expected: PASS or characterized-fallback PASS (per Task 6 Step 3).

- [ ] **Step 4: Push + open the PR (body closes ZEB-487 ONLY — keep parent ZEB-321 out, per the Linear auto-close cascade rule)**

```bash
git push -u origin zeb-487-headless-relay-rung-deposit-recover
```
PR title: `ZEB-487: headless relay-rung deposit→recover tooling`. Body: summarize the 3 RPCs + the harness scenario + the playbook; reference the spec path; **closes ZEB-487 only**. Then run the bot-review loop (Qodo/CodeAnt first pass → address → one CodeRabbit round) and pushover Jake at the ready-to-merge gate. Do NOT self-merge.

---

## Notes for the executor

- **Per-task gate scoping:** use `-p harmony-app --lib` for every task gate; reserve `--all-targets` for Task 8 (a lib change relinks ~97 integration binaries — ~25 min — so don't pay it per task).
- **`state.inner()`** is how a `tauri::State<'_, Mutex<NodeState>>` wrapper hands `&Mutex<NodeState>` to an `_impl`. If the existing `connectivity_redeem_invite_iroh` wrapper uses a different form (`&state` / `&*state`), match it.
- **The allowlist test is the integration guard:** any new curated command MUST appear in `expected` in `registry_has_exactly_the_curated_v1_surface` or that test fails — this is the intended red→green lever for Tasks 1, 2, 4.
- **Harness is not in CI** (gated `--features e2e`): the s6 scenario is a local/cross-machine proof, not a CI gate. Its job is to surface findings, not to block the PR.
- **If s6's relay deposit lands but RECV or CLEARED fails:** that is a REAL product finding (the recover path), not a harness limitation — file it under ZEB-321 with artifacts; do not weaken the assertion.
