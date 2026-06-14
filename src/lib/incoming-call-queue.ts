// ZEB-364: a tiny FIFO buffer for incoming-call signaling events that arrive
// during the startup window — after the `incoming-call` / `incoming-group-call`
// listeners are registered but before the lazily-built CallSession /
// GroupCallSession exists.
//
// Without buffering, a ring landing in that window is silently dropped: the
// listener routes through `callSession?.onIncoming(...)`, the `?.` no-ops while
// the session is null, and the banner / OS-notification block (gated on the
// session being present) is skipped — so neither the in-app toast nor the
// ZEB-356 OS escalation fires. App.svelte buffers such events here and drains
// them once `buildVoiceSession` builds the session. Mirrors the ZEB-338
// deep-link queue (a small module-owned buffer drained by App.svelte once its
// consumer is ready).

/**
 * The minimal incoming-call payload shared by the `incoming-call` and
 * `incoming-group-call` Tauri events — the three fields both handlers route
 * into `onIncoming` / `onIncomingGroup`.
 */
export interface IncomingCallEvent {
  callId: string;
  callerOwner: string;
  spaceId: string;
}

// Defensive cap. The startup window is sub-second, so a real queue holds 0-1
// events; this only bounds growth in the pathological case where the session
// never builds (e.g. owner identity never loads) and rings keep arriving. On
// overflow the newest event is dropped — an unbounded buffer would be the
// worse failure.
const MAX_PENDING = 32;

export interface IncomingCallQueue {
  /** Buffer an event that arrived before the session was ready. */
  queue(event: IncomingCallEvent): void;
  /**
   * Record that `callId` was canceled during the startup window. A buffered ring
   * for that call is suppressed on drain rather than replayed — otherwise a
   * caller who rings then cancels before the session builds would surface a
   * phantom (answerable) ring until the 30s timeout. Order-independent: works
   * whether the cancel is recorded before or after the matching `queue`.
   */
  cancel(callId: string): void;
  /**
   * Return buffered events in arrival (FIFO) order, minus any whose callId was
   * `cancel`ed, and clear the buffer (events + cancellations).
   */
  drain(): IncomingCallEvent[];
  /** Number of currently-buffered events (for assertions / diagnostics). */
  readonly size: number;
}

/**
 * Create an independent incoming-call buffer. App.svelte holds one for 1:1 calls
 * and one for group calls; each is drained in `buildVoiceSession` once its
 * session is built.
 */
export function createIncomingCallQueue(): IncomingCallQueue {
  const pending: IncomingCallEvent[] = [];
  const canceled = new Set<string>();
  return {
    queue(event: IncomingCallEvent): void {
      if (pending.length >= MAX_PENDING) return;
      pending.push(event);
    },
    cancel(callId: string): void {
      // Same defensive bound as the event buffer (a flood of cancels while the
      // session never builds must not grow the set without limit).
      if (canceled.size >= MAX_PENDING) return;
      canceled.add(callId);
    },
    drain(): IncomingCallEvent[] {
      const out = pending.filter((e) => !canceled.has(e.callId));
      pending.length = 0;
      canceled.clear();
      return out;
    },
    get size(): number {
      return pending.length;
    },
  };
}
