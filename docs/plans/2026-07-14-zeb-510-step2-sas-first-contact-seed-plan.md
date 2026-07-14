# ZEB-510 Step 2 — SAS First-Contact Endpoint Seed Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a device observe its fleet sibling's iroh endpoint first-hand during the SAS pairing handshake and persist it as a dial seed, so P can dial B2 before fleet-net has ever converged — breaking the circular bootstrap that made step 1 insufficient.

**Architecture:** Piggyback each device's own iroh endpoint (node_id + home_relay) onto the **bidirectional** SAS `EncryptedPayload::Confirm` message (both roles send it). Each side stashes the peer's endpoint from the received Confirm, carries it into its enroll-result, and the persistence layer writes it to a new plaintext `fleet_peer_seed.cbor` store (mirroring `fleet_net_persist`). At boot, `start_node` feeds each seed row into the `ReachabilityResolver` as a `FleetSibling` entry — reusing everything step 1 built. B2's real FleetNetDoc row supersedes the seed via the resolver's existing per-source LWW once fleet-net converges (best-effort; the node_id is identical either way).

**Tech Stack:** Rust (Tauri backend, `src-tauri/`), zenoh LAN pairing transport, iroh transport, `cargo nextest`, the e2e-harness two-node driver.

**Design doc:** `docs/specs/2026-07-14-zeb-510-step2-sas-first-contact-seed-design.md` (approved 2026-07-14).
**Predecessor:** Step 1 is committed on this branch (`zeb-510-fleet-sibling-dial-seeding`). This plan builds on it. Steps 1+2 ship as ONE PR once s7 goes green.

## Global Constraints

- **CI gates (run from `src-tauri/`, all must pass before PR):**
  - `cargo fmt --all -- --check`
  - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast`
- **`--all-targets`, `--locked`, `--features test-fixtures` are load-bearing** (CLAUDE.md). This plan touches `lib.rs` and the pairing SM — per-task gates stay SCOPED (`cargo check` + targeted `-E` filters); the full `--workspace --all-targets` sweep is the single final gate.
- **Back-compat:** the pairing wire change MUST be additive `#[serde(default)]` optional fields — an old peer omits them and a new peer tolerates their absence (`None`). The `fleet_keytree_cbor_hex` field at `types.rs:129` is the precedent.
- **Verification-exempt seeds:** the seed feeds the resolver as `ReachabilitySource::FleetSibling` (zero signature); the endpoint's integrity comes from the SAS-authenticated channel it was observed on, not a per-record signature. Reuse the existing `FleetSibling` source/slot from step 1 — do NOT add a new source.
- **Endpoint tuple:** internal representation is `Option<([u8; 32], String)>` = `(iroh_node_id, home_relay)`; `Some` only when both are known. The wire carries two separate `Option<String>` fields (`iroh_node_id_hex`, `iroh_home_relay`).
- **`cd` drifts between Bash calls** — use absolute paths or a single compound `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && ...`.
- **Keychain in tests:** never construct `KeychainStore::new()` reachable from tests; use the `*_inner` seams with `None` (CLAUDE.md ZEB-428).

---

### Task 1: Wire — carry the iroh endpoint on `EncryptedPayload::Confirm`

**Files:**
- Modify: `src-tauri/src/pairing/types.rs:117-120` (the `Confirm` variant)
- Test: `src-tauri/src/pairing/types.rs` (inline `#[cfg(test)] mod tests`, `:142+`)

**Interfaces:**
- Produces: `EncryptedPayload::Confirm { sas_digits, iroh_node_id_hex: Option<String>, iroh_home_relay: Option<String> }`. Both new fields `#[serde(default, skip_serializing_if = "Option::is_none")]`.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `src-tauri/src/pairing/types.rs`:

