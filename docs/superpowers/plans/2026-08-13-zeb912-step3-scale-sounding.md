# ZEB-912 step-3 Scale Sounding Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Measure router-mode zenoh's linkstate flood + spanning-tree recompute cost as a full-mesh community grows to N=200, and decide flip-router-by-default vs per-community opt-in vs R4-prerequisite.

**Architecture:** A standalone raw-zenoh probe crate (outside the repo, never a workspace member) opens N router-mode sessions over loopback TCP in one process, wired into full-mesh / ring / line topologies. It builds each session through `RuntimeBuilder` (the exact path harmony production uses) so it can hold the `Runtime` handle and read per-transport `stats`. It measures four things per (topology, N): routing-OAM flood per churn event (data-quiescent), reconvergence time, whole-process CPU/RSS, and hop latency. One real-stack datapoint from the s14 e2e harness anchors the probe's latency. Output is a research findings doc with the decision.

**Tech Stack:** Rust, `zenoh = "=1.9.0"` (features `internal`, `stats`, `unstable`), `tokio`, `libc` (getrusage). No changes to production code paths.

## Global Constraints

- **zenoh pinned `=1.9.0`** — the version the client ships; the whole sounding is version-specific.
- **Production-mirroring session config**, verbatim: `mode: "router"`, `scouting/multicast/enabled: false`, `scouting/gossip/enabled: false`, `timestamping/enabled: false` (router-mode default is `true` — pin it, per spike §3.3).
- **Session build order MUST mirror `zenoh::open`**: `RuntimeBuilder::new(cfg).build().await` → `zenoh::session::init(runtime.clone().into()).await` → `runtime.start().await` (harmony `event_loop.rs:13196-13202`).
- **Flood measured data-quiescent** — zero application `put`s during a flood window, so every transport byte moved across a churn event is routing overhead.
- **N sweep: 10 / 25 / 50 / 100 / 200.** 200 is a hard product ceiling. Extend toward 200 until a metric crosses "unacceptable" OR the host runs out of headroom — the limit logged explicitly, never a silent cap.
- **The probe is NOT committed to the repo** (not a workspace member). Its full source lands in the findings-doc appendix. Recommended build location: `$HOME/work/zeb912-scale-probe/` (sibling of the repo, outside the git tree).
- **Findings doc** → `docs/research/2026-08-13-zeb912-r3-scale-sounding.md`. Decision recorded on **ZEB-912** (then closed); target degree recorded on **ZEB-914** if the call is opt-in / R4-prerequisite.
- **Do NOT merge anything to main autonomously — Jake merges.** Doc-only artifacts land direct-to-main only when Jake approves at close-out.

---

### Task 1: Probe scaffold + router multi-hop baseline

Stand up the crate and prove the vehicle: N router sessions built the production way, wired into a topology, multi-hopping data. Reproduces the spike's headline result (2-hop router delivery ~sub-ms) before any metric work.

**Files:**
- Create: `$HOME/work/zeb912-scale-probe/Cargo.toml`
- Create: `$HOME/work/zeb912-scale-probe/src/main.rs`

**Interfaces:**
- Produces: `enum Topo { Mesh, Ring, Line }`; `fn node_cfg(port: u16, connect: &[u16]) -> zenoh::Config`; `fn connects_for(topo: &Topo, i: usize, n: usize, base: u16) -> Vec<u16>`; `async fn open_router_session(cfg: zenoh::Config) -> (Runtime, zenoh::Session)`; `async fn spawn_topology(topo: &Topo, n: usize, base: u16) -> Vec<(Runtime, zenoh::Session)>` returning nodes index-ordered.

- [ ] **Step 1: Create `Cargo.toml`**

```toml
[package]
name = "zeb912-scale-probe"
version = "0.1.0"
edition = "2021"

[dependencies]
zenoh = { version = "=1.9.0", features = ["internal", "stats", "unstable"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "time"] }
libc = "0.2"
```

- [ ] **Step 2: Write the config + topology builders in `src/main.rs`**

