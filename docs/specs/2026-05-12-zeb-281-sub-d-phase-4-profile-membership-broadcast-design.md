# ZEB-281 Sub-D Phase 4 — ProfileMembershipBroadcast Primitive

**Status:** Design approved 2026-05-12.

**Parent:** [ZEB-218](https://linear.app/zeblith/issue/ZEB-218/) Sub-D library-federated discovery directory.

**Predecessors:**
- Phase 1 vertical slice: PR [#108](https://github.com/zeblithic/harmony-client/pull/108).
- Phase 2 auto-discovery: PR [#109](https://github.com/zeblithic/harmony-client/pull/109).
- Phase 3 federated republication: PR [#110](https://github.com/zeblithic/harmony-client/pull/110).

## 1. Goal

Add the **third independent Sub-D discovery primitive** (alongside library directory and library auto-discovery — Phase 3 was a cryptographic enhancement of library directory, not a new primitive): a privacy-preserving mechanism where users curate a per-community-opt-in subset of their memberships and broadcast on a per-owner Zenoh topic. Peers viewing a user's profile can see ONLY the communities the user explicitly chose to share — never their full membership list.

The primitive completes Sub-D's discovery layer:

1. **Library directory** (Phase 1): paste a library address to surface its catalog.
2. **Library auto-discovery** (Phase 2): trusted libraries self-announce on a global topic.
3. **Federated republication** (Phase 3): libraries can re-syndicate each others' entries with cryptographic attestation.
4. **Profile membership broadcast** (this round): users opt-in per-community to share which communities they're in.

After Phase 4, "how do users find communities?" has three answers: paste-an-address, library catalogs, and peer-graph discovery via shared memberships.

## 2. Why this shape

**Privacy-protective by default.** A fresh-install user produces ZERO broadcasts. Their address is indistinguishable from a non-Harmony address at the broadcast layer. First publication only happens after an explicit opt-in.

**Per-community opt-in.** The opt-in is a per-`Space` boolean (`Space.shared_in_profile`), not a global "share my memberships" flag. Users curate exactly which communities are visible — selective rather than all-or-nothing.

**Self-sovereign + polycentric.** No third-party escrow; the owner publishes via their own Zenoh node. No platform admin; the broadcast is the user's signed statement about themselves, verified by recipients. No global moderation; opting out is unilateral and ROTATES the prior broadcast.

**Owner-state CRDT integration.** Opt-in flags live on the existing `Space` struct, replicating across the owner's bound devices via the existing CRDT sync. Opt-in on one device shows on all.

## 3. Architecture overview

```text
┌────────────────────────────────────────────────────────────────┐
│ Owner device (publisher)                                       │
│                                                                │
│  CommunitySettingsPanel.svelte → toggle                        │
│                ↓                                               │
│  IPC: set_space_shared_in_profile(community_id, shared)        │
│                ↓                                               │
│  owner_state mutation: Space.shared_in_profile = shared        │
│  Space.updated_at = bump_hlc()                                 │
│                ↓                                               │
│  profile_broadcast::Publisher (notify channel)                 │
│                ↓                                               │
│  Debounce 2s                                                   │
│                ↓                                               │
│  Compute opted-in set from OwnerState.spaces                   │
│                ↓                                               │
│  If set is empty AND we've never published before:             │
│      skip (privacy default).                                   │
│  Else:                                                         │
│      Sign canonical CBOR with signature=[0;64], bump HLC.      │
│      Zenoh PUT to harmony/discovery/profile/{addr}/memberships │
│                                                                │
│  Periodic refresh: 10-min timer republishes if any communities │
│  opted in (defeats Zenoh PUT non-stickiness for late subs).    │
└────────────────────────────────────────────────────────────────┘
                              │
                              ▼  (Zenoh)
┌────────────────────────────────────────────────────────────────┐
│ Peer device (subscriber)                                       │
│                                                                │
│  ProfilePopover.svelte mounts for peer X                       │
│                ↓                                               │
│  IPC: subscribe_peer_profile(peer_x_addr) → SubscriptionId     │
│                ↓                                               │
│  profile_broadcast::Subscriber spawns tokio task               │
│  subscribes harmony/discovery/profile/{x_addr}/memberships     │
│                ↓                                               │
│  On sample:                                                    │
│    verify_broadcast(broadcast) →                               │
│      1. bounds (≤200 communities)                              │
│      2. sorted+deduped community_ids                           │
│      3. parse owner_identity_pub                               │
│      4. verify Ed25519 sig (canonical CBOR, sig zeroed)        │
│    attribution check: derived addr == topic owner              │
│    replay defense: HLC strictly newer than cached              │
│    cache: Mutex<Option<VerifiedBroadcast>>                     │
│    emit: profile-broadcast-received event                      │
│                                                                │
│  Frontend polls IPC: get_cached_peer_profile(id)               │
│  → renders communityIds list (or "no memberships shared")      │
│                                                                │
│  ProfilePopover close:                                         │
│    IPC: unsubscribe_peer_profile(id) — cancels task, drops sub │
└────────────────────────────────────────────────────────────────┘
```

The protocol is consumer-side per-peer. Each peer's profile is its own subscription, scoped to the popover's lifetime. No persistent subscription. No third-party intermediary.

## 4. Data model

### 4.1 Wire format — `ProfileMembershipBroadcast`

New struct in new module `src-tauri/src/profile_broadcast.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileMembershipBroadcast {
    /// 64-byte identity bundle (X25519_pub || Ed25519_pub) of the
    /// owner publishing this broadcast. Same shape as Phase 1+2 +
    /// admin identity bundles in Sub-D.
    #[serde(
        rename = "ai",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub owner_identity_pub: [u8; 64],

    /// Sorted, strictly-increasing (no duplicates) subset of the
    /// owner's joined community SpaceIds that they have opted to
    /// share publicly. MAY be empty (used to rotate prior non-empty
    /// broadcasts when the owner opts out of their last community).
    /// Hard cap: MAX_SHARED_COMMUNITIES = 200.
    #[serde(rename = "cs")]
    pub community_ids: Vec<SpaceId>,

    /// Hybrid Logical Clock — recipients prefer newer broadcasts over
    /// older ones; publisher rotates stale state by bumping the HLC.
    #[serde(rename = "sa")]
    pub shared_at: Hlc,

    /// Ed25519 sig over canonical CBOR with `signature` zeroed.
    /// Same idiom as LibraryAnnounce (Phase 2) and LibraryDirectoryEntry
    /// admin sig (Phase 1).
    #[serde(
        rename = "sg",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub signature: [u8; 64],
}
```

**Field-key choices:** `ai`, `cs`, `sa`, `sg` — all 2-char (preserves `canonical_cbor_encode`'s same-length-keys precondition). Distinct from existing Phase 1-3 wire types.

**Caps:** `MAX_SHARED_COMMUNITIES = 200`. Worst-case payload at 200 SpaceIds × 32 bytes + framing + sig ≈ 6.5 KB. Generous bound; power users rarely exceed it.

**Topic namespace:** `harmony/discovery/profile/{owner_addr_hex}/memberships`.

The original Sub-D design at `docs/specs/2026-04-30-zeb-206-nav-tree-design.md` L235-246 specified `harmony/announce/{owner_addr}/memberships`. **We renamed** because `harmony/announce/{cid_hex}` is already used by the storage tier's CID content-availability protocol (`src-tauri/src/lib.rs::parse_content_announcement` at line 4235). The new `harmony/discovery/profile/...` namespace nests under the existing Sub-D `harmony/discovery/library/...` family — consistent grouping, no collision.

### 4.2 Owner-state — `Space.shared_in_profile`

A new field on the existing `Space` struct in `src-tauri/src/owner_state_types.rs`:

```rust
pub struct Space {
    // ... existing fields (id, kind, parent, community_id, name, ...) ...

    /// Sub-D Phase 4 (ZEB-281): opt-in flag for including this Space's
    /// `community_id` in the owner's ProfileMembershipBroadcast.
    /// Default `false` (no communities shared until user explicitly
    /// opts in). Replicated across the owner's bound devices via the
    /// existing owner-state CRDT sync — opting in on one device shows
    /// on all of them.
    ///
    /// Only meaningful for `kind == community`. The invariant check
    /// rejects `true` on DM/group-DM/profile/other Space kinds.
    #[serde(rename = "sp", default, skip_serializing_if = "core::ops::Not::not")]
    pub shared_in_profile: bool,
}
```

**Wire-compat:**
- `default` makes existing persisted state deserialize cleanly (old Spaces had no `sp` field → defaults to `false`).
- `skip_serializing_if = "core::ops::Not::not"` (skips when value is `false`) means non-opted-in Spaces encode byte-identically to pre-Phase-4 Spaces. Existing wire-format pinning fixtures for `Space` / `OwnerState` MUST remain byte-identical for the default-false case.

**Validation:** `validate_invariants` gains a rule — `shared_in_profile` may be `true` only when `kind == community`. Rejects malformed peers attempting to set it on DMs/profiles/groups.

**Mutation:** new IPC `set_space_shared_in_profile(community_id: SpaceId, shared: bool)`. Flips the field, bumps `Space.updated_at` via existing HLC source, notifies the profile broadcaster.

### 4.3 Attestation — `AttestationStatus`-style outcome

`verify_broadcast` returns `Result<OwnerAddr, BroadcastVerifyError>` — the derived owner address on success (caller compares against topic owner for attribution check). Unlike Phase 3's `AttestationStatus` enum (which had Unwrapped / Attested / Unattested for handling Phase 1-back-compat + tamper cases), Phase 4 broadcasts have a single trust path: the sig either verifies or it doesn't. Invalid sigs are dropped, not surfaced — there is no "unattested badge" equivalent because there's no admin-sig-as-fallback to trust.

```rust
#[derive(Debug, thiserror::Error)]
pub enum BroadcastVerifyError {
    #[error("community_ids exceeds {MAX_SHARED_COMMUNITIES} entries")]
    TooManyCommunities,
    #[error("community_ids must be strictly increasing (sorted + deduped)")]
    CommunityIdsNotSortedDeduped,
    #[error("malformed owner identity_pub: {0}")]
    InvalidIdentityPub(String),
    #[error("Ed25519 signature verification failed")]
    SignatureInvalid,
    #[error("canonical CBOR encode failed: {0}")]
    Encode(#[from] crate::owner_state_crypto::CryptoError),
}
```

## 5. Publisher lifecycle

`profile_broadcast::Publisher` (held in `NodeState`) owns the publishing side. Three triggers:

1. **Opt-in/opt-out change.** `set_space_shared_in_profile` IPC mutates `Space.shared_in_profile`, then `Notify::notify_one()` on a publisher notify channel. The publisher task:
   - Sleeps 2 seconds (debounce window).
   - Re-walks `OwnerState.spaces` for currently opted-in communities.
   - Compares against the last-published set.
   - Publishes if different.

2. **Periodic refresh.** Every 10 minutes, the publisher task republishes the current opted-in set if non-empty. Defeats Zenoh PUT non-stickiness for late subscribers.

3. **Startup.** On node start, if the owner has any opted-in communities, publish once immediately. Otherwise no publication.

**Debounce:** 2-second window via `tokio::select!` on (notify_one, sleep(2s)). User toggling 5 communities in rapid succession produces ONE broadcast.

**Rotation semantics:**

| State | Action |
|---|---|
| Never opted in, no opted-in communities | **No broadcast ever** |
| First-ever opt-in | Publish with `community_ids = [new_community]`, HLC `t1` |
| Opt-in/out where result still non-empty | Publish with updated set, HLC `t1 > prior` |
| Opt-out of LAST community (N → 0) | **Publish with `community_ids = []`** and HLC `t1 > prior` (rotation) |
| Already at zero and stays at zero | No publish |

The rotation publish is the load-bearing privacy invariant. Without it, the prior non-empty broadcast remains valid forever for any peer that cached it.

**HLC source:** existing `Hlc` generator in `owner_state_types.rs`. Bumped on each publish.

**State machine:**

```text
                no opted-in communities yet, no history
                              │
                              ▼
                  ┌─── Never-published ───┐
                  │                       │
      first opt-in│                       │ (no transition without opt-in)
                  ▼                       │
              ┌─Active─┐                  │
              │        │ debounce 2s      │
              │        ▼                  │
              │  Publish + bump HLC ──────┤ (back to Active if any communities remain)
              │                           │
              │  N→0 opt-out (rotation):  │
              │   publish empty + bump ───┤
              │                           │
              │  10min timer ─────────────┤ (periodic refresh if any communities)
              └─...continues until shutdown
```

**Termination:** no special "goodbye" publish on shutdown. Periodic refresh stops; cached values at peers persist by HLC staleness at peer-side discretion.

## 6. Subscriber lifecycle + verification

`profile_broadcast::Subscriber` (held in `NodeState`) owns the subscription side. Three IPCs form the public API:

```rust
async fn subscribe_peer_profile(peer_addr: String) -> Result<u64, String>;
async fn unsubscribe_peer_profile(subscription_id: u64) -> Result<(), String>;
async fn get_cached_peer_profile(subscription_id: u64) -> Result<Option<DiscoveredProfileInfo>, String>;
```

### Lifecycle

1. `subscribe_peer_profile(peer_addr)` spawns a tokio task that subscribes to `harmony/discovery/profile/{peer_addr}/memberships`. Returns a `u64` `SubscriptionId` handle.
2. On each Zenoh sample, the task:
   - Decodes CBOR → `ProfileMembershipBroadcast`.
   - Runs `verify_broadcast(&broadcast)` (see below).
   - Performs attribution check: derived addr MUST equal the topic owner. Otherwise reject (`AttributionMismatch`).
   - Performs replay defense: incoming HLC MUST be strictly newer than cached HLC. Otherwise drop (idempotent).
   - On accept: replace cached `Option<VerifiedBroadcast>`, emit IPC event `profile-broadcast-received`.
3. `get_cached_peer_profile(id)` returns the latest verified broadcast if any. Frontend uses for "loading vs. loaded" decisions.
4. `unsubscribe_peer_profile(id)` cancels the task, drops the Zenoh subscriber, releases the cached state.

### `verify_broadcast`

```rust
pub fn verify_broadcast(
    broadcast: &ProfileMembershipBroadcast,
) -> Result<OwnerAddr, BroadcastVerifyError> {
    // (1) Bounds
    if broadcast.community_ids.len() > MAX_SHARED_COMMUNITIES {
        return Err(BroadcastVerifyError::TooManyCommunities);
    }
    // (2) Sortedness + dedup (strictly increasing)
    if !broadcast.community_ids.windows(2).all(|w| w[0] < w[1]) {
        return Err(BroadcastVerifyError::CommunityIdsNotSortedDeduped);
    }
    // (3) Parse identity_pub
    let identity = harmony_identity::Identity::from_public_bytes(&broadcast.owner_identity_pub)
        .map_err(|e| BroadcastVerifyError::InvalidIdentityPub(format!("{e:?}")))?;
    // (4) Verify sig over canonical CBOR with `sg` zeroed
    let mut for_sig = broadcast.clone();
    for_sig.signature = [0u8; 64];
    let bytes = canonical_cbor_encode(&for_sig)?;
    let sig = Signature::from_bytes(&broadcast.signature);
    identity.verifying_key.verify_strict(&bytes, &sig)
        .map_err(|_| BroadcastVerifyError::SignatureInvalid)?;
    Ok(OwnerAddr(identity.address_hash))
}
```

### Frontend DTO

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredProfileInfo {
    /// Hex-encoded 16-byte OwnerAddr (32 hex chars).
    pub owner_addr: String,
    /// Hex-encoded SpaceIds the peer opted to share.
    pub community_ids: Vec<String>,
    /// `shared_at.wall_ms` as base-10 string. Display-only;
    /// callers MUST NOT use this for HLC ordering decisions.
    pub shared_at: String,
}
```

Wire keys are camelCase (matches Phase 2's `DiscoveredLibraryInfo` precedent for discovery-side DTOs).

## 7. IPC surface

| IPC | Direction | Purpose |
|---|---|---|
| `set_space_shared_in_profile(community_id, shared)` | JS → Rust | Toggle per-community opt-in. Bumps `Space.updated_at`, notifies publisher. |
| `subscribe_peer_profile(peer_addr)` → `u64` | JS → Rust | Subscribe to a peer's broadcast topic. Returns handle. |
| `unsubscribe_peer_profile(subscription_id)` | JS → Rust | Cancel a subscription. |
| `get_cached_peer_profile(subscription_id)` → `Option<DiscoveredProfileInfo>` | JS → Rust | Retrieve latest verified broadcast for a subscription. |
| `profile-broadcast-received` event | Rust → JS | Emitted on each verified receive: `{ subscriptionId, ownerAddr, communityIds, sharedAt }`. |

No changes to existing IPCs (no surface needed in `add_library`, `browse_library`, etc.).

## 8. Frontend

### 8.1 Service layer

`src/lib/profile-broadcast-service.ts`:

```typescript
export interface DiscoveredProfileInfo {
  ownerAddr: string;
  communityIds: string[];
  sharedAt: string;
}

export class ProfileBroadcastService {
  constructor(private adapter: TauriAdapter) {}

  async setShared(communityId: string, shared: boolean): Promise<void> {
    await this.adapter.invoke('set_space_shared_in_profile', { communityId, shared });
  }

  async subscribe(peerAddr: string): Promise<number> {
    return await this.adapter.invoke('subscribe_peer_profile', { peerAddr }) as number;
  }

  async unsubscribe(subscriptionId: number): Promise<void> {
    await this.adapter.invoke('unsubscribe_peer_profile', { subscriptionId });
  }

  async getCached(subscriptionId: number): Promise<DiscoveredProfileInfo | null> {
    return await this.adapter.invoke('get_cached_peer_profile', { subscriptionId }) as DiscoveredProfileInfo | null;
  }
}
```

### 8.2 `CommunitySettingsPanel.svelte` — toggle

A new section in the existing 417-line settings panel:

```svelte
<section class="settings-section">
  <h3>Public profile</h3>
  <label class="toggle-row">
    <input
      type="checkbox"
      bind:checked={sharedInProfile}
      onchange={handleSharedToggle}
    />
    <span class="toggle-label">
      Share this community in my public profile
    </span>
  </label>
  <p class="toggle-help">
    When enabled, peers viewing your profile will see that you've
    joined <strong>{communityName}</strong>. Off by default.
  </p>
</section>
```

Bound to `Space.shared_in_profile` (read via existing community-info IPC; toggled via `service.setShared`).

### 8.3 `ProfilePopover.svelte` — Public memberships section

When the popover opens for a peer (NOT self):

1. On mount: `service.subscribe(peer.address)` → `subscriptionId`.
2. Poll `service.getCached(subscriptionId)` every 500ms OR listen for `profile-broadcast-received` IPC event.
3. Render one of three states:
   - **Loading** (first 3 seconds): "Looking up public memberships…"
   - **Empty/not shared** (after 3s no broadcast OR broadcast with `communityIds: []`): "No public memberships shared."
   - **Has shared**: list of community names (resolved via `OwnerState.spaces` if viewer is a co-member; otherwise short-form hex).
4. On unmount: `service.unsubscribe(subscriptionId)`.

```svelte
{#if profile.address !== ownAddress}
  <div class="popover-memberships">
    <div class="memberships-label">Public memberships</div>
    {#if memberships === null}
      <div class="memberships-loading">Looking up public memberships…</div>
    {:else if memberships.communityIds.length === 0}
      <div class="memberships-empty">No public memberships shared.</div>
    {:else}
      <ul class="memberships-list">
        {#each memberships.communityIds as communityId}
          <li>{resolveCommunityName(communityId) ?? shortAddr(communityId)}</li>
        {/each}
      </ul>
    {/if}
  </div>
{/if}
```

**Name resolution helper** `resolveCommunityName(communityId)`:
- Returns the community name if the viewer is a co-member (looked up in their own `OwnerState.spaces`).
- Returns `null` otherwise — caller renders short-form hex.

Cross-resolution via the library directory (Phase 1+2+3 infrastructure) is a deferred follow-up.

## 9. Error handling

| Scenario | Verifier outcome | Cache outcome | UI |
|---|---|---|---|
| Valid broadcast, newer HLC | `Ok(addr)` | Updated | Renders list |
| Valid broadcast, older HLC | `Ok(addr)` | Unchanged (replay-drop) | Last-known list |
| Signature invalid | `Err(SignatureInvalid)` | Unchanged | Last-known list (or loading state) |
| `community_ids` > 200 | `Err(TooManyCommunities)` | Unchanged | (as above) |
| `community_ids` unsorted/duplicate | `Err(CommunityIdsNotSortedDeduped)` | Unchanged | (as above) |
| Malformed identity_pub | `Err(InvalidIdentityPub)` | Unchanged | (as above) |
| Attribution mismatch (derived addr ≠ topic owner) | (process_sample level — not a verifier error) | Unchanged | (as above) |
| CBOR decode failure | (process_sample level — drop sample) | Unchanged | (as above) |

All failures are silently dropped at receive — never surfaced to UI as toasts/errors. UI either shows the most recently verified broadcast or the empty/loading state.

## 10. Performance / scale

- **Verification cost:** one Ed25519 verify per receive ≈ 50 µs. Negligible.
- **Wire-size:** 200 SpaceIds × 32 bytes + framing + sig ≈ 6.5 KB worst-case. Typical (5-20 communities) ≈ 0.3-1 KB.
- **Subscription cost:** Zenoh subscriber per active ProfilePopover. Bounded by popover lifetime (seconds to minutes). At typical user behavior (1-2 open popovers concurrently), this is 1-2 Zenoh subscriptions — trivial.
- **Periodic refresh:** 1 broadcast per 10 minutes per opted-in user. At a peer-network of 1000 users, recipients receive ≤ 1000 broadcasts per 10-min window (when their subs are open). On-demand subscriptions cap this drastically.

## 11. Testing

### 11.1 Test fixtures (`tests/common/profile_fixtures.rs`)

```rust
pub fn build_test_owner_identity(seed: [u8; 32]) -> (SigningKey, [u8; 64]);

pub fn mock_profile_broadcast(
    seed: [u8; 32],
    community_ids: Vec<SpaceId>,
    shared_at: Hlc,
) -> (Vec<u8>, OwnerAddr);
```

### 11.2 Unit tests (`profile_broadcast.rs::tests`)

| Test | What it pins |
|---|---|
| `verify_broadcast_valid_returns_owner_addr` | Round-trip signed broadcast verifies; returned addr derived from identity bundle |
| `verify_broadcast_tampered_signature_rejected` | XOR'd sig → `Err(SignatureInvalid)` |
| `verify_broadcast_tampered_payload_rejected` | XOR'd `community_ids[0]` byte → `Err(SignatureInvalid)` |
| `verify_broadcast_too_many_communities_rejected` | 201 IDs → `Err(TooManyCommunities)` |
| `verify_broadcast_unsorted_community_ids_rejected` | `[B, A]` (out of order) → `Err(CommunityIdsNotSortedDeduped)` |
| `verify_broadcast_duplicate_community_ids_rejected` | `[A, A]` → `Err(CommunityIdsNotSortedDeduped)` |
| `verify_broadcast_malformed_identity_pub_rejected` | Bad bundle bytes → `Err(InvalidIdentityPub)` |
| `verify_broadcast_empty_community_ids_accepted` | `[]` with valid sig → `Ok(addr)` (rotation case) |
| `state_replay_old_hlc_rejected` | Cached HLC `t1`; incoming HLC `t0 < t1` → state unchanged |
| `state_replay_newer_hlc_accepted` | Cached HLC `t0`; incoming HLC `t1 > t0` → state updated |

### 11.3 Integration tests (`tests/profile_broadcast_integration.rs`)

| Test | What it pins |
|---|---|
| `peer_subscribe_receives_broadcast` | Mock peer publishes; subscriber decodes + verifies + caches; `get_cached_peer_profile` returns the broadcast |
| `attribution_mismatch_rejected` | Adversary publishes on peer X's topic with peer Y's identity bundle → subscriber rejects, cache empty |
| `subscribe_unsubscribe_lifecycle` | Subscribe → receive → unsubscribe → cache cleared, task ended |
| `self_publish_on_opt_in_change` | `set_space_shared_in_profile(c, true)` triggers debounced republish; second device subscribing sees `[c]` |
| `self_publish_rotation_to_empty` | After opting out of last community, broadcast carries `community_ids: []` with newer HLC |

### 11.4 Wire-format pinning (`tests/wire_format_profile_broadcast_fixtures.rs`)

| Test | What it pins |
|---|---|
| `profile_broadcast_canonical_cbor_pinned` | Deterministic fixture: pinned full canonical hex |
| `profile_broadcast_field_keys_are_2char` | `ciborium::Value::Map` iter confirms exactly `{ai, cs, sa, sg}` keys |

### 11.5 Frontend vitest (`src/lib/__tests__/profile-broadcast-service.test.ts` + popover/panel tests)

| Test | What it pins |
|---|---|
| `service_subscribe_returns_handle` | IPC mock returns u64; service exposes it |
| `popover_subscribes_on_mount` | Mount popover → `subscribe_peer_profile` called once |
| `popover_unsubscribes_on_close` | Close popover → `unsubscribe_peer_profile` called |
| `popover_shows_loading_then_loaded` | Initial render shows "Looking up…"; after cache populates, shows list |
| `popover_shows_no_memberships_after_timeout` | 3s with no broadcast → "No public memberships shared" |
| `settings_panel_toggle_invokes_set_shared` | Click toggle → `set_space_shared_in_profile` invoked with `{communityId, shared}` |

## 12. Deferred follow-ups

| Tag | Title | Description |
|---|---|---|
| 4.5 | Cross-resolution via library directory | Resolve community names for IDs the viewer isn't in via the user's trusted libraries' directories. Useful UX but adds significant surface. |
| 4.5 | Self-profile summary view | Show the user their own broadcast contents in `ProfileEditor.svelte`. Currently invisible — must trust the per-community toggles. |
| 4.5 | Zenoh-queryable publisher | Replace periodic 10-min refresh with a Zenoh queryable so on-demand subscribers get immediate values via query (sub-100ms latency). Reduces bandwidth, eliminates "loading" UX cost. |
| 4.5 | Persistent subscriptions for DM partners | Maintain background subscriptions to addresses you DM regularly. Better UX, higher resource usage. |

(No new Linear sub-tickets need to be filed for the 4.5 row — these are speculative future work. The existing Phase 6 ticket [ZEB-252](https://linear.app/zeblith/issue/ZEB-252/) is unchanged.)

## 13. Out of scope this round

- **Profile-level metadata beyond memberships** (displayName, statusText, avatarUrl). Already covered by the existing `Profile` type and `ProfileEditor.svelte` — not part of the broadcast protocol.
- **Cross-device sync of subscriptions.** Subscriptions are local-only and ephemeral (tied to popover lifetime). Each device manages its own.
- **Broadcast retention / deletion at peer caches.** Old broadcasts may persist at peer caches; rotation via HLC is the only deletion mechanism on the protocol side. Peer-side cache policy is out of scope.
- **Anti-correlation across multiple peer addresses.** An attacker correlating multiple `owner_addr`s could build a peer-graph; this is inherent to any public-key address scheme and out of scope for v1.
- **Membership reachability proofs.** Proving the user actually joined the community they claim to share could be added via co-signed receipts from community admins; YAGNI for v1. Recipients trust the broadcaster's claim (admin sig in invite URL already covers community existence; membership claim is implicit).

## 14. Acceptance criteria

1. **Default privacy preserved.** A fresh-install user with no opt-in produces ZERO `ProfileMembershipBroadcast` publications. Their address is indistinguishable from a non-Harmony address at the broadcast layer. Verified by integration test that observes the publisher's Zenoh PUT count.

2. **First opt-in starts publishing.** Toggling `shared_in_profile = true` on a community publishes a broadcast with `community_ids = [that_community]` within ~2 seconds (debounce window). Verified by `self_publish_on_opt_in_change`.

3. **Last-opt-out rotates.** Toggling `shared_in_profile = false` on the only opted-in community publishes a broadcast with `community_ids = []` and a newer HLC, **rotating** the prior non-empty broadcast. Verified by `self_publish_rotation_to_empty`.

4. **Peer view shows opted-in set only.** Subscribing to a peer's topic returns ONLY the communities they've opted to share — never their full membership list. Verified by integration test `peer_subscribe_receives_broadcast`.

5. **Tampering rejected.** Broadcasts with invalid signatures, unsorted/duplicate `community_ids`, mismatched topic-owner attribution, or community counts exceeding `MAX_SHARED_COMMUNITIES = 200` are rejected at receive — never surfaced to UI.

6. **All 5 CI gates green:**
   - `cargo fmt --all -- --check`
   - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
   - `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
   - `cargo check --locked --all-targets --features test-fixtures` (MSRV)
   - `npx tsc --noEmit` + `npx vitest run`

## 15. References

- Parent: [ZEB-218](https://linear.app/zeblith/issue/ZEB-218/) — Sub-D library-federated discovery directory (stays In Progress; Phase 6 remains).
- This phase: [ZEB-281](https://linear.app/zeblith/issue/ZEB-281/) — Phase 4 ProfileMembershipBroadcast.
- Phase 1 spec: `docs/specs/2026-05-11-zeb-218-sub-d-library-directory-vertical-slice-design.md`.
- Phase 2 spec: `docs/specs/2026-05-11-zeb-279-sub-d-phase-2-library-auto-discovery-design.md`.
- Phase 3 spec: `docs/specs/2026-05-12-zeb-280-sub-d-phase-3-federated-republication-design.md`.
- Original Sub-D scope: `docs/specs/2026-04-30-zeb-206-nav-tree-design.md` L235-246 (specced the wire type with the now-renamed topic `harmony/announce/{owner_addr}/memberships`).
- Phase 1 PR: [#108](https://github.com/zeblithic/harmony-client/pull/108).
- Phase 2 PR: [#109](https://github.com/zeblithic/harmony-client/pull/109).
- Phase 3 PR: [#110](https://github.com/zeblithic/harmony-client/pull/110).
- Sibling implementation patterns:
  - `src-tauri/src/library_directory.rs::verify_announce` — Phase 2 single-layer sig verification (most direct parallel for `verify_broadcast`).
  - `src-tauri/src/library_directory.rs::process_sample` — attribution-mismatch defense pattern.
  - `src-tauri/src/owner_state_types.rs::serialize_bytes_as_bstr` — bstr serde helper.
  - `src-tauri/src/owner_state_types.rs::Space` (current 30+ field struct) — Space extension pattern.
  - `src/lib/components/CommunitySettingsPanel.svelte` (417 lines) — existing settings panel for the toggle integration.
  - `src/lib/components/ProfilePopover.svelte` (139 lines) — existing popover for the memberships section.
