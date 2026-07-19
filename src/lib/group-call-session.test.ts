// src/lib/group-call-session.test.ts
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { get } from 'svelte/store';
import { GroupCallSession, getGroupCallSession, type GroupCallSessionDeps } from './group-call-session';

// ── Identity fixtures ────────────────────────────────────────────────────────
// callId is 16 bytes = 32 hex. owner-hex is 32 hex. device-hex is 64 hex whose
// first 32 hex are the senderHash (the controller compares device.slice(0,32)).
const SELF_OWNER = 'aa'.repeat(16); // 32 hex
const ALICE = 'bb'.repeat(16);
const BOB = 'cc'.repeat(16);
const CAROL = 'dd'.repeat(16);

const SELF_DEVICE = '11'.repeat(32); // 64 hex; first 32 hex = senderHash hex
const ALICE_DEVICE = '22'.repeat(32);
const SENDER_HASH = new Uint8Array(16).fill(0x11); // bytesToHex ⇒ '11'.repeat(16) = first 32 hex of SELF_DEVICE

const PLACED_CALL_ID = 'ab'.repeat(16); // 32 hex — what place_group_call resolves

/** Build injectable deps mirroring call-session.test.ts's `deps()` template. */
function makeDeps(overrides: Partial<GroupCallSessionDeps> = {}) {
  const invoke = vi.fn(async (cmd: string) => {
    if (cmd === 'place_group_call') return PLACED_CALL_ID;
    return undefined;
  });

  // Capture every listen() handler by event so the test can emit.
  const listeners = new Map<string, ((e: { payload: unknown }) => void)[]>();
  const listen = vi.fn(async (ev: string, h: (e: { payload: unknown }) => void) => {
    (listeners.get(ev) ?? listeners.set(ev, []).get(ev)!).push(h);
    return () => {};
  });
  const emit = (ev: string, payload: unknown) =>
    (listeners.get(ev) ?? []).forEach((h) => h({ payload }));

  // No-op media stubs so connect() never touches real audio.
  const sender = {
    start: vi.fn(async () => {}),
    stop: vi.fn(async () => {}),
    switchInputDevice: vi.fn(async () => {}),
  };
  const receiver = {
    init: vi.fn(async () => {}),
    destroy: vi.fn(),
    getActiveSenders: () => [],
    isSpeaking: () => false,
  };
  const mixer = {
    init: vi.fn(async () => {}),
    pushFrame: vi.fn(),
    drain: vi.fn(),
    setDeafened: vi.fn(),
    destroy: vi.fn(async () => {}),
  };

  const deps: GroupCallSessionDeps = {
    invoke,
    listen,
    selfOwnerHex: SELF_OWNER,
    selfDeviceHex: SELF_DEVICE,
    senderHash: SENDER_HASH,
    resolveMembers: () => [SELF_OWNER, ALICE, BOB, CAROL],
    makeSender: () => sender as never,
    makeReceiver: () => receiver as never,
    makeMixer: () => mixer as never,
    ...overrides,
  };

  return { deps, invoke, listen, emit, sender, receiver, mixer };
}

/** All cmd names ever passed to invoke, in order. */
function invokedCmds(invoke: ReturnType<typeof vi.fn>): string[] {
  return invoke.mock.calls.map((c) => c[0] as string);
}

const tick = () => new Promise((r) => setTimeout(r, 0));

