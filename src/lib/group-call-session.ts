// src/lib/group-call-session.ts
//
// GroupCallSession — controller for group-DM voice calls (3–16 participants).
// Generalizes the 1:1 CallSession to N parties: ring-all signaling + a
// presence-driven participant roster (VoiceSession-style) + the V3 N-stream
// media engine, addressed by a `callId`. Media core PORTED from CallSession;
// roster merge new. Drop-in + ring (D1).

import { writable, get, type Readable } from 'svelte/store';
import { VoiceActivityDetector } from './voice/vad';
import { makeTalkGate } from './voice/talk-gate';
import { VoiceSender } from './voice/voice-sender';
import { VoiceReceiver } from './voice/voice-receiver';
import { VoiceMixer } from './voice/voice-mixer';
import { AudioCapture } from './voice/audio-capture';
import { OpusCodec } from './voice/opus-codec';
import { Codec2Codec } from './voice/codec2-codec';
import type { CodecType } from './voice/voice-codec';
import { followAudioDevices, type DeviceFollowerDeps } from './voice/audio-device-follower';
import { resolveMemberName } from './display-label';

export type GroupCallPhase = 'idle' | 'incoming' | 'connecting' | 'active' | 'leaving';

export interface Participant {
  ownerHex: string;
  deviceHex: string;
  muted: boolean;
  speaking: boolean;
  displayName?: string;
  avatarUrl?: string;
  state: 'in-call' | 'ringing' | 'declined';
}

export interface GroupCallSessionState {
  phase: GroupCallPhase;
  callId: string | null;
  spaceId: string | null;
  participants: Participant[];
  muted: boolean;
  pttMode: boolean;
  pttHeld: boolean;
  deafened: boolean;
  startedAt: number | null;
  reconnecting: boolean;
  callerOwnerHex: string | null;
}

type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
type Listen = (ev: string, h: (e: { payload: unknown }) => void) => Promise<() => void>;
type FrameGate = (pcm: Float32Array) => { send: boolean; ptt: boolean };

function bytesToHex(b: Uint8Array): string {
  return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
}

const RING_TIMEOUT_MS = 30_000;

export interface GroupCallSessionDeps {
  invoke: Invoke;
  listen: Listen;
  selfOwnerHex: string;
  selfDeviceHex: string;
  senderHash: Uint8Array;
  vadThreshold?: number;
  makeSender?: (gate: FrameGate) => Pick<VoiceSender, 'start' | 'stop' | 'switchInputDevice'>;
  makeReceiver?: () => Pick<VoiceReceiver, 'init' | 'destroy' | 'getActiveSenders' | 'isSpeaking'>;
  makeMixer?: () => Pick<VoiceMixer, 'init' | 'pushFrame' | 'drain' | 'setDeafened' | 'destroy' | 'setOutputDevice'>;
  /** ZEB-359: device preference source. Absent ⇒ system defaults, no following. */
  audioDevices?: DeviceFollowerDeps['prefs'];
  resolveCard?: (ownerHex: string) => { displayName?: string; avatarUrl?: string } | undefined;
  /** Friend nickname resolver — the top rung of the name ladder (ZEB-958). */
  resolveNickname?: (ownerHex: string) => string | undefined;
  /** Resolve a group DM space → its full member owner-hex list (drives the
   *  ringing/declined rows that have no live beacon yet). */
  resolveMembers?: (spaceId: string) => string[];
  onRosterOwners?: (ownerHexes: string[]) => void;
}

const INITIAL: GroupCallSessionState = {
  phase: 'idle', callId: null, spaceId: null, participants: [],
  muted: true, pttMode: false, pttHeld: false, deafened: false,
  startedAt: null, reconnecting: false, callerOwnerHex: null,
};

export class GroupCallSession {
  readonly state: Readable<GroupCallSessionState>;
  private store = writable<GroupCallSessionState>({ ...INITIAL });
  private deps: GroupCallSessionDeps;

