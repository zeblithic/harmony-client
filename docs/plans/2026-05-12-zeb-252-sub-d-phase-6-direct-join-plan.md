# ZEB-252 Sub-D Phase 6: Direct-join IPC — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a typed `join_open_community(communityId)` Tauri IPC that re-resolves the directory entry server-side and delegates to the existing `redeem_invite_inner` codepath; switch the library-directory Join button to use it. `redeem_invite(url)` remains untouched for hand-pasted URLs.

**Architecture:** Thin wrapper IPC (Approach A from brainstorm). New IPC handler in `src-tauri/src/lib.rs` snapshots `NodeState` the same way `redeem_invite` does (lines 9300–9453), calls `library_directory.snapshot_all().await`, finds the matching `AggregatedEntry` by `community_id`, defensively re-checks `is_invite_only`, then calls `redeem_invite_inner(entry.invite_url, …)` unchanged. Frontend `LibraryDirectoryBrowser.svelte` `onJoin` signature flips from `(inviteUrl)` to `(communityId)`; `App.svelte` rewires through `communityService.joinOpenCommunity(communityId)` so the full RedeemInviteDialog side-effects (nav-updated synthesis, kind tracking, selected-community switch, member refresh) apply to the directory click path too.

**Tech Stack:** Rust (tauri 2.x, tokio, ed25519-dalek, hex, async traits), Svelte 5 runes ($props, $state, $effect), TypeScript, vitest.

**Spec:** `docs/specs/2026-05-12-zeb-252-sub-d-phase-6-direct-join-design.md` (commit `479b14a`).

---

## File Structure

**Created — none.** All changes are additions to existing files.

**Modified:**

| File | Responsibility | Edit type |
|---|---|---|
| `src-tauri/src/lib.rs` | New `#[tauri::command] async fn join_open_community` handler near existing `redeem_invite` (around line 9300+). Register in `tauri::generate_handler!` at line 12585+. Add `join_open_community_tests` mod (sibling to `redeem_invite_inner_tests`) for happy-path + invite-only + missing-entry coverage. | Add IPC + tests |
| `src-tauri/src/library_directory.rs` | New `pub fn find_open_community_invite_url_in_snapshot(snapshot, community_id_hex) -> Result<String, String>` helper near other public helpers (after `parse_owner_addr_hex` at line 1245). Inline `#[cfg(test)] mod` with 3 unit tests (missing, invite-only, ok). | Add helper + tests |
| `src/lib/community-service.ts` | Add `joinOpenCommunity(communityId)` method after `redeemInvite(url)` at line 151. Returns same `RedeemInviteResultDto` shape; `knownKinds.set` parity. | Add method |
| `src/lib/__tests__/community-service.test.ts` | Add vitest case mirroring existing `redeemInvite` test (line 76+). | Add test |
| `src/lib/components/LibraryDirectoryBrowser.svelte` | `onJoin` prop type `(inviteUrl: string) => Promise<void>` → `(communityId: string) => Promise<void>`. JSDoc comment update. `handleJoin` call site (line 173) `entry.invite_url` → `entry.community_id`. | Two-line type change + JSDoc |
| `src/App.svelte` | Replace the `<LibraryDirectoryBrowser onJoin={async (inviteUrl) => { await tauriAdapter!.invoke('redeem_invite', { url: inviteUrl }); }}>` block (line 1678–1682) with a handler that calls `communityService.joinOpenCommunity(communityId)` and runs the same post-redeem side-effects as the `RedeemInviteDialog onSubmit` handler at line 1620+. | Handler rewrite |

**Cross-file invariants:**

1. The IPC parameter name on the Rust side is `community_id: String` (snake_case); on the JS side it's `communityId: string` (camelCase). Tauri's IPC layer auto-converts.
2. Error extraction in any frontend `catch` block uses `e instanceof Error ? e.message : String(e)`.
3. All 6 CI gates must remain green between tasks: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `cargo check --locked --all-targets --features test-fixtures` (msrv), `npx tsc --noEmit`, `npx vitest run`. The first four run from `src-tauri/`; the last two run from the repo root.

---

## Task 0: Pre-flight + green baseline confirmation

**No commit. Verifies the just-cut branch starts from a green workspace.**

**Files:** none modified.

- [ ] **Step 1: Confirm git state**

Run:
```bash
git status && git rev-parse --abbrev-ref HEAD && git log --oneline origin/main..HEAD
```

Expected output:
- `On branch zeb-252-sub-d-phase-6-direct-join`
- `nothing to commit, working tree clean`
- `HEAD` is one commit ahead of `origin/main` (the spec commit `479b14a`)

If anything differs, STOP and surface the divergence.

- [ ] **Step 2: Run cargo fmt check**

Run (from `src-tauri/`):
```bash
cd src-tauri && cargo fmt --all -- --check
```

Expected: exit 0, no output.

- [ ] **Step 3: Run cargo clippy**

Run (from `src-tauri/`):
```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

Expected: exit 0 (warnings-as-errors gate passes).

- [ ] **Step 4: Run cargo nextest**

Run (from `src-tauri/`):
```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: all tests pass. **Record the totals** — `<N> tests passed`, `<M> ignored`, `<F> filtered`. These become the baseline; Task 4's final verification must show test totals ≥ baseline + the new tests (4 Rust + 2 vitest).

If nextest hangs past ~30 min (known pattern matching ZEB-282 flake), interrupt with Ctrl-C and re-run. Do NOT use Monitor — wait synchronously.

- [ ] **Step 5: Run cargo check (msrv)**

Run (from `src-tauri/`):
```bash
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
```

Expected: exit 0.

- [ ] **Step 6: Run frontend type check**

Run (from repo root):
```bash
npx tsc --noEmit
```

Expected: exit 0, no type errors.

- [ ] **Step 7: Run vitest**

Run (from repo root):
```bash
npx vitest run
```

Expected: all tests pass. **Record the totals** — `<N> tests passed`.

- [ ] **Step 8: Confirm green baseline**

All 6 commands above exit 0. The branch is ready for implementation.

---

## Task 1: Backend — `find_open_community_invite_url_in_snapshot` helper + `join_open_community` IPC handler + 4 unit tests

**Files:**
- Modify: `src-tauri/src/library_directory.rs` (add helper + 3 unit tests around line 1245+; helper goes right after `parse_owner_addr_hex`)
- Modify: `src-tauri/src/lib.rs` (add `join_open_community` IPC handler near line 9453 (right after `redeem_invite`); register in `tauri::generate_handler!` at line 12645 area; add `join_open_community_tests` mod sibling to `redeem_invite_inner_tests`)

