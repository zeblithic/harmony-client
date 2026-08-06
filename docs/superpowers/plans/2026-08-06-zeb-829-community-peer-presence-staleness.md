# ZEB-829 — Per-community peer-presence staleness gating: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `derive_sync_staleness`'s "never received" proxy with a real per-community reachable-peer signal, so a `communitySync` row goes `null` because there is genuinely no co-member to sync with — not merely because nothing has arrived yet.

**Architecture:** A pure fold (`reachable_peers_by_community`) over the `peers: Vec<PeerHealth>` already built in `NetworkHealthService::snapshot()` produces per-community reachable-co-member counts (keyed by full lowercase hex `SpaceId`). The count is threaded into `community_sync_row` → `derive_sync_staleness`, which adopts the **Option B** rule: `null` iff `reachable_peers == 0 || last_inbound_ms.is_none()`; else tier from `last_advance_ms`. The count is also surfaced as `reachablePeers` on `CommunitySyncHealth`. No engine change, no new service field, no identity translation.

**Tech Stack:** Rust (Tauri backend), serde camelCase DTOs, TypeScript mirror, `cargo nextest` / `vitest`.

## Global Constraints

- Rust gates run from `src-tauri/`: `cargo fmt --all -- --check`; `cargo clippy --locked --lib --bins --no-deps -- -D warnings` (shipping); `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- Frontend gates run from repo root: `npx tsc --noEmit`; `npx vitest run`.
- New wire fields are additive and `#[serde(default)]`; DTOs are `#[serde(rename_all = "camelCase")]`.
- `--all-targets` and `--locked` are load-bearing (CLAUDE.md).
- Each task ends green (compiles + its tests pass) so task boundaries are reviewable. Commit at the end of each task; commit trailer: `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D`.
- Iterative gating may use `scripts/test-select --context task`; the final pre-PR sweep is the full CI-parity commands above.

---

## File Structure

- `src-tauri/src/network_health.rs` — the whole Rust change: new `reachable_peers_by_community` helper; `reachable_peers` param on `community_sync_row` and `derive_sync_staleness`; `reachable_peers` field on `CommunitySyncHealth`; `snapshot()` wiring; doc rewrite; unit + serde + e2e tests.
- `src-tauri/src/community_state_sync.rs` — one existing test call-site update (`publish_retry_backoff_surfaces_in_community_sync_row_zeb762`).
- `src/lib/types/network-health.ts` — `reachablePeers?: number` on the `CommunitySyncHealth` interface.

---

## Task 1: `reachable_peers_by_community` helper + unit test

**Files:**
- Modify: `src-tauri/src/network_health.rs` (add helper near `filter_peers_by_shared_membership`, ~line 2183; add test in the `mod tests` block)

**Interfaces:**
- Produces: `pub(crate) fn reachable_peers_by_community(peers: &[PeerHealth]) -> std::collections::BTreeMap<String, u32>` — used by `snapshot()` in Task 2.

- [ ] **Step 1: Write the failing test** (in `mod tests`, `network_health.rs`)

```rust
// Local fixture: PeerHealth has no Default derive; the helper only reads
// connection_mode + shared_communities, so fill the rest with None.
fn peer_fixture(mode: ConnectionMode, communities: Vec<String>) -> PeerHealth {
    PeerHealth {
        owner_addr: String::new(),
        display_name: None,
        shared_communities: communities,
        connection_mode: mode,
        rtt_ms: None,
        last_seen_ms: None,
        reachability_record_age_ms: None,
        protocol_incompat_reason: None,
        last_traffic_ms: None,
        last_relay_pull_served_ms: None,
        connected_since_ms: None,
        staleness: None,
    }
}

#[test]
fn reachable_peers_by_community_counts_live_comembers_only() {
    let a = hex::encode([0xAAu8; 16]);
    let b = hex::encode([0xBBu8; 16]);
    let peers = vec![
        peer_fixture(ConnectionMode::Direct, vec![a.clone(), b.clone()]),
        peer_fixture(ConnectionMode::Relay, vec![a.clone()]),
        peer_fixture(ConnectionMode::Degraded, vec![b.clone()]),
        peer_fixture(ConnectionMode::NoConnection, vec![a.clone(), b.clone()]), // excluded
    ];
    let counts = reachable_peers_by_community(&peers);
    assert_eq!(counts.get(&a), Some(&2)); // Direct + Relay; NoConnection excluded
    assert_eq!(counts.get(&b), Some(&2)); // Direct + Degraded; NoConnection excluded
    // A community with only a NoConnection peer never appears.
    let counts2 = reachable_peers_by_community(&[peer_fixture(
        ConnectionMode::NoConnection,
        vec![a.clone()],
    )]);
    assert_eq!(counts2.get(&a), None);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(reachable_peers_by_community_counts_live_comembers_only)'`
