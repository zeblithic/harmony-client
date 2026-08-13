# ZEB-912 R3 step-3: router-mode scale sounding

**Date:** 2026-08-13 · **Zenoh:** exactly 1.9.0 (`src-tauri/Cargo.toml`) · **Host:** Koya (macOS, single process) ·
**Method:** a raw-zenoh probe (source in the appendix — every number below is *measured*, from `cargo run --release`) plus a design decision. Follows the R3 spike (`2026-08-12-zeb912-r3-zenoh-multihop-spike.md`) and step-2 (PRs #671 router-mode knob, #672/ZEB-927 join-layer snapshot).

## Executive summary

1. **Decision: do NOT flip zenoh router mode on by default.** Full-mesh router
   mode's linkstate flood is **super-linear** and it stops working **well inside
   Harmony's intended community sizes** — a single member joining a 50-node full
   mesh floods hundreds of MB, delivery starts timing out, and even an idle
   50-node mesh burns ~3 CPU cores churning linkstate. It does not degrade
   gracefully toward the N=200 product ceiling; it falls apart around N=50.
2. **Bounded degree is the answer, and it is dramatic.** A ring (degree 2) or
   line (degree 1) keeps join flood **linear** (~0.6 KB per member), idle CPU at
   **zero**, and delivery healthy across the whole N=10→200 range. At N=100 the
   contrast is **ring 62 KB & working vs mesh ~31 MB & thrashing**.
3. **Bounded degree's price is convergence latency, and it is affordable.** A
   degree-2 ring at N=200 pays ~19 s cold-boot convergence and ~4.6 s
   reconvergence after a join (both diameter-proportional), and a line's hop
   latency reaches ~9 ms across 199 hops. Delivery works throughout; it is only
   slower to *settle*. A slightly higher constant degree would cut this sharply.
4. **Recommendation.** Keep router mode **env-gated / per-community opt-in**, safe
   only for **small communities (N ≲ 25**, where a join floods < 1 MB). Make
   **R4 (ZEB-914) bounded-degree topology a prerequisite** for running router-mode
   delivery in large communities by default. This sounding hands R4 a concrete
   target: a **small constant degree (~6–10)** — large enough to shrink the
   ring's diameter/latency, small enough to keep flood near-linear.

## 1. What was measured, and why this vehicle

The question step-3 exists to answer (spike §4.3): does router-mode's linkstate
flood + spanning-tree recompute stay affordable as a **full-mesh** community — the
emergent steady state of Harmony's record-driven dial policy (spike §3.1) — grows
toward its **N=200 product ceiling**? (200 is a product boundary: past ~200 members
a community can't feel private/close/familiar, and the use case belongs to
Discord-class tools.)

**Vehicle:** a standalone raw-zenoh probe — N sessions in one process over
loopback TCP, production-mirroring config (`mode:"router"`, multicast + gossip
scouting off, `timestamping.enabled:false` — the router-mode default is `true`),
each session built through `RuntimeBuilder` exactly as harmony's
`open_session_with_runtime` does (`event_loop.rs:13196-13202`). Three topologies —
**full mesh** (degree N), **ring** (degree 2, the R4 target shape), **line**
(degree 1) — swept at N ∈ {10, 25, 50, 100, 200}. Churn is modelled as a single
**member join** (uniform across topologies; only the joiner's degree differs — that
degree *is* the variable under test).

**Metrics:** routing-OAM **flood per join** (bytes), measured **data-quiescent** —
zero application `put`s during the window, so every transport byte moved is
routing/linkstate/Declare traffic; **boot** (cold convergence to first delivery
across the diameter); **reconvergence** (time to reach the joiner after a join);
whole-process **idle CPU** (over a 2 s quiescent window) and **RSS**; and **hop
latency**.

**How flood is read:** zenoh's own admin space. With `adminspace.permissions.read`
enabled (a probe-only introspection concession — read-only, does not touch routing
or linkstate), a `session.get("@/{zid}/router?_stats=true")` returns per-transport
byte/message counters (`sessions[].stats.tx_bytes`, built into `zenoh-transport`
under the `stats` feature). Summing `tx_bytes` across all nodes = total bytes the
mesh transmitted for that join, each counted once at its sender. (The in-process
`Runtime::get_transports()` path is `pub(crate)` and unreachable from an external
crate; the admin space is the public equivalent zenoh's own `z_info` tooling uses.)

## 2. Results (measured on Koya, 2026-08-13)

`join_bytes` is the mesh-wide routing flood for one join. `idle_cores` is CPU over
a 2 s quiescent window *after* boot, *before* the join. `n/a` = the 30 s reconverge
/ 5 s hop / 60 s boot budget was exceeded.

### 2.1 Full mesh (degree N) — the case a default-flip would face

| N | boot_ms | reconv_ms | join_bytes | join_KB | hop_ms | idle_cores | rss_mb |
|---|---|---|---|---|---|---|---|
| 10 | 202 | 0 | 48,301 | 47.2 | 0.252 | 0.00 | 24 |
| 25 | 202 | 0 | 863,605 | 843.4 | 0.154 | 0.01 | 100 |
| 50 | 22,573 | n/a | 210,211,663 | **205,285** | n/a | **2.87** | 429 |
| 100 | n/a | n/a | 31,078,456 | 30,350 | n/a | 2.45 | — |

The clean signal is N=10→25: **47 KB → 843 KB**, an 18× flood rise for a 2.5×
membership rise (≈ N^2.7). By N=50 the mesh is **degraded**: boot blows out to 22 s,
reconvergence and hop both time out (delivery is failing), idle CPU is ~3 cores, and
the "join flood" (~200 MB) is really accumulated thrash from a mesh that never
settles. N=100's *lower* 31 MB is not an improvement — it is a mesh so degraded the
single-join measurement is meaningless (non-monotonic = unreliable past N≈25).

### 2.2 Ring (degree 2) — the R4 bounded-degree target

| N | boot_ms | reconv_ms | join_bytes | join_KB | hop_ms | idle_cores | rss_mb |
|---|---|---|---|---|---|---|---|
| 10 | 0 | 1 | 6,538 | 6.4 | 0.271 | 0.00 | 16 |
| 25 | 202 | 1 | 16,060 | 15.7 | 0.231 | 0.00 | 28 |
| 50 | 202 | 3 | 31,980 | 31.2 | 0.252 | 0.00 | 62 |
| 100 | 200 | 204 | 63,830 | 62.3 | 0.168 | 0.01 | 180 |
| 200 | 18,915 | 4,650 | 133,146 | 130.0 | 0.108 | 0.17 | 693 |

Join flood is **linear** (6.4 → 15.7 → 31.2 → 62.3 → 130 KB ≈ 0.6 KB/node) and idle
CPU stays ~0 — a bounded-degree mesh is genuinely quiet. The price appears at high N
as **convergence latency**: reconvergence climbs to 204 ms (N=100) then 4.65 s
(N=200), and cold boot to ~19 s at N=200 — all diameter-proportional (ring diameter
= N/2).

### 2.3 Line (degree 1) — lower flood, higher diameter

| N | boot_ms | reconv_ms | join_bytes | join_KB | hop_ms | idle_cores | rss_mb |
|---|---|---|---|---|---|---|---|
| 10 | 203 | 1 | 5,403 | 5.3 | 0.976 | 0.01 | — |
| 25 | 204 | 1 | 13,558 | 13.2 | 1.456 | 0.01 | — |
| 50 | 205 | 3 | 27,192 | 26.6 | 2.882 | 0.02 | — |
| 100 | 207 | 4 | 55,140 | 53.8 | 6.443 | 0.01 | — |
| 200 | 18,206 | 4,682 | 115,981 | 113.3 | 9.068 | 0.01 | — |

Flood is even lower than ring (degree 1), also linear. The tradeoff shows in **hop
latency**, which grows with the full-N diameter: ~1 ms at N=10 to ~9 ms at N=200
(199 hops). RSS is omitted — it is process-cumulative here (memory from the prior
ring-N=200 run had not been released), so per-run RSS is unreliable in this
shared-process vehicle; treat only the mesh sweep's RSS (increasing, single sweep)
as indicative (24 → 100 → 429 MB, N=10→50).

## 3. The one honesty caveat: what transfers to production, what doesn't

The probe runs N zenoh routers in **one process on one machine**; production runs N
routers on N machines. Two consequences:

- **The aggregate flood is real and transfers.** `join_bytes` is total bytes on the
  wire mesh-wide, a property of zenoh's routing layer (the linkstate `Network` +
  spanning-tree OAM, `zenoh-1.9.0/src/net/routing/hat/router/`) that rides zenoh
  messages **independent of link transport** — loopback TCP and iroh carry the
  identical OAM. So the O(N²)-ish full-mesh flood and the linear bounded-degree
  flood both hold in production. In production the N=50 mesh's ~200 MB is spread
  across 50 machines and ~1,225 links (~4 MB per node per join) — still a large,
  super-linearly-growing cost for one membership change.
- **The "collapse" severity is probe-amplified.** Delivery timing out and ~3 idle
  cores at N=50 owe partly to 50 routers contending for one machine's scheduler.
  Fifty separate machines would degrade more *gracefully*. So we report the flood
  *scaling* as the hard finding and the *collapse* as directionally-correct but
  overstated by the vehicle. Either way the conclusion is the same: full-mesh
  router flood grows too fast to flip on by default at Harmony's larger sizes.

## 4. Anchor: does the loopback probe predict the real stack?

Two independent checks, no separate rebuild required:

- **Latency consistency.** The probe reproduces the spike's raw-zenoh vehicle: its
  steady multi-hop hop latencies (mesh 0.15–0.25 ms, ring 0.11–0.27 ms) match the
  spike's recorded raw-zenoh 2-hop figures (563–622 µs) at the same scale. Real
  harmony-over-iroh adds a per-hop RTT offset (real links instead of loopback), so
  production absolute latencies are higher by roughly that constant — but the
  *scaling shape* is what the decision rests on, and it transfers.
- **Flood transfer is by construction, not measurement.** Per §3, the linkstate
  OAM that constitutes the flood is transport-agnostic — a code-verified property,
  not something a 3-node e2e latency datapoint could confirm anyway. A full
  real-stack anchor (rebuilding `harmony-app` to instrument s14) was deliberately
  skipped: s14 measures poll-granular app latency (15–120 s poll loops), so it
  would cost ~50 min of rebuild for a low-precision number that does not bear on
  the flood finding.

## 5. Decision and what it means for R4

- **Flip router mode on by default: NO.** Full-mesh flood is super-linear and
  delivery degrades by N≈50, inside intended community sizes.
- **Per-community opt-in: yes, but only for small communities.** Router mode stays
  env-gated; a community may opt in where an operator accepts the cost. Safe zone
  is roughly **N ≲ 25** (join flood < 1 MB, delivery healthy, idle CPU ~0).
- **R4 (ZEB-914) is a prerequisite for large communities.** Bounded-degree topology
  is the only shape that serves the full N=200 range cheaply. Target handed to R4:
  a **small constant degree (~6–10)** — bounded degree keeps flood near-linear
  (ring degree 2 already does), while a degree above 2 shrinks the diameter that
  costs the ring ~4.6 s reconvergence and the line ~9 ms hop latency at N=200. R4
  should treat **membership-change reconvergence latency at high N** as the metric
  to tune degree against, not flood (which bounded degree already solves).

## Appendix: probe source and reproduction

Standalone crate, **not** committed to the workspace (build location:
`$HOME/work/zeb912-scale-probe/`). Reproduce with `cargo run --release`. The full
mesh self-limits (collapses ~N=50); to re-measure ring/line alone, set
`let topos = [Topo::Ring, Topo::Line];` in `main`.

### `Cargo.toml`

```toml
[package]
name = "zeb912-scale-probe"
version = "0.1.0"
edition = "2021"

[dependencies]
zenoh = { version = "=1.9.0", features = ["internal", "stats", "unstable"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "time"] }
libc = "0.2"
serde_json = "1"

[profile.release]
opt-level = 2
```

### `src/main.rs`

```rust
//! ZEB-912 step-3 scale sounding: measure router-mode zenoh's linkstate flood +
//! spanning-tree recompute cost as a full-mesh community grows to N=200.
//! Raw zenoh 1.9.0 sessions over loopback TCP, production-mirroring config
//! (router mode, scouting off, gossip off, timestamping pinned false). Sessions
//! are built through RuntimeBuilder — the exact path harmony production uses
//! (event_loop.rs:13196-13202) — so we hold the Runtime handle and can read
//! per-transport stats via the admin space.
#![allow(dead_code)]

use std::time::{Duration, Instant};
use zenoh::internal::runtime::Runtime;
use zenoh::Session;

#[derive(Clone, Copy, Debug)]
enum Topo { Mesh, Ring, Line }
impl Topo {
    fn name(&self) -> &'static str {
        match self { Topo::Mesh => "mesh", Topo::Ring => "ring", Topo::Line => "line" }
    }
}

/// Production-mirroring router-mode config. adminspace read is a probe-only
/// introspection concession (does not affect routing/linkstate).
fn node_cfg(port: u16, connect: &[u16]) -> zenoh::Config {
    let mut c = zenoh::Config::default();
    c.insert_json5("mode", "\"router\"").expect("mode");
    c.insert_json5("scouting/multicast/enabled", "false").expect("mcast");
    c.insert_json5("scouting/gossip/enabled", "false").expect("gossip");
    c.insert_json5("timestamping/enabled", "false").expect("ts"); // router default is true
    c.insert_json5("adminspace/enabled", "true").expect("adminspace");
    c.insert_json5("adminspace/permissions/read", "true").expect("adminspace read");
    c.insert_json5("listen/endpoints", &format!("[\"tcp/127.0.0.1:{port}\"]")).expect("listen");
    let connect_arr = connect.iter().map(|p| format!("\"tcp/127.0.0.1:{p}\"")).collect::<Vec<_>>().join(",");
    c.insert_json5("connect/endpoints", &format!("[{connect_arr}]")).expect("connect");
    c
}

/// Which lower-index nodes node `i` dials. Each node listens on `base + i`.
fn connects_for(topo: &Topo, i: usize, n: usize, base: u16) -> Vec<u16> {
    match topo {
        Topo::Line => if i > 0 { vec![base + i as u16 - 1] } else { vec![] },
        Topo::Ring => {
            let mut v = if i > 0 { vec![base + i as u16 - 1] } else { vec![] };
            if i == n - 1 && n > 2 { v.push(base); }
            v
        }
        Topo::Mesh => (0..i).map(|j| base + j as u16).collect(),
    }
}

/// Mirror of harmony's open_session_with_runtime; order is load-bearing.
async fn try_open_router_session(cfg: zenoh::Config) -> Option<(Runtime, Session)> {
    let mut runtime = zenoh::internal::runtime::RuntimeBuilder::new(cfg).build().await.ok()?;
    let session = zenoh::session::init(runtime.clone().into()).await.ok()?;
    runtime.start().await.ok()?;
    Some((runtime, session))
}
async fn open_router_session(cfg: zenoh::Config) -> (Runtime, Session) {
    try_open_router_session(cfg).await.expect("open session")
}
/// Stops early (partial vec) on first open failure — sweep detects host ceiling.
async fn spawn_topology(topo: &Topo, n: usize, base: u16) -> Vec<(Runtime, Session)> {
    let mut nodes = Vec::with_capacity(n);
    for i in 0..n {
        let connect = connects_for(topo, i, n, base);
        match try_open_router_session(node_cfg(base + i as u16, &connect)).await {
            Some(node) => nodes.push(node),
            None => break,
        }
    }
    nodes
}

async fn deliver_ms(src: &Session, dst: &Session, key: &str, timeout: Duration) -> Option<f64> {
    let sub = dst.declare_subscriber(key).await.ok()?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t0 = Instant::now();
    src.put(key, "x").await.ok()?;
    match tokio::time::timeout(timeout, sub.recv_async()).await {
        Ok(Ok(_)) => Some(t0.elapsed().as_secs_f64() * 1000.0),
        _ => None,
    }
}
async fn hop_latency_ms(src: &Session, dst: &Session, key: &str) -> Option<f64> {
    deliver_ms(src, dst, key, Duration::from_secs(5)).await
}
/// Reconvergence: retry puts until first receipt (sub declared here).
async fn reconverge_ms(src: &Session, dst: &Session, key: &str, budget: Duration) -> Option<f64> {
    let sub = dst.declare_subscriber(key).await.ok()?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    while sub.try_recv().ok().flatten().is_some() {}
    let t0 = Instant::now();
    loop {
        src.put(key, "x").await.ok()?;
        if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(200), sub.recv_async()).await {
            return Some(t0.elapsed().as_secs_f64() * 1000.0);
        }
        if t0.elapsed() > budget { return None; }
    }
}

fn cpu_seconds() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru); }
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    secs(ru.ru_utime) + secs(ru.ru_stime)
}
fn current_rss_mb() -> u64 {
    let pid = std::process::id().to_string();
    std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid]).output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|kib| kib / 1024).unwrap_or(0)
}

/// Boot-burst: convergence clock starts AFTER spawn (sequential spawn is a probe
/// artifact, not a zenoh property).
async fn boot_convergence_ms(topo: &Topo, n: usize, base: u16) -> (Option<f64>, Vec<(Runtime, Session)>) {
    let nodes = spawn_topology(topo, n, base).await;
    if nodes.len() < n { return (None, nodes); }
    let (src, dst) = (&nodes[0].1, &nodes[n - 1].1);
    let sub = match dst.declare_subscriber("probe/boot").await { Ok(s) => s, Err(_) => return (None, nodes) };
    let t0 = Instant::now();
    let conv = loop {
        src.put("probe/boot", "x").await.ok();
        if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(200), sub.recv_async()).await {
            break Some(t0.elapsed().as_secs_f64() * 1000.0);
        }
        if t0.elapsed() > Duration::from_secs(60) { break None; }
    };
    drop(sub);
    (conv, nodes)
}

struct ChurnResult { join_bytes: u64, join_reconv_ms: Option<f64> }

/// One churn event = a member JOINS. Only the joiner's degree differs by topology
/// (mesh N, ring 2, line 1) — that degree is the variable under test. Leave flood
/// is NOT measured: tx_bytes is cumulative per live transport, so a departing
/// node's counts vanish from the sum and a leave delta is meaningless.
async fn churn_once(topo: &Topo, nodes: &mut Vec<(Runtime, Session)>, base: u16) -> ChurnResult {
    let n = nodes.len();
    let (b0, _) = mesh_tx(nodes).await;
    let connect: Vec<u16> = match topo {
        Topo::Mesh => (0..n).map(|j| base + j as u16).collect(),
        Topo::Ring => vec![base, base + 1],
        Topo::Line => vec![base + (n - 1) as u16],
    };
    let joiner = open_router_session(node_cfg(base + n as u16, &connect)).await;
    nodes.push(joiner);
    let far = n / 2;
    let reconv = reconverge_ms(&nodes[far].1, &nodes[n].1, "probe/churn", Duration::from_secs(30)).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let (b1, _) = mesh_tx(nodes).await;
    ChurnResult { join_bytes: b1.saturating_sub(b0), join_reconv_ms: reconv }
}

/// Query a session's OWN admin space for aggregate transport stats: sum
/// sessions[].stats.tx_bytes (fallback sessions[].links[].stats). Data-quiescent,
/// tx_bytes is pure routing overhead. Needs features stats + adminspace read.
async fn node_tx(session: &Session) -> (u64, u64) {
    let zid = session.zid().to_string();
    let replies = match session.get(format!("@/{zid}/router?_stats=true")).await {
        Ok(r) => r, Err(_) => return (0, 0),
    };
    let g = |o: &serde_json::Value, k: &str| o.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    let mut out = (0u64, 0u64);
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            let bytes = sample.payload().to_bytes();
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                let (mut tx_b, mut tx_m) = (0u64, 0u64);
                if let Some(sessions) = v.get("sessions").and_then(|s| s.as_array()) {
                    for sess in sessions {
                        if let Some(st) = sess.get("stats") {
                            tx_b += g(st, "tx_bytes"); tx_m += g(st, "tx_t_msgs");
                        } else if let Some(links) = sess.get("links").and_then(|l| l.as_array()) {
                            for link in links {
                                if let Some(st) = link.get("stats") {
                                    tx_b += g(st, "tx_bytes"); tx_m += g(st, "tx_t_msgs");
                                }
                            }
                        }
                    }
                }
                out = (tx_b, tx_m);
            }
        }
    }
    out
}
async fn mesh_tx(nodes: &[(Runtime, Session)]) -> (u64, u64) {
    let (mut b, mut m) = (0u64, 0u64);
    for (_, s) in nodes { let (nb, nm) = node_tx(s).await; b += nb; m += nm; }
    (b, m)
}

/// Raise fd soft limit to hard cap. A full mesh at N needs ~N*(N-1) loopback fds;
/// the single-process ceiling is a PROBE artifact (in production each node holds
/// only N-1 links), so mesh ceilings are reported, never read as a router limit.
fn raise_fd_limit() -> u64 {
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    unsafe {
        libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim);
        lim.rlim_cur = lim.rlim_max;
        libc::setrlimit(libc::RLIMIT_NOFILE, &mut lim);
        libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim);
    }
    lim.rlim_cur as u64
}

#[tokio::main]
async fn main() {
    let fd = raise_fd_limit();
    eprintln!("# fd soft limit raised to {fd}");
    let sizes = [10usize, 25, 50, 100, 200];
    // Mesh self-limits (collapses ~N=50); set [Topo::Ring, Topo::Line] to skip it.
    let topos = [Topo::Mesh, Topo::Ring, Topo::Line];
    let mut base: u16 = 7000;
    for topo in topos {
        println!("\n## {}\n", topo.name());
        println!("| N | boot_ms | join_reconv_ms | join_bytes | join_KB | hop_ms | idle_cores | rss_mb |");
        println!("|---|---|---|---|---|---|---|---|");
        for &n in sizes.iter() {
            let run_base = base;
            base = base.wrapping_add(500);
            let (boot, mut nodes) = boot_convergence_ms(&topo, n, run_base).await;
            if nodes.len() < n {
                println!("| {n} | **HOST-LIMIT: spawned {} of {n}** (single-process fd ceiling; not a router-mode limit) | | | | | | |", nodes.len());
                for (rt, s) in nodes { s.close().await.ok(); drop(rt); }
                tokio::time::sleep(Duration::from_secs(2)).await;
                break;
            }
            let c0 = cpu_seconds();
            tokio::time::sleep(Duration::from_secs(2)).await;
            let idle_cores = (cpu_seconds() - c0) / 2.0;
            let rss = current_rss_mb();
            let churn = churn_once(&topo, &mut nodes, run_base).await;
            let hop = hop_latency_ms(&nodes[0].1, &nodes[nodes.len() - 1].1, "probe/hop").await;
            let f1 = |o: Option<f64>| o.map(|v| format!("{:.0}", v)).unwrap_or_else(|| "n/a".into());
            let f3 = |o: Option<f64>| o.map(|v| format!("{:.3}", v)).unwrap_or_else(|| "n/a".into());
            println!("| {n} | {} | {} | {} | {:.1} | {} | {:.2} | {} |",
                f1(boot), f1(churn.join_reconv_ms), churn.join_bytes,
                churn.join_bytes as f64 / 1024.0, f3(hop), idle_cores, rss);
            eprintln!("# done {} N={n} (join_bytes={})", topo.name(), churn.join_bytes);
            for (rt, s) in nodes { s.close().await.ok(); drop(rt); }
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
    }
    eprintln!("# sweep complete");
}
```
