import type { TauriAdapter } from './zenoh-service';

/**
 * ZEB-669 S3: frontend binding for the storage-buddy backend (slice 2,
 * PR #449). Mirrors `friend-service.ts` (adapter-based, `connectAdapter` /
 * private `invoke` / `destroy`, event-listener wiring) so it's unit-testable
 * against a mock `TauriAdapter`.
 *
 * IPCs:
 *   - `get_storage_buddies`      → StorageBuddyDto[] (pacts + invites)
 *   - `set_buddy_pledge`         → void (pledging IS the invite/accept;
 *                                   0 bytes is a valid accept)
 *   - `remove_storage_buddy`     → void (doubles as invite decline)
 *   - `set_shared_budget`        → void
 *   - `get_contribution_summary` → ContributionSummaryDto
 *
 * Events (payload-less; receivers re-fetch):
 *   - `storage-buddies-updated` — pledge/removal/backup-flag changes
 *   - `contribution-updated`    — shared-budget changes
 *
 * `onChange` subscribes BOTH: the contribution numerator (hostedBytes) also
 * moves on pledge/backup changes, so a meter listening only to
 * `contribution-updated` would go stale.
 */

/** Mirrors Rust `BuddyStatus` (`#[serde(rename_all = "camelCase")]`). */
export type BuddyStatus = 'active' | 'pendingIncoming' | 'pendingOutgoing';

/** Mirrors Rust `BuddyHealth` — the spec §3 three-state rule, no invented
 *  scoring. */
export type BuddyHealth = 'healthy' | 'catchingUp' | 'overBudget';

/** Mirrors `StorageBuddyDto` in src-tauri/src/lib.rs. */
export interface StorageBuddyDto {
  /** 32-char lowercase hex owner address (same space as
   *  `FriendDto.ownerIdHex`). */
  ownerAddress: string;
  /** ZEB-419 local-only pet-name join; never on the wire. */
  petName: string | null;
  status: BuddyStatus;
  /** Bytes I pledge to them. 0 when the invite is theirs and unaccepted. */
  myPledgeBytes: number;
  /** Bytes they pledge to me; `null` until their pledge list arrives. */
  theirPledgeBytes: number | null;
  /** Ledger truth: bytes I actually hold for them right now. */
  hostedForThemBytes: number;
  /** From their signed hosting report; `null` when no report yet. */
  theyReportHoldingBytes: number | null;
  reportAgeMs: number | null;
}

/** Mirrors `ContributionSummaryDto` in src-tauri/src/lib.rs. */
export interface ContributionSummaryDto {
  hostedBytes: number;
  budgetBytes: number;
  buddyCount: number;
  health: BuddyHealth;
}

export class StorageBuddyService {
  /** Listeners notified when either backend event fires. Set (not a slot) so
   *  the meter and an open manage sheet can subscribe independently. */
  private changeListeners = new Set<() => void>();

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];

  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    this.adapter = adapter;

    const notify = () => {
      // Snapshot before iterating so a listener that unsubscribes itself
      // during notification doesn't mutate the live set mid-loop.
      for (const cb of [...this.changeListeners]) cb();
    };
    this.unlisteners.push(await adapter.listen('storage-buddies-updated', notify));
    this.unlisteners.push(await adapter.listen('contribution-updated', notify));
  }

  /**
   * Register a callback fired when buddies, pledges, backup flags, or the
   * shared budget change. Receivers should re-fetch. Returns an unsubscribe
   * function for component `onDestroy`.
   */
  onChange(cb: () => void): () => void {
    this.changeListeners.add(cb);
    return () => {
      this.changeListeners.delete(cb);
    };
  }

  /** All pacts + pending invites (dismissed invites are suppressed
   *  backend-side). */
  async listBuddies(): Promise<StorageBuddyDto[]> {
    return this.invoke<StorageBuddyDto[]>('get_storage_buddies', {});
  }

  /**
   * Pledge `bytes` of storage to `ownerAddress`. This is both the invite
   * (first pledge toward a non-buddy) and the accept (any pledge toward a
   * pending-incoming inviter — 0 bytes is a valid accept).
   */
  async setPledge(ownerAddress: string, bytes: number): Promise<void> {
    await this.invoke<void>('set_buddy_pledge', { ownerAddress, bytes });
  }

  /** Remove a pact / cancel an outgoing invite / decline an incoming one
   *  (a decline records a local dismissal; a re-issued invite re-surfaces). */
  async removeBuddy(ownerAddress: string): Promise<void> {
    await this.invoke<void>('remove_storage_buddy', { ownerAddress });
  }

  /** Set the enforced shared-budget denominator (bytes). */
  async setSharedBudget(bytes: number): Promise<void> {
    await this.invoke<void>('set_shared_budget', { bytes });
  }

  /** Real hosted-vs-budget totals + the spec §3 health rule. */
  async getContributionSummary(): Promise<ContributionSummaryDto> {
    return this.invoke<ContributionSummaryDto>('get_contribution_summary', {});
  }

  destroy(): void {
    for (const fn of this.unlisteners) {
      // Exception-safe teardown (PendingJoinsPanel safeUnlisten precedent):
      // one throwing unlisten must not leak the rest.
      try {
        fn();
      } catch {
        // ignore — teardown must complete
      }
    }
    this.unlisteners = [];
    this.changeListeners.clear();
    // Null the adapter so connectAdapter's duplicate-init guard doesn't no-op
    // on reconnect after destroy() (mirrors FriendService.destroy()).
    this.adapter = null;
  }

  private async invoke<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
    if (!this.adapter) throw new Error(`StorageBuddyService.${cmd}: adapter not connected`);
    try {
      return (await this.adapter.invoke(cmd, args)) as T;
    } catch (e) {
      // Normalize both production (string) + test (Error) rejection shapes
      // (CLAUDE.md "Tauri IPC error extraction").
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }
}
