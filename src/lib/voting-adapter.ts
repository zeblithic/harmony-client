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
import {
  DFROST_BEACON_READY,
  DFROST_DKG_PROGRESS,
  DFROST_REFRESH_PROGRESS,
  DFROST_REPAIR_PROGRESS,
  type DfrostBeaconReadyPayload,
  type DfrostDkgProgressPayload,
  type DfrostRefreshProgressPayload,
  type DfrostRepairProgressPayload,
} from './types/dfrost-events';
import type {
  AutoExecAction,
  BridgingScoreExport,
  CreateTier3ProposalArgs,
  DeliberationVoteCode,
  DelegationEdgeExport,
  PollMeta,
  PollStateExport,
  Tier2ProposalExport,
  Tier3DeliberationStatementCreatedPayload,
  Tier3DeliberationVoteCastPayload,
  Tier3DraftApprovalPayload,
  Tier3DraftCandidatePayload,
  Tier3MiniPublicDeclinePayload,
  Tier3TallyShareAppliedPayload,
  VotingBallotCastPayload,
  VotingDelegateSignaledOnYourBehalfPayload,
  VotingDelegationChangedPayload,
  VotingPollClosedPayload,
  VotingPollCreatedPayload,
  VotingProposalFinalizedPayload,
  VotingThresholdReachedPayload,
  VotingThresholdRevertedPayload,
  VotingTier2ProposalCreatedPayload,
  VotingTier2SignalCastPayload,
  VotingTier3DraftingOpenPayload,
  VotingTier3FinalizedPayload,
  VotingTier3PollCreatedPayload,
  VotingTier3RatificationOpenPayload,
  VotingTier3SortitionCompletePayload,
  Tier3PollExport,
  Tier3PollSummary,
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
  // ZEB-292 Phase 3 — delegation event subscribers.
  private delegationChangedSubs: Array<(p: VotingDelegationChangedPayload) => void> = [];
  private delegateSignaledOnYourBehalfSubs: Array<
    (p: VotingDelegateSignaledOnYourBehalfPayload) => void
  > = [];

  // ZEB-310 Phase 4a-main — Tier 3 (Sortition + STAR) event subscribers.
  private tier3PollCreatedSubs: Array<(p: VotingTier3PollCreatedPayload) => void> = [];
  private tier3SortitionCompleteSubs: Array<
    (p: VotingTier3SortitionCompletePayload) => void
  > = [];
  private tier3DraftingOpenSubs: Array<(p: VotingTier3DraftingOpenPayload) => void> = [];
  private tier3RatificationOpenSubs: Array<
    (p: VotingTier3RatificationOpenPayload) => void
  > = [];
  private tier3FinalizedSubs: Array<(p: VotingTier3FinalizedPayload) => void> = [];

  // ZEB-294 — Tier 3 deliberation event subscribers.
  private tier3DeliberationStatementCreatedSubs: Array<
    (p: Tier3DeliberationStatementCreatedPayload) => void
  > = [];
  private tier3DeliberationVoteCastSubs: Array<
    (p: Tier3DeliberationVoteCastPayload) => void
  > = [];

  // ZEB-295 Phase 6 — Tier 3 tally-share-applied subscriber. Emitted on
  // every accepted kd=ts (se-mode poll) so the awaiting-tally banner can
  // update its k-of-n committee-share progress incrementally.
  private tier3TallyShareAppliedSubs: Array<
    (p: Tier3TallyShareAppliedPayload) => void
  > = [];

  // ZEB-319 — Tier 3 granular drafting-stage event subscribers.
  private tier3MiniPublicDeclineSubs: Array<
    (p: Tier3MiniPublicDeclinePayload) => void
  > = [];
  private tier3DraftCandidateSubs: Array<
    (p: Tier3DraftCandidatePayload) => void
  > = [];
  private tier3DraftApprovalSubs: Array<
    (p: Tier3DraftApprovalPayload) => void
  > = [];

  // ZEB-1018 — D-FROST committee ceremony event subscribers. These fire
  // on BOTH local IPC-driven applies and (now that the transport is
  // wired) inbound peer events, so the panel's ceremony status reflects
  // multi-node progress.
  private dfrostDkgProgressSubs: Array<(p: DfrostDkgProgressPayload) => void> = [];
  private dfrostBeaconReadySubs: Array<(p: DfrostBeaconReadyPayload) => void> = [];
  private dfrostRefreshProgressSubs: Array<
    (p: DfrostRefreshProgressPayload) => void
  > = [];
  // ZEB-1027 — RTS share-repair progress (rn=1 request, rn=2 deltas,
  // rn=3 sigmas), local and inbound alike.
  private dfrostRepairProgressSubs: Array<
    (p: DfrostRepairProgressPayload) => void
  > = [];

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

  // ─── ZEB-292 Phase 3 — delegation event subscribers ─────────────────
  subscribeDelegationChanged(
    handler: (p: VotingDelegationChangedPayload) => void,
  ): () => void {
    this.delegationChangedSubs.push(handler);
    return () => {
      const i = this.delegationChangedSubs.indexOf(handler);
      if (i >= 0) this.delegationChangedSubs.splice(i, 1);
    };
  }
  subscribeDelegateSignaledOnYourBehalf(
    handler: (p: VotingDelegateSignaledOnYourBehalfPayload) => void,
  ): () => void {
    this.delegateSignaledOnYourBehalfSubs.push(handler);
    return () => {
      const i = this.delegateSignaledOnYourBehalfSubs.indexOf(handler);
      if (i >= 0) this.delegateSignaledOnYourBehalfSubs.splice(i, 1);
    };
  }

  // ─── ZEB-310 Phase 4a-main — Tier 3 event subscribers ───────────────
  subscribeTier3PollCreated(
    handler: (p: VotingTier3PollCreatedPayload) => void,
  ): () => void {
    this.tier3PollCreatedSubs.push(handler);
    return () => {
      const i = this.tier3PollCreatedSubs.indexOf(handler);
      if (i >= 0) this.tier3PollCreatedSubs.splice(i, 1);
    };
  }
  subscribeTier3SortitionComplete(
    handler: (p: VotingTier3SortitionCompletePayload) => void,
  ): () => void {
    this.tier3SortitionCompleteSubs.push(handler);
    return () => {
      const i = this.tier3SortitionCompleteSubs.indexOf(handler);
      if (i >= 0) this.tier3SortitionCompleteSubs.splice(i, 1);
    };
  }
  subscribeTier3DraftingOpen(
    handler: (p: VotingTier3DraftingOpenPayload) => void,
  ): () => void {
    this.tier3DraftingOpenSubs.push(handler);
    return () => {
      const i = this.tier3DraftingOpenSubs.indexOf(handler);
      if (i >= 0) this.tier3DraftingOpenSubs.splice(i, 1);
    };
  }
  subscribeTier3RatificationOpen(
    handler: (p: VotingTier3RatificationOpenPayload) => void,
  ): () => void {
    this.tier3RatificationOpenSubs.push(handler);
    return () => {
      const i = this.tier3RatificationOpenSubs.indexOf(handler);
      if (i >= 0) this.tier3RatificationOpenSubs.splice(i, 1);
    };
  }
  subscribeTier3Finalized(
    handler: (p: VotingTier3FinalizedPayload) => void,
  ): () => void {
    this.tier3FinalizedSubs.push(handler);
    return () => {
      const i = this.tier3FinalizedSubs.indexOf(handler);
      if (i >= 0) this.tier3FinalizedSubs.splice(i, 1);
    };
  }

  // ─── ZEB-294 — Tier 3 deliberation event subscribers ────────────────
  subscribeTier3DeliberationStatementCreated(
    handler: (p: Tier3DeliberationStatementCreatedPayload) => void,
  ): () => void {
    this.tier3DeliberationStatementCreatedSubs.push(handler);
    return () => {
      const i = this.tier3DeliberationStatementCreatedSubs.indexOf(handler);
      if (i >= 0) this.tier3DeliberationStatementCreatedSubs.splice(i, 1);
    };
  }

  subscribeTier3DeliberationVoteCast(
    handler: (p: Tier3DeliberationVoteCastPayload) => void,
  ): () => void {
    this.tier3DeliberationVoteCastSubs.push(handler);
    return () => {
      const i = this.tier3DeliberationVoteCastSubs.indexOf(handler);
      if (i >= 0) this.tier3DeliberationVoteCastSubs.splice(i, 1);
    };
  }

  // ─── ZEB-295 Phase 6 — Tier 3 tally-share-applied subscriber ────────
  subscribeTier3TallyShareApplied(
    handler: (p: Tier3TallyShareAppliedPayload) => void,
  ): () => void {
    this.tier3TallyShareAppliedSubs.push(handler);
    return () => {
      const i = this.tier3TallyShareAppliedSubs.indexOf(handler);
      if (i >= 0) this.tier3TallyShareAppliedSubs.splice(i, 1);
    };
  }

  // ─── ZEB-319 — Tier 3 granular drafting-stage subscribers ────────────

  /**
   * ZEB-319: subscribe to mid-stage sortition-decline events.
   * Returns an unlisten function that should be called on teardown.
   */
  subscribeTier3MiniPublicDecline(
    handler: (p: Tier3MiniPublicDeclinePayload) => void,
  ): () => void {
    this.tier3MiniPublicDeclineSubs.push(handler);
    return () => {
      const i = this.tier3MiniPublicDeclineSubs.indexOf(handler);
      if (i >= 0) this.tier3MiniPublicDeclineSubs.splice(i, 1);
    };
  }

  subscribeTier3DraftCandidate(
    handler: (p: Tier3DraftCandidatePayload) => void,
  ): () => void {
    this.tier3DraftCandidateSubs.push(handler);
    return () => {
      const i = this.tier3DraftCandidateSubs.indexOf(handler);
      if (i >= 0) this.tier3DraftCandidateSubs.splice(i, 1);
    };
  }

  subscribeTier3DraftApproval(
    handler: (p: Tier3DraftApprovalPayload) => void,
  ): () => void {
    this.tier3DraftApprovalSubs.push(handler);
    return () => {
      const i = this.tier3DraftApprovalSubs.indexOf(handler);
      if (i >= 0) this.tier3DraftApprovalSubs.splice(i, 1);
    };
  }

  // ─── ZEB-1018 — D-FROST committee ceremony subscribers ──────────────
  subscribeDfrostDkgProgress(
    handler: (p: DfrostDkgProgressPayload) => void,
  ): () => void {
    this.dfrostDkgProgressSubs.push(handler);
    return () => {
      const i = this.dfrostDkgProgressSubs.indexOf(handler);
      if (i >= 0) this.dfrostDkgProgressSubs.splice(i, 1);
    };
  }

  subscribeDfrostBeaconReady(
    handler: (p: DfrostBeaconReadyPayload) => void,
  ): () => void {
    this.dfrostBeaconReadySubs.push(handler);
    return () => {
      const i = this.dfrostBeaconReadySubs.indexOf(handler);
      if (i >= 0) this.dfrostBeaconReadySubs.splice(i, 1);
    };
  }

  subscribeDfrostRefreshProgress(
    handler: (p: DfrostRefreshProgressPayload) => void,
  ): () => void {
    this.dfrostRefreshProgressSubs.push(handler);
    return () => {
      const i = this.dfrostRefreshProgressSubs.indexOf(handler);
      if (i >= 0) this.dfrostRefreshProgressSubs.splice(i, 1);
    };
  }

  subscribeDfrostRepairProgress(
    handler: (p: DfrostRepairProgressPayload) => void,
  ): () => void {
    this.dfrostRepairProgressSubs.push(handler);
    return () => {
      const i = this.dfrostRepairProgressSubs.indexOf(handler);
      if (i >= 0) this.dfrostRepairProgressSubs.splice(i, 1);
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

        // ZEB-292 Phase 3 — delegation events (local + inbound).
        const unlistenDelegationChanged = await adapter.listen(
          'voting-delegation-changed',
          (event) => {
            const payload = event.payload as VotingDelegationChangedPayload;
            for (const sub of [...this.delegationChangedSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenDelegationChanged);

        const unlistenDelegateOnBehalf = await adapter.listen(
          'voting-delegate-signaled-on-your-behalf',
          (event) => {
            const payload = event.payload as VotingDelegateSignaledOnYourBehalfPayload;
            for (const sub of [...this.delegateSignaledOnYourBehalfSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenDelegateOnBehalf);

        // ZEB-310 Phase 4a-main — Tier 3 (Sortition + STAR) events.
        const unlistenTier3PollCreated = await adapter.listen(
          'voting-tier3-poll-created',
          (event) => {
            const payload = event.payload as VotingTier3PollCreatedPayload;
            for (const sub of [...this.tier3PollCreatedSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenTier3PollCreated);

        const unlistenTier3SortitionComplete = await adapter.listen(
          'voting-tier3-sortition-complete',
          (event) => {
            const payload = event.payload as VotingTier3SortitionCompletePayload;
            for (const sub of [...this.tier3SortitionCompleteSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenTier3SortitionComplete);

        const unlistenTier3DraftingOpen = await adapter.listen(
          'voting-tier3-drafting-open',
          (event) => {
            const payload = event.payload as VotingTier3DraftingOpenPayload;
            for (const sub of [...this.tier3DraftingOpenSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenTier3DraftingOpen);

        const unlistenTier3RatificationOpen = await adapter.listen(
          'voting-tier3-ratification-open',
          (event) => {
            const payload = event.payload as VotingTier3RatificationOpenPayload;
            for (const sub of [...this.tier3RatificationOpenSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenTier3RatificationOpen);

        const unlistenTier3Finalized = await adapter.listen(
          'voting-tier3-finalized',
          (event) => {
            const payload = event.payload as VotingTier3FinalizedPayload;
            for (const sub of [...this.tier3FinalizedSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenTier3Finalized);

        // ZEB-294 — Tier 3 deliberation events.
        const unlistenTier3DeliberationStatementCreated = await adapter.listen(
          'voting-tier3-deliberation-statement-created',
          (event) => {
            const payload = event.payload as Tier3DeliberationStatementCreatedPayload;
            for (const sub of [...this.tier3DeliberationStatementCreatedSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenTier3DeliberationStatementCreated);

        const unlistenTier3DeliberationVoteCast = await adapter.listen(
          'voting-tier3-deliberation-vote-cast',
          (event) => {
            const payload = event.payload as Tier3DeliberationVoteCastPayload;
            for (const sub of [...this.tier3DeliberationVoteCastSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenTier3DeliberationVoteCast);

        // ZEB-295 Phase 6 — incremental committee-share-count progress.
        const unlistenTier3TallyShareApplied = await adapter.listen(
          'voting-tier3-tally-share-applied',
          (event) => {
            const payload = event.payload as Tier3TallyShareAppliedPayload;
            for (const sub of [...this.tier3TallyShareAppliedSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenTier3TallyShareApplied);

        // ZEB-319 — granular drafting-stage events.
        const unlistenTier3MiniPublicDecline = await adapter.listen(
          'voting-tier3-mini-public-decline',
          (event) => {
            const payload = event.payload as Tier3MiniPublicDeclinePayload;
            for (const sub of [...this.tier3MiniPublicDeclineSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenTier3MiniPublicDecline);

        const unlistenTier3DraftCandidate = await adapter.listen(
          'voting-tier3-draft-candidate',
          (event) => {
            const payload = event.payload as Tier3DraftCandidatePayload;
            for (const sub of [...this.tier3DraftCandidateSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenTier3DraftCandidate);

        const unlistenTier3DraftApproval = await adapter.listen(
          'voting-tier3-draft-approval',
          (event) => {
            const payload = event.payload as Tier3DraftApprovalPayload;
            for (const sub of [...this.tier3DraftApprovalSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenTier3DraftApproval);

        // ZEB-1018 — D-FROST committee ceremony events. Emitted by the
        // engine on inbound peer applies and by the IPC layer on local
        // applies; same copy-then-iterate delivery as every block above.
        const unlistenDfrostDkgProgress = await adapter.listen(
          DFROST_DKG_PROGRESS,
          (event) => {
            const payload = event.payload as DfrostDkgProgressPayload;
            for (const sub of [...this.dfrostDkgProgressSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenDfrostDkgProgress);

        const unlistenDfrostBeaconReady = await adapter.listen(
          DFROST_BEACON_READY,
          (event) => {
            const payload = event.payload as DfrostBeaconReadyPayload;
            for (const sub of [...this.dfrostBeaconReadySubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenDfrostBeaconReady);

        const unlistenDfrostRefreshProgress = await adapter.listen(
          DFROST_REFRESH_PROGRESS,
          (event) => {
            const payload = event.payload as DfrostRefreshProgressPayload;
            for (const sub of [...this.dfrostRefreshProgressSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenDfrostRefreshProgress);

        const unlistenDfrostRepairProgress = await adapter.listen(
          DFROST_REPAIR_PROGRESS,
          (event) => {
            const payload = event.payload as DfrostRepairProgressPayload;
            for (const sub of [...this.dfrostRepairProgressSubs]) sub(payload);
          },
        );
        stagedUnlisteners.push(unlistenDfrostRepairProgress);

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

  // ─── ZEB-310 Phase 4a-main — Tier 3 (Sortition + STAR) IPC wrappers ────
  // Param names use camelCase per the Tauri snake_case ↔ camelCase boundary
  // convention (see harmony-client/CLAUDE.md). The Rust IPC functions in
  // src-tauri/src/lib.rs declare these as snake_case; Tauri auto-converts.

  /** Create a Tier 3 (Sortition + STAR) governance proposal. Returns the new
   *  PollId as a hex string (32 bytes → 64 chars). */
  async createTier3Proposal(args: CreateTier3ProposalArgs): Promise<string> {
    return this.invoke<string>('voting_create_tier3_proposal', {
      communityId: args.communityId,
      channelId: args.channelId,
      proposalText: args.proposalText,
      sortitionSize: args.sortitionSize,
      deliberationWindowSeconds: args.deliberationWindowSeconds,
      draftingWindowSeconds: args.draftingWindowSeconds,
      ratificationWindowSeconds: args.ratificationWindowSeconds,
      incentiveMode: args.incentiveMode,
      minPower: args.minPower,
      minVouchingDepth: args.minVouchingDepth,
      retryOf: args.retryOf,
      // ZEB-295 Phase 6 Task 11: optional privacy_mode passthrough.
      // Rust IPC accepts Option<String>; undefined here serializes to
      // null and the backend substitutes the "pu" default.
      privacyMode: args.privacyMode,
    });
  }

  /** Submit a deliberation statement (kd=ds) for a Tier 3 poll. Phase 5
   *  scaffold — emits valid events but no clustering yet. Returns event_hash hex. */
  async submitDeliberationStatement(pollId: string, text: string): Promise<string> {
    return this.invoke<string>('voting_submit_deliberation_statement', { pollId, text });
  }

  /** Cast a deliberation vote (kd=dv) on a statement. `vote` is one of
   *  'agree' | 'disagree' | 'pass'. Mini-public only (backend enforces). */
  async castDeliberationVote(
    pollId: string,
    statementEventHash: string,
    vote: DeliberationVoteCode,
  ): Promise<void> {
    await this.invoke<void>('voting_cast_deliberation_vote', {
      pollId,
      statementEventHash,
      vote,
    });
  }

  /** List up to `topN` deliberation statements for a poll, sorted by
   *  bridging score DESC. Returns the full `BridgingScoreExport` rows
   *  including diversity and bridging-score fixed-point strings. */
  async listBridgingStatements(
    pollId: string,
    topN: number = 10,
  ): Promise<BridgingScoreExport[]> {
    return this.invoke<BridgingScoreExport[]>('voting_list_bridging_statements', {
      pollId,
      topN,
    });
  }

  /** Propose a draft candidate (kd=dc). Returns candidate_event_hash hex
   *  for downstream approval references. */
  async proposeDraftCandidate(pollId: string, candidateText: string): Promise<string> {
    return this.invoke<string>('voting_propose_draft_candidate', { pollId, candidateText });
  }

  /** Approve someone else's draft candidate (kd=da). Mini-public only
   *  (verify-side enforces SD1 + candidate-exists). */
  async approveDraftCandidate(pollId: string, candidateEventHash: string): Promise<void> {
    await this.invoke<void>('voting_approve_draft_candidate', { pollId, candidateEventHash });
  }

  /** Decline a sortition invitation (kd=md). Optional 2-char ASCII
   *  alphanumeric reason tag (e.g., "u1", "co"). */
  async declineSortition(pollId: string, reason?: string): Promise<void> {
    await this.invoke<void>('voting_decline_sortition', { pollId, reason });
  }

  /** Cast a ratification ballot (kd=rb). `scores` is one byte per
   *  ratification candidate in `candidateOrdering` (0..=5). */
  async castRatificationBallot(pollId: string, scores: number[]): Promise<void> {
    await this.invoke<void>('voting_cast_ratification_ballot', { pollId, scores });
  }

  /** ZEB-311: Fetch the full state of a single Tier 3 poll by id.
   *  Returns a `Tier3PollExport` including caller-derived my_role / my_*
   *  fields. Errors propagate to the caller. */
  async getTier3Poll(pollId: string): Promise<Tier3PollExport> {
    return this.invoke<Tier3PollExport>('voting_get_tier3_poll', { pollId });
  }

  /** ZEB-311: List all Tier 3 polls in a community, ordered by
   *  PollCreate.hlc descending. Finalized polls are included. */
  async listTier3Polls(communityId: string): Promise<Tier3PollSummary[]> {
    return this.invoke<Tier3PollSummary[]>('voting_list_tier3_polls', { communityId });
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
