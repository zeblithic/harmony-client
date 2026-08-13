# ZEB-907 + ZEB-921: Display-name surfaces — self-row resolver parity + published-card observable (design)

One PR, two legs of the same confusion: three distinct "my name" notions
(device label, published owner-card name, friend nickname) leaking into the
wrong surfaces. All receipts verified on main @ `1cbfc93d` (2026-08-12).

## 1. Problem

### 1a. ZEB-907 — self row renders hex in the Manage-community members list

`CommunitySettingsPanel.svelte` renders member rows with a bare
roster-or-hex fallback and NO resolver rungs:

* `:560` avatar — `{(m.displayName ?? m.address).slice(0, 1).toUpperCase()}`
* `:562` name — `{m.displayName ?? m.address.slice(0, 8)}{… ' (you)'}`

Roster `displayName`s are filled from **received** profile cards; you never
receive your own, so the self entry's `m.displayName` is null and the self
row falls through to hex. The sibling surface (`CommunityMembersPanel` →
`MemberRow.svelte:127-130`) resolves through the 4-rung ladder
*nickname → live card → roster → hex* — and shows self correctly, because
the local node's ZEB-884 queryable answers own-card query-on-subscribe, so
`resolveCard` covers self.

The mount site `CommunityView.svelte:573` already has `resolveCard` /
`resolveNickname` in scope — it passes them to the members panel a few lines
up (`:560-561`). The fix is plumbing, not new infrastructure.

### 1b. ZEB-921 — `ownerDisplayName` is the device label, hardcoded

`get_owner_state_inner` sets `let display_name = "this device".to_string();`
(`owner_commands.rs:738`) and `build_owner_state_view` copies it into
`owner_display_name` (`:592`). It is never updated by `republish_owner_card`.
The name peers actually resolve — the published owner-card `display_name` —
has **no owner-state observable**; a headless agent's only self-check is
`subscribe_member_card` on its own owner id (the ZEB-898 field confusion that
tripped two fleet agents).

The publisher caches the last signed card as wire bytes:
`ProfileCardPublisher.latest` is `Option<CardWire>` where
`type CardWire = (String, Vec<u8>)` — (topic, signed canonical CBOR of
`ProfileCardBroadcast`, whose `dn` field is the display name)
(`profile_card_broadcast.rs:523/528/628`). `NodeState` holds the publisher
(`lib.rs:1157`). Both the 600s refresh and the ZEB-884 queryable serve from
this same handle — it IS the serving surface.

## 2. Design

### 2a. ZEB-921 Rust — surface `cardDisplayName` (ticket Option 1)

* **Decode helper** in `profile_card_broadcast.rs`:
  `pub fn decode_card_display_name(bytes: &[u8]) -> Option<String>` —
  `ciborium::from_reader::<ProfileCardBroadcast, _>(bytes)` (mirrors the
  subscriber ingest decode, `event_loop.rs:7781-7787`) → `.display_name`.
  **No signature verification**: the cache is written only by our own
  publish path with bytes we just signed (`publish_now`, `:603-606`);
  verification would add enrollment-cert plumbing for zero new guarantee.
  Decode failure → `None` (defensive only).
* **View field**: `OwnerStateView` gains
  `#[serde(default)] pub card_display_name: Option<String>` (after
  `owner_display_name`, `owner_state.rs:18`; struct-level
  `rename_all = "camelCase"` puts `cardDisplayName` on the wire).
  `ownerDisplayName` is untouched.
* **Threading**: in `get_owner_state_inner`, clone
  `g.profile_card_publisher` inside the existing up-front NodeState-lock
  block (`owner_commands.rs:669-679`); after it, snapshot
  `latest_handle().lock().await` (async context) → decode → the
  `Option<String>` is moved into `build_owner_state_view` as a new
  parameter, at BOTH call sites (resident path `:777` and the
  `run_blocking` tail — compute before the blocking closure, move in).
