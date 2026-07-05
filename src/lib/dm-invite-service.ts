import type { TauriAdapter } from './zenoh-service';

/**
 * ZEB-236: frontend DM-invite service. Structural clone of
 * `friend-service.ts`'s `connectAdapter` / listener-Set fan-out / tracked
 * unlisteners / private `invoke` wrapper (`FriendService`, lines ~161-360), so
 * it's unit-testable against a mock `TauriAdapter`.
 *
 * IPCs:
 *   - `list_pending_dm_invites` → pending inbound DM-invite rows
 *   - `accept_dm_invite`        → void (arg: `spaceId`)
 *   - `decline_dm_invite`       → void (arg: `spaceId`)
 *
 * Events:
 *   - `dm-invite-received`      → a new inbound DM invite arrived
 *   - `dm-invite-list-changed`  → the pending list was mutated (accept/decline,
 *                                 possibly from another device)
 *
 * Both events re-fetch the same pending list, so `onPendingChanged` fans out
 * to a single listener registry for both.
 */

/**
 * A pending inbound DM invite (mirrors `PendingDmInviteDto` in src-tauri,
 * `#[serde(rename_all = "camelCase")]`).
 */
export interface PendingDmInviteDto {
  /** The invite's space_id, hex-encoded. */
  spaceIdHex: string;
  /** The inviter's 16-byte master owner_id, hex-encoded. */
  inviterOwnerIdHex: string;
  /**
   * SpaceKind wire tag: `'d'` (Dm) or `'g'` (GroupDm) — the raw serde
   * discriminant from `SpaceKind` in owner_state_types.rs. Surfaces map it to a
   * human label (see `DmInviteToast.svelte` / `FriendsPanel.svelte`).
   */
  kind: 'd' | 'g';
  memberOwnerIdsHex: string[];
  createdAtMs: number;
  receivedAtMs: number;
}

export class DmInviteService {
  /** Listeners notified when the pending DM-invite list changes — fired on
   *  both `dm-invite-received` (new inbound) and `dm-invite-list-changed`
   *  (accept/decline mutated the list). A registry (not a single slot) so
   *  multiple subscribers can each listen without stomping one another. */
  private pendingChangedListeners = new Set<() => void>();

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];

  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    this.adapter = adapter;

    try {
      const unlistenReceived = await adapter.listen('dm-invite-received', () => {
        // Snapshot before iterating so a listener that unsubscribes itself
        // during notification doesn't mutate the live set mid-loop.
        for (const cb of [...this.pendingChangedListeners]) cb();
      });
      this.unlisteners.push(unlistenReceived);

      const unlistenListChanged = await adapter.listen('dm-invite-list-changed', () => {
        for (const cb of [...this.pendingChangedListeners]) cb();
      });
      this.unlisteners.push(unlistenListChanged);
    } catch (e) {
      // Roll back the half-wired state (adapter was assigned before the guard
      // above could see a failure) so a later connectAdapter can retry cleanly.
      // Deliberately NOT destroy(): that would also wipe pendingChangedListeners
      // registered before connect, silencing subscribers across the retry.
      for (const fn of this.unlisteners) fn();
      this.unlisteners = [];
      this.adapter = null;
      throw e;
    }
  }

  /**
   * Register a callback fired when the pending DM-invite list changes (new
   * invite received, or accept/decline mutated the list, possibly on another
   * device). Returns an unsubscribe function; call it (e.g. in a component's
   * `onDestroy`) to remove ONLY this listener without disturbing others.
   * Multiple subscribers are supported.
   */
  onPendingChanged(cb: () => void): () => void {
    this.pendingChangedListeners.add(cb);
    return () => {
      this.pendingChangedListeners.delete(cb);
    };
  }

  /** List pending inbound DM invites (not yet accepted or declined). */
  async listPending(): Promise<PendingDmInviteDto[]> {
    return this.call<PendingDmInviteDto[]>('list_pending_dm_invites', {});
  }

  /** Accept a pending DM invite by its space_id hex. */
  async accept(spaceIdHex: string): Promise<void> {
    await this.call<void>('accept_dm_invite', { spaceId: spaceIdHex });
  }

  /** Decline a pending DM invite by its space_id hex. */
  async decline(spaceIdHex: string): Promise<void> {
    await this.call<void>('decline_dm_invite', { spaceId: spaceIdHex });
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
    this.pendingChangedListeners.clear();
    // Null the adapter so connectAdapter's duplicate-init guard doesn't no-op
    // on reconnect after destroy() (mirrors FriendService.destroy()).
    this.adapter = null;
  }

  private async call<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
    if (!this.adapter) throw new Error(`DmInviteService.${cmd}: adapter not connected`);
    try {
      return (await this.adapter.invoke(cmd, args)) as T;
    } catch (e) {
      // Normalize both production (string) + test (Error) rejection shapes
      // (CLAUDE.md "Tauri IPC error extraction").
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }
}
