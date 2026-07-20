//! src-tauri/src/api/watch.rs — ZEB-480: `harmony-app watch` — filtered,
//! resumable, self-healing channel-message watch over the headless WS firehose.
//!
//! The push analog of `api --events`: subscribe to `/v1/events`, keep only
//! `channel-message-received` frames for the target channel(s), resume across
//! restarts/reconnects from an HLC cursor (via `list_channel_messages` backfill,
//! because the firehose `seq` is process-lifetime and non-resumable), and emit
//! one normalized NDJSON line per message.
//!
//! Stdout purity (PR #231 discipline, inherited from `api_cli`): NDJSON frames
//! are the ONLY stdout; every diagnostic goes to stderr. Exit codes: 0 clean
//! stop, 1 server error, 2 local/usage, 3 `--no-retry` disconnect.

use std::cmp::Ordering;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

use crate::community_channel_log_engine::HlcDto;

/// Bounded recent-`messageId` window per channel. The emit gate is id-dedupe
/// (NOT strict-greater HLC): this system allows backdated HLCs (at-event-HLC
/// model), so an HLC gate would silently drop legitimately-late messages. The
/// window need only exceed the messages in flight at any resume boundary — a
/// re-backfill from the persisted high-water cursor only re-fetches the small
/// overlap, well within this bound.
const DEDUPE_WINDOW: usize = 256;

/// Backfill page size (the RPC caps `limit` at 1000).
const BACKFILL_PAGE: u32 = 200;

// ---------------------------------------------------------------------------
// Projection: WireMessage (Deserialize) -> WatchLine (Serialize)
// ---------------------------------------------------------------------------

/// Wire twin of `ChannelMessageDto`, used only to extract the fields the watch
/// needs (id/at for the cursor, plus the projection fields). The DTO is
/// `Serialize`-only (its `kind: Option<&'static str>` can't deserialize), so
/// both the backfill array rows and the live `payload.message` parse into this.
/// `--raw` never goes through here — it emits the untouched original row/frame
/// (see `backfill`/`handle_frame`), so fields not modeled here (e.g. reactions,
/// attachments) still appear verbatim in raw output.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mentions: Option<Vec<String>>,
}

/// One emitted NDJSON line — the normalized projection, uniform across
/// backfill (`list_channel_messages`) and live (firehose) sources.
#[derive(serde::Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WatchLine {
    /// `"backfill"` | `"live"`.
    pub source: &'static str,
    /// Firehose `seq` for live frames; `null` for backfill rows.
    pub seq: Option<u64>,
    pub community_id: String,
    pub channel_id: String,
    pub message_id: String,
    pub author: String,
    pub at: HlcDto,
    /// Decoded UTF-8 (lossy; the channel-log engine enforces UTF-8 bodies).
    pub body: String,
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

// ---------------------------------------------------------------------------
// HLC cursor helpers
// ---------------------------------------------------------------------------

/// Total order over an HLC: `(wallMs, logical, deviceId)`.
pub fn hlc_cmp(a: &HlcDto, b: &HlcDto) -> Ordering {
    a.wall_ms
        .cmp(&b.wall_ms)
        .then(a.logical.cmp(&b.logical))
        .then(a.device_id.cmp(&b.device_id))
}

/// Parse a `--since` cursor `wallMs:logical:deviceId`.
pub fn parse_since(s: &str) -> Result<HlcDto, String> {
    let (w, l, d) = {
        let mut it = s.splitn(3, ':');
        match (it.next(), it.next(), it.next()) {
            (Some(w), Some(l), Some(d)) => (w, l, d),
            _ => return Err(format!("--since {s:?}: expected wallMs:logical:deviceId")),
        }
    };
    Ok(HlcDto {
        wall_ms: w
            .parse()
            .map_err(|e| format!("--since wallMs {w:?}: {e}"))?,
        logical: l
            .parse()
            .map_err(|e| format!("--since logical {l:?}: {e}"))?,
        device_id: d.to_string(),
    })
}

/// Inverse of [`parse_since`].
pub fn format_since(h: &HlcDto) -> String {
    format!("{}:{}:{}", h.wall_ms, h.logical, h.device_id)
}

/// Per-channel resume state: a high-water HLC (bounds the backfill `since`) plus
/// a bounded recent-id window (the emit gate).
#[derive(Default, Debug)]
pub struct ChannelCursor {
    hlc: Option<HlcDto>,
    recent: VecDeque<String>,
}

