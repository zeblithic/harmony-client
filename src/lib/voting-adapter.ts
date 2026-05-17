/**
 * ZEB-290 Phase 1 — thin TypeScript wrapper over the 4 voting IPC
 * commands + 3 voting Tauri event listeners.
 *
 * IPC commands (Rust side: `src-tauri/src/lib.rs`):
 *   - `voting_create_tier1_poll`
 *   - `voting_cast_tier1_ballot`
 *   - `voting_list_active_polls`
 *   - `voting_get_poll`
 *
 * Tauri events:
 *   - `voting-poll-created` (fired on local create — Phase 1.5 will
 *     fan-out via Zenoh so peers see remote creates too)
 *   - `voting-ballot-cast` (fired on local cast — same Phase 1.5
 *     fan-out story)
 *   - `voting-poll-closed` (NOT fired in Phase 1 — backend will emit
 *     this in Phase 1.5 with the Zenoh auto-close tick. Listener is
 *     wired now for forward compatibility.)
 *
 * Error handling follows the project memory `feedback_tauri_error_extraction`:
 * production rejections are strings, tests use Error objects with
 * "Error: " prefix — `e instanceof Error ? e.message : String(e)`.
 */

import type { TauriAdapter } from './zenoh-service';
import type {
  PollMeta,
  PollStateExport,
  VotingBallotCastPayload,
  VotingPollClosedPayload,
  VotingPollCreatedPayload,
} from './types/voting';

/** Args for `createTier1Poll`. Mirrors the Rust IPC signature 1:1. */
export interface CreateTier1PollArgs {
  communityId: string;
  channelId: string;
  options: string[];
  windowSeconds: number;
  minPower: number;
  minVouchingDepth?: number;
  quorum?: number;
  thresholdPercent?: number;
  multiWinner?: number;
}

export class VotingAdapter {
  /** Convenience single-slot setters (kept for backward compat with
   *  existing consumers that own the only subscription). Setting these
   *  appends a subscriber; clearing requires the returned unsubscribe
   *  from the explicit `subscribeXxx` methods. Prefer the explicit
   *  subscribers for components that mount/unmount in non-LIFO order
   *  (Cursor #130 round-6 caught a leak in the old monkey-patch
   *  linked-list pattern). */
  get onPollCreated(): ((payload: VotingPollCreatedPayload) => void) | undefined {
    return this._onPollCreated;
  }
  set onPollCreated(handler: ((payload: VotingPollCreatedPayload) => void) | undefined) {
    this._onPollCreated = handler;
  }
  get onBallotCast(): ((payload: VotingBallotCastPayload) => void) | undefined {
    return this._onBallotCast;
  }
  set onBallotCast(handler: ((payload: VotingBallotCastPayload) => void) | undefined) {
    this._onBallotCast = handler;
  }
  get onPollClosed(): ((payload: VotingPollClosedPayload) => void) | undefined {
    return this._onPollClosed;
  }
  set onPollClosed(handler: ((payload: VotingPollClosedPayload) => void) | undefined) {
    this._onPollClosed = handler;
  }
  private _onPollCreated?: (payload: VotingPollCreatedPayload) => void;
  private _onBallotCast?: (payload: VotingBallotCastPayload) => void;
  private _onPollClosed?: (payload: VotingPollClosedPayload) => void;

  /** Multi-subscriber lists. `subscribeXxx` returns an unsubscribe
   *  closure that splices the handler out of the list — order-
   *  independent cleanup with no chain-leak risk. */
  private pollCreatedSubs: Array<(p: VotingPollCreatedPayload) => void> = [];
  private ballotCastSubs: Array<(p: VotingBallotCastPayload) => void> = [];
  private pollClosedSubs: Array<(p: VotingPollClosedPayload) => void> = [];

