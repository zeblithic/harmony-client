// src/lib/voice-session.test.ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { get } from 'svelte/store';
import { VoiceSession, classifyMicError, getVoiceSession, speakerQueue } from './voice-session';
import type { RosterMember } from './voice-session';

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
    // The Rust command takes a single `payload: SetVoiceMutedPayload` struct, so
    // Tauri v2 requires the args wrapped under `payload` (mirrors the DM-voice
    // path in call-session.ts). Sending them flat rejects with "missing required
    // key payload" — the ZEB-577 stuck-unmute bug.
    expect(d.invoke).toHaveBeenCalledWith('set_voice_muted',
      { payload: { communityId: 'comm', channelId: 'chan', muted: false } });
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

describe('VoiceSession raise-hand + invite-to-speak (ZEB-612)', () => {
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

  it('surfaces handRaisedAt from the presence payload (null when absent)', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    d.emit('voice-presence-changed', {
      community: 'comm', channel: 'chan',
      roster: [
        { owner: 'cc'.repeat(16), device: 'dd'.repeat(16), muted: false, handRaisedAt: 1720 },
        { owner: 'ee'.repeat(16), device: 'ff'.repeat(16), muted: true, handRaisedAt: null },
        // Pre-ZEB-612 payload shape (key absent entirely) → null.
        { owner: '11'.repeat(16), device: '22'.repeat(16), muted: true },
      ],
    });
    const roster = get(s.state).roster;
    expect(roster.map((m) => m.handRaisedAt)).toEqual([1720, null, null]);
  });

  it('marks invited members and selfInvited from the moderation overlay', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    d.emit('voice-presence-changed', {
      community: 'comm', channel: 'chan',
      roster: [
        { owner: 'cc'.repeat(16), device: 'dd'.repeat(16), muted: false, handRaisedAt: 5 },
        { owner: 'ee'.repeat(16), device: 'ff'.repeat(16), muted: true, handRaisedAt: null },
      ],
    });
    d.emit('voice-moderation-changed', {
      community: 'comm', channel: 'chan',
      mutedOwners: [], kickedOwners: [],
      invitedOwners: ['cc'.repeat(16), 'aa'.repeat(16)],
      powers: {}, selfPower: 0, selfModMuted: false, selfKicked: false,
      selfInvited: true,
    });
    const st = get(s.state);
    expect(st.selfInvited).toBe(true);
    expect(st.roster.find((m) => m.ownerHex === 'cc'.repeat(16))?.invited).toBe(true);
    expect(st.roster.find((m) => m.ownerHex === 'ee'.repeat(16))?.invited).toBe(false);
  });

  it('tolerates a pre-ZEB-612 overlay payload (invite keys absent)', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    d.emit('voice-moderation-changed', {
      community: 'comm', channel: 'chan',
      mutedOwners: [], kickedOwners: [],
      powers: {}, selfPower: 0, selfModMuted: false, selfKicked: false,
    });
    expect(get(s.state).selfInvited).toBe(false);
  });

  it('setHand invokes set_voice_hand with the camelCase payload', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    await s.setHand(true);
    expect(d.invoke).toHaveBeenCalledWith('set_voice_hand', {
      payload: { communityId: 'comm', channelId: 'chan', raised: true },
    });
    await s.setHand(false);
    expect(d.invoke).toHaveBeenCalledWith('set_voice_hand', {
      payload: { communityId: 'comm', channelId: 'chan', raised: false },
    });
  });

  it('setHand is a no-op when not connected', async () => {
    const s = newSession();
    await s.setHand(true);
    expect(d.invoke).not.toHaveBeenCalledWith('set_voice_hand', expect.anything());
  });

  it('inviteToSpeak issues a moderate_voice invite directive', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    await s.inviteToSpeak('cc'.repeat(16));
    expect(d.invoke).toHaveBeenCalledWith('moderate_voice', {
      payload: {
        communityId: 'comm', channelId: 'chan',
        targetOwnerHex: 'cc'.repeat(16), action: 'invite',
      },
    });
  });
});