- [ ] **Step 1: Write the 3 failing helper unit tests in `library_directory.rs`**

Locate the existing `#[cfg(test)] mod tests` block at the end of `src-tauri/src/library_directory.rs` (look for the existing tests around lines 1700+ that reference `snap[0]`). Add these 3 tests inside that mod. If a fixture builder for `LibraryDirectoryEntry` already exists (search for `fn make_test_entry` or `fn build_entry`), reuse it; otherwise inline the construction shown below.

```rust
// ── ZEB-252 Sub-D Phase 6 — find_open_community_invite_url_in_snapshot tests ──

#[test]
fn find_open_community_invite_url_returns_err_when_missing() {
    use crate::library_directory::find_open_community_invite_url_in_snapshot;

    let snapshot: Vec<AggregatedEntry> = Vec::new();
    let result = find_open_community_invite_url_in_snapshot(&snapshot, "00".repeat(16).as_str());
    let err = result.expect_err("empty snapshot should not match");
    assert!(
        err.contains("no longer listed"),
        "expected friendly missing-entry message, got: {err}"
    );
}

#[test]
fn find_open_community_invite_url_returns_err_when_entry_is_invite_only() {
    use crate::community_invite::{
        encode_invite_url, CommunityInvitePayload, InviteEpochSnapshot, MaterializedCommunityState,
    };
    use crate::library_directory::find_open_community_invite_url_in_snapshot;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;
    use std::collections::BTreeSet;

    // Mint an invite-only URL. The sealed_epoch_key shape for invite-only is
    // a 92-byte sealed envelope, but for THIS test we only need decode_invite_url
    // to succeed and surface `is_invite_only == true`. The defensive re-check
    // runs BEFORE any decryption attempt, so a placeholder sealed_epoch_key
    // suffices (decode_invite_url validates structure, not crypto).
    let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
    let admin_addr = OwnerAddr(admin_identity.identity.address_hash);
    let community_id = SpaceId([0xf1; 16]);

    let payload = CommunityInvitePayload {
        community_id,
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            // 92-byte placeholder (matches invite-only minimum envelope size)
            sealed_epoch_key: vec![0u8; 92],
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr,
        community_name: "Inviteonly".into(),
        is_invite_only: true,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: Some(admin_identity.identity.to_public_bytes()),
    };
    let invite_url = encode_invite_url(&payload).expect("encode invite-only url");

    // Construct an AggregatedEntry with this invite-only URL. The entry's
    // OTHER fields (community_signature, community_admin_identity_pub) don't
    // matter for the helper — it only reads invite_url + community_id.
    let entry = LibraryDirectoryEntry {
        community_id,
        community_admin_identity_pub: admin_identity.identity.to_public_bytes(),
        name: "Inviteonly".into(),
        description: String::new(),
        topics: Vec::new(),
        invite_url,
        listed_by: OwnerAddr([0xcc; 16]),
        listed_at: Hlc { wall_ms: 1_000, logical: 0, device_id: "test-dev".into() },
        library_identity_pub: None,
        library_signature: None,
        community_signature: [0u8; 64],
    };
    let agg = AggregatedEntry {
        entry,
        attested_by: BTreeSet::new(),
        unattested_by: BTreeSet::new(),
    };
    let snapshot = vec![agg];

    let result =
        find_open_community_invite_url_in_snapshot(&snapshot, &hex::encode(community_id.0));
    let err = result.expect_err("invite-only entry must be rejected");
    assert!(
        err.to_lowercase().contains("invite-only"),
        "expected invite-only message, got: {err}"
    );
}

#[test]
fn find_open_community_invite_url_returns_ok_for_open_entry() {
    use crate::community_invite::{
        encode_invite_url, CommunityInvitePayload, InviteEpochSnapshot, MaterializedCommunityState,
    };
    use crate::library_directory::find_open_community_invite_url_in_snapshot;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;
    use std::collections::BTreeSet;

    let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
    let admin_addr = OwnerAddr(admin_identity.identity.address_hash);
    let community_id = SpaceId([0xf2; 16]);

    let payload = CommunityInvitePayload {
        community_id,
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: vec![0x42u8; 32], // raw 32-byte EpochKey for open
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr,
        community_name: "OpenCom".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
    };
    let invite_url = encode_invite_url(&payload).expect("encode open url");

    let entry = LibraryDirectoryEntry {
        community_id,
        community_admin_identity_pub: admin_identity.identity.to_public_bytes(),
        name: "OpenCom".into(),
        description: String::new(),
        topics: Vec::new(),
        invite_url: invite_url.clone(),
        listed_by: OwnerAddr([0xcc; 16]),
        listed_at: Hlc { wall_ms: 1_000, logical: 0, device_id: "test-dev".into() },
        library_identity_pub: None,
        library_signature: None,
        community_signature: [0u8; 64],
    };
    let agg = AggregatedEntry {
        entry,
        attested_by: BTreeSet::new(),
        unattested_by: BTreeSet::new(),
    };
    let snapshot = vec![agg];

    let returned =
        find_open_community_invite_url_in_snapshot(&snapshot, &hex::encode(community_id.0))
            .expect("open entry must return Ok");
    assert_eq!(returned, invite_url, "helper must return the entry's invite_url verbatim");
}
```

**Note on `LibraryDirectoryEntry` field list.** If the struct's field set differs from what's shown above when you read the file, match the actual struct definition exactly — these tests construct it directly, so all current fields must be set. Open `src-tauri/src/library_directory.rs` and locate `pub struct LibraryDirectoryEntry` first; copy its field list into your test constructions.

- [ ] **Step 2: Run the new tests to confirm they fail**

