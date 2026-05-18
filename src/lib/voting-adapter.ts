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
  AutoExecAction,
  DelegationEdgeExport,
  PollMeta,
  PollStateExport,
  Tier2ProposalExport,
  VotingBallotCastPayload,
  VotingPollClosedPayload,
  VotingPollCreatedPayload,
  VotingProposalFinalizedPayload,
  VotingThresholdReachedPayload,
  VotingThresholdRevertedPayload,
  VotingTier2ProposalCreatedPayload,
  VotingTier2SignalCastPayload,
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

  // ZEB-291 Phase 2 — Tier 2 subscriber lists. Same single-pattern as
  // Phase 1's round-6 refactor: handler list + splice-on-unsubscribe.
  private proposalCreatedSubs: Array<(p: VotingTier2ProposalCreatedPayload) => void> = [];
  private signalCastSubs: Array<(p: VotingTier2SignalCastPayload) => void> = [];
  private thresholdReachedSubs: Array<(p: VotingThresholdReachedPayload) => void> = [];
  private thresholdRevertedSubs: Array<(p: VotingThresholdRevertedPayload) => void> = [];
  private proposalFinalizedSubs: Array<(p: VotingProposalFinalizedPayload) => void> = [];

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

  // ─── Tier 2 (Conviction) event subscribers ──────────────────────────
  subscribeProposalCreated(
    handler: (p: VotingTier2ProposalCreatedPayload) => void,
  ): () => void {
    this.proposalCreatedSubs.push(handler);
    return () => {
      const i = this.proposalCreatedSubs.indexOf(handler);
      if (i >= 0) this.proposalCreatedSubs.splice(i, 1);
    };
  }
  subscribeSignalCast(
    handler: (p: VotingTier2SignalCastPayload) => void,
  ): () => void {
    this.signalCastSubs.push(handler);
    return () => {
      const i = this.signalCastSubs.indexOf(handler);
      if (i >= 0) this.signalCastSubs.splice(i, 1);
    };
  }
  subscribeThresholdReached(
    handler: (p: VotingThresholdReachedPayload) => void,
  ): () => void {
    this.thresholdReachedSubs.push(handler);
    return () => {
      const i = this.thresholdReachedSubs.indexOf(handler);
      if (i >= 0) this.thresholdReachedSubs.splice(i, 1);
    };
  }
  subscribeThresholdReverted(
    handler: (p: VotingThresholdRevertedPayload) => void,
  ): () => void {
    this.thresholdRevertedSubs.push(handler);
    return () => {
      const i = this.thresholdRevertedSubs.indexOf(handler);
      if (i >= 0) this.thresholdRevertedSubs.splice(i, 1);
    };
  }
  subscribeProposalFinalized(
    handler: (p: VotingProposalFinalizedPayload) => void,
  ): () => void {
    this.proposalFinalizedSubs.push(handler);
    return () => {
      const i = this.proposalFinalizedSubs.indexOf(handler);
      if (i >= 0) this.proposalFinalizedSubs.splice(i, 1);
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

        // ZEB-291 Phase 2 — Tier 2 events. The same copy-then-iterate
        // pattern as Tier 1 above so a handler that unsubscribes itself
        // during delivery doesn't skip a sibling at the same index.
        const unlistenProposalCreated = await adapter.listen(
          'voting-tier2-proposal-created',
          (event) => {
            const payload = event.payload as VotingTier2ProposalCreatedPayload;
            for (const sub of [...this.proposalCreatedSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenProposalCreated);

        const unlistenSignalCast = await adapter.listen(
          'voting-tier2-signal-cast',
          (event) => {
            const payload = event.payload as VotingTier2SignalCastPayload;
            for (const sub of [...this.signalCastSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenSignalCast);

        const unlistenThresholdReached = await adapter.listen(
          'voting-threshold-reached',
          (event) => {
            const payload = event.payload as VotingThresholdReachedPayload;
            for (const sub of [...this.thresholdReachedSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenThresholdReached);

        const unlistenThresholdReverted = await adapter.listen(
          'voting-threshold-reverted',
          (event) => {
            const payload = event.payload as VotingThresholdRevertedPayload;
            for (const sub of [...this.thresholdRevertedSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenThresholdReverted);

        const unlistenProposalFinalized = await adapter.listen(
          'voting-proposal-finalized',
          (event) => {
            const payload = event.payload as VotingProposalFinalizedPayload;
            for (const sub of [...this.proposalFinalizedSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenProposalFinalized);

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

  // ─── ZEB-291 Phase 2 — Tier 2 (Conviction) IPC wrappers ────────────
  // Param names use camelCase per the Tauri snake_case ↔ camelCase
  // boundary convention (see harmony-client/CLAUDE.md). The Rust IPC
  // functions in src-tauri/src/lib.rs declare these as snake_case;
  // Tauri auto-converts at the JSON IPC boundary.

  /** Create a Tier 2 (Conviction) proposal. Returns the new proposal id
   *  as a hex string (32 bytes → 64 chars). The Rust IPC accepts
   *  optional config fields and substitutes spec defaults when
   *  omitted (TIER2_DEFAULT_HALF_LIFE_SECONDS etc.). */
  async createTier2Proposal(args: {
    communityId: string;
    channelId: string;
    proposalText: string;
    halfLifeSeconds?: number;
    thresholdMin?: number;
    thresholdMax?: number;
    beta?: number;
    delegationAllowed?: boolean;
    autoExec?: AutoExecAction;
    minPower?: number;
  }): Promise<string> {
    return this.invoke<string>('voting_create_tier2_proposal', {
      communityId: args.communityId,
      channelId: args.channelId,
      proposalText: args.proposalText,
      halfLifeSeconds: args.halfLifeSeconds,
      thresholdMin: args.thresholdMin,
      thresholdMax: args.thresholdMax,
      beta: args.beta,
      delegationAllowed: args.delegationAllowed,
      autoExec: args.autoExec,
      minPower: args.minPower,
    });
  }

  /** Cast (or withdraw) a Tier 2 Signal on a Conviction proposal.
   *  `support = true` registers support; `support = false` withdraws
   *  previously-cast support (acts as a "contest" if the proposal is
   *  in ThresholdReached). Idempotent at the per-voter state layer. */
  async signalTier2(proposalId: string, support: boolean): Promise<void> {
    await this.invoke<void>('voting_signal_tier2', { proposalId, support });
  }

  /** Install a Tier 2 Delegate edge: caller → `delegate` (the
   *  delegate's 16-byte `OwnerAddr`, hex; 32 hex chars). Community-wide;
   *  affects every Tier 2 proposal in the community per spec §5. */
  async delegateTier2(communityId: string, delegate: string): Promise<void> {
    await this.invoke<void>('voting_delegate_tier2', { communityId, delegate });
  }

  /** Revoke the caller's Delegate edge in `communityId`. */
  async undelegateTier2(communityId: string): Promise<void> {
    await this.invoke<void>('voting_undelegate_tier2', { communityId });
  }

  /** ZEB-292 Phase 3: read the caller's current delegate (32-hex
   *  `OwnerAddr`), or null when the caller votes directly. */
  async getMyDelegate(communityId: string): Promise<string | null> {
    const r = await this.invoke<string | null>('voting_get_my_delegate', { communityId });
    return r ?? null;
  }

  /** ZEB-292 Phase 3: list every live Delegate edge in the community —
   *  consumed by the delegation-graph visualization. */
  async listDelegations(communityId: string): Promise<DelegationEdgeExport[]> {
    return this.invoke<DelegationEdgeExport[]>('voting_list_delegations', { communityId });
  }

  /** List all Tier 2 proposals (any lifecycle) for `communityId`. */
  async listTier2Proposals(communityId: string): Promise<Tier2ProposalExport[]> {
    return this.invoke<Tier2ProposalExport[]>('voting_list_tier2_proposals', {
      communityId,
    });
  }

  /** Fetch full state for one Tier 2 proposal. */
  async getTier2Proposal(proposalId: string): Promise<Tier2ProposalExport> {
    return this.invoke<Tier2ProposalExport>('voting_get_tier2_proposal', {
      proposalId,
    });
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