impl ChannelCursor {
    pub fn with_since(hlc: Option<HlcDto>) -> Self {
        ChannelCursor {
            hlc,
            recent: VecDeque::new(),
        }
    }

    /// True iff `message_id` has not been emitted recently. On accept, records
    /// the id (FIFO, capped at `DEDUPE_WINDOW`) and advances the high-water HLC.
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

// ---------------------------------------------------------------------------
// Config + cursor set (with per-emit cursor-file persistence)
// ---------------------------------------------------------------------------

/// Resolved `watch` invocation.
#[derive(Clone, Debug)]
pub struct WatchConfig {
    pub community_id: String,
    pub channels: Vec<String>,
    pub since: Option<HlcDto>,
    pub cursor_file: Option<PathBuf>,
    pub raw: bool,
    pub no_retry: bool,
}

/// Per-channel cursors plus the optional durable cursor-file. Persists after
/// each accepted message so re-emission on an ungraceful restart is bounded to
/// the single in-flight message (honors the "exactly once" resume contract).
pub struct CursorSet {
    per_channel: BTreeMap<String, ChannelCursor>,
    cursor_file: Option<PathBuf>,
}

impl CursorSet {
    /// Seed each channel's initial HLC: cursor-file entry (if any) > `--since` > none.
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
        Ok(CursorSet {
            per_channel,
            cursor_file: cfg.cursor_file.clone(),
        })
    }

    /// Emit gate for `channel`: false if not watched or already emitted.
    pub fn accept(&mut self, channel: &str, id: &str, at: &HlcDto) -> bool {
        match self.per_channel.get_mut(channel) {
            Some(c) => c.accept(id, at),
            None => false,
        }
    }

    pub fn since(&self, channel: &str) -> Option<HlcDto> {
        self.per_channel.get(channel).and_then(|c| c.since())
    }

    /// Persist iff a cursor-file is configured. Best-effort: a write failure is
    /// reported to stderr but does not abort the stream (the in-memory cursor is
    /// still authoritative for this run).
    pub fn maybe_persist(&self) {
        if let Some(p) = &self.cursor_file {
            if let Err(e) = self.persist(p) {
                eprintln!("watch: cursor-file: {e}");
            }
        }
    }

    /// Atomic write of `{channelHex: HlcDto}` (temp + rename). Creates the
    /// parent dir first so a first-run `--cursor-file .../watch/fleet.json`
    /// under a not-yet-existing directory persists instead of failing ENOENT.
    pub fn persist(&self, path: &Path) -> Result<(), String> {
        let map: BTreeMap<&String, HlcDto> = self
            .per_channel
            .iter()
            .filter_map(|(ch, c)| c.since().map(|h| (ch, h)))
            .collect();
        let json = serde_json::to_string(&map).map_err(|e| format!("serialize cursors: {e}"))?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create cursor-file dir {}: {e}", parent.display()))?;
            }
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("rename cursor-file: {e}"))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Async core: backfill + live stream + reconnect loop + blocking wrapper
// ---------------------------------------------------------------------------

use crate::api::cli::{read_discovery, rpc_call, stream_events, Discovery};

/// Why a single live-stream pass ended.
pub enum StreamEnd {
    /// The consumer (stdout pipe) went away — a clean, terminal stop.
    ConsumerStop,
    /// Socket closed or `_lagged` — the caller should re-backfill + reconnect.
    Reconnect,
}

/// Per-frame disposition (the sync inner of the live stream).
pub enum FrameOutcome {
    Continue,
    ConsumerStop,
    Reconnect,
}

/// Sync per-frame handler (unit-testable): filter → project → emit → persist.
/// `emit` returns false when the consumer is gone.
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
    let ch = payload
        .get("channelId")
        .and_then(|c| c.as_str())
        .unwrap_or_default();
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
    let keep_going = emit(&out);
    cursors.maybe_persist();
    if keep_going {
        FrameOutcome::Continue
    } else {
        FrameOutcome::ConsumerStop
    }
}