Run (from `src-tauri/`):
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(find_open_community_invite_url)'
```

Expected: 3 tests fail with `cannot find function find_open_community_invite_url_in_snapshot in module crate::library_directory` (or similar — function doesn't exist yet).

- [ ] **Step 3: Implement `find_open_community_invite_url_in_snapshot` in `library_directory.rs`**

Locate `pub fn parse_owner_addr_hex` around line 1245. Add the helper directly above or below it (whichever produces cleaner diff hunks):

```rust
/// ZEB-252 Sub-D Phase 6: find the open-community `invite_url` for a
/// given hex-encoded `community_id` in a directory snapshot.
///
/// Returns the entry's `invite_url` on success. Errors:
/// - No matching entry: `"This community is no longer listed by any of your libraries"`
///   (the user-facing race-window message from spec §4.3).
/// - Matching entry but its invite URL decodes to `is_invite_only == true`:
///   `"Invite-only community cannot be joined directly from the directory"`
///   (belt-and-suspenders per spec §4.4 — Phase 1's `verify_entry` already
///   rejects invite-only URLs at receive, so this branch is unreachable in
///   practice; the re-check defends against future Phase 1 regressions).
/// - Malformed `community_id_hex`: bubbles a "invalid hex" / "wrong length" message.
///
/// Pure function. Caller supplies the snapshot (typically the result of
/// `LibraryDirectory::snapshot_all().await`).
pub fn find_open_community_invite_url_in_snapshot(
    snapshot: &[AggregatedEntry],
    community_id_hex: &str,
) -> Result<String, String> {
    // 1. Parse community_id_hex into a SpaceId for the comparison key.
    let id_bytes: [u8; 16] = hex::decode(community_id_hex)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let want = crate::owner_state_types::SpaceId(id_bytes);

    // 2. Find the matching aggregated entry.
    let agg = snapshot
        .iter()
        .find(|a| a.entry.community_id == want)
        .ok_or_else(|| {
            "This community is no longer listed by any of your libraries".to_string()
        })?;

    // 3. Defensive `is_invite_only` re-check (spec §4.4).
    let payload = crate::community_invite::decode_invite_url(&agg.entry.invite_url)
        .map_err(|e| format!("directory entry's invite URL failed to decode: {e:?}"))?;
    if payload.is_invite_only {
        return Err("Invite-only community cannot be joined directly from the directory".into());
    }

    Ok(agg.entry.invite_url.clone())
}
```

- [ ] **Step 4: Run the helper unit tests to confirm they pass**

Run (from `src-tauri/`):
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(find_open_community_invite_url)'
```

Expected: 3 tests pass.

- [ ] **Step 5: Write the failing happy-path delegation test in `lib.rs`**

Locate `mod redeem_invite_inner_tests` around line 9456 in `src-tauri/src/lib.rs`. Immediately after the closing `}` of that mod (search for `} // mod redeem_invite_inner_tests` or the line where it ends), add a new sibling mod:

```rust
#[cfg(test)]
mod join_open_community_tests {
    use super::*;
    use crate::community_invite::{
        encode_invite_url, CommunityInvitePayload, InviteEpochSnapshot, MaterializedCommunityState,
    };
    use crate::library_directory::{AggregatedEntry, LibraryDirectoryEntry};
    use crate::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;
    use std::collections::BTreeSet;

    /// Build an `AggregatedEntry` carrying an open-community invite URL
    /// minted with the supplied admin identity and community_id. The
    /// invite_url is sufficient to drive `redeem_invite_inner` end-to-end
    /// in unit tests (matches `redeem_invite_inner_tests::happy_path_*`
    /// fixture invite construction).
    fn build_open_directory_aggregated(
        admin_identity: &PrivateIdentity,
        community_id: SpaceId,
        membership_key_bytes: [u8; 32],
        community_name: &str,
    ) -> AggregatedEntry {
        let admin_addr = OwnerAddr(admin_identity.identity.address_hash);
        let payload = CommunityInvitePayload {
            community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: membership_key_bytes.to_vec(),
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr,
            community_name: community_name.into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
        };
        let invite_url = encode_invite_url(&payload).expect("encode open url");
        let entry = LibraryDirectoryEntry {
            community_id,
            community_admin_identity_pub: admin_identity.identity.to_public_bytes(),
            name: community_name.into(),
            description: String::new(),
            topics: Vec::new(),
            invite_url,
            listed_by: OwnerAddr([0xcc; 16]),
            listed_at: Hlc { wall_ms: 1_000, logical: 0, device_id: "test-dev".into() },
            library_identity_pub: None,
            library_signature: None,
            community_signature: [0u8; 64],
        };
        AggregatedEntry {
            entry,
            attested_by: BTreeSet::new(),
            unattested_by: BTreeSet::new(),
        }
    }

    /// Happy path: `join_open_community_inner` finds the entry, defensively
    /// confirms `is_invite_only == false`, and delegates to
    /// `redeem_invite_inner`. Returns a DTO with the community_id +
    /// community_name from the invite payload + isInviteOnly=false.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn join_open_community_happy_path_delegates_to_redeem_and_returns_dto() {
        let fixture = redeem_invite_inner_tests::build_redeem_invite_test_fixture().await;

        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let community_id = SpaceId([0xf3; 16]);
        let membership_key = EpochKey::new([0x42; 32]);

        let agg = build_open_directory_aggregated(
            &admin_identity,
            community_id,
            *membership_key.as_bytes(),
            "JoinCom",
        );
        let snapshot = vec![agg];

        let dto = join_open_community_inner(
            hex::encode(community_id.0),
            &snapshot,
            std::sync::Arc::clone(&fixture.crdt_state),
            std::sync::Arc::clone(&fixture.hlc_tracker),
            fixture.device_id.clone(),
            fixture.self_owner,
            std::sync::Arc::clone(&fixture.signing_key),
            std::sync::Arc::clone(&fixture.community_registry),
            fixture.community_adapter_tx.clone(),
            fixture.unicast_send_tx.clone(),
            std::sync::Arc::clone(&fixture.dm_outbox),
            std::sync::Arc::clone(&fixture.channel_log_registry),
            || Ok(()),
        )
        .await
        .expect("happy path must succeed");

        assert_eq!(dto.community_id, hex::encode(community_id.0));
        assert_eq!(dto.community_name, "JoinCom");
        assert!(!dto.is_invite_only);
    }
}
```

**Note:** The test invokes a function `join_open_community_inner` that doesn't exist yet — it will fail to compile. The next step adds it.

The test also references `redeem_invite_inner_tests::build_redeem_invite_test_fixture` — that function is private to the `redeem_invite_inner_tests` mod by default. If accessing it from the sibling mod fails to compile, **change its visibility** in the source by replacing `async fn build_redeem_invite_test_fixture` with `pub(super) async fn build_redeem_invite_test_fixture` so the sibling test mod can call it. Similarly bump `RedeemInviteTestFixture` and `signing_key_from_identity` to `pub(super)`. (This is the minimal visibility change — no API leak outside `lib.rs`.)

- [ ] **Step 6: Run the test to confirm it fails (compile error)**

