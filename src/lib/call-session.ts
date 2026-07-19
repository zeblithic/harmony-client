// src/lib/call-session.ts
//
// CallSession — controller for 1:1 DM voice calls.
//
// A signaling state machine (idle → ringing → connecting → active → ended)
// layered over the same V3 media engine that VoiceSession uses for channel
// voice (VoiceSender / VoiceReceiver / VoiceMixer), but addressed by a `callId`
// 2-party room instead of a community/channel. The media-control verbs
// (setMuted / setPttMode / setPttHeld / setDeafened) are PORTED from
// VoiceSession — including the local-rollback-on-reject behavior (ZEB-351) — so
// the DM path can't drift from the channel path's hard-won correctness.

import { writable, get, type Readable } from 'svelte/store';
import type { CallEventPayload, CallOutcome } from './call-log';
import { VoiceActivityDetector } from './voice/vad';
import { makeTalkGate } from './voice/talk-gate';
import { VoiceSender } from './voice/voice-sender';
import { VoiceReceiver } from './voice/voice-receiver';
import { VoiceMixer } from './voice/voice-mixer';
import { AudioCapture } from './voice/audio-capture';
import { OpusCodec } from './voice/opus-codec';
import { Codec2Codec } from './voice/codec2-codec';
import type { CodecType } from './voice/voice-codec';

export type CallPhase =
  | 'idle'
  | 'ringingOut'
  | 'incoming'
  | 'connecting'
  | 'active'
  | 'ended';

export interface CallSessionState {
  phase: CallPhase;
  callId: string | null;
  peerOwnerHex: string | null;
  muted: boolean;
  pttMode: boolean;
  pttHeld: boolean;
  deafened: boolean;
  startedAt: number | null;
  /** Last decline/end reason surfaced to the UI ("No answer", "busy", …). */
  endReason: string | null;
  /**
   * True while the inbound DM media subscriber is re-establishing after a
   * transport drop (the backend emits `voice-transport-lost` /
   * `voice-transport-restored` tagged by callId around its re-declare loop).
   * Surfaces a "Reconnecting…" indicator in the in-call bar.
   */
  reconnecting: boolean;
}

type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
type Listen = (ev: string, h: (e: { payload: unknown }) => void) => Promise<() => void>;
type FrameGate = (pcm: Float32Array) => { send: boolean; ptt: boolean };

/** Lowercase hex-encode a byte array (16-byte senderHash → 32 hex chars). */
function bytesToHex(b: Uint8Array): string {
  return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
}

/** A callee that doesn't answer within this window auto-declines (timeout). */
const RING_TIMEOUT_MS = 30_000;

export interface CallSessionDeps {
  invoke: Invoke;
  listen: Listen;
  selfOwnerHex: string;
  selfDeviceHex: string;
  /** 16-byte sender hash stamped into outbound packets; see VoiceSessionDeps. */
  senderHash: Uint8Array;
  vadThreshold?: number;
  // Factories (injected in tests; real defaults built inside connect()).
  makeSender?: (gate: FrameGate) => Pick<VoiceSender, 'start' | 'stop'>;
  makeReceiver?: () => Pick<VoiceReceiver, 'init' | 'destroy' | 'getActiveSenders' | 'isSpeaking'>;
  makeMixer?: () => Pick<VoiceMixer, 'init' | 'pushFrame' | 'drain' | 'setDeafened' | 'destroy'>;
  /** Resolve an owner hex → { displayName, avatarUrl } for the call UI (optional). */
  resolveCard?: (ownerHex: string) => { displayName?: string; avatarUrl?: string } | undefined;
  /**
   * ZEB-357: fired once per call at its terminal transition, ONLY when this
   * side placed the call (single-writer rule — the caller observes every
   * outcome, so the callee never records). The App wires this to author the
   * call-event DM message; the session itself stays IPC-schema-agnostic.
   */
  onCallOutcome?: (spaceId: string, payload: CallEventPayload) => void;
}

const INITIAL: CallSessionState = {
  phase: 'idle',
  callId: null,
  peerOwnerHex: null,
  muted: true,
  pttMode: false,
  pttHeld: false,
  deafened: false,
  startedAt: null,
  endReason: null,
  reconnecting: false,
};

