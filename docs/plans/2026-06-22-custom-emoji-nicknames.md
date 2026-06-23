# Custom-Emoji Nicknames Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a user assign a short personal name to a *public* custom reaction emoji and reuse it by name (search/pick) across channels.

**Architecture:** A new local-only `emoji_names.json` store mirrors the existing friend-nickname store (`friend_nicknames.rs`): CID-keyed `{name, mime, size, updated_ms}`, LWW by `updated_ms`, kept out of `OwnerState`. Three new Tauri IPCs (`set_emoji_name`, `list_emoji_names`, `preview_named_emoji`) mirror the friend-nickname IPC shape; `preview_named_emoji` reuses the public, no-channel-scope CAS fetch pattern of `fetch_avatar`. The frontend gets emoji-name methods on the existing `ChannelMessageService`, one new picker popover, and a lightweight in-context naming affordance.

**Tech Stack:** Rust (Tauri commands, serde, tokio), Svelte 5 (runes), TypeScript, `cargo nextest` + `vitest`.

**Key references (read before starting):**
- Spec: `docs/specs/2026-06-22-custom-emoji-nicknames-design.md`
- Store template: `src-tauri/src/friend_nicknames.rs` (mirror its structure + tests exactly)
- IPC template: `set_friend_nickname` at `src-tauri/src/lib.rs:45271`; `NICKNAME_WRITE_LOCK`/`MAX_NICKNAME_LEN` at `:45255`
- Public no-scope fetch: `fetch_avatar` at `src-tauri/src/lib.rs:15857`; `FetchRequest` at `src-tauri/src/event_loop.rs:264`
- Public/encrypted CID branch + fetch+ingest idioms: `authorize_and_fetch_artifact` (`lib.rs:20942`), `ingest_channel_artifact_bytes_inner` (`lib.rs:20739`)
- Emoji preview test fixture style: `preview_reaction_emoji_happy_path_returns_plaintext` (`lib.rs:23916`)
- Reaction DTO: `ReactionDto` + `reactions_for` (`src-tauri/src/community_channel_log.rs:851` and `:964`)
- Frontend service template: `src/lib/friend-service.ts` (listeners + `setNickname` + `invoke`); reaction/emoji methods in `src/lib/channel-message-service.ts:539-618`
- Render component: `src/lib/components/ReactionEmojiImage.svelte`
- Feed integration: `src/lib/components/ChannelMessageFeed.svelte` (`handleCustomEmojiPick` `:487`, chips `:684`, picker `:727`)
- Command registration: `tauri::generate_handler![` at `src-tauri/src/lib.rs:48100` (the live-app builder)

**Conventions (from CLAUDE.md / user memory):**
- Run cargo from `src-tauri/`: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; clippy `--all-targets ... -D warnings`; `cargo fmt --all`.
- Run frontend from repo root: `npx tsc --noEmit` and `npx vitest run` (full suite — `ChannelMessageFeed` is exercised by other component tests).
- Tauri IPC: snake_case Rust params ↔ camelCase JS keys. `cid`, `mime`, `size`, `name` are single words (no case transform); `Option<String>` ↔ `string | null`.
- Keep ZEB IDs out of branch/commit/PR names. Commits end with `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.
- Branch `custom-emoji-nicknames` already exists off `main @ 05cebeb2`; spec committed `691cf78b`.

---

## File Structure

**Create:**
- `src-tauri/src/emoji_names.rs` — the local-only CID→name store + name validator (mirrors `friend_nicknames.rs`).
- `src/lib/components/NamedEmojiPicker.svelte` — the one new surface: name-search quick-pick + rename/remove.

**Modify:**
- `src-tauri/src/lib.rs` — `mod emoji_names;`; `EMOJI_NAMES_WRITE_LOCK` + `MAX_EMOJI_NAME_LEN`; `ensure_public_cid` helper; `set_emoji_name`/`list_emoji_names`/`preview_named_emoji` commands + impls; `pin_public_emoji_best_effort` helper; register the 3 commands in `generate_handler!`.
- `src-tauri/src/community_channel_log.rs` — add `encrypted: Option<bool>` to `ReactionDto` and populate it in `reactions_for`.
- `src-tauri/tests/ipc_arg_casing.rs` — pin the camelCase arg mapping for the new commands.
- `src/lib/channel-message-service.ts` — `EmojiNameDto`, `encrypted?: boolean` on the reaction summary + reaction-received payload, `setEmojiName`/`listEmojiNames`/`previewNamedEmoji`, and the `emoji-names-changed` listener + `onEmojiNamesChanged`.
- `src/lib/components/ReactionEmojiImage.svelte` — make `communityId`/`channelId` optional; fall back to `previewNamedEmoji(cid)` when absent.
- `src/lib/components/ChannelMessageFeed.svelte` — open-popover button + `<NamedEmojiPicker>`; in-context "name this" affordance on public custom chips; optional name field at upload.

---

## Task 1: `emoji_names.rs` store + name validator

**Files:**
- Create: `src-tauri/src/emoji_names.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod emoji_names;` next to `mod friend_nicknames;`)

- [ ] **Step 1: Write the store + validator with unit tests**

Create `src-tauri/src/emoji_names.rs`:

```rust
//! Local-only, per-user custom-emoji nicknames (CID → personal name).
//!
//! A purely-local label the user attaches to a PUBLIC custom emoji so they can
//! find/reuse it by name. NEVER published, broadcast, or synced in this phase —
//! the privacy guarantee is structural: these bytes live in their OWN file,
//! outside `OwnerState`. Entries carry a monotonic `updated_ms` LWW key so the
//! ZEB-417 fleet-sync substrate can later adopt the whole map as a replicated
//! dataset (parity with `friend_nicknames.rs`). Persistence mirrors
//! `friend_nicknames.rs`: `load_or_default` tolerates a missing/corrupt file
//! (→ empty), `save` writes atomically.

use std::collections::BTreeMap;
use std::path::Path;

/// Max emoji-name length, in chars. Matches the spec charset bound.
pub const MAX_EMOJI_NAME_LEN: usize = 32;

