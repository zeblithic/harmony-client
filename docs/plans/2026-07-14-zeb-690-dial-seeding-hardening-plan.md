# ZEB-690 Dial-Seeding Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the deferred Minors from ZEB-510 PR #469 with focused tests, one testability refactor, two doc/cosmetic fixes, and a harness stale-binary freshness guard.

**Architecture:** Additive test-and-hardening only — no shipped-behavior change. Item 5's production fix already landed in PR #469 (test-only here). Item 4 lifts an inline decode into a pure helper to make its defensive branch testable. Item 8 adds an mtime-vs-source freshness gate to `bin_resolver` that hard-fails a stale `harmony-app`.

**Tech Stack:** Rust (edition 2021, MSRV 1.91), `cargo-nextest`, `ciborium` (CBOR), `walkdir` + `tempfile` (already deps of `e2e-harness`), `ed25519_dalek` (test key material).

## Global Constraints

- Cargo commands run from `src-tauri/`. Gates: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. The `e2e-harness` crate is standalone — gate it from `e2e-harness/` with its own `cargo nextest run` / `cargo clippy` / `cargo fmt`.
- No shipped-behavior change. Item 4's extraction must be behavior-preserving (byte-for-byte same decode result as the current inline code).
- Item 8 freshness guard: **hard-fail** when the resolved binary is older than the newest first-party source under `src-tauri/` (`src/**/*.rs` + `Cargo.toml` + `Cargo.lock` + `build.rs`), excluding `target/` and `vendor/`; bypass with env `HARMONY_APP_FRESHNESS=off`; **graceful skip** (return `Ok`) when the source tree is not locatable.
- Item 7 is scoped to the `.expect()` string at `e2e-harness/tests/e2e_two_node.rs:1835` only (the `B2->P` comment on a nearby line is out of scope).

---

### Task 1: Item 8 — `bin_resolver` freshness guard + tests

**Files:**
- Modify: `e2e-harness/src/bin_resolver.rs`

**Interfaces:**
- Consumes: nothing new (uses `walkdir`, `tempfile`, std).
- Produces: `resolve_harmony_app_bin()` unchanged public signature (`-> anyhow::Result<PathBuf>`), now also freshness-gated. New private fns `check_freshness(bin: &Path, src_tauri: &Path) -> anyhow::Result<()>`, `newest_source_under(dir: &Path) -> Option<(SystemTime, PathBuf)>`, `freshness_disabled() -> bool`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `e2e-harness/src/bin_resolver.rs`:

```rust
    use std::path::Path;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn set_mtime(p: &Path, unix_secs: u64) {
        let t = UNIX_EPOCH + Duration::from_secs(unix_secs);
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    /// A src_tauri tempdir with `src/lib.rs` at `src_secs` and a binary at
    /// `bin_secs`. Returns (src_tauri_dir, bin_path); keep the TempDir alive.
    fn fixture(src_secs: u64, bin_secs: u64) -> (tempfile::TempDir, std::path::PathBuf) {
        let td = tempfile::tempdir().unwrap();
        let src = td.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let source = src.join("lib.rs");
        std::fs::write(&source, "// source").unwrap();
        set_mtime(&source, src_secs);
        let bin = td.path().join("harmony-app");
        std::fs::write(&bin, "ELF").unwrap();
        set_mtime(&bin, bin_secs);
        (td, bin)
    }

    #[test]
    fn fresh_binary_passes_freshness() {
        let (td, bin) = fixture(1_000, 2_000); // binary NEWER than source
        assert!(check_freshness(&bin, td.path()).is_ok());
    }

    #[test]
    fn stale_binary_fails_freshness() {
        let (td, bin) = fixture(2_000, 1_000); // binary OLDER than source
        let err = check_freshness(&bin, td.path()).unwrap_err().to_string();
        assert!(err.contains("stale harmony-app binary"), "got: {err}");
        assert!(err.contains("lib.rs"), "error names the newer source: {err}");
    }

    #[test]
    fn missing_source_tree_skips_freshness() {
        // src_tauri dir with NO src/ subdir → not locatable → skip (Ok).
        let td = tempfile::tempdir().unwrap();
        let bin = td.path().join("harmony-app");
        std::fs::write(&bin, "ELF").unwrap();
        set_mtime(&bin, 1_000);
        assert!(check_freshness(&bin, td.path()).is_ok());
    }

    #[test]
    fn manifest_change_also_trips_freshness() {
        // A newer Cargo.toml (not just .rs) must trip the guard.
        let (td, bin) = fixture(1_000, 2_000); // src older, bin newer
        let manifest = td.path().join("Cargo.toml");
        std::fs::write(&manifest, "[package]").unwrap();
        set_mtime(&manifest, 3_000); // manifest NEWER than binary
        let err = check_freshness(&bin, td.path()).unwrap_err().to_string();
        assert!(err.contains("Cargo.toml"), "got: {err}");
    }

    #[test]
    fn freshness_disabled_reads_env() {
        // nextest runs each test in its own process → env mutation is isolated.
        std::env::remove_var("HARMONY_APP_FRESHNESS");
        assert!(!freshness_disabled());
        std::env::set_var("HARMONY_APP_FRESHNESS", "off");
        assert!(freshness_disabled());
        std::env::remove_var("HARMONY_APP_FRESHNESS");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd e2e-harness && cargo nextest run -E 'test(freshness)' -E 'test(stale)' -E 'test(missing_source)'`
Expected: FAIL to compile — `check_freshness` / `freshness_disabled` not defined.

- [ ] **Step 3: Implement the freshness guard**

In `e2e-harness/src/bin_resolver.rs`, rename the current body of `resolve_harmony_app_bin` to a private `locate_harmony_app_bin`, and add the guard. Replace lines 11–40 (the current `pub fn resolve_harmony_app_bin`) with:

```rust
pub fn resolve_harmony_app_bin() -> anyhow::Result<PathBuf> {
    let path = locate_harmony_app_bin()?;
    if !freshness_disabled() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let src_tauri = manifest.join("..").join("src-tauri");
        check_freshness(&path, &src_tauri)?;
    }
    Ok(path)
}

fn locate_harmony_app_bin() -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("HARMONY_APP_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Ok(pb);
        }
        anyhow::bail!("HARMONY_APP_BIN is set but not a file: {}", pb.display());
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let exe = if cfg!(windows) {
        "harmony-app.exe"
    } else {
        "harmony-app"
    };
    for profile in ["release", "debug"] {
        let cand = manifest
            .join("..")
            .join("src-tauri")
            .join("target")
            .join(profile)
            .join(exe);
        if cand.is_file() {
            return Ok(cand);
        }
    }
    anyhow::bail!(
        "harmony-app binary not found. Build it first:\n  cd src-tauri && cargo build --bin harmony-app\n\
         or set HARMONY_APP_BIN to an explicit path."
    )
}

/// ZEB-690: `HARMONY_APP_FRESHNESS=off` disables the stale-binary guard.
fn freshness_disabled() -> bool {
    std::env::var("HARMONY_APP_FRESHNESS").ok().as_deref() == Some("off")
}

/// ZEB-690: guard against silently testing a stale `harmony-app`. `cargo nextest
/// --features e2e` rebuilds the harness but NOT the spawned binary, so a days-old
/// artifact can shadow freshly-edited source (this invalidated s7 gates on the
/// ZEB-510 branch). Hard-fail when `bin` is older than the newest first-party
/// source under `src_tauri` (`src/**/*.rs` + Cargo.toml/Cargo.lock/build.rs,
/// excluding target/ & vendor/). Skips (Ok) when the tree isn't locatable —
/// never a false failure in installed/packaged contexts.
fn check_freshness(bin: &Path, src_tauri: &Path) -> anyhow::Result<()> {
    let src_dir = src_tauri.join("src");
    if !src_dir.is_dir() {
        return Ok(()); // source tree not locatable → skip.
    }
    let bin_mtime = match std::fs::metadata(bin).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return Ok(()), // can't stat the binary → don't block.
    };
    let mut newest = newest_source_under(&src_dir);
    for f in ["Cargo.toml", "Cargo.lock", "build.rs"] {
        let p = src_tauri.join(f);
        if let Ok(t) = std::fs::metadata(&p).and_then(|m| m.modified()) {
            if newest.as_ref().map_or(true, |(n, _)| t > *n) {
                newest = Some((t, p));
            }
        }
    }
    if let Some((newest_t, newest_p)) = newest {
        if bin_mtime < newest_t {
            anyhow::bail!(
                "stale harmony-app binary: {}\n  is older than source: {}\n\
                 Rebuild it:  cd src-tauri && cargo build --bin harmony-app\n\
                 (bypass with HARMONY_APP_FRESHNESS=off)",
                bin.display(),
                newest_p.display()
            );
        }
    }
    Ok(())
}

