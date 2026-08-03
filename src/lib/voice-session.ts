// src/lib/voice-session.ts
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

export type SessionPhase = 'idle' | 'joining' | 'connected' | 'leaving';

/**
 * Soft cap on community voice-channel participants. The roster is decentralized
 * — a joining client only learns the count AFTER it subscribes to presence — so
 * this is a best-effort REACTIVE bounce, not a blocking pre-join gate. A join
 * with 64 OTHER members already present (you'd be the 65th) is refused shortly
 * after the roster arrives; the common under-cap join pays zero added latency.
 */
export const VOICE_CHANNEL_SOFT_CAP = 64;

export interface RosterMember {
  ownerHex: string;     // 32 hex
  deviceHex: string;
  muted: boolean;
  speaking: boolean;    // derived (Task 5)
  displayName?: string; // resolved from member card (Task 5)
  avatarUrl?: string;   // resolved from member card (Task 5)
  modMuted: boolean;    // server-muted by a moderator (distinct from self-mute `muted`)
  power: number;        // moderation power level (for FE control gating)
  /** ZEB-612: peer-supplied wall-clock ms of this member's raised hand; null =
   *  lowered. Display-only; the speaker queue orders on `handFirstObservedMs`. */
  handRaisedAt: number | null;
  /** ZEB-853 D5: LOCAL first-observed monotonic ms of this member's raised hand
   *  (null = lowered). The peer `handRaisedAt` is attacker-controlled, so the
   *  speaker queue orders on THIS (DoS-proof). When it is absent the queue sorts
   *  the entry LAST — it NEVER falls back to the peer `handRaisedAt` value (that
   *  reopens the epoch-squat D5 fixes). */
  handFirstObservedMs: number | null;
  /** ZEB-612: holds an unexpired invite-to-speak from a moderator. */
  invited: boolean;
}

/**
 * ZEB-612 / ZEB-853 D5: the derived speaker queue — raised hands ordered by
 * LOCAL first-observed time (oldest first), owner-hex tiebreak, device-hex final
 * tiebreak (the roster is device-keyed, so two rows CAN share an owner — the
 * device leg makes the comparator total, satisfying the sort contract; Qodo
 * PR #442). Deterministic on every client; no store, no sync protocol (spec §5).
 * Exported for TownHallView (S5).
 *
 * The FILTER still keys on `handRaisedAt` (a peer must assert intent to be
 * queued), but the SORT keys on `handFirstObservedMs` — the local monotonic
 * stamp taken when we first saw the raise — so a hostile peer that forges
 * `handRaisedAt=1` (epoch) can't squat the front of the queue. An entry that is
 * raised but has no local stamp yet sorts LAST (`Number.MAX_SAFE_INTEGER`),
 * never using the attacker-controlled peer value as a fallback.
 */
export function speakerQueue(roster: RosterMember[]): RosterMember[] {
  const cmp = (a: string, b: string) => (a < b ? -1 : a > b ? 1 : 0);
  // Missing local stamp ⇒ sort LAST; do NOT fall back to the peer-controlled
  // `handRaisedAt` (a forged epoch there would re-squat the queue front).
  const key = (m: RosterMember) => m.handFirstObservedMs ?? Number.MAX_SAFE_INTEGER;
  return roster
    .filter((m) => m.handRaisedAt !== null)
    .sort(
      (a, b) =>
        key(a) - key(b) ||
        cmp(a.ownerHex, b.ownerHex) ||
        cmp(a.deviceHex, b.deviceHex),
    );
}