```rust
    #[test]
    fn confirm_carries_iroh_endpoint_and_omits_when_absent() {
        // Present: round-trips through CBOR.
        let with = EncryptedPayload::Confirm {
            sas_digits: "123456".into(),
            iroh_node_id_hex: Some("ab".repeat(32)),
            iroh_home_relay: Some("https://relay.example/".into()),
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&with, &mut buf).unwrap();
        let back: EncryptedPayload = ciborium::from_reader(&buf[..]).unwrap();
        match back {
            EncryptedPayload::Confirm { sas_digits, iroh_node_id_hex, iroh_home_relay } => {
                assert_eq!(sas_digits, "123456");
                assert_eq!(iroh_node_id_hex.as_deref(), Some("ab".repeat(32).as_str()));
                assert_eq!(iroh_home_relay.as_deref(), Some("https://relay.example/"));
            }
            _ => panic!("expected Confirm"),
        }

        // Absent: `skip_serializing_if` omits the endpoint keys from the wire,
        // and `#[serde(default)]` fills them as None on decode — this IS the
        // back-compat guarantee (a pre-step-2 peer's Confirm never carries them).
        let without = EncryptedPayload::Confirm {
            sas_digits: "654321".into(),
            iroh_node_id_hex: None,
            iroh_home_relay: None,
        };
        let mut buf2 = Vec::new();
        ciborium::into_writer(&without, &mut buf2).unwrap();
        let back2: EncryptedPayload = ciborium::from_reader(&buf2[..]).unwrap();
        match back2 {
            EncryptedPayload::Confirm { sas_digits, iroh_node_id_hex, iroh_home_relay } => {
                assert_eq!(sas_digits, "654321");
                assert!(iroh_node_id_hex.is_none());
                assert!(iroh_home_relay.is_none());
            }
            _ => panic!("expected Confirm"),
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(confirm_carries_iroh_endpoint)'`
Expected: FAIL to compile — the `Confirm` variant has no `iroh_node_id_hex`/`iroh_home_relay` fields.

- [ ] **Step 3: Add the fields**

In `src-tauri/src/pairing/types.rs`, replace the `Confirm` variant (`:118-120`):

```rust
    Confirm {
        sas_digits: String,
        /// ZEB-510 step 2: the sender's iroh transport endpoint, observed
        /// first-hand over the SAS-authenticated channel so each device can seed
        /// a dial route to its fleet sibling before fleet-net converges. Hex of
        /// the 32-byte iroh node_id. `#[serde(default)]` keeps pre-step-2 peers
        /// decodable (they omit it; the receiver tolerates `None`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        iroh_node_id_hex: Option<String>,
        /// ZEB-510 step 2: the sender's iroh home-relay URL (may be empty even
        /// when `iroh_node_id_hex` is present, if the relay is not yet known).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        iroh_home_relay: Option<String>,
    },
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(confirm_carries_iroh_endpoint)'`
Expected: PASS. (This will also surface every `EncryptedPayload::Confirm { .. }` construction/match site in the crate as a compile error in the *next* tasks — expected; Task 3 fixes the two SM sites. If any OTHER site fails to compile, it must add `iroh_node_id_hex: None, iroh_home_relay: None` or `..` — note it in the report.)

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/pairing/types.rs && git commit -m "feat(zeb-510): carry iroh endpoint on SAS Confirm wire message"
```

---

### Task 2: New `fleet_peer_seed` store + `FleetSibling` mapper

**Files:**
- Create: `src-tauri/src/fleet_peer_seed.rs`
- Create: `src-tauri/src/fleet_peer_seed_persist.rs`
- Modify: `src-tauri/src/lib.rs:175-189` (add two `pub mod` lines)
- Test: inline `#[cfg(test)] mod tests` in both new files

**Interfaces:**
- Produces:
  - `FleetPeerSeedDoc { seeds: BTreeMap<String /*node_id hex*/, FleetPeerSeedRow> }` (`Default`).
  - `FleetPeerSeedRow { iroh_node_id: [u8;32], home_relay: String, observed_at_ms: u64 }`.
  - `fleet_peer_seed::seed_reachability_payload(row: &FleetPeerSeedRow) -> crate::reachability_record::ReachabilityAnnouncePayload` (zero signature, empty butler_set — mirrors `fleet_net::sibling_reachability_payload`).
  - `fleet_peer_seed_persist::{FLEET_PEER_SEED_FILENAME, load, load_doc_or_recover, save}`.
- Consumes: `crate::reachability_record::ReachabilityAnnouncePayload`, `crate::fleet_sync::SyncError`, `crate::owner_state_persist::save_atomically`.

- [ ] **Step 1: Create `fleet_peer_seed.rs` with a failing test**

Create `src-tauri/src/fleet_peer_seed.rs`:

```rust
//! ZEB-510 step 2: same-owner fleet-peer dial seeds observed during SAS
//! pairing. A one-shot local store (NOT a synced CRDT) that lets a device dial
//! a freshly-paired sibling before fleet-net has ever converged. Fed into the
//! ReachabilityResolver at boot as a `FleetSibling` entry; superseded by the
//! sibling's real FleetNetDoc row (same node_id) once fleet-net converges.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetPeerSeedDoc {
    /// Keyed by the peer's iroh node_id (hex). Both sides learn the peer's
    /// node_id directly from the received SAS `Confirm`; the resolver key is
    /// `(self_owner, iroh_node_id)` regardless, so a seed and the eventual real
    /// FleetNetDoc row converge on the same resolver slot.
    #[serde(rename = "sd")]
    pub seeds: BTreeMap<String, FleetPeerSeedRow>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetPeerSeedRow {
    #[serde(rename = "ep")]
    pub iroh_node_id: [u8; 32],
    #[serde(rename = "hr")]
    pub home_relay: String,
    /// Pairing-time wall-clock ms; the resolver entry's announce time.
    #[serde(rename = "oa")]
    pub observed_at_ms: u64,
}

/// Project a seed row into a dial-target reachability payload for the
/// ReachabilityResolver. Verification-exempt (zero signature): the endpoint's
/// integrity comes from the SAS-authenticated channel it was observed on.
/// Mirrors `crate::fleet_net::sibling_reachability_payload`.
pub fn seed_reachability_payload(
    row: &FleetPeerSeedRow,
) -> crate::reachability_record::ReachabilityAnnouncePayload {
    crate::reachability_record::ReachabilityAnnouncePayload {
        iroh_node_id: row.iroh_node_id,
        home_relay_url: row.home_relay.clone(),
        direct_addresses: Vec::new(),
        announced_at_ms: row.observed_at_ms,
        identity_signature: [0u8; 64],
        butler_set: Vec::new(),
        bs_at: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_payload_maps_fields_and_is_unsigned() {
        let row = FleetPeerSeedRow {
            iroh_node_id: [0xB2; 32],
            home_relay: "https://relay.example/".into(),
            observed_at_ms: 4242,
        };
        let p = seed_reachability_payload(&row);
        assert_eq!(p.iroh_node_id, [0xB2; 32]);
        assert_eq!(p.home_relay_url, "https://relay.example/");
        assert_eq!(p.announced_at_ms, 4242);
        assert!(p.direct_addresses.is_empty());
        assert_eq!(p.identity_signature, [0u8; 64]);
        assert!(p.butler_set.is_empty());
        assert_eq!(p.bs_at, 0);
    }
}
```

