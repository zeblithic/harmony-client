# ZEB-929 Part 1 — R4 bounded-degree scale validation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Empirically prove the R4 bounded-degree dial filter (PR #674) bounds per-node degree at scale and that the resulting sparse graph delivers with sub-second membership reconvergence.

**Architecture:** Two vehicles. (1) An out-of-tree raw-zenoh **scale probe** extended with the engine's real circulant R4 graph — measures the *graph's* degree/flood/reconvergence at N=50…200. (2) Two in-tree **Rust tests** proving the live `controller→oracle→supervisor` pipeline produces exactly that graph at scale. No real `harmony-app` fleet.

**Tech Stack:** raw `zenoh 1.9.0` (probe, `~/work/zeb912-scale-probe/`, out-of-tree); `cargo nextest` + `admission_oracle.rs` / `reconnect_supervisor.rs` / `community_topology.rs` (in-tree tests).

**Spec:** `docs/superpowers/specs/2026-08-13-zeb929-r4-scale-validation-design.md`.

## Global Constraints

- **Router gate:** filter engages only under `HARMONY_ZENOH_MODE=router`; peer mode unchanged, out of scope.
- **`FULL_MESH_THRESHOLD = 32`** (`community_topology.rs:33`) — bounded-degree measurements use ≥ 32 devices. Not overridden.
- **Degree = exact `community_neighbors(...).len()`**, never a naive formula. Antipode exception: when `N/2` is a power of two the top offset `o=N/2` collapses (`(i+o)≡(i−o) mod N`), degree is one less (N=64→11, N=32→9). N=100→12 and N=200→14 are antipode-free.
- **Two-hash discipline:** enrolled device key `[u8;32]` (on the ring, in `admitted`) ≠ iroh `node_id` `[u8;32]` (dial target) ≠ `OwnerAddr` `[u8;16]`. The oracle binds them; tests keep them distinct.
- **Probe stays out-of-tree** (scratch, R3 precedent); source + results captured in the findings doc appendix. No repo/CI weight.
- **Gates from `src-tauri/`:** `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(<name>)'` for a single test; final sweep is full `--workspace --all-targets` (CLAUDE.md).

## File Structure

- **`src-tauri/src/admission_oracle.rs`** — add one `#[test]` (`compute_admitted_at_n200_bounded_degree_is_14`) in the existing `mod tests`; reuse the file's `synth` helper. Pins claim C at the ticket's headline N.
- **`src-tauri/src/reconnect_supervisor.rs`** — add one `#[tokio::test(start_paused=true)]` (`r4_bounded_degree_partition_and_delta_at_scale`) in the existing `mod tests`; reuse `RecordingDialer`, `run_reconnect_supervisor`, `peer`, `cfg`, plus a new local `seed_peer` helper. Proves claim D at scale.
- **`~/work/zeb912-scale-probe/src/main.rs`** *(out-of-tree)* — add `Topo::R4`, `ring_offsets`, `r4_neighbors`, a `degree` table column; run a ring+R4 sweep. Produces claims A/B/C numbers.
- **`docs/research/2026-08-13-zeb929-r4-scale-validation.md`** — findings: R4 table vs R3 ring baseline, the reconvergence comparison, degree confirmation, threats, probe source appendix.

The two in-tree tests are independent; the probe feeds the findings doc. PR (`zeblith/zeb-929-r4-harness-validation`) bundles the two tests **and** the findings doc (the doc gives the reviewer the empirical context for the tests). The probe is not committed.

---

### Task 1: V2a — pin the N=200 bounded degree (`admission_oracle.rs`)

**Files:**
- Modify: `src-tauri/src/admission_oracle.rs` (add a test in `mod tests`, after `compute_admitted_above_threshold_is_bounded_and_excludes_self` ~line 261)