export interface VoiceSessionState {
  phase: SessionPhase;
  community: string | null;
  channel: string | null;
  muted: boolean;
  deafened: boolean;
  pttMode: boolean;
  /** Push-to-talk hold state — true only while the talk control is held. */
  pttHeld: boolean;
  roster: RosterMember[];
  /**
   * Set true when a join was reactively bounced because the channel was at/over
   * the soft cap (VOICE_CHANNEL_SOFT_CAP others already present). The session is
   * back at idle-phase; the join pane surfaces this so the user knows why. Reset
   * to false on the next join attempt.
   */
  channelFull: boolean;
  /**
   * True while the inbound media subscriber is re-establishing after a transport
   * drop (the backend emits `voice-transport-lost`/`voice-transport-restored`
   * around its re-declare-with-backoff loop). Surfaces a "Reconnecting…" badge.
   */
  reconnecting: boolean;
  /**
   * Set true when the mic capture couldn't start because the microphone
   * permission was denied or no input device exists. We DON'T tear the join
   * down for this — the mixer + receiver are already up, so we join listen-only
   * (no sender) and surface a persistent "Mic blocked — listening only" note.
   * Reset to false on the next join attempt.
   */
  micBlocked: boolean;
  selfPower: number;      // this node's moderation power in the active channel
  selfModMuted: boolean;  // this node is currently server-muted by a moderator
  selfKicked: boolean;    // this node is currently kicked from voice by a moderator
  /** ZEB-612: this node holds an unexpired invite-to-speak (drives the
   *  "You've been invited to speak — Unmute?" banner in TownHallView). */
  selfInvited: boolean;
}

type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
type Listen = (ev: string, h: (e: { payload: unknown }) => void) => Promise<() => void>;
type FrameGate = (pcm: Float32Array) => { send: boolean; ptt: boolean };

/** Lowercase hex-encode a byte array (16-byte senderHash → 32 hex chars). */
function bytesToHex(b: Uint8Array): string {
  return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
}

/** Shallow per-member equality so refreshRoster only patches the store on real change. */
function rostersEqual(a: RosterMember[], b: RosterMember[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    const x = a[i], y = b[i];
    if (
      x.ownerHex !== y.ownerHex || x.deviceHex !== y.deviceHex ||
      x.muted !== y.muted || x.speaking !== y.speaking ||
      x.displayName !== y.displayName || x.avatarUrl !== y.avatarUrl ||
      x.modMuted !== y.modMuted || x.power !== y.power ||
      x.handRaisedAt !== y.handRaisedAt ||
      x.handFirstObservedMs !== y.handFirstObservedMs || x.invited !== y.invited
    ) {
      return false;
    }
  }
  return true;
}

export interface VoiceSessionDeps {
  invoke: Invoke;
  listen: Listen;
  selfOwnerHex: string;
  selfDeviceHex: string;
  /**
   * 16-byte sender hash stamped into every outbound voice packet header; the
   * receiver keys (and self-filters) senders by its hex.
   *
   * INVARIANT: this MUST be the first 16 bytes of this node's 32-byte device id
   * (`selfDeviceHex`). The presence roster keys members by the full 32-byte
   * device (64 hex); the receiver keys speaking senders by this 16-byte hash
   * (32 hex). Correlating a roster member to "is speaking" relies on
   * `device.slice(0, 32) === hex(senderHash)`, which only holds when senderHash
   * is the device prefix. The app wiring (getVoiceSession) MUST build it so.
   */
  senderHash: Uint8Array;
  vadThreshold?: number;
  // Factories (injected in tests; real defaults below).
  makeSender?: (gate: FrameGate) => Pick<VoiceSender, 'start' | 'stop' | 'switchInputDevice'>;
  makeReceiver?: () => Pick<VoiceReceiver, 'init' | 'destroy' | 'getActiveSenders' | 'isSpeaking'>;
  makeMixer?: () => Pick<VoiceMixer, 'init' | 'pushFrame' | 'drain' | 'setDeafened' | 'destroy' | 'setOutputDevice'>;
  /** ZEB-359: device preference source. Absent ⇒ system defaults, no following. */
  audioDevices?: DeviceFollowerDeps['prefs'];
  /** Resolve an owner hex → { displayName, avatarUrl } for tiles (optional). */
  resolveCard?: (ownerHex: string) => { displayName?: string; avatarUrl?: string } | undefined;
  /** Subscribe/refresh member cards for visible roster owners (optional). */
  onRosterOwners?: (ownerHexes: string[]) => void;
}