- [ ] **Step 2: Create `fleet_peer_seed_persist.rs` with round-trip + recovery tests**

Create `src-tauri/src/fleet_peer_seed_persist.rs` — this mirrors `fleet_net_persist.rs:1-227` exactly, minus the replay-tracker and `FleetPersist` trait (a seed store is a one-shot local write, not a CRDT):

```rust
//! ZEB-510 step 2: on-disk persistence for `FleetPeerSeedDoc`. Same idiom as
//! `fleet_net_persist`: 1-byte schema-version prefix + plaintext CBOR, atomic
//! write via `owner_state_persist::save_atomically`, corrupt-file quarantine on
//! decode failure. Plaintext at rest is deliberate (dialing coordinates, not a
//! secret — same class as `fleet_net.cbor`, and captured over the SAS channel).

use crate::fleet_peer_seed::FleetPeerSeedDoc;
use crate::fleet_sync::SyncError;
use ciborium::{from_reader, into_writer};
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::path::Path;

/// File name for the persisted seed store. Lives at `<identity_dir>/…`.
pub const FLEET_PEER_SEED_FILENAME: &str = "fleet_peer_seed.cbor";

const FLEET_PEER_SEED_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct FleetPeerSeedFileV1(FleetPeerSeedDoc);

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SyncError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SyncError::Persist(format!("create_dir_all {}: {e}", path.display())))?;
    }
    crate::owner_state_persist::save_atomically(path, bytes)
        .map_err(|e| SyncError::Persist(e.to_string()))
}

/// Load the seed doc. Returns `Ok(default())` when the file does not exist.
pub fn load(path: &Path) -> Result<FleetPeerSeedDoc, SyncError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FleetPeerSeedDoc::default())
        }
        Err(e) => return Err(SyncError::Persist(format!("read {}: {e}", path.display()))),
    };
    if bytes.is_empty() {
        return Err(SyncError::CborDecode(format!(
            "fleet-peer-seed file is empty: {}",
            path.display()
        )));
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        FLEET_PEER_SEED_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: FleetPeerSeedFileV1 = from_reader(&mut cursor)
                .map_err(|e| SyncError::CborDecode(format!("load {}: {e}", path.display())))?;
            let pos = cursor.position() as usize;
            if pos != payload.len() {
                return Err(SyncError::CborDecode(format!(
                    "trailing bytes after fleet-peer-seed value: consumed {} of {}",
                    pos,
                    payload.len()
                )));
            }
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown fleet-peer-seed schema version {v:#x} in {}",
            path.display()
        ))),
    }
}

/// Load the seed doc, quarantining a genuinely-corrupt file and self-healing to
/// `default()` so boot never bricks; transient I/O errors are propagated.
pub fn load_doc_or_recover(path: &Path) -> Result<FleetPeerSeedDoc, SyncError> {
    match load(path) {
        Ok(doc) => Ok(doc),
        Err(e @ SyncError::CborDecode(_)) => {
            quarantine(path, &e);
            Ok(FleetPeerSeedDoc::default())
        }
        Err(e) => Err(e),
    }
}

fn quarantine(path: &Path, err: &SyncError) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut corrupt = path.as_os_str().to_os_string();
    corrupt.push(format!(".corrupt-{ms}"));
    tracing::error!(path = %path.display(), error = %err,
        "fleet-peer-seed load failed; quarantining corrupt file and starting fresh (bytes preserved)");
    if let Err(re) = std::fs::rename(path, &corrupt) {
        tracing::warn!(path = %path.display(), error = %re, "failed to quarantine corrupt fleet-peer-seed file");
    }
}

/// Save the seed doc atomically (tempfile + fsync + parent-dir fsync + rename).
pub fn save(path: &Path, doc: &FleetPeerSeedDoc) -> Result<(), SyncError> {
    let mut bytes = vec![FLEET_PEER_SEED_SCHEMA_V1];
    into_writer(&FleetPeerSeedFileV1(doc.clone()), &mut bytes)
        .map_err(|e| SyncError::CborEncode(format!("encode {}: {e}", path.display())))?;
    atomic_write(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet_peer_seed::FleetPeerSeedRow;

    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FLEET_PEER_SEED_FILENAME);
        // Missing file → default.
        assert_eq!(load(&path).unwrap(), FleetPeerSeedDoc::default());

        let mut doc = FleetPeerSeedDoc::default();
        doc.seeds.insert(
            "ab".repeat(32),
            FleetPeerSeedRow { iroh_node_id: [0xAB; 32], home_relay: "r".into(), observed_at_ms: 7 },
        );
        save(&path, &doc).unwrap();
        assert_eq!(load(&path).unwrap(), doc);
    }

    #[test]
    fn corrupt_file_is_quarantined_and_recovers_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FLEET_PEER_SEED_FILENAME);
        std::fs::write(&path, [0x01, 0xff, 0xff, 0xff]).unwrap(); // valid version byte, junk CBOR
        let recovered = load_doc_or_recover(&path).unwrap();
        assert_eq!(recovered, FleetPeerSeedDoc::default());
        // Original path is gone (renamed to .corrupt-*).
        assert!(!path.exists());
    }
}
```

