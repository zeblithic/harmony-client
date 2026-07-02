# ZEB-616 — Stale zenoh-face teardown on same-zid reconnect — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When a peer's iroh transport drops mid-session and it reconnects with the same deterministic zenoh zid, close the stale connection it replaces *before* admitting the reconnect's link, so the fresh same-zid declarations install cleanly instead of colliding ("Remapping unsupported").

**Architecture:** Add a per-peer live-connection registry to `IrohZenohLinkManager`. In the accept loop's zenoh-ALPN branch, swap each inbound connection into the registry keyed by the peer's iroh `EndpointId`; if a prior connection was present, close it and (bounded) await its closure — that drives upstream zenoh to reap the stale face — then admit the new link exactly as today. A per-connection `closed()` watcher evicts the registry entry (identity-guarded) so the map stays bounded and drop-and-never-return peers still get reaped. A defense-in-depth zenoh keepalive/lease config shortens the worst-case stale-face lifetime for peers that drop and never reconnect.

**Tech Stack:** Rust, Tokio, iroh 0.98.2 (`iroh::endpoint::Connection`), zenoh 1.9.0, `cargo-nextest`. All new logic lives in `src-tauri/src/zenoh_iroh_transport.rs` plus a small config addition in `src-tauri/src/event_loop.rs`.

## Global Constraints

- **All Cargo commands run from `src-tauri/`.** Always pass `--locked` and `--features test-fixtures`.
- **`--all-targets` is load-bearing** for clippy and tests — integration/test-module compile errors slip through a lib-only run.
- **Clippy gate is `-D warnings`** — no unused items may exist at any task boundary (every new item must have a caller in the same task).
- **macOS one-time setup required:** `spctl developer-mode enable-terminal` + Developer Tools ON, or fresh test binaries hang under XprotectService (per CLAUDE.md).
- **iroh test hygiene:** every test that binds a real iroh endpoint must call `crate::iroh_endpoint::warm_up_iroh_global_init().await` first and wrap its body in a `tokio::time::timeout(Duration::from_secs(45), …)` (first-bind global init costs ~10–30s; see the existing `handshake_connection_queued_pre_install_dispatched_on_install` test).
- **No `std::sync::Mutex` guard held across an `.await`** — the registry lock is taken only for synchronous map ops; the async close/await operates on the *returned* connection after the lock is dropped.
- **Deterministic zid is load-bearing (ZEB-390)** — the fix works *with* stable zids; do not change the zid scheme.
- **Branch:** `zeb-616-stale-face-teardown` (already created off latest `origin/main`; spec committed). Do NOT rebase or switch branches mid-implementation.

## Verified codebase facts (do not re-derive)

- `IrohZenohLinkManager` struct: `src-tauri/src/zenoh_iroh_transport.rs:129`. Constructor `IrohZenohLinkManager::new(endpoint, resolver, new_link_tx) -> Self`: `:207`.
- Accept loop `spawn_accept_loop(self: &Arc<Self>)`: `:378`. The zenoh-ALPN branch (the only code we modify): `:399`–`:421`. It currently does `accept_bi().await` → `let peer_id = conn.remote_id();` → build `IrohZenohLink` → `new_link_tx.send_async(LinkUnicast(NewLink::Single(link)))`.
- `Connection` is imported as `use iroh::endpoint::Connection;` (`:84`). `Duration`/`Instant` imported via `use std::time::{Duration, Instant};` (`:81`).
- iroh 0.98.2 `Connection` API (all confirmed in `iroh-0.98.2/src/endpoint/connection.rs`):
  - `remote_id(&self) -> EndpointId`
  - `stable_id(&self) -> usize`
  - `async closed(&self) -> ConnectionError` (resolves when the connection closes)
  - `close_reason(&self) -> Option<ConnectionError>` (`None` while open)
  - `close(&self, error_code: VarInt, reason: &[u8])` — called in-tree as `conn.close(0u32.into(), b"…")`
  - `Connection: Clone` (cheap handle; cloning does not keep the connection open).
