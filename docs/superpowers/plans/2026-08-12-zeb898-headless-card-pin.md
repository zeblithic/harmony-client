# ZEB-898 (re-scoped) Implementation Plan — headless card-flow regression pins + optional `statusText`

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pin the headless stash→mint→drain owner-card flow with regression tests, and make `republish_owner_card`'s `statusText` optional on the RPC surface.

**Architecture:** No production behavior change except one `#[serde(default)]` on an RPC args struct. Two new regression tests pin the joints of the already-working flow (`stop_inner` latch survival; full stash → mint with real `start_node_inner` restart → published card), following the `zeb_687_revoked_feed_boot_tests` full-boot pattern in `lib.rs`.

**Tech Stack:** Rust, tokio, cargo-nextest, serial_test, ciborium.

## Global Constraints

- Cargo commands run from `src-tauri/`; always `--locked` and `--features test-fixtures` (spec §4, CLAUDE.md).
- Clippy gate: `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; fmt gate: `cargo fmt --all -- --check`.
- Never construct `KeychainStore::new()` in test-reachable code; tests set `HARMONY_PASSPHRASE` + `HARMONY_DISABLE_KEYCHAIN=1` via `EnvVarGuard` (ZEB-428).
- Tasks 1–2 are **regression pins of already-fixed behavior**: their tests pass on first run by design (the "red" is historical — pre-#635 binaries fail this flow). Task 3 is a true red→green.
- Spec: `docs/superpowers/specs/2026-08-12-zeb898-headless-card-pin-design.md`.

---

### Task 1: `stop_inner` preserves the pending-card latch (unit pin)

**Files:**
- Modify: `src-tauri/src/lib.rs` — append test to `mod pending_owner_card_tests` (module starts ~line 15966, ends ~16024).

**Interfaces:**
- Consumes: `NodeState::default()`, `PendingCard { display_name, status_text, avatar_cid, profile_page_root }`, `crate::stop_inner(&Mutex<NodeState>, Option<u64>) -> bool` — all crate-private, reachable from this in-crate test module.
- Produces: nothing downstream; standalone pin.

- [ ] **Step 1: Write the test** (append inside `mod pending_owner_card_tests`, after `drain_pending_owner_card_leaves_latch_when_runtime_not_ready`):

```rust
    /// ZEB-898: mint's Phase 1 calls `stop_inner` before the mint+restart, and
    /// the fresh-mint headless flow depends on the boot-stashed card SURVIVING
    /// that stop so the post-mint start's drain can publish it. Pin it: a
    /// "clear stale state" sweep added to `stop_inner` would silently
    /// reintroduce the ZEB-898 field failure (peers resolve hex forever).
    #[tokio::test]
    async fn stop_inner_preserves_pending_card_latch() {
        let state = std::sync::Mutex::new(NodeState::default());
        state.lock().unwrap().pending_card = Some(PendingCard {
            display_name: "Zeb898".to_string(),
            status_text: String::new(),
            avatar_cid: None,
            profile_page_root: None,
        });
        // None = unconditional stop, exactly what mint Phase 1 passes.
        crate::stop_inner(&state, None);
        let guard = state.lock().unwrap();
        let pending = guard
            .pending_card
            .as_ref()
            .expect("stop_inner must leave the pending-card latch for the next start");
        assert_eq!(pending.display_name, "Zeb898");
    }
```

- [ ] **Step 2: Run it — expect PASS (regression pin; red is historical)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(=stop_inner_preserves_pending_card_latch)'`
Expected: PASS (1 test run). If it FAILS, stop — `stop_inner` behavior changed since the audit; re-audit before proceeding.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "test(ZEB-898): pin stop_inner preserving the pending-card latch"
```

---

### Task 2: full headless stash→mint→drain flow publishes the card (integration pin)

**Files:**
- Modify: `src-tauri/src/lib.rs` — new module `zeb_898_headless_card_flow_tests` directly after `mod zeb_687_revoked_feed_boot_tests` (that module starts ~line 85441; place the new module after its closing brace). Copy the module-local `EnvVarGuard` (existing convention — it is deliberately duplicated per test module, see ZEB-193).

**Interfaces:**
- Consumes: `republish_owner_card_impl(&Mutex<NodeState>, String, String, Option<String>, Option<String>) -> Result<(), String>`; `crate::owner_commands::mint_owner_identity_inner_for_test(&Mutex<NodeState>, F) -> Result<MintIpcResult, String>` (test-fixtures-gated); `start_node_inner(None, sink, None, &state, None)`; `crate::iroh_endpoint::warm_up_iroh_global_init()`; `NodeState.profile_card_publisher: Option<Arc<ProfileCardPublisher>>` → `.latest_handle() -> Arc<tokio::sync::Mutex<Option<(String, Vec<u8>)>>>`; `crate::profile_card_broadcast::ProfileCardBroadcast` (fields `display_name`, `status_text`).
- Produces: nothing downstream; standalone pin.

- [ ] **Step 1: Write the module**

```rust
/// ZEB-898: end-to-end pin of the fresh-mint HEADLESS display-name flow —
/// the field failure class that broke silently pre-#635 and was fixed
/// incidentally to the ZEB-882 GUI boot-race fix. Drives the exact
/// `serve --display-name` + `api mint_owner_identity` sequence at the impl
/// level: boot-publish stashes (runtime not ready) → mint (stop → persist →
/// REAL `start_node_inner` restart, the same closure shape production's
/// `mint_owner_identity_impl` passes) → the start's drain publishes the
/// stashed card. If any joint regresses (stash dropped, latch cleared on
/// stop, drain skipped/reordered, publisher unwired at drain time) this
/// fails.
///
/// Gated on `test-fixtures` for `mint_owner_identity_inner_for_test`
/// (mirrors `zeb_687_revoked_feed_boot_tests`).
#[cfg(all(test, feature = "test-fixtures"))]
mod zeb_898_headless_card_flow_tests {
    use super::*;
    use serial_test::serial;