* **Semantics**: `Some(name)` ⇔ this run holds a cached card being served
  to peers (refresh + queryable read the same handle). `null` ⇔ nothing
  served this run — node down, never published, or the boot window before
  the first publish. Both GUI boot and serve boot (ZEB-882,
  `lib.rs:30812-30826`) publish at start, so the `null` window is
  boot-transient on named nodes; a bare anonymous serve stays honestly
  `null`.
* Reaches headless agents for free: `get_owner_state_impl` is the ZEB-445
  shared IPC/RPC seam, so `api get_owner_state` surfaces the field.

### 2b. ZEB-921 TS — type only

`owner-service.ts` `OwnerStateView` gains
`cardDisplayName?: string | null` (optional — stale-backend omission
tolerated, same convention as the quorum fields). No GUI consumer change:
the ticket's ask is the observable, and DevicesPanel's device labels are a
different notion by design.

### 2c. ZEB-907 frontend — resolver parity via the shared ladder

* `CommunitySettingsPanel` gains optional props
  `resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined` and
  `resolveNickname?: (ownerIdHex: string) => string | undefined` — the
  exact contracts `CommunityMembersPanel` declares (`:32-35`).
* Each row computes ONE label:
  `resolveMentionLabel(m.address, resolveNickname, resolveCard, () => m.displayName ?? undefined)`
  (`mention-render.ts:52` — documented as mirroring MemberRow's 4-rung
  ladder; empty/whitespace handled by the shared `nonEmpty`). Name (`:562`)
  = label + existing `' (you)'` suffix; avatar (`:560`) = label first char
  uppercased (the hex fallback rung yields the same character as today's
  `m.address` first char, so no-resolver rendering is unchanged).
* `CommunityView` passes `{resolveCard} {resolveNickname}` at the `:573`
  mount.
* Effect: the self row resolves exactly like the sibling panel (the bug);
  other members' rows upgrade from roster-only to live-card-first — the
  same freshest-name-wins semantic every other member surface already has.

## 3. Declined alternatives

* **Reuse `MemberRow` in the settings list** (ticket's "ideally"):
  MemberRow carries presence-dot/kebab/card-popover machinery; the settings
  row carries RoleBadge/Set-role/Kick/pending-badge. That is a layout
  refactor of a working surface to fix a label bug. The anti-drift goal is
  met at the label level: both surfaces now call the SAME ladder function.
* **Rename `ownerDisplayName` → `deviceLabel`** (ticket Option 2): breaking
  for GUI consumers (DevicesPanel reads it at `:900/:903/:1398`); the
  ticket itself defers it. Residual below.
* **Verify the cached card before decoding**: self-produced post-sign
  bytes; adds plumbing, no guarantee.
* **Read the persisted profile name instead of the publisher cache**:
  reports *intent*, not what peers can query; the cache handle is the
  serving surface.

## 4. Tests

* Rust (`profile_card_broadcast.rs`): `decode_card_display_name` round-trip
  on `sign_card`-produced bytes; garbage bytes → `None`.
* Rust (`owner_commands.rs`): `build_owner_state_view` threads
  `Some("name")` / `None` into the view field (construction-level, like the
  existing view tests).
* Frontend (`CommunitySettingsPanel.test.ts`): self row with a
  `resolveCard` hit renders the card name + `(you)` (not hex); nickname
  rung beats card rung (ladder-order pin); no-resolver render unchanged
  (existing tests double as the regression net); other-member row prefers
  live card over stale roster name.
* Existing suites: `owner-service.test.ts`, `DevicesPanel.test.ts`, and the
  Rust owner-state families must stay green untouched (additive field with
  `#[serde(default)]`).

## 5. Residuals

* ZEB-907's secondary observation (manage/moderation overlay reachable by
  non-admin members) — explicitly out of scope; recorded in the ticket.
* `ownerDisplayName` rename to `deviceLabel` (ticket Option 3's second
  half) — deferred; deserves its own GUI-consumer pass.
* `cardDisplayName` is per-run by design. If agents ever need the last
  published name across a restart before the boot publish lands, a read of
  the persisted card store could close that window; no current need.