  subscribePollCreated(handler: (p: VotingPollCreatedPayload) => void): () => void {
    this.pollCreatedSubs.push(handler);
    return () => {
      const i = this.pollCreatedSubs.indexOf(handler);
      if (i >= 0) this.pollCreatedSubs.splice(i, 1);
    };
  }
  subscribeBallotCast(handler: (p: VotingBallotCastPayload) => void): () => void {
    this.ballotCastSubs.push(handler);
    return () => {
      const i = this.ballotCastSubs.indexOf(handler);
      if (i >= 0) this.ballotCastSubs.splice(i, 1);
    };
  }
  subscribePollClosed(handler: (p: VotingPollClosedPayload) => void): () => void {
    this.pollClosedSubs.push(handler);
    return () => {
      const i = this.pollClosedSubs.indexOf(handler);
      if (i >= 0) this.pollClosedSubs.splice(i, 1);
    };
  }

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];
  private connectInFlight: Promise<void> | null = null;

  /** Wire the three event listeners onto a Tauri adapter. Idempotent
   *  (second call with a different adapter is a no-op).
   *
   *  Stages unlisteners in a local array and only commits to `this.adapter`
   *  after every `listen` resolves; on failure, unwires any partial
   *  registrations so a retry sees a clean slate. Uses a `connectInFlight`
   *  promise singleton so two overlapping callers share the in-flight
   *  connect rather than double-registering — both PR #130 review rounds
   *  caught successive corners of this code path. */
  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    if (this.connectInFlight) return this.connectInFlight;

    this.connectInFlight = (async () => {
      const stagedUnlisteners: Array<() => void> = [];
      try {
        const unlistenCreated = await adapter.listen(
          'voting-poll-created',
          (event) => {
            const payload = event.payload as VotingPollCreatedPayload;
            this._onPollCreated?.(payload);
            // Copy first so a handler that unsubscribes itself during
            // delivery doesn't skip a sibling at the same index.
            for (const sub of [...this.pollCreatedSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenCreated);

        const unlistenCast = await adapter.listen(
          'voting-ballot-cast',
          (event) => {
            const payload = event.payload as VotingBallotCastPayload;
            this._onBallotCast?.(payload);
            for (const sub of [...this.ballotCastSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenCast);

        const unlistenClosed = await adapter.listen(
          'voting-poll-closed',
          (event) => {
            const payload = event.payload as VotingPollClosedPayload;
            this._onPollClosed?.(payload);
            for (const sub of [...this.pollClosedSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenClosed);

        this.adapter = adapter;
        this.unlisteners.push(...stagedUnlisteners);
      } catch (e) {
        for (const u of stagedUnlisteners) {
          try {
            u();
          } catch {
            // swallow cleanup failures
          }
        }
        throw e;
      } finally {
        this.connectInFlight = null;
      }
    })();
    return this.connectInFlight;
  }

  /** Tear down all event listeners. Safe to call before connect. */
  destroy(): void {
    for (const u of this.unlisteners) {
      try {
        u();
      } catch {
        // Best-effort cleanup; listeners that already errored are fine.
      }
    }
    this.unlisteners = [];
    this.adapter = null;
  }

  /** Create a Tier 1 (Approval) poll. Returns the new poll id (hex). */
  async createTier1Poll(args: CreateTier1PollArgs): Promise<string> {
    return this.invoke<string>('voting_create_tier1_poll', {
      communityId: args.communityId,
      channelId: args.channelId,
      options: args.options,
      windowSeconds: args.windowSeconds,
      minPower: args.minPower,
      minVouchingDepth: args.minVouchingDepth,
      quorum: args.quorum,
      thresholdPercent: args.thresholdPercent,
      multiWinner: args.multiWinner,
    });
  }

  /** Cast a Tier 1 (Approval) ballot. `approvedIndices` are option
   *  indices (0..options.length). Re-casting replaces (last-write-wins
   *  by HLC). */
  async castTier1Ballot(pollId: string, approvedIndices: number[]): Promise<void> {
    await this.invoke<void>('voting_cast_tier1_ballot', {
      pollId,
      approvedIndices,
    });
  }

  /** List all Open polls for a community. */
  async listActivePolls(communityId: string): Promise<PollMeta[]> {
    return this.invoke<PollMeta[]>('voting_list_active_polls', { communityId });
  }

  /** Fetch full state (meta + tally + your-ballot + options) for one poll. */
  async getPoll(pollId: string): Promise<PollStateExport> {
    return this.invoke<PollStateExport>('voting_get_poll', { pollId });
  }

  private async invoke<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
    if (!this.adapter) {
      throw new Error(`VotingAdapter not connected — cannot invoke '${cmd}'`);
    }
    try {
      return (await this.adapter.invoke(cmd, args)) as T;
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(`${cmd} failed: ${msg}`);
    }
  }
}