  private vad: VoiceActivityDetector;
  private sender: Pick<VoiceSender, 'start' | 'stop' | 'switchInputDevice'> | null = null;
  private receiver: Pick<VoiceReceiver, 'init' | 'destroy' | 'getActiveSenders' | 'isSpeaking'> | null = null;
  private mixer: Pick<VoiceMixer, 'init' | 'pushFrame' | 'drain' | 'setDeafened' | 'destroy' | 'setOutputDevice'> | null = null;

  private muted = true;
  private deafened = false;
  private pttMode = false;
  private pttHeld = false;
  private callId: string | null = null;
  private spaceId: string | null = null;
  private drainTimer: ReturnType<typeof setInterval> | null = null;
  private ringTimer: ReturnType<typeof setTimeout> | null = null;
  /** Transport-lifecycle unlisteners (voice-transport-lost/restored). Torn down
   *  by teardownMedia/destroy alongside the rest of the media engine. */
  private unlisteners: (() => void)[] = [];

  // ── Group-specific roster state ──────────────────────────────────────────
  /** Live presence beacons (in-call participants), keyed off the group presence
   *  feed via onPresenceChanged(). */
  private liveRoster: { ownerHex: string; deviceHex: string; muted: boolean }[] = [];
  /** Owners who declined the ring; demote their ringing row to 'declined'. */
  private declinedOwners = new Set<string>();
  /** Last self-speaking value pushed through the gate, for the roster diff. */
  private lastSelfSpeaking = false;

  constructor(deps: GroupCallSessionDeps) {
    this.deps = deps;
    this.state = this.store;
    this.vad = new VoiceActivityDetector({ threshold: deps.vadThreshold ?? 0.02 });
    // Built here (not as a field initializer) so `this.vad` is already assigned.
    // Deafen folds into the muted bit (deafen implies self-mute), mirroring
    // VoiceSession.
    this.coreGate = makeTalkGate(
      () => ({ muted: this.muted || this.deafened, pttMode: this.pttMode, pttHeld: this.pttHeld }),
      this.vad,
    );
  }

  /**
   * The per-frame send decision (mute / PTT / VAD). The core logic lives in the
   * shared `makeTalkGate`; here — UNLIKE the 1:1 CallSession — we ALSO drive the
   * self-speaking indicator on top of the pure result, like VoiceSession, since
   * the group roster shows a speaking ring for self.
   */
  private coreGate: FrameGate;
  private gate: FrameGate = (pcm) => {
    const decision = this.coreGate(pcm);
    this.setSelfSpeaking(decision.send);
    return decision;
  };

  private patch(p: Partial<GroupCallSessionState>): void {
    this.store.update((s) => ({ ...s, ...p }));
  }

  private resetToIdle(): void {
    this.clearRingTimer();
    this.muted = true; this.deafened = false; this.pttMode = false; this.pttHeld = false;
    this.callId = null; this.spaceId = null;
    this.liveRoster = []; this.declinedOwners = new Set(); this.lastSelfSpeaking = false;
    this.vad.reset();
    this.store.set({ ...INITIAL });
  }

  private clearRingTimer(): void {
    if (this.ringTimer) { clearTimeout(this.ringTimer); this.ringTimer = null; }
  }

  // ---------------------------------------------------------------------------
  // Signaling verbs (group-specific)
  // ---------------------------------------------------------------------------

  /** Caller: drop straight into media + ring all others (backend fans the invite). */
  async placeGroupCall(spaceId: string): Promise<string> {
    if (get(this.store).phase !== 'idle') throw new Error('A call is already in progress');
    const callId = (await this.deps.invoke('place_group_call', { spaceId })) as string;
    this.callId = callId; this.spaceId = spaceId;
    this.declinedOwners = new Set(); this.liveRoster = []; this.lastSelfSpeaking = false;
    this.muted = true; this.deafened = false; this.pttMode = false; this.pttHeld = false; this.vad.reset();
    this.patch({ phase: 'connecting', callId, spaceId, callerOwnerHex: null, startedAt: null,
      muted: true, deafened: false, pttMode: false, pttHeld: false });
    await this.connect(spaceId);
    return callId;
  }