const INITIAL: VoiceSessionState = {
  phase: 'idle', community: null, channel: null,
  muted: true, deafened: false, pttMode: false, pttHeld: false, roster: [],
  channelFull: false, reconnecting: false, micBlocked: false,
  selfPower: 0, selfModMuted: false, selfKicked: false, selfInvited: false,
};

/**
 * Classify a `sender.start()` rejection as a recoverable microphone error.
 * A mic permission/device fault should not tear the whole join down — the user
 * can still LISTEN — so we match the standard `getUserMedia` `DOMException`
 * names (and a message fallback for non-DOMException test/polyfill errors):
 *
 *  - `'blocked'`  → permission denied / blocked by policy. The user can grant it
 *                   and retry.
 *  - `'notfound'` → no usable input device (unplugged, or constraints unmet).
 *  - `null`       → not a mic error; the caller rethrows for full rollback.
 */
export function classifyMicError(e: unknown): 'blocked' | 'notfound' | null {
  const name = (e as { name?: unknown })?.name;
  const nameStr = typeof name === 'string' ? name : '';
  const msg = e instanceof Error ? e.message
    : typeof (e as { message?: unknown })?.message === 'string'
      ? (e as { message: string }).message
      : String(e ?? '');
  if (
    nameStr === 'NotAllowedError' || nameStr === 'PermissionDeniedError' ||
    nameStr === 'SecurityError' || /permission|notallowed|denied/i.test(msg)
  ) {
    return 'blocked';
  }
  if (
    nameStr === 'NotFoundError' || nameStr === 'DevicesNotFoundError' ||
    nameStr === 'OverconstrainedError' ||
    // `\bno\b` (whole word) so "Cannot initialize audio context" / "unknown" /
    // "connection" don't false-match "no" as a substring and misroute a generic
    // system error into the silent listen-only path.
    /notfound|requested device|\bno\b[^.]*\b(microphone|audio|device)\b/i.test(msg)
  ) {
    return 'notfound';
  }
  return null;
}

export class VoiceSession {
  readonly state: Readable<VoiceSessionState>;
  private store = writable<VoiceSessionState>({ ...INITIAL });
  private deps: VoiceSessionDeps;

  private vad: VoiceActivityDetector;
  private sender: Pick<VoiceSender, 'start' | 'stop' | 'switchInputDevice'> | null = null;
  private receiver: Pick<VoiceReceiver, 'init' | 'destroy' | 'getActiveSenders' | 'isSpeaking'> | null = null;
  private mixer: Pick<VoiceMixer, 'init' | 'pushFrame' | 'drain' | 'setDeafened' | 'destroy' | 'setOutputDevice'> | null = null;

  private muted = true;
  private deafened = false;
  private pttMode = false;
  private pttHeld = false;
  private community: string | null = null;
  private channel: string | null = null;
  private unlisteners: (() => void)[] = [];
  private drainTimer: ReturnType<typeof setInterval> | null = null;
  /** Set while join() is in flight so leave() can serialize behind it. */
  private joinPromise: Promise<void> | null = null;
  /**
   * Monotonic deadline (Date.now ms) until which a fresh join is still eligible
   * for the soft-cap bounce. Set on a successful connect; cleared once we bounce
   * or the window lapses. Outside the window an over-cap roster is tolerated
   * (you were already admitted; don't kick a settled participant).
   */
  private joinGraceUntilMs = 0;
  /** Last roster pushed to the store, for the change-only patch guard. */
  private lastEmittedRoster: RosterMember[] = [];

  constructor(deps: VoiceSessionDeps) {
    this.deps = deps;
    this.state = this.store;
    this.vad = new VoiceActivityDetector({ threshold: deps.vadThreshold ?? 0.02 });
    // Built here (not as a field initializer) so `this.vad` is already assigned.
    this.coreGate = makeTalkGate(
      () => ({ muted: this.muted || this.deafened, pttMode: this.pttMode, pttHeld: this.pttHeld }),
      this.vad,
    );
  }

  private patch(p: Partial<VoiceSessionState>): void {
    this.store.update((s) => ({ ...s, ...p }));
  }

