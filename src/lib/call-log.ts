/**
 * ZEB-357 — call-event DM messages: codec + presentation helpers.
 *
 * A 1:1 DM call leaves a durable trace as a REAL synced DM message authored
 * by the CALLER at the call's terminal transition (single-writer: the caller
 * observes every outcome — accept, decline-with-reason, own cancel — so the
 * callee never writes, and there is no two-sides dedupe problem). The message
 * rides the normal `send_dm` path (deposit + tunnel, E2E-sealed), which is
 * what lets an OFFLINE callee learn of a missed call on next boot — call
 * signaling itself is a live-only Zenoh topic with no store-and-forward.
 *
 * The discriminator is the message's `mimeType` (already threaded end-to-end
 * by `send_dm` / `read_dm_thread` / `dm-received`), NOT a new field on the
 * durable types. The body is a tiny versioned JSON payload.
 *
 * Old clients render the JSON body as a plain text bubble — accepted alpha
 * degradation (we control both ends).
 */

export const CALL_EVENT_MIME = 'application/x-harmony-call-event+json';

/**
 * Terminal call outcomes, from the caller's perspective:
 * - `answered` — connected; `durationMs` measured from media-active to hangup
 *   (0 when the call ended before reaching active).
 * - `no_answer` — the callee's 30s ring timer auto-declined (reason `timeout`).
 * - `declined` — the callee explicitly declined (reason `user`).
 * - `busy` — the callee's device auto-declined while in another call; the
 *   callee never saw it ring.
 * - `canceled` — the caller gave up pre-answer (also the only terminal for an
 *   unreachable/offline callee: there is no caller-side ring timeout).
 */
export type CallOutcome = 'answered' | 'no_answer' | 'declined' | 'busy' | 'canceled';

const OUTCOMES: ReadonlySet<string> = new Set([
  'answered',
  'no_answer',
  'declined',
  'busy',
  'canceled',
]);

export interface CallEventPayload {
  v: 1;
  /** Hex callId (16 bytes → 32 chars) minted by `place_call`. */
  callId: string;
  outcome: CallOutcome;
  /** Only meaningful for `answered`. */
  durationMs?: number;
}

/** Whose screen is this rendered on relative to the message author (the
 *  caller)? `author` = the caller (and their sibling devices); `recipient`
 *  = the callee. Matches the existing `isSelf` computation in the feed. */
export type CallEventDirection = 'author' | 'recipient';

export function encodeCallEvent(payload: CallEventPayload): string {
  return JSON.stringify(payload);
}

/**
 * Parse a DM message into a call-event payload. Returns `null` for anything
 * that isn't a well-formed v1 call event (wrong mime, bad JSON, unknown
 * version/outcome, missing callId) — callers then fall back to rendering the
 * message as ordinary text. A non-numeric `durationMs` is dropped rather than
 * rejecting the whole event (the outcome line still renders without it).
 */
export function parseCallEvent(mimeType: string, bodyText: string): CallEventPayload | null {
  if (mimeType !== CALL_EVENT_MIME) return null;
  let raw: unknown;
  try {
    raw = JSON.parse(bodyText);
  } catch {
    return null;
  }
  if (typeof raw !== 'object' || raw === null) return null;
  const o = raw as Record<string, unknown>;
  if (o.v !== 1) return null;
  if (typeof o.callId !== 'string') return null;
  if (typeof o.outcome !== 'string' || !OUTCOMES.has(o.outcome)) return null;
  const payload: CallEventPayload = {
    v: 1,
    callId: o.callId,
    outcome: o.outcome as CallOutcome,
  };
  if (typeof o.durationMs === 'number' && Number.isFinite(o.durationMs) && o.durationMs >= 0) {
    payload.durationMs = o.durationMs;
  }
  return payload;
}

/** `263000` → `4m 23s`; `42000` → `42s`; `3720000` → `1h 2m`. */
function formatDuration(ms: number): string {
  const totalSecs = Math.floor(ms / 1000);
  if (totalSecs < 60) return `${totalSecs}s`;
  const totalMins = Math.floor(totalSecs / 60);
  if (totalMins < 60) return `${totalMins}m ${totalSecs % 60}s`;
  const hours = Math.floor(totalMins / 60);
  return `${hours}h ${totalMins % 60}m`;
}

/**
 * Human-readable system line for a call event. The label depends on which
 * side is looking: outcomes where the callee made no explicit choice read as
 * "Missed call" on the callee's screen.
 */
export function describeCallEvent(
  payload: CallEventPayload,
  direction: CallEventDirection,
): string {
  switch (payload.outcome) {
    case 'answered':
      return payload.durationMs !== undefined
        ? `Voice call · ${formatDuration(payload.durationMs)}`
        : 'Voice call';
    case 'no_answer':
      return direction === 'author' ? 'Call — no answer' : 'Missed call';
    case 'canceled':
      return direction === 'author' ? 'Call canceled' : 'Missed call';
    case 'busy':
      return direction === 'author' ? 'Call — busy' : 'Missed call (you were on a call)';
    case 'declined':
      return 'Call declined';
  }
}

/**
 * True when this event should count toward the recipient's missed-call badge:
 * outcomes where the callee made no explicit choice (`busy` auto-declines
 * before the ring toast ever shows). Never true on the author's side — the
 * `from !== self` arrival filter additionally excludes the caller's own
 * sibling devices upstream.
 */
export function isMissedCallEvent(
  payload: CallEventPayload,
  direction: CallEventDirection,
): boolean {
  if (direction !== 'recipient') return false;
  return (
    payload.outcome === 'no_answer' ||
    payload.outcome === 'canceled' ||
    payload.outcome === 'busy'
  );
}

/**
 * Missed-class check straight off the wire shapes that carry a HEX body
 * (the `dm-received` payload and the `read_dm_thread` page), for callers
 * that never build a `Message` — the unread/badge service. Recipient-side
 * by definition: the caller must have already filtered out self-authored
 * arrivals. False on any decode/parse failure.
 */
export function isMissedCallHex(mimeType: string, bodyHex: string): boolean {
  if (mimeType !== CALL_EVENT_MIME) return false;
  const pairs = bodyHex.match(/.{2}/g);
  if (!pairs || pairs.length * 2 !== bodyHex.length) return false;
  const bytes = new Uint8Array(pairs.length);
  for (let i = 0; i < pairs.length; i++) {
    const n = parseInt(pairs[i], 16);
    if (Number.isNaN(n)) return false;
    bytes[i] = n;
  }
  const payload = parseCallEvent(mimeType, new TextDecoder().decode(bytes));
  return payload !== null && isMissedCallEvent(payload, 'recipient');
}