describe('GroupCallSession signaling + roster', () => {
  let d: ReturnType<typeof makeDeps>;
  beforeEach(() => {
    d = makeDeps();
  });

  function newSession() {
    return new GroupCallSession(d.deps);
  }

  // 1 ─────────────────────────────────────────────────────────────────────────
  it('placeGroupCall drops straight into media: place_group_call → join_group_call, no ringingOut', async () => {
    const s = newSession();
    const callId = await s.placeGroupCall('space-1');
    expect(callId).toBe(PLACED_CALL_ID);

    const cmds = invokedCmds(d.invoke);
    expect(cmds[0]).toBe('place_group_call');
    expect(cmds).toContain('join_group_call');
    // place precedes join.
    expect(cmds.indexOf('place_group_call')).toBeLessThan(cmds.indexOf('join_group_call'));

    expect(d.invoke).toHaveBeenCalledWith('place_group_call', { spaceId: 'space-1' });
    expect(d.invoke).toHaveBeenCalledWith('join_group_call', { callId: PLACED_CALL_ID, spaceId: 'space-1' });

    const st = get(s.state);
    expect(st.phase).toBe('active');
    expect(st.callId).toBe(PLACED_CALL_ID);
    expect(st.startedAt).not.toBeNull();
    // 'ringingOut' does not exist in the group phase union — it is never reached.
    expect(st.phase).not.toBe('ringingOut');
  });

  // 3 ─────────────────────────────────────────────────────────────────────────
  it('accept from incoming joins (no accept-signal IPC) and goes active', async () => {
    const s = newSession();
    s.onIncomingGroup('c1'.repeat(16), BOB, 'space-1');
    expect(get(s.state).phase).toBe('incoming');
    expect(get(s.state).callerOwnerHex).toBe(BOB);

    await s.accept();
    expect(get(s.state).phase).toBe('active');
    expect(d.invoke).toHaveBeenCalledWith('join_group_call', { callId: 'c1'.repeat(16), spaceId: 'space-1' });
    // There is no accept-signal IPC for the ring-all model.
    expect(invokedCmds(d.invoke)).not.toContain('accept_group_call');
    expect(invokedCmds(d.invoke)).not.toContain('accept_call');
  });

  // 4 ─────────────────────────────────────────────────────────────────────────
  it('decline from incoming invokes decline_group_call(callerOwner) and returns to idle', async () => {
    const s = newSession();
    s.onIncomingGroup('c1'.repeat(16), BOB, 'space-1');
    await s.decline();
    expect(get(s.state).phase).toBe('idle');
    expect(get(s.state).callId).toBeNull();
    expect(d.invoke).toHaveBeenCalledWith('decline_group_call', {
      callId: 'c1'.repeat(16),
      spaceId: 'space-1',
      callerOwner: BOB,
    });
  });

  // 5 ─────────────────────────────────────────────────────────────────────────
  it('joinActive from idle joins straight to active with no ring', async () => {
    const s = newSession();
    await s.joinActive('c9'.repeat(16), 'space-9');
    expect(get(s.state).phase).toBe('active');
    expect(get(s.state).callId).toBe('c9'.repeat(16));
    expect(d.invoke).toHaveBeenCalledWith('join_group_call', { callId: 'c9'.repeat(16), spaceId: 'space-9' });
    // No incoming/ring path: callerOwnerHex stays null and no decline timer arms.
    expect(get(s.state).callerOwnerHex).toBeNull();
  });

  // 6 ─────────────────────────────────────────────────────────────────────────
  it('roster merge: live beacons → in-call, non-live members → ringing, self not duplicated', async () => {
    const s = newSession();
    await s.joinActive('c1'.repeat(16), 'space-1');

    s.onPresenceChanged('c1'.repeat(16), [
      { owner: SELF_OWNER, device: SELF_DEVICE, muted: true },
      { owner: ALICE, device: ALICE_DEVICE, muted: false },
    ]);

    const parts = get(s.state).participants;
    const byOwner = new Map(parts.map((p) => [p.ownerHex, p]));

    // self + alice are live ⇒ in-call.
    expect(byOwner.get(SELF_OWNER)?.state).toBe('in-call');
    expect(byOwner.get(SELF_OWNER)?.muted).toBe(true);
    expect(byOwner.get(ALICE)?.state).toBe('in-call');
    expect(byOwner.get(ALICE)?.muted).toBe(false);

    // bob + carol are members but have no beacon ⇒ ringing.
    expect(byOwner.get(BOB)?.state).toBe('ringing');
    expect(byOwner.get(CAROL)?.state).toBe('ringing');

    // self appears exactly once (live row), never duplicated into a ringing row.
    expect(parts.filter((p) => p.ownerHex === SELF_OWNER)).toHaveLength(1);
    expect(parts).toHaveLength(4); // self, alice, bob, carol
  });

  // 7 ─────────────────────────────────────────────────────────────────────────
  it('onDeclined flips a ringing member to declined', async () => {
    const s = newSession();
    await s.joinActive('c1'.repeat(16), 'space-1');
    s.onPresenceChanged('c1'.repeat(16), [
      { owner: SELF_OWNER, device: SELF_DEVICE, muted: true },
      { owner: ALICE, device: ALICE_DEVICE, muted: false },
    ]);
    expect(get(s.state).participants.find((p) => p.ownerHex === BOB)?.state).toBe('ringing');

    s.onDeclined('c1'.repeat(16), BOB);
    expect(get(s.state).participants.find((p) => p.ownerHex === BOB)?.state).toBe('declined');
    // alice (live) and carol (still ringing) unaffected.
    expect(get(s.state).participants.find((p) => p.ownerHex === ALICE)?.state).toBe('in-call');
    expect(get(s.state).participants.find((p) => p.ownerHex === CAROL)?.state).toBe('ringing');
  });

  // 8 ─────────────────────────────────────────────────────────────────────────
  it('setMuted rolls back muted when set_group_call_muted rejects', async () => {
    const s = newSession();
    await s.joinActive('c1'.repeat(16), 'space-1');
    expect(get(s.state).muted).toBe(true); // starts muted (D10)

    d.invoke.mockRejectedValueOnce(new Error('backend refused'));
    await expect(s.setMuted(false)).rejects.toThrow(/refused/);
    // Store must NOT advertise unmuted when the backend stayed muted.
    expect(get(s.state).muted).toBe(true);
  });

  // 9 ─────────────────────────────────────────────────────────────────────────
  it('setPttMode rolls BOTH pttMode and pttHeld back when the coupled setMuted fails', async () => {
    const s = newSession();
    await s.joinActive('c1'.repeat(16), 'space-1');
    expect(get(s.state).pttMode).toBe(false);
    expect(get(s.state).muted).toBe(true);

    d.invoke.mockRejectedValueOnce(new Error('mute refused'));
    await expect(s.setPttMode(true)).rejects.toThrow(/refused/);
    // No "PTT on but muted" limbo: mode and mute roll back together.
    expect(get(s.state).pttMode).toBe(false);
    expect(get(s.state).pttHeld).toBe(false);
    expect(get(s.state).muted).toBe(true);
  });

  // deafen ─────────────────────────────────────────────────────────────────────
  it('setDeafened(true) while unmuted implies self-mute (set_group_call_muted true)', async () => {
    const s = newSession();
    await s.joinActive('c1'.repeat(16), 'space-1');
    await s.setMuted(false); // unmute first
    expect(get(s.state).muted).toBe(false);
    d.invoke.mockClear();

    await s.setDeafened(true);
    expect(get(s.state).deafened).toBe(true);
    expect(get(s.state).muted).toBe(true); // deafen implied self-mute
    expect(d.mixer.setDeafened).toHaveBeenCalledWith(true);
    expect(d.invoke).toHaveBeenCalledWith('set_group_call_muted', {
      payload: { callId: 'c1'.repeat(16), spaceId: 'space-1', muted: true },
    });
  });

  it('setDeafened rolls deafened back when the coupled setMuted rejects', async () => {
    const s = newSession();
    await s.joinActive('c1'.repeat(16), 'space-1');
    await s.setMuted(false); // unmute first so deafen triggers the coupled self-mute
    expect(get(s.state).muted).toBe(false);
    expect(get(s.state).deafened).toBe(false);
    d.invoke.mockClear();

    d.invoke.mockRejectedValueOnce(new Error('mute refused'));
    await expect(s.setDeafened(true)).rejects.toThrow(/refused/);
    // The coupled mute IPC failed → deafen must NOT stick (no "deafened but the
    // backend never self-muted" limbo), and the mixer is rolled back too.
    expect(get(s.state).deafened).toBe(false);
    expect(d.mixer.setDeafened).toHaveBeenLastCalledWith(false);
  });

  // 10 ────────────────────────────────────────────────────────────────────────
  it('leave invokes leave_group_call, returns to idle, clears participants', async () => {
    const s = newSession();
    await s.joinActive('c1'.repeat(16), 'space-1');
    s.onPresenceChanged('c1'.repeat(16), [
      { owner: SELF_OWNER, device: SELF_DEVICE, muted: true },
      { owner: ALICE, device: ALICE_DEVICE, muted: false },
    ]);
    expect(get(s.state).participants.length).toBeGreaterThan(0);

    await s.leave();
    expect(get(s.state).phase).toBe('idle');
    expect(get(s.state).participants).toEqual([]);
    expect(get(s.state).callId).toBeNull();
    expect(d.invoke).toHaveBeenCalledWith('leave_group_call', { callId: 'c1'.repeat(16), spaceId: 'space-1' });
    expect(d.sender.stop).toHaveBeenCalled();
    expect(d.receiver.destroy).toHaveBeenCalled();
    expect(d.mixer.destroy).toHaveBeenCalled();
  });

  // 11 ────────────────────────────────────────────────────────────────────────
  it('busy: onIncomingGroup while active is a no-op (call untouched)', async () => {
    const s = newSession();
    await s.joinActive('c1'.repeat(16), 'space-1');
    expect(get(s.state).phase).toBe('active');
    d.invoke.mockClear();

    s.onIncomingGroup('ee'.repeat(16), CAROL, 'space-other');
    // Phase + callId unchanged; no signaling fired for the second invite.
    expect(get(s.state).phase).toBe('active');
    expect(get(s.state).callId).toBe('c1'.repeat(16));
    expect(get(s.state).callerOwnerHex).toBeNull();
    expect(invokedCmds(d.invoke)).not.toContain('decline_group_call');
  });
});