- `EndpointId = PublicKey` and `PublicKey: Copy + Eq + Hash` (iroh-base-0.98.0 `src/key.rs:28`, `impl Hash` at `:72`) — usable directly as a `HashMap` key and freely copied.
- zenoh 1.9.0 config schema (confirmed in `zenoh-config-1.9.0/src/lib.rs:720-723`, defaults `src/defaults.rs:257-258`): `transport/link/tx/lease` is `u64` ms (default `10000`); `transport/link/tx/keep_alive` is `usize` (default `4`), the number of keep-alive messages per lease → probe interval = `lease / keep_alive`.
- In-file test harness (`#[cfg(test)] mod tests`, `:652`): `build_hermetic_iroh_endpoint()` (`:671`) builds a loopback endpoint with a **random** secret. Pattern for driving a real connection is in `handshake_connection_queued_pre_install_dispatched_on_install_inner` (`:841`): build alice manager + `spawn_accept_loop()`, build a dialer endpoint, form `EndpointAddr::new(node_id).with_ip_addr(bound_socket)`, `dialer_ep.inner().connect(addr, alpn).await`. `SecretKey::from_bytes(&[u8;32]).public()` yields an `EndpointId` (`:731`). Tests read private fields directly (e.g. `alice_mgr.pending_handshakes.lock().await`), so the new tests may read `alice_mgr.zenoh_conns` directly.

---

### Task 1: Component A + B — close-old-on-reconnect registry + guarded drop-watcher

**Files:**
- Modify: `src-tauri/src/zenoh_iroh_transport.rs` — add `zenoh_conns` field to the struct (`:129`), init in `new` (`:207`), add module const + two helpers, rewrite the zenoh-ALPN branch (`:399`–`:421`), add a shared-secret endpoint test helper, add three tests to `mod tests`.

**Interfaces:**
- Produces:
  - Struct field `zenoh_conns: std::sync::Mutex<std::collections::HashMap<EndpointId, Connection>>`.
  - `fn swap_zenoh_conn(&self, peer_id: EndpointId, conn: Connection) -> Option<Connection>` (method on `IrohZenohLinkManager`).
  - `fn should_evict_on_close(stored: Option<usize>, watcher: usize) -> bool` (private free fn in the module).
  - `const STALE_CONN_CLOSE_TIMEOUT: Duration` (module const).
  - Test helper `async fn build_hermetic_iroh_endpoint_with_secret(secret: SecretKey) -> Arc<IrohEndpoint>`.
- Consumes: nothing from other tasks.

- [ ] **Step 1: Write the three failing tests**

Add to the `#[cfg(test)] mod tests` block in `src-tauri/src/zenoh_iroh_transport.rs`. First, refactor the existing endpoint helper to expose a shared-secret variant (so two endpoints can share one identity), and add the tests.

Replace the existing `build_hermetic_iroh_endpoint` body (currently `:671`–`:696`) with a delegating pair:

```rust
    /// Build a hermetic iroh endpoint on loopback with a **random**
    /// identity. Delegates to the shared-secret variant.
    async fn build_hermetic_iroh_endpoint() -> Arc<IrohEndpoint> {
        let mut buf = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf);
        build_hermetic_iroh_endpoint_with_secret(SecretKey::from_bytes(&buf)).await
    }

    /// Build a hermetic iroh endpoint on loopback with a **caller-supplied**
    /// identity. Two endpoints built from the same secret share one
    /// `EndpointId` (and hence one deterministic zenoh zid) — the shape of a
    /// ZEB-390 reconnect after a socket rebind. Binds BOTH ALPNs so the
    /// accept loop routes zenoh + handshake connections (mirrors production
    /// bind set, iroh_endpoint.rs).
    async fn build_hermetic_iroh_endpoint_with_secret(secret: SecretKey) -> Arc<IrohEndpoint> {
        let inner = Endpoint::builder(presets::Minimal)
            .secret_key(secret)
            .alpns(vec![
                alpn::HARMONY_ZENOH_V1.to_vec(),
                alpn::HARMONY_HANDSHAKE_V1.to_vec(),
            ])
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("bind_addr loopback")
            .bind()
            .await
            .expect("bind iroh endpoint");
        Arc::new(IrohEndpoint::from_endpoint_for_test(inner))
    }
```

