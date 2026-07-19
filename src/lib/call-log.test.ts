// ZEB-357 — call-event DM message codec + presentation helpers.
import { describe, it, expect } from 'vitest';
import {
  CALL_EVENT_MIME,
  encodeCallEvent,
  parseCallEvent,
  describeCallEvent,
  isMissedCallEvent,
  isMissedCallHex,
  type CallEventPayload,
} from './call-log';

const answered: CallEventPayload = {
  v: 1,
  callId: 'ab'.repeat(16),
  outcome: 'answered',
  durationMs: 263_000, // 4m 23s
};

describe('encodeCallEvent / parseCallEvent', () => {
  it('round-trips every outcome', () => {
    const outcomes = ['answered', 'no_answer', 'declined', 'busy', 'canceled'] as const;
    for (const outcome of outcomes) {
      const payload: CallEventPayload = {
        v: 1,
        callId: 'cd'.repeat(16),
        outcome,
        ...(outcome === 'answered' ? { durationMs: 5_000 } : {}),
      };
      const parsed = parseCallEvent(CALL_EVENT_MIME, encodeCallEvent(payload));
      expect(parsed).toEqual(payload);
    }
  });

  it('returns null for a non-call-event mime type', () => {
    expect(parseCallEvent('text/plain', encodeCallEvent(answered))).toBeNull();
  });

  it('returns null for malformed JSON', () => {
    expect(parseCallEvent(CALL_EVENT_MIME, '{not json')).toBeNull();
  });

  it('returns null for an unknown version', () => {
    const body = JSON.stringify({ ...answered, v: 2 });
    expect(parseCallEvent(CALL_EVENT_MIME, body)).toBeNull();
  });

  it('returns null for an unknown outcome', () => {
    const body = JSON.stringify({ ...answered, outcome: 'exploded' });
    expect(parseCallEvent(CALL_EVENT_MIME, body)).toBeNull();
  });

  it('returns null when callId is missing or not a string', () => {
    const noId = JSON.stringify({ v: 1, outcome: 'answered' });
    expect(parseCallEvent(CALL_EVENT_MIME, noId)).toBeNull();
    const numId = JSON.stringify({ v: 1, callId: 7, outcome: 'answered' });
    expect(parseCallEvent(CALL_EVENT_MIME, numId)).toBeNull();
  });

  // PR #494 R1 (CodeRabbit): callId is documented as 16 bytes hex-encoded —
  // enforce the shape so junk payloads fall back to text instead of rendering
  // as trusted system lines / badge increments.
  it('returns null when callId is not 32 hex chars', () => {
    for (const bad of ['call-7', 'ab'.repeat(15), 'ab'.repeat(17), 'zz'.repeat(16), '']) {
      const body = JSON.stringify({ v: 1, callId: bad, outcome: 'answered' });
      expect(parseCallEvent(CALL_EVENT_MIME, body)).toBeNull();
    }
    // Uppercase hex is still hex — accepted.
    const upper = JSON.stringify({ v: 1, callId: 'AB'.repeat(16), outcome: 'answered' });
    expect(parseCallEvent(CALL_EVENT_MIME, upper)).not.toBeNull();
  });

  it('drops a non-numeric durationMs instead of rejecting', () => {
    const body = JSON.stringify({ ...answered, durationMs: 'long' });
    const parsed = parseCallEvent(CALL_EVENT_MIME, body);
    expect(parsed).not.toBeNull();
    expect(parsed!.durationMs).toBeUndefined();
  });
});

describe('describeCallEvent', () => {
  it('renders answered with duration for both directions', () => {
    expect(describeCallEvent(answered, 'author')).toBe('Voice call · 4m 23s');
    expect(describeCallEvent(answered, 'recipient')).toBe('Voice call · 4m 23s');
  });

  it('renders answered without a duration when durationMs is absent', () => {
    const p: CallEventPayload = { v: 1, callId: answered.callId, outcome: 'answered' };
    expect(describeCallEvent(p, 'author')).toBe('Voice call');
  });

  it('formats sub-minute and hour-scale durations', () => {
    const secs: CallEventPayload = { ...answered, durationMs: 42_000 };
    expect(describeCallEvent(secs, 'author')).toBe('Voice call · 42s');
    const hours: CallEventPayload = { ...answered, durationMs: 3_720_000 };
    expect(describeCallEvent(hours, 'author')).toBe('Voice call · 1h 2m');
  });

  it('renders the caller/callee label matrix for unanswered outcomes', () => {
    const p = (outcome: CallEventPayload['outcome']): CallEventPayload => ({
      v: 1,
      callId: answered.callId,
      outcome,
    });
    expect(describeCallEvent(p('no_answer'), 'author')).toBe('Call — no answer');
    expect(describeCallEvent(p('no_answer'), 'recipient')).toBe('Missed call');
    expect(describeCallEvent(p('canceled'), 'author')).toBe('Call canceled');
    expect(describeCallEvent(p('canceled'), 'recipient')).toBe('Missed call');
    expect(describeCallEvent(p('busy'), 'author')).toBe('Call — busy');
    expect(describeCallEvent(p('busy'), 'recipient')).toBe('Missed call (you were on a call)');
    expect(describeCallEvent(p('declined'), 'author')).toBe('Call declined');
    expect(describeCallEvent(p('declined'), 'recipient')).toBe('Call declined');
  });
});

describe('isMissedCallHex', () => {
  const hexOf = (s: string) =>
    Array.from(new TextEncoder().encode(s))
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');
  const body = (outcome: string) =>
    hexOf(JSON.stringify({ v: 1, callId: 'ab'.repeat(16), outcome }));

  it('classifies missed-class outcomes from a hex-encoded body', () => {
    expect(isMissedCallHex(CALL_EVENT_MIME, body('no_answer'))).toBe(true);
    expect(isMissedCallHex(CALL_EVENT_MIME, body('canceled'))).toBe(true);
    expect(isMissedCallHex(CALL_EVENT_MIME, body('busy'))).toBe(true);
    expect(isMissedCallHex(CALL_EVENT_MIME, body('answered'))).toBe(false);
    expect(isMissedCallHex(CALL_EVENT_MIME, body('declined'))).toBe(false);
  });

  it('is false for other mime types and undecodable bodies', () => {
    expect(isMissedCallHex('text/plain', body('no_answer'))).toBe(false);
    expect(isMissedCallHex(CALL_EVENT_MIME, 'zz-not-hex')).toBe(false);
    expect(isMissedCallHex(CALL_EVENT_MIME, hexOf('{broken'))).toBe(false);
  });
});

describe('isMissedCallEvent', () => {
  const p = (outcome: CallEventPayload['outcome']): CallEventPayload => ({
    v: 1,
    callId: answered.callId,
    outcome,
  });

  it('flags the no-explicit-choice outcomes for the recipient only', () => {
    for (const outcome of ['no_answer', 'canceled', 'busy'] as const) {
      expect(isMissedCallEvent(p(outcome), 'recipient')).toBe(true);
      expect(isMissedCallEvent(p(outcome), 'author')).toBe(false);
    }
  });

  it('never flags answered or declined', () => {
    for (const outcome of ['answered', 'declined'] as const) {
      expect(isMissedCallEvent(p(outcome), 'recipient')).toBe(false);
      expect(isMissedCallEvent(p(outcome), 'author')).toBe(false);
    }
  });
});
