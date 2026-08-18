// src/lib/call-session.test.ts
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { get } from 'svelte/store';
import { CallSession } from './call-session';

function deps() {
  const invoke = vi.fn().mockResolvedValue(undefined);
  const listeners = new Map<string, ((e: { payload: unknown }) => void)[]>();
  const listen = vi.fn(async (ev: string, h: (e: { payload: unknown }) => void) => {
    (listeners.get(ev) ?? listeners.set(ev, []).get(ev)!).push(h);
    return () => {};
  });
  const emit = (ev: string, payload: unknown) =>
    (listeners.get(ev) ?? []).forEach((h) => h({ payload }));
  // Capture the frameGate the controller hands to its sender.
  let capturedGate: ((pcm: Float32Array) => { send: boolean; ptt: boolean }) | undefined;
  const sender = {
    start: vi.fn(async () => {}), stop: vi.fn(async () => {}),
    switchInputDevice: vi.fn(async () => {}),
    __setGate: (g: never) => { capturedGate = g; },
  };
  const receiver = { init: vi.fn(async () => {}), destroy: vi.fn(), getActiveSenders: () => [], isSpeaking: () => false };
  const mixer = { init: vi.fn(async () => {}), pushFrame: vi.fn(), drain: vi.fn(), setDeafened: vi.fn(), destroy: vi.fn(async () => {}) };
  return {
    invoke, listen, emit,
    getGate: () => capturedGate,
    factories: {
      makeSender: (gate: (pcm: Float32Array) => { send: boolean; ptt: boolean }) => { sender.__setGate(gate as never); return sender as never; },
      makeReceiver: () => receiver as never,
      makeMixer: () => mixer as never,
    },
    sender, receiver, mixer,
  };
}

/** First arg-object of the most recent invoke(cmd, args) call for `cmd`. */
function lastArgsFor(invoke: ReturnType<typeof vi.fn>, cmd: string): Record<string, unknown> | undefined {
  for (let i = invoke.mock.calls.length - 1; i >= 0; i--) {
    if (invoke.mock.calls[i][0] === cmd) return invoke.mock.calls[i][1] as Record<string, unknown>;
  }
  return undefined;
}

// ZEB-958 — the 1:1 in-call bar showed the caller's raw hex. The session now
// resolves a peerDisplayName through the nickname → card ladder on an incoming
// invite (activating the previously-dead resolveCard dep + a new resolveNickname
// rung), leaving it null when neither yields a non-blank name so the bar falls
// back to its own hex short-id.
describe('CallSession peer name ladder (ZEB-958)', () => {
  const CALLER = 'cc'.repeat(16);

  function sessionWith(
    resolveCard: (h: string) => { displayName?: string } | undefined,
    resolveNickname: (h: string) => string | undefined,
  ) {
    const d = deps();
    return new CallSession({
      invoke: d.invoke, listen: d.listen,
      selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      resolveCard, resolveNickname,
      ...d.factories,
    });
  }

  it('resolves peerDisplayName via the nickname over the card on an incoming invite', () => {
    const s = sessionWith(
      (h) => (h === CALLER ? { displayName: 'CallerCard' } : undefined),
      (h) => (h === CALLER ? 'Ziggy' : undefined),
    );
    s.onIncoming('call-1', CALLER, 'space-1');
    expect(get(s.state).peerDisplayName).toBe('Ziggy');
  });

  it('falls to the card name when there is no nickname', () => {
    const s = sessionWith((h) => (h === CALLER ? { displayName: 'CallerCard' } : undefined), () => undefined);
    s.onIncoming('call-1', CALLER, 'space-1');
    expect(get(s.state).peerDisplayName).toBe('CallerCard');
  });

  it('leaves peerDisplayName null for a whitespace-only published card name (bar shows hex)', () => {
    const s = sessionWith(() => ({ displayName: '   ' }), () => undefined);
    s.onIncoming('call-1', CALLER, 'space-1');
    expect(get(s.state).peerDisplayName).toBeNull();
  });
});