Expected: FAIL to compile — `reachable_peers_by_community` not defined.

- [ ] **Step 3: Write the helper** (place after `filter_peers_by_shared_membership`, ~line 2183)

```rust
/// ZEB-829: count reachable co-member peers per community, keyed by full
/// lowercase hex `SpaceId` — the exact key `PeerHealth::shared_communities`
/// carries (`communities_shared_with` emits it via `hex::encode`), so the
/// snapshot's `hex::encode(community_id.0)` lookup matches byte-for-byte.
/// "Reachable" = any live `ConnectionMode`; `NoConnection` does not count.
///
/// A pure fold over the `peers` vec `snapshot` has already built: it needs no
/// membership handle and no identity translation, because the
/// membership∩reachability join is already baked into `shared_communities`.
/// This is also the per-community signal ZEB-803's acceptor watchdog should
/// adopt in place of its current global `count_peer_states().connected`.
pub(crate) fn reachable_peers_by_community(
    peers: &[PeerHealth],
) -> std::collections::BTreeMap<String, u32> {
    let mut counts: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for p in peers {
        if p.connection_mode == ConnectionMode::NoConnection {
            continue;
        }
        for community_hex in &p.shared_communities {
            *counts.entry(community_hex.clone()).or_insert(0) += 1;
        }
    }
    counts
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(reachable_peers_by_community_counts_live_comembers_only)'`
Expected: PASS. Also run `cargo clippy --locked --lib --no-deps --features test-fixtures -- -D warnings` to confirm no unused-import / lint issues from the new helper.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/network_health.rs
git commit  # feat(zeb-829): add reachable_peers_by_community helper (+ trailer)
```

---

## Task 2: Surface `reachablePeers` telemetry (count wiring, behaviour unchanged)

Adds the `reachable_peers` count to the DTO and threads it from `snapshot()` into `community_sync_row`, **without** yet changing the staleness rule. This isolates the additive telemetry (and proves the hex-key parity end-to-end) from the behaviour change in Task 3; every existing staleness assertion must still pass unchanged here.

**Files:**
- Modify: `src-tauri/src/network_health.rs` — `CommunitySyncHealth` struct; `community_sync_row` signature + field population; `snapshot()` wiring; `community_sync_row_serde_is_camel_case`; the three `community_sync_row`-based tier tests; new field/e2e tests.
- Modify: `src-tauri/src/community_state_sync.rs` — `publish_retry_backoff_surfaces_in_community_sync_row_zeb762` call-site.

**Interfaces:**
- Consumes: `reachable_peers_by_community` (Task 1).
- Produces: `community_sync_row(id, raw, now_ms, reachable_peers: u32)`; `CommunitySyncHealth { reachable_peers: u32, .. }`.

- [ ] **Step 1: Add the DTO field** (`CommunitySyncHealth`, after `publish_retry`, ~line 165)

```rust
    /// ZEB-829: count of reachable co-member peers for this community at snapshot
    /// assembly (any live `ConnectionMode`; `NoConnection` excluded). Makes the
    /// `staleness == null` decision legible — "zero peers to sync with" vs "no
    /// data yet" — and is the signal ZEB-803's acceptor watchdog will adopt.
    /// `#[serde(default)]` keeps a pre-field cached snapshot forward-compatible.
    #[serde(default)]
    pub reachable_peers: u32,
