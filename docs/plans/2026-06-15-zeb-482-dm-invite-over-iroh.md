# ZEB-482 — DM-Space `DmInvite` over the iroh tunnel (Move 1b) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Re-wire the already-built-but-discarded per-recipient `DmInvite` fan-out (`add_space_impl`) onto the ZEB-473 PQ iroh tunnel so two friend-owners share the DM `Space`, making co-located 1:1 DM delivery work end-to-end (un-ignore `s2_dm_delivery_over_tunnel_hard_assert`).

**Architecture:** Client-only (no harmony core change). The invite rides the existing `FrameTag::Dm` using the client `DmPacket::Invite` discriminant that `decode_packet` already parses. Send side = route the invite bytes through the same recipient→`DeviceTunnelContact`→`TunnelManager::send_dm` resolution `IrohTunnelDmTransport` uses. Receive side = generalize `ingest_dm_packet` from CidNotify-only-reject to a `DmPacket` dispatch, routing `Invite` into the intact `handle_invite` auto-accept (extracted to a shared `apply_invite`). Tunnel-only durability; deposit parity is ZEB-483.

**Tech Stack:** Rust (tokio, async_trait), the ZEB-473 `tunnel_manager`/`tunnel_task` machinery, `dm_envelope`/`dm_outbox`/`dm_inbox_ingest`, harmony-client `src-tauri`. Spec: `docs/specs/2026-06-15-dm-invite-over-iroh-move-1b-design.md`.

**Reference (read before starting):** the spec above; CLAUDE.md gates (`--locked --all-targets --features test-fixtures`, keychain-hermetic tests via `*_inner` seams).

---

### Task 1: Receive path — extract `apply_invite` + generalize `ingest_dm_packet` dispatch

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (extract `apply_invite` free fn from `handle_invite` ~`:1527–1641`)
- Modify: `src-tauri/src/dm_inbox_ingest.rs` (`ingest_dm_packet` ~`:347–364`; thread `self_owner`)
- Modify: the inbound-DM drain that calls `ingest_dm_packet` (boot wiring in `src-tauri/src/lib.rs`; grep `ingest_dm_packet(` to find the call site) — pass `self_owner`
- Test: `src-tauri/src/dm_inbox_ingest.rs` `mod tests` (reuse `build_dm_ingest_fixture`)

- [ ] **Step 1: Extract `apply_invite` as a shared free function.** In `dm_outbox.rs`, move the body of `handle_invite` (sanity gates 1–3, `verify_dm_packet_signature`, the `device_identity_pubs` build, `apply_owner_device_update`, the `Space` build, `apply_space_with_canonicalization`) into:

```rust
/// ZEB-482: auto-accept a received DmInvite — write the DM Space + cache the
/// inviter's devices/identity-pub. Idempotent on `space_id`. Shared by the
/// (dormant) outbox `handle_invite` method and the tunnel ingest path so both
/// apply identical trust gates. No IPC emit (invites carry no `dm-received`).
pub(crate) fn apply_invite(
    state: &mut OwnerState,
    self_owner: OwnerAddr,
    device_id: &str,
    signed: crate::dm_envelope::DmInviteSigned,
    signature: [u8; 64],
    signed_bytes: &[u8],
    wall_now_ms: u64,
) -> Result<DrainOutcome, DmReceiveError> {
    // … exact body moved from handle_invite, with `self.self_owner` → `self_owner`
    //     and `self.device_id` → `device_id` …
}
```

Then make `handle_invite` delegate: `apply_invite(state, self.self_owner, &self.device_id, signed, signature, signed_bytes, wall_now_ms)`. (Keep `handle_invite`'s doc comment; it's still the outbox entry point.)

- [ ] **Step 2: Run the existing invite test to confirm the extraction is behavior-preserving.** The existing `handle_invite` unit test (grep `handle_invite_writes_space_and_cache`) must still pass.

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(handle_invite)'`
Expected: PASS (unchanged behavior through the delegating wrapper).

- [ ] **Step 3: Write the failing ingest-dispatch test.** In `dm_inbox_ingest.rs` tests, add a test that builds a signed `DmInvite` wire packet (mirror how `build_dm_ingest_fixture` builds the CidNotify; use `dm_envelope::build_signed_invite` + `encode_packet`, with `self_owner` ∈ members) and asserts `ingest_dm_packet` applies the Space (present in `crdt_state.spaces` afterward), caches the inviter, returns `Ok(false)` (no emit), and emits NO `dm-received` frame on the sink:

```rust
#[tokio::test]
async fn ingest_dm_packet_applies_a_tunnel_delivered_invite() { /* … */ }
```

- [ ] **Step 4: Run it to confirm it fails** (current code returns `Err("tunnel DM packet is not a CidNotify")`).

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(ingest_dm_packet_applies_a_tunnel_delivered_invite)'`
Expected: FAIL with "not a CidNotify".

- [ ] **Step 5: Thread `self_owner` into `ingest_dm_packet` and generalize the dispatch.** Add `self_owner: OwnerAddr` to `ingest_dm_packet`'s signature (the inbound drain has it — it's the same identity that built `dm_self_owner` on `NodeState`; pass it through from the boot wiring, alongside `device_id`). Replace the `let-else` at `:357–364` with a variant dispatch:

```rust
match crate::dm_envelope::decode_packet(packet_bytes).map_err(|e| format!("decode_packet: {e}"))? {
    crate::dm_envelope::DmPacket::Invite { signed, signature, signed_bytes } => {
        let now_ms = /* local wall clock, as the CidNotify path computes it */;
        let mut state = crdt_state.lock().await;
        crate::dm_outbox::apply_invite(&mut state, self_owner, device_id, signed, signature, &signed_bytes, now_ms)
            .map_err(|e| format!("apply_invite: {e:?}"))?;
        return Ok(false); // invites never emit dm-received
    }
    crate::dm_envelope::DmPacket::CidNotify { signed, signature, signed_bytes } => {
        // … the existing admission → CAS → CID-rebind → Phase-C → decrypt → apply_inbox → emit, unchanged …
    }
    crate::dm_envelope::DmPacket::Ack { .. } => {
        return Err("tunnel DM packet is an Ack (not handled on the tunnel ingest path)".into());
    }
}
```

(Confirm the exact `DmPacket` variant field names against `dm_envelope.rs`. Keep the CidNotify arm byte-for-byte as today — only the wrapping changes from `let-else` to a `match` arm.)

- [ ] **Step 6: Run the new test + the CidNotify regression tests.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(ingest_dm_packet) + test(tunnel_delivered_dm_ingests_end_to_end)'`
Expected: PASS (invite applied + no emit; all existing CidNotify ingest tests still green).

- [ ] **Step 7: Update the inbound-drain call site + compile.** Pass `self_owner` at the `ingest_dm_packet(` call site (boot wiring). Then:

Run: `cd src-tauri && cargo check --locked --all-targets --features test-fixtures`
Expected: GREEN.

- [ ] **Step 8: Commit.**

```bash
git add -u
git commit -m "ZEB-482: ingest dispatches DmPacket::Invite into shared apply_invite (receive path)"
```

---

### Task 2: Send path — shared tunnel-send helper for arbitrary DM packets

**Files:**
- Modify: `src-tauri/src/iroh_tunnel_dm_transport.rs` (factor `resolve_tunnel_targets`)
- Create or modify: a shared helper (put it in `tunnel_manager.rs` or `iroh_tunnel_dm_transport.rs` — wherever keeps the resolver + `send_dm` together with least coupling)
- Test: same module's `mod tests`