describe('speakerQueue (ZEB-612 derived queue)', () => {
  const member = (ownerHex: string, handRaisedAt: number | null): RosterMember => ({
    ownerHex, deviceHex: ownerHex, muted: true, speaking: false,
    modMuted: false, power: 0, handRaisedAt, invited: false,
  });

  it('orders raised hands by raise time, oldest first', () => {
    const q = speakerQueue([
      member('cc'.repeat(16), 300),
      member('aa'.repeat(16), 100),
      member('bb'.repeat(16), null), // lowered → excluded
      member('dd'.repeat(16), 200),
    ]);
    expect(q.map((m) => m.handRaisedAt)).toEqual([100, 200, 300]);
  });

  it('breaks raise-time ties by owner hex (deterministic on every client)', () => {
    const q = speakerQueue([
      member('ff'.repeat(16), 100),
      member('11'.repeat(16), 100),
    ]);
    expect(q.map((m) => m.ownerHex)).toEqual(['11'.repeat(16), 'ff'.repeat(16)]);
  });

  it('is a total order: same owner + same raise time falls to device hex', () => {
    // The roster is device-keyed, so one owner can hold two rows. The
    // comparator must return 0/-1/1 consistently (sort contract), with the
    // device leg making the order total and stable across clients.
    const a = { ...member('aa'.repeat(16), 100), deviceHex: 'ff'.repeat(32) };
    const b = { ...member('aa'.repeat(16), 100), deviceHex: '11'.repeat(32) };
    expect(speakerQueue([a, b]).map((m) => m.deviceHex))
      .toEqual(['11'.repeat(32), 'ff'.repeat(32)]);
    expect(speakerQueue([b, a]).map((m) => m.deviceHex))
      .toEqual(['11'.repeat(32), 'ff'.repeat(32)]);
  });

  it('returns empty for a roster with no raised hands', () => {
    expect(speakerQueue([member('aa'.repeat(16), null)])).toEqual([]);
  });
});

describe('VoiceSession transport reconnect (ZEB-353)', () => {
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

  it('voice-transport-lost for the active channel sets reconnecting', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    expect(get(s.state).reconnecting).toBe(false);
    d.emit('voice-transport-lost', { communityId: 'comm', channelId: 'chan' });
    expect(get(s.state).reconnecting).toBe(true);
  });

  it('voice-transport-restored clears reconnecting', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    d.emit('voice-transport-lost', { communityId: 'comm', channelId: 'chan' });
    expect(get(s.state).reconnecting).toBe(true);
    d.emit('voice-transport-restored', { communityId: 'comm', channelId: 'chan' });
    expect(get(s.state).reconnecting).toBe(false);
  });

  it('ignores transport events for a different channel', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    d.emit('voice-transport-lost', { communityId: 'comm', channelId: 'other' });
    expect(get(s.state).reconnecting).toBe(false);
    d.emit('voice-transport-lost', { communityId: 'other', channelId: 'chan' });
    expect(get(s.state).reconnecting).toBe(false);
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
  it('a failing sender.start() (non-mic) resets to idle, tears down, and leaves the backend', async () => {
    // A NON-mic sender fault (NOT a denied-mic permission — that path now joins
    // listen-only, covered separately): backend join succeeds, then
    // sender.start() rejects. The session must not wedge in 'joining' or leak
    // the partially-built mixer/receiver/timer — and must release the backend.
    const invoke = vi.fn(async () => undefined);
    const listen = vi.fn(async () => () => {});
    const receiver = { init: vi.fn(async () => {}), destroy: vi.fn(), getActiveSenders: () => [], isSpeaking: () => false };
    const mixer = { init: vi.fn(async () => {}), pushFrame: vi.fn(), drain: vi.fn(), setDeafened: vi.fn(), destroy: vi.fn(async () => {}) };
    const sender = {
      start: vi.fn(async () => { throw new Error('encoder init failed'); }),
      stop: vi.fn(async () => {}),
    };
    const s = new VoiceSession({
      invoke, listen, selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      makeSender: () => sender as never, makeReceiver: () => receiver as never, makeMixer: () => mixer as never,
    });

    await expect(s.join('comm', 'chan')).rejects.toThrow(/encoder init failed/);
    expect(get(s.state).phase).toBe('idle');          // not wedged in 'joining'
    expect(mixer.destroy).toHaveBeenCalled();          // partial audio torn down
    expect(receiver.destroy).toHaveBeenCalled();
    expect(sender.stop).toHaveBeenCalled();
    // Backend join succeeded, so it must be released on the failure path.
    expect(invoke).toHaveBeenCalledWith('leave_voice_channel', { communityId: 'comm', channelId: 'chan' });
    // Retryable: a second join is rejected by the failing sender, NOT by an
    // "already active" guard left over from the wedged first attempt.
    await expect(s.join('comm', 'chan')).rejects.toThrow(/encoder init failed/);
  });
});