```rust
use std::time::{Duration, Instant};
use zenoh::internal::runtime::Runtime;
use zenoh::Session;

#[derive(Clone, Copy, Debug)]
enum Topo { Mesh, Ring, Line }

impl Topo {
    fn name(&self) -> &'static str { match self { Topo::Mesh => "mesh", Topo::Ring => "ring", Topo::Line => "line" } }
}

/// Production-mirroring router-mode config (Global Constraints). Every node
/// listens on its own loopback port and dials the given lower-index ports.
fn node_cfg(port: u16, connect: &[u16]) -> zenoh::Config {
    let mut c = zenoh::Config::default();
    c.insert_json5("mode", "\"router\"").expect("mode");
    c.insert_json5("scouting/multicast/enabled", "false").expect("mcast");
    c.insert_json5("scouting/gossip/enabled", "false").expect("gossip");
    c.insert_json5("timestamping/enabled", "false").expect("ts"); // router default is true
    c.insert_json5("listen/endpoints", &format!("[\"tcp/127.0.0.1:{port}\"]")).expect("listen");
    let connect_arr = connect.iter()
        .map(|p| format!("\"tcp/127.0.0.1:{p}\""))
        .collect::<Vec<_>>().join(",");
    c.insert_json5("connect/endpoints", &format!("[{connect_arr}]")).expect("connect");
    c
}

/// Which lower-index nodes node `i` dials. Each node listens on `base + i`.
/// Mesh: dial every lower index (each pair connects once). Line: dial i-1.
/// Ring: line plus the last node dials the first to close the loop.
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
```

- [ ] **Step 3: Write the session opener (mirrors `open_session_with_runtime`)**

```rust
/// Mirror of harmony's open_session_with_runtime (event_loop.rs:13196-13202).
/// Order is load-bearing: build -> session::init -> runtime.start.
async fn open_router_session(cfg: zenoh::Config) -> (Runtime, Session) {
    let mut runtime = zenoh::internal::runtime::RuntimeBuilder::new(cfg)
        .build().await.expect("runtime build");
    let session = zenoh::session::init(runtime.clone().into()).await.expect("session init");
    runtime.start().await.expect("runtime start");
    (runtime, session)
}

/// Open all N nodes in index order; return them so callers can index neighbours.
async fn spawn_topology(topo: &Topo, n: usize, base: u16) -> Vec<(Runtime, Session)> {
    let mut nodes = Vec::with_capacity(n);
    for i in 0..n {
        let connect = connects_for(topo, i, n, base);
        nodes.push(open_router_session(node_cfg(base + i as u16, &connect)).await);
    }
    nodes
}
```

- [ ] **Step 4: Write a `main` that reproduces the spike baseline (N=3 line, 2-hop delivery)**

```rust
async fn deliver_ms(src: &Session, dst: &Session, key: &str, timeout: Duration) -> Option<f64> {
    let sub = dst.declare_subscriber(key).await.ok()?;
    tokio::time::sleep(Duration::from_millis(300)).await; // let the sub declaration flood
    let t0 = Instant::now();
    src.put(key, "x").await.ok()?;
    match tokio::time::timeout(timeout, sub.recv_async()).await {
        Ok(Ok(_)) => Some(t0.elapsed().as_secs_f64() * 1000.0),
        _ => None,
    }
}

#[tokio::main]
async fn main() {
    let nodes = spawn_topology(&Topo::Line, 3, 7600).await;
    tokio::time::sleep(Duration::from_millis(1500)).await; // converge
    let a = &nodes[0].1;
    let c = &nodes[2].1;
    match deliver_ms(a, c, "probe/baseline", Duration::from_secs(5)).await {
        Some(ms) => println!("BASELINE line N=3 A->C (2-hop): DELIVERED in {ms:.3} ms"),
        None => println!("BASELINE line N=3 A->C (2-hop): NOT delivered (router multi-hop broken!)"),
    }
    for (rt, s) in nodes { s.close().await.ok(); drop(rt); }
}
```

- [ ] **Step 5: Build and run — verify multi-hop works**

