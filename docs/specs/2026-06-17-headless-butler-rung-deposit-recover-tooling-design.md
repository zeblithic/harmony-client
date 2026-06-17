# Headless butler-rung deposit→recover tooling — design (ZEB-489)

**Status:** approved 2026-06-17. Fast-follow to ZEB-487 (headless relay-rung tooling, merged `882dffec`). Branch `zeb-489-headless-butler-rung-deposit-recover` off `main` `882dffec`. Single-repo (harmony-client).

## 1. Goal

Make the offline-at-create → deposit → recover DM durability path (ZEB-483) testable and inspectable **headlessly** when the deposit lands on the recipient's own **butler** device (ZEB-418), exactly as ZEB-487 did for the community **relay** rung. The butler rung is real product surface (a headless always-on butler — e.g. a home server — is a genuine deployment), not throwaway scaffolding.

Deposit and recovery already fire **automatically**: the butler acceptor (`iroh_butler_acceptor.rs`) is always-on for the owner's enrolled devices (no enable flag), and recovery is an inbox sweep on startup + a debounced nudge on each fleet merge (`dm_inbox_ingest.rs`), bootstrapping the DM Space from `apply_deposited_invite` (ZEB-483). The only gaps are headless **control** (designate a butler) and **observability** (inspect what the butler holds). This design adds **no** transport, verify, deposit, or recovery logic.

## 2. Non-goals

