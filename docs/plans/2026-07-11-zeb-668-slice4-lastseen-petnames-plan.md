# ZEB-668 S4 — Per-device last-seen + fleet-synced petnames Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Surface each enrolled device's fleet-net `seen_at` heartbeat + live-connection status in the DevicesPanel, and add a fleet-synced LWW petname map so a rename on one device shows on all of the owner's devices.

**Architecture:** Last-seen is a pure read-path join — `FleetNetRow.seen_at` already exists and is heartbeat-stamped every ~7.5 min; `get_owner_state` joins it (plus a peer-liveness Connected check on the row's `iroh_endpoint_id`) into `DeviceView` by `device_vk_hex`. Petnames are the only new synced state: a `petnames: BTreeMap<String, FleetNetPetname>` LWW map on `FleetNetDoc` (additive, wire-compatible), mutated by a new `set_device_petname` IPC that copies the `set_butler_pin` template, and read through the same `DeviceView` join. The frontend label ladder becomes `petName ?? localStorage label (self only) ?? backend displayName`.

**Tech Stack:** Rust (Tauri 2, tokio, serde/ciborium canonical CBOR, FleetSyncEngine), Svelte 5 runes, vitest, cargo-nextest.

## Global Constraints

- Spec: `docs/specs/2026-07-11-zeb-668-device-management-design.md` §5 (S4). Honesty rule: `lastSeenMs: null` → render **nothing**; copy tolerates the ~7.5-min heartbeat cadence.
- Petnames live on `FleetNetDoc`, **NOT** inside `FleetNetRow` (rows are self-stamped by their device; petnames are assigned by any device about any device).
- Additive wire compat: new fields use `#[serde(default)]` + `skip_serializing_if` so an empty map encodes to the **byte-identical** old wire form. `EXPECTED_FLEET_NET_DOC_HEX` in `fleet_net.rs` must NOT be regenerated — it must keep passing untouched.
- IPC naming: Rust params `snake_case`, JS callers `camelCase` (`deviceVkHex`, `petname`).
- Empty/whitespace petname = **clear** (LWW entry with `name: ""`, never entry removal — removal breaks LWW convergence).
- `connectedNow` = the device's `iroh_endpoint_id` has a `LivenessStateWire::Connected` slot (NOT `Degraded`).
- Gates: `scripts/test-select --context task` per task (paste the `round=…/bucket=…` line), `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `npx tsc --noEmit` + `npx vitest run` for the UI task; full `cargo nextest run --locked --workspace --all-targets --features test-fixtures` before PR only.
- Commits end with the two standard trailers (Co-Authored-By + Claude-Session).

## Design decisions (locked)

1. **Petname merge = device-row semantics, not pin-pair semantics.** Per-key LWW by `set_at.is_strictly_newer_than`; absent key inserts unconditionally; any insert/replace flags `changed = true` (uniform with the `devices` map; avoids the pin pair's mutate-but-report-unchanged subtlety for a keyed map).
2. **Stamp seeding is per-entry:** `next_hlc(Some(&prev_entry.set_at), now_ms, self_device_id)` — the new stamp strictly exceeds anything this replica observed *for that key* (field-local recipe, same as `set_butler_pin_inner`).
3. **Validation target = known enrollment rows** (the same enrolled set `set_butler_pin` uses, boot snapshot ∪ live disk read). Enrollment rows survive revocation (S2), so revoked devices remain namable — their rows still render in the Removed section.
4. **No `routing_republish` / reachability republish on petname writes** — petnames don't affect butler selection (`selection_view` reads only `devices` + pin) or any network record. Instead the GUI wrapper emits `owner-devices-updated` on success so open panels refresh.
5. **Remote petname merges emit `owner-devices-updated`** from the existing fleet-net snapshot-refresh task (lib.rs ~8280) — gate: `prev_doc.petnames != new_doc.petnames`. Deliberately NOT emitted for `seen_at`-only churn (every sibling heartbeat would fire it; last-seen copy is minutes-granular and refreshes on panel open).
6. **localStorage migration is one-shot, user-label-only:** on panel mount, if the self row has no petname AND `loadDeviceLabel()` returns non-null (only true when the user explicitly named this machine — hostname defaults are never persisted), write it via `set_device_petname` best-effort. localStorage stays as a read-only fallback afterwards (spec §5).
7. **Presence line renders only for non-self active rows.** Self is trivially "online" (and peer-liveness doesn't track self); revoked rows skip it (their heartbeat is halted/stale — showing it adds noise, not honesty).
8. **`MAX_DEVICE_PETNAME_CHARS = 64`** server-side cap (chars, after trim), rejected with a plain string error.

## File map

- Modify: `src-tauri/src/fleet_net.rs` — `FleetNetPetname`, `FleetNetDoc.petnames`, merge, tests (T1)
- Modify: `src-tauri/src/fleet_net_persist.rs` — `sample_doc` gains a petname entry (T1)
- Modify: `src-tauri/src/lib.rs` — `set_device_petname_inner/_impl/command`, `generate_handler!` ×2, snapshot-task emit (T2, T3)
- Modify: `src-tauri/src/api/rpc.rs` — args struct + `rpc!` + method-name list (T2)
- Modify: `src-tauri/src/owner_state.rs` — `DeviceView` +3 fields (T3)
- Modify: `src-tauri/src/owner_commands.rs` — `FleetJoin`, snapshot read, `build_owner_state_view` join, tests (T3)
- Create: `src/lib/device-petname-service.ts` (T4)
- Modify: `src/lib/owner-service.ts`, `src/lib/components/DevicesPanel.svelte`, tests (T4)

---

### Task 1: `FleetNetDoc.petnames` LWW map (wire-compatible)

**Files:**
- Modify: `src-tauri/src/fleet_net.rs` (struct ~:49–86, merge ~:94–131, tests mod :321+)
- Modify: `src-tauri/src/fleet_net_persist.rs` (`sample_doc` ~:250)

**Interfaces:**
- Produces: `pub struct FleetNetPetname { pub name: String, pub set_at: Hlc }` (serde keys `n`/`st`); `FleetNetDoc.petnames: BTreeMap<String, FleetNetPetname>` (serde key `pt`, default, skip-if-empty); merge handles it per-key LWW. T2/T3 rely on these exact names.

- [ ] **Step 1: Write the failing tests** — append to `fleet_net.rs` tests mod (after the pin LWW tests). Add a local fixture + five merge/wire tests:

```rust
    fn petname(name: &str, set_at: Hlc) -> FleetNetPetname {
        FleetNetPetname {
            name: name.into(),
            set_at,
        }
    }

    // ── Petname LWW tests (ZEB-668 S4) ───────────────────────────────────────

    #[test]
    fn petname_lww_newer_remote_wins() {
        let mut local = FleetNetDoc::default();
        local
            .petnames
            .insert("dev-a".into(), petname("old", hlc(5, "dev-x")));
        let mut remote = FleetNetDoc::default();
        remote
            .petnames
            .insert("dev-a".into(), petname("new", hlc(10, "dev-y")));

        let out = local.merge_from(remote);
        assert!(out.changed);
        assert_eq!(local.petnames["dev-a"].name, "new");
        assert_eq!(local.petnames["dev-a"].set_at.wall_ms, 10);
    }

    #[test]
    fn petname_lww_older_remote_ignored_and_tie_keeps_local() {
        let mut local = FleetNetDoc::default();
        local
            .petnames
            .insert("dev-a".into(), petname("keep", hlc(10, "dev-x")));

        let mut older = FleetNetDoc::default();
        older
            .petnames
            .insert("dev-a".into(), petname("stale", hlc(5, "dev-x")));
        assert!(!local.merge_from(older).changed);
        assert_eq!(local.petnames["dev-a"].name, "keep");

        let mut tie = FleetNetDoc::default();
        tie.petnames
            .insert("dev-a".into(), petname("tie", hlc(10, "dev-x")));
        assert!(!local.merge_from(tie).changed);
        assert_eq!(local.petnames["dev-a"].name, "keep");
    }

    #[test]
    fn petname_absent_key_inserts_unconditionally() {
        let mut local = FleetNetDoc::default();
        let mut remote = FleetNetDoc::default();
        remote
            .petnames
            .insert("dev-b".into(), petname("KRILE", hlc(1, "dev-b")));
        assert!(local.merge_from(remote).changed);
        assert_eq!(local.petnames["dev-b"].name, "KRILE");
    }

    #[test]
    fn empty_petnames_map_is_omitted_from_wire_encoding() {
        // Additive wire compat: a doc with no petnames must encode WITHOUT the
        // "pt" key — byte-identical to the pre-S4 shape. (The pinned
        // EXPECTED_FLEET_NET_DOC_HEX fixture above is the cross-check: it
        // must keep passing untouched.)
        let mut doc = FleetNetDoc::default();
        doc.devices
            .insert("dev-a".into(), row(0x01, "relay.example.com", hlc(1, "dev-a")));
        let mut buf = Vec::new();
        ciborium::into_writer(&doc, &mut buf).expect("encode");
        let val: ciborium::Value = ciborium::from_reader(buf.as_slice()).expect("decode");
        let map = val.as_map().expect("top-level map");
        assert!(
            !map.iter()
                .any(|(k, _)| k.as_text() == Some("pt")),
            "empty petnames must be skip-serialized"
        );
    }

    #[test]
    fn petnames_round_trip_and_old_bytes_decode_to_empty_map() {
        // Round-trip with an entry present.
        let mut doc = FleetNetDoc::default();
        doc.petnames
            .insert("dev-a".into(), petname("KRILE", hlc(7, "dev-b")));
        let mut buf = Vec::new();
        ciborium::into_writer(&doc, &mut buf).expect("encode");
        let back: FleetNetDoc = ciborium::from_reader(buf.as_slice()).expect("decode");
        assert_eq!(back, doc);

        // Pre-S4 bytes (the pinned fixture hex) decode with petnames defaulted.
        let old = hex::decode(EXPECTED_FLEET_NET_DOC_HEX).expect("fixture hex");
        let decoded: FleetNetDoc = ciborium::from_reader(old.as_slice()).expect("decode old");
        assert!(decoded.petnames.is_empty());
    }
```

Note: `EXPECTED_FLEET_NET_DOC_HEX` is currently a fn-local `const` inside `fleet_net_doc_canonical_cbor_pinned` — hoist it to a tests-mod-level `const` (same value, moved verbatim) so the old-bytes test can reference it.

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app -E 'test(petname)' 2>&1 | tail -5`
Expected: compile FAIL (`FleetNetPetname` not found / no field `petnames`).

- [ ] **Step 3: Implement.** In `fleet_net.rs`:

After `FleetNetRow` (~:47), add:

```rust
/// A fleet-synced device petname (ZEB-668 S4). Assigned by ANY of the
/// owner's devices ABOUT any device — deliberately outside `FleetNetRow`
/// (rows are self-stamped by their subject device; petnames are not).
/// `name: ""` means "cleared" (kept as an LWW value so a clear replicates;
/// entry removal would not converge).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetNetPetname {
    #[serde(rename = "n")]
    pub name: String,
    /// LWW stamp; strictly-newer wins, ties keep local.
    #[serde(rename = "st")]
    pub set_at: Hlc,
}
```

In `FleetNetDoc`, after `pinned_at`:

```rust
    /// Fleet-synced device petnames (ZEB-668 S4), keyed like `devices` by SP1
    /// 64-hex device id. Per-key LWW by `set_at`. Additive: absent on the
    /// wire when empty, so pre-S4 payloads and peers are unaffected.
    #[serde(rename = "pt", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub petnames: BTreeMap<String, FleetNetPetname>,
```

`impl Default for FleetNetDoc`: add `petnames: BTreeMap::new(),`.

Register canonical-payload impls next to the existing ones:

```rust
impl CanonicalPayloadSealed for FleetNetPetname {}
impl CanonicalPayload for FleetNetPetname {}
```

In `merge_from`, after the pin LWW pair block (before `MergeOutcome { changed }`):

```rust
        // Petnames: per-key LWW by set_at — same shape as the device rows.
        for (device_id, remote_pn) in remote.petnames {
            match self.petnames.get(&device_id) {
                None => {
                    self.petnames.insert(device_id, remote_pn);
                    changed = true;
                }
                Some(local_pn) => {
                    if remote_pn.set_at.is_strictly_newer_than(&local_pn.set_at) {
                        self.petnames.insert(device_id, remote_pn);
                        changed = true;
                    }
                }
            }
        }
```

In `fleet_net_persist.rs` `sample_doc()`, extend so persistence round-trips cover the new field:

```rust
    fn sample_doc() -> FleetNetDoc {
        let mut doc = FleetNetDoc::default();
        doc.devices.insert("dev-a".into(), sample_row());
        doc.petnames.insert(
            "dev-a".into(),
            crate::fleet_net::FleetNetPetname {
                name: "sample".into(),
                set_at: Hlc {
                    wall_ms: 2,
                    logical: 0,
                    device_id: "dev-a".into(),
                },
            },
        );
        doc
    }
```

- [ ] **Step 4: Run tests + task gate**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app -E 'test(petname) | test(fleet_net)' --features test-fixtures 2>&1 | tail -5`
Expected: PASS including the untouched `fleet_net_doc_canonical_cbor_pinned`.
Then: `scripts/test-select --context task` (record the round/bucket line), `cargo fmt --all`, clippy per Global Constraints.

- [ ] **Step 5: Commit** — `git add -A && git commit` message `ZEB-668 S4 T1: FleetNetDoc petnames LWW map (wire-compatible additive field)` + trailers.

---

### Task 2: `set_device_petname` IPC (GUI + headless)

**Files:**
- Modify: `src-tauri/src/lib.rs` (new block after `set_butler_pin` wrapper ~:55691; `generate_handler!` lists ~:56108+ and the test builder list ~:56411 — add next to `set_butler_pin`)
- Modify: `src-tauri/src/api/rpc.rs` (args struct near `SetButlerPinArgs`, `rpc!` entry ~:930, method-name list ~:1822)

**Interfaces:**
- Consumes: `FleetNetDoc.petnames` + `FleetNetPetname` (T1); `crate::dm_outbox::next_hlc(prev, now_ms, device_id)`.
- Produces: `pub(crate) async fn set_device_petname_impl(state: &Mutex<NodeState>, device_vk_hex: String, petname: String) -> Result<(), String>` (T4's frontend + headless RPC both call through it); `pub(crate) const MAX_DEVICE_PETNAME_CHARS: usize = 64`.

- [ ] **Step 1: Write the failing tests.** Find the existing `set_butler_pin_inner` tests (`grep -n "set_butler_pin_inner" src-tauri/src/lib.rs` — tests live in the same tests mod) and add beside them:

```rust
    #[tokio::test]
    async fn set_device_petname_inner_sets_trimmed_name_with_monotonic_stamp() {
        let doc = tokio::sync::Mutex::new(crate::fleet_net::FleetNetDoc::default());
        let enrolled: std::collections::BTreeSet<String> =
            [String::from("aa").repeat(32)].into_iter().collect();
        let dev = "aa".repeat(32);

        set_device_petname_inner(&doc, &enrolled, dev.clone(), "  KRILE  ".into(), "self-dev", 1000)
            .await
            .expect("first set");
        {
            let g = doc.lock().await;
            let pn = &g.petnames[&dev];
            assert_eq!(pn.name, "KRILE");
            assert_eq!(pn.set_at.wall_ms, 1000);
        }
        // Second write with a REGRESSED wall clock must still strictly exceed
        // the prior stamp (next_hlc bumps logical).
        set_device_petname_inner(&doc, &enrolled, dev.clone(), "AVALON".into(), "self-dev", 500)
            .await
            .expect("second set");
        let g = doc.lock().await;
        let pn = &g.petnames[&dev];
        assert_eq!(pn.name, "AVALON");
        assert!(pn.set_at.is_strictly_newer_than(&crate::owner_state_types::Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "self-dev".into(),
        }));
    }

    #[tokio::test]
    async fn set_device_petname_inner_empty_clears_and_unknown_rejects() {
        let doc = tokio::sync::Mutex::new(crate::fleet_net::FleetNetDoc::default());
        let dev = "bb".repeat(32);
        let enrolled: std::collections::BTreeSet<String> = [dev.clone()].into_iter().collect();

        set_device_petname_inner(&doc, &enrolled, dev.clone(), "Koya".into(), "self-dev", 10)
            .await
            .unwrap();
        // Whitespace-only → clear: entry kept, name emptied (LWW tombstone).
        set_device_petname_inner(&doc, &enrolled, dev.clone(), "   ".into(), "self-dev", 20)
            .await
            .unwrap();
        assert_eq!(doc.lock().await.petnames[&dev].name, "");

        let err = set_device_petname_inner(&doc, &enrolled, "cc".repeat(32), "x".into(), "self-dev", 30)
            .await
            .unwrap_err();
        assert!(err.contains("not in the enrolled device set"), "{err}");
    }

    #[tokio::test]
    async fn set_device_petname_inner_rejects_over_cap() {
        let doc = tokio::sync::Mutex::new(crate::fleet_net::FleetNetDoc::default());
        let dev = "dd".repeat(32);
        let enrolled: std::collections::BTreeSet<String> = [dev.clone()].into_iter().collect();
        let long = "x".repeat(MAX_DEVICE_PETNAME_CHARS + 1);
        let err = set_device_petname_inner(&doc, &enrolled, dev.clone(), long, "self-dev", 10)
            .await
            .unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
        assert!(doc.lock().await.petnames.is_empty(), "rejected write must not mutate");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app -E 'test(set_device_petname)' 2>&1 | tail -5`
Expected: compile FAIL (`set_device_petname_inner` not found).

- [ ] **Step 3: Implement.** In `lib.rs`, directly after the `set_butler_pin` `#[tauri::command]` wrapper (~:55691), add a `// ── ZEB-668 S4: set_device_petname ──…` section:

```rust
/// Server-side petname length cap (chars, after trim). UI enforces the same.
pub(crate) const MAX_DEVICE_PETNAME_CHARS: usize = 64;

/// Core of `set_device_petname`, extracted for testability (mirrors
/// `set_butler_pin_inner`). Empty/whitespace `petname` CLEARS: the entry is
/// kept with `name: ""` so the clear replicates by LWW (removal would not
/// converge). The stamp is seeded from the entry's own prior stamp so it
/// strictly exceeds anything this replica has observed for that key.
pub(crate) async fn set_device_petname_inner(
    doc: &tokio::sync::Mutex<crate::fleet_net::FleetNetDoc>,
    enrolled: &std::collections::BTreeSet<String>,
    device_vk_hex: String,
    petname: String,
    self_device_id: &str,
    now_ms: u64,
) -> Result<(), String> {
    if !enrolled.contains(&device_vk_hex) {
        return Err(format!(
            "set_device_petname: device '{device_vk_hex}' is not in the enrolled device set"
        ));
    }
    let trimmed = petname.trim();
    if trimmed.chars().count() > MAX_DEVICE_PETNAME_CHARS {
        return Err(format!(
            "set_device_petname: petname exceeds {MAX_DEVICE_PETNAME_CHARS} characters"
        ));
    }
    let mut guard = doc.lock().await;
    let prev = guard.petnames.get(&device_vk_hex).map(|p| p.set_at.clone());
    let new_stamp = crate::dm_outbox::next_hlc(prev.as_ref(), now_ms, self_device_id);
    guard.petnames.insert(
        device_vk_hex,
        crate::fleet_net::FleetNetPetname {
            name: trimmed.to_string(),
            set_at: new_stamp,
        },
    );
    Ok(())
}
```

Then `set_device_petname_impl` — copy `set_butler_pin_impl` (:55553–55679) verbatim with these deltas: params `(state, device_vk_hex: String, petname: String)`; every `"set_butler_pin:"` message prefix becomes `"set_device_petname:"`; the snapshot tuple **drops** `routing_republish` (petnames are selection-irrelevant); call `set_device_petname_inner(&fleet_net_doc_arc, &enrolled, device_vk_hex, petname, &self_device_id, now_ms).await?`; keep the ZEB-491 live-enrolled union block, the snapshot re-clone under the doc lock, `notify_dirty()`, and the `flush_now()` log-warn — but **omit** the trailing `routing_republish` and `force_reachability_republish` calls (add a short comment: petnames feed only the Devices panel; nothing network-advertised changes).

GUI wrapper (emits so any open panel refreshes — local writes never fire `on_applied`):

```rust
/// ZEB-668 S4: set or clear a fleet-synced device petname. Shared by GUI +
/// headless RPC via `set_device_petname_impl`; the GUI wrapper additionally
/// emits `owner-devices-updated` (local writes bypass `on_applied`).
#[tauri::command]
async fn set_device_petname(
    device_vk_hex: String,
    petname: String,
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    set_device_petname_impl(state.inner(), device_vk_hex, petname).await?;
    if let Err(e) = tauri::Emitter::emit(&app, "owner-devices-updated", ()) {
        tracing::warn!(error = %e, "set_device_petname: event emit failed");
    }
    Ok(())
}
```

(Check the file's existing emit idiom first — `grep -n '\.emit(' src-tauri/src/lib.rs | head` — and match it; if commands emit via `app.emit(...)` directly, do that.)

Registration:
1. `generate_handler![…]` prod list: add `set_device_petname,` next to `set_butler_pin`.
2. Test/headless builder list (~:56411): same.
3. `api/rpc.rs`: next to `SetButlerPinArgs` add (match the file's args-struct idiom — likely `#[derive(Deserialize)] #[serde(rename_all = "camelCase")]`):

```rust
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetDevicePetnameArgs {
    device_vk_hex: String,
    petname: String,
}
```

`rpc!` entry after `set_butler_pin`'s:

```rust
    rpc!(
        m,
        "set_device_petname",
        SetDevicePetnameArgs,
        |state, _sink, a| async move {
            crate::set_device_petname_impl(state, a.device_vk_hex, a.petname).await
        }
    );
```

And add `"set_device_petname",` to the method-name list (~:1822, after the butler rung block, tagged `// device management (ZEB-668 S4)` beside the existing S2 entry).

- [ ] **Step 4: Run tests + task gate**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app -E 'test(set_device_petname)' --features test-fixtures 2>&1 | tail -5` → PASS.
Then `scripts/test-select --context task`, fmt, clippy.

- [ ] **Step 5: Commit** — `ZEB-668 S4 T2: set_device_petname IPC (GUI + headless RPC)`.

---

### Task 3: read-path join — `DeviceView.{petName,lastSeenMs,connectedNow}` + remote-merge emit

**Files:**
- Modify: `src-tauri/src/owner_state.rs` (`DeviceView` ~:14–55)
- Modify: `src-tauri/src/owner_commands.rs` (`build_owner_state_view` :188–248, `get_owner_state_inner` :304–408, tests mod)
- Modify: `src-tauri/src/lib.rs` (fleet-net snapshot-refresh task ~:8280–8360)

**Interfaces:**
- Consumes: `FleetNetDoc.petnames` (T1); `NodeState.reachability_resolver` → `ReachabilityResolver::liveness() -> Option<LivenessHandle>` → `states_snapshot() -> Vec<([u8;32], LivenessStateWire)>`.
- Produces: `DeviceView` fields `pet_name: Option<String>`, `last_seen_ms: Option<u64>`, `connected_now: bool` (camelCase `petName`/`lastSeenMs`/`connectedNow` — T4 relies on these exact wire keys); `struct FleetJoin` in owner_commands.rs.

- [ ] **Step 1: Write the failing tests.** In `owner_commands.rs`, find the existing `build_owner_state_view` test fixtures (S2 added butler-pin join tests — `grep -n "build_owner_state_view" src-tauri/src/owner_commands.rs`) and add, following the same `LoadedOwnerState` fixture idiom the existing tests use:

```rust
    #[test]
    fn view_joins_petname_last_seen_and_connected() {
        // Reuse the existing loaded-state fixture from the butler-pin tests.
        let loaded = /* same fixture the neighboring tests construct */;
        let dev_vk_hex = /* hex::encode of the fixture cert's ed25519_verify — same
                            derivation the neighboring butler_pinned test uses */;
        let ep = [0x42u8; 32];
        let mut fleet = FleetJoin::default();
        fleet.petnames.insert(dev_vk_hex.clone(), "KRILE".into());
        fleet.rows.insert(dev_vk_hex.clone(), (123_456, ep));
        fleet.connected_eps.insert(ep);

        let view = build_owner_state_view(&loaded, "this device".into(), fleet);
        let d = view
            .devices
            .iter()
            .find(|d| d.device_vk_hex == dev_vk_hex)
            .unwrap();
        assert_eq!(d.pet_name.as_deref(), Some("KRILE"));
        assert_eq!(d.last_seen_ms, Some(123_456));
        assert!(d.connected_now);
    }

    #[test]
    fn view_absent_fleet_row_yields_honest_nulls() {
        let loaded = /* same fixture */;
        let view = build_owner_state_view(&loaded, "this device".into(), FleetJoin::default());
        let d = &view.devices[0];
        assert_eq!(d.pet_name, None);
        assert_eq!(d.last_seen_ms, None);
        assert!(!d.connected_now);
    }

    #[test]
    fn view_empty_petname_entry_reads_as_none() {
        // A cleared petname (name: "") must surface as None, not Some("").
        let loaded = /* same fixture */;
        let dev_vk_hex = /* as above */;
        let mut fleet = FleetJoin::default();
        fleet.petnames.insert(dev_vk_hex.clone(), String::new());
        let view = build_owner_state_view(&loaded, "this device".into(), fleet);
        let d = view.devices.iter().find(|d| d.device_vk_hex == dev_vk_hex).unwrap();
        assert_eq!(d.pet_name, None);
    }
```

(The `/* same fixture */` placeholders are resolved by copying the construction lines from the immediately-neighboring butler-pin join test in that mod — do not invent a new fixture.) Decide there whether the empty-string→None mapping lives in `FleetJoin` population (preferred: don't put `""` in the map at all) or in the loop — the test pins the observable either way; put the filter at map-population time in `get_owner_state_inner` AND defensively `.filter(|s| !s.is_empty())` in the loop.

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app -E 'test(view_joins_petname) | test(view_absent_fleet) | test(view_empty_petname)' 2>&1 | tail -5`
Expected: compile FAIL (`FleetJoin` not found / unknown fields).

- [ ] **Step 3: Implement.**

`owner_state.rs` — append to `DeviceView` (after `revoked_reason`):

```rust
    /// ZEB-668 S4: fleet-synced petname (LWW). None = never named / cleared.
    #[serde(default)]
    pub pet_name: Option<String>,
    /// ZEB-668 S4: wall-clock ms of the device's last fleet-net heartbeat
    /// (`FleetNetRow.seen_at`, ~7.5-min cadence). None = never fleet-synced —
    /// the panel renders NOTHING (honesty rule), never a fabricated time.
    #[serde(default)]
    pub last_seen_ms: Option<u64>,
    /// ZEB-668 S4: true iff the device's iroh endpoint currently holds a
    /// Connected peer-liveness slot (Degraded does not count).
    #[serde(default)]
    pub connected_now: bool,
```

`owner_commands.rs` — above `build_owner_state_view`:

```rust
/// ZEB-668 S4: everything `build_owner_state_view` joins from the fleet-net
/// doc + peer liveness, snapshotted in async context before the blocking task.
#[derive(Default)]
pub(crate) struct FleetJoin {
    /// 64-hex fleet `pinned` value (butler pin).
    pub pinned: Option<String>,
    /// device_vk_hex → non-empty petname (cleared entries are filtered out).
    pub petnames: std::collections::BTreeMap<String, String>,
    /// device_vk_hex → (seen_at.wall_ms, iroh_endpoint_id).
    pub rows: std::collections::BTreeMap<String, (u64, [u8; 32])>,
    /// Endpoint ids with a live Connected liveness slot.
    pub connected_eps: std::collections::BTreeSet<[u8; 32]>,
}
```

Change `build_owner_state_view(loaded, this_device_name, pinned_device_id_hex: Option<String>)` → `(loaded, this_device_name, fleet: FleetJoin)`. Inside the loop replace the `butler_pinned` computation's source (`fleet.pinned` instead of `pinned_device_id_hex`) and add before the `DeviceView` literal:

```rust
            let pet_name = fleet
                .petnames
                .get(&dev_id_hex)
                .filter(|s| !s.is_empty())
                .cloned();
            let (last_seen_ms, connected_now) = match fleet.rows.get(&dev_id_hex) {
                Some((ms, ep)) => (Some(*ms), fleet.connected_eps.contains(ep)),
                None => (None, false),
            };
```

and the three fields in the literal. In `get_owner_state_inner`, replace the `pinned_device_id_hex` snapshot block (:312–323) with one that also captures the resolver, builds the whole `FleetJoin`, and pass `fleet` (it is `Send`; for the two `build_owner_state_view` call sites, move it into the respective closure — the resident path uses it directly, the file-only path moves it into `run_blocking`):

```rust
    let fleet: FleetJoin = {
        let (fleet_net_doc_arc, resolver) = {
            let g = state
                .lock()
                .map_err(|e| format!("NodeState poisoned: {e}"))?;
            (g.fleet_net_doc.clone(), g.reachability_resolver.clone())
        };
        let mut fleet = FleetJoin::default();
        if let Some(arc) = fleet_net_doc_arc {
            let doc = arc.lock().await;
            fleet.pinned = doc.pinned.clone();
            for (id, row) in &doc.devices {
                fleet
                    .rows
                    .insert(id.clone(), (row.seen_at.wall_ms, row.iroh_endpoint_id));
            }
            for (id, pn) in &doc.petnames {
                if !pn.name.is_empty() {
                    fleet.petnames.insert(id.clone(), pn.name.clone());
                }
            }
        }
        if let Some(h) = resolver.and_then(|r| r.liveness()) {
            for (ep, st) in h.states_snapshot() {
                if matches!(
                    st,
                    crate::peer_liveness::LivenessStateWire::Connected { .. }
                ) {
                    fleet.connected_eps.insert(ep);
                }
            }
        }
        fleet
    };
```

`lib.rs` snapshot-refresh task (~:8285): clone the event sink into the task (`let task_emit = std::sync::Arc::clone(&app);` beside the other `task_*` clones — same `app` the trust detector's emit closure clones at :5484) and, in the nudge branch right before `prev_doc = new_doc;`:

```rust
                                        // ZEB-668 S4: a remote petname merge
                                        // must live-refresh an open Devices
                                        // panel. seen_at-only churn is
                                        // deliberately excluded (every sibling
                                        // heartbeat would fire it).
                                        if prev_doc.petnames != new_doc.petnames {
                                            crate::node_event_sink::emit_ser(
                                                &*task_emit,
                                                "owner-devices-updated",
                                                &serde_json::Value::Null,
                                            );
                                        }
```

- [ ] **Step 4: Run tests + task gate**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app -E 'test(view_) | test(owner_state)' --features test-fixtures 2>&1 | tail -5` → PASS (plus any pre-existing callers of `build_owner_state_view` fixed to pass `FleetJoin`).
Then `scripts/test-select --context task`, fmt, clippy.

- [ ] **Step 5: Commit** — `ZEB-668 S4 T3: DeviceView last-seen/connected/petname join + remote petname emit`.

---

### Task 4: frontend — label ladder, rename-via-petname, presence line

**Files:**
- Create: `src/lib/device-petname-service.ts`
- Modify: `src/lib/owner-service.ts` (DTO :10–33)
- Modify: `src/lib/components/DevicesPanel.svelte` (overlay :107–117, rename :170–195, row markup :565–625, helpers :451+)
- Test: `src/lib/components/__tests__/DevicesPanel.test.ts`, `src/lib/device-petname-service.test.ts`

**Interfaces:**
- Consumes: wire keys `petName` / `lastSeenMs` / `connectedNow` (T3); IPC `set_device_petname { deviceVkHex, petname }` (T2).
- Produces: `setDevicePetname(deviceVkHex: string, petname: string): Promise<void>`.

- [ ] **Step 1: Write the failing tests.** `src/lib/device-petname-service.test.ts` (mirror the repo's service-test idiom — check `src/lib/device-label-service.test.ts` for the mock setup):

```typescript
import { describe, expect, it, vi } from 'vitest';

const invokeMock = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({ invoke: (...a: unknown[]) => invokeMock(...a) }));

import { setDevicePetname } from './device-petname-service';

describe('device-petname-service', () => {
  it('invokes set_device_petname with camelCase args', async () => {
    invokeMock.mockResolvedValueOnce(undefined);
    await setDevicePetname('ab'.repeat(32), 'KRILE');
    expect(invokeMock).toHaveBeenCalledWith('set_device_petname', {
      deviceVkHex: 'ab'.repeat(32),
      petname: 'KRILE',
    });
  });

  it('clears with an empty string', async () => {
    invokeMock.mockResolvedValueOnce(undefined);
    await setDevicePetname('ab'.repeat(32), '');
    expect(invokeMock).toHaveBeenCalledWith('set_device_petname', {
      deviceVkHex: 'ab'.repeat(32),
      petname: '',
    });
  });
});
```

DevicesPanel tests (extend `__tests__/DevicesPanel.test.ts`, reusing its existing mount/mocking helpers — the file already mocks `get_owner_state`): (a) a device with `petName: 'Ildwyn'` renders "Ildwyn" as the row name even when a localStorage label exists and it's not the self row; (b) a non-self row with `connectedNow: true` shows the online badge; (c) `connectedNow: false, lastSeenMs: Date.now() - 2*3600_000` renders text containing "last seen" and "~2h"; (d) `lastSeenMs: null` renders neither; (e) clicking Rename on a **sibling** row now works and Save invokes `set_device_petname` with that row's `deviceVkHex`. Follow the file's existing mock-view fixtures — every new DeviceView literal needs the three new fields.

- [ ] **Step 2: Run to verify failure**

Run: `npx vitest run src/lib/device-petname-service.test.ts 2>&1 | tail -5`
Expected: FAIL (module not found).

- [ ] **Step 3: Implement.**

`src/lib/device-petname-service.ts` (clone of butler-pin-service shape):

```typescript
/**
 * ZEB-668 S4 — fleet-synced device petname IPC wrapper.
 *
 * Thin wrapper around the `set_device_petname` Tauri command. LWW per
 * device: the newest write wins fleet-wide. Empty string clears.
 */

import { invoke } from '@tauri-apps/api/core';

/** Max petname length (chars, post-trim) — mirrors the backend cap. */
export const MAX_DEVICE_PETNAME_CHARS = 64;

/** Set or clear (empty string) the petname for any of the owner's devices. */
export async function setDevicePetname(deviceVkHex: string, petname: string): Promise<void> {
  await invoke('set_device_petname', { deviceVkHex, petname });
}
```

`owner-service.ts` `DeviceView` additions (after `revokedReason`):

```typescript
  /**
   * ZEB-668 S4: fleet-synced petname (LWW; null = never named/cleared).
   * Wins the label ladder over the local device label.
   */
  petName: string | null;
  /**
   * ZEB-668 S4: wall-ms of the last fleet-net heartbeat (~7.5 min cadence).
   * null = never fleet-synced → render NOTHING (honesty rule).
   */
  lastSeenMs: number | null;
  /** ZEB-668 S4: live Connected transport slot for this device right now. */
  connectedNow: boolean;
```

`DevicesPanel.svelte`:

1. **Label ladder** — in `applyLocalOverlay`, replace the devices map with:

```typescript
      devices: view.devices.map((d) => {
        // ZEB-668 S4 ladder: petName ?? local label (self only, pre-S4
        // fallback) ?? backend displayName ("Device xxxxxxxx").
        const pet = d.petName?.trim();
        if (pet) return { ...d, displayName: pet };
        if (d.isThisDevice && deviceLabel) return { ...d, displayName: deviceLabel };
        return d;
      }),
```

2. **Rename → petname IPC, all rows.** Replace `saveRename` (async now; keep localStorage write for the self row as the read-only fallback the spec keeps alive) and render the Rename button for every active row (move it out of the `isThisDevice` branch so both branches have it), disabled while in flight:

```typescript
  let renameError = $state<string | null>(null);
  let renameInFlight = $state(false);

  async function saveRename(device: DeviceView) {
    const trimmed = renameDraft.trim();
    if (trimmed.length === 0 || trimmed.length > MAX_DEVICE_PETNAME_CHARS || renameInFlight) return;
    renameError = null;
    renameInFlight = true;
    try {
      await setDevicePetname(device.deviceVkHex, trimmed);
      if (device.isThisDevice) {
        // Keep the pre-S4 local label in step as the offline fallback.
        saveDeviceLabel(trimmed);
        deviceLabel = trimmed;
      }
      renamingDeviceId = null;
      await svc.refresh();
    } catch (e) {
      renameError = extractError(e);
    } finally {
      renameInFlight = false;
    }
  }
```

(`startRename` seeds `renameDraft` from the row's current `displayName` — unchanged. Update the two markup call sites `saveRename(device.deviceId)` → `saveRename(device)`; render `{#if renameError}` copy near the input, mirroring `butlerPinError`'s markup.)

3. **One-shot migration** — in the existing `onMount` after the initial `svc.refresh()` succeeds:

```typescript
    // ZEB-668 S4: one-shot migration — a user-set pre-S4 local label seeds
    // the fleet petname map so siblings stop showing "Device xxxxxxxx".
    // Best-effort (node may be down); idempotent (guard is false once set).
    const selfDev = svc.state?.devices.find((d) => d.isThisDevice);
    if (selfDev && !selfDev.petName && deviceLabel) {
      try {
        await setDevicePetname(selfDev.deviceVkHex, deviceLabel);
        await svc.refresh();
      } catch {
        // Non-fatal: retried on next panel open.
      }
    }
```

4. **Presence line** — helper beside `formatEnrolledAt`:

```typescript
  // ZEB-668 S4: heartbeat-tolerant relative time (~7.5-min stamp cadence —
  // hence "just now" out to 10 min and the "~" prefix; honesty ledger).
  function formatLastSeen(ms: number): string {
    const min = Math.floor((Date.now() - ms) / 60000);
    if (min < 10) return 'just now';
    if (min < 60) return `~${min}m ago`;
    const h = Math.floor(min / 60);
    if (h < 24) return `~${h}h ago`;
    const d = Math.floor(h / 24);
    if (d < 30) return `${d}d ago`;
    return new Date(ms).toLocaleDateString();
  }
```

In the `device-secondary` div after the `added …` span, non-self rows only:

```svelte
                {#if !device.isThisDevice}
                  {#if device.connectedNow}
                    <span class="separator">·</span>
                    <span class="online-badge">● online</span>
                  {:else if device.lastSeenMs !== null}
                    <span class="separator">·</span>
                    <span>last seen {formatLastSeen(device.lastSeenMs)}</span>
                  {/if}
                {/if}
```

Style `.online-badge` with the panel's existing token idiom (match `.trust-badge.full`'s color source — app.css tokens only, no raw hex; the style-token guard enforces this).

- [ ] **Step 4: Run tests + gates**

Run: `npx vitest run 2>&1 | tail -5` → PASS; `npx tsc --noEmit` → clean. Fix any pre-existing DeviceView literals in tests missing the three new fields.
Then `scripts/test-select --context task`, fmt, clippy (owner_commands tests changed in T3 may rotate in).

- [ ] **Step 5: Commit** — `ZEB-668 S4 T4: DevicesPanel petname ladder, rename-via-fleet, presence line`.

---

### Final pre-PR sweep

- [ ] `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` (full; ~20–25 min)
- [ ] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` + `cargo fmt --all -- --check`
- [ ] `npx tsc --noEmit && npx vitest run`
- [ ] Open PR (body: slice summary, spec §5 link, honesty-ledger notes, gates evidence), fire `@coderabbitai review` once.

## Self-review notes

- Spec §5 coverage: read seam (T3), UI relative-time + null-renders-nothing (T4), `connectedNow` (T3/T4), petname LWW map additive on FleetNetDoc not FleetNetRow (T1), IPC + empty-clears (T2), `DeviceView.petName` + label ladder + localStorage migration/read-only fallback (T3/T4). No gaps.
- Type consistency: `FleetNetPetname{name,set_at}` (T1) ↔ `set_device_petname_inner` insert (T2) ↔ `FleetJoin.petnames` filter (T3) ↔ `petName` wire key (T4). `saveRename(device)` signature updated at both call sites.
- Known intentional deviations: presence line hidden on self + revoked rows (decision 7); seen_at churn excluded from the live-refresh emit (decision 5).
