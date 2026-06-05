# ZEB-380 — User-configurable multi-relay pool + per-relay health

**Status:** Design approved 2026-06-05.
**Ticket:** [ZEB-380](https://linear.app/zeblith/issue/ZEB-380) (High) — "pkarr first-contact bootstrap: single relay SPOF (relay.pkarr.org), 5s/30s, no redundancy or direct DHT."
**Repos:** `harmony` (core, `harmony-pkarr` crate) + `harmony-client`.

## 1. Problem

First-contact bootstrap (invite redeem → resolve inviter reachability, plus identity/community publish) depends on a **single** pkarr relay, `https://relay.pkarr.org`, hardcoded at `harmony-client/src-tauri/src/lib.rs:4395`. The HTTP relay client uses a 5 s per-request timeout and a 30 s cooldown. There is no relay redundancy. During the first Koya ↔ Ildwyn cross-machine bring-up (ZEB-330), Ildwyn's invite redeem failed with `NoRelaysAvailable` because the one relay was timing out / rate-limiting *that host* — and with a single relay, one host-level hiccup is terminal for first-contact.

The pool plumbing already supports more: `RelayPool` is an ordered `Vec<String>` and `RelayClient::available_relays()` already rotates and skips cooled-down relays (the `unreachable_relay_falls_through_to_next` unit test proves fall-through works). What is missing is (a) more than one relay in the pool, (b) a way for users to manage relays, and (c) any visibility into per-relay health.

## 2. Goals / Non-goals

**Goals**
- The relay pool holds ≥ 2 relays out of the box; bring-up succeeds when one relay is unreachable or slow (acceptance #1).
- Per-relay health is observable in the Network Health panel (acceptance #2).
- Relays are a **user-configurable**, persisted list (add / remove / restore-recommended), applied **live** without an app restart.

**Non-goals (explicitly out of scope)**
- Direct Mainline-DHT (UDP) fallback path → deferred to ZEB-323.
- De-stubbing the existing Phase-1 pkarr health fields (`identityPublished`, `identityLastPublishMs`, `communityPublishCount`) — those are ZEB-329 follow-ups. This work adds relay health *beside* them and does not touch the stubs.
- User-facing timeout / cooldown knobs — kept as code-level defaults (see §4.1).

## 3. Architecture overview

Cross-repo, following the ZEB-382 rhythm (core API lands first, then the client bumps to consume it):

```
PR 1  harmony/harmony-pkarr (relay.rs)   ──►  hot-swappable pool + RelayConfig + per-relay health accessor
  │                                            (merge → SHA)
  ▼
PR 2  harmony-client                      ──►  bump harmony-pkarr rev to PR1 SHA
                                               + PkarrSettings.relays (persist)
                                               + boot wiring from settings
                                               + set/get relay IPCs (validate, persist, live-swap)
                                               + RelaySnapshot → NetworkHealthSnapshot
                                               + Settings relay-manager UI + panel health rows
```

The user-facing capability and most of the code is in `harmony-client`; the core change is the enabling dependency.

## 4. Core changes — `harmony-pkarr/src/relay.rs` (PR 1)

### 4.1 Hot-swappable pool
`RelayClient` currently owns `pool: RelayPool` (immutable). Change to an atomically swappable handle:

- Hold the pool as `std::sync::RwLock<RelayPool>` — dependency-free, and the pool is read-mostly with rare swaps, so read contention is negligible. (If `arc-swap` is already in the dependency graph at impl time, `ArcSwap<RelayPool>` is a fine lock-free alternative, but do not add a new core dependency just for this.)
- Add `pub fn set_relays(&self, relays: Vec<String>)` — builds a new `RelayPool` and replaces the guarded value. Takes effect on the next `put`/`get`.
- `available_relays()` reads a clone of the current pool under a short read-lock instead of the owned field.
- Cooldown map (`Mutex<HashMap<url, Instant>>`) is unchanged. Entries keyed on a now-removed relay simply never match a live relay again and age out — harmless; no explicit pruning needed (a `set_relays` may optionally drop stale keys, but it is not required for correctness).

### 4.2 Configurable timeout / cooldown
Replace the `REQUEST_TIMEOUT` / `COOLDOWN` consts with a config struct supplied at construction:

```rust
#[derive(Debug, Clone, Copy)]
pub struct RelayConfig {
    pub request_timeout: Duration, // default 5s
    pub cooldown: Duration,        // default 30s
}
impl Default for RelayConfig { /* 5s / 30s */ }
```

`RelayClient::new(pool)` keeps its signature via `RelayClient::new(pool)` = `with_config(pool, RelayConfig::default())`; add `RelayClient::with_config(pool, cfg)`. **Not user-facing** — multi-relay already removes the "5 s timeout is terminal" failure mode, so exposing these knobs to users adds confusion for little benefit. The config exists so tests can use short values and so a future ticket can wire it up if needed.

### 4.3 Per-relay health accessor
Add a synchronous accessor so the health flows through `NetworkHealthService::snapshot` (which is sync) with no async hop — mirroring ZEB-373's `DialTelemetry` pattern.

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RelayState { Healthy, CoolingDown { until_ms: u64 } }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RelayOutcome { Success, Timeout, Transport, Http(u16) }
// `Transport` (added during PR-1 impl) = a non-timeout transport failure
// (connection refused / DNS / TLS), split from `Timeout` via
// `reqwest::Error::is_timeout()` so the health badge is honest. PR-2's
// `RelayOutcomeWire` + TS type mirror all four variants.

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RelayHealth {
    pub url: String,
    pub state: RelayState,
    pub last_outcome: Option<RelayOutcome>,
    pub last_success_ms: Option<u64>,
}

impl RelayClient {
    pub fn relay_health(&self) -> Vec<RelayHealth> { /* one entry per current pool relay */ }
}
```

Implementation: a per-URL record (`Mutex<HashMap<String, RelayRecord>>` or fold into the existing cooldown map upgraded to carry last-outcome + last-success) updated on each `put`/`get` branch. `state` is derived from the cooldown map at read time (`CoolingDown` if its expiry is in the future, else `Healthy`). `until_ms` / `last_success_ms` are wall-clock millis (the crate already has no clock helper; add a small `now_ms()` like `network_health.rs` has, or accept an injected clock for testability).

### 4.4 Core tests
- `set_relays` takes effect mid-flight: publish to pool A, swap to pool B, next publish hits B.
- `relay_health` reflects a cooled-down relay (`CoolingDown { until_ms }`) after a forced timeout, and `Healthy` after cooldown expiry.
- `last_outcome` records `Timeout` vs `Http(code)` vs `Success` correctly.
- Existing multi-relay fall-through test stays green.
- `RelayConfig` short timeout is honored (a slow relay trips cooldown within the configured window).

## 5. Client changes — `harmony-client` (PR 2)

### 5.1 Persistence — `PkarrSettings` (`pkarr_settings.rs`)
Add a relays field with a serde field-default for forward-compatibility (same pattern the file already uses for `friend_auto_accept_known`):

```rust
#[serde(default = "default_relays")]
pub relays: Vec<String>,

fn default_relays() -> Vec<String> {
    vec![
        "https://relay.pkarr.org".to_string(),
        "https://pkarr.pubky.app".to_string(),
    ]
}
```

An existing `connectivity-settings.json` with no `relays` key inherits the default ≥2 set on load. `Default for PkarrSettings` also uses `default_relays()`.

> **Default set provenance:** liveness-probed 2026-06-05 — `relay.pkarr.org`, `pkarr.pubky.app`, `pkarr.pubky.org` all responded `200`. At implementation time, confirm `pkarr.pubky.app` speaks the relay protocol with a real `GET /<z32key>` → `404` (not just a `200` on `/`) before locking it in as the second default; fall back to `pkarr.pubky.org` if not.

### 5.2 Boot wiring (`lib.rs` ~4395)
Build the pool from `PkarrSettings.relays` (loaded earlier in boot) instead of the hardcoded vec. The `Arc<RelayClient>` is retained in scope (it already is — it feeds `PkarrPublisher` + `PkarrResolver`) so the IPC layer can call `set_relays` on it for live updates.

### 5.3 IPCs
- `set_pkarr_relays(relays: Vec<String>) -> Result<(), String>` — **validate** (each URL parses; scheme `https` for remote hosts, `http` allowed only for loopback / private addresses per pkarr's local-relay-on-:6881 guidance; dedup; non-empty; cap at 8), **persist** to `PkarrSettings`, then `relay_client.set_relays(validated)` for live effect.
- `get_pkarr_relays() -> Vec<RelayHealthDto>` — current list + per-relay health (calls `relay_client.relay_health()`).

Validation rejects an **empty** list (must keep ≥1); the UI prevents removing the last relay and offers "Restore recommended."

### 5.4 Health surfacing (`network_health.rs`)
Mirror the `DialSnapshot` / `ProdDialSnapshot` source-trait pattern exactly:

- New source trait `RelaySnapshot { fn relay_health(&self) -> Vec<RelayHealth>; }` and `ProdRelaySnapshot(Arc<RelayClient>)`.
- Add `relays: Vec<RelayHealth>` to `PkarrHealthSummary` (the "Discovery (pkarr)" section).
- `NetworkHealthService` gains a `relay: Arc<dyn RelaySnapshot>` source; `snapshot()` populates `pkarr_status.relays`.
- Bump `NetworkHealthSnapshot::schema_version` 2 → 3 (export-format change, per the spec §4.4 rule the module already documents). Update `empty()` + `format_export_markdown` to render the relay rows (redaction-safe — relay URLs are public, no redaction needed, but emit them in the "## Discovery (pkarr)" block).

### 5.5 UI
- **Network Health panel:** under "Discovery (pkarr)", a per-relay list — each row shows the URL + a health badge: `Healthy` / `Cooling down (Ns)` / `Last error: <outcome>`. Satisfies acceptance #2.
- **Settings → Connectivity:** a relay manager —
  - list of current relays with inline health badge,
  - add: a validated URL text input (inline error on invalid),
  - remove per row (disabled when only one remains),
  - "Restore recommended" button (resets to `default_relays()`),
  - changes are **live** (call `set_pkarr_relays`, no restart), and the list re-reads health via `get_pkarr_relays`.

## 6. Error handling

| Condition | Behavior |
|---|---|
| Invalid relay URL submitted | `set_pkarr_relays` returns a typed error string; UI shows inline validation, no persist. |
| Attempt to remove the last relay | UI disables removal; `set_pkarr_relays([])` rejected server-side as a backstop. |
| All relays down at runtime | Same `NoRelaysAvailable` as today — but now each relay's failure is visible per-row in the panel. |
| Corrupt / missing settings file | `load_or_default` already returns defaults (existing behavior) → default ≥2 relays. |
| Removed relay still in cooldown map | Ages out; no effect on correctness. |

## 7. Testing

**Core (PR 1):** see §4.4.

**Client (PR 2):**
- `PkarrSettings` round-trips `relays`; an old file with no `relays` key loads the default set (serde forward-compat test, mirroring `missing_auto_accept_field_defaults_on`).
- `set_pkarr_relays` validation: rejects malformed URL, rejects empty, dedups, caps length; persists + would-live-swap (unit-test the validation pure-fn; the swap is covered by the core test).
- `RelaySnapshot` → `snapshot().pkarr_status.relays` is populated from a fake.
- `format_export_markdown` includes relay rows; redaction test still passes (no new hex leak).
- vitest: relay-manager add/remove/validation + "Restore recommended"; panel renders health badges for healthy / cooling-down / errored states.
- Keep the gated `HARMONY_PKARR_LIVE_RELAY=1` real-relay test green (no regression).

## 8. Rollout / sequencing

1. PR 1 (`harmony`): relay.rs hot-swap + `RelayConfig` + `relay_health` + tests. Merge.
2. PR 2 (`harmony-client`): bump `harmony-pkarr` rev → PR 1 merge SHA, then all §5 client work. (The ZEB-382 bump, PR #194, is expected to be on `main` by then; PR 2's bump supersedes it by moving to PR 1's SHA.)

Both PRs reference ZEB-380 in their bodies; only PR 2's body carries the Linear-closing reference (ZEB-380 is a `harmony-client` ticket). Per the Linear auto-close rule, no other open ticket IDs appear in either body.