- No change to deposit/recovery/verify/CRDT logic.
- **No co-located harness scenario.** Unlike the relay (any node opts into a community), the butler must be the recipient's own second enrolled device in one fleet, which in a multi-process harness needs real device pairing (ZEB-446 SAS RPCs) the harness has no helpers for — and s6 already showed co-located deposit-routing hits the ZEB-488 gap. A reusable harness pairing helper + a co-located `s7` are a deliberate follow-up. The authoritative proof here is the cross-WAN playbook Scenario D3.
- No GUI/frontend changes (the three commands' GUI behavior is preserved exactly via the shared `*_impl`).

## 3. Architecture — three curated headless RPCs

Each RPC routes through an extracted `*_impl(&Mutex<NodeState>)` core that the existing Tauri GUI command also calls, so GUI and headless observe identical behavior and error strings (the `connectivity_redeem_invite_iroh_impl` pattern; same shape ZEB-487 used).

### 3.1 `set_butler_pin` — promote (extract `*_impl`)

Today: `#[tauri::command] async fn set_butler_pin(device_id: Option<String>, state)` (`lib.rs:43828`) snapshots handles from `NodeState` (`fleet_net_doc`, `fleet_net_sync`, `fleet_net_snapshot`, `fleet_net_enrolled`, `fleet_net_device_id`, `routing_republish`), then calls `set_butler_pin_inner(doc, enrolled, device_id, self_device_id, now_ms)` (`lib.rs:43797`), updates the sync snapshot, notifies the engine, flushes, and fires the routing-republish trigger.

Change: extract the command body into `pub(crate) async fn set_butler_pin_impl(state: &Mutex<NodeState>, device_id: Option<String>) -> Result<(), String>`. The Tauri command becomes a thin wrapper: `set_butler_pin_impl(state.inner(), device_id).await`. `set_butler_pin_inner` is untouched. `device_id = Some(hex)` pins; `None` clears. Validation (target in enrolled set) is preserved inside `_inner`.

### 3.2 `get_butler_pin` — new status reader

`pub(crate) async fn get_butler_pin_impl(state: &Mutex<NodeState>) -> Result<ButlerPinStatus, String>`. Snapshot the `fleet_net_doc` Arc from the `NodeState` std-`Mutex` (drop the guard), then `fleet_net_doc.lock().await`, read `pinned: Option<String>` and `pinned_at: Hlc`. Returns:

```rust
#[serde(rename_all = "camelCase")]
struct ButlerPinStatus { pinned_device_id: Option<String>, pinned_at_ms: u64 }
```

No getter exists today (status is read off `fleet_net_doc.pinned`). Errors `"get_butler_pin: fleet-net not running (node not started)"` when the handle is absent, matching `set_butler_pin`'s phrasing.

### 3.3 `get_butler_held` — new observability

`pub(crate) async fn get_butler_held_impl(state: &Mutex<NodeState>) -> Result<ButlerHeldResponse, String>`. Snapshot the `dm_inbox_doc` Arc (`NodeState.dm_inbox_doc`, `lib.rs:1025`), drop the `NodeState` guard, then `dm_inbox_doc.lock().await` and `map_butler_held(&guard)` — the map is sync (no `.await` while the inbox lock is held), so the hold is brief and `await_holding_lock` passes (identical to `get_relay_held_impl`). Returns `{ held: ButlerHeldEntryDto[] }`. Errors `"get_butler_held: dm-inbox not running (node not started)"` when the handle is absent.

## 4. Data — `butler_held_dto.rs` (new module, mirrors `relay_held_dto.rs`)

The butler inbox is `DmInboxDoc` (`dm_inbox_crdt.rs:44`), `entries: BTreeMap<String, DmInboxEntry>` keyed `"{space_id_hex}:{message_cid_hex}"`. Each `DmInboxEntry` carries `sender_owner: [u8;16]`, `deposited_at: Hlc`, `deposited_by: String` (depositing device id), `ingested_by: BTreeSet<String>` (grow-only ack set), plus the sealed/bulky payload (`cidnotify_packet`, `storage_blob`, `invite_packet`).

```rust
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

pub struct ButlerHeldResponse { pub held: Vec<ButlerHeldEntryDto> }

pub fn map_butler_held(doc: &DmInboxDoc) -> Vec<ButlerHeldEntryDto>
```

The mapper splits the entry key on the **first** `':'` into `(space_id_hex, message_cid_hex)` (both pure hex, so the first colon is the unambiguous separator; a malformed key falls back to `(key, "")`), hex-encodes `sender_owner`, projects `deposited_at.wall_ms` and `deposited_by`, and collects `ingested_by` into a `Vec<String>`. It **never** reads `cidnotify_packet` / `storage_blob` / `invite_packet`.

This DTO is intentionally **richer** than `RelayHeldEntryDto`: because the butler is the recipient's own fleet device (not an opaque third-party relay), the entry key exposes `space_id`+`message_cid`, and `ingested_by_devices` is the built-in "recovered/cleared" signal — when the offline primary's device id appears in the set, it has recovered the deposit; when `ingested_by ⊇ enrolled`, GC removes the entry.

## 5. RPC registration (`api/rpc.rs`)

- `set_butler_pin` → `SetButlerPinArgs { device_id: Option<String> }` (camelCase `deviceId`), handler calls `set_butler_pin_impl(state, args.device_id)`.
- `get_butler_pin` → no args (mirror an existing no-arg RPC's registration, e.g. `get_owner_state`), handler calls `get_butler_pin_impl(state)`.
- `get_butler_held` → no args, handler calls `get_butler_held_impl(state)`.

Add the three names to the allowlist test `registry_has_exactly_the_curated_v1_surface` (46 → **49**) and bump the `build_registry` doc-comment count (`46` → `49`). The allowlist test is the red→green lever.

## 6. Cross-WAN proof — playbook Scenario D3

Append Scenario D3 to `docs/playbooks/e2e-two-agent-suite.md` (agent-driven, mirrors D2): **Ildwyn = sender A**; **AVALON = recipient**, running two local profiles — primary `P` and butler `B2` — paired into one fleet via the ZEB-446 pairing RPCs, then `set_butler_pin(B2's device id)`. Flow:

1. AVALON: mint `P`; pair `B2` into P's fleet (`start_inviter_pairing`/`start_joiner_pairing` + SAS confirm); `get_pairing_state` until enrolled; `set_butler_pin(B2)`; `get_butler_pin` confirms.
2. A friends P; `add_space` (DM) with P; **kill P (real PID kill)**.
3. A `send_dm` while P is offline → deposit lands on B2.
4. B2: `get_butler_held` shows the entry — **HELD** while offline (sender/space/cid metadata; `ingestedByDevices` does not yet contain P).
5. Relaunch P → auto-recovers (startup sweep + fleet merge). B2: `get_butler_held` shows `ingestedByDevices` now contains P (or the entry GC'd) — **CLEARED**. P: `read_dm_thread` shows A's plaintext — **RECV**.

Baseline is 2 machines with local pairing on AVALON (sidesteps cross-WAN pairing); a 3-machine variant (butler on Koya) is noted as optional. This is a bring-up/discovery run, not a regression gate.

## 7. Testing

- Unit test for `map_butler_held` (a `DmInboxDoc` fixture with two entries, varied `ingested_by`; assert all fields incl. key-split + empty doc → empty vec) in `butler_held_dto.rs`.
- Unit tests for the `*_impl` seams where feasible against an in-process `NodeState` (or assert the GUI command and the `_impl` share one code path by construction — the command is a one-line delegate).
- The allowlist test enforces the exact 49-command surface.
- Gates (from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked -p harmony-app --lib --features test-fixtures`; final `--all-targets` clippy sweep.

## 8. File-touch map

- **Create** `src-tauri/src/butler_held_dto.rs` — `ButlerHeldEntryDto` + `ButlerHeldResponse` + `map_butler_held` + unit test.
- **Modify** `src-tauri/src/lib.rs` — `pub mod butler_held_dto;`; extract `set_butler_pin_impl`; thin `set_butler_pin` wrapper; add `get_butler_pin_impl` + `get_butler_pin` wrapper + `ButlerPinStatus`; add `get_butler_held_impl` + `get_butler_held` wrapper.
- **Modify** `src-tauri/src/api/rpc.rs` — `SetButlerPinArgs`; three `rpc!` registrations; three names in the allowlist test; doc-comment count bump.
- **Modify** `docs/playbooks/e2e-two-agent-suite.md` — append Scenario D3.

## 9. Risks

- **`set_butler_pin_impl` extraction must preserve GUI behavior exactly** — it is a pure refactor (snapshot → `_inner` → sync → notify → flush → republish). Existing `set_butler_pin_*` unit tests (`lib.rs:50370`, `50408`) plus `device_vk_hex_round_trips_through_set_butler_pin` (`owner_commands.rs:723`) guard the seam.
- **Arg-struct name collisions** (`SetButlerPinArgs`) — verify against existing structs in `rpc.rs` (ZEB-487 hit one with `CommunityIdArgs`); pick a free name if taken.
- **Entry-key format** — the mapper assumes `"{space_id_hex}:{message_cid_hex}"`; verify exact construction at `dm_inbox_crdt.rs` before relying on `split_once(':')`.
- **PR body closes ZEB-489 only** — keep parent ZEB-321 and refs ZEB-418/483/487/488 out of the close-trigger format (Linear auto-close cascade).