```

- [ ] **Step 2: Add the param to `community_sync_row` and populate the field** (do NOT touch the `derive_sync_staleness` call yet — still 3-arg)

Signature (~line 2053):
```rust
pub fn community_sync_row(
    community_id: crate::owner_state_types::SpaceId,
    raw: CommunitySyncRaw,
    now_ms: u64,
    reachable_peers: u32,
) -> CommunitySyncHealth {
```
In the returned struct literal, add `reachable_peers,` (the `staleness:` line stays `derive_sync_staleness(last_inbound_ms, last_advance_ms, now_ms)` for now).

- [ ] **Step 3: Wire `snapshot()`** — compute counts once after `peers` is built (insert near line 2903, before `peers` is moved into the snapshot at ~2916), and pass the per-community count at the `community_sync` arm (~3023).

After the `let peers = filter_peers_by_shared_membership(...)` block (line 2902):
```rust
        let reachable_by_community = reachable_peers_by_community(&peers);
```
At the `community_sync` arm (replace the `.map` closure at 3023):
```rust
                    .map(|(id, raw)| {
                        let reachable_peers = reachable_by_community
                            .get(&hex::encode(id.0))
                            .copied()
                            .unwrap_or(0);
                        community_sync_row(id, raw, now, reachable_peers)
                    })
```

- [ ] **Step 4: Update existing call-sites so the tree compiles** — add the new arg. In each of these tests, pass a value that preserves the current assertion:
  - `network_health.rs` `receiving_and_discarding_renders_dark_not_fresh` (~6419): pass `1` (scenario expects `Dark`).
  - `network_health.rs` `arrivals_with_no_merge_ever_render_dark` (~6452): pass `1` (expects `Dark`).
  - `network_health.rs` `community_with_no_inbound_ever_has_no_tier` (~6474): pass `0` (default = no peers; expects `None`).
  - `network_health.rs` `community_sync_row_serde_is_camel_case` (~6499): pass `3` (a distinctive non-zero for the serde assertions below).
  - `community_state_sync.rs` `publish_retry_backoff_surfaces_in_community_sync_row_zeb762` (~8205/8211): pass `1` (publish-retry assertions are unaffected by peer count).

- [ ] **Step 5: Extend the serde test** (`community_sync_row_serde_is_camel_case`) — add `reachablePeers` to the expected camelCase key list and assert `"reachablePeers":3` is present; add `reachable_peers` to the no-snake-leak list. Add a sibling `#[serde(default)]` absence test:

```rust
#[test]
fn community_sync_health_tolerates_absent_reachable_peers() {
    // JSON from before the field existed deserializes with reachable_peers == 0.
    let json = r#"{"communityShort":"aabbccdd","lastInboundMs":null,"lastAdvanceMs":null,
        "staleness":null,"fetchRetriesScheduled":0,"fetchRetriesDropped":0,
        "fetchRetriesExhausted":0}"#;
    let h: CommunitySyncHealth = serde_json::from_str(json).unwrap();
    assert_eq!(h.reachable_peers, 0);
}
```
(Match the exact key spelling/style already used in the neighbouring `..tolerates_absent_publish_retry` test.)

- [ ] **Step 6: Add the e2e count/parity test** through `snapshot()` — extend the module's snapshot test (near `snapshot_community_sync_section_present_iff_source_installed`, ~5217). Seed a `FakeResolver` with one `ResolverPeerRecord` (a chosen `owner_addr`, a live `connection_mode` e.g. `Direct`) and a membership fake where that `owner_addr` shares the community `SpaceId` the `FakeCommunitySync` reports; assert the resulting row's `reachable_peers == 1`. Construct the fakes following the existing pattern at ~5233 (`FakeResolver { records: ... }`, membership fake). This proves compute → `hex::encode(id.0)` lookup → key-parity → field, end-to-end.

- [ ] **Step 7: Run the scoped tests + gate**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_sync) + test(reachable_peers) + test(publish_retry_backoff_surfaces_in_community_sync_row_zeb762)'`
Expected: PASS (all existing staleness assertions unchanged; new field asserted). Then `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit  # feat(zeb-829): surface reachablePeers on communitySync rows (telemetry, no behaviour change) (+ trailer)
```