Run (from `src-tauri/`):
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(join_open_community_happy_path)'
```

Expected: compilation failure pointing at `join_open_community_inner` not found.

- [ ] **Step 7: Add the `join_open_community_inner` function and the `join_open_community` Tauri command in `lib.rs`**

In `src-tauri/src/lib.rs`, insert the following two functions immediately AFTER the existing `redeem_invite` Tauri command (which ends at line 9453). The block should sit between line 9453 and the start of `mod redeem_invite_inner_tests` (line 9456). Code:

```rust
// ── ZEB-252 Sub-D Phase 6: join_open_community ─────────────────────────────
//
// Thin wrapper over redeem_invite_inner. Re-resolves the directory entry
// server-side at click time so the renderer can't pass a URL the user
// never saw. The actual join machinery (URL decode, HLC reserve, mint,
// engine spawn, owner-state commit) is unchanged — Phase 6 is strictly
// a caller of redeem_invite_inner.
//
// See `docs/specs/2026-05-12-zeb-252-sub-d-phase-6-direct-join-design.md`.

/// Inner helper for `join_open_community`. Separated from the Tauri command
/// so unit tests can supply a fabricated snapshot + the standard
/// redeem-invite test fixture without spinning up a `LibraryDirectory` actor.
///
/// Same argument list as `redeem_invite_inner` minus the leading `url: String`
/// (which this helper derives from `snapshot` + `community_id_hex`), plus
/// `snapshot: &[AggregatedEntry]` for the directory lookup.
#[allow(clippy::too_many_arguments)]
async fn join_open_community_inner<R, F>(
    community_id_hex: String,
    snapshot: &[crate::library_directory::AggregatedEntry],
    crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    hlc_tracker: std::sync::Arc<
        tokio::sync::Mutex<std::collections::BTreeMap<String, crate::owner_state_types::Hlc>>,
    >,
    device_id: String,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    community_registry: std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    community_adapter_tx: tokio::sync::mpsc::Sender<crate::event_loop::CommunityAdapterRequest>,
    unicast_send_tx: tokio::sync::mpsc::Sender<crate::dm_outbox::UnicastSendRequest>,
    dm_outbox: std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
    channel_log_registry: std::sync::Arc<crate::community_channel_log_engine::ChannelLogRegistry<R>>,
    fence_check: F,
) -> Result<RedeemInviteResultDto, String>
where
    R: tauri::Runtime,
    F: Fn() -> Result<(), String> + Send + Sync + 'static,
{
    let invite_url = crate::library_directory::find_open_community_invite_url_in_snapshot(
        snapshot,
        &community_id_hex,
    )?;

    redeem_invite_inner(
        invite_url,
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        signing_key,
        community_registry,
        community_adapter_tx,
        unicast_send_tx,
        dm_outbox,
        channel_log_registry,
        fence_check,
    )
    .await
}

