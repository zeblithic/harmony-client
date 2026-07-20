# ZEB-480: `api watch` — push-based channel-monitor harness

**Status:** design approved 2026-07-20 (Jake). Under ZEB-451 (agent dev/testing ecosystem). Graduates the ZEB-470 → ZEB-477 Linear-coordination hack onto Harmony itself.

## Problem

The fleet (Koya / Ildwyn / AVALON) already flipped its coordination bus onto Harmony
(2026-06-21): agents talk in the "Zeblithic Fleet" community and watch live with
`harmony-app … api --events`. But `api --events` is an **unfiltered, live-from-connect,
no-resume** dump of the whole event firehose. If the watcher process dies (agent restart,
serve-node restart, or a `broadcast` overflow), every message in that window is **silently
missed** — there is no catch-up. And the agent has to eyeball the whole firehose to find the
one channel it cares about.

ZEB-480 closes that hole: a **filtered, resumable, self-healing** channel watch that turns the
firehose into a reliable agent-wake trigger — the Harmony analog of Claude Code's GitHub PR
`Monitor`.

## Key facts (established by code investigation, 2026-07-20)

All file:line references are on `main` @ `dad0e64e`.

- **Firehose (`GET /v1/events`, WS)** emits `EventFrame { seq: u64, event: String, payload:
  Value }` (`src-tauri/src/api/events.rs:11-16`), one JSON text frame per emit. `seq` is
  **process-lifetime monotonic, not persisted, not per-connection** (`events.rs:22-69`). There
  is **no server-side resume** — module doc: *"No replay: agents connect before acting"*
  (`events.rs:6-7`). The channel is a bounded `broadcast` (capacity 1024); a lagging client
  gets one sentinel frame `{"seq":null,"event":"_lagged","payload":{"missed":n}}`
  (`events.rs:73-110`) — the only frame with a null `seq`.
- **Channel messages are already on the firehose.** Event name **`channel-message-received`**,
  payload `ChannelMessageReceivedPayload { communityId, channelId, message: ChannelMessageDto }`
  (`community_channel_log_engine.rs:285-291`, emitted at `:1242-1250` on the node-wide sink for
  both locally-posted and inbound messages). So serve nodes already stream it.
- **`ChannelMessageDto`** (`community_channel_log_engine.rs:142-178`, camelCase) carries
  everything a wake needs: `messageId`, `communityId`, `channelId`, `author` (owner-id hex),
  `at: HlcDto { wallMs, logical, deviceId }`, `body: Vec<u8>` (UTF-8-enforced plaintext),
  optional `replyTo`, `kind` (`"poll"`), `pollId`, `mentions` (owner-id hex list), `reactions`,
  `attachments`.
- **The only resumable coordinate is the per-message HLC.** Read RPC **`list_channel_messages`**
  (`api/rpc.rs:795`, args `ListChannelMessagesArgs { communityId, channelId, since:
  Option<HlcDto>, limit: u32, order: Option<String> }` at `:262-271`; `order` is `"asc"`
  default / `"desc"`, ZEB-602) replays history from an HLC cursor. The live event's
  `message.at` is the same coordinate space. **Firehose `seq` is a gap-detector, not an
  offset** — resume must be client-side, keyed on HLC.
- **CLI seam.** `enum Command` in `src-tauri/src/main.rs:20-81` (`RotatePassphrase, Export,
  Restore, Serve, Api`). `Command::Api { command, args, events }` → `harmony_app::api_cli(...)`.
  The connect pattern (`src-tauri/src/api/cli.rs`): `resolve_app_data_dir()` →
  `read_discovery(&data_dir)` (reads `<data-dir>/api/{port,token}`) → tokio current-thread
  runtime → HTTP `rpc_call` (`cli.rs:51`) / WS `stream_events` (`cli.rs:83`). Auth = bearer
  token (`auth.rs`).
- **WS client already exists** in two forms: `api/cli.rs::stream_events` (raw-text `FnMut(&str)
  -> bool` callback) and `e2e-harness/src/events.rs::subscribe` (typed `EventFrame { seq:
  Option<u64>, event, payload }` over an mpsc receiver, plus `await_event(rx, timeout, pred)`).
  Both use `tokio-tungstenite`. `NodeHandle::events()` (`e2e-harness/src/node.rs:306`) opens the
  firehose for harness tests.