/// Drain `list_channel_messages` history for every watched channel from its
/// current cursor, emitting `source:"backfill"`. Returns `Ok(true)` if the
/// consumer stopped (stdout closed) mid-drain — so the caller can terminate
/// instead of opening a live stream — or `Ok(false)` on normal completion.
pub async fn backfill(
    d: &Discovery,
    cfg: &WatchConfig,
    cursors: &mut CursorSet,
    emit: &mut impl FnMut(&str) -> bool,
) -> Result<bool, String> {
    for ch in &cfg.channels {
        loop {
            let since = cursors.since(ch);
            let args = serde_json::json!({
                "communityId": cfg.community_id,
                "channelId": ch,
                "since": since,
                "limit": BACKFILL_PAGE,
                "order": "asc",
            });
            let (status, body) =
                rpc_call(d, "list_channel_messages", Some(&args.to_string())).await?;
            if status != 200 {
                return Err(format!("list_channel_messages HTTP {status}: {body}"));
            }
            // Parse as raw JSON values so `--raw` emits rows verbatim (including
            // fields WireMessage doesn't model, e.g. reactions/attachments); each
            // row is then deserialized into WireMessage for the cursor + projection.
            let rows: Vec<serde_json::Value> =
                serde_json::from_str(&body).map_err(|e| format!("parse backfill: {e}"))?;
            let page = rows.len();
            for row in &rows {
                let m: WireMessage = serde_json::from_value(row.clone())
                    .map_err(|e| format!("parse backfill row: {e}"))?;
                if !cursors.accept(ch, &m.message_id, &m.at) {
                    continue;
                }
                let out = if cfg.raw {
                    serde_json::to_string(row).map_err(|e| format!("serialize row: {e}"))?
                } else {
                    serde_json::to_string(&WatchLine::from_wire(&m, "backfill", None))
                        .map_err(|e| format!("serialize line: {e}"))?
                };
                let keep_going = emit(&out);
                cursors.maybe_persist();
                if !keep_going {
                    return Ok(true); // consumer stop
                }
            }
            if page < BACKFILL_PAGE as usize {
                break; // last page
            }
        }
    }
    Ok(false)
}

/// One live-stream pass: subscribe to `/v1/events`, dispatch each frame through
/// [`handle_frame`], and report why it ended.
pub async fn stream_once(
    d: &Discovery,
    cfg: &WatchConfig,
    cursors: &mut CursorSet,
    emit: &mut impl FnMut(&str) -> bool,
) -> Result<StreamEnd, String> {
    let mut reason = StreamEnd::Reconnect; // default when the socket closes
    stream_events(d, |frame| match handle_frame(frame, cfg, cursors, emit) {
        FrameOutcome::Continue => true,
        FrameOutcome::ConsumerStop => {
            reason = StreamEnd::ConsumerStop;
            false
        }
        FrameOutcome::Reconnect => {
            reason = StreamEnd::Reconnect;
            false
        }
    })
    .await?;
    Ok(reason)
}

/// The full watch: catch-up → live → (self-heal reconnect). Returns the process
/// exit code. `emit` returns false when the consumer is gone (clean stop).
pub async fn run_watch(
    data_dir: PathBuf,
    cfg: WatchConfig,
    mut emit: impl FnMut(&str) -> bool,
) -> Result<i32, String> {
    let mut cursors = CursorSet::load(&cfg)?;
    let mut d = read_discovery(&data_dir)?;
    let mut backoff = std::time::Duration::from_millis(250);
    let progressed = std::cell::Cell::new(false);
    loop {
        progressed.set(false);
        let outcome = {
            let mut counting = |line: &str| {
                progressed.set(true);
                emit(line)
            };
            async {
                if backfill(&d, &cfg, &mut cursors, &mut counting).await? {
                    return Ok(StreamEnd::ConsumerStop); // stdout closed during catch-up
                }
                stream_once(&d, &cfg, &mut cursors, &mut counting).await
            }
            .await
        };
        // Exit-code contract (module header): 0 clean stop, 1 server error,
        // 2 local/usage (mapped by api_watch from the initial-Err path above),
        // 3 --no-retry disconnect.
        match outcome {
            Ok(StreamEnd::ConsumerStop) => return Ok(0),
            Ok(StreamEnd::Reconnect) => {
                if cfg.no_retry {
                    return Ok(3); // clean disconnect / _lagged under --no-retry
                }
            }
            Err(e) => {
                eprintln!("watch: {e}");
                if cfg.no_retry {
                    return Ok(1); // server/transport error under --no-retry
                }
            }
        }
        // Port/token may have rotated on a node restart — re-read discovery.
        if let Ok(nd) = read_discovery(&data_dir) {
            d = nd;
        }
        // Reset backoff after a productive cycle (fast resume); otherwise grow to
        // the 5s cap so a flapping/unreachable node can't spin a tight reconnect
        // loop (the node's discovery files persist even when its firehose is down).
        backoff = if progressed.get() {
            std::time::Duration::from_millis(250)
        } else {
            (backoff * 2).min(std::time::Duration::from_secs(5))
        };
        tokio::time::sleep(backoff).await;
    }
}

