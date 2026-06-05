# ZEB-380 PR 2 — harmony-client user-configurable relay manager + per-relay health Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **GATE:** Task 0 (the `harmony-pkarr` rev bump) CANNOT run until **PR 1** (`harmony` repo, `ZEB-380 PR 1: hot-swappable relay pool…`) is **merged to `harmony` main**. Every later task depends on the new `harmony_pkarr` API (`set_relays`, `relay_health`, `RelayConfig`, `RelayHealth`/`RelayState`/`RelayOutcome`). Do not start until the controller supplies PR 1's merge SHA.

**Goal:** Surface a user-configurable, persisted pkarr relay list (add / remove / restore-recommended, applied live with no restart) plus per-relay health in the Network Health panel and a Settings relay-manager — consuming PR 1's hot-swappable pool.

**Architecture:** Persist `relays: Vec<String>` in `PkarrSettings` (serde field-default = a vetted ≥2 set). Boot builds the pool from settings instead of the hardcoded single relay, and retains the `Arc<RelayClient>` in `NodeState` so the IPC layer can live-swap it. Two new IPCs (`set_pkarr_relays` validate→persist→live-swap, `get_pkarr_relays` list+health). A new `RelaySnapshot` source on `NetworkHealthService` (mirroring `DialSnapshot`) feeds `relays: Vec<RelayHealthWire>` onto `PkarrHealthSummary` (schema_version 2→3). Frontend renders health rows in the panel and a relay manager in Connectivity settings.

**Tech Stack:** Rust (Tauri IPC, serde, `url` crate for validation), Svelte + TypeScript (vitest). Crates/files: `src-tauri/src/{pkarr_settings.rs, network_health.rs, lib.rs}`, `src/lib/{types/network-health.ts, connectivity-adapter.ts, components/NetworkHealthView.svelte, components/NetworkDiscoverabilitySettings.svelte}`.

**Spec:** `docs/specs/2026-06-05-zeb-380-configurable-multi-relay-pool-design.md` §5.