  onIncomingGroup(callId: string, callerOwnerHex: string, spaceId: string): void {
    if (get(this.store).phase !== 'idle') return; // busy (D6) — ignore
    this.callId = callId; this.spaceId = spaceId;
    this.declinedOwners = new Set(); this.liveRoster = [];
    this.muted = true; this.deafened = false; this.pttMode = false; this.pttHeld = false;
    this.patch({ phase: 'incoming', callId, spaceId, callerOwnerHex, startedAt: null,
      muted: true, deafened: false, pttMode: false, pttHeld: false });
    this.clearRingTimer();
    this.ringTimer = setTimeout(() => { void this.decline(); }, RING_TIMEOUT_MS);
  }

  async accept(): Promise<void> {
    if (get(this.store).phase !== 'incoming') return;
    this.clearRingTimer();
    const spaceId = this.spaceId;
    if (!spaceId) throw new Error('missing space for incoming group call');
    this.patch({ phase: 'connecting' });
    await this.connect(spaceId);
  }

  async decline(): Promise<void> {
    if (get(this.store).phase !== 'incoming') return;
    this.clearRingTimer();
    const callId = this.callId, spaceId = this.spaceId, caller = get(this.store).callerOwnerHex;
    if (callId && spaceId && caller) {
      await this.deps.invoke('decline_group_call', { callId, spaceId, callerOwner: caller }).catch(() => {});
    }
    this.resetToIdle();
  }

  async joinActive(callId: string, spaceId: string): Promise<void> {
    if (get(this.store).phase !== 'idle') throw new Error('A call is already in progress');
    this.callId = callId; this.spaceId = spaceId;
    this.declinedOwners = new Set(); this.liveRoster = []; this.lastSelfSpeaking = false;
    this.muted = true; this.deafened = false; this.pttMode = false; this.pttHeld = false; this.vad.reset();
    this.patch({ phase: 'connecting', callId, spaceId, callerOwnerHex: null,
      muted: true, deafened: false, pttMode: false, pttHeld: false });
    await this.connect(spaceId);
  }

  async leave(): Promise<void> {
    const phase = get(this.store).phase;
    if (phase === 'idle' || phase === 'leaving') return;
    this.patch({ phase: 'leaving' });
    const callId = this.callId;
    await this.teardownMedia(callId);
    this.resetToIdle();
  }

  // ---------------------------------------------------------------------------
  // Remote-event handlers (the App wires these from `listen`). Each is guarded
  // on a matching callId so stale events for an old call can't disturb a current
  // one.
  // ---------------------------------------------------------------------------

  onPresenceChanged(callId: string, roster: { owner: string; device: string; muted: boolean }[]): void {
    if (this.callId !== callId) return;
    this.liveRoster = roster.map((r) => ({ ownerHex: r.owner, deviceHex: r.device, muted: r.muted }));
    this.deps.onRosterOwners?.(this.liveRoster.map((r) => r.ownerHex));
    this.refreshParticipants();
  }

  onDeclined(callId: string, ownerHex: string): void {
    if (this.callId !== callId) return;
    this.declinedOwners.add(ownerHex);
    this.refreshParticipants();
  }

  private setSelfSpeaking(v: boolean): void {
    if (v !== this.lastSelfSpeaking) { this.lastSelfSpeaking = v; this.refreshParticipants(); }
  }