describe('classifyMicError', () => {
  it('classifies permission/blocked errors', () => {
    expect(classifyMicError(Object.assign(new Error('x'), { name: 'NotAllowedError' }))).toBe('blocked');
    expect(classifyMicError(Object.assign(new Error('x'), { name: 'PermissionDeniedError' }))).toBe('blocked');
    expect(classifyMicError(Object.assign(new Error('x'), { name: 'SecurityError' }))).toBe('blocked');
    expect(classifyMicError(new Error('Permission denied'))).toBe('blocked');
  });
  it('classifies notfound/device errors', () => {
    expect(classifyMicError(Object.assign(new Error('x'), { name: 'NotFoundError' }))).toBe('notfound');
    expect(classifyMicError(Object.assign(new Error('x'), { name: 'OverconstrainedError' }))).toBe('notfound');
    expect(classifyMicError(new Error('Requested device not found'))).toBe('notfound');
    expect(classifyMicError(new Error('no microphone available'))).toBe('notfound');
  });
  it('returns null for non-mic errors', () => {
    expect(classifyMicError(new Error('boom'))).toBe(null);
    expect(classifyMicError(undefined)).toBe(null);
  });
  it('does not false-match "no" as a substring of a generic system error (round 5)', () => {
    // "Cannot initialize audio context" contains "no" (in "Cannot") and "audio",
    // but is NOT a mic device fault — the tightened `\bno\b` word boundary must
    // keep this on the null (full-rollback) path, not the silent listen-only one.
    expect(classifyMicError(new Error('Cannot initialize audio context'))).toBe(null);
    // A real "no microphone found" still classifies as notfound.
    expect(
      classifyMicError(Object.assign(new Error('no microphone found'), { name: 'NotFoundError' })),
    ).toBe('notfound');
    // Sanity: a denied-permission error still classifies as blocked.
    expect(
      classifyMicError(Object.assign(new Error('Permission denied'), { name: 'NotAllowedError' })),
    ).toBe('blocked');
  });
});

describe('VoiceSession mic-blocked → listen-only join (ZEB-353)', () => {
  function makeDeps(senderStart: () => Promise<void>) {
    const invoke = vi.fn(async () => undefined);
    const listen = vi.fn(async () => () => {});
    const receiver = { init: vi.fn(async () => {}), destroy: vi.fn(), getActiveSenders: () => [], isSpeaking: () => false };
    const mixer = { init: vi.fn(async () => {}), pushFrame: vi.fn(), drain: vi.fn(), setDeafened: vi.fn(), destroy: vi.fn(async () => {}) };
    let capturedGate: ((pcm: Float32Array) => { send: boolean; ptt: boolean }) | undefined;
    const sender = {
      start: vi.fn(senderStart),
      stop: vi.fn(async () => {}),
    };
    const s = new VoiceSession({
      invoke, listen, selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      makeSender: (g) => { capturedGate = g; return sender as never; },
      makeReceiver: () => receiver as never, makeMixer: () => mixer as never,
    });
    return { s, invoke, receiver, mixer, sender, getGate: () => capturedGate };
  }

  it('a mic-permission error joins listen-only (connected, micBlocked, no sender)', async () => {
    const { s, invoke, receiver, mixer, sender, getGate } = makeDeps(
      async () => { throw Object.assign(new Error('Permission denied'), { name: 'NotAllowedError' }); },
    );
    await s.join('comm', 'chan'); // resolves — does NOT reject

    expect(get(s.state).phase).toBe('connected'); // listen-only still connects
    expect(get(s.state).micBlocked).toBe(true);
    // The mixer + receiver were built and inited before the sender, so they're up.
    expect(mixer.init).toHaveBeenCalled();
    expect(receiver.init).toHaveBeenCalled();
    expect(sender.start).toHaveBeenCalled();
    // The half-started sender is dropped — gate has nothing to publish through.
    expect(getGate()!(new Float32Array(320)).send).toBe(false); // muted anyway
    // We stayed joined — no teardown of the backend channel.
    expect(invoke).not.toHaveBeenCalledWith('leave_voice_channel', { communityId: 'comm', channelId: 'chan' });
  });

  it('a NON-mic error still rolls back to idle and rejects (transactional)', async () => {
    const { s, invoke, mixer, receiver } = makeDeps(
      async () => { throw new Error('boom'); },
    );
    await expect(s.join('comm', 'chan')).rejects.toThrow(/boom/);
    expect(get(s.state).phase).toBe('idle');
    expect(get(s.state).micBlocked).toBe(false); // never flips for a non-mic fault
    expect(mixer.destroy).toHaveBeenCalled();
    expect(receiver.destroy).toHaveBeenCalled();
    expect(invoke).toHaveBeenCalledWith('leave_voice_channel', { communityId: 'comm', channelId: 'chan' });
  });
});

