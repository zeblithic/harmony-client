// src/lib/voice-session.test.ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { get } from 'svelte/store';
import { VoiceSession } from './voice-session';

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

describe('VoiceSession lifecycle + gate', () => {
  let d: ReturnType<typeof deps>;
  beforeEach(() => { d = deps(); });

  function newSession() {
    return new VoiceSession({
      invoke: d.invoke, listen: d.listen,
      selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      ...d.factories,
    });
  }

  it('joins muted and is connected', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    expect(get(s.state).phase).toBe('connected');
    expect(get(s.state).muted).toBe(true);
    expect(d.invoke).toHaveBeenCalledWith('join_voice_channel', { communityId: 'comm', channelId: 'chan' });
  });

  it('rejects a second join while active', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    await expect(s.join('comm', 'chan2')).rejects.toThrow(/already/i);
  });

  it('muted gate never sends', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    const gate = d.getGate()!;
    expect(gate(new Float32Array(320)).send).toBe(false);
  });

  it('open-mic gate follows VAD energy', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    await s.setMuted(false);
    const gate = d.getGate()!;
    const loud = new Float32Array(320).fill(0.2) as unknown as Float32Array;
    const quiet = new Float32Array(320);
    expect(gate(loud).send).toBe(true);
    // hangover then silence
    for (let i = 0; i < 11; i++) gate(quiet);
    expect(gate(quiet).send).toBe(false);
  });

  it('PTT mode ignores VAD and follows hold', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    await s.setMuted(false);
    s.setPttMode(true);
    const gate = d.getGate()!;
    const quiet = new Float32Array(320);
    expect(gate(quiet).send).toBe(false);  // not held
    s.setPttHeld(true);
    expect(gate(quiet).send).toBe(true);   // held, VAD ignored
  });

  it('setMuted invokes set_voice_muted', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    await s.setMuted(false);
    expect(d.invoke).toHaveBeenCalledWith('set_voice_muted',
      { communityId: 'comm', channelId: 'chan', muted: false });
  });

  it('setMuted rolls back local state when the backend rejects', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    d.invoke.mockRejectedValueOnce(new Error('backend refused'));
    await expect(s.setMuted(false)).rejects.toThrow(/refused/);
    // Local gate + store must NOT advertise unmuted when the backend stayed muted.
    expect(get(s.state).muted).toBe(true);
    expect(d.getGate()!(new Float32Array(320)).send).toBe(false);
  });

  it('PTT mode enters unmuted (hold-gated) and tracks hold state', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    await s.setPttMode(true);
    expect(get(s.state).pttMode).toBe(true);
    expect(get(s.state).muted).toBe(false); // entering PTT unmutes; gate is hold-driven
    s.setPttHeld(true);
    expect(get(s.state).pttHeld).toBe(true);
    s.setPttHeld(false);
    expect(get(s.state).pttHeld).toBe(false);
    await s.setPttMode(false);
    expect(get(s.state).muted).toBe(true); // leaving PTT re-mutes to the safe default
    expect(get(s.state).pttHeld).toBe(false);
  });

  it('setPttMode rolls back pttMode when the coupled setMuted fails', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    d.invoke.mockRejectedValueOnce(new Error('mute refused'));
    await expect(s.setPttMode(true)).rejects.toThrow(/refused/);
    // Mode and mute roll back together — no "PTT on but muted" limbo.
    expect(get(s.state).pttMode).toBe(false);
    expect(get(s.state).muted).toBe(true);
  });

  it('setPttMode(false) failure restores pttHeld, not just pttMode', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
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

  it('leave returns to idle and tears down', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    await s.leave();
    expect(get(s.state).phase).toBe('idle');
    expect(d.invoke).toHaveBeenCalledWith('leave_voice_channel', { communityId: 'comm', channelId: 'chan' });
    expect(d.mixer.destroy).toHaveBeenCalled();
    expect(d.receiver.destroy).toHaveBeenCalled();
  });

  it('updates roster from voice-presence-changed for the active channel', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    d.emit('voice-presence-changed', {
      community: 'comm', channel: 'chan',
      roster: [
        { owner: 'cc'.repeat(16), device: 'dd'.repeat(16), muted: false },
        { owner: 'ee'.repeat(16), device: 'ff'.repeat(16), muted: true },
      ],
    });
    const roster = get(s.state).roster;
    expect(roster.map((m) => m.ownerHex)).toEqual(['cc'.repeat(16), 'ee'.repeat(16)]);
    expect(roster[1].muted).toBe(true);
  });

  it('ignores presence for a different channel', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    d.emit('voice-presence-changed', { community: 'comm', channel: 'other', roster: [
      { owner: 'cc'.repeat(16), device: 'dd'.repeat(16), muted: false },
    ] });
    expect(get(s.state).roster).toHaveLength(0);
  });
});