describe('CallSession DM signaling', () => {
  let d: ReturnType<typeof deps>;
  beforeEach(() => { d = deps(); });

  function newSession() {
    return new CallSession({
      invoke: d.invoke, listen: d.listen,
      selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      ...d.factories,
    });
  }

  it('placeCall → ringingOut, invokes place_call, returns the callId', async () => {
    d.invoke.mockImplementation(async (cmd: string) =>
      cmd === 'place_call' ? 'call-xyz' : undefined);
    const s = newSession();
    const callId = await s.placeCall('space-1');
    expect(callId).toBe('call-xyz');
    expect(get(s.state).phase).toBe('ringingOut');
    expect(get(s.state).callId).toBe('call-xyz');
    expect(get(s.state).peerOwnerHex).toBeNull();
    expect(d.invoke).toHaveBeenCalledWith('place_call', { spaceId: 'space-1' });
  });

  it('incoming invite → incoming; accept invokes accept_call + join_dm_call, connects muted', async () => {
    const s = newSession();
    s.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    expect(get(s.state).phase).toBe('incoming');
    expect(get(s.state).peerOwnerHex).toBe('cc'.repeat(16));

    await s.accept();
    expect(get(s.state).phase).toBe('active');
    expect(get(s.state).muted).toBe(true); // starts muted (D10)
    expect(d.invoke).toHaveBeenCalledWith('accept_call', { callId: 'call-1', spaceId: 'space-1' });
    expect(d.invoke).toHaveBeenCalledWith('join_dm_call', { callId: 'call-1', spaceId: 'space-1' });
    // Media engine built.
    expect(d.sender.start).toHaveBeenCalled();
    expect(d.receiver.init).toHaveBeenCalled();
    expect(d.mixer.init).toHaveBeenCalled();
    // Muted gate transmits nothing.
    expect(d.getGate()!(new Float32Array(320)).send).toBe(false);
  });

  it('decline on an incoming invite invokes decline_call(user) and resets to idle', async () => {
    const s = newSession();
    s.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    await s.decline('user');
    expect(get(s.state).phase).toBe('idle');
    expect(get(s.state).callId).toBeNull();
    expect(d.invoke).toHaveBeenCalledWith('decline_call',
      { callId: 'call-1', spaceId: 'space-1', reason: 'user' });
  });

  it('busy: onIncoming while active auto-declines with reason busy and leaves the active call untouched', async () => {
    const s = newSession();
    // Establish an active call first.
    s.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    await s.accept();
    expect(get(s.state).phase).toBe('active');
    d.invoke.mockClear();

    // A second invite arrives while busy.
    s.onIncoming('call-2', 'dd'.repeat(16), 'space-2');
    // Phase unchanged; original call still active.
    expect(get(s.state).phase).toBe('active');
    expect(get(s.state).callId).toBe('call-1');
    expect(d.invoke).toHaveBeenCalledWith('decline_call',
      { callId: 'call-2', spaceId: 'space-2', reason: 'busy' });
  });

  it('caller cancel before answer invokes cancel_call and resets', async () => {
    d.invoke.mockImplementation(async (cmd: string) =>
      cmd === 'place_call' ? 'call-9' : undefined);
    const s = newSession();
    await s.placeCall('space-1');
    expect(get(s.state).phase).toBe('ringingOut');
    await s.cancel();
    expect(get(s.state).phase).toBe('idle');
    expect(d.invoke).toHaveBeenCalledWith('cancel_call', { callId: 'call-9', spaceId: 'space-1' });
  });

  it('onRemoteAccepted (caller) → active + join_dm_call', async () => {
    d.invoke.mockImplementation(async (cmd: string) =>
      cmd === 'place_call' ? 'call-7' : undefined);
    const s = newSession();
    await s.placeCall('space-1');
    await s.onRemoteAccepted('call-7');
    expect(get(s.state).phase).toBe('active');
    expect(get(s.state).muted).toBe(true);
    expect(d.invoke).toHaveBeenCalledWith('join_dm_call', { callId: 'call-7', spaceId: 'space-1' });
    expect(d.sender.start).toHaveBeenCalled();
  });

  it('onRemoteAccepted ignores a mismatched callId', async () => {
    d.invoke.mockImplementation(async (cmd: string) =>
      cmd === 'place_call' ? 'call-7' : undefined);
    const s = newSession();
    await s.placeCall('space-1');
    await s.onRemoteAccepted('call-OTHER');
    expect(get(s.state).phase).toBe('ringingOut'); // unchanged
    expect(d.sender.start).not.toHaveBeenCalled();
  });

  it('onRemoteDeclined surfaces the reason and resets the caller', async () => {
    d.invoke.mockImplementation(async (cmd: string) =>
      cmd === 'place_call' ? 'call-7' : undefined);
    const s = newSession();
    await s.placeCall('space-1');
    s.onRemoteDeclined('call-7', 'busy');
    expect(get(s.state).phase).toBe('idle');
    expect(get(s.state).endReason).toBe('busy');
  });

  it('onRemoteEnded tears down (leave_dm_call) and resets to idle', async () => {
    const s = newSession();
    s.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    await s.accept();
    expect(get(s.state).phase).toBe('active');

    await s.onRemoteEnded('call-1');
    expect(get(s.state).phase).toBe('idle');
    expect(d.invoke).toHaveBeenCalledWith('leave_dm_call', { callId: 'call-1' });
    expect(d.sender.stop).toHaveBeenCalled();
    expect(d.receiver.destroy).toHaveBeenCalled();
    expect(d.mixer.destroy).toHaveBeenCalled();
  });

  it('end tears down media + invokes end_call and leave_dm_call', async () => {
    const s = newSession();
    s.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    await s.accept();
    await s.end();
    expect(get(s.state).phase).toBe('idle');
    expect(d.invoke).toHaveBeenCalledWith('end_call', { callId: 'call-1', spaceId: 'space-1' });
    expect(d.invoke).toHaveBeenCalledWith('leave_dm_call', { callId: 'call-1' });
  });

  it('destroy() tears down live media + timers and resets to idle', async () => {
    const s = newSession();
    s.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    await s.accept();
    expect(get(s.state).phase).toBe('active');
    s.destroy();
    expect(get(s.state).phase).toBe('idle');
    expect(d.sender.stop).toHaveBeenCalled();
    expect(d.receiver.destroy).toHaveBeenCalled();
    expect(d.mixer.destroy).toHaveBeenCalled();
  });

  it('wires the receiver to the dm-voice-frame-received event, callId-filtered', async () => {
    // Use the REAL receiver factory (no makeReceiver override) so we can assert
    // the default DM wiring (frameEvent + frameFilter) the controller sets.
    const realDeps = deps();
    const s = new CallSession({
      invoke: realDeps.invoke, listen: realDeps.listen,
      selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      // Only stub sender + mixer; let the receiver be the real VoiceReceiver.
      makeSender: realDeps.factories.makeSender,
      makeMixer: realDeps.factories.makeMixer,
    });
    s.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    await s.accept();
    // The real receiver subscribed to the DM event, not the channel one.
    expect(realDeps.listen).toHaveBeenCalledWith('dm-voice-frame-received', expect.any(Function));
    expect(realDeps.listen).not.toHaveBeenCalledWith('voice-frame-received', expect.any(Function));
  });

  it('the sender publishFrame routes through send_dm_voice_frame by callId', async () => {
    // Real sender so we can drive a captured frame through publishFrame.
    const realDeps = deps();
    const captures: { onFrame?: (pcm: Float32Array) => void } = {};
    const codec = {
      codecType: 'opus' as const, init: vi.fn(async () => {}),
      encode: () => new Uint8Array([1, 2, 3]), decode: () => new Float32Array(0), destroy: vi.fn(),
    };
    const capture = {
      start: vi.fn(async (cb: (pcm: Float32Array) => void) => { captures.onFrame = cb; }),
      stop: vi.fn(async () => {}), isActive: () => true,
    };
    // Patch the module's default sender by injecting a makeSender that builds a
    // real VoiceSender with our fake codec/capture + the DM publishFrame.
    const { VoiceSender } = await import('./voice/voice-sender');
    const s = new CallSession({
      invoke: realDeps.invoke, listen: realDeps.listen,
      selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      makeReceiver: realDeps.factories.makeReceiver,
      makeMixer: realDeps.factories.makeMixer,
      makeSender: (gate) => new VoiceSender({
        senderHash: new Uint8Array(16), communityId: '', channelId: '',
        invoke: realDeps.invoke, codec: codec as never, capture: capture as never,
        frameGate: gate,
        publishFrame: (frameBytes) =>
          realDeps.invoke('send_dm_voice_frame', {
            payload: { callId: 'call-1', frameBytes },
          }),
      }),
    });
    s.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    await s.accept();
    await s.setMuted(false); // open the gate
    captures.onFrame!(new Float32Array(320).fill(0.2)); // loud → sent
    const args = lastArgsFor(realDeps.invoke, 'send_dm_voice_frame');
    expect(args).toBeDefined();
    // The Tauri command takes a single `payload` struct (snake_case Rust side):
    // the frame fields must be wrapped, not passed top-level (ZEB-352 review).
    const payload = (args as { payload: { callId: string; frameBytes: number[] } }).payload;
    expect(payload.callId).toBe('call-1');
    expect(Array.isArray(payload.frameBytes)).toBe(true);
  });

  it('setMuted rolls back local state when set_dm_call_muted rejects', async () => {
    const s = newSession();
    s.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    await s.accept();
    d.invoke.mockRejectedValueOnce(new Error('backend refused'));
    await expect(s.setMuted(false)).rejects.toThrow(/refused/);
    // Local gate + store must NOT advertise unmuted when the backend stayed muted.
    expect(get(s.state).muted).toBe(true);
    expect(d.getGate()!(new Float32Array(320)).send).toBe(false);
  });

  it('setPttMode rolls back pttMode when the coupled setMuted fails (ZEB-351)', async () => {
    const s = newSession();
    s.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    await s.accept();
    d.invoke.mockRejectedValueOnce(new Error('mute refused'));
    await expect(s.setPttMode(true)).rejects.toThrow(/refused/);
    // Mode and mute roll back together — no "PTT on but muted" limbo.
    expect(get(s.state).pttMode).toBe(false);
    expect(get(s.state).muted).toBe(true);
  });

  it('setPttMode(false) failure restores pttHeld, not just pttMode (ZEB-352 review)', async () => {
    const s = newSession();
    s.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    await s.accept();
    await s.setPttMode(true); // enter PTT (unmutes)
    s.setPttHeld(true); // user is holding the key
    expect(get(s.state).pttHeld).toBe(true);
    // Leaving PTT couples a setMuted(true) round-trip; make it fail.
    d.invoke.mockRejectedValueOnce(new Error('mute refused'));
    await expect(s.setPttMode(false)).rejects.toThrow(/refused/);
    // Leaving PTT already forced pttHeld=false; a failed round-trip must restore
    // BOTH mode and hold, or the gate is stranded in a state mode doesn't match.
    expect(get(s.state).pttMode).toBe(true);
    expect(get(s.state).pttHeld).toBe(true);
  });
});

