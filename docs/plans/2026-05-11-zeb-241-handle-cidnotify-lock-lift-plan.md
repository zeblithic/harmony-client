# ZEB-241: handle_cidnotify lock-lift Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Lift the 500ms CAS fetch in `handle_cidnotify` outside the OwnerState/DmOutbox locks via a `tokio::spawn`-per-CidNotify task (Phase A locked → Phase B unlocked → Phase C re-locked).

**Architecture:** New `DmOutbox::handle_cidnotify_lifted` method takes Arc handles + manages internal lock cycles. event_loop pre-decodes inbound packets, spawns the lifted task for CidNotify, falls through to existing handle_unicast for Invite/Ack. Old monolithic `handle_cidnotify` removed; existing unit tests migrate (mechanical &mut → Arc swap).

**Tech Stack:** Rust 2021, tokio async runtime, `Arc<tokio::sync::Mutex<...>>`.

**Spec:** `docs/specs/2026-05-11-zeb-241-handle-cidnotify-lock-lift-design.md` (commit `8f2e196`).

**Branch:** `zeb-241-handle-cidnotify-lock-lift` (cut from `origin/main` `c9ccb81`).

---

## Task 0: Pre-flight green-baseline confirm

**Files:** none (verification only)

- [ ] **Step 1: Confirm branch state**

```bash
git status
git log --oneline -3
```

Expected: clean tree, on `zeb-241-handle-cidnotify-lock-lift`, HEAD `8f2e196` (spec commit) with `c9ccb81` (origin/main) below.

- [ ] **Step 2: Run all 5 CI gates**

From `src-tauri/`:
```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cargo check --locked --all-targets --features test-fixtures
```

From repo root:
```bash
npx tsc --noEmit
npx vitest run
```

Expected: all green. Any red here means baseline is dirty — STOP and report.

- [ ] **Step 3: NO COMMIT** — verification only.

---

## Task 1: Implement `handle_cidnotify_lifted` alongside old

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (add new method; existing `handle_cidnotify` untouched for now)

This task adds the new lifted implementation but does NOT wire it from event_loop or remove the old monolith. Lets us verify the new path compiles and the unit-test scaffolding works before disrupting callers.

- [ ] **Step 1: Locate the existing `handle_cidnotify` method**

```bash
grep -n "pub async fn handle_cidnotify" src-tauri/src/dm_outbox.rs
```

Expected: line ~1105.

- [ ] **Step 2: Add the new `handle_cidnotify_lifted` method on `DmOutbox`**

Add immediately after the existing `handle_cidnotify` (around line ~1304). Signature:

```rust
/// ZEB-241: lock-lifted variant of `handle_cidnotify`. Manages its
/// own lock cycles internally so the slow CAS fetch in Phase B
/// happens with NO locks held. Designed to be called from a
/// `tokio::spawn`'d task (event_loop fires-and-forgets).
///
/// Three phases:
///   - Phase A (locked): verify signature + resolve owner + check
///     sender match + snapshot Space (cloned). Drops locks.
///   - Phase B (unlocked): cas.get(message_cid) under 500ms timeout.
///   - Phase C (re-locked): re-fetch Space (TOCTOU window — content_key
///     could have rotated, member could have been kicked, Space could
///     have been deleted), decrypt with prior_content_keys fallback,
///     apply_owner_device_update, apply_inbox, build + try_send acks.
///     IPC emits happen AFTER lock drop.
///
/// On any error inside the task, logs via tracing::warn! and returns.
/// Never panics on caller. Spawned-task panic recovery is the caller's
/// responsibility (event_loop wraps with a top-level error log).
#[allow(clippy::too_many_arguments)]
pub async fn handle_cidnotify_lifted<R: tauri::Runtime>(
    outbox_arc: std::sync::Arc<tokio::sync::Mutex<DmOutbox>>,
    state_arc: std::sync::Arc<tokio::sync::Mutex<OwnerState>>,
    cas: std::sync::Arc<dyn ContentStore>,
    unicast_send_tx: tokio::sync::mpsc::Sender<UnicastSendRequest>,
    app: tauri::AppHandle<R>,
    signed: crate::dm_envelope::DmCidNotifySigned,
    signature: [u8; 64],
    signed_bytes: Vec<u8>,
    wall_now_ms: u64,
) {
    // Phase A — locked, fast: verify + resolve + snapshot.
    let (space_a, identity_pub, resolved_owner) = {
        let _outbox_g = outbox_arc.lock().await;
        let state_g = state_arc.lock().await;
        let identity_pub = match lookup_pubkey_for_device(
            &state_g.owner_device_cache,
            signed.signing_device_hash,
        ) {
            Some(pub_) => pub_,
            None => {
                tracing::warn!(
                    error = ?DmReceiveError::UnknownSigningKey,
                    "handle_cidnotify_lifted Phase A: dropping packet"
                );
                return;
            }
        };
        if let Err(e) = crate::dm_signing::verify_dm_packet_signature(
            &signed_bytes,
            &signature,
            &identity_pub,
            signed.signing_device_hash,
        ) {
            tracing::warn!(error = ?e, "handle_cidnotify_lifted Phase A: dropping packet");
            return;
        }
        let resolved_owner = match resolve_signed_origin_owner(
            &state_g.owner_device_cache,
            signed.signing_device_hash,
        ) {
            Ok(addr) => addr,
            Err(e) => {
                tracing::warn!(error = ?e, "handle_cidnotify_lifted Phase A: dropping packet");
                return;
            }
        };
        if signed.sender_owner_addr != resolved_owner {
            tracing::warn!(
                error = ?DmReceiveError::OwnerFieldMismatch,
                "handle_cidnotify_lifted Phase A: dropping packet"
            );
            return;
        }
        let space = match state_g.spaces.get(&signed.space_id) {
            Some(s) => s.clone(),
            None => {
                tracing::warn!(
                    error = ?DmReceiveError::SpaceNotFound,
                    "handle_cidnotify_lifted Phase A: dropping packet"
                );
                return;
            }
        };
        if !space.members.contains(&resolved_owner) {
            tracing::warn!(
                error = ?DmReceiveError::SenderNotInSpaceMembers,
                "handle_cidnotify_lifted Phase A: dropping packet"
            );
            return;
        }
        (space, identity_pub, resolved_owner)
    }; // locks dropped here

    // Phase B — unlocked, slow: CAS fetch.
    let blob = match tokio::time::timeout(
        std::time::Duration::from_millis(500),
        cas.get(&signed.message_cid),
    )
    .await
    {
        Ok(Ok(Some(bytes))) => bytes,
        Ok(Ok(None)) => {
            tracing::warn!(
                error = ?DmReceiveError::CasFetchFailed("blob not found".into()),
                "handle_cidnotify_lifted Phase B: dropping packet"
            );
            return;
        }
        Ok(Err(e)) => {
            tracing::warn!(
                error = ?DmReceiveError::CasFetchFailed(format!("{e:?}")),
                "handle_cidnotify_lifted Phase B: dropping packet"
            );
            return;
        }
        Err(_) => {
            tracing::warn!(
                error = ?DmReceiveError::CasFetchFailed("500ms fetch timeout".into()),
                "handle_cidnotify_lifted Phase B: dropping packet"
            );
            return;
        }
    };

    // Phase C — re-locked, fast: re-fetch Space + decrypt + apply + ack.
    let drain_outcome = {
        let outbox_g = outbox_arc.lock().await;
        let mut state_g = state_arc.lock().await;

        // TOCTOU re-check: Space may have been deleted, members may have
        // shrunk (GroupDm), or content_key may have rotated.
        let space_c = match state_g.spaces.get(&signed.space_id) {
            Some(s) => s.clone(),
            None => {
                tracing::warn!(
                    error = ?DmReceiveError::SpaceNotFound,
                    "handle_cidnotify_lifted Phase C: dropping packet (Space deleted in TOCTOU window)"
                );
                return;
            }
        };
        if !space_c.members.contains(&resolved_owner) {
            tracing::warn!(
                error = ?DmReceiveError::SenderNotInSpaceMembers,
                "handle_cidnotify_lifted Phase C: dropping packet (sender lost membership in TOCTOU window)"
            );
            return;
        }

        // Decrypt with current Space + prior_content_keys fallback —
        // handles content_key rotation between Phase A and Phase C.
        let aad = match crate::dm_crypto::compute_aad(&space_c) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(
                    error = ?DmReceiveError::AadCompute(e.to_string()),
                    "handle_cidnotify_lifted Phase C: dropping packet"
                );
                return;
            }
        };
        let payload = match crate::dm_crypto::decrypt_dm_message(
            space_c.content_key.as_ref()
                .expect("DM Space MUST have content_key per validate_invariants"),
            &space_c.prior_content_keys,
            &aad,
            &blob,
        ) {
            Ok(p) => p,
            Err(_) => {
                tracing::warn!(
                    error = ?DmReceiveError::DecryptFailed,
                    "handle_cidnotify_lifted Phase C: dropping packet"
                );
                return;
            }
        };

        // Sender-binding check.
        if let Err(_) = crate::dm_crypto::verify_sender_binding(&payload, resolved_owner) {
            tracing::warn!(
                error = ?DmReceiveError::SenderImpersonation,
                "handle_cidnotify_lifted Phase C: dropping packet"
            );
            return;
        }

        // Refresh OwnerDeviceCache (Step 8 in original handle_cidnotify).
        let mut updated_pubs: Vec<Option<[u8; 64]>> =
            vec![None; signed.sender_devices.len()];
        if let Some(idx) = signed.sender_devices.iter()
            .position(|d| *d == signed.signing_device_hash)
        {
            updated_pubs[idx] = Some(identity_pub);
        }
        let _ = state_g.apply_owner_device_update(
            resolved_owner,
            signed.sender_devices.clone(),
            updated_pubs,
            Hlc {
                wall_ms: wall_now_ms,
                logical: 0,
                device_id: outbox_g.device_id.clone(),
            },
        );

        // apply_inbox — atomic-emit semantics.
        let inbox_entry = crate::owner_state_types::InboxEntry {
            space_id: signed.space_id,
            message_cid: signed.message_cid,
            from: resolved_owner,
            received_at: Hlc {
                wall_ms: wall_now_ms,
                logical: 0,
                device_id: outbox_g.device_id.clone(),
            },
        };
        let outcome = state_g.apply_inbox(inbox_entry.clone());
        let mut drain_outcome = DrainOutcome::default();
        if matches!(outcome, ApplyOutcome::Inserted) {
            drain_outcome.newly_received.push(crate::owner_state_types::ReceivedMessage {
                inbox_entry,
                body: payload.body.clone(),
                mime_type: payload.mime_type.clone(),
                sent_at: payload.sent_at.clone(),
            });
        }

        // Build + try_send acks (cheap — non-blocking).
        let our_ack_devices = state_g.owner_device_cache.devices
            .get(&outbox_g.self_owner)
            .map(|e| e.devices.clone())
            .filter(|devs| devs.contains(&outbox_g.our_signing_device_hash))
            .unwrap_or_else(|| vec![outbox_g.our_signing_device_hash]);
        let ack_signed = crate::dm_envelope::DmAckSigned {
            space_id: signed.space_id,
            message_cid: signed.message_cid,
            ack_from_owner_addr: outbox_g.self_owner,
            ack_from_devices: our_ack_devices,
            signing_device_hash: outbox_g.our_signing_device_hash,
        };
        let ack_packet = match crate::dm_envelope::build_signed_ack(ack_signed, &outbox_g.signing_key) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    error = ?DmReceiveError::Decode(format!("build_signed_ack: {e}")),
                    "handle_cidnotify_lifted Phase C: ack build failed"
                );
                return;
            }
        };
        let ack_wire = match crate::dm_envelope::encode_packet(&ack_packet) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(
                    error = ?DmReceiveError::Decode(format!("encode_packet ack: {e}")),
                    "handle_cidnotify_lifted Phase C: ack encode failed"
                );
                return;
            }
        };
        for device in &signed.sender_devices {
            let dest_hash = crate::dm_signing::compute_dm_destination_hash(device.0);
            if let Err(e) = unicast_send_tx.try_send(UnicastSendRequest {
                destination_hash: dest_hash,
                packet: ack_wire.clone(),
            }) {
                tracing::warn!(
                    error = ?e,
                    "handle_cidnotify_lifted Phase C: ack fan-out dropped due to channel pressure"
                );
            }
        }

        // Suppress unused-variable warning: space_a is captured for future
        // diff-vs-snapshot diagnostics if needed; kept for traceability.
        let _ = space_a;

        drain_outcome
    }; // locks dropped here

    // IPC emit — locks released, safe to .await on app.emit (synchronous
    // anyway, but we're outside the lock scope per existing event_loop pattern).
    for rm in drain_outcome.newly_received {
        let _ = app.emit(
            "dm-received",
            serde_json::json!({
                "spaceId": hex::encode(rm.inbox_entry.space_id.0),
                "messageCid": hex::encode(rm.inbox_entry.message_cid.to_bytes()),
                "from": hex::encode(rm.inbox_entry.from.0),
                "receivedAt": rm.inbox_entry.received_at.wall_ms,
                "sentAt": rm.sent_at.wall_ms,
                "body": hex::encode(&rm.body),
                "mimeType": rm.mime_type,
            }),
        );
    }
}
```