  /** Merge live beacons (in-call) with full membership (ringing/declined). */
  private refreshParticipants(): void {
    const members = this.spaceId ? (this.deps.resolveMembers?.(this.spaceId) ?? []) : [];
    const live = new Map(this.liveRoster.map((r) => [r.ownerHex, r] as const));
    const out: Participant[] = [];
    for (const r of this.liveRoster) {
      const isSelf = r.deviceHex.slice(0, 32) === this.deps.selfDeviceHex.slice(0, 32);
      const speaking = isSelf
        ? (!this.muted && !this.deafened && this.lastSelfSpeaking)
        : (this.receiver?.isSpeaking(r.deviceHex.slice(0, 32)) ?? false);
      const card = this.deps.resolveCard?.(r.ownerHex);
      const displayName = resolveMemberName(r.ownerHex, this.deps.resolveNickname, this.deps.resolveCard);
      out.push({ ownerHex: r.ownerHex, deviceHex: r.deviceHex, muted: r.muted, speaking, state: 'in-call',
        ...(displayName ? { displayName } : {}),
        ...(card?.avatarUrl ? { avatarUrl: card.avatarUrl } : {}) });
    }
    for (const ownerHex of members) {
      if (live.has(ownerHex) || ownerHex === this.deps.selfOwnerHex) continue;
      const card = this.deps.resolveCard?.(ownerHex);
      const displayName = resolveMemberName(ownerHex, this.deps.resolveNickname, this.deps.resolveCard);
      out.push({ ownerHex, deviceHex: '', muted: true, speaking: false,
        state: this.declinedOwners.has(ownerHex) ? 'declined' : 'ringing',
        ...(displayName ? { displayName } : {}),
        ...(card?.avatarUrl ? { avatarUrl: card.avatarUrl } : {}) });
    }
    this.patch({ participants: out });
  }

  // ---------------------------------------------------------------------------
  // Media core — PORTED from CallSession with the group IPC swaps.
  // ---------------------------------------------------------------------------

  /**
   * Join the group call room and build the V3 media engine addressed by callId.
   * Group media reuses the DM media path (per the backend): frames arrive on
   * `dm-voice-frame-received` and are admitted by callId. Starts muted (D10):
   * the gate transmits nothing until the user unmutes.
   */
  private async connect(spaceId: string): Promise<void> {
    const callId = this.callId!;
    try {
      await this.deps.invoke('join_group_call', { callId, spaceId });
      await this.subscribeTransport();

      this.mixer = this.deps.makeMixer
        ? this.deps.makeMixer()
        : new VoiceMixer({
            outputDeviceId: () => this.deps.audioDevices?.getOutput() ?? null,
          });
      await this.mixer.init();

      this.receiver = this.deps.makeReceiver
        ? this.deps.makeReceiver()
        : new VoiceReceiver({
            listen: this.deps.listen,
            createCodec: (t: CodecType) => (t === 'codec2' ? new Codec2Codec() : new OpusCodec()),
            onPlayFrame: (hex, pcm) => this.mixer?.pushFrame(hex, pcm),
            ownSenderHex: bytesToHex(this.deps.senderHash),
            // Group media reuses the DM frame event; admit only this call's frames
            // (callIds are globally unique, so callId-filtering is safe).
            frameEvent: 'dm-voice-frame-received',
            frameFilter: (p) =>
              (p as { callId?: string }).callId === this.callId
                ? (p as { frameBytes: number[] }).frameBytes
                : null,
          });
      await this.receiver.init();

      this.sender = this.deps.makeSender
        ? this.deps.makeSender(this.gate)
        : new VoiceSender({
            senderHash: this.deps.senderHash,
            communityId: '', channelId: '', // unused on the group/DM media path; publishFrame routes instead.
            invoke: this.deps.invoke, codec: new OpusCodec(), capture: new AudioCapture(),
            frameGate: this.gate,
            publishFrame: (frameBytes) =>
              this.deps.invoke('send_group_voice_frame', {
                payload: { callId: this.callId, frameBytes },
              }),
            inputDeviceId: () => this.deps.audioDevices?.getInput() ?? null,
          });
      await this.sender.start(); // capture starts; muted gate ⇒ nothing transmits

      // ZEB-359: follow device-pref / hot-plug changes for the call's
      // lifetime. Unfollowed with the other unlisteners in teardownMedia.
      if (this.deps.audioDevices) {
        this.unlisteners.push(
          followAudioDevices({
            prefs: this.deps.audioDevices,
            getSender: () => this.sender,
            getMixer: () => this.mixer,
            onMicError: (e) => {
              // Keep the call up listen-only; the sender already deactivated
              // itself on the failed restart (mirrors CallSession).
              const msg = e instanceof Error ? e.message : String(e);
              console.warn('group call input switch failed; continuing listen-only:', msg);
              void this.sender?.stop().catch(() => {});
              this.sender = null;
            },
          }),
        );
      }

      // Drain the mixer every 20ms (audio cadence), mirroring VoiceSession.
      this.drainTimer = setInterval(() => { this.mixer?.drain(); }, 20);

      this.muted = true; this.deafened = false; this.pttMode = false; this.pttHeld = false;
      this.patch({
        phase: 'active', startedAt: Date.now(),
        muted: true, deafened: false, pttMode: false, pttHeld: false,
      });
      // Render the in-call roster the instant media is up (the caller may be the
      // only live beacon until the presence feed arrives).
      this.refreshParticipants();
    } catch (err) {
      // A step after join_group_call failed — tear down any partially-built media
      // and leave the backend room so the session returns to idle instead of
      // stranding in 'connecting'. teardownMedia is best-effort and null-safe.
      await this.teardownMedia(callId);
      this.resetToIdle();
      throw err;
    }
  }