## Design

### Surface — new top-level `Command::Watch`, thin shell over a testable `api_watch(...)` lib fn

Mirror the `Api`/`Serve` seam: a **top-level** `Command::Watch` variant in `main.rs` (sibling to
`serve`/`api`) dispatches to a new `harmony_app::api_watch(cfg)` library function (so the logic
is unit/integration-testable in-process, exactly as `api_cli` is), with a new module
`src-tauri/src/api/watch.rs`.

```
harmony-app [--profile P] watch
    --community <hex>                 # required (list_channel_messages needs it)
    --channel   <hex>                 # repeatable, >=1; each within --community
    [--since <wallMs:logical:deviceId>]   # explicit resume cursor (HLC), seeds all channels
    [--cursor-file <path>]            # self-persist + auto-resume (hybrid convenience)
    [--raw]                           # emit untouched firehose frames instead of the projection
    [--no-retry]                      # exit on disconnect instead of reconnecting (tests/scripts)
```

**Clap shape decision.** `watch` is a **top-level** subcommand (sibling to `serve`/`api`), NOT
nested under `api`. `Command::Api { command: String, args, events }` takes a positional RPC name
plus `--events`; nesting `watch` there would force restructuring that surface and **break the
fleet's existing `api --events` / `api <rpc>` usage**. A top-level `watch` is non-breaking and
clap-clean — the push analog of `api --events`. `--channel` is required (≥1) so v1 needs no
list-channels RPC; watching *all* channels in a community is a documented follow-up.

### Behavior

1. **Resolve cursor.** Per-channel initial cursors resolve as: `--cursor-file`'s stored entry for
   that channel (if the file exists and has one) > `--since` (a single HLC that **seeds every**
   `--channel`) > none. "None" means live-only for that channel (start from now, no backfill).
   `--since` and `--cursor-file` may be combined: the file supplies known channels, `--since`
   seeds any channel absent from it.
2. **Catch-up.** For each `--channel`, page `list_channel_messages{communityId, channelId,
   since: cursor, order: "asc", limit: 200}` draining until a page returns fewer than `limit`,
   emit each with `source:"backfill"`, advance the cursor. (Per-channel cursors are tracked
   independently; see Resume correctness.)
3. **Live.** Open `/v1/events`; keep only frames where `event == "channel-message-received"` and
   `payload.channelId ∈ channels`; emit each with `source:"live"` and its firehose `seq`;
   advance that channel's cursor.
4. **Self-heal.** On a `_lagged` sentinel **or** a socket close/error, re-read discovery (a node
   restart rotates port+token), reconnect with bounded exponential backoff (250 ms → 5 s cap),
   then **re-run catch-up from the current per-channel cursors** before resuming live. `--no-retry`
   exits with a distinct non-zero code instead.
5. **Persist.** If `--cursor-file` is set, it stores a per-channel map, JSON `{ "<channelHex>":
   HlcDto, … }`. After each emit, atomically rewrite the file (temp-file + rename) with that
   channel's advanced cursor, so a restarted watcher resumes each channel exactly where it left
   off.

### Output — NDJSON projection (one object per line; `--raw` opts out)

The live payload's `message` and the backfill row are the **same `ChannelMessageDto`**, so the
projection is uniform. The one transform is decoding `body: Vec<u8>` → UTF-8 text (via
`String::from_utf8_lossy`; the engine enforces UTF-8 so this is normally lossless) — a shell
consumer should not have to decode a JSON number array.

```json
{"source":"live","seq":42,"communityId":"…","channelId":"…","messageId":"…",
 "author":"…","at":{"wallMs":1721000000000,"logical":3,"deviceId":"…"},
 "body":"(Ildwyn) roster converged","replyTo":null,"kind":null,"mentions":[]}
```