  /**
   * Clear the soft-cap "voice channel full" banner. `channelFull` lives on this
   * app-wide singleton, so a bounce on one channel would otherwise leak its
   * banner onto a different channel's view — callers clear it on navigation.
   */
  clearChannelFull(): void {
    if (get(this.store).channelFull) this.patch({ channelFull: false });
  }

  /**
   * The per-frame send decision (mute / PTT / VAD). The core muted / PTT / VAD
   * logic lives in the shared `makeTalkGate` (so the DM CallSession can't drift
   * from it); here we fold deafen into the muted bit (deafen implies self-mute)
   * and layer the self-speaking side-effect that drives the roster's speaking
   * indicator on top of the pure result.
   */
  private coreGate: FrameGate;
  private gate: FrameGate = (pcm) => {
    const decision = this.coreGate(pcm);
    this.setSelfSpeaking(decision.send);
    return decision;
  };

  private setSelfSpeaking(v: boolean): void {
    if (v !== this.lastSelfSpeaking) { this.lastSelfSpeaking = v; this.refreshRoster(); }
  }

  private lastSelfSpeaking = false;
  private lastRoster: {
    ownerHex: string; deviceHex: string; muted: boolean;
    handRaisedAt: number | null; handFirstObservedMs: number | null;
  }[] = [];

  private modMutedOwners = new Set<string>();
  private kickedOwners = new Set<string>();
  private invitedOwners = new Set<string>();
  private powers: Record<string, number> = {};
  private selfModMuted = false;
  private selfKicked = false;

  async join(community: string, channel: string): Promise<void> {
    if (get(this.store).phase !== 'idle') {
      throw new Error('A voice session is already active');
    }
    // Track the in-flight join so a racing leave() serializes behind it rather
    // than tearing down half-built state (dangling drain timer / live sender).
    this.joinPromise = this._doJoin(community, channel);
    try {
      await this.joinPromise;
    } finally {
      this.joinPromise = null;
    }
  }

  private async _doJoin(community: string, channel: string): Promise<void> {
    this.community = community;
    this.channel = channel;
    this.muted = true; this.deafened = false; this.pttMode = false; this.pttHeld = false;
    this.vad.reset();
    this.lastRoster = []; this.lastEmittedRoster = []; this.lastSelfSpeaking = false;
    this.joinGraceUntilMs = 0;
    this.patch({
      phase: 'joining', community, channel,
      muted: true, deafened: false, pttMode: false, pttHeld: false, roster: [],
      channelFull: false, // fresh attempt clears any prior bounce banner
      reconnecting: false, // fresh attempt clears any stale reconnect badge
      micBlocked: false, // fresh attempt clears any prior listen-only note
    });

    // Transactional: once the backend join succeeds we own teardown for every
    // subsequent step. If any of them rejects — most realistically
    // `sender.start()` on a denied-microphone permission, but also mixer/
    // receiver init or the presence subscribe — we must dismantle the partial
    // state (open AudioContext, live subscriber, 20 ms drain timer), best-effort
    // leave the backend channel, and reset to idle so the join pane re-renders
    // for a clean retry rather than wedging the UI in 'joining' forever.
    let backendJoined = false;
    try {
      // Backend join (spawns subscribers + presence publisher, starts muted).
      await this.deps.invoke('join_voice_channel', { communityId: community, channelId: channel });
      backendJoined = true;

      // Build engine pieces.
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
            // Self-echo filter: the receiver keys senders by hex(senderHash), so
            // filter on the actual senderHash we stamp into packets — NOT a slice
            // of the device id (correct only because senderHash = device[0..16]).
            ownSenderHex: bytesToHex(this.deps.senderHash),
          });
      await this.receiver.init();