- [ ] **Step 3: Register the modules**

In `src-tauri/src/lib.rs`, in the `pub mod` block (`:175-189`), add after `pub mod fleet_net_persist;`:

```rust
pub mod fleet_peer_seed;
pub mod fleet_peer_seed_persist;
```

- [ ] **Step 4: Run the tests**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(seed_payload_maps_fields) + test(save_load_round_trip) + test(corrupt_file_is_quarantined)'`
Expected: PASS (3 tests). If `SyncError` lacks a `CborEncode` variant, check `crate::fleet_sync::SyncError` and use the exact variant `fleet_net_persist::save` uses (it uses `SyncError::CborEncode` per the template).

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/fleet_peer_seed.rs src-tauri/src/fleet_peer_seed_persist.rs src-tauri/src/lib.rs && git commit -m "feat(zeb-510): fleet_peer_seed store + FleetSibling mapper"
```

---

### Task 3: Thread the iroh endpoint through the SAS handshake

**Files:**
- Modify: `src-tauri/src/pairing/state_machine.rs` — `SessionCtx` (:381-489), `PairingCommand::Start{Inviter,Joiner}` (:39-69), `run_state_machine` arms (:217-227), `start_inviter` (:492-528) + `start_joiner`, `on_confirm_sas` (:729-804), the `Confirm` handler (:1525-1552), `InviterEnrollResult`/`JoinerEnrollResult` (:73-123) + their build sites (:1267, :1734)
- Modify: `src-tauri/src/pairing_commands.rs` — `start_inviter_pairing_with_keychain` (:36-137), `start_joiner_pairing_inner` (:147-161)
- Test: `src-tauri/src/pairing/state_machine.rs` test module (full-pairing round-trip)

**Interfaces:**
- Consumes: `EncryptedPayload::Confirm` endpoint fields (Task 1); `NodeState.iroh_endpoint` (`lib.rs:1449`).
- Produces: `InviterEnrollResult.peer_iroh_endpoint: Option<([u8;32], String)>` and `JoinerEnrollResult.peer_iroh_endpoint: Option<([u8;32], String)>`, each set to the endpoint observed in the peer's `Confirm`. `PairingCommand::Start{Inviter,Joiner}` gain `local_iroh_endpoint: Option<([u8;32], String)>`.

- [ ] **Step 1: Write the failing round-trip test**

Add to `state_machine.rs`'s `#[cfg(test)] mod tests`. **Model it on the existing full-pairing tests in this module** (search the test module for how it constructs `InMemoryTransport`, spawns two `run_state_machine` tasks, sends `StartInviter`/`StartJoiner` + `ConfirmSas`, and drains `result_rx`/`inviter_result_rx`). Drive a complete inviter↔joiner pairing where each side is given a distinct `local_iroh_endpoint`, then assert each side's enroll result carries the OTHER side's endpoint:

```rust
    // ZEB-510 step 2: the iroh endpoint observed in the peer's CONFIRM must
    // round-trip into the enroll result on BOTH sides.
    // (Wire this to the module's existing full-pairing harness — same setup as
    // the existing end-to-end pairing test, with these two assertions added:)
    //   let inviter_ep = ([0x11u8; 32], "https://inviter.relay/".to_string());
    //   let joiner_ep  = ([0x22u8; 32], "https://joiner.relay/".to_string());
    //   -> StartInviter { .., local_iroh_endpoint: Some(inviter_ep.clone()) }
    //   -> StartJoiner  { .., local_iroh_endpoint: Some(joiner_ep.clone()) }
    //   after both reach Complete and results are drained:
    //   assert_eq!(inviter_result.peer_iroh_endpoint, Some(joiner_ep));   // P learned B2
    //   assert_eq!(joiner_result.peer_iroh_endpoint,  Some(inviter_ep));  // B2 learned P
```

Write it as a real, compiling test using the in-file harness. If no full end-to-end pairing test exists to model on, add a minimal one that drives both SMs over `InMemoryTransport` to Complete (the harness types are all in this file).