Run: `cd $HOME/work/zeb912-scale-probe && cargo run --release`
Expected: first build compiles zenoh (minutes, cold) then prints `BASELINE line N=3 A->C (2-hop): DELIVERED in <~1> ms`. If it says NOT delivered, the router-mode config or build order is wrong — stop and fix before proceeding (the whole sounding depends on multi-hop working).

- [ ] **Step 6: Commit the probe locally (in its own dir, not the repo)**

```bash
cd $HOME/work/zeb912-scale-probe && git init -q && git add -A && git commit -q -m "zeb912 scale probe: scaffold + router multi-hop baseline"
```

---

### Task 2: Routing-OAM flood readout (front-loaded de-risk)

The riskiest metric: reading per-node routing bytes. Prove it at N=3 data-quiescent before trusting it at N=200. The access path is confirmed (`Runtime::get_transports_blocking()` → `TransportUnicast::get_stats()`, crate built with `stats`); this task nails the exact struct field spellings and proves a churn event produces a measurable byte delta.

**Files:**
- Modify: `$HOME/work/zeb912-scale-probe/src/main.rs`

**Interfaces:**
- Consumes: `Runtime` handles from `spawn_topology` (Task 1).
- Produces: `async fn routing_totals(rt: &Runtime) -> (u64, u64)` returning `(bytes, msgs)` summed over the node's unicast transports; `async fn mesh_routing_totals(nodes: &[(Runtime, Session)]) -> (u64, u64)` summing across all nodes.

- [ ] **Step 1: Write the stats readout, confirming field names against rustdoc**

Confirm the exact `Transport` enum variant and `TransportStats` getters first:
Run: `cd $HOME/work/zeb912-scale-probe && cargo doc -p zenoh --no-deps --features internal,stats 2>/dev/null; cargo doc -p zenoh-stats --no-deps 2>/dev/null` then grep the generated docs, OR inspect the source directly:
Run: `grep -rn 'stats_struct\|tx_bytes\|rx_bytes\|t_msgs' ~/.cargo/registry/src/*/zenoh-stats-1.9.0/src/ 2>/dev/null | head`
The `stats_struct!` macro generates `get_<field>()` accessors. Write the readout using the confirmed names (the code below assumes `get_tx_bytes/get_rx_bytes/get_tx_t_msgs/get_rx_t_msgs` — adjust to whatever the macro emits):

```rust
/// Sum routing/transport bytes+messages across a node's unicast transports.
/// Requires the crate built with feature "stats" or get_stats() errors.
async fn routing_totals(rt: &Runtime) -> (u64, u64) {
    let (mut bytes, mut msgs) = (0u64, 0u64);
    for t in rt.get_transports_blocking() {
        // Match the unicast variant; confirm the exact path via `cargo build` errors.
        if let zenoh::internal::runtime::Transport::Unicast(u) = t {
            if let Ok(s) = u.get_stats() {
                bytes += s.get_tx_bytes() as u64 + s.get_rx_bytes() as u64;
                msgs  += s.get_tx_t_msgs() as u64 + s.get_rx_t_msgs() as u64;
            }
        }
    }
    (bytes, msgs)
}

async fn mesh_routing_totals(nodes: &[(Runtime, Session)]) -> (u64, u64) {
    let (mut b, mut m) = (0u64, 0u64);
    for (rt, _) in nodes {
        let (nb, nm) = routing_totals(rt).await;
        b += nb; m += nm;
    }
    (b, m)
}
```

- [ ] **Step 2: Prove a churn event yields a measurable delta at N=3**

Replace `main` body with a data-quiescent flood probe: converge a 3-node mesh, snapshot totals, append a 4th node (one join = one churn event), let it converge with NO puts, snapshot again.