NOTE: this is a long function — that's by design. The phase boundaries are explicit (`{ ... }` blocks for lock scope), and inlining keeps the lock-lifecycle visible at one glance. Splitting into per-phase helpers would obscure the invariant.

Confirm there are no stray uses of `space_a` (unused variable on the happy path — captured for traceability only). The `let _ = space_a;` line suppresses the warning; alternatively name it `_space_a` to suppress without the discard expression.

- [ ] **Step 3: Confirm imports**

The new method needs these in scope (most already imported):
- `tauri::Runtime`, `tauri::AppHandle`
- `std::sync::Arc`, `tokio::sync::Mutex`
- `Hlc`, `ApplyOutcome`, `DmReceiveError`, `DrainOutcome`
- `lookup_pubkey_for_device`, `resolve_signed_origin_owner`

Add any missing imports at the top of `dm_outbox.rs`.

- [ ] **Step 4: Run formatter + clippy + full nextest**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: 0 fmt diff, 0 clippy warnings, all existing tests pass (no behavior change yet — old `handle_cidnotify` still used).

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/dm_outbox.rs
git commit -m "$(cat <<'EOF'
feat(zeb-241): add handle_cidnotify_lifted alongside old monolith

Three-phase lifted variant: Phase A (locked: verify + resolve +
Space snapshot) → Phase B (unlocked: 500ms CAS fetch) → Phase C
(re-locked: TOCTOU re-check + decrypt with prior_content_keys
fallback + apply_inbox + ack fan-out + IPC emit).

