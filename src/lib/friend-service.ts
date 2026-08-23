import type { TauriAdapter } from './zenoh-service';
import type { Profile } from './types';
import { nonEmpty } from './display-label';

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
 *   - `browse_friend_referrals`     → ReferralView[] (fetch a verified friend referral catalog)
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

/** Inbound-introduction policy (mirrors Rust `friend_graph::PeerIntroPolicy`).
 *  Enforced on THIS node as the *target* of an introduction: governs whether —
 *  and how — it accepts an inbound `Introduction` in which a friend vouches for
 *  a stranger. `open` accepts any; `fof` accepts only when the voucher is an
 *  active friend; `ask` stages a prompt; `closed` rejects. (It does not govern
 *  this node's own broker behavior when relaying an `IntroduceRequest`.) */
export type PeerIntroPolicy = 'open' | 'fof' | 'ask' | 'closed';

/**
 * Mirrors `FriendDto` in src-tauri/src/lib.rs (`#[serde(rename_all =
 * "camelCase")]`). `status` / `establishedVia` arrive as their lowercase
 * string forms.
 */
export interface FriendDto {
  /** The friend's 16-byte master owner_id, hex-encoded (32 chars). */
  ownerIdHex: string;
  display?: string | null;
  /** ZEB-419: local-only personal nickname (joined from `friend_nicknames`,
   *  never the published CRDT). `null`/absent when the user hasn't set one. */
  nickname?: string | null;
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
  /** ZEB-376 Task 11 (AskMe): the voucher (F) owner hex when this row is an
   *  AskMe-staged introduction offer; `null` for a plain Path-A link request.
   *  Lets the UI badge the row as "introduced by …". */
  introducedBy: string | null;
}

/**
 * One of THIS user's own outbound friend requests that hasn't been answered
 * yet (mirrors `OutboundFriendRequestDto` in src-tauri). Delivered by
 * `list_outbound_friend_requests` — ZEB-783.
 *
 * There is deliberately no owner id or display name: a peer who hasn't
 * accepted has disclosed neither, so the only thing we can honestly show is
 * the key the user typed.
 */