export class CallSession {
  readonly state: Readable<CallSessionState>;
  private store = writable<CallSessionState>({ ...INITIAL });
  private deps: CallSessionDeps;

  private vad: VoiceActivityDetector;
  private coreGate: FrameGate;
  private sender: Pick<VoiceSender, 'start' | 'stop'> | null = null;
  private receiver: Pick<VoiceReceiver, 'init' | 'destroy' | 'getActiveSenders' | 'isSpeaking'> | null = null;
  private mixer: Pick<VoiceMixer, 'init' | 'pushFrame' | 'drain' | 'setDeafened' | 'destroy'> | null = null;

  private muted = true;
  private deafened = false;
  private pttMode = false;
  private pttHeld = false;
  private callId: string | null = null;
  /** The spaceId (DM space) used to place/accept this call; passed to the
   *  signaling verbs (cancel/decline/end) that the backend keys by space. */
  private spaceId: string | null = null;
  /** ZEB-357: true iff this side placed the current call. Gates outcome
   *  recording (single-writer: only the caller authors the call-event). */
  private isCaller = false;
  /** PR #494 R1 (Qodo): per-call once-only latch for outcome emission. The
   *  callId guard alone is not enough — end() awaits teardownMedia BEFORE
   *  resetToIdle clears callId, so a concurrently-delivered call-ended event
   *  could pass the guards in that window and record a duplicate. */
  private outcomeRecorded = false;
  private drainTimer: ReturnType<typeof setInterval> | null = null;
  private ringTimer: ReturnType<typeof setTimeout> | null = null;
  /** Transport-lifecycle unlisteners (voice-transport-lost/restored). Torn down
   *  by teardownMedia/destroy alongside the rest of the media engine. */
  private unlisteners: (() => void)[] = [];

  constructor(deps: CallSessionDeps) {
    this.deps = deps;
    this.state = this.store;
    this.vad = new VoiceActivityDetector({ threshold: deps.vadThreshold ?? 0.02 });
    // Built here (not as a field initializer) so `this.vad` is already assigned.
    // Deafen folds into the muted bit (deafen implies self-mute), mirroring
    // VoiceSession; the DM call has no roster, so there is no self-speaking
    // side-effect to layer on top.
    this.coreGate = makeTalkGate(
      () => ({ muted: this.muted || this.deafened, pttMode: this.pttMode, pttHeld: this.pttHeld }),
      this.vad,
    );
  }

  private patch(p: Partial<CallSessionState>): void {
    this.store.update((s) => ({ ...s, ...p }));
  }

  private resetToIdle(endReason: string | null = null): void {
    this.clearRingTimer();
    this.muted = true; this.deafened = false; this.pttMode = false; this.pttHeld = false;
    this.callId = null; this.spaceId = null;
    this.isCaller = false;
    this.outcomeRecorded = false;
    this.vad.reset();
    // Single store notification: callers that need to surface an end reason pass
    // it here rather than reset-then-patch, which would briefly flash endReason
    // null to subscribers.
    this.store.set({ ...INITIAL, endReason });
  }

  private clearRingTimer(): void {
    if (this.ringTimer) { clearTimeout(this.ringTimer); this.ringTimer = null; }
  }

  /**
   * ZEB-357: emit the terminal outcome for the current call. Must run BEFORE
   * the resetToIdle that clears callId/spaceId. No-op on the callee side
   * (single-writer rule) or when the dep isn't wired. The callId guard in
   * every remote handler already ensures at most one terminal per call — by
   * the time a stale second terminal arrives, callId is null.
   */
  private recordOutcome(outcome: CallOutcome, durationMs?: number): void {
    if (!this.isCaller || !this.callId || !this.spaceId) return;
    if (this.outcomeRecorded) return; // once per call, even across overlapping terminals
    this.outcomeRecorded = true;
    const payload: CallEventPayload = { v: 1, callId: this.callId, outcome };
    if (durationMs !== undefined) payload.durationMs = durationMs;
    this.deps.onCallOutcome?.(this.spaceId, payload);
  }

