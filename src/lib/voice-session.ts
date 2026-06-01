// src/lib/voice-session.ts
import { writable, type Readable } from 'svelte/store';
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
  roster: RosterMember[];
}

type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
type Listen = (ev: string, h: (e: { payload: unknown }) => void) => Promise<() => void>;
type FrameGate = (pcm: Float32Array) => { send: boolean; ptt: boolean };

export interface VoiceSessionDeps {
  invoke: Invoke;
  listen: Listen;
  selfOwnerHex: string;
  selfDeviceHex: string;
  senderHash: Uint8Array;          // 16 bytes
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
  muted: true, deafened: false, pttMode: false, roster: [],
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
    let phase: SessionPhase = 'idle';
    this.store.update((s) => { phase = s.phase; return s; });
    if (phase !== 'idle') throw new Error('A voice session is already active');

    this.community = community;
    this.channel = channel;
    this.muted = true; this.deafened = false; this.pttMode = false; this.pttHeld = false;
    this.vad.reset();
    this.patch({ phase: 'joining', community, channel, muted: true, deafened: false, pttMode: false, roster: [] });

    // Backend join (spawns subscribers + presence publisher, starts muted).
    await this.deps.invoke('join_voice_channel', { communityId: community, channelId: channel });

    // Build engine pieces.
    this.mixer = this.deps.makeMixer ? this.deps.makeMixer() : new VoiceMixer();
    await this.mixer.init();

    this.receiver = this.deps.makeReceiver
      ? this.deps.makeReceiver()
      : new VoiceReceiver({
          listen: this.deps.listen,
          createCodec: (t: CodecType) => (t === 'codec2' ? new Codec2Codec() : new OpusCodec()),
          onPlayFrame: (hex, pcm) => this.mixer?.pushFrame(hex, pcm),
          ownSenderHex: this.deps.selfDeviceHex.slice(0, 32),
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

    // 20ms mixer drain.
    this.drainTimer = setInterval(() => { this.mixer?.drain(); this.refreshRoster(); }, 20);

    await this.subscribePresence();   // Task 5

    this.patch({ phase: 'connected' });
  }

  async setMuted(muted: boolean): Promise<void> {
    this.muted = muted;
    if (muted) this.vad.reset();
    this.patch({ muted });
    if (this.community && this.channel) {
      await this.deps.invoke('set_voice_muted',
        { communityId: this.community, channelId: this.channel, muted });
    }
  }

  setPttMode(on: boolean): void { this.pttMode = on; this.patch({ pttMode: on }); }
  setPttHeld(held: boolean): void { this.pttHeld = held; }

  async setDeafened(deaf: boolean): Promise<void> {
    this.deafened = deaf;
    this.mixer?.setDeafened(deaf);
    this.patch({ deafened: deaf });
    if (deaf && !this.muted) await this.setMuted(true);   // deafen implies self-mute
  }

  async leave(): Promise<void> {
    this.patch({ phase: 'leaving' });
    if (this.drainTimer) { clearInterval(this.drainTimer); this.drainTimer = null; }
    for (const u of this.unlisteners) u();
    this.unlisteners = [];
    await this.sender?.stop().catch(() => {});
    this.receiver?.destroy();
    await this.mixer?.destroy().catch(() => {});
    this.sender = null; this.receiver = null; this.mixer = null;
    const community = this.community, channel = this.channel;
    this.community = null; this.channel = null;
    if (community && channel) {
      await this.deps.invoke('leave_voice_channel', { communityId: community, channelId: channel }).catch(() => {});
    }
    this.store.set({ ...INITIAL });
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
    this.patch({ roster });
  }
}