- [ ] **Step 2: Run to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(pairing) and test(iroh_endpoint)' 2>&1 | tail -20`
Expected: FAIL to compile — `local_iroh_endpoint`/`peer_iroh_endpoint` fields don't exist yet.

- [ ] **Step 3: Add the ctx fields**

In `SessionCtx` (`state_machine.rs:381-453`), after `sas_digits: Option<String>,` (the "After Handshake" group) add:

```rust
    /// ZEB-510 step 2: this device's own iroh endpoint (node_id, home_relay),
    /// threaded from the Start* command; attached to our outgoing CONFIRM.
    local_iroh_endpoint: Option<([u8; 32], String)>,
    /// ZEB-510 step 2: the peer's iroh endpoint as observed in their CONFIRM;
    /// carried into the enroll result for the seed store.
    peer_iroh_endpoint: Option<([u8; 32], String)>,
```

In `SessionCtx::new` (`:455-489`), add both to the initializer (after `sas_digits: None,`):

```rust
            local_iroh_endpoint: None,
            peer_iroh_endpoint: None,
```

- [ ] **Step 4: Add the field to `PairingCommand` and thread it to the ctx**

In `PairingCommand::StartInviter` (`:15-34`) add (after `fleet_current_epoch: u32,`):

```rust
        /// ZEB-510 step 2: this device's iroh endpoint (node_id, home_relay),
        /// attached to our CONFIRM so the joiner can seed a dial route to us.
        local_iroh_endpoint: Option<([u8; 32], String)>,
```

In `PairingCommand::StartJoiner` (`:35-38`) add (after `signing_key: SigningKey,`):

```rust
        /// ZEB-510 step 2: this device's iroh endpoint, attached to our CONFIRM.
        local_iroh_endpoint: Option<([u8; 32], String)>,
```

In `run_state_machine`'s match arms (`:175-182`), thread it through:

```rust
                    PairingCommand::StartInviter { display_name, owner_state, master_seed, fleet_keytree, quorum_ctx, fleet_current_epoch, local_iroh_endpoint } => {
                        ctx = start_inviter(&transport, &state_tx, display_name, owner_state, master_seed, fleet_keytree, quorum_ctx, fleet_current_epoch, local_iroh_endpoint, &now_fn).await;
                    }
                    PairingCommand::StartJoiner { display_name, signing_key, local_iroh_endpoint } => {
                        ctx = start_joiner(&transport, &state_tx, display_name, signing_key, local_iroh_endpoint, &now_fn).await;
                    }
```

In `start_inviter` (`:492-528`): add the param `local_iroh_endpoint: Option<([u8; 32], String)>,` (before `_now_fn`) and set `ctx.local_iroh_endpoint = local_iroh_endpoint;` alongside the other `ctx.*` assignments. Do the same for `start_joiner` (find its signature near `start_inviter`; add the param and `ctx.local_iroh_endpoint = local_iroh_endpoint;`).

- [ ] **Step 5: Attach the local endpoint on the outgoing CONFIRM**

In `on_confirm_sas` (`state_machine.rs:729-804`), replace the `EncryptedPayload::Confirm` construction (`:745-747` in the extract, currently `Confirm { sas_digits: sas_digits.clone() }`):

```rust
    let (iroh_node_id_hex, iroh_home_relay) = match ctx.local_iroh_endpoint.as_ref() {
        Some((node_id, relay)) => (Some(hex::encode(node_id)), Some(relay.clone())),
        None => (None, None),
    };
    let payload = EncryptedPayload::Confirm {
        sas_digits: sas_digits.clone(),
        iroh_node_id_hex,
        iroh_home_relay,
    };
```

- [ ] **Step 6: Stash the peer's endpoint from the received CONFIRM**

In the `Confirm` handler (`state_machine.rs:1525`), change the match pattern to bind the new fields and stash them after the SAS check:

```rust
        EncryptedPayload::Confirm { sas_digits, iroh_node_id_hex, iroh_home_relay } => {
            if Some(&sas_digits) != ctx.sas_digits.as_ref() {
                let _ = state_tx.send(PairingState::Failed {
                    reason: "SAS mismatch in CONFIRM".to_string(),
                });
                return;
            }
            // ZEB-510 step 2: record the peer's dialing coordinates observed
            // over this SAS-authenticated channel (best-effort — a pre-step-2
            // peer omits them, leaving this None).
            ctx.peer_iroh_endpoint = match (iroh_node_id_hex, iroh_home_relay) {
                (Some(nid_hex), relay) => match hex::decode(&nid_hex) {
                    Ok(bytes) if bytes.len() == 32 => {
                        let mut nid = [0u8; 32];
                        nid.copy_from_slice(&bytes);
                        Some((nid, relay.unwrap_or_default()))
                    }
                    _ => None,
                },
                _ => None,
            };
            ctx.peer_confirmed = true;
            if ctx.our_confirmed {
                maybe_advance_to_enroll(
                    transport, state_tx, ctx, now_fn,
                    inviter_result_tx, persist_done_tx, quorum_done_tx,
                ).await;
            }
        }
```

- [ ] **Step 7: Add `peer_iroh_endpoint` to both enroll results + set at build sites**