      // Isolate the sender build+start. Mixer + receiver are already up above, so
      // a mic permission/device failure here doesn't have to abort the join — we
      // can still LISTEN. Only a non-mic fault rethrows into the outer catch for
      // the full transactional rollback (unchanged behavior).
      try {
        this.sender = this.deps.makeSender
          ? this.deps.makeSender(this.gate)
          : new VoiceSender({
              senderHash: this.deps.senderHash, communityId: community, channelId: channel,
              invoke: this.deps.invoke, codec: new OpusCodec(), capture: new AudioCapture(),
              frameGate: this.gate,
              inputDeviceId: () => this.deps.audioDevices?.getInput() ?? null,
            });
        await this.sender.start();   // capture starts; muted gate ⇒ nothing transmits
      } catch (senderErr) {
        if (classifyMicError(senderErr) === null) throw senderErr; // non-mic → full rollback
        // Mic blocked / no device: drop the (possibly half-started) sender and
        // continue listen-only. The gate just has no sender to publish through;
        // the drain/mixer/receiver path below is unaffected.
        await this.sender?.stop().catch(() => {});
        this.sender = null;
        this.patch({ micBlocked: true });
      }

      // Drain the mixer every 20ms (audio cadence). Refresh the roster view at a
      // far lower rate (~10Hz) — the drain-tick refresh only updates remote
      // speaking state, and refreshRoster() is a no-op unless the view actually
      // changed (diff guard), so this avoids a 50Hz Svelte re-render storm.
      let drainTick = 0;
      this.drainTimer = setInterval(() => {
        this.mixer?.drain();
        if (++drainTick % 5 === 0) this.refreshRoster();
      }, 20);

      await this.subscribePresence();
      await this.subscribeTransport();
      await this.subscribeModeration();

      // ZEB-359: follow device-pref / hot-plug changes for the session's
      // lifetime. Unfollowed with the other unlisteners in cleanupEngine.
      if (this.deps.audioDevices) {
        this.unlisteners.push(
          followAudioDevices({
            prefs: this.deps.audioDevices,
            getSender: () => this.sender,
            getMixer: () => this.mixer,
            onMicError: (e) => {
              // Mirror the join-time mic-error path: drop to listen-only
              // rather than tearing the whole session down. The sender has
              // already deactivated itself on the failed restart.
              const msg = e instanceof Error ? e.message : String(e);
              console.warn('voice input switch failed; listen-only:', msg);
              void this.sender?.stop().catch(() => {});
              this.sender = null;
              this.patch({ micBlocked: true });
            },
          }),
        );
      }

      // Open the soft-cap grace window: an over-cap roster arriving within the
      // next few seconds bounces this join (see maybeBounceForFull). The window
      // covers both the first presence event and the 10Hz drain-tick refreshes.
      this.joinGraceUntilMs = Date.now() + 3000;