Then add the three tests (also in `mod tests`):

```rust
    /// Component B identity guard (pure): a connection's drop-watcher may
    /// only evict the registry entry when the entry still points at THAT
    /// connection. A superseded watcher (stored != its own id) must not
    /// evict the connection that replaced it.
    #[test]
    fn should_evict_on_close_is_identity_guarded() {
        assert!(should_evict_on_close(Some(7), 7), "own conn still stored → evict");
        assert!(!should_evict_on_close(Some(9), 7), "superseded (9 replaced 7) → keep");
        assert!(!should_evict_on_close(None, 7), "already gone → nothing to evict");
    }

    /// Component A: a same-zid reconnect closes the stale connection it
    /// replaces before admitting the new link, and the registry ends with
    /// exactly the reconnect (no stale duplicate).
    ///
    /// PRE-FIX: `alice_mgr.zenoh_conns` / `swap_zenoh_conn` don't exist →
    /// compile failure. With the field present but the close logic absent,
    /// the `conn1.closed()` await below would instead time out (alice never
    /// closes the stale conn) — that timeout is the behavioral regression
    /// this guards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn zenoh_reconnect_closes_stale_connection() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(
            Duration::from_secs(45),
            zenoh_reconnect_closes_stale_connection_inner(),
        )
        .await
        .expect("test must finish within 45s");
    }

    async fn zenoh_reconnect_closes_stale_connection_inner() {
        // Alice: link manager + accept loop.
        let alice_ep = build_hermetic_iroh_endpoint().await;
        let (new_link_tx, _rx) = flume::unbounded::<LinkUnicast>();
        let alice_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&alice_ep),
            ReachabilityResolver::new(),
            new_link_tx,
        ));
        let _accept = alice_mgr.spawn_accept_loop();

        // Bob's stable identity across two endpoints (a rebind that keeps the
        // node-id → same deterministic zid). `buf` is Copy, so re-deriving the
        // key three times avoids depending on `SecretKey: Clone`.
        let mut buf = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf);
        let bob_id = SecretKey::from_bytes(&buf).public();
        let bob_ep1 = build_hermetic_iroh_endpoint_with_secret(SecretKey::from_bytes(&buf)).await;
        let bob_ep2 = build_hermetic_iroh_endpoint_with_secret(SecretKey::from_bytes(&buf)).await;

        // Alice's dialable loopback address.
        let alice_node_id = alice_ep.node_id();
        let alice_socket = *alice_ep
            .bound_sockets()
            .first()
            .expect("alice has a bound socket");
        let alice_addr = EndpointAddr::new(alice_node_id).with_ip_addr(alice_socket);

        // First connection → alice registers bob under his node-id.
        let conn1 = bob_ep1
            .inner()
            .connect(alice_addr.clone(), alpn::HARMONY_ZENOH_V1)
            .await
            .expect("bob1 dial alice on zenoh ALPN");
        for _ in 0..300 {
            if alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id),
            "alice must register bob's first connection"
        );
        assert!(conn1.close_reason().is_none(), "conn1 open before reconnect");

        // Reconnect: second endpoint, SAME node-id.
        let conn2 = bob_ep2
            .inner()
            .connect(alice_addr, alpn::HARMONY_ZENOH_V1)
            .await
            .expect("bob2 dial alice on zenoh ALPN (same node-id)");

        // THE FIX: alice closes the stale conn1 on the reconnect. Pre-fix
        // this times out.
        tokio::time::timeout(Duration::from_secs(10), conn1.closed())
            .await
            .expect("ZEB-616: alice must close the stale connection on reconnect");

        // The reconnect stays live; registry holds exactly one entry for bob.
        assert!(conn2.close_reason().is_none(), "reconnect must stay open");
        for _ in 0..300 {
            if alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            alice_mgr.zenoh_conns.lock().unwrap().len(),
            1,
            "registry holds exactly the reconnect, no stale duplicate"
        );

        alice_ep.shutdown().await;
        bob_ep1.shutdown().await;
        bob_ep2.shutdown().await;
    }

    /// Component B: when a registered connection closes, its watcher evicts
    /// the registry entry (map stays bounded to live peers).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn zenoh_conn_registry_evicts_on_drop() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(
            Duration::from_secs(45),
            zenoh_conn_registry_evicts_on_drop_inner(),
        )
        .await
        .expect("test must finish within 45s");
    }

    async fn zenoh_conn_registry_evicts_on_drop_inner() {
        let alice_ep = build_hermetic_iroh_endpoint().await;
        let (new_link_tx, _rx) = flume::unbounded::<LinkUnicast>();
        let alice_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&alice_ep),
            ReachabilityResolver::new(),
            new_link_tx,
        ));
        let _accept = alice_mgr.spawn_accept_loop();

        let bob_ep = build_hermetic_iroh_endpoint().await;
        let bob_id = bob_ep.node_id();

        let alice_socket = *alice_ep
            .bound_sockets()
            .first()
            .expect("alice has a bound socket");
        let alice_addr = EndpointAddr::new(alice_ep.node_id()).with_ip_addr(alice_socket);

        let conn = bob_ep
            .inner()
            .connect(alice_addr, alpn::HARMONY_ZENOH_V1)
            .await
            .expect("bob dial alice on zenoh ALPN");
        for _ in 0..300 {
            if alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id),
            "alice must register bob's connection"
        );

        // Bob closes → alice's watcher evicts the entry.
        conn.close(0u32.into(), b"test-drop");
        for _ in 0..300 {
            if !alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id),
            "watcher must evict the registry entry when the connection closes"
        );

        alice_ep.shutdown().await;
        bob_ep.shutdown().await;
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zenoh_reconnect_closes_stale_connection) + test(zenoh_conn_registry_evicts_on_drop) + test(should_evict_on_close_is_identity_guarded)'`