---

## Task 3: Adopt the Option B staleness rule

Flips `derive_sync_staleness` to gate on `reachable_peers`, isolating the behaviour change with its own semantic tests.

**Files:**
- Modify: `src-tauri/src/network_health.rs` — `derive_sync_staleness` signature + rule + doc; the `community_sync_row` call to it; the direct-call test; new semantic + e2e tests.

**Interfaces:**
- Consumes: `community_sync_row`'s `reachable_peers` (Task 2).
- Produces: `derive_sync_staleness(last_inbound_ms, last_advance_ms, reachable_peers: u32, now_ms) -> Option<PeerStaleness>`.

- [ ] **Step 1: Write the failing semantic tests** (in `mod tests`) — these encode Option B:

```rust
#[test]
fn zeb829_lost_all_peers_after_traffic_renders_null_not_dark() {
    let now = 1_000_000_000u64;
    let old = now - (STALENESS_DARK_MS + 60_000); // would be Dark with peers
    // Had traffic, advanced long ago, but zero reachable peers now → null.
    assert_eq!(derive_sync_staleness(Some(now - 10_000), Some(old), 0, now), None);
    // Same, but a reachable peer exists → the wedge is real → Dark.
    assert_eq!(
        derive_sync_staleness(Some(now - 10_000), Some(old), 1, now),
        Some(PeerStaleness::Dark)
    );
}

#[test]
fn zeb829_fresh_join_with_peers_but_no_inbound_stays_null() {
    let now = 1_000_000_000u64;
    // Peers present, but nothing has ever arrived → no evidence of a wedge → null
    // (the inbound guard; avoids the imprecision-2 false alarm).
    assert_eq!(derive_sync_staleness(None, None, 3, now), None);
}

#[test]
fn zeb829_arrivals_without_merge_with_peers_still_dark() {
    let now = 1_000_000_000u64;
    // The ZEB-805 shape is preserved: inbound present, never merged, peers live.
    assert_eq!(
        derive_sync_staleness(Some(now - 1_000), None, 1, now),
        Some(PeerStaleness::Dark)
    );
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zeb829)'`
Expected: FAIL to compile — `derive_sync_staleness` takes 3 args, tests pass 4.

- [ ] **Step 3: Change the signature + rule + doc** (`derive_sync_staleness`, ~2032)

```rust
pub fn derive_sync_staleness(
    last_inbound_ms: Option<u64>,
    last_advance_ms: Option<u64>,
    reachable_peers: u32,
    now_ms: u64,
) -> Option<PeerStaleness> {
    // ZEB-829: no reachable co-member to sync with, or nothing has ever
    // arrived → no positive evidence of a wedge to report.
    if reachable_peers == 0 || last_inbound_ms.is_none() {
        return None;
    }
    let Some(advance_ms) = last_advance_ms else {
        return Some(PeerStaleness::Dark);
    };
    let age = now_ms.saturating_sub(advance_ms);
    Some(if age < STALENESS_QUIET_MS {
        PeerStaleness::Fresh
    } else if age <= STALENESS_DARK_MS {
        PeerStaleness::Quiet
    } else {
        PeerStaleness::Dark
    })
}
```
Rewrite the doc comment (1996-2031): keep the `last_advance_ms`-keyed rationale and the global-shortcut warning, but replace the "PROXY / two imprecisions / ZEB-829 is the real fix" paragraphs (2014-2031) with the implemented rule — `null` iff `reachable_peers == 0 || last_inbound_ms.is_none()`; why the inbound guard keeps a peers-present-but-never-received fresh join quiet rather than `Dark` (imprecision 2 avoided); and that this fixed imprecision 1 (a community that lost every peer now reads `null`, not a false `Dark`).