```rust
#[tokio::main]
async fn main() {
    let mut nodes = spawn_topology(&Topo::Mesh, 3, 7600).await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let (b0, m0) = mesh_routing_totals(&nodes).await;
    println!("steady N=3: bytes={b0} msgs={m0}");

    // one churn event: a 4th member joins (dials all 3), data-quiescent.
    let joiner = open_router_session(node_cfg(7603, &[7600, 7601, 7602])).await;
    nodes.push(joiner);
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let (b1, m1) = mesh_routing_totals(&nodes).await;
    println!("after 1 join: bytes={b1} msgs={m1}  DELTA bytes={} msgs={}", b1 - b0, m1 - m0);
    assert!(b1 > b0, "a join must move routing bytes; got no delta (stats wired wrong?)");

    for (rt, s) in nodes { s.close().await.ok(); drop(rt); }
}
```

- [ ] **Step 3: Build and run — verify a positive, sane delta**

Run: `cd $HOME/work/zeb912-scale-probe && cargo run --release`
Expected: prints a steady total, then a positive `DELTA bytes=...` for one join. The assert guards against a silently-zero readout. If `get_stats()` errors or the enum path is wrong, fix the field/variant spelling (Step 1) now — this is the front-loaded de-risk.

- [ ] **Step 4: Fallback ONLY if the accessor is unreachable**

If `get_transports_blocking`/`get_stats` cannot be reached from the built crate (visibility surprise), route every inter-node link through a byte-counting loopback relay: node `i` dials relay ports; each relay `tokio::io::copy`s both directions through `AtomicU64` counters, forwarding to the real listen port. Read the counters instead of `routing_totals`. Document in the findings that flood was measured at the socket layer. Do NOT spend more than one attempt here before falling back — the relay is guaranteed to work.

- [ ] **Step 5: Commit**

```bash
cd $HOME/work/zeb912-scale-probe && git add -A && git commit -q -m "zeb912 probe: routing-OAM flood readout via Runtime stats"
```

---

### Task 3: Reconvergence, CPU/RSS, and hop-latency instrumentation

Add the remaining three metrics as helpers, each verified at N=3 so the sweep can trust them.

**Files:**
- Modify: `$HOME/work/zeb912-scale-probe/src/main.rs`

**Interfaces:**
- Produces: `async fn reconverge_ms(src: &Session, dst: &Session, key: &str, budget: Duration) -> Option<f64>`; `fn cpu_and_peak_rss() -> (f64, u64)`; `async fn hop_latency_ms(src: &Session, dst: &Session, key: &str) -> Option<f64>` (alias of `deliver_ms` semantics, kept named for the sweep table).

- [ ] **Step 1: Reconvergence timer — pre-declared sub, poll puts until first receipt**

```rust
/// Time from now until `dst` first receives a put from `src`, retrying the put.
/// Use after a churn event with the subscriber ALREADY declared (so the timer
/// captures route reconvergence, not fresh sub-declaration flood).
async fn reconverge_ms(src: &Session, dst: &Session, key: &str, budget: Duration) -> Option<f64> {
    let sub = dst.declare_subscriber(key).await.ok()?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // drain anything already queued
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
```

- [ ] **Step 2: CPU/RSS sampler via getrusage**

```rust
/// (cumulative CPU seconds user+sys for the whole process, peak RSS bytes).
/// macOS ru_maxrss is BYTES (this probe runs on Koya/macOS); on Linux it is KiB.
fn cpu_and_peak_rss() -> (f64, u64) {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru); }
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    (secs(ru.ru_utime) + secs(ru.ru_stime), ru.ru_maxrss as u64)
}
```
CPU% over a window = `(cpu_after - cpu_before) / wall_seconds` cores. Peak RSS is monotonic, so read it at each N's end.

- [ ] **Step 3: Hop-latency helper (rename-through of the baseline)**

```rust
async fn hop_latency_ms(src: &Session, dst: &Session, key: &str) -> Option<f64> {
    deliver_ms(src, dst, key, Duration::from_secs(5)).await
}
```

- [ ] **Step 4: Verify all three at N=3**