describe('CallSession ring timeout', () => {
  let d: ReturnType<typeof deps>;
  beforeEach(() => { vi.useFakeTimers(); d = deps(); });
  afterEach(() => { vi.useRealTimers(); });

  function newSession() {
    return new CallSession({
      invoke: d.invoke, listen: d.listen,
      selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      ...d.factories,
    });
  }

  it('auto-declines an unanswered incoming call after 30s with reason timeout', async () => {
    const s = newSession();
    s.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    expect(get(s.state).phase).toBe('incoming');
    await vi.advanceTimersByTimeAsync(30_000);
    expect(get(s.state).phase).toBe('idle');
    expect(d.invoke).toHaveBeenCalledWith('decline_call',
      { callId: 'call-1', spaceId: 'space-1', reason: 'timeout' });
  });

  it('accepting before the timeout cancels the auto-decline', async () => {
    const s = newSession();
    s.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    await s.accept();
    d.invoke.mockClear();
    await vi.advanceTimersByTimeAsync(30_000);
    // No decline fired after accept cleared the ring timer.
    expect(d.invoke).not.toHaveBeenCalledWith('decline_call', expect.anything());
    expect(get(s.state).phase).toBe('active');
  });
});

describe('CallSession transport reconnect (ZEB-353)', () => {
  let d: ReturnType<typeof deps>;
  beforeEach(() => { d = deps(); });

  function newSession() {
    return new CallSession({
      invoke: d.invoke, listen: d.listen,
      selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      ...d.factories,
    });
  }

  /** Bring a session up to an active call on 'call-1'. */
  async function activeCall() {
    const s = newSession();
    s.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    await s.accept();
    expect(get(s.state).phase).toBe('active');
    return s;
  }

  it('voice-transport-lost for the active call sets reconnecting', async () => {
    const s = await activeCall();
    expect(get(s.state).reconnecting).toBe(false);
    d.emit('voice-transport-lost', { callId: 'call-1' });
    expect(get(s.state).reconnecting).toBe(true);
  });

  it('voice-transport-restored clears reconnecting', async () => {
    const s = await activeCall();
    d.emit('voice-transport-lost', { callId: 'call-1' });
    expect(get(s.state).reconnecting).toBe(true);
    d.emit('voice-transport-restored', { callId: 'call-1' });
    expect(get(s.state).reconnecting).toBe(false);
  });

  it('ignores transport events for a different call', async () => {
    const s = await activeCall();
    d.emit('voice-transport-lost', { callId: 'call-2' });
    expect(get(s.state).reconnecting).toBe(false);
  });
});