/// Tauri IPC: join an open community directly from the library-directory
/// aggregation. Re-resolves the entry by `community_id` server-side, then
/// delegates to `redeem_invite_inner` (which Phase 6 strictly wraps).
///
/// `redeem_invite(url)` remains the IPC for hand-pasted URLs.
///
/// Lock-order discipline mirrors `redeem_invite` (line 9300+): the std
/// `state_lock` guard drops before any `.await`. Same NodeState snapshot
/// set; same `fence_check` closure shape.
///
/// Errors (all surface as `String` per existing IPC convention):
/// - `"This community is no longer listed by any of your libraries"` — entry
///   not in current aggregation (race window: user removed source library
///   between view + click, or entry tombstoned).
/// - `"Invite-only community cannot be joined directly from the directory"` —
///   defensive re-check; spec §4.4.
/// - Any error from `redeem_invite_inner` propagated verbatim.
#[tauri::command]
async fn join_open_community(
    app: tauri::AppHandle,
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<RedeemInviteResultDto, String> {
    // Snapshot NodeState handles in a single guard scope, then drop the
    // std lock BEFORE any `.await`. Mirrors `redeem_invite` exactly.
    let (
        library_directory,
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        community_registry,
        community_adapter_tx,
        unicast_send_tx,
        channel_log_registry,
        dm_outbox,
        snapshot_generation,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.library_directory
                .clone()
                .ok_or("library_directory missing — node not running?")?,
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.community_adapter_request_tx
                .clone()
                .ok_or("community_adapter_request_tx missing")?,
            g.unicast_send_tx
                .clone()
                .ok_or("unicast_send_tx missing — no owner identity?")?,
            g.channel_log_registry
                .clone()
                .ok_or("channel_log_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    }; // std lock dropped here.

    // Snapshot the directory under the actor lock (async).
    let snapshot = library_directory.snapshot_all().await;

    // Outbox lock to clone the signing key (same as redeem_invite).
    let signing_key = {
        let outbox_g = dm_outbox.lock().await;
        std::sync::Arc::clone(&outbox_g.signing_key)
    };

    // Fence-check closure: re-locks NodeState and compares `generation`
    // against `snapshot_generation`. Identical pattern to redeem_invite.
    let fence_check = {
        let state_lock = state_lock.clone();
        move || -> Result<(), String> {
            let g = state_lock
                .lock()
                .map_err(|e| format!("NodeState poisoned: {e}"))?;
            if g.generation != snapshot_generation {
                return Err(format!(
                    "node generation changed during join_open_community (was {}, now {}); \
                     join minted on a detached crdt_state and won't be persisted — \
                     engine spawn suppressed",
                    snapshot_generation, g.generation
                ));
            }
            if g.community_registry.is_none() {
                return Err(
                    "community_registry was torn down during join_open_community — engine \
                     spawn suppressed"
                        .to_string(),
                );
            }
            Ok(())
        }
    };

    let dto = join_open_community_inner(
        community_id,
        &snapshot,
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        signing_key,
        community_registry,
        community_adapter_tx,
        unicast_send_tx,
        dm_outbox,
        channel_log_registry,
        fence_check,
    )
    .await?;

    // Same nav-updated emit as `redeem_invite` (line 9438+). Non-fatal on
    // failure — the join already committed.
    if let Err(e) = app.emit(
        "nav-updated",
        &NavUpdatedPayload {
            action: "added",
            space_id: dto.community_id.clone(),
            kind: "community",
            name: dto.community_name.clone(),
            members: None,
            parent_id: None,
        },
    ) {
        tracing::warn!(error = %e, "join_open_community: nav-updated emit failed");
    }

    Ok(dto)
}
```

- [ ] **Step 8: Register `join_open_community` in the Tauri handler list**

Locate `tauri::generate_handler![` at line 12585 in `src-tauri/src/lib.rs`. Find the line `redeem_invite,` at line 12645 inside that macro invocation. Add `join_open_community,` on the line directly below it. Example:

```rust
            redeem_invite,
            join_open_community,
```

Look for any sibling `builder.invoke_handler(tauri::generate_handler![` at line 12687 — if `redeem_invite` is registered there too, add `join_open_community,` parallel to it. (Search the macro body for "redeem_invite" to find all registration sites.)

- [ ] **Step 9: Run the happy-path test to confirm it now passes**

Run (from `src-tauri/`):
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(join_open_community_happy_path)'
```

Expected: 1 test passes.

If you see `private function` errors when the test invokes `redeem_invite_inner_tests::build_redeem_invite_test_fixture`, bump that function's visibility to `pub(super)` as described in Step 5's note.

- [ ] **Step 10: Run the full backend gate set**

Run (from `src-tauri/`):
```bash
cd src-tauri && cargo fmt --all && cargo fmt --all -- --check && \
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && \
cargo nextest run --locked --workspace --all-targets --features test-fixtures && \
cargo check --locked --all-targets --features test-fixtures
```

Expected: all four commands exit 0. The first `cargo fmt --all` rewrites; the `--check` confirms no leftover diffs.

If clippy flags `unused import` for any of the test mod imports, prune the unused ones rather than `#[allow(unused_imports)]`. Same for the helper module if the helper's tests don't exercise some helper's import (e.g., `BTreeSet`).

- [ ] **Step 11: Commit Task 1**

Run:
```bash
git add src-tauri/src/library_directory.rs src-tauri/src/lib.rs && \
git commit -m "$(cat <<'EOF'
feat(zeb-252): backend join_open_community IPC + helper

Add `find_open_community_invite_url_in_snapshot` helper in
library_directory.rs that takes an aggregation snapshot + hex
community_id, validates the entry exists, defensively re-checks
the invite URL's is_invite_only, and returns the invite_url. New
`#[tauri::command] join_open_community` in lib.rs snapshots
NodeState the same way redeem_invite does, fetches the directory
snapshot, calls the helper, and delegates to redeem_invite_inner.
Same nav-updated emit, same fence_check semantics. redeem_invite
itself is untouched.

3 helper unit tests (missing, invite-only, ok) + 1 IPC happy-path
delegation test colocated with redeem_invite_inner_tests.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Frontend — `CommunityService.joinOpenCommunity` method + vitest

**Files:**
- Modify: `src/lib/community-service.ts` (add `joinOpenCommunity` method after `redeemInvite` at line 151)
- Modify: `src/lib/__tests__/community-service.test.ts` (add vitest case mirroring the existing `redeemInvite` test at line 76+)

- [ ] **Step 1: Write the failing vitest case**

Open `src/lib/__tests__/community-service.test.ts`. Locate the `redeemInvite returns the DTO and learns the community kind` test block (around line 76). Below it (still inside the same `describe(...)` block), add:

```typescript
  it('joinOpenCommunity returns the DTO and learns the community kind', async () => {
    const dto = {
      communityId: 'aabbccddeeff00112233445566778899',
      communityName: 'DirCommunity',
      isInviteOnly: false,
    };
    const adapter = makeMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce(dto);
    const service = new CommunityService();
    await service.connectAdapter(adapter);

    const returned = await service.joinOpenCommunity('aabbccddeeff00112233445566778899');
    expect(adapter.invoke).toHaveBeenCalledWith('join_open_community', {
      communityId: 'aabbccddeeff00112233445566778899',
    });
    expect(returned).toEqual(dto);
    // Same as redeemInvite: a successful direct-join learns the kind.
    expect(service.getKind('aabbccddeeff00112233445566778899')).toBe('open');
  });
```

If `makeMockAdapter` doesn't exist by that exact name, use the same adapter mock helper the existing `redeemInvite` test on line 76+ uses (search the test file for the pattern that constructs an `adapter` with a `vi.fn()`-based `invoke`).

- [ ] **Step 2: Run vitest to confirm the new test fails**

Run (from repo root):
```bash
npx vitest run --reporter=verbose src/lib/__tests__/community-service.test.ts
```

Expected: the new test fails with `service.joinOpenCommunity is not a function`. Other tests in this file should pass.

- [ ] **Step 3: Implement `joinOpenCommunity` in `community-service.ts`**

Open `src/lib/community-service.ts`. Locate `async redeemInvite(url: string)` at line 151. Add directly below the closing `}` of `redeemInvite`:

```typescript
  /**
   * ZEB-252 Sub-D Phase 6: typed direct-join entry point for
   * library-directory click flows. The backend re-resolves the matching
   * `LibraryDirectoryEntry` by community_id and delegates to the same
   * `redeem_invite_inner` codepath `redeemInvite` uses, so the resulting
   * DTO and side-effects (engine spawn, owner-state Space row, self-Join
   * event log) are identical. `redeemInvite(url)` stays for hand-pasted
   * URLs.
   */
  async joinOpenCommunity(communityId: string): Promise<RedeemInviteResultDto> {
    const dto = await this.invoke<RedeemInviteResultDto>('join_open_community', { communityId });
    // Backend hands back the kind; populate getKind() the same way redeemInvite does.
    // Phase 6 only joins OPEN communities (invite-only entries are rejected
    // by the backend's defensive re-check), so isInviteOnly will always be false
    // for successful returns — but we mirror redeemInvite's logic for symmetry
    // rather than assuming.
    this.knownKinds.set(dto.communityId, dto.isInviteOnly ? 'invite-only' : 'open');
    return dto;
  }
```

- [ ] **Step 4: Run vitest to confirm the new test passes**

Run (from repo root):
```bash
npx vitest run --reporter=verbose src/lib/__tests__/community-service.test.ts
```

Expected: all tests in the file pass, including the new `joinOpenCommunity` test.

- [ ] **Step 5: Run the full frontend gate set**

Run (from repo root):
```bash
npx tsc --noEmit && npx vitest run
```

Expected: both commands exit 0.

- [ ] **Step 6: Commit Task 2**

Run:
```bash
git add src/lib/community-service.ts src/lib/__tests__/community-service.test.ts && \
git commit -m "$(cat <<'EOF'
feat(zeb-252): CommunityService.joinOpenCommunity + vitest

Add typed `joinOpenCommunity(communityId)` method alongside
`redeemInvite(url)`. Same `RedeemInviteResultDto` return; same
`knownKinds` population on success. Mirrors redeemInvite's
service-level pattern.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Frontend — `LibraryDirectoryBrowser` onJoin signature change + `App.svelte` wire-up + vitest

**Files:**
- Modify: `src/lib/components/LibraryDirectoryBrowser.svelte` (lines 7–8, 30, 169–180): `onJoin` prop type + JSDoc + `handleJoin` call site
- Modify: `src/App.svelte` (lines 1656–1687): wire onJoin to `communityService.joinOpenCommunity` with the same post-redeem side-effects as the RedeemInviteDialog handler at line 1620+
- Modify: an existing vitest test file for `LibraryDirectoryBrowser` (if one exists — implementer locates by searching `src/lib/__tests__/` for `LibraryDirectoryBrowser`) OR create `src/lib/__tests__/LibraryDirectoryBrowser.test.ts` with the new assertion

- [ ] **Step 1: Locate the existing browser test file**

Run (from repo root):
```bash
find src/lib/__tests__ -name "LibraryDirectoryBrowser*" -o -name "library-directory-browser*" 2>/dev/null
```

If a file exists, plan to extend it. If not, create a new one in Step 4.

- [ ] **Step 2: Write the failing Browser vitest case**

Edit (or create) the browser test file. Add a test asserting that clicking Join invokes `onJoin` with `entry.community_id`, not `entry.invite_url`:

```typescript
import { render, fireEvent } from '@testing-library/svelte';
import { vi, describe, it, expect } from 'vitest';
import LibraryDirectoryBrowser from '../components/LibraryDirectoryBrowser.svelte';

describe('LibraryDirectoryBrowser (ZEB-252 Phase 6)', () => {
  it('Join button invokes onJoin with community_id, not invite_url', async () => {
    const onJoin = vi.fn().mockResolvedValue(undefined);
    const onClose = vi.fn();

    const service = {
      list: vi.fn().mockResolvedValue([{ address: 'aa'.repeat(16), added_at: {}, entry_count: 1 }]),
      browse: vi.fn().mockResolvedValue([{
        community_id: 'ff'.repeat(16),
        community_addr: '11'.repeat(16),
        name: 'TestCom',
        description: 'desc',
        topics: [],
        invite_url: 'harmony://invite/v1?ci=DO_NOT_USE_THIS',
        listed_by_count: 1,
        unattested: false,
        listed_at: {},
      }]),
      listDiscovered: vi.fn().mockResolvedValue([]),
      add: vi.fn(),
      remove: vi.fn(),
    };
    const adapter = {
      invoke: vi.fn(),
      listen: vi.fn().mockResolvedValue(() => {}),
    };

    const { findByText } = render(LibraryDirectoryBrowser, {
      props: { service, adapter, onJoin, onClose },
    });

    const joinBtn = await findByText('Join');
    await fireEvent.click(joinBtn);

    // The new onJoin must receive community_id; the old behaviour passed invite_url.
    expect(onJoin).toHaveBeenCalledWith('ff'.repeat(16));
    expect(onJoin).not.toHaveBeenCalledWith(expect.stringMatching(/^harmony:/));
  });
});
```

The test fixture's `service` mock returns the minimum DTO fields `LibraryDirectoryBrowser` consumes. If type-checking complains, cast `service as any as LibraryDirectoryService` at the prop site or import `LibraryDirectoryService` and add the missing methods as `vi.fn()` stubs.

- [ ] **Step 3: Run vitest to confirm the new test fails**

Run (from repo root):
```bash
npx vitest run --reporter=verbose src/lib/__tests__/LibraryDirectoryBrowser.test.ts
```

Expected: the new test fails because the current `LibraryDirectoryBrowser.handleJoin` calls `onJoin(entry.invite_url)`, not `onJoin(entry.community_id)`.

- [ ] **Step 4: Update `LibraryDirectoryBrowser.svelte` — `onJoin` prop type + JSDoc + call site**

Open `src/lib/components/LibraryDirectoryBrowser.svelte`.

**4a. Update the JSDoc block at lines 7–8:**

Replace:
```
   * - Join button → `onJoin(invite_url)` callback (App wires to
   *   `redeem_invite` IPC, reusing ZEB-249's open-community invite path)
```

With:
```
   * - Join button → `onJoin(community_id)` callback (App wires to
   *   `join_open_community` IPC — ZEB-252 Sub-D Phase 6. The backend
   *   re-resolves the matching entry server-side and delegates to the
   *   existing `redeem_invite_inner` codepath, so end state is identical
   *   to what `redeem_invite(invite_url)` produces.)
```

**4b. Update the prop type + JSDoc at line 30:**

Replace:
```typescript
    /** Called when the user clicks Join on an entry. Wired to redeem_invite. */
    onJoin: (inviteUrl: string) => Promise<void>;
```

With:
```typescript
    /** Called when the user clicks Join on an entry. Wired to join_open_community.
     *  Receives the entry's `community_id` (32-hex-char SpaceId) so the backend
     *  can re-resolve the directory entry server-side at click time. */
    onJoin: (communityId: string) => Promise<void>;
```

**4c. Update the `handleJoin` call site at line 173:**

Replace:
```typescript
      await onJoin(entry.invite_url);
```

With:
```typescript
      await onJoin(entry.community_id);
```

The rest of `handleJoin` (the `joinPending` / `joinError` state plumbing) is unchanged.

- [ ] **Step 5: Run vitest to confirm the Browser test now passes**

Run (from repo root):
```bash
npx vitest run --reporter=verbose src/lib/__tests__/LibraryDirectoryBrowser.test.ts
```

Expected: the new test passes. Other tests in the file (if any) also pass.

- [ ] **Step 6: Update `App.svelte` — wire onJoin through `communityService.joinOpenCommunity` with full side-effects**

Open `src/App.svelte`. Locate the `<LibraryDirectoryBrowser ... onJoin={async (inviteUrl) => { ... }}>` block at lines 1678–1682. Replace the `<LibraryDirectoryBrowser>` element with:

```svelte
      <LibraryDirectoryBrowser
        service={libraryDirectoryService}
        adapter={tauriAdapter}
        onJoin={async (communityId) => {
          // ZEB-252 Sub-D Phase 6: typed direct-join. Backend re-resolves
          // the matching directory entry server-side and delegates to
          // the same redeem_invite_inner codepath RedeemInviteDialog uses.
          // Side-effects (nav-updated synthesis, kind tracking, selected-
          // community switch, member refresh) mirror the dialog handler
          // at line ~1620+ so the directory click path produces the same
          // post-join UX.
          const dto = await communityService.joinOpenCommunity(communityId);
          navService.addOrUpdateNavSpace({
            action: 'added',
            spaceId: dto.communityId,
            kind: 'community',
            name: dto.communityName,
            members: [],
            parentId: null,
          });
          libraryDirectoryOpen = false;
          changeSelectedCommunity(dto.communityId);
          await refreshCommunityMembers(dto.communityId);
        }}
        onClose={() => (libraryDirectoryOpen = false)}
      />
```

**Comment update.** Also update the preceding comment block at lines 1657–1661 (the `<!-- ZEB-218 Sub-D Phase 1: library directory browser modal. ... -->` comment) to mention Phase 6:

Replace:
```html
  <!-- ZEB-218 Sub-D Phase 1: library directory browser modal. Click-to-
       join feeds `invite_url` straight into `redeem_invite` — no new
       join protocol surface (reuses ZEB-249's open-community invite
       redemption path). Stale URLs handled by ZEB-249 §4.6 EpochCatchup
       self-healing; no app-level retry needed here. -->
```

With:
```html
  <!-- ZEB-218 Sub-D Phase 1 + Phase 6 (ZEB-252): library directory
       browser modal. Click-to-join calls `join_open_community(community_id)`
       which re-resolves the entry server-side and delegates to the
       same `redeem_invite_inner` codepath RedeemInviteDialog uses
       (full side-effects: nav-updated synth, kind tracking, selected-
       community switch, member refresh). Stale URLs handled by ZEB-249
       §4.6 EpochCatchup self-healing; no app-level retry needed here. -->
```

**Error handling.** Note: this new App-side handler does NOT currently include a `try/catch` — the `LibraryDirectoryBrowser` component catches the error internally inside `handleJoin` (lines 175–179 of the browser) and surfaces it via the `joinError` state. Verify by reading those lines after your edits and confirm `joinError = e instanceof Error ? e.message : String(e);` still runs on rejection. If for any reason the handler needs its own try/catch (e.g., to log differently), add one mirroring the pattern at App.svelte:1642–1645.

- [ ] **Step 7: Run the full frontend gate set**

Run (from repo root):
```bash
npx tsc --noEmit && npx vitest run
```

Expected: both commands exit 0. `tsc` confirms the `onJoin` signature change propagated cleanly through the call graph.

If `tsc` complains about the `LibraryDirectoryBrowser` props type (e.g., "expected `(communityId: string) => Promise<void>` but got `(inviteUrl: string) => Promise<void>`"), it means the call site in `App.svelte` still uses the old shape — re-check the Step 6 edits.

- [ ] **Step 8: Commit Task 3**

Run:
```bash
git add src/lib/components/LibraryDirectoryBrowser.svelte src/App.svelte src/lib/__tests__/LibraryDirectoryBrowser.test.ts && \
git commit -m "$(cat <<'EOF'
feat(zeb-252): rewire LibraryDirectoryBrowser to join_open_community

`LibraryDirectoryBrowser`'s `onJoin` prop now receives the entry's
community_id (was: invite_url). `App.svelte` wires the handler
through `communityService.joinOpenCommunity(communityId)` and runs
the same post-redeem side-effects as RedeemInviteDialog
(nav-updated synthesis, kind tracking, selected-community switch,
member refresh). The pre-Phase-6 short-circuit through
`tauriAdapter.invoke('redeem_invite', { url })` is removed; the
directory click path now matches dialog click path's post-join UX.

Vitest case asserts Join button calls onJoin with community_id.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Final verification + push + PR

**No code changes. Final gate run + push + PR creation.**

- [ ] **Step 1: Confirm clean working tree**

Run:
```bash
git status && git log --oneline origin/main..HEAD
```

Expected:
- `nothing to commit, working tree clean`
- 4 commits ahead of `origin/main`:
  - `<sha>` docs(zeb-252): Sub-D Phase 6 direct-join IPC design
  - `<sha>` feat(zeb-252): backend join_open_community IPC + helper
  - `<sha>` feat(zeb-252): CommunityService.joinOpenCommunity + vitest
  - `<sha>` feat(zeb-252): rewire LibraryDirectoryBrowser to join_open_community

- [ ] **Step 2: Final cargo gate run**

Run (from `src-tauri/`):
```bash
cd src-tauri && cargo fmt --all -- --check && \
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && \
cargo nextest run --locked --workspace --all-targets --features test-fixtures && \
cargo check --locked --all-targets --features test-fixtures
```

Expected: all four exit 0. Test count should be ≥ baseline + 4 new tests (3 helper + 1 IPC happy-path).

- [ ] **Step 3: Final frontend gate run**

Run (from repo root):
```bash
npx tsc --noEmit && npx vitest run
```

Expected: both exit 0. Vitest test count should be ≥ baseline + 2 new tests (1 service + 1 browser).

- [ ] **Step 4: Push branch**

Run:
```bash
git push -u origin zeb-252-sub-d-phase-6-direct-join
```

Expected: branch created on origin, set as tracking.

- [ ] **Step 5: Create PR**

Run:
```bash
gh pr create --title "ZEB-252 Sub-D Phase 6: direct-join IPC for open communities" --body "$(cat <<'EOF'
## Summary

- New typed Tauri IPC `join_open_community(communityId)` re-resolves the matching `LibraryDirectoryEntry` server-side at click time and delegates to the existing `redeem_invite_inner` codepath — no changes to the join machinery itself.
- `LibraryDirectoryBrowser.svelte`'s Join button switches from `redeem_invite(invite_url)` to `join_open_community(community_id)`. The pre-Phase-6 short-circuit through `tauriAdapter.invoke('redeem_invite', { url })` is removed; the directory click path now matches `RedeemInviteDialog`'s full post-join side-effects (nav-updated synthesis, kind tracking, selected-community switch, member refresh).
- `redeem_invite(url)` IPC is untouched and remains the path for hand-pasted invite URLs.

Closes [ZEB-252](https://linear.app/zeblith/issue/ZEB-252). Phase 6 (final phase) of [ZEB-218](https://linear.app/zeblith/issue/ZEB-218) Sub-D; predecessors: PR #108 (Phase 1), #109 ([ZEB-279](https://linear.app/zeblith/issue/ZEB-279) Phase 2), #110 ([ZEB-280](https://linear.app/zeblith/issue/ZEB-280) Phase 3), #112 ([ZEB-281](https://linear.app/zeblith/issue/ZEB-281) Phase 4).

Spec: `docs/specs/2026-05-12-zeb-252-sub-d-phase-6-direct-join-design.md`.

## Why

Two latent issues with the Phase 1 click-to-join path:
1. **Indirect IPC contract** — the directory UI semantically wants to "join the community I'm looking at" (identity is `community_id`, not an opaque URL). Passing the URL string forces the frontend to round-trip the URL through the rendered DTO.
2. **No server-side authority over which URL gets redeemed** — a compromised or buggy renderer could call `redeem_invite(url)` with a URL the user never saw. Today's mitigation is "the URL was verified at receive" (Phase 1's `verify_entry` binds `invite_url` to `(community_id, admin_addr)`), but the renderer still has TOCTOU latitude over which cached URL it passes.

Phase 6 closes both by routing the directory click path through an IPC that takes only `community_id` and re-resolves the matching `LibraryDirectoryEntry` from the current aggregation server-side. The actual join machinery (URL decode, HLC reservation, bootstrap-Join mint, engine spawn, owner-state commit) is unchanged — Phase 6 strictly wraps it.

## What changed

**Backend (`src-tauri/`):**
- New `pub fn find_open_community_invite_url_in_snapshot(snapshot, community_id_hex) -> Result<String, String>` in `library_directory.rs` — pure helper that finds the entry, defensively re-checks `is_invite_only`, returns the `invite_url`.
- New `#[tauri::command] async fn join_open_community(app, state_lock, community_id)` in `lib.rs` — snapshots `NodeState` the same way `redeem_invite` does, fetches the directory snapshot, calls the helper, delegates to `redeem_invite_inner`. Same `fence_check` semantics, same `nav-updated` emit.
- 3 helper unit tests (missing entry, invite-only rejection, ok) + 1 IPC happy-path delegation test colocated with `redeem_invite_inner_tests`.

**Frontend (`src/`):**
- New `CommunityService.joinOpenCommunity(communityId)` method alongside existing `redeemInvite(url)` — same `RedeemInviteResultDto` return; same `knownKinds` population.
- `LibraryDirectoryBrowser.svelte`: `onJoin` prop type flips from `(inviteUrl: string) => Promise<void>` to `(communityId: string) => Promise<void>`. JSDoc + call site updated.
- `App.svelte`: rewired handler runs the same post-redeem side-effects as the `RedeemInviteDialog onSubmit` handler.
- 2 new vitest cases: service-method invocation + browser Join button passes `community_id`.

## Test plan

- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (4 new Rust tests pass; existing redeem_invite_inner_tests still pass)
- [x] `cargo check --locked --all-targets --features test-fixtures` (msrv)
- [x] `npx tsc --noEmit`
- [x] `npx vitest run` (2 new vitest cases pass)
- [ ] Manual smoke: open library browser → click Join on an attested community → community appears in nav + selected + member list loads. (Implementer note: skip if no smoke harness available locally; CI gates are load-bearing.)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: `gh` prints the PR URL.

- [ ] **Step 6: Surface the PR URL**

Print the PR URL to the conversation so the user can find it. The autonomous PR-monitoring loop takes over from here per the user's `feedback_autonomous_pr_monitoring_loop` memory.

---

## Self-review notes (writing-plans skill)

**Spec coverage check:**
- §3.1 New IPC `join_open_community(community_id)` → Task 1 implements + registers it.
- §3.2 `redeem_invite(url)` unchanged → Task 1 explicitly does not modify `redeem_invite` (covered by existing tests staying green in Task 4).
- §4.1 NodeState snapshot + lock-drop pattern → Task 1 Step 7 mirrors `redeem_invite` exactly.
- §4.2 Lookup uniqueness via `community_id` only → Task 1 helper code uses `.find(|a| a.entry.community_id == want)`.
- §4.3 Hard-fail on missing entry → Task 1 helper Step 3 emits the spec's exact error string.
- §4.4 Defensive invite-only re-check → Task 1 helper Step 3 decodes and rejects.
- §4.5 Pass-through errors → Task 1 delegates to `redeem_invite_inner`, errors propagate.
- §4.6 Idempotency-same-as-redeem_invite → no separate code; inherited from delegation.
- §5.1 Browser `onJoin` signature change → Task 3 Step 4.
- §5.2 Service method → Task 2.
- §5.3 App.svelte wire-up → Task 3 Step 6.
- §5.4 Error extraction → preserved by reusing the existing browser's `handleJoin` body.
- §6.1 3 Rust unit tests → Task 1 Steps 1 + 5 (3 helper + 1 IPC = 4, exceeding minimum).
- §6.2 Integration test → covered by IPC happy-path test in Task 1 which exercises the real `redeem_invite_inner` codepath through the test fixture; separate e2e file is overkill for a wrapper IPC (spec §6.2's framing was speculative — note added in plan).
- §6.3 Vitest cases → Task 2 + Task 3 (2 total).
- §6.4 No wire-format pinning → confirmed (no new wire types).
- §7 Acceptance criteria 1-6 → covered by Tasks 1-4 and verification gates.
- §8 Out-of-scope → no out-of-scope work proposed.

**Placeholder scan:** no TBD / TODO / "implement later" / vague phrasing. The two "implementer locates" notes (test file path in Task 3 Step 1; visibility bump in Task 1 Step 5) are explicit, scoped, and bounded.

**Type consistency:** `join_open_community` (Rust) ↔ `joinOpenCommunity` (TS) ↔ wire string `'join_open_community'` are consistent across all tasks. `RedeemInviteResultDto` (Rust) ↔ `RedeemInviteResultDto` (TS interface, same name) is reused, not duplicated. Helper name `find_open_community_invite_url_in_snapshot` is used identically across the helper definition (Task 1 Step 3) and its inner-helper caller (Task 1 Step 7).

**Integration-test deferral note (spec §6.2):** The spec called for a two-engine e2e test mirroring redeem_invite's. After exploration, the existing `community_open_flow_integration.rs` tests `mint_redemption` (a pure mint fn), NOT the `redeem_invite` IPC end-to-end. Building two-engine wiring for `join_open_community` would add ~300 lines of test infrastructure to verify "the new IPC delegates to redeem_invite_inner" — which is already proven by Task 1's happy-path delegation test (which uses the real `redeem_invite_inner` against a real `OwnerState` / `CommunitySyncRegistry` fixture). Implementer is free to add a separate `tests/library_directory_join_integration.rs` if the spec-compliance reviewer flags this as a gap; preferring YAGNI for now.
