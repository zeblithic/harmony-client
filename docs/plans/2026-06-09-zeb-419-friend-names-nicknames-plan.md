# Friend Owner-Names + Local Nicknames — Implementation Plan (ZEB-419)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render friends + pending requests by their live owner-card name (and avatar) and let users set purely-local, per-friend nicknames, with the verifiable owner_id always one drill-down away.

**Architecture:** Reuse the ZEB-341 member-card pub/sub pipeline for live names (a panel-owned `MemberCardService`); add a new backend local-only `friend_nicknames` store (outside the published CRDT) surfaced as `FriendDto.nickname`; compose a label ladder `nickname ► card name ► frozen display ► short-hex` in `FriendsPanel`. Drill-down reuses `ProfilePopover` owner-card mode via App's existing `openMemberCard`.

**Tech Stack:** Rust/Tauri (`src-tauri`), Svelte 5 runes, vitest, `cargo nextest`/`clippy`/`fmt`. Design: `docs/specs/2026-06-09-zeb-419-friend-names-nicknames-design.md`.

**Deviation note (flagged for review):** the local store uses a wall-clock `updated_ms: u64` as its LWW key rather than a reserved HLC. Rationale: the store is single-writer + local-only this phase; coupling it to the device HLC tracker adds plumbing/poisoning edge cases for no local benefit. ZEB-417 assigns real op HLCs when it adopts the dataset. The substrate-ready intent (a monotonic LWW key per entry) is preserved.

---

## File Structure

**Create:**
- `src-tauri/src/friend_nicknames.rs` — local nickname store (load/save/set/get/remove + unit tests).

**Modify:**
- `src-tauri/src/lib.rs` — `mod friend_nicknames;`; add `FriendDto.nickname`; `list_friends_inner` sets `nickname: None`; new pure `apply_nicknames`; `list_friends` IPC loads store + joins; new `set_friend_nickname` IPC + handler registration; privacy guard test; casing-guard extension.
- `src/lib/friend-service.ts` — `FriendDto.nickname`; `setNickname()`.
- `src/lib/components/FriendsPanel.svelte` — `cardService` + `onOpenCard` props; card lifecycle; label ladder + avatar + short-hex; nickname edit UI; drill-down wiring.
- `src/App.svelte` — construct + wire `friendCardService`; pass `cardService` + `onOpenCard={openMemberCard}` to `FriendsPanel`.
- `src/lib/components/FriendsPanel.test.ts` — new tests.

**Conventions (load-bearing):** Cargo cmds run from `src-tauri/`; frontend cmds from repo root. Tauri IPCs use plain `#[tauri::command]` (camelCase JS → snake_case Rust; NO `rename_all` — ZEB-414). Always `--locked --all-targets --features test-fixtures`. Restore `src-tauri/gen/schemas/` before every commit (regenerated on build; never committed). Stage files explicitly (never `git add -A`). Commit trailer: `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.

---

## Task 0: Branch + commit the spec

- [ ] **Step 1: Create the feature branch**

```bash
cd /c/zeblith/work/zeblithic/harmony-client
git checkout main && git pull --ff-only
git checkout -b zeblith/zeb-419-harmony-client-friends-display-owner-names-local-client-side
```

- [ ] **Step 2: Commit the design + plan docs**

```bash
git add docs/specs/2026-06-09-zeb-419-friend-names-nicknames-design.md docs/plans/2026-06-09-zeb-419-friend-names-nicknames-plan.md
git commit -m "docs(zeb-419): friend owner-names + local nicknames design + plan

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 1: `friend_nicknames` local store

**Files:**
- Create: `src-tauri/src/friend_nicknames.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod friend_nicknames;` near the other `mod` decls, e.g. by `mod pkarr_settings;`)

- [ ] **Step 1: Write the module with failing unit tests**

Create `src-tauri/src/friend_nicknames.rs`:

```rust
//! ZEB-419: local-only, per-owner friend nicknames.
//!
//! A purely-local label the user attaches to a friend for their own reference.
//! NEVER published, broadcast, or synced in this phase — the privacy guarantee
//! ("nobody sees the nickname you give a contact") is structural: these bytes
//! live in their OWN file, outside `OwnerState.friend_graph` (the published
//! CRDT). Entries carry a monotonic `updated_ms` LWW key so the ZEB-417
//! fleet-sync substrate can later adopt the whole map as a replicated dataset.
//!
//! Persistence mirrors `pkarr_settings.rs`: `load_or_default` tolerates a
//! missing/corrupt file (→ empty), `save` writes atomically (temp + rename).

use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FriendNicknames {
    /// owner_id hex (lowercase, 32 chars) -> entry.
    #[serde(default)]
    pub entries: BTreeMap<String, NicknameEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NicknameEntry {
    pub nickname: String,
    /// Wall-clock ms at last write — local LWW key (see module docs).
    pub updated_ms: u64,
}

impl FriendNicknames {
    /// Load from `path`, or return an empty map when the file is missing or
    /// unparseable (never panics; a corrupt file must not brick the panel).
    pub fn load_or_default(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Atomically persist to `path` (write temp in the same dir, then rename).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| format!("encode: {e}"))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| format!("write tmp: {e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;
        Ok(())
    }

    /// Upsert (`Some` non-blank) or clear (`None`/blank) a nickname. `owner_id_hex`
    /// is lowercased. Returns true when the map changed.
    pub fn set(&mut self, owner_id_hex: &str, nickname: Option<&str>, now_ms: u64) -> bool {
        let key = owner_id_hex.to_lowercase();
        match nickname.map(str::trim).filter(|s| !s.is_empty()) {
            Some(nick) => {
                let entry = NicknameEntry { nickname: nick.to_string(), updated_ms: now_ms };
                self.entries.insert(key, entry).as_ref() != Some(&self.entries[&owner_id_hex.to_lowercase()])
                    || true
            }
            None => self.entries.remove(&key).is_some(),
        }
    }

    /// The nickname for `owner_id_hex`, if any.
    pub fn get(&self, owner_id_hex: &str) -> Option<&str> {
        self.entries.get(&owner_id_hex.to_lowercase()).map(|e| e.nickname.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_roundtrips_and_lowercases() {
        let mut n = FriendNicknames::default();
        n.set("AABB", Some("Koya"), 100);
        assert_eq!(n.get("aabb"), Some("Koya"));
        assert_eq!(n.get("AABB"), Some("Koya")); // get also lowercases
    }

    #[test]
    fn blank_or_none_clears() {
        let mut n = FriendNicknames::default();
        n.set("aa", Some("x"), 1);
        n.set("aa", Some("   "), 2); // whitespace clears
        assert_eq!(n.get("aa"), None);
        n.set("aa", Some("y"), 3);
        n.set("aa", None, 4); // None clears
        assert_eq!(n.get("aa"), None);
    }

    #[test]
    fn updated_ms_advances_on_reset() {
        let mut n = FriendNicknames::default();
        n.set("aa", Some("x"), 10);
        n.set("aa", Some("y"), 20);
        assert_eq!(n.entries["aa"].updated_ms, 20);
        assert_eq!(n.entries["aa"].nickname, "y");
    }

    #[test]
    fn load_or_default_tolerates_missing_and_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("friend_nicknames.json");
        assert!(FriendNicknames::load_or_default(&path).entries.is_empty());
        std::fs::write(&path, b"not json").unwrap();
        assert!(FriendNicknames::load_or_default(&path).entries.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("friend_nicknames.json");
        let mut n = FriendNicknames::default();
        n.set("aa", Some("Koya"), 7);
        n.save(&path).unwrap();
        let loaded = FriendNicknames::load_or_default(&path);
        assert_eq!(loaded.get("aa"), Some("Koya"));
        assert_eq!(loaded.entries["aa"].updated_ms, 7);
    }
}
```