Old handle_cidnotify(&mut self, &mut state, ...) stays for now;
event_loop wiring switch + old-handler removal + test migration
land in subsequent commits.

Spec: docs/specs/2026-05-11-zeb-241-handle-cidnotify-lock-lift-design.md (8f2e196)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Wire event_loop + remove old handle_cidnotify + migrate tests

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (pre-decode + spawn for CidNotify; existing path now Invite/Ack-only)
- Modify: `src-tauri/src/dm_outbox.rs` (remove old `handle_cidnotify`; remove its dispatch from `handle_unicast`)
- Modify: `src-tauri/src/dm_outbox.rs::tests` (migrate existing `handle_cidnotify_*` tests to call `handle_cidnotify_lifted` with stub Arcs)

- [ ] **Step 1: Modify `event_loop.rs::handle_runtime_action_or_dispatch`**

In the `RuntimeAction::UnicastReceived` branch (around line 1577-1664), add a pre-decode + dispatch:

```rust
if let (Some(outbox), Some(state), Some(cas), Some(tx)) =
    (dm_outbox, crdt_state, cas_handle, unicast_send_tx)
{
    // ZEB-241: pre-decode to detect CidNotify; spawn lifted handler
    // so 500ms CAS fetch doesn't hold outbox + state locks.
    let packet_bytes = match &action {
        RuntimeAction::UnicastReceived { packet, .. } => packet.clone(),
        _ => unreachable!("matched UnicastReceived above"),
    };
    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    match crate::dm_envelope::decode_packet(&packet_bytes) {
        Ok(crate::dm_envelope::DmPacket::CidNotify {
            signed,
            signature,
            signed_bytes,
        }) => {
            // Spawn lifted handler — fire-and-forget. Panic recovery:
            // tokio::spawn swallows panics; the handler is structured
            // to never panic (all errors are tracing::warn! and
            // early-return).
            let outbox_clone = std::sync::Arc::clone(outbox);
            let state_clone = std::sync::Arc::clone(state);
            let cas_clone = std::sync::Arc::clone(cas);
            let tx_clone = tx.clone();
            let app_clone = app.clone();
            tokio::spawn(async move {
                crate::dm_outbox::DmOutbox::handle_cidnotify_lifted(
                    outbox_clone,
                    state_clone,
                    cas_clone,
                    tx_clone,
                    app_clone,
                    signed,
                    signature,
                    signed_bytes.to_vec(),
                    wall_now_ms,
                )
                .await;
            });
            return;
        }
        Ok(_) | Err(_) => {
            // Invite / Ack / decode-failure: fall through to existing
            // try_lock + handle_unicast path (unchanged behavior).
        }
    }

    // Existing path for Invite/Ack/decode-failure.
    let outbox_try = outbox.try_lock();
    let state_try = state.try_lock();
    match (outbox_try, state_try) {
        // ...existing match arms unchanged...
    }
    return;
}
```

