# ZEB-480 `api watch` channel-monitor harness — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a top-level `harmony-app watch` subcommand that streams a filtered, resumable, self-healing feed of channel messages from a running headless node's WS firehose, as NDJSON on stdout — the push analog of `api --events`.

**Architecture:** New module `src-tauri/src/api/watch.rs`. A pure projection (`WireMessage` → `WatchLine`) and pure cursor logic (`ChannelCursor`/`CursorSet`), an async core (`backfill` via the `list_channel_messages` RPC + `stream_once` over the existing `stream_events` WS client), a `run_watch` reconnect loop, and a blocking `api_watch(cfg)` wrapper mirroring `api_cli`. CLI wiring adds a `Command::Watch` variant in `main.rs`. Reuses `api::cli::{Discovery, read_discovery, rpc_call, stream_events}` verbatim.

**Tech Stack:** Rust, tokio (current-thread runtime in the wrapper), `tokio-tungstenite` (via the existing `stream_events`), `reqwest` (via `rpc_call`), serde/serde_json, clap.

## Global Constraints

- Cargo commands run from `src-tauri/`. Gates: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Iterative gates use `scripts/test-select --context task|round`; the final pre-PR sweep is the full `--workspace --all-targets` run. (A lib change relinks integ binaries — expect ~5-12 min warm.)
- Stdout purity (PR #231 discipline, inherited from `api_cli`): NDJSON frames are the ONLY stdout; every diagnostic goes to stderr. Exit codes: 0 clean stop, 1 server error, 2 local/usage error, 3 `--no-retry` disconnect.
- All wire shapes are camelCase (`#[serde(rename_all = "camelCase")]`), matching the existing DTOs.
- Integration tests need `--features test-fixtures` and set `HARMONY_PASSPHRASE` (keychain-hermetic, ZEB-428). Register new test submodules in `tests/api_tests.rs` via `#[path]` (files under `tests/api/` are NOT auto-compiled).
- `git add` new files before gating (untracked files are invisible to `scripts/test-select`).

## File Structure

- **Create** `src-tauri/src/api/watch.rs` — the whole watch surface (types, projection, cursor, async core, `api_watch` wrapper) + inline `#[cfg(test)]` unit tests.
- **Modify** `src-tauri/src/api/mod.rs` — add `pub mod watch;` alongside the existing `pub mod cli;`.
- **Modify** `src-tauri/src/lib.rs` — add `pub use crate::api::watch::api_watch;` next to the `pub use crate::api::cli::api_cli;` at ~line 25170.
- **Modify** `src-tauri/src/main.rs` — add a `Command::Watch { … }` variant (~after `Api`) + a dispatch arm.
- **Create** `src-tauri/tests/api/watch.rs` — in-process integration test booting a node and driving the async core.
- **Modify** `src-tauri/tests/api_tests.rs` — register `#[path = "api/watch.rs"] mod watch;`.
- **Create** `docs/playbooks/agent-channel-watch.md` — the `run_in_background` wake-pattern playbook.
- **Create** `e2e-harness/tests/e2e_watch.rs` — `--features e2e` subprocess smoke (never in CI).

---

### Task 1: `WireMessage` → `WatchLine` projection (pure)

**Files:**
- Create: `src-tauri/src/api/watch.rs`
- Modify: `src-tauri/src/api/mod.rs` (add `pub mod watch;`)

**Interfaces:**
- Consumes: `crate::community_channel_log_engine::HlcDto` (Serialize + Deserialize + Clone + PartialEq).
- Produces: `WireMessage` (Deserialize), `WatchLine` (Serialize), `WatchLine::from_wire(&WireMessage, source: &'static str, seq: Option<u64>) -> WatchLine`.

- [ ] **Step 1: Add the module declaration.** In `src-tauri/src/api/mod.rs`, add next to the other `pub mod` lines:

```rust
pub mod watch;
```

- [ ] **Step 2: Write the failing test** (append to a new `#[cfg(test)] mod tests` in `watch.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wire(body: &[u8], kind: Option<&str>) -> WireMessage {
        serde_json::from_value(json!({
            "messageId": "m1", "communityId": "c1", "channelId": "ch1",
            "author": "a1", "at": {"wallMs": 100u64, "logical": 2u32, "deviceId": "d1"},
            "body": body, "kind": kind,
        })).expect("wire msg")
    }

    #[test]
    fn projection_maps_fields_and_decodes_body() {
        let line = WatchLine::from_wire(&wire(b"hello", None), "live", Some(7));
        assert_eq!(line.source, "live");
        assert_eq!(line.seq, Some(7));
        assert_eq!(line.channel_id, "ch1");
        assert_eq!(line.author, "a1");
        assert_eq!(line.body, "hello");
        assert_eq!(line.at.wall_ms, 100);
        // Optional fields omitted when absent.
        let v = serde_json::to_value(&line).unwrap();
        assert!(v.get("replyTo").is_none() && v.get("kind").is_none());
        assert_eq!(v["source"], "backfill".to_string().is_empty().then(|| "").unwrap_or("live"));
    }

    #[test]
    fn projection_backfill_has_null_seq_and_camelcase() {
        let line = WatchLine::from_wire(&wire(b"hi", Some("poll")), "backfill", None);
        let v = serde_json::to_value(&line).unwrap();
        assert_eq!(v["source"], "backfill");
        assert!(v.get("seq").unwrap().is_null());
        assert_eq!(v["communityId"], "c1");
        assert_eq!(v["kind"], "poll");
    }
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cd src-tauri && cargo test --lib --features test-fixtures api::watch::tests 2>&1 | tail -20`
Expected: FAIL — `WireMessage`/`WatchLine` not defined.

- [ ] **Step 4: Write the minimal implementation** (top of `watch.rs`):

```rust
//! src-tauri/src/api/watch.rs — ZEB-480: `harmony-app watch` — filtered,
//! resumable, self-healing channel-message watch over the headless WS firehose.
//!
//! Stdout purity (PR #231 discipline, inherited from `api_cli`): NDJSON frames
//! are the only stdout; diagnostics go to stderr. Exit codes: 0 clean stop,
//! 1 server error, 2 local/usage, 3 --no-retry disconnect.

use crate::community_channel_log_engine::HlcDto;

/// Wire twin of `ChannelMessageDto` that is `Deserialize`-able. The DTO is
/// `Serialize`-only (its `kind: Option<&'static str>` can't deserialize), so
/// both the backfill array rows and the live `payload.message` parse into this.
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WireMessage {
    pub message_id: String,
    pub community_id: String,
    pub channel_id: String,
    pub author: String,
    pub at: HlcDto,
    #[serde(default)]
    pub body: Vec<u8>,
    #[serde(default)]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub poll_id: Option<String>,
    #[serde(default)]
    pub mentions: Option<Vec<String>>,
}

/// One emitted NDJSON line — the normalized projection, uniform across
/// backfill (`list_channel_messages`) and live (firehose) sources.
#[derive(serde::Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WatchLine {
    pub source: &'static str, // "backfill" | "live"
    pub seq: Option<u64>,     // firehose seq (live) or null (backfill)
    pub community_id: String,
    pub channel_id: String,
    pub message_id: String,
    pub author: String,
    pub at: HlcDto,
    pub body: String, // decoded UTF-8 (lossy; the engine enforces UTF-8)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mentions: Option<Vec<String>>,
}

