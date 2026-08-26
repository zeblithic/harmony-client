# ZEB-206: harmony-client nav tree — wire real channels, communities, DMs

**Date:** 2026-04-30
**Linear:** [ZEB-206](https://linear.app/zeblith/issue/ZEB-206)
**Origin:** Mock-data audit (2026-04-30) flagged the NavService folder/channel/DM tree as the largest mock surface in the app. ZEB-206 was filed alongside ZEB-207/208/209 to track the wiring work. This spec is the result of brainstorming after Gemini Deep Research on decentralized social platform information architecture.

## Goal

Replace the entirely-mock NavService tree with a real data flow rooted in the owner-identity layer. A connected harmony-client should show only real channels, communities, DMs, and folders — synced across the owner's bound devices — and provide working discovery, joining, and direct-messaging flows.

## Guiding principles (decision values)

These principles came out of brainstorming and shape every architectural choice below.

1. **Ostrom polycentric governance.** Communities are harmony's only first-class moderation primitive. There is no global moderation, no platform-level admin, no algorithmic content promotion. Communities self-govern (admins, power levels, kicks) and are responsible for upholding any regulation/legal requirements that apply to *that* community. Public broadcast and private messaging stay outside any moderation surface.
2. **Owner-identity as a federated cluster.** ZEB-197 device pairing gives us a coherent set of "my devices." This is harmony's distinguishing primitive against the surveyed protocols — closer to a distributed Personal Data Server than to single-device-bound (SSB/Briar) or always-on-server (Matrix/ATProto/Farcaster) models.
3. **Privacy by transport choice, not by ad-hoc encryption.** DMs use Reticulum unicast (no broadcast subscription pattern leaks). Public channels use Zenoh pub/sub (anyone can subscribe, no metadata to hide). Owner-state CRDT topic uses encrypted payloads (observers see "owner is active" but not what changed).
4. **Civic infrastructure for the digital-physical bridge.** Where harmony needs centralized presence (e.g., a public community directory), lean on existing civic institutions with established trust — libraries. Federated, opt-in, curated.
5. **Engineering for real scale.** Power-efficient by default. Bounded convergence latency, not unbounded gossip. CAS dedup leveraged at every layer where attachment payloads dominate. Designed for billions of users, not "we'll figure it out later."

## Non-goals

- Native voice/video calling in DMs — separate transport design, deferred.
- Threaded replies within channels — existing flat-channel UX fine for v1.
- Reactions/emoji on DMs — channel reactions ship via ZEB-32; DM parity deferred.
- Cross-community channel migration tooling — when group-DM hits 17 members and converts to a community, no auto-migration of history; "keep history?" deferred to a UX prompt.
- Read-receipts visible to DM peers — privacy default. Future: opt-in per-DM signed event type.
- Global blocklist / federated reputation — each user blocks libraries/peers locally. (Different from Bluesky labelers — deliberate.)

## Architecture

Four layers, each with a distinct authority and transport choice.

```text
┌─────────────────────────────────────────────────────────────┐
│ UI Layer:        NavTree.svelte ← NavService (Svelte 5)     │
├─────────────────────────────────────────────────────────────┤
│ IPC Bridge:      list_spaces, get_space_detail, send_dm,    │
│                  subscribe_to_owner_state_updates, ...       │
├─────────────────────────────────────────────────────────────┤
│ State Layer:     Owner-State Prolly Tree CRDT (per owner)   │
│                  ├─ spaces[]    (unified Space entries)      │
│                  ├─ outbox[]    (pending outgoing DMs)       │
│                  ├─ inbox[]     (received DM messages)       │
│                  └─ markers{}   (per-space last_read_at)     │
│                                                              │
│                  Community Membership CRDTs (per community)  │
│                  └─ events[]    (signed membership log)      │
├─────────────────────────────────────────────────────────────┤
│ Transport:       Zenoh pub/sub  → public channels, comm.    │
│                                    membership announces,    │
│                                    library directory topic, │
│                                    encrypted owner-state    │
│                  Reticulum link → DMs (≤16 members)          │
└─────────────────────────────────────────────────────────────┘
```

### Source of truth distribution

| State | Authority | Replicated to |
|---|---|---|
| My spaces, folders, read markers, DM outbox + inbox | Owner-state Prolly Tree | This owner's bound devices (ZEB-197) |
| Community membership / power levels | Per-community CRDT | All members of that community |
| Public channel "membership" | Transport-derived | Anyone subscribed to the Zenoh topic |
| DM "membership" | Asymmetric in v1: **adds** are authoritative + grow-only via CRDT (any current member adds, ≤16 cap); **leaves are owner-local only** and not propagated. See "Group DM mutability" for full semantics. | `Space.members` is the canonical add list. OutboxEntry/InboxEntry `recipient_owners` mirror current members minus sender. Leaves never modify `Space.members` on other devices in v1. |
| Library directory entries | Per-library | Subscribers to that library's directory topic |
| Profile membership broadcast | Owner | Subscribers to that owner's profile topic |

### Forks considered and rejected

The brainstorming progression considered and rejected these alternatives:

1. **SSB-style append-only logs everywhere.** Perfect offline correctness, unbounded multi-device convergence latency. Rejected for failing "join on phone, see on desktop" UX.
2. **Matrix-style per-space membership CRDT for *every* space.** Over-engineers public broadcast (forces a CRDT write to subscribe). Rejected for fighting Zenoh's natural pub/sub topology.
3. **Gift-wrapped Zenoh DMs (Nostr NIP-17 style).** Single transport, but subscription pattern itself is a fingerprint. Rejected; Reticulum's unlinkability is harmony's distinguishing privacy story.
4. **No global directory at all.** Strongest privacy, but cold-start UX is rough. Rejected in favor of opt-in library-federated directories (each library is a small federated index, not a singleton).

   > **⚠️ Update 2026-06-26 (ZEB-556 scope-2 decision).** The public-community directory — including the library-federated model below (built in ZEB-218 / ZEB-252) — is **not surfaced in alpha and is not on the roadmap as a global/top-level index**. A global registry becomes overwhelmingly large + noisy to sift through; alpha discovery is **word-of-mouth** (a community shares its Harmony link via its website / Discord / socials / member profiles). The future *exploratory* direction (not committed) is **community-owned micro-directories**: a directed graph / "webring" of per-community registries (partner links, "if you like X you might like Y"), where crawl-the-graph friction is a feature — it distributes/parallelizes load, and you only crawl/serve communities you're a part of. See **ZEB-574**. The `library_directory.rs` / `LibraryDirectoryBrowser.svelte` code remains built but intentionally unsurfaced.
5. **Local-only nav state per device.** Simplest implementation, but joining a community on phone wouldn't show on desktop. Major UX regression. Rejected.

## Data model

All types are CBOR-encoded over Prolly Tree nodes. Notation below is TypeScript-ish for readability.

### `Space` — unified entry in owner-state CRDT (the nav tree)

```typescript
type SpaceKind =
  | 'folder'         // local UX organization; no transport, no members
  | 'community'      // moderated org; has its own membership CRDT
  | 'channel'        // pub/sub inside a community
  | 'public-channel' // pub/sub, no community wrapper
  | 'dm'             // exactly 2 members total (sender + 1 recipient), Reticulum
  | 'group-dm';      // 3-16 members total (sender + 2-15 recipients), Reticulum

interface Space {
  id: SpaceId;                    // ULID-style; locally unique per creation
  kind: SpaceKind;
  parent: SpaceId | null;         // folder/community parent ref
  community_id: SpaceId | null;   // for channels: which community owns this
  name: string;
  transport: TransportBinding | null;   // null only for folders
  members: OwnerAddr[];           // for DM/group-DM only — INCLUDES sender; communities use separate CRDT
  custom_name: string | null;     // owner's local rename, applies only on this owner's devices
  notification_pref: 'all' | 'mentions' | 'muted' | null;
  left_at: HLC | null;            // dm/group-dm/community: HLC of when *I* left this Space
                                  // (communities since the 2026-06-10 ZEB-427 amendment below).
                                  // Owner-local field — never propagates to other members.
                                  // Receive layer drops incoming DMs when non-null.
                                  // Re-invitation clears back to null (NOT a tombstone).
  created_at: HLC;
  updated_at: HLC;
}

type TransportBinding =
  | { kind: 'zenoh'; topic: string }
  | { kind: 'reticulum'; participants: ReticulumDest[] };
```

**Invariants** (verified at write time and after CRDT merge):

- `kind === 'folder'` ⇒ `transport === null && members === []`
- `kind === 'channel'` ⇒ `community_id !== null && transport.kind === 'zenoh'`
- `kind === 'public-channel'` ⇒ `community_id === null && transport.kind === 'zenoh'`
- `kind === 'dm'` ⇒ `members.length === 2 && transport.kind === 'reticulum'`
- `kind === 'group-dm'` ⇒ `3 ≤ members.length ≤ 16 && transport.kind === 'reticulum'`
- `kind === 'community'` ⇒ a corresponding `CommunityMembership` CRDT exists at `id`

### `CommunityMembership` — per-community CRDT

Separate from owner-state. Replicated only among that community's members.

```typescript
interface CommunityMembership {
  community_id: SpaceId;
  events: SignedMembershipEvent[];   // append-only DAG
  // Materialized views derived from event log:
  //   members:      Map<OwnerAddr, MemberState>
  //   power_levels: Map<OwnerAddr, number>  // power 0..100 per member; default 0 on join
}

// Hardcoded power thresholds for v1 — see "Power-level thresholds" below.
const POWER_THRESHOLDS = {
  invite:    0,    // any member can invite
  kick:      50,   // moderator-tier
  set_power: 100,  // owner/admin-tier (must already have power 100 to grant power)
  max:       100,
};

type SignedMembershipEvent =
  | { kind: 'join';      actor: OwnerAddr; at: HLC; sig: Signature }
  | { kind: 'leave';     actor: OwnerAddr; at: HLC; sig: Signature }
  | { kind: 'invite';    actor: OwnerAddr; target: OwnerAddr; at: HLC; sig: Signature }
  | { kind: 'kick';      actor: OwnerAddr; target: OwnerAddr; reason?: string;
                         at: HLC; sig: Signature }
  | { kind: 'set_power'; actor: OwnerAddr; target: OwnerAddr; level: number;
                         at: HLC; sig: Signature };

interface MemberState {
  status: 'joined' | 'invited' | 'left' | 'banned';
  joined_at: HLC;
  left_at?: HLC;
}
```

### `OutboxEntry` — outgoing DM record (in owner-state CRDT)

OutboxEntries are the **persistent log of DMs I've sent**, not just a delivery queue. The active delivery loop drives entries with `delivery_status: 'pending'`; once complete (or expired), the entry stays in the CRDT as sent-message history. This is what the UI renders for outgoing messages.

```typescript
interface OutboxEntry {
  id: string;                      // ULID
  space_id: SpaceId;               // DM/group-DM Space this outgoing message belongs to
  recipient_owners: OwnerAddr[];   // EXCLUDES sender — 1 for DM, 2-15 for group-DM (snapshot at send time)
  message_cid: ContentId;          // encrypted message blob lives in CAS (per "DM content confidentiality")
  created_at: HLC;
  delivered_to: OwnerAddr[];       // ack list (subset of recipient_owners)
  delivery_status: 'pending' | 'partial' | 'complete' | 'expired';
                                   // pending: delivery loop still active for unack'd recipients
                                   // partial: at least one recipient acked, others pending
                                   // complete: all recipient_owners are in delivered_to
                                   // expired: 30-day timer fired with one or more unacked recipients
                                   //          (silent leavers indistinguishable from genuinely-offline)
}
```

**OutboxEntries are NEVER GC'd** in v1 — they're chat history. Long-term archival/pruning is a deliberate followup. The `delivery_status` field's transitions:

- New → `pending`
- Each ack → updates `delivered_to[]`; `delivery_status` becomes `partial` (or `complete` when all acked)
- After 30 days with at least one unacked recipient → `expired`. The delivery loop stops retrying. UI shows "undeliverable" badge for the unacked recipient(s).

### `InboxEntry` — received DM (in owner-state CRDT)

When a DM arrives via Reticulum on any of the recipient's bound devices, that device writes an `InboxEntry` into its owner-state CRDT. Owner-state sync (Flow A) replicates the entry across the recipient's other bound devices, so all of them see the new message without needing direct Reticulum delivery to each.

```typescript
interface InboxEntry {
  // Lookup key: (space_id, message_cid) — see "Idempotency" below.
  space_id: SpaceId;               // the DM/group-DM Space this belongs to
  message_cid: ContentId;          // encrypted message blob in CAS — content-addressed, unique per message
  from: OwnerAddr;                 // sender's owner address
  received_at: HLC;                // first device's receive time (CRDT picks earliest on merge)
}
```

**Idempotency:** Inbox writes are **upsert by `(space_id, message_cid)`**, not append-by-ULID. Reticulum delivery may retry (sender's loop) and the same message may arrive on multiple bound devices via overlapping links. The lookup key `(space_id, message_cid)` is naturally unique per message because `message_cid` is a content-addressed BLAKE3 hash of the (encrypted) blob — duplicates collapse into a single InboxEntry on CRDT merge. Implementations MUST check for an existing InboxEntry with the same `(space_id, message_cid)` before writing.

UI message rendering for a DM Space joins (`OutboxEntry` filtered by `space_id` for outgoing) ∪ (`InboxEntry` filtered by `space_id` for incoming), ordered by HLC. Read state lives in `ReadMarker`.

### `ReadMarker` — per-space (in owner-state CRDT)

```typescript
interface ReadMarker {
  space_id: SpaceId;
  last_read_at: HLC;   // monotone-advancing across owner's bound devices
}
```

### `LibraryDirectoryEntry` — discovery surface

Published by libraries to `harmony/discovery/library/{library_addr}/communities`.

```typescript
interface LibraryDirectoryEntry {
  community_id: SpaceId;
  community_addr: OwnerAddr;       // creator/admin signing key
  name: string;
  description: string;
  topics: string[];                // tags for browse filtering
  listed_by: OwnerAddr;            // library's identity
  listed_at: HLC;
  community_signature: Signature;  // community admin's sig over the manifest
  library_signature: Signature;    // library's sig over the listing
}
```

### `ProfileMembershipBroadcast` — discovery surface

Published to `harmony/announce/{owner_addr}/memberships`. Each owner curates which memberships they make public.

```typescript
interface ProfileMembershipBroadcast {
  owner: OwnerAddr;
  community_ids: SpaceId[];        // owner-curated subset; per-community opt-in
  shared_at: HLC;
  signature: Signature;
}
```

### Hybrid Logical Clock (`HLC`)

`HLC` is used as the timestamp type throughout this spec (`Space.created_at`, `OutboxEntry.created_at`, `ReadMarker.last_read_at`, etc.). It's an alias for `HybridLogicalClock`:

```typescript
type HLC = HybridLogicalClock;

interface HybridLogicalClock {
  wall_ms: number;       // unix milliseconds
  logical: number;       // monotone counter for same-wall-ms events
  device_id: string;     // bound-device identifier (for tiebreak across owner's devices)
}
```

HLC gives total ordering across the owner's bound devices without requiring wall-clock sync.

## Data flow / sync protocol

Four load-bearing flows. Profile-based discovery and read-marker sync are minor variations.

### Flow A — Owner-state CRDT sync across bound devices

```text
Device A:                                  Device B (same owner):
─────────                                  ──────────
1. User joins #general
2. NavService → IPC add_space(...)
3. harmony-client commits Prolly Tree
   node → new owner-state root CID
4. Encrypt root CID with owner-derived
   key. Publish to Zenoh:
   harmony/owner/{addr}/state-root  ───►   5. Subscriber sees encrypted root CID
                                           6. Decrypts with owner-derived key
                                           7. DAG-sync fetches missing blocks
                                              (ZEB-108 protocol)
                                           8. Reconcile local Prolly Tree
                                           9. Emit IPC `nav-updated` event
                                          10. NavService refreshes view
```

**Latency:** sub-second LAN, seconds across NATs. Bounded by Zenoh propagation + DAG sync RTT, not unbounded gossip.

### Flow B — Joining a community

1. User clicks "Join" (got `community_id` from directory / profile / invite)
2. IPC `join_community(community_id, invite_token?)`
3. Rust subscribes to `harmony/community/{community_id}/membership`, replays signed event log → materializes current `CommunityMembership` state locally
4. Composes a `{kind: 'join', actor: my_addr, at: HLC, sig}` event signed by owner key
5. **Open communities:** publishes the event to the membership topic. Other members append to their local CRDT.
6. **Invite-only communities:** sends event via Reticulum to the inviter (or any member with `power ≥ invite_threshold`); they vouch by counter-signing and publishing.
7. Locally, also writes a new `Space {kind: 'community', ...}` to owner-state CRDT
8. Owner-state sync (Flow A) propagates to other bound devices

### Flow C — Sending a DM (offline recipient — the hard case)

1. User sends DM. MessageService → IPC `send_dm(space_id, content)`
2. Rust **encrypts** the message blob with the per-DM-Space content key (see "DM content confidentiality" below), stores ciphertext in CAS → gets CID over ciphertext
3. Attempts Reticulum link to recipient's owner (tries known device endpoints)
4. **Link fails / no endpoints reachable:**
   - Writes `OutboxEntry {id, space_id, recipient_owners, message_cid, created_at, delivered_to: []}` to owner-state CRDT
   - Owner-state sync replicates outbox to *sender's* other bound devices
5. **Delivery loop** (any of sender's bound devices, whichever is online):
   - Schedules retries per OutboxEntry using **exponential backoff with jitter, bounded max interval** (consistent with the failure-mode row "Retry with exponential backoff" — see Failure modes table). Implementations: backoff base ~5s, multiplier 2×, max ~5min, plus ±20% jitter; tune as needed.
   - For each pending entry, attempts Reticulum link to each unack'd recipient
   - On successful link: delivers `message_cid` (recipient fetches the **encrypted blob** from CAS via `fetch_content`; see "DM content confidentiality" below)
   - **Recipient device writes a new `InboxEntry` to its owner-state CRDT** — `{id, space_id, message_cid, from: sender_addr, received_at: HLC}`
   - Recipient acks via the same link → sender updates `delivered_to[]`
6. When `delivered_to.length === recipient_owners.length`, GC the OutboxEntry

**Multi-device convergence on the recipient side:** the recipient also has multiple bound devices. Delivery succeeds when **any** of the recipient's devices acks. The receiving device's owner-state CRDT now contains the new `InboxEntry`; Flow A replicates it to the recipient's other bound devices, which then see the new message without needing direct Reticulum delivery. Each of the recipient's other devices runs DAG-sync to fetch the `message_cid` blob from CAS and decrypts it locally with the per-DM-Space content key (which is in their owner-state CRDT, replicated via Flow A).

### DM content confidentiality

CAS is content-addressed but **not access-controlled** — anyone who learns a CID can fetch the blob. To prevent CID-leak attacks (e.g., timing analysis, accidental log exposure) from compromising DM contents, **all DM message blobs are encrypted before CAS storage**.

The per-DM-Space content key is established at Space creation, distributed to invitees alongside the `id` via the Reticulum invite payload, and stored in each member's owner-state CRDT (where it inherits the encryption-at-rest provided by ZEB-211). Senders encrypt with the per-Space key + a fresh random nonce; recipients fetch ciphertext from CAS and decrypt with the same key.

**The specific encryption scheme** (key-derivation, AEAD primitive, nonce policy, group-DM key rotation under membership growth) is the subject of a dedicated companion spec — parallel to ZEB-211 for owner-state. Sub-B (ZEB-216) is blocked on that spec landing; this section is the design contract.

### Flow D — Library-federated discovery

> **⚠️ Not surfaced in alpha (ZEB-556, 2026-06-26).** Built (ZEB-218 / ZEB-252) but intentionally not exposed; alpha discovery is word-of-mouth. Future exploratory direction: community-owned micro-directory "webring" (ZEB-574), not a global or library-curated index.

1. User opens "Browse Communities"
2. harmony-client has list of trusted libraries (user-configured + auto-discovered via `harmony/discovery/library/announce`)
3. For each library, subscribes to `harmony/discovery/library/{lib_addr}/communities`
4. Receives `LibraryDirectoryEntry` records. Aggregates into a browsable list. Deduplicates by `community_id` across libraries.
5. **Federation:** libraries can re-publish entries from peer libraries with their own signature wrapping the original. Curation is per-library — each library chooses what to syndicate.
6. User clicks a community → Flow B

## Component design (harmony-client side)

### New Rust modules (`src-tauri/src/`)

| Module | Responsibility |
|---|---|
| `owner_state_crdt.rs` | Prolly Tree CRDT for owner-state: spaces, outbox, inbox, read markers. Read/write API for all four collections, plus sync via encrypted Zenoh topic (Flow A). InboxEntry writes from Flow C land here; Flow A propagates them to other bound devices. |
| `community_membership.rs` | Per-community signed-event CRDT. Join/leave/kick/power-level event composition + verification. Materialized-state computation. |
| `dm_outbox.rs` | Outbox delivery loop. Walks `delivery_status: 'pending'` and `'partial'` entries, attempts Reticulum delivery, handles acks, transitions entries to `'complete'` (all acked) or `'expired'` (30-day timeout). Does NOT GC; OutboxEntry is the persistent sent-message log. |
| `library_directory.rs` | Subscribe to known libraries' directory topics. Aggregate + dedupe entries. Handle federated republication signatures. |
| `space_commands.rs` | Tauri command handlers (the public API surface below). Pure orchestration over the modules above. |

### Modified Rust files

- `lib.rs` — register new commands; wire `dm_outbox` drain loop into the App tick.
- `event_loop.rs` — new Zenoh subscriptions: `harmony/owner/{addr}/state-root`, `harmony/community/{id}/membership`, `harmony/discovery/library/announce`, per-library directory topics.
- existing pairing/owner code — provides the bound-device list `dm_outbox` needs for store-and-forward.

### IPC command surface (public API)

#### `remove_space` semantics (resolves earlier ambiguity)

`remove_space(space_id)` is **explicit, irreversible, local tombstone**. Behavior depends on `kind`:

| Kind | Behavior |
|---|---|
| `folder` | Removes from owner-state CRDT. Children are NOT cascade-deleted; they get `parent: null` and become top-level. |
| `dm` / `group-dm` | Tombstones the local Space. **Caller likely wanted `leave_space()` instead** — that sets `left_at` (reversible). `remove_space()` writes a permanent tombstone, blocking re-creation via the same dedupe key (sorted-members or `id`) until the user manually clears the tombstone. UI MUST distinguish "leave" vs "delete forever." |
| `community` | **Returns `Err` directing caller to use `leave_community(community_id)` first.** Calling `remove_space` directly on a community Space without a corresponding leave event would leave the caller as a "ghost member" in the community CRDT (other members still see them, admins can still kick them, messages still try to route). After `leave_community` completes, `remove_space` may be called to clean up the local Space entry — at that point the leave event has been broadcast and the caller has correctly disenrolled. |
| `channel` | Tombstones the local Space (you stop subscribing). Doesn't propagate; channels are subscription-based, not membership-based. |
| `public-channel` | Same as `channel`. |

```rust
// Owner-state mutations
list_spaces() -> Vec<Space>
get_space_detail(space_id) -> SpaceDetail            // includes membership for communities
add_space(kind, name, parent?, members?, transport?) -> SpaceId
remove_space(space_id) -> Result<()>                 // explicit local tombstone — see semantics below
leave_space(space_id) -> Result<()>                  // dm/group-dm only — sets Space.left_at; reversible
move_space(space_id, new_parent) -> Result<()>
rename_space(space_id, custom_name) -> Result<()>
set_notification_pref(space_id, pref) -> Result<()>
mark_read(space_id, until_hlc) -> Result<()>

// Community membership mutations (power-gated)
join_community(community_id, invite_token?) -> Result<()>
leave_community(community_id) -> Result<()>
kick_from_community(community_id, target_addr, reason?) -> Result<()>
set_power_level(community_id, target_addr, level) -> Result<()>

// DM lifecycle
send_dm(space_id, content) -> Result<MessageId>
// (receive is push via dm-received event)

// Discovery
list_libraries() -> Vec<LibraryInfo>
add_library(library_addr) -> Result<()>
remove_library(library_addr) -> Result<()>
browse_library(library_addr) -> Vec<LibraryDirectoryEntry>
generate_invite(community_id, expires_at?) -> InviteLink
redeem_invite(invite_link) -> Result<SpaceId>
```

### IPC events (push to frontend)

| Event | Triggered by |
|---|---|
| `nav-updated` | Any owner-state CRDT change (this device or a peer device) |
| `community-members-changed` | Community membership CRDT update |
| `dm-delivered` | Outbox entry drained for one recipient |
| `dm-received` | New DM arrived via Reticulum |
| `library-directory-updated` | New entries from a subscribed library |

### Frontend changes

**Rewrites:**
- `src/lib/nav-service.ts` — drop `mockNavNodes`/`mockProfileStore`; subscribe to `nav-updated` event; on init call `list_spaces()`. Mirrors the `connectAdapter` pattern from `FileManagerService` (clears mocks on connect, per ZEB-146 — also closes ZEB-209 for NavService).
- `src/lib/mock-data.ts` — delete `navNodes` + `mockProfileStore`. Remaining mocks (messages, vines) stay until ZEB-209 is fully resolved.

**New Svelte components:**
- `LibraryDirectoryBrowser.svelte` — browse-communities UI: pick a library, see its catalog, click to join.
- `CommunitySettingsPanel.svelte` — admin UI: invite, kick, power-levels, leave.
- `InviteLinkManager.svelte` — generate/share/redeem invite links.

**Modified Svelte components:**
- The current nav-rendering panel — handle empty state (newly-paired device with no spaces yet); add "Browse Libraries" entry; remove any UI assumption that nav data exists at component mount (now async).

### What gets removed

- `mockNavNodes`, `mockProfileStore` exports from `mock-data.ts`
- NavService's mock-seeding constructor logic
- Any UI assumption that nav data exists synchronously

## Error handling, edge cases, security

### CRDT convergence semantics

#### Dedupe key per Space kind

`Space.id` is a ULID — locally unique per creation. ULIDs **do not** dedupe two devices that independently create what the user thinks of as "the same Space." For each kind, the CRDT identifies "same Space" via a kind-specific dedupe key. When two devices write entries that collide on the dedupe key, the CRDT merges them (last `updated_at` HLC wins per field; explicit-delete tombstones win over re-adds — see "Tombstones vs leaves" below).

| Kind | Dedupe key | Rationale |
|---|---|---|
| `folder` | none | Folders are owner-private UX organization. Same name on different devices = genuinely different folders. User can manually consolidate later. |
| `community` | `id` | Community ID is assigned at community creation and broadcast in the community membership CRDT events. All members reference the same id. |
| `channel` | `id` | Channel ID is assigned by community admin at channel creation and embedded in the community CRDT. Members joining reference the existing id. |
| `public-channel` | `transport.topic` | Natural identity is the Zenoh topic name. Two devices joining the same public channel reference the same topic string. |
| `dm` | sorted `members[]` | Two-person DMs have **immutable** membership (a DM is forever a DM with the same two people). Sorted-members is a stable identity. Both my devices "starting a DM with Alice" produce the same sorted member set → CRDT merges. |
| `group-dm` | `id` (ULID, assigned at creation, propagated via invite) | Group-DM membership is **mutable** (grow-only, see "Group DM mutability" below). Sorted-members would change when members are added, splitting the conversation identity. The ULID `id` is the durable conversation identity; invitees receive it as part of the Reticulum invite payload and create their Space with the same `id`. |

When the dedupe key matches across two device-local writes, the merged Space takes:
- `id`: the lexicographically smaller of the two ULIDs (deterministic tie-break)
- `created_at`: earlier HLC
- All other fields: last `updated_at` HLC wins per field

**Dependent-record canonicalization on merge.** When the winning `id` differs from a duplicate's losing `id`, the local owner-state CRDT MUST atomically rewrite all dependent records that reference the losing `id` so they reference the winning `id`:
- `OutboxEntry.space_id`: rewritten
- `InboxEntry.space_id`: rewritten
- `ReadMarker.space_id`: rewritten

The rewrite happens as part of the same merge transaction; partial application would orphan messages and read state. This is a per-device local operation (each bound device runs the rewrite when it processes the merge).

#### Tombstones vs leaves

Two distinct concepts that the spec previously conflated:

- **Tombstone** = explicit deletion. Result of `remove_space(space_id)`. Permanent for the deleted Space itself: its `id` is blocked forever and its history is gone. Re-creating a Space with the same dedupe key is **blocked while the deletion dominates** — a deliberate later re-create mints a NEW Space and passes; see the ZEB-1000 amendment below for the exact rule. (For community-kind spaces, joining the community fresh produces a new `id` and is always allowed.)
  - *Amended 2026-08-26 (ZEB-1000):* the dedupe-key block is **LWW by the deletion HLC**, not a forever-block — and the deliberate re-create gesture replaces the "manually clears the tombstone" step (no separate clear API exists or is needed). `remove_space_permanent` records `dedupe_tombstones[dedupe_key] = deletion HLC` (advanced strictly past the deleted row's `updated_at`); `apply_space` rejects an incoming Space whose dedupe key matches unless its `created_at` is **strictly newer** than the deletion HLC. Stale-sibling resurrections via a fresh `id` die convergently on every replica (this was the gap: the SpaceId tombstone alone could not block a re-mint with a fresh ULID), while an explicit user re-create after the deletion carries a newer `created_at` and passes. Concurrent delete-vs-create resolves deterministically on the total HLC order. Merge unions the map per-key by HLC max — rejecting future-dated stamps against the receiver's clock (ZEB-847 discipline: a forward-skewed deletion stamp would otherwise suppress every re-creation until real time caught up) — then sweeps any live row the merged deletion dominates.
- **Leave** = sets `Space.left_at: HLC` to mark "I'm not actively in this conversation right now." NOT a tombstone. Re-invitation clears `left_at` back to `null`. The Space's `id` is preserved, so chat history (OutboxEntry/InboxEntry) is intact and visible if/when you rejoin.
  - *Amended 2026-06-10 (ZEB-427):* originally scoped to DM/group-DM only; **communities now use the same mechanism.** `leave_community` first commits the self-Leave (+ EpochRotation) to the community membership CRDT — which remains the membership source of truth — then sets `left_at` on the owner-state Space row and fences it to disk. The row is retained for a future `remove_space` (ZEB-435). Nav rehydration, the boot engine sweep (the startup pass that spawns a per-community engine for each owner-state community Space), and the profile-broadcast shared-set all exclude left rows, so a left community no longer resurrects at reboot. A later invite redemption recommits the Space row (adopting the existing row's immutable `created_at` pin) with `left_at: null`, relisting it — leave stays reversible for communities exactly as for group-DMs.

**This means leaving a group-DM is reversible** if you're re-invited (with the same `id`). Tombstoning via explicit `remove_space` is not — the deleted Space and its history are gone for good; starting over with the same dedupe identity later mints a NEW Space under the ZEB-1000 LWW rule. UI should make the distinction obvious — the standard "leave" action sets `left_at`; only an explicit "delete this conversation forever" gesture writes a tombstone.

#### Convergence cases

| Case | Resolution |
|---|---|
| Two of my devices both join the same channel while disconnected | Both write `Space {kind: 'channel', id: <admin-assigned channel id>, ...}`. Same `id` → CRDT merges. Last `updated_at` HLC wins for `custom_name`/`parent`/`notification_pref`. |
| Two of my devices both create a DM with the same person while disconnected | Both write `Space {kind: 'dm', members: [me, peer]}`. Same sorted member set → CRDT merges via the dedupe-key rule above. |
| Two of my devices both create folders named "Work" while disconnected | Folders never dedupe → two distinct folders. User can manually merge if they want. |
| One device tombstones (explicit `remove_space`), another re-adds the same Space | Tombstone wins. Removal is explicit; we err on the side of "user wanted this gone." Different from leave (`left_at`) which is reversible. |
| Two devices set different `custom_name` | Last-writer-wins on `updated_at` HLC. (No merge — a UI rename is opaque.) |
| Same Space moved to two different folders | Last-writer-wins on `updated_at`. |
| Two devices' Spaces dedupe with different ULIDs | Lexicographically-smaller ULID wins. Dependent records (OutboxEntry/InboxEntry/ReadMarker) atomically rewrite their `space_id` on the local merge. |
| Bound device clock skew → HLC drift | HLC's logical counter dominates wall-clock; correct ordering preserved up to bounded skew. |

### Signature & power-level verification

- Every `SignedMembershipEvent` verified at receive time:
  1. Signature valid against `actor`'s owner pubkey
  2. `actor` has required power level for this action — see **Power-level thresholds** below
  3. If either check fails → reject + do not replicate
- Library directory entries have **two** signatures:
  - Community admin's sig over the manifest (required, always verified)
  - Library's sig over the listing (verified for trusted libraries; absent or wrong → entry shown but flagged "unattested")

#### Power-level thresholds (v1: hardcoded defaults)

```typescript
const POWER_THRESHOLDS = {
  invite:    0,    // any joined member can invite
  kick:      50,   // moderator-tier; can kick members at lower power
  set_power: 100,  // owner/admin-tier; required to grant power to others
  max:       100,
};
```

Rules:
- **Default power on join is 0.** New joiners can invite (threshold 0) but can't moderate.
- **Power level is monotone-comparable**: an action requiring power N is allowed only if the actor's current power is ≥ N. Additionally, `kick` requires the actor's power to be **strictly greater** than the target's power (you can't kick someone at your own level), preventing kick wars at the same tier.
- **Community creator starts at power 100** (set by an initial `set_power` event signed by the creator at community creation).
- **No per-community customization in v1.** Per-community threshold overrides (e.g., a community where `invite` requires power 25) are a deliberate followup; not blocking ZEB-217 Sub-C.

### Privacy mitigations

- **Owner-state Zenoh topic encryption.** The `harmony/owner/{addr}/state-root` topic is observable (anyone knowing your address can see you're publishing). But the payload is encrypted with an owner-key-derived symmetric key. Observers learn "this owner is active" but not what they're doing. *Departure from default Zenoh — needs a dedicated spec (see Followups).*
- **Read receipts stay private (v1: always; no opt-in toggle).** `last_read_at` lives only in owner-state CRDT; never shared with DM peers. Adding a per-DM opt-in toggle is a deliberate followup (see Followups), not a v1 omission. (Departure from IRCv3 MARKREAD broadcast.)
- **DM outbox is per-owner.** OutboxEntry only replicates to *my* bound devices, never to recipient. Recipient gets the message via Reticulum, never sees the outbox metadata.

### Failure modes

| Failure | Behavior |
|---|---|
| Reticulum link to recipient fails (NAT/firewall/offline) | Retry with exponential backoff. Don't surface transient errors to UI. |
| Recipient genuinely offline for >30 days, OR silently left a group-DM, OR typo'd owner_addr | All indistinguishable in v1. After 30 days, the OutboxEntry's `delivery_status` flips from `pending`/`partial` to `expired`; the delivery loop stops retrying for the unacked recipients. UI shows an "undeliverable" badge for those recipients but the OutboxEntry stays in chat history (it's the persistent sent-message log, not GC'd). User can manually delete the entry if they want it gone. **This resolves the GC-stall liveness gap that pure equality-based GC would have.** |
| Library publishes spam | User-revocable trust — `remove_library(addr)`. No global blocklist. |
| Community admin loses private key | Meta-governance failure. **Out of scope for v1.** Mitigation path: M-of-N admin power model (followup ticket). |
| Local DB corruption on one device | DAG-sync re-fetches owner-state from a peer bound device. |
| All bound devices corrupted | Hard recovery via ZEB-173 identity backup (extend backup to include owner-state CRDT root — followup ticket). |
| Group DM at 16 members tries to add a 17th | UI blocks the add. Prompt offers "Convert this group DM to a community" (carrying members forward) or "Create a new community separately" (fresh start). User decides per case. |

### Group DM mutability (resolves earlier ambiguity)

- **`members[]` is grow-only in the CRDT (3–16 cap).** Any current member can invite another, growing the list up to 16. The `id` (ULID) is stable for the lifetime of the conversation regardless of membership changes — sorted-members would shift on every add and corrupt conversation identity, which is why group-dm dedupes by `id` (see dedupe table above).
- **Leaving sets `Space.left_at`, not a tombstone.** When you leave, your local Space gets `left_at: HLC` set on this device (and replicates to your other bound devices via Flow A — your other devices see "left at HLC X" too). This is owner-local in the social sense: other participants see no change to their `members[]` view. Your receive layer drops incoming DMs while `left_at !== null`. **Re-invitation clears `left_at` back to `null`** (because dedupe by `id` means the re-invite collides with the existing left Space), so leaving a group-DM is reversible if someone re-invites you. Distinct from explicit `remove_space` (tombstone) which is irreversible — see "Tombstones vs leaves" above.
- **No "kick" primitive.** Kicking is a community-level moderation action; group DMs deliberately don't have it. If kicking someone matters to you, the relationship has already outgrown a group DM.
- **`members.length` cannot drop below 3 via the CRDT** (because `members[]` is grow-only — adds only, never CRDT-level removes). It only "appears" to drop on a particular owner's nav when their `left_at` is set, which doesn't propagate to other members' view.
- **At 17 → must convert.** UI blocks the add and prompts: "Convert to community" (membership carries forward, history retention is a separate UX choice) or "Create a new community" (fresh, no history).

### Locked-in YAGNI calls

- **One DM Space per pair forever.** Because `dm` dedupes by sorted `members[]`, there is exactly one DM Space per pair of people across all my devices and all time. There is no "start a fresh DM with Alice" flow that produces a separate Space — the existing one always wins by dedupe. This matches Signal/iMessage/WhatsApp behavior; "New DM" UI should disable when a DM with that peer already exists. Consciously different from systems like Discord where you can have multiple separate DMs with the same person.
- **No "kick" or admin tier inside group DMs.** Use a community if you need moderation.
- **No global blocklist.** Each user blocks libraries and peers locally; no federated reputation.
- **No community-to-community migration tools in v1.** If you want to "rebrand" or fork a community, you create a new one and post about it.
- **OutboxEntry not GC'd in v1.** Outbox is the persistent sent-message log; long-term archival/pruning is a deliberate followup. The active delivery loop only drives entries with `delivery_status: 'pending'` or `'partial'`; `complete`/`expired` entries stay for chat history but don't consume retry cycles.
- **Per-community power-threshold customization.** v1 hardcodes the power thresholds (see "Power-level thresholds"); per-community overrides are a deliberate followup.

## Decomposition into sub-tickets

ZEB-206 splits into four PR-sized sub-tickets. Each ships something user-visible. Build order: A → B → C → D.

| Sub | Scope | Ships when done |
|---|---|---|
| **A. Foundation** | Owner-state CRDT (Spaces + folders + read markers + outbox skeleton); multi-device DAG sync via encrypted Zenoh topic; IPC for folder management; NavService rewrite (drops `mockNavNodes`/`mockProfileStore`, consumes real state). | Empty nav tree with working folder management. CRDT plumbing proven. **Closes ZEB-209 for NavService.** |
| **B. DMs** | `dm_outbox.rs`; Reticulum link establishment + delivery loop; outbox drain; IPC `send_dm` + `dm-received`/`dm-delivered` events; ≤16-member cap; at-17 conversion UX. | Real DMs work end-to-end including offline-recipient store-and-forward. **Activates ZEB-16 plane B as a user-visible feature.** |
| **C. Communities** | `community_membership.rs`; signed-event CRDT + power levels + signature verification; IPC for join/leave/kick/invite/power; `CommunitySettingsPanel.svelte` + `InviteLinkManager.svelte`; community + channel Space kinds in NavService. | Communities can be created, joined via invite, moderated. |
| **D. Discovery** | `library_directory.rs`; library subscription + federation/republication; profile-broadcast memberships (third discovery primitive); IPC for library management; `LibraryDirectoryBrowser.svelte`. | Library-federated directory browsing + profile-linked discovery. Onboarding flow complete. |

## Acceptance criteria (umbrella ZEB-206)

- All four sub-tickets land green on `main`.
- A connected harmony-client shows zero hardcoded nav nodes.
- DMs deliver via Reticulum even when recipient was offline at send-time, and reach the recipient when *any* of their bound devices comes online.
- Communities can be joined via library directory **or** invite link **or** profile-linked discovery.
- Joining a community / starting a DM / creating a folder on phone surfaces on desktop within bounded latency (sub-second LAN, seconds across NATs).
- Existing pairing/identity tests still pass; ZEB-209 closes for NavService.
- New unit + integration test coverage for: CRDT convergence, signed-event verification, outbox drain semantics, library federation.
- Manual GUI smoke: create folder → add channel → rename → reload → state preserved across devices.

## Migration considerations

- No data migration needed — fresh users start with empty owner-state. Existing `mockNavNodes`-shaped state was never persisted to disk; it was constructed at boot.
- Test rewrites: NavService unit tests drop mock-content assertions; instead mock the IPC layer.
- `mock-data.ts` shrinks (removes `navNodes`, `mockProfileStore`); MessageService and VineService mock surfaces remain pending ZEB-209 follow-up work.

## Followup tickets (out of ZEB-206 scope)

Filed as separate Linear tickets (post-brainstorm):

1. **Reticulum global routing at billion-user scale** — transport-layer concern (related to ZEB-16). Possibly a hybrid where Zenoh handles route discovery and Reticulum handles the unicast link.
2. **Owner-state Zenoh topic encryption spec** (ZEB-211) — symmetric-key-from-owner-key derivation; blocks Sub-A.
3. **DM content encryption spec** — companion to ZEB-211 covering encryption of DM message blobs before CAS storage (per-DM-Space content key, key distribution via Reticulum invite, group-DM key rotation under membership growth). **Blocks Sub-B.**
4. **M-of-N community admin recovery** — governance failure case. Borrows from ZEB-173 identity-recovery patterns.
5. **Extend ZEB-173 backup to include owner-state CRDT root** — makes hard-recovery actually recover the user's nav tree, not just identity.
6. **Read-receipts as opt-in signed event type** — nice-to-have; very deliberate to not bake in privacy default.

## Verification gates

Before sub-ticket PRs request review:

- `npx vitest run` — all tests green
- `npx tsc --noEmit` — clean
- `cargo test --manifest-path src-tauri/Cargo.toml` — green
- `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings` — clean
- `cargo fmt --all -- --check` — clean
- Manual smoke: launch GUI on two paired devices, verify nav state converges across both