describe('VoiceSession moderation (ZEB-358)', () => {
  let d: ReturnType<typeof deps>;
  beforeEach(() => { d = deps(); });
  const newSession = () => new VoiceSession({
    invoke: d.invoke, listen: d.listen,
    selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
    senderHash: new Uint8Array(16), ...d.factories,
  });

  it('overlay marks roster modMuted + power and hides kicked owners', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    d.emit('voice-presence-changed', { community: 'comm', channel: 'chan', roster: [
      { owner: 'a'.repeat(32), device: 'a'.repeat(64), muted: false },
      { owner: 'c'.repeat(32), device: 'c'.repeat(64), muted: false },
    ]});
    d.emit('voice-moderation-changed', { community: 'comm', channel: 'chan',
      mutedOwners: ['a'.repeat(32)], kickedOwners: ['c'.repeat(32)],
      powers: { ['a'.repeat(32)]: 0 }, selfPower: 60, selfModMuted: false, selfKicked: false });
    const st = get(s.state);
    expect(st.roster.find((m) => m.ownerHex === 'a'.repeat(32))?.modMuted).toBe(true);
    expect(st.roster.find((m) => m.ownerHex === 'c'.repeat(32))).toBeUndefined();
    expect(st.selfPower).toBe(60);
  });

  it('selfModMuted force-mutes, blocks self-unmute, and stays muted after it clears', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    await s.setMuted(false);                       // allowed: not moderated yet
    expect(get(s.state).muted).toBe(false);
    d.emit('voice-moderation-changed', { community: 'comm', channel: 'chan',
      mutedOwners: ['aa'.repeat(16)], kickedOwners: [], powers: {}, selfPower: 0, selfModMuted: true, selfKicked: false });
    expect(get(s.state).muted).toBe(true);         // force-muted
    await s.setMuted(false);                        // blocked
    expect(get(s.state).muted).toBe(true);
    d.emit('voice-moderation-changed', { community: 'comm', channel: 'chan',
      mutedOwners: [], kickedOwners: [], powers: {}, selfPower: 0, selfModMuted: false, selfKicked: false });
    expect(get(s.state).muted).toBe(true);         // unattended-mic guard: stays muted
  });

  it('force-mute stays muted even when set_voice_muted rejects (no rollback)', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    await s.setMuted(false);                        // open the mic first
    expect(get(s.state).muted).toBe(false);
    // Make ONLY set_voice_muted reject; everything else still resolves.
    d.invoke.mockImplementation((cmd: string) =>
      cmd === 'set_voice_muted' ? Promise.reject(new Error('x')) : Promise.resolve(undefined));
    d.emit('voice-moderation-changed', { community: 'comm', channel: 'chan',
      mutedOwners: ['aa'.repeat(16)], kickedOwners: [], powers: {}, selfPower: 0, selfModMuted: true, selfKicked: false });
    expect(get(s.state).muted).toBe(true);          // mic STAYS muted despite the IPC rejection
    d.invoke.mockResolvedValue(undefined);          // restore the mock
  });

  it('moderate() invokes moderate_voice', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    await s.moderate('cc'.repeat(16), 'mute');
    // `payload: ModerateVoicePayload` struct → Tauri requires the `payload` wrapper.
    expect(d.invoke).toHaveBeenCalledWith('moderate_voice',
      { payload: { communityId: 'comm', channelId: 'chan', targetOwnerHex: 'cc'.repeat(16), action: 'mute' } });
  });

  it('selfKicked is exposed and silences', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    d.emit('voice-moderation-changed', { community: 'comm', channel: 'chan',
      mutedOwners: [], kickedOwners: ['aa'.repeat(16)], powers: {}, selfPower: 0, selfModMuted: false, selfKicked: true });
    expect(get(s.state).selfKicked).toBe(true);
    expect(get(s.state).muted).toBe(true);
  });
});