/// Newest (mtime, path) among `*.rs` files under `dir`, pruning `target/` and
/// `vendor/` subtrees. `None` if the dir has no readable `.rs` files.
fn newest_source_under(dir: &Path) -> Option<(std::time::SystemTime, PathBuf)> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_str().unwrap_or("");
            !(e.file_type().is_dir() && (name == "target" || name == "vendor"))
        })
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().is_file()
                && e.path().extension().and_then(|x| x.to_str()) == Some("rs")
        })
        .filter_map(|e| {
            let t = std::fs::metadata(e.path()).and_then(|m| m.modified()).ok()?;
            Some((t, e.path().to_path_buf()))
        })
        .max_by_key(|(t, _)| *t)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd e2e-harness && cargo nextest run bin_resolver`
Expected: PASS (all `bin_resolver` tests, including the existing `env_override_to_missing_file_errors`).

- [ ] **Step 5: Gate + commit**

Run: `cd e2e-harness && cargo fmt && cargo clippy --all-targets -- -D warnings`
Then:
```bash
git add e2e-harness/src/bin_resolver.rs
git commit -m "ZEB-690: bin_resolver stale-binary freshness guard (item 8)"
```

---

### Task 2: Item 4 — extract `decode_peer_iroh_endpoint` + tests

**Files:**
- Modify: `src-tauri/src/pairing/state_machine.rs` (the Confirm-handler decode at lines 1578–1588; extract to a module-level fn)

**Interfaces:**
- Produces: `fn decode_peer_iroh_endpoint(node_id_hex: Option<String>, home_relay: Option<String>) -> Option<([u8; 32], String)>` (private to the module).

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `src-tauri/src/pairing/state_machine.rs` (use `super::*`):

```rust
    #[test]
    fn decode_peer_iroh_endpoint_valid() {
        let nid = "ab".repeat(32); // 32 bytes hex
        let got = super::decode_peer_iroh_endpoint(Some(nid), Some("https://r/".into()));
        assert_eq!(got, Some(([0xAB; 32], "https://r/".to_string())));
    }

    #[test]
    fn decode_peer_iroh_endpoint_empty_relay_defaults() {
        let nid = "cd".repeat(32);
        let got = super::decode_peer_iroh_endpoint(Some(nid), None);
        assert_eq!(got, Some(([0xCD; 32], String::new())));
    }

    #[test]
    fn decode_peer_iroh_endpoint_malformed_hex_is_none() {
        // Odd length / non-hex → hex::decode errors → None.
        assert_eq!(
            super::decode_peer_iroh_endpoint(Some("zzz".into()), Some("r".into())),
            None
        );
    }

    #[test]
    fn decode_peer_iroh_endpoint_wrong_length_is_none() {
        // Valid hex but not 32 bytes → None.
        assert_eq!(
            super::decode_peer_iroh_endpoint(Some("abab".into()), None),
            None
        );
    }

    #[test]
    fn decode_peer_iroh_endpoint_absent_is_none() {
        assert_eq!(super::decode_peer_iroh_endpoint(None, Some("r".into())), None);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(decode_peer_iroh_endpoint)'`
Expected: FAIL to compile — `decode_peer_iroh_endpoint` not defined.

- [ ] **Step 3: Add the helper and call it from the handler**

Add this module-level fn near the Confirm handler in `src-tauri/src/pairing/state_machine.rs`:

```rust
/// ZEB-510 step 2 / ZEB-690: decode a peer's SAS-advertised iroh endpoint.
/// `None` when the node id is absent, non-hex, or not exactly 32 bytes (a
/// pre-step-2 peer omits it; a malformed value is tolerated, not fatal). An
/// absent relay defaults to an empty string.
fn decode_peer_iroh_endpoint(
    node_id_hex: Option<String>,
    home_relay: Option<String>,
) -> Option<([u8; 32], String)> {
    let nid_hex = node_id_hex?;
    match hex::decode(&nid_hex) {
        Ok(bytes) if bytes.len() == 32 => {
            let mut nid = [0u8; 32];
            nid.copy_from_slice(&bytes);
            Some((nid, home_relay.unwrap_or_default()))
        }
        _ => None,
    }
}
```

Then replace the inline decode at `state_machine.rs:1578–1588`:

```rust
            ctx.peer_iroh_endpoint = match (iroh_node_id_hex, iroh_home_relay) {
                (Some(nid_hex), relay) => match hex::decode(&nid_hex) {
                    Ok(bytes) if bytes.len() == 32 => {
                        let mut nid = [0u8; 32];
                        nid.copy_from_slice(&bytes);
                        Some((nid, relay.unwrap_or_default()))
                    }
                    _ => None,
                },
                _ => None,
            };
```

with:

```rust
            ctx.peer_iroh_endpoint = decode_peer_iroh_endpoint(iroh_node_id_hex, iroh_home_relay);
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(decode_peer_iroh_endpoint)'`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/pairing/state_machine.rs
git commit -m "ZEB-690: extract + test decode_peer_iroh_endpoint (item 4)"
```

---

### Task 3: Item 1 — fleet-heartbeat-refeed test + Item 6 docstring

**Files:**
- Modify: `src-tauri/src/reachability_resolver.rs` (add test to the SECOND test module — the one at `use super::*` line ~1559 with the kick tests; fix docstring at `resolve_entry_by_node_id` line ~510)

**Interfaces:**
- Consumes: `ReachabilityResolver::update_with_source(actor, payload, hlc, ReachabilitySource::FleetSibling)`, `SupervisorHandle::new()`, `sup.pending_trigger(node_id)`, test helpers `make_payload(u8, u64)` / `make_hlc(u64, u32, &str)` (defined in that module).

- [ ] **Step 1: Write the failing test**

Add after `identical_payload_replay_does_not_kick` (~line 1377) in `src-tauri/src/reachability_resolver.rs`:

```rust
    /// ZEB-510/ZEB-690: a FLEET-source heartbeat re-feed (a `FleetSibling`
    /// republish with a newer HLC but identical addressing) must NOT re-fire
    /// `NewPeer` — `was_present` (the fleet slot already exists) suppresses the
    /// first-learn kick, and the unchanged `addr_key` keeps `RecordChanged`
    /// silent too. Mirrors `identical_payload_replay_does_not_kick` on the fleet
    /// slot (the ZEB-510 step-1 addition that the durable-path tests don't cover).
    #[test]
    fn fleet_heartbeat_refeed_does_not_refire_new_peer() {
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([0xAA; 16]);
        let node_id = [0x11; 32];
        // First fleet-sibling learn with NO supervisor installed — no kick recorded.
        r.update_with_source(
            owner,
            make_payload(0x11, 1000),
            make_hlc(1000, 0, "a"),
            ReachabilitySource::FleetSibling,
        );
        let sup = SupervisorHandle::new();
        r.set_supervisor(sup.clone());
        // Fleet heartbeat: same key + source, newer HLC, IDENTICAL addressing.
        r.update_with_source(
            owner,
            make_payload(0x11, 1000),
            make_hlc(2000, 0, "a"),
            ReachabilitySource::FleetSibling,
        );
        assert_eq!(
            sup.pending_trigger(node_id),
            None,
            "fleet heartbeat re-feed must not re-fire NewPeer",
        );
    }
```

- [ ] **Step 2: Run test to verify it fails/passes correctly**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(fleet_heartbeat_refeed_does_not_refire_new_peer)'`
Expected: PASS (the production behavior already exists — this test pins it). If it FAILS, the `was_present` fleet-slot inclusion regressed; stop and escalate.

- [ ] **Step 3: Fix the item-6 docstring**

At `resolve_entry_by_node_id` (line ~510–517), the docstring says "across the peer's durable and pkarr slots (ties → durable)". Update it to include the fleet slot. Replace:

```rust
    /// pkarr slots (ties → durable), matching `resolve_by_node_id`'s freshest-wins
    /// dial semantics.
```

with:

```rust
    /// pkarr, and fleet slots (ties → durable), matching `resolve_by_node_id`'s
    /// freshest-wins dial semantics. (ZEB-510 added the fleet slot, which
    /// `freshest()` includes.)
```

- [ ] **Step 4: Run the resolver test module**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(reachability)'`
Expected: PASS (whole module, incl. the new test).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/reachability_resolver.rs
git commit -m "ZEB-690: fleet-heartbeat-refeed test + resolve_entry docstring (items 1,6)"
```

---

### Task 4: Item 2 — `fleet_peer_seed_persist` branch tests

**Files:**
- Modify: `src-tauri/src/fleet_peer_seed_persist.rs` (add two tests to the `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `load(&Path)`, `load_doc_or_recover(&Path)`, `save(&Path, &FleetPeerSeedDoc)`, `FLEET_PEER_SEED_FILENAME`, `FleetPeerSeedDoc::default()`, `SyncError::CborDecode`.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `src-tauri/src/fleet_peer_seed_persist.rs`:

```rust
    #[test]
    fn trailing_bytes_after_value_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FLEET_PEER_SEED_FILENAME);
        // Write a valid doc, then append one extra CBOR token after the value.
        save(&path, &FleetPeerSeedDoc::default()).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0x00); // integer 0 — a distinct trailing value
        std::fs::write(&path, &bytes).unwrap();
        match load(&path) {
            Err(SyncError::CborDecode(msg)) => {
                assert!(msg.contains("trailing bytes"), "got: {msg}")
            }
            other => panic!("expected CborDecode trailing-bytes, got {other:?}"),
        }
    }

    #[test]
    fn transient_io_error_propagates_not_quarantined() {
        // Reading a path that IS a directory fails with a non-NotFound,
        // non-decode IO error → load_doc_or_recover must PROPAGATE it, not
        // self-heal to default() (which is reserved for genuinely-corrupt files).
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load_doc_or_recover(dir.path()).is_err(),
            "transient IO error must propagate, not quarantine-to-default"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail-then-pass**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(trailing_bytes_after_value_are_rejected)' -E 'test(transient_io_error_propagates_not_quarantined)'`
Expected: PASS (both branches already exist in production; these pin them). If `trailing_bytes` fails because ciborium consumed the extra byte, change the appended token to `0x01` and re-run.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/fleet_peer_seed_persist.rs
git commit -m "ZEB-690: fleet_peer_seed_persist trailing-bytes + transient-IO tests (item 2)"
```

---

### Task 5: Item 5 — partial-pub idempotency regression test

**Files:**
- Modify: `src-tauri/src/fleet_net.rs` (add a test to the `mod tests` block, alongside `seed_sibling_device_cache_makes_vk_lookup_resolve`)

**Interfaces:**
- Consumes: `seed_sibling_device_cache(&mut OwnerState, OwnerAddr, [u8;32], u64) -> bool`, `vk_map_from_device_cache`, `OwnerState::apply_owner_device_update(owner, devices, pubs, contacts, hlc)`, `crate::dm_signing::{ed25519_pub_to_x25519, derive_device_hash_from_identity_pub}`, `crate::owner_state_types::{OwnerAddr, Hlc}`.

- [ ] **Step 1: Write the failing/pinning test**

Add to `src-tauri/src/fleet_net.rs` `mod tests`:

```rust
    // ZEB-690 (item 5): pins the converge-fix fall-through — when the self-owner
    // entry already holds the sibling's device HASH but with its aligned pub
    // `None` (a Path-B "known by hash, pub not yet propagated" state), seeding
    // must fill the pub (so vk_lookup resolves) and NOT duplicate the hash.
    #[test]
    fn seed_sibling_fills_pub_when_hash_present_without_pub() {
        use crate::owner_state_crdt::OwnerState;
        use crate::owner_state_types::OwnerAddr;

        let self_owner = OwnerAddr([0x11; 16]);
        let self_vk = [0x99u8; 32];
        let sib_ed: [u8; 32] = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])
            .verifying_key()
            .to_bytes();
        // Reconstruct the sibling's device hash exactly as the seed does.
        let x_pub = crate::dm_signing::ed25519_pub_to_x25519(&sib_ed).unwrap();
        let mut identity_pub = [0u8; 64];
        identity_pub[..32].copy_from_slice(&x_pub);
        identity_pub[32..].copy_from_slice(&sib_ed);
        let hash =
            crate::dm_signing::derive_device_hash_from_identity_pub(&identity_pub).unwrap();

        let mut state = OwnerState::default();
        // Pre-seed: hash present, pub None (older HLC than the seed below).
        state.apply_owner_device_update(
            self_owner,
            vec![hash],
            vec![None],
            vec![None],
            hlc(500, "pre"),
        );
        let vk_map = |st: &OwnerState| {
            vk_map_from_device_cache(&st.owner_device_cache, &self_owner, "self-dev", self_vk)
        };
        // vk_lookup does NOT resolve yet — pub is None.
        assert!(!vk_map(&state).contains_key(&hex::encode(sib_ed)));

        // Seed → falls through the idempotency guard and fills the pub.
        assert!(seed_sibling_device_cache(&mut state, self_owner, sib_ed, 1_000));
        assert_eq!(vk_map(&state).get(&hex::encode(sib_ed)), Some(&sib_ed));

        // No duplicate hash in the device list.
        let entry = state.owner_device_cache.devices.get(&self_owner).unwrap();
        assert_eq!(
            entry.devices.iter().filter(|d| **d == hash).count(),
            1,
            "device hash must not be duplicated"
        );
    }
