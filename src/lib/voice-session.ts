// src/lib/voice-session.ts
import { writable, get, type Readable } from 'svelte/store';
import { VoiceActivityDetector } from './voice/vad';
import { VoiceSender } from './voice/voice-sender';
import { VoiceReceiver } from './voice/voice-receiver';
import { VoiceMixer } from './voice/voice-mixer';
import { AudioCapture } from './voice/audio-capture';
import { OpusCodec } from './voice/opus-codec';
import { Codec2Codec } from './voice/codec2-codec';
import type { CodecType } from './voice/voice-codec';

export type SessionPhase = 'idle' | 'joining' | 'connected' | 'leaving';

export interface RosterMember {
  ownerHex: string;     // 32 hex
  deviceHex: string;
  muted: boolean;
  speaking: boolean;    // derived (Task 5)
  displayName?: string; // resolved from member card (Task 5)
  avatarUrl?: string;   // resolved from member card (Task 5)
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
      x.displayName !== y.displayName || x.avatarUrl !== y.avatarUrl
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
  makeSender?: (gate: FrameGate) => Pick<VoiceSender, 'start' | 'stop'>;
  makeReceiver?: () => Pick<VoiceReceiver, 'init' | 'destroy' | 'getActiveSenders' | 'isSpeaking'>;
  makeMixer?: () => Pick<VoiceMixer, 'init' | 'pushFrame' | 'drain' | 'setDeafened' | 'destroy'>;
  /** Resolve an owner hex → { displayName, avatarUrl } for tiles (optional). */
  resolveCard?: (ownerHex: string) => { displayName?: string; avatarUrl?: string } | undefined;
  /** Subscribe/refresh member cards for visible roster owners (optional). */
  onRosterOwners?: (ownerHexes: string[]) => void;
}

const INITIAL: VoiceSessionState = {
  phase: 'idle', community: null, channel: null,
  muted: true, deafened: false, pttMode: false, pttHeld: false, roster: [],
};

export class VoiceSession {
  readonly state: Readable<VoiceSessionState>;
  private store = writable<VoiceSessionState>({ ...INITIAL });
  private deps: VoiceSessionDeps;

  private vad: VoiceActivityDetector;
  private sender: Pick<VoiceSender, 'start' | 'stop'> | null = null;
  private receiver: Pick<VoiceReceiver, 'init' | 'destroy' | 'getActiveSenders' | 'isSpeaking'> | null = null;
  private mixer: Pick<VoiceMixer, 'init' | 'pushFrame' | 'drain' | 'setDeafened' | 'destroy'> | null = null;

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
  /** Last roster pushed to the store, for the change-only patch guard. */
  private lastEmittedRoster: RosterMember[] = [];

  constructor(deps: VoiceSessionDeps) {
    this.deps = deps;
    this.state = this.store;
    this.vad = new VoiceActivityDetector({ threshold: deps.vadThreshold ?? 0.02 });
  }

  private patch(p: Partial<VoiceSessionState>): void {
    this.store.update((s) => ({ ...s, ...p }));
  }

  /** The per-frame send decision (mute / PTT / VAD). */
  private gate: FrameGate = (pcm) => {
    if (this.muted || this.deafened) { this.setSelfSpeaking(false); return { send: false, ptt: false }; }
    if (this.pttMode) { this.setSelfSpeaking(this.pttHeld); return { send: this.pttHeld, ptt: true }; }
    const speaking = this.vad.process(pcm);
    this.setSelfSpeaking(speaking);
    return { send: speaking, ptt: speaking };
  };

  private setSelfSpeaking(v: boolean): void {
    if (v !== this.lastSelfSpeaking) { this.lastSelfSpeaking = v; this.refreshRoster(); }
  }

