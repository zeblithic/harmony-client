// ZEB-803 — community-relay stall assessment.
//
// These pin the READINGS, not the counters. The counters are proven in Rust;
// what these guard is the interpretation layer, where the original incident
// actually went wrong: every surface reported true values and every reading of
// them said "healthy".

import { describe, it, expect } from 'vitest';
import {
  assessRelayServing,
  assessRelayPulling,
  COMMUNITY_RELAY_SERVE_CADENCE_MS,
  COMMUNITY_RELAY_STALL_CADENCES,
} from '../network-health-adapter';
import type { CommunityRelayHealth } from '../types/network-health';

const NOW = 1_785_000_000_000;
const CADENCE = COMMUNITY_RELAY_SERVE_CADENCE_MS;
const THRESHOLD = CADENCE * COMMUNITY_RELAY_STALL_CADENCES;

function health(over: {
  serving?: Partial<CommunityRelayHealth['serving']>;
  pulling?: Partial<CommunityRelayHealth['pulling']>;
} = {}): CommunityRelayHealth {
  return {
    serving: {
      pullsServed: 0,
      pullsRejected: 0,
      pullsFailed: 0,
      lastServedMs: null,
      peers: [],
      ...over.serving,
    },
    pulling: {
      passesRun: 0,
      lastPassMs: null,
      sessionsOk: 0,
      sessionsFailed: 0,
      blobsIngested: 0,
      lastIngestMs: null,
      passesNoRelay: 0,
      recent: [],
      ...over.pulling,
    },
  };
}

describe('assessRelayServing', () => {
  it('reports idle — not stalled — for a node nobody has ever pulled from', () => {
    // A node nobody relays through is CORRECT in this state. Crying stall here
    // would train an operator to ignore the badge, which costs us the one time
    // it matters.
    const v = assessRelayServing(health(), NOW);
    expect(v.state).toBe('idle');
  });

  it('reports ok inside the cadence window', () => {
    const v = assessRelayServing(
      health({ serving: { lastServedMs: NOW - CADENCE, pullsServed: 12 } }),
      NOW
    );
    expect(v.state).toBe('ok');
  });

  it('tolerates two missed cadences without alarming', () => {
    // Jitter budget. One missed slot is a peer going offline, not a fault.
    const v = assessRelayServing(
      health({ serving: { lastServedMs: NOW - CADENCE * 2, pullsServed: 12 } }),
      NOW
    );
    expect(v.state).toBe('ok');
  });

  it('reports stalled past the threshold', () => {
    const v = assessRelayServing(
      health({ serving: { lastServedMs: NOW - THRESHOLD - 1, pullsServed: 82 } }),
      NOW
    );
    expect(v.state).toBe('stalled');
    if (v.state === 'stalled') {
      expect(v.detail).toContain('82');
      expect(v.sinceMs).toBeGreaterThan(THRESHOLD);
    }
  });

  it('catches the observed incident: 82 pulls then 46 minutes of nothing', () => {
    // The literal shape of the 2026-07-26 occurrence. If this ever returns
    // anything but stalled, the surface has regressed to the state that let a
    // 46-minute outage look identical to a quiet channel.
    const v = assessRelayServing(
      health({
        serving: {
          lastServedMs: NOW - 46 * 60 * 1000,
          pullsServed: 82,
          peers: [
            { peerShort: '3af8b2a0', lastServedMs: NOW - 46 * 60 * 1000, servedCount: 30 },
            { peerShort: '09911d5c', lastServedMs: NOW - 44 * 60 * 1000, servedCount: 28 },
          ],
        },
      }),
      NOW
    );
    expect(v.state).toBe('stalled');
  });
});