  /**
   * Subscribe to the backend's media-transport lifecycle events. The inbound
   * subscriber re-declares itself with backoff on a transport drop and brackets
   * that with `voice-transport-lost` / `voice-transport-restored` events tagged
   * by callId. We filter to the active call and flip `reconnecting` so the
   * in-call bar can show "Reconnecting…". Torn down with the media engine.
   */
  private async subscribeTransport(): Promise<void> {
    const matches = (p: unknown): boolean =>
      (p as { callId?: string }).callId === this.callId;
    // Track each handle the instant it resolves — if the second listen() rejects,
    // teardown still unsubscribes the first instead of leaking it.
    const unLost = await this.deps.listen('voice-transport-lost', (e) => {
      if (!matches(e.payload)) return;
      this.patch({ reconnecting: true });
    });
    this.unlisteners.push(unLost);
    const unRestored = await this.deps.listen('voice-transport-restored', (e) => {
      if (!matches(e.payload)) return;
      this.patch({ reconnecting: false });
    });
    this.unlisteners.push(unRestored);
  }

  /** Dismantle the media engine + leave the group call room. Best-effort. */
  private async teardownMedia(callId: string | null): Promise<void> {
    if (this.drainTimer) { clearInterval(this.drainTimer); this.drainTimer = null; }
    for (const u of this.unlisteners) u();
    this.unlisteners = [];
    await this.sender?.stop().catch(() => {});
    this.receiver?.destroy();
    await this.mixer?.destroy().catch(() => {});
    this.sender = null; this.receiver = null; this.mixer = null;
    if (callId) {
      await this.deps.invoke('leave_group_call', { callId, spaceId: this.spaceId }).catch(() => {});
    }
  }

  // ---------------------------------------------------------------------------
  // Media controls — PORTED from CallSession (incl. the ZEB-351 rollback), with
  // the IPC swapped to the group call variant. The IPC only fires while the call
  // is connected ('active'); before then the toggles just update local state so
  // the pre-call UI can pre-arm mute/PTT.
  // ---------------------------------------------------------------------------

  async setMuted(muted: boolean): Promise<void> {
    const prev = this.muted;
    this.muted = muted;
    if (muted) this.vad.reset();
    this.patch({ muted });
    if (get(this.store).phase === 'active' && this.callId) {
      try {
        await this.deps.invoke('set_group_call_muted', {
          payload: { callId: this.callId, spaceId: this.spaceId, muted },
        });
      } catch (err) {
        // Backend rejected the toggle — roll the local gate + store back so the
        // UI never advertises a mute state the backend won't reflect.
        this.muted = prev;
        this.patch({ muted: prev });
        throw err;
      }
    }
  }