  private lastSelfSpeaking = false;
  private lastRoster: { ownerHex: string; deviceHex: string; muted: boolean }[] = [];

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
    this.patch({
      phase: 'joining', community, channel,
      muted: true, deafened: false, pttMode: false, pttHeld: false, roster: [],
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
      this.mixer = this.deps.makeMixer ? this.deps.makeMixer() : new VoiceMixer();
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

      this.sender = this.deps.makeSender
        ? this.deps.makeSender(this.gate)
        : new VoiceSender({
            senderHash: this.deps.senderHash, communityId: community, channelId: channel,
            invoke: this.deps.invoke, codec: new OpusCodec(), capture: new AudioCapture(),
            frameGate: this.gate,
          });
      await this.sender.start();   // capture starts; muted gate ⇒ nothing transmits

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

      this.patch({ phase: 'connected' });
    } catch (err) {
      await this.cleanupEngine(backendJoined);
      this.store.set({ ...INITIAL });
      throw err;
    }
  }

  async setMuted(muted: boolean): Promise<void> {
    const prev = this.muted;
    this.muted = muted;
    if (muted) this.vad.reset();
    this.patch({ muted });
    if (this.community && this.channel) {
      try {
        await this.deps.invoke('set_voice_muted',
          { communityId: this.community, channelId: this.channel, muted });
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

  /**
   * Switch push-to-talk mode. PTT makes the mic "available but gated by hold":
   * entering PTT unmutes (the gate then transmits only while held) and leaving
   * it re-mutes to the safe default. Async because the mute coupling round-trips
   * `set_voice_muted`.
   */
  async setPttMode(on: boolean): Promise<void> {
    const prevMode = this.pttMode;
    this.pttMode = on;
    this.patch({ pttMode: on });
    if (!on) this.setPttHeld(false);
    try {
      await this.setMuted(!on);
    } catch (err) {
      // The coupled mute toggle was rejected by the backend (setMuted already
      // rolled its own muted state back). Roll pttMode back too so mode and
      // mute stay consistent instead of stranding the UI in "PTT on but muted".
      this.pttMode = prevMode;
      this.patch({ pttMode: prevMode });
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
    if (invokeLeave && community && channel) {
      await this.deps.invoke('leave_voice_channel', { communityId: community, channelId: channel }).catch(() => {});
    }
  }

  protected async subscribePresence(): Promise<void> {
    const un = await this.deps.listen('voice-presence-changed', (e) => {
      const p = e.payload as { community: string; channel: string;
        roster: { owner: string; device: string; muted: boolean }[] };
      if (p.community !== this.community || p.channel !== this.channel) return;
      this.lastRoster = p.roster.map((r) => ({
        ownerHex: r.owner, deviceHex: r.device, muted: r.muted,
      }));
      this.deps.onRosterOwners?.(this.lastRoster.map((r) => r.ownerHex));
      this.refreshRoster();
    });
    this.unlisteners.push(un);
  }

  /** Recompute roster view (speaking + card resolution). Call on presence + each drain. */
  private refreshRoster(): void {
    const roster: RosterMember[] = this.lastRoster.map((r) => {
      const isSelf = r.deviceHex.slice(0, 32) === this.deps.selfDeviceHex.slice(0, 32);
      const speaking = isSelf
        ? (!this.muted && !this.deafened && this.lastSelfSpeaking)
        : (this.receiver?.isSpeaking(r.deviceHex.slice(0, 32)) ?? false);
      const card = this.deps.resolveCard?.(r.ownerHex);
      return {
        ownerHex: r.ownerHex, deviceHex: r.deviceHex, muted: r.muted, speaking,
        ...(card?.displayName ? { displayName: card.displayName } : {}),
        ...(card?.avatarUrl ? { avatarUrl: card.avatarUrl } : {}),
      } as RosterMember;
    });
    // Only touch the store when the view actually changed — the drain tick calls
    // this ~10×/s, but most ticks produce an identical roster.
    if (rostersEqual(roster, this.lastEmittedRoster)) return;
    this.lastEmittedRoster = roster;
    this.patch({ roster });
  }
}

/**
 * App-lifetime singleton. There is at most one active voice session per process
 * (joining a second channel throws while one is connected), so the whole app
 * shares a single VoiceSession built once from the wiring deps. The first caller
 * constructs it with its deps; subsequent calls return the same instance and
 * ignore their deps (identity/adapter are stable for the session).
 */
let _voiceSessionSingleton: VoiceSession | null = null;
export function getVoiceSession(deps: VoiceSessionDeps): VoiceSession {
  if (!_voiceSessionSingleton) _voiceSessionSingleton = new VoiceSession(deps);
  return _voiceSessionSingleton;
}