NOTE: the `signed_bytes` field on `decode_packet`'s output is `&[u8]` borrowed from `packet_bytes`; we need an owned `Vec<u8>` for the spawned task. The `.to_vec()` clone is unavoidable since `tokio::spawn` requires `'static`.

- [ ] **Step 2: Remove old `handle_cidnotify` + its dispatch in `handle_unicast`**

In `dm_outbox.rs`:
1. In `handle_unicast`'s match (line ~877-911), remove the `DmPacket::CidNotify { ... }` arm. The remaining arms are `Invite` and `Ack`.
2. Delete the entire `handle_cidnotify(&mut self, &mut state, ...)` method (currently lines ~1105-1303).

- [ ] **Step 3: Migrate existing `handle_cidnotify_*` unit tests**

Search for tests that call the old method:
```bash
grep -n "handle_cidnotify\|fn handle_cidnotify" src-tauri/src/dm_outbox.rs | head -30
```

Each test that previously called `outbox.handle_cidnotify(&mut state, ...)` needs to:
1. Build `Arc<Mutex<DmOutbox>>` and `Arc<Mutex<OwnerState>>` from the test's `outbox`/`state`.
2. Build a stub `Arc<dyn ContentStore>` (the existing `InMemoryStub` should work — wrap in Arc).
3. Build a stub AppHandle for `app` parameter — use `tauri::test::mock_app()` or similar.
4. Call `DmOutbox::handle_cidnotify_lifted(outbox_arc, state_arc, cas_arc, tx, app, signed, signature, signed_bytes.to_vec(), wall_now_ms).await`
5. Acquire the locks AFTER the call to inspect resulting state for assertions.

For tests that previously asserted on the returned `DrainOutcome`: the lifted variant doesn't return — it emits directly via `app.emit`. Tests need to either:
- Inspect state via `state_arc.lock().await` post-call (most assertions), OR
- Use `tauri::test::MockRuntime`-style event recording to capture `dm-received` emits.

If `tauri::test::mock_app` is unavailable or scaffolds too much, an alternative is to have the test build a simple AppHandle wrapper that records emits to a shared `Vec<Value>`. Existing tests in the codebase may already do this — check `dm_send_integration.rs` and `pairing_integration.rs` for patterns.

- [ ] **Step 4: Run all gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: all green. If any test fails on the migration, diagnose + fix before proceeding.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/event_loop.rs src-tauri/src/dm_outbox.rs
git commit -m "$(cat <<'EOF'
refactor(zeb-241): wire event_loop to spawn handle_cidnotify_lifted; remove old monolith

event_loop pre-decodes UnicastReceived; CidNotify packets spawn
handle_cidnotify_lifted (fire-and-forget). Invite/Ack continue
through the existing try_lock + handle_unicast path (unchanged).

Removes the old handle_cidnotify(&mut self, &mut state, ...) and
its dispatch from handle_unicast. Single source of truth for
production CidNotify handling.

Migrates existing handle_cidnotify_* unit tests to call the
lifted variant with stub Arcs.

Spec: docs/specs/2026-05-11-zeb-241-handle-cidnotify-lock-lift-design.md (8f2e196)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Add 3 TOCTOU regression tests

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs::tests` (add 3 new `#[tokio::test]` functions)