  // ---------------------------------------------------------------------------
  // Signaling verbs
  // ---------------------------------------------------------------------------

  /** Caller side: place an outbound call into a DM space. Returns the callId. */
  async placeCall(spaceId: string): Promise<string> {
    if (get(this.store).phase !== 'idle') {
      throw new Error('A call is already in progress');
    }
    const callId = (await this.deps.invoke('place_call', { spaceId })) as string;
    this.callId = callId;
    this.spaceId = spaceId;
    this.isCaller = true;
    this.outcomeRecorded = false;
    this.muted = true; this.deafened = false; this.pttMode = false; this.pttHeld = false;
    this.vad.reset();
    this.patch({
      phase: 'ringingOut', callId, peerOwnerHex: null,
      muted: true, deafened: false, pttMode: false, pttHeld: false,
      startedAt: null, endReason: null,
    });
    return callId;
  }

  /** Caller side: abandon a call we placed before it was answered. */
  async cancel(): Promise<void> {
    if (get(this.store).phase !== 'ringingOut') return;
    const callId = this.callId, spaceId = this.spaceId;
    // PR #494 R1 (CodeRabbit): commit the terminal BEFORE awaiting the IPC —
    // a decline landing during the await must not win the race and record a
    // competing outcome. The user's cancellation is the single terminal.
    // `canceled` is also the only terminal an unreachable/offline callee ever
    // gets (there is no caller-side ring timeout) — the recipient renders it
    // as a missed call.
    this.recordOutcome('canceled');
    this.resetToIdle();
    if (callId) {
      await this.deps.invoke('cancel_call', { callId, spaceId }).catch(() => {});
    }
  }

  /**
   * Callee side: an inbound invite arrived. If we're already busy (any non-idle
   * phase) we immediately decline with reason 'busy' (D11) and stay put.
   * Otherwise we move to 'incoming' and arm the 30s ring timeout.
   */
  onIncoming(callId: string, callerOwnerHex: string, spaceId: string): void {
    if (get(this.store).phase !== 'idle') {
      // Busy — decline this new invite without disturbing the active call.
      this.deps.invoke('decline_call', { callId, spaceId, reason: 'busy' }).catch(() => {});
      return;
    }
    this.callId = callId;
    this.spaceId = spaceId;
    this.patch({
      phase: 'incoming', callId, peerOwnerHex: callerOwnerHex,
      muted: true, deafened: false, pttMode: false, pttHeld: false,
      startedAt: null, endReason: null,
    });
    this.clearRingTimer();
    this.ringTimer = setTimeout(() => { void this.decline('timeout'); }, RING_TIMEOUT_MS);
  }

  /** Callee side: accept the inbound call and connect the media engine. */
  async accept(): Promise<void> {
    if (get(this.store).phase !== 'incoming') return;
    this.clearRingTimer();
    const callId = this.callId!;
    // Use the spaceId pinned by onIncoming() — the single source of truth for
    // which DM this call belongs to. Taking it from the caller risks a stale UI
    // prop acknowledging one invite while join_dm_call targets a different room.
    const spaceId = this.spaceId;
    if (!spaceId) throw new Error('missing DM space for incoming call');
    await this.deps.invoke('accept_call', { callId, spaceId });
    this.patch({ phase: 'connecting' });
    await this.connect(spaceId);
  }

  /** Callee side: reject the inbound call (user action or ring timeout). */
  async decline(reason: string): Promise<void> {
    const phase = get(this.store).phase;
    if (phase !== 'incoming') return;
    this.clearRingTimer();
    const callId = this.callId, spaceId = this.spaceId;
    if (callId) {
      await this.deps.invoke('decline_call', { callId, spaceId, reason }).catch(() => {});
    }
    this.resetToIdle();
  }

  /** Either side: hang up an active (or connecting) call and tear down media. */
  async end(): Promise<void> {
    const phase = get(this.store).phase;
    if (phase === 'idle' || phase === 'ended') return;
    const callId = this.callId, spaceId = this.spaceId;
    if (callId) {
      await this.deps.invoke('end_call', { callId, spaceId }).catch(() => {});
    }
    this.recordAnsweredTerminal(phase);
    await this.teardownMedia(callId);
    this.resetToIdle();
  }

