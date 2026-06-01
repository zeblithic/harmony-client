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
