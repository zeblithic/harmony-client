# ZEB-357: missed-call + call-history surface

Branch `zeb-357-call-history` off `main@8dbed576`. One PR closing ZEB-357.
Design decision (Jake, 2026-07-19): call outcomes are **synced E2E DM messages**,
not local-only state.

## Verified current state

- **V1** Signaling is live-only: `harmony/voice-signal/{owner}` Zenoh put, fire-and-forget
  (event_loop.rs ~6169); `VoiceSignal` appears 0× in butler_deposit / dm_envelope /
  dm_outbox / iroh_tunnel_dm_transport. An OFFLINE callee never learns of an invite.
  ⇒ only a caller-authored durable record can surface "missed while offline".
- **V2** The caller observes every outcome: `onRemoteAccepted` / `onRemoteDeclined(reason:
  user|busy|timeout)` / own `cancel()` / `end()` / `onRemoteEnded` (call-session.ts
  251-276). There is NO caller-side ring timeout — an unreachable callee terminates only
  via caller `cancel()`. The callee's 30s timer auto-declines with `timeout`.
- **V3** `send_dm` IPC (lib.rs:13286) already accepts arbitrary `mime_type`; the DTO
  (`DmThreadMessage`, camelCase) and the `dm-received` event both carry `mimeType`
  end-to-end. The frontend drops it at both ingestion points (message-service.ts
  168-207 live, 450-484 scrollback). **Zero Rust changes needed for transport.**
- **V4** DM feed renders via `TextFeed.svelte` (`feedItems` from `groupMessages`,
  kinds `message`/`quiet-group`); no system-line kind exists on the DM path
  (community `fork-divider` row is the repo precedent).
- **V5** Badges: `DmUnreadService` (dm-unread-service.ts) with subset interfaces
  `DmArrival`/`DmThreadPageEntry`, persisted cursor (localStorage `harmony-unread`,
  ns `dm`), `setUnread` → NavService → `NavNode.unreadCount` → NavNodeRow. The
  `mentionCount` field is the second-badge precedent (session-ephemeral); a missed-call
  set filtered the same way as the unread set is restart-durable via the same cursor.
- **V6** No OS-notification path renders raw DM bodies (checked); the call-declined
  toast (App.svelte 2635-2650) stays as the transient surface.

## Design

**Single-writer rule:** the CALLER authors exactly ONE call-event DM message per call at
its terminal transition, via the existing `send_dm` with
`mimeType = 'application/x-harmony-call-event+json'` and body JSON
`{"v":1,"callId":"<hex32>","outcome":"answered|no_answer|declined|busy|canceled","durationMs"?:n}`.
Rides deposit+tunnel like any DM ⇒ offline callee sees it on next boot; E2E-sealed;
synced to both parties' devices. The callee writes nothing (no dedupe problem).

Outcome mapping (caller-side terminals):
- `end()` / `onRemoteEnded` after accept → `answered` + durationMs (0 if never reached
  `active`).
- `onRemoteDeclined`: `timeout`→`no_answer`, `busy`→`busy`, else→`declined`.
- `cancel()` from `ringingOut` → `canceled` (covers the offline/unreachable callee).
- Caller crash mid-call → no entry (accepted residue; no timer can fix a dead process).

Render matrix (direction = is the viewer the author?):
| outcome | author (caller) view | recipient (callee) view | missed-class |
|---|---|---|---|
| answered | Voice call · 4m 23s | Voice call · 4m 23s | no |
| no_answer | Call — no answer | Missed call | YES |
| canceled | Call canceled | Missed call | YES |
| busy | Call — busy | Missed call (you were on a call) | YES |
| declined | Call declined | Call declined | no |

Missed-class = outcomes where the callee made no explicit choice (busy auto-declines
invisibly, onIncoming 189-192). These drive the 📞 badge on the callee only (the
`from !== self` filter already excludes the author and their sibling devices).

Old-client degradation: pre-feature clients render the JSON body as a text bubble
(alpha-acceptable, we control both ends). Group calls (GroupCallSession) out of scope.

## Tasks (red-first each)

- **T1** `src/lib/call-log.ts` — `CALL_EVENT_MIME`, `CallEventPayload`, `encodeCallEvent`,
  `parseCallEvent(mimeType, bodyText)` (tolerant: null on wrong mime / bad JSON / bad
  version / unknown outcome), `describeCallEvent(payload, direction)` label matrix,
  `isMissedCallEvent(payload, direction)`.
- **T2** `types.ts` `Message.callEvent?: CallEventPayload`; message-service parses at BOTH
  ingestion points → sets `callEvent` + `text = describeCallEvent(...)` (search/preview
  fallback).
- **T3** `call-session.ts` — track `isCaller`; new dep `onCallOutcome?(spaceId, payload)`;
  fire at the four caller terminals (capture fields BEFORE `resetToIdle`). Callee path
  never fires.
- **T4** App.svelte — `recordCallOutcome`: optimistic push + `send_dm` +
  `replaceOptimisticId` (mirrors `handleSend` 3360-3404); wire dep in buildVoiceSession.
- **T5** `CallEventLine.svelte` + TextFeed branch: `item.message.callEvent` renders the
  system line (separator-styled row, 📞 glyph, label + duration + time) instead of
  `TextMessage`.
- **T6** Badge: `DmArrival`/`DmThreadPageEntry` gain `mimeType`+`body`; parallel
  missed-CID set (seed + live + markThreadRead-clear, same cursor discipline);
  `setUnread` dep gains `missedCalls`; NavService optional param → `NavNode.missedCallCount`;
  NavNodeRow 📞 badge; App adapter threads the new fields.
- **T7** Rust: verify/add ONE pin — arbitrary mime round-trips `send_dm` →
  `read_dm_thread` unchanged (skip if already covered).
- **T8** Gates: `npx vitest run`, `npx tsc --noEmit`; Rust gates only if T7 adds code
  (`cargo fmt`, clippy `--all-targets`, `scripts/test-select --context task`); full
  sweep pre-PR. PR body: "Closes ZEB-357."
