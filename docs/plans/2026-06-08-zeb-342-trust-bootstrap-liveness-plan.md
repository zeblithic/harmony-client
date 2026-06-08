# ZEB-342 Trust-Bootstrap Liveness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop a freshly-minted (and any existing) sole device from showing a red "● refused" trust badge by giving the local device a fresh `LivenessCert` so `evaluate_trust` returns `Full`.

**Architecture:** Two repos. Upstream `harmony` (`harmony-owner` crate): `mint_owner` publishes device #1's liveness at mint. Client `harmony-client`: a `refresh_self_liveness` helper re-publishes/refreshes the local device's liveness on owner-state load, persisted via a new keychain-safe `save_owner_state_cbor_only` writer under `OWNER_STATE_WRITE_LOCK`. The upstream change fixes new identities; the client refresh fixes existing ones and self-heals before the 30-day freshness window lapses.

**Tech Stack:** Rust, `harmony-owner` (trust/liveness CRDT), Tauri IPC, `cargo nextest`, Playwright/CDP for live-verify.

**Spec:** `docs/specs/2026-06-08-zeb-342-trust-bootstrap-liveness-design.md`

---

## File Structure

**Repo A — `harmony` (`/c/zeblith/work/zeblithic/harmony`):**
- Modify: `crates/harmony-owner/src/lifecycle/mint.rs` — `mint_owner` publishes initial liveness; strengthen `mint_produces_active_device_one`.

**Repo B — `harmony-client` (`/c/zeblith/work/zeblithic/harmony-client`):**
- Modify: `src-tauri/src/owner_state.rs` — add `refresh_self_liveness` + `save_owner_state_cbor_only` + their unit tests.
- Modify: `src-tauri/src/owner_commands.rs` — wire the refresh into `get_owner_state` under the write lock.
- Modify: `src-tauri/Cargo.toml` — bump the seven `harmony-*` git deps `04449d6` → merged Repo-A rev.

**Sequencing:** Task 1 (Repo A) opens its own PR. Tasks 2–4 (Repo B core) develop in parallel against the current `04449d6` rev — they do not depend on Task 1. Task 5 (dep bump) is **gated on Task 1 merging**. Tasks 6–7 finish the client PR.

---

## Task 1: Upstream — `mint_owner` publishes device #1 liveness (Repo A)

**Files:**
- Modify: `crates/harmony-owner/src/lifecycle/mint.rs` (in `/c/zeblith/work/zeblithic/harmony`)

- [ ] **Step 1: Branch off harmony main**

```bash
git -C /c/zeblith/work/zeblithic/harmony fetch origin --quiet
git -C /c/zeblith/work/zeblithic/harmony checkout -b zeb-342-mint-self-liveness origin/main
```

- [ ] **Step 2: Strengthen the failing test**

In `crates/harmony-owner/src/lifecycle/mint.rs`, replace the existing `mint_produces_active_device_one` test body with:

```rust
    #[test]
    fn mint_produces_active_device_one() {
        let now = 1_700_000_000;
        let result = mint_owner(now).unwrap();
        assert_eq!(result.state.enrollments.len(), 1);
        assert_eq!(result.recovery_artifact.as_bytes().len(), 32);
        // ZEB-342: the sole minted device must be alive + trusted, not Refused.
        assert_eq!(result.state.liveness.len(), 1, "device #1 must have an initial liveness cert");
        let device_id = *result.state.enrollments.keys().next().unwrap();
        assert_eq!(
            crate::trust::evaluate_trust(
                &result.state,
                device_id,
                now,
                crate::trust::DEFAULT_ACTIVE_WINDOW_SECS,
                crate::trust::DEFAULT_FRESHNESS_WINDOW_SECS,
            ),
            crate::trust::TrustDecision::Full,
            "freshly-minted sole device must evaluate to Full trust"
        );
    }
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p harmony-owner --manifest-path /c/zeblith/work/zeblithic/harmony/Cargo.toml mint_produces_active_device_one`
Expected: FAIL — `assertion failed: result.state.liveness.len() == 1` (currently 0), or the `evaluate_trust` assert showing `Refused(StaleTrustState)` != `Full`.

- [ ] **Step 4: Add the liveness publish to `mint_owner`**

In `mint_owner`, immediately after the `state.add_enrollment(cert, now, crate::trust::DEFAULT_ACTIVE_WINDOW_SECS)?;` line and before `let recovery_artifact = RecoveryArtifact::from_seed(seed);`, insert:

```rust
    // ZEB-342: device #1 is alive at mint. Without an initial liveness cert the
    // device is not "active", so evaluate_trust refuses the sole device with
    // StaleTrustState (the fresh-mint "● refused" badge). device_sk + owner_id
    // are already in scope here.
    let liveness = crate::certs::LivenessCert::sign(&device_sk, owner_id, now)?;
    state.add_liveness(liveness)?;
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p harmony-owner --manifest-path /c/zeblith/work/zeblithic/harmony/Cargo.toml mint_produces_active_device_one`
Expected: PASS.

- [ ] **Step 6: Run the full harmony-owner test module + fmt/clippy**

Run:
```bash
cargo test -p harmony-owner --manifest-path /c/zeblith/work/zeblithic/harmony/Cargo.toml
cargo fmt --manifest-path /c/zeblith/work/zeblithic/harmony/Cargo.toml -p harmony-owner -- --check
cargo clippy -p harmony-owner --manifest-path /c/zeblith/work/zeblithic/harmony/Cargo.toml -- -D warnings
```
Expected: all green (the existing `lifecycle::mint` and `trust` tests still pass; the mint change is additive).

- [ ] **Step 7: Commit + open the Repo-A PR**

```bash
git -C /c/zeblith/work/zeblithic/harmony add crates/harmony-owner/src/lifecycle/mint.rs
git -C /c/zeblith/work/zeblithic/harmony commit -m "$(cat <<'EOF'
fix(owner): mint_owner publishes device #1 liveness (ZEB-342)

A freshly-minted sole device had no LivenessCert, so evaluate_trust's
freshness gate refused it with StaleTrustState (the client's fresh-mint
"● refused" badge). Publish device #1's liveness at mint so it is active
and evaluates to Full. Strengthen mint_produces_active_device_one to
assert liveness present + evaluate_trust == Full.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
git -C /c/zeblith/work/zeblithic/harmony push -u origin zeb-342-mint-self-liveness
gh --repo zeblithic/harmony pr create --title "fix(owner): mint_owner publishes device #1 liveness (ZEB-342)" --body "<see PR body below>"
```
PR body should explain the root cause (freshness gate refuses a device with no liveness), the one-line fix, and link ZEB-342. **Record the eventual squash-merge commit SHA — Task 5 needs it.**

---

## Task 2: Client — `save_owner_state_cbor_only` writer (Repo B)

**Files:**
- Modify: `src-tauri/src/owner_state.rs`

- [ ] **Step 1: Write the failing test**

In the `#[cfg(test)] mod tests` block of `src-tauri/src/owner_state.rs` that already uses `mint_owner` + `tempdir` (near the `serial` tests), add:

```rust
    #[test]
    #[serial]
    fn cbor_only_persists_state_without_touching_keychain() {
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "cbor-only-test-pp");
        let dir = tempdir().unwrap();
        let MintResult { mut state, recovery_artifact, device_signing_key } =
            mint_owner(1_700_000_111).unwrap();
        // Full save first: writes device_sk + master_seed keychain entries + cbor.
        save_owner_state_atomic(dir.path(), &state, &device_signing_key,
            Some(recovery_artifact.as_bytes()), None).unwrap();

        // Mutate the CRDT (simulate a liveness refresh) and persist cbor-only.
        state.liveness.clear();
        save_owner_state_cbor_only(dir.path(), &state).unwrap();

        // Reload: cbor reflects the mutation AND the master seed survived.
        let loaded = load_owner_state(dir.path(), None).unwrap().expect("must be Some");
        assert_eq!(loaded.state.liveness.len(), 0, "cbor-only write must persist the CRDT mutation");
        assert!(loaded.master_seed.is_some(), "cbor-only write must NOT clear the master seed");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(cbor_only_persists_state_without_touching_keychain)'`
Expected: FAIL to compile — `save_owner_state_cbor_only` is not defined.

- [ ] **Step 3: Implement the writer**

In `src-tauri/src/owner_state.rs`, after `save_owner_state_atomic` (ends ~line 461), add:

```rust
/// Persist only the OwnerState CRDT to `owner_state.cbor` (canonical CBOR,
/// atomic 0600). Unlike `save_owner_state_atomic`, this does NOT touch the
/// `device_signing_key` / `master_seed` keychain entries — it is for callers
/// (e.g. the ZEB-342 liveness refresh) that mutate only the CRDT and must not
/// risk clearing the master seed. Callers MUST hold `OWNER_STATE_WRITE_LOCK`.
pub fn save_owner_state_cbor_only(identity_dir: &Path, state: &OwnerState) -> Result<(), String> {
    let cbor_bytes =
        cbor::to_canonical(state).map_err(|e| format!("CBOR encode of OwnerState failed: {e}"))?;
    let cbor_path = identity_dir.join(OWNER_STATE_FILENAME);
    write_atomic_0600(&cbor_path, &cbor_bytes)
        .map_err(|e| format!("failed to write {}: {e}", cbor_path.display()))?;
    Ok(())
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(cbor_only_persists_state_without_touching_keychain)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_state.rs
git commit -m "$(cat <<'EOF'
feat(zeb-342): keychain-safe cbor-only owner-state writer

save_owner_state_cbor_only persists only the OwnerState CRDT to
owner_state.cbor, leaving the device_sk/master_seed keychain entries
untouched — for the liveness refresh, which must not risk the master
seed via save_owner_state_atomic's None-clears-seed branch.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Client — `refresh_self_liveness` helper (Repo B)

**Files:**
- Modify: `src-tauri/src/owner_state.rs`

- [ ] **Step 1: Write the failing tests**

In the same `#[cfg(test)] mod tests` block, add:

```rust
    #[test]
    fn refresh_self_liveness_publishes_when_missing_then_full() {
        let now = 1_700_000_222;
        let MintResult { mut state, device_signing_key, .. } = mint_owner(now).unwrap();
        state.liveness.clear(); // simulate a legacy identity with no liveness
        let device_id = *state.enrollments.keys().next().unwrap();

        let mutated = refresh_self_liveness(&mut state, &device_signing_key, now);
        assert!(mutated, "missing liveness must trigger a publish");
        assert_eq!(state.liveness.len(), 1);
        assert_eq!(
            harmony_owner::trust::evaluate_trust(
                &state, device_id, now,
                harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS,
                harmony_owner::trust::DEFAULT_FRESHNESS_WINDOW_SECS,
            ),
            harmony_owner::trust::TrustDecision::Full,
        );
    }

    #[test]
    fn refresh_self_liveness_is_noop_when_fresh() {
        let now = 1_700_000_333;
        let MintResult { mut state, device_signing_key, .. } = mint_owner(now).unwrap();
        // mint_owner (post-ZEB-342) already stamps fresh liveness; ensure one exists.
        refresh_self_liveness(&mut state, &device_signing_key, now);
        let mutated = refresh_self_liveness(&mut state, &device_signing_key, now);
        assert!(!mutated, "fresh liveness must NOT be re-published");
    }

    #[test]
    fn refresh_self_liveness_resigns_when_stale() {
        let mint_t = 1_700_000_000;
        let MintResult { mut state, device_signing_key, .. } = mint_owner(mint_t).unwrap();
        refresh_self_liveness(&mut state, &device_signing_key, mint_t);
        let device_id = *state.enrollments.keys().next().unwrap();
        let old_ts = state.liveness.get(&device_id).unwrap().timestamp;

        // Advance past the refresh threshold (freshness / 2).
        let later = mint_t + harmony_owner::trust::DEFAULT_FRESHNESS_WINDOW_SECS / 2 + 1;
        let mutated = refresh_self_liveness(&mut state, &device_signing_key, later);
        assert!(mutated, "stale liveness must be re-signed");
        assert!(state.liveness.get(&device_id).unwrap().timestamp > old_ts);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(refresh_self_liveness)'`
Expected: FAIL to compile — `refresh_self_liveness` is not defined.

- [ ] **Step 3: Implement the helper**

In `src-tauri/src/owner_state.rs`, add near `save_owner_state_cbor_only` (these are the two ZEB-342 helpers; keep them together):

