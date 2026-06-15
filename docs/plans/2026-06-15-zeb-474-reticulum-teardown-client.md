# ZEB-474: Reticulum teardown (client half) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In `harmony-client`, replace the DM unicast transport with a deposit-only stub and remove the client's Reticulum carrier (UDP socket / `udp0` / `SendOnInterface` egress / `UnicastReceived` handling / `parse_reticulum_port`), the `TransportBinding::Reticulum` wire variant, and the dead `allow_no_reticulum_destinations` fast-fail — leaving the workspace green, with DMs delivering via the existing butler/community-relay deposit path.

**Architecture:** The client currently sends DMs through `RuntimeUnicastTransport` → an mpsc unicast channel → `RuntimeEvent::SendUnicastToDevice` → the (pinned) core runtime's Reticulum router → a client-side UDP LAN broadcast. That path is egress-broken off-LAN. This plan swaps `RuntimeUnicastTransport` for a `DepositOnlyDmTransport` whose `send()` is a no-op that steers every DM into the outbox's *existing* butler→community-relay deposit rung (which carries the signed `cidnotify` to the recipient over iroh and calls `mark_ack_delivered` on butler-ack). It then deletes the client's own Reticulum UDP carrier. The transport-agnostic DM application logic (the outbox, `cidnotify`/`ack`/`DmInvite` packets, `handle_cidnotify_lifted`, `compute_dm_destination_hash`, `OwnerDeviceCache`) **stays**, because Move 1a (ZEB-473) rewires it onto the iroh tunnel within weeks — deleting it now would mean rewriting it then.

**Tech Stack:** Rust, Tauri (lib crate `harmony-app`), `tokio`, `async_trait`, `ed25519_dalek`, `serde`/canonical-CBOR. Pinned core rev `c982079` (still exposes the Reticulum-bound runtime APIs — this PR stops *using* them; ZEB-475 removes them).

---

## Context the implementer needs

### The three unicast-channel consumers (load-bearing — read before touching anything)

`unicast_send_tx: mpsc::Sender<UnicastSendRequest>` is fed from **three** places. Knowing which is which is the whole plan:

1. **Site 1 — outbound DM cidnotify**, via the `DmTransport::send` seam. The production impl is `RuntimeUnicastTransport` (`src/dm_outbox.rs:207–291`). **This is the only consumer that goes through `DmTransport`, and the only one this plan converts.**
2. **Site 2 — inbound ack fan-out**, inside `DmOutbox::handle_cidnotify_lifted` (`src/dm_outbox.rs:1836–1848`), pushes `unicast_send_tx.try_send(...)` directly.
3. **Site 3 — DmInvite fan-out for Space membership**, inside `add_space_inner` (`src/lib.rs:~10302–10312`), pushes `unicast_send_tx.try_send(...)` directly.

**Sites 2 and 3 are NOT converted and NOT deleted by this plan.** They are transport-agnostic DM logic. After this PR their emissions reach the (pinned) core's `SendUnicastToDevice` handler → the core resolves → emits `SendOnInterface` → the client no longer handles it → dropped. That is **no worse than today** (off-LAN already dropped) and is **rewired to the iroh tunnel in Move 1a (ZEB-473)**. They get a `// DORMANT (ZEB-474 → rewired in ZEB-473/Move 1a)` comment, nothing more.

### Why deposit-only delivers (don't doubt the premise mid-task)

Inbound DMs already arrive over **iroh**, not Reticulum: the `harmony/butler-deposit/v1` ALPN → `IrohButlerDepositAcceptor::handle_connection` → `handle_deposit_core` (`src/iroh_butler_acceptor.rs:480`) → the `dm_inbox_ingest` path → the same `dm-received` UI event (`src/dm_outbox.rs:2612–2633`). This is a **separate path from `handle_cidnotify_lifted`**. Removing the Reticulum `UnicastReceived` carrier therefore does not break inbound DM delivery.

The sender side: the outbox's deposit rung (`src/dm_outbox.rs` `drain_phase_c` collection ~1093–1288 + the deposit execution in `drain`/`drain_lifted` ~931–983 / ~2164–2290) builds a `ButlerDepositRequest { entry_id, recipient_owner, space_id, message_cid, cidnotify_packet, now_ms }` from `build_cidnotify_packet_bytes(entry)` (the **byte-identical** signed cidnotify that `RuntimeUnicastTransport::send` built), tries butler then community-relay, and on ack calls `mark_ack_delivered` → emits `dm-delivered`. All of this already exists and stays untouched.

### `handle_cidnotify_lifted` goes dormant but is NOT dead code

Its only **production** caller is the `UnicastReceived` arm (removed in Task 6). All other callers are `#[cfg(test)]` (`run_handle_cidnotify_lifted`, `src/dm_outbox.rs:5338`). Because it is a `pub async fn` on `DmOutbox` (a lib-crate public item), orphaning it does **not** trigger `dead_code`. Leave it in place for Move 1a. Do not delete it.

### `reticulum_identity_bytes` mostly STAYS in this PR