Add to `main`: a mesh N=3, print `reconverge_ms(a,c,...)`, `cpu_and_peak_rss()` before/after a 2s sleep, and `hop_latency_ms(a,c,...)`.
Run: `cd $HOME/work/zeb912-scale-probe && cargo run --release`
Expected: reconverge and hop-latency print sub-second/sub-ms values; CPU delta is a small positive number of core-seconds; peak RSS is tens–hundreds of MB. Any `None` at N=3 mesh (diameter 1) means a helper is broken — fix before the sweep.

- [ ] **Step 5: Commit**

```bash
cd $HOME/work/zeb912-scale-probe && git add -A && git commit -q -m "zeb912 probe: reconverge, cpu/rss, hop-latency metrics"
```

---

### Task 4: Churn drivers (topology-aware)

Encode the churn semantics — they differ by topology, and getting this wrong silently measures the wrong thing.

**Files:**
- Modify: `$HOME/work/zeb912-scale-probe/src/main.rs`

**Interfaces:**
- Consumes: `spawn_topology`, `mesh_routing_totals`, `reconverge_ms`.
- Produces: `async fn boot_convergence_ms(topo, n, base) -> (Option<f64>, Vec<(Runtime, Session)>)`; `struct ChurnResult { join_bytes: u64, join_reconv_ms: Option<f64>, leave_bytes: u64 }`; `async fn churn_once(topo, nodes: &mut Vec<(Runtime,Session)>, base) -> ChurnResult`.

- [ ] **Step 1: Boot-burst convergence**

```rust
/// Time from all-open to first end-to-end delivery across the topology diameter.
async fn boot_convergence_ms(topo: &Topo, n: usize, base: u16)
    -> (Option<f64>, Vec<(Runtime, Session)>) {
    let t0 = Instant::now();
    let nodes = spawn_topology(topo, n, base).await;
    let (src, dst) = (&nodes[0].1, &nodes[n - 1].1); // ends: diameter-far in line/ring
    let sub = dst.declare_subscriber("probe/boot").await.expect("sub");
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
```

- [ ] **Step 2: One churn event, topology-aware**

Semantics (bake into the doc comment):
- **Mesh:** a member joins (appends at index n, dials all existing) → measure join flood + join reconvergence; then it leaves → measure leave flood. Preserves the full-mesh invariant.
- **Ring:** a mid-node drops (ring → line, still connected) → measure survivor-repath reconvergence between its two former neighbours; then a replacement rejoins those two neighbours → leave/join flood.
- **Line:** steady churn is N/A — dropping a mid-node PARTITIONS the line. `churn_once` for `Line` returns `join_reconv_ms: None` and only records boot convergence + hop latency elsewhere. Note this explicitly in output.

```rust
struct ChurnResult { join_bytes: u64, join_reconv_ms: Option<f64>, leave_bytes: u64 }

async fn churn_once(topo: &Topo, nodes: &mut Vec<(Runtime, Session)>, base: u16) -> ChurnResult {
    match topo {
        Topo::Line => ChurnResult { join_bytes: 0, join_reconv_ms: None, leave_bytes: 0 },
        Topo::Mesh => {
            let n = nodes.len();
            let (b0, _) = mesh_routing_totals(nodes).await;
            let connect: Vec<u16> = (0..n).map(|j| base + j as u16).collect();
            let joiner = open_router_session(node_cfg(base + n as u16, &connect)).await;
            nodes.push(joiner);
            let reconv = reconverge_ms(&nodes[0].1, &nodes[n].1, "probe/churn", Duration::from_secs(30)).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            let (b1, _) = mesh_routing_totals(nodes).await;
            let (rt, s) = nodes.pop().unwrap();
            s.close().await.ok(); drop(rt);
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let (b2, _) = mesh_routing_totals(nodes).await;
            ChurnResult { join_bytes: b1.saturating_sub(b0), join_reconv_ms: reconv, leave_bytes: b2.saturating_sub(b1) }
        }
        Topo::Ring => {
            // drop a mid node (index n/2), measure repath between its neighbours, then rejoin.
            let n = nodes.len();
            let mid = n / 2;
            let (b0, _) = mesh_routing_totals(nodes).await;
            let (rt, s) = nodes.remove(mid);
            s.close().await.ok(); drop(rt);
            let left = &nodes[mid - 1].1;
            let right = &nodes[mid % nodes.len()].1;
            let reconv = reconverge_ms(left, right, "probe/repath", Duration::from_secs(30)).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            let (b1, _) = mesh_routing_totals(nodes).await;
            ChurnResult { join_bytes: 0, join_reconv_ms: reconv, leave_bytes: b1.saturating_sub(b0) }
        }
    }
}
```