**Interfaces:**
- Consumes: `compute_admitted(&[(BTreeSet<[u8;32]>, Vec<u8>)], &[u8;32]) -> BTreeSet<[u8;32]>`, `community_neighbors(&BTreeSet<[u8;32]>, &[u8;32], &[u8]) -> BTreeSet<[u8;32]>`, the module-local `synth(n) -> BTreeSet<[u8;32]>` helper (blake3 of the index — 200 distinct keys).
- Produces: nothing consumed downstream (leaf regression test).

This is a **characterization** test of shipped code: it pins the exact headline claim (degree 14 at N=200) as a regression guard. To confirm it discriminates, assert the wrong value first, watch it fail, then correct.

- [ ] **Step 1: Write the test with a deliberately wrong expected degree**

Add to `mod tests`:
```rust
#[test]
fn compute_admitted_at_n200_bounded_degree_is_14() {
    // The ticket's headline: at N=200 the bounded degree is ~2·log₂N = 14
    // (100 is not a power of two, so no antipode collapse). Pins claim C.
    let devices = synth(200);
    let self_vk = *devices.iter().next().unwrap();
    let salt = b"zeb929".to_vec();
    let out = compute_admitted(&[(devices.clone(), salt.clone())], &self_vk);
    // Structural: equals the engine directly (never a naive formula).
    assert_eq!(out, community_neighbors(&devices, &self_vk, &salt));
    assert!(!out.contains(&self_vk));
    assert_eq!(out.len(), 13, "WRONG on purpose — confirm the test discriminates");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(compute_admitted_at_n200_bounded_degree_is_14)'`
Expected: FAIL — `assertion failed: (left == right) left: 14, right: 13` (proves the path yields 14, not the wrong 13).

- [ ] **Step 3: Correct the expected degree to 14**

Change the last assertion to:
```rust
    assert_eq!(out.len(), 14, "bounded degree at N=200 is 2·(⌊log₂100⌋+1) = 14");
```