**Repo / branch:** `harmony-client`. Create branch `zeb-380-client-relay-manager` off latest `origin/main` **after** PR 1 merges (and ideally after PR #194 — the ZEB-382 bump — merges, since both touch the `harmony-pkarr` rev line; if #194 is still open, base on main anyway and resolve the single-line rev to PR 1's SHA, which supersedes both).

**Per-task gates:**
- Rust (lib changes relink integration tests — scope per-task per memory `feedback_harmony_app_relink_cost`):
  ```bash
  cd src-tauri && cargo fmt --all -- --check \
    && cargo clippy -p harmony-app --lib --all-features -- -D warnings \
    && cargo nextest run -p harmony-app --lib --features test-fixtures
  ```
  Reserve the full `--all-targets` sweep for Task 7.
- Frontend: `npx tsc --noEmit && npx vitest run` (from repo root).
- Commit BEFORE the gate. 10-min wall-clock kill switch per cargo command; on stall, commit WIP + report `DONE_WITH_CONCERNS`. Use `set -o pipefail` when piping.

---

## File Structure

- **Modify** `src-tauri/Cargo.toml` — bump `harmony-pkarr` rev (×2 pins) to PR 1 SHA; add `url = "2"` (validation).
- **Modify** `src-tauri/src/pkarr_settings.rs` — `relays` field + `default_relays()` + `MAX_RELAYS` + `validate_relay_urls()` + `is_local_host()` + tests.
- **Modify** `src-tauri/src/lib.rs` — boot pool from settings; `NodeState.pkarr_relay_client` field + guard assign + clear; `set_pkarr_relays`/`get_pkarr_relays` IPCs + registration (×2 handler lists); `StubEmptyRelaySnapshot`; `ProdRelaySnapshot` wiring into `NetworkHealthService::new`.
- **Modify** `src-tauri/src/network_health.rs` — `RelaySnapshot` trait + `ProdRelaySnapshot` + `EmptyRelaySnapshot` (test) + `RelayHealthWire`/`RelayStateWire`/`RelayOutcomeWire` + `From` mapping; `relays` on `PkarrHealthSummary`; `relay` source on service + `new()` param + `snapshot()` populate; schema_version 2→3; `empty()` + `format_export_markdown` relay rows; update all `NetworkHealthService::new` call sites.
- **Modify** `src/lib/types/network-health.ts` — TS `RelayHealth`/`RelayState`/`RelayOutcome` + `relays` on the pkarr-status type.
- **Modify** `src/lib/connectivity-adapter.ts` — `getPkarrRelays()` / `setPkarrRelays()` wrappers.
- **Modify** `src/lib/components/NetworkHealthView.svelte` — per-relay health rows under "Discovery (pkarr)".
- **Modify** `src/lib/components/NetworkDiscoverabilitySettings.svelte` — relay-manager UI (add / remove / restore-recommended, live).
- **Tests:** inline Rust `#[cfg(test)]`; `src/lib/components/__tests__/*.test.ts` (vitest).

---

## Task 0: Bump `harmony-pkarr` to PR 1 SHA + add `url`  *(GATED on PR 1 merge)*

**Files:** Modify `src-tauri/Cargo.toml`

- [ ] **Step 1: Bump both pins to PR 1's merge SHA**

In `src-tauri/Cargo.toml`, the prod dep (~L93) and the `test-fixtures` dev-dep (~L155) both pin `harmony-pkarr`. Replace the rev on BOTH with PR 1's merge SHA `<PR1_SHA>` (controller supplies it). Use the `.git` URL form to match the other harmony crates and keep a single git source:
```toml
harmony-pkarr = { git = "https://github.com/zeblithic/harmony.git", rev = "<PR1_SHA>" }
# …and the dev-dep:
harmony-pkarr = { git = "https://github.com/zeblithic/harmony.git", rev = "<PR1_SHA>", features = ["test-fixtures"] }
```
Update the `# ZEB-323 Phase 2b:` comment above the prod dep to note the rev now also carries ZEB-380's hot-swappable pool + per-relay health.

- [ ] **Step 2: Add the `url` crate (validation dep)**

In `[dependencies]` add:
```toml
# ZEB-380: relay-URL validation (scheme + host checks) in pkarr_settings.rs.
# Already in the lock transitively (reqwest/iroh); a direct entry just makes it importable.
url = "2"
```

- [ ] **Step 3: Regenerate the lock + verify build**

```bash
cd src-tauri && cargo update -p harmony-pkarr && cargo build -p harmony-app
```
Expected: resolves to a single `git+https://github.com/zeblithic/harmony.git?rev=<PR1_SHA>` source for harmony-pkarr; `url` resolves without a new download; build green. The new symbols `harmony_pkarr::{RelayConfig, RelayHealth, RelayState, RelayOutcome}` + `RelayClient::{set_relays, relay_health, with_config}` are now importable (verify with a throwaway `cargo doc -p harmony-pkarr` or just that later tasks compile).

- [ ] **Step 4: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "build(zeb-380): bump harmony-pkarr to PR1 (hot-swap pool + relay health) + add url"
```

---

## Task 1: `PkarrSettings.relays` — persist the configurable list

**Files:** Modify `src-tauri/src/pkarr_settings.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block:
```rust
#[test]
fn defaults_to_recommended_relays() {
    let settings = PkarrSettings::default();
    assert_eq!(settings.relays, default_relays());
    assert!(settings.relays.len() >= 2, "must ship a >=2 relay default");
}

#[test]
fn missing_relays_field_defaults_on_load() {
    // A pre-ZEB-380 settings file has no `relays` key; serde's field default
    // must fill it with the recommended >=2 set so existing users gain
    // redundancy on upgrade rather than booting with an empty pool.
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("legacy.json");
    std::fs::write(&path, r#"{"identity_discoverable":true}"#).expect("write");
    let loaded = PkarrSettings::load_or_default(&path);
    assert_eq!(loaded.relays, default_relays());
}

#[test]
fn round_trips_custom_relays() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("connectivity-settings.json");
    let settings = PkarrSettings {
        identity_discoverable: false,
        friend_auto_accept_known: true,
        relays: vec!["https://relay.pkarr.org".to_string()],
    };
    settings.save(&path).expect("save");
    assert_eq!(PkarrSettings::load_or_default(&path).relays, settings.relays);
}
```
> The existing `round_trip_save_then_load` builds a `PkarrSettings { identity_discoverable, friend_auto_accept_known }` literal — it must gain `relays: …`. Update it (and any other literal `PkarrSettings { … }` constructor in this file) to include the new field.

- [ ] **Step 2: Run, verify they fail**

`cd src-tauri && cargo nextest run -p harmony-app --lib -E 'test(relays)'` → FAIL (no `relays` field / `default_relays`).

- [ ] **Step 3: Add the field + default**

```rust
    /// ZEB-380: user-configurable, persisted pkarr relay pool. A serde field
    /// default fills a vetted >=2 set for old settings files (forward-compat),
    /// guaranteeing relay redundancy on upgrade. Applied live via
    /// `set_pkarr_relays` (no restart).
    #[serde(default = "default_relays")]
    pub relays: Vec<String>,
```
add after `friend_auto_accept_known`. Then:
```rust
/// Default for [`PkarrSettings::relays`]: a vetted, liveness-probed >=2 set
/// (2026-06-05). `relay.pkarr.org` (n0-operated) + `pkarr.pubky.app` (Pubky).
/// Redundancy means one host-level relay hiccup is no longer terminal for
/// first-contact (ZEB-330).
pub fn default_relays() -> Vec<String> {
    vec![
        "https://relay.pkarr.org".to_string(),
        "https://pkarr.pubky.app".to_string(),
    ]
}
```
Update `impl Default for PkarrSettings` to set `relays: default_relays()`.

> **Default-set provenance check (do this now, before locking it in):** confirm `pkarr.pubky.app` speaks the relay protocol, not just serves a homepage. Run:
> ```bash
> curl -s -o /dev/null -w "%{http_code}\n" https://pkarr.pubky.app/8pinxxgqs41n4aididenw5apqp1urfmzdztr8jt4abrkdn435ewo
> ```
> A relay returns `404` (valid z32 key, not present) — **not** `200` (that's a homepage, meaning it is NOT a relay endpoint). If it does not behave as a relay, substitute `https://pkarr.pubky.org` (also probed `200` on `/`; re-run the same `GET /<z32>` check). Record the chosen second relay + its `404` proof in the commit message.

- [ ] **Step 4: Run, verify pass**

`cd src-tauri && cargo nextest run -p harmony-app --lib -E 'test(relays) + test(round_trip)'` → PASS.

- [ ] **Step 5: Gate + commit**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy -p harmony-app --lib --all-features -- -D warnings && cargo nextest run -p harmony-app --lib --features test-fixtures
cd .. && git add src-tauri/src/pkarr_settings.rs && git commit -m "feat(zeb-380): persist user-configurable relays in PkarrSettings (>=2 default)"
```

---

## Task 2: Relay-URL validation (`validate_relay_urls`)

Pure function: parse each URL, enforce scheme (`https` for remote, `http` only loopback/private), dedup (trailing-slash-normalized), reject empty, cap at 8.

**Files:** Modify `src-tauri/src/pkarr_settings.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn validate_rejects_empty_list() {
    assert!(validate_relay_urls(vec![]).is_err());
}

#[test]
fn validate_rejects_blank_and_malformed() {
    assert!(validate_relay_urls(vec!["".into()]).is_err());
    assert!(validate_relay_urls(vec!["not a url".into()]).is_err());
    assert!(validate_relay_urls(vec!["ftp://relay.example".into()]).is_err());
}

#[test]
fn validate_rejects_http_for_remote_host() {
    assert!(validate_relay_urls(vec!["http://relay.pkarr.org".into()]).is_err());
}

#[test]
fn validate_allows_http_for_loopback() {
    let ok = validate_relay_urls(vec!["http://127.0.0.1:6881".into()]).expect("loopback ok");
    assert_eq!(ok, vec!["http://127.0.0.1:6881".to_string()]);
    assert!(validate_relay_urls(vec!["http://localhost:6881".into()]).is_ok());
}

#[test]
fn validate_dedups_trailing_slash() {
    let ok = validate_relay_urls(vec![
        "https://relay.pkarr.org".into(),
        "https://relay.pkarr.org/".into(),
    ])
    .expect("dedup");
    assert_eq!(ok, vec!["https://relay.pkarr.org".to_string()]);
}

#[test]
fn validate_caps_at_eight() {
    let many: Vec<String> = (0..9).map(|i| format!("https://r{i}.example.com")).collect();
    assert!(validate_relay_urls(many).is_err());
}
```

- [ ] **Step 2: Run, verify fail**

`cd src-tauri && cargo nextest run -p harmony-app --lib -E 'test(validate_)'` → FAIL.

- [ ] **Step 3: Implement**

```rust
/// Maximum number of relays a user may configure.
pub const MAX_RELAYS: usize = 8;

/// Validate + normalize a user-submitted relay list. Rejects an empty list,
/// blank/malformed URLs, non-`https` remote schemes (`http` allowed only for
/// loopback / private hosts — pkarr's local-relay-on-:6881 guidance), and more
/// than [`MAX_RELAYS`]. Dedups on the trailing-slash-normalized URL, preserving
/// first-seen order. Returns the normalized list on success.
pub fn validate_relay_urls(input: Vec<String>) -> Result<Vec<String>, String> {
    if input.is_empty() {
        return Err("at least one relay is required".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in input {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("relay URL must not be empty".to_string());
        }
        let parsed = url::Url::parse(trimmed).map_err(|_| format!("invalid relay URL: {trimmed}"))?;
        match parsed.scheme() {
            "https" => {}
            "http" => {
                let host = parsed.host_str().unwrap_or("");
                if !is_local_host(host) {
                    return Err(format!("http:// is only allowed for localhost relays: {trimmed}"));
                }
            }
            other => return Err(format!("unsupported relay scheme '{other}': {trimmed}")),
        }
        let normalized = trimmed.trim_end_matches('/').to_string();
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    if out.len() > MAX_RELAYS {
        return Err(format!("too many relays (max {MAX_RELAYS})"));
    }
    Ok(out)
}

/// True for loopback / private / link-local hosts where a plaintext `http://`
/// relay is acceptable (a local pkarr relay on :6881).
fn is_local_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        Ok(std::net::IpAddr::V6(v6)) => v6.is_loopback(),
        Err(_) => false,
    }
}
```

- [ ] **Step 4: Run, verify pass; Step 5: gate + commit**

```bash
cd src-tauri && cargo nextest run -p harmony-app --lib -E 'test(validate_)'
cargo fmt --all -- --check && cargo clippy -p harmony-app --lib --all-features -- -D warnings && cargo nextest run -p harmony-app --lib --features test-fixtures
cd .. && git add src-tauri/src/pkarr_settings.rs && git commit -m "feat(zeb-380): validate_relay_urls (scheme/host/dedup/cap)"
```

---

## Task 3: Boot the pool from settings + retain the relay-client handle

Build the pool from `PkarrSettings.relays` instead of the hardcoded single relay, and stash the `Arc<RelayClient>` in `NodeState` for live swaps.

**Files:** Modify `src-tauri/src/lib.rs`

- [ ] **Step 1: Add the `NodeState` field**

Near the pkarr handles (after `pub pkarr_resolver: …`, ~L816), add:
```rust
    /// ZEB-380: the live relay client, retained so `set_pkarr_relays` can
    /// hot-swap the pool (and `get_pkarr_relays` can read per-relay health)
    /// without a restart. `None` pre-start_node / when pkarr isn't wired;
    /// cleared on stop alongside the other pkarr handles.
    pub pkarr_relay_client: Option<std::sync::Arc<harmony_pkarr::RelayClient>>,
```
Set it to `None` in every `NodeState` default/reset site that the other pkarr `Option` fields use (mirror `pkarr_resolver`: the `Default`/`new` at ~L1068 and `clear`/reset at ~L918). Grep `pkarr_resolver:` and `pkarr_resolver =` to find every site and add the parallel `pkarr_relay_client` line.

- [ ] **Step 2: Boot the pool from settings + retain the Arc**

The settings are loaded at ~L4544–4546 (`pkarr_settings`), but the pool is built earlier at ~L4395. **Move** the `PkarrSettings::load_or_default` so `pkarr_settings.relays` is available where the pool is built (or read just the relays earlier). Then replace L4395–4400:
```rust
                    let pkarr_relay_pool = harmony_pkarr::RelayPool::new(vec![
                        "https://relay.pkarr.org".to_string(),
                    ]);
                    let pkarr_relay_client =
                        std::sync::Arc::new(harmony_pkarr::RelayClient::new(pkarr_relay_pool));
```
with:
```rust
                    // ZEB-380: build the pool from the persisted user-configurable
                    // relay list (>=2 by default). Empty/missing settings → default set.
                    let configured_relays = pkarr_settings.relays.clone();
                    let pkarr_relay_pool = harmony_pkarr::RelayPool::new(if configured_relays.is_empty() {
                        crate::pkarr_settings::default_relays()
                    } else {
                        configured_relays
                    });
                    let pkarr_relay_client =
                        std::sync::Arc::new(harmony_pkarr::RelayClient::new(pkarr_relay_pool));
```
> **Implementer:** `pkarr_settings` must be in scope at L4395. The cleanest move is to hoist the `let pkarr_settings_path = …; let pkarr_settings = PkarrSettings::load_or_default(&pkarr_settings_path);` pair (currently ~L4544) to just above the pool construction (~L4394), and delete the original lower copy (keep the single source of truth — verify `pkarr_settings`/`pkarr_settings_path` are still used at their later sites, e.g. `identity_discoverable` at L4547 and `friend_auto_accept_known_for_state` at L4552, and that those now read the hoisted bindings). Run `cargo build` to confirm no use-before-move / borrow issues.

The resolver currently MOVES the client (`PkarrResolver::new(pkarr_relay_client)` at ~L4407). Change to clone so the handle survives:
```rust
                    let pkarr_resolver_arc =
                        std::sync::Arc::new(harmony_pkarr::PkarrResolver::new(
                            std::sync::Arc::clone(&pkarr_relay_client),
                        ));
```

- [ ] **Step 3: Carry the Arc to the guard-assignment block**

The guard assignments for pkarr handles are at ~L5305–5329 (`guard.dial_telemetry`, `guard.pkarr_publisher`, `guard.pkarr_resolver`). Following the existing `*_for_state.take()` pattern used there, introduce a `pkarr_relay_client_for_state` binding (clone of `pkarr_relay_client` made at construction, mirroring how `pkarr_resolver_for_state` is produced) and assign:
```rust
                        guard.pkarr_relay_client = pkarr_relay_client_for_state.take();
```
right after `guard.pkarr_resolver = …` (~L5329). Mirror exactly how `pkarr_resolver_for_state` is declared + populated (grep `pkarr_resolver_for_state`).

- [ ] **Step 4: Clear on stop**

In `stop_inner` / `clear_iroh_handles` (wherever `guard.pkarr_resolver = None;` is set), add `guard.pkarr_relay_client = None;`. Grep `pkarr_resolver = None` to find the site(s).

- [ ] **Step 5: Build gate + commit**

```bash
cd src-tauri && cargo build -p harmony-app && cargo fmt --all -- --check && cargo clippy -p harmony-app --lib --all-features -- -D warnings && cargo nextest run -p harmony-app --lib --features test-fixtures
cd .. && git add src-tauri/src/lib.rs && git commit -m "feat(zeb-380): boot relay pool from settings + retain RelayClient handle"
```
Expected: builds; existing tests green. (No new unit test here — behavior is covered by the IPC test in Task 5 + the live-swap core test in PR 1.)

---

## Task 4: `RelaySnapshot` source + relay health on the snapshot

Add the relay source-trait, wire types, the `relays` field on `PkarrHealthSummary`, the service plumbing, schema bump, and export rows.

**Files:** Modify `src-tauri/src/network_health.rs` (+ `lib.rs` call site in Task 5).

- [ ] **Step 1: Write the failing test**

Add to `network_health.rs` `#[cfg(test)] mod tests`:
```rust
struct FakeRelaySnapshot(Vec<harmony_pkarr::RelayHealth>);
impl RelaySnapshot for FakeRelaySnapshot {
    fn relay_health(&self) -> Vec<harmony_pkarr::RelayHealth> {
        self.0.clone()
    }
}

#[tokio::test]
async fn snapshot_populates_relay_health() {
    let relay = harmony_pkarr::RelayHealth {
        url: "https://relay.pkarr.org".to_string(),
        state: harmony_pkarr::RelayState::CoolingDown { until_ms: 123 },
        last_outcome: Some(harmony_pkarr::RelayOutcome::Http(503)),
        last_success_ms: Some(42),
    };
    let svc = NetworkHealthService::new(
        std::sync::Arc::new(FakeIroh::none()),       // existing test double — match the file's helper
        std::sync::Arc::new(StubPkarr),              // existing test double
        std::sync::Arc::new(EmptyResolver),          // existing test double
        std::sync::Arc::new(FakeMembership { table: Default::default() }),
        std::sync::Arc::new(EmptyDialSnapshot),
        std::sync::Arc::new(FakeRelaySnapshot(vec![relay.clone()])),
    );
    let snap = svc.snapshot().await;
    assert_eq!(snap.schema_version, 3);
    assert_eq!(snap.pkarr_status.relays.len(), 1);
    assert_eq!(snap.pkarr_status.relays[0].url, "https://relay.pkarr.org");
    assert_eq!(
        snap.pkarr_status.relays[0].state,
        RelayStateWire::CoolingDown { until_ms: 123 }
    );
    assert_eq!(
        snap.pkarr_status.relays[0].last_outcome,
        Some(RelayOutcomeWire::Http { status: 503 })
    );
}
```
> **Implementer:** the test doubles (`FakeIroh`, `StubPkarr`, `EmptyResolver`, etc.) above are placeholders — read the existing `network_health.rs` tests (lines ~1700–1920, where `NetworkHealthService::new` is already called five times) and reuse whatever doubles those call sites use. The point is: add the 6th `new()` arg + assert `schema_version == 3` + relay mapping.

- [ ] **Step 2: Run, verify it fails** (`new` arity + `RelayStateWire` missing).

- [ ] **Step 3: Add the wire types + `From` mapping**

After the `DialSnapshot` block (~L428), add:
```rust
/// ZEB-380: per-relay health source for the snapshot. Mirrors `DialSnapshot`.
/// Returns the core `harmony_pkarr::RelayHealth`; `snapshot()` maps it to the
/// camelCase wire DTO below.
pub trait RelaySnapshot: Send + Sync {
    fn relay_health(&self) -> Vec<harmony_pkarr::RelayHealth>;
}

/// Production source: reads the live `Arc<RelayClient>` retained in NodeState.
pub struct ProdRelaySnapshot(pub std::sync::Arc<harmony_pkarr::RelayClient>);
impl RelaySnapshot for ProdRelaySnapshot {
    fn relay_health(&self) -> Vec<harmony_pkarr::RelayHealth> {
        self.0.relay_health()
    }
}

#[cfg(test)]
pub struct EmptyRelaySnapshot;
#[cfg(test)]
impl RelaySnapshot for EmptyRelaySnapshot {
    fn relay_health(&self) -> Vec<harmony_pkarr::RelayHealth> {
        Vec::new()
    }
}
```
Near the other wire types (after `PkarrFallbackHit`, ~L84), add the camelCase DTOs (internally-tagged enums for clean TS):
```rust
/// ZEB-380: camelCase wire shape of one relay's health (maps from
/// `harmony_pkarr::RelayHealth`, whose core type stays idiomatic snake_case).
/// Owned client-side so the IPC contract lives in the consumer repo, same as
/// `DialHealthSummary`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RelayHealthWire {
    pub url: String,
    pub state: RelayStateWire,
    pub last_outcome: Option<RelayOutcomeWire>,
    pub last_success_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RelayStateWire {
    Healthy,
    CoolingDown { until_ms: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RelayOutcomeWire {
    Success,
    Timeout,
    Transport,
    Http { status: u16 },
}

impl From<harmony_pkarr::RelayHealth> for RelayHealthWire {
    fn from(h: harmony_pkarr::RelayHealth) -> Self {
        RelayHealthWire {
            url: h.url,
            state: match h.state {
                harmony_pkarr::RelayState::Healthy => RelayStateWire::Healthy,
                harmony_pkarr::RelayState::CoolingDown { until_ms } => {
                    RelayStateWire::CoolingDown { until_ms }
                }
            },
            last_outcome: h.last_outcome.map(|o| match o {
                harmony_pkarr::RelayOutcome::Success => RelayOutcomeWire::Success,
                harmony_pkarr::RelayOutcome::Timeout => RelayOutcomeWire::Timeout,
                harmony_pkarr::RelayOutcome::Transport => RelayOutcomeWire::Transport,
                harmony_pkarr::RelayOutcome::Http(status) => RelayOutcomeWire::Http { status },
            }),
            last_success_ms: h.last_success_ms,
        }
    }
}
```

- [ ] **Step 4: Add `relays` to `PkarrHealthSummary`**

```rust
    /// ZEB-380: per-relay health for the configured pool. Empty pre-wiring.
    pub relays: Vec<RelayHealthWire>,
```
Add `relays: Vec::new()` to the `empty()` constructor (~L238) and to any test literal building `PkarrHealthSummary { … }` (grep `PkarrHealthSummary {`).

- [ ] **Step 5: Add the `relay` source to the service**

In `NetworkHealthService` struct add:
```rust
    /// ZEB-380: per-relay health source.
    relay: std::sync::Arc<dyn RelaySnapshot>,
```
Add `relay: std::sync::Arc<dyn RelaySnapshot>` as the final param of `new()` and set `relay,` in the struct literal. In `snapshot()`, set on the `PkarrHealthSummary`:
```rust
                relays: self.relay.relay_health().into_iter().map(Into::into).collect(),
```

- [ ] **Step 6: Bump schema_version 2 → 3**

Change both `schema_version: 2` literals (`empty()` ~L232, `snapshot()` ~L497) to `3`. Grep `schema_version` for any test asserting `== 2` and update to `3`.

- [ ] **Step 7: Export rows in `format_export_markdown`**

In the "## Discovery (pkarr)" block (after the fallback-events loop, ~L671), append:
```rust
    for relay in &snapshot.pkarr_status.relays {
        // Relay URLs are public infrastructure — no redaction needed.
        let state = match &relay.state {
            RelayStateWire::Healthy => "healthy".to_string(),
            RelayStateWire::CoolingDown { until_ms } => format!("coolingDown(until={until_ms})"),
        };
        let last = match &relay.last_outcome {
            None => String::new(),
            Some(RelayOutcomeWire::Success) => " lastOutcome=success".to_string(),
            Some(RelayOutcomeWire::Timeout) => " lastOutcome=timeout".to_string(),
            Some(RelayOutcomeWire::Transport) => " lastOutcome=transport".to_string(),
            Some(RelayOutcomeWire::Http { status }) => format!(" lastOutcome=http:{status}"),
        };
        let _ = writeln!(out, "relay {} [{}]{}", relay.url, state, last);
    }
```

- [ ] **Step 8: Update all 5 in-file `NetworkHealthService::new` test call sites**

Add `std::sync::Arc::new(EmptyRelaySnapshot)` as the 6th arg to each (~L1740, 1755, 1783, 1817, 1916 — grep `NetworkHealthService::new` within `network_health.rs`).

- [ ] **Step 9: Run + gate + commit**

```bash
cd src-tauri && cargo nextest run -p harmony-app --lib -E 'test(snapshot_populates_relay_health) + test(format_export) + test(redact)'
cargo fmt --all -- --check && cargo clippy -p harmony-app --lib --all-features -- -D warnings && cargo nextest run -p harmony-app --lib --features test-fixtures
cd .. && git add src-tauri/src/network_health.rs && git commit -m "feat(zeb-380): relay health on NetworkHealthSnapshot (schema v3) + RelaySnapshot source"
```
Expected: the redaction-leak test still passes (relay URLs are not 32+ hex runs). If a test literal-constructs `NetworkHealthSnapshot`/`PkarrHealthSummary`, it now needs `relays`.

---

## Task 5: `set_pkarr_relays` / `get_pkarr_relays` IPCs + wire `ProdRelaySnapshot`

**Files:** Modify `src-tauri/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add near the existing `set_friend_auto_accept_persists_round_trips` test (~L35987):
```rust
#[test]
fn set_pkarr_relays_validation_and_persist_round_trip() {
    // Mirrors the friend-auto-accept persistence test: the IPC's pure pieces
    // (validate + PkarrSettings load/save) round-trip through a temp file.
    let td = tempfile::TempDir::new().expect("tempdir");
    let path = td.path().join("connectivity-settings.json");

    // Invalid input is rejected by the validator (no persist).
    assert!(crate::pkarr_settings::validate_relay_urls(vec!["http://relay.pkarr.org".into()]).is_err());

    // Valid input persists + reloads.
    let validated =
        crate::pkarr_settings::validate_relay_urls(vec!["https://relay.pkarr.org".into()])
            .expect("valid");
    let mut settings = crate::pkarr_settings::PkarrSettings::load_or_default(&path);
    settings.relays = validated.clone();
    settings.save(&path).expect("save");
    assert_eq!(
        crate::pkarr_settings::PkarrSettings::load_or_default(&path).relays,
        validated
    );
}
```

- [ ] **Step 2: Run, verify fail** (compiles only once `validate_relay_urls` is `pub` — it is, from Task 2; this test will pass immediately if Task 2 landed. If so, treat it as a regression guard and proceed — the IPC wiring itself is exercised by tsc/vitest + manual smoke).

- [ ] **Step 3: Add the two IPCs**

Place near the other connectivity IPCs (after `connectivity_get_identity_discoverable`, ~L33842). Mirror that command's locking/emit idiom exactly:
```rust
/// ZEB-380: replace the user-configurable pkarr relay list. Validates, persists
/// to `connectivity-settings.json`, then hot-swaps the live pool (no restart).
#[tauri::command(rename_all = "snake_case")]
async fn set_pkarr_relays(
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<NodeState>>,
    relays: Vec<String>,
) -> Result<(), String> {
    let validated = crate::pkarr_settings::validate_relay_urls(relays)?;
    let (settings_path, relay_client) = {
        let guard = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            guard.pkarr_settings_path.clone(),
            guard.pkarr_relay_client.clone(),
        )
    };
    let Some(path) = settings_path else {
        return Err("pkarr_settings_path missing".into());
    };
    let mut settings = pkarr_settings::PkarrSettings::load_or_default(&path);
    settings.relays = validated.clone();
    settings
        .save(&path)
        .map_err(|e| format!("save connectivity-settings: {e}"))?;
    // Live-swap. No-op if pkarr isn't wired yet — the persisted list is read at
    // the next boot.
    if let Some(rc) = relay_client {
        rc.set_relays(validated);
    }
    if let Err(e) = app.emit("connectivity-relays-changed", ()) {
        tracing::warn!(error = %e, "set_pkarr_relays: emit failed");
    }
    Ok(())
}

/// ZEB-380: current relay list + per-relay health. Prefers the live client's
/// health; falls back to the persisted list (Healthy placeholders) pre-wiring
/// so the Settings UI can render + edit before/without a running node.
#[tauri::command(rename_all = "snake_case")]
async fn get_pkarr_relays(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<crate::network_health::RelayHealthWire>, String> {
    let (settings_path, relay_client) = {
        let guard = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            guard.pkarr_settings_path.clone(),
            guard.pkarr_relay_client.clone(),
        )
    };
    if let Some(rc) = relay_client {
        return Ok(rc.relay_health().into_iter().map(Into::into).collect());
    }
    let relays = match settings_path {
        Some(p) => pkarr_settings::PkarrSettings::load_or_default(&p).relays,
        None => crate::pkarr_settings::default_relays(),
    };
    Ok(relays
        .into_iter()
        .map(|url| crate::network_health::RelayHealthWire {
            url,
            state: crate::network_health::RelayStateWire::Healthy,
            last_outcome: None,
            last_success_ms: None,
        })
        .collect())
}
```

- [ ] **Step 4: Register in BOTH handler lists**

Add `set_pkarr_relays,` and `get_pkarr_relays,` to the production `tauri::generate_handler![` block (~L36492, near `connectivity_get_identity_discoverable` / `network_health_snapshot`) AND the test handler block (~L36727). Both lists, or the test harness 404s on the command.

- [ ] **Step 5: Wire `ProdRelaySnapshot` into `NetworkHealthService::new`**

At the production construction (~L5396), add the 6th arg. Mirror the pkarr `Some/else-stub` pattern using the retained handle:
```rust
                            let prod_relay: std::sync::Arc<dyn crate::network_health::RelaySnapshot> =
                                if let Some(rc) = guard.pkarr_relay_client.as_ref() {
                                    std::sync::Arc::new(crate::network_health::ProdRelaySnapshot(
                                        std::sync::Arc::clone(rc),
                                    ))
                                } else {
                                    std::sync::Arc::new(StubEmptyRelaySnapshot)
                                };

                            let mut nh = crate::network_health::NetworkHealthService::new(
                                prod_iroh,
                                prod_pkarr,
                                prod_resolver,
                                prod_membership,
                                prod_dial,
                                prod_relay,
                            );
```
Add the stub near `StubEmptyPkarrSnapshot` (~L36195):
```rust
/// Production `RelaySnapshot` stub for when the relay client isn't wired
/// (pre-start_node / pkarr unavailable). Empty list — panel shows no relays.
struct StubEmptyRelaySnapshot;
impl crate::network_health::RelaySnapshot for StubEmptyRelaySnapshot {
    fn relay_health(&self) -> Vec<harmony_pkarr::RelayHealth> {
        Vec::new()
    }
}
```

- [ ] **Step 6: Gate + commit**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy -p harmony-app --lib --all-features -- -D warnings && cargo nextest run -p harmony-app --lib --features test-fixtures
cd .. && git add src-tauri/src/lib.rs && git commit -m "feat(zeb-380): set_pkarr_relays/get_pkarr_relays IPCs + ProdRelaySnapshot wiring"
```

---

## Task 6: Frontend — health rows + relay-manager UI

**Files:** Modify `src/lib/types/network-health.ts`, `src/lib/connectivity-adapter.ts`, `src/lib/components/NetworkHealthView.svelte`, `src/lib/components/NetworkDiscoverabilitySettings.svelte`, + vitest.

- [ ] **Step 1: TS types**

In `src/lib/types/network-health.ts` add (match the camelCase + internally-tagged Rust DTO from Task 4):
```typescript
export type RelayState =
  | { kind: 'healthy' }
  | { kind: 'coolingDown'; untilMs: number };

export type RelayOutcome =
  | { kind: 'success' }
  | { kind: 'timeout' }
  | { kind: 'transport' }
  | { kind: 'http'; status: number };

export interface RelayHealth {
  url: string;
  state: RelayState;
  lastOutcome: RelayOutcome | null;
  lastSuccessMs: number | null;
}
```
Add `relays: RelayHealth[];` to the pkarr-status interface (the TS shape of `PkarrHealthSummary` — find it in this file; it has `identityPublished`, etc.).

- [ ] **Step 2: Adapter wrappers + vitest**

In `src/lib/connectivity-adapter.ts` add (mirror the existing `setIdentityDiscoverable`/`getIdentityDiscoverable` wrappers' invoke + error-extraction style — `e instanceof Error ? e.message : String(e)` per memory `feedback_tauri_error_extraction`):
```typescript
import type { RelayHealth } from './types/network-health';

export async function getPkarrRelays(): Promise<RelayHealth[]> {
  return adapter.invoke('get_pkarr_relays', {});
}

export async function setPkarrRelays(relays: string[]): Promise<void> {
  // snake_case IPC param: Tauri maps camelCase JS → snake_case Rust. The Rust
  // arg is `relays`, so the key is `relays`.
  await adapter.invoke('set_pkarr_relays', { relays });
}
```
> **Implementer:** read `connectivity-adapter.ts` first — match its actual `adapter`/`invoke` import + the existing wrapper signatures. Add a `connectivity-adapter.test.ts` (or extend the existing one) mocking the invoke for both wrappers (happy path + a rejected `set` surfaces the error string).

- [ ] **Step 3: Panel health rows (`NetworkHealthView.svelte`)**

Read the component; under the "Discovery (pkarr)" section render one row per `pkarrStatus.relays`: the URL + a badge derived from `state`/`lastOutcome`:
- `state.kind === 'healthy'` → `Healthy` (green).
- `state.kind === 'coolingDown'` → `Cooling down (Ns)` where `N = Math.max(0, Math.ceil((untilMs - Date.now())/1000))` (amber).
- if `lastOutcome` is an error variant → also show `Last error: timeout|transport|http <status>` (muted).
Keep markup/classes consistent with the existing peer/health rows in that file.

- [ ] **Step 4: Relay manager (`NetworkDiscoverabilitySettings.svelte`)**

Read the component; add a "Relays" section:
- on mount / on `connectivity-relays-changed` event, call `getPkarrRelays()` and list each relay with its health badge.
- **Add:** a URL text input + "Add" button → push to the working list, call `setPkarrRelays(list)`; on rejection show the returned error string inline (no optimistic mutation on failure).
- **Remove:** per-row "Remove" (disabled when only one relay remains) → call `setPkarrRelays(listWithoutRow)`.
- **Restore recommended:** button → `setPkarrRelays(DEFAULT_RELAYS)` where `DEFAULT_RELAYS = ['https://relay.pkarr.org','https://pkarr.pubky.app']` (keep in sync with Rust `default_relays()` — Task 1's chosen second relay).
- Changes are live (no restart copy). Re-read health after each mutation.

- [ ] **Step 5: vitest**

Extend `src/lib/components/__tests__/NetworkDiscoverabilitySettings.test.ts` (+ `NetworkHealthView.test.ts`):
- relay manager renders the configured relays + badges (healthy / cooling-down / errored).
- add submits `setPkarrRelays` with the new list; invalid URL → rejection shows inline error, list unchanged.
- remove disabled at one relay; "Restore recommended" submits the default set.
- panel renders a `Cooling down (Ns)` badge for a `coolingDown` relay and `Last error: http 503` for an `http` outcome.

- [ ] **Step 6: Frontend gate + commit**

```bash
npx tsc --noEmit && npx vitest run
git add src/ && git commit -m "feat(zeb-380): relay-manager UI + per-relay health rows (frontend)"
```

---

## Task 7: Final full sweep + PR

- [ ] **Step 1: Full-target Rust + frontend gate**

```bash
cd src-tauri && cargo fmt --all -- --check \
  && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
  && cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd .. && npx tsc --noEmit && npx vitest run
```
Use `set -o pipefail`. The `--all-targets` clippy/nextest relinks integration binaries — budget ~40–60 min, set a wall-clock heartbeat (ScheduleWakeup 1800s or foreground+timeout) per memory `feedback_long_running_background_supervision`. Known macOS-only iroh/zenoh transport orphan-flakes are non-blocking (they pass on CI Linux); a real harmony-app failure is blocking.

- [ ] **Step 2: Final code review** over `git diff origin/main...HEAD`, then push + open the PR.

- [ ] **Step 3: Push + PR**

```bash
git push -u origin zeb-380-client-relay-manager
gh pr create --title "ZEB-380: user-configurable multi-relay pool + per-relay health" --body "$(cat <<'EOF'
## Summary
Closes ZEB-380. First-contact bootstrap no longer depends on a single hardcoded pkarr relay.

- Persist a user-configurable relay list in `PkarrSettings` (serde field-default = vetted ≥2 set: `relay.pkarr.org` + `<second>`), forward-compatible for old settings files.
- Boot the pool from settings; retain the `Arc<RelayClient>` so relays hot-swap **live** (no restart) via `set_pkarr_relays`.
- New IPCs: `set_pkarr_relays` (validate scheme/host/dedup/cap → persist → live-swap) and `get_pkarr_relays` (list + per-relay health).
- Per-relay health on `NetworkHealthSnapshot` (schema_version 2→3) via a new `RelaySnapshot` source — Network Health panel shows each relay's `Healthy`/`Cooling down`/`Last error` badge.
- Settings → Connectivity relay manager: add / remove (last-relay guarded) / Restore recommended, all live.

Consumes harmony PR 1 (`harmony-pkarr` hot-swappable pool + `relay_health()`); bumps the rev to its merge SHA.

## Test plan
- `cargo fmt --check` + `clippy -D warnings` + `nextest --workspace --all-targets --features test-fixtures`
- `tsc --noEmit` + `vitest run`
- Manual: Settings → Connectivity → add/remove a relay; confirm the panel badge updates live and `connectivity-settings.json` persists across restart.

Spec: `docs/specs/2026-06-05-zeb-380-configurable-multi-relay-pool-design.md`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```
> **Linear auto-close:** ZEB-380 is the ONLY ticket id in the body (it is the intended close). Do not add other open ticket IDs.

- [ ] **Step 4: Autonomous bot loop**

CodeRabbit / Cursor / CodeAnt / Qodo. **NEVER** Greptile / never write the literal `@greptile` (paid; charges the user — write plain "Greptile"). Scan all three buckets (inline review threads, PR issue-comments, PR reviews). One bundled push per round + reply. Wait for CI green (5 jobs) AND bot reviews. Pushover at ready-to-merge; **do NOT self-merge** (Jake's gate). Pushover instantly on a true blocker.
