import type { TauriAdapter } from './zenoh-service';

/**
 * Mirrors `profile_broadcast::DiscoveredProfileInfo` IPC return shape
 * (ZEB-281 Sub-D Phase 4). The Rust DTO uses `#[serde(rename_all =
 * "camelCase")]`, so wire keys are camelCase (matching
 * `DiscoveredLibraryInfo` from Phase 2). `sharedAt` is a base-10 string
 * of `shared_at.wall_ms` for display only — callers MUST NOT use this
 * for HLC ordering decisions.
 */
export interface ProfileMembershipBroadcastInfo {
  /** Hex-encoded 16-byte OwnerAddr (32 hex chars). */
  ownerAddr: string;
  /** Hex-encoded SpaceIds (32 hex chars each). */
  communityIds: string[];
  /** `shared_at.wall_ms` as base-10 string for display only. */
  sharedAt: string;
}

/**
 * Thin IPC wrapper for the Sub-D Phase 4 profile-broadcast IPCs. Mirrors
 * the `library-directory-service.ts` shape: constructor takes a
 * `TauriAdapter`, each method translates JS-side camelCase arg names to
 * the Rust snake_case IPC parameter names that Tauri rewrites at the
 * boundary.
 *
 * Error extraction: production rejections are strings; tests use Error
 * objects with `"Error: "` prefix. Callers should wrap invocations with
 * `e instanceof Error ? e.message : String(e)` if they need to surface
 * the message to UI (per CLAUDE.md "Tauri IPC error extraction").
 */
export class ProfileBroadcastService {
  constructor(private adapter: TauriAdapter) {}

  /** Toggle per-community opt-in. Server-side mutates `Space.shared_in_profile`,
   *  bumps `Space.updated_at`, notifies the publisher. */
  async setShared(communityId: string, shared: boolean): Promise<void> {
    await this.adapter.invoke('set_space_shared_in_profile', {
      communityId,
      shared,
    });
  }

  /** Subscribe to a peer's broadcast topic. Returns a u64 handle the
   *  caller passes to subsequent unsubscribe / getCached calls. */
  async subscribe(peerAddr: string): Promise<number> {
    return (await this.adapter.invoke('subscribe_peer_profile', {
      peerAddr,
    })) as number;
  }

  /** Cancel a subscription. Idempotent server-side. */
  async unsubscribe(subscriptionId: number): Promise<void> {
    await this.adapter.invoke('unsubscribe_peer_profile', {
      subscriptionId,
    });
  }

  /** Retrieve the latest verified broadcast for a subscription, or null
   *  if none has arrived yet. */
  async getCached(
    subscriptionId: number,
  ): Promise<ProfileMembershipBroadcastInfo | null> {
    return (await this.adapter.invoke('get_cached_peer_profile', {
      subscriptionId,
    })) as ProfileMembershipBroadcastInfo | null;
  }
}
