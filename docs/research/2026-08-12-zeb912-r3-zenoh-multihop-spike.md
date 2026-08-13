# ZEB-912 R3 spike: does Zenoh route community data over any connected subgraph?

**Date:** 2026-08-12 · **Zenoh:** exactly 1.9.0 (our pin, `src-tauri/Cargo.toml:45`) ·
**Method:** code verification against the pinned crates + an empirical 3–4-session probe
(source in the appendix — every claim below marked *measured* comes from a run, not a read).

## Executive summary

1. **The ticket's premise is dead in zenoh 1.9.0.** "Zenoh peer-mode is
   linkstate-capable and CAN multi-hop through intermediate peers" was true of
   pre-1.0 zenoh (`routing.peer.mode: "linkstate"`). In 1.9.0 that knob — and
   `routing.router.peers_failover_brokering` — are deprecated **no-ops**
   (`zenoh-config-1.9.0/src/lib.rs:472-501`). *Measured:* on a sparse line
   topology A—B—C in peer mode (production config), direct-hop delivery works
   (3.3 ms) and **2-hop pub/sub and queries fail in both directions**.
2. **Router mode delivers exactly what R3 wants, today.** Session
   `mode: "router"` alone selects the router routing hat — the linkstate
   `Network` + spanning-tree machinery (`zenoh-1.9.0/src/net/routing/gateway.rs:156-172`,
   `hat/router/mod.rs:92-178`). *Measured:* same sparse line, all-router:
   A→C pub/sub **622 µs**, C→A **563 µs**, A→C queryable round-trip **5.9 ms**,
   and 3-hop A→B→C→D **786 µs**. Multi-hop works for pub/sub AND queryables
   (so channel events, RBSR sync, card query-on-subscribe, snapshot queryables
   all benefit).
3. **It is all-or-nothing — mixed modes do not broker.** *Measured:* peer-A ↔
   router-B ↔ peer-C exchanges data fine on each direct link (422 µs) but B
   forwards **nothing** between A and C (the deprecated `peers_failover_brokering`
   behavior is gone). Every session that should participate in forwarding must
   run router mode. The saving grace for rollout: mixed = today's status quo
   (no multi-hop), not a regression, so a staged flip degrades to current
   behavior, never below it.