  /**
   * ZEB-357: caller-side terminal for a call that got past the ring — records
   * `answered` with the active-phase duration (0 when it ended while still
   * connecting). An `end()` from `ringingOut` (not reachable via the UI, which
   * uses cancel there) records `canceled` so it can't masquerade as answered.
   */
  private recordAnsweredTerminal(phase: CallPhase): void {
    if (phase === 'ringingOut') {
      this.recordOutcome('canceled');
      return;
    }
    const startedAt = get(this.store).startedAt;
    this.recordOutcome('answered', startedAt !== null ? Date.now() - startedAt : 0);
  }

  // ---------------------------------------------------------------------------
  // Remote-event handlers (the App wires these from `listen`).
  // Each is guarded on a matching callId so stale events for an old call can't
  // disturb a current one.
  // ---------------------------------------------------------------------------

  /** Caller side: the callee accepted — connect the media engine. */
  async onRemoteAccepted(callId: string): Promise<void> {
    if (this.callId !== callId || get(this.store).phase !== 'ringingOut') return;
    this.patch({ phase: 'connecting' });
    await this.connect(this.spaceId!);
  }

  /** Caller side: the callee declined — surface the reason and reset. */
  onRemoteDeclined(callId: string, reason: string): void {
    if (this.callId !== callId || get(this.store).phase !== 'ringingOut') return;
    // timeout = the callee's ring timer expired (they never picked up);
    // busy = their device auto-declined while on another call; anything else
    // is an explicit user decline.
    this.recordOutcome(
      reason === 'timeout' ? 'no_answer' : reason === 'busy' ? 'busy' : 'declined',
    );
    this.resetToIdle(reason);
  }

  /** Callee side: the caller canceled before we answered. */
  onRemoteCanceled(callId: string): void {
    if (this.callId !== callId || get(this.store).phase !== 'incoming') return;
    this.resetToIdle();
  }

  /** Either side: the peer hung up an active call — tear down and reset. */
  async onRemoteEnded(callId: string): Promise<void> {
    if (this.callId !== callId) return;
    const phase = get(this.store).phase;
    if (phase === 'idle' || phase === 'ended') return;
    this.recordAnsweredTerminal(phase);
    await this.teardownMedia(callId);
    this.resetToIdle();
  }

  // ---------------------------------------------------------------------------
  // Media core
  // ---------------------------------------------------------------------------

  /**
   * Join the DM call room and build the V3 media engine addressed by callId.
   * Starts muted (D10): the gate transmits nothing until the user unmutes.
   */
  private async connect(spaceId: string): Promise<void> {
    const callId = this.callId!;
    try {
      await this.deps.invoke('join_dm_call', { callId, spaceId });
      await this.subscribeTransport();

      this.mixer = this.deps.makeMixer ? this.deps.makeMixer() : new VoiceMixer();
      await this.mixer.init();

      this.receiver = this.deps.makeReceiver
        ? this.deps.makeReceiver()
        : new VoiceReceiver({
            listen: this.deps.listen,
            createCodec: (t: CodecType) => (t === 'codec2' ? new Codec2Codec() : new OpusCodec()),
            onPlayFrame: (hex, pcm) => this.mixer?.pushFrame(hex, pcm),
            ownSenderHex: bytesToHex(this.deps.senderHash),
            // DM frames arrive on a distinct event and carry a callId; only this
            // call's frames are admitted.
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
            communityId: '', channelId: '', // unused on the DM path; publishFrame routes instead.
            invoke: this.deps.invoke, codec: new OpusCodec(), capture: new AudioCapture(),
            frameGate: this.gate,
            publishFrame: (frameBytes) =>
              this.deps.invoke('send_dm_voice_frame', {
                payload: { callId: this.callId, frameBytes },
              }),
          });
      await this.sender.start(); // capture starts; muted gate ⇒ nothing transmits

      // Drain the mixer every 20ms (audio cadence), mirroring VoiceSession.
      this.drainTimer = setInterval(() => { this.mixer?.drain(); }, 20);

      this.muted = true; this.deafened = false; this.pttMode = false; this.pttHeld = false;
      this.patch({
        phase: 'active', startedAt: Date.now(),
        muted: true, deafened: false, pttMode: false, pttHeld: false,
      });
    } catch (err) {
      // A step after join_dm_call failed — tear down any partially-built media
      // and leave the backend room so the session returns to idle instead of
      // stranding in 'connecting' (which would block all future calls until a
      // reload and could leak capture/playback resources). teardownMedia is
      // best-effort and null-safe, so a partial setup is fine to unwind.
      await this.teardownMedia(callId);
      this.resetToIdle();
      throw err;
    }
  }