`WatchLine` fields: `source` (`"backfill"|"live"`), `seq` (`u64` live / `null` backfill),
`communityId`, `channelId`, `messageId`, `author`, `at: HlcDto`, `body: String`, and
`replyTo` / `kind` / `pollId` / `mentions` (omitted when absent, matching the DTO's
`skip_serializing_if`). `reactions` and `attachments` are **not** in the projection (not
wake-relevant); a consumer that needs them uses `--raw` or a follow-up RPC. `--raw` emits the
untouched firehose frame for live and the raw `ChannelMessageDto` JSON for backfill.

Stdout carries **only** NDJSON (frames); all diagnostics (reconnects, backoff, catch-up counts)
go to stderr — same discipline as `api_cli`. Exit codes mirror `api_cli`: 0 clean stop, 1 server
error, 2 local/usage error; `--no-retry` disconnect = a distinct code (e.g. 3).

### Resume correctness

The cursor is an **HLC compared in total order** `(wallMs, logical, deviceId)`. A message is
emitted iff its HLC is **strictly greater** than the channel's current cursor. Because
`list_channel_messages{since}`'s inclusive/exclusive boundary is not guaranteed and the
backfill→live handoff can overlap, the watch also keeps a **bounded recent-`messageId` set**
(per channel, last K ids) and drops any message whose id it has already emitted. Strict-greater
HLC + id-dedupe together guarantee the handoff never double-emits and never skips.

### Auth / scoping

Reuses the serve bearer token (read from `<data-dir>/api/token`), so the watch is already scoped
to the local node. A node can only watch channels it is a member of — it already receives those
`channel-message-received` events and can `list_channel_messages` them; no new ACL in v1.

## Testing

- **Unit (CI):**
  - channel filter: a `channel-message-received` frame for a target channel is kept; one for a
    non-target channel and a non-`channel-message-received` frame are dropped.
  - projection: `ChannelMessageDto` → `WatchLine` maps every field; `body` decodes to text;
    `kind`/`replyTo`/`mentions` omit-when-absent.
  - cursor: HLC total-order comparison; strict-greater gate; recent-id dedupe drops a replayed
    id at the backfill→live boundary; `--cursor-file` round-trips (`parse(format(hlc)) == hlc`).
- **Integration (CI, in-process, no subprocess):** boot a node like `serve_cli` (reuse the
  `tests/api/api_server.rs` harness pattern), drive `api_watch(...)` against it: post a message
  via the `post_channel_message` RPC → assert a `source:"live"` line for it; advance/stop, post
  again while "down", restart the watch with the persisted cursor → assert the missed message
  arrives as `source:"backfill"` exactly once (dedupe holds across the boundary).
- **e2e smoke (`--features e2e`, never in CI):** spawn the real `harmony-app api-watch` CLI via
  `NodeHandle`, post from a second node, assert the CLI prints the NDJSON line. This proves the
  subprocess/stdout path the CC-harness wrapper depends on.

Iterative gates use `scripts/test-select --context task|round` (the lib change relinks integ
binaries); the final pre-PR sweep is the full `--workspace --all-targets` CI-parity run.

## Deliverable scope (approved)

Ship the `api-watch` primitive (filter + resume + self-heal + `--cursor-file`) **plus a short
playbook** (`docs/playbooks/agent-channel-watch.md`) showing the `run_in_background` line-read
wake pattern — the CC-harness Monitor wiring is *documented and demonstrated*, not productized
(it lives in harness config, outside Harmony product code).

## Non-goals / deferred (follow-ups)

- Multi-community watch in one process (v1 is one `--community`, N channels).
- Watching *all* channels in a community without enumerating them (needs a list-channels RPC).
- Non-channel event types (DMs / mentions / presence) through the same filter surface.
- Server-side firehose replay / persisted `seq` (out of scope; client-side HLC resume is the
  design).
- The productized Claude Code `Monitor` integration for the fleet (documented pattern only).

## Decisions (approved 2026-07-20)

1. **Cursor ownership = hybrid.** Stateless core (`--since`) + optional self-persisting
   `--cursor-file`. Consumer picks batteries-included vs. explicit.
2. **Scope = watch primitive + usage playbook.** Tests + `run_in_background` playbook; no
   productized fleet wrapper this PR.