export interface OutboundFriendRequestDto {
  /** The target's 64-byte transport identity pub, hex (what the user typed). */
  identityPubHex: string;
  /** Epoch-ms the request was sent. */
  requestedAtMs: number;
  /** Epoch-ms the node stops retrying this request. */
  expiresAtMs: number;
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

/**
 * One referral entry surfaced by `browse_friend_referrals` (mirrors Rust
 * `ReferralView`, `#[serde(rename_all = "camelCase")]`). The backend has already
 * VERIFIED the catalog signature + author/subject binding before projecting, so
 * each entry is an authenticated referral from the friend we asked.
 * `alreadyFriend` is true when this peer is already an Active/Pending friend of
 * ours (the UI flags it; there is no introduction action in Phase 2a).
 */
export interface ReferralView {
  /** The referred peer's 16-byte master owner_id, hex-encoded. */
  ownerIdHex: string;
  /** The author's display-name hint for this peer, if any. */
  display: string | null;
  /** Whether we already have this peer as an Active/Pending friend. */
  alreadyFriend: boolean;
}

/**
 * ZEB-431: derive the DM contact-picker entries from the friend graph.
 *
 * The picker previously rendered `navService.profiles`, which is fed only by
 * live zenoh `profile-update` presence broadcasts AND is keyed by Reticulum
 * device identity hash — the wrong identifier class for `add_space`, whose
 * `members[]` are 16-byte master `OwnerAddr`s (both are 16 bytes, so the
 * mismatch decoded silently). Active friends are the correct contact set:
 * `ownerIdHex` is exactly the `OwnerAddr` the DM path resolves through
 * `OwnerDeviceCache`, and the SP2 butler deposit acceptor requires
 * friend-Active admission anyway, so a non-friend entry could never receive.
 *
 * Pending friends are excluded (no DM until the request is accepted).
 * Display precedence: local nickname (ZEB-419) → published display name →
 * short-hex prefix (which also keeps the entry findable by typing the hex).
 * Empty strings are treated as absent.
 */
export function contactsFromFriends(friends: FriendDto[]): Map<string, Profile> {
  const contacts = new Map<string, Profile>();
  for (const f of friends) {
    if (f.status !== 'active') continue;
    const shortHex =
      f.ownerIdHex.length > 8 ? f.ownerIdHex.slice(0, 8) + '…' : f.ownerIdHex;
    contacts.set(f.ownerIdHex, {
      address: f.ownerIdHex,
      // ZEB-962: `nonEmpty` (not `||`) so a whitespace-only nickname/display
      // is treated as absent — `||` drops only `""`, letting `"   "` bake in.
      displayName: nonEmpty(f.nickname) ?? nonEmpty(f.display) ?? shortHex,
    });
  }
  return contacts;
}

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
   * Mint a `harmony://friend/...` URL to share. `ttlMs` is an optional TTL
   * *duration* in milliseconds; the backend derives the absolute expiry as
   * `now + ttlMs` and binds it into the token signature (ZEB-507 — callers no
   * longer supply an absolute epoch, so a wrong-unit value can't mint a
   * dead-on-arrival token). Omitted → `null` (no app-level expiry).
   */
  async generateFriendToken(ttlMs?: number): Promise<string> {
    return this.invoke<string>('generate_friend_token', {
      ttlMs: ttlMs ?? null,
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
   * ZEB-783: list this user's OWN unanswered outbound requests — the mirror of
   * `listPendingRequests`. The node retries each one until the target accepts,
   * so a row here means "sent, still waiting on them", not "stuck".
   */
  async listOutboundRequests(): Promise<OutboundFriendRequestDto[]> {
    return this.invoke<OutboundFriendRequestDto[]>('list_outbound_friend_requests', {});
  }

  /**
   * ZEB-783: stop retrying an outbound request and drop it from the list.
   * Purely local — nothing is sent to the target. Idempotent.
   */
  async cancelOutboundRequest(identityPubHex: string): Promise<void> {
    await this.invoke<void>('cancel_outbound_friend_request', { identityPubHex });
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

  /**
   * Returns the local node's 64-byte transport identity pub as 128 lowercase
   * hex chars — the value a peer pastes into "Add friend by key". `null` when
   * the node isn't started yet (owner identity not loaded). ZEB-388.
   */
  async getMyIdentityPubHex(): Promise<string | null> {
    return this.invoke<string | null>('connectivity_get_my_identity_pub_hex', {});
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

  /**
   * ZEB-977 (was ZEB-419's `set_friend_nickname`): set (or clear, with
   * `null`) the LOCAL petname for this friend — now backed by the contacts
   * dataset (`set_contact_petname`), so it works for any identity and syncs
   * across the owner's own devices (never to the peer). The backend emits
   * `friend-list-changed`, so a subscribed panel re-fetches `listFriends()`
   * and re-renders with the new label. Kept on FriendService so FriendsPanel's
   * existing editor keeps its one-service dependency; ContactsService is the
   * general-purpose surface.
   */
  async setNickname(ownerIdHex: string, nickname: string | null): Promise<void> {
    await this.invoke<void>('set_contact_petname', { ownerIdHex, petname: nickname });
  }

  /**
   * Browse an Active friend's referral catalog (by their 16-byte master
   * owner_id hex). The backend resolves the friend via their Case-D slot, dials
   * the `harmony/friend-pex/v1` ALPN, sends a signed `CatalogRequest`, VERIFIES
   * the returned `ReferralCatalog` (author == the friend, subject == us), and
   * projects each entry into a `ReferralView`. A verify failure or unreachable
   * friend rejects the whole call (no partial trust). Read-only: Phase 2a has no
   * "request introduction" action.
   */
  async browseReferrals(ownerIdHex: string): Promise<ReferralView[]> {
    return this.invoke<ReferralView[]>('browse_friend_referrals', { ownerIdHex });
  }

  // ── Phase 2b: introduction broker ───────────────────────────────────────

  /** Read the current inbound-introduction policy. */
  async getPeerIntroPolicy(): Promise<PeerIntroPolicy> {
    return this.invoke<PeerIntroPolicy>('get_peer_intro_policy', {});
  }

  /** Set the inbound-introduction policy (applies live on the next introduction). */
  async setPeerIntroPolicy(policy: PeerIntroPolicy): Promise<void> {
    await this.invoke<void>('set_peer_intro_policy', { policy });
  }

  /** Ask a friend (`viaOwnerIdHex`) to introduce us to one of their referrable
   *  friends (`targetOwnerIdHex`). The eventual link surfaces via friend-list-changed. */
  async requestIntroduction(viaOwnerIdHex: string, targetOwnerIdHex: string): Promise<void> {
    await this.invoke<void>('request_introduction', { viaOwnerIdHex, targetOwnerIdHex });
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