/// True iff `name` is a valid emoji nickname: 1..=32 chars of `[A-Za-z0-9_-]`.
pub fn valid_emoji_name(name: &str) -> bool {
    let n = name.chars().count();
    n >= 1
        && n <= MAX_EMOJI_NAME_LEN
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EmojiNames {
    /// cid hex (lowercase, 64 chars) -> entry.
    #[serde(default)]
    pub entries: BTreeMap<String, EmojiNameEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EmojiNameEntry {
    pub name: String,
    /// MIME of the emoji image (e.g. "image/png") — stored so reuse needs no
    /// re-fetch; advisory (the renderer detects the real format from the header).
    pub mime: String,
    /// Plaintext byte length — the signed React descriptor size, supplied by the
    /// caller (a chip's `emojiSize` or the upload's returned size). Authoritative
    /// for re-reacting; the serve path re-derives + hard-caps regardless.
    pub size: u64,
    /// Wall-clock ms at last write — local LWW key (see module docs).
    pub updated_ms: u64,
}

impl EmojiNames {
    /// Load from `path`. Missing file → empty (normal first run). Corrupt/unreadable
    /// → empty + WARN (a bad file can't brick the picker, but a real problem stays
    /// visible). Mirrors `FriendNicknames::load_or_default`.
    pub fn load_or_default(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        error = %e, path = %path.display(),
                        "emoji_names: corrupt file; using empty set"
                    );
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(
                    error = %e, path = %path.display(),
                    "emoji_names: read failed; using empty set (names may be temporarily unavailable)"
                );
                Self::default()
            }
        }
    }

    /// Atomically persist to `path`, creating the parent dir first. Reuses
    /// `owner_state_persist::save_atomically` (parity with `FriendNicknames::save`).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| format!("encode: {e}"))?;
        crate::owner_state_persist::save_atomically(path, &bytes)
            .map_err(|e| format!("save emoji_names: {e}"))
    }

    /// Upsert (`Some` name) or clear (`None`) the name for `cid_hex` (lowercased).
    /// `mime`/`size` are recorded with a set and ignored on clear. The caller is
    /// responsible for validating the name (`valid_emoji_name`) and the CID's
    /// public-ness before calling.
    pub fn set(&mut self, cid_hex: &str, name: Option<&str>, mime: &str, size: u64, now_ms: u64) {
        let key = cid_hex.to_lowercase();
        match name {
            Some(n) => {
                self.entries.insert(
                    key,
                    EmojiNameEntry {
                        name: n.to_string(),
                        mime: mime.to_string(),
                        size,
                        updated_ms: now_ms,
                    },
                );
            }
            None => {
                self.entries.remove(&key);
            }
        }
    }

    /// The entry for `cid_hex`, if any.
    pub fn get(&self, cid_hex: &str) -> Option<&EmojiNameEntry> {
        self.entries.get(&cid_hex.to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_name_charset_and_length() {
        assert!(valid_emoji_name("catjam"));
        assert!(valid_emoji_name("ship_it-2"));
        assert!(valid_emoji_name("a"));
        assert!(valid_emoji_name(&"x".repeat(32)));
        assert!(!valid_emoji_name("")); // empty
        assert!(!valid_emoji_name(&"x".repeat(33))); // too long
        assert!(!valid_emoji_name("has space"));
        assert!(!valid_emoji_name("emoji!")); // punctuation
        assert!(!valid_emoji_name("café")); // non-ascii
    }

    #[test]
    fn set_get_roundtrips_and_lowercases_cid() {
        let mut m = EmojiNames::default();
        m.set("AABB", Some("catjam"), "image/png", 200, 100);
        let e = m.get("aabb").expect("entry");
        assert_eq!(e.name, "catjam");
        assert_eq!(e.mime, "image/png");
        assert_eq!(e.size, 200);
        assert_eq!(e.updated_ms, 100);
        // get also lowercases.
        assert_eq!(m.get("AABB").unwrap().name, "catjam");
    }

    #[test]
    fn none_clears() {
        let mut m = EmojiNames::default();
        m.set("aa", Some("x"), "image/png", 1, 1);
        m.set("aa", None, "", 0, 2);
        assert!(m.get("aa").is_none());
    }

    #[test]
    fn updated_ms_and_fields_advance_on_reset() {
        let mut m = EmojiNames::default();
        m.set("aa", Some("x"), "image/png", 10, 10);
        m.set("aa", Some("y"), "image/jpeg", 20, 20);
        let e = m.get("aa").unwrap();
        assert_eq!(e.name, "y");
        assert_eq!(e.mime, "image/jpeg");
        assert_eq!(e.size, 20);
        assert_eq!(e.updated_ms, 20);
    }

    #[test]
    fn two_cids_may_share_a_name() {
        // Soft uniqueness: backend does NOT enforce name uniqueness.
        let mut m = EmojiNames::default();
        m.set("aa", Some("dup"), "image/png", 1, 1);
        m.set("bb", Some("dup"), "image/png", 1, 1);
        assert_eq!(m.get("aa").unwrap().name, "dup");
        assert_eq!(m.get("bb").unwrap().name, "dup");
    }

    #[test]
    fn load_or_default_tolerates_missing_and_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("emoji_names.json");
        assert!(EmojiNames::load_or_default(&path).entries.is_empty());
        std::fs::write(&path, b"not json").unwrap();
        assert!(EmojiNames::load_or_default(&path).entries.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips_and_creates_parent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/sub/emoji_names.json");
        let mut m = EmojiNames::default();
        m.set("aa", Some("catjam"), "image/png", 7, 7);
        m.save(&path).expect("save creates the parent dir");
        let loaded = EmojiNames::load_or_default(&path);
        assert_eq!(loaded.get("aa").unwrap().name, "catjam");
        assert_eq!(loaded.get("aa").unwrap().size, 7);
    }
}
```

- [ ] **Step 2: Declare the module**

In `src-tauri/src/lib.rs`, find the line `mod friend_nicknames;` (grep: `grep -n "mod friend_nicknames;" src/lib.rs`) and add directly below it:

```rust
mod emoji_names;
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(emoji_names)'`
Expected: 7 tests pass (the `emoji_names::tests::*`).

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/emoji_names.rs src-tauri/src/lib.rs
git commit -m "feat(emoji): local CID->name store + name validator

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: `ensure_public_cid` helper + `set_emoji_name` / `list_emoji_names` IPCs

**Files:**
- Modify: `src-tauri/src/lib.rs` (add `EMOJI_NAMES_WRITE_LOCK`, `ensure_public_cid`, the two commands + a testable `set_emoji_name_impl`)

`set_emoji_name` mirrors `set_friend_nickname` (`lib.rs:45271`) but (a) validates the CID is **public** instead of an active friend, (b) takes `mime`/`size` from the caller, (c) writes `emoji_names.json`, (d) emits `emoji-names-changed`. The retain-on-name pin is added in Task 4.

- [ ] **Step 1: Write the failing tests**

Add to the test module that already hosts `preview_reaction_emoji_happy_path_returns_plaintext` (search `grep -n "preview_reaction_emoji_happy_path_returns_plaintext" src/lib.rs`; add these alongside it). The first two are pure-unit and need no fixture; the third uses a minimal `NodeState` with only `pkarr_settings_path` set.

```rust
    #[test]
    fn ensure_public_cid_accepts_public_rejects_encrypted_and_malformed() {
        // CID flags live in the bytes; a public emoji has the encrypted flag
        // unset. [0x42; 32] is public, [0xB2; 32] is encrypted (high bit set).
        let public_hex = hex::encode([0x42u8; 32]);
        let encrypted_hex = hex::encode([0xB2u8; 32]);
        assert!(ensure_public_cid(&public_hex).is_ok());
        let err = ensure_public_cid(&encrypted_hex).unwrap_err();
        assert!(err.contains("encrypted"), "got: {err}");
        assert!(ensure_public_cid("zz").is_err()); // not 64 hex
        assert!(ensure_public_cid(&"q".repeat(64)).is_err()); // non-hex
    }

    #[tokio::test]
    async fn set_emoji_name_impl_writes_and_clears_public_only() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("pkarr_settings.json");
        let state = std::sync::Mutex::new(NodeState {
            pkarr_settings_path: Some(settings.clone()),
            ..NodeState::default()
        });
        let public_hex = hex::encode([0x42u8; 32]);

        // Set a name.
        set_emoji_name_impl(
            &state,
            public_hex.clone(),
            Some("catjam".to_string()),
            "image/png".to_string(),
            200,
            1_000,
        )
        .await
        .expect("set must succeed for a public cid");

        // list reflects it.
        let listed = list_emoji_names_impl(&state).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].cid, public_hex);
        assert_eq!(listed[0].name, "catjam");
        assert_eq!(listed[0].mime, "image/png");
        assert_eq!(listed[0].size, 200);

        // Clear it (name = None).
        set_emoji_name_impl(&state, public_hex.clone(), None, String::new(), 0, 2_000)
            .await
            .expect("clear must succeed");
        assert!(list_emoji_names_impl(&state).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn set_emoji_name_impl_rejects_encrypted_and_bad_name() {
        let dir = tempfile::tempdir().unwrap();
        let state = std::sync::Mutex::new(NodeState {
            pkarr_settings_path: Some(dir.path().join("pkarr_settings.json")),
            ..NodeState::default()
        });
        let encrypted_hex = hex::encode([0xB2u8; 32]);
        let public_hex = hex::encode([0x42u8; 32]);

        let err = set_emoji_name_impl(
            &state,
            encrypted_hex,
            Some("nope".to_string()),
            "image/png".to_string(),
            1,
            1,
        )
        .await
        .unwrap_err();
        assert!(err.contains("encrypted"), "got: {err}");

        let err = set_emoji_name_impl(
            &state,
            public_hex,
            Some("has space".to_string()),
            "image/png".to_string(),
            1,
            1,
        )
        .await
        .unwrap_err();
        assert!(err.contains("name"), "got: {err}");
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(set_emoji_name_impl) + test(ensure_public_cid)'`
Expected: FAIL to compile (`ensure_public_cid`, `set_emoji_name_impl`, `list_emoji_names_impl`, `EmojiNameDto` not defined).

- [ ] **Step 3: Add the lock, the DTO, the helper, the impls, and the commands**

Near `NICKNAME_WRITE_LOCK` (`lib.rs:45255`), add:

```rust
/// Serializes the emoji-names file read-modify-write so two concurrent
/// `set_emoji_name` calls can't lose an update (parity with `NICKNAME_WRITE_LOCK`).
static EMOJI_NAMES_WRITE_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> =
    std::sync::OnceLock::new();
```

Add the IPC DTO (place near `ReactionEmojiInput` at `lib.rs:20203`, or with the other emoji code):

```rust
/// IPC projection of one named emoji for the picker. Serializes camelCase.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EmojiNameDto {
    pub cid: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
}
```

Add the public-CID validator + the two impls + the commands (place the impls + commands near the other emoji IPCs around `lib.rs:20139`, the commands among the `#[tauri::command]` cluster):

```rust
/// Validate that `cid` is a well-formed 64-hex ContentId whose `encrypted` flag
/// is UNSET. Naming is public-only: an encrypted emoji can't render outside its
/// origin community, so it can't be a globally reusable named emoji.
pub(crate) fn ensure_public_cid(cid: &str) -> Result<(), String> {
    use harmony_content::cid::ContentId;
    if cid.len() != 64 {
        return Err("invalid cid hex".to_string());
    }
    let bytes: [u8; 32] = hex::decode(cid)
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .ok_or_else(|| "invalid cid hex".to_string())?;
    if ContentId::from_bytes(bytes).flags().encrypted {
        return Err("encrypted emoji can't be named — they can't be reused outside their community".to_string());
    }
    Ok(())
}

/// Resolve the `emoji_names.json` path from `NodeState.pkarr_settings_path`,
/// mirroring how the nickname store co-locates beside `pkarr_settings.json`.
fn emoji_names_path(state: &std::sync::Mutex<NodeState>) -> Result<std::path::PathBuf, String> {
    let g = state.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
    let p = g
        .pkarr_settings_path
        .clone()
        .ok_or_else(|| OWNER_NOT_LOADED_MSG.to_string())?;
    Ok(p.with_file_name("emoji_names.json"))
}

pub(crate) async fn set_emoji_name_impl(
    state: &std::sync::Mutex<NodeState>,
    cid: String,
    name: Option<String>,
    mime: String,
    size: u64,
    now_ms: u64,
) -> Result<(), String> {
    // Public-only gate (also rejects malformed cid) BEFORE any write.
    ensure_public_cid(&cid)?;
    // Normalize: trim, blank → clear. Validate a real name's charset/length.
    let trimmed = name.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(n) = trimmed {
        if !crate::emoji_names::valid_emoji_name(n) {
            return Err(format!(
                "invalid emoji name (use 1–{} of A–Z, a–z, 0–9, _ or -)",
                crate::emoji_names::MAX_EMOJI_NAME_LEN
            ));
        }
    }
    let path = emoji_names_path(state)?;
    {
        let _guard = EMOJI_NAMES_WRITE_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let mut store = crate::emoji_names::EmojiNames::load_or_default(&path);
        store.set(&cid, trimmed, &mime, size, now_ms);
        store.save(&path)?;
    }
    // Task 4 wires the best-effort retain-on-name pin here (only when setting).
    Ok(())
}

pub(crate) async fn list_emoji_names_impl(
    state: &std::sync::Mutex<NodeState>,
) -> Result<Vec<EmojiNameDto>, String> {
    let path = emoji_names_path(state)?;
    let store = crate::emoji_names::EmojiNames::load_or_default(&path);
    Ok(store
        .entries
        .iter()
        .map(|(cid, e)| EmojiNameDto {
            cid: cid.clone(),
            name: e.name.clone(),
            mime: e.mime.clone(),
            size: e.size,
        })
        .collect())
}

/// Set (or clear, with `name = null`) the LOCAL-ONLY personal name for a PUBLIC
/// custom emoji. `mime`/`size` are the caller's known descriptor (a chip's
/// `emojiSize` or the upload's returned size); ignored on clear. Emits
/// `emoji-names-changed` so subscribed UIs re-fetch.
#[tauri::command]
async fn set_emoji_name(
    app: tauri::AppHandle,
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    cid: String,
    name: Option<String>,
    mime: String,
    size: u64,
) -> Result<(), String> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    set_emoji_name_impl(state_lock.inner(), cid, name, mime, size, now_ms).await?;
    let _ = app.emit("emoji-names-changed", ());
    Ok(())
}

#[tauri::command]
async fn list_emoji_names(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
) -> Result<Vec<EmojiNameDto>, String> {
    list_emoji_names_impl(state_lock.inner()).await
}
```

Note: confirm `use tauri::Emitter;` (or the equivalent providing `app.emit`) is already in scope — `set_friend_nickname` uses `app.emit("friend-list-changed", ())` in the same file, so it is. `OWNER_NOT_LOADED_MSG` is the existing constant used by `set_friend_nickname`.

- [ ] **Step 4: Run to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(set_emoji_name_impl) + test(ensure_public_cid)'`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(emoji): set_emoji_name + list_emoji_names IPCs (public-only)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: `preview_named_emoji` IPC (public-only, non-channel-scoped)

**Files:**
- Modify: `src-tauri/src/lib.rs` (add `preview_named_emoji_impl` + command)

Mirrors `fetch_avatar` (`lib.rs:15857`): a bare CAS fetch by CID via `FetchRequest`, with NO channel/log authorization (the named emoji may not appear in the current channel). Adds a public-only guard and the `MAX_CUSTOM_EMOJI_BYTES` cap. Returns plaintext bytes (public = not encrypted, no decrypt).

- [ ] **Step 1: Write the failing test**

Add alongside `preview_reaction_emoji_happy_path_returns_plaintext` (it shows the in-memory CAS ingest/fetch drainer pattern to copy):

```rust
    #[tokio::test]
    async fn preview_named_emoji_fetches_public_bytes_no_channel_scope() {
        // In-memory CAS: ingest stores (cid_hex -> bytes); fetch returns them.
        let cas: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>> =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let (ingest_tx, mut ingest_rx) =
            tokio::sync::mpsc::channel::<event_loop::IngestRequest>(16);
        let (fetch_tx, mut fetch_rx) = tokio::sync::mpsc::channel::<event_loop::FetchRequest>(16);
        let cas_i = std::sync::Arc::clone(&cas);
        let ingest_drainer = tokio::spawn(async move {
            while let Some(req) = ingest_rx.recv().await {
                cas_i.lock().unwrap().insert(req.cid_hex, req.data);
                let _ = req.reply.send(Ok(()));
            }
        });
        let cas_f = std::sync::Arc::clone(&cas);
        let fetch_drainer = tokio::spawn(async move {
            while let Some(req) = fetch_rx.recv().await {
                let got = cas_f.lock().unwrap().get(&req.cid_hex).cloned();
                let _ = req.reply.send(got.ok_or_else(|| "not found".to_string()));
            }
        });

        let state = std::sync::Mutex::new(NodeState {
            ingest_tx: Some(ingest_tx),
            fetch_tx: Some(fetch_tx),
            ..NodeState::default()
        });

        // Ingest PUBLIC bytes (encrypt = false) → a hash(plaintext) CID.
        let plaintext: Vec<u8> = (0u8..200).collect();
        let dto = ingest_channel_artifact_bytes_inner(
            &state,
            crate::owner_state_types::SpaceId([0u8; 16]),
            plaintext.clone(),
            String::new(),
            "image/png".to_string(),
            false,
        )
        .await
        .expect("public ingest");
        assert!(!dto.encrypted);

        // Preview by CID — no community/channel, no React in any channel.
        let bytes = preview_named_emoji_impl(&state, dto.cid.clone())
            .await
            .expect("preview must fetch the public bytes");
        assert_eq!(bytes, plaintext);

        // Encrypted CID is rejected up front (public-only).
        let err = preview_named_emoji_impl(&state, hex::encode([0xB2u8; 32]))
            .await
            .unwrap_err();
        assert!(err.contains("encrypted"), "got: {err}");

        drop(state);
        ingest_drainer.abort();
        fetch_drainer.abort();
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(preview_named_emoji)'`
Expected: FAIL to compile (`preview_named_emoji_impl` not defined).

- [ ] **Step 3: Add the impl + command**

Place near `preview_reaction_emoji` (`lib.rs:20146` / impl `:21195`):

```rust
pub(crate) async fn preview_named_emoji_impl(
    state: &std::sync::Mutex<NodeState>,
    cid: String,
) -> Result<Vec<u8>, String> {
    // Public-only (also validates the hex). Named emoji are always public, so a
    // non-channel-scoped fetch-by-CID is legitimate and never decrypts.
    ensure_public_cid(&cid)?;
    let fetch_tx = {
        let g = state.lock().map_err(|e| format!("lock: {e}"))?;
        g.fetch_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    fetch_tx
        .send(event_loop::FetchRequest {
            cid_hex: cid,
            reply: reply_tx,
            max_bytes: Some(MAX_CUSTOM_EMOJI_BYTES as usize),
            serveable: false,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "event loop dropped fetch request".to_string())?
}

/// Fetch a NAMED (public) custom emoji's plaintext bytes by CID with NO channel
/// scope — for the picker, where the emoji may not appear in the current channel.
/// Public-only + capped at `MAX_CUSTOM_EMOJI_BYTES`. Mirrors `fetch_avatar`.
#[tauri::command]
async fn preview_named_emoji(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    cid: String,
) -> Result<Vec<u8>, String> {
    preview_named_emoji_impl(state_lock.inner(), cid).await
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(preview_named_emoji)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(emoji): preview_named_emoji IPC (public-only, no channel scope)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: retain-on-name best-effort pin

**Files:**
- Modify: `src-tauri/src/lib.rs` (add `pin_public_emoji_best_effort`, call it from `set_emoji_name_impl`)

Naming an emoji should keep its bytes locally so the picker can render it later and the node can re-serve it. Best-effort: fetch the public bytes, re-ingest them serveable (same CID, content-addressed). Any failure is logged and swallowed — the name is still saved (the picker preview degrades to on-demand fetch / fallback icon).

- [ ] **Step 1: Write the failing test**

Add alongside the Task 3 test (reuses the same ingest/fetch drainer pattern):

```rust
    #[tokio::test]
    async fn pin_public_emoji_stores_bytes_locally() {
        let cas: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>> =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let (ingest_tx, mut ingest_rx) =
            tokio::sync::mpsc::channel::<event_loop::IngestRequest>(16);
        let (fetch_tx, mut fetch_rx) = tokio::sync::mpsc::channel::<event_loop::FetchRequest>(16);
        let cas_i = std::sync::Arc::clone(&cas);
        let ingest_drainer = tokio::spawn(async move {
            while let Some(req) = ingest_rx.recv().await {
                cas_i.lock().unwrap().insert(req.cid_hex, req.data);
                let _ = req.reply.send(Ok(()));
            }
        });
        let cas_f = std::sync::Arc::clone(&cas);
        let fetch_drainer = tokio::spawn(async move {
            while let Some(req) = fetch_rx.recv().await {
                let got = cas_f.lock().unwrap().get(&req.cid_hex).cloned();
                let _ = req.reply.send(got.ok_or_else(|| "not found".to_string()));
            }
        });
        let state = std::sync::Mutex::new(NodeState {
            ingest_tx: Some(ingest_tx),
            fetch_tx: Some(fetch_tx),
            ..NodeState::default()
        });

        // Seed CAS with a public emoji, then clear local copy to prove pin re-stores.
        let plaintext: Vec<u8> = (0u8..200).collect();
        let dto = ingest_channel_artifact_bytes_inner(
            &state,
            crate::owner_state_types::SpaceId([0u8; 16]),
            plaintext.clone(),
            String::new(),
            "image/png".to_string(),
            false,
        )
        .await
        .expect("seed ingest");
        cas.lock().unwrap().clear(); // simulate bytes only reachable via fetch peer
        // Re-seed under the same cid so the fetch leg can succeed (peer has it).
        cas.lock().unwrap().insert(dto.cid.clone(), plaintext.clone());

        pin_public_emoji_best_effort(&state, &dto.cid).await;
        // The pin re-ingested the bytes → present in local CAS under the same cid.
        assert!(cas.lock().unwrap().contains_key(&dto.cid));

        drop(state);
        ingest_drainer.abort();
        fetch_drainer.abort();
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(pin_public_emoji)'`
Expected: FAIL to compile (`pin_public_emoji_best_effort` not defined).

- [ ] **Step 3: Implement the pin + wire it into `set_emoji_name_impl`**

Add near `preview_named_emoji_impl`:

```rust
/// Best-effort: fetch a public emoji's bytes and re-ingest them serveable so the
/// named emoji survives locally (picker render + re-serve). Idempotent (content-
/// addressed). Any failure is logged, NOT propagated — naming must never block on
/// a transient fetch hiccup.
async fn pin_public_emoji_best_effort(state: &std::sync::Mutex<NodeState>, cid: &str) {
    match preview_named_emoji_impl(state, cid.to_string()).await {
        Ok(bytes) => {
            // Re-ingest public + serveable; the root CID is hash(plaintext) and
            // must equal the requested cid.
            match ingest_channel_artifact_bytes_inner(
                state,
                crate::owner_state_types::SpaceId([0u8; 16]),
                bytes,
                String::new(),
                "image/png".to_string(),
                false,
            )
            .await
            {
                Ok(dto) if dto.cid == cid => {}
                Ok(dto) => tracing::warn!(
                    requested = %cid, got = %dto.cid,
                    "pin_public_emoji: re-ingest CID mismatch; name saved without local pin"
                ),
                Err(e) => tracing::warn!(error = %e, cid = %cid, "pin_public_emoji: re-ingest failed"),
            }
        }
        Err(e) => tracing::warn!(error = %e, cid = %cid, "pin_public_emoji: fetch failed; name saved without local pin"),
    }
}
```

In `set_emoji_name_impl`, replace the `// Task 4 wires ...` comment line with a pin call that runs only when a name was set:

```rust
    if trimmed.is_some() {
        pin_public_emoji_best_effort(state, &cid).await;
    }
    Ok(())
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(pin_public_emoji) + test(set_emoji_name_impl)'`
Expected: PASS (the `set_emoji_name_impl` tests still pass — they have no `fetch_tx`/`ingest_tx`, so the pin's fetch leg fails and is swallowed; the name is still written, which is what those tests assert).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(emoji): retain-on-name best-effort pin

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: register the three commands + pin IPC arg casing

**Files:**
- Modify: `src-tauri/src/lib.rs` (`generate_handler!` at `:48100`)
- Modify: `src-tauri/tests/ipc_arg_casing.rs`

- [ ] **Step 1: Write the failing casing test**

Append to `src-tauri/tests/ipc_arg_casing.rs` (mirror `set_friend_nickname_takes_camelcase_args_via_plain_command` at line 74; it reads `src/lib.rs` as text and asserts the signature shape):

```rust
#[test]
fn emoji_name_ipcs_use_plain_commands_with_camelcase_args() {
    let src = std::fs::read_to_string("src/lib.rs").expect("read src/lib.rs");
    assert!(
        src.contains("async fn set_emoji_name("),
        "set_emoji_name IPC not found in src/lib.rs",
    );
    // snake_case Rust params map from camelCase JS keys (cid/name/mime/size are
    // single words; Option<String> ↔ string | null).
    assert!(
        src.contains("cid: String,")
            && src.contains("name: Option<String>,")
            && src.contains("mime: String,")
            && src.contains("size: u64,"),
        "set_emoji_name must declare cid/name/mime/size params",
    );
    assert!(
        src.contains("async fn list_emoji_names("),
        "list_emoji_names IPC not found",
    );
    assert!(
        src.contains("async fn preview_named_emoji("),
        "preview_named_emoji IPC not found",
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(emoji_name_ipcs_use_plain_commands)'`
Expected: PASS already for the `async fn` asserts (added in Tasks 2–3) — but the registration is not yet done, so proceed to wire it and rely on the build + a runtime smoke. (If any assert fails, the command decls from Tasks 2–3 are missing.)

- [ ] **Step 3: Register the commands**

In the `tauri::generate_handler![` block at `lib.rs:48100`, find the channel-artifact cluster (it contains `preview_reaction_emoji,` and `ingest_channel_artifact_bytes,`) and add three lines:

```rust
            preview_reaction_emoji,
            preview_named_emoji,
            ingest_channel_artifact,
            ingest_channel_artifact_bytes,
            set_emoji_name,
            list_emoji_names,
```

Then check whether the second `generate_handler!` at `lib.rs:48366` is a live builder (`grep -n "generate_handler" src/lib.rs`); if it registers the same user-facing commands (i.e. contains `set_friend_nickname`), add the same three identifiers there too. If it is a test-only/secondary builder that does not list `set_friend_nickname`, leave it.

- [ ] **Step 4: Verify the whole crate builds + the casing test passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(emoji_name_ipcs_use_plain_commands)' && cargo build --locked`
Expected: test PASS; build succeeds (registration compiles — a typo'd command name fails the `generate_handler!` macro here).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/ipc_arg_casing.rs
git commit -m "feat(emoji): register emoji-name IPCs + pin arg casing

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: `ReactionDto.encrypted` (so the UI can hide naming on encrypted chips)

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs` (`ReactionDto` `:851`, `reactions_for` `:964`)

- [ ] **Step 1: Write the failing test**

Add to the test module in `community_channel_log.rs` (search for an existing `reactions_for` test, e.g. `grep -n "fn .*reactions_for\|reactions_for(" src/community_channel_log.rs` and place it nearby). This builds the reaction index directly; mirror the construction style of the nearest existing reaction test:

```rust
    #[test]
    fn reactions_for_surfaces_encrypted_flag_for_custom_emoji() {
        use harmony_content::cid::ContentId;
        // A public custom emoji (encrypted flag unset) and an encrypted one.
        let public_cid = [0x42u8; 32];
        let encrypted_cid = [0xB2u8; 32];
        assert!(!ContentId::from_bytes(public_cid).flags().encrypted);
        assert!(ContentId::from_bytes(encrypted_cid).flags().encrypted);

        let mut idx = ReactionIndex::default();
        let me = OwnerAddr([1u8; 32]);
        let target = MessageId([9u8; 32]);
        // Record one public-custom and one encrypted-custom reaction by `me`.
        // (Use the same recording entry point the neighboring tests use; both
        // carry a ChannelAttachment descriptor with the respective cid.)
        record_custom_reaction(&mut idx, &target, &me, public_cid, 200);
        record_custom_reaction(&mut idx, &target, &me, encrypted_cid, 200);

        let dtos = idx.reactions_for(&target, &me);
        let pub_dto = dtos
            .iter()
            .find(|d| d.emoji_cid.as_deref() == Some(hex::encode(public_cid).as_str()))
            .expect("public reaction present");
        let enc_dto = dtos
            .iter()
            .find(|d| d.emoji_cid.as_deref() == Some(hex::encode(encrypted_cid).as_str()))
            .expect("encrypted reaction present");
        assert_eq!(pub_dto.encrypted, Some(false));
        assert_eq!(enc_dto.encrypted, Some(true));
    }
```

If there is no existing `record_custom_reaction` test helper, inline the same recording call the nearest existing custom-emoji reaction test uses (search `grep -n "descriptor: Some\|ChannelAttachment {" src/community_channel_log.rs` to find how a custom reaction is recorded in tests) — record the reaction with a `ChannelAttachment { cid: public_cid, mime: "image/png".into(), name: String::new(), size: 200 }` (match the real `ChannelAttachment` field set) for each cid, then call `reactions_for`.

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(reactions_for_surfaces_encrypted_flag)'`
Expected: FAIL (`ReactionDto` has no field `encrypted`).

- [ ] **Step 3: Add the field + populate it**

In `ReactionDto` (`community_channel_log.rs:851`), add after `emoji_size`:

```rust
    /// True/false for a custom (CAS-backed) emoji: whether its CID is encrypted.
    /// `None` for unicode reactions. Serializes as `encrypted`. Lets the UI hide
    /// the "name this emoji" affordance on encrypted chips (naming is public-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted: Option<bool>,
```

In `reactions_for` (`:979`), change the descriptor match + struct literal:

```rust
            let (emoji, emoji_cid, emoji_size, encrypted) = match &state.descriptor {
                Some(att) => (
                    String::new(),
                    Some(hex::encode(att.cid)),
                    Some(att.size),
                    Some(harmony_content::cid::ContentId::from_bytes(att.cid).flags().encrypted),
                ),
                None => (key.clone(), None, None, None),
            };
            out.push(ReactionDto {
                emoji,
                count: present.len() as u32,
                mine: present.contains(&me),
                reactors: present.iter().map(|a| hex::encode(a.0)).collect(),
                emoji_cid,
                emoji_size,
                encrypted,
            });
```

Then fix any other `ReactionDto { .. }` literal the compiler flags (grep `ReactionDto {` — e.g. a `..Default::default()`-free construction in tests) by adding `encrypted: None,` for unicode cases.

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(reactions_for)'`
Expected: PASS (new test + existing `reactions_for` tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_channel_log.rs
git commit -m "feat(emoji): expose encrypted flag on reaction summary DTO

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 7: frontend `ChannelMessageService` emoji-name methods + event

**Files:**
- Modify: `src/lib/channel-message-service.ts`
- Test: `src/lib/__tests__/channel-message-service.test.ts`

- [ ] **Step 1: Write the failing tests**

Add to `src/lib/__tests__/channel-message-service.test.ts` (mirror the existing `ingestEmojiBytes`/`reactToMessage` tests there — they construct a service with a mock adapter whose `invoke` records calls):

```typescript
  it('setEmojiName invokes set_emoji_name with cid/name/mime/size', async () => {
    const calls: Array<{ cmd: string; args: any }> = [];
    const adapter = makeMockAdapter((cmd, args) => {
      calls.push({ cmd, args });
      return undefined;
    });
    const svc = new ChannelMessageService();
    await svc.connectAdapter(adapter);
    await svc.setEmojiName('aa', 'catjam', 'image/png', 200);
    expect(calls.at(-1)).toEqual({
      cmd: 'set_emoji_name',
      args: { cid: 'aa', name: 'catjam', mime: 'image/png', size: 200 },
    });
    // Clearing passes name: null.
    await svc.setEmojiName('aa', null, 'image/png', 200);
    expect(calls.at(-1)!.args.name).toBeNull();
  });

  it('listEmojiNames returns the DTO array from the IPC', async () => {
    const adapter = makeMockAdapter((cmd) =>
      cmd === 'list_emoji_names'
        ? [{ cid: 'aa', name: 'catjam', mime: 'image/png', size: 200 }]
        : undefined,
    );
    const svc = new ChannelMessageService();
    await svc.connectAdapter(adapter);
    const list = await svc.listEmojiNames();
    expect(list).toEqual([{ cid: 'aa', name: 'catjam', mime: 'image/png', size: 200 }]);
  });

  it('previewNamedEmoji invokes preview_named_emoji and returns Uint8Array', async () => {
    const adapter = makeMockAdapter((cmd, args) =>
      cmd === 'preview_named_emoji' && args.cid === 'aa' ? [1, 2, 3] : undefined,
    );
    const svc = new ChannelMessageService();
    await svc.connectAdapter(adapter);
    const bytes = await svc.previewNamedEmoji('aa');
    expect(Array.from(bytes)).toEqual([1, 2, 3]);
  });
```

Use the same mock-adapter helper the existing tests in this file use (search the file for how `connectAdapter` is given a mock and how `invoke`/`listen` are stubbed; reuse that helper rather than introducing a new one). If the file's existing adapter mock doesn't capture `(cmd, args)`, extend it minimally in the same shape the other tests already rely on.

- [ ] **Step 2: Run to verify they fail**

Run (repo root): `npx vitest run src/lib/__tests__/channel-message-service.test.ts`
Expected: FAIL (`setEmojiName`/`listEmojiNames`/`previewNamedEmoji` are not functions).

- [ ] **Step 3: Add the DTO type, the methods, and the event subscription**

In `src/lib/channel-message-service.ts`:

Add the DTO near `ChannelAttachmentDto` (`:56`):

```typescript
export interface EmojiNameDto {
  cid: string;
  name: string;
  mime: string;
  size: number;
}
```

Add `encrypted?: boolean` to the reaction summary (the `reactions?:` array at `:38-45`) after `emojiSize?: number;`:

```typescript
    /** ZEB — present iff custom: whether the emoji CID is encrypted. The UI
     *  hides the "name this emoji" affordance on encrypted chips. */
    encrypted?: boolean;
```

and to `ChannelReactionReceivedPayload` (`:83-98`) after `emojiSize?: number;` (so a live-arriving custom reaction can carry it too; harmless if the backend omits it):

```typescript
  encrypted?: boolean;
```

Add the three methods next to `previewReactionEmoji` (`:601`), inside the class:

```typescript
  /**
   * Set (or clear, with `null`) the LOCAL-ONLY personal name for a PUBLIC custom
   * emoji. `mime`/`size` are the descriptor already known to the caller (a chip's
   * `emojiSize` or an upload's returned size). The backend emits
   * `emoji-names-changed`, so subscribed UIs re-fetch via `listEmojiNames`.
   */
  async setEmojiName(cid: string, name: string | null, mime: string, size: number): Promise<void> {
    if (!this.adapter) throw new Error('ChannelMessageService.setEmojiName: adapter not connected');
    try {
      await this.adapter.invoke('set_emoji_name', { cid, name, mime, size });
    } catch (e: unknown) {
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }

  /** List the user's named emoji for the picker. */
  async listEmojiNames(): Promise<EmojiNameDto[]> {
    if (!this.adapter) throw new Error('ChannelMessageService.listEmojiNames: adapter not connected');
    try {
      return (await this.adapter.invoke('list_emoji_names', {})) as EmojiNameDto[];
    } catch (e: unknown) {
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }

  /**
   * Fetch a NAMED (public) emoji's bytes by CID with no channel scope, for the
   * picker thumbnail. Mirrors {@link previewReactionEmoji} but uses the
   * public-only `preview_named_emoji` IPC.
   */
  async previewNamedEmoji(cid: string): Promise<Uint8Array> {
    if (!this.adapter) throw new Error('ChannelMessageService.previewNamedEmoji: adapter not connected');
    try {
      const bytes = (await this.adapter.invoke('preview_named_emoji', { cid })) as number[];
      return new Uint8Array(bytes);
    } catch (e: unknown) {
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }
```

Add a listener + subscription mirroring `FriendService.onFriendsChanged` (`friend-service.ts:143-197`). In `connectAdapter` (find where this service registers `channel-reaction-received` via `adapter.listen`), add a sibling registration and a listener Set field:

```typescript
  // (class field, near the other listener Sets)
  private emojiNamesChangedListeners = new Set<() => void>();

  // (inside connectAdapter, alongside the other adapter.listen calls)
  const unlistenEmojiNames = await adapter.listen('emoji-names-changed', () => {
    for (const cb of [...this.emojiNamesChangedListeners]) cb();
  });
  this.unlisteners.push(unlistenEmojiNames); // use the file's existing unlisten registry

  // (public method, near the other on*Changed/subscribe methods)
  onEmojiNamesChanged(cb: () => void): () => void {
    this.emojiNamesChangedListeners.add(cb);
    return () => {
      this.emojiNamesChangedListeners.delete(cb);
    };
  }
```

Match the exact field/registry names this file already uses for its existing listeners (it may name the unlisten array differently than `unlisteners`); place the `clear()` of `emojiNamesChangedListeners` in the existing `destroy()`.

- [ ] **Step 4: Run to verify they pass**

Run (repo root): `npx vitest run src/lib/__tests__/channel-message-service.test.ts`
Expected: PASS.

- [ ] **Step 5: Type-check + commit**

Run: `npx tsc --noEmit` (expect clean), then:

```bash
git add src/lib/channel-message-service.ts src/lib/__tests__/channel-message-service.test.ts
git commit -m "feat(emoji): emoji-name service methods + change event

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 8: `ReactionEmojiImage` named-preview mode

**Files:**
- Modify: `src/lib/components/ReactionEmojiImage.svelte`
- Test: `src/lib/components/__tests__/ReactionEmojiImage.test.ts`

Make `communityId`/`channelId` optional; when both are absent, fetch via `previewNamedEmoji(cid)` (the picker has no channel context). All blob-lifecycle / decode-bomb logic is unchanged.

- [ ] **Step 1: Write the failing test**

Add to `src/lib/components/__tests__/ReactionEmojiImage.test.ts` (mirror an existing render test there; it stubs `channelMessageService.previewReactionEmoji`):

```typescript
  it('uses previewNamedEmoji when communityId/channelId are absent', async () => {
    const previewNamedEmoji = vi.fn().mockResolvedValue(new Uint8Array([/* PNG header */ 0x89, 0x50, 0x4e, 0x47]));
    const previewReactionEmoji = vi.fn();
    const svc = { previewNamedEmoji, previewReactionEmoji } as any;
    render(ReactionEmojiImage, { props: { cid: 'aa', channelMessageService: svc } });
    await waitFor(() => expect(previewNamedEmoji).toHaveBeenCalledWith('aa'));
    expect(previewReactionEmoji).not.toHaveBeenCalled();
  });
```

(Use the file's existing `render`/`waitFor` imports and its existing PNG-bytes fixture helper if present; the header guard rejects non-image bytes, so provide a minimally valid PNG header as the other tests do.)

- [ ] **Step 2: Run to verify it fails**

Run (repo root): `npx vitest run src/lib/components/__tests__/ReactionEmojiImage.test.ts`
Expected: FAIL (named path not wired; `previewNamedEmoji` never called).

- [ ] **Step 3: Make community/channel optional + branch the fetch**

In `ReactionEmojiImage.svelte`, change the props block (`:12-17`) to make the two ids optional:

```svelte
  let { communityId, channelId, cid, channelMessageService }: {
    communityId?: string;
    channelId?: string;
    cid: string;
    channelMessageService: ChannelMessageService;
  } = $props();
```

In `load` (`:40-42`), branch the fetch:

```svelte
      const bytes = communityId && channelId
        ? await channelMessageService.previewReactionEmoji(communityId, channelId, forCid)
        : await channelMessageService.previewNamedEmoji(forCid);
```

- [ ] **Step 4: Run to verify it passes**

Run (repo root): `npx vitest run src/lib/components/__tests__/ReactionEmojiImage.test.ts`
Expected: PASS (new + existing tests; existing tests still pass `communityId`/`channelId`).

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/ReactionEmojiImage.svelte src/lib/components/__tests__/ReactionEmojiImage.test.ts
git commit -m "feat(emoji): ReactionEmojiImage named-preview mode (no channel)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 9: `NamedEmojiPicker` popover component

**Files:**
- Create: `src/lib/components/NamedEmojiPicker.svelte`
- Test: `src/lib/components/__tests__/NamedEmojiPicker.test.ts`

A self-contained popover: loads named emoji via `listEmojiNames`, filters by a search box, renders each as a `ReactionEmojiImage` (named mode) tile, emits a `pick` callback with `{cid, mime, size}`, and offers rename/remove via `setEmojiName`. Subscribes to `onEmojiNamesChanged` to refresh.

- [ ] **Step 1: Write the failing test**

Create `src/lib/components/__tests__/NamedEmojiPicker.test.ts`:

```typescript
import { render, waitFor, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import NamedEmojiPicker from '../NamedEmojiPicker.svelte';

function svcWith(names: Array<{ cid: string; name: string; mime: string; size: number }>) {
  return {
    listEmojiNames: vi.fn().mockResolvedValue(names),
    previewNamedEmoji: vi.fn().mockResolvedValue(new Uint8Array([0x89, 0x50, 0x4e, 0x47])),
    setEmojiName: vi.fn().mockResolvedValue(undefined),
    onEmojiNamesChanged: vi.fn().mockReturnValue(() => {}),
  } as any;
}

describe('NamedEmojiPicker', () => {
  it('lists named emoji and filters by search', async () => {
    const svc = svcWith([
      { cid: 'aa', name: 'catjam', mime: 'image/png', size: 1 },
      { cid: 'bb', name: 'shipit', mime: 'image/png', size: 1 },
    ]);
    const { getByLabelText, queryByTitle } = render(NamedEmojiPicker, {
      props: { channelMessageService: svc, onpick: vi.fn() },
    });
    await waitFor(() => expect(queryByTitle('catjam')).toBeTruthy());
    expect(queryByTitle('shipit')).toBeTruthy();
    await fireEvent.input(getByLabelText('Search named emoji'), { target: { value: 'cat' } });
    expect(queryByTitle('catjam')).toBeTruthy();
    expect(queryByTitle('shipit')).toBeNull();
  });

  it('calls onpick with the descriptor when a tile is clicked', async () => {
    const onpick = vi.fn();
    const svc = svcWith([{ cid: 'aa', name: 'catjam', mime: 'image/png', size: 7 }]);
    const { getByTitle } = render(NamedEmojiPicker, {
      props: { channelMessageService: svc, onpick },
    });
    await waitFor(() => expect(getByTitle('catjam')).toBeTruthy());
    await fireEvent.click(getByTitle('catjam'));
    expect(onpick).toHaveBeenCalledWith({ cid: 'aa', mime: 'image/png', size: 7 });
  });

  it('remove calls setEmojiName(cid, null, ...)', async () => {
    const svc = svcWith([{ cid: 'aa', name: 'catjam', mime: 'image/png', size: 7 }]);
    const { getByLabelText, getByTitle } = render(NamedEmojiPicker, {
      props: { channelMessageService: svc, onpick: vi.fn() },
    });
    await waitFor(() => expect(getByTitle('catjam')).toBeTruthy());
    await fireEvent.click(getByLabelText('Remove name catjam'));
    expect(svc.setEmojiName).toHaveBeenCalledWith('aa', null, 'image/png', 7);
  });
});
```

- [ ] **Step 2: Run to verify it fails**

Run (repo root): `npx vitest run src/lib/components/__tests__/NamedEmojiPicker.test.ts`
Expected: FAIL (component does not exist).

- [ ] **Step 3: Implement the component**

Create `src/lib/components/NamedEmojiPicker.svelte`:

```svelte
<script lang="ts">
  import type { ChannelMessageService, EmojiNameDto } from '../channel-message-service';
  import ReactionEmojiImage from './ReactionEmojiImage.svelte';

  let { channelMessageService, onpick }: {
    channelMessageService: ChannelMessageService;
    onpick: (descriptor: { cid: string; mime: string; size: number }) => void;
  } = $props();

  let all = $state<EmojiNameDto[]>([]);
  let query = $state('');
  let error = $state<string | null>(null);
  let renaming = $state<string | null>(null); // cid being renamed
  let renameValue = $state('');

  const filtered = $derived(
    query.trim()
      ? all.filter((e) => e.name.toLowerCase().includes(query.trim().toLowerCase()))
      : all,
  );

  async function refresh() {
    try {
      all = await channelMessageService.listEmojiNames();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  $effect(() => {
    void refresh();
    // Re-fetch when any device/path changes the names.
    return channelMessageService.onEmojiNamesChanged(() => void refresh());
  });

  function pick(e: EmojiNameDto): void {
    onpick({ cid: e.cid, mime: e.mime, size: e.size });
  }

  function startRename(e: EmojiNameDto): void {
    renaming = e.cid;
    renameValue = e.name;
  }

  async function commitRename(e: EmojiNameDto): Promise<void> {
    const name = renameValue.trim();
    renaming = null;
    if (!name || name === e.name) return;
    try {
      await channelMessageService.setEmojiName(e.cid, name, e.mime, e.size);
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    }
  }

  async function remove(e: EmojiNameDto): Promise<void> {
    try {
      await channelMessageService.setEmojiName(e.cid, null, e.mime, e.size);
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    }
  }
</script>

<div class="named-emoji-picker" role="menu" aria-label="React with a named emoji">
  <input
    class="named-search"
    type="text"
    placeholder="Search named emoji…"
    aria-label="Search named emoji"
    bind:value={query}
  />
  {#if error}
    <p class="named-error" role="alert">{error}</p>
  {/if}
  <div class="named-grid">
    {#each filtered as e (e.cid)}
      <div class="named-tile" title={e.name}>
        {#if renaming === e.cid}
          <input
            class="named-rename"
            type="text"
            aria-label={`Rename ${e.name}`}
            bind:value={renameValue}
            onkeydown={(ev) => ev.key === 'Enter' && commitRename(e)}
            onblur={() => commitRename(e)}
          />
        {:else}
          <button type="button" class="named-pick" role="menuitem" onclick={() => pick(e)}>
            <ReactionEmojiImage cid={e.cid} {channelMessageService} />
            <span class="named-label">{e.name}</span>
          </button>
          <button type="button" class="named-act" aria-label={`Rename ${e.name}`} onclick={() => startRename(e)}>✎</button>
          <button type="button" class="named-act" aria-label={`Remove name ${e.name}`} onclick={() => remove(e)}>🗑</button>
        {/if}
      </div>
    {/each}
    {#if filtered.length === 0}
      <p class="named-empty">No named emoji yet. Name one from a reaction or at upload.</p>
    {/if}
  </div>
</div>

<style>
  .named-emoji-picker {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    padding: 0.5rem;
    min-width: 14rem;
    max-width: 18rem;
  }
  .named-search,
  .named-rename {
    width: 100%;
    box-sizing: border-box;
    padding: 0.25rem 0.4rem;
  }
  .named-grid {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem;
  }
  .named-tile {
    display: inline-flex;
    align-items: center;
    gap: 0.15rem;
  }
  .named-pick {
    display: inline-flex;
    align-items: center;
    gap: 0.25rem;
    cursor: pointer;
  }
  .named-label {
    font-size: 0.8em;
    opacity: 0.85;
  }
  .named-act {
    cursor: pointer;
    opacity: 0.6;
  }
  .named-act:hover {
    opacity: 1;
  }
  .named-empty,
  .named-error {
    font-size: 0.8em;
    opacity: 0.7;
    margin: 0;
  }
</style>
```

- [ ] **Step 4: Run to verify it passes**

Run (repo root): `npx vitest run src/lib/components/__tests__/NamedEmojiPicker.test.ts`
Expected: PASS (3 tests).

- [ ] **Step 5: Type-check + commit**

Run: `npx tsc --noEmit` (clean), then:

```bash
git add src/lib/components/NamedEmojiPicker.svelte src/lib/components/__tests__/NamedEmojiPicker.test.ts
git commit -m "feat(emoji): NamedEmojiPicker popover (search/pick/rename/remove)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 10: integrate into `ChannelMessageFeed` (open popover + in-context naming + upload naming)

**Files:**
- Modify: `src/lib/components/ChannelMessageFeed.svelte`
- Test: `src/lib/components/__tests__/ChannelMessageFeed.test.ts`

Three integrations: (a) a button in the reaction picker row opens `<NamedEmojiPicker>`, whose `pick` reacts on the target message; (b) a "name this" affordance on public custom chips opens a tiny inline name input → `setEmojiName`; (c) an optional name field at upload routes the entered name to `setEmojiName` after ingest.

- [ ] **Step 1: Write the failing tests**

Add to `src/lib/components/__tests__/ChannelMessageFeed.test.ts` (mirror the existing custom-emoji tests there — they open the picker via `.picker-toggle` and stub `channelMessageService`):

```typescript
  it('picking from the named-emoji popover reacts with the stored descriptor', async () => {
    // Render the feed with a message + a service whose listEmojiNames returns one
    // named emoji; open the picker, open the named popover, click the tile.
    const reactToMessage = vi.fn().mockResolvedValue(undefined);
    const svc = makeFeedService({
      reactToMessage,
      listEmojiNames: vi.fn().mockResolvedValue([{ cid: 'aa', name: 'catjam', mime: 'image/png', size: 7 }]),
      previewNamedEmoji: vi.fn().mockResolvedValue(new Uint8Array([0x89, 0x50, 0x4e, 0x47])),
      onEmojiNamesChanged: vi.fn().mockReturnValue(() => {}),
    });
    const { container, getByLabelText, getByTitle } = renderFeedWithOneMessage(svc);
    await fireEvent.click(container.querySelector('.picker-toggle')!);
    await fireEvent.click(getByLabelText('Named emoji'));
    await waitFor(() => expect(getByTitle('catjam')).toBeTruthy());
    await fireEvent.click(getByTitle('catjam'));
    expect(reactToMessage).toHaveBeenCalledWith(
      expect.any(String), expect.any(String), expect.any(String), '', true,
      { cid: 'aa', mime: 'image/png', size: 7 },
    );
  });

  it('name-this on a public custom chip calls setEmojiName with the chip descriptor', async () => {
    const setEmojiName = vi.fn().mockResolvedValue(undefined);
    const svc = makeFeedService({ setEmojiName, previewReactionEmoji: vi.fn().mockResolvedValue(new Uint8Array([0x89, 0x50, 0x4e, 0x47])) });
    // Message has one PUBLIC custom reaction chip (encrypted: false).
    const { getByLabelText } = renderFeedWithCustomReaction(svc, { emojiCid: 'aa', emojiSize: 7, encrypted: false });
    await fireEvent.click(getByLabelText('Name this emoji')); // opens inline input
    await fireEvent.input(getByLabelText('Emoji name'), { target: { value: 'catjam' } });
    await fireEvent.click(getByLabelText('Save emoji name'));
    expect(setEmojiName).toHaveBeenCalledWith('aa', 'catjam', 'image/png', 7);
  });

  it('name-this affordance is absent on an encrypted custom chip', () => {
    const svc = makeFeedService({ previewReactionEmoji: vi.fn().mockResolvedValue(new Uint8Array([0x89, 0x50, 0x4e, 0x47])) });
    const { queryByLabelText } = renderFeedWithCustomReaction(svc, { emojiCid: 'bb', emojiSize: 7, encrypted: true });
    expect(queryByLabelText('Name this emoji')).toBeNull();
  });
```

Reuse the file's existing feed-render + mock-service helpers (it already has helpers that render the feed with a message and a stub `channelMessageService`; extend the stub factory to accept the extra methods above). If helpers like `makeFeedService`/`renderFeedWithOneMessage`/`renderFeedWithCustomReaction` don't exist by those names, adapt to the actual helpers the file uses — do not introduce a parallel harness.

- [ ] **Step 2: Run to verify they fail**

Run (repo root): `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts`
Expected: FAIL (no "Named emoji" button, no "Name this emoji" affordance).

- [ ] **Step 3: Wire the three integrations**

In `ChannelMessageFeed.svelte`:

Import the popover (top `<script>`, with the other component imports):

```svelte
  import NamedEmojiPicker from './NamedEmojiPicker.svelte';
```

Add state for the popover + inline-naming, near `customEmojiPrivate` (`:468`):

```svelte
  // Named-emoji popover open-for-message + inline "name this" state.
  let namedPickerFor = $state<string | null>(null);
  let namingCid = $state<string | null>(null); // chip cid being named inline
  let namingValue = $state('');
  let namingMime = '';
  let namingSize = 0;
  // Optional name typed at upload time (applied after ingest).
  let uploadName = $state('');
```

Add a handler to react from the popover (near `pickFromPicker` `:450`):

```svelte
  function pickNamedEmoji(msg: ChannelMessageDto, d: { cid: string; mime: string; size: number }): void {
    namedPickerFor = null;
    pickerOpenFor = null;
    void channelMessageService
      .reactToMessage(communityId, channelId, msg.messageId, '', true, d)
      .catch((e) => console.warn('named-emoji pick failed', e instanceof Error ? e.message : String(e)));
  }

  function startNameThis(r: NonNullable<ChannelMessageDto['reactions']>[number]): void {
    if (!r.emojiCid) return;
    namingCid = r.emojiCid;
    namingValue = '';
    namingMime = 'image/png';
    namingSize = r.emojiSize ?? 0;
  }

  async function commitNameThis(): Promise<void> {
    const cid = namingCid;
    const name = namingValue.trim();
    namingCid = null;
    if (!cid || !name) return;
    try {
      await channelMessageService.setEmojiName(cid, name, namingMime, namingSize);
    } catch (e) {
      reactionError = e instanceof Error ? e.message : String(e);
    }
  }
```

In `handleCustomEmojiPick` (`:487`), capture + apply the optional upload name. After the existing `const { cid: emojiCid, size } = await channelMessageService.ingestEmojiBytes(...)` line and the `reactToMessage` call (`:508-514`), add — but only when public and a name was typed (capture `uploadName` next to `makePrivate` at `:492` and reset it):

```svelte
    // (with the makePrivate capture near :492)
    const nameAtUpload = uploadName.trim();
    uploadName = '';
```

```svelte
    // (after the successful reactToMessage at :514, inside the try)
    if (nameAtUpload && !makePrivate) {
      await channelMessageService.setEmojiName(emojiCid, nameAtUpload, 'image/png', size);
    }
```

In the picker markup (`:727-758`), add a "Named emoji" open button + the popover, after the `.picker-custom` button:

```svelte
                <button
                  type="button"
                  class="picker-named"
                  role="menuitem"
                  aria-label="Named emoji"
                  onclick={() => (namedPickerFor = namedPickerFor === msg.messageId ? null : msg.messageId)}
                >🔖</button>
                <label class="picker-upload-name">
                  <span>Name (optional)</span>
                  <input type="text" aria-label="Name new emoji" bind:value={uploadName} placeholder="catjam" />
                </label>
                {#if namedPickerFor === msg.messageId}
                  <div class="named-popover">
                    <NamedEmojiPicker {channelMessageService} onpick={(d) => pickNamedEmoji(msg, d)} />
                  </div>
                {/if}
```

In the chip render (`:684-705`), add the in-context affordance for PUBLIC custom chips. Inside the `{#each msg.reactions ...}` loop, after the `</button>` of the `.reaction-chip` (still inside the `{#each}`), add:

```svelte
                  {#if r.emojiCid && r.encrypted !== true}
                    {#if namingCid === r.emojiCid}
                      <input
                        class="name-this-input"
                        type="text"
                        aria-label="Emoji name"
                        bind:value={namingValue}
                        onkeydown={(ev) => ev.key === 'Enter' && commitNameThis()}
                      />
                      <button type="button" class="name-this-save" aria-label="Save emoji name" onclick={() => commitNameThis()}>✓</button>
                    {:else}
                      <button type="button" class="name-this" aria-label="Name this emoji" onclick={() => startNameThis(r)}>✎</button>
                    {/if}
                  {/if}
```

Add minimal CSS in the component `<style>` (near the existing `.reaction-picker`/`.picker-custom` rules):

```css
  .picker-named { cursor: pointer; }
  .picker-upload-name { display: flex; flex-direction: column; font-size: 0.75em; gap: 0.1rem; }
  .named-popover { position: absolute; z-index: 10; }
  .name-this { opacity: 0.4; cursor: pointer; font-size: 0.85em; }
  .name-this:hover { opacity: 1; }
  .name-this-input { width: 7rem; }
```

- [ ] **Step 4: Run to verify they pass**

Run (repo root): `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts`
Expected: PASS (new tests + all existing feed tests).

- [ ] **Step 5: Type-check + commit**

Run: `npx tsc --noEmit` (clean), then:

```bash
git add src/lib/components/ChannelMessageFeed.svelte src/lib/components/__tests__/ChannelMessageFeed.test.ts
git commit -m "feat(emoji): named-emoji popover + in-context + upload naming in feed

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Final verification (after all tasks)

- [ ] **Full Rust gate sweep:**

```bash
cd src-tauri && cargo fmt --all -- --check \
  && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
  && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: fmt clean, clippy 0 warnings, all tests pass.

- [ ] **Full frontend gate sweep (from repo root):**

```bash
npx tsc --noEmit && npx vitest run
```

Expected: type-check clean, all tests pass.

- [ ] **Manual smoke (optional, if a dev build is handy):** react with a custom emoji (public) → hover the chip → "name this" → type `catjam` → save. Open another message's picker → 🔖 → search `catjam` → click the tile → it reacts. Confirm an encrypted-emoji chip shows no "name this" affordance.

---

## Notes for the implementer

- **No protocol/wire change, no migration.** Names are local JSON; reactions on the wire are unchanged. Existing emoji keep working.
- **`set_emoji_name` takes `{mime, size}` from the caller** — the frontend always knows them (a chip's `emojiSize`; an upload's returned `size`). The retain-on-name pin is a *separate, best-effort* durability step; naming never blocks on it.
- **Public-only is enforced twice:** `set_emoji_name` rejects encrypted CIDs (the hard gate), and the UI hides the affordance on chips where `r.encrypted === true` (the soft gate; a live-arriving chip with unknown `encrypted` still shows it and is caught by the hard gate).
- **Soft name uniqueness is front-end-only / not enforced** — two CIDs may share a name (a `reactions_for`-style backend test pins this for the store).
- **Run `--all-targets` for clippy/nextest** — the IPC + DTO changes are reached by integration tests; a `--lib`-only run would miss breakage (CLAUDE.md).
- **Run the FULL `vitest` suite** before pushing — `ChannelMessageFeed`/`ReactionEmojiImage` are rendered by other component tests; a scoped run can miss cross-file drift.
- **Keep ZEB IDs out of branch/commit/PR names** (Linear auto-close cascade).
- **Match existing helper/field names** in `channel-message-service.ts` and the two `*.test.ts` files rather than introducing parallel mocks/registries — the snippets above name things by their role; bind them to the file's actual identifiers.
- **Two `generate_handler!` blocks** exist (`lib.rs:48100` live, `:48366` second) — register in the live one; only touch the second if it also lists `set_friend_nickname`.
- **CID flag bit:** `[0x42; 32]` = public, `[0xB2; 32]` = encrypted (high bit of byte 0). Used in tests; production derives via `ContentId::from_bytes(cid).flags().encrypted`, never by hand.