- [ ] **Step 4: Run it to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(compute_admitted_at_n200_bounded_degree_is_14)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/admission_oracle.rs
git commit -m "ZEB-929: pin bounded degree = 14 at N=200 (claim C)"
```

---

### Task 2: V2b — live pipeline produces the bounded set at scale (`reconnect_supervisor.rs`)

**Files:**
- Modify: `src-tauri/src/reconnect_supervisor.rs` (add a `seed_peer` helper near `seed` ~line 1145, and a test in `mod tests` after `r4_denied_peer_parked_until_admitted_then_dialed` ~line 2369)

**Interfaces:**
- Consumes: `RecordingDialer::succeeding() -> Arc<Self>` + `count_for([u8;32]) -> usize`; `run_reconnect_supervisor(handle, dialer, resolver, telemetry, self_id, config)`; `SupervisorHandle::{new, set_admission_oracle, kick, kick_sweep, states_snapshot}`; `PeerStateWire::{Connected, Dormant}`; `ReconnectTrigger::{NewPeer, Dropped}`; `peer(u8) -> [u8;32]`; `cfg(base_ms, cap_ms, dormant_ms, cooldown_ms, max_dials, fallback_ms) -> SupervisorConfig`; `AdmissionOracle::{new(bool), bind([u8;16],[u8;32],[u8;32]), publish_admitted(BTreeSet<[u8;32]>)}`; `compute_admitted`; `community_neighbors`; `ReachabilityResolver::{new, update(OwnerAddr, ReachabilityAnnouncePayload, Hlc)}`.
- Produces: nothing downstream (leaf test).

Prove: driving the real oracle + real dispatch enforcement over a 100-device community yields **exactly** the engine's ring-neighbor selection as the live dial set — plus admission-delta recovery and revocation, at scale.

- [ ] **Step 1: Add a per-peer resolver seed helper**

The existing `seed` hardcodes `OwnerAddr([0xAA;16])`, so seeding many peers would collide on one owner. Add next to it (verbatim shape, owner parameterized):
```rust
/// Seed one dialable peer: a reachability record under a distinct owner carrying `node_id`.
fn seed_peer(resolver: &ReachabilityResolver, owner: [u8; 16], node_id: [u8; 32]) {
    resolver.update(
        OwnerAddr(owner),
        ReachabilityAnnouncePayload {
            iroh_node_id: node_id,
            home_relay_url: String::new(),
            direct_addresses: vec![],
            announced_at_ms: 1,
            identity_signature: [0u8; 64],
            butler_set: vec![],
            bs_at: 0,
        },
        Hlc { wall_ms: 1, logical: 0, device_id: String::new() },
    );
}
```

- [ ] **Step 2: Write the test, Phase 1 with a deliberately wrong expectation**

Add to `mod tests`:
```rust
/// ZEB-929 (R4 validation): the live controller→oracle→supervisor pipeline, driven over a
/// 100-device community, dials EXACTLY the engine's ring-neighbor selection (degree 12) and
/// parks every non-neighbor Dormant — the bound "proven in practice, not just unit tests."
/// Then: an admission delta recovers a parked peer, and a revoked peer whose conn drops is
/// NOT re-dialed (the dispatch-point revocation guard, at scale).
#[tokio::test(start_paused = true)]
async fn r4_bounded_degree_partition_and_delta_at_scale() {
    use crate::admission_oracle::{compute_admitted, AdmissionOracle};
    use crate::community_topology::community_neighbors;
    use std::collections::BTreeSet;

    const N: usize = 100; // self + 99 peers; degree 12 (50 not a power of two → no antipode)
    let salt = b"zeb929-scale".to_vec();
    let device_keys: Vec<[u8; 32]> = (0..N)
        .map(|i| harmony_crypto::hash::blake3_hash(&(i as u64).to_be_bytes()))
        .collect();
    let self_vk = device_keys[0];
    let devices: BTreeSet<[u8; 32]> = device_keys.iter().copied().collect();

    let dialer = RecordingDialer::succeeding();
    let resolver = Arc::new(ReachabilityResolver::new());
    let telemetry = Arc::new(DialTelemetry::new());
    let oracle = Arc::new(AdmissionOracle::new(true));

    // 99 peers: peer i → node_id peer(i), owner [i;16], enrolled key device_keys[i].
    let node_ids: Vec<[u8; 32]> = (1..N)
        .map(|i| {
            let node_id = peer(i as u8);
            let owner = [i as u8; 16];
            seed_peer(&resolver, owner, node_id);
            oracle.bind(owner, node_id, device_keys[i]);
            node_id
        })
        .collect();

    // Publish the realized admitted device-key set = the engine's neighbor union.
    let neighbors = community_neighbors(&devices, &self_vk, &salt);
    oracle.publish_admitted(compute_admitted(&[(devices.clone(), salt.clone())], &self_vk));

    // Expected live dial set: peers whose enrolled key is a chosen neighbor.
    let expected_connected: BTreeSet<[u8; 32]> = (1..N)
        .filter(|&i| neighbors.contains(&device_keys[i]))
        .map(|i| peer(i as u8))
        .collect();

    let handle = SupervisorHandle::new();
    handle.set_admission_oracle(Arc::clone(&oracle));
    let config = cfg(1_000, 64_000, 10_000, 1_000, 16, 3_000);
    tokio::spawn(run_reconnect_supervisor(
        handle.clone(), dialer.clone(), resolver.clone(), telemetry.clone(), peer(0), config,
    ));

    for &n in &node_ids {
        handle.kick(n, ReconnectTrigger::NewPeer);
    }
    tokio::time::sleep(ms(60_000)).await;

    let snap = handle.states_snapshot();
    let connected: BTreeSet<[u8; 32]> = snap.iter()
        .filter(|(_, st)| matches!(st, PeerStateWire::Connected { .. }))
        .map(|(id, _)| *id)
        .collect();
    // WRONG on purpose (all-99) to confirm the partition assertion discriminates.
    let all: BTreeSet<[u8; 32]> = node_ids.iter().copied().collect();
    assert_eq!(connected, all, "WRONG on purpose");
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(r4_bounded_degree_partition_and_delta_at_scale)'`
Expected: FAIL — `connected` (12 peers) ≠ `all` (99). Confirms only the neighbors connect.

- [ ] **Step 4: Replace the wrong assertion with the real partition + degree check**

Replace the two `WRONG on purpose` lines with:
```rust
    assert_eq!(neighbors.len(), 12, "N=100 bounded degree is 12");
    assert_eq!(
        connected, expected_connected,
        "live dial set must equal the engine's ring-neighbor selection"
    );
    for &n in &node_ids {
        if expected_connected.contains(&n) {
            assert!(dialer.count_for(n) >= 1, "a neighbor must dial");
        } else {
            assert_eq!(dialer.count_for(n), 0, "a non-neighbor never dials (parked at dispatch)");
            assert!(
                snap.iter().any(|(id, st)| *id == n && matches!(st, PeerStateWire::Dormant { .. })),
                "a non-neighbor sits Dormant"
            );
        }
    }
```

- [ ] **Step 5: Run it to verify Phase 1 passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(r4_bounded_degree_partition_and_delta_at_scale)'`
Expected: PASS.

- [ ] **Step 6: Add Phase 2 (recovery) and Phase 3 (revocation) at the end of the test**

Append before the closing brace:
```rust
    // Phase 2 — admission delta recovers a parked peer. Pick a Dormant non-neighbor,
    // admit its key, sweep: it must dial and connect.
    let recover_i = (1..N).find(|&i| !expected_connected.contains(&peer(i as u8))).unwrap();
    let recover_id = peer(recover_i as u8);
    let mut admitted2 = compute_admitted(&[(devices.clone(), salt.clone())], &self_vk);
    admitted2.insert(device_keys[recover_i]);
    oracle.publish_admitted(admitted2.clone());
    handle.kick_sweep();
    tokio::time::sleep(ms(30_000)).await;
    assert!(
        handle.states_snapshot().iter()
            .any(|(id, st)| *id == recover_id && matches!(st, PeerStateWire::Connected { .. })),
        "a newly-admitted parked peer recovers to Connected after the sweep"
    );

    // Phase 3 — revocation guard at scale. Revoke a currently-Connected neighbor, then drop it:
    // the re-dial must be denied at dispatch (parked), not slipped through the arming path.
    let revoke_id = *expected_connected.iter().next().unwrap();
    let revoke_i = (1..N).find(|&i| peer(i as u8) == revoke_id).unwrap();
    let calls_before = dialer.count_for(revoke_id);
    admitted2.remove(&device_keys[revoke_i]);
    oracle.publish_admitted(admitted2);
    handle.kick(revoke_id, ReconnectTrigger::Dropped); // simulate its connection dropping
    tokio::time::sleep(ms(30_000)).await;
    assert_eq!(
        dialer.count_for(revoke_id), calls_before,
        "a revoked peer whose conn drops is NOT re-dialed (denied at dispatch)"
    );
    assert!(
        handle.states_snapshot().iter()
            .any(|(id, st)| *id == revoke_id && matches!(st, PeerStateWire::Dormant { .. })),
        "the revoked, dropped peer is parked Dormant"
    );
```

- [ ] **Step 7: Run the full test to verify all three phases pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(r4_bounded_degree_partition_and_delta_at_scale)'`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/reconnect_supervisor.rs
git commit -m "ZEB-929: prove live pipeline yields the bounded dial set at scale (claim D)"
```

---

### Task 3: V1 — extend the scale probe with the engine's R4 graph, run the sweep *(out-of-tree)*

**Files:**
- Modify: `~/work/zeb912-scale-probe/src/main.rs` (out-of-tree; if the directory is absent, reconstruct `Cargo.toml` + `src/main.rs` from the appendix of `docs/research/2026-08-13-zeb912-r3-scale-sounding.md`, then apply the edits below).

**Interfaces:**
- Consumes: the probe's `Topo` enum, `connects_for`, `churn_once`, `boot_convergence_ms`, `reconverge_ms`, `node_tx`, the `main` sweep.
- Produces: three markdown tables (ring + R4) on stdout, captured to `~/work/zeb912-scale-probe/sweep-r4.md`.

This is a measurement run, not a unit test; its "assertion" is the built-in degree self-check plus the empirical tables. No TDD red/green — the check is that the sweep completes and the degree column matches the engine's law.

- [ ] **Step 1: Add the exact engine offset math + R4 neighbor selector**

Add near `connects_for`:
```rust
/// Mirror of community_topology::ring_offsets — powers of two {1,2,4,…,≤ n/2}.
fn ring_offsets(n: usize) -> Vec<usize> {
    let max_off = n / 2;
    if max_off == 0 { return vec![]; }
    let (mut offs, mut o) = (vec![], 1usize);
    loop { offs.push(o); match o.checked_mul(2) { Some(x) if x <= max_off => o = x, _ => break } }
    offs
}

/// R4 neighbors of ring-rank `i` on a size-n ring (index-as-rank: valid because the graph is a
/// vertex-transitive circulant — the engine's salted-hash sort only relabels vertices). Below
/// FULL_MESH_THRESHOLD=32, full mesh — mirrors neighbors_on_ring. The BTreeSet collapses the
/// antipodal offset when n/2 is a power of two, matching the engine's degree exactly.
fn r4_neighbors(i: usize, n: usize) -> Vec<usize> {
    if n < 32 { return (0..n).filter(|&j| j != i).collect(); }
    let mut s = std::collections::BTreeSet::new();
    for o in ring_offsets(n) { s.insert((i + o) % n); s.insert((i + n - o) % n); }
    s.remove(&i);
    s.into_iter().collect()
}
```

- [ ] **Step 2: Add the `R4` topology variant**

Extend `enum Topo` with `R4` and `name()`:
```rust
enum Topo { Mesh, Ring, Line, R4 }
// in name():  Topo::R4 => "r4",
```
Add the `R4` arm to `connects_for` (dial only lower indices → each undirected edge dialed once):
```rust
Topo::R4 => r4_neighbors(i, n).into_iter().filter(|&j| j < i).map(|j| base + j as u16).collect(),
```
Add the `R4` arm to `churn_once`'s `connect` (the joiner at rank n on the grown size-(n+1) ring; all its neighbors are existing lower ranks):
```rust
Topo::R4 => r4_neighbors(n, n + 1).into_iter().map(|j| base + j as u16).collect(),
```

- [ ] **Step 3: Add a `degree` column to the sweep table**

In `main`, compute a nominal degree per topology and add it to the header + row:
```rust
// header: add "| degree " before "| boot_ms"
let degree = match topo {
    Topo::Mesh => n.saturating_sub(1),
    Topo::Ring => 2,
    Topo::Line => 1,
    Topo::R4   => r4_neighbors(n / 2, n).len(),
};
// include `degree` as the first data cell in the row's println!
```

- [ ] **Step 4: Set the sweep to ring + R4 (same host, same run, apples-to-apples)**

```rust
let topos = [Topo::Ring, Topo::R4];
let sizes = [32usize, 50, 100, 200];
```

- [ ] **Step 5: Build and run the sweep, capturing output**

Run:
```bash
cd ~/work/zeb912-scale-probe && cargo run --release > sweep-r4.md 2> sweep-r4.log
```
Expected: two tables (ring, r4). For R4: `degree` column reads 9 (N=32), 10 (N=50), 12 (N=100), 14 (N=200); `join_reconv_ms` at N=200 is sub-second (the headline vs ring's ~4.6 s); `join_KB` linear in N. If R4 reconv at N=200 is NOT sub-second, that is a reportable finding (record it, do not massage).

- [ ] **Step 6: Sanity-check the degree column against the engine**

Confirm the R4 `degree` values match the antipode-aware law (9/10/12/14). If any differ, the probe's `r4_neighbors` diverged from `community_topology.rs` — stop and reconcile before writing the doc (the whole comparison rests on graph fidelity). No commit (out-of-tree).

---

### Task 4: Findings doc + open the PR

**Files:**
- Create: `docs/research/2026-08-13-zeb929-r4-scale-validation.md`

**Interfaces:**
- Consumes: the `sweep-r4.md` numbers (Task 3); the R3 ring baseline from `docs/research/2026-08-13-zeb912-r3-scale-sounding.md`.
- Produces: the ticket's empirical deliverable + the go/no-go on Parts 2–3.

- [ ] **Step 1: Write the findings doc**

Include: (a) the R4 sweep table beside the R3 ring baseline; (b) the **reconvergence comparison** (ring ~4.6 s → R4 @ N=200) as the headline, with the diameter rationale (~log₂N hops); (c) degree confirmation (9/10/12/14, antipode-aware); (d) delivery survival + hop-latency; (e) idle CPU/RSS; (f) the threats to validity from the spec (probe ≠ real stack, index-as-rank, static churn model, single host); (g) an appendix with the full R4-topology probe source (reconstructable, per R3); (h) a go/no-go recommendation on Parts 2–3 including whether boot over-dial is worth quantifying.

- [ ] **Step 2: Verify the doc's numbers match the captured sweep**

Cross-check every cited number against `sweep-r4.md`. No invented figures; if a run was partial or hit a host ceiling, say so explicitly (never a silent cap).

- [ ] **Step 3: Run the full pre-PR gate**

Run (from `src-tauri/`):
```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: fmt clean, clippy clean, all tests pass (including the two new ones). Ensure the working tree is clean (committed) before declaring green.

- [ ] **Step 4: Commit the doc and open the PR**

```bash
git add docs/research/2026-08-13-zeb929-r4-scale-validation.md
git commit -m "ZEB-929: R4 scale-validation findings (ring 4.6s → R4 sub-second reconvergence)"
git push -u origin zeblith/zeb-929-r4-harness-validation
gh pr create --repo zeblithic/harmony-client --title "ZEB-929 Part 1: R4 bounded-degree scale validation" --body "<summary + the reconvergence comparison table + link to spec/findings>"
```
Then fire exactly one `@coderabbitai review` (per the review protocol; no further `@`), let Greptile/CodeAnt auto-review, and converge.

---

## Self-Review

**1. Spec coverage.** Claim A (delivery survives) → Task 3 (probe delivery/hop-latency). Claim B (sub-second reconvergence vs ring 4.6 s) → Task 3 (`reconverge_ms` R4 vs ring) + Task 4 (headline). Claim C (degree ≈14 @ N=200) → Task 1 (compute_admitted N=200) + Task 3 (degree column). Claim D (live pipeline produces the graph) → Task 2 (partition + delta at scale). Deliverables: findings doc → Task 4; V2 tests → Tasks 1–2; go/no-go on Parts 2–3 → Task 4 Step 1(h). Non-goals honored (no real fleet, no `FULL_MESH_THRESHOLD` change). **No gaps.**

**2. Placeholder scan.** All test bodies, probe edits, and commands are concrete; no TBD/"handle edge cases"/"similar to". The PR body is the one intentional fill-in (needs the real numbers), flagged as `<summary…>`.

**3. Type consistency.** `compute_admitted(&[(BTreeSet<[u8;32]>, Vec<u8>)], &[u8;32])`, `community_neighbors(&BTreeSet, &[u8;32], &[u8])`, `oracle.bind([u8;16],[u8;32],[u8;32])`, `publish_admitted(BTreeSet<[u8;32]>)`, `states_snapshot() -> Vec<([u8;32], PeerStateWire)>`, `RecordingDialer::succeeding()`/`count_for`, `cfg(6×)`, `peer(u8)`, `seed_peer(&ReachabilityResolver,[u8;16],[u8;32])` — all match the read source. Degree literals (14 @ N=200, 12 @ N=100, 9 @ N=32) are antipode-correct. `synth` used only in Task 1 (its home module); Task 2 inlines the blake3 generation (the supervisor test module has no `synth`).