  /** The per-frame send decision (mute / PTT / VAD), via the shared talk-gate. */
  private gate: FrameGate = (pcm) => this.coreGate(pcm);

  /**
   * Subscribe to the backend's DM media-transport lifecycle events. The inbound
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

  /** Dismantle the media engine + leave the DM call room. Best-effort. */
  private async teardownMedia(callId: string | null): Promise<void> {
    if (this.drainTimer) { clearInterval(this.drainTimer); this.drainTimer = null; }
    for (const u of this.unlisteners) u();
    this.unlisteners = [];
    await this.sender?.stop().catch(() => {});
    this.receiver?.destroy();
    await this.mixer?.destroy().catch(() => {});
    this.sender = null; this.receiver = null; this.mixer = null;
    if (callId) {
      await this.deps.invoke('leave_dm_call', { callId }).catch(() => {});
    }
  }

  // ---------------------------------------------------------------------------
  // Media controls — PORTED from VoiceSession (incl. the ZEB-351 rollback),
  // with the IPC swapped to the DM call variant. The IPC only fires while the
  // call is connected ('active'); before then the toggles just update local
  // state so the pre-call UI can pre-arm mute/PTT.
  // ---------------------------------------------------------------------------

  async setMuted(muted: boolean): Promise<void> {
    const prev = this.muted;
    this.muted = muted;
    if (muted) this.vad.reset();
    this.patch({ muted });
    if (get(this.store).phase === 'active' && this.callId) {
      try {
        await this.deps.invoke('set_dm_call_muted', {
          payload: { callId: this.callId, muted },
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
   * coupling round-trips set_dm_call_muted.
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
    this.deafened = deaf;
    this.mixer?.setDeafened(deaf);
    this.patch({ deafened: deaf });
    if (deaf && !this.muted) await this.setMuted(true); // deafen implies self-mute
  }

  /**
   * Dispose this session before it is discarded (e.g. on identity switch, when
   * getCallSession rebuilds the singleton). Clears timers and tears down any
   * live media so a replaced instance never leaves the mic open, the drain/ring
   * timers firing, or media bound to stale IPC handles. Fire-and-forget.
   */
  destroy(): void {
    this.clearRingTimer();
    if (this.drainTimer) { clearInterval(this.drainTimer); this.drainTimer = null; }
    for (const u of this.unlisteners) u();
    this.unlisteners = [];
    void this.sender?.stop().catch(() => {});
    this.receiver?.destroy();
    void this.mixer?.destroy().catch(() => {});
    this.sender = null; this.receiver = null; this.mixer = null;
    this.resetToIdle();
  }
}

/**
 * Per-identity singleton. There is at most one active DM call per process, so
 * the app shares a single CallSession. It is rebuilt when the identity changes
 * (e.g. the user switches identity via stop_node / start_node) so the session
 * never signs signals or joins media rooms with a previous identity's keys or
 * IPC handles; calls with the same identity return the cached instance.
 */
let _callSessionSingleton: CallSession | null = null;
let _callSessionIdentity: string | null = null;
export function getCallSession(deps: CallSessionDeps): CallSession {
  const identity = `${deps.selfOwnerHex}:${deps.selfDeviceHex}`;
  if (!_callSessionSingleton || _callSessionIdentity !== identity) {
    // Tear down the outgoing instance so its media/timers don't keep running on
    // the previous identity's handles after the swap.
    _callSessionSingleton?.destroy();
    _callSessionSingleton = new CallSession(deps);
    _callSessionIdentity = identity;
  }
  return _callSessionSingleton;
}