```rust
/// Ensure the local device (derived from `device_sk`) has a fresh `LivenessCert`
/// in `state`. Returns `true` if it mutated `state` (caller must then persist via
/// `save_owner_state_cbor_only`). Publishes when the local device has no liveness
/// or its liveness is older than `DEFAULT_FRESHNESS_WINDOW_SECS / 2` (~15 days),
/// bounding writes to ~once per boot per fortnight. On a signing/add error it
/// logs at warn and returns `false` — the panel falls back to today's behavior
/// rather than failing.
pub fn refresh_self_liveness(
    state: &mut OwnerState,
    device_sk: &SigningKey,
    now: u64,
) -> bool {
    use harmony_owner::certs::LivenessCert;
    use harmony_owner::pubkey_bundle::PubKeyBundle;

    let device_id =
        PubKeyBundle::classical_only(device_sk.verifying_key().to_bytes()).identity_hash();
    let threshold = harmony_owner::trust::DEFAULT_FRESHNESS_WINDOW_SECS / 2;
    let stale = match state.liveness.get(&device_id) {
        Some(c) => c.timestamp < now.saturating_sub(threshold),
        None => true,
    };
    if !stale {
        return false;
    }
    let cert = match LivenessCert::sign(device_sk, state.owner_id, now) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "refresh_self_liveness: liveness sign failed");
            return false;
        }
    };
    match state.add_liveness(cert) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, "refresh_self_liveness: add_liveness failed");
            false
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(refresh_self_liveness)'`
Expected: PASS (all three).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_state.rs
git commit -m "$(cat <<'EOF'
feat(zeb-342): refresh_self_liveness publishes local device liveness

Publishes/refreshes the local device's LivenessCert so evaluate_trust
sees an active device and returns Full instead of refusing the sole
device with StaleTrustState. Refresh-if-stale (threshold = freshness/2)
bounds disk writes; fails open to current behavior on error.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Client — wire refresh into `get_owner_state` (Repo B)

**Files:**
- Modify: `src-tauri/src/owner_commands.rs`
- Test: `src-tauri/src/owner_state.rs` (load→refresh→save→reload sequence; `get_owner_state` itself is hard to unit-test due to `resolve_identity_dir` reading `HOME`)

> **TDD note:** the `get_owner_state` command (tauri + `HOME` coupling) has no direct unit test; its wiring is verified live in Task 6. The on-disk **sequence** it performs (load→refresh→persist→reload) is what we regression-test here. Because that sequence uses the helpers already built in Tasks 2–3, this test goes **green immediately** — it is a regression guard for the sequence + the master-seed-survival guarantee, not a red-first test for new logic. Write the wiring (Step 3) in the same task.

- [ ] **Step 1: Write the on-disk sequence regression test**

In `src-tauri/src/owner_state.rs` `#[cfg(test)] mod tests`, add:

```rust
    #[test]
    #[serial]
    fn legacy_identity_self_heals_to_full_on_load_and_persists() {
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "self-heal-test-pp");
        let dir = tempdir().unwrap();
        // Seed a legacy on-disk identity: enrolled device, NO liveness, has master seed.
        let MintResult { mut state, recovery_artifact, device_signing_key } =
            mint_owner(1_700_000_444).unwrap();
        state.liveness.clear();
        save_owner_state_atomic(dir.path(), &state, &device_signing_key,
            Some(recovery_artifact.as_bytes()), None).unwrap();

        // Simulate get_owner_state's load→refresh→persist sequence.
        let now = 1_700_000_500;
        let mut loaded = load_owner_state(dir.path(), None).unwrap().expect("Some");
        let device_id = *loaded.state.enrollments.keys().next().unwrap();
        assert_eq!(
            harmony_owner::trust::evaluate_trust(&loaded.state, device_id, now,
                harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS,
                harmony_owner::trust::DEFAULT_FRESHNESS_WINDOW_SECS),
            harmony_owner::trust::TrustDecision::Refused(
                harmony_owner::trust::RefusalReason::StaleTrustState),
            "precondition: legacy identity is Refused before refresh"
        );

        if refresh_self_liveness(&mut loaded.state, &loaded.device_signing_key, now) {
            save_owner_state_cbor_only(dir.path(), &loaded.state).unwrap();
        }

        // Reload from disk: now Full + persisted + master seed intact.
        let reloaded = load_owner_state(dir.path(), None).unwrap().expect("Some");
        assert_eq!(reloaded.state.liveness.len(), 1, "liveness must be persisted");
        assert_eq!(
            harmony_owner::trust::evaluate_trust(&reloaded.state, device_id, now,
                harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS,
                harmony_owner::trust::DEFAULT_FRESHNESS_WINDOW_SECS),
            harmony_owner::trust::TrustDecision::Full,
        );
        assert!(reloaded.master_seed.is_some(), "master seed must survive the refresh-persist");
    }
```

- [ ] **Step 2: Run it — expect PASS (validates Tasks 2–3 compose on disk)**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(legacy_identity_self_heals_to_full_on_load_and_persists)'`
Expected: PASS — the test drives the helpers from Tasks 2–3 directly through a load→refresh→persist→reload cycle. If it fails to compile on `RefusalReason`, that enum is not re-exported at `harmony_owner::trust::RefusalReason`; relax the precondition assert to `assert!(matches!(decision, harmony_owner::trust::TrustDecision::Refused(_)))` where `decision` is bound from the `evaluate_trust` call.