Per spec §6:

- [ ] **Step 1: Add `handle_cidnotify_lifted_decrypts_via_prior_keys_when_content_key_rotates_during_lift`**

Test that the prior_content_keys fallback handles content_key rotation between Phase A and Phase C. Implementation hints:
- Use a CAS stub that introduces a controllable delay (e.g., `tokio::sync::oneshot` to gate the `cas.get` return).
- Phase A snapshots Space with K1.
- During the gated CAS fetch, the test rotates the Space's content_key in `state` to K2 with `prior_content_keys=[K1]`.
- Phase C decrypts using K2 + prior_content_keys=[K1]; the K1-encrypted blob succeeds via fallback.
- Assert: state's inbox contains the new InboxEntry; ack was try_send'd; no DecryptFailed warning.

- [ ] **Step 2: Add `handle_cidnotify_lifted_returns_space_not_found_when_space_deleted_during_lift`**

Similar setup but during Phase B, the test removes the Space from state. Phase C re-checks → SpaceNotFound → early return. Assert: state's inbox is unchanged; no ack was try_send'd.

- [ ] **Step 3: Add `handle_cidnotify_lifted_returns_sender_not_in_members_when_kicked_during_lift`**

GroupDm Space; sender ∈ members at Phase A; during Phase B, test removes sender from `space.members`. Phase C → SenderNotInSpaceMembers → early return.

- [ ] **Step 4: Run tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --features test-fixtures \
    -E 'test(handle_cidnotify_lifted)'
```

Expected: 3 new tests + all existing handle_cidnotify_lifted tests pass.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/dm_outbox.rs
git commit -m "$(cat <<'EOF'
test(zeb-241): TOCTOU regression tests for handle_cidnotify_lifted

Three regression tests exercising the Phase A / Phase C TOCTOU
window:
- content_key rotation between phases (decrypt via prior_content_keys)
- Space deleted between phases (Phase C returns SpaceNotFound)
- GroupDm member kicked between phases (Phase C returns
  SenderNotInSpaceMembers)

Spec: docs/specs/2026-05-11-zeb-241-handle-cidnotify-lock-lift-design.md (8f2e196)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Final verification + push + PR creation

**Files:** none (verification + remote actions only)

- [ ] **Step 1: Run all 5 CI gates**

From `src-tauri/`:
```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cargo check --locked --all-targets --features test-fixtures
```

From repo root:
```bash
npx tsc --noEmit
npx vitest run
```

- [ ] **Step 2: Push branch**

```bash
git push -u origin zeb-241-handle-cidnotify-lock-lift
```

- [ ] **Step 3: Create PR**

```bash
gh pr create --title "ZEB-241: lift CAS fetch + ack fan-out outside DmOutbox/OwnerState locks in handle_cidnotify" --body "$(cat <<'EOF'
## Summary
- Fixes ZEB-241: `handle_cidnotify`'s 500ms CAS fetch was holding `DmOutbox` + `OwnerState` mutex guards across the await, causing event_loop lock contention (`try_lock` retry path) under load
- New `DmOutbox::handle_cidnotify_lifted` runs as a fire-and-forget spawned task: Phase A (locked: verify + resolve + Space snapshot) → Phase B (unlocked: 500ms CAS fetch) → Phase C (re-locked: TOCTOU re-check + decrypt with prior_content_keys fallback + apply + ack)
- event_loop pre-decodes UnicastReceived; CidNotify spawns the lifted task, Invite/Ack continue through the existing try_lock + handle_unicast path
- Old monolithic `handle_cidnotify(&mut self, &mut state, ...)` removed; existing unit tests migrated to call the lifted variant