// ZEB-354: getVoiceSession must be a PER-IDENTITY singleton (mirror
// getCallSession / getGroupCallSession), so a stop_node/start_node identity
// switch rebuilds the session instead of keeping the previous identity's keys/
// adapter/IPC handles.
describe('getVoiceSession identity-switch singleton', () => {
  function vsDeps(selfOwnerHex: string) {
    const d = deps();
    return {
      invoke: d.invoke,
      listen: d.listen,
      selfOwnerHex,
      selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      ...d.factories,
    };
  }

  it('returns a new instance and destroys the old on identity switch', () => {
    const first = getVoiceSession(vsDeps('aa'.repeat(16)));
    const destroySpy = vi.spyOn(first, 'destroy');

    // Same identity ⇒ cached instance, not destroyed.
    const same = getVoiceSession(vsDeps('aa'.repeat(16)));
    expect(same).toBe(first);
    expect(destroySpy).not.toHaveBeenCalled();

    // Different identity ⇒ new instance, the old one destroyed.
    const second = getVoiceSession(vsDeps('fe'.repeat(16)));
    expect(second).not.toBe(first);
    expect(destroySpy).toHaveBeenCalled();
  });
});

describe('VoiceSession audio device following (ZEB-359)', () => {
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

  it('an input pref change while connected restarts the sender capture', async () => {
    const d = deps();
    const p = prefsStub();
    const s = new VoiceSession({
      invoke: d.invoke, listen: d.listen,
      selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      audioDevices: p.prefs,
      ...d.factories,
    });
    await s.join('comm', 'chan');
    expect(p.subCount()).toBe(1);
    p.setInput('mic-b');
    await flush();
    expect(d.sender.switchInputDevice).toHaveBeenCalledTimes(1);
    await s.leave();
  });

  it('leave() unfollows — later pref changes touch nothing', async () => {
    const d = deps();
    const p = prefsStub();
    const s = new VoiceSession({
      invoke: d.invoke, listen: d.listen,
      selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      audioDevices: p.prefs,
      ...d.factories,
    });
    await s.join('comm', 'chan');
    await s.leave();
    expect(p.subCount()).toBe(0);
    p.setInput('mic-b');
    await flush();
    expect(d.sender.switchInputDevice).not.toHaveBeenCalled();
  });

  it('a failed live switch drops to listen-only (micBlocked), keeping the call up', async () => {
    const d = deps();
    const p = prefsStub();
    d.sender.switchInputDevice.mockRejectedValueOnce(
      Object.assign(new Error('no microphone found'), { name: 'NotFoundError' }),
    );
    const s = new VoiceSession({
      invoke: d.invoke, listen: d.listen,
      selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      audioDevices: p.prefs,
      ...d.factories,
    });
    await s.join('comm', 'chan');
    p.setInput('mic-b');
    await flush();
    expect(get(s.state).phase).toBe('connected');
    expect(get(s.state).micBlocked).toBe(true);
    await s.leave();
  });
});
