import type { TauriAdapter } from './zenoh-service';

/**
 * ZEB-370 Phase 1 + ZEB-371 Phase 1b: frontend friend-graph service. Mirrors
 * `community-service.ts` (adapter-based, `connectAdapter` / private `invoke` /
 * `destroy`, event-listener wiring) so it's unit-testable against a mock
 * `TauriAdapter`.
 *
 * Phase 1 IPCs:
 *   - `generate_friend_token` → a `harmony://friend/...` URL
 *   - `redeem_friend_token`   → adds the inviter as an Active friend
 *   - `list_friends`          → non-Revoked friend rows
 *   - `unfriend`              → writes a Revoked tombstone
 *
 * Phase 1b IPCs:
 *   - `add_friend_by_key`           → AddFriendOutcome (linked/pending/unreachable)
 *   - `list_pending_friend_requests` → pending inbound request rows
 *   - `accept_friend_request`       → void
 *   - `decline_friend_request`      → void
 *   - `set_friend_auto_accept`      → void
 *   - `get_friend_auto_accept`      → boolean
 *
 * Phase 2a IPCs:
 *   - `set_friend_referrable`       → void (toggle a friend's referral-catalog opt-in)
 *
 * Events:
 *   - `friend-list-changed`              → re-fetch friends + pending
 *   - `friend-request-received`          → re-fetch pending
 *   - `connectivity-friend-auto-accept-changed` → auto-accept setting changed
 */

/** Lifecycle of a friend link (mirrors Rust `FriendStatus`). `revoked` rows are
 *  filtered out backend-side, so `list_friends` never returns them. */
export type FriendStatus = 'pending' | 'active' | 'revoked';

/** Provenance of a friend link (mirrors Rust `FriendOrigin`). Phase 1 only ever
 *  produces `token`. */
export type FriendOrigin = 'mutual_key' | 'token' | 'introduction';

/**
 * Mirrors `FriendDto` in src-tauri/src/lib.rs (`#[serde(rename_all =
 * "camelCase")]`). `status` / `establishedVia` arrive as their lowercase
 * string forms.
 */
export interface FriendDto {
  /** The friend's 16-byte master owner_id, hex-encoded (32 chars). */
  ownerIdHex: string;
  display?: string | null;
  status: FriendStatus;
  establishedVia: FriendOrigin;
  referrable: boolean;
}

/** Mirrors `FriendLinkResultDto` in src-tauri/src/lib.rs. */
export interface FriendLinkResultDto {
  ownerIdHex: string;
  display?: string | null;
}

/**
 * A pending inbound friend request (mirrors `PendingFriendRequestDto` in
 * src-tauri). Delivered by `list_pending_friend_requests`.
 */
export interface PendingFriendRequestDto {
  ownerIdHex: string;
  display: string | null;
  receivedAtMs: number;
}

/**
 * Outcome of `add_friend_by_key`. The backend returns an externally-tagged
 * enum:
 *   - `{ linked: { ownerIdHex, display } }` → the peer was immediately linked
 *   - `"pending"` → a request was sent; the peer needs to accept
 *   - `"unreachable"` → the peer's DHT record could not be resolved
 *
 * We normalise all three into a discriminated union with a `kind` field.
 */
export type AddFriendOutcome =
  | { kind: 'linked'; ownerIdHex: string; display: string | null }
  | { kind: 'pending' }
  | { kind: 'unreachable' };

/** Raw shape of the externally-tagged `AddFriendOutcome` from the backend. */
type RawAddFriendOutcome =
  | { linked: { ownerIdHex: string; display: string | null } }
  | 'pending'
  | 'unreachable';

export class FriendService {
  /** Listeners notified when the backend emits `friend-list-changed` (a friend
   *  was added or removed, possibly on another device). A registry (not a
   *  single slot) so multiple `FriendsPanel` instances can each subscribe
   *  without stomping one another — see `onFriendsChanged`. */
  private friendsChangedListeners = new Set<() => void>();