- [ ] **Step 3: Wire the refresh into `get_owner_state`**

In `src-tauri/src/owner_commands.rs`, add `refresh_self_liveness` and `save_owner_state_cbor_only` to the `use crate::owner_state::{…}` import list (lines 8–11), then replace the `get_owner_state` body (lines 134–142) with:

```rust
#[tauri::command]
pub async fn get_owner_state(_app: tauri::AppHandle) -> Result<Option<OwnerStateView>, String> {
    let identity_dir = resolve_identity_dir()?;
    let display_name = "this device".to_string();
    run_blocking(move || {
        // Hold the write lock across load+refresh+save so the cbor write stays
        // serialized with mint / pairing-install (ZEB-342). Loading inside the
        // lock closes the read-modify-write race.
        let _guard = OWNER_STATE_WRITE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut loaded = match load_owner_state(&identity_dir, KeychainStore::new().ok())? {
            Some(l) => l,
            None => return Ok(None),
        };
        if refresh_self_liveness(&mut loaded.state, &loaded.device_signing_key, now_unix()) {
            save_owner_state_cbor_only(&identity_dir, &loaded.state)?;
        }
        Ok(Some(build_owner_state_view(&loaded, display_name)))
    })
    .await
}
```

- [ ] **Step 4: Run the test + the broader owner-state module**

Run:
```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(owner_state) + test(refresh_self_liveness) + test(legacy_identity_self_heals)'
```
Expected: PASS. Then fmt + clippy:
```bash
cargo fmt --all -- --check
cargo clippy --locked -p harmony-app --features test-fixtures --no-deps -- -D warnings
```
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_state.rs src-tauri/src/owner_commands.rs
git commit -m "$(cat <<'EOF'
feat(zeb-342): get_owner_state refreshes local device liveness on load

Loads under OWNER_STATE_WRITE_LOCK, refreshes the local device's
liveness, and persists via the cbor-only writer when mutated. Existing
liveness-less identities now self-heal to Full on the next Devices-panel
open; master seed is never touched.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Client — bump `harmony-*` deps to merged Repo-A rev (gated on Task 1 merge)

**Files:**
- Modify: `src-tauri/Cargo.toml` (lines 81–87), `src-tauri/Cargo.lock`

- [ ] **Step 1: Confirm Task 1 is merged and capture the new rev**

```bash
git -C /c/zeblith/work/zeblithic/harmony fetch origin --quiet
git -C /c/zeblith/work/zeblithic/harmony log --oneline -3 origin/main   # note the squash-merge SHA
```
Let `<NEWREV>` = the full 40-char merge commit SHA.

- [ ] **Step 2: Update the seven `harmony-*` git revs**

In `src-tauri/Cargo.toml`, change the `rev = "04449d603c042c121ee9836ebd244310adaf7f6a"` on exactly these seven deps (lines 81–87) to `rev = "<NEWREV>"`: `harmony-runtime`, `harmony-identity`, `harmony-content`, `harmony-compute`, `harmony-telemetry`, `harmony-mailbox`, `harmony-owner`. **Do NOT touch `harmony-pkarr`** (lines 96/166) — it is intentionally pinned at its own rev `2aaf403…`.

- [ ] **Step 3: Refresh Cargo.lock + build**

Run:
```bash
cd src-tauri && cargo update -p harmony-owner --precise <NEWREV> 2>/dev/null || true
cargo build --locked -p harmony-app
```
If `cargo update --precise` complains because all seven share a source, instead run `cargo build` (no `--locked`) once to let cargo re-resolve, then re-add `--locked`. Confirm `Cargo.lock` now shows `<NEWREV>` for the seven crates.

- [ ] **Step 4: Add a bump-tripwire test proving `mint_owner` alone now stamps liveness**

This test asserts the *upstream* behavior is compiled in — it would have failed at the old `04449d6` rev and passes only after the bump (so it also guards against a future accidental downgrade). In `src-tauri/src/owner_state.rs` `#[cfg(test)] mod tests`, add:

```rust
    #[test]
    fn bumped_mint_owner_stamps_initial_liveness() {
        // ZEB-342: post-bump, mint_owner publishes device #1 liveness WITHOUT
        // any client-side refresh. Tripwire against a dep downgrade.
        let result = mint_owner(1_700_000_777).unwrap();
        assert_eq!(result.state.liveness.len(), 1, "bumped mint_owner must stamp liveness");
    }
```

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(bumped_mint_owner_stamps_initial_liveness)'`
Expected: PASS (would FAIL at the pre-bump rev). Also confirm `Cargo.lock` shows `<NEWREV>` for the seven crates.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/owner_state.rs
git commit -m "$(cat <<'EOF'
chore(zeb-342): bump harmony-* deps to mint-self-liveness rev

Picks up mint_owner's initial liveness publish so newly-minted
identities are born active/Full. Adds a tripwire test asserting
mint_owner stamps liveness. harmony-pkarr stays pinned.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Live-verify on Ildwyn (Playwright/CDP, isolated throwaway HOME)

**Files:**
- Create (gitignored, NEVER commit): `.playwright-scratch/zeb342-verify.mjs`

- [ ] **Step 1: Launch the isolated dev app**

Per `[[driving-harmony-tauri-apps-headless-on-windows-keychain-vs-passphrase]]`: set `$env:HOME` to a fresh temp dir, `HARMONY_PASSPHRASE_FILE` to `.playwright-scratch/smoke-passphrase.txt`, `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222`, `RUST_MIN_STACK=8388608`, then `npm run tauri dev` (background). Poll until 9222 is listening.

- [ ] **Step 2: Onboard + read the Devices panel**

Reuse `zeb393-drive.mjs onboard` to create the identity, then write `.playwright-scratch/zeb342-verify.mjs` to: invoke `get_owner_state` via `window.__TAURI_INTERNALS__.invoke('get_owner_state')` and assert `devices[0].trustDecision.kind === 'full'`; also navigate Profile → Devices in the UI and screenshot the badge.

- [ ] **Step 3: Assert trusted, not refused**

Run: `node .playwright-scratch/zeb342-verify.mjs`
Expected: `trustDecision.kind: "full"`, no red "refused"; screenshot `.playwright-scratch/zeb342-trusted.png` shows the device badge as trusted. **Look at the screenshot** — a blank frame is a failure.

- [ ] **Step 4: (Optional) existing-identity self-heal**

Stop the app; with a Rust scratch or by reusing the throwaway, confirm an `owner_state.cbor` that had its liveness cleared comes back `full` after a relaunch (the unit test `legacy_identity_self_heals_to_full_on_load_and_persists` already proves this deterministically; the live check is corroboration).

- [ ] **Step 5: Tear down**

Stop the app + this session's node procs (spare the morning MCP nodes), remove the temp HOME, confirm real `~/.harmony` untouched.

---

## Task 7: Client PR + ticket

- [ ] **Step 1: Full local gate (no app running → no exe lock)**

Run:
```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo fmt --all -- --check
cd .. && npx tsc --noEmit && npx vitest run
```
Expected: all green (note: the Windows-flaky `profile_broadcast::publisher_debounce_coalesces_rapid_toggles` may fail locally — confirm via `git diff origin/main` that the file is untouched; it passes on Linux CI).

- [ ] **Step 2: Push + open the client PR**

```bash
git push -u origin zeb-342-trust-bootstrap-liveness
gh --repo zeblithic/harmony-client pr create --title "ZEB-342: trust-bootstrap liveness — fix fresh-mint \"● refused\" badge" --body "<body>"
```
Body: root cause (no liveness → StaleTrustState refusal), the two-surface fix, the cbor-only/keychain-safety note, the live-verify screenshot, and a link to the merged Repo-A PR. End with the Claude Code generated-by line.

- [ ] **Step 3: Update Linear ZEB-342 → In Review** with a comment summarizing the root cause + both PRs.

- [ ] **Step 4: Monitor CI + bot reviews** (CodeRabbit / Cursor / Qodo / CodeAnt; Greptile excluded as author), address feedback per the established pipeline. Jake merges.

---

## Self-Review notes
- **Spec coverage:** Change set A → Task 1; Change set B helper → Tasks 2–3; call-site/lock → Task 4; dep bump/sequencing → Task 5; live-verify → Task 6; tests (mint, refresh missing/fresh/stale, get_owner_state-level, master-seed survival) → Tasks 1–4; out-of-scope multi-device sync → not built (follow-up note in PR).
- **Follow-up to file (not built here):** ongoing multi-device liveness heartbeat that propagates liveness to paired devices.
