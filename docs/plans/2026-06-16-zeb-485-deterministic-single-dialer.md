# ZEB-485 Deterministic Single-Dialer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make PQ DM tunnel establishment deterministic by ensuring exactly one iroh connection is ever created per NodeId pair — the lower-NodeId peer is the sole dialer; the higher-NodeId peer buffers and accepts, fallback-dialing after 1s only if no inbound arrives.

**Architecture:** All dial-initiation in `TunnelManager::send_dm` routes through a new `dial_or_await` gate that compares `self_node_id` to `peer_node_id`. The higher peer parks its outbound DMs in a new `TunnelHandleState::AwaitingInbound` handle (tagged `role = Initiator`, so the existing `keep_new` lower-wins dedup math needs no changes) and arms a 1s fallback-dial timer. The existing collision-dedup machinery (`keep_new`, `register_inbound`, `note_active`, `drain_pending_into`) stays untouched as defense-in-depth.

**Tech Stack:** Rust, `tokio` (mpsc + timers), `iroh` QUIC, the sans-I/O `harmony_tunnel` session. All changes are in `src-tauri/src/tunnel_manager.rs` plus un-ignoring one e2e test.

**Spec:** `docs/specs/2026-06-16-zeb-485-deterministic-single-dialer-design.md`

---

## File structure

- `src-tauri/src/tunnel_manager.rs` — the entire production change: `AwaitingInbound` variant, `dial_or_await` gate, `spawn_await_inbound` (with fallback timer), `FALLBACK_DIAL_DELAY` const, and the routing of all three `spawn_dial` call sites through the gate. New unit tests live in its existing `#[cfg(test)] mod tests`.
- `e2e-harness/tests/e2e_two_node.rs` — un-ignore `s2_dm_delivery_over_tunnel_hard_assert` (Task 4, after the e2e passes 5/5).

## Background the implementer needs

- **Crate is `harmony-app`.** Run unit tests/lints scoped to the lib to avoid the ~25-min integration relink: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(tunnel_manager)'` and `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`. Always `cargo fmt --all` before committing (CI gate).
- **`self_node_id` is random per test** (`test_manager()` mints a fresh PQ identity). To exercise the gate deterministically without controlling it: a peer of `[0xFF; 32]` is **always ≥ self** (so *we* are lower → we dial); a peer of `[0x00; 32]` is **always ≤ self** (so *we* are higher → we await). NodeIds are `blake3` hashes, so self is never all-`0x00` or all-`0xFF`.
- **`handle_snapshot(&peer)`** returns `Option<(TunnelHandleState, TunnelRole, usize)>` (state, role, pending-len) — the test observation seam. `fixed_node_id(b) = [b; 32]`.
- **Existing `send_dm` dial sites** (all currently call `spawn_dial`): the `None` arm (initial dial), the `Active` → `TrySendError::Closed(SendDm)` arm (redial after loop death, seeds the failed packet), and the `Closing` arm (redial after a dedup-loser teardown). Each passes a `seed_pending: Vec<Vec<u8>>`.
- **`spawn_dial`** inserts a `Dialing`/`Initiator` handle (with a double-check that re-routes seeds if an inbound raced in) and spawns `run_tunnel_initiator`. The new `spawn_await_inbound` mirrors its structure.

---

### Task 1: The dial-or-await gate + `AwaitingInbound` state (no fallback timer yet)