  /**
   * Switch push-to-talk mode. Entering PTT unmutes (the gate then transmits only
   * while held); leaving re-mutes to the safe default. Async because the mute
   * coupling round-trips set_group_call_muted.
   */
  async setPttMode(on: boolean): Promise<void> {
    const prevMode = this.pttMode;
    const prevHeld = this.pttHeld;
    this.pttMode = on;
    this.patch({ pttMode: on });
    if (!on) this.setPttHeld(false);
    try {
      await this.setMuted(!on);
    } catch (err) {
      // The coupled mute toggle was rejected (setMuted already rolled its own
      // muted state back). Roll BOTH mode and hold back: turning PTT off already
      // forced pttHeld=false above, so without restoring it a failed round-trip
      // would strand the gate in a hold state the rolled-back mode doesn't match.
      this.pttMode = prevMode;
      this.pttHeld = prevHeld;
      this.patch({ pttMode: prevMode, pttHeld: prevHeld });
      throw err;
    }
  }

  setPttHeld(held: boolean): void {
    if (held === this.pttHeld) return;
    this.pttHeld = held;
    this.patch({ pttHeld: held });
  }

  async setDeafened(deaf: boolean): Promise<void> {
    const prevDeafened = this.deafened;
    this.deafened = deaf;
    this.mixer?.setDeafened(deaf);
    this.patch({ deafened: deaf });
    try {
      if (deaf && !this.muted) await this.setMuted(true); // deafen implies self-mute
    } catch (err) {
      // The coupled mute IPC was rejected (setMuted already rolled its own muted
      // state back). Roll the deafen state back too so the UI never advertises a
      // deafened state whose self-mute the backend won't reflect.
      this.deafened = prevDeafened;
      this.mixer?.setDeafened(prevDeafened);
      this.patch({ deafened: prevDeafened });
      throw err;
    }
  }

  /**
   * Dispose this session before it is discarded (e.g. on identity switch, when
   * getGroupCallSession rebuilds the singleton). Clears timers and tears down any
   * live media so a replaced instance never leaves the mic open, the drain/ring
   * timers firing, or media bound to stale IPC handles. Fire-and-forget.
   */
  destroy(): void {
    // Capture call identity BEFORE resetToIdle() nulls it, so we can leave the
    // backend room. An 'incoming' session never joined media (ring toast only),
    // so there's nothing to leave there.
    const phase = get(this.store).phase;
    const callId = this.callId;
    const spaceId = this.spaceId;
    this.clearRingTimer();
    if (this.drainTimer) { clearInterval(this.drainTimer); this.drainTimer = null; }
    for (const u of this.unlisteners) u();
    this.unlisteners = [];
    void this.sender?.stop().catch(() => {});
    this.receiver?.destroy();
    void this.mixer?.destroy().catch(() => {});
    this.sender = null; this.receiver = null; this.mixer = null;
    // Fire-and-forget leave so an identity switch that rebuilds the singleton
    // drops group presence immediately instead of lingering to the TTL.
    if (callId && phase !== 'incoming') {
      void this.deps.invoke('leave_group_call', { callId, spaceId }).catch(() => {});
    }
    this.resetToIdle();
  }
}

/**
 * Per-identity singleton. There is at most one active group call per process, so
 * the app shares a single GroupCallSession. It is rebuilt when the identity
 * changes (e.g. the user switches identity via stop_node / start_node) so the
 * session never signs signals or joins media rooms with a previous identity's
 * keys or IPC handles; calls with the same identity return the cached instance.
 */
let _groupCallSingleton: GroupCallSession | null = null;
let _groupCallIdentity: string | null = null;
export function getGroupCallSession(deps: GroupCallSessionDeps): GroupCallSession {
  const identity = `${deps.selfOwnerHex}:${deps.selfDeviceHex}`;
  if (!_groupCallSingleton || _groupCallIdentity !== identity) {
    _groupCallSingleton?.destroy();
    _groupCallSingleton = new GroupCallSession(deps);
    _groupCallIdentity = identity;
  }
  return _groupCallSingleton;
}
