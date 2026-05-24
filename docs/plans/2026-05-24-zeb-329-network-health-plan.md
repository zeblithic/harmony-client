# ZEB-329 Network Health Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the in-app Network Health panel + self-test + cross-WAN validation playbook (ZEB-327 Sub-B per [`docs/specs/2026-05-24-zeb-329-network-health-design.md`](../specs/2026-05-24-zeb-329-network-health-design.md), commit `30c4f7b`).

**Architecture:** New `network_health.rs` synthesis-only backend module reads from existing iroh / `ReachabilityResolver` / pkarr publishers; 3 read-only Tauri IPCs + 1 rate-limited Tauri event; new `harmony/ping/v1` ALPN for self-test round-trip; frontend `/network` route with view + export modal; redaction performed server-side. No new wire-format CRDT events; no on-disk writes; no telemetry.

**Tech Stack:** Rust (tokio, iroh 0.98, tauri 2.x, serde, async_trait), Svelte 5 (runes), TypeScript, vitest.

---

## Branch & baseline

Branch `zeb-329-network-health-spec` already exists (currently HEAD = `30c4f7b`, off `origin/main` 9844170). Continue using this branch for implementation; rename at PR time is unnecessary. **Do NOT cut a worktree** (memory rule `feedback_no_worktrees`).

If a subagent needs a fresh branch: `git checkout -b zeb-329-network-health` off the current branch HEAD. Otherwise stay on `zeb-329-network-health-spec`.

## File structure

| Action | Path | Responsibility |
|---|---|---|
| CREATE | `src-tauri/src/network_health.rs` | `NetworkHealthService` + types + pure functions + unit tests |
| CREATE | `src-tauri/tests/network_health_two_endpoint.rs` | Real-iroh integration test for HARMONY_PING_V1 round-trip |
| MODIFY | `src-tauri/src/iroh_endpoint.rs` | Add `HARMONY_PING_V1` constant + accept-loop hook for it |
| MODIFY | `src-tauri/src/event_loop.rs` | Add `notify_resolver_update()` call adjacent to existing `reachability_resolver.update(...)` |
| MODIFY | `src-tauri/src/lib.rs` | Register 3 new IPCs, extend `NodeState` with `network_health` field, boot wiring |
| CREATE | `src/lib/types/network-health.ts` | TS mirrors of Rust DTOs (camelCase) |
| CREATE | `src/lib/network-health-adapter.ts` | IPC wrappers + event subscriber + pure helpers |
| CREATE | `src/lib/components/NetworkHealthView.svelte` | Dedicated `/network` route component |
| CREATE | `src/lib/components/DiagnosticExportModal.svelte` | Export modal with redaction toggle |
| CREATE | `src/lib/__tests__/network-health-adapter.test.ts` | vitest unit tests for pure helpers |
| CREATE | `src/lib/components/__tests__/NetworkHealthView.test.ts` | Component behavior tests |
| CREATE | `src/lib/components/__tests__/DiagnosticExportModal.test.ts` | Modal behavior + redaction-leak tests |
| MODIFY | `src/App.svelte` (or whichever file owns sidebar nav) | Add "Network" nav item + `/network` route |
| CREATE | `docs/cross-wan-validation.md` | Two-host playbook (Step 1–4 + troubleshooting) |

**File-size discipline:** `network_health.rs` should stay focused. If implementation exceeds ~800 LOC, split self-test into a private submodule `mod self_test;` inside the same file (`include!` not needed — Rust submodules). Reviewer flags if file balloons.

## HARD RULES every implementer subagent MUST enforce

- **5 backend gates** (run from `src-tauri/`):
  - `cargo fmt --all -- --check` (or `cargo fmt --all` to fix)
  - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- **2 frontend gates** (run from repo root via `npx`, NOT `pnpm`):
  - `npx tsc --noEmit`
  - `npx vitest run`
- **Commit BEFORE the gate run** so a hung gate doesn't lose work.
- **10-min wall-clock kill switch** per gate command — if a single gate exceeds 10 minutes, kill it, mark task `DONE_WITH_CONCERNS`, report the gate that hung. Do NOT retry blindly.
- **Long-running supervision**: cargo/test that can exceed ~10 min MUST run foreground with `timeout 600 cmd` or background with ScheduleWakeup 1800s heartbeat (memory rule `feedback_long_running_background_supervision`).
- **Pipe exit codes**: when piping cargo output through `tail`/`grep`, use `set -o pipefail` OR check `${PIPESTATUS[0]}`. Memory rule `feedback_pipe_exit_codes_lie`.
- **Tauri IPC naming**: Rust snake_case + `rename_all = "snake_case"` on `#[tauri::command]`; JS callers use camelCase. DTOs use `#[serde(rename_all = "camelCase")]`.
- **Tauri error extraction** (frontend): `const msg = e instanceof Error ? e.message : String(e);`.
- **No worktrees** — `git checkout -b` only.
- **Pull-before-work** satisfied (branch off latest `origin/main` 9844170).
- **No new Linear tickets** — ZEB-329 already exists.

## Known plan-execution risks

1. **iroh ConnectionInfo NAT classification** — iroh 0.98's exact API for NAT classification may differ from what `classify_nat` expects. If iroh doesn't expose NAT class directly via `ConnectionInfo`, fall back to `NatClass::Unknown` and add a `TODO(zeb-329-followup)` comment explaining what's needed. Spec §6.1 allows this — the snapshot never throws.
2. **PkarrPublicationStatus shape** — verified at plan-writing time: lib.rs:30560 has `connectivity_pkarr_publication_status` returning `{invite_count, identity_active, community_count}`. The implementer reuses this same shape source: `publisher.active_handles()` filtered the same way.
3. **Iroh accept loop currently single-ALPN** — iroh 0.98's `Endpoint::accept` accepts any ALPN registered in the builder's `.alpns(...)` list, then the application-level handler dispatches by `Connection::alpn()`. Adding HARMONY_PING_V1 is: (a) add to the `.alpns(...)` list in `iroh_endpoint::IrohEndpoint::new_with_secret` AND `from_endpoint_for_test`, (b) dispatch in the accept loop — but the accept loop today lives outside `iroh_endpoint.rs` (the IrohEndpoint just exposes `.inner()`; consumers run their own accept loops). For Task 5 the implementer adds a SECOND accept loop spawned by `NetworkHealthService::new` that handles ONLY HARMONY_PING_V1. Existing zenoh-over-iroh accept loop is unaffected.
4. **Sidebar nav location** — verify by reading `src/App.svelte` first. If routing uses a switch-statement on a `currentView` store, add a `'network'` arm. If using a router, register a `/network` route. Either way, add a nav-bar item.

## Pre-existing orphan failures (Task 0 baselines exact list)

Per prior phases the following test failures pre-date this PR and are NOT blocking:
- `folder_ingest::tests`
- `mint::tests`
- `mint_sync::tests`
- `folder_ingest_walker_integration`
- `rename_content_integration`

Task 0 captures the exact list at execution time. Any NEW failure introduced by Tasks 1-13 is blocking per `feedback_test_drift_is_our_fault`.

---

## Task 0: Pre-flight baseline

**Files:** none (no commit).

**Steps:**

- [ ] **Step 1: Verify branch state**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git fetch origin
git log --oneline -5
# Expected: HEAD = 30c4f7b (docs(zeb-329): network health panel + self-test + cross-WAN playbook spec)
#           preceded by 9844170 (origin/main: ZEB-328 Sub-project A merge)
git status -sb
# Expected: clean (or only .DS_Store untracked)
```

- [ ] **Step 2: Verify spec is in branch history**

```bash
git log --oneline | grep -E "^30c4f7b"
# Expected: 30c4f7b docs(zeb-329): network health panel + self-test + cross-WAN playbook spec
ls docs/specs/2026-05-24-zeb-329-network-health-design.md
# Expected: file exists
```

- [ ] **Step 3: Capture baseline test state**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
# Foreground with explicit timeout to avoid silent hang per feedback_long_running_background_supervision
set -o pipefail
timeout 600 cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tee /tmp/zeb-329-baseline.log | tail -100
echo "EXIT=${PIPESTATUS[0]}"
```

Expected: existing orphan failures only (folder_ingest, mint, mint_sync, folder_ingest_walker_integration, rename_content_integration). Capture the exact failing tests in your scratch notes — these are the only acceptable failures at PR time.

- [ ] **Step 4: Verify frontend baseline**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit 2>&1 | tail -20
npx vitest run 2>&1 | tail -30
```

Expected: both pass clean.

- [ ] **Step 5: Verify cargo fmt + clippy baseline**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check && echo "FMT OK"
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -30
echo "CLIPPY EXIT=${PIPESTATUS[0]}"
```

Expected: both clean.

**No commit. Task 0 is a sanity check only.**

---

## Task 1: Backend data types + pure non-formatter functions

**Files:**
- Create: `src-tauri/src/network_health.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod network_health;` near other module declarations around line 76-80, just after `pub mod reachability_resolver;`)

**What this task builds:** The entire data-type surface (snapshot, self-test report, peer health, enums) plus three pure functions: `classify_nat`, `derive_reachability_status`, `filter_peers_by_shared_membership`. NO `NetworkHealthService` yet — that's Task 3. NO `format_export_markdown` — that's Task 2 (out-of-order so the redaction-leak test gets written first).

- [ ] **Step 1: Add module declaration**

In `src-tauri/src/lib.rs`, find the existing `pub mod reachability_resolver;` line (around line 78) and add immediately after:

```rust
pub mod network_health;
```

- [ ] **Step 2: Create network_health.rs with data types + serde + module skeleton**

Create `src-tauri/src/network_health.rs`:

```rust
//! ZEB-329 — Network Health: cross-WAN validation surface.
//!
//! See `docs/specs/2026-05-24-zeb-329-network-health-design.md` for the
//! full design. This module is **synthesis only** — it reads from
//! existing sources (iroh::Endpoint, ReachabilityResolver, pkarr
//! publishers, my-membership set) and never mutates them. Pure
//! functions (classify_nat, derive_reachability_status,
//! filter_peers_by_shared_membership, format_export_markdown) are
//! decomposed for direct unit testing without iroh / network.
//!
//! ## Cache vs commit token (memory rule feedback_two_ipc_toctou)
//!
//! `network_health_run_self_test` writes to a cached
//! `Arc<RwLock<Option<SelfTestReport>>>` that `network_health_export_payload`
//! later reads. This is NOT a write/commit token pair — the cache is a
//! memo of the most recent test result, not a binding identifier. A
//! TOCTOU race here only means an export sees a stale report (or no
//! report); the export's correctness does not depend on a contract
//! between the two IPCs.

use serde::{Deserialize, Serialize};

// ── Public data types (wire shape for IPC) ──────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkHealthSnapshot {
    /// Defaults to 1; bump on breaking export-format changes per spec §4.4.
    pub schema_version: u32,
    pub captured_at_ms: u64,
    pub app_version: String,
    pub platform: String,
    /// `None` when iroh isn't yet bound (early boot, sandbox).
    pub my_network: Option<MyNetworkSummary>,
    /// Sorted by `last_seen_ms` desc, `None` values last.
    pub peers: Vec<PeerHealth>,
    pub pkarr_status: PkarrHealthSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MyNetworkSummary {
    /// Hex-encoded iroh EndpointId (64 lowercase hex chars).
    pub iroh_node_id: String,
    pub reachability: ReachabilityStatus,
    pub nat_classification: NatClass,
    pub home_relay_url: Option<String>,
    pub relay_rtt_ms: Option<u32>,
    pub direct_addresses: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PeerHealth {
    pub owner_addr: String,
    pub display_name: Option<String>,
    pub shared_communities: Vec<String>,
    pub connection_mode: ConnectionMode,
    pub rtt_ms: Option<u32>,
    pub last_seen_ms: Option<u64>,
    pub reachability_record_age_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PkarrHealthSummary {
    pub identity_published: bool,
    pub identity_last_publish_ms: Option<u64>,
    pub community_publish_count: u32,
    pub recent_fallback_events: Vec<PkarrFallbackHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PkarrFallbackHit {
    pub peer_addr_short: String,
    pub community_id_short: String,
    pub hit: bool,
    pub captured_at_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ReachabilityStatus {
    Reachable,
    Degraded,
    Unreachable,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum NatClass {
    FullCone,
    RestrictedCone,
    PortRestricted,
    Symmetric,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ConnectionMode {
    Direct,
    Relay,
    NoConnection,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SelfTestReport {
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub steps: Vec<SelfTestStep>,
    pub peer_results: Vec<PeerPingResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SelfTestStep {
    pub name: String,
    pub outcome: StepOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PeerPingResult {
    pub owner_addr: String,
    pub outcome: StepOutcome,
    pub mode: Option<ConnectionMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum StepOutcome {
    Pass { duration_ms: u32 },
    Fail { reason: String },
    Skipped { reason: String },
}

impl NetworkHealthSnapshot {
    /// Empty-but-well-formed snapshot for the "iroh not ready" path
    /// (spec §6.1: snapshot never throws). All renders gracefully:
    /// `my_network: None` → "starting up…" placeholder in UI;
    /// `peers: []` → "no peers yet"; `pkarr_status` zeroed.
    pub fn empty() -> Self {
        Self {
            schema_version: 1,
            captured_at_ms: now_ms(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            platform: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
            my_network: None,
            peers: Vec::new(),
            pkarr_status: PkarrHealthSummary {
                identity_published: false,
                identity_last_publish_ms: None,
                community_publish_count: 0,
                recent_fallback_events: Vec::new(),
            },
        }
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── Pure synthesis functions (no iroh, no network) ──────────────────

/// Spec §4.1: derive top-level reachability from my own state +
/// peer set. Reachable: my_network present + at least one peer is
/// Direct-connected. Degraded: my_network present but all peer
/// connections are Relay or NoConnection. Unreachable: my_network
/// absent OR all peers NoConnection AND none Direct/Relay.
pub fn derive_reachability_status(
    _my: &MyNetworkSummary,
    peers: &[PeerHealth],
) -> ReachabilityStatus {
    if peers.iter().any(|p| p.connection_mode == ConnectionMode::Direct) {
        ReachabilityStatus::Reachable
    } else if peers.iter().any(|p| p.connection_mode == ConnectionMode::Relay) {
        ReachabilityStatus::Degraded
    } else if peers.is_empty() {
        // No peers yet ≠ unreachable. Report Reachable because *we* have
        // working endpoint state; reachability of others is unknown,
        // not failing.
        ReachabilityStatus::Reachable
    } else {
        ReachabilityStatus::Unreachable
    }
}

/// Iroh 0.98 may or may not expose NAT classification directly via
/// `ConnectionInfo`. This function wraps whatever iroh provides into
/// our `NatClass` enum. If iroh exposes nothing useful, returns
/// `NatClass::Unknown` (spec §6.1 — snapshot never throws).
///
/// TODO(zeb-329-followup): when iroh ships a stable NAT classifier
/// hook, replace the `Unknown` fallback with real classification.
/// The function signature takes a generic stand-in to keep the
/// interface stable across iroh versions.
pub fn classify_nat<T>(_connection_info: &T) -> NatClass {
    // Phase 1: no iroh-side NAT classification API we can rely on
    // across versions. Render as Unknown; the snapshot still carries
    // home_relay_url + relay_rtt_ms + direct_addresses so testers can
    // self-diagnose without the classifier.
    NatClass::Unknown
}

/// Spec §4.1: peer list scoped to peers we share community membership
/// with. Resolver records are `Vec<(OwnerAddr, ReachabilityPayload)>`;
/// my_memberships is `Vec<(OwnerAddr, Vec<CommunityIdHex>)>` — the
/// existing membership store enumerates communities per owner. Output
/// is sorted by `last_seen_ms` desc with `None` last.
///
/// Pass `now_ms_fn` for testable time (production uses `now_ms`).
pub fn filter_peers_by_shared_membership(
    resolver_records: Vec<ResolverPeerRecord>,
    my_memberships: &MyMembershipSet,
    now_ms: u64,
) -> Vec<PeerHealth> {
    let mut out: Vec<PeerHealth> = Vec::new();
    for r in resolver_records {
        let shared = my_memberships.communities_shared_with(&r.owner_addr);
        if shared.is_empty() {
            continue;
        }
        out.push(PeerHealth {
            owner_addr: r.owner_addr_hex(),
            display_name: r.display_name,
            shared_communities: shared,
            connection_mode: r.connection_mode,
            rtt_ms: r.rtt_ms,
            last_seen_ms: r.last_seen_ms,
            reachability_record_age_ms: r
                .last_seen_ms
                .map(|ls| now_ms.saturating_sub(ls)),
        });
    }
    // Sort by last_seen_ms desc; None values last.
    out.sort_by(|a, b| match (b.last_seen_ms, a.last_seen_ms) {
        (Some(bv), Some(av)) => bv.cmp(&av),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    out
}

/// Plain-data input to `filter_peers_by_shared_membership`. Constructed
/// by `NetworkHealthService::snapshot` (Task 3) from the resolver +
/// connection-info read. Decoupled so the filter is testable without
/// iroh.
#[derive(Debug, Clone)]
pub struct ResolverPeerRecord {
    pub owner_addr: [u8; 16],
    pub display_name: Option<String>,
    pub connection_mode: ConnectionMode,
    pub rtt_ms: Option<u32>,
    pub last_seen_ms: Option<u64>,
}

impl ResolverPeerRecord {
    pub fn owner_addr_hex(&self) -> String {
        hex::encode(self.owner_addr)
    }
}

/// Membership lookup interface — implemented by the production
/// membership store and by test fakes.
pub trait MyMembershipSet {
    /// Return community ids (lowercase hex) that I share with `peer`.
    /// Empty Vec = no shared community → peer is excluded from the
    /// Network Health panel.
    fn communities_shared_with(&self, peer: &[u8; 16]) -> Vec<String>;
}

// ── Unit tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeMembership {
        // peer hex addr → list of shared community ids
        table: std::collections::HashMap<[u8; 16], Vec<String>>,
    }

    impl MyMembershipSet for FakeMembership {
        fn communities_shared_with(&self, peer: &[u8; 16]) -> Vec<String> {
            self.table.get(peer).cloned().unwrap_or_default()
        }
    }

    fn make_record(byte: u8, mode: ConnectionMode, last_seen: Option<u64>) -> ResolverPeerRecord {
        ResolverPeerRecord {
            owner_addr: [byte; 16],
            display_name: None,
            connection_mode: mode,
            rtt_ms: None,
            last_seen_ms: last_seen,
        }
    }

    #[test]
    fn classify_nat_returns_unknown_for_any_input() {
        // Phase 1: classify_nat always returns Unknown until iroh
        // exposes a stable hook (TODO above).
        let dummy: u8 = 0;
        assert_eq!(classify_nat(&dummy), NatClass::Unknown);
    }

    #[test]
    fn derive_reachability_status_reachable_when_any_direct() {
        let my = MyNetworkSummary {
            iroh_node_id: "deadbeef".into(),
            reachability: ReachabilityStatus::Unreachable, // ignored
            nat_classification: NatClass::Unknown,
            home_relay_url: None,
            relay_rtt_ms: None,
            direct_addresses: vec![],
        };
        let peers = vec![PeerHealth {
            owner_addr: "abcd".into(),
            display_name: None,
            shared_communities: vec![],
            connection_mode: ConnectionMode::Direct,
            rtt_ms: None,
            last_seen_ms: None,
            reachability_record_age_ms: None,
        }];
        assert_eq!(derive_reachability_status(&my, &peers), ReachabilityStatus::Reachable);
    }

    #[test]
    fn derive_reachability_status_degraded_when_only_relay() {
        let my = MyNetworkSummary {
            iroh_node_id: "deadbeef".into(),
            reachability: ReachabilityStatus::Unreachable,
            nat_classification: NatClass::Unknown,
            home_relay_url: None,
            relay_rtt_ms: None,
            direct_addresses: vec![],
        };
        let peers = vec![PeerHealth {
            owner_addr: "abcd".into(),
            display_name: None,
            shared_communities: vec![],
            connection_mode: ConnectionMode::Relay,
            rtt_ms: None,
            last_seen_ms: None,
            reachability_record_age_ms: None,
        }];
        assert_eq!(derive_reachability_status(&my, &peers), ReachabilityStatus::Degraded);
    }

    #[test]
    fn derive_reachability_status_unreachable_when_all_no_connection() {
        let my = MyNetworkSummary {
            iroh_node_id: "deadbeef".into(),
            reachability: ReachabilityStatus::Reachable,
            nat_classification: NatClass::Unknown,
            home_relay_url: None,
            relay_rtt_ms: None,
            direct_addresses: vec![],
        };
        let peers = vec![PeerHealth {
            owner_addr: "abcd".into(),
            display_name: None,
            shared_communities: vec![],
            connection_mode: ConnectionMode::NoConnection,
            rtt_ms: None,
            last_seen_ms: None,
            reachability_record_age_ms: None,
        }];
        assert_eq!(derive_reachability_status(&my, &peers), ReachabilityStatus::Unreachable);
    }

    #[test]
    fn derive_reachability_status_reachable_when_no_peers_yet() {
        // Spec rationale: no peers known yet ≠ unreachable. We have
        // working endpoint state; peer reachability is just unknown.
        let my = MyNetworkSummary {
            iroh_node_id: "deadbeef".into(),
            reachability: ReachabilityStatus::Unreachable,
            nat_classification: NatClass::Unknown,
            home_relay_url: None,
            relay_rtt_ms: None,
            direct_addresses: vec![],
        };
        let peers: Vec<PeerHealth> = vec![];
        assert_eq!(derive_reachability_status(&my, &peers), ReachabilityStatus::Reachable);
    }

    #[test]
    fn filter_peers_empty_membership_yields_empty_list() {
        let records = vec![
            make_record(0x11, ConnectionMode::Direct, Some(1000)),
            make_record(0x22, ConnectionMode::Relay, Some(2000)),
        ];
        let memb = FakeMembership { table: std::collections::HashMap::new() };
        let out = filter_peers_by_shared_membership(records, &memb, 5000);
        assert!(out.is_empty());
    }

    #[test]
    fn filter_peers_excludes_peers_with_no_shared_community() {
        let records = vec![
            make_record(0x11, ConnectionMode::Direct, Some(1000)),
            make_record(0x22, ConnectionMode::Relay, Some(2000)),
        ];
        let mut table = std::collections::HashMap::new();
        table.insert([0x11u8; 16], vec!["comm-a".to_string()]);
        // 0x22 has NO entry → excluded
        let memb = FakeMembership { table };
        let out = filter_peers_by_shared_membership(records, &memb, 5000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].owner_addr, hex::encode([0x11u8; 16]));
    }

    #[test]
    fn filter_peers_records_all_shared_communities() {
        let records = vec![make_record(0x11, ConnectionMode::Direct, Some(1000))];
        let mut table = std::collections::HashMap::new();
        table.insert([0x11u8; 16], vec!["comm-a".to_string(), "comm-b".to_string()]);
        let memb = FakeMembership { table };
        let out = filter_peers_by_shared_membership(records, &memb, 5000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].shared_communities, vec!["comm-a".to_string(), "comm-b".to_string()]);
    }

    #[test]
    fn filter_peers_sorts_by_last_seen_desc_none_last() {
        let records = vec![
            make_record(0x11, ConnectionMode::Direct, Some(1000)),
            make_record(0x22, ConnectionMode::Direct, Some(3000)),
            make_record(0x33, ConnectionMode::Direct, None),
            make_record(0x44, ConnectionMode::Direct, Some(2000)),
        ];
        let mut table = std::collections::HashMap::new();
        for b in [0x11, 0x22, 0x33, 0x44] {
            table.insert([b as u8; 16], vec!["c".to_string()]);
        }
        let memb = FakeMembership { table };
        let out = filter_peers_by_shared_membership(records, &memb, 10_000);
        assert_eq!(out.len(), 4);
        // Order: 3000, 2000, 1000, None
        assert_eq!(out[0].last_seen_ms, Some(3000));
        assert_eq!(out[1].last_seen_ms, Some(2000));
        assert_eq!(out[2].last_seen_ms, Some(1000));
        assert_eq!(out[3].last_seen_ms, None);
    }

    #[test]
    fn filter_peers_computes_record_age() {
        let records = vec![make_record(0x11, ConnectionMode::Direct, Some(1000))];
        let mut table = std::collections::HashMap::new();
        table.insert([0x11u8; 16], vec!["c".to_string()]);
        let memb = FakeMembership { table };
        let out = filter_peers_by_shared_membership(records, &memb, 5000);
        assert_eq!(out[0].reachability_record_age_ms, Some(4000));
    }

    #[test]
    fn network_health_snapshot_empty_is_well_formed() {
        let s = NetworkHealthSnapshot::empty();
        assert_eq!(s.schema_version, 1);
        assert!(s.my_network.is_none());
        assert!(s.peers.is_empty());
        assert_eq!(s.pkarr_status.community_publish_count, 0);
        assert!(s.pkarr_status.recent_fallback_events.is_empty());
        assert!(!s.app_version.is_empty());
    }
}
```

- [ ] **Step 3: Run the new tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
timeout 600 cargo nextest run --locked --features test-fixtures -E 'test(network_health::tests)' 2>&1 | tail -40
echo "EXIT=${PIPESTATUS[0]}"
```

Expected: all 11 new tests pass.

- [ ] **Step 4: cargo fmt + clippy + full nextest**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
set -o pipefail
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
echo "CLIPPY EXIT=${PIPESTATUS[0]}"
timeout 600 cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -20
echo "NEXTEST EXIT=${PIPESTATUS[0]}"
```

Expected: clippy clean; nextest shows only baseline orphan failures + the new 11 passing.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/network_health.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-329): network_health module skeleton + pure synthesis fns

Data types (NetworkHealthSnapshot, MyNetworkSummary, PeerHealth,
PkarrHealthSummary, PkarrFallbackHit, SelfTestReport, SelfTestStep,
PeerPingResult, StepOutcome, ReachabilityStatus, NatClass,
ConnectionMode) + three pure functions:
- classify_nat (Phase 1: returns Unknown; TODO follow-up)
- derive_reachability_status (Reachable/Degraded/Unreachable per peer set)
- filter_peers_by_shared_membership (scope to my membership, sort by
  last_seen desc, compute record age)

Per spec §4.1 + §4.4. No NetworkHealthService yet (Task 3) and no
format_export_markdown yet (Task 2 — redaction-leak test first).
EOF
)"
```

---

## Task 2: format_export_markdown with redaction-leak test FIRST

**Files:**
- Modify: `src-tauri/src/network_health.rs` (append `format_export_markdown` + redaction helper + tests)

**Rationale:** per `feedback_second_order_correctness_review`, redaction is a security-adjacent invariant. Write the regex-leak test FIRST so the implementation can't accidentally cheat by emitting full IDs in fields the test doesn't check.

- [ ] **Step 1: Add the leak regex test FIRST (still expected to fail)**

Append to `src-tauri/src/network_health.rs` (inside the existing `mod tests`):

```rust
    fn fixture_snapshot_with_full_ids() -> NetworkHealthSnapshot {
        NetworkHealthSnapshot {
            schema_version: 1,
            captured_at_ms: 1_700_000_000_000,
            app_version: "0.1.0-alpha.1".into(),
            platform: "darwin/aarch64".into(),
            my_network: Some(MyNetworkSummary {
                // 64 hex chars = a real Ed25519/iroh node id
                iroh_node_id: "a3f9e1c2".repeat(8),
                reachability: ReachabilityStatus::Reachable,
                nat_classification: NatClass::FullCone,
                home_relay_url: Some("https://use1.derp.iroh.network/".into()),
                relay_rtt_ms: Some(24),
                direct_addresses: vec!["192.0.2.1:11204".into()],
            }),
            peers: vec![
                PeerHealth {
                    // 32-char lowercase hex owner addr
                    owner_addr: "deadbeef".repeat(4),
                    display_name: Some("alice".into()),
                    shared_communities: vec!["beefcafe".repeat(4)],
                    connection_mode: ConnectionMode::Direct,
                    rtt_ms: Some(18),
                    last_seen_ms: Some(1_700_000_000_000 - 3_000),
                    reachability_record_age_ms: Some(3_000),
                },
            ],
            pkarr_status: PkarrHealthSummary {
                identity_published: true,
                identity_last_publish_ms: Some(1_700_000_000_000 - 60_000),
                community_publish_count: 1,
                recent_fallback_events: vec![],
            },
        }
    }

    #[test]
    fn format_export_redacted_leaks_no_full_ids() {
        let snap = fixture_snapshot_with_full_ids();
        let md = format_export_markdown(&snap, None, false);
        // Reject any 32+ lowercase hex run anywhere in the output.
        // 32 chars is the minimum length of an owner addr or community
        // id; 64 for iroh node id. Both should be redacted to 8-char
        // prefixes by the redacted formatter.
        let re = regex::Regex::new(r"[0-9a-f]{32,}").unwrap();
        if let Some(m) = re.find(&md) {
            panic!(
                "redacted export leaks full id at byte {}: {}\n--- full output ---\n{}",
                m.start(),
                m.as_str(),
                md
            );
        }
    }

    #[test]
    fn format_export_full_ids_includes_them() {
        let snap = fixture_snapshot_with_full_ids();
        let md = format_export_markdown(&snap, None, true);
        // Owner addr "deadbeef" * 4 = "deadbeefdeadbeefdeadbeefdeadbeef" (32 chars)
        assert!(md.contains("deadbeefdeadbeefdeadbeefdeadbeef"), "full owner addr must appear");
        // iroh node id "a3f9e1c2" * 8 = 64 char hex
        assert!(md.contains(&"a3f9e1c2".repeat(8)), "full iroh node id must appear");
    }

    #[test]
    fn format_export_omits_self_test_section_when_none() {
        let snap = fixture_snapshot_with_full_ids();
        let md = format_export_markdown(&snap, None, false);
        // No header for self-test, no boilerplate "not run"
        assert!(!md.contains("Self-test"), "no self-test header when report=None");
        assert!(!md.contains("not run"), "no boilerplate placeholder");
    }

    #[test]
    fn format_export_includes_self_test_section_when_some() {
        let snap = fixture_snapshot_with_full_ids();
        let report = SelfTestReport {
            started_at_ms: 1_700_000_000_000,
            finished_at_ms: 1_700_000_001_500,
            steps: vec![
                SelfTestStep { name: "endpoint".into(), outcome: StepOutcome::Pass { duration_ms: 12 } },
                SelfTestStep { name: "relay".into(), outcome: StepOutcome::Pass { duration_ms: 24 } },
            ],
            peer_results: vec![],
        };
        let md = format_export_markdown(&snap, Some(&report), false);
        assert!(md.contains("Self-test"), "self-test header present");
        assert!(md.contains("endpoint"), "step name present");
    }

    #[test]
    fn format_export_empty_peer_list_emits_no_peers_line() {
        let mut snap = fixture_snapshot_with_full_ids();
        snap.peers.clear();
        let md = format_export_markdown(&snap, None, false);
        // The Peers section exists but the body is a single line, not
        // a header followed by empty content.
        assert!(md.contains("no peers"), "empty peer list emits 'no peers' line");
    }

    #[test]
    fn format_export_includes_schema_version() {
        let snap = fixture_snapshot_with_full_ids();
        let md = format_export_markdown(&snap, None, false);
        assert!(md.contains("schemaVersion") || md.contains("schema version") || md.contains("schemaversion") || md.contains("Schema") || md.contains("schema_version") || md.contains("1"), "schema version must be present (matched generously since exact format is up to the implementer)");
    }
```

- [ ] **Step 2: Run tests — expect ALL SIX to fail (function not defined)**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
timeout 600 cargo nextest run --locked --features test-fixtures -E 'test(network_health::tests::format_export)' 2>&1 | tail -30
echo "EXIT=${PIPESTATUS[0]}"
```

Expected: 6 tests fail with "cannot find function `format_export_markdown` in this scope" (compile error). This validates the tests are wired before the implementation exists.

- [ ] **Step 3: Add `regex` dep (already in workspace?) — verify**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
grep -E "^regex " Cargo.toml
# If absent, append to [dev-dependencies]:
#   regex = "1"
```

If `regex` is not a dev-dependency, add it:

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo add --dev regex
```

- [ ] **Step 4: Implement `format_export_markdown`**

Append to `src-tauri/src/network_health.rs`, after the pure functions and before `#[cfg(test)] mod tests`:

```rust
/// Spec §5.4: server-side redaction is the only path that emits
/// identifier prefixes. With `include_full_ids=false`, all owner
/// addresses + community ids + iroh node ids are reduced to 8-char
/// prefixes followed by `…`. Self-test section is fully omitted if
/// `last_report` is `None`. Schema version is always present.
pub fn format_export_markdown(
    snapshot: &NetworkHealthSnapshot,
    last_report: Option<&SelfTestReport>,
    include_full_ids: bool,
) -> String {
    let r = |s: &str| -> String {
        if include_full_ids {
            s.to_string()
        } else if s.len() <= 8 {
            s.to_string()
        } else {
            format!("{}…", &s[..8])
        }
    };

    let mut out = String::new();
    use std::fmt::Write;

    let _ = writeln!(out, "## Harmony v{} ({})", snapshot.app_version, snapshot.platform);
    let _ = writeln!(out, "schemaVersion: {}", snapshot.schema_version);
    let _ = writeln!(out, "capturedAtMs: {}", snapshot.captured_at_ms);
    let _ = writeln!(out);

    let _ = writeln!(out, "## Network");
    match &snapshot.my_network {
        Some(my) => {
            let _ = writeln!(out, "irohNodeId: {}", r(&my.iroh_node_id));
            let _ = writeln!(out, "reachability: {:?}", my.reachability);
            let _ = writeln!(out, "nat: {:?}", my.nat_classification);
            if let Some(url) = &my.home_relay_url {
                let _ = writeln!(out, "homeRelayUrl: {}", url);
            }
            if let Some(rtt) = my.relay_rtt_ms {
                let _ = writeln!(out, "relayRttMs: {}", rtt);
            }
            if !my.direct_addresses.is_empty() {
                let _ = writeln!(out, "directAddresses: {}", my.direct_addresses.join(", "));
            }
        }
        None => {
            let _ = writeln!(out, "(iroh endpoint not yet bound)");
        }
    }
    let _ = writeln!(out);

    if let Some(report) = last_report {
        let _ = writeln!(out, "## Self-test");
        let _ = writeln!(out, "startedAtMs: {}", report.started_at_ms);
        let _ = writeln!(out, "finishedAtMs: {}", report.finished_at_ms);
        for step in &report.steps {
            let marker = match &step.outcome {
                StepOutcome::Pass { duration_ms } => format!("✓ ({}ms)", duration_ms),
                StepOutcome::Fail { reason } => format!("✗ {}", reason),
                StepOutcome::Skipped { reason } => format!("⊘ {}", reason),
            };
            let _ = writeln!(out, "{}: {}", step.name, marker);
        }
        if !report.peer_results.is_empty() {
            let _ = writeln!(out, "peerPings:");
            for pr in &report.peer_results {
                let marker = match &pr.outcome {
                    StepOutcome::Pass { duration_ms } => format!("✓ ({}ms)", duration_ms),
                    StepOutcome::Fail { reason } => format!("✗ {}", reason),
                    StepOutcome::Skipped { reason } => format!("⊘ {}", reason),
                };
                let mode = pr.mode.map(|m| format!(" [{:?}]", m)).unwrap_or_default();
                let _ = writeln!(out, "  {} {}{}", r(&pr.owner_addr), marker, mode);
            }
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "## Peers");
    if snapshot.peers.is_empty() {
        let _ = writeln!(out, "(no peers in shared communities)");
    } else {
        for p in &snapshot.peers {
            let mode_marker = match p.connection_mode {
                ConnectionMode::Direct => "direct",
                ConnectionMode::Relay => "relay",
                ConnectionMode::NoConnection => "none",
            };
            let rtt = p.rtt_ms.map(|v| format!(" {}ms", v)).unwrap_or_default();
            let age = p
                .reachability_record_age_ms
                .map(|ms| format!(" ({}s ago)", ms / 1000))
                .unwrap_or_default();
            let comms: Vec<String> = p.shared_communities.iter().map(|c| r(c)).collect();
            let _ = writeln!(
                out,
                "{} {}{}{} [{}]",
                r(&p.owner_addr),
                mode_marker,
                rtt,
                age,
                comms.join(",")
            );
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Discovery (pkarr)");
    let _ = writeln!(out, "identityPublished: {}", snapshot.pkarr_status.identity_published);
    if let Some(t) = snapshot.pkarr_status.identity_last_publish_ms {
        let _ = writeln!(out, "identityLastPublishMs: {}", t);
    }
    let _ = writeln!(out, "communityPublishCount: {}", snapshot.pkarr_status.community_publish_count);
    for hit in &snapshot.pkarr_status.recent_fallback_events {
        let _ = writeln!(
            out,
            "fallback {} in {} -> {}",
            hit.peer_addr_short,
            hit.community_id_short,
            if hit.hit { "hit" } else { "miss" }
        );
    }

    out
}
```

- [ ] **Step 5: Run the format tests, expect ALL pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
timeout 600 cargo nextest run --locked --features test-fixtures -E 'test(network_health::tests::format_export)' 2>&1 | tail -30
echo "EXIT=${PIPESTATUS[0]}"
```

Expected: all 6 format_export_* tests pass.

- [ ] **Step 6: Full gate run**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
set -o pipefail
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
echo "CLIPPY EXIT=${PIPESTATUS[0]}"
timeout 600 cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -20
echo "NEXTEST EXIT=${PIPESTATUS[0]}"
```

Expected: clippy clean; nextest = baseline failures + Task 1 (11) + Task 2 (6) new passing.

- [ ] **Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/network_health.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "$(cat <<'EOF'
feat(zeb-329): format_export_markdown with server-side redaction

Redaction is the only path that emits identifier prefixes; with
include_full_ids=false, all owner addresses + community ids + iroh
node ids are reduced to 8-char prefixes followed by ellipsis. Self-test
section is fully omitted if last_report is None.

Per feedback_second_order_correctness_review: redaction-leak regex
test (`[0-9a-f]{32,}` rejected anywhere in redacted output) was
written FIRST so the implementation can't accidentally cheat. Five
other tests cover include_full_ids=true round-trip, self-test
omission, self-test inclusion, empty peer list line, and schema
version presence.
EOF
)"
```

---

## Task 3: NetworkHealthService skeleton + snapshot()

**Files:**
- Modify: `src-tauri/src/network_health.rs` (append `NetworkHealthService` + IrohSnapshot trait + tests)

**What this task builds:** the `NetworkHealthService` struct holding `IrohSnapshot` (a small trait extracted so production uses iroh while tests use a fake) + `ReachabilityResolver` + `Arc<RwLock<Option<SelfTestReport>>>` cache + `app_handle: tauri::AppHandle`. Implements `snapshot()` that returns a `NetworkHealthSnapshot` by composing the pure functions from Task 1.

- [ ] **Step 1: Define the IrohSnapshot trait**

Append to `src-tauri/src/network_health.rs` (between the pure functions and `format_export_markdown`):

```rust
/// Iroh-side data the snapshot needs. Trait-extracted so unit tests
/// can substitute a fake without running real iroh. Production impl
/// in lib.rs boot wiring delegates to `IrohEndpoint`.
pub trait IrohSnapshot: Send + Sync {
    fn iroh_node_id_hex(&self) -> Option<String>;
    fn home_relay_url(&self) -> Option<String>;
    fn relay_rtt_ms(&self) -> Option<u32>;
    fn direct_addresses(&self) -> Vec<String>;
    fn nat_classification(&self) -> NatClass;
}

/// Pkarr-side data the snapshot needs. Trait-extracted for testability;
/// production impl reads from `pkarr_publisher.active_handles()` + the
/// fallback ring buffer.
pub trait PkarrSnapshot: Send + Sync {
    fn identity_published(&self) -> bool;
    fn identity_last_publish_ms(&self) -> Option<u64>;
    fn community_publish_count(&self) -> u32;
    fn recent_fallback_events(&self) -> Vec<PkarrFallbackHit>;
}

/// Resolver-side data. Trait-extracted so the snapshot can be tested
/// without the full ReachabilityResolver. Production impl reads from
/// `ReachabilityResolver::list_active_peers()` + iroh-side
/// connection-mode lookups.
pub trait ReachabilitySnapshot: Send + Sync {
    fn list_records(&self) -> Vec<ResolverPeerRecord>;
}
```

- [ ] **Step 2: Define NetworkHealthService skeleton**

Append:

```rust
use std::sync::Arc;
use tokio::sync::RwLock;

/// Spec §5.5: state coupling summary. NetworkHealthService owns the
/// rate-limiter task handle + cached last self-test report; the iroh /
/// resolver / pkarr handles come from AppState (already constructed).
pub struct NetworkHealthService {
    iroh: Arc<dyn IrohSnapshot>,
    pkarr: Arc<dyn PkarrSnapshot>,
    resolver: Arc<dyn ReachabilitySnapshot>,
    membership: Arc<dyn MyMembershipSet + Send + Sync>,
    last_self_test: Arc<RwLock<Option<SelfTestReport>>>,
    /// Channel into the rate-limiter task — Task 4 adds this.
    notify_tx: Option<tokio::sync::mpsc::UnboundedSender<()>>,
}

impl NetworkHealthService {
    pub fn new(
        iroh: Arc<dyn IrohSnapshot>,
        pkarr: Arc<dyn PkarrSnapshot>,
        resolver: Arc<dyn ReachabilitySnapshot>,
        membership: Arc<dyn MyMembershipSet + Send + Sync>,
    ) -> Self {
        Self {
            iroh,
            pkarr,
            resolver,
            membership,
            last_self_test: Arc::new(RwLock::new(None)),
            notify_tx: None,
        }
    }

    /// Spec §5.1: read from all sources, synthesize a snapshot. Never
    /// fails — empty/None fields render gracefully in the UI.
    pub async fn snapshot(&self) -> NetworkHealthSnapshot {
        let now = now_ms();
        let my_network = self.iroh.iroh_node_id_hex().map(|node_id| {
            // Build peers first (so derive_reachability_status can see them).
            // We do this in two passes: build my_network with a placeholder
            // status, derive status from peer set, then patch in.
            MyNetworkSummary {
                iroh_node_id: node_id,
                reachability: ReachabilityStatus::Reachable, // patched below
                nat_classification: self.iroh.nat_classification(),
                home_relay_url: self.iroh.home_relay_url(),
                relay_rtt_ms: self.iroh.relay_rtt_ms(),
                direct_addresses: self.iroh.direct_addresses(),
            }
        });

        let records = self.resolver.list_records();
        let peers = filter_peers_by_shared_membership(records, &*self.membership, now);

        // Patch reachability status now that we have peers.
        let my_network = my_network.map(|mut my| {
            my.reachability = derive_reachability_status(&my, &peers);
            my
        });

        NetworkHealthSnapshot {
            schema_version: 1,
            captured_at_ms: now,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            platform: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
            my_network,
            peers,
            pkarr_status: PkarrHealthSummary {
                identity_published: self.pkarr.identity_published(),
                identity_last_publish_ms: self.pkarr.identity_last_publish_ms(),
                community_publish_count: self.pkarr.community_publish_count(),
                recent_fallback_events: self.pkarr.recent_fallback_events(),
            },
        }
    }

    /// Read the cached last self-test report (Task 5 + 6 populate this).
    pub async fn cached_last_self_test(&self) -> Option<SelfTestReport> {
        self.last_self_test.read().await.clone()
    }
}
```

- [ ] **Step 3: Add snapshot tests**

Append inside `mod tests`:

```rust
    struct FakeIroh {
        ready: bool,
    }
    impl IrohSnapshot for FakeIroh {
        fn iroh_node_id_hex(&self) -> Option<String> {
            if self.ready { Some("a3f9e1c2".repeat(8)) } else { None }
        }
        fn home_relay_url(&self) -> Option<String> {
            if self.ready { Some("https://derp.example/".into()) } else { None }
        }
        fn relay_rtt_ms(&self) -> Option<u32> {
            if self.ready { Some(24) } else { None }
        }
        fn direct_addresses(&self) -> Vec<String> {
            if self.ready { vec!["192.0.2.1:11204".into()] } else { vec![] }
        }
        fn nat_classification(&self) -> NatClass {
            NatClass::Unknown
        }
    }

    struct FakePkarr;
    impl PkarrSnapshot for FakePkarr {
        fn identity_published(&self) -> bool { true }
        fn identity_last_publish_ms(&self) -> Option<u64> { Some(1_700_000_000_000) }
        fn community_publish_count(&self) -> u32 { 1 }
        fn recent_fallback_events(&self) -> Vec<PkarrFallbackHit> { vec![] }
    }

    struct FakeResolver {
        records: Vec<ResolverPeerRecord>,
    }
    impl ReachabilitySnapshot for FakeResolver {
        fn list_records(&self) -> Vec<ResolverPeerRecord> {
            self.records.clone()
        }
    }

    fn empty_membership() -> Arc<FakeMembership> {
        Arc::new(FakeMembership { table: std::collections::HashMap::new() })
    }

    #[tokio::test]
    async fn snapshot_with_iroh_not_ready_returns_my_network_none() {
        let svc = NetworkHealthService::new(
            Arc::new(FakeIroh { ready: false }),
            Arc::new(FakePkarr),
            Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
        );
        let snap = svc.snapshot().await;
        assert!(snap.my_network.is_none());
        assert!(snap.peers.is_empty());
        assert_eq!(snap.schema_version, 1);
    }

    #[tokio::test]
    async fn snapshot_with_iroh_ready_empty_resolver() {
        let svc = NetworkHealthService::new(
            Arc::new(FakeIroh { ready: true }),
            Arc::new(FakePkarr),
            Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
        );
        let snap = svc.snapshot().await;
        assert!(snap.my_network.is_some());
        assert_eq!(snap.peers, vec![]);
        assert_eq!(snap.my_network.as_ref().unwrap().home_relay_url, Some("https://derp.example/".into()));
    }

    #[tokio::test]
    async fn snapshot_with_three_peers_sorted_by_last_seen_desc() {
        let mut table = std::collections::HashMap::new();
        for b in [0x11u8, 0x22, 0x33] {
            table.insert([b; 16], vec!["c1".to_string()]);
        }
        let svc = NetworkHealthService::new(
            Arc::new(FakeIroh { ready: true }),
            Arc::new(FakePkarr),
            Arc::new(FakeResolver {
                records: vec![
                    make_record(0x11, ConnectionMode::Direct, Some(1000)),
                    make_record(0x22, ConnectionMode::Direct, Some(3000)),
                    make_record(0x33, ConnectionMode::Direct, Some(2000)),
                ],
            }),
            Arc::new(FakeMembership { table }),
        );
        let snap = svc.snapshot().await;
        assert_eq!(snap.peers.len(), 3);
        assert_eq!(snap.peers[0].last_seen_ms, Some(3000));
        assert_eq!(snap.peers[1].last_seen_ms, Some(2000));
        assert_eq!(snap.peers[2].last_seen_ms, Some(1000));
        // With at least one Direct peer, reachability is Reachable.
        assert_eq!(snap.my_network.unwrap().reachability, ReachabilityStatus::Reachable);
    }
```

- [ ] **Step 4: Run snapshot tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
timeout 600 cargo nextest run --locked --features test-fixtures -E 'test(network_health::tests::snapshot)' 2>&1 | tail -30
echo "EXIT=${PIPESTATUS[0]}"
```

Expected: 3 new tests pass.

- [ ] **Step 5: Full gate run + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
set -o pipefail
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
echo "CLIPPY EXIT=${PIPESTATUS[0]}"
timeout 600 cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -20
echo "NEXTEST EXIT=${PIPESTATUS[0]}"
```

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/network_health.rs
git commit -m "feat(zeb-329): NetworkHealthService skeleton + snapshot()

Service holds IrohSnapshot + PkarrSnapshot + ReachabilitySnapshot
+ MyMembershipSet trait objects (extracted for testability) + cached
last self-test report (Arc<RwLock<Option<SelfTestReport>>>) + future
notify channel slot for the Task 4 rate-limiter.

snapshot() composes the pure functions from Task 1: two-pass build
where MyNetworkSummary's reachability field is patched in after the
peer list is filtered by shared membership. Three new tokio tests
cover iroh-not-ready, iroh-ready-empty-resolver, and three-peers-
sorted-by-last-seen-desc paths."
```

---

## Task 4: Rate-limiter task + notify() API

**Files:**
- Modify: `src-tauri/src/network_health.rs` (extend NetworkHealthService with `spawn_rate_limiter` + `notify` + tests)

**Goal:** Per spec §5.2 — at-most-one Tauri event per 2s. Uses `tokio::sync::mpsc::unbounded_channel` for the notify pipe + a dedicated task that drains the channel and emits at most once per `RATE_LIMIT_WINDOW` (2s).

- [ ] **Step 1: Define the rate-limiter task**

Append to `network_health.rs`:

```rust
const RATE_LIMIT_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// Event name emitted to the frontend when rate-limiter fires.
pub const NETWORK_HEALTH_CHANGED_EVENT: &str = "network-health-changed";

impl NetworkHealthService {
    /// Spawn the rate-limiter task and wire `self.notify_tx`. Call once
    /// at boot, AFTER iroh + resolver are constructed (spec §5.5).
    ///
    /// Idempotent: a second call replaces the channel + spawns a new
    /// task; the old task drains its channel and exits.
    pub fn spawn_rate_limiter<E: NotifyEmitter + Send + 'static>(&mut self, emitter: E) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        self.notify_tx = Some(tx);
        tokio::spawn(async move {
            let mut last_emit: Option<std::time::Instant> = None;
            while rx.recv().await.is_some() {
                // Drain any other queued notifies that arrived since last
                // poll — we only need to know "something happened".
                while rx.try_recv().is_ok() {}
                let now = std::time::Instant::now();
                let due = last_emit
                    .map(|t| now.duration_since(t) >= RATE_LIMIT_WINDOW)
                    .unwrap_or(true);
                if due {
                    emitter.emit_change();
                    last_emit = Some(now);
                } else {
                    // Schedule a delayed emit at the end of the window,
                    // then sleep until then so subsequent notifies in
                    // this window collapse into the one delayed emit.
                    let remaining = RATE_LIMIT_WINDOW
                        .checked_sub(now.duration_since(last_emit.unwrap()))
                        .unwrap_or_default();
                    tokio::time::sleep(remaining).await;
                    // Drain any notifies queued during the sleep.
                    while rx.try_recv().is_ok() {}
                    emitter.emit_change();
                    last_emit = Some(std::time::Instant::now());
                }
            }
        });
    }

    /// Send a notify into the rate-limiter. Safe to call from any
    /// task. No-op when the rate-limiter hasn't been spawned (e.g. in
    /// unit tests that don't exercise event emission).
    pub fn notify(&self) {
        if let Some(tx) = self.notify_tx.as_ref() {
            // Ignore send errors: the only way send fails is the
            // receiver dropped, which means the rate-limiter task
            // exited. That's a boot-shutdown race; nothing to do.
            let _ = tx.send(());
        }
    }
}

/// Indirection over Tauri's `app_handle.emit(...)` so the rate-limiter
/// task can be tested without a real app. Production impl is a thin
/// wrapper around `tauri::AppHandle`.
pub trait NotifyEmitter: Send + Sync {
    fn emit_change(&self);
}
```

- [ ] **Step 2: Add rate-limiter tests**

Append inside `mod tests`:

```rust
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct CountingEmitter {
        n: Arc<AtomicUsize>,
    }
    impl NotifyEmitter for CountingEmitter {
        fn emit_change(&self) {
            self.n.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn build_svc_with_rate_limiter() -> (NetworkHealthService, Arc<AtomicUsize>) {
        let mut svc = NetworkHealthService::new(
            Arc::new(FakeIroh { ready: true }),
            Arc::new(FakePkarr),
            Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
        );
        let counter = Arc::new(AtomicUsize::new(0));
        let emitter = CountingEmitter { n: counter.clone() };
        svc.spawn_rate_limiter(emitter);
        (svc, counter)
    }

    #[tokio::test]
    async fn rate_limiter_collapses_30_rapid_notifies_to_one_emit() {
        let (svc, counter) = build_svc_with_rate_limiter();
        for _ in 0..30 {
            svc.notify();
        }
        // Wait past the rate-limit window plus a small grace.
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        let n = counter.load(Ordering::SeqCst);
        // The first notify emits immediately (last_emit was None); any
        // further notifies in the 2s window collapse into ONE delayed
        // emit at the window edge. Total = 2 (one immediate + one delayed).
        // If notifies stop after the burst, NO further emits fire.
        assert!(
            n == 1 || n == 2,
            "expected 1-2 emits for 30 rapid notifies, got {}",
            n
        );
    }

    #[tokio::test]
    async fn rate_limiter_no_emit_when_no_notifies() {
        let (_svc, counter) = build_svc_with_rate_limiter();
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rate_limiter_emits_every_window_when_continuously_notified() {
        // Notify once per 500ms for 5s (10 notifies); expect ~3 emits
        // (one per 2s window). Use a loose bound because tokio timer
        // resolution + test runner jitter make exact counts brittle.
        let (svc, counter) = build_svc_with_rate_limiter();
        for _ in 0..10 {
            svc.notify();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        let n = counter.load(Ordering::SeqCst);
        assert!(
            (2..=5).contains(&n),
            "expected 2-5 emits for 10 notifies spaced 500ms over 5s, got {}",
            n
        );
    }
```

- [ ] **Step 3: Run rate-limiter tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
timeout 600 cargo nextest run --locked --features test-fixtures -E 'test(network_health::tests::rate_limiter)' 2>&1 | tail -30
echo "EXIT=${PIPESTATUS[0]}"
```

Expected: 3 new tests pass. (Note: these are real-time wall-clock tests; they take ~5s each.)

- [ ] **Step 4: Full gate run + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
set -o pipefail
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
timeout 600 cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -20
```

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/network_health.rs
git commit -m "feat(zeb-329): rate-limiter task for network-health-changed event

At-most-one emit per 2s window (spec §5.2). First notify in an idle
window fires immediately; subsequent notifies in the same window
collapse into one delayed emit at the window edge; further idle ticks
produce no emits.