`reticulum_identity_bytes` (`src/lib.rs:3079`) is a client-local `Zeroizing<[u8;64]>` that feeds: (a) `private_identity_arc` (countersign identity — KEEP), (b) `signing_key_arc = bytes[32..64]` (the DM cidnotify signing key — KEEP; deposit-only still signs), and (c) the **core** `NodeConfig.reticulum_identity_bytes` field (`src/lib.rs:7470`). The core field is removed in ZEB-475; the client stops passing it + renames the local in the PR-3 rev-bump. **Do not remove `reticulum_identity_bytes` or any `reticulum_identity_bytes: None` test field in this PR** — the pinned core still requires the config field. (This corrects the recon doc, which suggested stripping it now.)

### Recon references (current code excerpts + anchors)

- `docs/analysis/2026-06-14-transport-04-reticulum-footprint.md`
- The spec: `docs/specs/2026-06-15-reticulum-teardown-move-2-design.md`

**Line numbers in this plan are anchors from a 2026-06-15 scan and WILL drift as you delete code.** Every deletion task gives you a `grep` to relocate the current site + a distinctive snippet to confirm, then a compile/clippy/test gate that proves the deletion is complete. Trust the gates, not the line numbers.

### Gates (run from `src-tauri/`)

- Format: `cargo fmt --all -- --check`
- Lint (per-task, scoped): `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
- Test (per-task, scoped): `cargo nextest run --locked -p harmony-app --lib --features test-fixtures`
- Integration test (when a task touches `tests/`): `cargo nextest run --locked -p harmony-app --features test-fixtures --test <name>`
- **Final sweep only** (Task 11): `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` then `cargo nextest run --locked --all-targets --features test-fixtures`

Per-task uses `--lib` to avoid the ~50-min relink of ~97 integration binaries under `--all-targets`. Commit before every gate (10-min wall-clock budget per gate; if a gate exceeds it, commit WIP and report `DONE_WITH_CONCERNS`).

---

## File structure

**Modified:**
- `src/dm_outbox.rs` — add `DepositOnlyDmTransport` (+ test); add DORMANT comment at Site 2.
- `src/lib.rs` — swap the production transport construction (`~3833`); DORMANT comment at Site 3; remove `allow_no_reticulum_destinations` param + threading + the `destinations.is_empty()` fast-fail; later, `TransportBinding::Reticulum` fixture sweep.
- `src/event_loop.rs` — remove `parse_reticulum_port`/`RETICULUM_UDP_PORT` + its test mod; UDP socket bind + `broadcast_addr`; `udp0` inbound arm; `SendOnInterface` egress arm; `UnicastReceived` handling (collapse `handle_runtime_action_or_dispatch` → `dispatch_action`) + `retry_buffer`; `UdpSocket` import.
- `src/owner_state_types.rs` — remove `TransportBinding::Reticulum` variant + `ReticulumDest` struct + its `impl_canonical!` entry.
- `src/voice_presence.rs`, `src/profile_broadcast.rs`, `src/dm_crypto.rs`, `src/owner_state_crdt.rs`, `src/owner_state_sync.rs`, `src/owner_state_persist.rs`, `src/owner_state_crypto.rs` — `TransportBinding::Reticulum` fixture/test sweep → `None` or `TransportBinding::Zenoh { topic: String::new() }`.
- `src/dm_signing.rs` — rename the `compute_dm_destination_hash_matches_reticulum_formula` test (formula is transport-agnostic; rename only).

**Deleted:**
- `src/inbound_packet.rs` (entire file — only the `UnicastReceived` arm called it).
- `tests/dm_unicast_integration.rs` (entire file — exercised the Reticulum unicast channel round-trip).

**Test sweep (fixture-only edits):**
- `tests/dm_create_integration.rs`, `tests/dm_send_integration.rs`, `tests/dm_thread_integration.rs`, `tests/butler_outhold_integration.rs`, `tests/group_dm_voice_three_engine_integration.rs`, `tests/profile_isolation.rs`.

---

## Task 1: `DepositOnlyDmTransport` (the functional core)

**Files:**
- Modify: `src/dm_outbox.rs` (add struct + impl near `RuntimeUnicastTransport`, ~line 291; add test in the `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test.** In the `#[cfg(test)] mod tests` block of `src/dm_outbox.rs`, add:

```rust
#[tokio::test]
async fn deposit_only_transport_send_signals_transient_to_steer_into_deposit_rung() {
    // ZEB-474: the deposit-only transport must never claim a direct send
    // succeeded — returning Transient is what steers the outbox into its
    // butler/community-relay deposit rung (which performs real delivery and
    // calls mark_ack_delivered on ack). An Ok here would be a silent
    // black-hole: the outbox would treat the DM as "sent, awaiting ack"
    // and never deposit it.
    let t = DepositOnlyDmTransport;
    let entry = OutboxEntry {
        id: OutboxEntryId(1),
        space_id: SpaceId([7u8; 32]),
        message_cid: ContentId::from_bytes([9u8; 32]),
        recipients: vec![],
        attempts: 0,
        created_wall_ms: 0,
    };
    let recipient = OwnerAddr([3u8; 32]);
    let err = t
        .send(&entry, recipient, vec![[1u8; 16]])
        .await
        .expect_err("deposit-only send must signal Transient, never Ok");
    assert!(matches!(err, TransportError::Transient(_)));
}
```