- [ ] **Step 3: Verify at N=10 mesh and ring**

Add a temporary `main` calling `boot_convergence_ms` then `churn_once` for Mesh N=10 and Ring N=10; print the `ChurnResult`.
Run: `cd $HOME/work/zeb912-scale-probe && cargo run --release`
Expected: mesh join reconverges quickly with a positive `join_bytes`; ring repath returns `Some(..)` (still connected after mid-drop). A `None` ring repath at N=10 means the ring didn't actually close — check `connects_for(Ring, ...)`.

- [ ] **Step 4: Commit**

```bash
cd $HOME/work/zeb912-scale-probe && git add -A && git commit -q -m "zeb912 probe: topology-aware churn drivers"
```

---

### Task 5: Sweep harness + markdown tables + headroom handling

The run that produces the data. Loop topologies × N, collect all metrics, print copy-pasteable markdown, and handle the host ceiling gracefully.

**Files:**
- Modify: `$HOME/work/zeb912-scale-probe/src/main.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: final `main` that prints one markdown table per topology and a per-run raw line; no further code depends on it.

- [ ] **Step 1: Raise the OS fd limit and pick a base port per run**

At `main` start, raise the soft fd limit (200 mesh nodes ≈ 200² sockets in one process):

```rust
fn raise_fd_limit() {
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim); }
    lim.rlim_cur = lim.rlim_max; // raise soft to hard
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim); }
}
```
Use a distinct base port per (topology, N) run (e.g. `base = 7000 + run_index * 300`) so torn-down runs don't collide with `TIME_WAIT` sockets.

- [ ] **Step 2: The sweep loop with headroom guard**

```rust
#[tokio::main]
async fn main() {
    raise_fd_limit();
    let sizes = [10usize, 25, 50, 100, 200];
    let topos = [Topo::Mesh, Topo::Ring, Topo::Line];
    for topo in topos {
        println!("\n## {} \n", topo.name());
        println!("| N | boot_ms | join_reconv_ms | join_bytes | leave_bytes | hop_ms | cpu_cores | peak_rss_mb |");
        println!("|---|---|---|---|---|---|---|---|");
        for (ri, &n) in sizes.iter().enumerate() {
            let base = 7000 + (ri as u16) * 300;
            let (cpu0, _) = cpu_and_peak_rss();
            let t_wall = Instant::now();
            // guard: spawning N sessions may exhaust the host before 200.
            let (boot, mut nodes) = boot_convergence_ms(&topo, n, base).await;
            if nodes.len() < n {
                println!("| {n} | HOST-LIMIT: only spawned {} sessions | | | | | | |", nodes.len());
                for (rt, s) in nodes { s.close().await.ok(); drop(rt); }
                break; // ceiling reached for this topology
            }
            let churn = churn_once(&topo, &mut nodes, base).await;
            let hop = hop_latency_ms(&nodes[0].1, &nodes[n - 1].1, "probe/hop").await;
            let (cpu1, rss) = cpu_and_peak_rss();
            let cores = (cpu1 - cpu0) / t_wall.elapsed().as_secs_f64();
            let f = |o: Option<f64>| o.map(|v| format!("{v:.1}")).unwrap_or_else(|| "n/a".into());
            println!("| {n} | {} | {} | {} | {} | {} | {:.2} | {} |",
                f(boot), f(churn.join_reconv_ms), churn.join_bytes, churn.leave_bytes,
                f(hop), cores, rss / (1024 * 1024));
            for (rt, s) in nodes { s.close().await.ok(); drop(rt); }
            tokio::time::sleep(Duration::from_secs(2)).await; // let sockets drain between runs
        }
    }
}
```

- [ ] **Step 3: Run the full sweep, capture output**

Run: `cd $HOME/work/zeb912-scale-probe && cargo run --release 2>/dev/null | tee sweep-output.md`
Expected: three markdown tables (mesh/ring/line), rows for N=10..200 or a `HOST-LIMIT` row where the host gave out. Mesh `join_bytes` and `boot_ms` should climb with N; the shape of that climb is the headline result. If mesh N=200 shows a `HOST-LIMIT` row, that ceiling is itself a finding — record the N reached.

- [ ] **Step 4: Sanity-check the numbers before trusting them**

Eyeball: does mesh boot_ms grow super-linearly? Is ring cheaper than mesh at the same N (the R4 payoff)? Are hop_ms values physically plausible (sub-10ms)? If mesh and ring are identical, the topology builder isn't actually differing — investigate before writing findings. Re-run once to check variance (these are single-host timings).

- [ ] **Step 5: Commit the probe + raw output**

```bash
cd $HOME/work/zeb912-scale-probe && git add -A && git commit -q -m "zeb912 probe: full sweep harness + captured sweep-output.md"
```

---

### Task 6: Real-stack latency anchor (s14)

One datapoint proving the loopback probe predicts the real harmony-over-iroh stack's latency. Scope: extract an existing number if s14 already logs delivery timing; add a minimal timestamp only if it doesn't.

**Files:**
- Read: `e2e-harness/tests/e2e_two_node.rs` (the s14 test)
- Modify (only if needed): `e2e-harness/tests/e2e_two_node.rs` — add a `tracing::info!` timestamp around the B↔C delivery assertion.

**Interfaces:**
- Produces: one real-stack 3-node router delivery-latency figure for the findings' anchor section.

- [ ] **Step 1: Read s14, decide extract-vs-instrument**

Run: `grep -n 's14\|delivered\|Instant::now\|elapsed\|channel message' e2e-harness/tests/e2e_two_node.rs | head -30`
If s14 already records a send→receive interval, extract it from a run's logs. If not, add a single `let t = std::time::Instant::now();` before the send and `tracing::info!(elapsed_ms = t.elapsed().as_secs_f64()*1000.0, "s14 B->C delivery")` after the receive — the minimal change.

- [ ] **Step 2: Run s14 with the ZEB-690 fresh-binary guard satisfied**

Router mode is env-gated, so the harness spawns real nodes with `HARMONY_ZENOH_MODE=router`. Build the app binary fresh and pin it (stale-binary trap):
Run:
```bash
cd src-tauri && cargo build --bin harmony-app --features <as s14 requires>
export HARMONY_APP_BIN=$(pwd)/target/debug/harmony-app
cd ../e2e-harness && cargo test --features e2e s14 -- --nocapture 2>&1 | grep -i 'delivery\|elapsed'
```
Expected: an `elapsed_ms` figure for a real 3-node router-mode delivery (single-digit to low-tens of ms over loopback iroh).

- [ ] **Step 3: Compare to the probe's N=3 latency**

Take the probe's N=3 line/mesh `hop_ms` from Task 5 and note the ratio to the s14 real-stack figure. Expected: the real stack is higher (real iroh links, harmony overhead) but within a small constant factor — that offset is what the findings report as the "loopback→production latency multiplier." A wildly different ratio (>~50×) means loopback is not predictive; say so plainly in the findings.

- [ ] **Step 4: Revert any temporary s14 instrumentation (keep the tree clean)**

If Step 1 added a timestamp purely to read a number, revert it (the anchor value is captured in the findings; the probe is not shipping). If it's a genuinely useful permanent assertion, keep it and note it for the close-out PR. Either way:
Run: `cd /Users/zeblith/work/zeblithic/harmony-client && git status`
Expected: either a clean tree, or exactly the intended s14 change staged for close-out.

---

### Task 7: Findings doc + decision + close-out

Turn the numbers into the decision and the doc, and close the ticket.

**Files:**
- Create: `docs/research/2026-08-13-zeb912-r3-scale-sounding.md`

**Interfaces:**
- Consumes: `sweep-output.md` (Task 5), the anchor figure (Task 6), the probe source (Tasks 1–5).

- [ ] **Step 1: Write the findings doc (spike-doc style)**

Sections, matching `docs/research/2026-08-12-zeb912-r3-zenoh-multihop-spike.md`:
1. **Executive summary** — the decision (flip-by-default / opt-in / R4-prerequisite) in the first sentence, then the 3–4 load-bearing numbers.
2. **Method** — probe design, production-mirroring config, data-quiescent flood, the N=200 product ceiling, single-host caveat.
3. **Results** — the three markdown tables verbatim from `sweep-output.md`; call out the mesh flood/boot scaling shape and the mesh-vs-ring gap.
4. **The anchor** — the s14 real-stack figure, the loopback→production latency multiplier, and the explicit statement that flood/recompute is transport-agnostic *by construction* (linkstate OAM rides zenoh messages regardless of link type; `zenoh-1.9.0/src/net/routing/hat/router/`) — a code-verified claim, not measured.
5. **Decision** — which threshold was crossed and at what N; the recommendation; what it means for router-by-default and for R4/ZEB-914 (target degree if any).
6. **Appendix** — the full probe `Cargo.toml` + `src/main.rs`, plus the exact `cargo run --release` reproduction command.

- [ ] **Step 2: Self-review the findings against the spec's decision framework**

Confirm the doc answers "does full-mesh router serve a ~200 community?" with a yes/no and a number, and that the recommendation matches the measured threshold crossing. Fix any hand-waving.

- [ ] **Step 3: Update ZEB-912 with the decision and prepare close-out**

Post a Linear comment on ZEB-912 summarizing the decision + linking the findings doc. Do NOT set Done yet if a close-out PR/commit is pending Jake's merge — surface the decision and the proposed disposition (doc-only → direct-to-main on approval; any s14 anchor change via small PR) and STOP for Jake. If the decision is opt-in / R4-prerequisite, post the target degree / N\* on ZEB-914.

- [ ] **Step 4: Surface to Jake and hold**

Report the decision, the headline numbers, and the proposed close-out (land findings doc on main, close ZEB-912). Do not merge or push to main autonomously — Jake merges. Pushover if he's gone idle.

---

## Self-Review

**Spec coverage:**
- Vehicle (raw-zenoh probe + s14 anchor, not committed, appendix source) → Tasks 1–2, 6, 7. ✓
- Topologies (mesh/ring/line) → Task 1 `connects_for`, exercised in Tasks 4–5. ✓
- N sweep 10..200 with headroom limit → Task 5. ✓
- Churn (boot-burst + steady, topology-aware) → Task 4. ✓
- Metrics (flood data-quiescent, reconvergence, CPU/RSS, hop-latency) → Tasks 2, 3, 5. ✓
- Decision framework (flip/opt-in/R4 anchored to N=200) → Task 7. ✓
- Close-out (findings doc, close ZEB-912, feed ZEB-914) → Task 7. ✓
- Transport-agnostic flood argument + honesty boundary → Task 7 Step 1.4. ✓

**Placeholder scan:** The one residual API-spelling unknown (TransportStats getter names / Transport enum variant) is explicitly front-loaded and de-risked in Task 2 Steps 1–3 with a guaranteed fallback in Step 4 — not a hidden gap. Task 6 is scoped extract-first, instrument-only-if-needed. No "TBD"/"handle edge cases" placeholders.

**Type consistency:** `Topo`, `node_cfg`, `connects_for`, `open_router_session`, `spawn_topology`, `routing_totals`/`mesh_routing_totals`, `reconverge_ms`, `cpu_and_peak_rss`, `hop_latency_ms`, `boot_convergence_ms`, `ChurnResult`, `churn_once` are named consistently across tasks; `ChurnResult` fields (`join_bytes`, `join_reconv_ms`, `leave_bytes`) match between Task 4 definition and Task 5 use.