> Simplify the `set` return value: replace the convoluted upsert expression with a clear version:
> ```rust
> Some(nick) => {
>     let prev = self.entries.insert(key, NicknameEntry { nickname: nick.to_string(), updated_ms: now_ms });
>     !matches!(prev, Some(p) if p.nickname == nick)
> }
> ```
> (The bool is informational; callers don't currently branch on it.)

Add `mod friend_nicknames;` to `src-tauri/src/lib.rs` beside the other module declarations. Confirm `tempfile` is a dev-dependency (it is used widely in this crate; if `cargo` complains, it's already in `[dev-dependencies]`).

- [ ] **Step 2: Run the tests — verify they pass**

```bash
cd /c/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(friend_nicknames)'
```
Expected: 5 tests pass. (TDD note: write each test, watch it fail against an empty `impl`, then fill the method. The module above already pairs each method with its test.)

- [ ] **Step 3: Commit**

```bash
cd /c/zeblith/work/zeblithic/harmony-client
git checkout -- src-tauri/gen/schemas/ 2>/dev/null || true
git add src-tauri/src/friend_nicknames.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-419): local-only friend nickname store

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: `FriendDto.nickname` + `apply_nicknames` join + privacy guard

**Files:**
- Modify: `src-tauri/src/lib.rs` (`FriendDto` struct ~`:34693`; `list_friends_inner` ~`:34722`; the `list_friends` IPC wrapper — locate with `grep -n "async fn list_friends(" src-tauri/src/lib.rs`).

- [ ] **Step 1: Add the field + pure join helper with failing tests**

In `lib.rs`, add to `FriendDto` (after `display`):

```rust
    /// ZEB-419: local-only nickname (joined from `friend_nicknames`, never from
    /// the published CRDT). `None` when the user hasn't set one.
    pub nickname: Option<String>,
```

In `list_friends_inner`'s map closure, add `nickname: None,` to the constructed `FriendDto` (the join happens later, in `apply_nicknames`). Then add the pure join helper near `list_friends_inner`:

```rust
/// ZEB-419: overlay local nicknames onto a projected friend list. Pure so it's
/// unit-testable without a NodeState harness; the `list_friends` IPC calls it
/// after `list_friends_inner`. A friend with no nickname entry is unchanged.
pub fn apply_nicknames(
    mut friends: Vec<FriendDto>,
    nicknames: &crate::friend_nicknames::FriendNicknames,
) -> Vec<FriendDto> {
    for f in &mut friends {
        f.nickname = nicknames.get(&f.owner_id_hex).map(str::to_owned);
    }
    friends
}
```

Add tests in the existing `lib.rs` test module (find an existing `mod tests` with friend-graph helpers, or add a focused one):

```rust
#[test]
fn apply_nicknames_overlays_only_matching_owners() {
    let friends = vec![
        FriendDto { owner_id_hex: "aa".into(), display: None, nickname: None,
            status: crate::friend_graph::FriendStatus::Active,
            established_via: crate::friend_graph::FriendOrigin::MutualKey, referrable: false },
        FriendDto { owner_id_hex: "bb".into(), display: Some("Hint".into()), nickname: None,
            status: crate::friend_graph::FriendStatus::Active,
            established_via: crate::friend_graph::FriendOrigin::Token, referrable: false },
    ];
    let mut nicks = crate::friend_nicknames::FriendNicknames::default();
    nicks.set("AA", Some("Koya"), 1); // note casing differs from owner_id_hex
    let out = apply_nicknames(friends, &nicks);
    assert_eq!(out[0].nickname.as_deref(), Some("Koya"));
    assert_eq!(out[1].nickname, None); // bb has no nickname; display hint untouched
    assert_eq!(out[1].display.as_deref(), Some("Hint"));
}
```

> The exact `FriendDto` / `FriendStatus` / `FriendOrigin` variant names must match the crate — verify against `friend_graph.rs`. Any other place that constructs a `FriendDto` literal (tests, fixtures) now needs `nickname: None`; the compiler will flag them — add the field.

- [ ] **Step 2: Wire the join into the `list_friends` IPC**

Locate the `list_friends` IPC wrapper. It currently does roughly: lock NodeState → snapshot `crdt_state` + `pkarr_settings_path` → call `list_friends_inner(&state)`. Change it to also load the nickname store and apply the overlay:

```rust
// inside the list_friends IPC, after computing `friends` via list_friends_inner:
let nicknames = match &pkarr_settings_path {
    Some(p) => crate::friend_nicknames::FriendNicknames::load_or_default(
        &p.with_file_name("friend_nicknames.json"),
    ),
    None => crate::friend_nicknames::FriendNicknames::default(),
};
let friends = apply_nicknames(friends, &nicknames);
```

Ensure `pkarr_settings_path` is snapshotted from NodeState in that wrapper (it's the same `Option<PathBuf>` field `set_friend_auto_accept` uses at `:35744`). If the wrapper doesn't already hold it, add it to the locked snapshot.

- [ ] **Step 3: Privacy guard test**

Add a test asserting a nickname never rides a published owner-state serialization. Place it near the friend-graph tests:

```rust
#[test]
fn nickname_never_appears_in_published_owner_state() {
    // Build an OwnerState with one friend, then serialize it via the SAME
    // canonical encoder the sync/backup path publishes with. Nicknames live in
    // a separate file, so the published bytes must not contain the nickname.
    // This LOCKS the structural invariant: if a future refactor moves nickname
    // into FriendEntry/OwnerState, the published bytes would contain it and
    // this test fails.
    let state = /* construct a minimal OwnerState with a friend addr 0xAA.. via
                   the existing test helper used by list_friends_inner tests */;
    let bytes = crate::owner_state_crypto::canonical_cbor_encode(&state)
        .expect("encode owner state");
    let needle = b"Koya-secret-nickname";
    assert!(
        !bytes.windows(needle.len()).any(|w| w == needle),
        "published owner-state must not contain any nickname bytes",
    );
}
```

> Reuse whatever helper the existing `list_friends_inner` tests use to build an `OwnerState` with a friend (search `list_friends_inner` test usages). The nickname string `"Koya-secret-nickname"` is never inserted into `OwnerState` anywhere — the assertion is trivially green today and stays as a tripwire. If `canonical_cbor_encode` isn't the publish encoder, use the encoder `owner_state_sync` actually serializes with.

- [ ] **Step 4: Run the gates**

```bash
cd /c/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(apply_nicknames) + test(nickname_never) + test(list_friends)'
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```
Expected: new tests pass; clippy clean.

- [ ] **Step 5: Commit** (restore gen/schemas first)

```bash
cd /c/zeblith/work/zeblithic/harmony-client
git checkout -- src-tauri/gen/schemas/ 2>/dev/null || true
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-419): join nicknames into FriendDto + privacy guard

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: `set_friend_nickname` IPC

**Files:**
- Modify: `src-tauri/src/lib.rs` (new IPC near `set_friend_auto_accept` ~`:35734`; register in the `tauri::generate_handler!` list near `set_friend_auto_accept` ~`:37902`).
- Modify: `src-tauri/tests/ipc_arg_casing.rs` (extend the casing guard).

- [ ] **Step 1: Write the IPC**

```rust
/// ZEB-419: set or clear the local-only nickname for a friend (by owner_id hex).
/// `nickname = None`/blank clears it. Persists to `friend_nicknames.json` beside
/// the connectivity settings, then emits `friend-list-changed` so the panel
/// re-fetches with the new label. Local-only: never published or synced.
#[tauri::command]
async fn set_friend_nickname(
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<NodeState>>,
    owner_id_hex: String,
    nickname: Option<String>,
) -> Result<(), String> {
    // Validate the owner_id (reject malformed before any write). decode_owner_id_16
    // is the 16-byte master owner_id decoder used by the other friend IPCs.
    let _ = decode_owner_id_16(&owner_id_hex)?;

    let path = {
        state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?
            .pkarr_settings_path
            .clone()
    };
    let Some(path) = path else {
        return Err(OWNER_NOT_LOADED_MSG.into());
    };
    let nick_path = path.with_file_name("friend_nicknames.json");

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let mut store = crate::friend_nicknames::FriendNicknames::load_or_default(&nick_path);
    store.set(&owner_id_hex, nickname.as_deref(), now_ms);
    store
        .save(&nick_path)
        .map_err(|e| format!("save friend_nicknames: {e}"))?;

    let _ = app.emit("friend-list-changed", ());
    Ok(())
}
```

Register `set_friend_nickname,` in the `generate_handler!` invocation (alongside `set_friend_auto_accept`, `get_friend_auto_accept`).

- [ ] **Step 2: Extend the casing guard**

In `src-tauri/tests/ipc_arg_casing.rs`, add `set_friend_nickname` to whatever list/assertion enumerates friend IPC arg names, asserting it expects camelCase `ownerIdHex` + `nickname` (NOT `owner_id_hex`). Mirror an existing entry (e.g. `accept_friend_request` / `add_friend_by_key`). Run it red→green if the harness is data-driven; otherwise add an explicit assertion.

- [ ] **Step 3: Gates**

```bash
cd /c/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --all-targets --features test-fixtures -E 'test(ipc_arg_casing)'
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo fmt --all -- --check
```
Expected: green.

- [ ] **Step 4: Commit** (restore gen/schemas first)

```bash
cd /c/zeblith/work/zeblithic/harmony-client
git checkout -- src-tauri/gen/schemas/ 2>/dev/null || true
git add src-tauri/src/lib.rs src-tauri/tests/ipc_arg_casing.rs
git commit -m "feat(zeb-419): set_friend_nickname IPC + casing guard

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: Frontend service — `FriendDto.nickname` + `setNickname`

**Files:**
- Modify: `src/lib/friend-service.ts`
- Modify: `src/lib/friend-service.test.ts`

- [ ] **Step 1: Failing test**

In `friend-service.test.ts`, add (mirroring the existing `setReferrable`/`unfriend` adapter-call tests):

```ts
it('setNickname forwards camelCase args to set_friend_nickname', async () => {
  const invoke = vi.fn().mockResolvedValue(undefined);
  const svc = new FriendService();
  await svc.connectAdapter(makeMockAdapter(invoke)); // reuse the file's adapter helper
  await svc.setNickname('aa'.repeat(16), 'Koya');
  expect(invoke).toHaveBeenCalledWith('set_friend_nickname', { ownerIdHex: 'aa'.repeat(16), nickname: 'Koya' });
  await svc.setNickname('aa'.repeat(16), null);
  expect(invoke).toHaveBeenLastCalledWith('set_friend_nickname', { ownerIdHex: 'aa'.repeat(16), nickname: null });
});
```

Run red:
```bash
cd /c/zeblith/work/zeblithic/harmony-client
npx vitest run src/lib/friend-service.test.ts
```
Expected: FAIL (`setNickname` undefined).

- [ ] **Step 2: Implement**

Add to `FriendDto` interface: `nickname?: string | null;`. Add the method:

```ts
  /** ZEB-419: set (or clear, with `null`) the LOCAL-ONLY nickname for a friend.
   *  Never shared with the peer or other devices (this phase). */
  async setNickname(ownerIdHex: string, nickname: string | null): Promise<void> {
    await this.invoke<void>('set_friend_nickname', { ownerIdHex, nickname });
  }
```

Run green (Step 1 command). Expected: PASS. Also `npx tsc --noEmit` clean.

- [ ] **Step 3: Commit**

```bash
git add src/lib/friend-service.ts src/lib/friend-service.test.ts
git commit -m "feat(zeb-419): FriendService.setNickname + nickname DTO field

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: FriendsPanel — live card resolution (name + avatar + short-hex)

**Files:**
- Modify: `src/lib/components/FriendsPanel.svelte`
- Modify: `src/lib/components/FriendsPanel.test.ts`
- Modify: `src/App.svelte`

**Context for the implementer:** `MemberCardService` (`src/lib/member-card-service.ts`) resolves owner_id → `{ displayName, avatarUrl, statusText }` via subscribe/poll/event. `subscribeVisible(ids)` reconciles to *exactly* `ids`, so the panel needs its OWN instance (don't share App's roster one). Follow the same liveness discipline already in this file (the `destroyed` flag set in `onDestroy`): never mutate `$state` or schedule work after teardown.

- [ ] **Step 1: App constructs + injects a dedicated instance**

In `src/App.svelte`:
- Near `const friendService = new FriendService();` (`:1053`), add:
  ```ts
  // ZEB-419: a SECOND MemberCardService dedicated to the Friends panel. Separate
  // from the roster instance because subscribeVisible(ids) reconciles to exactly
  // the passed set — sharing would make friends + roster unsubscribe each other.
  const friendCardService = new MemberCardService();
  ```
- Where the roster instance is wired, mirror for `friendCardService`: `friendCardService.setAdapter(adapter)` (next to the roster `memberCardService.setAdapter(adapter)` ~`:1334`) and `friendCardService.setAvatarResolver(avatarResolver)` (next to ~`:1064`). Do NOT call `subscribeVisible` on it from App — the panel drives it.
- At the `<FriendsPanel ... />` mount (`:2753`), pass: `cardService={friendCardService} onOpenCard={openMemberCard}`.

- [ ] **Step 2: Failing test — label ladder precedence**

In `FriendsPanel.test.ts`, add a mock card service helper and a precedence test. Mirror the existing harness (`render(FriendsPanel, { props })`, `vi.mock('../connectivity-adapter')`, mock `FriendService`). The mock card service:

```ts
function makeMockCardService(cards: Record<string, { displayName: string; avatarUrl?: string }> = {}) {
  return {
    onUpdate: undefined as (() => void) | undefined,
    resolve: (id: string) => cards[id.toLowerCase()],
    subscribeVisible: vi.fn().mockResolvedValue(undefined),
    unsubscribeAll: vi.fn().mockResolvedValue(undefined),
  };
}
```

Test:
```ts
it('label ladder: nickname > card name > display hint > short-hex', async () => {
  const ID = (b: string) => b.repeat(32); // 64-hex placeholder owner id
  const friends = [
    { ownerIdHex: ID('a'), display: null, nickname: 'Nick',  status: 'active', establishedVia: 'mutual_key', referrable: false },
    { ownerIdHex: ID('b'), display: 'Hint', nickname: null,  status: 'active', establishedVia: 'token',      referrable: false },
    { ownerIdHex: ID('c'), display: 'Hint', nickname: null,  status: 'active', establishedVia: 'token',      referrable: false },
    { ownerIdHex: ID('d'), display: null, nickname: null,    status: 'active', establishedVia: 'mutual_key', referrable: false },
  ];
  const service = makeMockFriendService({ friends });
  const cardService = makeMockCardService({ [ID('c')]: { displayName: 'CardName' } });
  render(FriendsPanel, { props: { service, cardService } });
  await screen.findByTestId('friend-list');
  // a: nickname wins
  expect(screen.getByText('Nick')).toBeInTheDocument();
  // b: no card, no nickname → display hint
  expect(screen.getByText('Hint')).toBeInTheDocument();
  // c: card name beats display hint
  expect(screen.getByText('CardName')).toBeInTheDocument();
  // d: nothing → short-hex
  expect(screen.getByText(shortId(ID('d')))).toBeInTheDocument(); // or assert the rendered prefix
});
```

(Use the file's existing helpers for `makeMockFriendService` / rendering; `shortId` is the same prefix the component uses.)

Run red:
```bash
npx vitest run src/lib/components/FriendsPanel.test.ts
```
Expected: FAIL (component ignores `cardService`/`nickname`).

- [ ] **Step 3: Implement resolution + markup**

In `FriendsPanel.svelte`:
- Imports: `import Avatar from './Avatar.svelte';`, `import { MemberCardService } from '../member-card-service';` (type only), and `import type { OpenCardPayload } from './MemberRow.svelte';`.
- Props: extend to `let { service, cardService, onOpenCard }: { service: FriendService; cardService: MemberCardService; onOpenCard?: (payload: OpenCardPayload, ev: MouseEvent) => void } = $props();`.
- State: `let cardVersion = $state(0);`.
- `onMount`: `cardService.onUpdate = () => { if (!destroyed) cardVersion += 1; };`
- A reactive subscription effect:
  ```ts
  $effect(() => {
    // Re-subscribe whenever the visible owner-id set changes. subscribeVisible
    // reconciles (idempotent); errors are swallowed inside the service.
    const ids = [
      ...friends.filter((f) => f.status === 'active' || f.status === 'pending').map((f) => f.ownerIdHex),
      ...pendingRequests.map((r) => r.ownerIdHex),
    ];
    void cardService.subscribeVisible(ids);
  });
  ```
- `onDestroy` (existing): add `cardService.onUpdate = undefined; void cardService.unsubscribeAll();`.
- Label helpers (read `cardVersion` so they re-run on card updates):
  ```ts
  function cardName(ownerIdHex: string): string | undefined { cardVersion; return cardService.resolve(ownerIdHex)?.displayName; }
  function avatarUrl(ownerIdHex: string): string | undefined { cardVersion; return cardService.resolve(ownerIdHex)?.avatarUrl; }
  function friendLabel(f: FriendDto): string { return f.nickname ?? cardName(f.ownerIdHex) ?? f.display ?? shortId(f.ownerIdHex); }
  function requestLabel(r: PendingFriendRequestDto): string { return cardName(r.ownerIdHex) ?? r.display ?? shortId(r.ownerIdHex); }
  ```
- Markup: in the friend row (`~:561`) and pending row (`~:712`), prepend `<Avatar address={f.ownerIdHex} displayName={friendLabel(f)} avatarUrl={avatarUrl(f.ownerIdHex)} size={28} />`, and change `<span class="friend-name">{f.display ?? shortId(f.ownerIdHex)}</span>` → `<span class="friend-name">{friendLabel(f)}</span>` (and the pending equivalent → `{requestLabel(req)}`). Keep the existing `.friend-addr` short-hex line (it already shows `shortId` with full hex in `title`). Wrap the row's avatar+text in a flex container if needed (mirror `MemberRow`'s `.member-row` / `.member-info`).

Run green (Step 2 command). Expected: PASS.

- [ ] **Step 4: Failing test — subscription lifecycle**

```ts
it('subscribes to active+pending ids and unsubscribes on unmount', async () => {
  const ID = (b: string) => b.repeat(32);
  const friends = [{ ownerIdHex: ID('a'), display: null, nickname: null, status: 'active', establishedVia: 'mutual_key', referrable: false }];
  const pending = [{ ownerIdHex: ID('b'), display: null, receivedAtMs: 0 }];
  const service = makeMockFriendService({ friends, pending });
  const cardService = makeMockCardService();
  const { unmount } = render(FriendsPanel, { props: { service, cardService } });
  await screen.findByTestId('friend-list');
  expect(cardService.subscribeVisible).toHaveBeenCalledWith(expect.arrayContaining([ID('a'), ID('b')]));
  unmount();
  expect(cardService.unsubscribeAll).toHaveBeenCalled();
});
```
Implement already satisfies this (Step 3). Run green.

- [ ] **Step 5: Gates + commit**

```bash
cd /c/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit && npx vitest run src/lib/components/FriendsPanel.test.ts
git add src/lib/components/FriendsPanel.svelte src/lib/components/FriendsPanel.test.ts src/App.svelte
git commit -m "feat(zeb-419): live card name + avatar in FriendsPanel rows

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: FriendsPanel — nickname edit UI (active friends)

**Files:**
- Modify: `src/lib/components/FriendsPanel.svelte`
- Modify: `src/lib/components/FriendsPanel.test.ts`

- [ ] **Step 1: Failing test**

```ts
it('sets a nickname via the edit affordance', async () => {
  const ID = 'a'.repeat(32);
  const friends = [{ ownerIdHex: ID, display: null, nickname: null, status: 'active', establishedVia: 'mutual_key', referrable: false }];
  const service = makeMockFriendService({ friends });
  service.setNickname = vi.fn().mockResolvedValue(undefined);
  const cardService = makeMockCardService();
  render(FriendsPanel, { props: { service, cardService } });
  await screen.findByTestId('friend-list');
  await fireEvent.click(screen.getByTestId(`set-nickname-btn-${ID}`));
  const input = screen.getByTestId(`nickname-input-${ID}`);
  await fireEvent.input(input, { target: { value: 'Koya' } });
  await fireEvent.click(screen.getByTestId(`nickname-save-${ID}`));
  expect(service.setNickname).toHaveBeenCalledWith(ID, 'Koya');
});
```
Run red.

- [ ] **Step 2: Implement**

Add per-row edit state + handler (mirror the existing `unfriending`/`referrableSaving` Set-guard pattern):

```ts
let editingNickname = $state<string | null>(null); // ownerIdHex currently editing, or null
let nicknameDraft = $state('');
let nicknameSaving = $state<Set<string>>(new Set());

function startEditNickname(f: FriendDto) { editingNickname = f.ownerIdHex; nicknameDraft = f.nickname ?? ''; }
function cancelEditNickname() { editingNickname = null; nicknameDraft = ''; }
async function saveNickname(ownerIdHex: string) {
  if (nicknameSaving.has(ownerIdHex)) return;
  nicknameSaving = new Set(nicknameSaving).add(ownerIdHex);
  try {
    await service.setNickname(ownerIdHex, nicknameDraft.trim() || null);
    if (destroyed) return;
    editingNickname = null; nicknameDraft = '';
    // friend-list-changed → refresh() repaints with the new nickname.
  } catch (e) {
    if (destroyed) return;
    addStatus = `Couldn't save nickname: ${e instanceof Error ? e.message : String(e)}`;
  } finally {
    const next = new Set(nicknameSaving); next.delete(ownerIdHex); nicknameSaving = next;
  }
}
```

Markup (active rows only — guard `{#if f.status === 'active'}`): a `data-testid="set-nickname-btn-{f.ownerIdHex}"` button → toggles an inline editor `{#if editingNickname === f.ownerIdHex}` with `data-testid="nickname-input-{f.ownerIdHex}"` (bind `nicknameDraft`), a save button `data-testid="nickname-save-{f.ownerIdHex}"` → `saveNickname(f.ownerIdHex)`, Enter triggers save, Esc/cancel button → `cancelEditNickname()`. Disable controls while `nicknameSaving.has(f.ownerIdHex)`.

Run green.

- [ ] **Step 3: Gates + commit**

```bash
npx tsc --noEmit && npx vitest run src/lib/components/FriendsPanel.test.ts
git add src/lib/components/FriendsPanel.svelte src/lib/components/FriendsPanel.test.ts
git commit -m "feat(zeb-419): inline nickname editor for active friends

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 7: FriendsPanel — drill-down (identity verification)

**Files:**
- Modify: `src/lib/components/FriendsPanel.svelte`
- Modify: `src/lib/components/FriendsPanel.test.ts`

- [ ] **Step 1: Failing test**

```ts
it('drill-down opens the owner card with full hex + real card name (not the nickname)', async () => {
  const ID = 'a'.repeat(32);
  const friends = [{ ownerIdHex: ID, display: null, nickname: 'Nick', status: 'active', establishedVia: 'mutual_key', referrable: false }];
  const service = makeMockFriendService({ friends });
  const cardService = makeMockCardService({ [ID]: { displayName: 'RealCardName' } });
  const onOpenCard = vi.fn();
  render(FriendsPanel, { props: { service, cardService, onOpenCard } });
  await screen.findByTestId('friend-list');
  await fireEvent.click(screen.getByTestId(`friend-identity-${ID}`));
  expect(onOpenCard).toHaveBeenCalled();
  const payload = onOpenCard.mock.calls[0][0];
  expect(payload.ownerIdHex).toBe(ID);            // full hex, not short
  expect(payload.displayName).toBe('RealCardName'); // card name, NOT the nickname
});
```
Run red.

- [ ] **Step 2: Implement**

Add a handler + make the short-hex line / a ⓘ button trigger it:

```ts
function openIdentity(ownerIdHex: string, ev: MouseEvent) {
  const resolved = cardService.resolve(ownerIdHex);
  onOpenCard?.(
    { ownerIdHex, displayName: resolved?.displayName ?? '', statusText: resolved?.statusText ?? '', avatarUrl: resolved?.avatarUrl },
    ev,
  );
}
```

Markup: make the `.friend-addr` line (or an adjacent ⓘ button) a `<button type="button" data-testid="friend-identity-{f.ownerIdHex}" onclick={(e) => openIdentity(f.ownerIdHex, e)} title="Verify identity">…</button>` for both friend and pending rows. Note: payload `displayName` is the **card name**, never the nickname — the popover is the anti-spoof surface.

Run green.

- [ ] **Step 3: Gates + commit**

```bash
npx tsc --noEmit && npx vitest run src/lib/components/FriendsPanel.test.ts
git add src/lib/components/FriendsPanel.svelte src/lib/components/FriendsPanel.test.ts
git commit -m "feat(zeb-419): friend identity drill-down via owner-card popover

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 8: Full gate sweep, push, open PR

- [ ] **Step 1: Full local gates**

```bash
cd /c/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --all-targets --features test-fixtures -E 'test(friend) + test(nickname) + test(ipc_arg_casing)'
```
Expected: all green. (Full `--workspace` nextest has ~29 known-local reds — UDP :4242 bind + profile_broadcast timer — that pass on CI; scope the run as above and trust CI for the rest.)

- [ ] **Step 2: Restore gen/schemas, push**

```bash
cd /c/zeblith/work/zeblithic/harmony-client
git checkout -- src-tauri/gen/schemas/ 2>/dev/null || true
git status   # confirm no gen/schemas/*.json or .playwright-scratch staged
git push -u origin zeblith/zeb-419-harmony-client-friends-display-owner-names-local-client-side
```

- [ ] **Step 3: Open the PR**

```bash
gh pr create --title "ZEB-419: friend owner-names + local client-side nicknames" --body "$(cat <<'EOF'
## Summary

Renders friends + pending requests by their **live owner-card name + avatar** (reusing the ZEB-341 member-card pipeline) and adds purely **local, per-friend nicknames** that never leave the device.

- Label ladder: `nickname ► live card name ► frozen display hint ► short-hex`.
- Verifiable owner_id stays one drill-down away (reuses `ProfilePopover` owner-card mode); the popover always shows the peer's real card name, so a nickname can't fully spoof identity.
- Nicknames live in a new local-only `friend_nicknames` store **outside** the published CRDT — privacy is structural. HLC/LWW-shaped for later adoption by the ZEB-417 fleet-sync substrate (no sync this phase).

## Test plan
- Backend: nickname store round-trip; `apply_nicknames` join; **privacy guard** (nickname never in a published owner-state serialization); IPC casing guard.
- Frontend: label-precedence ladder; nickname set/clear; subscribe/unsubscribe lifecycle on list change + unmount; drill-down opens owner-card with full hex + card name (not nickname).

Design: `docs/specs/2026-06-09-zeb-419-friend-names-nicknames-design.md`
Plan: `docs/plans/2026-06-09-zeb-419-friend-names-nicknames-plan.md`

Closes ZEB-419.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 4: Set ZEB-419 → In Progress / In Review** (via Linear) and link the PR.

---

## Task 9 (inline, post-PR): live cross-peer smoke

Not a subagent task — driven inline with Playwright/CDP on a real Tauri instance (see `project_friend_by_key_verified` driving recipe). Confirm on a live node: a friend with a published card renders its name + avatar; setting a nickname overrides it and survives restart; clearing falls back to the card name; the drill-down shows full hex + the real card name. This is the gate the unit tests can't cover (real card resolution + persistence). Optional / can be folded into the next manual test pass.

---

## Self-Review

- **Spec coverage:** owner-name resolution (T5) ✓, avatars (T5) ✓, nicknames active-only (T1–T4, T6) ✓, short-hex persistent (T5, existing `.friend-addr`) ✓, drill-down (T7) ✓, privacy guard (T2) ✓, substrate-ready store (T1) ✓, casing guard (T3) ✓.
- **Type consistency:** `FriendDto.nickname` added on both sides (T2 Rust, T4 TS); `cardService`/`onOpenCard` props consistent T5→T7; `OpenCardPayload` reused from `MemberRow`.
- **Placeholders:** the `set` return expression in T1 is replaced by the clarified version in the same step; the privacy-guard `OwnerState` construction defers to the existing test helper (named, not invented) — implementer wires the real helper.
- **Known follow-up (not a blocker):** "View full profile" for a friend who isn't also a visible community member may show an empty header (App's roster `resolveCard` doesn't subscribe friends); the popover itself is fully populated from the panel payload. Acceptable v1; note in PR.