impl WatchLine {
    pub fn from_wire(m: &WireMessage, source: &'static str, seq: Option<u64>) -> WatchLine {
        WatchLine {
            source,
            seq,
            community_id: m.community_id.clone(),
            channel_id: m.channel_id.clone(),
            message_id: m.message_id.clone(),
            author: m.author.clone(),
            at: m.at.clone(),
            body: String::from_utf8_lossy(&m.body).into_owned(),
            reply_to: m.reply_to.clone(),
            kind: m.kind.clone(),
            poll_id: m.poll_id.clone(),
            mentions: m.mentions.clone(),
        }
    }
}
```

Also fix the messy assert in Step 2 before running — replace the last line of `projection_maps_fields_and_decodes_body` with `assert_eq!(v["source"], "live");`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd src-tauri && cargo test --lib --features test-fixtures api::watch::tests 2>&1 | tail -20`
Expected: PASS (2 tests).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/api/watch.rs src-tauri/src/api/mod.rs
git commit -m "feat(watch): WireMessage->WatchLine projection for ZEB-480"
```

---

### Task 2: HLC cursor + `--since` parse/format + dedupe (pure)

**Files:**
- Modify: `src-tauri/src/api/watch.rs`

**Interfaces:**
- Produces: `parse_since(&str) -> Result<HlcDto, String>`, `format_since(&HlcDto) -> String`, `hlc_cmp(&HlcDto, &HlcDto) -> Ordering`, `ChannelCursor` with `accept(&mut self, message_id: &str, at: &HlcDto) -> bool` and `since(&self) -> Option<HlcDto>`.
- Consumes: `HlcDto` (Task 1's import).

**Design note (correctness):** The emit gate is **id-dedupe only**, NOT strict-greater HLC — this system allows backdated HLCs (at-event-HLC model), so gating on HLC would drop legitimately-late messages. `accept` returns true iff `message_id` is not in the bounded recent-id window; on accept it records the id (FIFO, cap `DEDUPE_WINDOW = 256`) and advances the high-water HLC via `max`. `since()` returns that high-water HLC — used ONLY to bound the backfill `list_channel_messages{since}`. Known limitation (documented): a message that materializes during watcher downtime with an HLC earlier than the high-water mark is not re-fetched by backfill; acceptable for coordination (full gap-freedom would need count/per-author cursors — deferred).

- [ ] **Step 1: Write the failing tests** (add to `mod tests`):

```rust
fn hlc(w: u64, l: u32, d: &str) -> HlcDto { HlcDto { wall_ms: w, logical: l, device_id: d.into() } }

#[test]
fn since_roundtrips() {
    let h = hlc(1721000000000, 3, "abc");
    assert_eq!(parse_since(&format_since(&h)).unwrap(), h);
    assert!(parse_since("nope").is_err());
    assert!(parse_since("1:2").is_err()); // needs 3 parts
}