  /**
   * Listeners notified when the pending-requests list changes. Fired on
   * `friend-request-received` (new inbound) and `friend-list-changed`
   * (accept/decline also mutates the pending list). Mirror of
   * `friendsChangedListeners` — same multi-listener Set pattern.
   */
  private pendingRequestsChangedListeners = new Set<() => void>();

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];

  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    this.adapter = adapter;

    const unlistenChanged = await adapter.listen('friend-list-changed', () => {
      // Snapshot before iterating so a listener that unsubscribes itself
      // during notification doesn't mutate the live set mid-loop.
      for (const cb of [...this.friendsChangedListeners]) cb();
      // friend-list-changed also affects the pending list (accept/decline).
      for (const cb of [...this.pendingRequestsChangedListeners]) cb();
    });
    this.unlisteners.push(unlistenChanged);

    const unlistenRequestReceived = await adapter.listen('friend-request-received', () => {
      for (const cb of [...this.pendingRequestsChangedListeners]) cb();
    });
    this.unlisteners.push(unlistenRequestReceived);
  }

  /**
   * Register a callback fired when the backend emits `friend-list-changed`
   * (a friend was added or removed, possibly on another device). Receivers
   * should re-fetch `listFriends()`. Returns an unsubscribe function; call it
   * (e.g. in a component's `onDestroy`) to remove ONLY this listener without
   * disturbing others. Multiple subscribers are supported.
   */
  onFriendsChanged(cb: () => void): () => void {
    this.friendsChangedListeners.add(cb);
    return () => {
      this.friendsChangedListeners.delete(cb);
    };
  }

  /** List the local owner's active (non-Revoked) friends. */
  async listFriends(): Promise<FriendDto[]> {
    return this.invoke<FriendDto[]>('list_friends', {});
  }

  /**
   * Mint a `harmony://friend/...` URL to share. `expiresAt` is an optional
   * absolute wall-clock ms deadline bound into the token signature; omitted →
   * `null` (no app-level expiry beyond the backend default).
   */
  async generateFriendToken(expiresAt?: number): Promise<string> {
    return this.invoke<string>('generate_friend_token', {
      expiresAt: expiresAt ?? null,
    });
  }

  /** Redeem a pasted friend URL — runs the outbound friend handshake and adds
   *  the inviter as an Active friend. */
  async redeemFriendToken(url: string): Promise<FriendLinkResultDto> {
    return this.invoke<FriendLinkResultDto>('redeem_friend_token', { url });
  }

  /** Unfriend a peer by their 16-byte master owner_id hex (writes a Revoked
   *  tombstone). The IPC param is `peer_addr` → camelCased `peerAddr`. */
  async unfriend(ownerIdHex: string): Promise<void> {
    await this.invoke<void>('unfriend', { peerAddr: ownerIdHex });
  }

  // ── Phase 1b ─────────────────────────────────────────────────────────────

  /**
   * Register a callback fired when the pending-requests list changes (a new
   * inbound request arrived via `friend-request-received`, or the list was
   * mutated by accept/decline via `friend-list-changed`). Returns an
   * unsubscribe function. Multiple subscribers are supported.
   */
  onPendingRequestsChanged(cb: () => void): () => void {
    this.pendingRequestsChangedListeners.add(cb);
    return () => {
      this.pendingRequestsChangedListeners.delete(cb);
    };
  }

  /** List pending inbound friend requests (not yet accepted or declined). */
  async listPendingRequests(): Promise<PendingFriendRequestDto[]> {
    return this.invoke<PendingFriendRequestDto[]>('list_pending_friend_requests', {});
  }

  /** Accept an inbound friend request by the sender's owner_id hex. */
  async acceptRequest(ownerIdHex: string): Promise<void> {
    await this.invoke<void>('accept_friend_request', { ownerIdHex });
  }

  /** Decline an inbound friend request by the sender's owner_id hex. */
  async declineRequest(ownerIdHex: string): Promise<void> {
    await this.invoke<void>('decline_friend_request', { ownerIdHex });
  }

  /**
   * Attempt to add a friend by their 64-byte transport identity public key
   * (hex). The backend resolves the peer's DHT record and either:
   *   - immediately links them (both sides already trust each other) → `linked`
   *   - sends them an inbound request to accept later → `pending`
   *   - can't reach their DHT record → `unreachable`
   *
   * Arg naming mirrors `connectivity_discover_identity` (`identityPubHex`).
   */
  async addByKey(identityPubHex: string): Promise<AddFriendOutcome> {
    const raw = await this.invoke<RawAddFriendOutcome>('add_friend_by_key', { identityPubHex });
    return FriendService.parseAddFriendOutcome(raw);
  }

  private static parseAddFriendOutcome(raw: RawAddFriendOutcome): AddFriendOutcome {
    if (raw === 'pending') return { kind: 'pending' };
    if (raw === 'unreachable') return { kind: 'unreachable' };
    // Externally-tagged object variant: { linked: { ownerIdHex, display } }.
    // Validate the inner payload's field types — this parser is the runtime
    // guard at the IPC boundary, so a malformed `{ linked: {} }` or
    // `{ linked: { ownerIdHex: 7 } }` must be rejected, not coerced into an
    // invalid AddFriendOutcome.
    if (raw && typeof raw === 'object' && 'linked' in raw && raw.linked && typeof raw.linked === 'object') {
      const linked = raw.linked as Record<string, unknown>;
      if (
        typeof linked.ownerIdHex === 'string' &&
        (linked.display === undefined || linked.display === null || typeof linked.display === 'string')
      ) {
        return { kind: 'linked', ownerIdHex: linked.ownerIdHex, display: linked.display ?? null };
      }
    }
    throw new Error(`unexpected add_friend_by_key outcome: ${JSON.stringify(raw)}`);
  }

  /** Enable or disable auto-accepting friend requests from known peers. */
  async setAutoAccept(enabled: boolean): Promise<void> {
    await this.invoke<void>('set_friend_auto_accept', { enabled });
  }

  /** Retrieve the current auto-accept setting. */
  async getAutoAccept(): Promise<boolean> {
    return this.invoke<boolean>('get_friend_auto_accept', {});
  }

  /**
   * Toggle whether a friend (by their 16-byte master owner_id hex) may be
   * surfaced in our referral catalog to others (the ZEB-375 Phase 2a sharer-side
   * opt-in). The backend stamps a fresh HLC and re-syncs to the user's other
   * devices; the UI refreshes on the resulting `friend-list-changed` event.
   */
  async setReferrable(ownerIdHex: string, referrable: boolean): Promise<void> {
    await this.invoke<void>('set_friend_referrable', { ownerIdHex, referrable });
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
    this.friendsChangedListeners.clear();
    this.pendingRequestsChangedListeners.clear();
    // Null the adapter so connectAdapter's duplicate-init guard doesn't no-op
    // on reconnect after destroy() (mirrors CommunityService.destroy()).
    this.adapter = null;
  }

  private async invoke<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
    if (!this.adapter) throw new Error(`FriendService.${cmd}: adapter not connected`);
    try {
      return (await this.adapter.invoke(cmd, args)) as T;
    } catch (e) {
      // Normalize both production (string) + test (Error) rejection shapes
      // (CLAUDE.md "Tauri IPC error extraction").
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }
}