> Confirm `OutboxEntry`'s exact fields first (grep `struct OutboxEntry` in `src/dm_outbox.rs` and copy the real field set — the literal above is illustrative; match the actual struct, or build the entry via whatever test constructor the existing tests use, e.g. search for how `StubTransport` tests build an `OutboxEntry`). `ContentId::from_bytes` / `SpaceId` / `OwnerAddr` / `OutboxEntryId` come from `crate::owner_state_types`.

- [ ] **Step 2: Run to verify it fails.** Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(deposit_only_transport_send)'` — Expected: FAIL, `cannot find ... DepositOnlyDmTransport`.

- [ ] **Step 3: Implement.** Immediately after the `RuntimeUnicastTransport` impl block (~line 291) add:

```rust
/// ZEB-474 (coalescence Move 2): the Reticulum unicast carrier is gone.
/// In the interim before Move 1a (ZEB-473) brings up a live iroh-tunnel DM
/// carrier, DM delivery is store-and-forward only — the outbox's deposit
/// rung (butler → community-relay) carries the signed cidnotify to the
/// recipient over iroh and marks the entry delivered on butler-ack.
///
/// This transport is therefore a no-op "direct send" that always signals
/// `Transient`. Returning `Transient` (not `Ok`) is deliberate: it steers
/// every DM into the deposit rung (the `pre_failure_count >= 1` transient
/// gate fires deposit on the next drain pass). An `Ok` would make the
/// outbox treat the DM as "sent, awaiting ack" and it would never deposit.
///
/// Move 1a replaces this with `IrohTunnelDmTransport` on the same
/// `DmTransport` seam — no other outbox code changes.
pub struct DepositOnlyDmTransport;

#[async_trait]
impl DmTransport for DepositOnlyDmTransport {
    async fn send(
        &self,
        _entry: &OutboxEntry,
        _recipient: OwnerAddr,
        _destinations: Vec<[u8; 16]>,
    ) -> Result<(), TransportError> {
        Err(TransportError::Transient(
            "deposit-only interim (ZEB-474): no direct DM carrier; \
             routing via butler/community-relay deposit"
                .to_string(),
        ))
    }
}
```

> Match the exact `DmTransport::send` signature at `src/dm_outbox.rs:40–62` (param names/types). If it differs from the above, copy the trait's signature verbatim and prefix unused params with `_`.

- [ ] **Step 4: Run to verify it passes.** Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(deposit_only_transport_send)'` — Expected: PASS.

- [ ] **Step 5: Commit.**
```bash
git add src-tauri/src/dm_outbox.rs
git commit -m "feat(zeb-474): DepositOnlyDmTransport — steer DMs into the butler/relay deposit rung

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Wire `DepositOnlyDmTransport` into production boot

**Files:**
- Modify: `src/lib.rs` (~3824–3839, the `let transport: Arc<dyn DmTransport> = ...` block)

- [ ] **Step 1: Locate the construction.** Run: `grep -n "RuntimeUnicastTransport::new" src-tauri/src/lib.rs`. Expect one production site (~3834) inside `start_node`.

- [ ] **Step 2: Replace it.** Replace the whole `let transport: ... = Arc::new(RuntimeUnicastTransport::new( ... ));` statement (the comment block above it about "Production transport: RuntimeUnicastTransport pushes ... into unicast_send_tx" at ~3824–3839) with:

```rust
                    // ZEB-474 (Move 2): the Reticulum unicast carrier is
                    // removed. DM delivery is deposit-only in the interim —
                    // DepositOnlyDmTransport::send signals Transient, which
                    // steers every DM into the outbox's butler/community-relay
                    // deposit rung (carried to the recipient over iroh).
                    // Move 1a (ZEB-473) swaps in IrohTunnelDmTransport here.
                    let transport: std::sync::Arc<dyn crate::dm_outbox::DmTransport> =
                        std::sync::Arc::new(crate::dm_outbox::DepositOnlyDmTransport);
```

> `unicast_send_tx`, `self_owner`, `our_signing_device_hash`, and `signing_key_arc` are NOT removed — they're still used by the outbox, Sites 2/3, the bridge, and redeem. Only the transport's *use* of `unicast_send_tx.clone()` + the signing args goes away. If the compiler warns one of those bindings is now unused, that means it had no other consumer — investigate before silencing; do not blanket-`_`.

- [ ] **Step 3: Compile gate.** Run: `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings` — Expected: clean (no unused-variable errors). If `RuntimeUnicastTransport` itself is now unused in production it will still be referenced by `#[cfg(test)]` tests and the `pub` API, so no `dead_code`; leave it (it documents the prior shape and may inform Move 1a).

- [ ] **Step 4: Test gate.** Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures` — Expected: PASS (lib tests). Some `dm_outbox` tests exercise the deposit rung directly via `StubTransport`; they are unaffected.

- [ ] **Step 5: Commit.**
```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-474): use DepositOnlyDmTransport as the production DM transport

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Deposit-routing integration test (spec §7)

Proves a DM with a butler/relay installed routes to the deposit path under the deposit-only transport, and durably queues when neither is configured.