Expected: **FAIL to compile** — `error[E0609]: no field 'zenoh_conns'`, `error[E0599]: no method 'swap_zenoh_conn'`, `cannot find function 'should_evict_on_close'`. (Compile failure is the red state; the behavioral timeout is unreachable until the field exists.)

- [ ] **Step 3: Add the registry field and initializer**

In the `struct IrohZenohLinkManager` definition (after the `tunnel_acceptor` field, `:203`, before the closing `}` at `:204`), add:

```rust
    /// ZEB-616: the live inbound zenoh-ALPN iroh `Connection` per peer,
    /// keyed by the peer's iroh `EndpointId` (== its deterministic zenoh
    /// zid). A same-zid reconnect for a peer already present here closes the
    /// prior connection before the new link is admitted, so the stale zenoh
    /// face is reaped before the reconnect's declarations install — avoiding
    /// the upstream "Remapping unsupported" collision (ZEB-390 gives every
    /// node a stable zid, so a reconnect reuses it). A `std::sync::Mutex` is
    /// correct here: it is only ever held for synchronous map ops, never
    /// across an `.await` (the async close operates on the *returned* prior
    /// connection after the guard is dropped).
    zenoh_conns: std::sync::Mutex<std::collections::HashMap<EndpointId, Connection>>,
```

In `IrohZenohLinkManager::new`'s struct literal (after `tunnel_acceptor: std::sync::OnceLock::new(),`, `:223`), add:

```rust
            zenoh_conns: std::sync::Mutex::new(std::collections::HashMap::new()),
```

- [ ] **Step 4: Add the module const and the two helpers**

Add the const near the other module constants (top of the file, alongside `HANDSHAKE_PENDING_QUEUE_CAP`; grep for `const HANDSHAKE_PENDING_QUEUE_CAP` and place adjacent):

```rust
/// ZEB-616: how long to wait for a stale connection's close to complete
/// before admitting the reconnect anyway. Bounded so a wedged old
/// connection can't stall the accept path; on timeout we proceed and fall
/// back to today's behavior (stale face lingers until the lease reaps it).
const STALE_CONN_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
```

Add the pure guard as a private free fn in the module (e.g. just above `impl IrohZenohLinkManager` at `:206`):

```rust
/// ZEB-616 identity guard for the drop-watcher: only evict a peer's
/// registry entry if the currently-stored connection IS the one whose
/// watcher is firing. `stored` is the registered connection's `stable_id`
/// (None if the peer has no entry); `watcher` is the firing connection's
/// `stable_id`. Prevents a superseded connection's watcher from evicting the
/// live connection that replaced it.
fn should_evict_on_close(stored: Option<usize>, watcher: usize) -> bool {
    stored == Some(watcher)
}
```

Add the swap helper as a method inside `impl IrohZenohLinkManager` (e.g. directly after `new`, before `install_butler_deposit_acceptor` at `:232`):

```rust
    /// ZEB-616: register `conn` as the live zenoh-ALPN connection for
    /// `peer_id`, returning the prior connection (if any) for the caller to
    /// close. Pure synchronous map op — the lock is not held across any
    /// await; the async close + await happens on the returned connection.
    fn swap_zenoh_conn(&self, peer_id: EndpointId, conn: Connection) -> Option<Connection> {
        self.zenoh_conns.lock().unwrap().insert(peer_id, conn)
    }
```

- [ ] **Step 5: Rewrite the zenoh-ALPN branch of the accept loop**

Replace the current zenoh-ALPN block (`:399`–`:421`, from `if alpn_used == alpn::HARMONY_ZENOH_V1 {` through its closing `}` immediately before `} else if alpn_used == alpn::HARMONY_HANDSHAKE_V1`) with:

```rust
                    if alpn_used == alpn::HARMONY_ZENOH_V1 {
                        // ZEB-616: `remote_id()` is available immediately
                        // post-handshake (before the bi stream), so swap this
                        // connection into the per-peer registry FIRST and
                        // close the stale one it replaces. Doing it here — one
                        // beat before the reconnect opens its bi stream and
                        // zenoh re-declares resources — reaps the old face
                        // ahead of the collision window (avoids "Remapping
                        // unsupported"). Reordered above `accept_bi()` from the
                        // pre-ZEB-616 code, which read `remote_id()` after it.
                        let peer_id = conn.remote_id();
                        let conn_id = conn.stable_id();
                        if let Some(old) = mgr.swap_zenoh_conn(peer_id, conn.clone()) {
                            tracing::debug!(
                                peer = %peer_id,
                                "ZEB-616: same-zid reconnect; closing stale zenoh \
                                 connection before admitting new link"
                            );
                            old.close(0u32.into(), b"zeb616-reconnect");
                            // Bounded: guarantee the old iroh conn is gone (→
                            // its zenoh link read-errors → zenoh reaps the
                            // stale face) before admitting the new link's
                            // declarations. Best-effort on timeout.
                            if tokio::time::timeout(STALE_CONN_CLOSE_TIMEOUT, old.closed())
                                .await
                                .is_err()
                            {
                                tracing::debug!(
                                    peer = %peer_id,
                                    "ZEB-616: stale connection close timed out; \
                                     admitting new link anyway"
                                );
                            }
                        }
                        // ZEB-616: evict this peer's registry entry when THIS
                        // connection finally closes, so the map stays bounded
                        // to live peers and a drop-and-never-return peer still
                        // has its face reaped. Identity-guarded so a superseded
                        // connection's watcher can't evict its replacement.
                        {
                            let mgr_for_watch = Arc::clone(&mgr);
                            let conn_for_watch = conn.clone();
                            tokio::spawn(async move {
                                conn_for_watch.closed().await;
                                let mut map = mgr_for_watch.zenoh_conns.lock().unwrap();
                                let stored = map.get(&peer_id).map(|c| c.stable_id());
                                if should_evict_on_close(stored, conn_id) {
                                    map.remove(&peer_id);
                                }
                            });
                        }

                        let (send, recv) = match conn.accept_bi().await {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::warn!("iroh accept_bi failed: {e}");
                                return;
                            }
                        };
                        let src = locator_from_endpoint_id(&mgr.endpoint.node_id());
                        let dst = locator_from_endpoint_id(&peer_id);
                        let link: Arc<dyn LinkUnicastTrait> =
                            Arc::new(IrohZenohLink::new(send, recv, src, dst));
                        // zenoh-link 1.9.0: LinkUnicast wraps NewLink
                        // (Single or MixedReliability). One QUIC bidi stream →
                        // one link → Single.
                        if let Err(e) = mgr
                            .new_link_tx
                            .send_async(LinkUnicast(NewLink::Single(link)))
                            .await
                        {
                            tracing::warn!("zenoh new_link channel closed: {e}");
                        }
                    } else if alpn_used == alpn::HARMONY_HANDSHAKE_V1
```

Note: the final line above (`} else if alpn_used == alpn::HARMONY_HANDSHAKE_V1`) is the *existing* next branch — keep it; only the zenoh block above it changes.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zenoh_reconnect_closes_stale_connection) + test(zenoh_conn_registry_evicts_on_drop) + test(should_evict_on_close_is_identity_guarded)'`

Expected: **PASS** (3 tests). If a reconnect test flakes on timing, first widen the `0..300` poll loops (not the assertion) — never the `conn1.closed()` 10s assertion, which is the actual fix contract.

- [ ] **Step 7: Run fmt + clippy (all-targets)**

Run: `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`

Expected: no diff, zero warnings. Every new item (`zenoh_conns`, `swap_zenoh_conn`, `should_evict_on_close`, `STALE_CONN_CLOSE_TIMEOUT`, `build_hermetic_iroh_endpoint_with_secret`) has a caller, so no dead-code lint.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/zenoh_iroh_transport.rs
git commit -m "feat(zeb-616): close stale zenoh face on same-zid iroh reconnect

Per-peer live-connection registry on IrohZenohLinkManager. On an inbound
zenoh-ALPN reconnect, close the prior same-zid connection and (bounded)
await its closure before admitting the new link, so the stale face is
reaped before the reconnect's declarations install (avoids upstream
zenoh 'Remapping unsupported'). Identity-guarded drop-watcher keeps the
map bounded. Components A + B of the ZEB-616 design.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX"
```

---

### Task 2: Component C — zenoh keepalive/lease config (defense-in-depth)

**Files:**
- Modify: `src-tauri/src/event_loop.rs` — add two `insert_json5` keys to the zenoh `Config` block (`:925`–`:1023`); add a small `#[cfg(test)]` module asserting the key paths remain schema-valid.

**Interfaces:**
- Consumes: nothing from Task 1 (independent).
- Produces: no code interface; a runtime config change plus a schema-validity guard test.

- [ ] **Step 1: Write the failing config-validity test**

Append a dedicated test module at the end of `src-tauri/src/event_loop.rs` (a uniquely-named module avoids colliding with any existing `mod tests`):