```

- [ ] **Step 2: Run test to verify it passes (pins the merged fix)**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(seed_sibling_fills_pub_when_hash_present_without_pub)'`
Expected: PASS. If it FAILS, the converge fix regressed; stop and escalate.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/fleet_net.rs
git commit -m "ZEB-690: partial-pub idempotency regression test (item 5)"
```

---

### Task 6: Item 3 — Confirm back-compat fixture + Item 7 arrow char

**Files:**
- Modify: `src-tauri/src/pairing/types.rs` (add a test to `mod tests`)
- Modify: `e2e-harness/tests/e2e_two_node.rs:1835` (one-char fix)

**Interfaces:**
- Consumes: `EncryptedPayload::Confirm { sas_digits, iroh_node_id_hex, iroh_home_relay }`, `ciborium::{into_writer, from_reader, value::Value}`.

- [ ] **Step 1: Write the failing test (item 3)**

Add to the `mod tests` block in `src-tauri/src/pairing/types.rs`:

```rust
    /// ZEB-690 (item 3): pin the wire back-compat contract that
    /// `#[serde(default, skip_serializing_if = "Option::is_none")]` provides.
    #[test]
    fn confirm_none_fields_omit_iroh_keys_and_old_wire_decodes() {
        use ciborium::value::Value;

        // (a) Forward: a None-Confirm serializes WITHOUT the iroh keys.
        let p = EncryptedPayload::Confirm {
            sas_digits: "012845".to_string(),
            iroh_node_id_hex: None,
            iroh_home_relay: None,
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&p, &mut bytes).unwrap();
        let v: Value = ciborium::from_reader(&bytes[..]).unwrap();
        let keys: Vec<String> = match &v {
            Value::Map(entries) => entries
                .iter()
                .filter_map(|(k, _)| match k {
                    Value::Text(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => panic!("expected CBOR map"),
        };
        assert!(keys.contains(&"kind".to_string()));
        assert!(keys.contains(&"sasDigits".to_string()));
        assert!(
            !keys.contains(&"irohNodeIdHex".to_string()),
            "None iroh_node_id_hex must be skipped: {keys:?}"
        );
        assert!(
            !keys.contains(&"irohHomeRelay".to_string()),
            "None iroh_home_relay must be skipped: {keys:?}"
        );

        // (b) Backward: hand-built old-style wire (kind + sasDigits ONLY) decodes
        // to a Confirm with both iroh fields None.
        let old = Value::Map(vec![
            (Value::Text("kind".into()), Value::Text("confirm".into())),
            (Value::Text("sasDigits".into()), Value::Text("012845".into())),
        ]);
        let mut old_bytes = Vec::new();
        ciborium::into_writer(&old, &mut old_bytes).unwrap();
        let decoded: EncryptedPayload = ciborium::from_reader(&old_bytes[..]).unwrap();
        match decoded {
            EncryptedPayload::Confirm {
                sas_digits,
                iroh_node_id_hex,
                iroh_home_relay,
            } => {
                assert_eq!(sas_digits, "012845");
                assert!(iroh_node_id_hex.is_none());
                assert!(iroh_home_relay.is_none());
            }
            _ => panic!("expected Confirm"),
        }
    }
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(confirm_none_fields_omit_iroh_keys_and_old_wire_decodes)'`
Expected: PASS. If `into_writer` for `EncryptedPayload` needs `ciborium` in scope, it is already a dep; use the fully-qualified `ciborium::` paths shown.

- [ ] **Step 3: Fix the item-7 arrow char**

In `e2e-harness/tests/e2e_two_node.rs`, the `.expect()` string at line ~1835 reads:

```rust
         P should have learned B2's iroh endpoint via the FleetNetDoc->resolver wiring",
```

Change `FleetNetDoc->resolver` to `FleetNetDoc→resolver`:

```rust
         P should have learned B2's iroh endpoint via the FleetNetDoc→resolver wiring",
```

- [ ] **Step 4: Verify item 7 compiles**

Run: `cd e2e-harness && cargo build --tests`
Expected: builds clean.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/pairing/types.rs e2e-harness/tests/e2e_two_node.rs
git commit -m "ZEB-690: Confirm back-compat fixture + e2e arrow char (items 3,7)"
```

---

## Final gate (after all tasks)

- [ ] `cd src-tauri && cargo fmt --all -- --check`
- [ ] `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- [ ] `cd e2e-harness && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo nextest run`
- [ ] Whole-branch review, then open PR.

## Self-Review notes

- **Spec coverage:** items 1–8 all mapped (5 code = already merged, test in Task 5; 3 = Task 6; 6/7 = Tasks 3/6). ✅
- **Type consistency:** `decode_peer_iroh_endpoint(Option<String>, Option<String>) -> Option<([u8;32], String)>`, `check_freshness(&Path,&Path)`, `newest_source_under(&Path)`, `freshness_disabled()` used consistently across steps. ✅
- **Placeholder scan:** no TBD/TODO; every code step shows complete code. ✅