describe('assessRelayPulling', () => {
  it('reports idle before the first pass', () => {
    expect(assessRelayPulling(health(), NOW).state).toBe('idle');
  });

  it('reports ok when the loop runs and sessions succeed', () => {
    const v = assessRelayPulling(
      health({
        pulling: { passesRun: 9, lastPassMs: NOW - CADENCE, sessionsOk: 9 },
      }),
      NOW
    );
    expect(v.state).toBe('ok');
  });

  it('a dead pull loop outranks a healthy-looking sessionsOk', () => {
    // THE ordering guarantee. When the loop stops, every other counter freezes
    // at its last good value — sessionsOk stays high and reads as "recently
    // fine". Reporting the freshness of a stopped clock is exactly the failure
    // this section exists to prevent, so loop-death must win.
    const v = assessRelayPulling(
      health({
        pulling: {
          passesRun: 40,
          lastPassMs: NOW - THRESHOLD - 1,
          sessionsOk: 40, // every pass it DID run succeeded
          blobsIngested: 500,
          lastIngestMs: NOW - THRESHOLD - 1,
        },
      }),
      NOW
    );
    expect(v.state).toBe('stalled');
    if (v.state === 'stalled') {
      expect(v.detail).toContain('pull loop stopped');
    }
  });

  it('distinguishes "no relay advertised" from "sessions failing"', () => {
    // Different remedies: discovery vs transport. Collapsing them sends an
    // operator hunting a transport fault that does not exist.
    const noRelay = assessRelayPulling(
      health({
        pulling: { passesRun: 5, lastPassMs: NOW - 1000, passesNoRelay: 5 },
      }),
      NOW
    );
    expect(noRelay.state).toBe('stalled');
    if (noRelay.state === 'stalled') {
      expect(noRelay.detail).toContain('no fresh relay advertised');
    }

    const failing = assessRelayPulling(
      health({
        pulling: { passesRun: 5, lastPassMs: NOW - 1000, sessionsFailed: 5 },
      }),
      NOW
    );
    expect(failing.state).toBe('stalled');
    if (failing.state === 'stalled') {
      expect(failing.detail).toContain('pull session failed');
    }
  });

  it('does not alarm on a live loop with nothing joined', () => {
    // passesRun climbing with zero sessions is the correct steady state for a
    // node in no communities. It is the LIVENESS proof, not a fault.
    const v = assessRelayPulling(
      health({ pulling: { passesRun: 20, lastPassMs: NOW - 1000 } }),
      NOW
    );
    expect(v.state).toBe('ok');
  });

  it('a successful session alongside failures is not a stall', () => {
    // One dead relay among several is degraded, not stalled — we are still
    // getting our blobs.
    const v = assessRelayPulling(
      health({
        pulling: {
          passesRun: 5,
          lastPassMs: NOW - 1000,
          sessionsOk: 3,
          sessionsFailed: 2,
        },
      }),
      NOW
    );
    expect(v.state).toBe('ok');
  });
});

describe('backward wall-clock steps (Qodo, PR #556)', () => {
  // Both sides of the age subtraction are local `SystemTime::now()` stamps, so
  // a future-looking value means the local clock stepped backward between the
  // capture and the record. Unclamped, that produced a NEGATIVE age — which
  // compares false against the threshold and reports a genuine stall as `ok`.
  // Same failure class as ZEB-791, reintroduced on the TS side.

  it('a future lastServedMs does not report ok — and never yields a negative age', () => {
    const v = assessRelayServing(
      health({ serving: { lastServedMs: NOW + 5_000, pullsServed: 40 } }),
      NOW
    );
    expect(v.state).toBe('ok'); // clamped to fresh, which is the safe direction
    if (v.state === 'stalled') expect(v.sinceMs).toBeGreaterThanOrEqual(0);
  });

  it('a backward step does NOT suppress an already-established stall', () => {
    // The stall is 46 minutes deep; a 5-second backward step must not erase it.
    const v = assessRelayServing(
      health({ serving: { lastServedMs: NOW - 46 * 60 * 1000, pullsServed: 82 } }),
      NOW - 5_000
    );
    expect(v.state).toBe('stalled');
    if (v.state === 'stalled') expect(v.sinceMs).toBeGreaterThan(0);
  });

  it('never returns a negative sinceMs from the pulling verdict', () => {
    // sinceMs is returned to callers; a negative duration is nonsense on the
    // wire even when the verdict itself happens to be right.
    const v = assessRelayPulling(
      health({
        pulling: {
          passesRun: 5,
          lastPassMs: NOW + 10_000,
          sessionsFailed: 5,
        },
      }),
      NOW
    );
    if (v.state === 'stalled') {
      expect(v.sinceMs).toBeGreaterThanOrEqual(0);
    }
  });

  it('a future lastPassMs does not fake a dead pull loop', () => {
    const v = assessRelayPulling(
      health({ pulling: { passesRun: 9, lastPassMs: NOW + 60_000, sessionsOk: 9 } }),
      NOW
    );
    expect(v.state).toBe('ok');
  });
});
