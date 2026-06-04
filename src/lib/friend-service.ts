import type { TauriAdapter } from './zenoh-service';

/**
 * ZEB-370 Phase 1: frontend friend-graph service. Mirrors
 * `community-service.ts` (adapter-based, `connectAdapter` / private `invoke` /
 * `destroy`, event-listener wiring) so it's unit-testable against a mock
 * `TauriAdapter`.
 *
 * Backed by the four friend IPCs in `src-tauri/src/lib.rs`:
 *   - `generate_friend_token` → a `harmony://friend/...` URL
 *   - `redeem_friend_token`   → adds the inviter as an Active friend
 *   - `list_friends`          → non-Revoked friend rows
 *   - `unfriend`              → writes a Revoked tombstone
 *
 * The backend emits `friend-list-changed` (no payload) after a redeem / accept
 * / unfriend; subscribers re-fetch `listFriends()` on receipt.
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

export class FriendService {
  /** Fired when the backend emits `friend-list-changed` (a friend was added or
   *  removed, possibly on another device). Receivers should re-fetch
   *  `listFriends()`. */
  onFriendsChanged?: () => void;

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];

  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    this.adapter = adapter;

    const unlistenChanged = await adapter.listen('friend-list-changed', () => {
      this.onFriendsChanged?.();
    });
    this.unlisteners.push(unlistenChanged);
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

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
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