- [ ] **Step 4: Pass the count through `community_sync_row`** — change its `staleness:` line (~2069) to:
```rust
        staleness: derive_sync_staleness(last_inbound_ms, last_advance_ms, reachable_peers, now_ms),
```

- [ ] **Step 5: Update the direct-call test** `sync_tier_boundaries_track_the_advance_stamp` (~6485) — add a reachable-peers arg (`1`) to each `derive_sync_staleness(Some(now-1), Some(now-age), 1, now)` call (scenario expects real tiers, so peers must be non-zero). Also extend `community_with_no_inbound_ever_has_no_tier` (~6474) with an assertion that `reachable_peers > 0` with `inbound == None` still yields `None` (the fresh-join guard), if not already covered by the new `zeb829_fresh_join...` test.

- [ ] **Step 6: Extend the e2e test for staleness gating** — building on Task 2's seeded snapshot test: with a reachable co-member seeded and `last_inbound_ms`/`last_advance_ms` set to the wedge shape, assert the row's `staleness == Some(Dark)`; with zero reachable peers (no seeded record) but the same wedge stamps, assert `staleness == None`. This proves the rule through the real assembly path.

- [ ] **Step 7: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zeb829) + test(sync_tier_boundaries) + test(community_sync) + test(no_inbound_ever)'`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit  # feat(zeb-829): gate community staleness on reachable-peer presence (Option B) (+ trailer)
```

---

## Task 4: TypeScript mirror

**Files:**
- Modify: `src/lib/types/network-health.ts` — `CommunitySyncHealth` interface (~line 257-283).

- [ ] **Step 1: Add the field** — next to `staleness?: PeerStaleness | null` on the `CommunitySyncHealth` interface:

```typescript
  /** ZEB-829: reachable co-member peers for this community at snapshot assembly;
   *  makes the `staleness === null` decision legible. Additive; absent on
   *  pre-field snapshots. */
  reachablePeers?: number;
```

- [ ] **Step 2: Type-check**

Run (from repo root): `npx tsc --noEmit`
Expected: exit 0.

- [ ] **Step 3: Commit**

```bash
git add src/lib/types/network-health.ts
git commit  # feat(zeb-829): mirror reachablePeers on the CommunitySyncHealth TS type (+ trailer)
```

---

## Finishing (post-plan, per autonomous flow)

After Task 4: run the **full CI-parity gate** (all Global-Constraints commands, not the scoped subsets) from a clean tree; confirm no base drift (`git fetch origin main`); open the PR against `main`; fire `@coderabbitai review` exactly once; converge Qodo + CodeAnt + CodeRabbit findings (bundle → fix → full gate → push once); report merge-ready. **Never auto-merge.**

## Self-Review (author checklist — completed)

- **Spec coverage:** helper (§signal) → Task 1; Option B rule (§chosen semantics) → Task 3; `reachablePeers` surface (§threading 4) → Task 2 + Task 4; snapshot wiring (§threading 1-2) → Task 2; hex-parity guard (§implementation verification) → Task 2 Step 6 e2e + confirmed against source (both sides `hex::encode(x.0)`); tests (§testing) → distributed across Tasks 1-3; non-goals (no engine/`CommunitySyncRaw` change, no ZEB-803 rewire, no panel) → respected (only `CommunitySyncHealth` gains a field). ✓
- **Placeholder scan:** no TBD/TODO; all code steps carry real code except the e2e fake-construction (Task 2 Step 6 / Task 3 Step 6), which is described precisely and finalized against the existing `FakeResolver`/`FakeCommunitySync` at execution — an intentional adaptation, not a gap. ✓
- **Type consistency:** `reachable_peers: u32` (Rust) ↔ `reachablePeers?: number` (TS); `community_sync_row(id, raw, now, reachable_peers)` and `derive_sync_staleness(last_inbound_ms, last_advance_ms, reachable_peers, now_ms)` argument orders are used identically at every call-site named above. ✓