/// Blocking CLI entry (mirrors `api_cli`): validate, resolve discovery dir, run
/// on a current-thread runtime, write NDJSON to stdout, return the exit code.
pub fn api_watch(cfg: WatchConfig) -> i32 {
    if cfg.channels.is_empty() {
        eprintln!("watch: at least one --channel is required");
        return 2;
    }
    let data_dir = match crate::resolve_app_data_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("watch: {e}");
            return 2;
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("watch: cannot build tokio runtime: {e}");
            return 2;
        }
    };
    rt.block_on(async move {
        use std::io::Write;
        match run_watch(data_dir, cfg, |line| {
            // Explicit flush: stdout is block-buffered when piped, and agents
            // tail this stream live. A failed write/flush means the consumer is
            // gone (e.g. `| head -n1` exited) — stop cleanly.
            let mut out = std::io::stdout();
            writeln!(out, "{line}").and_then(|()| out.flush()).is_ok()
        })
        .await
        {
            Ok(code) => code,
            Err(e) => {
                eprintln!("watch: {e}");
                2
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hlc(w: u64, l: u32, d: &str) -> HlcDto {
        HlcDto {
            wall_ms: w,
            logical: l,
            device_id: d.into(),
        }
    }

    fn wire(body: &[u8], kind: Option<&str>) -> WireMessage {
        serde_json::from_value(json!({
            "messageId": "m1", "communityId": "c1", "channelId": "ch1",
            "author": "a1", "at": {"wallMs": 100u64, "logical": 2u32, "deviceId": "d1"},
            "body": body, "kind": kind,
        }))
        .expect("wire msg")
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
        let v = serde_json::to_value(&line).unwrap();
        assert_eq!(v["source"], "live");
        assert!(v.get("replyTo").is_none() && v.get("kind").is_none());
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

    #[test]
    fn since_roundtrips() {
        let h = hlc(1721000000000, 3, "abc");
        assert_eq!(parse_since(&format_since(&h)).unwrap(), h);
        assert!(parse_since("nope").is_err());
        assert!(parse_since("1:2").is_err());
    }

    #[test]
    fn hlc_total_order() {
        use std::cmp::Ordering::*;
        assert_eq!(hlc_cmp(&hlc(1, 0, "a"), &hlc(2, 0, "a")), Less);
        assert_eq!(hlc_cmp(&hlc(1, 5, "a"), &hlc(1, 2, "a")), Greater);
        assert_eq!(hlc_cmp(&hlc(1, 2, "a"), &hlc(1, 2, "b")), Less);
    }

    #[test]
    fn cursor_dedupes_by_id_not_hlc() {
        let mut c = ChannelCursor::default();
        assert!(c.accept("m1", &hlc(10, 0, "d")));
        assert!(!c.accept("m1", &hlc(10, 0, "d")));
        // A backdated but NEW message still emits (id-dedupe, not HLC gate).
        assert!(c.accept("m0", &hlc(5, 0, "d")));
        // since() is the high-water mark, unaffected by the backdated message.
        assert_eq!(c.since().unwrap(), hlc(10, 0, "d"));
    }

    #[test]
    fn cursor_window_evicts_oldest() {
        let mut c = ChannelCursor::default();
        for i in 0..(DEDUPE_WINDOW + 10) {
            assert!(c.accept(&format!("m{i}"), &hlc(i as u64, 0, "d")));
        }
        // "m0" scrolled out of the window → accepted again (bounded memory).
        assert!(c.accept("m0", &hlc(0, 0, "d")));
    }

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
        set.maybe_persist();

        let set2 = CursorSet::load(&cfg).unwrap();
        assert_eq!(set2.since("ch1").unwrap(), hlc(50, 0, "d"));
        assert!(set2.since("ch2").is_none());
    }

    #[test]
    fn cursor_file_creates_missing_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        // Parent dirs do not exist yet (first-run --cursor-file .../watch/fleet.json).
        let path = dir.path().join("watch").join("nested").join("fleet.json");
        let cfg = WatchConfig {
            community_id: "c1".into(),
            channels: vec!["ch1".into()],
            since: None,
            cursor_file: Some(path.clone()),
            raw: false,
            no_retry: false,
        };
        let mut set = CursorSet::load(&cfg).unwrap();
        assert!(set.accept("ch1", "m1", &hlc(7, 0, "d")));
        set.persist(&path)
            .expect("persist must create parent dirs, not ENOENT");
        assert!(
            path.exists(),
            "cursor-file written under freshly-created dirs"
        );

        let set2 = CursorSet::load(&cfg).unwrap();
        assert_eq!(set2.since("ch1").unwrap(), hlc(7, 0, "d"));
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

    #[test]
    fn accept_unwatched_channel_is_false() {
        let cfg = WatchConfig {
            community_id: "c1".into(),
            channels: vec!["ch1".into()],
            since: None,
            cursor_file: None,
            raw: false,
            no_retry: false,
        };
        let mut set = CursorSet::load(&cfg).unwrap();
        assert!(!set.accept("nope", "m1", &hlc(1, 0, "d")));
    }

    fn frame(ch: &str, id: &str) -> String {
        json!({
            "seq": 1u64, "event": "channel-message-received",
            "payload": {"communityId": "c1", "channelId": ch, "message": {
                "messageId": id, "communityId": "c1", "channelId": ch, "author": "a",
                "at": {"wallMs": 1u64, "logical": 0u32, "deviceId": "d"}, "body": [104, 105]
            }}
        })
        .to_string()
    }

    #[test]
    fn handle_frame_filters_projects_and_dedupes() {
        let cfg = WatchConfig {
            community_id: "c1".into(),
            channels: vec!["ch1".into()],
            since: None,
            cursor_file: None,
            raw: false,
            no_retry: false,
        };
        let mut cursors = CursorSet::load(&cfg).unwrap();
        let mut got: Vec<String> = vec![];
        let mut emit = |s: &str| {
            got.push(s.to_string());
            true
        };

        // target channel → emitted
        assert!(matches!(
            handle_frame(&frame("ch1", "m1"), &cfg, &mut cursors, &mut emit),
            FrameOutcome::Continue
        ));
        // other channel → dropped
        assert!(matches!(
            handle_frame(&frame("ch2", "m2"), &cfg, &mut cursors, &mut emit),
            FrameOutcome::Continue
        ));
        // duplicate id on target channel → deduped
        assert!(matches!(
            handle_frame(&frame("ch1", "m1"), &cfg, &mut cursors, &mut emit),
            FrameOutcome::Continue
        ));
        // non-message event → dropped
        assert!(matches!(
            handle_frame(
                r#"{"seq":2,"event":"profile-update","payload":{}}"#,
                &cfg,
                &mut cursors,
                &mut emit
            ),
            FrameOutcome::Continue
        ));
        // _lagged sentinel → reconnect
        assert!(matches!(
            handle_frame(
                r#"{"seq":null,"event":"_lagged","payload":{"missed":3}}"#,
                &cfg,
                &mut cursors,
                &mut emit
            ),
            FrameOutcome::Reconnect
        ));

        assert_eq!(got.len(), 1, "only one target-channel message emitted once");
        let line: serde_json::Value = serde_json::from_str(&got[0]).unwrap();
        assert_eq!(line["channelId"], "ch1");
        assert_eq!(line["source"], "live");
        assert_eq!(line["seq"], 1);
        assert_eq!(line["body"], "hi");
    }

    #[test]
    fn handle_frame_raw_emits_untouched_frame() {
        let cfg = WatchConfig {
            community_id: "c1".into(),
            channels: vec!["ch1".into()],
            since: None,
            cursor_file: None,
            raw: true,
            no_retry: false,
        };
        let mut cursors = CursorSet::load(&cfg).unwrap();
        let mut got = vec![];
        let mut emit = |s: &str| {
            got.push(s.to_string());
            true
        };
        let f = frame("ch1", "m1");
        handle_frame(&f, &cfg, &mut cursors, &mut emit);
        assert_eq!(got, vec![f], "raw mode emits the untouched firehose frame");
    }

    #[test]
    fn handle_frame_consumer_stop_propagates() {
        let cfg = WatchConfig {
            community_id: "c1".into(),
            channels: vec!["ch1".into()],
            since: None,
            cursor_file: None,
            raw: false,
            no_retry: false,
        };
        let mut cursors = CursorSet::load(&cfg).unwrap();
        let mut emit = |_: &str| false; // consumer gone
        assert!(matches!(
            handle_frame(&frame("ch1", "m1"), &cfg, &mut cursors, &mut emit),
            FrameOutcome::ConsumerStop
        ));
    }
}