    /// RAII guard: sets an env var, restores the prior value (or removes it)
    /// on drop, incl. panic. Boot-path env (HOME etc.) is process-global;
    /// module-local copy by convention (ZEB-193).
    struct EnvVarGuard {
        name: &'static str,
        prev: Option<String>,
    }
    impl EnvVarGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let prev = std::env::var(name).ok();
            std::env::set_var(name, value);
            Self { name, prev }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.name, v),
                None => std::env::remove_var(self.name),
            }
        }
    }

    /// NO outer whole-test timeout: a full node boot legitimately runs
    /// 80-130s under `--workspace --all-targets` sweep contention (see the
    /// sibling full-boot tests' note in `zeb_687_revoked_feed_boot_tests`).
    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn fresh_mint_headless_flow_publishes_stashed_display_name() {
        // (a) Scope every boot-path env var to this test (ZEB-428 posture).
        let home = tempfile::tempdir().expect("tempdir for HOME override");
        let home_str = home
            .path()
            .to_str()
            .expect("tempdir path is valid utf8")
            .to_string();
        let _g_home = EnvVarGuard::set("HOME", &home_str);
        let _g_userprofile = EnvVarGuard::set("USERPROFILE", &home_str);
        let _g_pass = EnvVarGuard::set("HARMONY_PASSPHRASE", "zeb898-headless-card-pin");
        let _g_xdg = EnvVarGuard::set("XDG_DATA_HOME", &format!("{home_str}/xdg-data"));
        let _g_appdata = EnvVarGuard::set("APPDATA", &format!("{home_str}/appdata"));
        let _g_nokeychain = EnvVarGuard::set("HARMONY_DISABLE_KEYCHAIN", "1");

        // (b) The headless serve boot publish, pre-mint: no owner runtime →
        //     not-ready Err AND the card is stashed (exactly what
        //     `serve_cli` hits on a fresh profile with --display-name).
        let state = std::sync::Arc::new(Mutex::new(NodeState::default()));
        let err = republish_owner_card_impl(&state, "Zeb898".to_string(), String::new(), None, None)
            .await
            .expect_err("fresh NodeState has no owner runtime wired");
        assert!(
            err.contains("owner card runtime not ready"),
            "unexpected error: {err}"
        );
        assert!(
            state.lock().unwrap().pending_card.is_some(),
            "boot publish must stash the card for the post-mint drain"
        );

        // (c) Mint with the REAL node restart — the same closure shape
        //     production's `mint_owner_identity_impl` passes. The restart's
        //     `start_node_inner` success path drains the latch. Prime the
        //     one-time global iroh bind first (ZEB-347) so it isn't paid
        //     under the assertion.
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        let events = crate::api::events::ApiEventSink::new();
        let sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(events.clone());
        let state_for_restart = std::sync::Arc::clone(&state);
        let sink_for_restart = std::sync::Arc::clone(&sink);
        crate::owner_commands::mint_owner_identity_inner_for_test(&state, move || async move {
            start_node_inner(None, sink_for_restart, None, &state_for_restart, None)
                .await
                .map(|_| ())
        })
        .await
        .expect("mint + real restart must succeed");

        // (d) The drain ran inside the restart (before start_node_inner
        //     returned), so both observables are already settled — no polling.
        let publisher = {
            let guard = state.lock().expect("NodeState lock");
            assert!(
                guard.pending_card.is_none(),
                "post-mint start must drain the pending-card latch"
            );
            guard
                .profile_card_publisher
                .clone()
                .expect("profile_card_publisher wired after identity boot")
        };
        let latest = publisher.latest_handle();
        let cached = latest.lock().await.clone();
        let (_topic, bytes) = cached
            .expect("drained publish must cache the card for refresh/queryable");
        let decoded: crate::profile_card_broadcast::ProfileCardBroadcast =
            ciborium::de::from_reader(&bytes[..]).expect("cached card decodes");
        assert_eq!(decoded.display_name, "Zeb898");
        assert_eq!(decoded.status_text, "");

        // (e) Teardown before the env guards drop.
        crate::stop_inner(&state, None);
    }
}
```

- [ ] **Step 2: Run it — expect PASS (regression pin; red is historical)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(=fresh_mint_headless_flow_publishes_stashed_display_name)'`
Expected: PASS (1 test; allow ~1-3 min — full node boot). If it FAILS on the `pending_card.is_none()` or `latest` assert, stop and re-audit: the drain chain changed since the 2026-08-12 live repro.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "test(ZEB-898): pin the fresh-mint headless flow publishing the boot-stashed card"
```

---

### Task 3: `republish_owner_card.statusText` optional (RPC widening, red→green)

**Files:**
- Modify: `src-tauri/src/api/rpc.rs:645-650` (`RepublishOwnerCardArgs`) + the args-shape test (`~line 2642`, inside the file's `#[cfg(test)]` module).