In `InviterEnrollResult` (`:100-113`) add (after `master_seed`):

```rust
    /// ZEB-510 step 2: the joiner's iroh endpoint observed in their CONFIRM,
    /// persisted as a first-contact dial seed. `None` when paired with a
    /// pre-step-2 joiner (or the endpoint was unknown).
    pub peer_iroh_endpoint: Option<([u8; 32], String)>,
```

In `JoinerEnrollResult` (`:73-82`) add (after `fleet_keytree`):

```rust
    /// ZEB-510 step 2: the inviter's iroh endpoint observed in their CONFIRM,
    /// persisted as a first-contact dial seed. `None` when paired with a
    /// pre-step-2 inviter.
    pub peer_iroh_endpoint: Option<([u8; 32], String)>,
```

At the inviter build site (`:1267-1275`) set it in the `InviterEnrollResult { .. }` literal:

```rust
            result: InviterEnrollResult {
                cert,
                now,
                master_seed: result_master_seed,
                peer_iroh_endpoint: ctx.peer_iroh_endpoint.clone(),
            },
```

At the joiner build site (`:1731-1738`) set it in the `JoinerEnrollResult { .. }` literal:

```rust
                .send(JoinerEnrollResult {
                    our_signing_key: our_sk,
                    owner_state,
                    our_device_id,
                    fleet_keytree,
                    peer_iroh_endpoint: ctx.peer_iroh_endpoint.clone(),
                })
```

- [ ] **Step 8: Thread the local endpoint from `pairing_commands.rs`**

In `start_inviter_pairing_with_keychain` (`pairing_commands.rs:36-137`), read the local endpoint under the existing lock pattern and add it to BOTH `PairingCommand::StartInviter { .. }` literals. Add near the `fleet_current_epoch` read (`:65-68`):

```rust
    let local_iroh_endpoint = {
        let guard = state.lock().unwrap_or_else(|p| p.into_inner());
        guard.iroh_endpoint.as_ref().map(|ep| {
            (
                *ep.node_id().as_bytes(),
                ep.home_relay().map(|r| r.to_string()).unwrap_or_default(),
            )
        })
    };
```

Then add `local_iroh_endpoint: local_iroh_endpoint.clone(),` to the first `StartInviter` literal (`:78-85`) and `local_iroh_endpoint,` to the second (`:151-158`).

In `start_joiner_pairing_inner` (`:147-161`), read it the same way and add to the `StartJoiner` literal:

```rust
    let local_iroh_endpoint = {
        let guard = state.lock().unwrap_or_else(|p| p.into_inner());
        guard.iroh_endpoint.as_ref().map(|ep| {
            (
                *ep.node_id().as_bytes(),
                ep.home_relay().map(|r| r.to_string()).unwrap_or_default(),
            )
        })
    };
    let (cmd_tx, _state_rx) = require_pairing_handle(state)?;
    cmd_tx
        .send(PairingCommand::StartJoiner {
            display_name,
            signing_key,
            local_iroh_endpoint,
        })
```

> `NodeState.iroh_endpoint` is `Option<Arc<IrohEndpoint>>` (`lib.rs:1449`); `ep.node_id().as_bytes()` returns `&[u8;32]` and `ep.home_relay()` returns an `Option<_>` you `.map(|r| r.to_string())` (see `lib.rs:5624-5635`). Confirm `state` is the `&Mutex<NodeState>` already in scope (it is — read for `fleet_current_epoch`).

- [ ] **Step 9: Fix any other `Confirm` construction/match sites, then run the round-trip test**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo check --locked --features test-fixtures 2>&1 | tail -20`
Fix any remaining `EncryptedPayload::Confirm { .. }` match/build sites the compiler flags (add the two fields or `..`). Then:
Run: `cargo nextest run --locked --features test-fixtures -E 'test(pairing)' 2>&1 | tail -25`
Expected: the new round-trip test PASSES and all existing pairing tests stay green.

- [ ] **Step 10: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/pairing/state_machine.rs src-tauri/src/pairing_commands.rs && git commit -m "feat(zeb-510): thread iroh endpoint through SAS handshake into enroll results"
```

---

### Task 4: Persist the observed endpoint as a dial seed

**Files:**
- Modify: `src-tauri/src/pairing/persist.rs` — `install_inviter_state_inner` (:128-182), `install_joiner_state_inner` (:29-100)
- Test: `src-tauri/src/pairing/persist.rs` test module

**Interfaces:**
- Consumes: `InviterEnrollResult.peer_iroh_endpoint` / `JoinerEnrollResult.peer_iroh_endpoint` (Task 3); `fleet_peer_seed_persist::{load_doc_or_recover, save, FLEET_PEER_SEED_FILENAME}` + `fleet_peer_seed::{FleetPeerSeedDoc, FleetPeerSeedRow}` (Task 2).
- Produces: after a successful pairing persist, `<identity_dir>/fleet_peer_seed.cbor` contains a row keyed by the peer's node_id hex.

- [ ] **Step 1: Write the failing test**