**Files:**
- Modify: `src/dm_outbox.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Find the existing deposit test harness.** Run: `grep -n "ButlerDepositClient\|set_butler_deposit_client\|StubButler\|fn .*deposit" src-tauri/src/dm_outbox.rs | head -40`. Identify the existing stub deposit client(s) and the test that drives a transient-then-deposit flow (there are several — reuse their fixtures and `make_outbox_synthetic`).

- [ ] **Step 2: Write the test.** Model it on the closest existing transient→deposit test, but construct the outbox with `DepositOnlyDmTransport` as the transport (instead of a `StubTransport` seeded with Transient), install a stub butler deposit client that acks, drain twice, and assert the entry is marked delivered (and a `dm-delivered`/`mark_ack_delivered` effect is observed). Add a second case: NO deposit client installed → after several drains the entry remains queued (not errored, not delivered). Reuse the exact assertion style of the neighbouring deposit tests rather than inventing new helpers.

> Keep this test in `src/dm_outbox.rs` (lib tests) so it runs under the fast `--lib` gate and reuses the in-module stubs. Do not create a new `tests/` integration file for it.

- [ ] **Step 3: Run.** `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(deposit)'` — Expected: PASS (new test + existing deposit tests).

- [ ] **Step 4: Commit.**
```bash
git add src-tauri/src/dm_outbox.rs
git commit -m "test(zeb-474): deposit-only transport routes DMs to the deposit rung

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: DORMANT comments on Sites 2 and 3

No behavior change — annotate so the dead-end emissions aren't mistaken for live and are captured for Move 1a.

**Files:**
- Modify: `src/dm_outbox.rs` (Site 2, ~1836), `src/lib.rs` (Site 3, ~10302)

- [ ] **Step 1: Site 2.** Locate the ack fan-out: `grep -n "ack fan-out\|compute_dm_destination_hash" src-tauri/src/dm_outbox.rs | head`. Immediately above the `for device in &signed.sender_devices {` loop (~1836), add:
```rust
            // DORMANT (ZEB-474 → ZEB-473/Move 1a): the Reticulum unicast
            // carrier is removed; these ack packets reach the pinned core's
            // SendUnicastToDevice handler and are dropped (no worse than
            // today — off-LAN acks already dropped). Move 1a rewires this
            // fan-out onto the iroh tunnel. Delivery confirmation in the
            // interim comes from the butler-deposit ack, not this read-ack.
```

- [ ] **Step 2: Site 3.** Locate the DmInvite fan-out: `grep -n "compute_dm_destination_hash\|DmInvite" src-tauri/src/lib.rs | head`. Above the per-device `unicast_send_tx.try_send(...)` in `add_space_inner` (~10302), add:
```rust
        // DORMANT (ZEB-474 → ZEB-473/Move 1a): Reticulum carrier removed;
        // these DmInvite packets reach the pinned core's SendUnicastToDevice
        // handler and are dropped. Off-LAN Space-membership invites already
        // failed to deliver; Move 1a rewires this onto the iroh tunnel.
```

- [ ] **Step 3: Compile gate.** `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings` — Expected: clean.

- [ ] **Step 4: Commit.**
```bash
git add src-tauri/src/dm_outbox.rs src-tauri/src/lib.rs
git commit -m "docs(zeb-474): mark DM ack + invite unicast fan-out dormant (rewired in Move 1a)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: Remove the `SendOnInterface` UDP egress + the UDP socket + `udp0` inbound + port parsing

This removes the client's own Reticulum LAN carrier. Do the egress, the inbound arm, the socket, and the port parser together — they share the `udp`/`broadcast_addr` bindings, so removing one without the others leaves unused-binding errors. `UnicastReceived` is Task 6 (it's a different function).

**Files:**
- Modify: `src/event_loop.rs`

- [ ] **Step 1: Remove the `SendOnInterface` egress arm.** In `dispatch_action`, `grep -n "SendOnInterface" src-tauri/src/event_loop.rs`. Delete the whole `RuntimeAction::SendOnInterface { raw, weight, .. } => { ... udp.send_to(&raw, broadcast_addr) ... }` arm (~5695–5704). The `_ => {}` catch-all (~5871) absorbs `SendOnInterface` from the still-emitting core.

- [ ] **Step 2: Remove the `udp0` inbound `select!` arm.** `grep -n "udp.recv_from\|udp0" src-tauri/src/event_loop.rs`. Delete the `result = udp.recv_from(&mut udp_buf) => { ... RuntimeEvent::InboundPacket { interface_name: "udp0", ... } }` arm (~3141–3161) and the `let mut udp_buf` scratch buffer declaration that feeds only it (grep `udp_buf`).

- [ ] **Step 3: Remove the UDP socket bind + `broadcast_addr`.** `grep -n "UdpSocket::bind\|broadcast_addr\|live_udp\|let udp " src-tauri/src/event_loop.rs`. Delete the `reticulum_port`/`live_udp`/`udp`/`broadcast_addr` block (~888–954). The `ready_tx.send(Err(...))` fallback-bind-failure early-return goes with it (the fallback bind only existed to keep the Reticulum socket alive).