// ZEB-357 — caller-authored call outcomes. The CALLER is the single writer of
// one call-event record per call, fired via the onCallOutcome dep at each
// terminal transition. The callee side must never fire it.
describe('CallSession call-outcome recording (ZEB-357)', () => {
  let d: ReturnType<typeof deps>;
  let outcomes: Array<{ spaceId: string; payload: unknown }>;
  beforeEach(() => {
    d = deps();
    outcomes = [];
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  function newSession() {
    return new CallSession({
      invoke: d.invoke, listen: d.listen,
      selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      onCallOutcome: (spaceId, payload) => { outcomes.push({ spaceId, payload }); },
      ...d.factories,
    });
  }

  /** Place a call as the caller on 'space-1'; backend mints 'call-7'. */
  async function ringingOut() {
    d.invoke.mockImplementation(async (cmd: string) =>
      cmd === 'place_call' ? 'call-7' : undefined);
    const s = newSession();
    await s.placeCall('space-1');
    return s;
  }

  it('caller cancel records canceled (captured before the state reset)', async () => {
    const s = await ringingOut();
    await s.cancel();
    expect(get(s.state).phase).toBe('idle');
    expect(outcomes).toEqual([
      { spaceId: 'space-1', payload: { v: 1, callId: 'call-7', outcome: 'canceled' } },
    ]);
  });

  it('remote decline maps timeout→no_answer, busy→busy, user→declined', async () => {
    for (const [reason, outcome] of [
      ['timeout', 'no_answer'],
      ['busy', 'busy'],
      ['user', 'declined'],
    ] as const) {
      outcomes = [];
      const s = await ringingOut();
      s.onRemoteDeclined('call-7', reason);
      expect(get(s.state).phase).toBe('idle');
      expect(outcomes).toEqual([
        { spaceId: 'space-1', payload: { v: 1, callId: 'call-7', outcome } },
      ]);
    }
  });

  it('answered call hung up by the caller records answered with the active-phase duration', async () => {
    vi.useFakeTimers();
    const s = await ringingOut();
    await s.onRemoteAccepted('call-7'); // → connecting → active (startedAt stamped)
    expect(get(s.state).phase).toBe('active');
    vi.advanceTimersByTime(263_000);
    await s.end();
    expect(outcomes).toEqual([
      {
        spaceId: 'space-1',
        payload: { v: 1, callId: 'call-7', outcome: 'answered', durationMs: 263_000 },
      },
    ]);
  });

  it('answered call hung up by the PEER records answered on the caller', async () => {
    vi.useFakeTimers();
    const s = await ringingOut();
    await s.onRemoteAccepted('call-7');
    vi.advanceTimersByTime(5_000);
    await s.onRemoteEnded('call-7');
    expect(get(s.state).phase).toBe('idle');
    expect(outcomes).toEqual([
      {
        spaceId: 'space-1',
        payload: { v: 1, callId: 'call-7', outcome: 'answered', durationMs: 5_000 },
      },
    ]);
  });

  it('records exactly once per call (remote end after own end does not double-fire)', async () => {
    const s = await ringingOut();
    await s.onRemoteAccepted('call-7');
    await s.end();
    await s.onRemoteEnded('call-7'); // stale echo for the already-ended call
    expect(outcomes).toHaveLength(1);
  });

  // PR #494 R1 (Qodo): end() records, then AWAITS teardownMedia before
  // resetToIdle clears callId — a call-ended event delivered in that window
  // passed both guards and recorded a second call-event DM.
  it('overlapping end() and onRemoteEnded() during a slow teardown record exactly once', async () => {
    const s = await ringingOut();
    await s.onRemoteAccepted('call-7');
    // Block teardownMedia mid-flight: sender.stop() hangs on a deferred, and
    // stopEntered proves end() is parked there BEFORE the overlapping event
    // runs (so the once-mock can't be consumed by the wrong teardown).
    let releaseStop!: () => void;
    const stopEntered = new Promise<void>((entered) => {
      d.sender.stop.mockImplementationOnce(() => {
        entered();
        return new Promise<void>((r) => { releaseStop = r; });
      });
    });
    const ending = s.end(); // enters teardown, does NOT resolve yet
    await stopEntered; // end() is now parked inside teardownMedia
    await s.onRemoteEnded('call-7'); // concurrent peer hangup in the window
    releaseStop();
    await ending;
    expect(outcomes).toHaveLength(1);
  });

  // PR #494 R1 (CodeRabbit): cancel() awaited the cancel_call IPC BEFORE
  // recording — a decline landing during that await recorded first, then
  // cancel recorded again (double record, and the caller's own cancellation
  // must stay the single terminal).
  it('a decline racing an in-flight cancel records only the canceled outcome', async () => {
    let releaseCancel!: () => void;
    d.invoke.mockImplementation((cmd: string) => {
      if (cmd === 'place_call') return Promise.resolve('call-7');
      if (cmd === 'cancel_call') {
        return new Promise<void>((r) => { releaseCancel = r; });
      }
      return Promise.resolve(undefined);
    });
    const s = newSession();
    await s.placeCall('space-1');
    const cancelling = s.cancel(); // IPC in flight
    s.onRemoteDeclined('call-7', 'timeout'); // decline arrives mid-cancel
    releaseCancel();
    await cancelling;
    expect(outcomes.map((o) => (o.payload as { outcome: string }).outcome)).toEqual(['canceled']);
  });

  it('callee flows never record: decline, ring-timeout, and answered-then-ended', async () => {
    // Explicit decline.
    const s1 = newSession();
    s1.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    await s1.decline('user');
    // Ring timeout (auto-decline).
    vi.useFakeTimers();
    const s2 = newSession();
    s2.onIncoming('call-2', 'cc'.repeat(16), 'space-1');
    await vi.advanceTimersByTimeAsync(30_000);
    expect(get(s2.state).phase).toBe('idle');
    vi.useRealTimers();
    // Full answered call on the callee side.
    const s3 = newSession();
    s3.onIncoming('call-3', 'cc'.repeat(16), 'space-1');
    await s3.accept();
    await s3.end();
    expect(outcomes).toEqual([]);
  });

  it('caller cancel of an unreachable callee still records canceled (offline-callee path)', async () => {
    // No decline/accept ever arrives — the only terminal is the caller's cancel.
    const s = await ringingOut();
    await s.cancel();
    expect(outcomes.map((o) => (o.payload as { outcome: string }).outcome)).toEqual(['canceled']);
  });
});

describe('CallSession audio device following (ZEB-359)', () => {
  function prefsStub() {
    let input: string | null = null;
    let output: string | null = null;
    const subs = new Set<() => void>();
    return {
      prefs: {
        getInput: () => input,
        getOutput: () => output,
        listDevices: async () => ({
          inputs: input ? [{ deviceId: input, label: 'Mic' }] : [],
          outputs: [],
        }),
        subscribe: (cb: () => void) => {
          subs.add(cb);
          return () => subs.delete(cb);
        },
      },
      setInput(id: string | null) { input = id; for (const cb of [...subs]) cb(); },
      setOutput(id: string | null) { output = id; for (const cb of [...subs]) cb(); },
      subCount: () => subs.size,
    };
  }
  const flush = () => new Promise((r) => setTimeout(r, 0));

  it('an input pref change during an active call restarts the sender capture', async () => {
    const d = deps();
    const p = prefsStub();
    const s = new CallSession({
      invoke: d.invoke, listen: d.listen,
      selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      audioDevices: p.prefs,
      ...d.factories,
    });
    s.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    await s.accept();
    expect(p.subCount()).toBe(1);
    p.setInput('mic-b');
    await flush();
    expect(d.sender.switchInputDevice).toHaveBeenCalledTimes(1);
    await s.end();
  });

  it('ending the call unfollows — later pref changes touch nothing', async () => {
    const d = deps();
    const p = prefsStub();
    const s = new CallSession({
      invoke: d.invoke, listen: d.listen,
      selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      audioDevices: p.prefs,
      ...d.factories,
    });
    s.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    await s.accept();
    await s.end();
    expect(p.subCount()).toBe(0);
    p.setInput('mic-b');
    await flush();
    expect(d.sender.switchInputDevice).not.toHaveBeenCalled();
  });

  it('an output pref change routes the live mixer', async () => {
    const d = deps();
    const p = prefsStub();
    const mixerWithSink = { ...d.mixer, setOutputDevice: vi.fn(async () => {}) };
    const s = new CallSession({
      invoke: d.invoke, listen: d.listen,
      selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      audioDevices: p.prefs,
      ...d.factories,
      makeMixer: () => mixerWithSink as never,
    });
    s.onIncoming('call-1', 'cc'.repeat(16), 'space-1');
    await s.accept();
    p.setOutput('spk-2');
    await flush();
    expect(mixerWithSink.setOutputDevice).toHaveBeenCalledWith('spk-2');
    await s.end();
  });
});