```rust
#[cfg(test)]
mod zeb616_lease_config_tests {
    /// ZEB-616 Component C: pin that the keepalive/lease config key paths
    /// remain valid in the zenoh version we build against. `insert_json5`
    /// returns `Err` for an unknown key path, so this fails loudly if a
    /// zenoh upgrade renames the schema (which would otherwise break node
    /// boot at `zenoh::open`).
    #[test]
    fn lease_and_keepalive_keys_are_valid() {
        let mut config = zenoh::Config::default();
        config
            .insert_json5("transport/link/tx/lease", "4000")
            .expect("transport/link/tx/lease must be a valid zenoh config key");
        config
            .insert_json5("transport/link/tx/keep_alive", "4")
            .expect("transport/link/tx/keep_alive must be a valid zenoh config key");
    }
}
```

- [ ] **Step 2: Run the test to verify it passes-as-written but guards the real change**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(lease_and_keepalive_keys_are_valid)'`

Expected: **PASS** (the keys are valid in zenoh 1.9.0). This is a schema-drift guard, not a red→green behavioral test — Component C tunes upstream transport timers with no unit-observable behavior; its real validation is that a full node still boots (the existing boot integration tests below) plus this key-validity pin.

- [ ] **Step 3: Add the config keys to the production config block**

In `src-tauri/src/event_loop.rs`, immediately before the session-open line `let (zenoh_runtime, session) = match cancellable!(open_session_with_runtime(config), "zenoh::open") {` (currently `:1025`), insert:

```rust
    // ZEB-616 Component C: bound how long a silently-dead path's zenoh face
    // can linger when no reconnect arrives to trigger the accept-loop
    // teardown (Component A). In zenoh 1.9.0 `keep_alive` is the number of
    // keep-alive probes per lease (probe interval = lease / keep_alive):
    // lease 4000ms with keep_alive 4 → a probe every 1s, a dead path's face
    // reaped within ~4s (vs the ~10s default lease). Conservative — large
    // enough not to false-positive a briefly-quiet healthy link, small
    // enough to shrink the stale-face window and speed post-partition
    // convergence. keep_alive=4 matches the current default but is set
    // explicitly so the probe cadence is pinned against a future default
    // change.
    if let Err(e) = config.insert_json5("transport/link/tx/lease", "4000") {
        let e = format!("zenoh config error (tx/lease): {e}");
        let _ = ready_tx.send(Err(e));
        return;
    }
    if let Err(e) = config.insert_json5("transport/link/tx/keep_alive", "4") {
        let e = format!("zenoh config error (tx/keep_alive): {e}");
        let _ = ready_tx.send(Err(e));
        return;
    }
```

- [ ] **Step 4: Verify a real node still boots with the new config**