#[test]
fn hlc_total_order() {
    use std::cmp::Ordering::*;
    assert_eq!(hlc_cmp(&hlc(1, 0, "a"), &hlc(2, 0, "a")), Less);
    assert_eq!(hlc_cmp(&hlc(1, 5, "a"), &hlc(1, 2, "a")), Greater);
    assert_eq!(hlc_cmp(&hlc(1, 2, "a"), &hlc(1, 2, "b")), Less); // deviceId tiebreak
}

#[test]
fn cursor_dedupes_by_id_not_hlc() {
    let mut c = ChannelCursor::default();
    assert!(c.accept("m1", &hlc(10, 0, "d")));
    assert!(!c.accept("m1", &hlc(10, 0, "d")));        // same id → reject
    // A BACKDATED but NEW message still emits (id-dedupe, not HLC gate):
    assert!(c.accept("m0", &hlc(5, 0, "d")));
    // since() is the high-water mark, unaffected by the backdated message:
    assert_eq!(c.since().unwrap(), hlc(10, 0, "d"));
}

#[test]
fn cursor_window_evicts_oldest() {
    let mut c = ChannelCursor::default();
    for i in 0..(DEDUPE_WINDOW + 10) { assert!(c.accept(&format!("m{i}"), &hlc(i as u64, 0, "d"))); }
    // "m0" scrolled out of the window → accepted again (bounded memory).
    assert!(c.accept("m0", &hlc(0, 0, "d")));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo test --lib --features test-fixtures api::watch::tests 2>&1 | tail -20`
Expected: FAIL — items not defined.

- [ ] **Step 3: Write the implementation** (add to `watch.rs`, after the types):

```rust
use std::cmp::Ordering;
use std::collections::VecDeque;

const DEDUPE_WINDOW: usize = 256;

pub fn hlc_cmp(a: &HlcDto, b: &HlcDto) -> Ordering {
    a.wall_ms
        .cmp(&b.wall_ms)
        .then(a.logical.cmp(&b.logical))
        .then(a.device_id.cmp(&b.device_id))
}

pub fn parse_since(s: &str) -> Result<HlcDto, String> {
    let mut it = s.splitn(3, ':');
    let (w, l, d) = match (it.next(), it.next(), it.next()) {
        (Some(w), Some(l), Some(d)) => (w, l, d),
        _ => return Err(format!("--since {s:?}: expected wallMs:logical:deviceId")),
    };
    Ok(HlcDto {
        wall_ms: w.parse().map_err(|e| format!("--since wallMs {w:?}: {e}"))?,
        logical: l.parse().map_err(|e| format!("--since logical {l:?}: {e}"))?,
        device_id: d.to_string(),
    })
}

pub fn format_since(h: &HlcDto) -> String {
    format!("{}:{}:{}", h.wall_ms, h.logical, h.device_id)
}

#[derive(Default, Debug)]
pub struct ChannelCursor {
    hlc: Option<HlcDto>,
    recent: VecDeque<String>,
}

impl ChannelCursor {
    pub fn with_since(hlc: Option<HlcDto>) -> Self {
        ChannelCursor { hlc, recent: VecDeque::new() }
    }
    /// True iff this message has not been emitted recently. Records the id
    /// (bounded FIFO) and advances the high-water HLC. Id-dedupe, not HLC gate.
    pub fn accept(&mut self, message_id: &str, at: &HlcDto) -> bool {
        if self.recent.iter().any(|id| id == message_id) {
            return false;
        }
        self.recent.push_back(message_id.to_string());
        if self.recent.len() > DEDUPE_WINDOW {
            self.recent.pop_front();
        }
        self.hlc = Some(match self.hlc.take() {
            Some(cur) if hlc_cmp(&cur, at) == Ordering::Greater => cur,
            _ => at.clone(),
        });
        true
    }
    pub fn since(&self) -> Option<HlcDto> {
        self.hlc.clone()
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo test --lib --features test-fixtures api::watch::tests 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/api/watch.rs
git commit -m "feat(watch): HLC cursor, --since parse, id-dedupe (ZEB-480)"
```

---

### Task 3: `WatchConfig` + `CursorSet` + cursor-file persistence (pure/fs)

**Files:**
- Modify: `src-tauri/src/api/watch.rs`

**Interfaces:**
- Produces: `WatchConfig { community_id: String, channels: Vec<String>, since: Option<HlcDto>, cursor_file: Option<PathBuf>, raw: bool, no_retry: bool }`; `CursorSet` mapping `channelId -> ChannelCursor` with `load(&WatchConfig) -> Result<CursorSet, String>`, `accept(&mut self, channel: &str, id: &str, at: &HlcDto) -> bool`, `since(&self, channel: &str) -> Option<HlcDto>`, `persist(&self, path: &Path) -> Result<(), String>` (atomic temp+rename), writing JSON `{ "<channelHex>": HlcDto }`.

- [ ] **Step 1: Write the failing test** (add to `mod tests`):

```rust
#[test]
fn cursor_file_roundtrips_per_channel() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cur.json");
    let cfg = WatchConfig {
        community_id: "c1".into(),
        channels: vec!["ch1".into(), "ch2".into()],
        since: None,
        cursor_file: Some(path.clone()),
        raw: false,
        no_retry: false,
    };
    let mut set = CursorSet::load(&cfg).unwrap();
    assert!(set.accept("ch1", "m1", &hlc(50, 0, "d")));
    set.persist(&path).unwrap();

    // Reload: ch1 resumes at its stored HLC; ch2 has none.
    let set2 = CursorSet::load(&cfg).unwrap();
    assert_eq!(set2.since("ch1").unwrap(), hlc(50, 0, "d"));
    assert!(set2.since("ch2").is_none());
}

#[test]
fn since_seeds_all_channels_when_no_file_entry() {
    let cfg = WatchConfig {
        community_id: "c1".into(),
        channels: vec!["ch1".into()],
        since: Some(hlc(9, 0, "d")),
        cursor_file: None,
        raw: false,
        no_retry: false,
    };
    let set = CursorSet::load(&cfg).unwrap();
    assert_eq!(set.since("ch1").unwrap(), hlc(9, 0, "d"));
}
```

- [ ] **Step 2: Run to verify failure.** `cd src-tauri && cargo test --lib --features test-fixtures api::watch::tests 2>&1 | tail -20` — FAIL.

- [ ] **Step 3: Write the implementation** (add to `watch.rs`):

```rust
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct WatchConfig {
    pub community_id: String,
    pub channels: Vec<String>,
    pub since: Option<HlcDto>,
    pub cursor_file: Option<PathBuf>,
    pub raw: bool,
    pub no_retry: bool,
}

#[derive(Default)]
pub struct CursorSet {
    per_channel: BTreeMap<String, ChannelCursor>,
}

impl CursorSet {
    /// Seed each channel's initial HLC: cursor-file entry (if any) > --since > none.
    pub fn load(cfg: &WatchConfig) -> Result<CursorSet, String> {
        let stored: BTreeMap<String, HlcDto> = match &cfg.cursor_file {
            Some(p) if p.exists() => {
                let raw = std::fs::read_to_string(p)
                    .map_err(|e| format!("read cursor-file {}: {e}", p.display()))?;
                if raw.trim().is_empty() {
                    BTreeMap::new()
                } else {
                    serde_json::from_str(&raw)
                        .map_err(|e| format!("parse cursor-file {}: {e}", p.display()))?
                }
            }
            _ => BTreeMap::new(),
        };
        let mut per_channel = BTreeMap::new();
        for ch in &cfg.channels {
            let seed = stored.get(ch).cloned().or_else(|| cfg.since.clone());
            per_channel.insert(ch.clone(), ChannelCursor::with_since(seed));
        }
        Ok(CursorSet { per_channel })
    }

    pub fn accept(&mut self, channel: &str, id: &str, at: &HlcDto) -> bool {
        match self.per_channel.get_mut(channel) {
            Some(c) => c.accept(id, at),
            None => false, // not a watched channel
        }
    }

    pub fn since(&self, channel: &str) -> Option<HlcDto> {
        self.per_channel.get(channel).and_then(|c| c.since())
    }

    /// Atomic write of `{channelHex: HlcDto}` (temp + rename).
    pub fn persist(&self, path: &Path) -> Result<(), String> {
        let map: BTreeMap<&String, HlcDto> = self
            .per_channel
            .iter()
            .filter_map(|(ch, c)| c.since().map(|h| (ch, h)))
            .collect();
        let json = serde_json::to_string(&map).map_err(|e| format!("serialize cursors: {e}"))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("rename cursor-file: {e}"))?;
        Ok(())
    }
}
```

- [ ] **Step 4: Run tests to verify they pass.** Same command — PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/api/watch.rs
git commit -m "feat(watch): WatchConfig + CursorSet + atomic per-channel cursor-file (ZEB-480)"
```

---

### Task 4: async core — `backfill` + `stream_once` + `run_watch` + `api_watch` wrapper

**Files:**
- Modify: `src-tauri/src/api/watch.rs`
- Modify: `src-tauri/src/lib.rs` (re-export `api_watch`)

**Interfaces:**
- Consumes: `crate::api::cli::{Discovery, read_discovery, rpc_call, stream_events}`, `crate::resolve_app_data_dir`.
- Produces: `pub async fn backfill(d, cfg, cursors, emit) -> Result<(), String>`; `pub async fn stream_once(d, cfg, cursors, emit) -> Result<StreamEnd, String>`; `pub async fn run_watch(data_dir, cfg, emit) -> Result<i32, String>`; `pub fn api_watch(cfg: WatchConfig) -> i32`. `emit: FnMut(&str) -> bool` (false = consumer stop). `enum StreamEnd { ConsumerStop, Reconnect }`.

**Design:** `backfill` pages `list_channel_messages{communityId, channelId, since, limit:200, order:"asc"}` per channel via `rpc_call`, deserializes the bare `Vec<WireMessage>` body, `cursors.accept`, emits `source:"backfill"` (or raw row when `cfg.raw`). `stream_once` runs `stream_events`, and in the sync `on_frame`: parse the frame as `serde_json::Value`; if `event == "_lagged"` set reason=Reconnect and return false; if `event == "channel-message-received"` and `payload.channelId ∈ cfg.channels`, deserialize `payload.message` → `WireMessage`, `cursors.accept`, emit `source:"live"` with the frame's `seq` (or the raw frame when `cfg.raw`); if `emit` returns false set reason=ConsumerStop and return false. `run_watch` loops: `backfill` → `stream_once`; on `ConsumerStop` return 0; on `Reconnect`/error, if `cfg.no_retry` return 3, else sleep backoff (250 ms→5 s, reset on success), re-`read_discovery` (port/token may rotate), repeat. `api_watch` mirrors `api_cli`: validate (≥1 channel), `resolve_app_data_dir`, build current-thread runtime, `run_watch` with a stdout-writing `emit` (writeln + flush, `.is_ok()`), persist the cursor-file after each emit when set.

- [ ] **Step 1: Write the failing integration test** — see Task 5 (the integration test lives in `tests/api/watch.rs` and exercises this core end-to-end against a booted node). For this task, add a focused unit test of the frame filter using a hand-built frame + a collecting emit:

```rust
#[tokio::test]
async fn stream_filter_keeps_only_target_channel_messages() {
    // Drive the on_frame logic directly via a helper the impl exposes:
    // `handle_frame(frame_text, cfg, cursors, emit) -> FrameOutcome`.
    let cfg = WatchConfig {
        community_id: "c1".into(), channels: vec!["ch1".into()],
        since: None, cursor_file: None, raw: false, no_retry: false,
    };
    let mut cursors = CursorSet::load(&cfg).unwrap();
    let mut got: Vec<String> = vec![];
    let mut emit = |s: &str| { got.push(s.to_string()); true };

    let msg = |ch: &str, id: &str| serde_json::json!({
        "seq": 1u64, "event": "channel-message-received",
        "payload": {"communityId":"c1","channelId":ch,"message":{
            "messageId": id, "communityId":"c1","channelId":ch,"author":"a",
            "at":{"wallMs":1u64,"logical":0u32,"deviceId":"d"},"body":[104,105]
        }}
    }).to_string();

    // target channel → emitted; other channel → dropped; non-message event → dropped.
    assert!(matches!(handle_frame(&msg("ch1","m1"), &cfg, &mut cursors, &mut emit), FrameOutcome::Continue));
    assert!(matches!(handle_frame(&msg("ch2","m2"), &cfg, &mut cursors, &mut emit), FrameOutcome::Continue));
    assert!(matches!(handle_frame(r#"{"seq":2,"event":"profile-update","payload":{}}"#, &cfg, &mut cursors, &mut emit), FrameOutcome::Continue));
    assert!(matches!(handle_frame(r#"{"seq":null,"event":"_lagged","payload":{"missed":3}}"#, &cfg, &mut cursors, &mut emit), FrameOutcome::Reconnect));

    assert_eq!(got.len(), 1);
    let line: serde_json::Value = serde_json::from_str(&got[0]).unwrap();
    assert_eq!(line["channelId"], "ch1");
    assert_eq!(line["source"], "live");
    assert_eq!(line["body"], "hi");
}
```

- [ ] **Step 2: Run to verify failure.** FAIL — `handle_frame`/`FrameOutcome` not defined.

- [ ] **Step 3: Implement the async core.** Add to `watch.rs` (extract the sync frame handler `handle_frame` so it's unit-testable; `stream_once` calls it inside `on_frame`):

```rust
use crate::api::cli::{read_discovery, rpc_call, stream_events, Discovery};

pub enum StreamEnd { ConsumerStop, Reconnect }
pub enum FrameOutcome { Continue, ConsumerStop, Reconnect }

/// Sync per-frame handler (unit-testable). Filters, projects, emits.
pub fn handle_frame(
    frame: &str,
    cfg: &WatchConfig,
    cursors: &mut CursorSet,
    emit: &mut impl FnMut(&str) -> bool,
) -> FrameOutcome {
    let v: serde_json::Value = match serde_json::from_str(frame) {
        Ok(v) => v,
        Err(_) => return FrameOutcome::Continue, // ignore unparseable frames
    };
    match v.get("event").and_then(|e| e.as_str()) {
        Some("_lagged") => return FrameOutcome::Reconnect,
        Some("channel-message-received") => {}
        _ => return FrameOutcome::Continue,
    }
    let payload = &v["payload"];
    let ch = payload.get("channelId").and_then(|c| c.as_str()).unwrap_or("");
    if !cfg.channels.iter().any(|c| c == ch) {
        return FrameOutcome::Continue;
    }
    let msg: WireMessage = match serde_json::from_value(payload["message"].clone()) {
        Ok(m) => m,
        Err(_) => return FrameOutcome::Continue,
    };
    if !cursors.accept(ch, &msg.message_id, &msg.at) {
        return FrameOutcome::Continue; // deduped
    }
    let out = if cfg.raw {
        frame.to_string()
    } else {
        match serde_json::to_string(&WatchLine::from_wire(
            &msg,
            "live",
            v.get("seq").and_then(|s| s.as_u64()),
        )) {
            Ok(s) => s,
            Err(_) => return FrameOutcome::Continue,
        }
    };
    if emit(&out) { FrameOutcome::Continue } else { FrameOutcome::ConsumerStop }
}

pub async fn backfill(
    d: &Discovery,
    cfg: &WatchConfig,
    cursors: &mut CursorSet,
    emit: &mut impl FnMut(&str) -> bool,
) -> Result<(), String> {
    for ch in &cfg.channels {
        loop {
            let since = cursors.since(ch);
            let args = serde_json::json!({
                "communityId": cfg.community_id, "channelId": ch,
                "since": since, "limit": 200u32, "order": "asc",
            });
            let (status, body) = rpc_call(d, "list_channel_messages", Some(&args.to_string())).await?;
            if status != 200 {
                return Err(format!("list_channel_messages HTTP {status}: {body}"));
            }
            let rows: Vec<WireMessage> =
                serde_json::from_str(&body).map_err(|e| format!("parse backfill: {e}"))?;
            let n = rows.len();
            for m in &rows {
                if cursors.accept(ch, &m.message_id, &m.at) {
                    let out = if cfg.raw {
                        serde_json::to_string(m).unwrap_or_default()
                    } else {
                        serde_json::to_string(&WatchLine::from_wire(m, "backfill", None))
                            .unwrap_or_default()
                    };
                    if !emit(&out) {
                        return Ok(()); // consumer stop
                    }
                }
            }
            if n < 200 { break; } // last page
        }
    }
    Ok(())
}

pub async fn stream_once(
    d: &Discovery,
    cfg: &WatchConfig,
    cursors: &mut CursorSet,
    emit: &mut impl FnMut(&str) -> bool,
) -> Result<StreamEnd, String> {
    let mut reason = StreamEnd::Reconnect; // default when the socket closes
    let mut ended = false;
    stream_events(d, |frame| match handle_frame(frame, cfg, cursors, emit) {
        FrameOutcome::Continue => true,
        FrameOutcome::ConsumerStop => { reason = StreamEnd::ConsumerStop; ended = true; false }
        FrameOutcome::Reconnect => { reason = StreamEnd::Reconnect; ended = true; false }
    })
    .await?;
    let _ = ended;
    Ok(reason)
}
```

Note the `WireMessage` needs `#[derive(serde::Serialize)]` too (for the `cfg.raw` backfill branch) — add `Serialize` to its derives in Task 1's struct, or serialize the raw row from `serde_json::Value` instead. Simplest: add `serde::Serialize` to `WireMessage`'s derive list.

- [ ] **Step 4: Implement `run_watch` + `api_watch`:**

```rust
pub async fn run_watch(
    data_dir: std::path::PathBuf,
    cfg: WatchConfig,
    mut emit: impl FnMut(&str) -> bool,
) -> Result<i32, String> {
    let mut cursors = CursorSet::load(&cfg)?;
    let mut d = read_discovery(&data_dir)?;
    let mut backoff = std::time::Duration::from_millis(250);
    loop {
        let cycle = async {
            backfill(&d, &cfg, &mut cursors, &mut emit).await?;
            stream_once(&d, &cfg, &mut cursors, &mut emit).await
        }
        .await;
        // Persist after every cycle (best-effort; also persisted inside emit for durability).
        if let Some(p) = &cfg.cursor_file {
            let _ = cursors.persist(p);
        }
        match cycle {
            Ok(StreamEnd::ConsumerStop) => return Ok(0),
            Ok(StreamEnd::Reconnect) => {}
            Err(e) => eprintln!("watch: {e}"),
        }
        if cfg.no_retry {
            return Ok(3);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(std::time::Duration::from_secs(5));
        if let Ok(nd) = read_discovery(&data_dir) {
            d = nd;
            backoff = std::time::Duration::from_millis(250);
        }
    }
}

/// Blocking CLI entry (mirrors `api_cli`).
pub fn api_watch(cfg: WatchConfig) -> i32 {
    if cfg.channels.is_empty() {
        eprintln!("watch: at least one --channel is required");
        return 2;
    }
    let data_dir = match crate::resolve_app_data_dir() {
        Ok(d) => d,
        Err(e) => { eprintln!("watch: {e}"); return 2; }
    };
    let cursor_file = cfg.cursor_file.clone();
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => { eprintln!("watch: cannot build tokio runtime: {e}"); return 2; }
    };
    rt.block_on(async move {
        use std::io::Write;
        // Cursor-file persistence after each emit is handled by run_watch's
        // per-cycle persist; for finer durability we persist here too via a
        // shared RefCell is unnecessary — per-cycle is sufficient for v1.
        let _ = &cursor_file;
        match run_watch(data_dir, cfg, |line| {
            let mut out = std::io::stdout();
            writeln!(out, "{line}").and_then(|()| out.flush()).is_ok()
        })
        .await
        {
            Ok(code) => code,
            Err(e) => { eprintln!("watch: {e}"); 2 }
        }
    })
}
```

- [ ] **Step 5: Re-export in `lib.rs`.** Next to `pub use crate::api::cli::api_cli;` (~25170) add:

```rust
pub use crate::api::watch::{api_watch, WatchConfig};
```

- [ ] **Step 6: Run unit tests + clippy (lib-scoped).**

Run: `cd src-tauri && cargo test --lib --features test-fixtures api::watch 2>&1 | tail -20 && cargo clippy --lib --features test-fixtures --no-deps 2>&1 | tail -15`
Expected: PASS, no warnings.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/api/watch.rs src-tauri/src/lib.rs
git commit -m "feat(watch): backfill+live core, reconnect loop, api_watch wrapper (ZEB-480)"
```

---

### Task 5: In-process integration test (booted node → watch)

**Files:**
- Create: `src-tauri/tests/api/watch.rs`
- Modify: `src-tauri/tests/api_tests.rs` (register the module)

**Interfaces:**
- Consumes: the `tests/api/api_server.rs` boot pattern (temp HOME, `HARMONY_PASSPHRASE`, `warm_up_iroh_global_init`, `start_node_inner(None, sink, None, &state, Some(Arc::clone(&state)))`, `ApiEventSink`), and the RPC surface `mint_owner_identity`, `create_community`, a channel-create RPC, `post_channel_message`, plus `harmony_app::api::watch::{WatchConfig, backfill, stream_once, run_watch}`.

**Design:** Boot a node in-process (mirror `tests/api/api_server.rs`). Mint owner, create a community + one channel (discover the exact RPC command names by reading `api_server.rs`/`rpc.rs` — e.g. `create_community`, `create_channel`). Post a message via the `post_channel_message` RPC. Then: (a) run `backfill(...)` with a collecting emit and assert the message appears as `source:"backfill"` exactly once; (b) run `backfill` again with the advanced cursor and assert **zero** re-emission (dedupe/cursor holds). This exercises the CI-durable path deterministically without a subprocess. (A live `stream_once` assertion is optional — it needs a concurrent post; if flaky, rely on the backfill+dedupe assertions, which cover the resume contract.)

- [ ] **Step 1: Register the module.** In `tests/api_tests.rs`, after the `api_server` line:

```rust
#[path = "api/watch.rs"]
mod watch;
```

- [ ] **Step 2: Write the integration test.** Read `tests/api/api_server.rs` first for the exact boot + community/channel/post RPC calls, then write `tests/api/watch.rs` mirroring it. Skeleton:

```rust
//! tests/api/watch.rs — ZEB-480: in-process channel-watch backfill + resume.
use crate::common;
use std::sync::{Arc, Mutex};

#[tokio::test(flavor = "multi_thread")]
async fn watch_backfill_emits_then_dedupes() {
    // 1. temp HOME + passphrase (copy api_server.rs preamble).
    // 2. warm_up_iroh_global_init().await; boot via start_node_inner(...).
    // 3. mint_owner_identity; create_community; create_channel → capture communityId, channelId.
    // 4. post_channel_message {communityId, channelId, body: b"hello".to_vec()}.
    // 5. Build Discovery pointing at the in-process server (reuse the api_server harness's
    //    server-start helper, or drive backfill against the RPC impls directly if the harness
    //    exposes a Discovery). Simplest: start the real HTTP server like api_server.rs does and
    //    read_discovery from its data-dir.
    // 6. let cfg = WatchConfig { community_id, channels: vec![channel_id], since: None,
    //       cursor_file: None, raw: false, no_retry: true };
    //    let mut cursors = CursorSet::load(&cfg).unwrap();
    //    let mut got = vec![]; let mut emit = |s:&str|{got.push(s.to_string()); true};
    //    backfill(&d, &cfg, &mut cursors, &mut emit).await.unwrap();
    //    assert exactly one line, source=="backfill", body=="hello".
    // 7. got.clear(); backfill(&d, &cfg, &mut cursors, &mut emit).await.unwrap();
    //    assert got.is_empty()  // cursor advanced; no re-emission.
}
```

Fill in the preamble/RPC names from `api_server.rs` (do not invent command names — use the ones that file already calls). Use camelCase arg keys.

- [ ] **Step 3: Run the integration test.**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(watch_backfill)' 2>&1 | tail -25`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/tests/api/watch.rs src-tauri/tests/api_tests.rs
git commit -m "test(watch): in-process backfill + resume-dedupe integration (ZEB-480)"
```

---

### Task 6: `Command::Watch` CLI wiring

**Files:**
- Modify: `src-tauri/src/main.rs`

**Interfaces:**
- Consumes: `harmony_app::{api_watch, WatchConfig}`, `harmony_app::api::watch::parse_since` (or re-export `parse_since`).

- [ ] **Step 1: Add the `Watch` variant** to `enum Command` (after `Api`):

```rust
    /// Watch a running node's channel(s) for new messages, emitting one JSON
    /// line per message on stdout (filtered, resumable, self-healing). The push
    /// analog of `api --events`. Reads <data-dir>/api/{port,token}.
    Watch {
        /// Community id (hex) the channels belong to.
        #[arg(long, value_name = "HEX")]
        community: String,
        /// Channel id (hex) to watch; repeatable (>=1).
        #[arg(long = "channel", value_name = "HEX", required = true)]
        channels: Vec<String>,
        /// Resume cursor `wallMs:logical:deviceId`; seeds all channels.
        #[arg(long, value_name = "HLC")]
        since: Option<String>,
        /// Self-persist + auto-resume per-channel cursors at this path.
        #[arg(long, value_name = "PATH")]
        cursor_file: Option<PathBuf>,
        /// Emit untouched firehose frames instead of the normalized projection.
        #[arg(long)]
        raw: bool,
        /// Exit on disconnect (code 3) instead of reconnecting.
        #[arg(long)]
        no_retry: bool,
    },
```

- [ ] **Step 2: Add the dispatch arm** (after the `Command::Api` arm, ~main.rs:323):

```rust
                Some(Command::Watch {
                    community,
                    channels,
                    since,
                    cursor_file,
                    raw,
                    no_retry,
                }) => {
                    init_tracing();
                    let since = match since.as_deref().map(harmony_app::api::watch::parse_since) {
                        Some(Ok(h)) => Some(h),
                        Some(Err(e)) => { eprintln!("watch: {e}"); std::process::exit(2); }
                        None => None,
                    };
                    let cfg = harmony_app::WatchConfig {
                        community_id: community,
                        channels,
                        since,
                        cursor_file,
                        raw,
                        no_retry,
                    };
                    std::process::exit(harmony_app::api_watch(cfg));
                }
```

Ensure `parse_since` is reachable: it's `pub fn` in `api::watch`, and `api` is `pub mod`, so `harmony_app::api::watch::parse_since` resolves.

- [ ] **Step 3: Build the binary + smoke the arg parsing.**

Run: `cd src-tauri && cargo build --bin harmony-app 2>&1 | tail -5 && ./target/debug/harmony-app watch --help 2>&1 | head -20`
Expected: builds; `--help` shows the options.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/main.rs
git commit -m "feat(watch): wire top-level `harmony-app watch` subcommand (ZEB-480)"
```

---

### Task 7: Playbook doc

**Files:**
- Create: `docs/playbooks/agent-channel-watch.md`

- [ ] **Step 1: Write the playbook.** Cover: what `harmony-app watch` is; the exact invocation against a fleet coordination node (`harmony-app --profile fleet-koya watch --community <hex> --channel <hex> --cursor-file <path>`); the NDJSON line shape; and the Claude-Code `run_in_background` wake pattern — a background process running `watch` whose stdout lines re-invoke the agent loop (the analog of the GitHub PR Monitor), noting that the CC-side trigger glue lives in harness config, not Harmony product code. Include a resume example (kill + relaunch with the same `--cursor-file` catches up).

- [ ] **Step 2: Commit**

```bash
git add docs/playbooks/agent-channel-watch.md
git commit -m "docs(watch): agent channel-watch playbook (ZEB-480)"
```

---

### Task 8: e2e subprocess smoke (`--features e2e`, never in CI)

**Files:**
- Create: `e2e-harness/tests/e2e_watch.rs`

**Interfaces:**
- Consumes: `e2e_harness::{NodeConfig, NodeHandle, RunDir}` (spawn pattern from `e2e_two_node.rs`), `tokio::process::Command` (to spawn the real `harmony-app watch` and read its stdout).

- [ ] **Step 1: Write the smoke** (gated `#![cfg(feature = "e2e")]`): spawn a node, mint, create community + channel, spawn `harmony-app --profile <p> watch --community <hex> --channel <hex> --no-retry` as a child capturing stdout, post a message via the node RPC, read child stdout lines with a timeout until the message body appears, assert, then kill the child. This proves the subprocess/stdout path the playbook's wrapper depends on. If it proves flaky, the in-process integration test (Task 5) is the durable coverage — note that in the file header.

- [ ] **Step 2: Build + run behind the feature.**

Run: `cd src-tauri && cargo build --bin harmony-app && cd ../e2e-harness && cargo nextest run --features e2e -E 'test(watch)' 2>&1 | tail -25`
(Requires the fresh `harmony-app` bin — the e2e stale-binary trap: rebuild the bin first, pin `HARMONY_APP_BIN` if the harness reads it.)

- [ ] **Step 3: Commit**

```bash
git add e2e-harness/tests/e2e_watch.rs
git commit -m "test(watch): --features e2e subprocess smoke (ZEB-480)"
```

---

## Final gate (before PR)

- [ ] `cd src-tauri && cargo fmt --all -- --check`
- [ ] `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` (full CI-parity sweep)
- [ ] Open PR; fire `@coderabbitai review` once at open; converge one push per round; never auto-merge.

## Self-review notes

- **Spec coverage:** filter (Task 4 `handle_frame`), resume/backfill (Task 4 `backfill` + Task 3 cursor), self-heal reconnect (Task 4 `run_watch`), hybrid `--since`/`--cursor-file` (Task 3), NDJSON projection + `--raw` (Tasks 1, 4), CLI surface (Task 6), playbook (Task 7), tests (Tasks 1-5, 8). All spec sections mapped.
- **Type consistency:** `WireMessage` (Deserialize+Serialize) and `WatchLine` (Serialize) are the only new wire types; `HlcDto` reused. `WatchConfig`/`CursorSet`/`ChannelCursor` names consistent across Tasks 2-6. `parse_since`/`format_since` inverse pair. `FrameOutcome`/`StreamEnd` distinguish consumer-stop from reconnect.
- **Known limitation (documented in Task 2):** a message materializing during downtime with an HLC earlier than the high-water mark is not re-fetched by backfill — acceptable for coordination; full gap-freedom deferred.
