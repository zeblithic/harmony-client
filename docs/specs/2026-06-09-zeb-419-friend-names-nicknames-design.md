# Friend Owner-Names + Local Nicknames — Design (ZEB-419)

**Goal:** Show each friend / pending-request by their *currently-configured* owner name (and avatar) in the Friends panel, and let the user attach a purely-local, per-friend **nickname** that never leaves the device — while keeping the verifiable owner_id one drill-down away.

**Architecture:** Two loosely-coupled halves. (1) **Owner names** reuse the existing ZEB-341 owner-card pub/sub pipeline verbatim — `FriendsPanel` runs its *own* `MemberCardService` instance over the friend+pending owner_id set and reads resolved names/avatars reactively. (2) **Nicknames** are a new backend **local-only** store (`friend_nicknames`), HLC-stamped and kept *outside* the published owner-state CRDT, surfaced as `FriendDto.nickname` and edited via a new `set_friend_nickname` IPC. The frontend composes a display-label ladder over both.

**Tech stack:** Rust/Tauri backend (`src-tauri/src/lib.rs` + new module), Svelte 5 runes frontend, `member-card-service.ts` (existing), `ProfilePopover.svelte` owner-card mode (existing), vitest + `cargo nextest`.

---

## 1. Background — current state

`FriendsPanel.svelte` renders both lists as `{display ?? shortId(ownerIdHex)}` with a muted short-hex line (`.friend-addr`, full hex in `title=`). `FriendDto.display` is a **frozen** hint captured at link time (`display = accepted.display.or(payload.display_hint)`); it is `null` for add-by-key friends (`self_display = None` — the node's own card name is not threaded into the handshake) and never updates when the peer renames. There is no nickname concept anywhere.

What already exists and is reused wholesale:

| Capability | Symbol(s) | Reuse |
|---|---|---|
| Live owner-card resolution by owner_id | `subscribe_member_card` / `unsubscribe_member_card` / `get_cached_member_card` IPCs, `member-card-received` event, `member-card-service.ts` (`MemberCardService`) | Name + avatar source |
| Per-row card consumption pattern | `MemberRow.svelte` (`resolveCard()` `$derived` ladder) | Template for the friend row |
| Drill-down identity surface | `ProfilePopover.svelte` `mode='owner-card'` (full hex + copy + avatar + status + "View full profile") | The ⓘ drill-down, verbatim |
| Local, non-published settings store | `pkarr_settings.rs` (`load_or_default`, backs `friend_auto_accept_known` / `identity_discoverable`) | Pattern for the nickname store |
| Friend projection | `list_friends_inner(state) -> Vec<FriendDto>` (pure over `&OwnerState`) | Extended to join nicknames |

## 2. Decisions (resolved during brainstorming)

1. **Owner-name source → live member-card resolution** (not the frozen hint). The frozen `display` stays as an instant-paint fallback while the card resolves.
2. **Nickname storage → backend local-only store**, HLC-stamped, built to be adopted later by the ZEB-417 Fleet-Sync substrate as a replicated dataset (`write(dataset, op)`), exactly like ZEB-361 Notes. **No cross-device sync in this ticket.**
3. **Identity display → name + persistent short-hex.** Each row keeps a muted short-hex line always visible; full hex lives in the drill-down.
4. **Avatars → included** (small, per row) — they ride the same card resolution for free and reinforce "recognize your real friend".
5. **Nickname scope → active friends only.** Pending requests show the live owner-card name + hex for recognition; you nickname a peer after accepting.

## 3. Display-label ladder

Evaluated per row in a `$derived`, both lists:

```
nickname  ►  live member-card displayName  ►  frozen FriendDto.display  ►  shortId(ownerId)
```

Pending requests use the same ladder minus the nickname rung (out of scope this ticket).

Row layout (both lists):

```
[avatar28]  <name label>                 [row actions]
            <muted short-hex>   ⓘ
```

- `<name label>` is a button → opens the drill-down popover.
- Active-friend rows add a "Set nickname" affordance (see §6).
- The short-hex line is always present (anti-spoof: the verifiable id is never fully hidden by a name).

## 4. Backend — nickname local store

### 4.1 New module `src-tauri/src/friend_nicknames.rs`

```rust
/// Local-only, per-owner friend nicknames. NEVER published or synced in this
/// phase. HLC-stamped so the ZEB-417 fleet-sync substrate can later adopt the
/// whole map as a replicated dataset with deterministic LWW merge.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FriendNicknames {
    /// owner_id hex (lowercase, 32 chars) -> entry.
    pub entries: std::collections::BTreeMap<String, NicknameEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NicknameEntry {
    pub nickname: String,
    /// HLC at last write (substrate-ready LWW key).
    pub updated_hlc: crate::owner_state_types::Hlc,
}
```

- `load_or_default(path) -> FriendNicknames` and `save(path, &self)` mirror `pkarr_settings.rs` (atomic write via temp + rename; missing file → default empty).
- `set(&mut self, owner_id_hex, nickname: Option<String>, hlc)`: `Some(non-empty)` upserts with the new HLC; `None` or empty/whitespace removes the entry. Owner_id is lowercased on the way in.
- `get(&self, owner_id_hex) -> Option<&str>`.
- File location: a dedicated `friend_nicknames.json` alongside the existing per-owner settings (same dir resolution `pkarr_settings` uses).

### 4.2 IPC `set_friend_nickname`

```rust
#[tauri::command]
async fn set_friend_nickname(
    owner_id_hex: String,
    nickname: Option<String>,        // None / empty / whitespace => clear
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String>
```

- Validates `owner_id_hex` via `parse_owner_addr_hex` (reject malformed before any write).
- Reserves a fresh HLC (same `reserve_next_hlc_for_device` path used elsewhere), loads-or-defaults, `set(...)`, saves.
- Emits `friend-list-changed` so the panel re-fetches and re-renders the new label. (Reuses the existing refresh path — no new event type.)
- Plain `#[tauri::command]` (camelCase JS → snake_case Rust per the project convention; **no** `rename_all` — see ZEB-414).

### 4.3 Projection join

`FriendDto` gains `nickname: Option<String>`. `list_friends_inner` takes the nickname map and joins at projection, staying pure + unit-testable:

```rust
pub fn list_friends_inner(
    state: &crate::owner_state_crdt::OwnerState,
    nicknames: &crate::friend_nicknames::FriendNicknames,
) -> Vec<FriendDto> { /* ... nickname: nicknames.get(&owner_id_hex).map(str::to_owned) ... */ }
```

The `list_friends` IPC wrapper loads-or-defaults the nickname store and passes it in.

### 4.4 Privacy invariant (structural)

Nicknames live **only** in `friend_nicknames.json` and `FriendDto` (a frontend projection). They are **never** written into `OwnerState.friend_graph`, any `FriendEntry`, any owner-card payload, or any broadcast/publish path. The guarantee is enforced by *where the bytes live*, not by remembering to strip a field. A regression test asserts a published owner-state serialization containing a friend with a set nickname does **not** contain the nickname bytes (see §8).

## 5. Frontend — owner-name resolution in `FriendsPanel`

### 5.1 Dedicated `MemberCardService` instance

`App.svelte` already runs one `MemberCardService` driven by the **community roster** via `subscribeVisible(rosterIds)`. Because `subscribeVisible(ids)` reconciles to *exactly* the passed set (unsubscribing anything absent), the Friends panel must not share it — it would unsubscribe the roster and vice-versa. So:

- `App.svelte` constructs a **second** instance `friendCardService = new MemberCardService()`, wires `setAdapter(adapter)` (at the same point the roster instance is wired, ~`:1334`) and `setAvatarResolver(avatarResolver)` (~`:1064`), and passes it into `FriendsPanel` as a prop. App does **not** call `subscribeVisible` on it.
- `FriendsPanel` props become `{ service, cardService, onOpenCard }`:
  - `cardService: MemberCardService` — the dedicated instance.
  - `onOpenCard?: (payload: OpenCardPayload, ev: MouseEvent) => void` — opens App's shared owner-card `ProfilePopover` (same payload shape `MemberRow` emits). Omitted in tests.
- `FriendsPanel` owns its reactive version counter:
  - `onMount`: `cardService.onUpdate = () => cardVersion += 1;`
  - A `$effect` calls `cardService.subscribeVisible([...activeIds, ...pendingIds])` whenever the friend/pending lists change (snapshot the ids; reconcile is idempotent).
  - `onDestroy`: `cardService.onUpdate = undefined; void cardService.unsubscribeAll();` — same liveness discipline hardened in ZEB-415 (`destroyed` flag already present; no `$state` mutation after teardown).

### 5.2 Reactive label ladder

```ts
let cardVersion = $state(0);
function labelFor(f: FriendDto): string {
  // touch cardVersion so resolve() re-reads after a poll/event update
  cardVersion;
  return f.nickname
    ?? cardService.resolve(f.ownerIdHex)?.displayName
    ?? f.display
    ?? shortId(f.ownerIdHex);
}
```

Avatar URL: `cardService.resolve(f.ownerIdHex)?.avatarUrl` → `Avatar.svelte` (identicon fallback built in). Pending rows use the same `labelFor` minus the `f.nickname` rung.

### 5.3 `friend-service.ts`

- `FriendDto` interface gains `nickname?: string | null`.
- New method `setNickname(ownerIdHex: string, nickname: string | null): Promise<void>` → `invoke('set_friend_nickname', { ownerIdHex, nickname })`.

## 6. Nickname editing UI (active friends)

Per active-friend row, a compact edit affordance:

- A "✎" / "Set nickname" button toggles a small inline text input seeded with the current nickname (or empty).
- Enter / "Save" → `service.setNickname(ownerIdHex, value.trim() || null)`; empty clears (falls back down the ladder). Esc / blur cancels.
- A per-row in-flight guard `Set<ownerIdHex>` (mirrors the existing `unfriending` / `referrableSaving` guards) disables the control while saving.
- On success the backend emits `friend-list-changed` → existing `refresh()` re-fetches with the new `nickname` field. No optimistic local state needed (keeps a single source of truth).

## 7. Drill-down (identity verification)

The name-label button and the ⓘ both call `onOpenCard(payload, ev)` with an `OpenCardPayload`:

```ts
{ ownerIdHex, displayName: <live card name or fallback>, statusText, avatarUrl }
```

App renders the existing `ProfilePopover` in `owner-card` mode → full owner_id hex (copyable), avatar, status, "View full profile". Crucially the popover shows the peer's **own card displayName**, not the nickname — so a misleading nickname can never fully mask the real identity. This is the anti-spoof surface the spoofing-risk tradeoff relies on.

## 8. Testing strategy

**Backend (`cargo nextest`):**
- `friend_nicknames`: set → get round-trip; clear via `None` / empty / whitespace removes the entry; lowercasing of owner_id; load-or-default on missing file; HLC monotonic on re-set.
- `list_friends_inner` join: friend with a nickname → `FriendDto.nickname == Some`; without → `None`; revoked still filtered.
- **Privacy guard:** serialize a published owner-state (the same path the sync engine publishes) for an owner who has a friend *with* a nickname set, and assert the serialized bytes do **not** contain the nickname string. Locks the structural invariant against future refactors.
- IPC arg-casing: `set_friend_nickname` accepts camelCase `ownerIdHex` (extends the ZEB-414 `ipc_arg_casing` guard).

**Frontend (`vitest`):**
- Label ladder precedence: nickname > card name > frozen display > short-hex (4 cases) for both lists.
- Nickname set persists / clear falls back; in-flight guard disables the control.
- `subscribeVisible` is called with the union of active + pending ids on list change; `unsubscribeAll` on unmount; `onUpdate` re-renders the label (bump `cardVersion`). Liveness: no state mutation after destroy (reuses ZEB-415 `destroyed` pattern).
- Drill-down: clicking a name/ⓘ invokes `onOpenCard` with the resolved card name + full `ownerIdHex` (not the nickname).
- Pending row resolves the live card name; never shows a nickname rung.

TDD throughout (per `superpowers:test-driven-development`): each behavior change lands test-first.

## 9. Scope

**In:** owner-name + avatar resolution (friends + pending), client-side nicknames (active friends), persistent short-hex, drill-down popover reuse, the backend nickname store + IPC + projection + privacy guard.

**Out (deferred):**
- Nicknames on pending requests (pre-accept).
- Referral-catalog sub-rows (`ReferralView`) — keep current `display ?? shortId`.
- Cross-device sync of nicknames / friends — waits for **ZEB-417** Fleet-Sync substrate; the store is shaped (HLC-stamped, op-friendly) to become a `write(dataset, op)` consumer with minimal rework. Friend-graph cross-device sync is also future.

## 10. Risks / open notes

- **Subscription cost:** N friends + M pending = N+M live card subscriptions on a second poll loop (3s). Friend lists are small; acceptable. If a friend is also a visible community member they get two subscriptions (roster + friends) — negligible duplication, not worth reference-counting now (YAGNI).
- **Card never resolves** (peer offline / discovery off): label falls back to frozen `display` then short-hex; avatar → identicon. No spinner churn (the ladder degrades silently).
- **Nickname vs. card-name confusion:** mitigated by the always-visible short-hex + the drill-down always showing the real card name.

## 11. File structure

**Create:**
- `src-tauri/src/friend_nicknames.rs` — local store.
- `docs/plans/2026-06-09-zeb-419-friend-names-nicknames-plan.md` — implementation plan (writing-plans output).

**Modify:**
- `src-tauri/src/lib.rs` — `mod friend_nicknames;`, `FriendDto.nickname`, `list_friends_inner` signature + join, `list_friends` wrapper loads the store, new `set_friend_nickname` IPC + handler registration, privacy guard test, casing-guard extension.
- `src/lib/friend-service.ts` — `FriendDto.nickname`, `setNickname()`.
- `src/lib/components/FriendsPanel.svelte` — `cardService` + `onOpenCard` props, dedicated-instance lifecycle, label ladder, avatar, short-hex (already present), nickname edit UI, drill-down wiring.
- `src/App.svelte` — construct + wire `friendCardService`, pass `cardService` + `onOpenCard` into `FriendsPanel`.
- `src/lib/components/FriendsPanel.test.ts` — new tests (§8).
- Backend test module(s) for `friend_nicknames` + projection + privacy guard.