- [ ] **Step 4: Remove `parse_reticulum_port` + `RETICULUM_UDP_PORT` + the test module.** `grep -n "parse_reticulum_port\|RETICULUM_UDP_PORT\|reticulum_port_tests" src-tauri/src/event_loop.rs`. Delete the const (~20–22), the fn (~24–47), and `mod reticulum_port_tests { ... }` (~9028–9057). Fix the file-level doc comment (~4) that lists "UDP socket (Reticulum mesh broadcast/unicast)".

- [ ] **Step 5: Drop `udp`/`broadcast_addr` from `dispatch_action`'s signature** and its call site(s). `grep -n "fn dispatch_action\|dispatch_action(" src-tauri/src/event_loop.rs`. Remove the `udp: &UdpSocket` and `broadcast_addr: &SocketAddr` params and the corresponding args at every caller.

- [ ] **Step 6: Remove the now-unused import.** Delete `use tokio::net::UdpSocket;` (~17). If `SocketAddr` is now unused, remove it too (the compiler will tell you).

- [ ] **Step 7: Compile gate.** `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`. Expect possible follow-on unused-binding errors — chase each to its Reticulum origin (do not silence with `_`). Note: `handle_runtime_action_or_dispatch` still takes `udp`/`broadcast_addr` and is handled in Task 6; if its signature blocks compilation here, you may remove its `udp`/`broadcast_addr` params now and update the `UnicastReceived`→`dispatch_action` shim accordingly, then finish the `UnicastReceived` removal in Task 6.

- [ ] **Step 8: Commit.**
```bash
git add src-tauri/src/event_loop.rs
git commit -m "refactor(zeb-474): remove the client Reticulum UDP carrier (socket, udp0, SendOnInterface egress, port parse)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: Remove `UnicastReceived` handling + delete `inbound_packet.rs`

**Files:**
- Modify: `src/event_loop.rs` (`handle_runtime_action_or_dispatch` ~5487–5681; the `retry_buffer` machinery ~3116–3134 + ~5644–5660; the dispatch call site)
- Delete: `src/inbound_packet.rs`

- [ ] **Step 1: Confirm `inbound_packet.rs` is only reached via `UnicastReceived`.** Run: `grep -rn "inbound_packet\|try_dispatch_community" src-tauri/src/`. Expect references only from the `UnicastReceived` arm + the module declaration in `lib.rs`/`event_loop.rs`. If anything else references it, STOP and report.

- [ ] **Step 2: Collapse `handle_runtime_action_or_dispatch` into a direct `dispatch_action` call.** `grep -n "handle_runtime_action_or_dispatch" src-tauri/src/event_loop.rs`. This fn (~5487) is `if matches!(action, RuntimeAction::UnicastReceived { .. }) { <the whole DM/community-invite receive block> return; } dispatch_action(...).await;`. Delete the entire `UnicastReceived` `if`-block (~5508–5669) and the wrapper, replacing every `handle_runtime_action_or_dispatch(action, ...)` call site with a direct `dispatch_action(action, ...)` call (dropping the extra params the wrapper took: `dm_outbox`, `crdt_state`, `cas_handle`, `unicast_send_tx`, `community_registry`, `retry_buffer`, `retry_buffer_cap`). If any of those params has no other consumer in `run()`, remove its creation too (trace each to confirm it was Reticulum-receive-only — `cas_handle`/`crdt_state`/`dm_outbox` likely ARE used elsewhere; `retry_buffer` is not).

- [ ] **Step 3: Remove the `retry_buffer`.** `grep -n "retry_buffer\|RUNTIME_ACTION_RETRY_CAP\|runtime_action_retry" src-tauri/src/event_loop.rs`. Delete the `VecDeque` declaration + cap const (~3116–3134) and the requeue-drain logic — it existed solely to requeue `UnicastReceived` on lock contention.

- [ ] **Step 4: Delete the file.** `git rm src-tauri/src/inbound_packet.rs`. Remove its module declaration: `grep -rn "mod inbound_packet" src-tauri/src/` and delete that line.

- [ ] **Step 5: Compile gate.** `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`. Confirm `handle_cidnotify_lifted` does NOT warn (it's `pub`, so it stays). Chase any unused-binding error to its Reticulum origin.

- [ ] **Step 6: Test gate.** `cargo nextest run --locked -p harmony-app --lib --features test-fixtures`.

- [ ] **Step 7: Commit.**
```bash
git add -A src-tauri/src/event_loop.rs && git rm src-tauri/src/inbound_packet.rs
git commit -m "refactor(zeb-474): remove Reticulum UnicastReceived handling + inbound_packet.rs

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 7: Remove the `allow_no_reticulum_destinations` fast-fail

With no Reticulum carrier, the `destinations.is_empty()` fast-fail gates a dead path. Invite redemption always proceeds via iroh (Zenoh CRDT sync / iroh handshake), which is exactly the `allow_no_reticulum_destinations == true` behavior.

**Files:**
- Modify: `src/lib.rs` (the `redeem_invite_inner` / `redeem_invite_inner_with_overrides` signatures ~21499/21570, their threading, the `destinations.is_empty()` fast-fail in the body, all callers, and 3 tests)