- [ ] **Step 1: Write the failing helper test.** Add a test that, given an `OwnerState` whose `owner_device_cache` has a recipient with one `DeviceTunnelContact` (valid key sizes: `pq_dsa_pubkey` 1952B / `pq_kem_pubkey` 1184B per ZEB-473's `has_valid_key_sizes`), calls the new helper with arbitrary `packet` bytes and asserts `TunnelManager::send_dm` registered a session for `blake3(pq_dsa)` carrying exactly those bytes (via `mgr.test_pending_packets`). Model it on `send_resolves_contact_and_routes_to_manager` in `iroh_tunnel_dm_transport.rs`.

- [ ] **Step 2: Run it to confirm it fails** (helper doesn't exist).

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(send_packet_to_owner_tunnels)'`
Expected: FAIL (unresolved name).

- [ ] **Step 3: Implement the helper.** Factor the resolver out of `IrohTunnelDmTransport::resolve_tunnel_targets` into a free fn over `&OwnerState`, and add the send helper:

```rust
/// Resolve a recipient owner's reachable per-device tunnel targets:
/// (NodeId = blake3(pq_dsa_pubkey), contact) for each bound device that
/// advertised a DeviceTunnelContact. Devices with no contact are skipped.
pub(crate) fn resolve_owner_tunnel_targets(
    state: &OwnerState,
    recipient: OwnerAddr,
) -> Vec<([u8; 32], crate::owner_state_types::DeviceTunnelContact)> { /* moved body */ }

/// ZEB-482: fire `packet` (any pre-built DmPacket wire bytes — e.g. a signed
/// DmInvite) to every reachable tunnel device of `recipient`, best-effort.
/// Caller holds NO lock across this (resolves from the passed `&OwnerState`,
/// then calls `send_dm` which locks the session map itself).
pub(crate) fn send_packet_to_owner_tunnels(
    state: &OwnerState,
    mgr: &TunnelManager,
    recipient: OwnerAddr,
    packet: &[u8],
) {
    for (node_id, contact) in resolve_owner_tunnel_targets(state, recipient) {
        mgr.send_dm(node_id, &contact, packet.to_vec());
    }
}
```

Refactor `IrohTunnelDmTransport::resolve_tunnel_targets` to call `resolve_owner_tunnel_targets` (no behavior change).

- [ ] **Step 4: Run the helper test + the existing transport tests.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(send_packet_to_owner_tunnels) + test(send_resolves_contact_and_routes_to_manager) + test(send_without_contact)'`
Expected: PASS (helper routes; transport regression intact).

- [ ] **Step 5: Commit.**

```bash
git add -u
git commit -m "ZEB-482: shared send_packet_to_owner_tunnels helper (reuses tunnel-target resolver)"
```

---

### Task 3: Send path — route the invite over the tunnel from `add_space_impl`

**Files:**
- Modify: `src-tauri/src/lib.rs` — `add_space_dm_inner` (~`:10409–10432`) return shape + `add_space_impl` (~`:10520–10600`) snapshot + routing
- Test: `src-tauri/src/lib.rs` (the `add_space`/`add_space_dm_inner` test module) or a focused integration test

- [ ] **Step 1: Change `add_space_dm_inner` to return the invite fan-out instead of per-device Reticulum sends.** The `sends: Vec<UnicastSendRequest>` (per-device `destination_hash`, built for the removed Reticulum carrier) is dead for the tunnel. Replace the return tuple's middle element with the invite material the tunnel needs:

```rust
// returns None when the Space merged into an existing one (already invited);
// Some((invite_wire, recipient_owners)) on a fresh create.
Ok((canonical_space_id, fanout /* Option<(Vec<u8>, Vec<OwnerAddr>)> */, was_merge))
```

Build `fanout = (!was_merge).then(|| (invite_wire.clone(), recipients.clone()))`. Drop the `for r in &recipients { for device … UnicastSendRequest }` loop (and `compute_dm_destination_hash` use here). If `UnicastSendRequest` becomes unused project-wide, remove it (grep first); otherwise leave it.

- [ ] **Step 2: Update `add_space_dm_inner`'s existing unit tests** to the new return shape (they likely assert on `sends`; re-point them at `fanout`). Run them:

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(add_space_dm_inner)'`
Expected: PASS after the shape update.

- [ ] **Step 3: Snapshot the `TunnelManager` in `add_space_impl`.** Add `tunnel_manager` (the `Option<Arc<TunnelManager>>` published on `NodeState` in ZEB-473 — confirm the field name by grepping `tunnel_manager` in the `NodeState` struct) to the snapshot block at `:10520`. Then capture the `fanout` from `add_space_dm_inner` instead of discarding `_sends`:

```rust
let (space_id, fanout, _was_merge, new_hlc) = { /* build-lock block; returns the fanout */ };
```

- [ ] **Step 4: Route the invite over the tunnel AFTER the build-lock block releases.** With the `dm_outbox`/`crdt_state`/`hlc_tracker` locks dropped:

```rust
if let (Some(mgr), Some((invite_wire, recipients))) = (tunnel_manager.as_ref(), fanout) {
    let state_g = crdt_state.lock().await;            // short read lock to resolve contacts
    for recipient in &recipients {
        crate::iroh_tunnel_dm_transport::send_packet_to_owner_tunnels(
            &state_g, mgr, *recipient, &invite_wire,
        );
    }
}
```

(If `tunnel_manager` is `None` — deposit-only node — skip: the Space is already applied locally; the invite tunnel-send is a no-op, durability is ZEB-483. Keep the `crdt_state` lock and the `send_dm` calls in the same scope — `send_dm` does not await, so holding the `tokio::Mutex` read across it is fine, but do NOT hold it across any `.await`.)

- [ ] **Step 5: Write the send-routing test.** A test (unit or focused integration) that drives `add_space_impl` for a `Dm` whose single recipient has a valid `DeviceTunnelContact` in the cache + a `Some(tunnel_manager)` on `NodeState`, and asserts the recipient's `blake3(pq_dsa)` NodeId session received the `DmInvite` wire bytes (`mgr.test_pending_packets`). Decode the routed bytes with `decode_packet` and assert it's a `DmPacket::Invite` whose `space_id` matches the created Space. Add a second assertion: a deposit-only node (`tunnel_manager: None`) creates the Space (returned id is live) and routes nothing.

- [ ] **Step 6: Run the routing test + compile gate.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(add_space)' && cargo check --locked --all-targets --features test-fixtures`
Expected: PASS + GREEN.

- [ ] **Step 7: Commit.**

```bash
git add -u
git commit -m "ZEB-482: route the DmInvite over the iroh tunnel from add_space_impl (send path)"
```

---

### Task 4: DoD — un-ignore the S2 e2e + full-gate verify

**Files:**
- Modify: `e2e-harness/tests/e2e_two_node.rs` (`s2_dm_delivery_over_tunnel_hard_assert` ~`:444–451`)

- [ ] **Step 1: Remove the `#[ignore]` and refresh the stale FINDING comment.** Delete the `#[ignore = "ZEB-473: …SpaceNotFound…"]` attribute on `s2_dm_delivery_over_tunnel_hard_assert`. Update the preceding comment block (`:425–443`) to state the carrier landed in ZEB-482 (invite now rides the tunnel; the Space bootstraps before the first CidNotify) rather than describing the gap.

- [ ] **Step 2: Run the S2 e2e and confirm it passes.**

Run: `cd src-tauri && cargo nextest run --locked --features e2e -E 'test(s2_dm_delivery_over_tunnel_hard_assert)' --run-ignored all` (the `--run-ignored all` is harmless now that the attribute is gone; keep it for parity with the doc-comment invocation)
Expected: PASS — Bob fires `dm-received` with the body, plaintext lands in his DM thread. (First-contact is racy ~75–120s; the test already retries within its own deadlines.) If it flakes on first-contact timing rather than the DM logic, re-run; a genuine DM-logic failure surfaces as the `dm-received`/plaintext asserts, not the friend-handshake loop.

- [ ] **Step 3: Full local gate sweep** (matches CI; commit BEFORE running, per the time-budget discipline).

```bash
cd src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: fmt clean, clippy clean (`-D warnings`), nextest 0 failures.

- [ ] **Step 4: Frontend gates** (no frontend change expected, but CI runs them):

```bash
npx tsc --noEmit && npx vitest run
```
Expected: clean (no-op for this PR).

- [ ] **Step 5: Commit.**

```bash
git add -u
git commit -m "ZEB-482: un-ignore s2_dm_delivery_over_tunnel_hard_assert (DM-Space invite carrier landed)"
```

---

## Self-review checklist (controller, before dispatch)

- **Spec coverage:** §7.1 send re-wire → Task 3; §7.2 receive dispatch → Task 1; §7.4 reuse (`handle_invite`/resolver) → Tasks 1+2; §9 tests → Tasks 1–4; DoD → Task 4. ✓
- **Type consistency:** `apply_invite` signature is identical between its definition (Task 1 Step 1) and its call sites (`handle_invite` delegate + ingest Invite arm). `send_packet_to_owner_tunnels(&OwnerState, &TunnelManager, OwnerAddr, &[u8])` consistent between Task 2 (def) and Task 3 Step 4 (call). `add_space_dm_inner` new return `(SpaceId, Option<(Vec<u8>, Vec<OwnerAddr>)>, bool)` consistent between Task 3 Step 1 (def) and Step 3 (destructure). ✓
- **Lock discipline:** invite resolution + `send_dm` happen after the build-lock block; `send_dm` is non-awaiting so the short `crdt_state` read lock never spans an `.await`. ✓
- **Open detail for the implementer to confirm against live code:** the `DmPacket` variant field names; the `NodeState.tunnel_manager` field name; the `ingest_dm_packet` boot call site (for `self_owner` threading); whether `UnicastSendRequest` is now dead workspace-wide.
