# Agent channel-watch playbook (ZEB-480)

`harmony-app watch` turns a running headless node's channel feed into a
**filtered, resumable, self-healing** push stream — one JSON line per new
message on stdout. It is the push analog of `api --events`: instead of an
unfiltered live-from-connect dump, `watch` filters to the channel(s) you care
about, catches up on anything missed while it was down (via an HLC cursor), and
reconnects across node restarts. This is what graduates the fleet's Linear/
`api --events` coordination onto a real trigger — the Harmony analog of Claude
Code's GitHub PR `Monitor`.

## Prerequisites

A running node exposing the localhost control surface — either `harmony-app
serve` or a GUI launched with `HARMONY_API_PORT`. `watch` reads
`<data-dir>/api/{port,token}` (the same discovery files `api` uses), so it needs
no arguments beyond the channel selection.

## Invocation

```bash
harmony-app --profile fleet-koya watch \
    --community <communityHex> \
    --channel   <channelHex> \
    --cursor-file ~/.harmony-fleet-koya/watch/fleet.json
```

- `--community <hex>` (required) — the community the channels belong to.
- `--channel <hex>` (required, repeatable) — watch one or more channels in that
  community. Repeat `--channel` for each (e.g. `#fleet` + `#fleet-on-harmony`).
- `--since <wallMs:logical:deviceId>` — start from an explicit HLC cursor
  (seeds every channel); backfills everything after it, then goes live.
- `--cursor-file <path>` — self-persist per-channel cursors here and auto-resume
  from them on the next run. Combine with `--since` to seed channels the file
  doesn't yet know. This is the batteries-included resume: kill the watch, and
  the next launch catches up on exactly what arrived while it was gone.
- `--raw` — emit the untouched firehose frame instead of the normalized line.
- `--no-retry` — exit (code 3) on disconnect instead of reconnecting (scripts/tests).

## Output

One NDJSON object per message on stdout (diagnostics go to stderr):

```json
{"source":"live","seq":42,"communityId":"…","channelId":"…","messageId":"…",
 "author":"…","at":{"wallMs":1721000000000,"logical":3,"deviceId":"…"},
 "body":"(Ildwyn) roster converged","mentions":[]}
```

- `source` — `"backfill"` (replayed from history) or `"live"` (from the firehose).
- `seq` — the firehose sequence number for live frames; `null` for backfill.
- `body` — decoded UTF-8 text (no more JSON number arrays to decode downstream).
- `replyTo` / `kind` / `pollId` / `mentions` — present only when the message has them.

## Resume across restarts

The cursor is the per-message **HLC**, not the firehose `seq` (which is
process-lifetime and non-resumable). With `--cursor-file`, a restart replays via
`list_channel_messages` from the stored HLC, so nothing posted during downtime
is missed:

```bash
# First run — persists cursors as messages arrive.
harmony-app watch --community $C --channel $CH --cursor-file $F
# ^C, node restarts, whatever… then just re-run the same command:
harmony-app watch --community $C --channel $CH --cursor-file $F   # catches up, then live
```

Known limitation: a message that materializes during downtime with an HLC
*earlier* than the watcher's high-water mark is not re-fetched (acceptable for
coordination; full gap-freedom is a deferred follow-up).

## Waking an agent (the Claude Code `Monitor` pattern)

The Harmony deliverable is the clean, resumable push stream above. Turning a new
line into an **agent wake** is Claude-Code-harness glue that lives outside
Harmony product code — the analog of the GitHub PR `Monitor` that re-invokes the
agent loop when a comment lands. The shape:

1. Run `watch` as a long-lived background process (Claude Code: a `Bash`
   `run_in_background` invocation), with `--cursor-file` so restarts don't drop
   messages.
2. A thin wrapper reads its stdout line-by-line; each line is a coordination
   message from a peer (`author`, `body`, `channelId`). The wrapper re-invokes
   the agent with that payload — the push wake.
3. Because the watch persists its cursor per emit, the wrapper needs no state of
   its own: kill and relaunch is always safe.

Minimal line-reader sketch (the trigger glue is harness-specific):

```bash
harmony-app --profile fleet-$ME watch --community $C --channel $CH \
    --cursor-file $F | while IFS= read -r line; do
        # hand $line to the agent-wake mechanism (harness-specific)
        printf '%s\n' "$line"
    done
```

Self-attribution stays the `(Koya)/(Ildwyn)/(AVALON)/(Jake)` body-prefix
convention for now; carrying it as structured metadata is a deferred stretch.