- [ ] **Step 1: Map the surface.** Run: `grep -n "allow_no_reticulum_destinations" src-tauri/src/lib.rs`. Expect: the two fn params (~21499, ~21570), the inner→inner forwarding (~21520), the body's fast-fail use, ~2 production callers, and ~3 test fns/assertions (~23261, ~23544, ~23734).

- [ ] **Step 2: Find the fast-fail.** Inside `redeem_invite_inner_with_overrides`, `grep -n "destinations.is_empty\|is_empty()" ` near the redeem body — find the early-`Err` guarded by `if !allow_no_reticulum_destinations && destinations.is_empty()`. Remove the guard and the early-`Err` entirely (always proceed). If `destinations` becomes unused afterward, trace it: it likely fed the dormant unicast fan-out of the `PendingJoin` — leave that fan-out as DORMANT (it already routes through `unicast_send_tx` like Sites 2/3) or, if `destinations` is now wholly unused, remove only the now-dead local. Do NOT remove the `unicast_send_tx` param from redeem (still used by the dormant fan-out + threading).

- [ ] **Step 3: Remove the param.** Delete `allow_no_reticulum_destinations: bool` from both signatures, the forwarding arg (~21520), and every caller's argument. Update the doc comments that reference "Reticulum-destinations fast-fail" (~21563–21569) to note delivery is now iroh-only.

- [ ] **Step 4: Fix the 3 tests.** The test fns named around `allow_no_reticulum_destinations` (~23261, etc.): drop the param from their `redeem_invite_inner*` calls. If a test exists *only* to assert the false-path fast-fail (e.g. `redeem_fails_with_no_reticulum_destinations`), delete that test (the behavior it pins is gone); keep tests that assert successful redemption (they now match the only path). Use judgement per test; grep each name and read it.

- [ ] **Step 5: Compile + test gate.** `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings` then `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(redeem)'`.

