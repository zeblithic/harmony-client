# ZEB-248 — Sub-C v2: channels-within-communities (design)

**Status:** Draft (2026-05-09).
**Linear:** [ZEB-248](https://linear.app/zeblith/issue/ZEB-248).
**Parent:** [ZEB-206](https://linear.app/zeblith/issue/ZEB-206) (Sub-C lineage).
**Predecessor:** Sub-C v1 (ZEB-217) — shipped 2026-05-09 — gave us *communities as governance primitives*: rooms with members + admin actions, **no message surface**.
**This spec:** Sub-C v2 adds the message surface — channels — without redesigning v1.
**Base commit:** `0d4fca4` (`origin/main` after PR #92 merge).

---

## 1. Context & motivation

Sub-C v1 ships communities as joinable spaces with membership, power levels, kick/invite/SetPower admin actions, and encrypted state-sync — but no way to *talk* in one. Today, opening a community in the harmony-client UI shows the member list and admin panels and nothing else. To make communities useful, we need channels: named, scoped, persistent message surfaces inside each community.

Channels are the dominant surface in modern community software (Slack, Discord, Matrix, IRC, mailing lists). v2 ships them in Harmony with three constraints driving the shape:

1. **Polycentric governance** — channels are scoped to a community; there is no global moderation, no platform admin, no algorithmic promotion. Channel admin is community-internal (extends Sub-C v1 power thresholds).
2. **Engineer for real scale** — design the substrate for billions of messages across many channels per community across many communities. Cheap per-message, bounded per-receiver, content-addressable history.
3. **Designed for eventual state** — v2 surface is intentionally Discord-minimal (text channels, anyone-Joined posts, persistent ordered history). v3 will add edits, deletes, reactions, threads, deeper history backfill, and richer content kinds. The v2 substrate must accommodate all of this *additively* — no wire-format break, no CRDT redesign.

## 2. Goals

* **Multiple named text channels per community.** Members see a per-community channel list. Selecting a channel opens its message feed.
* **Anyone-Joined posts by default.** Per-channel `write_power` knob in the wire format from day one for future announcement-style channels (UI to set it ships in v3; v2 always submits 0).
* **Mod-tier channel admin.** Power ≥ `POWER_THRESHOLDS.kick` (50) to create / modify / delete channels — same role that already moderates membership.
* **Persistent ordered history.** Receivers locally persist messages they've received. Reconnect catch-up + new-joiner backfill from peers (best-effort: whoever's online serves).
* **Auto-create `#general` on community-create.** Every community has a usable surface from the moment it exists. New joiners always have somewhere to land.
* **Substrate scales for v3.** Wire format reserves room for edits / deletes / reactions / threads / non-text content kinds. Storage shape accommodates content-addressed segment migration without wire-format break.

## 3. Non-goals (explicit scope cuts)

* **Edits / deletes / reactions on messages** — v3. Wire format reserves enum slots; v2 ships only `Post`.
* **Threading UI** — v3. Wire format includes `reply_to: Option<MessageId>`; v2 always submits `None` and the UI doesn't render thread parents.
* **Non-text content kinds (image, attachment, voice clip)** — v3. Wire format includes `kd: u8` content-kind code (0 = text in v2; 1+ reserved); v2 receivers reject non-zero kinds.
* **CAS-backed segment storage** — v3. Sealed segments in v2 are local files; the `SegmentHandle` enum reserves a `CasBook { cid }` variant for v3 swap-in. Migration is wire-format-stable.
* **Compaction / GC of sealed segments** — v3. v2 seals on threshold (1024 events / ~1 MiB) and keeps every segment indefinitely. No retention policy yet.
* **Per-community customizable `PowerThresholds`** — overlaps with [ZEB-251](https://linear.app/zeblith/issue/ZEB-251); v2 keeps Sub-C v1 hardcoded thresholds.
* **Pinned channels in nav** — declined during design (layout-C variant). All channels live inside the community view; nav stays compact.
* **Read receipts / unread tracking per channel** — separate concern, future ticket.
* **Private channels** (subset of community members can read/write) — v3+. ChannelKey HKDF derivation already supports per-channel key isolation, so future private-channel work is wire-format-additive.
* **Voice / video channels** — out of scope for ZEB-248 entirely; lives on the Voice Engine track.

## 4. Architecture overview

Two parallel substrates per community:

```
┌─────────────────────────────────────────────────────────────┐
│ Per-community state-CRDT (existing — Sub-C v1 substrate)    │
│   MembershipKey-AEAD over Zenoh state-root + CAS blob       │
│   Carries: Join, Leave, Invite, Kick, SetPower, +           │
│            ChannelCreate, ChannelModify, ChannelDelete  ←   │ ← v2 adds
│   Volume profile: low (membership + channel-config events)  │
│   Engine: CommunitySyncEngine (one per joined community)    │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│ Per-channel ChannelLog (new — Sub-C v2 substrate)            │
│   ChannelKey-AEAD over per-channel Zenoh broadcast topic    │
│   Carries: SignedChannelEvent::Post (v2)                    │
│            +Edit/Delete/React (v3 reserved)                 │
│   Volume profile: high (per-message events)                 │
│   Engine: ChannelLogEngine (one per joined channel)         │
│   Storage: segmented append-log (manifest + tail)           │
└─────────────────────────────────────────────────────────────┘
```

**Why two substrates instead of one:** the existing `CommunitySyncEngine` is optimized for low-frequency events — every change re-encrypts and re-publishes the entire community blob via state-root + CAS. That pattern is correct for membership change rates (tens per community lifetime), wrong for messages (potentially hundreds per channel per day). Folding messages into the same blob would cause O(blob-size) re-encrypt per message and unbounded blob growth. Splitting at the substrate level keeps both layers cheap in their own regime.

**Why one community-state-CRDT for channel-config (instead of a third substrate):** channel-config events (`ChannelCreate/Modify/Delete`) are the same volume profile as membership events — low frequency, governance-shaped. They naturally fit the existing engine; adding three new variants to `MembershipEventKind` is the smallest viable extension.

**Lifecycle wiring:**

* Community starts → `ChannelLogRegistry::on_community_started` walks materialized channel list, spawns a `ChannelLogEngine` per channel (subscribes to per-channel Zenoh topic, loads persisted log).
* `ChannelCreate` event materialized via state-CRDT merge → registry spawns engine for new channel.
* `ChannelDelete` event materialized → registry stops engine. Persisted log retained on disk for forensics; v3 will GC.
* App startup → load communities, materialize channels, spawn engines.

## 5. Wire format

### 5.1 Channel-config events (extends `MembershipEventKind`)

Three new variants. 1-char variant codes (consistent with v1 codes `j/l/i/k/p`); inner field keys 2-char to satisfy the same-length-keys CBOR invariant at this nesting level.

```rust
pub enum MembershipEventKind {
    // existing (Sub-C v1):
    // Join (j) / Leave (l) / Invite (i) / Kick (k) / SetPower (p)

    #[serde(rename = "c")]
    ChannelCreate {
        #[serde(rename = "ch")] channel_id: ChannelId,
        #[serde(rename = "nm")] name: String,
        #[serde(rename = "wp")] write_power: u8,
    },

    #[serde(rename = "m")]
    ChannelModify {
        #[serde(rename = "ch")] channel_id: ChannelId,
        #[serde(rename = "nm", skip_serializing_if = "Option::is_none", default)]
        name: Option<String>,
        #[serde(rename = "wp", skip_serializing_if = "Option::is_none", default)]
        write_power: Option<u8>,
    },

    #[serde(rename = "d")]
    ChannelDelete {
        #[serde(rename = "ch")] channel_id: ChannelId,
    },
}

pub type ChannelId = [u8; 16]; // ULID, generated client-side at create
```

`ChannelModify` allows partial updates: setting only `name` modifies just the display name; setting only `write_power` modifies just the gate; setting both modifies both. Modify with all fields `None` is a no-op (verified at IPC boundary; rejected before signing).

### 5.2 Channel-message events (new `SignedChannelEvent`)

```rust
#[serde(tag = "tg", content = "vl")]
pub enum SignedChannelEvent {
    #[serde(rename = "p")]
    Post {
        #[serde(rename = "id")] id: MessageId,         // ULID, stable identity for v3 references
        #[serde(rename = "ci")] community_id: SpaceId, // misroute defense
        #[serde(rename = "ch")] channel_id: ChannelId,
        #[serde(rename = "au")] author: OwnerAddr,
        #[serde(rename = "at")] at: Hlc,
        #[serde(rename = "kd")] content_kind: u8,      // 0 = text (v2); 1+ reserved
        #[serde(rename = "bd")] body: String,          // text body; v3 may make opaque if kd != 0
        #[serde(rename = "rt", skip_serializing_if = "Option::is_none", default)]
        reply_to: Option<MessageId>,                   // reserved for v3 threads; v2 always None
        #[serde(rename = "sg")] sig: [u8; 64],         // Ed25519 over canonical CBOR of the
                                                        // signed-set (id, ci, ch, au, at, kd, bd, rt)
    },
    // v3 reserved (additive — no v2 wire-format break):
    // Edit { id (target), ci, ch, au, at, kd, bd, sg }
    // Delete { id (target), ci, ch, au, at, sg }
    // React { id (target), ci, ch, au, at, em (emoji codepoint), sg }
}

pub type MessageId = [u8; 16]; // ULID, generated client-side at post
```

**Same-length-keys invariant.** All inner field keys are 2 chars (`id`, `ci`, `ch`, `au`, `at`, `kd`, `bd`, `rt`, `sg`). Outer adjacent tag uses `tg` / `vl` (2 chars). Enum-variant codes are values, not keys, so 1-char codes are fine.

**`sg` covers** canonical CBOR of `(id, community_id, channel_id, author, at, content_kind, body, reply_to)` — the entire post minus the signature itself. v3 edit / react variants will sign the same fixed-length tuple of *their own* fields (no field reuse across variants — each variant signs its own typed payload).

### 5.3 Wire packet on Zenoh

Per-channel topic: `harmony/channels/{community_id_hex}/{channel_id_hex}`.

Packet: `[12B random nonce][ChaCha20-Poly1305(ChannelKey, plaintext = canonical_cbor(SignedChannelEvent), AAD = b"harmony-channel-msg-v1")]`.

Random nonce is correct here — every packet is distinct on the wire. Replay protection lives in `ChannelLogReplayTracker`, not at the AEAD layer.

## 6. Encryption: per-channel `ChannelKey` derivation

```rust
// Derived from community MembershipKey + per-channel salt.
pub fn derive_channel_key(mk: &MembershipKey, community_id: &SpaceId, channel_id: &ChannelId) -> ChannelKey {
    let salt = community_id.0;                             // 16 B
    let info = [b"channel:" as &[u8], &channel_id[..]].concat(); // 24 B
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&salt), &mk.0)
        .expand(&info, &mut out)
        .expect("32 ≤ 8160");
    ChannelKey(out)
}
```

**Rationale:** v2 doesn't expose private channels (every Joined member knows MembershipKey, so they can derive every ChannelKey). Per-channel keys are about *future-proofing*: when v3 adds private channels, the access-control surface is "distribute ChannelKey to a subset of members" — which already works because each channel has its own key. Using MembershipKey directly for messages would force a wire-format-breaking change to add private channels later. The cost today is one HKDF expand per channel join — negligible.

## 7. Verification: `verify_channel_event`

Receive-side gate, runs on every incoming packet (live or backfill). Cheapest-first ordering to drop garbage early without expensive operations:

1. **AEAD decrypt with ChannelKey.** Failure → `AeadFailed`. Drop.
2. **Canonical CBOR decode.** Decode failure or non-canonical encoding → `MalformedWire`. Drop.
3. **Misroute defense.** Reject if `event.community_id != engine.community_id` or `event.channel_id != engine.channel_id`. Drop.
4. **Identity resolution.** `IdentityResolver::resolve(event.author) → identity_pub`. Failure → `UnknownAuthor`. Drop.
5. **Signature verify.** Ed25519-verify `sg` over canonical CBOR of `(id, ci, ch, au, at, kd, bd, rt)`. Failure → `BadSignature`. Drop.
6. **Replay-tracker check.** Reject if `at <= last_seen[(ch, au, device_id)]` per `ChannelLogReplayTracker`. → `Replay`. Drop. (Note: `device_id` is derived from the publisher's per-device HLC channel — same shape as `CommunityRootHlcTracker`.)
7. **Membership-at-HLC gate.** Materialize community state at `event.at`. Read the `ChannelInfo` for `event.channel_id` from that materialized snapshot — both `write_power` and `deleted_at` must be evaluated **as of `event.at`**, not as of "now," because `ChannelModify` may have raised/lowered `write_power` between post-time and receive-time. Reject if `author` not `Joined` at `event.at` OR `author.power` (at `event.at`) `< channel.write_power` (at `event.at`) OR (`channel.deleted_at.is_some() && event.at > channel.deleted_at`). → `NotAuthorized`. Drop.
8. **Append.** Push to `ChannelLog.tail`. Update tracker. Notify subscribers via `channel-message-received` Tauri event.

**Verification for channel-config events** (`ChannelCreate/Modify/Delete`) folds into the existing `verify_event` chain in `community_membership.rs` — adds a new branch:

```rust
MembershipEventKind::ChannelCreate { .. }
    | MembershipEventKind::ChannelModify { .. }
    | MembershipEventKind::ChannelDelete { .. } => {
    if actor_power < POWER_THRESHOLDS.kick {
        return Err(VerifyError::ChannelAdminInsufficientPower);
    }
    // No further per-variant gating in v2 — every mod-tier+ member can
    // create/modify/delete any channel. v3 may add per-channel admin scoping.
}
```

## 8. Persistence: segmented `ChannelLog`

```rust
pub struct ChannelLog {
    pub manifest: ChannelLogManifest,  // small; in-memory + on-disk
    pub tail: Vec<SignedChannelEvent>, // active append-only batch
}

pub struct ChannelLogManifest {
    pub community_id: SpaceId,
    pub channel_id: ChannelId,
    pub segments: Vec<SegmentDescriptor>, // ordered ascending by range.0
}

pub struct SegmentDescriptor {
    pub range: (Hlc, Hlc),     // [first_event.at, last_event.at] inclusive
    pub count: u32,
    pub handle: SegmentHandle,
}

pub enum SegmentHandle {
    #[serde(rename = "f")] LocalFile { rel_path: String }, // v2
    // v3 reserved (additive):
    // #[serde(rename = "c")] CasBook { cid: ContentId },
}
```

**Layout on disk:**

```
identity_dir/communities/{cid_hex}/channels/{ch_id_hex}/
├── manifest.cbor           # ChannelLogManifest
├── tail.cbor               # active Vec<SignedChannelEvent>
└── segments/
    ├── 00000000.cbor       # sealed segment 0
    ├── 00000001.cbor
    └── ...
```

**Seal operation:**

* Triggered when `tail.len() >= SEAL_THRESHOLD_EVENTS` (default 1024) OR `tail` byte size ≥ `SEAL_THRESHOLD_BYTES` (default 1 MiB), evaluated after each successful append.
* Tail written to `segments/{N:08x}.cbor` (N = next index).
* `SegmentDescriptor { range: (tail.first().at, tail.last().at), count: tail.len(), handle: LocalFile { rel_path: "segments/{N:08x}.cbor" } }` appended to manifest.
* Manifest re-serialized to disk atomically (write-temp + rename).
* Tail reset to empty.

**Seal is idempotent + crash-safe.** If we crash after writing the segment file but before updating the manifest, replay on next startup re-discovers the segment file as orphaned and replays the seal (deduped via segment content hash). Implementation detail covered at planning time.

**v3 CAS migration:** swap `LocalFile { rel_path }` for `CasBook { cid }` at seal time. Manifest descriptors are tagged-enum, so old `LocalFile` segments coexist with new `CasBook` segments indefinitely. Backfill walk reads either kind via a unified `SegmentReader` trait. CAS naturally dedupes segments across replicas of the same channel — popular channels' history can cold-bootstrap from any peer with the segment CIDs.

## 9. Sync: live broadcast + queryable backfill

### 9.1 Live broadcast

`ChannelLogEngine::publish` (post path):
1. Local IPC `post_channel_message` builds `SignedChannelEvent::Post`, signs with author identity key.
2. Engine appends to local `tail` via `verify_channel_event` (same gate as receive, ensures author can post in this channel).
3. Engine encrypts with ChannelKey, publishes on per-channel Zenoh topic.
4. Returns `message_id` to IPC caller.

`ChannelLogEngine::on_zenoh_message` (receive path):
1. Decrypt + verify per §7 chain.
2. Append to tail (and seal if threshold crossed).
3. Emit `channel-message-received` Tauri event.

### 9.2 Backfill via Zenoh queryable

Each `ChannelLogEngine` registers a Zenoh queryable on prefix `harmony/channels/{cid}/{ch_id}/since/{hlc_hex}/{limit}`. Replier:

1. Walk manifest → find segments overlapping `[since, ∞)`.
2. Read those segments in order; concatenate with current tail events `at > since`.
3. Cap at `limit` events (default 256, configurable). If more available, include continuation HLC in response trailer.
4. Re-encrypt each event with ChannelKey + per-packet random nonce, return as a stream of opaque packets (the requester re-runs `verify_channel_event` per packet — no privileged trust path on backfill).

Requester:
1. IPC `request_channel_backfill(community_id, channel_id, since)` triggers `ChannelLogEngine::request_backfill`.
2. Engine sends Zenoh query, accumulates responses.
3. Each response packet runs through `verify_channel_event` — same gate as live. Drop on any failure.
4. Verified events appended to local tail (deduped against existing by `id`).
5. Emit `channel-backfill-progress { fetched, totalEstimate? }` per N events.

**Backfill is best-effort.** If no peer is online, the request times out (default 10 s). UI shows "couldn't reach any peers" and offers retry. v3 may add background-retry + persistent queue.

## 10. Permissions

Sub-C v1 hardcoded `POWER_THRESHOLDS = { invite: 0, kick: 50, set_power: 100, max: 100 }`. v2 reuses these; no new thresholds.

| Action | Gate |
|---|---|
| Create channel | actor power ≥ 50 (kick-tier) |
| Modify channel (name/write_power) | actor power ≥ 50 |
| Delete channel | actor power ≥ 50 |
| Post in channel | actor `Joined` AND actor power ≥ `channel.write_power` (default 0) |
| List channels | actor `Joined` |
| List messages / request backfill | actor `Joined` |

**v2 UI sets `write_power = 0` on every channel** (anyone-Joined posts). The wire-format field is present but the UI to set it ships in v3 (admin-only restricted-write channels).

## 11. IPC surface

All commands return DTOs with camelCase serde rename (mirrors ZEB-265 `RedeemInviteResultDto` convention).

```rust
#[tauri::command]
async fn create_channel(
    app: AppHandle,
    community_id: String,           // hex SpaceId
    name: String,
    write_power: u8,                 // v2 UI passes 0
) -> Result<String, String>;        // returns channel_id (hex MessageId-shape)

#[tauri::command]
async fn modify_channel(
    app: AppHandle,
    community_id: String,
    channel_id: String,
    name: Option<String>,
    write_power: Option<u8>,
) -> Result<(), String>;

#[tauri::command]
async fn delete_channel(
    app: AppHandle,
    community_id: String,
    channel_id: String,
) -> Result<(), String>;

#[tauri::command]
async fn list_channels(
    community_id: String,
) -> Result<Vec<ChannelInfoDto>, String>;

#[tauri::command]
async fn post_channel_message(
    app: AppHandle,
    community_id: String,
    channel_id: String,
    body: String,
    reply_to: Option<String>,       // v2 UI always passes None
) -> Result<String, String>;        // returns message_id (hex)

#[tauri::command]
async fn list_channel_messages(
    community_id: String,
    channel_id: String,
    since: Option<HlcDto>,
    limit: u32,
) -> Result<Vec<ChannelMessageDto>, String>;

#[tauri::command]
async fn request_channel_backfill(
    app: AppHandle,
    community_id: String,
    channel_id: String,
    since: HlcDto,
) -> Result<(), String>;            // results stream via channel-message-received events
```

DTOs:

```rust
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelInfoDto {
    pub channel_id: String,        // hex
    pub name: String,
    pub write_power: u8,
    pub created_at: HlcDto,
    pub deleted_at: Option<HlcDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelMessageDto {
    pub message_id: String,         // hex
    pub channel_id: String,
    pub author: String,             // hex OwnerAddr
    pub at: HlcDto,
    pub content_kind: u8,
    pub body: String,
    pub reply_to: Option<String>,
}
```

Tauri events emitted:

* `channel-config-updated { communityId, channelId, action: 'created'|'modified'|'deleted', name?, writePower? }` — emitted from community-state-CRDT debounced merge when materialization detects a `ChannelCreate/Modify/Delete` mutation.
* `channel-message-received { communityId, channelId, message: ChannelMessageDto }` — emitted by `ChannelLogEngine` after a successful verify+append. Fired for both live and backfill admits.
* `channel-backfill-progress { communityId, channelId, fetched, totalEstimate? }` — incremental progress.

**`create_community` extension.** The existing IPC atomically emits founding `Join` for the creator. v2 extends it to also emit `ChannelCreate { name: "general", write_power: 0 }` in the same engine transaction — the call returns only after both events are durably persisted. Failure of either rolls back the whole community-create (atomicity preserved per ZEB-258 pattern).

## 12. UI surface

### 12.1 Layout

When a community NavNode is selected in the global nav, mount `CommunityView.svelte` in the main content area. Three vertical columns:

```
┌────────────┬─────────────────────────────────┬──────────────┐
│ Channel    │ Active channel feed             │ Members      │
│ sub-       │ (virtualized message list +     │ panel        │
│ sidebar    │  compose box)                   │ (collapsible)│
│            │                                 │              │
│ # general  │  alice  10:32                   │ alice (a)    │
│ # announce │   welcome everyone!             │ bob          │
│ # dev-talk │                                 │ carol        │
│ # 17-club  │  bob    10:33                   │ dan          │
│            │   got the docs up               │ ...          │
│  [+]       │                                 │              │
│            │  [ compose… ]                   │              │
└────────────┴─────────────────────────────────┴──────────────┘
```

* **Channel sub-sidebar.** Lists `ChannelInfo` sorted by `created_at` (oldest first — `#general` is always at top). Active channel highlighted. Bottom: "+" button visible only if `myPower ≥ 50` → opens `CreateChannelDialog`. Right-click on channel → context menu `Rename / Set write_power / Delete` (visible only if `myPower ≥ 50`).
* **Message feed.** Virtualized list of `ChannelMessageDto` rendered oldest-at-top, newest-at-bottom (auto-scroll-to-bottom on new arrivals if user is already at bottom; suppressed if user has scrolled up). Compose box at bottom (text input, Enter posts, Shift+Enter newline). Author rendered with profile popover hook (existing pattern).
* **Members panel.** Same surface as today's `CommunitySettingsPanel` member list, mounted in the right column. Collapsible; default-shown above ~1024 px viewport.

### 12.2 Backfill scroll-trigger

When the user scrolls to the top of the message feed AND there's an older HLC available (oldest local message `at > 0`):

1. Show "Loading older messages…" skeleton at top.
2. Fire `request_channel_backfill(community_id, channel_id, since: 0)` (or chunked: most recent already-shown `at` minus a window).
3. Listen for incoming `channel-message-received` events with `at < current oldest`.
4. After timeout or backfill-progress completion event, replace skeleton with results (or "couldn't reach peers" + retry button).

### 12.3 Dialogs

* **`CreateChannelDialog`** — name input (required, 1–32 chars). v2 always submits `write_power = 0`; the slider+number-input pair (per slider-pairing memory rule) is in the source but hidden behind a `// v3 unhide` comment so v3 just removes the hide.
* **`ModifyChannelDialog`** — same shape with current values pre-filled; allows partial update.
* **Channel deletion** — `ConfirmDialog` typed-confirm tier (per severity-confirmation memory rule — channel deletion is severe-irreversible from a UX standpoint even though messages persist on-chain). User must type the channel name to confirm.

### 12.4 Frontend service layer

* **Extend `CommunityService`** with channel-config methods: `createChannel`, `modifyChannel`, `deleteChannel`, `listChannels`. Subscribe to `channel-config-updated` event in `connectAdapter`; expose `onChannelConfigChanged` callback.
* **New `ChannelMessageService`.** Holds adapter ref + per-channel message caches. Methods: `postMessage(communityId, channelId, body, replyTo?)`, `listMessages(communityId, channelId, since?, limit)`, `subscribeToChannel(communityId, channelId, onMessage)` (returns unsub), `requestBackfill(communityId, channelId, since)`. Subscribes to `channel-message-received` and `channel-backfill-progress` events; routes them to per-channel subscribers.

### 12.5 App.svelte routing

* When selected NavNode is a community, replace `CommunitySettingsPanel`-or-blank rendering with `CommunityView`. `CommunityView` internally hosts `CommunitySettingsPanel` as a tab/modal (members + admin live there).
* Selected channel state is per-community-scoped (held inside `CommunityView`); persists across nav re-selection of the same community within a session. Defaults to the channel last viewed in this session, or `#general` on first visit.

## 13. Tests

### 13.1 Unit tests (per phase)

* **Phase 1** (`community_membership.rs` + `community_state_crdt.rs`): verify-chain for ChannelCreate/Modify/Delete with mod-tier vs below-mod-tier actors; materialize() returns correct `channels` map; tombstone semantics for ChannelDelete; canonical-CBOR fixtures for the three new variants (extends `wire_format_community_fixtures.rs`).
* **Phase 2** (`community_channel_log.rs`): `verify_channel_event` chain rejects (mismatched community/channel ID, bad signature, replay, non-Joined author, insufficient power); seal-on-threshold round-trip (write 1024 events → seal → manifest grows → tail empty → reload from disk yields same events); `derive_channel_key` HKDF determinism; AEAD round-trip with ChannelKey + AAD binding.
* **Phase 3** (`community_channel_log.rs` integration): two-engine post-and-receive; backfill walks manifest including sealed segments; ChannelLogRegistry spawn-on-create / stop-on-delete lifecycle; replay-tracker rejection.
* **Phase 4** (frontend): vitest for `CommunityService` channel-config methods, `ChannelMessageService` post/list/subscribe; component tests for `CommunityView`, `CreateChannelDialog`, scroll-trigger backfill behavior.

### 13.2 Integration tests

* **Phase 1**: `community_channel_config_integration.rs` — two engines, A creates channel → B materializes via state-CRDT sync; verify mod-tier gating end-to-end; verify default-`#general` auto-creation in `create_community`.
* **Phase 3**: `community_channel_messages_integration.rs` — two engines + one default channel; A posts 100 messages; B receives all live; B disconnects; A posts 50 more; B reconnects + backfills, verifies all 150 received in HLC order; replay rejection of duplicated packet.

### 13.3 Wire-format fixtures

Extend `wire_format_community_fixtures.rs` with canonical CBOR fixtures for:
* `ChannelCreate`, `ChannelModify` (with each field Some/None permutation), `ChannelDelete`
* `SignedChannelEvent::Post` with `reply_to: None` and `reply_to: Some(...)` permutations

## 14. Phasing

Four sequential phases. Each = its own PR cut from latest `origin/main`. Each Linear sub-issue created by the user (per never-invent-Linear-IDs rule) — this spec proposes the phase shape; sub-ticket IDs filed at planning time.

| Phase | Scope summary | Test gate |
|---|---|---|
| **1** | Channel-config CRDT — `MembershipEventKind` extension + materialize + verify gates + IPCs `create_channel/modify_channel/delete_channel/list_channels` + `channel-config-updated` event + default-`#general` auto-create in `create_community`. **Backend only.** | Two-engine integration test for create→sync→materialize. |
| **2** | ChannelLog data plane — `community_channel_log.rs` (SignedChannelEvent, ChannelKey HKDF, ChannelLog manifest+tail+segmentation, ChannelLogReplayTracker, verify_channel_event). **In-process only — no Zenoh wiring.** | Unit tests: verify chain, seal/restore round-trip, replay rejection, AEAD binding. |
| **3** | ChannelLog Zenoh transport — ChannelLogEngine wired to per-channel Zenoh broadcast + queryable backfill; ChannelLogRegistry lifecycle; IPCs `post_channel_message/list_channel_messages/request_channel_backfill`; events `channel-message-received` + `channel-backfill-progress`. | Two-engine integration: live post-and-receive + offline-then-backfill + replay rejection. |
| **4** | Channel UI — `CommunityView.svelte` three-column layout; channel sub-sidebar with create/modify/delete dialogs (typed-confirm delete); message feed with virtualization + compose + scroll-trigger backfill; `ChannelMessageService`; App.svelte routing changes; `CommunityService` channel-config methods. | Vitest service + component tests; manual smoke at end of phase. |

**Cross-repo:** none — entirely in `harmony-client`.

**Branch / PR shape:** four separate PRs in sequence. Each cuts from latest `origin/main`. Phase N+1 cannot start until Phase N is merged on main.

**Acceptance for ZEB-248 closure:** all four phases shipped + UI smoke passes for: create channel, post message, see it land on second device, delete channel, backfill on cold reconnect.

## 15. Open questions deferred to plan-time

These don't change the architecture but need decisions during the per-phase implementation plan:

* **Zenoh queryable response format.** Stream of opaque packets vs single concatenated packet. Probably stream (matches how community state-sync handles CAS-blob fetches). Plan-time.
* **Backfill request batching / rate-limit.** Currently spec says best-effort with 10 s timeout. May need server-side queue if multiple peers ask simultaneously. Plan-time.
* **`ChannelLog.tail` flush cadence.** Currently spec says debounced (mirroring state-CRDT engine). Threshold values (debounce interval, batch size) at plan-time.
* **`ChannelLogRegistry` storage of stopped engines.** When `ChannelDelete` materializes, stop the engine — but does the registry retain the descriptor for later "channel undelete"? v2 says no (delete is one-way). Plan-time confirmation.
* **Test fixture for sealed-segment-replay.** The seal threshold is 1024 events; integration tests probably want a lowered threshold (e.g., 8) to exercise seal/reload paths in reasonable time. Plan-time configurable.

## 16. References

* **Sub-C v1 spec:** `docs/specs/2026-05-05-zeb-217-sub-c-communities-design.md` — substrate this builds on.
* **Sub-C v1 phases:** ZEB-217 Tasks 1–5 (Phases 1–5: membership CRDT → state CRDT → open community flow → invite-only/kick/SetPower → frontend).
* **DM transport:** `docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md` — pattern reference for per-recipient encrypted unicast (different shape but similar threat-model thinking).
* **Reference files in `harmony-client`:**
  - `src-tauri/src/community_membership.rs` — MembershipEventKind, verify_event, POWER_THRESHOLDS
  - `src-tauri/src/community_state_crdt.rs` — CommunityState, materialize, MaterializedMembership
  - `src-tauri/src/community_state_sync.rs` — CommunitySyncEngine, CommunityRootHlcTracker
  - `src-tauri/src/community_invite.rs` — admin-bootstrap pattern, side-channel verification
  - `src-tauri/src/lib.rs` — IPC patterns, atomic-rollback in `create_community`
  - `src/lib/nav-service.ts` — NavNode shape, addOrUpdateNavSpace
  - `src/lib/community-service.ts` — frontend service pattern, RedeemInviteResultDto camelCase convention
  - `src/App.svelte` — top-level mount routing, dialog handlers
* **Memory rules applied** (HARD RULES):
  - No worktrees; pull-before-work; never invent Linear IDs
  - Engineer for real scale (per-channel substrate; segmentation; CAS-future-compat)
  - Polycentric governance (channel admin community-scoped, no global moderation)
  - Tier confirmation to severity (typed-confirm on channel deletion)
  - Slider pair with number input (write_power UI in v3)
  - Design for eventual state (channels exist from community-create; new joiners always have a surface)
