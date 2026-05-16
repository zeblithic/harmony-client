# ZEB-285 Phase 2: community forking UX (disclosure + descendants + chain)

**Branch:** `zeb-285-phase2-fork-lineage-ux`
**Parent:** [ZEB-285](https://linear.app/zeblith/issue/ZEB-285) (community forking primitive — Phase 1 shipped via PR [#122](https://github.com/zeblithic/harmony-client/pull/122))
**Phase 1 spec:** [`docs/specs/2026-05-14-zeb-285-phase1-community-forking-design.md`](2026-05-14-zeb-285-phase1-community-forking-design.md) (commit `e318823`)
**Phase 2 Linear ticket:** filed post-spec — to be referenced in the PR body
**Related:** [ZEB-217](https://linear.app/zeblith/issue/ZEB-217) (Sub-C v1 community CRDT), [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) (Sub-C v2 channels), [ZEB-281](https://linear.app/zeblith/issue/ZEB-281) (Profile-membership broadcast — referenced for deferred follow-up only)

## 1. Context

### 1.1 Origin

Phase 1 (PR [#122](https://github.com/zeblithic/harmony-client/pull/122), merged 2026-05-15) shipped the community forking primitive — `MembershipEventKind::Fork` variant, `CommunityState.forked_from: Option<SpaceId>` field, `PreForkSnapshot` bundle in invite payloads, `verify_snapshot_event` dual-keyset verifier, `fork_community` IPC, plus a minimal UI surface (ForkConfirmDialog, NavService fork-glyph, single-line Lineage block).

The PR body's "Out of scope" section enumerated 10 concrete Phase 2/3 follow-ups. Phase 2 takes the three user-visible items (#1, #2, #4 in that list) — disclosure UX, surfacing visible Fork events as descendants of the original community, and multi-hop fork-of-fork chain visualization.

### 1.2 Phase 1 baseline

Phase 1 stores fork lineage as a single-hop pointer: `CommunityState.forked_from: Option<SpaceId>`. The `PreForkSnapshot` carries this pointer in fork-invites so a freshly-redeemed fork knows its immediate parent. The Phase 1 Lineage block in `CommunitySettingsPanel.svelte` renders only when `forked_from` is `Some` and shows the immediate parent's name only.

Visible vs silent fork modes are both supported in Phase 1: visible forks emit a signed `MembershipEventKind::Fork { fork_space_id }` event in the original community's membership log; silent forks emit nothing. Either way, the fork itself is a fully-independent new community.

The Phase 1 UI does not surface visible Fork events anywhere — they're persisted in the original's log but invisible to original-community members. The Lineage block does not walk a chain beyond the immediate parent.

### 1.3 What Phase 2 lands

- A renamed-and-restructured **Forks** section in `CommunitySettingsPanel.svelte` that always renders for every community (not only forks), carrying a polycentric-framing explainer paragraph and a `ForkLineageTree` visualization.
- Multi-hop ancestor-chain support via a new `parent_lineage: Vec<ParentLineageEntry>` field on `CommunityState` and `PreForkSnapshot`. The chain is **baked in at fork-time** — the forker freezes their full ancestor chain into the invite snapshot, sidestepping the encrypted-ancestor-state problem (a fork joiner who was never a member of the grandparent can't decrypt the grandparent's state, but they CAN read a frozen list embedded in the snapshot they received).
- A new `list_community_forks` IPC that walks the original community's membership log for `MembershipEventKind::Fork` events, resolves forker display names via the existing `member_info_for` ladder, and returns a chronological list of `ForkDescendantDto` rows.
- A new `get_community_lineage` IPC that exposes the new lineage fields on `CommunityState` to the frontend behind a tight DTO (rather than leaking CommunityState wholesale).
- A new `ForkLineageTree.svelte` component rendering ancestors above, "you are here" highlighted in the middle, descendants below — single coherent tree, semantic HTML, keyboard-navigable, `aria-current="page"` on self.
- All wire-format additions are backwards-compatible: Phase 1 forks decode cleanly with default-empty lineage; new Phase 2+ forks carry the full chain.

## 2. Decisions (with rationale)

| Decision | Rationale |
|---|---|
| **Polycentric feature framing** for the disclosure (not privacy-warning framing) | Aligns with `project_harmony_polycentric_governance` memory — communities are sovereign, forking is the secession primitive. Matter-of-fact tone, not a warning. |
| **CommunitySettingsPanel as the single disclosure surface** (no nav badges, no first-join modals, no per-message banners) | Discoverable on-demand. Avoids paternalistic privacy notices. Keeps the polycentric framing self-contained. |
| **Forks section always renders** (even for non-fork / no-descendant communities) | Reinforces "every community is forkable" as the default state. The minimal "no forks yet" render is short — single line. |
| **Descendants list lives in the Forks section, not in channel timelines** | Channel timelines stay for messages. Fork-of-this-community history is a settings/lineage concern, not a per-channel one. |
| **Silent forks remain invisible by design** | Silent mode is an escape-hatch. Surfacing silent forks would defeat the mode. Visible vs silent is the user's choice at fork-creation time; the system honors it. |
| **Bake ancestor chain into PreForkSnapshot at fork-time** (vs walk by `forked_from` pointers locally) | Walking locally fails when the joiner isn't a member of an ancestor (can't decrypt that ancestor's `CommunityState`). Baking-in sidesteps the encrypted-state problem. Trade-off: ancestor names freeze at fork-time. Acceptable per polycentric framing — fork lineage is historical record. |
| **16-deep cap on baked chain at fork-time** | Protects against pathological abuse (1000-deep fork chains inflating snapshot size). Cap is at build-time. Overflow rendered as "…and N earlier ancestors". |
| **Tree visualization** (vs flat list) | Matches the genealogy mental model. Single coherent visualization for both backward (ancestors) and forward (descendants) directions. |
| **Forker-name resolution ladder** (active member → cross-community cache → fallback string) | Phase 2 uses only the local-knowable resolution paths. Profile-broadcast resolution (ZEB-281) is deferred — Phase 2 isn't blocked on Sub-D PMB integration. |
| **No `community_membership.rs` or sync-engine changes** | Phase 1's Fork event variant is sufficient. Phase 2 is rendering work over data Phase 1 already collects, plus one wire-format extension. |

## 3. Wire surface extensions

### 3.1 New `ParentLineageEntry` type

Defined in `src-tauri/src/community_invite.rs` (alongside `PreForkSnapshot`):

```rust
/// ZEB-285 Phase 2: one entry in a fork's ancestor chain. Frozen at the
/// time it was added to a fork's lineage; ancestor renames after this
/// do not propagate. Bundled into PreForkSnapshot.parent_lineage and
/// persisted in CommunityState.parent_lineage.
///
/// CBOR keys (2-char, per same-length-keys invariant at this nesting):
/// - `si`: space_id (16 bytes — SpaceId raw)
/// - `nm`: name (UTF-8 string, no length cap beyond what serde_cbor enforces)
/// - `at`: forked_at_wall_ms (Option<u64>; absent for root entries)
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParentLineageEntry {
    #[serde(rename = "si")]
    pub space_id: SpaceId,

    #[serde(rename = "nm")]
    pub name: String,

    /// wall_ms component of the Fork event that created THIS community
    /// from its predecessor in the chain. `None` for the root (top of
    /// the chain — never forked, has no predecessor).
    #[serde(rename = "at", skip_serializing_if = "Option::is_none", default)]
    pub forked_at_wall_ms: Option<u64>,
}
```

The CBOR keys `si` / `nm` / `at` are all 2-char and within their own nesting level (the entries inside a `Vec<ParentLineageEntry>`), so they don't collide with the outer types' keys.

### 3.2 `PreForkSnapshot.parent_lineage` extension

Phase 1's `PreForkSnapshot` has 6 fields. Phase 2 adds a 7th:

```rust
/// ZEB-285 Phase 2: ordered list of ancestors above the immediate parent
/// (root → immediate parent), frozen at fork-time. The immediate parent
/// is encoded separately via the existing `original_community_id` /
/// `original_community_name` fields, NOT duplicated here.
///
/// Length capped at 16 entries at fork-build time (per §3.4 build logic).
/// Phase 1 fork-invites encode without this field; decoded as empty Vec
/// via `default`.
#[serde(rename = "pl", skip_serializing_if = "Vec::is_empty", default)]
pub parent_lineage: Vec<ParentLineageEntry>,
```

CBOR key `"pl"`. The skip-if-empty + default-empty pair preserves byte-for-byte identity with Phase 1 snapshots that have no chain to carry.

### 3.3 `CommunityState` extensions

Phase 1 added `forked_from: Option<SpaceId>`. Phase 2 adds two siblings:

```rust
/// ZEB-285 Phase 2: wall_ms component of the Fork event that created
/// THIS community from its parent. Set at redeem-time from
/// PreForkSnapshot.forked_at (Phase 1's existing Hlc field).
/// `None` for top-level (non-fork) communities. Byte-compatible with
/// Phase 1 blobs (omitted when None).
#[serde(rename = "fa", skip_serializing_if = "Option::is_none", default)]
pub forked_at_wall_ms: Option<u64>,

/// ZEB-285 Phase 2: ordered list of ancestors above the immediate
/// parent (root → immediate parent). Mirrors PreForkSnapshot.parent_lineage
/// — populated at redeem-time. Empty for top-level communities and for
/// Phase 1 forks (which carried no chain). Byte-compatible.
#[serde(rename = "fl", skip_serializing_if = "Vec::is_empty", default)]
pub parent_lineage: Vec<ParentLineageEntry>,
```

CBOR keys `"fa"` and `"fl"`. Both backwards-compatible.

### 3.4 Build logic (in `community_fork.rs::build_fork_snapshot`)

When member of community `B` forks community `B` into a new community `A`:

```text
A.parent_lineage = clone(B.parent_lineage)
    .push(ParentLineageEntry {
        space_id: B.id,
        name: B.name,                       # frozen at fork-time
        forked_at_wall_ms: B.forked_at_wall_ms,  # None if B is a top-level community
    })

# 16-deep cap: drop the OLDEST entries (root-side) if necessary
if A.parent_lineage.len() > 16 {
    let overflow = A.parent_lineage.len() - 16;
    A.parent_lineage.drain(0..overflow);
}
```

Forker's local `CommunityState` already has the data needed:
- `B.parent_lineage` — already populated (or empty for top-level non-forks)
- `B.forked_at_wall_ms` — populated when B was itself redeemed from a fork-invite (or None if B is top-level)
- `B.id`, `B.name` — fundamental CommunityState fields

If `B` is a top-level community (`B.forked_at_wall_ms = None` and `B.parent_lineage = []`), then `A.parent_lineage = [B-entry with forked_at_wall_ms: None]` — a single root entry. As `A` is later forked into `C`, `C.parent_lineage = [B-entry-root, A-entry]`, etc.

### 3.5 Redeem-side wiring (in `redeem_invite_inner`)

When a fork-invite is redeemed, the new community's CommunityState picks up the Phase 2 fields from the snapshot:

```rust
let new_state = CommunityState {
    // Phase 1 fields:
    forked_from: Some(snapshot.original_community_id),
    // ... other Phase 1 fields ...

    // Phase 2 fields:
    forked_at_wall_ms: Some(snapshot.forked_at.wall_ms),
    parent_lineage: snapshot.parent_lineage.clone(),
};
```

Phase 1 fork-invites (no `parent_lineage`) decode into `snapshot.parent_lineage = Vec::new()` — and the resulting CommunityState has `parent_lineage = []`, which is the correct "I don't know my ancestry beyond my immediate parent" state for legacy forks.

### 3.6 Depth-cap rationale

The 16-deep cap protects against:
- **Snapshot size inflation**: each `ParentLineageEntry` is roughly 32 bytes (SpaceId) + 8 bytes (wall_ms) + ~32-128 bytes (name) ≈ 100-200 bytes serialized. A 1000-deep chain would add ~100-200 KB to every fork-invite. Hard cap at 16 keeps the lineage overhead under ~3 KB.
- **Render performance**: a 1000-row tree visualization is hostile UX. 16 is "many but tractable".
- **Abuse vectors**: an adversarial forker could chain-fork to inflate other forks' lineage payloads. The cap nullifies this.

The cap is applied at **build-time** (when the new fork's `parent_lineage` is constructed). Decode does NOT enforce the cap — a fork-invite carrying > 16 entries decodes successfully but render-time truncates the displayed list. This preserves backwards-compatibility with any future protocol revision that might lift the cap.

## 4. IPC surface

### 4.1 `list_community_forks(communityId)` (new)

```rust
#[tauri::command]
async fn list_community_forks(
    community_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<ForkDescendantDto>, String> { ... }
```

**Behavior**:
- Resolve `community_id` to the local CommunityState via the existing engine registry.
- Walk the membership event log filtering for `MembershipEventKind::Fork { fork_space_id }`.
- For each Fork event, build a `ForkDescendantDto`:
  - `fork_space_id`: `fork_space_id` from the event (hex-encoded)
  - `forker_addr`: signer's OwnerAddr (hex-encoded)
  - `forker_display_name`: resolved via `member_info_for(community_id, forker_addr)` if forker is currently `Joined`, else `None`
  - `forked_at_wall_ms`: `event.at.wall_ms`
  - `locally_known`: `true` if a community with SpaceId = `fork_space_id` exists in local NavService / OwnerState
- Return sorted ascending by `forked_at_wall_ms`, with stable secondary sort by `forker_addr` for HLC-tie cases.

**Authorization**: caller must be `Joined` in `community_id` (matches the existing `list_community_members` gate — non-members shouldn't enumerate forks of communities they aren't in).

**Error cases**:
- Caller not Joined → `Err("not a member")`
- Community not found → `Err("community not found")`

### 4.2 `get_community_lineage(communityId)` (new)

```rust
#[tauri::command]
async fn get_community_lineage(
    community_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<CommunityLineageDto, String> { ... }
```

**Behavior**:
- Resolve `community_id` to the local CommunityState.
- Return a `CommunityLineageDto` carrying ONLY the lineage-relevant fields. This avoids leaking the full CommunityState shape across the IPC boundary.

**Authorization**: caller must be `Joined` in `community_id`.

### 4.3 `ForkDescendantDto`

```rust
#[derive(serde::Serialize)]
struct ForkDescendantDto {
    fork_space_id: String,           // hex
    forker_addr: String,             // hex
    forker_display_name: Option<String>,
    forked_at_wall_ms: u64,
    locally_known: bool,
}
```

The frontend uses `locally_known` to gate clickability and to choose between "{name}" and "0x{hex8}…" rendering for the fork's display.

### 4.4 `CommunityLineageDto`

```rust
#[derive(serde::Serialize)]
struct CommunityLineageDto {
    /// Phase 1 field — immediate parent SpaceId (hex), or None for top-level
    forked_from: Option<String>,
    /// Phase 2 — wall_ms of THIS community's Fork event, None for top-level
    forked_at_wall_ms: Option<u64>,
    /// Phase 2 — root → immediate-parent ancestor chain (without immediate parent — that's `forked_from`)
    parent_lineage: Vec<ParentLineageDto>,
    /// Convenience: this community's own name + id, so the frontend can
    /// render "you are here" without a second IPC.
    self_space_id: String,
    self_name: String,
}

#[derive(serde::Serialize)]
struct ParentLineageDto {
    space_id: String,                // hex
    name: String,
    forked_at_wall_ms: Option<u64>,
}
```

**Note on naming**: the IPC DTO carries `parent_lineage` as `ParentLineageDto[]`, not `ParentLineageEntry`. The `Dto` suffix is used at IPC boundaries (hex-encoded SpaceId, `String` instead of `SpaceId`); the bare `ParentLineageEntry` is the internal Rust type.

## 5. UI surface

### 5.1 `CommunitySettingsPanel.svelte` — Forks section restructure

Phase 1's "Lineage" section is renamed to "Forks" and restructured. Always renders for every community (no `{#if forked_from}` gate).

```text
┌─ CommunitySettingsPanel ─────────────────────────────┐
│ ...                                                  │
│                                                      │
│ Forks                                                │
│ ─────                                                │
│                                                      │
│ Any member of a community can fork it at any time,   │
│ creating a new community with the snapshot of        │
│ history they had access to. The fork is independent  │
│ — it has its own membership, channels, and admin.    │
│ Forks are how communities preserve continuity if     │
│ members want to take their conversation elsewhere.   │
│                                                      │
│ <ForkLineageTree />                                  │
│                                                      │
│ [ Fork this community ]                              │
│                                                      │
│ ...                                                  │
└──────────────────────────────────────────────────────┘
```

Phase 1's separate "Fork this community" button (whatever its current placement) moves into this section for cohesion.

### 5.2 `ForkLineageTree.svelte` (new component)

**Inputs** (Svelte props):
- `lineage: CommunityLineageDto` — the result of `get_community_lineage` IPC
- `descendants: ForkDescendantDto[]` — the result of `list_community_forks` IPC

**Outputs** (events):
- `navigate-to-community: (spaceId: string) => void` — fired when the user clicks a clickable row

**Tree rendering shape**:

Three contiguous regions in a single `<ul>` per accessibility:

```text
[ancestor rows, from oldest to immediate parent]
[self row, highlighted with aria-current="page"]
[descendant rows, ascending by forked_at_wall_ms]
```

**Ancestor row template**:

```text
{depth-indent}↳ {entry.name} {forked-at if Some}
```

Where:
- `{depth-indent}` is CSS `padding-left: calc(var(--row-depth) * 1.5rem)`, NOT nested `<ul>`
- `{entry.name}` clickable if the entry's `space_id` is in the local NavService cache; otherwise non-clickable + `cursor: default` + tooltip "You're not a member of this community."
- `{forked-at}` rendered as ISO date `YYYY-MM-DD` if `forked_at_wall_ms` is `Some`; omitted (along with leading whitespace) if `None`

**Truncation marker** (if `parent_lineage.length > 16` — defensive against future-protocol-revision data):

```text
…and {N - 16} earlier ancestors
```

Rendered at the top of the ancestor region, non-clickable, muted.

**"You are here" row template**:

```text
{self-depth-indent}You are here ← {self_name}
```

Where:
- `{self-depth-indent}` = `parent_lineage.length` (deepest yet — one more indent than the deepest ancestor)
- The row has `aria-current="page"` and a subtle highlighted background
- Non-clickable (you're already here)
- The arrow direction is intentional: "the marker for current position points at the community name", reading naturally LTR

**Descendant row template**:

```text
{descendant-depth-indent}↳ {display} {forked-at} {by-clause}
```

Where:
- `{descendant-depth-indent}` = `parent_lineage.length + 1` (one deeper than self)
- `{display}`:
  - If `locally_known`: descendant community's display name from local NavService
  - Otherwise: `0x{first 8 hex chars of fork_space_id}…`
- `{forked-at}`: ISO date from `forked_at_wall_ms`
- `{by-clause}`:
  - If `forker_display_name` is `Some(name)`: `by {name}`
  - Otherwise: `by an unknown member`
- Clickable if `locally_known: true`. Non-clickable otherwise with tooltip "You're not a member of this fork."

**Empty state** (no ancestors AND no descendants):

```text
You are here ← {self_name}

(no forks yet)
```

The "(no forks yet)" line is muted, non-interactive, sub-text.

**Accessibility**:
- Outermost `<ul role="tree">`
- Each row is an `<li role="treeitem">`
- Clickable rows are `<button>` elements inside the `<li>` for keyboard navigation
- `aria-current="page"` on the self row
- `aria-level` attribute on each `<li>` matching its depth (1-indexed; root = 1)
- Tooltips use `title` attribute + `aria-describedby`

### 5.3 Explainer copy (final wording)

```text
Any member of a community can fork it at any time, creating a new
community with the snapshot of history they had access to. The fork
is independent — it has its own membership, channels, and admin.
Forks are how communities preserve continuity if members want to take
their conversation elsewhere.
```

Rendered as a single `<p>` element above the tree. Plain prose. No bullets, no emphasis on specific words.

### 5.4 Click behavior

| Row type | Locally known | Behavior |
|---|---|---|
| Ancestor | yes | Click → fire `navigate-to-community(space_id)` event; parent component invokes NavService.navigateTo |
| Ancestor | no | Non-clickable; tooltip "You're not a member of this community." |
| Self | (always self) | Non-clickable |
| Descendant | yes | Click → fire `navigate-to-community(fork_space_id)` event |
| Descendant | no | Non-clickable; tooltip "You're not a member of this fork." |
| Truncation marker | (always) | Non-clickable; no tooltip |

The `navigate-to-community` event uses NavService's existing community-navigation primitives — Phase 2 does NOT introduce a new navigation mechanism; the tree is just a click-router into Phase 1's navigation.

### 5.5 Sort + date format

- **Ancestor sort**: array order, which is root → immediate-parent (per the build logic in §3.4).
- **Descendant sort**: ascending by `forked_at_wall_ms` (oldest first), with stable secondary sort by `forker_addr` for HLC-tie cases. This matches the IPC's own return order.
- **Date format**: `YYYY-MM-DD` only — coarse-grained. The exact time within the day doesn't matter for lineage display. Computed via `new Date(wall_ms).toISOString().slice(0, 10)` in the renderer.

## 6. Backwards compatibility

### 6.1 Wire-format byte-compat

Phase 1 `CommunityState` blobs decode byte-identically under the Phase 2 type definition — both new fields use `#[serde(skip_serializing_if, default)]` so absent fields are correctly recovered as `None` / `Vec::new()`.

Phase 1 `PreForkSnapshot` blobs (carried in legacy fork-invites) decode byte-identically — the new `parent_lineage` field is skip-if-empty + default-empty.

A Phase 2 client encoding a `CommunityState` with empty/None new fields produces byte-identical output to a Phase 1 client encoding the same logical state. Pinned via wire-format fixtures (§7.1).

### 6.2 Phase 1 forks degrade gracefully

A community redeemed via a Phase 1 fork-invite has:
- `forked_from: Some(parent_id)` (Phase 1, intact)
- `forked_at_wall_ms: None` (Phase 2 redeem path doesn't run for Phase 1 invites — but even when Phase 2 redeem runs against a Phase 1 invite, `snapshot.forked_at` already exists in Phase 1, so we DO set this from Phase 1 invite snapshots; this gives mixed-version Phase 1 forks a forked_at)
- `parent_lineage: []` (Phase 1 invite carried no chain)

The Lineage tree for a Phase 1 fork renders:

```text
↳ {immediate_parent_name}            (no forked-at shown — None)
  You are here ← {self_name}
  [descendants if any]
```

This is "the immediate parent is known but the chain back beyond that isn't." Correct degradation.

### 6.3 Mixed-version interop

- **Phase 2 client + Phase 1 community state on disk**: Phase 2 client decodes Phase 1 state; new fields default. UI renders the single-hop ancestor. No data loss.
- **Phase 2 client + Phase 1 fork-invite**: same — decode succeeds, single-hop lineage in resulting fork.
- **Phase 1 client + Phase 2 fork-invite**: Phase 1 client decodes the snapshot; new `pl` key is ignored by Phase 1 serde (unknown field, default behavior depends on serde_cbor settings, but `harmony-client` uses `#[serde(deny_unknown_fields)]` only on auth-bearing types — `PreForkSnapshot` does NOT deny unknown fields, so Phase 1 decode silently ignores `parent_lineage`). Phase 1 stores `forked_from` only. Phase 1 client's tree (which renders only single-hop) shows what it always showed.
- **Phase 1 client + Phase 2 community state**: only happens if a Phase 1 client receives a state-root publish from a Phase 2 client. Same as the previous case — Phase 1 decode silently drops `fa` / `fl`. Phase 1 functionality unchanged.

No version negotiation is required. Phase 2 fields are additive and silently-droppable.

### 6.4 Invariant verifier note

`CommunityState::validate_invariants` must NOT reject Phase 1-shaped states (no `parent_lineage`, no `forked_at_wall_ms`). The Phase 1 invariants stand. Phase 2 adds NO new invariants — `parent_lineage` and `forked_at_wall_ms` are pure-render data with no consistency requirements relative to other CommunityState fields.

## 7. Testing strategy

### 7.1 Wire-format pinning

Extend `src-tauri/tests/wire_format_zeb285_fixtures.rs` with:

1. `parent_lineage_entry_canonical_cbor` — pin a `ParentLineageEntry` to byte-exact CBOR with `{si: <16-byte SpaceId>, nm: "Cool Community", at: 1715811234567}` and its root-form variant (no `at`).
2. `pre_fork_snapshot_with_parent_lineage_canonical_cbor` — pin a 2-entry chain inside a snapshot.
3. `community_state_with_parent_lineage_canonical_cbor` — pin a Phase 2 community state with non-empty lineage.
4. `phase1_community_state_decodes_under_phase2_types` — load a Phase 1 fixture blob, decode under the Phase 2 `CommunityState` type, assert `parent_lineage: []` and `forked_at_wall_ms: None`.
5. `phase2_community_state_round_trip_byte_identical_when_empty` — assert that a Phase 2 CommunityState with all-default new fields encodes to the EXACT same bytes as a Phase 1 CommunityState fixture.

### 7.2 Multi-hop integration

Extend `src-tauri/tests/community_fork_integration.rs` with:

1. `three_deep_fork_chain_preserves_lineage_through_snapshot` — drive `fork_community` twice across three generations (C → B → A). After A's redeem completes, assert A's local `CommunityState.parent_lineage` is `[C-entry, B-entry]` with correct names + wall_ms.
2. `lineage_depth_cap_truncates_root_side` — synthesize a 20-deep `parent_lineage` via direct CommunityState construction (bypassing `fork_community`), then drive `fork_community` once more. Assert the result is exactly 16 entries (oldest 4 dropped from root side, newest 16 retained).
3. `phase1_snapshot_redeems_with_default_lineage` — redeem a synthetic Phase 1-shaped fork-invite (no `parent_lineage` field) and assert the resulting CommunityState has `parent_lineage: []` and `forked_at_wall_ms: Some(snapshot.forked_at.wall_ms)` (filled from Phase 1's existing field).

### 7.3 Forker resolution + IPC

New unit tests (in `lib.rs` or a sibling test file):

1. `list_community_forks_resolves_active_member_name` — community with one Fork event from an active member; verify DTO's `forker_display_name: Some(name)`.
2. `list_community_forks_falls_back_when_forker_kicked` — same setup, then admin kicks the forker; verify the Fork event still surfaces with `forker_display_name: None`.
3. `list_community_forks_marks_locally_unknown_descendants` — Fork event with `fork_space_id` not in local NavService; verify DTO's `locally_known: false`.
4. `list_community_forks_rejects_non_member_caller` — caller not Joined in the community; verify `Err("not a member")`.
5. `list_community_forks_sorts_chronologically` — multiple Fork events with varied HLCs; verify ascending wall_ms ordering with stable forker_addr secondary sort.
6. `get_community_lineage_returns_phase1_state_with_default_phase2_fields` — community whose CommunityState is Phase 1-shape; verify `parent_lineage: []` and `forked_at_wall_ms: None`.
7. `get_community_lineage_returns_phase2_chain` — community whose state has a 3-entry chain; verify the DTO carries all 3 entries with correct ordering.
8. `get_community_lineage_rejects_non_member_caller` — caller not Joined; verify error.

### 7.4 Frontend ForkLineageTree variants

`src/lib/components/__tests__/ForkLineageTree.test.ts` (new). 8 variants:

1. `renders_non_fork_no_descendants_minimally` — empty `parent_lineage` and empty `descendants[]`; verify "(no forks yet)" line + "You are here" row only.
2. `renders_ancestors_only_for_leaf_fork` — `parent_lineage = [C, B]`, `descendants = []`; verify both ancestor rows + self row, no descendants region.
3. `renders_descendants_only_for_root_with_forks` — empty `parent_lineage`, 2 descendants; verify self row + 2 descendant rows, no ancestors region.
4. `renders_full_tree_three_deep_two_descendants` — full lineage and descendants; verify correct depth indents (ancestor at 0, deeper ancestor at 1, self at 2, descendants at 3).
5. `renders_truncation_marker_for_overlong_lineage` — 18-entry `parent_lineage`; verify "…and 2 earlier ancestors" rendered at top and only 16 ancestor rows visible.
6. `click_navigates_to_locally_known_community` — fire click on a `locally_known: true` row; assert `navigate-to-community` event fired with correct SpaceId.
7. `non_clickable_for_unknown_community` — verify ancestor rows with `space_id` not in NavService cache are not clickable (cursor:default, no event on click attempt, tooltip present).
8. `aria_current_page_on_self_row` — assert the self row has the `aria-current="page"` attribute.

### 7.5 CommunitySettingsPanel section refactor

Augment `src/lib/components/__tests__/CommunitySettingsPanel.test.ts` (create if not yet present):

1. `forks_section_always_renders_for_non_fork_community` — render with Phase 1-shape lineage; verify the "Forks" heading + explainer paragraph + ForkLineageTree component all present.
2. `forks_section_renders_explainer_text_present` — verify the polycentric-framing explainer paragraph text is in the rendered HTML (substring match on the final wording).
3. `fork_this_community_button_inside_forks_section` — verify the existing "Fork this community" button is rendered as a descendant of the Forks section.

### 7.6 Smoke test (manual, per PR body)

Per `feedback_engineer_for_real_scale` memory + Phase 1's smoke pattern. Two-engine local run:

1. Engine A creates community C.
2. Engine A invites Engine B to C.
3. Engine A (still in C) forks C into B.
4. Engine A (now also in B) forks B into A.
5. On Engine A: opens A's settings; verify Forks tree shows `C ← B ← You are here`, no descendants under A.
6. On Engine A: opens B's settings; verify Forks tree shows `C ← You are here ← A (forked by Engine A)`.
7. On Engine A: opens C's settings; verify Forks tree shows `You are here ← B (forked by Engine A)`. A is NOT in C's descendants because Engine B doesn't see A's Fork event (A's Fork event landed in B's log, which is what's visible from C's perspective).
8. On Engine B: opens C's settings; verify Forks tree shows `You are here ← B (forked by Engine A)`. Same as Engine A's view of C — descendants list is bound to the community's own log, not the viewer's identity.

This validates the privacy boundary and the cross-engine tree consistency.

### 7.7 CI gates

All five must be green before merge:
- `cd src-tauri && cargo fmt --all -- --check`
- `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- `npx tsc --noEmit` (from repo root)
- `npx vitest run` (from repo root)

**Test count target**: Rust 1369 → ~1379-1382 (+wire fixtures + multi-hop integration + IPC unit tests). Frontend 1740 → ~1750-1755 (+ForkLineageTree variants + panel integration).

## 8. Acceptance criteria

1. Forks section in `CommunitySettingsPanel.svelte` always renders (every community, fork or not, descendant-bearing or not).
2. Polycentric-framing explainer paragraph present in all variants. Exact wording per §5.3.
3. Ancestor chain rendered correctly for forks created by Phase 2+ clients (up to 16 deep; truncation marker for overflow).
4. Phase 1 forks (no `parent_lineage` data) degrade gracefully to single-hop display.
5. Descendants list shows all visible Fork events chronologically (oldest first). Silent forks remain absent — no UI surface lists them anywhere.
6. Forker display name resolves via the documented ladder (active member → cross-community cache → "an unknown member"). Profile-broadcast resolution is explicitly out of scope.
7. Click navigation works to locally-known communities (ancestors or descendants). Non-clickable rows have tooltips explaining unavailability.
8. Backwards-compat: existing Phase 1 wire fixtures still decode byte-identically. Pinned via tests in §7.1.
9. All 5 CI gates green.
10. New Linear ticket filed for Phase 2 BEFORE the PR is opened, referenced in PR body via markdown link.
11. PR body includes a smoke-test checklist per §7.6.

## 9. Out of scope

Explicit deferral. File as Phase 3+ follow-up tickets if/when needed.

- **Original-community channel-timeline rendering of Fork events as system messages** — Phase 2 picked settings-only surfacing. If demand emerges later, file a follow-up.
- **Disclosure surfaced outside CommunitySettingsPanel** — no nav badges, no tooltips, no first-join modal. Polycentric framing assumes informed users discovering forks on-demand.
- **Library-directory fork-inheritance affordance** (Phase 1 deferral #3) — when a community is forked, the fork doesn't automatically appear in libraries the parent was listed in. Future Phase 3 work crossing into ZEB-218 Sub-D plumbing.
- **Verify-on-redeem of snapshot signatures** (Phase 1 deferral #5) — Phase 1's lazy verification stays. Hardening to eager verify-at-redeem is a separate security-shaped phase.
- **"Recently forked" cross-cutting surface across multiple communities** (Phase 1 deferral #7) — a unified "communities-you-belong-to-with-recent-fork-activity" view. Out of scope for Phase 2.
- **Pre-fork message author display via profile-broadcast resolution** (Phase 1 deferral #8) — Phase 2 resolves forker NAMES for Fork events but does NOT touch pre-fork message authorship display. The latter depends on ZEB-281 PMB integration and is a separate concern.
- **Snapshots > 5000 via content-addressed delivery** (Phase 1 deferral #6) — Phase 2 inherits Phase 1's snapshot policy unchanged.
- **Retry surface for failed announce/leave** (Phase 1 deferral #9) — current log-and-continue behavior stays.
- **`forked_from` persistence race fix** (Phase 1 deferral #10) — separate backend hardening; no UI work.
- **Fork-disclaimer i18n / multi-language explainer text** — English-only; i18n is a project-wide concern.
- **"Manage fork" affordance** — renaming a fork, changing its admin, etc., are handled by existing per-community settings, NOT by anything fork-specific. Phase 2 does not add fork-specific management UI.
- **Block/mute incoming fork notifications** — out of scope; we surface only in settings, no notifications exist.
- **CRDT changes to `MembershipEventKind::Fork`** — Phase 1's variant is sufficient.
- **Sync-engine / `community_state_sync.rs` changes** — Phase 2 is rendering work; no sync logic touches.

## 10. References

- Phase 1 spec: [`docs/specs/2026-05-14-zeb-285-phase1-community-forking-design.md`](2026-05-14-zeb-285-phase1-community-forking-design.md) (commit `e318823`)
- Phase 1 plan: [`docs/plans/2026-05-14-zeb-285-phase1-community-forking-plan.md`](../plans/2026-05-14-zeb-285-phase1-community-forking-plan.md) (commit `42a8124`)
- Phase 1 PR: [#122](https://github.com/zeblithic/harmony-client/pull/122) (merged 2026-05-15 as `bec4e03`)
- Memory: `project_harmony_polycentric_governance.md` — communities-only governance, communities are sovereign
- Memory: `feedback_design_for_eventual_state.md` — design for the eventual UX (forks-as-feature, not as edge case)
- Memory: `feedback_engineer_for_real_scale.md` — bake-in vs walk-locally decision informed by encrypted-state cross-membership reality
- Memory: `feedback_severe_action_confirmation.md` — Phase 1 set the tier convention; Phase 2 adds no new confirmations
- Source: [`src-tauri/src/community_invite.rs`](../../src-tauri/src/community_invite.rs) (Phase 1 `PreForkSnapshot` host)
- Source: [`src-tauri/src/community_state_crdt.rs`](../../src-tauri/src/community_state_crdt.rs) (`CommunityState` host)
- Source: [`src-tauri/src/community_fork.rs`](../../src-tauri/src/community_fork.rs) (Phase 1 `build_fork_snapshot`)
- Source: [`src/lib/components/CommunitySettingsPanel.svelte`](../../src/lib/components/CommunitySettingsPanel.svelte) (Phase 1 Lineage block — being restructured)
- Source: [`src/lib/components/ForkConfirmDialog.svelte`](../../src/lib/components/ForkConfirmDialog.svelte) (Phase 1 confirmation dialog — unchanged in Phase 2)