describe('VoiceSession 64-participant soft cap (ZEB-353 reactive bounce)', () => {
  let d: ReturnType<typeof deps>;
  beforeEach(() => { d = deps(); });

  function newSession() {
    return new VoiceSession({
      invoke: d.invoke, listen: d.listen,
      selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      ...d.factories,
    });
  }

  // A roster of `n` distinct NON-self members (self device is 'bb'.repeat(16)).
  function others(n: number) {
    return Array.from({ length: n }, (_, i) => ({
      // 64 hex chars per device, unique per member, none == self prefix.
      owner: (i + 0x10).toString(16).padStart(2, '0').repeat(16),
      device: ((i + 0x100).toString(16).padStart(4, '0') + 'cc'.repeat(14)),
      muted: false,
    }));
  }

  it('bounces an over-cap join: 64 non-self members → idle + channelFull', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    expect(get(s.state).phase).toBe('connected');
    expect(get(s.state).channelFull).toBe(false);

    // 64 others already present → joining would be the 65th → refuse.
    d.emit('voice-presence-changed', { community: 'comm', channel: 'chan', roster: others(64) });
    // The bounce fires leave() (several async teardown awaits) then stamps the
    // banner in a trailing .then(); drain the microtask queue until the final
    // channelFull:true settles (leave() resets to idle first, then the patch).
    for (let i = 0; i < 30 && !get(s.state).channelFull; i++) await Promise.resolve();

    expect(get(s.state).phase).toBe('idle');
    expect(get(s.state).channelFull).toBe(true);
    expect(d.invoke).toHaveBeenCalledWith('leave_voice_channel', { communityId: 'comm', channelId: 'chan' });
  });

  it('allows an at-cap join: 63 non-self members stays connected (you are the 64th)', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    d.emit('voice-presence-changed', { community: 'comm', channel: 'chan', roster: others(63) });
    await Promise.resolve();
    await Promise.resolve();

    expect(get(s.state).phase).toBe('connected');
    expect(get(s.state).channelFull).toBe(false);
    expect(d.invoke).not.toHaveBeenCalledWith('leave_voice_channel', { communityId: 'comm', channelId: 'chan' });
  });
});

describe('VoiceSession leave/join race (C2 regression)', () => {
  it('leave() during an in-flight join serializes and ends fully torn down', async () => {
    // Hang join_voice_channel until released, opening a mid-join window.
    let releaseJoin!: () => void;
    const joinGate = new Promise<void>((res) => { releaseJoin = res; });
    const invoke = vi.fn(async (cmd: string) => {
      if (cmd === 'join_voice_channel') await joinGate;
      return undefined;
    });
    const listen = vi.fn(async () => () => {});
    const receiver = { init: vi.fn(async () => {}), destroy: vi.fn(), getActiveSenders: () => [], isSpeaking: () => false };
    const mixer = { init: vi.fn(async () => {}), pushFrame: vi.fn(), drain: vi.fn(), setDeafened: vi.fn(), destroy: vi.fn(async () => {}) };
    const sender = { start: vi.fn(async () => {}), stop: vi.fn(async () => {}) };
    const s = new VoiceSession({
      invoke, listen, selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      makeSender: () => sender as never, makeReceiver: () => receiver as never, makeMixer: () => mixer as never,
    });

    const joinP = s.join('comm', 'chan'); // hangs at the join_voice_channel await
    await Promise.resolve();
    expect(get(s.state).phase).toBe('joining');

    const leaveP = s.leave();             // must serialize behind the in-flight join
    releaseJoin();                        // let join run to completion
    await Promise.all([joinP, leaveP]);

    expect(get(s.state).phase).toBe('idle');         // not a phantom 'connected'
    expect(sender.stop).toHaveBeenCalled();          // sender torn down
    expect(receiver.destroy).toHaveBeenCalled();
    expect(mixer.destroy).toHaveBeenCalled();
    expect(invoke).toHaveBeenCalledWith('leave_voice_channel', { communityId: 'comm', channelId: 'chan' });
  });
});

describe('VoiceSession join failure (transactional teardown)', () => {
  it('a failing sender.start() resets to idle, tears down, and leaves the backend', async () => {
    // Models a denied-microphone permission: backend join succeeds, then
    // sender.start() rejects. The session must not wedge in 'joining' or leak
    // the partially-built mixer/receiver/timer — and must release the backend.
    const invoke = vi.fn(async () => undefined);
    const listen = vi.fn(async () => () => {});
    const receiver = { init: vi.fn(async () => {}), destroy: vi.fn(), getActiveSenders: () => [], isSpeaking: () => false };
    const mixer = { init: vi.fn(async () => {}), pushFrame: vi.fn(), drain: vi.fn(), setDeafened: vi.fn(), destroy: vi.fn(async () => {}) };
    const sender = {
      start: vi.fn(async () => { throw new Error('NotAllowedError: mic denied'); }),
      stop: vi.fn(async () => {}),
    };
    const s = new VoiceSession({
      invoke, listen, selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      makeSender: () => sender as never, makeReceiver: () => receiver as never, makeMixer: () => mixer as never,
    });

    await expect(s.join('comm', 'chan')).rejects.toThrow(/mic denied/);
    expect(get(s.state).phase).toBe('idle');          // not wedged in 'joining'
    expect(mixer.destroy).toHaveBeenCalled();          // partial audio torn down
    expect(receiver.destroy).toHaveBeenCalled();
    expect(sender.stop).toHaveBeenCalled();
    // Backend join succeeded, so it must be released on the failure path.
    expect(invoke).toHaveBeenCalledWith('leave_voice_channel', { communityId: 'comm', channelId: 'chan' });
    // Retryable: a second join is rejected by the failing sender, NOT by an
    // "already active" guard left over from the wedged first attempt.
    await expect(s.join('comm', 'chan')).rejects.toThrow(/mic denied/);
  });
});