// 2 ────────────────────────────────────────────────────────────────────────────
describe('GroupCallSession ring timeout', () => {
  let d: ReturnType<typeof makeDeps>;
  beforeEach(() => {
    vi.useFakeTimers();
    d = makeDeps();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('onIncomingGroup → incoming; unanswered 30s auto-declines via decline_group_call', async () => {
    const s = new GroupCallSession(d.deps);
    s.onIncomingGroup('c1'.repeat(16), BOB, 'space-1');
    expect(get(s.state).phase).toBe('incoming');
    expect(get(s.state).callerOwnerHex).toBe(BOB);

    await vi.advanceTimersByTimeAsync(30_000);
    expect(get(s.state).phase).toBe('idle');
    expect(d.invoke).toHaveBeenCalledWith('decline_group_call', {
      callId: 'c1'.repeat(16),
      spaceId: 'space-1',
      callerOwner: BOB,
    });
  });

  it('accepting before the timeout cancels the auto-decline', async () => {
    const s = new GroupCallSession(d.deps);
    s.onIncomingGroup('c1'.repeat(16), BOB, 'space-1');
    await s.accept();
    d.invoke.mockClear();
    await vi.advanceTimersByTimeAsync(30_000);
    expect(d.invoke).not.toHaveBeenCalledWith('decline_group_call', expect.anything());
    expect(get(s.state).phase).toBe('active');
  });
});

// 12 ───────────────────────────────────────────────────────────────────────────
describe('GroupCallSession transport reconnect + identity rebuild', () => {
  let d: ReturnType<typeof makeDeps>;
  beforeEach(() => {
    d = makeDeps();
  });

  async function activeCall(callId = 'c1'.repeat(16)) {
    const s = new GroupCallSession(d.deps);
    await s.joinActive(callId, 'space-1');
    expect(get(s.state).phase).toBe('active');
    return s;
  }

  it('voice-transport-lost (callId-matched) sets reconnecting; restored clears it', async () => {
    const s = await activeCall();
    expect(get(s.state).reconnecting).toBe(false);

    d.emit('voice-transport-lost', { callId: 'c1'.repeat(16) });
    expect(get(s.state).reconnecting).toBe(true);

    d.emit('voice-transport-restored', { callId: 'c1'.repeat(16) });
    expect(get(s.state).reconnecting).toBe(false);
  });

  it('ignores transport events for a different call', async () => {
    const s = await activeCall();
    d.emit('voice-transport-lost', { callId: 'ff'.repeat(16) });
    expect(get(s.state).reconnecting).toBe(false);
  });

  it('getGroupCallSession returns a new instance and destroys the old on identity switch', async () => {
    const first = getGroupCallSession(d.deps);
    const destroySpy = vi.spyOn(first, 'destroy');

    // Same identity ⇒ cached instance, not destroyed.
    const same = getGroupCallSession(d.deps);
    expect(same).toBe(first);
    expect(destroySpy).not.toHaveBeenCalled();

    // Different identity ⇒ new instance, first destroyed.
    const d2 = makeDeps({ selfOwnerHex: 'fe'.repeat(16) });
    const second = getGroupCallSession(d2.deps);
    expect(second).not.toBe(first);
    expect(destroySpy).toHaveBeenCalled();

    await tick();
  });
});

describe('GroupCallSession audio device following (ZEB-359)', () => {
  const flush = () => new Promise((r) => setTimeout(r, 0));

  it('an input pref change during an active group call restarts the sender capture', async () => {
    let input: string | null = null;
    const subs = new Set<() => void>();
    const prefs = {
      getInput: () => input,
      getOutput: () => null,
      listDevices: async () => ({
        inputs: input ? [{ deviceId: input, label: 'Mic' }] : [],
        outputs: [],
      }),
      subscribe: (cb: () => void) => {
        subs.add(cb);
        return () => subs.delete(cb);
      },
    };
    const d = makeDeps({ audioDevices: prefs });
    const s = new GroupCallSession(d.deps);
    s.onIncomingGroup('c1'.repeat(16), BOB, 'space-1');
    await s.accept();
    expect(subs.size).toBe(1);
    input = 'mic-b';
    for (const cb of [...subs]) cb();
    await flush();
    expect(d.sender.switchInputDevice).toHaveBeenCalledTimes(1);
    await s.leave();
    expect(subs.size).toBe(0);
  });
});