**Interfaces:**
- Consumes: `build_registry()`, `test_state()`, `test_sink()`, `RpcError::{Command, BadArgs}` — all local to `rpc.rs`.
- Produces: `RepublishOwnerCardArgs.status_text` defaults to `""` when omitted (RPC surface only; Tauri IPC signature unchanged).

- [ ] **Step 1: Write the failing test** (append right after the existing `republish_owner_card` dispatch block ~line 2658):

```rust
        // ZEB-898: statusText is optional on the RPC surface — omitting it
        // must parse (default "") and reach the same pre-node Command error,
        // not BadArgs. Headless agents setting only a display name were
        // getting HTTP 400 `missing field statusText`.
        let err = reg
            .dispatch(
                "republish_owner_card",
                test_state(),
                test_sink(),
                serde_json::json!({ "displayName": "OnlyName" }),
            )
            .await
            .unwrap_err();
        match err {
            RpcError::Command(msg) => {
                assert_eq!(msg, "owner card runtime not ready")
            }
            other => panic!("republish_owner_card sans statusText: expected Command, got {other:?}"),
        }
```

And the companion pure-deserialize assert (same test fn, right after the block above) pinning the defaulted VALUE:

```rust
        // The omitted field must default to "" specifically (not garbage).
        let args: RepublishOwnerCardArgs =
            serde_json::from_value(serde_json::json!({ "displayName": "OnlyName" }))
                .expect("statusText omitted must deserialize");
        assert_eq!(args.status_text, "");
```

- [ ] **Step 2: Run it — expect FAIL**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(profile_rpcs_dispatch)'`
(The enclosing test fn is the one containing the ~2642 block; confirm its name via `grep -n "async fn" src/api/rpc.rs | awk -F: '$1 < 2600' | tail -2` and use it in `-E 'test(=<name>)'`.)
Expected: FAIL with panic `expected Command, got BadArgs("missing field \`statusText\`")`.

- [ ] **Step 3: Implement** — `RepublishOwnerCardArgs` (`rpc.rs:645`):

```rust
/// ZEB-464: `republish_owner_card` args (avatar/profile-page CIDs optional).
/// ZEB-898: `status_text` optional too (default "") — headless agents set
/// only the display name; an empty status is the natural "no status" card.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RepublishOwnerCardArgs {
    display_name: String,
    #[serde(default)]
    status_text: String,
    avatar_cid: Option<String>,
    profile_page_root: Option<String>,
}
```

- [ ] **Step 4: Run the test — expect PASS**

Same command as Step 2. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/api/rpc.rs
git commit -m "feat(ZEB-898): republish_owner_card statusText optional on the RPC surface (default \"\")"
```

---

### Task 4: gates, PR, ticket hygiene

- [ ] **Step 1: Local gates** (from `src-tauri/`, working tree clean — commit first):

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: all green (full sweep ~5-6k tests; the new full-boot test adds ~1-3 min).

- [ ] **Step 2: Push branch + open PR** (`--repo zeblithic/harmony-client`, base main). PR body: re-scope rationale (premise verified fixed by #635, live-repro evidence), the two pins, the RPC widening, `Closes ZEB-898`, standard footer. Fire `@coderabbitai review` ONCE at open; never again.

- [ ] **Step 3: File the split-out ticket** for the `ownerDisplayName` device-label-vs-card-name DX confusion (Linear, describing the confirmed confusion + options: rename field vs. surface card name), and link it from ZEB-898.