Add to `persist.rs`'s test module. Use the `*_inner` seam with `None` keychains + `HARMONY_PASSPHRASE` (see the existing `install_*_state` tests in this module for the exact harness — mint/prepare an on-disk owner state in a tempdir first, since `install_inviter_state_inner` requires existing owner state and `install_joiner_state_inner` writes one):

```rust
    // ZEB-510 step 2: a completed pairing with an observed peer endpoint writes
    // a fleet_peer_seed row keyed by the peer's node_id hex.
    // (Model the owner-state setup on this module's existing install_*_state
    // tests; the new assertion:)
    //   let seed_path = identity_dir.join(crate::fleet_peer_seed_persist::FLEET_PEER_SEED_FILENAME);
    //   let doc = crate::fleet_peer_seed_persist::load(&seed_path).unwrap();
    //   let key = hex::encode([0x22u8; 32]);
    //   assert_eq!(doc.seeds.get(&key).unwrap().iroh_node_id, [0x22u8; 32]);
    //   assert_eq!(doc.seeds.get(&key).unwrap().home_relay, "https://peer.relay/");
```

Write it as a real compiling test: build a `JoinerEnrollResult` (or `InviterEnrollResult`) with `peer_iroh_endpoint: Some(([0x22;32], "https://peer.relay/".into()))`, call `install_joiner_state_inner` (or inviter) against a tempdir, then assert the seed row landed. Also add a companion case asserting `peer_iroh_endpoint: None` writes NO seed file / leaves the store empty.

- [ ] **Step 2: Run to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(seed) and test(install)' 2>&1 | tail -20`
Expected: FAIL — no seed is written yet (the store is empty / file absent).

- [ ] **Step 3: Add a shared seed-write helper and call it from both installers**

In `pairing/persist.rs`, add a private helper (near the top of the impl section):

```rust
/// ZEB-510 step 2: upsert a first-contact dial seed for a freshly-paired peer,
/// keyed by the peer's iroh node_id. No-op when `endpoint` is `None` (pre-step-2
/// peer). Best-effort: a seed-write failure is logged, never fails the pairing
/// persist (the owner-state write already succeeded; the seed only accelerates
/// first dial and is superseded by fleet-net convergence).
fn persist_peer_seed(identity_dir: &Path, endpoint: &Option<([u8; 32], String)>) {
    let Some((node_id, home_relay)) = endpoint else {
        return;
    };
    let observed_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let path = identity_dir.join(crate::fleet_peer_seed_persist::FLEET_PEER_SEED_FILENAME);
    let mut doc = match crate::fleet_peer_seed_persist::load_doc_or_recover(&path) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "fleet-peer-seed load failed; skipping seed write");
            return;
        }
    };
    doc.seeds.insert(
        hex::encode(node_id),
        crate::fleet_peer_seed::FleetPeerSeedRow {
            iroh_node_id: *node_id,
            home_relay: home_relay.clone(),
            observed_at_ms,
        },
    );
    if let Err(e) = crate::fleet_peer_seed_persist::save(&path, &doc) {
        tracing::warn!(error = %e, "fleet-peer-seed save failed; first dial will wait for fleet-net convergence");
    }
}
```

In `install_inviter_state_inner` (`:128-182`), after the `save_owner_state_atomic(...)?;` call succeeds and before `Ok(())`, add:

```rust
    persist_peer_seed(identity_dir, &result.peer_iroh_endpoint);
    Ok(())
```

In `install_joiner_state_inner` (`:29-100`), after the `save_owner_state_atomic(...)?;` call and before `Ok(())`, add the same:

```rust
    persist_peer_seed(identity_dir, &result.peer_iroh_endpoint);
    Ok(())
```

> Placement note: the seed write is deliberately AFTER the owner-state save (which is the operation whose failure must abort). A seed-write failure is best-effort (logged, non-fatal), so it does not use `?`. Add `use std::path::Path;` if not already imported (it is — `install_*` take `identity_dir: &Path`).

