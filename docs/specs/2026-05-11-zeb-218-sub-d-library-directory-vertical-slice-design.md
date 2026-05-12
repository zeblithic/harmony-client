# ZEB-218 Sub-D Phase 1 — Library-Federated Discovery Directory (Vertical Slice)

**Status:** Design  
**Author:** J Eng  
**Date:** 2026-05-11  
**Parent:** [ZEB-218](https://linear.app/zeblith/issue/ZEB-218/) (Sub-D library-federated discovery directory + browse UI)  
**Grandparent:** [ZEB-206](https://linear.app/zeblith/issue/ZEB-206/) (nav-tree + spaces unified design)

---

## 1. Goal

Ship a working "Browse Communities" feature: the user adds one or more
trusted libraries by pasting an `OwnerAddr`, the client subscribes to each
library's directory topic, the user browses an aggregated catalog with
deduplication across libraries, and clicking an entry triggers the
existing `redeem_invite` IPC flow.

This is **Phase 1 of Sub-D** — the consumer side only. Phases 2-6 (auto-
discovery, federated republication, profile-membership broadcast, and
direct-join IPC) are explicitly deferred to follow-up sub-tickets.

## 2. Why this shape (not the full Sub-D scope)

Sub-D's original scope has six logical phases. Bundling all six into one
PR would produce the largest cross-cutting change in this project's
history, concentrate protocol-correctness risk, and delay any user-
visible value. Phase 1 + minimal Phase 5 (frontend) is the **smallest
vertical slice that ships a working Browse Communities feature** end-to-
end. Once it's in production, follow-up sub-tickets layer in auto-
discovery, federation, and profile-broadcast incrementally — each
verifiable against a working baseline.

The slice deliberately reuses `redeem_invite` rather than building a
direct-join IPC because:

1. **Open-community invites already exist** post-[ZEB-249](https://linear.app/zeblith/issue/ZEB-249/).
   `CommunityInvitePayload` with `is_invite_only: false` carries an
   unsealed 32-byte EpochKey ("the key is public for open communities —
   anyone with the link can join and receive it" per
   `src-tauri/src/community_invite.rs:25-29`). Library entries carrying
   an open-community invite URL just work.
2. **Stale-rotation case is already handled.** When an open-community
   invite URL is published to a library and the community later rotates
   its epoch (via Kick/Leave from ZEB-249), the URL's `epoch_snapshot`
   goes stale. The joiner redeeming a stale invite gets caught up
   automatically by `EpochCatchup` from ZEB-249 §4.6 (admin observes
   pending_catchup_for and synthesizes the catchup event). No new
   protocol surface.
3. **Direct-join IPC** ([ZEB-252](https://linear.app/zeblith/issue/ZEB-252/))
   currently uses stale `MembershipKey` language. Updating it in the
   same PR as this round would double the scope. Deferred — that ticket
   will be rewritten before its own implementation round.

## 3. Architecture overview

```
                ┌───────────────────────────────────┐
                │ harmony/discovery/library/        │
                │   {lib_addr}/communities          │
                └──────────────┬────────────────────┘
                               │ Zenoh subscribe
                               ▼
            ┌─────────────────────────────────────────┐
            │ library_directory.rs                    │
            │  ▸ subscriptions: HashMap<LibAddr, Sub> │
            │  ▸ entries: BTreeMap<CommunityId,       │
            │             AggregatedEntry>            │
            │  ▸ verify per-entry community_signature │
            │  ▸ dedupe by community_id, LWW on HLC   │
            └──────────────┬──────────────────────────┘
                           │
              ┌────────────┴──────────────┐
              ▼                           ▼
    list_libraries / add /          library-directory-
    remove / browse_library          updated IPC event
    IPCs                                    │
              │                             ▼
              ▼                    LibraryDirectoryBrowser
       owner-state CRDT             (Svelte)
       (library list syncs           ▸ Add library dialog
        across devices)              ▸ Catalog list
                                     ▸ Click → existing
                                       redeem_invite path
```

## 4. Data model

### 4.1 Wire format — `LibraryDirectoryEntry`

Published by libraries to `harmony/discovery/library/{lib_addr}/communities`.
CBOR with 2-char field keys (codebase convention; see Sub-C
`MembershipEventKind` for prior art).

```rust
/// Published by libraries to their per-library directory topic.
/// 2-char field keys (codebase convention; aligns with MembershipEventKind).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryDirectoryEntry {
    /// Community being listed (also the dedupe key across libraries).
    #[serde(rename = "cd")]
    pub community_id: SpaceId,

    /// Community admin's signing key (verifies `community_signature`).
    #[serde(rename = "ca")]
    pub community_addr: OwnerAddr,

    /// User-visible community name (admin-curated).
    #[serde(rename = "nm")]
    pub name: String,

    /// User-visible description (admin-curated).
    #[serde(rename = "ds")]
    pub description: String,

    /// Tags for browse filtering (admin-curated).
    #[serde(rename = "tp")]
    pub topics: Vec<String>,

    /// Open-community invite URL. Clicking "Join" feeds this through
    /// the existing `redeem_invite` IPC. Library must only publish
    /// entries for OPEN communities (invite-only communities require
    /// per-invitee URLs that cannot be shared in a directory).
    #[serde(rename = "iu")]
    pub invite_url: String,

    /// Library's identity (also the topic owner).
    #[serde(rename = "lb")]
    pub listed_by: OwnerAddr,

    /// Library's HLC at publish time. Used for LWW dedupe across libraries
    /// that list the same community.
    #[serde(rename = "la")]
    pub listed_at: Hlc,

    /// Community admin's Ed25519 signature over all preceding fields
    /// (canonical CBOR, fields in the declared order, `cs` excluded).
    /// Verified against `community_addr`; mismatch → entry rejected.
    #[serde(rename = "cs")]
    pub community_signature: Signature,

    // Library wrapping signature deferred to Phase 3 federation.
}
```

**Validation invariants** (enforced at receive time):
- `community_addr.len() == 32` and decodes to a valid Ed25519 pubkey
- `listed_by.len() == 32` and matches the topic's library address
- `community_signature` verifies against `community_addr` over the
  canonical-CBOR encoding of all fields except `cs`
- `invite_url` parses as a valid open-community invite via existing
  `parse_invite_url` (with `is_invite_only == false`). Invite-only URLs
  in directory entries are rejected.
- `name.len() <= 200`, `description.len() <= 2000`,
  `topics.iter().all(|t| t.len() <= 64)`, `topics.len() <= 16`
  (anti-spam bounds; see §7)

### 4.2 Owner-state CRDT — `LibraryEntry`

A new collection `libraries: BTreeMap<OwnerAddr, LibraryEntry>` joins
`spaces`, `outbox`, `inbox`, `read_markers` in owner-state.

```rust
/// User's per-library trust record. Lives in owner-state CRDT; syncs
/// across bound devices via existing Flow A.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryEntry {
    /// Library OwnerAddr (also the BTreeMap key).
    #[serde(rename = "ad")]
    pub address: OwnerAddr,

    /// HLC when the user added this library. Used for LWW tie-break.
    #[serde(rename = "at")]
    pub added_at: Hlc,

    /// Tombstone HLC. When `Some`, the user removed this library;
    /// removed_at > added_at means remove wins. Re-add clears the
    /// tombstone if the new added_at > removed_at.
    #[serde(rename = "rm", skip_serializing_if = "Option::is_none")]
    pub removed_at: Option<Hlc>,
}
```

**LWW semantics** for add/remove:
- Effective state at any HLC = `removed_at.is_none() || added_at > removed_at`
- Add on Device A + Remove on Device B at later HLC → remove wins
- Subsequent re-add on Device A at an HLC later than the remove → re-add
  wins (clears `removed_at` semantically; the stored value updates only
  when the higher-HLC operation is observed)
- Cross-device convergence proven by the same LWW tests Sub-C already
  uses for membership events

**Tombstone retention**: tombstones are NEVER GC'd. A user adding and
removing 1000 libraries over a lifetime accumulates 1000 entries; at
~80 bytes per `LibraryEntry`, that's 80 KB. Acceptable.

### 4.3 In-memory aggregation — `AggregatedEntry`

Lives in `library_directory.rs`. Not persisted; rebuilt on startup by
replaying subscriptions.

```rust
/// One entry per community_id, deduped across libraries.
#[derive(Debug, Clone)]
struct AggregatedEntry {
    /// Latest (highest-HLC) entry observed for this community.
    entry: LibraryDirectoryEntry,

    /// Set of libraries that have listed this community. Useful for the
    /// frontend to surface "listed by 3 libraries" hints. Also used at
    /// remove_library time to evict entries that no library still lists.
    listed_by: BTreeSet<OwnerAddr>,
}

type Aggregation = BTreeMap<SpaceId, AggregatedEntry>;
```

## 5. Subscription lifecycle

### 5.1 Subscribe path

Driven by `library_directory.rs`, parallel to how `community_state_sync.rs`
manages per-community subscriptions:

1. On `add_library(addr)`:
   - Validate `addr` is a well-formed Ed25519 pubkey hex (64 chars, 32
     bytes decoded)
   - Acquire owner-state lock, insert `LibraryEntry { address: addr,
     added_at: now_hlc(), removed_at: None }` (or update if exists with
     newer HLC than any tombstone). Release lock.
   - Persist via existing owner-state CRDT writeback (Flow A
     replicates to bound devices)
   - Register Zenoh subscriber for
     `harmony/discovery/library/{addr}/communities`. Hand the subscriber
     to the event loop, which routes incoming samples to
     `LibraryDirectory::on_entry`.
2. On app startup:
   - Walk owner-state `libraries` map; for each non-tombstoned entry,
     register a subscriber. Same path as above without the persist step.

### 5.2 Receive path (per entry arrival)

`LibraryDirectory::on_entry(sample: CborBytes)`:

1. Decode CBOR into `LibraryDirectoryEntry`. **Reject on decode failure**
   (logged at warn-level, dropped, no IPC event)
2. Run validation invariants from §4.1. **Reject on any failure**
3. Verify `community_signature` against `community_addr` over the
   canonical-CBOR encoding of all preceding fields. **Reject on
   verification failure**
4. Look up `community_id` in `Aggregation`:
   - **New**: insert `AggregatedEntry { entry, listed_by: {listed_by} }`
   - **Existing with older `listed_at`**: replace `entry` field
     (latest-HLC wins); union the new `listed_by` into the set
   - **Existing with same `listed_at` from same library**: idempotent
     no-op
   - **Existing with same `listed_at` from different library**: union
     `listed_by` only
   - **Existing with newer `listed_at`**: drop (older entry, no-op)
5. Emit `library-directory-updated` IPC event with payload
   `{ communityId: hex(community_id) }`

### 5.3 Teardown on `remove_library`

1. Set `LibraryEntry.removed_at = now_hlc()` in owner-state CRDT (persist
   + replicate)
2. Drop Zenoh subscriber for that library's topic
3. Walk `Aggregation`; for each entry, remove the library from `listed_by`
4. If `listed_by` becomes empty, evict the aggregation entry
5. Emit `library-directory-updated` (no community_id — caller re-fetches)

**Cost**: O(N) where N = total entries from that library. At the per-
library cap (§7) of 10k, this is <10ms.

## 6. IPC surface

All snake_case Rust; Tauri auto-converts to camelCase at the JS
boundary (codebase convention; see `feedback_tauri_error_extraction`).

```rust
/// List all libraries the user has added (effective set; excludes
/// tombstoned entries).
async fn list_libraries() -> Vec<LibraryInfo>;

/// LibraryInfo carries enough for the frontend to render a chip:
/// short-hex address + count of entries currently aggregated from
/// this library.
struct LibraryInfo {
    address: String,        // hex
    added_at: Hlc,
    entry_count: usize,     // from aggregation
}

/// Add a library by OwnerAddr (hex). Idempotent on re-add.
/// Validates well-formed pubkey; subscribes; persists; replicates.
async fn add_library(library_addr: String) -> Result<(), String>;

/// Remove a library. Tombstones the LibraryEntry; drops subscription;
/// evicts aggregation entries that no other library still lists.
async fn remove_library(library_addr: String) -> Result<(), String>;

/// Browse the catalog. `None` = aggregated across all libraries; `Some`
/// filters to entries from one specific library (mostly diagnostic;
/// primary UX is the aggregated view).
async fn browse_library(library_addr: Option<String>)
    -> Vec<DirectoryEntryDTO>;

/// DTO for frontend rendering. Strips `community_signature` (already
/// verified) and adds the `listed_by_count` hint.
struct DirectoryEntryDTO {
    community_id: String,    // hex
    community_addr: String,  // hex
    name: String,
    description: String,
    topics: Vec<String>,
    invite_url: String,      // for click-to-join
    listed_by_count: usize,  // how many libraries list this community
    listed_at: Hlc,
}
```

**IPC event**: `library-directory-updated` — payload
`{ communityId: string | null }`. Emitted on entry arrival and on
remove_library teardown. Frontend debounces 200ms before re-fetching.

## 7. Frontend

### 7.1 New component — `LibraryDirectoryBrowser.svelte`

Three states:

**Empty (no libraries)**:
- Single CTA card: *"Add a library to start browsing communities"*
- Click → opens `AddLibraryDialog` (inline `<dialog>`-style, matching
  `CreateCommunityDialog.svelte` pattern)

**With libraries, browsing**:
- Top bar: chips for each added library showing short-hex (8 chars +
  ellipsis) with a remove ✕. Plus a "+ Add library" button.
- Main panel: scrollable list of `DirectoryEntryDTO`. Each row:
  - Name (bold) + truncated description
  - Topic chips
  - `listed_by_count` hint: *"Listed by 2 libraries"* (subtle)
  - Primary action button: **Join** → calls `redeem_invite(invite_url)`,
    closes the browser, navigates to the new community Space on success

**Add-library dialog**:
- Input: hex `OwnerAddr` (64 hex chars). Inline validation (length +
  hex-charset).
- Submit → `add_library(addr)`. On success, dialog closes, browser
  refreshes via `browse_library(null)`. On error, surface inline.

### 7.2 Mount point

NavPanel's existing "Browse Communities" affordance — spec
`docs/specs/2026-04-30-zeb-206-nav-tree-design.md` §421 prescribes adding
a "Browse Libraries" entry. Exact wiring (FAB menu item vs. empty-state
button vs. dedicated nav-tree node) belongs in the implementation plan;
the design only specifies that the browser is reachable from NavPanel.

### 7.3 Event handling

- Subscribe to `library-directory-updated` event on `onMount`
- On event: debounce 200ms; then re-fetch `browse_library(null)` for
  the current aggregated view
- Unsubscribe on `onDestroy`

## 8. Click-to-join flow

The library entry's `invite_url` IS the entire join protocol surface:

1. User clicks "Join" on an entry
2. Frontend calls `redeem_invite(invite_url)` — existing IPC, unchanged
3. `redeem_invite` parses the URL, decodes the `CommunityInvitePayload`,
   validates it's `is_invite_only: false` (open community), pulls the
   unsealed 32-byte EpochKey out of `InviteEpochSnapshot.sealed_epoch_key`,
   subscribes to community state, materializes from the snapshot, and
   creates the local Space
4. If the URL's `epoch_snapshot` is stale (community rotated since the
   library published the entry), the joiner's `current_epoch_key` won't
   match the live epoch on the membership topic. An admin's self-healing
   observer (ZEB-249 §4.6) issues an `EpochCatchup` for the new joiner.
   The joiner's `pending_catchup_for` clears, `current_epoch_key`
   advances, and decryption succeeds.

**No new code path.** The stale-URL handling is entirely covered by
ZEB-249's existing self-healing.

## 9. Error handling

| Failure | Behavior |
|---|---|
| User pastes malformed library address | `add_library` returns `Err("invalid library address: ...")`; frontend surfaces inline in dialog |
| Library address valid but never publishes | Silent — no entries appear under that filter. Acceptable; no UI affordance to distinguish "library is silent" from "no entries." |
| Library entry arrives with malformed CBOR | Logged at warn-level; dropped; no IPC event emitted |
| Library entry's `community_signature` fails to verify | Logged at warn-level; dropped. Federation/republication (the "unattested" badge in spec §486-489) is deferred to Phase 3. |
| Library entry's `invite_url` is invite-only (not open) | Validation rejects at receive time. Logged at warn-level; entry not added to aggregation. |
| Library entry references a community whose admin pubkey rotated | `community_signature` won't verify against the new key — entry dropped. Admin-key-rotation is not a primitive Harmony ships yet. |
| Click "Join" but the invite URL is stale (epoch rotated since publish) | `redeem_invite` itself handles via ZEB-249 §4.6 EpochCatchup. No new code path. UI shows a brief "syncing" state until catchup completes. |
| Cross-device convergence: add-on-A + remove-on-B at same HLC | LWW tie-break by HLC's logical counter + device_id (existing pattern). Deterministic across replicas. |
| User removes a library while a subscriber is mid-receive | The subscriber is dropped at the Zenoh handle level; any in-flight `on_entry` calls that complete after drop write to an aggregation that immediately evicts them on next `remove_library` pass. No corruption. |
| Aggregation map at per-library cap (10k entries) | Newest-first by `listed_at`: when a new entry from library L arrives and L already contributes 10k entries to aggregation, evict L's oldest contribution. |

## 10. Performance / scale

**Per-library entry cap**: `MAX_ENTRIES_PER_LIBRARY = 10_000`. Hardcoded
constant. Libraries publishing more get their oldest contributions
evicted as new ones arrive (newest-first by `listed_at`).

**Aggregation map size**: At 10 libraries × 10k entries = 100k entries
worst case. Each `AggregatedEntry` is ~500 bytes (long strings dominate).
~50 MB worst case heap. Acceptable on a desktop client; mobile dev
considerations belong in Phase 5+ work, not this slice.

**Signature verification rate**: Ed25519 verify is ~50 μs per entry on
typical desktop HW (zeblith's reference machine). 10k entries cold-
replay = ~500 ms. On app startup with N libraries each replaying
catalogs, this could approach 5 s at the 10k×10 worst case. Mitigation:
verify in a `tokio::task::spawn_blocking` pool with yield points;
emit `library-directory-updated` incrementally so the UI lights up as
entries arrive. Implementation detail.

**Subscription teardown** (`remove_library`): O(N) over total entries
from that library. At cap of 10k, <10 ms.

**Browse fetch latency**: `browse_library(null)` snapshots the
`Aggregation` map — O(N) clone but small const-factor (BTreeMap iter is
cache-friendly). At 100k entries the clone is ~50 ms. For v1 we accept
this; if it becomes a UX issue, paginate or stream via a Tauri channel.

## 11. Testing

### 11.1 Test fixture (Cargo feature `test-fixtures`)

New module `tests/common/library_fixtures.rs`. Provides:

```rust
/// Constructs a LibraryDirectoryEntry signed by a deterministic test
/// keypair. Mirrors how community_membership::tests build signed
/// MembershipEvents.
pub fn mock_directory_entry(
    community_id: SpaceId,
    community_signing_key: &SigningKey,
    library_addr: OwnerAddr,
    listed_at: Hlc,
    invite_url: String,
) -> LibraryDirectoryEntry { ... }

/// Publishes a sequence of entries through an in-process Zenoh handle
/// for end-to-end consumer testing.
pub async fn mock_library_publisher(
    library_signing_key: &SigningKey,
    entries: Vec<LibraryDirectoryEntry>,
) -> InProcessZenohHandle { ... }
```

### 11.2 Integration tests — `tests/library_directory_integration.rs` (new)

- `subscribe_to_library_receives_published_entries` — add library, mock
  publishes 3 entries, consumer aggregates, `browse_library(None)`
  returns 3
- `aggregation_dedupes_same_community_from_two_libraries` — both
  libraries publish entry for community X, single entry in browse,
  `listed_by_count == 2`
- `latest_hlc_wins_on_conflict` — same community_id, library publishes
  twice with different HLCs, only the newer survives
- `invalid_community_signature_rejected` — malformed sig → entry
  dropped, no `library-directory-updated` event
- `invite_only_invite_url_rejected_at_receive` — entry carrying an
  invite-only URL is dropped (open-only directory invariant)
- `remove_library_evicts_entries_and_drops_subscription` — full
  teardown path; aggregation empty after remove
- `click_to_join_redeem_invite_smoke` — entry's `invite_url` feeds
  through existing `redeem_invite` happy path; new Space appears
- `cross_device_library_list_converges` — add on Device A, remove on
  Device B at later HLC → remove wins on both
- `per_library_cap_evicts_oldest_on_overflow` — publish 10001 entries
  from one library; oldest dropped; newest 10000 present

### 11.3 Unit tests — `library_directory.rs::tests`

- Wire format CBOR round-trip
- 2-char field key absence test (CBOR-prefix matching, like Sub-C's
  `non_community_space_skips_membership_fields_in_wire`)
- Signature verification: happy + tampered-payload + wrong-key
- Aggregation invariants:
  - Latest-HLC-wins replaces `entry`
  - `listed_by` set unions correctly across libraries
  - Eviction when `listed_by` becomes empty
  - Per-library cap eviction order
- LWW for `LibraryEntry` add/remove:
  - effective(add@H1) = true
  - effective(add@H1 + remove@H2 where H2 > H1) = false
  - effective(add@H1 + remove@H2 + add@H3 where H3 > H2) = true
  - Same-HLC tie-break by logical counter + device_id

### 11.4 Wire-format pinning fixtures — `tests/wire_format_library_directory_fixtures.rs` (new)

Canonical CBOR bytes for `LibraryDirectoryEntry` and `LibraryEntry`,
pinned against accidental field-key rename. Follows the existing pattern
in `tests/wire_format_community_sync_fixtures.rs`.

### 11.5 Frontend vitest — `src/lib/components/__tests__/LibraryDirectoryBrowser.test.ts` (new)

- Empty-state CTA renders when `list_libraries()` returns `[]`
- Add-library dialog: paste valid hex → submit → `add_library` IPC
  called with correct arg; dialog closes on success; error displays
  inline on failure
- Browse list renders aggregated entries; topic chips, listed-by hint
  visible
- Click "Join" → `redeem_invite` IPC called with `entry.invite_url`
- Remove library chip → `remove_library` IPC called; entry list refetches
- `library-directory-updated` event triggers debounced refetch

## 12. Deferred follow-ups (each becomes a Backlog sub-ticket of ZEB-218)

Filed **before** this PR merges, so the PR body can reference them
cleanly:

| Phase | Title | Description |
|---|---|---|
| 2 | "Sub-D library auto-discovery via announce topic" | Subscription to `harmony/discovery/library/announce`; UI affordance to enroll discovered libraries (with explicit user consent — auto-add is incompatible with paste-an-address-only trust model) |
| 3 | "Sub-D federated republication of directory entries" | Library wrapping signature; cross-library re-syndication; "unattested" badge per spec §486-489 |
| 4 | "Sub-D ProfileMembershipBroadcast primitive" | Third discovery primitive — opt-in per-community, owner-curated subset broadcast via `harmony/announce/{owner_addr}/memberships` |
| 6 | (rewrite [ZEB-252](https://linear.app/zeblith/issue/ZEB-252/)) | Direct-join IPC bypassing redeem_invite for open communities. ZEB-252's current description uses stale `MembershipKey` terminology; rewrite to align with the open-community invite-URL path shipped in this round |

## 13. Out of scope this round (explicit non-goals)

- **Library hosting itself**. A library is a separate role (separate
  repo if it exists). This round ships only the consumer side. Tests use
  mock fixtures.
- **Library auto-discovery** via the announce topic. Deferred to Phase 2.
- **Federated republication** with library wrapping signatures.
  Deferred to Phase 3.
- **ProfileMembershipBroadcast** as a discovery primitive. Deferred to
  Phase 4.
- **Direct-join IPC** that bypasses redeem_invite. Deferred to Phase 6
  (rewrites ZEB-252).
- **Library-side curation policy** (what does the library choose to
  publish?). Out of scope; library is just a publisher we trust by
  manual address-add.
- **Spam blocklist / reputation**. User-revocable trust only:
  `remove_library(addr)`. No global blocklist (anti-polycentric).
- **Invite-only communities in the directory**. Entries with invite-only
  invite URLs are explicitly rejected at receive. Invite-only discovery
  could come later if needed but adds privacy concerns out of scope here.

## 14. Acceptance criteria

ZEB-218 parent acceptance criteria → coverage in this round:

| Criterion | Coverage |
|---|---|
| Adding a library subscribes to its directory topic | ✅ `add_library` IPC + integration test `subscribe_to_library_receives_published_entries` |
| Browsing a library shows its catalog with proper signature verification | ✅ `browse_library` IPC + community-sig verification at receive + sig-fail integration test |
| Joining a community from the directory triggers Sub-C's join flow correctly | ✅ `redeem_invite` reuse + click-to-join smoke test |
| Federated republication: library A republishes library B's entry with wrapping sig | ⏭ Deferred to Phase 3 |
| Profile membership broadcast: opt-in per-community visibility | ⏭ Deferred to Phase 4 |
| All gates green | ✅ 5 CI gates (cargo fmt + clippy + nextest + check + frontend tsc + vitest) |

ZEB-218 remains In Progress after this PR merges. Phases 2-6 sub-tickets
close it when each lands.

## 15. References

- Original Sub-D scope: `docs/specs/2026-04-30-zeb-206-nav-tree-design.md`
  §3.3 (LibraryDirectoryEntry at L217-232), §3.4 (ProfileMembershipBroadcast
  at L235-246), Flow D at L327-334, Component design at L340-352, IPC
  surface at L391-394, frontend at L416, signature verification at
  L486-489.
- ZEB-249 backward secrecy: `docs/specs/2026-05-11-zeb-249-community-backward-secrecy-design.md`
  for the open-community invite-URL shape (§3.4, §5.1) and the
  EpochCatchup self-healing (§4.6) that handles stale invite URLs.
- Existing patterns referenced:
  - `src-tauri/src/community_invite.rs::InviteEpochSnapshot` — open vs
    invite-only invite payload shape
  - `src-tauri/src/community_invite.rs:7069 build_open_invite_url` — IPC
    libraries call (off-client) to mint URLs for their entries
  - `src-tauri/src/community_state_sync.rs::CommunitySyncRegistry` — the
    subscription-lifecycle pattern `library_directory.rs` mirrors
  - `src-tauri/src/community_membership.rs::MembershipEventKind` — the
    2-char-field-key + canonical-CBOR + Ed25519-signature pattern
  - `tests/wire_format_community_sync_fixtures.rs` — the wire-format
    pinning pattern