## TOCTOU handling
Phase A snapshot is advisory; Phase C is authoritative. Three TOCTOU windows handled:
1. **content_key rotation** — Phase C decrypts using current Space's `content_key` + `prior_content_keys` fallback (already in `dm_crypto::decrypt_dm_message`)
2. **Space deleted** — Phase C re-fetches Space; absent → SpaceNotFound → drop
3. **GroupDm member kicked** — Phase C re-checks `space.members.contains(resolved_owner)` → SenderNotInSpaceMembers → drop

3 new regression tests cover each window.

## Spec
[docs/specs/2026-05-11-zeb-241-handle-cidnotify-lock-lift-design.md](https://github.com/zeblithic/harmony-client/blob/zeb-241-handle-cidnotify-lock-lift/docs/specs/2026-05-11-zeb-241-handle-cidnotify-lock-lift-design.md) (commit `8f2e196`)

## Plan
[docs/plans/2026-05-11-zeb-241-handle-cidnotify-lock-lift-plan.md](https://github.com/zeblithic/harmony-client/blob/zeb-241-handle-cidnotify-lock-lift/docs/plans/2026-05-11-zeb-241-handle-cidnotify-lock-lift-plan.md)

## Notable design decisions (do NOT relitigate)
1. **Spawn-per-CidNotify task.** Inline `.await` on locks would re-block event_loop, defeating the lift's purpose.
2. **Removes old `handle_cidnotify` (single source of truth in lifted variant).** Test migration is mechanical (&mut → Arc).
3. **Invite/Ack stay synchronous.** No slow operations; lift would be over-rotation.

## Known limitations (per spec §10)
- Spawned-task panic recovery is structural: handler is panic-free by construction (all errors are `tracing::warn!` + early return). No `panic::catch_unwind` wrapper added since there's no panic site.
- No bounded queue for spawned tasks; under pathological load (adversary flooding CidNotify), could spawn unbounded. Mitigated by Reticulum transport rate-limiting + bounded `unicast_send_tx` channel + 500ms CAS timeout. Follow-up `tokio::sync::Semaphore` cap if it becomes a problem.

## Test plan
- [x] `cargo fmt --check` clean
- [x] `cargo clippy --features test-fixtures -D warnings` clean
- [x] `cargo nextest run --workspace --features test-fixtures` green (existing + 3 new TOCTOU tests)
- [x] `cargo check --features test-fixtures` (MSRV) clean
- [x] `npx tsc --noEmit` clean
- [x] `npx vitest run` clean
- [x] Existing handle_cidnotify_* tests migrated to handle_cidnotify_lifted; observable behavior unchanged

Resolves ZEB-241
EOF
)"
```

The PR body uses BARE `Resolves ZEB-241` (correct — auto-closes ZEB-241 on merge per Linear's GH integration). Parent ZEB-216 and predecessor PR #80 referenced contextually but not as bare refs (they're already DONE — no auto-cascade needed).

- [ ] **Step 4: NO additional commit** — push + PR is the terminal action.

---

## Self-review checklist

- [x] Spec coverage: every acceptance criterion in spec §8 has a task that satisfies it.
- [x] No placeholders — every step has actual commands or actual code.
- [x] Type consistency — `handle_cidnotify_lifted` signature matches the spec §7. The Arc handles match what event_loop already passes around.
- [x] Each task except Task 0 ends in a commit.
- [x] Final verification (Task 4) covers all 5 CI gates.

## Out of scope (per spec §5)

1. Lifting Invite/Ack handlers — no slow ops.
2. Refactoring handle_unicast's signature uniformly.
3. Bounded queue / Semaphore for spawned tasks (deferred to follow-up if needed).
4. Lifting handle_invite's CAS fetch (none currently).
5. Tuning the 500ms CAS timeout value.