- [ ] **Step 4: Run the tests**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(persist) and test(seed)' 2>&1 | tail -20`
Expected: PASS — seed row present when endpoint is `Some`, store empty/absent when `None`.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/pairing/persist.rs && git commit -m "feat(zeb-510): persist first-contact peer endpoint as a dial seed"
```

---

### Task 5: Boot-feed the seed store into the resolver

**Files:**
- Modify: `src-tauri/src/lib.rs` — immediately after the step-1 boot-replay hook (`:5668-5689`)

**Interfaces:**
- Consumes: `fleet_peer_seed_persist::{load_doc_or_recover, FLEET_PEER_SEED_FILENAME}` + `fleet_peer_seed::seed_reachability_payload` (Task 2); in-scope `identity_dir` (`:3764`), `iroh_endpoint_arc`, `self_owner` (`:4715`), `reachability_resolver` (`:4404`).
- Produces: at boot, one `FleetSibling` resolver entry per non-self seed row.

- [ ] **Step 1: Add the boot-feed hook**

In `src-tauri/src/lib.rs`, immediately AFTER the step-1 sibling-seeding block closes (the `}` at `:5689`, before `fleet_net_doc_opt = Some(...)` at `:5690`), insert:

```rust
                    // ZEB-510 step 2: feed SAS first-contact seeds into the
                    // resolver so P can dial a freshly-paired sibling BEFORE
                    // fleet-net has ever converged. A real FleetNetDoc row for
                    // the same node (fed by the step-1 hook above) supersedes a
                    // seed via LWW once it exists; correctness does not depend on
                    // the ordering (same stable node_id either way).
                    {
                        let self_node_id =
                            iroh_endpoint_arc.as_ref().map(|ep| *ep.node_id().as_bytes());
                        let seed_path = identity_dir
                            .join(crate::fleet_peer_seed_persist::FLEET_PEER_SEED_FILENAME);
                        let seed_doc =
                            crate::fleet_peer_seed_persist::load_doc_or_recover(&seed_path)
                                .map_err(|e| format!("load fleet-peer-seed doc: {e}"))?;
                        for row in seed_doc.seeds.values() {
                            if Some(row.iroh_node_id) == self_node_id {
                                continue; // never seed ourselves
                            }
                            reachability_resolver.update_with_source(
                                self_owner,
                                crate::fleet_peer_seed::seed_reachability_payload(row),
                                crate::owner_state_types::Hlc {
                                    wall_ms: row.observed_at_ms,
                                    logical: 0,
                                    device_id: String::new(),
                                },
                                crate::reachability_resolver::ReachabilitySource::FleetSibling,
                            );
                        }
                    }
```

- [ ] **Step 2: Verify it compiles**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo check --locked --features test-fixtures 2>&1 | tail -5`
Expected: compiles clean. (`iroh_endpoint_arc`, `identity_dir`, `self_owner`, `reachability_resolver` are all confirmed in scope at `:5690` — the step-1 hook above uses `self_owner`/`reachability_resolver`, `identity_dir` is used at `:5513`, and `iroh_endpoint_arc` at `:5600`.)

- [ ] **Step 3: Run the fleet + reachability + pairing smoke tests**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(fleet_peer_seed) + test(fleet_net) + test(reachability) + test(pairing)' 2>&1 | tail -8`
Expected: PASS (no regressions; the hook is additive).

- [ ] **Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/lib.rs && git commit -m "feat(zeb-510): boot-feed SAS first-contact seeds into the resolver"
```

---

### Task 6: Validate end-to-end (s7 gate — the acceptance test)

**Files:**
- Modify (only if RECV/CLEARED pass): `e2e-harness/tests/e2e_two_node.rs` — `s7_butler_deposit_recover`

**IMPORTANT — this is the empirical acceptance gate.** s7's `HELD` boundary is already a hard assert (promoted in step 1). Step 2 aims to make it PASS. Two outcomes:
1. **s7 goes green** → step 2 works; the fix is complete. If `RECV`/`CLEARED` (still soft-characterize) also succeed co-located, promote them to hard asserts too (mirror the `HELD` promotion; replace each `poll_until(...).is_err()` soft-fallback with an `.expect(...)`). If they don't, leave them soft with the existing residual note.
2. **s7's `HELD` still times out** → step 2 did not converge the co-located deposit, meaning the gap is deeper than endpoint knowledge (a transport-layer issue: P dials B2 with the seeded endpoint but the iroh+zenoh link or fleet-net sync still doesn't establish co-located). **Do NOT weaken the assert.** Halt and surface to the controller/Jake — this is a new finding, not a defect to paper over.

- [ ] **Step 1: Run the full CI-parity sweep first (validates Tasks 1–5)**

Run:
```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri \
  && cargo fmt --all -- --check \
  && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
  && cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast
```
Expected: fmt clean, clippy clean, all tests pass. (This is the ~relink full sweep; budget for it and supervise.)

- [ ] **Step 2: Run s7 (the gate)**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/e2e-harness && HARMONY_E2E_KEEP=1 cargo nextest run --features e2e --test-threads 1 -E 'test(s7_butler_deposit_recover)' --no-fail-fast`
Do NOT pipe through `tee` and read the exit from the pipeline (pipe-exit-lie); read the nextest summary line for PASS/FAIL.
Expected — one of the two outcomes above. Record which.

- [ ] **Step 3 (only if RECV/CLEARED pass co-located): promote them + commit**

If s7 is green AND RECV/CLEARED succeed, promote them to hard asserts (same shape as the `HELD` promotion). Then:
```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add e2e-harness/tests/e2e_two_node.rs && git commit -m "test(zeb-510): promote s7 RECV/CLEARED to hard asserts"
```
Otherwise, no commit for this task — s7 already exercises the promoted `HELD` from step 1.

---

## Final gate (after all tasks)

The Task 6 Step 1 full sweep IS the final gate. Confirm it green, confirm the s7 result, then the branch is ready to open as ONE PR covering steps 1+2. Do NOT auto-merge.

## Out of scope (do NOT build)

- Cross-WAN pkarr rendezvous for never-SAS-paired siblings (~ZEB-513).
- TTL/expiry on seeds (design decision 4: none — LWW-supersession + node_id stability suffice).
- Any owner-state, community-membership, or DM-signing change beyond the pairing wire field.