      this.patch({ phase: 'connected' });
    } catch (err) {
      await this.cleanupEngine(backendJoined);
      this.store.set({ ...INITIAL });
      throw err;
    }
  }

  async setMuted(muted: boolean): Promise<void> {
    // A moderator server-mute / kick blocks the target from self-unmuting.
    if (muted === false && (this.selfModMuted || this.selfKicked)) return;
    const prev = this.muted;
    this.muted = muted;
    if (muted) this.vad.reset();
    this.patch({ muted });
    if (this.community && this.channel) {
      try {
        await this.deps.invoke('set_voice_muted',
          { payload: { communityId: this.community, channelId: this.channel, muted } });
      } catch (err) {
        // Backend rejected the toggle — roll the local gate + store back so the
        // UI never advertises a mute state the presence roster won't reflect
        // (and the send gate can't silently disagree with the backend flag).
        this.muted = prev;
        this.patch({ muted: prev });
        throw err;
      }
    }
  }

  /** Force the local mic muted and best-effort publish it, WITHOUT rolling back
   *  on IPC failure — a moderator server-mute/kick must stick locally even if
   *  the presence beacon publish transiently fails. */
  private forceLocalMute(): void {
    this.muted = true;
    this.patch({ muted: true });
    if (this.community && this.channel) {
      void this.deps
        .invoke('set_voice_muted', { payload: { communityId: this.community, channelId: this.channel, muted: true } })
        .catch(() => {});
    }
  }

  /**
   * Issue a moderation directive against another participant (requires power).
   * Backend re-verifies the moderator's power before broadcasting.
   */
  async moderate(
    targetOwnerHex: string,
    action: 'mute' | 'unmute' | 'kick' | 'unkick' | 'invite',
  ): Promise<void> {
    if (!this.community || !this.channel) return;
    await this.deps.invoke('moderate_voice', {
      payload: { communityId: this.community, channelId: this.channel, targetOwnerHex, action },
    });
  }

  /**
   * ZEB-612 Town Hall: raise/lower our hand. The backend keeps the FIRST
   * raise's timestamp across repeat raises (stable queue position) and
   * republishes it on every presence heartbeat. No-op unless connected.
   */
  async setHand(raised: boolean): Promise<void> {
    if (!this.community || !this.channel) return;
    await this.deps.invoke('set_voice_hand', {
      payload: { communityId: this.community, channelId: this.channel, raised },
    });
  }

  /**
   * ZEB-612 Town Hall: invite `targetOwnerHex` to speak (benign directive,
   * power-gated ≥50 backend-side; an unclaimed invite lapses in ~2 min).
   * The TARGET's own client lowers its hand on accept/dismiss.
   */
  async inviteToSpeak(targetOwnerHex: string): Promise<void> {
    await this.moderate(targetOwnerHex, 'invite');
  }

  /**
   * Switch push-to-talk mode. PTT makes the mic "available but gated by hold":
   * entering PTT unmutes (the gate then transmits only while held) and leaving
   * it re-mutes to the safe default. Async because the mute coupling round-trips
   * `set_voice_muted`.
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
      // The coupled mute toggle was rejected by the backend (setMuted already
      // rolled its own muted state back). Roll BOTH mode and hold back: turning
      // PTT off already forced pttHeld=false above, so without restoring it a
      // failed round-trip would strand the gate in a hold state the rolled-back
      // mode doesn't match.
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
    this.deafened = deaf;
    this.mixer?.setDeafened(deaf);
    this.patch({ deafened: deaf });
    if (deaf && !this.muted) await this.setMuted(true);   // deafen implies self-mute
  }

  async leave(): Promise<void> {
    // Serialize behind an in-flight join so we never tear down half-built state.
    if (this.joinPromise) await this.joinPromise.catch(() => {});
    if (get(this.store).phase === 'idle') return; // nothing to tear down (also guards double-leave)
    this.patch({ phase: 'leaving' });
    await this.cleanupEngine(true);
    this.store.set({ ...INITIAL });
  }

  /**
   * ZEB-354: dispose this session before it is discarded (e.g. on identity
   * switch, when getVoiceSession rebuilds the singleton). Tears down any live
   * channel media, timers, and listeners so a replaced instance never leaves the
   * mic open, drains audio, or holds media bound to a previous identity's IPC
   * handles. Fire-and-forget (mirrors CallSession / GroupCallSession.destroy);
   * delegates to leave(), which is a no-op when already idle.
   */
  destroy(): void {
    void this.leave().catch(() => {});
  }

  /**
   * Dismantle every engine piece, timer, and listener and clear the connection
   * fields. Shared by leave() and the _doJoin() failure path. `invokeLeave`
   * gates the `leave_voice_channel` IPC so the join-failure path only calls it
   * when the backend actually joined. Best-effort throughout — never throws, so
   * a teardown fault can't mask the original error or wedge the session.
   */
  private async cleanupEngine(invokeLeave: boolean): Promise<void> {
    if (this.drainTimer) { clearInterval(this.drainTimer); this.drainTimer = null; }
    for (const u of this.unlisteners) u();
    this.unlisteners = [];
    await this.sender?.stop().catch(() => {});
    this.receiver?.destroy();
    await this.mixer?.destroy().catch(() => {});
    this.sender = null; this.receiver = null; this.mixer = null;
    const community = this.community, channel = this.channel;
    this.community = null; this.channel = null;
    this.lastRoster = []; this.lastEmittedRoster = []; this.lastSelfSpeaking = false;
    this.modMutedOwners = new Set(); this.kickedOwners = new Set(); this.powers = {};
    this.invitedOwners = new Set();
    this.selfModMuted = false; this.selfKicked = false;
    if (invokeLeave && community && channel) {
      await this.deps.invoke('leave_voice_channel', { communityId: community, channelId: channel }).catch(() => {});
    }
  }

  protected async subscribePresence(): Promise<void> {
    const un = await this.deps.listen('voice-presence-changed', (e) => {
      const p = e.payload as { community: string; channel: string;
        roster: { owner: string; device: string; muted: boolean;
          handRaisedAt?: number | null; handFirstObservedMs?: number | null }[] };
      if (p.community !== this.community || p.channel !== this.channel) return;
      this.lastRoster = p.roster.map((r) => ({
        ownerHex: r.owner, deviceHex: r.device, muted: r.muted,
        handRaisedAt: r.handRaisedAt ?? null,
        handFirstObservedMs: r.handFirstObservedMs ?? null,
      }));
      this.deps.onRosterOwners?.(this.lastRoster.map((r) => r.ownerHex));
      this.refreshRoster();
    });
    this.unlisteners.push(un);
  }

  /**
   * Subscribe to the backend's media-transport lifecycle events. The inbound
   * media subscriber re-declares itself with backoff on a transport drop and
   * brackets that with `voice-transport-lost` / `voice-transport-restored`
   * events tagged by community+channel. We filter to the active channel and flip
   * `reconnecting` so the UI can show "Reconnecting…". Torn down with the rest of
   * the unlisteners on leave (cleanupEngine).
   */
  protected async subscribeTransport(): Promise<void> {
    const matches = (p: unknown): boolean => {
      const e = p as { communityId?: string; channelId?: string };
      return e.communityId === this.community && e.channelId === this.channel;
    };
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

  protected async subscribeModeration(): Promise<void> {
    const un = await this.deps.listen('voice-moderation-changed', (e) => {
      const p = e.payload as {
        community: string; channel: string;
        mutedOwners: string[]; kickedOwners: string[];
        invitedOwners?: string[];
        powers: Record<string, number>;
        selfPower: number; selfModMuted: boolean; selfKicked: boolean;
        selfInvited?: boolean;
      };
      if (p.community !== this.community || p.channel !== this.channel) return;
      this.modMutedOwners = new Set(p.mutedOwners);
      this.kickedOwners = new Set(p.kickedOwners);
      this.invitedOwners = new Set(p.invitedOwners ?? []);
      this.powers = p.powers ?? {};
      const wasSilenced = this.selfModMuted || this.selfKicked;
      this.selfModMuted = p.selfModMuted;
      this.selfKicked = p.selfKicked;
      const nowSilenced = this.selfModMuted || this.selfKicked;
      this.patch({
        selfPower: p.selfPower, selfModMuted: p.selfModMuted, selfKicked: p.selfKicked,
        selfInvited: p.selfInvited ?? false,
      });
      // On ANY transition of the silenced state, force local self-mute. On a
      // LIFT (nowSilenced=false, wasSilenced=true) this keeps the mic muted —
      // the unattended-mic guard: a server-mute lapsing must NOT auto-resume
      // transmitting; the user opts back in explicitly. This INTENTIONALLY
      // extends the spec's D3 guard to the kick class too: an unkick (or a
      // kick's TTL expiry) is a reinstatement, not a presence signal, so we
      // don't know the user is back at the mic — keep them muted until they
      // choose to talk again (Greptile P2).
      if (nowSilenced !== wasSilenced) {
        this.forceLocalMute();
      }
      this.refreshRoster();
    });
    this.unlisteners.push(un);
  }

  /** Recompute roster view (speaking + card resolution). Call on presence + each drain. */
  private refreshRoster(): void {
    const roster: RosterMember[] = this.lastRoster
      .filter((r) => !this.kickedOwners.has(r.ownerHex))
      .map((r) => {
        const isSelf = r.deviceHex.slice(0, 32) === this.deps.selfDeviceHex.slice(0, 32);
        const speaking = isSelf
          ? (!this.muted && !this.deafened && this.lastSelfSpeaking)
          : (this.receiver?.isSpeaking(r.deviceHex.slice(0, 32)) ?? false);
        const card = this.deps.resolveCard?.(r.ownerHex);
        return {
          ownerHex: r.ownerHex, deviceHex: r.deviceHex, muted: r.muted, speaking,
          modMuted: this.modMutedOwners.has(r.ownerHex),
          power: this.powers[r.ownerHex] ?? 0,
          handRaisedAt: r.handRaisedAt,
          handFirstObservedMs: r.handFirstObservedMs,
          invited: this.invitedOwners.has(r.ownerHex),
          ...(card?.displayName ? { displayName: card.displayName } : {}),
          ...(card?.avatarUrl ? { avatarUrl: card.avatarUrl } : {}),
        } as RosterMember;
      });
    // Only touch the store when the view actually changed — the drain tick calls
    // this ~10×/s, but most ticks produce an identical roster.
    if (!rostersEqual(roster, this.lastEmittedRoster)) {
      this.lastEmittedRoster = roster;
      this.patch({ roster });
    }
    // Run the soft-cap check on every refresh (presence events AND drain ticks),
    // so a roster that arrives during the grace window bounces us even if the
    // roster view itself was unchanged by the diff guard above.
    this.maybeBounceForFull();
  }

  /**
   * Soft-cap reactive bounce. If we're connected, still inside the post-join
   * grace window, and the channel already holds VOICE_CHANNEL_SOFT_CAP OTHER
   * members (we'd be the (cap+1)th), refuse the join: tear down and surface
   * `channelFull` so the join pane re-renders with the reason.
   *
   * Ordering is the subtle part: leave() resets the store to INITIAL (which has
   * channelFull:false), so we must stamp channelFull:true AFTER leave() settles,
   * not before — otherwise the reset clobbers the banner. We also clear the
   * grace deadline up front so a racing drain tick can't fire a second bounce.
   */
  private maybeBounceForFull(): void {
    if (get(this.store).phase !== 'connected') return;
    if (Date.now() >= this.joinGraceUntilMs) return;
    // Count OTHERS using the same self-identification refreshRoster uses, and
    // EXCLUDE kicked owners (Cursor Medium): a kicked member is presence-evicted
    // and must not inflate the count into a false "channel full" bounce.
    const selfPrefix = this.deps.selfDeviceHex.slice(0, 32);
    const others = this.lastRoster.reduce(
      (n, r) =>
        n +
        (r.deviceHex.slice(0, 32) === selfPrefix || this.kickedOwners.has(r.ownerHex)
          ? 0
          : 1),
      0,
    );
    if (others < VOICE_CHANNEL_SOFT_CAP) return;
    this.joinGraceUntilMs = 0; // one-shot: don't let the drain tick re-bounce
    // leave() reset the store to INITIAL (channelFull:false); stamp the banner
    // afterward so it survives, leaving idle-phase + channelFull:true for the UI.
    // Stamp on BOTH settle paths so a rejecting leave() still surfaces the reason
    // (and never leaks an unhandled rejection).
    const stampFull = () => this.patch({ channelFull: true });
    void this.leave().then(stampFull, stampFull);
  }
}

/**
 * Per-identity singleton. There is at most one active voice session per process
 * (joining a second channel throws while one is connected), so the app shares a
 * single VoiceSession. It is rebuilt when the identity changes (e.g. the user
 * switches identity via stop_node / start_node without a process restart) so the
 * session never signs presence/voice packets or holds media bound to a previous
 * identity's keys, sealed-relay deps, or IPC handles; calls with the same
 * identity return the cached instance (ZEB-354 — mirrors getCallSession /
 * getGroupCallSession).
 */
let _voiceSessionSingleton: VoiceSession | null = null;
let _voiceSessionIdentity: string | null = null;
export function getVoiceSession(deps: VoiceSessionDeps): VoiceSession {
  const identity = `${deps.selfOwnerHex}:${deps.selfDeviceHex}`;
  if (!_voiceSessionSingleton || _voiceSessionIdentity !== identity) {
    // Tear down the outgoing instance so its media/timers/listeners don't keep
    // running on the previous identity's handles after the swap.
    _voiceSessionSingleton?.destroy();
    _voiceSessionSingleton = new VoiceSession(deps);
    _voiceSessionIdentity = identity;
  }
  return _voiceSessionSingleton;
}