**Files:**
- Modify: `src-tauri/src/tunnel_manager.rs` (enum, `send_dm` `None` arm, new `dial_or_await` + `spawn_await_inbound`, new tests)

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src-tauri/src/tunnel_manager.rs`:

```rust
#[tokio::test(flavor = "current_thread")]
async fn send_dm_lower_self_dials_immediately() {
    // ZEB-485: when WE are the lower NodeId, send_dm to an unknown peer dials
    // right away (a Dialing/Initiator handle appears synchronously).
    let (mgr, _ingest_rx) = test_manager();
    let peer = fixed_node_id(0xFF); // always >= our (hashed) self => we are lower.
    let contact = DeviceTunnelContact {
        iroh_node_id: peer,
        home_relay_url: None,
        pq_dsa_pubkey: vec![1; 1952],
        pq_kem_pubkey: vec![2; 1184],
    };
    mgr.send_dm(peer, &contact, b"hi".to_vec());
    assert_eq!(
        mgr.handle_snapshot(&peer).map(|(s, r, _)| (s, r)),
        Some((TunnelHandleState::Dialing, TunnelRole::Initiator)),
        "lower-NodeId self must dial immediately"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn send_dm_higher_self_awaits_inbound() {
    // ZEB-485: when WE are the higher NodeId, send_dm buffers in an
    // AwaitingInbound handle instead of dialing.
    let (mgr, _ingest_rx) = test_manager();
    let peer = fixed_node_id(0x00); // always <= our (hashed) self => we are higher.
    let contact = DeviceTunnelContact {
        iroh_node_id: peer,
        home_relay_url: None,
        pq_dsa_pubkey: vec![1; 1952],
        pq_kem_pubkey: vec![2; 1184],
    };
    mgr.send_dm(peer, &contact, b"hi".to_vec());
    assert_eq!(
        mgr.handle_snapshot(&peer).map(|(s, _, p)| (s, p)),
        Some((TunnelHandleState::AwaitingInbound, 1)),
        "higher-NodeId self must buffer + await the inbound dial"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(send_dm_lower_self_dials_immediately) + test(send_dm_higher_self_awaits_inbound)'`
Expected: compile error (`AwaitingInbound` does not exist) — that is a valid failing-test state for TDD.

- [ ] **Step 3: Add the `AwaitingInbound` variant**

In the `TunnelHandleState` enum (currently `Dialing` / `Active` / `Closing`), add:

```rust
    /// ZEB-485: higher-NodeId peer is buffering DMs and waiting to ACCEPT the
    /// lower peer's inbound dial (the single-dialer rule). A fallback timer
    /// promotes this to `Dialing` if no inbound arrives within
    /// `FALLBACK_DIAL_DELAY`. Like `Dialing`, DMs queue in `pending`.
    AwaitingInbound,
```

- [ ] **Step 4: Handle the new variant in `send_dm`'s existing-handle match**

In `send_dm`, the `Some(handle) => match handle.state { ... }` block: change the `Dialing` arm so it also covers `AwaitingInbound` (both just buffer):

```rust
                TunnelHandleState::Dialing | TunnelHandleState::AwaitingInbound => {
                    push_pending(&mut handle.pending, packet);
                }
```

- [ ] **Step 5: Add the `dial_or_await` gate and route the `None` arm through it**

Add these methods to `impl TunnelManager` (place them right after `send_dm`, before `spawn_dial`):

```rust
    /// ZEB-485 single-dialer gate. The LOWER NodeId is the sole dialer; the
    /// higher NodeId buffers `seed_pending` and waits to accept the lower
    /// peer's inbound dial. Routes EVERY fresh dial-initiation so a redial
    /// can't re-create the simultaneous-dial collision either.
    fn dial_or_await(
        self: &Arc<Self>,
        peer_node_id: [u8; 32],
        contact: &DeviceTunnelContact,
        seed_pending: Vec<Vec<u8>>,
    ) {
        if self.self_node_id < peer_node_id {
            self.spawn_dial(peer_node_id, contact, seed_pending);
        } else {
            self.spawn_await_inbound(peer_node_id, contact, seed_pending);
        }
    }

    /// Insert an `AwaitingInbound` handle holding `seed_pending` and wait for
    /// the lower peer's inbound dial. (Task 2 adds the fallback-dial timer.)
    fn spawn_await_inbound(
        self: &Arc<Self>,
        peer_node_id: [u8; 32],
        _contact: &DeviceTunnelContact,
        seed_pending: Vec<Vec<u8>>,
    ) {
        let (cmd_tx, _cmd_rx) = mpsc::channel(CMD_CHANNEL_CAP);
        let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
        for p in seed_pending {
            push_pending(&mut pending, p);
        }
        let epoch = self.alloc_epoch();
        let mut sessions = self
            .sessions
            .lock()
            .expect("tunnel sessions mutex poisoned");
        // Double-check: an inbound dial may have raced in while we computed.
        if let Some(existing) = sessions.get_mut(&peer_node_id) {
            for p in pending.drain(..) {
                match existing.state {
                    TunnelHandleState::Active => {
                        let _ = existing.cmd_tx.try_send(TunnelCommand::SendDm(p));
                    }
                    _ => push_pending(&mut existing.pending, p),
                }
            }
            return;
        }
        sessions.insert(
            peer_node_id,
            TunnelHandle {
                cmd_tx,
                state: TunnelHandleState::AwaitingInbound,
                role: TunnelRole::Initiator,
                epoch,
                pending,
            },
        );
    }
```

Then in `send_dm`'s `None` arm, replace the `spawn_dial` call with the gate:

```rust
            None => {
                drop(sessions);
                self.dial_or_await(peer_node_id, contact, vec![packet]);
            }
```

- [ ] **Step 6: Run the tests to verify they pass + full file lints**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(tunnel_manager)'`
Expected: all `tunnel_manager` tests PASS (the two new ones plus the existing suite — `send_dm_buffers_while_dialing`, `note_active_*`, `register_inbound_*`, etc.).
Run: `cd src-tauri && cargo fmt --all && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
Expected: 0 warnings. (If a new exhaustive `match` on `TunnelHandleState` elsewhere now fails to compile, add an `AwaitingInbound` arm that mirrors `Dialing`.)

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/tunnel_manager.rs
git commit -m "feat(zeb-485): single-dialer gate — lower dials, higher awaits (AwaitingInbound)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Fallback dial + route the redial sites through the gate

**Files:**
- Modify: `src-tauri/src/tunnel_manager.rs` (`FALLBACK_DIAL_DELAY` const, extend `spawn_await_inbound`, redial arms, new tests)

- [ ] **Step 1: Write the failing tests**

Add to `mod tests`:

```rust
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn await_inbound_falls_back_to_dial_after_delay() {
    // ZEB-485: if no inbound arrives, the higher peer dials after the delay.
    let (mgr, _ingest_rx) = test_manager();
    let peer = fixed_node_id(0x00); // we are higher => we await first.
    let contact = DeviceTunnelContact {
        iroh_node_id: peer,
        home_relay_url: None,
        pq_dsa_pubkey: vec![1; 1952],
        pq_kem_pubkey: vec![2; 1184],
    };
    mgr.send_dm(peer, &contact, b"hi".to_vec());
    assert_eq!(
        mgr.handle_snapshot(&peer).map(|(s, _, _)| s),
        Some(TunnelHandleState::AwaitingInbound),
        "starts in AwaitingInbound"
    );

    // Before the delay elapses, still awaiting.
    tokio::time::advance(FALLBACK_DIAL_DELAY / 2).await;
    tokio::task::yield_now().await;
    assert_eq!(
        mgr.handle_snapshot(&peer).map(|(s, _, _)| s),
        Some(TunnelHandleState::AwaitingInbound),
        "still awaiting before the fallback delay"
    );

    // After the delay, the fallback fired: the handle is no longer awaiting
    // (it promoted to Dialing; the background dial to the bogus contact may
    // then fail and evict it — either way it left AwaitingInbound).
    tokio::time::advance(FALLBACK_DIAL_DELAY).await;
    tokio::task::yield_now().await;
    assert_ne!(
        mgr.handle_snapshot(&peer).map(|(s, _, _)| s),
        Some(TunnelHandleState::AwaitingInbound),
        "fallback dial must have fired after the delay"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn await_inbound_fallback_is_noop_when_inbound_arrives_first() {
    // ZEB-485: an inbound dial that lands before the delay cancels the fallback.
    let (mgr, _ingest_rx) = test_manager();
    let peer = fixed_node_id(0x00); // we are higher => we await.
    let contact = DeviceTunnelContact {
        iroh_node_id: peer,
        home_relay_url: None,
        pq_dsa_pubkey: vec![1; 1952],
        pq_kem_pubkey: vec![2; 1184],
    };
    mgr.send_dm(peer, &contact, b"hi".to_vec());

    // The lower peer's inbound dial lands: keep_new(peer<=self)=true keeps the
    // inbound Responder session and drains our buffered DM onto it.
    let _rx = mgr.register_inbound(peer);
    assert_eq!(
        mgr.handle_snapshot(&peer).map(|(s, r, _)| (s, r)),
        Some((TunnelHandleState::Active, TunnelRole::Responder)),
        "inbound replaced the awaiting handle"
    );

    // Advancing past the delay must NOT promote anything (no second dial).
    tokio::time::advance(FALLBACK_DIAL_DELAY * 2).await;
    tokio::task::yield_now().await;
    assert_eq!(
        mgr.handle_snapshot(&peer).map(|(s, r, _)| (s, r)),
        Some((TunnelHandleState::Active, TunnelRole::Responder)),
        "fallback must be a no-op once an inbound session exists"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(await_inbound_falls_back_to_dial_after_delay) + test(await_inbound_fallback_is_noop_when_inbound_arrives_first)'`
Expected: compile error (`FALLBACK_DIAL_DELAY` does not exist), or the fallback test fails because the handle stays `AwaitingInbound` forever.

- [ ] **Step 3: Add the `FALLBACK_DIAL_DELAY` constant**

Near the other consts at the top of the file (after `MAX_PENDING_PER_PEER`):

```rust
/// ZEB-485: how long the higher-NodeId peer waits to ACCEPT the lower peer's
/// inbound dial before dialing itself. The lower peer dials immediately, so an
/// inbound normally lands in tens of ms; this only fires when the lower peer
/// has nothing to send (so isn't dialing). Short enough for responsive
/// liveness, long enough that a normal inbound cancels it. Durability is
/// covered by the always-deposit rung during the wait.
const FALLBACK_DIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
```

- [ ] **Step 4: Extend `spawn_await_inbound` to arm the fallback timer**

Replace the whole `spawn_await_inbound` body from Task 1 with the version that retains `cmd_rx` and arms the timer:

```rust
    /// Insert an `AwaitingInbound` handle holding `seed_pending`, wait for the
    /// lower peer's inbound dial, and arm a fallback dial: if no inbound has
    /// registered within `FALLBACK_DIAL_DELAY`, promote to a real dial (the
    /// lower peer isn't dialing). The retained `cmd_rx` is handed to
    /// `run_tunnel_initiator` only if the fallback fires.
    fn spawn_await_inbound(
        self: &Arc<Self>,
        peer_node_id: [u8; 32],
        contact: &DeviceTunnelContact,
        seed_pending: Vec<Vec<u8>>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_CAP);
        let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
        for p in seed_pending {
            push_pending(&mut pending, p);
        }
        let epoch = self.alloc_epoch();
        {
            let mut sessions = self
                .sessions
                .lock()
                .expect("tunnel sessions mutex poisoned");
            // Double-check: an inbound dial may have raced in.
            if let Some(existing) = sessions.get_mut(&peer_node_id) {
                for p in pending.drain(..) {
                    match existing.state {
                        TunnelHandleState::Active => {
                            let _ = existing.cmd_tx.try_send(TunnelCommand::SendDm(p));
                        }
                        _ => push_pending(&mut existing.pending, p),
                    }
                }
                return;
            }
            sessions.insert(
                peer_node_id,
                TunnelHandle {
                    cmd_tx,
                    state: TunnelHandleState::AwaitingInbound,
                    role: TunnelRole::Initiator,
                    epoch,
                    pending,
                },
            );
        }

        // Arm the fallback dial.
        let mgr = Arc::clone(self);
        let endpoint = self.endpoint.clone();
        let local_pq = Arc::clone(&self.local_pq);
        let ingest_tx = self.ingest_tx.clone();
        let contact = contact.clone();
        tokio::spawn(async move {
            tokio::time::sleep(FALLBACK_DIAL_DELAY).await;
            // Promote to a real dial ONLY if still awaiting under our epoch (an
            // inbound that landed first replaced the handle / changed the epoch).
            {
                let mut sessions = mgr
                    .sessions
                    .lock()
                    .expect("tunnel sessions mutex poisoned");
                match sessions.get_mut(&peer_node_id) {
                    Some(h)
                        if h.state == TunnelHandleState::AwaitingInbound
                            && h.epoch == epoch =>
                    {
                        h.state = TunnelHandleState::Dialing;
                    }
                    _ => return, // inbound arrived / evicted / replaced — done.
                }
            }
            crate::tunnel_task::run_tunnel_initiator(
                endpoint, contact, local_pq, peer_node_id, mgr, epoch, ingest_tx, cmd_rx,
            )
            .await;
        });
    }
```

- [ ] **Step 5: Route the two redial sites through the gate**

In `send_dm`, the `Active` arm's `TrySendError::Closed(TunnelCommand::SendDm(packet))` branch — replace `self.spawn_dial(peer_node_id, contact, vec![packet]);` with:

```rust
                                self.dial_or_await(peer_node_id, contact, vec![packet]);
```

In the same arm's loud-invariant `TrySendError::Closed(_)` branch — replace `self.spawn_dial(peer_node_id, contact, vec![]);` with:

```rust
                                self.dial_or_await(peer_node_id, contact, vec![]);
```

In the `Closing` arm — replace `self.spawn_dial(peer_node_id, contact, vec![packet]);` with:

```rust
                    self.dial_or_await(peer_node_id, contact, vec![packet]);
```

- [ ] **Step 6: Run the tests + lints**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(tunnel_manager)'`
Expected: all `tunnel_manager` tests PASS, including the two new fallback tests.
Run: `cd src-tauri && cargo fmt --all && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
Expected: 0 warnings.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/tunnel_manager.rs
git commit -m "feat(zeb-485): fallback-dial after 1s + route all redial sites through the gate

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: Confirm `register_inbound` drains an `AwaitingInbound` handle (no production change expected)

**Files:**
- Modify: `src-tauri/src/tunnel_manager.rs` (one new test)

- [ ] **Step 1: Write the test**

Add to `mod tests`:

```rust
#[tokio::test(flavor = "current_thread")]
async fn register_inbound_drains_awaiting_inbound_pending() {
    // ZEB-485: when the lower peer's inbound dial lands while we (higher) are
    // AwaitingInbound, register_inbound keeps the inbound and redirects our
    // buffered DMs onto it. AwaitingInbound is tagged role=Initiator, so the
    // existing keep_new/drain_pending_into path covers it with NO change.
    let (mgr, _ingest_rx) = test_manager();
    let peer = fixed_node_id(0x00); // peer <= self => keep_new(peer, self) == true.

    // Install an AwaitingInbound handle holding two buffered DMs. Its cmd_tx is
    // dead (rx dropped) — the drain targets the SURVIVOR (the new inbound), not
    // this handle, so that is fine.
    let (cmd_tx, _dead_rx) = mpsc::channel(CMD_CHANNEL_CAP);
    {
        let mut sessions = mgr.sessions.lock().unwrap();
        sessions.insert(
            peer,
            TunnelHandle {
                cmd_tx,
                state: TunnelHandleState::AwaitingInbound,
                role: TunnelRole::Initiator,
                epoch: 0,
                pending: VecDeque::from(vec![b"p1".to_vec(), b"p2".to_vec()]),
            },
        );
    }

    let (mut cmd_rx, _epoch) = mgr.register_inbound(peer);
    assert_eq!(
        mgr.handle_snapshot(&peer).map(|(s, r, _)| (s, r)),
        Some((TunnelHandleState::Active, TunnelRole::Responder)),
        "the inbound survivor replaces the awaiting handle"
    );

    let mut drained = Vec::new();
    while let Ok(TunnelCommand::SendDm(p)) = cmd_rx.try_recv() {
        drained.push(p);
    }
    assert_eq!(
        drained,
        vec![b"p1".to_vec(), b"p2".to_vec()],
        "the awaiting handle's pending DMs are redirected onto the inbound survivor"
    );
}
```

- [ ] **Step 2: Run the test**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(register_inbound_drains_awaiting_inbound_pending)'`
Expected: PASS with NO production change. (If it fails, the `register_inbound` collision path mishandles an `AwaitingInbound` existing handle — add the minimal fix so the inbound survivor wins and `drain_pending_into` redirects the pending; then re-run.)

- [ ] **Step 3: Full `tunnel_manager` suite + lints**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(tunnel_manager)'`
Expected: all PASS.
Run: `cd src-tauri && cargo fmt --all && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
Expected: 0 warnings.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/tunnel_manager.rs
git commit -m "test(zeb-485): register_inbound drains AwaitingInbound pending onto the survivor

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: E2E validation — run S2 5× green, then un-ignore it

> This task runs the real two-node harness, not a unit test. It is the DoD proof. It needs the **release** `harmony-app` binary (the harness spawns it). This is the expensive step — budget for a cold release build; supervise with a wall-clock safety net.

**Files:**
- Modify: `e2e-harness/tests/e2e_two_node.rs` (remove the `#[ignore]` on `s2_dm_delivery_over_tunnel_hard_assert`)

- [ ] **Step 1: Build the release binary with the fix**

Run: `cd src-tauri && cargo build --locked --release --bin harmony-app`
Expected: builds `src-tauri/target/release/harmony-app` (the path `bin_resolver.rs` resolves by default).

- [ ] **Step 2: Run S2 five times (still `#[ignore]`'d — use `--run-ignored all`)**

Run (from `e2e-harness/`), five times:
```bash
cd e2e-harness && for i in 1 2 3 4 5; do \
  echo "=== S2 run $i ==="; \
  cargo nextest run --locked --features e2e --run-ignored all \
    -E 'test(s2_dm_delivery_over_tunnel_hard_assert)' || echo "RUN $i FAILED"; \
done
```
Expected: 5/5 PASS (recipient fires `dm-received`, plaintext lands). If any run fails with `read TunnelAccept: connection lost` reappearing, STOP — the fix is incomplete; inspect the per-run logs under `e2e-harness/target/e2e-runs/s2-hard-*/{alice,bob}.stderr.log` (set `RUST_LOG=harmony_app::tunnel_task=debug,harmony_app::tunnel_manager=debug` to capture the handshake trace) before proceeding.

- [ ] **Step 3: Un-ignore the test**

In `e2e-harness/tests/e2e_two_node.rs`, remove the `#[ignore = "..."]` attribute on `s2_dm_delivery_over_tunnel_hard_assert` (and update/remove its now-stale ignore-reason doc comment to note ZEB-485 closed it). CI never passes `--features e2e`, so the un-ignored test never runs in CI — this is CI-safe.

- [ ] **Step 4: Confirm it now runs by default under the e2e feature**

Run: `cd e2e-harness && cargo nextest run --locked --features e2e -E 'test(s2_dm_delivery_over_tunnel_hard_assert)'`
Expected: PASS (it runs without `--run-ignored`).

- [ ] **Step 5: Commit**

```bash
git add e2e-harness/tests/e2e_two_node.rs
git commit -m "test(zeb-485): un-ignore s2_dm_delivery_over_tunnel_hard_assert (tunnel establishes deterministically)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Final verification (before PR)

- [ ] `cd src-tauri && cargo fmt --all -- --check` → clean
- [ ] `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` → 0 (full `--all-targets` sweep for CI parity)
- [ ] `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(tunnel_manager)'` → all green
- [ ] S2 e2e: 5/5 green (Task 4)
- [ ] Spec DoD met: `s2_dm_delivery_over_tunnel_hard_assert` passes reliably, un-ignored, one connection, no `connection lost` on the surviving path.