4. **Recommendation:** pursue **router-mode community sessions** as the R3
   mechanism. App-level merge-then-forward (ticket step 2's design) should stay
   on the shelf unless router-mode costs disqualify it at scale — a question
   that belongs to R4's topology work anyway.

## 1. Code-verified premises (all at our exact pins)

- Production sessions run zenoh defaults plus exactly: scouting multicast+gossip
  off (ZEB-809), deterministic `id`, `connect/endpoints` (empty in serve),
  `listen/endpoints` (+`iroh/<hex>`), `transport/link/tx/{lease,keep_alive}`.
  **Nothing under `routing/`, no `mode`** → mode defaults to peer
  (`src-tauri/src/event_loop.rs:1284-1440`).
- The peer hat's `compute_data_route` only inserts *directly-connected* faces
  with their own declared subscriptions — no linkstate graph, no next-hop
  computation (`zenoh-1.9.0/src/net/routing/hat/peer/pubsub.rs:232-320`). The
  linkstate OAM the peer hat ingests feeds gossip *discovery*, not data routing
  (`hat/peer/mod.rs:80-87,363-379`) — and we run gossip off.
- Hat selection is purely by session mode: `(_, WhatAmI::Router) => router::Hat`
  (`gateway.rs:156-172`). Our `open_session_with_runtime`
  (`src-tauri/src/event_loop.rs:13139-13153`) builds `RuntimeBuilder::new(config)`,
  so `config.insert_json5("mode", "\"router\"")` rides the exact production
  open path.
- The vendored iroh zenoh-link fork is mode-agnostic (zero `WhatAmI` references
  in `src-tauri/vendor/zenoh-link/`); links are added at runtime via
  `Runtime::connect_peer` (ZEB-373), not static endpoints, and that API is
  mode-independent.

## 2. The probe (methodology + results)

Three (or four) raw zenoh 1.9.0 sessions in one process, loopback TCP,
scouting/gossip disabled (mirroring production), deliberately sparse links via
explicit `connect/endpoints`; transport sparsity verified via
`session.info().peers_zid()/routers_zid()` before each measurement. Full source
in the appendix; ~40 s to reproduce (`cargo run` in a scratch crate).

| Topology | Modes | Direct hop | 2-hop pub/sub | 2-hop query | 3-hop pub/sub |
|---|---|---|---|---|---|
| A—B—C | all peer (≈ production) | 3.3 ms ✓ | **✗ both directions** | **✗** | — |
| A—B—C | all router | 0.38 ms ✓ | **✓ 622 µs / 563 µs** | **✓ 5.9 ms** | — |
| A—B—C | peer, router, peer | 0.42 ms ✓ | ✗ | ✗ | — |
| A—B—C—D | all router | — | — | — | **✓ 786 µs** |

Forwarding overhead is negligible at this scale: 2-hop delivery lands ~0.2 ms
over the router-mode direct hop.

## 3. What this means at the Harmony layer

### 3.1 Delivery decouples from pairwise reachability — without touching the dial policy

A parallel recon of the dial path (this session) confirmed the full mesh is not
just policy but an emergent invariant: the dial set is *record-driven*
(`reachability_resolver.rs:490-499` — any learned record triggers a dial; no
membership/topology input), and at least six independent healing mechanisms
rebuild density (address-book row gossip `address_book_sync.rs:232,605,655`;
boot seeding `iroh_zenoh_registration.rs:135-145`; presence-roster sweeps
`community_presence.rs:536-552`; ZEB-910 Dormant parole
`reconnect_supervisor.rs:866-935`; pkarr stale-refresh
`reachability_resolver.rs:658-753`; gateway split-repair
`community_gateway_dial_driver.rs:318-351`). **R3 does not need to fight any of
that.** Router mode makes delivery survive whatever pairs happen to be
unreachable (NAT failure, relay mismatch, mid-heal windows): the mesh stays as
dense as it can get, and linkstate routes *around the holes*. Islands degrade to
a latency problem exactly as the ticket hoped — provided the island is connected
through any chain of members.

### 3.2 Intermediary forwarding is safe by construction

Community payloads are end-to-end protected independent of transport peers
(epoch-encrypted channel packets, sealed/signed address-book rows and cards), so
a member forwarding ciphertext it cannot read changes no trust boundary. The new
exposure is *metadata at intermediaries* (key expressions, sizes, timing) —
worth one deliberate paragraph in the step-2 design, not a blocker: intermediaries
are community members already entitled to subscribe to these topics.

### 3.3 Known seams a production flip must handle (found now, cheap later)

1. `iroh_zenoh_registration.rs:162-192` appends the iroh listen locator under
   the **`"peer"`** key of zenoh's per-mode endpoint map ("we always run peer
   mode"). Under a router-mode session those endpoints are silently ignored —
   append under the session's actual mode (or normalize to the plain-array form).
2. `timestamping.enabled` default flips **false→true** in router mode
   (`zenoh-config-1.9.0/src/defaults.rs:139-147`): every data message gets an
   HLC stamp if absent — wire-visible. Pin it explicitly (either value) so the
   flip is a decision, not a side effect.
3. `connect/listen` timeout/retry defaults are `ModeDependentValue`s; ours are
   explicit or empty, but enumerate once during implementation.
4. Session-info consumers: anything reading `peers_zid()` should also read
   `routers_zid()` (they partition by the *remote* node's mode — observed in the
   probe: an all-router mesh reports links under `routers_zid`).

### 3.4 Validation path exists and is cheap

The e2e harness already does 3-node community formation
(`e2e-harness/tests/e2e_two_node.rs:2189-2309`,
`s9_three_member_channel_convergence`, ~2 min wall). A harmony-layer validation
needs: an env-gated mode knob (e.g. `HARMONY_ZENOH_MODE=router`) + seam fix (1)
+ an s9-style scenario asserting delivery with a severed pair. Severing a pair
needs a test-only dial filter (no such seam exists today — cleanest is an
injected filtering `PeerDialer` at `event_loop.rs:1537` plus symmetric inbound
suppression), which is real but small PR-sized work.

## 4. Recommended step 2 (supersedes the ticket's step 2)

1. **Env-gated router-mode knob + seam fixes** (small PR): `HARMONY_ZENOH_MODE`,
   endpoint-key fix, explicit `timestamping.enabled`, `routers_zid` coverage.
2. **Harmony-layer proof** (e2e): 3 nodes, router mode, dial-filter seam severing
   A—C, assert channel message + RBSR backfill + card resolution all cross B.
3. **Scale sounding before default-flip** (feeds R4): linkstate flood + tree
   recompute cost at 10–50 sessions with membership churn; decide flip-by-default
   vs per-community opt-in.
4. App-level merge-then-forward: **not pursued** unless (3) disqualifies router
   mode.

## Appendix: probe source

```toml
# Cargo.toml
[package]
name = "zenoh-multihop-probe"
version = "0.1.0"
edition = "2021"

[dependencies]
zenoh = "=1.9.0"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "time"] }
```

```rust
//! ZEB-912 R3 spike: does zenoh 1.9.0 route DATA multi-hop between sessions
//! that are not directly connected? Line topologies over loopback TCP,
//! scouting (multicast + gossip) disabled, mirroring production (ZEB-809).

use std::time::{Duration, Instant};

fn cfg(mode: &str, listen: Option<u16>, connect: &[u16]) -> zenoh::Config {
    let mut c = zenoh::Config::default();
    c.insert_json5("mode", &format!("\"{mode}\"")).expect("mode");
    c.insert_json5("scouting/multicast/enabled", "false").expect("multicast off");
    c.insert_json5("scouting/gossip/enabled", "false").expect("gossip off");
    let listen_arr = match listen {
        Some(p) => format!("[\"tcp/127.0.0.1:{p}\"]"),
        None => "[]".to_string(),
    };
    c.insert_json5("listen/endpoints", &listen_arr).expect("listen");
    let connect_arr = connect
        .iter()
        .map(|p| format!("\"tcp/127.0.0.1:{p}\""))
        .collect::<Vec<_>>()
        .join(",");
    c.insert_json5("connect/endpoints", &format!("[{connect_arr}]")).expect("connect");
    c
}

async fn peers(s: &zenoh::Session) -> Vec<String> {
    let mut v: Vec<String> = s.info().peers_zid().await.map(|z| z.to_string()).collect();
    let mut r: Vec<String> = s.info().routers_zid().await.map(|z| z.to_string()).collect();
    v.append(&mut r);
    v.sort();
    v
}

async fn probe(mode: &str, base: u16) {
    println!("=== mode={mode} (ports {base}/{}) ===", base + 1);
    let a = zenoh::open(cfg(mode, Some(base), &[])).await.expect("open A");
    let b = zenoh::open(cfg(mode, Some(base + 1), &[base])).await.expect("open B");
    let c = zenoh::open(cfg(mode, None, &[base + 1])).await.expect("open C");
    tokio::time::sleep(Duration::from_millis(1500)).await;

    println!("A zid={} links={:?}", a.zid(), peers(&a).await);
    println!("B zid={} links={:?}", b.zid(), peers(&b).await);
    println!("C zid={} links={:?}", c.zid(), peers(&c).await);

    let sub_b = b.declare_subscriber("probe/base").await.expect("sub B");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let t0 = Instant::now();
    a.put("probe/base", "x").await.expect("put base");
    match tokio::time::timeout(Duration::from_secs(3), sub_b.recv_async()).await {
        Ok(Ok(_)) => println!("[{mode}] baseline A->B (direct): DELIVERED in {:?}", t0.elapsed()),
        _ => println!("[{mode}] baseline A->B (direct): NOT delivered in 3s"),
    }

    let sub_c = c.declare_subscriber("probe/ac").await.expect("sub C");
    tokio::time::sleep(Duration::from_millis(800)).await;
    let t0 = Instant::now();
    a.put("probe/ac", "hello-from-a").await.expect("put ac");
    match tokio::time::timeout(Duration::from_secs(5), sub_c.recv_async()).await {
        Ok(Ok(s)) => println!(
            "[{mode}] A->C (2-hop): DELIVERED in {:?} (key={})",
            t0.elapsed(),
            s.key_expr()
        ),
        _ => println!("[{mode}] A->C (2-hop): NOT delivered in 5s"),
    }

    let sub_a = a.declare_subscriber("probe/ca").await.expect("sub A");
    tokio::time::sleep(Duration::from_millis(800)).await;
    let t0 = Instant::now();
    c.put("probe/ca", "hello-from-c").await.expect("put ca");
    match tokio::time::timeout(Duration::from_secs(5), sub_a.recv_async()).await {
        Ok(Ok(_)) => println!("[{mode}] C->A (2-hop): DELIVERED in {:?}", t0.elapsed()),
        _ => println!("[{mode}] C->A (2-hop): NOT delivered in 5s"),
    }

    let q = c
        .declare_queryable("probe/q")
        .callback(|query| {
            let q = query.clone();
            tokio::spawn(async move {
                let _ = q.reply("probe/q", "answer-from-c").await;
            });
        })
        .await
        .expect("queryable C");
    tokio::time::sleep(Duration::from_millis(800)).await;
    let t0 = Instant::now();
    let replies = a.get("probe/q").await.expect("get");
    match tokio::time::timeout(Duration::from_secs(5), replies.recv_async()).await {
        Ok(Ok(_)) => println!("[{mode}] A get-> C (2-hop query): REPLIED in {:?}", t0.elapsed()),
        _ => println!("[{mode}] A get-> C (2-hop query): NO reply in 5s"),
    }
    drop(q);

    c.close().await.ok();
    a.close().await.ok();
    b.close().await.ok();
    println!();
}

async fn probe_mixed(base: u16) {
    println!("=== mixed: A=peer, B=router, C=peer (ports {base}/{}) ===", base + 1);
    let a = zenoh::open(cfg("peer", Some(base), &[])).await.expect("open A");
    let b = zenoh::open(cfg("router", Some(base + 1), &[base])).await.expect("open B");
    let c = zenoh::open(cfg("peer", None, &[base + 1])).await.expect("open C");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    println!("A links={:?}", peers(&a).await);
    println!("C links={:?}", peers(&c).await);

    let sub_b = b.declare_subscriber("mixed/base").await.expect("sub B");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let t0 = Instant::now();
    a.put("mixed/base", "x").await.expect("put base");
    match tokio::time::timeout(Duration::from_secs(3), sub_b.recv_async()).await {
        Ok(Ok(_)) => println!("[mixed] baseline peer-A -> router-B (direct): DELIVERED in {:?}", t0.elapsed()),
        _ => println!("[mixed] baseline peer-A -> router-B (direct): NOT delivered in 3s"),
    }

    let sub_c = c.declare_subscriber("mixed/ac").await.expect("sub C");
    tokio::time::sleep(Duration::from_millis(800)).await;
    let t0 = Instant::now();
    a.put("mixed/ac", "x").await.expect("put");
    match tokio::time::timeout(Duration::from_secs(5), sub_c.recv_async()).await {
        Ok(Ok(_)) => println!("[mixed] A->C via router B: DELIVERED in {:?}", t0.elapsed()),
        _ => println!("[mixed] A->C via router B: NOT delivered in 5s"),
    }

    let q = c
        .declare_queryable("mixed/q")
        .callback(|query| {
            let q = query.clone();
            tokio::spawn(async move {
                let _ = q.reply("mixed/q", "ans").await;
            });
        })
        .await
        .expect("queryable C");
    tokio::time::sleep(Duration::from_millis(800)).await;
    let t0 = Instant::now();
    let replies = a.get("mixed/q").await.expect("get");
    match tokio::time::timeout(Duration::from_secs(5), replies.recv_async()).await {
        Ok(Ok(_)) => println!("[mixed] A get->C via router B: REPLIED in {:?}", t0.elapsed()),
        _ => println!("[mixed] A get->C via router B: NO reply in 5s"),
    }
    drop(q);
    c.close().await.ok();
    a.close().await.ok();
    b.close().await.ok();
    println!();
}

async fn probe_three_hop(base: u16) {
    println!("=== 3-hop: A-B-C-D all routers (ports {base}..{}) ===", base + 2);
    let a = zenoh::open(cfg("router", Some(base), &[])).await.expect("open A");
    let b = zenoh::open(cfg("router", Some(base + 1), &[base])).await.expect("open B");
    let c = zenoh::open(cfg("router", Some(base + 2), &[base + 1])).await.expect("open C");
    let d = zenoh::open(cfg("router", None, &[base + 2])).await.expect("open D");
    tokio::time::sleep(Duration::from_millis(2000)).await;
    println!("A links={:?}", peers(&a).await);
    println!("D links={:?}", peers(&d).await);

    let sub_d = d.declare_subscriber("hop3/ad").await.expect("sub D");
    tokio::time::sleep(Duration::from_millis(800)).await;
    let t0 = Instant::now();
    a.put("hop3/ad", "x").await.expect("put");
    match tokio::time::timeout(Duration::from_secs(5), sub_d.recv_async()).await {
        Ok(Ok(_)) => println!("[3hop] A->D (3 hops): DELIVERED in {:?}", t0.elapsed()),
        _ => println!("[3hop] A->D (3 hops): NOT delivered in 5s"),
    }
    d.close().await.ok();
    c.close().await.ok();
    a.close().await.ok();
    b.close().await.ok();
    println!();
}

#[tokio::main]
async fn main() {
    probe("peer", 7511).await;
    probe("router", 7521).await;
    probe_mixed(7531).await;
    probe_three_hop(7541).await;
}
```

Raw output of the recorded run (Koya, 2026-08-12):

```
=== mode=peer (ports 7511/7512) ===
A zid=4f5893b0c1b963357f731698accfcc24 links=["77b429699239c1dfc77d0394ab5f348c"]
B zid=77b429699239c1dfc77d0394ab5f348c links=["4f5893b0c1b963357f731698accfcc24", "78f9a0269a5bdaba528d0514933e937b"]
C zid=78f9a0269a5bdaba528d0514933e937b links=["77b429699239c1dfc77d0394ab5f348c"]
[peer] baseline A->B (direct): DELIVERED in 3.321417ms
[peer] A->C (2-hop): NOT delivered in 5s
[peer] C->A (2-hop): NOT delivered in 5s
[peer] A get-> C (2-hop query): NO reply in 5s

=== mode=router (ports 7521/7522) ===
[router] baseline A->B (direct): DELIVERED in 382.75µs
[router] A->C (2-hop): DELIVERED in 622.166µs (key=probe/ac)
[router] C->A (2-hop): DELIVERED in 563.375µs
[router] A get-> C (2-hop query): REPLIED in 5.868291ms

=== mixed: A=peer, B=router, C=peer (ports 7531/7532) ===
[mixed] baseline peer-A -> router-B (direct): DELIVERED in 422.625µs
[mixed] A->C via router B: NOT delivered in 5s
[mixed] A get->C via router B: NO reply in 5s

=== 3-hop: A-B-C-D all routers (ports 7541..7543) ===
[3hop] A->D (3 hops): DELIVERED in 785.75µs
```