Run the existing zenoh-over-iroh boot integration test (proves the keys don't break `zenoh::open`):

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures --test iroh_zenoh_registration_integration`

Expected: **PASS** (node opens a session with the lease/keepalive config applied). If this test does not itself open a full session, fall back to: `cargo nextest run --locked --features test-fixtures --test channel_backfill_integration` (which boots a node).

- [ ] **Step 5: Run fmt + clippy (all-targets)**

Run: `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`

Expected: no diff, zero warnings.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/event_loop.rs
git commit -m "feat(zeb-616): shorter zenoh link lease + keepalive for faster face reaping

Set transport/link/tx/lease=4000ms and keep_alive=4 so a silently-dead
path's zenoh face is reaped within ~4s (vs the ~10s default) when no
reconnect arrives to trigger the accept-loop teardown. Defense-in-depth
for the drop-and-never-return case; Component C of the ZEB-616 design.
Adds a schema-validity guard on the config key paths.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX"
```

---

### Task 3: Final gates + open PR

**Files:** none (verification + PR).

- [ ] **Step 1: Full-workspace gate sweep**

Run each and confirm green:

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: fmt clean; clippy zero warnings; nextest all pass (0 failed). The two new reconnect tests each take up to ~45s under contention — this is expected.

- [ ] **Step 2: Finish the branch**

Invoke `superpowers:finishing-a-development-branch`. Choose **option 2 (Push and create a Pull Request)**. The PR body must reference:
- Spec: `docs/specs/2026-07-02-zeb-616-stale-face-teardown-design.md`
- Plan: `docs/plans/2026-07-02-zeb-616-stale-face-teardown-plan.md`
- Ticket **ZEB-616** (narrow slice of **ZEB-321** Phase 3); the 84-error ZEB-599 D3 capture as the evidence; ZEB-468 as the in-tree clean-shutdown analogue; ZEB-614 (refuted broader premise, real residue fixed here).
- Summary: per-peer live-connection registry closes the stale same-zid connection on reconnect before admitting the new link (A), identity-guarded drop-watcher keeps the map bounded (B), shorter lease/keepalive caps the drop-and-never-return case (C); 4 new tests.
- Out of scope: full ZEB-321 Phase 3 protocol; outbound re-dial; zid-scheme change; ZEB-615.
- Use `gh pr create --repo zeblithic/harmony-client …` (always pass explicit `--repo`).

After opening, run ONE CodeRabbit pass at PR creation only; do not auto-merge; converge bot findings; leave merge for the human.

---

## Self-Review

**1. Spec coverage:**
- Goal (close old face before reconnect's declarations install) → Task 1 (swap + close + bounded await, reordered above `accept_bi`).
- Component A (registry + `swap_zenoh_conn` + close-old + await) → Task 1 Steps 3–5.
- Component A ordering-risk fallback (zenoh-native per-peer transport close via adopted `Runtime` in a `OnceLock`) → **intentionally not implemented**; the spec names it as a contingency "only if the repro forces it." The `zenoh_reconnect_closes_stale_connection` test is the gate that decides. If it fails to go green via the `Connection::close` path, that is the signal to add the fallback — noted here so the implementer escalates rather than improvising.
- Component B (drop watcher + identity-guarded eviction) → Task 1 (watcher spawn + `should_evict_on_close`), unit-tested (guard) + integration-tested (eviction).
- Component C (lease/keepalive config) → Task 2.
- Testing — local 2-node reconnect repro → Task 1 tests. **Deviation from spec, intentional:** the spec listed `tests/` for the integration test; the plan places it in the in-file `#[cfg(test)] mod tests` of `zenoh_iroh_transport.rs` because that is where the required hermetic harness (`build_hermetic_iroh_endpoint`, private-field access to `zenoh_conns`) already lives. Same coverage, correct location.
- "Assert the fix's contract" honest-caveat fallback → realized directly: the test asserts `conn1.closed()` resolves (contract) rather than scraping the upstream remap log line.
- Edge cases (concurrent reconnects serialized by the mutex; `old.close()`/timeout best-effort; first-contact `None`; eviction race guarded; inbound-only; non-zenoh ALPNs untouched) → all satisfied by the Task 1 code as written (last-writer-wins insert, bounded timeout with debug-log-and-proceed, `if let Some(old)`, `should_evict_on_close`, zenoh-branch-scoped changes).
- Out-of-scope items → untouched; restated in the PR step.

**2. Placeholder scan:** none — every code step shows complete code; every command shows expected output.

**3. Type consistency:** `swap_zenoh_conn(EndpointId, Connection) -> Option<Connection>`, `should_evict_on_close(Option<usize>, usize) -> bool`, `zenoh_conns: Mutex<HashMap<EndpointId, Connection>>`, `STALE_CONN_CLOSE_TIMEOUT: Duration`, `build_hermetic_iroh_endpoint_with_secret(SecretKey) -> Arc<IrohEndpoint>` are used identically in every reference across Tasks 1–3. `conn_id`/`stored` are both `usize` (from `stable_id()`), matching `should_evict_on_close`'s signature. `peer_id`/`bob_id` are `EndpointId` (Copy), used as map keys and in closures without clones.
