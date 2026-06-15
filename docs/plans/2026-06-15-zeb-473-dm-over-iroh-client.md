# ZEB-473 — DM-over-iroh (Move 1a client) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development — fresh subagent per task, spec-compliance + code-quality review between tasks. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Give harmony-client a live carrier for 1:1 DMs by driving the post-quantum `harmony-tunnel` session directly from the client — restoring live DM byte-delivery (deposit-only since ZEB-474) and making DMs genuinely PQ.

**Architecture:** On friend handshake, persist the peer's iroh reachability + PQ keys (signed) onto `OwnerDeviceEntry`. On first DM to a friend, lazily dial a per-device PQ tunnel over the client's persistent iroh endpoint, carry the existing sealed+signed DM bytes under `FrameTag::Dm`, and feed inbound frames into the existing DM verify/decrypt/ingest pipeline. Always-deposit (durability) + attempt-tunnel (liveness); recipient dedups.

**Tech stack:** `harmony-tunnel` (sans-I/O PQ session, harmony-core @ rev `8b870ae`), iroh 0.98 QUIC (client's persistent endpoint + ALPN), `harmony-identity::PqPrivateIdentity`/`PqIdentity`, the client's `DmTransport` seam + owner-state CRDT.

**Design spec:** `docs/specs/2026-06-14-dm-over-iroh-move-1a-design.md` (APPROVED). **Seam map:** `docs/analysis/2026-06-14-transport-06-dm-over-iroh-integration-map.md` + the 2026-06-15 current-state reconciliation (anchors below are post-#271, verified).

**Conventions:** harmony-client CI gates = `cargo nextest run --locked --workspace --all-targets --features test-fixtures` + `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` + `cargo fmt --all -- --check` (from `src-tauri/`) + `npx tsc --noEmit` + `npx vitest run` (repo root). `--all-targets` and `--locked` are load-bearing. Commit-before-gate; `git add -u` (never `-A`). Keychain-hermetic test rules (CLAUDE.md ZEB-428) apply. **Flag-day-for-alpha:** owner-state CBOR + handshake sig preimage may change without back-compat; regenerate wire-format fixtures intentionally.

---

## File structure (created / modified)

**Created (client, `src-tauri/src/`):**
- `tunnel_manager.rs` — `TunnelManager`: per-peer-NodeId session map, lazy dial, keepalive, idle teardown, backoff, buffered-send, lower-NodeId collision dedup.
- `tunnel_task.rs` — per-session async driver: `run_tunnel_initiator` / `run_tunnel_responder` (handshake-over-bi-stream + select loop + length-prefixed framing + keepalive Tick), adapted from harmony-node `tunnel_task.rs`.
- `iroh_tunnel_acceptor.rs` — `IrohTunnelAcceptor impl IrohHandshakeDispatcher` (responder entry → registers session in `TunnelManager`).
- `iroh_tunnel_dm_transport.rs` — `IrohTunnelDmTransport impl DmTransport` (recipient-keyed routing into `TunnelManager`).

**Modified (client):**
- `Cargo.toml` — bump harmony rev `c982079`→`8b870ae`; add `harmony-tunnel` dep.
- `event_loop.rs` — remove the `unicast_send_rx → SendUnicastToDevice` arm (`:3409-3420`).
- `lib.rs` — drop `reticulum_identity_bytes` from `NodeConfig` (`:7461`) preserving the Ed25519 seed reuse (`:3801-3804`); retain `pq` past `:3067`; add `HARMONY_TUNNEL_V1` install at the acceptor-install boot block (`~:6879`); swap `DepositOnlyDmTransport`→`IrohTunnelDmTransport` (`:3829`); remove dormant DmInvite fan-out (`:22096-22120`, `:10529`); thread peer reachability into `apply_handshaked_friend` (`:39165`).
- `iroh_endpoint.rs` — add `HARMONY_TUNNEL_V1` const (`mod alpn` `:46-84`) + both bind lists (`:122`, `:375`) + assertion test (`:406`).
- `zenoh_iroh_transport.rs` — accept-loop tunnel arm (`:356`); `IrohZenohLinkManager` `tunnel_acceptor: OnceLock` + `install_tunnel_acceptor` (`:129`/`:223`).
- `owner_state_types.rs` — `DeviceTunnelContact` type + `OwnerDeviceEntry.device_tunnel_contacts` (`:445`) + manual `Deserialize` extension.
- `owner_state_crdt.rs` — `apply_owner_device_update` parallel `device_tunnel_contacts` arg (`:600`).
- `iroh_friend_acceptor.rs` — persist received reachability/PQ on receipt (`:1012`); extend `friend_devices_digest`→`contact_digest` (`:469`) covering the reachability/PQ fields.
- `dm_outbox.rs` — factor the live sign+seal construction (`:1324-1339`) into a shared free fn; remove dormant ack fan-out (`:1862-1881`); the inbound-ingest free helpers (`:2572`/`:2626`/`:2653`) are reused by the tunnel responder.
- `tests/wire_format_zeb370_fixtures.rs` + `tests/friend_token_roundtrip_integration.rs` — regenerate handshake hex pins.
- e2e-harness S2 scenario — flip DM delivery from characterization to hard-assert.

---

## Task 1: Pin bump + `harmony-tunnel` dep + clear the cross-repo break

**Goal:** compile green on harmony rev `8b870ae` (which has `FrameTag::Dm` AND removes `SendUnicastToDevice`/`reticulum_identity_bytes`). DMs remain deposit-only after this task.

**Files:**
- Modify: `src-tauri/Cargo.toml:91-103,193` (8 harmony crate pins + dev-dep), add `harmony-tunnel`.
- Modify: `src-tauri/src/event_loop.rs:3409-3420` (remove arm), param `:686`.
- Modify: `src-tauri/src/lib.rs:7461` (NodeConfig field), `:3078` (value build), `:3801-3804` (preserve seed reuse), `:22096-22120` + `:10529` (dormant DmInvite fan-out), channel `:2664` / NodeState field `:746`.
- Modify: `src-tauri/src/dm_outbox.rs:1862-1881` (dormant ack fan-out in `handle_cidnotify_lifted`).

- [ ] **Step 1:** Bump every `rev = "c982079980c378aa80aab6da5ed32d14e07471a1"` → `rev = "8b870ae05449e710a54fd03421dadfc582d26c6a"` in `Cargo.toml` (8 pinned crates + the `harmony-pkarr` dev-dep at `:193`). Add `harmony-tunnel = { git = "https://github.com/zeblithic/harmony.git", rev = "8b870ae05449e710a54fd03421dadfc582d26c6a" }`. Run `cargo update -p harmony-runtime --precise …` is unnecessary — `cargo check` re-resolves git pins; expect a wave of compile errors at the dormant refs (next steps).
- [ ] **Step 2:** Remove the `unicast_send_rx` select arm (`event_loop.rs:3409-3420`) that pushes `RuntimeEvent::SendUnicastToDevice` (the variant is deleted in core). Remove the now-unused `unicast_send_rx` param (`:686`) and its plumbing from the event-loop signature + call site.
- [ ] **Step 3:** Drop `reticulum_identity_bytes` from the `NodeConfig {…}` literal (`lib.rs:7461`) and its build (`:3078`). **Preserve** the Ed25519 signing-seed extraction at `lib.rs:3801-3804` by re-sourcing the seed directly from `ed25519` (the local `id.ed25519`) rather than via the removed config field.
- [ ] **Step 4:** Remove the dormant DmInvite fan-out loops (`lib.rs:22096-22120` in `redeem_invite_inner`, and the sibling `:10529`) — they push `UnicastSendRequest` to the dead bridge and are explicitly non-fatal (redemption proceeds via CRDT sync). Remove the dormant ack fan-out (`dm_outbox.rs:1862-1881`). Then remove the now-orphaned `unicast_send_tx`/`unicast_send_rx` channel (`lib.rs:2664`, NodeState field `:746`) and thread its removal through the IPC signatures that took it (`lib.rs:10389,10402,21474,22521,22606,22723` per reconciliation) — these become unused params; delete them. `handle_cidnotify_lifted` (`dm_outbox.rs:1642`) loses its `unicast_send_tx` param (it already has no live caller; keep the fn for now, drop the dead param + ack block).
- [ ] **Step 5: Gate.** `cargo check --locked --all-targets --features test-fixtures` green; then `cargo fmt --all`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked -p <touched crates>`. DMs still deposit-only (no behavior change yet). Commit: `ZEB-473: bump harmony pin past #280 + clear dormant Reticulum unicast refs`.

**Note for executor:** this is the largest mechanical task; the channel removal touches many signatures. If the `unicast_send_tx` removal balloons, an acceptable interim is to keep the channel but drain `unicast_send_rx` to a no-op sink (drop-on-recv) and delete only the producers + the `SendUnicastToDevice` push — flag as DONE_WITH_CONCERNS so the controller can decide. Prefer full removal.

---

## Task 2: Retain `PqPrivateIdentity` past boot

**Goal:** make the client's own `PqPrivateIdentity` available to the tunnel acceptor + dialer (it is currently `drop(pq)`'d at `lib.rs:3067`).

**Files:** Modify `src-tauri/src/lib.rs:3050-3067` (boot), NodeState (`~:746`), `identity.rs:60-86` (reference).

- [ ] **Step 1:** At `lib.rs:3067`, **do not** `drop(pq)`. Wrap it as `Arc<PqPrivateIdentity>` and store it on `NodeState` (new field `pq_identity: Arc<PqPrivateIdentity>`) so the boot block that installs acceptors (Task 6) and constructs the `TunnelManager` (Task 7) can clone it. Keep the existing public-key stashes (`dm_local_dsa_pubkey`/`dm_local_kem_pubkey`, `lib.rs:760-761`) unchanged.
- [ ] **Step 2:** Confirm `PqPrivateIdentity` is `Send + Sync` (it holds ML-DSA/ML-KEM secret keys; verify it does not need `!Sync` zeroize handling that breaks `Arc` sharing). If not `Sync`, hold it behind the boot seed instead: store `Zeroizing<[u8;32]> seed` and re-derive via `PqPrivateIdentity::from_seed(seed)` at each install point (deterministic). **Decision in plan:** prefer `Arc<PqPrivateIdentity>` if `Sync`; else seed-rederive. Executor verifies and picks; document which in the commit.
- [ ] **Step 3: Gate** (compile + existing identity tests; `tests/mint_owner_lifecycle.rs` keychain-hermetic rules apply). Commit: `ZEB-473: retain PqPrivateIdentity past boot for tunnel self-input`.

---

## Task 3: Tunnel ALPN constant + bind lists + assertion test

**Files:** Modify `src-tauri/src/iroh_endpoint.rs:46-84` (consts), `:122-131` + `:375-384` (BOTH bind lists), `:406-412` (assertion test).

- [ ] **Step 1:** Add `pub const HARMONY_TUNNEL_V1: &[u8] = b"harmony/tunnel/v1";` to `mod alpn` (matches the `harmony/<x>/v1` family).
- [ ] **Step 2:** Add `alpn::HARMONY_TUNNEL_V1` to **both** `.alpns(vec![…])` bind lists (`:122` and `:375` — the reconciliation flagged two; missing either silently drops inbound tunnel ALPN).
- [ ] **Step 3:** Extend the ALPN-value assertion test (`:406-412`) to pin the new constant.
- [ ] **Step 4: Gate** (`cargo nextest run -p <client crate> -E 'test(alpn)'` + fmt/clippy). Commit: `ZEB-473: client HARMONY_TUNNEL_V1 ALPN + bind list`.

---

## Task 4: `DeviceTunnelContact` type + `OwnerDeviceEntry` field + CRDT apply

**Files:** Modify `src-tauri/src/owner_state_types.rs:445` (struct + manual `Deserialize` after `:499`), `src-tauri/src/owner_state_crdt.rs:600-660` (`apply_owner_device_update`).

- [ ] **Step 1 (test first):** in `owner_state_crdt.rs` `#[cfg(test)]`, write a failing test: `apply_owner_device_update` with a `device_tunnel_contacts` vec of mismatched length pads/truncates to `devices.len()` (parity rule), and a contact at index i round-trips on the resulting `OwnerDeviceEntry.device_tunnel_contacts[i]`.
- [ ] **Step 2:** Define `DeviceTunnelContact { iroh_node_id: [u8;32], home_relay_url: Option<String>, pq_dsa_pubkey: Vec<u8>, pq_kem_pubkey: Vec<u8> }` (`#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]`, short serde renames consistent with the file). Add `pub device_tunnel_contacts: Vec<Option<DeviceTunnelContact>>` to `OwnerDeviceEntry` (`#[serde(default, rename="t")]`).
- [ ] **Step 3:** Extend the **manual `Deserialize` impl** for `OwnerDeviceEntry` to read the new vec and re-normalize it length-parallel to `devices` jointly with `device_identity_pubs` (same sort/merge/truncate path; missing → `None`).
- [ ] **Step 4:** Add the `device_tunnel_contacts: Vec<Option<DeviceTunnelContact>>` parameter to `apply_owner_device_update` (`owner_state_crdt.rs:600`); `resize(devices.len(), None)` (mirror `:616`) and carry it through the zip+sort+walk-merge (`:639-660`) as a third parallel element; on conflict prefer the non-`None`/newer per existing `learned_at` HLC rule (no `InvariantFail` for contacts — they are routing hints, last-writer-wins by HLC).
- [ ] **Step 5:** Update the two callers minimally to pass an empty/parallel vec for now: `iroh_friend_acceptor.rs:1014`, `lib.rs:39183` (real population in Task 5).
- [ ] **Step 6: Gate** (new test passes + `cargo nextest run -p <crate> -E 'test(owner_device)'`, fmt/clippy). Commit: `ZEB-473: persist per-device tunnel contact on OwnerDeviceEntry`.

---

## Task 5: Persist received reachability/PQ + sign it (`contact_digest`)

**Goal:** stop dropping the peer's `iroh_node_id/home_relay_url/pq_dsa_pubkey/pq_kem_pubkey` on handshake receipt, and bind them into the handshake signature (decision §6.3).

**Files:** Modify `src-tauri/src/iroh_friend_acceptor.rs:469` (`friend_devices_digest`→`contact_digest`), `:519/:533/:547` (preimage), `:898/:946/:1040` (build+verify in-file), `lib.rs:39165` (`apply_handshaked_friend` signature + `:39499/:41749` accept verify, `:39183` apply), receive-persist `:1012-1025`. Modify `tests/wire_format_zeb370_fixtures.rs`, `tests/friend_token_roundtrip_integration.rs`.

- [ ] **Step 1 (test first):** extend a handshake unit test to assert that tampering any of `iroh_node_id/pq_dsa_pubkey/pq_kem_pubkey/home_relay_url` makes `verify_strict` fail (proves they are now signed).
- [ ] **Step 2:** Generalize `friend_devices_digest` (`:469`) to `contact_digest(devices, pubs, iroh_node_id, home_relay_url, pq_dsa_pubkey, pq_kem_pubkey) -> [u8;32]` (SHA-256 over canonical CBOR of an extended `Bundle`). Update both build sites (request build in `lib.rs` send path; accept build `iroh_friend_acceptor.rs:1040`) and all verify sites (`:898`, `:946`, `lib.rs:39499`, `:41749`) to pass the reachability/PQ fields. Keep the field marked `#[serde(default)]` on the wire (unchanged) — only the *digest preimage* now covers them.
- [ ] **Step 3:** In `process_friend_request` (`:1012-1025`) build a `DeviceTunnelContact` from `req.{iroh_node_id, home_relay_url, pq_dsa_pubkey, pq_kem_pubkey}` for the sender's single device and pass it (parallel-indexed) into `apply_owner_device_update`. Do the same on the accept-receive path: add a `device_tunnel_contacts` param to `apply_handshaked_friend` (`lib.rs:39165`) and populate it at both call sites (`:39559`, `:41799`). Anti-forgery rule unchanged (local HLC, skip-on-empty).
- [ ] **Step 4:** Regenerate `wire_format_zeb370_fixtures.rs` hex pins (`:81/:82/:86-87`) and the `friend_token_roundtrip_integration.rs` expectations — the sig bytes change (new preimage) and the reachability fields are now populated. Regeneration is intentional + reviewed (flag-day); document in the commit.
- [ ] **Step 5: Gate** (`cargo nextest run -p <crate> -E 'test(friend) + test(wire_format_zeb370)'`, fmt/clippy, `--all-targets`). Commit: `ZEB-473: persist + sign peer reachability/PQ on friend handshake (contact_digest)`.

---

## Task 6: Tunnel responder (acceptor) + install on `IrohZenohLinkManager`

**Files:** Create `src-tauri/src/tunnel_task.rs` (responder half), `src-tauri/src/iroh_tunnel_acceptor.rs`; modify `zenoh_iroh_transport.rs:129/:212/:223/:356/:493`, `lib.rs:6879` (boot install). Templates: `iroh_invite_acceptor.rs:165` (trait), `iroh_pex_acceptor.rs:244` (responder body), harmony-node `tunnel_task.rs:168` (`run_responder` shape).

- [ ] **Step 1:** `tunnel_task.rs`: `pub async fn run_tunnel_responder(conn, local_pq: Arc<PqPrivateIdentity>, register: <handle into TunnelManager>, ingest: <DM ingest cb>)`. Body (adapt harmony-node `run_responder` + the length-prefixed helpers): `conn.accept_bi()` → read `TunnelInit` (`read_length_prefixed`, 4-byte BE) → `TunnelSession::new_responder` → write `TunnelAccept` → `run_tunnel_loop` (`tokio::select!` over `FramedRead`+`LengthDelimitedCodec` → `InboundBytes`; `cmd_rx` → `SendDm`; 10 s keepalive → `Tick`); two-pass dispatch (write `OutboundBytes` first). On `TunnelAction::DmReceived{payload}` → call the ingest cb (Task 9 wires it) and ensure the session is registered in `TunnelManager` so our outbound DMs reuse it.
- [ ] **Step 2:** `iroh_tunnel_acceptor.rs`: `struct IrohTunnelAcceptor { pq: Arc<PqPrivateIdentity>, mgr: Arc<TunnelManager>, ingest: <cb> }` `impl IrohHandshakeDispatcher` → `handle_connection` spawns `run_tunnel_responder`.
- [ ] **Step 3:** `IrohZenohLinkManager` (`zenoh_iroh_transport.rs:129`): add `tunnel_acceptor: OnceLock<Arc<dyn IrohHandshakeDispatcher>>` (+ init `:212`), `install_tunnel_acceptor(&self, a) -> Result<(),_>` (mirror `:223`).
- [ ] **Step 4:** accept-loop (`:356`): add `else if alpn_used == alpn::HARMONY_TUNNEL_V1 { if let Some(a)=mgr.tunnel_acceptor.get().cloned() { tokio::spawn(async move { a.handle_connection(conn).await }); } else { conn.close(...) } }` before the else-drop (`:503`), mirroring the butler arm (`:447-463`).
- [ ] **Step 5:** install at boot (`lib.rs:~6879`, same block as butler) once `pq` (Task 2) + `TunnelManager` (Task 7) exist. (Ordering note: Task 7 lands the manager; this install line may be stubbed here and completed in Task 7/8 — keep the acceptor constructible with a manager handle.)
- [ ] **Step 6 (test):** in-process two-session handshake test (`new_initiator` ↔ `run_tunnel_responder` over an in-memory/duplex stream or a loopback iroh pair) exchanging one `Dm` frame → `DmReceived` with byte-identical payload.
- [ ] **Step 7: Gate** (`cargo nextest run -p <crate> -E 'test(tunnel)'`, fmt/clippy). Commit: `ZEB-473: client tunnel responder + acceptor install`.

---

## Task 7: `TunnelManager` (session map + lifecycle + collision dedup)

**Files:** Create `src-tauri/src/tunnel_manager.rs`; complete `tunnel_task.rs` initiator half; modify `lib.rs` (construct + share `Arc<TunnelManager>` at boot). Dial templates: `butler_deposit.rs:443-471`, `lib.rs:39288-39336`.

- [ ] **Step 1:** `tunnel_manager.rs`:
  ```
  struct TunnelManager { sessions: Mutex<HashMap<[u8;32], TunnelHandle>>, endpoint: IrohEndpoint, local_pq: Arc<PqPrivateIdentity>, ingest: <cb> }
  struct TunnelHandle { cmd_tx: mpsc::Sender<TunnelCommand>, state: Dialing|Active|Closing, role, pending: VecDeque<Vec<u8>> }
  async fn send_dm(&self, node_id: [u8;32], contact: &DeviceTunnelContact, packet: Vec<u8>)
  ```
  `send_dm`: Active → `cmd_tx.send(SendDm(packet))`; Dialing → push to `pending`; absent → insert `Dialing` handle, spawn `run_tunnel_initiator` (Task 6 loop, initiator entry), push to `pending`. On handshake→Active flush `pending`.
- [ ] **Step 2:** `run_tunnel_initiator` (in `tunnel_task.rs`): build `EndpointAddr::new(EndpointId::from_bytes(&contact.iroh_node_id)).with_relay_url(contact.home_relay_url)` (+ `.with_ip_addr` if available) → `endpoint.inner().connect(addr, HARMONY_TUNNEL_V1)` (persistent endpoint) → `open_bi()` → `TunnelSession::new_initiator(rng, &local_pq, &peer_pq, now)` where `peer_pq = PqIdentity::from_public_keys(&contact.pq_kem_pubkey, &contact.pq_dsa_pubkey)` → write `TunnelInit`, read `TunnelAccept`, `handle_event` → Active → `run_tunnel_loop`.
- [ ] **Step 3 (keepalive/idle/backoff):** per-session task drives `TunnelEvent::Tick` on the jittered keepalive (harmony-tunnel internal 25–35 s; dead-peer 110 s). Idle teardown after a configurable no-traffic window (5 min) → `Close`; next DM re-dials. Dial failure → exponential backoff, bounded retries; on exhaustion the DM relies on deposit (Task 9).
- [ ] **Step 4 (collision dedup, §6.2):** on a completed dial OR accept where a session already exists for that peer NodeId, keep the tunnel whose **initiator NodeId is numerically lower** (compare our NodeId vs peer NodeId); close the loser; redirect `pending`/`cmd_tx` to the survivor. Both sides apply the identical rule → converge. **Test:** simulate both-dial; assert single survivor = lower-NodeId initiator on both sides; `pending` flushes over the survivor.
- [ ] **Step 5:** construct `Arc<TunnelManager>` at boot (after `pq` + endpoint + link manager available), share it into the acceptor (Task 6 install) and the transport (Task 8).
- [ ] **Step 6: Gate** (unit tests: dedup convergence, buffered-send flush, dialer addr construction from `DeviceTunnelContact`; fmt/clippy). Commit: `ZEB-473: TunnelManager — lazy per-peer PQ tunnels + collision dedup`.

---

## Task 8: `IrohTunnelDmTransport` + install + shared sign+seal helper

**Files:** Create `src-tauri/src/iroh_tunnel_dm_transport.rs`; modify `dm_outbox.rs:1324-1339` (factor sign+seal), `:74` (`resolve` peer device contacts), `lib.rs:3829` (install).

- [ ] **Step 1:** Factor the live sealed+signed construction (`dm_outbox.rs:1324-1339`, the deposit path) into a shared `pub(crate) fn build_dm_packet(signed: DmCidNotifySigned, signing_key: &SigningKey) -> Vec<u8>` (`build_signed_cidnotify`→`encode_packet`); call it from both the deposit path and the new transport. (The dead `RuntimeUnicastTransport::send` copy at `:260-263` is removed with that struct, or left as-is if test-only — prefer removing the now-unused struct.)
- [ ] **Step 2:** `iroh_tunnel_dm_transport.rs`: `struct IrohTunnelDmTransport { mgr: Arc<TunnelManager>, cache: Arc<...OwnerDeviceCache...>, signing_key: Arc<SigningKey>, self_owner, our_signing_device_hash }` `impl DmTransport`. `send(entry, recipient, _destinations)`: ignore `destinations`; resolve `recipient` OwnerAddr → its `OwnerDeviceEntry` → for each device index, read `(devices[i] → NodeId via blake3(pq_dsa) ; device_tunnel_contacts[i])`; build the packet via `build_dm_packet`; `mgr.send_dm(node_id, contact, packet)` per device with a contact. Return `Ok(())` if at least one device was attempted; the always-deposit layer (below) covers the rest.
- [ ] **Step 3 (always-deposit + attempt-tunnel, §5.7):** keep the existing deposit path firing unconditionally (it already runs alongside the transport in `dm_outbox`). Confirm the DM flow = deposit (durability, unchanged) **and** `transport.send` (tunnel attempt). Recipient dedups (CRDT inbox). Do **not** gate deposit on tunnel success (optimization deferred).
- [ ] **Step 4:** Install at `lib.rs:3829-3830`: replace `Arc::new(DepositOnlyDmTransport)` with `Arc::new(IrohTunnelDmTransport::new(mgr.clone(), cache, signing_key, …))`. Remove `DepositOnlyDmTransport` if now unused (or keep behind `#[cfg(test)]`).
- [ ] **Step 5: Gate** (unit: transport resolves recipient→contacts and calls `mgr.send_dm`; fmt/clippy). Commit: `ZEB-473: IrohTunnelDmTransport — live DM carrier over PQ tunnel`.

---

## Task 9: Inbound DM ingest wiring + end-to-end integration

**Files:** Modify `iroh_tunnel_acceptor.rs`/`tunnel_task.rs` ingest cb; reuse `dm_outbox.rs:2572` (`verify_cidnotify_admission`), `:2626` (`decrypt_and_bind_dm_blob`), `:2653` (`dm_received_event_payload`), `owner_state_crdt.rs:412` (`apply_inbox`), `dm_inbox_ingest.rs:432` (emit pattern).

- [ ] **Step 1:** Define the ingest callback the tunnel responder/initiator loops call on `DmReceived{payload}`: decode the packet → `verify_cidnotify_admission(state, signed, sig, bytes)` → `decrypt_and_bind_dm_blob(space, blob, owner)` → `apply_inbox(entry)` → emit `dm-received` via `dm_received_event_payload`. This is the **same** sequence as the deposit ingest (`dm_inbox_ingest.rs`); factor a shared `pub(crate) async fn ingest_dm_packet(ctx, packet_bytes)` both the tunnel path and (optionally) the deposit path call, to avoid drift. Do **not** resurrect `handle_cidnotify_lifted` (caller-less).
- [ ] **Step 2 (integration test):** acceptor responder loop end-to-end (two in-process sessions) → one `Dm` frame → ingest path emits a `dm-received` for a decryptable test DM. `IrohTunnelDmTransport.send` routes a built packet to the manager and the responder ingests it.
- [ ] **Step 3: Gate** (`cargo nextest run -p <crate> -E 'test(dm) + test(tunnel)'` + full `--all-targets`). Commit: `ZEB-473: wire inbound tunnel DM into existing verify/decrypt/ingest`.

---

## Task 10: e2e-harness S2 hard-assert + full sweep

**Files:** the e2e-harness S2 scenario (`s2_friend_graph_and_dm_send`), full-workspace gates.

- [ ] **Step 1:** Flip S2 from characterization to **hard-assert**: two co-located headless `harmony-app` nodes friend each other, one sends a DM, the other **receives the DM byte over the tunnel** (the exact assertion that failed pre-#269). Keep deposit fallback covered by a second assertion if a peer is offline.
- [ ] **Step 2: Full sweep:** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; `npx tsc --noEmit`; `npx vitest run`. Regenerate any remaining owner-state CBOR fixtures flagged. Address flakes per ZEB-459/374 patterns (deterministic readiness, not sleeps).
- [ ] **Step 3:** Commit: `ZEB-473: S2 hard-assert live DM delivery over PQ tunnel`. Cross-WAN two-machine proof (Koya↔AVALON) is tracked, not gating merge (AVALON agent down).

---

## Self-review checklist (run before opening the PR)

- **Spec coverage:** every §5 component (5.1 done in #279; 5.2 Task 4–5; 5.3 Task 3+6; 5.4 Task 7; 5.5 Task 7; 5.6 Task 8; 5.7 Task 8) + every §6 cross-cut (6.2 dedup Task 7; 6.3 sign Task 5; 6.4 fixtures Task 5/10) has a task. ✓
- **Cross-repo break:** `SendUnicastToDevice` + `reticulum_identity_bytes` cleared in Task 1 (the ZEB-473 obligation noted on the ticket). ✓
- **Identity surprise:** `drop(pq)` fixed in Task 2 (prerequisite for Tasks 6–8). ✓
- **No silent caps:** always-deposit preserved (Task 8 Step 3); no DM path drops without deposit. ✓
- **Flag-day fixtures:** zeb370 handshake hex + any owner-state CBOR regen are intentional + reviewed (Task 5/10). ✓
- **PR shape:** one harmony-client PR (bundle-small-PRs); body references ONLY ZEB-473 (avoid Linear cascade); ZEB-321 stays open.