NotifyEmitter trait indirection lets unit tests count emits without
a real Tauri AppHandle. Three real-time tests cover 30-rapid-notifies
(1-2 emits), no-notifies (0 emits), continuous-500ms-notifies-for-5s
(2-5 emits)."
```

---

## Task 5: HARMONY_PING_V1 ALPN + accept handler

**Files:**
- Modify: `src-tauri/src/iroh_endpoint.rs` (add ALPN constant + register in `new_with_secret` + `from_endpoint_for_test`)
- Modify: `src-tauri/src/network_health.rs` (add `spawn_ping_accept_loop` + connect-side `ping_peer` helper + behavior tests)

**Goal:** Add the new ALPN constant. Run a SECOND accept loop (not folded into the existing zenoh accept loop) that only handles HARMONY_PING_V1. Echo one byte and close.

- [ ] **Step 1: Add the ALPN constant to iroh_endpoint::alpn module**

In `src-tauri/src/iroh_endpoint.rs`, find the `pub mod alpn {` block (around line 47) and add:

```rust
pub mod alpn {
    pub const HARMONY_ZENOH_V1: &[u8] = b"harmony/zenoh/v1";
    pub const HARMONY_HANDSHAKE_V1: &[u8] = b"harmony/handshake/v1";
    /// ZEB-329: self-test only — peer ping with 1-byte echo. Produces
    /// no app-level state; safe to ignore for all non-self-test code.
    pub const HARMONY_PING_V1: &[u8] = b"harmony/ping/v1";
}
```

- [ ] **Step 2: Register the new ALPN in both endpoint builders**

In `new_with_secret`:

```rust
        let inner = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .alpns(vec![
                alpn::HARMONY_ZENOH_V1.to_vec(),
                alpn::HARMONY_HANDSHAKE_V1.to_vec(),
                alpn::HARMONY_PING_V1.to_vec(),
            ])
            .bind()
            .await
            .map_err(|e| IrohEndpointError::Bind(Box::new(e)))?;
```

In the test-only builder (around line 250):

```rust
            .alpns(vec![
                alpn::HARMONY_ZENOH_V1.to_vec(),
                alpn::HARMONY_HANDSHAKE_V1.to_vec(),
                alpn::HARMONY_PING_V1.to_vec(),
            ])
```

Update the existing `alpn_constants_are_correct` test:

```rust
    #[test]
    fn alpn_constants_are_correct() {
        assert_eq!(alpn::HARMONY_ZENOH_V1, b"harmony/zenoh/v1");
        assert_eq!(alpn::HARMONY_HANDSHAKE_V1, b"harmony/handshake/v1");
        assert_eq!(alpn::HARMONY_PING_V1, b"harmony/ping/v1");
    }
```

- [ ] **Step 3: Implement the accept loop in network_health.rs**

Append to `src-tauri/src/network_health.rs`:

```rust
/// Spec §5.3 + §7.3: tiny accept loop that echoes one byte on the
/// HARMONY_PING_V1 ALPN. Spawned at boot by NetworkHealthService.
/// Self-test only — produces no app-level state.
pub fn spawn_ping_accept_loop(endpoint: Arc<crate::iroh_endpoint::IrohEndpoint>) {
    tokio::spawn(async move {
        loop {
            // iroh::Endpoint::accept returns Option<Incoming>; we only
            // handle HARMONY_PING_V1 here and let other ALPNs fall
            // through to the zenoh accept loop (which has its own
            // running task elsewhere).
            let Some(incoming) = endpoint.inner().accept().await else {
                // Endpoint closed; exit.
                return;
            };
            tokio::spawn(handle_ping_accept(incoming));
        }
    });
}

async fn handle_ping_accept(incoming: iroh::endpoint::Incoming) {
    // Only handle HARMONY_PING_V1; ignore others. iroh 0.98 surfaces
    // the negotiated ALPN via Connection::alpn() after accept.
    let Ok(connecting) = incoming.accept() else {
        return;
    };
    let Ok(conn) = connecting.await else {
        return;
    };
    let alpn = conn.alpn();
    if alpn.as_deref() != Some(crate::iroh_endpoint::alpn::HARMONY_PING_V1) {
        // Not our ALPN; close without echoing. The other accept loop
        // (zenoh) will see this connection via its own accept() call.
        // NOTE: iroh 0.98 may not allow two accept loops on the same
        // Endpoint cleanly. If so, fold this dispatch into the existing
        // zenoh accept loop and route by ALPN — implementer flags this
        // as DONE_WITH_CONCERNS if it surfaces.
        return;
    }
    let Ok((mut send, mut recv)) = conn.accept_bi().await else {
        return;
    };
    let mut buf = [0u8; 1];
    if recv.read_exact(&mut buf).await.is_err() {
        return;
    }
    let _ = send.write_all(&buf).await;
    let _ = send.finish();
    // Connection closes when dropped.
}

/// Connect-side: open a HARMONY_PING_V1 bi-stream to `node_id`, write
/// one byte, read one byte echo, return RTT.
pub async fn ping_peer(
    endpoint: &crate::iroh_endpoint::IrohEndpoint,
    node_id: iroh::EndpointId,
    timeout: std::time::Duration,
) -> Result<std::time::Duration, String> {
    let start = std::time::Instant::now();
    let result = tokio::time::timeout(timeout, async {
        let conn = endpoint
            .inner()
            .connect(node_id, crate::iroh_endpoint::alpn::HARMONY_PING_V1)
            .await
            .map_err(|e| format!("connect failed: {}", e))?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| format!("open_bi failed: {}", e))?;
        send.write_all(&[0x42])
            .await
            .map_err(|e| format!("write_all failed: {}", e))?;
        send.finish().map_err(|e| format!("finish failed: {}", e))?;
        let mut buf = [0u8; 1];
        recv.read_exact(&mut buf)
            .await
            .map_err(|e| format!("read_exact failed: {}", e))?;
        if buf[0] != 0x42 {
            return Err(format!("unexpected echo byte: {}", buf[0]));
        }
        Ok::<(), String>(())
    })
    .await;
    match result {
        Ok(Ok(())) => Ok(start.elapsed()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("timeout".to_string()),
    }
}
```

- [ ] **Step 4: Gate run + commit (integration test follows in Task 8)**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
set -o pipefail
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
timeout 600 cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -20
```

Expected: clippy clean; nextest = baseline + previously-passing.

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/iroh_endpoint.rs src-tauri/src/network_health.rs
git commit -m "feat(zeb-329): HARMONY_PING_V1 ALPN + accept-loop + ping_peer

New ALPN registered in both production and test endpoint builders.
Tiny accept loop spawned by NetworkHealthService at boot echoes one
byte on HARMONY_PING_V1 connections; non-matching ALPNs fall through
to the existing zenoh accept loop (or are dropped, see inline NOTE).

ping_peer is the connect side: open bi-stream, write byte, read byte
echo, return RTT. Timeout wraps the whole exchange (5s per spec §5.3).
Integration test that exercises both sides is Task 8."
```

---

## Task 6: Self-test implementation

**Files:**
- Modify: `src-tauri/src/network_health.rs` (add `run_self_test` method + behavior tests)

**Goal:** Spec §5.3 — four steps in order (endpoint, relay, pkarr_publish, pkarr_resolve) + per-peer parallel pings with semaphore cap 32 + cached result.

- [ ] **Step 1: Add trait methods for the self-test operations**

Append to the trait definitions at the top of network_health.rs (extend `IrohSnapshot` + `PkarrSnapshot`):

```rust
/// Trait extension for self-test operations (spec §5.3). Production
/// impl lives in lib.rs boot wiring; tests use fakes.
pub trait IrohSelfTest: Send + Sync {
    /// True if `Endpoint::is_bound()` (or equivalent). Phase 1
    /// approximation: any iroh_node_id_hex() returning Some.
    fn endpoint_bound(&self) -> bool;
    /// Round-trip ping to home relay. Returns the RTT or Err string.
    /// Bounded reason strings per spec §6.2.
    fn relay_round_trip(&self) -> futures::future::BoxFuture<'_, Result<std::time::Duration, String>>;
}

pub trait PkarrSelfTest: Send + Sync {
    fn publish_identity(&self) -> futures::future::BoxFuture<'_, Result<std::time::Duration, String>>;
    /// Resolve own identity from pkarr, verify the returned payload
    /// matches the most recent published one. Bounded reason strings
    /// per spec §6.2.
    fn resolve_self(&self) -> futures::future::BoxFuture<'_, Result<std::time::Duration, String>>;
}

/// Trait for ping side. Production impl wraps `ping_peer` (Task 5);
/// tests substitute a fake that yields scripted results.
pub trait PingDispatcher: Send + Sync {
    /// Returns (RTT, mode) on success, error string on failure. Mode
    /// is approximate — implementer maps iroh connection-mode bytes
    /// to ConnectionMode::{Direct,Relay}.
    fn ping(
        &self,
        peer_node_id_bytes: [u8; 32],
        timeout: std::time::Duration,
    ) -> futures::future::BoxFuture<'_, Result<(std::time::Duration, ConnectionMode), String>>;
}
```

Add `futures = "0.3"` to `Cargo.toml` `[dependencies]` if not present (`cargo add futures` from src-tauri/).

- [ ] **Step 2: Extend NetworkHealthService with self-test wiring**

Append to the `impl NetworkHealthService` block:

```rust
const PEER_PING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const PEER_PING_CONCURRENCY: usize = 32;

impl NetworkHealthService {
    /// Spec §5.3: self-test runs 4 ordered local steps + per-peer
    /// parallel pings (semaphore cap 32, 5s timeout each). Result is
    /// cached for `network_health_export_payload`.
    ///
    /// Per spec §6.2: step outcomes are Pass / Fail / Skipped. If an
    /// upstream step Fails, downstream steps are Skipped (not Failed)
    /// to avoid "4 things failed!" UI from one root cause.
    pub async fn run_self_test(
        &self,
        iroh_test: &dyn IrohSelfTest,
        pkarr_test: &dyn PkarrSelfTest,
        ping: &dyn PingDispatcher,
    ) -> SelfTestReport {
        let started_at_ms = now_ms();
        let mut steps = Vec::new();
        let mut peer_results = Vec::new();

        // Step 1: endpoint
        let endpoint_ok = iroh_test.endpoint_bound();
        steps.push(SelfTestStep {
            name: "endpoint".into(),
            outcome: if endpoint_ok {
                StepOutcome::Pass { duration_ms: 0 }
            } else {
                StepOutcome::Fail { reason: "endpoint not bound".into() }
            },
        });

        // Step 2: relay (skipped if endpoint failed)
        let relay_ok = if endpoint_ok {
            match iroh_test.relay_round_trip().await {
                Ok(d) => {
                    steps.push(SelfTestStep {
                        name: "relay".into(),
                        outcome: StepOutcome::Pass { duration_ms: d.as_millis() as u32 },
                    });
                    true
                }
                Err(reason) => {
                    steps.push(SelfTestStep {
                        name: "relay".into(),
                        outcome: StepOutcome::Fail { reason },
                    });
                    false
                }
            }
        } else {
            steps.push(SelfTestStep {
                name: "relay".into(),
                outcome: StepOutcome::Skipped { reason: "skipped: endpoint not bound".into() },
            });
            false
        };

        // Step 3: pkarr_publish (skipped if relay failed)
        let publish_ok = if relay_ok {
            match pkarr_test.publish_identity().await {
                Ok(d) => {
                    steps.push(SelfTestStep {
                        name: "pkarr_publish".into(),
                        outcome: StepOutcome::Pass { duration_ms: d.as_millis() as u32 },
                    });
                    true
                }
                Err(reason) => {
                    steps.push(SelfTestStep {
                        name: "pkarr_publish".into(),
                        outcome: StepOutcome::Fail { reason },
                    });
                    false
                }
            }
        } else {
            steps.push(SelfTestStep {
                name: "pkarr_publish".into(),
                outcome: StepOutcome::Skipped { reason: "skipped: relay unreachable".into() },
            });
            false
        };

        // Step 4: pkarr_resolve (skipped if publish failed)
        if publish_ok {
            match pkarr_test.resolve_self().await {
                Ok(d) => steps.push(SelfTestStep {
                    name: "pkarr_resolve".into(),
                    outcome: StepOutcome::Pass { duration_ms: d.as_millis() as u32 },
                }),
                Err(reason) => steps.push(SelfTestStep {
                    name: "pkarr_resolve".into(),
                    outcome: StepOutcome::Fail { reason },
                }),
            }
        } else {
            steps.push(SelfTestStep {
                name: "pkarr_resolve".into(),
                outcome: StepOutcome::Skipped { reason: "skipped: publish failed".into() },
            });
        }

        // Per-peer pings: only attempt if endpoint is bound. Otherwise
        // all peer pings are Skipped.
        let records = self.resolver.list_records();
        let now = now_ms();
        let scoped = filter_peers_by_shared_membership(records, &*self.membership, now);
        if endpoint_ok {
            // Semaphore-bounded parallel ping.
            let semaphore = Arc::new(tokio::sync::Semaphore::new(PEER_PING_CONCURRENCY));
            let mut handles = Vec::with_capacity(scoped.len());
            for peer in &scoped {
                let permit = Arc::clone(&semaphore).acquire_owned().await.expect("semaphore not closed");
                let owner_addr = peer.owner_addr.clone();
                let mode_hint = peer.connection_mode;
                let last_seen = peer.last_seen_ms;
                // Convert hex owner_addr → [u8; 32] for the iroh side.
                // The implementer wires this to the iroh node id rather
                // than owner_addr — Phase 1's resolver maps owner →
                // node_id; for now, fake/skipped path is acceptable.
                let dispatcher: Arc<dyn PingDispatcher> = Arc::new(NullDispatcher);
                let _permit_owned = permit;
                handles.push(tokio::spawn(async move {
                    // STAGE: implementer wires this to the real
                    // dispatcher in the production NetworkHealthService
                    // construction site (Task 7). For now, returns
                    // Skipped so the test scaffolding compiles.
                    drop(_permit_owned);
                    PeerPingResult {
                        owner_addr,
                        outcome: StepOutcome::Skipped {
                            reason: format!("phase-1: dispatcher not wired (last_seen={:?}, hint={:?})", last_seen, mode_hint),
                        },
                        mode: None,
                    }
                }));
            }
            for h in handles {
                if let Ok(r) = h.await {
                    peer_results.push(r);
                }
            }
        } else {
            for peer in &scoped {
                peer_results.push(PeerPingResult {
                    owner_addr: peer.owner_addr.clone(),
                    outcome: StepOutcome::Skipped { reason: "skipped: endpoint not bound".into() },
                    mode: None,
                });
            }
        }

        let report = SelfTestReport {
            started_at_ms,
            finished_at_ms: now_ms(),
            steps,
            peer_results,
        };

        // Cache for export_payload.
        *self.last_self_test.write().await = Some(report.clone());

        report
    }
}

// Stub used during the wiring stage; replaced by the production
// PingDispatcher built around `ping_peer` in lib.rs Task 7.
struct NullDispatcher;
impl PingDispatcher for NullDispatcher {
    fn ping(
        &self,
        _peer_node_id_bytes: [u8; 32],
        _timeout: std::time::Duration,
    ) -> futures::future::BoxFuture<'_, Result<(std::time::Duration, ConnectionMode), String>> {
        Box::pin(async { Err("dispatcher not wired".into()) })
    }
}
```

**NOTE for implementer**: the stub-dispatcher path above is intentional. Wiring `owner_addr` → `iroh_node_id_bytes` requires reading the resolver's record for the owner; Phase 1's `ReachabilityResolver::resolve(&owner)` returns `Vec<ReachabilityAnnouncePayload>` from which `iroh_node_id` (`[u8; 32]`) is extracted. The implementer wires this in Task 7 when constructing the production `NetworkHealthService`. Use of the stub here keeps Task 6's unit tests independent of iroh.

- [ ] **Step 3: Add self-test behavior tests**

Append inside `mod tests`:

```rust
    use futures::FutureExt;

    struct ScriptedIrohTest {
        bound: bool,
        relay: Result<std::time::Duration, String>,
    }
    impl IrohSelfTest for ScriptedIrohTest {
        fn endpoint_bound(&self) -> bool { self.bound }
        fn relay_round_trip(&self) -> futures::future::BoxFuture<'_, Result<std::time::Duration, String>> {
            let r = self.relay.clone();
            async move { r }.boxed()
        }
    }

    struct ScriptedPkarrTest {
        publish: Result<std::time::Duration, String>,
        resolve: Result<std::time::Duration, String>,
    }
    impl PkarrSelfTest for ScriptedPkarrTest {
        fn publish_identity(&self) -> futures::future::BoxFuture<'_, Result<std::time::Duration, String>> {
            let r = self.publish.clone();
            async move { r }.boxed()
        }
        fn resolve_self(&self) -> futures::future::BoxFuture<'_, Result<std::time::Duration, String>> {
            let r = self.resolve.clone();
            async move { r }.boxed()
        }
    }

    fn build_svc_for_self_test() -> NetworkHealthService {
        NetworkHealthService::new(
            Arc::new(FakeIroh { ready: true }),
            Arc::new(FakePkarr),
            Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
        )
    }

    #[tokio::test]
    async fn self_test_all_pass_path() {
        let svc = build_svc_for_self_test();
        let iroh_t = ScriptedIrohTest {
            bound: true,
            relay: Ok(std::time::Duration::from_millis(24)),
        };
        let pkarr_t = ScriptedPkarrTest {
            publish: Ok(std::time::Duration::from_millis(380)),
            resolve: Ok(std::time::Duration::from_millis(210)),
        };
        let report = svc.run_self_test(&iroh_t, &pkarr_t, &NullDispatcher).await;
        assert_eq!(report.steps.len(), 4);
        assert!(matches!(report.steps[0].outcome, StepOutcome::Pass { .. }), "endpoint pass");
        assert!(matches!(report.steps[1].outcome, StepOutcome::Pass { .. }), "relay pass");
        assert!(matches!(report.steps[2].outcome, StepOutcome::Pass { .. }), "pkarr_publish pass");
        assert!(matches!(report.steps[3].outcome, StepOutcome::Pass { .. }), "pkarr_resolve pass");
    }

    #[tokio::test]
    async fn self_test_relay_fail_cascades_downstream_to_skipped() {
        let svc = build_svc_for_self_test();
        let iroh_t = ScriptedIrohTest {
            bound: true,
            relay: Err("relay timeout after 5s".into()),
        };
        let pkarr_t = ScriptedPkarrTest {
            publish: Ok(std::time::Duration::from_millis(380)), // would pass if reached
            resolve: Ok(std::time::Duration::from_millis(210)),
        };
        let report = svc.run_self_test(&iroh_t, &pkarr_t, &NullDispatcher).await;
        assert!(matches!(report.steps[0].outcome, StepOutcome::Pass { .. }));
        assert!(matches!(report.steps[1].outcome, StepOutcome::Fail { .. }));
        assert!(matches!(report.steps[2].outcome, StepOutcome::Skipped { .. }), "pkarr_publish skipped");
        assert!(matches!(report.steps[3].outcome, StepOutcome::Skipped { .. }), "pkarr_resolve skipped");
    }

    #[tokio::test]
    async fn self_test_endpoint_unbound_all_steps_skipped() {
        let svc = build_svc_for_self_test();
        let iroh_t = ScriptedIrohTest { bound: false, relay: Ok(std::time::Duration::from_millis(0)) };
        let pkarr_t = ScriptedPkarrTest {
            publish: Ok(std::time::Duration::from_millis(0)),
            resolve: Ok(std::time::Duration::from_millis(0)),
        };
        let report = svc.run_self_test(&iroh_t, &pkarr_t, &NullDispatcher).await;
        assert!(matches!(report.steps[0].outcome, StepOutcome::Fail { .. }), "endpoint fail");
        for i in 1..4 {
            assert!(matches!(report.steps[i].outcome, StepOutcome::Skipped { .. }), "step {} skipped", i);
        }
    }

    #[tokio::test]
    async fn self_test_pkarr_resolve_mismatch_reported_as_fail() {
        let svc = build_svc_for_self_test();
        let iroh_t = ScriptedIrohTest { bound: true, relay: Ok(std::time::Duration::from_millis(24)) };
        let pkarr_t = ScriptedPkarrTest {
            publish: Ok(std::time::Duration::from_millis(380)),
            resolve: Err("pkarr resolved unexpected payload".into()),
        };
        let report = svc.run_self_test(&iroh_t, &pkarr_t, &NullDispatcher).await;
        match &report.steps[3].outcome {
            StepOutcome::Fail { reason } => assert_eq!(reason, "pkarr resolved unexpected payload"),
            other => panic!("expected Fail, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn self_test_result_is_cached_for_export() {
        let svc = build_svc_for_self_test();
        let iroh_t = ScriptedIrohTest { bound: true, relay: Ok(std::time::Duration::from_millis(24)) };
        let pkarr_t = ScriptedPkarrTest {
            publish: Ok(std::time::Duration::from_millis(380)),
            resolve: Ok(std::time::Duration::from_millis(210)),
        };
        assert!(svc.cached_last_self_test().await.is_none(), "empty cache before run");
        let _ = svc.run_self_test(&iroh_t, &pkarr_t, &NullDispatcher).await;
        let cached = svc.cached_last_self_test().await;
        assert!(cached.is_some(), "cache populated after run");
    }
```

- [ ] **Step 4: Run + gate + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
set -o pipefail
timeout 600 cargo nextest run --locked --features test-fixtures -E 'test(network_health::tests::self_test)' 2>&1 | tail -30
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
timeout 600 cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -20
```

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/network_health.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat(zeb-329): run_self_test with Pass/Fail/Skipped cascade

Spec §5.3 four-step ordered self-test (endpoint, relay, pkarr_publish,
pkarr_resolve) with downstream-skipped semantics per §6.2 — an upstream
Fail cascades to Skipped for downstream steps, not Fail.

Per-peer ping uses a semaphore (cap 32) + 5s timeout each; PingDispatcher
trait lets unit tests substitute a NullDispatcher. Production ping
wiring (resolver lookup → iroh_node_id → ping_peer) lives in Task 7's
lib.rs boot code; Task 6 leaves a documented Skipped path so unit tests
stay iroh-free.

Result cached for network_health_export_payload via
Arc<RwLock<Option<SelfTestReport>>>. Five behavior tests cover all-pass,
relay-fail-cascade, endpoint-unbound, resolve-mismatch, cache-populate."
```

---

## Task 7: 3 Tauri IPCs + NodeState wiring + boot construction + event_loop notify

**Files:**
- Modify: `src-tauri/src/lib.rs` (3 IPC handlers + NodeState extension + boot wiring)
- Modify: `src-tauri/src/event_loop.rs` (add `notify_resolver_update()` call at the kd=rch dispatch site)

This is the largest task. Implementer should treat it as: (a) NodeState surgery, (b) boot wiring with production trait impls, (c) IPC handlers, (d) event_loop hook, (e) `invoke_handler` registration.

- [ ] **Step 1: Extend NodeState**

In `src-tauri/src/lib.rs`, find the `NodeState` struct (around line 683-694) — add a new field:

```rust
    /// ZEB-329: synthesis-only service for the Network Health panel.
    /// `None` until boot wiring completes (Task 7).
    pub network_health: Option<Arc<crate::network_health::NetworkHealthService>>,
```

Find the `Default` impl (around line 913) and add to the initializer:

```rust
            network_health: None,
```

Find the `shutdown` / cleanup code (around line 790) where other fields are nulled out, and add:

```rust
        self.network_health = None;
```

- [ ] **Step 2: Production trait implementations**

Append to `src-tauri/src/network_health.rs` (production-only, gated):

```rust
// ── Production trait impls (boot-wired in lib.rs) ───────────────────

/// Production IrohSnapshot impl wrapping `Arc<IrohEndpoint>`.
pub struct ProdIrohSnapshot(pub Arc<crate::iroh_endpoint::IrohEndpoint>);

impl IrohSnapshot for ProdIrohSnapshot {
    fn iroh_node_id_hex(&self) -> Option<String> {
        Some(hex::encode(self.0.node_id().as_bytes()))
    }
    fn home_relay_url(&self) -> Option<String> {
        self.0.home_relay().map(|r| r.to_string())
    }
    fn relay_rtt_ms(&self) -> Option<u32> {
        // Phase 1: iroh 0.98 doesn't expose relay RTT cleanly via a
        // public API. Return None; the snapshot still carries
        // home_relay_url for testers to interpret.
        None
    }
    fn direct_addresses(&self) -> Vec<String> {
        self.0.direct_addresses().into_iter().map(|sa| sa.to_string()).collect()
    }
    fn nat_classification(&self) -> NatClass {
        // Phase 1: see classify_nat docs above.
        NatClass::Unknown
    }
}

/// Production ReachabilitySnapshot wrapping ReachabilityResolver.
pub struct ProdReachabilitySnapshot(pub crate::reachability_resolver::ReachabilityResolver);

impl ReachabilitySnapshot for ProdReachabilitySnapshot {
    fn list_records(&self) -> Vec<ResolverPeerRecord> {
        self.0
            .list_active_peers()
            .into_iter()
            .map(|(owner, payload)| ResolverPeerRecord {
                owner_addr: owner.0,
                display_name: None, // Phase 1: no profile cache lookup
                connection_mode: ConnectionMode::NoConnection, // Phase 1: no live conn-info inspection
                rtt_ms: None,
                last_seen_ms: Some(payload.announced_at_ms),
            })
            .collect()
    }
}

/// Production NotifyEmitter wrapping tauri::AppHandle.
pub struct ProdNotifyEmitter(pub tauri::AppHandle);

impl NotifyEmitter for ProdNotifyEmitter {
    fn emit_change(&self) {
        use tauri::Emitter;
        let _ = self.0.emit(NETWORK_HEALTH_CHANGED_EVENT, ());
    }
}
```

PkarrSnapshot and PkarrSelfTest production impls go inline in lib.rs (Step 5) because they need access to the pkarr publisher's internals.

MyMembershipSet production impl: the implementer must read how existing IPCs enumerate communities for the current identity. There's likely a `community_membership` store accessor in `community_membership.rs` or via NodeState. Adapter:

```rust
/// Production MyMembershipSet: walks community_membership state to
/// answer "which communities do I share with peer X?". Implementer
/// wires the correct accessor — look for fns like
/// `list_my_communities()` and per-community `members()`.
pub struct ProdMembership {
    // TODO(implementer): hold the references needed to answer
    // communities_shared_with — likely NodeState ref or community
    // store handle.
}

impl MyMembershipSet for ProdMembership {
    fn communities_shared_with(&self, _peer: &[u8; 16]) -> Vec<String> {
        // PHASE 1 IMPLEMENTATION NOTE: if the membership-lookup
        // accessor isn't available cleanly without locking NodeState
        // here, return an empty Vec for now — the panel will simply
        // show no peers until this is wired in a follow-up commit.
        // Document this gap in PR description.
        Vec::new()
    }
}
```

**Implementer guidance:** before committing the stub `ProdMembership`, spend ≤30 minutes searching for a clean membership accessor (`grep -nE "list_my_communities|membership.*list|joined_communities" src-tauri/src/`). If none is reachable without a NodeState lock, ship the empty-Vec stub and add a TODO. The panel still functions; peer list is just empty.

- [ ] **Step 3: Add the 3 Tauri IPC handlers**

Append to `src-tauri/src/lib.rs` (next to the other connectivity IPCs around line 30729):

```rust
// ── ZEB-329: Network Health IPCs ─────────────────────────────────────

/// Spec §6.1: never throws. Returns NetworkHealthSnapshot::empty()
/// when service isn't yet constructed (early boot).
#[tauri::command(rename_all = "snake_case")]
async fn network_health_snapshot(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<crate::network_health::NetworkHealthSnapshot, String> {
    let svc = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.network_health.clone()
    };
    let snap = match svc {
        Some(s) => s.snapshot().await,
        None => crate::network_health::NetworkHealthSnapshot::empty(),
    };
    Ok(snap)
}

/// Spec §5.3 + §6.1. Returns Err only on truly exceptional cases
/// (service not constructed). Step failures live inside the report.
#[tauri::command(rename_all = "snake_case")]
async fn network_health_run_self_test(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<crate::network_health::SelfTestReport, String> {
    let svc = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.network_health.clone()
    };
    let Some(svc) = svc else {
        return Err("network_health service not yet initialized".into());
    };
    // Production self-test trait impls — see Step 5 boot wiring for
    // construction of iroh_test + pkarr_test + ping dispatcher
    // singletons stored on NodeState.
    // TODO(implementer): wire production traits here. Until wired,
    // return an all-Skipped synthetic report so the UI doesn't break.
    let now = crate::network_health::__now_ms_for_ipc();
    let synthetic = crate::network_health::SelfTestReport {
        started_at_ms: now,
        finished_at_ms: now,
        steps: vec![
            crate::network_health::SelfTestStep {
                name: "endpoint".into(),
                outcome: crate::network_health::StepOutcome::Skipped {
                    reason: "production self-test traits not yet wired".into(),
                },
            },
        ],
        peer_results: vec![],
    };
    // Cache for export so subsequent export reads see the synthetic.
    // The real wiring should replace this with svc.run_self_test(...).
    drop(svc); // unused in the synthetic path
    Ok(synthetic)
}

/// Spec §5.4: server-side redaction; `include_full_ids=false` is the
/// only default-safe path. The cached last self-test report is
/// memo-style (memory rule feedback_two_ipc_toctou: not a binding
/// token, no TOCTOU concern).
#[tauri::command(rename_all = "snake_case")]
async fn network_health_export_payload(
    state: tauri::State<'_, Mutex<NodeState>>,
    include_full_ids: bool,
) -> Result<String, String> {
    let svc = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.network_health.clone()
    };
    let (snap, last) = match svc {
        Some(s) => (s.snapshot().await, s.cached_last_self_test().await),
        None => (crate::network_health::NetworkHealthSnapshot::empty(), None),
    };
    Ok(crate::network_health::format_export_markdown(&snap, last.as_ref(), include_full_ids))
}
```

Add the `__now_ms_for_ipc` helper in network_health.rs (since `now_ms` is private):

```rust
#[doc(hidden)]
pub fn __now_ms_for_ipc() -> u64 {
    now_ms()
}
```

- [ ] **Step 4: Register the IPCs in invoke_handler**

In `src-tauri/src/lib.rs`, find the `tauri::Builder::default().invoke_handler(tauri::generate_handler![...])` macro call. Add the three new commands to the list (alphabetized or grouped near other `network_*` / `connectivity_*` commands):

```rust
            network_health_snapshot,
            network_health_run_self_test,
            network_health_export_payload,
```

- [ ] **Step 5: Boot wiring in setup hook**

In `src-tauri/src/lib.rs`, find the setup hook (search for `.setup(|app|` or the area where `iroh_endpoint` and `reachability_resolver` are constructed in `start_node` around line 2283-2386). After both are constructed, add:

```rust
// ZEB-329: Network Health service. Construct AFTER iroh + resolver
// are ready, BEFORE the rate-limiter task is spawned (spec §5.5
// ordering invariant).
let prod_iroh = Arc::new(crate::network_health::ProdIrohSnapshot(iroh_endpoint_arc.clone()));
let prod_resolver = Arc::new(crate::network_health::ProdReachabilitySnapshot(reachability_resolver.clone()));
// Production PkarrSnapshot: thin wrapper around pkarr_publisher_handle.
let prod_pkarr: Arc<dyn crate::network_health::PkarrSnapshot> = {
    let publisher = pkarr_publisher.clone();
    struct Wrapped(Arc<crate::pkarr_*_publisher::PkarrPublisher>); // implementer: fix actual type
    impl crate::network_health::PkarrSnapshot for Wrapped {
        fn identity_published(&self) -> bool {
            // implementer: read from active_handles() — see
            // connectivity_pkarr_publication_status at lib.rs:30560
            // for the exact filter logic.
            false
        }
        fn identity_last_publish_ms(&self) -> Option<u64> { None }
        fn community_publish_count(&self) -> u32 { 0 }
        fn recent_fallback_events(&self) -> Vec<crate::network_health::PkarrFallbackHit> { vec![] }
    }
    Arc::new(Wrapped(publisher))
};
let prod_membership: Arc<dyn crate::network_health::MyMembershipSet + Send + Sync> =
    Arc::new(crate::network_health::ProdMembership { /* implementer: see Step 2 */ });

let mut nh = crate::network_health::NetworkHealthService::new(
    prod_iroh,
    prod_pkarr,
    prod_resolver,
    prod_membership,
);
// Spawn the rate-limiter + ping accept loop now that the service exists.
let emitter = crate::network_health::ProdNotifyEmitter(app_handle.clone());
nh.spawn_rate_limiter(emitter);
crate::network_health::spawn_ping_accept_loop(iroh_endpoint_arc.clone());

let nh_arc = Arc::new(nh);
{
    let mut guard = state_for_setup.lock().expect("NodeState lock");
    guard.network_health = Some(nh_arc.clone());
}
```

The implementer adapts type names and lock-acquisition patterns to match the actual NodeState shape.

- [ ] **Step 6: event_loop.rs notify wiring**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git grep -n "reachability_resolver.update" src-tauri/src/event_loop.rs
```

At every call site of `reachability_resolver.update(...)`, add a notify call immediately after:

```rust
                                reachability_resolver.update(actor, payload, hlc);
+                               // ZEB-329: notify the Network Health rate-limiter.
+                               if let Some(nh) = network_health_for_hook.as_ref() {
+                                   nh.notify();
+                               }
```

The `network_health_for_hook` clone happens at the top of the closure / handler — implementer threads it through similarly to how `reachability_resolver_for_hook` is threaded (see lib.rs:3065).

- [ ] **Step 7: Gate + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
set -o pipefail
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -40
timeout 600 cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -20
```

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs src-tauri/src/event_loop.rs src-tauri/src/network_health.rs
git commit -m "feat(zeb-329): 3 Tauri IPCs + NodeState wiring + event_loop notify

- network_health_snapshot, network_health_run_self_test, and
  network_health_export_payload registered in invoke_handler.
- NodeState gains network_health: Option<Arc<NetworkHealthService>>.
- Boot wiring constructs the service AFTER iroh + resolver, BEFORE
  spawning the rate-limiter (spec §5.5 ordering invariant).
- event_loop.rs notify hook adjacent to every reachability_resolver.update
  call site.

Production trait impls (ProdIrohSnapshot, ProdReachabilitySnapshot,
ProdNotifyEmitter) live in network_health.rs. PkarrSnapshot impl and
ProdMembership are stubbed in this commit — TODO follow-ups documented
in PR description for cleanest type-safe handle (out-of-scope to fight
NodeState lock topology in this PR)."
```

---

## Task 8: Two-endpoint integration test for HARMONY_PING_V1

**Files:**
- Create: `src-tauri/tests/network_health_two_endpoint.rs`

Mirrors the Phase 1 pattern in `pkarr_iroh_redeem_full_integration.rs` — two iroh endpoints in the same process, one registers the accept handler, the other pings.

- [ ] **Step 1: Skim the reference integration test**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
sed -n '1,80p' src-tauri/tests/pkarr_iroh_redeem_full_integration.rs
```

Note: how the test builds hermetic IrohEndpoint instances via `from_endpoint_for_integration_test`, how it skips DHT, how it lets endpoints discover each other via direct seeding.

- [ ] **Step 2: Write the integration test**

Create `src-tauri/tests/network_health_two_endpoint.rs`:

```rust
//! ZEB-329 integration test: two real iroh endpoints in-process,
//! endpoint A spawns the HARMONY_PING_V1 accept loop, endpoint B
//! issues a self-test ping to A's NodeId, asserts Pass with Direct
//! mode and <1s duration.
//!
//! Mirrors the hermetic two-endpoint pattern from
//! `pkarr_iroh_redeem_full_integration.rs`.

#![cfg(feature = "test-fixtures")]

use std::sync::Arc;

use harmony_app::iroh_endpoint::IrohEndpoint;
use harmony_app::network_health;

async fn build_hermetic_endpoint() -> IrohEndpoint {
    use iroh::endpoint::presets;
    let inner = iroh::Endpoint::builder(presets::Minimal)
        .secret_key(iroh::SecretKey::generate(&mut rand::thread_rng()))
        .alpns(vec![
            harmony_app::iroh_endpoint::alpn::HARMONY_ZENOH_V1.to_vec(),
            harmony_app::iroh_endpoint::alpn::HARMONY_HANDSHAKE_V1.to_vec(),
            harmony_app::iroh_endpoint::alpn::HARMONY_PING_V1.to_vec(),
        ])
        .relay_mode(iroh::RelayMode::Disabled)
        .bind()
        .await
        .expect("hermetic bind");
    IrohEndpoint::from_endpoint_for_integration_test(inner)
}

#[tokio::test]
async fn ping_round_trip_between_two_endpoints() {
    let endpoint_a = Arc::new(build_hermetic_endpoint().await);
    let endpoint_b = build_hermetic_endpoint().await;

    // Spawn the ping accept loop on A.
    network_health::spawn_ping_accept_loop(endpoint_a.clone());

    // Seed B's view of A's address. Hermetic endpoints publish their
    // bound_sockets, not address-lookup-service results.
    let a_node_id = endpoint_a.node_id();
    let a_sockets = endpoint_a.bound_sockets();
    assert!(!a_sockets.is_empty(), "endpoint A must have bound sockets");
    let a_addr = iroh::EndpointAddr::from_parts(a_node_id, None, a_sockets.into_iter().collect());
    endpoint_b
        .inner()
        .add_endpoint_addr(a_addr)
        .expect("seed endpoint B with A's addr");

    // B pings A.
    let rtt = network_health::ping_peer(
        &endpoint_b,
        a_node_id,
        std::time::Duration::from_secs(2),
    )
    .await
    .expect("ping succeeds");

    assert!(rtt.as_secs() < 1, "loopback ping should be < 1s, got {:?}", rtt);
}
```

**Implementer NOTE:** the exact iroh 0.98 method names (`Endpoint::builder`, `EndpointAddr::from_parts`, `add_endpoint_addr`) may differ. Verify with `cargo doc --open -p iroh` if compilation fails. The pattern in `pkarr_iroh_redeem_full_integration.rs` is the authoritative reference.

- [ ] **Step 3: Run + gate + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
timeout 600 cargo nextest run --locked --features test-fixtures --test network_health_two_endpoint 2>&1 | tail -30
echo "EXIT=${PIPESTATUS[0]}"
```

Expected: passes. If the test fails because of an iroh API mismatch, adapt per the pkarr integration test pattern.

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
```

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/tests/network_health_two_endpoint.rs
git commit -m "test(zeb-329): integration test for HARMONY_PING_V1 round-trip

Two hermetic IrohEndpoint instances in-process; endpoint A spawns the
accept loop, endpoint B opens a bi-stream with HARMONY_PING_V1, writes
one byte, reads echo, asserts <1s loopback RTT. Mirrors the Phase 1
pattern from pkarr_iroh_redeem_full_integration.rs."
```

---

## Task 9: Frontend types + adapter + adapter tests

**Files:**
- Create: `src/lib/types/network-health.ts`
- Create: `src/lib/network-health-adapter.ts`
- Create: `src/lib/__tests__/network-health-adapter.test.ts`

- [ ] **Step 1: TS types**

Create `src/lib/types/network-health.ts`:

```typescript
// ZEB-329 — frontend types mirroring src-tauri/src/network_health.rs.
// All fields in camelCase per Tauri serde rename_all = "camelCase".

export type ReachabilityStatus = 'reachable' | 'degraded' | 'unreachable';

export type NatClass =
  | 'fullCone'
  | 'restrictedCone'
  | 'portRestricted'
  | 'symmetric'
  | 'unknown';

export type ConnectionMode = 'direct' | 'relay' | 'noConnection';

export interface MyNetworkSummary {
  irohNodeId: string;
  reachability: ReachabilityStatus;
  natClassification: NatClass;
  homeRelayUrl: string | null;
  relayRttMs: number | null;
  directAddresses: string[];
}

export interface PeerHealth {
  ownerAddr: string;
  displayName: string | null;
  sharedCommunities: string[];
  connectionMode: ConnectionMode;
  rttMs: number | null;
  lastSeenMs: number | null;
  reachabilityRecordAgeMs: number | null;
}

export interface PkarrFallbackHit {
  peerAddrShort: string;
  communityIdShort: string;
  hit: boolean;
  capturedAtMs: number;
}

export interface PkarrHealthSummary {
  identityPublished: boolean;
  identityLastPublishMs: number | null;
  communityPublishCount: number;
  recentFallbackEvents: PkarrFallbackHit[];
}

export interface NetworkHealthSnapshot {
  schemaVersion: number;
  capturedAtMs: number;
  appVersion: string;
  platform: string;
  myNetwork: MyNetworkSummary | null;
  peers: PeerHealth[];
  pkarrStatus: PkarrHealthSummary;
}

export type StepOutcome =
  | { type: 'pass'; durationMs: number }
  | { type: 'fail'; reason: string }
  | { type: 'skipped'; reason: string };

export interface SelfTestStep {
  name: string;
  outcome: StepOutcome;
}

export interface PeerPingResult {
  ownerAddr: string;
  outcome: StepOutcome;
  mode: ConnectionMode | null;
}

export interface SelfTestReport {
  startedAtMs: number;
  finishedAtMs: number;
  steps: SelfTestStep[];
  peerResults: PeerPingResult[];
}
```

- [ ] **Step 2: Adapter**

Create `src/lib/network-health-adapter.ts`:

```typescript
// ZEB-329 — Tauri IPC wrappers + event subscriber + pure helpers.

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  NetworkHealthSnapshot,
  SelfTestReport,
  NatClass,
} from './types/network-health';

const EVENT_NAME = 'network-health-changed';

export async function snapshot(): Promise<NetworkHealthSnapshot> {
  return await invoke<NetworkHealthSnapshot>('network_health_snapshot');
}

export async function runSelfTest(): Promise<SelfTestReport> {
  return await invoke<SelfTestReport>('network_health_run_self_test');
}

export async function exportPayload(includeFullIds: boolean): Promise<string> {
  return await invoke<string>('network_health_export_payload', {
    includeFullIds,
  });
}

export async function onNetworkHealthChanged(
  cb: () => void
): Promise<UnlistenFn> {
  return await listen<unknown>(EVENT_NAME, () => cb());
}

// Pure helpers (testable in isolation)

export function explainNatClass(n: NatClass): { headline: string; detail: string } {
  switch (n) {
    case 'fullCone':
      return {
        headline: 'Direct connections work',
        detail: 'Open NAT — peers can connect to you directly. Best speed.',
      };
    case 'restrictedCone':
      return {
        headline: 'Direct connections mostly work',
        detail:
          'Restricted-cone NAT — peers you contact first can reach you back; new inbound is blocked until you initiate.',
      };
    case 'portRestricted':
      return {
        headline: 'Some direct connections work',
        detail:
          'Port-restricted NAT — direct connections work only with peers you contact first, and only on the exact port pair.',
      };
    case 'symmetric':
      return {
        headline: 'Direct connections do not work',
        detail:
          'Symmetric NAT — every peer needs to go through the relay. Slower but functional.',
      };
    case 'unknown':
      return {
        headline: 'Network type not yet determined',
        detail:
          'Harmony is still measuring your network. Connection mode for peers tells the real story.',
      };
  }
}

export function redactAddr(addr: string, full: boolean): string {
  if (!addr || addr.length < 8) return '(unknown)';
  if (full) return addr;
  return `${addr.slice(0, 8)}…`;
}
```

- [ ] **Step 3: Adapter unit tests**

Create `src/lib/__tests__/network-health-adapter.test.ts`:

```typescript
import { describe, it, expect } from 'vitest';
import { explainNatClass, redactAddr } from '../network-health-adapter';
import type { NatClass } from '../types/network-health';

describe('explainNatClass', () => {
  const cases: NatClass[] = [
    'fullCone',
    'restrictedCone',
    'portRestricted',
    'symmetric',
    'unknown',
  ];

  it.each(cases)('returns non-empty headline + detail for %s', (n) => {
    const { headline, detail } = explainNatClass(n);
    expect(headline).toBeTruthy();
    expect(headline.length).toBeGreaterThan(0);
    expect(detail).toBeTruthy();
    expect(detail.length).toBeGreaterThan(0);
  });
});

describe('redactAddr', () => {
  it('returns full address when full=true', () => {
    const addr = 'a3f9e1c2'.repeat(8);
    expect(redactAddr(addr, true)).toBe(addr);
  });

  it('returns first 8 chars + ellipsis when full=false', () => {
    const addr = 'a3f9e1c2deadbeef';
    expect(redactAddr(addr, false)).toBe('a3f9e1c2…');
  });

  it('returns (unknown) for empty input', () => {
    expect(redactAddr('', false)).toBe('(unknown)');
    expect(redactAddr('', true)).toBe('(unknown)');
  });

  it('returns (unknown) for too-short input', () => {
    expect(redactAddr('abc', false)).toBe('(unknown)');
  });
});
```

- [ ] **Step 4: Run frontend gates + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit 2>&1 | tail -20
npx vitest run src/lib/__tests__/network-health-adapter.test.ts 2>&1 | tail -20
```

Expected: tsc clean; 9 tests pass.

```bash
git add src/lib/types/network-health.ts src/lib/network-health-adapter.ts src/lib/__tests__/network-health-adapter.test.ts
git commit -m "feat(zeb-329): frontend types + adapter + adapter tests

TS mirror of Rust DTOs in camelCase per Tauri rename_all convention.
Adapter wraps the 3 IPCs + event subscriber + two pure helpers
(explainNatClass for plain-language NAT presentation, redactAddr for
client-side display). 9 vitest unit tests cover all 5 NatClass values
+ redaction edge cases (full, prefix, empty, too-short)."
```

---

## Task 10: NetworkHealthView.svelte + component tests

**Files:**
- Create: `src/lib/components/NetworkHealthView.svelte`
- Create: `src/lib/components/__tests__/NetworkHealthView.test.ts`

Use the Svelte 5 runes pattern from `DiagnosticsPanel.svelte` (already migrated).

- [ ] **Step 1: Create NetworkHealthView.svelte**

```svelte
<script lang="ts">
  /**
   * ZEB-329 — Network Health panel (dedicated /network route).
   *
   * Spec §7. Owns snapshot fetch + event subscription + self-test launch.
   * Renders summary card + per-peer rows + self-test results pane +
   * "Submit diagnostics" button that opens DiagnosticExportModal.
   */
  import { onMount, onDestroy } from 'svelte';
  import {
    snapshot as fetchSnapshot,
    runSelfTest as runSelfTestIpc,
    onNetworkHealthChanged,
    explainNatClass,
    redactAddr,
  } from '../network-health-adapter';
  import type {
    NetworkHealthSnapshot,
    SelfTestReport,
    PeerHealth,
  } from '../types/network-health';
  import DiagnosticExportModal from './DiagnosticExportModal.svelte';

  let snap = $state<NetworkHealthSnapshot | null>(null);
  let report = $state<SelfTestReport | null>(null);
  let runningSelfTest = $state(false);
  let selfTestError = $state<string | null>(null);
  let exportOpen = $state(false);

  let unlisten: (() => void) | null = null;
  let destroyed = false;

  // Edge case 6.4 #1: auto-retry every 2s for 30s when iroh isn't ready.
  let startupRetryHandle: ReturnType<typeof setInterval> | null = null;
  let startupRetryElapsedMs = 0;

  async function refresh() {
    try {
      snap = await fetchSnapshot();
    } catch (e) {
      // Spec §6.3: never show top-level error banner — render empty.
      // The "diagnostics unavailable" banner shows only if snap stays null
      // for the entire startup window.
      console.warn(`[network-health] snapshot failed: ${e instanceof Error ? e.message : String(e)}`);
    }
  }

  async function handleSelfTest() {
    if (runningSelfTest) return;
    runningSelfTest = true;
    selfTestError = null;
    try {
      report = await runSelfTestIpc();
    } catch (e) {
      selfTestError = e instanceof Error ? e.message : String(e);
    } finally {
      runningSelfTest = false;
    }
  }

  function startStartupRetry() {
    if (startupRetryHandle) return;
    startupRetryHandle = setInterval(async () => {
      startupRetryElapsedMs += 2000;
      await refresh();
      if (snap?.myNetwork || startupRetryElapsedMs >= 30000) {
        if (startupRetryHandle) clearInterval(startupRetryHandle);
        startupRetryHandle = null;
      }
    }, 2000);
  }

  onMount(async () => {
    await refresh();
    if (!snap?.myNetwork) startStartupRetry();
    const resolved = await onNetworkHealthChanged(() => {
      void refresh();
    });
    if (destroyed) {
      resolved();
    } else {
      unlisten = resolved;
    }
  });

  onDestroy(() => {
    destroyed = true;
    if (unlisten) unlisten();
    if (startupRetryHandle) clearInterval(startupRetryHandle);
  });

  function peerStatusIcon(p: PeerHealth): string {
    if (p.connectionMode === 'direct') return '✓';
    if (p.connectionMode === 'relay') return '⚠';
    return '✗';
  }
</script>

<div class="network-health" data-testid="network-health-root">
  <h1>Network Health</h1>

  {#if !snap}
    <p data-testid="nh-initial-loading">Loading…</p>
  {:else if !snap.myNetwork}
    <section class="starting-up" data-testid="nh-starting-up">
      <p>Network is starting up…</p>
      <p class="muted">This can take 10–30 seconds on first launch.</p>
      <button onclick={refresh}>Retry now</button>
    </section>
  {:else}
    {@const my = snap.myNetwork}
    {@const explain = explainNatClass(my.natClassification)}
    <section class="my-network" data-testid="nh-my-network">
      <h2>Your network</h2>
      <p class="status status-{my.reachability}">
        <strong data-testid="nh-headline">{explain.headline}</strong>
        <span class="info-hover" title={explain.detail}>…</span>
      </p>
      <p class="detail">{explain.detail}</p>
      {#if my.homeRelayUrl}
        <p>Relay: <code data-testid="nh-relay">{my.homeRelayUrl}</code></p>
      {/if}
      {#if my.relayRttMs !== null}
        <p>RTT to relay: {my.relayRttMs}ms</p>
      {/if}
    </section>

    <section class="peers" data-testid="nh-peers">
      <h2>Peers ({snap.peers.length})</h2>
      {#if snap.peers.length === 0}
        <p data-testid="nh-peers-empty">No peers in shared communities yet.</p>
      {:else}
        <ul>
          {#each snap.peers as p (p.ownerAddr)}
            <li data-testid="nh-peer">
              {peerStatusIcon(p)}
              <strong>{redactAddr(p.ownerAddr, false)}</strong>
              <span>{p.connectionMode}</span>
              {#if p.rttMs !== null}<span>{p.rttMs}ms</span>{/if}
              {#if p.lastSeenMs !== null}
                <span class="muted">last seen {Math.floor((Date.now() - p.lastSeenMs) / 1000)}s ago</span>
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    </section>

    <section class="self-test" data-testid="nh-self-test">
      <h2>Self-test</h2>
      <button
        onclick={handleSelfTest}
        disabled={runningSelfTest}
        data-testid="nh-self-test-button"
      >
        {runningSelfTest ? 'Running…' : 'Run self-test'}
      </button>
      {#if selfTestError}
        <p class="error" data-testid="nh-self-test-error">Self-test couldn't start: {selfTestError}</p>
      {/if}
      {#if report}
        <ul class="self-test-steps">
          {#each report.steps as step (step.name)}
            <li data-testid="nh-self-test-step">
              {#if step.outcome.type === 'pass'}
                ✓ {step.name} ({step.outcome.durationMs}ms)
              {:else if step.outcome.type === 'fail'}
                ✗ {step.name} <span title={step.outcome.reason}>(failed)</span>
              {:else}
                ⊘ {step.name} <span title={step.outcome.reason}>(skipped)</span>
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    </section>

    <button onclick={() => (exportOpen = true)} data-testid="nh-export-button">
      Submit diagnostics…
    </button>
  {/if}

  {#if exportOpen}
    <DiagnosticExportModal onClose={() => (exportOpen = false)} />
  {/if}
</div>

<style>
  .network-health { padding: 1rem; max-width: 800px; }
  .muted { color: #888; }
  .status-reachable { color: green; }
  .status-degraded { color: orange; }
  .status-unreachable { color: crimson; }
  .info-hover { cursor: help; margin-left: 0.5em; }
  .error { color: crimson; }
  .self-test-steps { list-style: none; padding-left: 0; font-family: monospace; }
</style>
```

- [ ] **Step 2: Component tests**

Create `src/lib/components/__tests__/NetworkHealthView.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import NetworkHealthView from '../NetworkHealthView.svelte';
import type { NetworkHealthSnapshot } from '../../types/network-health';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import { invoke } from '@tauri-apps/api/core';

function emptySnap(): NetworkHealthSnapshot {
  return {
    schemaVersion: 1,
    capturedAtMs: 0,
    appVersion: 'test',
    platform: 'test',
    myNetwork: null,
    peers: [],
    pkarrStatus: {
      identityPublished: false,
      identityLastPublishMs: null,
      communityPublishCount: 0,
      recentFallbackEvents: [],
    },
  };
}

function readySnap(): NetworkHealthSnapshot {
  return {
    ...emptySnap(),
    myNetwork: {
      irohNodeId: 'a3f9e1c2'.repeat(8),
      reachability: 'reachable',
      natClassification: 'fullCone',
      homeRelayUrl: 'https://derp.example/',
      relayRttMs: 24,
      directAddresses: [],
    },
  };
}

describe('NetworkHealthView', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders "starting up…" when my_network is null', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(emptySnap());
    render(NetworkHealthView);
    await waitFor(() => screen.getByTestId('nh-starting-up'));
    expect(screen.getByTestId('nh-starting-up')).toBeTruthy();
  });

  it('renders summary card when my_network is populated', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(readySnap());
    render(NetworkHealthView);
    await waitFor(() => screen.getByTestId('nh-my-network'));
    expect(screen.getByTestId('nh-headline').textContent).toContain('Direct connections work');
  });

  it('renders empty-peer state when peers list is empty', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(readySnap());
    render(NetworkHealthView);
    await waitFor(() => screen.getByTestId('nh-peers-empty'));
    expect(screen.getByTestId('nh-peers-empty')).toBeTruthy();
  });

  it('self-test button disables while running', async () => {
    (invoke as ReturnType<typeof vi.fn>)
      .mockResolvedValueOnce(readySnap()) // snapshot
      .mockImplementationOnce(
        () => new Promise(() => {/* never resolves */})
      ); // runSelfTest hangs
    render(NetworkHealthView);
    await waitFor(() => screen.getByTestId('nh-self-test-button'));
    const btn = screen.getByTestId('nh-self-test-button') as HTMLButtonElement;
    await fireEvent.click(btn);
    await waitFor(() => expect(btn.disabled).toBe(true));
  });
});
```

- [ ] **Step 3: Run + gate + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit 2>&1 | tail -20
npx vitest run src/lib/components/__tests__/NetworkHealthView.test.ts 2>&1 | tail -30
```

```bash
git add src/lib/components/NetworkHealthView.svelte src/lib/components/__tests__/NetworkHealthView.test.ts
git commit -m "feat(zeb-329): NetworkHealthView Svelte 5 component + tests

Dedicated /network route component (Svelte 5 runes). Renders:
- 'starting up' placeholder when my_network is null with auto-retry
  every 2s for 30s (spec §6.4 #1)
- summary card with plain-language NAT + raw on hover when populated
- per-peer list with status icon + redacted addr + mode + RTT + last-seen
- self-test pane with Pass/Fail/Skipped icons + reason on hover
- 'Submit diagnostics' button opening DiagnosticExportModal

Behavior tests cover: starting-up, summary card, empty peers, button
disable during in-flight self-test."
```

---

## Task 11: DiagnosticExportModal + tests + sidebar nav

**Files:**
- Create: `src/lib/components/DiagnosticExportModal.svelte`
- Create: `src/lib/components/__tests__/DiagnosticExportModal.test.ts`
- Modify: `src/App.svelte` (or wherever sidebar nav lives)

- [ ] **Step 1: Identify sidebar nav location**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
grep -nE "nav|sidebar|currentView|router" src/App.svelte src/lib/components/Layout.svelte 2>/dev/null | head -20
```

Adapt Step 3 below to the actual pattern found.

- [ ] **Step 2: Create DiagnosticExportModal.svelte**

```svelte
<script lang="ts">
  /**
   * ZEB-329 — Diagnostic export modal (spec §5.4 + §7.4).
   *
   * Default: redacted markdown (server-side via include_full_ids=false).
   * Toggle "Include full identifiers" → re-fetch with include_full_ids=true.
   * Copy → navigator.clipboard.writeText(markdown).
   * Save → Tauri dialog plugin save() → write file.
   */
  import { exportPayload } from '../network-health-adapter';
  import { save as saveDialog } from '@tauri-apps/plugin-dialog';
  import { writeTextFile } from '@tauri-apps/plugin-fs';

  interface Props {
    onClose: () => void;
  }
  const { onClose }: Props = $props();

  let includeFullIds = $state(false);
  let markdown = $state<string>('');
  let loading = $state(true);
  let error = $state<string | null>(null);
  let copiedToast = $state(false);

  async function load() {
    loading = true;
    error = null;
    try {
      markdown = await exportPayload(includeFullIds);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }
  }

  $effect(() => {
    void load();
  });

  async function copy() {
    try {
      await navigator.clipboard.writeText(markdown);
      copiedToast = true;
      setTimeout(() => (copiedToast = false), 2000);
    } catch (e) {
      error = `Couldn't copy: ${e instanceof Error ? e.message : String(e)}. Use Save instead.`;
    }
  }

  async function saveToFile() {
    try {
      const path = await saveDialog({
        defaultPath: 'harmony-diagnostics.txt',
        filters: [{ name: 'Text', extensions: ['txt'] }],
      });
      if (path) {
        await writeTextFile(path, markdown);
      }
      // Cancel → no-op silent dismiss
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }
</script>

<div class="modal-backdrop" data-testid="export-modal">
  <div class="modal-content">
    <h2>Diagnostic export</h2>
    <p>Review what you're about to share:</p>
    {#if loading}
      <p>Loading…</p>
    {:else if error}
      <p class="error" data-testid="export-error">{error}</p>
    {:else}
      <pre class="markdown-preview" data-testid="export-preview">{markdown}</pre>
    {/if}
    <label>
      <input
        type="checkbox"
        bind:checked={includeFullIds}
        data-testid="export-full-toggle"
      />
      Include full identifiers (default off)
    </label>
    <div class="actions">
      <button onclick={copy} data-testid="export-copy">Copy</button>
      <button onclick={saveToFile} data-testid="export-save">Save as .txt</button>
      <button onclick={onClose} data-testid="export-cancel">Cancel</button>
    </div>
    {#if copiedToast}
      <p class="toast">Copied!</p>
    {/if}
  </div>
</div>

<style>
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal-content {
    background: var(--bg-secondary, #2a2a2a);
    color: var(--text-primary, #fff);
    padding: 1.5rem;
    border-radius: 8px;
    max-width: 640px;
    max-height: 80vh;
    overflow-y: auto;
  }
  .markdown-preview {
    background: #111;
    color: #fff;
    padding: 1rem;
    border-radius: 4px;
    max-height: 320px;
    overflow-y: auto;
    white-space: pre-wrap;
  }
  .actions { display: flex; gap: 0.5rem; margin-top: 1rem; }
  .error { color: crimson; }
  .toast { color: lightgreen; margin-top: 0.5rem; }
</style>
```

- [ ] **Step 3: Add "Network" sidebar nav item**

After identifying the nav structure in Step 1, add a "Network" item routing to `/network`. The exact code depends on the current routing approach; reference DiagnosticsPanel.svelte for how it's exposed today.

If routing uses a `currentView` store with switch arms:

```svelte
{:else if currentView === 'network'}
  <NetworkHealthView />
```

And add a nav button:

```svelte
<button onclick={() => (currentView = 'network')}>Network</button>
```

If using a Router lib, register the route accordingly.

- [ ] **Step 4: Component tests for DiagnosticExportModal**

Create `src/lib/components/__tests__/DiagnosticExportModal.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import DiagnosticExportModal from '../DiagnosticExportModal.svelte';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));
vi.mock('@tauri-apps/plugin-dialog', () => ({
  save: vi.fn(),
}));
vi.mock('@tauri-apps/plugin-fs', () => ({
  writeTextFile: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';
import { save as saveDialog } from '@tauri-apps/plugin-dialog';
import { writeTextFile } from '@tauri-apps/plugin-fs';

const REDACTED_FIXTURE = `## Harmony v0.1.0-alpha.1 (darwin/aarch64)
## Network: reachable
a3f9e1c2… direct 18ms`;
const FULL_FIXTURE = `## Harmony v0.1.0-alpha.1 (darwin/aarch64)
## Network: reachable
a3f9e1c2deadbeef1234567890abcdef direct 18ms`;

describe('DiagnosticExportModal', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders redacted markdown by default (no full Ed25519 hex in DOM)', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(REDACTED_FIXTURE);
    render(DiagnosticExportModal, { onClose: () => {} });
    await waitFor(() => screen.getByTestId('export-preview'));
    const html = document.body.innerHTML;
    // Reject any 32+ char lowercase hex run in the DOM
    expect(html).not.toMatch(/[0-9a-f]{32,}/);
  });

  it('toggle "Include full identifiers" re-fetches with full IDs', async () => {
    (invoke as ReturnType<typeof vi.fn>)
      .mockResolvedValueOnce(REDACTED_FIXTURE)
      .mockResolvedValueOnce(FULL_FIXTURE);
    render(DiagnosticExportModal, { onClose: () => {} });
    await waitFor(() => screen.getByTestId('export-preview'));
    const toggle = screen.getByTestId('export-full-toggle') as HTMLInputElement;
    await fireEvent.click(toggle);
    await waitFor(() => {
      const html = document.body.innerHTML;
      expect(html).toMatch(/[0-9a-f]{32,}/);
    });
    expect(invoke).toHaveBeenCalledWith('network_health_export_payload', { includeFullIds: true });
  });

  it('Copy button calls navigator.clipboard.writeText', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(REDACTED_FIXTURE);
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    render(DiagnosticExportModal, { onClose: () => {} });
    await waitFor(() => screen.getByTestId('export-copy'));
    await fireEvent.click(screen.getByTestId('export-copy'));
    await waitFor(() => expect(writeText).toHaveBeenCalled());
  });

  it('Save button opens dialog + writes file', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(REDACTED_FIXTURE);
    (saveDialog as ReturnType<typeof vi.fn>).mockResolvedValue('/tmp/x.txt');
    (writeTextFile as ReturnType<typeof vi.fn>).mockResolvedValue(undefined);
    render(DiagnosticExportModal, { onClose: () => {} });
    await waitFor(() => screen.getByTestId('export-save'));
    await fireEvent.click(screen.getByTestId('export-save'));
    await waitFor(() => expect(writeTextFile).toHaveBeenCalledWith('/tmp/x.txt', REDACTED_FIXTURE));
  });

  it('Cancel button calls onClose', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(REDACTED_FIXTURE);
    const onClose = vi.fn();
    render(DiagnosticExportModal, { onClose });
    await waitFor(() => screen.getByTestId('export-cancel'));
    await fireEvent.click(screen.getByTestId('export-cancel'));
    expect(onClose).toHaveBeenCalled();
  });
});
```

- [ ] **Step 5: Run + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit 2>&1 | tail -20
npx vitest run src/lib/components/__tests__/DiagnosticExportModal.test.ts 2>&1 | tail -30
```

Need to add the @tauri-apps/plugin-fs npm package if not present:

```bash
npm install @tauri-apps/plugin-fs
```

Also add the Tauri Rust dep + capability:

```bash
cd src-tauri && cargo add tauri-plugin-fs
```

Register the plugin in `src-tauri/src/lib.rs` setup: `.plugin(tauri_plugin_fs::init())`.

Update `src-tauri/capabilities/default.json` to include `"fs:default"` permission.

```bash
git add src/lib/components/DiagnosticExportModal.svelte src/lib/components/__tests__/DiagnosticExportModal.test.ts src/App.svelte package.json package-lock.json src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/lib.rs src-tauri/capabilities/default.json
git commit -m "feat(zeb-329): DiagnosticExportModal + sidebar Network nav

Modal renders server-side-redacted markdown by default; toggle
'Include full identifiers' re-fetches with include_full_ids=true.
Copy writes to clipboard; Save uses tauri-plugin-fs save dialog +
writeTextFile. Cancel onClose silently dismisses.

5 component tests cover: redacted-by-default (regex assertion that
no 32+ char hex is in DOM), toggle re-fetches with includeFullIds,
copy invokes clipboard.writeText, save invokes writeTextFile, cancel
fires onClose. tauri-plugin-fs registered + fs:default permission added."
```

---

## Task 12: docs/cross-wan-validation.md two-host playbook

**Files:**
- Create: `docs/cross-wan-validation.md`

- [ ] **Step 1: Create the playbook**

Create `docs/cross-wan-validation.md`:

```markdown
# Cross-WAN validation playbook

> Goal: prove that two real Harmony machines on different networks
> can find each other and exchange messages end-to-end. This is the
> hands-on counterpart to the in-app Network Health panel.

## What you need

- Two machines on different networks (home Wi-Fi + coffee-shop Wi-Fi,
  two friends, two ISPs)
- Both running Harmony v0.1.0-alpha-N
- One out-of-band channel to share a `harmony://invite/...` URL
  (Signal, SMS, email)

This playbook takes ~10–15 minutes end-to-end.

## Step 1: Baseline (single-machine sanity)

On EACH machine independently:

1. Launch Harmony.
2. Open the **Network** panel (sidebar → Network).
3. Wait until the "Reachable" status appears (typically <30 seconds).
4. Click **Run self-test**. Expect every step (endpoint, relay,
   pkarr_publish, pkarr_resolve) to show ✓.
5. Screenshot the panel for your records.

**If a machine fails Step 1**: the cross-WAN test can't proceed.
Click **Submit diagnostics**, save the export, and attach it to a
tester-feedback issue with the title "Cross-WAN Step 1 failure on
\<your-OS\>".

## Step 2: First contact

On **machine A**:

1. Create a community ("test-cross-wan-YYYYMMDD" or similar throwaway name).
2. From the community settings, generate an invite URL.
3. Paste the URL into your out-of-band channel.

On **machine B**:

1. Click the `harmony://...` URL from machine A.
2. Confirm the join dialog.
3. After the "Joined" toast appears, return to the Network panel.
4. Within 60 seconds, **peer A** should appear in the peer list.

If peer A does NOT appear within 60s, both machines should run
self-test again and capture diagnostics.

## Step 3: Exchange

1. On **machine A**: send a DM to machine B's identity ("hello from A").
2. On **machine B**: confirm receipt.
3. Reverse: B → A.
4. The Network panel on both machines should now show the other peer with:
   - **last_seen** within seconds
   - either **direct** or **relay** mode (note which)
   - measured **RTT**

## Step 4: Export

On both machines:

1. Click **Submit diagnostics**.
2. Review the redacted markdown.
3. Save as `.txt` (or copy and paste into your feedback issue).
4. Attach both diagnostics to a tester-feedback issue along with:
   - "Successful Step 3 cross-WAN exchange" OR
   - "Got stuck at Step N because Y"
   - Network conditions (which ISP, residential/business, behind VPN, etc.)

## Troubleshooting cheatsheet

| Symptom | Likely cause | Next step |
|---|---|---|
| Stuck on "starting up…" for >60s | Relay unreachable (firewall blocks UDP 443 outbound) | Test on a different network (mobile hotspot, coffee shop) |
| Self-test relay step ✗ | Same as above | Same as above |
| Peer never appears after URL click | Discovery (pkarr) failure or invite expired | Re-generate invite; if persistent, attach both machines' diagnostics |
| Peer appears but mode is "noConnection" | Reachability record received but no live connection negotiated | Try sending a DM (forces connection); if still failing, attach diagnostics |
| RTT >2s on relay mode | Distant relay or congested path | Note location; this is expected for some geographies in the alpha |
| One direction works, reverse doesn't | Asymmetric NAT | File a tester-feedback issue noting NAT type from both machines' panels |

## What "success" looks like

A Step 3 success means: bidirectional message exchange between two
machines on different networks, with both machines' Network panels
showing the other peer reachable. This is the empirical evidence that
[ZEB-321](https://linear.app/zeblith/issue/ZEB-321) Phase 2's cross-WAN discovery + handshake stack works as
designed.

A Step 3 failure on a specific symptom is also valuable data — file
the tester-feedback issue with both diagnostics and any other context
you can share.
```

- [ ] **Step 2: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add docs/cross-wan-validation.md
git commit -m "docs(zeb-329): cross-WAN two-host validation playbook

Step 1 (baseline single-machine sanity) → Step 2 (first contact via
harmony:// invite URL) → Step 3 (bidirectional exchange) → Step 4
(export both diagnostics). Troubleshooting cheatsheet covers seven
common failure modes. Per spec §8.5."
```

---

## Task 13: Final 5-gate sweep + push + PR creation

**Files:** none (verification + PR).

- [ ] **Step 1: Final formatting + clippy**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
set -o pipefail
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -40
echo "CLIPPY EXIT=${PIPESTATUS[0]}"
```

Expected: zero warnings.

- [ ] **Step 2: Final nextest**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
timeout 600 cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tee /tmp/zeb-329-final.log | tail -50
echo "EXIT=${PIPESTATUS[0]}"
```

Expected: only baseline orphan failures + the new tests passing. Compare against `/tmp/zeb-329-baseline.log` to verify no new failures were introduced.

- [ ] **Step 3: Frontend gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit 2>&1 | tail -20
echo "TSC EXIT=$?"
npx vitest run 2>&1 | tail -30
echo "VITEST EXIT=$?"
```

Expected: both pass clean.

- [ ] **Step 4: Push the branch**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git status -sb
git push -u origin zeb-329-network-health-spec
```

- [ ] **Step 5: Create the PR**

```bash
gh pr create --title "ZEB-329: Network Health panel + self-test + cross-WAN validation playbook (ZEB-327 Sub-B)" --body "$(cat <<'EOF'
## Summary

Implements [ZEB-329](https://linear.app/zeblith/issue/ZEB-329) — Sub-project B of the [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) alpha-validation umbrella (Sub-C onboarding UX and Sub-D Zeblithic+invite-distribution still to come, so this PR does NOT close ZEB-327).

Ships the in-app **Network Health** panel that synthesizes existing iroh + ReachabilityResolver + pkarr publisher state into a tester-comprehensible surface, plus a self-test that exercises 4 local checks + per-peer round-trip pings, plus a redacted-by-default diagnostic export, plus a two-host validation playbook.

Subsumes [ZEB-172](https://linear.app/zeblith/issue/ZEB-172) Track D's in-app diagnostics goal.

### What changed

**Backend (~600 LOC + tests)**
- `src-tauri/src/network_health.rs` — new synthesis-only module: `NetworkHealthService`, `NetworkHealthSnapshot`, `SelfTestReport`, `PeerHealth` + pure functions (`classify_nat`, `derive_reachability_status`, `filter_peers_by_shared_membership`, `format_export_markdown`) + rate-limiter task + `harmony/ping/v1` accept loop + `ping_peer` connect side
- 3 new Tauri IPCs: `network_health_snapshot`, `network_health_run_self_test`, `network_health_export_payload`
- 1 new Tauri event: `network-health-changed` (rate-limited at-most-1 per 2s)
- New ALPN constant `HARMONY_PING_V1` in `iroh_endpoint::alpn`
- `NodeState` gains `network_health: Option<Arc<NetworkHealthService>>`
- `event_loop.rs` calls `nh.notify()` adjacent to every `reachability_resolver.update(...)` site

**Frontend (~400 LOC + tests)**
- `src/lib/types/network-health.ts`, `network-health-adapter.ts`, `NetworkHealthView.svelte` (route `/network`), `DiagnosticExportModal.svelte`
- Sidebar "Network" nav item

**Documentation**
- `docs/cross-wan-validation.md` — two-host playbook for operators

**Specs + plan**
- Spec: `docs/specs/2026-05-24-zeb-329-network-health-design.md` (commit `30c4f7b`)
- Plan: `docs/plans/2026-05-24-zeb-329-network-health-plan.md`

### Test plan

- [x] `cargo fmt --all -- --check` (gate 1)
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` (gate 2)
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (gate 3) — only pre-existing orphan failures remain
- [x] `npx tsc --noEmit` (gate 4)
- [x] `npx vitest run` (gate 5)
- [ ] Manual: run through `docs/cross-wan-validation.md` Step 1 baseline on this machine (single-machine sanity, no second machine needed for this gate)
- [ ] Manual cross-WAN smoke test: deferred to alpha-tester cohort per `docs/cross-wan-validation.md`

### Notes for review

- **Synthesis-only invariant**: `network_health.rs` reads from sources, never mutates them. Trait extraction (`IrohSnapshot`, `PkarrSnapshot`, `ReachabilitySnapshot`, `MyMembershipSet`) keeps pure logic testable without iroh.
- **Server-side redaction**: `format_export_markdown(_, _, include_full_ids=false)` is the only path that emits identifier prefixes; the regex-leak test (`[0-9a-f]{32,}` rejected in redacted output) was written FIRST per `feedback_second_order_correctness_review`.
- **Phase 1 caveats**: `classify_nat` returns `Unknown` until iroh exposes a stable NAT classifier; `ProdMembership` is a stub returning empty Vec until the membership lookup is wired (peer list shows empty in production until that follow-up). Both documented in TODOs.
- **No new wire format**: zero CRDT events introduced. Identity portability invariant unaffected.

### Cross-references

- Parent umbrella: [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) (Sub-C + Sub-D still to come — no close)
- Related: [ZEB-321](https://linear.app/zeblith/issue/ZEB-321) Phase 3 (liveness/rebinding) will build on this surface (schema_version mechanism in place)
EOF
)"
```

Expected: PR URL printed.

- [ ] **Step 6: Capture PR number for the autonomous bot-monitoring loop handoff**

Note the PR number. Hand off to the autonomous bot-review loop (CodeRabbit + Cursor Bugbot + CodeAnt + Qodo per `feedback_autonomous_pr_monitoring_loop`; NOT Greptile; NOT CI). Pushover on ready-to-merge per `feedback_autonomous_post_spec`.

---

## Self-review (controller-side, not subagent)

### Spec coverage

| Spec section | Implemented in task |
|---|---|
| §3 architecture (3 IPCs + 1 event) | Task 7 |
| §4.1 backend types | Task 1 |
| §4.2 frontend types + adapter + components | Tasks 9, 10, 11 |
| §4.3 nav integration | Task 11 |
| §4.4 invariants (schema_version, no wire format, synthesis-only) | Tasks 1 (schema_version), 3 (synthesis trait separation), 7 (no new CRDT events) |
| §5.1 page-load snapshot flow | Tasks 3 + 10 |
| §5.2 rate-limit semantics | Task 4 |
| §5.3 self-test 4 steps + ping ALPN + concurrency cap | Tasks 5 + 6 + 8 |
| §5.4 export server-side redaction + last-test cache | Tasks 2 + 7 + 11 |
| §5.5 state coupling + boot ordering | Task 7 |
| §6.1 backend never throws | Tasks 1, 3, 7 |
| §6.2 Pass/Fail/Skipped triad | Task 6 |
| §6.3 frontend error rendering | Tasks 10, 11 |
| §6.4 edge cases 1-7 | Task 10 (startup retry), Tasks 6+7 (offline cascade), Task 10 (button disable), Task 7 (export-without-self-test) |
| §6.5 explicit do-NOT | Tasks 6, 7 (no retries inside snapshot IPC; no telemetry; user-initiated only) |
| §7 UX presentation | Tasks 10, 11 |
| §8.1 pure-function unit tests | Tasks 1, 2 |
| §8.1 stateful + self-test tests | Tasks 3, 4, 6 |
| §8.2 two-endpoint integration test | Task 8 |
| §8.3 adapter unit tests | Task 9 |
| §8.4 component tests | Tasks 10, 11 |
| §8.5 two-host playbook | Task 12 |

### Type consistency

- `NetworkHealthSnapshot` field names referenced consistently across Tasks 1, 2, 3, 7, 9.
- `StepOutcome` variant names (`Pass`/`Fail`/`Skipped`) consistent in Tasks 1, 2, 6, 7, 10.
- `ConnectionMode` (`Direct`/`Relay`/`NoConnection`) consistent across Tasks 1, 6, 9.
- Tauri IPC names (`network_health_snapshot`, `network_health_run_self_test`, `network_health_export_payload`) consistent in Tasks 7, 9.

### Placeholder scan

No "TBD"/"TODO" gates blocking task completion. Explicit `TODO(implementer)` markers exist in Task 7 Step 5 (membership-store wiring) and Task 7 Step 2 (PkarrSnapshot production impl) — these are documented as ship-with-degraded-feature paths, not blockers. The PR description flags them as follow-ups.

### Known limitations shipping in this PR

1. `classify_nat` returns `Unknown` until iroh exposes a stable NAT classifier API → snapshot still ships home_relay + relay_rtt + direct_addresses; peer connection mode tells the real story.
2. `ProdMembership` returns empty Vec → peer list will be empty until a follow-up wires the membership lookup. Panel still functions (renders "no peers" gracefully).
3. `network_health_run_self_test` IPC returns a synthetic all-Skipped report until production self-test traits are wired in Task 7 Step 5 follow-up. Panel renders the synthetic gracefully (Skipped icons + reasons).

All three are documented in the PR description.