- [ ] **Step 6: Commit.**
```bash
git add src-tauri/src/lib.rs
git commit -m "refactor(zeb-474): drop allow_no_reticulum_destinations fast-fail (redeem is iroh-only now)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 8: Remove the `TransportBinding::Reticulum` wire variant + `ReticulumDest`

`TransportBinding` is client-owned (`src/owner_state_types.rs:1100`). This is a CBOR wire-format change — acceptable under the flag-day-for-alpha decision (persisted state may be reset). DM/GroupDM Spaces switch to `transport: None` (deposit-only has no live point-to-point binding; Move 1a may add an iroh binding).

**Files:**
- Modify: `src/owner_state_types.rs` (variant + `ReticulumDest` + `impl_canonical!` entry)
- Modify (production constructors of DM/GroupDM spaces): find via grep.

- [ ] **Step 1: Find every production constructor of a Reticulum-bound Space.** Run: `grep -rn "TransportBinding::Reticulum" src-tauri/src/ | grep -v "#\[cfg(test)\]"` — but that won't catch test mods cleanly; instead grep all, then for each hit decide prod vs test by context. Production DM-space creation lives in the invite/handle paths (`grep -n "transport: Some(crate::owner_state_types::TransportBinding::Reticulum\|transport: Some(TransportBinding::Reticulum" src-tauri/src/lib.rs src-tauri/src/*.rs`). For each PRODUCTION constructor, change `transport: Some(TransportBinding::Reticulum { participants: ... })` → `transport: None`.

> The `participants: Vec<ReticulumDest>` field is not read in the delivery path (delivery uses `OwnerDeviceCache`), so dropping to `None` loses no live behavior. Confirm by grepping for any read of `.transport` that matches `Reticulum { participants }` and destructures `participants` for use — if one exists outside tests, STOP and report (it would be an unflagged live consumer).

- [ ] **Step 2: Remove the variant + struct.** In `src/owner_state_types.rs`: delete the `#[serde(rename = "r")] Reticulum { ... }` variant (~1107–1118), the `ReticulumDest` struct (~1092–1095), and the `ReticulumDest` entry in the `impl_canonical!` list (~1161, grep `ReticulumDest`).

- [ ] **Step 3: Compile gate (drives the fixture sweep).** `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`. This surfaces every remaining `TransportBinding::Reticulum` / `ReticulumDest` use as a hard error — the next task sweeps them.

- [ ] **Step 4: Commit (may not compile yet — that's fine; Task 9 finishes the sweep, so combine if you prefer).** If you keep tasks separate, commit only `owner_state_types.rs` + the production constructor edits:
```bash
git add src-tauri/src/owner_state_types.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-474): remove TransportBinding::Reticulum variant + ReticulumDest (DM spaces -> transport: None)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```
> If your workflow requires every commit to compile, do Tasks 8 and 9 as one unit and commit once after Task 9's gate is green.

---

## Task 9: Sweep `TransportBinding::Reticulum` fixtures (src + tests)

Mechanical. Every remaining reference is a test/fixture building a `Space`. Replace with the lowest-impact valid value: `None` if the field is `Option`, else `TransportBinding::Zenoh { topic: String::new() }`.

**Files (src):** `src/voice_presence.rs` (~1448 import, ~1475), `src/profile_broadcast.rs` (~788, ~852), `src/dm_crypto.rs` (~237, ~244), `src/owner_state_crdt.rs` (~13 sites), `src/owner_state_sync.rs` (~699/714/1444/1485), `src/owner_state_persist.rs` (~267/361/561), `src/owner_state_crypto.rs` (~948 `assert_canonical::<TransportBinding>()`), `src/lib.rs` (~10087/10168 test fixtures), `src/dm_outbox.rs` (~1570/2798/5237/5565/5769/5872/7202/8012).
**Files (tests):** `tests/dm_create_integration.rs` (~31/135 — this one is an *assertion* `Some(TransportBinding::Reticulum { .. })` that the created DM space carries a Reticulum binding → change to assert `None`), `tests/dm_send_integration.rs` (~12/41), `tests/dm_thread_integration.rs` (~24/37), `tests/butler_outhold_integration.rs` (~77/201), `tests/group_dm_voice_three_engine_integration.rs` (~49/108).

- [ ] **Step 1: Enumerate.** `grep -rln "TransportBinding::Reticulum\|ReticulumDest" src-tauri/src src-tauri/tests`.

- [ ] **Step 2: Sweep src fixtures.** For each `src/` hit: replace `Some(TransportBinding::Reticulum { participants: ... })` → `None` (preferred) or, if the surrounding code needs a `Some(TransportBinding)` (e.g. it later matches `Zenoh`), `Some(TransportBinding::Zenoh { topic: String::new() })`. Remove now-unused `TransportBinding` / `ReticulumDest` imports flagged by clippy.

- [ ] **Step 3: Handle the wire-format pinning sites specially.**
  - `src/owner_state_persist.rs:361` (live-format regression `let _ = (TransportBinding::Reticulum { ... })`): delete that line — the variant no longer exists. If the test's purpose was to pin the `r` tag's CBOR, replace it with a comment noting the variant was removed in ZEB-474 (flag-day-for-alpha), keeping the surrounding `Zenoh` pinning intact.
  - `src/owner_state_crypto.rs:948` (`assert_canonical::<TransportBinding>()`): keep the call (it round-trips the enum, now `Zenoh`-only) — verify it still compiles/passes.

- [ ] **Step 4: Sweep test fixtures + the one assertion.** For `tests/dm_create_integration.rs`, change the assertion that the created DM space has `Some(TransportBinding::Reticulum { .. })` to assert `transport` is `None` (the new deposit-only DM-space shape). Sweep the rest as fixtures.

- [ ] **Step 5: Rename the transport-agnostic test in `dm_signing.rs`.** `grep -n "compute_dm_destination_hash_matches_reticulum_formula" src-tauri/src/dm_signing.rs` → rename to `compute_dm_destination_hash_matches_pinned_formula` (the formula is transport-agnostic; the test body stays). Update the comment if it frames the hash as a "Reticulum address."

- [ ] **Step 6: Compile gate (lib + the swept integration tests).**
```bash
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```
Then the touched integration tests, e.g.: `cargo nextest run --locked -p harmony-app --features test-fixtures --test dm_create_integration --test dm_send_integration --test dm_thread_integration --test butler_outhold_integration`.

- [ ] **Step 7: Commit.**
```bash
git add -A src-tauri/src src-tauri/tests
git commit -m "refactor(zeb-474): sweep TransportBinding::Reticulum fixtures to None/Zenoh

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 10: Delete the Reticulum DM unicast tests + the port-degradation test

**Files:**
- Delete: `tests/dm_unicast_integration.rs`
- Modify: `tests/profile_isolation.rs`

- [ ] **Step 1: Confirm `dm_unicast_integration.rs` is wholly Reticulum-path.** `grep -n "RuntimeUnicastTransport\|unicast_send\|UnicastSendRequest\|TransportBinding::Reticulum" src-tauri/tests/dm_unicast_integration.rs`. It exercises the unicast-channel round trip (test fns `dm_full_round_trip_through_unicast_channel`, `dm_offline_recipient_then_online_delivers`). Its deposit-side coverage now lives in Task 3 + the existing `dm_outbox` deposit tests. `git rm src-tauri/tests/dm_unicast_integration.rs`.

- [ ] **Step 2: `profile_isolation.rs`.** `grep -n "HARMONY_RETICULUM_PORT\|occupied_reticulum_port\|Reticulum" src-tauri/tests/profile_isolation.rs`. Delete the whole `occupied_reticulum_port_degrades_instead_of_failing_boot` test fn (~82–114 — it tests the now-removed UDP bind degradation), and remove the `HARMONY_RETICULUM_PORT=0` env-set line + its comment (~31/65) from `node_boots_on_named_profile` (the rest of that test is valid).

- [ ] **Step 3: Gate.** `cargo nextest run --locked -p harmony-app --features test-fixtures --test profile_isolation`.

- [ ] **Step 4: Commit.**
```bash
git add -A src-tauri/tests && git rm src-tauri/tests/dm_unicast_integration.rs
git commit -m "test(zeb-474): delete Reticulum DM unicast tests + port-degradation test

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 11: Full-workspace green sweep + residual-grep

**Files:** none (verification + any cleanup it surfaces)

- [ ] **Step 1: Format.** `cargo fmt --all` then `cargo fmt --all -- --check`.

- [ ] **Step 2: Full clippy.** `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`. (Budget: this relinks integration binaries — allow up to the gate's time limit; if it exceeds, commit and report `DONE_WITH_CONCERNS` with the remaining target.) Fix any integration-target Reticulum fixture the per-`--lib` gates didn't compile.

- [ ] **Step 3: Full test.** `cargo nextest run --locked --all-targets --features test-fixtures`. Investigate every failure; iroh/zenoh transport orphan-flakes (first-bind timeouts) are known-noise — re-run the specific failures once to confirm they're flakes, never mask a real failure.

- [ ] **Step 4: Residual grep — confirm the intended teardown surface is gone and the intended-KEPT surface remains.**
```bash
cd src-tauri && grep -rni "reticulum\|udp0\|RETICULUM_UDP_PORT\|parse_reticulum_port\|SendOnInterface\|UnicastReceived\|broadcast_addr\|allow_no_reticulum" src/ tests/ | grep -v "DORMANT\|ZEB-475\|Move 1a\|reticulum_identity_bytes\|reticulum_interop"
```
Expect remaining hits to be ONLY: (a) `reticulum_identity_bytes` (stays this PR — feeds the signing key + core config), (b) DORMANT-commented Sites 2/3 + the `SendUnicastToDevice` bridge, (c) the `RuntimeUnicastTransport` type kept for reference, (d) doc/comment references that are intentionally retained. Anything else (a live `udp`, a `SendOnInterface` arm, a `TransportBinding::Reticulum`) means a missed site — fix it.

- [ ] **Step 5: Confirm the kept invariants.**
```bash
cd src-tauri && grep -rn "compute_dm_destination_hash\|OwnerDeviceCache\|handle_cidnotify_lifted\|DeviceIdentityHash" src/ | head
```
Confirm these still exist (transport-agnostic, KEEP). Run the device-cache + handshake tests: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(owner_device) + test(handshake) + test(cidnotify)'`.

- [ ] **Step 6: Final commit (if Step 1–5 changed anything).**
```bash
git add -A
git commit -m "chore(zeb-474): workspace-green sweep for Reticulum client teardown

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Interim behavior changes (carry into the PR description for Jake)

After this PR (and before Move 1a / ZEB-473):
- **Outbound DMs:** deliver via butler/community-relay deposit over iroh when one is configured + online; otherwise durably queue (retry with backoff). First deposit fires on the 2nd drain pass (~5s) — acceptable for async store-and-forward. **Strictly better than today off-LAN.**
- **DM read-acks (Site 2) and Space-membership DmInvites (Site 3):** no carrier in the interim (dormant). Off-LAN these already failed; on-LAN they will now stop delivering. **Move 1a (ZEB-473) restores both via the iroh tunnel.** Flag this explicitly in the PR body.
- **`TransportBinding::Reticulum` removal is a CBOR wire-format change** — persisted Spaces carrying a Reticulum binding won't deserialize. Covered by flag-day-for-alpha. Flag in the PR body.

## Sequencing note (do not act on in this PR)

This PR compiles against the current pinned core (`c982079`), which still exposes `RuntimeEvent::SendUnicastToDevice` / `RuntimeAction::{SendOnInterface,UnicastReceived}`. ZEB-475 removes the core router; to avoid breaking the dormant client bridge before Move 1a, ZEB-475 should **keep the `RuntimeEvent::SendUnicastToDevice` variant** (removing only its router *handling*) OR Move 1a removes the client bridge as part of rewiring. Capture this in the ZEB-475 plan. The client rev-bump (PR-3) + the `reticulum_identity_bytes` local rename happen after ZEB-475 merges.

---

## Self-review

- **Spec coverage:** §4.1 REMOVE — DM transport (Tasks 1–3), UDP/`udp0`/`SendOnInterface`/`UnicastReceived` (Tasks 5–6), dead annotations + `TransportBinding::Reticulum` (Tasks 8–9), `inbound_packet.rs` ZEB-367 remnant (Task 6), Reticulum DM tests (Task 10). §4.3 KEEP — `OwnerDeviceCache`, `DeviceIdentityHash` formula, `handle_cidnotify_lifted` verified kept (Task 11 Step 5). §5 deposit interim — Tasks 1–3. §7 testing — Tasks 3, 11. §8 risk #3 (`reticulum_identity_bytes`) — explicitly deferred with rationale. §8 risk #4 (multiple DM call sites) — resolved (Sites 2/3 dormant, Site 1 converted).
- **Placeholder scan:** the `OutboxEntry` literal in Task 1 is explicitly flagged "match the real struct"; all other code is concrete.
- **Type consistency:** `DepositOnlyDmTransport` (unit struct) used identically in Tasks 1/2/3; `DmTransport`/`TransportError`/`UnicastSendRequest`/`TransportBinding` names consistent throughout.
- **Out-of-spec discovery baked in:** Site 3 (DmInvite fan-out) — not in the spec's risk #4 list — is handled (Task 4 dormant). The recon's "strip `reticulum_identity_bytes` now" suggestion is explicitly overruled (sequencing).
