# ZEB-227 — ZEB-216 Sub-B Phase 3b: Real Reticulum DM Transport Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Phase 2 `StubTransport` with a real harmony-runtime `SendUnicastToDevice` adapter and add inbound `UnicastReceived` demux (`DmInvite` / `DmCidNotify` / `DmAck`) so DMs work end-to-end through real Reticulum transport, with full link-origin-binding security.

**Architecture:** Phase 2 already wired the entire drain state machine and `send_dm` IPC; Phase 3b grafts the real transport onto the existing `DmTransport` trait and adds inbound packet handling. Outbound: `DmOutbox::send_dm` → `DmTransport::send` → mpsc channel → `event_loop` arm pushes `RuntimeEvent::SendUnicastToDevice` into `NodeRuntime` → runtime emits `RuntimeAction::SendOnInterface` → existing `dispatch_action` arm. Inbound: UDP packet → runtime → `RuntimeAction::UnicastReceived { destination_hash, source: Some(identity_hash), packet }` → intercepted in the runtime-tick loop before `dispatch_action` → `dm_outbox::handle_unicast` decodes the packet, runs link-origin binding, and dispatches to `handle_invite` / `handle_cidnotify` / `handle_ack`. Tests mock at the `RuntimeAction` channel boundary (no real Reticulum wire transport in CI).

**Tech Stack:** Rust (`tokio`, `async-trait`, `chacha20poly1305`, `ciborium`, `tracing`), Tauri 2 IPC, harmony-runtime + harmony-content (cross-repo deps), Reticulum unicast plane B (via harmony-runtime).

**Cross-repo:** A small companion PR in `~/work/zeblithic/harmony` lands first (Task 1) — terminal-link → identity binding so `DeliverLocally.source` is `Some(_)`, plus a public `NodeRuntime::register_local_destination` API so harmony-client can register its DM destination. harmony-client then bumps the dep pin to that SHA (Task 2) and proceeds.

**Spec:** `docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md` (commit `55f30cd`).

**Branch:** `zeb-227-dm-transport-phase3b` (branched from `origin/main` at `97c2e90`, the just-merged Phase 2 PR #79).

---

## File Structure

### Cross-repo: harmony companion PR (Task 1, branch `zeb-227-runtime-link-identity-binding` in `~/work/zeblithic/harmony`)

| File | Change | Why |
|---|---|---|
| `crates/harmony-reticulum/src/node.rs` | Modify `process_data_packet:1284-1306` to look up `Link::remote_identity` via the existing `link_table`, populate `DeliverLocally.source` with `Some(identity_hash_truncated)`. Add unit test. | Spec §"Link-origin binding rule" — DM security spine; without it every inbound DM is droppable as `UnknownLinkOrigin`. |
| `crates/harmony-runtime/src/runtime.rs` | Add public `NodeRuntime::register_local_destination(&mut self, dest_hash: [u8; 16])` and `unregister_local_destination(&mut self, dest_hash: &[u8; 16]) -> bool` that delegate to the private `router.register_destination` / `router.unregister_destination`. Add unit tests. | harmony-client must register its DM destination so inbound packets surface as `DeliverLocally` → `UnicastReceived`. The router is a private field today (`runtime.rs:754`), so without these accessors the client literally cannot accept inbound DMs. |

Total harmony delta: ~80 lines + tests. Single squash-merge PR.

### harmony-client (Tasks 2-13, branch `zeb-227-dm-transport-phase3b`)

| File | Action | Estimated lines |
|---|---|---|
| `src-tauri/Cargo.toml` | Modify: bump `harmony-runtime` and `harmony-content` git revs to the harmony companion-PR merge SHA (Task 2). | +4 modified lines |
| `src-tauri/src/dm_outbox.rs` | Modify: add `DmReceiveError` enum variants, `resolve_link_origin_owner` helper, `handle_unicast` dispatcher, `handle_invite`, `handle_cidnotify`, `handle_ack`. Add `RuntimeUnicastTransport` adapter struct + `DmTransport` impl. Trim `StubTransport` to test-cfg only (it's still used by Phase 2 tests). | ~1129 → ~1900 |
| `src-tauri/src/event_loop.rs` | Modify: add an mpsc channel for outbound `RuntimeEvent::SendUnicastToDevice` requests and wire its arm. Add a `RuntimeAction::UnicastReceived` interception block in each `for action in runtime.tick()` loop site, before `dispatch_action`. Pass through `cas_op_tx`, `dm_outbox`, `crdt_state`, `app` to the new handler. | +~120 |
| `src-tauri/src/lib.rs` | Modify: replace `StubTransport::new()` at line 843-844 with `RuntimeUnicastTransport::new(unicast_send_tx, ...)`. Construct the new mpsc channel near `cas_op_tx` (line 572). Compute the local DM destination hash at start_node and call `runtime.register_local_destination(dm_dest)` once at startup. Add a follow-up Linear ticket for any remaining manual-LAN smoke surface. | +~60 |
| `src-tauri/tests/dm_unicast_integration.rs` | Create: end-to-end integration test exercising `RealUnicastTransport` outbound + inbound `UnicastReceived` re-entry via a fake runtime-channel pair. Mocks at the channel boundary, NOT the wire. | +~250 (new) |
| `src-tauri/tests/dm_send_integration.rs` | Modify: add Phase 3b coverage to the existing Phase 2 round-trip test (it currently only exercises `StubTransport`; assert the new transport adapter respects the same DmTransport contract under the same fixture shape). | +~30 |

Total harmony-client delta: ~700-900 lines spread across 5 files, plus a new integration test.

---

## Task list (TDD-shaped, one commit per task)

Each task ends with the verification gate quartet (per user memory rule "cargo fmt + cargo clippy gates required at every task verification, not just clippy") and a commit. Run gates with `set -o pipefail` in any pipe-using verification command:

```bash
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
npx tsc --noEmit
npx vitest run
```

Tasks 12 (PR creation) and 13 (manual LAN smoke follow-up) are process tasks, not implementation. **Task 1 is in the harmony repo and must merge before Task 2 begins** — pause + push + open PR after Task 1's commit and wait for human merge.

---

### Task 1: harmony companion PR — terminal-link identity binding + public destination registration

**Repo:** `~/work/zeblithic/harmony`
**Branch:** `zeb-227-runtime-link-identity-binding` (branched from `origin/main` at `b721148`)

**Files:**
- Modify: `crates/harmony-reticulum/src/node.rs:1284-1306` (link-identity binding) and `:454-465` area (verify register_destination is intact)
- Modify: `crates/harmony-runtime/src/runtime.rs:754` (router field — surface a new public method around it; do NOT make the field public)
- Test: `crates/harmony-reticulum/src/node.rs` test module (link-identity binding)
- Test: `crates/harmony-runtime/src/runtime.rs` test module (public register API)

This is a single PR with two coupled changes. Both are tiny but both are required for harmony-client Phase 3b. Sub-steps below.

- [ ] **Step 1.1: Branch off origin/main**

```bash
cd ~/work/zeblithic/harmony
git fetch origin
git checkout -b zeb-227-runtime-link-identity-binding origin/main
git log --oneline -3   # confirm starts at b721148 or later
```

Expected: branch tracks `origin/main`, HEAD shows `b721148 feat(runtime): SendUnicastToDevice + UnicastReceived IPC kinds (ZEB-226) (#267)` or newer.

- [ ] **Step 1.2: Write a failing test for terminal-link identity binding**

Add the following test to the test module at the end of `crates/harmony-reticulum/src/node.rs`:

```rust
#[test]
fn process_data_packet_for_local_destination_populates_source_from_link_table() {
    // After a Link is established to a local destination, an inbound data
    // packet on that link MUST surface DeliverLocally.source = Some(identity_hash),
    // not None. This is the link-origin-binding bedrock for ZEB-216 Sub-B
    // Phase 3b — without it every inbound DM is droppable as UnknownLinkOrigin.
    use crate::link::LinkState;

    let mut node = Node::new(NodeIdentity::generate());
    let dest_hash = [0xb0u8; 16];
    let remote_identity_hash = [0xa1u8; 16];
    node.register_destination(dest_hash);

    // Seed a link_table entry so process_data_packet can look it up.
    // The seam: link_table is keyed by destination_hash and each entry
    // carries the remote identity established at handshake time.
    seed_link_for_test(&mut node, dest_hash, remote_identity_hash);

    let packet = build_type1_data_packet_for_test(dest_hash, b"hello");
    let actions = node.process_data_packet(packet, Arc::from("udp0"), 0);

    assert_eq!(actions.len(), 1);
    match &actions[0] {
        NodeAction::DeliverLocally { source, .. } => {
            assert_eq!(
                *source,
                Some(remote_identity_hash),
                "DeliverLocally.source must be the remote identity from the link handshake, not None"
            );
        }
        other => panic!("expected DeliverLocally, got {:?}", other),
    }
}
```

The implementer will need to add `seed_link_for_test` and `build_type1_data_packet_for_test` test helpers (or use an existing equivalent — search the test module for similar packet builders before writing new ones).

- [ ] **Step 1.3: Run test to verify it fails**

```bash
cd ~/work/zeblithic/harmony
set -o pipefail
cargo test --manifest-path crates/harmony-reticulum/Cargo.toml process_data_packet_for_local_destination_populates_source_from_link_table 2>&1 | tail -20
```

Expected: FAIL — `assertion `left == right` failed: left: None, right: Some(...)`. The current implementation hardcodes `source: None` at `node.rs:1305`.

- [ ] **Step 1.4: Implement link-identity binding**

In `crates/harmony-reticulum/src/node.rs`, modify `process_data_packet:1293-1306` to look up the link entry by `destination_hash` (or by some other available key — the implementer should investigate how `link_table` is keyed; if `destination_hash → remote_identity_hash` lookup isn't directly available, add a thin lookup helper on `Link` or maintain a parallel index). Replace the hardcoded `None`:

```rust
// 1. Local delivery takes priority
if self.local_destinations.contains(&destination_hash) {
    // Look up the remote identity from the link handshake state.
    // The link_table maps destination_hash → Link state, and Link
    // carries the remote_identity established during handshake.
    let source = self
        .link_table
        .get(&destination_hash)
        .and_then(|link| link.remote_identity_hash());
    return vec![NodeAction::DeliverLocally {
        destination_hash,
        packet,
        interface_name,
        source,
    }];
}
```

If `link_table` does not currently carry `remote_identity_hash` (it might only carry handshake state), the implementer adds the field via a small follow-on edit and threads it through the handshake-completion path. Use file:line investigation — do not invent fields that don't exist.

Update the doc comment on `NodeAction::DeliverLocally.source` (`node.rs:235-251`) to remove the "deferred to ZEB-227" note and replace with the current behavior: "populated when an established Link exists for the destination_hash; None when no link state is available (e.g., a packet arriving before handshake completes — should not happen for valid traffic)."

- [ ] **Step 1.5: Run test to verify it passes**

```bash
cd ~/work/zeblithic/harmony
set -o pipefail
cargo test --manifest-path crates/harmony-reticulum/Cargo.toml process_data_packet_for_local_destination_populates_source_from_link_table 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 1.6: Run the full link-impacted test suite to verify no regression**

```bash
cd ~/work/zeblithic/harmony
set -o pipefail
cargo test --manifest-path crates/harmony-reticulum/Cargo.toml link 2>&1 | tail -20
cargo test --manifest-path crates/harmony-runtime/Cargo.toml unicast 2>&1 | tail -20
```

Expected: all green. The existing `unicast_round_trip_a_to_b_surfaces_as_unicast_received` test (in `harmony-runtime`) currently asserts `received.0 == None`; if your binding work caused that test to start surfacing a real identity, update its assertion to expect `Some(_)` (the test is in `crates/harmony-runtime/src/runtime.rs`; see commit `b721148`'s test descriptions for reference). Per "test drift is our fault" — fix in this PR.

- [ ] **Step 1.7: Write a failing test for public register_local_destination on NodeRuntime**

Add to the test module at the end of `crates/harmony-runtime/src/runtime.rs`:

```rust
#[test]
fn node_runtime_register_local_destination_accepts_inbound_to_that_dest() {
    // harmony-client (ZEB-227) needs a public API to register the DM
    // destination. Without it the router is unreachable from outside the
    // crate. This test pins the API shape and round-trips through tick().
    let identity = NodeIdentity::generate_for_tests();
    let config = NodeConfig::default();
    let store = MemoryBookStore::default();
    let (mut runtime, _startup) = NodeRuntime::new(config, store);

    let dm_dest = [0xdmu8; 16].map(|_| 0xd1u8);  // pick any 16-byte hash
    runtime.register_local_destination(dm_dest);

    // Build a Type1/Single/Data Reticulum packet addressed to dm_dest.
    let packet = build_type1_data_packet_for_test(dm_dest, b"hello");

    runtime.push_event(RuntimeEvent::InboundPacket {
        interface_name: "udp0".to_string(),
        raw: packet,
        now: 0,
    });
    let actions = runtime.tick();

    let mut found_unicast_received = false;
    for action in actions {
        if let RuntimeAction::UnicastReceived { destination_hash, .. } = action {
            assert_eq!(destination_hash, dm_dest);
            found_unicast_received = true;
        }
    }
    assert!(
        found_unicast_received,
        "registering dm_dest must cause inbound packets addressed to it to surface as UnicastReceived"
    );
}

#[test]
fn node_runtime_unregister_local_destination_returns_bool() {
    let config = NodeConfig::default();
    let store = MemoryBookStore::default();
    let (mut runtime, _) = NodeRuntime::new(config, store);

    let dm_dest = [0xd1u8; 16];
    runtime.register_local_destination(dm_dest);
    assert!(runtime.unregister_local_destination(&dm_dest));
    // Second call: already gone.
    assert!(!runtime.unregister_local_destination(&dm_dest));
}
```

If `build_type1_data_packet_for_test` doesn't already exist as a runtime-test helper, the implementer reuses the corresponding helper from `harmony-reticulum`'s test module via `pub(crate) use` or copies the construction inline.

- [ ] **Step 1.8: Run tests to verify they fail**

```bash
cd ~/work/zeblithic/harmony
set -o pipefail
cargo test --manifest-path crates/harmony-runtime/Cargo.toml node_runtime_register_local_destination 2>&1 | tail -20
```

Expected: FAIL with `no method named 'register_local_destination' found` (and similarly for `unregister_local_destination`).

- [ ] **Step 1.9: Implement the public register/unregister API**

In `crates/harmony-runtime/src/runtime.rs`, add to the `impl<B: BookStore> NodeRuntime<B>` block (near the existing `local_identity_hash`/`set_local_*_announce` methods around `runtime.rs:1623-1644`):

```rust
/// Register a 16-byte Reticulum destination hash for local delivery.
/// Inbound packets addressed to this destination will surface from
/// `tick()` as `RuntimeAction::UnicastReceived { destination_hash, .. }`
/// instead of being dropped with `DropReason::NoLocalDestination`.
///
/// The destination_hash is computed by the caller from
/// `SHA256(name_hash || identity_address_hash)[:16]` per Reticulum
/// destination naming. Idempotent — registering the same hash twice
/// is a no-op.
///
/// Used by harmony-client (ZEB-216 Sub-B Phase 3b) to register its DM
/// destination so inbound DmInvite/DmCidNotify/DmAck packets surface.
pub fn register_local_destination(&mut self, dest_hash: [u8; 16]) {
    self.router.register_destination(dest_hash);
}

/// Unregister a previously-registered local destination. Returns `true`
/// if the destination was present, `false` if it was not.
pub fn unregister_local_destination(&mut self, dest_hash: &[u8; 16]) -> bool {
    self.router.unregister_destination(dest_hash)
}
```

- [ ] **Step 1.10: Run tests to verify they pass**

```bash
cd ~/work/zeblithic/harmony
set -o pipefail
cargo test --manifest-path crates/harmony-runtime/Cargo.toml node_runtime_register_local_destination 2>&1 | tail -10
cargo test --manifest-path crates/harmony-runtime/Cargo.toml node_runtime_unregister_local_destination 2>&1 | tail -5
```

Expected: PASS for both.

- [ ] **Step 1.11: Run full workspace verification**

```bash
cd ~/work/zeblithic/harmony
set -o pipefail
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all green. Investigate and fix any breakage caused by the new API or the link-identity-binding change. Per the "test drift is our fault" rule, broken tests on main caused by your changes belong in this PR, not a follow-up.

If pre-existing fmt drift or clippy warnings outside your diff are in the way, do NOT include them in this PR — leave them as their own follow-up. Phase 3a's PR #267 set the precedent of explicitly stating which pre-existing warnings are NOT in scope.

- [ ] **Step 1.12: Commit**

```bash
cd ~/work/zeblithic/harmony
git add crates/harmony-reticulum/src/node.rs crates/harmony-runtime/src/runtime.rs
git commit -m "$(cat <<'EOF'
feat(zeb-227): wire link-identity binding + public register_local_destination

Two coupled changes the harmony-client DM transport (ZEB-216 Sub-B
Phase 3b, ZEB-227) needs:

1. process_data_packet now looks up the inbound link's remote_identity
   from link_table and populates DeliverLocally.source with
   Some(identity_hash) instead of the placeholder None left by Phase 3a
   (ZEB-226). Without this every inbound DM packet on harmony-client
   would drop as UnknownLinkOrigin (the link-origin-binding security
   spine of ZEB-216 §"Link-origin binding rule").

2. NodeRuntime exposes pub fn register_local_destination + unregister
   delegating to the previously-private router. harmony-client cannot
   reach router::register_destination from outside the crate; without
   these accessors it has no way to tell the runtime to accept inbound
   packets at the DM destination hash.

The unicast_round_trip_a_to_b_surfaces_as_unicast_received test from
ZEB-226 was asserting received.source == None as the placeholder. With
real identity binding, that assertion now expects Some(remote_identity);
test updated.

Test drift policy applied: any pre-existing fmt drift / clippy warnings
in the surrounding tree are NOT addressed here; only the diff covered
by this PR's TDD steps.
EOF
)"
```

- [ ] **Step 1.13: Push and open PR**

```bash
cd ~/work/zeblithic/harmony
git push -u origin zeb-227-runtime-link-identity-binding
gh pr create --title "feat(zeb-227): link-identity binding + register_local_destination API" --body "$(cat <<'EOF'
## Summary
- `process_data_packet` populates `DeliverLocally.source` from the inbound link's `remote_identity`, replacing Phase 3a's placeholder `None`.
- `NodeRuntime::register_local_destination` / `unregister_local_destination` exposed as public APIs (delegating to the previously-private `router`).

## Why
Both changes are blocking dependencies for **harmony-client ZEB-227** (DM transport Phase 3b). Without (1), every inbound DM packet would drop as `UnknownLinkOrigin`. Without (2), harmony-client cannot register the DM destination, so the runtime would drop every inbound DM packet as `NoLocalDestination` before reaching the link-binding check.

Companion PR pattern mirrors Phase 3a (PR #267) — small, targeted runtime-side surface change that the client-side PR consumes.

## Test plan
- [ ] `cargo test -p harmony-reticulum link` — green, including the new `process_data_packet_for_local_destination_populates_source_from_link_table` test.
- [ ] `cargo test -p harmony-runtime unicast` — green, including the updated `unicast_round_trip_a_to_b_surfaces_as_unicast_received` assertion (now `Some(...)` instead of `None`) and the new `node_runtime_register_local_destination_*` tests.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- [ ] `cargo fmt --all -- --check` — clean.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Capture the PR URL printed by `gh pr create` — Task 2 will reference the merge SHA after a human approves and merges this PR. **Pause here.** Do not start Task 2 until Task 1 is merged to harmony main.

---

### Task 2: Bump harmony deps to the Task 1 merge SHA

**Repo:** `harmony-client`
**Branch:** `zeb-227-dm-transport-phase3b`

**Files:**
- Modify: `src-tauri/Cargo.toml` (the `harmony-runtime` and `harmony-content` git revs)

**Pre-condition:** Task 1's PR has been squash-merged in `~/work/zeblithic/harmony`. Capture the merge SHA from `gh pr view <task-1-pr-num> --json mergeCommit -q .mergeCommit.oid` or from `git log` after `git fetch origin main`.

- [ ] **Step 2.1: Branch is ready and current dep state captured**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git checkout zeb-227-dm-transport-phase3b
git log --oneline -1   # confirm starts at 97c2e90
grep -E "harmony-runtime|harmony-content" src-tauri/Cargo.toml
```

Expected output for the grep:

```
harmony-runtime = { git = "https://github.com/zeblithic/harmony.git", rev = "ddf2ce07109eb30526a10bd37af3b0ddc901faa8" }
harmony-content = { git = "https://github.com/zeblithic/harmony.git", rev = "ddf2ce07109eb30526a10bd37af3b0ddc901faa8" }
```

- [ ] **Step 2.2: Resolve the Task 1 merge SHA**

```bash
cd ~/work/zeblithic/harmony
git fetch origin main
git log origin/main --oneline -3
```

The first commit listed should be the squash-merge of Task 1 (subject prefix `feat(zeb-227): wire link-identity binding ...`). Capture its 40-char SHA. For documentation purposes this plan refers to it as `<TASK_1_SHA>`.

- [ ] **Step 2.3: Bump both deps to TASK_1_SHA**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
```

Edit `src-tauri/Cargo.toml`, replacing `ddf2ce07109eb30526a10bd37af3b0ddc901faa8` (both occurrences) with the Task 1 merge SHA. Use the Edit tool with `replace_all` on the substring.

- [ ] **Step 2.4: Resolve dependencies and verify the build**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo update -p harmony-runtime -p harmony-content
set -o pipefail
cargo build --tests 2>&1 | tail -30
```

Expected: clean build. If clippy warnings appear after the dep bump (new lint surface from upstream), fix them — they belong in this PR per "test drift is our fault."

- [ ] **Step 2.5: Verify the new public APIs are reachable**

Quick smoke check that the new `NodeRuntime::register_local_destination` symbol is visible from harmony-client:

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo doc --no-deps --document-private-items 2>&1 | grep -E "register_local_destination|unregister_local_destination" | head -5
# Or, faster: run a one-off cargo check that calls it.
```

If the symbols don't appear, the dep bump didn't pick up Task 1 — recheck `cargo update` and `Cargo.lock`.

- [ ] **Step 2.6: Run the full verification quartet**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
npx tsc --noEmit
```

Expected: all green. (vitest can be skipped on dep-only bumps but include it if any frontend types depend on the bumped Rust types via `tauri-bindgen` or similar.)

- [ ] **Step 2.7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "$(cat <<'EOF'
chore(zeb-227): bump harmony deps to absorb link-identity binding + register_local_destination

harmony PR <task-1-pr-num> merged at <TASK_1_SHA>:
- `DeliverLocally.source` now `Some(identity_hash)` for established links
  (was `None` placeholder from ZEB-226 / Phase 3a).
- `NodeRuntime::register_local_destination` / `unregister_local_destination`
  newly public.

Both unblock the inbound DM demux + DM destination registration that
follow in this PR.
EOF
)"
```

(Replace `<task-1-pr-num>` and `<TASK_1_SHA>` with the actual values captured in Steps 1.13 and 2.2.)

---

### Task 3: Add `DmReceiveError` enum + `resolve_link_origin_owner` helper

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (add error variants near the existing `SendDmError` enum, add helper function)
- Test: `src-tauri/src/dm_outbox.rs` test module

Pure function over `OwnerDeviceCache` — easiest piece to TDD. Per spec §"Link-origin binding rule" (lines 329-403).

- [ ] **Step 3.1: Write failing tests for resolve_link_origin_owner**

Add to the `#[cfg(test)] mod tests` block at the bottom of `src-tauri/src/dm_outbox.rs`:

```rust
#[test]
fn resolve_link_origin_owner_single_match_returns_ok() {
    use crate::owner_state_types::{
        DeviceIdentityHash, OwnerAddr, OwnerDeviceCache, OwnerDeviceEntry, Hlc,
    };
    let mut cache = OwnerDeviceCache::default();
    cache.devices.insert(
        OwnerAddr([1; 16]),
        OwnerDeviceEntry {
            devices: vec![DeviceIdentityHash([0xa1; 16])],
            learned_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        },
    );
    let resolved = resolve_link_origin_owner(&cache, DeviceIdentityHash([0xa1; 16])).unwrap();
    assert_eq!(resolved, OwnerAddr([1; 16]));
}

#[test]
fn resolve_link_origin_owner_no_matches_is_unknown_link_origin() {
    use crate::owner_state_types::{
        DeviceIdentityHash, OwnerAddr, OwnerDeviceCache, OwnerDeviceEntry, Hlc,
    };
    let mut cache = OwnerDeviceCache::default();
    cache.devices.insert(
        OwnerAddr([1; 16]),
        OwnerDeviceEntry {
            devices: vec![DeviceIdentityHash([0xa1; 16])],
            learned_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        },
    );
    let err = resolve_link_origin_owner(&cache, DeviceIdentityHash([0xff; 16])).unwrap_err();
    assert!(matches!(err, DmReceiveError::UnknownLinkOrigin));
}

#[test]
fn resolve_link_origin_owner_multiple_matches_is_ambiguous() {
    // A single DeviceIdentityHash claimed by two different OwnerAddr entries.
    // Per spec, this is unreachable in normal operation — it would mean
    // either corrupted state or a malicious cache-poisoning DmInvite.
    // Either way the resolution is not trustworthy: drop with telemetry.
    use crate::owner_state_types::{
        DeviceIdentityHash, OwnerAddr, OwnerDeviceCache, OwnerDeviceEntry, Hlc,
    };
    let mut cache = OwnerDeviceCache::default();
    let shared = DeviceIdentityHash([0xa1; 16]);
    cache.devices.insert(
        OwnerAddr([1; 16]),
        OwnerDeviceEntry {
            devices: vec![shared],
            learned_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        },
    );
    cache.devices.insert(
        OwnerAddr([2; 16]),
        OwnerDeviceEntry {
            devices: vec![shared],
            learned_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        },
    );
    let err = resolve_link_origin_owner(&cache, shared).unwrap_err();
    assert!(matches!(err, DmReceiveError::AmbiguousLinkOrigin));
}
```

- [ ] **Step 3.2: Run tests to verify they fail**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test resolve_link_origin_owner 2>&1 | tail -20
```

Expected: FAIL — symbol `resolve_link_origin_owner` does not exist; `DmReceiveError` does not have the variants used.

- [ ] **Step 3.3: Add DmReceiveError enum and resolve_link_origin_owner helper**

In `src-tauri/src/dm_outbox.rs`, add the `DmReceiveError` enum (NOT the same as the one in `dm_crypto.rs` — Phase 3b needs additional variants beyond the single-variant `SenderImpersonation` in `dm_crypto`). Place it near the existing `SendDmError`:

```rust
/// Inbound-DM packet handling errors. Each variant maps to a "drop +
/// telemetry" decision in `handle_unicast` per ZEB-216 §"Link-origin
/// binding rule". Distinct from `dm_crypto::DmReceiveError` which only
/// carries the SenderImpersonation case for the encrypted-payload check.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DmReceiveError {
    #[error("from_identity_hash not present in any OwnerDeviceCache entry")]
    UnknownLinkOrigin,
    #[error("from_identity_hash claimed by multiple OwnerDeviceCache entries (corrupted state or cache-poisoning attempt)")]
    AmbiguousLinkOrigin,
    #[error("payload owner field does not match link-origin-resolved owner")]
    OwnerFieldMismatch,
    #[error("DmInvite.inviter must be in DmInvite.members")]
    InviterNotInMembers,
    #[error("from_identity_hash must be in DmInvite.sender_devices")]
    SenderDeviceNotInSenderDevices,
    #[error("self_owner_addr must be in DmInvite.members")]
    ReceiverNotInMembers,
    #[error("ack from owner not in OutboxEntry.recipient_owners")]
    AckFromNonRecipient,
    #[error("OutboxEntry not found for (space_id, message_cid)")]
    OutboxEntryNotFound,
    #[error("DmInvite for an existing Space (already accepted)")]
    InviteForExistingSpace,
    #[error("Space not found for incoming DmCidNotify (we are not a member?)")]
    SpaceNotFound,
    #[error("CAS fetch failed or timed out: {0}")]
    CasFetchFailed(String),
    #[error("DM blob decryption failed under all candidate keys")]
    DecryptFailed,
    #[error("payload sender does not match link-origin OwnerAddr (impersonation)")]
    SenderImpersonation,
    #[error("packet decode failed: {0}")]
    Decode(String),
    #[error("AAD compute failed: {0}")]
    AadCompute(String),
    #[error("CRDT rejected the apply (invariant violation): {0}")]
    CrdtRejected(String),
}
```

Add the `resolve_link_origin_owner` helper. Place it in the same module (private; only `handle_unicast` consumes it):

```rust
use crate::owner_state_types::{DeviceIdentityHash, OwnerDeviceCache};

/// Resolve the inbound link's `from_identity_hash` to the OwnerAddr that
/// owns it, by scanning OwnerDeviceCache entries. MUST match exactly one
/// owner; zero matches → UnknownLinkOrigin, multiple → AmbiguousLinkOrigin.
///
/// Per ZEB-216 §"Link-origin binding rule" — the receive-side bedrock of
/// DM sender-impersonation defense. Every inbound DmCidNotify and DmAck
/// flows through this resolver before any state mutation.
pub(crate) fn resolve_link_origin_owner(
    cache: &OwnerDeviceCache,
    from_identity_hash: DeviceIdentityHash,
) -> Result<OwnerAddr, DmReceiveError> {
    let matches: Vec<OwnerAddr> = cache
        .devices
        .iter()
        .filter(|(_, entry)| entry.devices.binary_search(&from_identity_hash).is_ok())
        .map(|(addr, _)| *addr)
        .collect();
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(DmReceiveError::UnknownLinkOrigin),
        _ => Err(DmReceiveError::AmbiguousLinkOrigin),
    }
}
```

The `OwnerDeviceEntry::devices` invariant (sorted ascending lex, see `owner_state_types.rs:286-307`) is what makes `binary_search` correct here. The deserializer re-establishes the invariant on every load (`owner_state_types.rs:332`), so a corrupted on-disk snapshot or malicious peer can't break the precondition.

- [ ] **Step 3.4: Run tests to verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test resolve_link_origin_owner 2>&1 | tail -10
```

Expected: PASS — all three tests.

- [ ] **Step 3.5: Verification gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

Expected: all green.

- [ ] **Step 3.6: Commit**

```bash
git add src-tauri/src/dm_outbox.rs
git commit -m "$(cat <<'EOF'
feat(zeb-227): add DmReceiveError enum + resolve_link_origin_owner helper

Phase 3b's inbound DM demux needs a single resolver that maps a
Reticulum link's `from_identity_hash` to the OwnerAddr that owns it.
Per ZEB-216 §"Link-origin binding rule" the resolver MUST yield exactly
one match — zero is UnknownLinkOrigin (drop + telemetry), more than
one is AmbiguousLinkOrigin (corrupted state or cache-poisoning attempt;
also drop + telemetry).

Adds a Phase-3b-scoped `DmReceiveError` distinct from the single-variant
`dm_crypto::DmReceiveError` (which only covers the encrypted-payload
sender-impersonation check). All variants needed across handle_invite /
handle_cidnotify / handle_ack are stubbed in upfront so subsequent tasks
can fill in the call sites without re-touching this enum.

The helper is `pub(crate)` — only handle_unicast consumes it; nothing
outside dm_outbox needs the resolution semantics.
EOF
)"
```

---

### Task 4: Add `RuntimeUnicastTransport` adapter struct + `DmTransport` impl

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (add the new adapter type + tests)
- Test: `src-tauri/src/dm_outbox.rs` test module

The adapter pushes outbound `RuntimeEvent::SendUnicastToDevice` via an mpsc channel. The actual `runtime.push_event` call lives in the event_loop's arm that drains this channel (Task 5). Phase 2's `StubTransport` is preserved for use in tests; Phase 3b production code uses `RuntimeUnicastTransport`.

- [ ] **Step 4.1: Write a failing test for RuntimeUnicastTransport::send**

Add to the `#[cfg(test)] mod tests` block at the bottom of `src-tauri/src/dm_outbox.rs`:

```rust
#[tokio::test]
async fn runtime_unicast_transport_send_pushes_event_into_channel() {
    use tokio::sync::mpsc;
    use crate::owner_state_types::{
        ContentId, DeliveryStatus, Hlc, OutboxEntry, OutboxEntryId, OwnerAddr, SpaceId,
    };
    use std::collections::BTreeSet;

    let (tx, mut rx) = mpsc::channel::<UnicastSendRequest>(8);

    // Stub destination resolver: maps recipient OwnerAddr -> known device hashes
    // -> destination_hash. Phase 3b production resolver uses OwnerDeviceCache;
    // for this unit test we hand it a closure that returns one fixed hash.
    let resolver = std::sync::Arc::new(StaticDestResolver::new([
        (OwnerAddr([1; 16]), vec![[0xd1u8; 16]]),
    ]));

    let transport = RuntimeUnicastTransport::new(tx, resolver);
    let entry = OutboxEntry {
        id: OutboxEntryId([0xab; 16]),
        space_id: SpaceId([0xcc; 16]),
        recipient_owners: vec![OwnerAddr([1; 16])],
        message_cid: ContentId::from_bytes([0xee; 32]),
        created_at: Hlc { wall_ms: 100, logical: 0, device_id: "d".into() },
        delivered_to: BTreeSet::new(),
        delivery_status: DeliveryStatus::Pending,
    };

    transport.send(&entry, OwnerAddr([1; 16])).await.unwrap();

    let req = rx.recv().await.expect("channel produced no event");
    assert_eq!(req.destination_hash, [0xd1u8; 16]);
    // The packet body is a CBOR-encoded DmCidNotify with the OutboxEntry's
    // (space_id, message_cid). Decode and verify shape.
    let packet = crate::dm_envelope::decode_packet(&req.packet).unwrap();
    match packet {
        crate::dm_envelope::DmPacket::CidNotify(notify) => {
            assert_eq!(notify.space_id, SpaceId([0xcc; 16]));
            assert_eq!(notify.message_cid, ContentId::from_bytes([0xee; 32]));
            // sender_owner_addr is "diagnostic only" per spec — the real
            // identity comes from link-origin binding on the receive side.
            // But the field is populated by the sender's self_owner.
        }
        other => panic!("expected CidNotify, got {:?}", other),
    }
}

/// Test-only resolver that maps recipient OwnerAddr -> device destination
/// hashes from a fixed table. Production resolver reads OwnerDeviceCache.
struct StaticDestResolver {
    table: HashMap<OwnerAddr, Vec<[u8; 16]>>,
}

impl StaticDestResolver {
    fn new(entries: impl IntoIterator<Item = (OwnerAddr, Vec<[u8; 16]>)>) -> Self {
        Self { table: entries.into_iter().collect() }
    }
}

impl DestinationResolver for StaticDestResolver {
    fn resolve(&self, recipient: OwnerAddr) -> Vec<[u8; 16]> {
        self.table.get(&recipient).cloned().unwrap_or_default()
    }
}
```

- [ ] **Step 4.2: Run test to verify it fails**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test runtime_unicast_transport_send_pushes_event_into_channel 2>&1 | tail -20
```

Expected: FAIL — `UnicastSendRequest`, `RuntimeUnicastTransport`, `DestinationResolver` don't exist yet.

- [ ] **Step 4.3: Implement RuntimeUnicastTransport + supporting types**

In `src-tauri/src/dm_outbox.rs`, after the existing `StubTransport` impl block, add:

```rust
/// Outbound request from a `RuntimeUnicastTransport` to the event-loop.
/// The event-loop drains these and pushes them as
/// `RuntimeEvent::SendUnicastToDevice` into `NodeRuntime`.
#[derive(Debug, Clone)]
pub struct UnicastSendRequest {
    pub destination_hash: [u8; 16],
    pub packet: Vec<u8>,
}

/// Strategy: how to map a recipient OwnerAddr → list of Reticulum
/// destination hashes (one per known bound device of that owner).
///
/// Production impl reads `OwnerDeviceCache`. Test impl is fixed-table.
/// Behind a trait so the unit test can isolate the transport from the
/// CRDT state.
pub trait DestinationResolver: Send + Sync {
    /// Returns the list of 16-byte destination hashes to fan-out to.
    /// May be empty (recipient has no known devices) — caller treats
    /// that as a transient transport error.
    fn resolve(&self, recipient: OwnerAddr) -> Vec<[u8; 16]>;
}

/// Phase 3b production transport. `DmTransport::send` builds a
/// DmCidNotify, encodes it, resolves the recipient's device destination
/// hashes via the injected resolver, and pushes one `UnicastSendRequest`
/// per device hash into the channel that the event-loop drains.
///
/// Cross-device fan-out is per spec (Flow 2 step 5): every known device
/// of the recipient gets its own SendUnicastToDevice. The runtime's
/// per-destination FIFO and cross-destination best-effort ordering
/// guarantees apply (see ZEB-226 round-13 doc).
pub struct RuntimeUnicastTransport {
    tx: tokio::sync::mpsc::Sender<UnicastSendRequest>,
    resolver: std::sync::Arc<dyn DestinationResolver>,
    self_owner: OwnerAddr,
    sender_devices: Vec<DeviceIdentityHash>,
}

impl RuntimeUnicastTransport {
    pub fn new(
        tx: tokio::sync::mpsc::Sender<UnicastSendRequest>,
        resolver: std::sync::Arc<dyn DestinationResolver>,
        self_owner: OwnerAddr,
        sender_devices: Vec<DeviceIdentityHash>,
    ) -> Self {
        Self { tx, resolver, self_owner, sender_devices }
    }
}

#[async_trait]
impl DmTransport for RuntimeUnicastTransport {
    async fn send(&self, entry: &OutboxEntry, recipient: OwnerAddr) -> Result<(), TransportError> {
        let destinations = self.resolver.resolve(recipient);
        if destinations.is_empty() {
            return Err(TransportError::Transient(format!(
                "no known devices for recipient {recipient:?}"
            )));
        }

        let notify = crate::dm_envelope::DmCidNotify {
            space_id: entry.space_id,
            message_cid: entry.message_cid,
            sender_owner_addr: self.self_owner,
            sender_devices: self.sender_devices.clone(),
        };
        let packet = crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::CidNotify(notify))
            .map_err(|e| TransportError::Permanent(format!("encode_packet failed: {e}")))?;

        for dest_hash in destinations {
            let req = UnicastSendRequest {
                destination_hash: dest_hash,
                packet: packet.clone(),
            };
            self.tx.send(req).await.map_err(|e| {
                TransportError::Transient(format!("event-loop channel closed: {e}"))
            })?;
        }
        Ok(())
    }
}
```

Update test imports as needed (the test uses `HashMap`, `tokio::sync::mpsc`).

(Note: this adapter only handles `DmCidNotify` outbound. `DmInvite` outbound is Phase 4's `add_space` IPC for DM kinds — the spec's Flow 1 walks through it. `DmAck` outbound is built by the receive-side `handle_cidnotify` and pushed directly into the same channel — not through `DmTransport::send` because Acks aren't tied to OutboxEntry retry. The plan's Task 8 wires that.)

- [ ] **Step 4.4: Run test to verify it passes**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test runtime_unicast_transport_send_pushes_event_into_channel 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 4.5: Add a no-known-devices test**

```rust
#[tokio::test]
async fn runtime_unicast_transport_no_known_devices_is_transient_error() {
    use tokio::sync::mpsc;
    let (tx, _rx) = mpsc::channel::<UnicastSendRequest>(8);
    let resolver = std::sync::Arc::new(StaticDestResolver::new(std::iter::empty()));
    let transport = RuntimeUnicastTransport::new(
        tx,
        resolver,
        OwnerAddr([0xff; 16]),
        vec![DeviceIdentityHash([7; 16])],
    );

    let entry = OutboxEntry {
        id: OutboxEntryId([0xab; 16]),
        space_id: SpaceId([0xcc; 16]),
        recipient_owners: vec![OwnerAddr([1; 16])],
        message_cid: ContentId::from_bytes([0xee; 32]),
        created_at: Hlc { wall_ms: 100, logical: 0, device_id: "d".into() },
        delivered_to: BTreeSet::new(),
        delivery_status: DeliveryStatus::Pending,
    };

    let err = transport.send(&entry, OwnerAddr([1; 16])).await.unwrap_err();
    assert!(matches!(err, TransportError::Transient(_)));
}
```

Run: `cargo test runtime_unicast_transport_no_known_devices_is_transient_error` — expect PASS.

- [ ] **Step 4.6: Verification gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

Expected: all green.

- [ ] **Step 4.7: Commit**

```bash
git add src-tauri/src/dm_outbox.rs
git commit -m "$(cat <<'EOF'
feat(zeb-227): add RuntimeUnicastTransport adapter

Phase 3b's production DmTransport. Builds a DmCidNotify, encodes it,
and fans out one UnicastSendRequest per known device of the recipient
into the channel that the event-loop drains and forwards into NodeRuntime
as RuntimeEvent::SendUnicastToDevice.

Resolver is behind a trait (DestinationResolver) so the unit test can
inject a fixed-table impl, isolating the transport from OwnerDeviceCache.
The production resolver wires up in Task 5 alongside the channel and
event-loop arm.

StubTransport is preserved (Phase 2's tests still depend on it; gating
to test-cfg is a future cleanup, not required by this PR).
EOF
)"
```

---

### Task 5: Wire outbound mpsc channel + event_loop arm

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (new mpsc channel parameter, new `tokio::select!` arm to drain it and push into `runtime.push_event`)
- Modify: `src-tauri/src/lib.rs` (construct the channel near `cas_op_tx` at line 572; thread it through `event_loop::run`'s call site)

- [ ] **Step 5.1: Write a failing integration-style test for the event_loop arm**

Skip this step. The select-arm wiring is integration-shaped and exercising it in isolation requires standing up a fake `NodeRuntime`, which is too much fixture for a single test. Coverage lands in Task 12's end-to-end integration test. **Exception to the TDD-first rule, justified by fixture cost.** Implementer skips straight to wiring + manual smoke verification, then Task 12's integration test exercises it.

- [ ] **Step 5.2: Add the mpsc channel parameter to event_loop::run**

Edit `src-tauri/src/event_loop.rs:134-162`. Add a new parameter `unicast_send_rx: Option<mpsc::Receiver<crate::dm_outbox::UnicastSendRequest>>` after `crdt_state` (the same Option-pattern as the existing dm_outbox/dm_transport/crdt_state params introduced in Phase 2):

```rust
pub async fn run<R: Runtime>(
    // ... existing 22 params ...
    dm_outbox: Option<std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>>,
    dm_transport: Option<std::sync::Arc<dyn crate::dm_outbox::DmTransport>>,
    crdt_state: Option<std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>>,
    mut unicast_send_rx: Option<mpsc::Receiver<crate::dm_outbox::UnicastSendRequest>>,
) {
```

- [ ] **Step 5.3: Add the new select arm**

Inside the `tokio::select!` block at `event_loop.rs:584` (the main loop), after the existing `Some(op) = cas_op_rx.recv()` arm, add:

```rust
// ── ZEB-227: outbound DM unicast → NodeRuntime ────────────────────
// RuntimeUnicastTransport pushes one UnicastSendRequest per recipient
// device hash into this channel; we forward each as a
// RuntimeEvent::SendUnicastToDevice into NodeRuntime, which queues it
// into pending_unicast_sends and drains in tick() against the path table
// (see ZEB-226's defer-then-drop semantics).
Some(req) = async {
    match unicast_send_rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
} => {
    runtime.push_event(RuntimeEvent::SendUnicastToDevice {
        destination_hash: req.destination_hash,
        packet: req.packet,
    });
    should_tick = true;
}
```

The `async { match ... pending().await }` shim is the same pattern Phase 2 uses for any Optional channel inside `select!` — when `None`, the future never resolves, so the arm is effectively skipped.

- [ ] **Step 5.4: Update event_loop::run callers**

`event_loop::run` is called in three places (per Phase 2's wiring): one in `lib.rs`'s `start_node` and two in test files (`tests/content_index_integration.rs` and `tests/folder_primitive_integration.rs`). Each call site needs `None` (or `Some(channel)` for the production path) appended.

Find them:

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
grep -nE "event_loop::run\b" src-tauri/src/lib.rs src-tauri/tests/*.rs
```

Expected: 3 hits. For the two test files, append `None` as the final argument. For `lib.rs`, defer the production wiring to Step 5.6 (after constructing the channel).

- [ ] **Step 5.5: Add `None` to test call sites**

Edit both `src-tauri/tests/content_index_integration.rs` and `src-tauri/tests/folder_primitive_integration.rs` — append `None` (with a brief inline comment `// unicast_send_rx — DM transport not exercised in this test`) to each `event_loop::run(...)` invocation.

- [ ] **Step 5.6: Construct the channel in lib.rs and thread it through**

In `src-tauri/src/lib.rs` near the existing `cas_op_tx, cas_op_rx` construction (search for `cas_op_tx, cas_op_rx) = tokio::sync::mpsc::channel`, around line 572):

```rust
let (cas_op_tx, cas_op_rx) = tokio::sync::mpsc::channel::<crate::content_store::CasOp>(8);
// ZEB-227: outbound DM unicast channel. Sized at 64 to accommodate group-DM
// fan-out (16 members × 4 devices = 64 worst-case dispatches per send_dm).
let (unicast_send_tx, unicast_send_rx) = tokio::sync::mpsc::channel::<crate::dm_outbox::UnicastSendRequest>(64);
```

Pass `unicast_send_tx` to `RuntimeUnicastTransport::new` (in Task 9 — for now, it's unused but constructed). Pass `Some(unicast_send_rx)` to `event_loop::run` at the existing call site.

You'll also need to lift `unicast_send_tx` to NodeState (so `send_dm` IPC's RuntimeUnicastTransport instantiation in Task 9 can reach it), and clear it in `stop_inner` and the restart path. Mirror the Phase 2 pattern for `dm_outbox`/`dm_transport`/`crdt_state` (`lib.rs:191-220`, `lib.rs:404-490`, `lib.rs:634-690`).

NodeState additions:
```rust
struct NodeState {
    // ... existing ...
    /// Phase 3b: outbound unicast channel. RuntimeUnicastTransport pushes
    /// here; event_loop drains and forwards to NodeRuntime.
    unicast_send_tx: Option<tokio::sync::mpsc::Sender<crate::dm_outbox::UnicastSendRequest>>,
}
```

- [ ] **Step 5.7: Verification gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

Expected: all green. The new arm has no consumers yet, so behavior is unchanged.

- [ ] **Step 5.8: Commit**

```bash
git add src-tauri/src/event_loop.rs src-tauri/src/lib.rs \
        src-tauri/tests/content_index_integration.rs \
        src-tauri/tests/folder_primitive_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-227): wire outbound RuntimeEvent::SendUnicastToDevice channel

Adds the second leg of Phase 3b's outbound DM path:
- New mpsc channel `unicast_send_tx/rx` (capacity 64 — group-DM fan-out
  worst case is 16 members × 4 devices) constructed in start_node.
- New tokio::select! arm in event_loop drains the receiver and forwards
  each UnicastSendRequest into NodeRuntime as
  RuntimeEvent::SendUnicastToDevice; the runtime queues it in
  pending_unicast_sends and resolves on next tick against the path table
  (per ZEB-226's defer-then-drop semantics).
- NodeState gains an unicast_send_tx field so send_dm IPC (Task 9) can
  instantiate RuntimeUnicastTransport on-demand. Stopped + restart
  cleanup mirrors the Phase 2 dm_outbox/dm_transport/crdt_state pattern.

Test call sites (content_index_integration.rs, folder_primitive_integration.rs)
get `None` appended to event_loop::run — they don't exercise DM transport.

The arm has no producers yet (Task 9 wires up RuntimeUnicastTransport).
EOF
)"
```

---

### Task 6: Wire inbound `RuntimeAction::UnicastReceived` interception

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (intercept UnicastReceived before dispatch_action at the three `for action in runtime.tick()` sites)
- Modify: `src-tauri/src/dm_outbox.rs` (add a stub `handle_unicast` skeleton that just decodes + dispatches; full handlers in Tasks 7-9)
- Test: a unit test that the interception path correctly decodes a packet

- [ ] **Step 6.1: Write a failing test for handle_unicast packet dispatch**

Add to the dm_outbox test module:

```rust
#[tokio::test]
async fn handle_unicast_invalid_packet_returns_decode_error() {
    use crate::owner_state_crdt::OwnerState;

    let mut state = OwnerState::default();
    let mut outbox = DmOutbox::new("device".into(), OwnerAddr([0xff; 16]));
    let cas = crate::content_store::InMemoryStub::default();
    let (tx, _rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);

    let bogus_packet = vec![0xff, 0xa0]; // invalid discriminant
    let err = outbox.handle_unicast(
        &mut state,
        &cas,
        &tx,
        Some([0xa1; 16]),
        bogus_packet,
        100, // wall_now_ms
    ).await.unwrap_err();
    assert!(matches!(err, DmReceiveError::Decode(_)));
}

#[tokio::test]
async fn handle_unicast_no_source_drops_packet() {
    // Phase 3b's harmony companion PR populates source from link state
    // when an established Link exists. None should be unreachable for
    // valid DM traffic (every DM packet rides a Link), but defensive
    // handling: drop with telemetry, never fall through to
    // resolve_link_origin_owner with a fabricated identity.
    use crate::owner_state_crdt::OwnerState;

    let mut state = OwnerState::default();
    let mut outbox = DmOutbox::new("device".into(), OwnerAddr([0xff; 16]));
    let cas = crate::content_store::InMemoryStub::default();
    let (tx, _rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);

    let packet = crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::Ack(
        crate::dm_envelope::DmAck {
            space_id: SpaceId([1; 16]),
            message_cid: ContentId::from_bytes([0xee; 32]),
            ack_from_owner_addr: OwnerAddr([2; 16]),
            ack_from_devices: vec![DeviceIdentityHash([7; 16])],
        },
    )).unwrap();

    let err = outbox.handle_unicast(
        &mut state,
        &cas,
        &tx,
        None,  // source unknown
        packet,
        100,
    ).await.unwrap_err();
    assert!(matches!(err, DmReceiveError::UnknownLinkOrigin));
}
```

- [ ] **Step 6.2: Run tests to verify they fail**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test handle_unicast_invalid_packet handle_unicast_no_source 2>&1 | tail -20
```

Expected: FAIL — `handle_unicast` symbol does not exist.

- [ ] **Step 6.3: Implement handle_unicast skeleton (decode + dispatch only)**

In `src-tauri/src/dm_outbox.rs`, add:

```rust
impl DmOutbox {
    /// Inbound DM packet entry point. Decodes, dispatches by discriminant,
    /// runs link-origin-binding sanity checks. Per ZEB-216 §"Link-origin
    /// binding rule", every dispatched arm uses the resolved owner from
    /// `from_identity_hash`, never a payload-controlled owner field.
    ///
    /// `source = None` is unreachable for valid DM traffic (DMs ride
    /// established Reticulum Links; the link handshake binds the remote
    /// identity). Defensive drop with telemetry — never fabricate an
    /// identity to make the resolver succeed.
    ///
    /// Returns `DrainOutcome` to convey newly_delivered / newly_expired
    /// to the caller, who emits IPC events (mirrors `drain`'s shape).
    pub async fn handle_unicast(
        &mut self,
        state: &mut OwnerState,
        cas: &dyn ContentStore,
        unicast_send_tx: &tokio::sync::mpsc::Sender<UnicastSendRequest>,
        source: Option<[u8; 16]>,
        packet_bytes: Vec<u8>,
        wall_now_ms: u64,
    ) -> Result<DrainOutcome, DmReceiveError> {
        let packet = crate::dm_envelope::decode_packet(&packet_bytes)
            .map_err(|e| DmReceiveError::Decode(e.to_string()))?;

        let from_identity_hash = match source {
            Some(h) => DeviceIdentityHash(h),
            None => {
                tracing::warn!("dropped DM packet with unknown link source");
                return Err(DmReceiveError::UnknownLinkOrigin);
            }
        };

        match packet {
            crate::dm_envelope::DmPacket::Invite(invite) => {
                self.handle_invite(state, invite, from_identity_hash, wall_now_ms).await
            }
            crate::dm_envelope::DmPacket::CidNotify(notify) => {
                self.handle_cidnotify(state, cas, unicast_send_tx, notify, from_identity_hash, wall_now_ms).await
            }
            crate::dm_envelope::DmPacket::Ack(ack) => {
                self.handle_ack(state, ack, from_identity_hash, wall_now_ms).await
            }
        }
    }

    /// STUB — Task 7 implements
    pub async fn handle_invite(
        &mut self,
        _state: &mut OwnerState,
        _invite: crate::dm_envelope::DmInvite,
        _from_identity_hash: DeviceIdentityHash,
        _wall_now_ms: u64,
    ) -> Result<DrainOutcome, DmReceiveError> {
        unimplemented!("Task 7")
    }

    /// STUB — Task 8 implements
    pub async fn handle_cidnotify(
        &mut self,
        _state: &mut OwnerState,
        _cas: &dyn ContentStore,
        _unicast_send_tx: &tokio::sync::mpsc::Sender<UnicastSendRequest>,
        _notify: crate::dm_envelope::DmCidNotify,
        _from_identity_hash: DeviceIdentityHash,
        _wall_now_ms: u64,
    ) -> Result<DrainOutcome, DmReceiveError> {
        unimplemented!("Task 8")
    }

    // handle_ack already exists from Phase 2; Task 9 will widen its signature
    // to accept the link-origin-resolved identity. For now:
    pub async fn handle_ack_phase3b_stub(
        &mut self,
        _state: &mut OwnerState,
        _ack: crate::dm_envelope::DmAck,
        _from_identity_hash: DeviceIdentityHash,
        _wall_now_ms: u64,
    ) -> Result<DrainOutcome, DmReceiveError> {
        unimplemented!("Task 9 — replaces existing Phase 2 handle_ack signature")
    }
}
```

(The bogus-packet test should pass once `decode_packet` returns Err; the no-source test should pass once the `None` arm returns `UnknownLinkOrigin`.)

- [ ] **Step 6.4: Run tests to verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test handle_unicast_invalid_packet handle_unicast_no_source 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 6.5: Wire interception in event_loop runtime.tick() loops**

There are three sites in `src-tauri/src/event_loop.rs` where `runtime.tick()` is consumed (per the grep at the start of plan-writing — line 871, line 898, line 1137). Each loops `for action in runtime.tick()` and calls `dispatch_action(action, ...)`. Phase 3b adds an interception:

```rust
for action in runtime.tick() {
    // ZEB-227: intercept inbound DM packets before dispatch_action.
    // RuntimeAction::UnicastReceived is not in dispatch_action's switch
    // (the catch-all _ => {} arm at the bottom would silently drop it).
    if let RuntimeAction::UnicastReceived { destination_hash: _, source, packet } = &action {
        if let (Some(outbox), Some(state), Some(unicast_send_tx)) =
            (dm_outbox.as_ref(), crdt_state.as_ref(), unicast_send_tx_for_loop.as_ref())
        {
            // Same try_lock + skip-this-tick pattern as the dm_outbox drain
            // block in the timer arm. send_dm IPC may hold these locks.
            let outbox_try = outbox.try_lock();
            let state_try = state.try_lock();
            match (outbox_try, state_try) {
                (Ok(mut outbox_g), Ok(mut state_g)) => {
                    let wall_now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let result = outbox_g.handle_unicast(
                        &mut state_g,
                        // CAS — same Arc as send_dm uses; pulled from NodeState in lib.rs
                        // and wired into event_loop::run as a new param (do this in Task 9).
                        unimplemented_cas_handle,
                        unicast_send_tx,
                        *source,
                        packet.clone(),
                        wall_now_ms,
                    ).await;
                    drop(state_g);
                    drop(outbox_g);
                    match result {
                        Ok(outcome) => {
                            for (entry_id, recipient) in outcome.newly_delivered {
                                let _ = app.emit("dm-delivered", serde_json::json!({
                                    "messageId": hex::encode(entry_id.0),
                                    "recipient": hex::encode(recipient.0),
                                }));
                            }
                            for entry_id in outcome.newly_expired {
                                let _ = app.emit("dm-expired", serde_json::json!({
                                    "messageId": hex::encode(entry_id.0),
                                }));
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "handle_unicast dropped packet");
                        }
                    }
                }
                _ => {
                    tracing::debug!("handle_unicast skipped this tick (locks contended); packet dropped");
                    // NOTE: Unlike drain (which can retry on the next tick), the
                    // packet is in our hand right now — dropping it loses the
                    // event. Phase 3b ships with this behavior; a follow-up
                    // ticket can investigate buffering. See ZEB-? (filed at
                    // PR creation in Task 12).
                }
            }
            continue; // Don't fall through to dispatch_action for this action.
        }
        // Falls through if dm_outbox isn't initialized — packet drops in dispatch_action's catch-all.
    }
    dispatch_action(action, /* ...existing args... */).await;
}
```

The `unimplemented_cas_handle` placeholder — Task 9 wires the real CAS handle into `event_loop::run`'s parameter list. For Task 6, the cleanest approach: skip the inbound interception arm entirely if Task 6's `handle_unicast` requires CAS (it does, transitively through `handle_cidnotify`). Solution:

**Refactor Step 6.5:** instead of plumbing the CAS handle in Task 6, keep Task 6 strictly to the wiring pattern with a TODO comment that Task 9 fills in. The interception block stays, but the inner call to `handle_unicast` is left as `unimplemented!("Task 9 wires cas")`. This may cause clippy warnings about unused variables — gate the interception block behind `#[cfg(any())]` / `if false` for Task 6, OR move the wiring entirely into Task 9.

**Decision:** roll wiring of the inbound interception into Task 9 (where the CAS handle plumbing happens anyway). Task 6 ships only the `dm_outbox::handle_unicast` skeleton + the unit tests for decode + None-source drops. The event_loop interception is deferred to Task 9.

Update Task 6's scope to drop Step 6.5 (and 6.6/6.7 collapse into just the gates + commit).

- [ ] **Step 6.5 (revised): Verification gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

Expected: all green. The skeleton's `unimplemented!()` arms are unreachable through the public API (no caller invokes them yet — Task 9 wires the event_loop side).

If clippy complains about the unreachable code in `handle_invite` / `handle_cidnotify` / `handle_ack_phase3b_stub` (each is `unimplemented!()`), use `#[allow(clippy::unimplemented)]` at the function level OR add a `#[cfg(test)]` gate so they only exist for the test that exercises decode + None-source. Prefer the cfg(test) approach so the symbols don't ship as production-reachable stubs.

Actually, the cleanest path: don't ship stubs at all. Implement minimal real bodies that just return `Err(DmReceiveError::Decode("not yet implemented"))` so clippy / tests see a finite function. Tasks 7-9 replace the bodies. The decode + None-source tests still pass without exercising the unimplemented arms.

- [ ] **Step 6.6: Commit**

```bash
git add src-tauri/src/dm_outbox.rs
git commit -m "$(cat <<'EOF'
feat(zeb-227): add handle_unicast skeleton + decode dispatch

Phase 3b's inbound DM entry point. Decodes the wire bytes into a
DmPacket discriminant, runs the source presence check (None drops with
telemetry per spec — DMs MUST ride established Links), and dispatches
to handle_invite / handle_cidnotify / handle_ack by variant.

The three downstream handlers ship as minimal placeholder bodies that
return DmReceiveError::Decode("not yet implemented") — Tasks 7-9 fill
them in. This is enough surface for the decode + None-source unit tests
to exercise the dispatch path; full coverage lands in Tasks 7-9.

The event_loop interception that calls handle_unicast is deferred to
Task 9 — it needs the CAS handle threading that handle_cidnotify
introduces.
EOF
)"
```

---

### Task 7: Implement `handle_invite` (auto-accept on valid; sanity gates per spec)

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (replace handle_invite stub with real body)
- Test: `src-tauri/src/dm_outbox.rs` test module — covers spec tests `handle_unicast_invite_creates_space`, `handle_unicast_invite_binds_inviter_field_not_members_zero`, `handle_unicast_invite_inviter_not_in_members_drops`, `handle_unicast_invite_sender_device_not_in_sender_devices_drops`, `handle_unicast_invite_receiver_not_in_members_drops`

**Scope decision:** Phase 3b auto-accepts every invite that passes the sanity gates. The `handle_unicast_invite_decline_writes_no_state` spec test is reframed: in Phase 3b without UI, "decline" is the structural-validity drop path (already covered by the three drop tests below). The user-driven decline UX (modal + accept/decline IPC) is deferred to Phase 4 alongside the rest of the DM UI surface, with a follow-up Linear ticket filed at Task 12.

- [ ] **Step 7.1: Write failing tests for handle_invite happy + drop paths**

Add to the dm_outbox test module:

```rust
#[tokio::test]
async fn handle_invite_writes_space_and_owner_device_cache_entry() {
    // ZEB-216 spec test: handle_unicast_invite_creates_space
    use crate::owner_state_crdt::OwnerState;
    let mut state = OwnerState::default();
    let mut outbox = DmOutbox::new("device".into(), OwnerAddr([2; 16]));

    let invite = crate::dm_envelope::DmInvite {
        space_id: SpaceId([7; 16]),
        kind: SpaceKind::Dm,
        members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
        inviter: OwnerAddr([1; 16]),
        content_key: DmContentKey::new([0xaa; 32]),
        sender_devices: vec![DeviceIdentityHash([0xa1; 16])],
        created_at: Hlc { wall_ms: 100, logical: 0, device_id: "alice".into() },
    };
    let from_identity_hash = DeviceIdentityHash([0xa1; 16]);

    outbox.handle_invite(&mut state, invite, from_identity_hash, 200).await.unwrap();

    // Space written.
    assert!(state.spaces.contains_key(&SpaceId([7; 16])));
    let space = state.spaces.get(&SpaceId([7; 16])).unwrap();
    assert_eq!(space.kind, SpaceKind::Dm);
    assert!(space.content_key.is_some());

    // OwnerDeviceCache updated under invite.inviter (NOT members[0]).
    assert!(state.owner_device_cache.devices.contains_key(&OwnerAddr([1; 16])));
}

#[tokio::test]
async fn handle_invite_binds_inviter_field_not_members_zero() {
    // ZEB-216 spec test: handle_unicast_invite_binds_inviter_field_not_members_zero
    // Group-DM where invite.inviter is the lex-LARGEST member (so members[0]
    // is a different OwnerAddr). Cache entry must be created under
    // invite.inviter, NOT members[0].
    use crate::owner_state_crdt::OwnerState;
    let mut state = OwnerState::default();
    let mut outbox = DmOutbox::new("device".into(), OwnerAddr([2; 16]));

    let inviter_addr = OwnerAddr([0xff; 16]);  // lex-largest
    let invite = crate::dm_envelope::DmInvite {
        space_id: SpaceId([7; 16]),
        kind: SpaceKind::GroupDm,
        members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16]), inviter_addr],
        inviter: inviter_addr,
        content_key: DmContentKey::new([0xaa; 32]),
        sender_devices: vec![DeviceIdentityHash([0xa1; 16])],
        created_at: Hlc { wall_ms: 100, logical: 0, device_id: "alice".into() },
    };
    let from_identity_hash = DeviceIdentityHash([0xa1; 16]);

    outbox.handle_invite(&mut state, invite, from_identity_hash, 200).await.unwrap();

    // Cache entry under inviter_addr, NOT members[0] (which is OwnerAddr([1; 16])).
    assert!(state.owner_device_cache.devices.contains_key(&inviter_addr));
    assert!(!state.owner_device_cache.devices.contains_key(&OwnerAddr([1; 16])));
}

#[tokio::test]
async fn handle_invite_inviter_not_in_members_drops() {
    // ZEB-216 spec test: handle_unicast_invite_inviter_not_in_members_drops
    use crate::owner_state_crdt::OwnerState;
    let mut state = OwnerState::default();
    let mut outbox = DmOutbox::new("device".into(), OwnerAddr([2; 16]));

    let invite = crate::dm_envelope::DmInvite {
        space_id: SpaceId([7; 16]),
        kind: SpaceKind::Dm,
        members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
        inviter: OwnerAddr([3; 16]),  // NOT in members
        content_key: DmContentKey::new([0xaa; 32]),
        sender_devices: vec![DeviceIdentityHash([0xa1; 16])],
        created_at: Hlc { wall_ms: 100, logical: 0, device_id: "alice".into() },
    };
    let err = outbox.handle_invite(&mut state, invite, DeviceIdentityHash([0xa1; 16]), 200).await.unwrap_err();
    assert!(matches!(err, DmReceiveError::InviterNotInMembers));
    assert!(!state.spaces.contains_key(&SpaceId([7; 16])));
    assert!(state.owner_device_cache.devices.is_empty());
}

#[tokio::test]
async fn handle_invite_sender_device_not_in_sender_devices_drops() {
    // ZEB-216 spec test: handle_unicast_invite_sender_device_not_in_sender_devices_drops
    use crate::owner_state_crdt::OwnerState;
    let mut state = OwnerState::default();
    let mut outbox = DmOutbox::new("device".into(), OwnerAddr([2; 16]));

    let invite = crate::dm_envelope::DmInvite {
        space_id: SpaceId([7; 16]),
        kind: SpaceKind::Dm,
        members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
        inviter: OwnerAddr([1; 16]),
        content_key: DmContentKey::new([0xaa; 32]),
        sender_devices: vec![DeviceIdentityHash([0xa1; 16])],  // does NOT include from_identity_hash
        created_at: Hlc { wall_ms: 100, logical: 0, device_id: "alice".into() },
    };
    let err = outbox.handle_invite(
        &mut state,
        invite,
        DeviceIdentityHash([0xff; 16]),  // not in sender_devices
        200,
    ).await.unwrap_err();
    assert!(matches!(err, DmReceiveError::SenderDeviceNotInSenderDevices));
    assert!(!state.spaces.contains_key(&SpaceId([7; 16])));
}

#[tokio::test]
async fn handle_invite_receiver_not_in_members_drops() {
    // ZEB-216 spec test: handle_unicast_invite_receiver_not_in_members_drops
    use crate::owner_state_crdt::OwnerState;
    let mut state = OwnerState::default();
    let mut outbox = DmOutbox::new("device".into(), OwnerAddr([99; 16]));  // NOT in invite.members

    let invite = crate::dm_envelope::DmInvite {
        space_id: SpaceId([7; 16]),
        kind: SpaceKind::Dm,
        members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],  // self_owner not here
        inviter: OwnerAddr([1; 16]),
        content_key: DmContentKey::new([0xaa; 32]),
        sender_devices: vec![DeviceIdentityHash([0xa1; 16])],
        created_at: Hlc { wall_ms: 100, logical: 0, device_id: "alice".into() },
    };
    let err = outbox.handle_invite(&mut state, invite, DeviceIdentityHash([0xa1; 16]), 200).await.unwrap_err();
    assert!(matches!(err, DmReceiveError::ReceiverNotInMembers));
    assert!(!state.spaces.contains_key(&SpaceId([7; 16])));
}
```

- [ ] **Step 7.2: Run tests to verify they fail**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test handle_invite_ 2>&1 | tail -25
```

Expected: FAIL — handle_invite is the placeholder body returning `DmReceiveError::Decode("not yet implemented")`.

- [ ] **Step 7.3: Implement handle_invite real body**

Replace the stub body in `src-tauri/src/dm_outbox.rs`:

```rust
pub async fn handle_invite(
    &mut self,
    state: &mut OwnerState,
    invite: crate::dm_envelope::DmInvite,
    from_identity_hash: DeviceIdentityHash,
    _wall_now_ms: u64,  // reserved for future use
) -> Result<DrainOutcome, DmReceiveError> {
    // Sanity gate 1: inviter must be in members.
    if !invite.members.contains(&invite.inviter) {
        tracing::warn!("dropped DmInvite: inviter not in members");
        return Err(DmReceiveError::InviterNotInMembers);
    }
    // Sanity gate 2: from_identity_hash must be in sender_devices.
    // Note: the wire decoder doesn't enforce sort on DmInvite.sender_devices
    // (unlike OwnerDeviceEntry.devices, which is invariant-checked at
    // deserialize); use linear .contains() rather than binary_search to
    // avoid a silent "not present, but binary_search lied" path.
    if !invite.sender_devices.contains(&from_identity_hash) {
        tracing::warn!("dropped DmInvite: from_identity_hash not in sender_devices");
        return Err(DmReceiveError::SenderDeviceNotInSenderDevices);
    }
    // Sanity gate 3: receiver (us) must be in members.
    if !invite.members.contains(&self.self_owner) {
        tracing::warn!("dropped DmInvite: self_owner_addr not in members");
        return Err(DmReceiveError::ReceiverNotInMembers);
    }

    // Phase 3b auto-accept: write the Space and update OwnerDeviceCache.
    // Phase 4 will replace this with a stage-pending-invite + UI prompt
    // path (see follow-up Linear ticket — filed at Task 12).

    // OwnerDeviceCache update keyed by invite.inviter (NOT members[0]).
    let cache_outcome = state.apply_owner_device_update(
        invite.inviter,
        invite.sender_devices.clone(),
        invite.created_at.clone(),
    );
    if let crate::owner_state_crdt::ApplyOutcome::Rejected(reason) = cache_outcome {
        return Err(DmReceiveError::CrdtRejected(format!("{:?}", reason)));
    }

    // Build the Space from the invite. Mirrors the wire-side fields that
    // dm_crypto::compute_aad will hash into the dedupe_key. The transport
    // binding is Reticulum (DM kinds always are).
    let space = crate::owner_state_types::Space {
        id: invite.space_id,
        kind: invite.kind,
        parent: None,
        community_id: None,
        name: format!("DM with {:?}", invite.inviter),
        transport: Some(crate::owner_state_types::TransportBinding::Reticulum {
            participants: invite.members.clone(),
        }),
        members: invite.members,
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: invite.created_at.clone(),
        updated_at: invite.created_at,
        content_key: Some(invite.content_key),
        prior_content_keys: vec![],
    };
    let space_outcome = state.apply_space_with_canonicalization(space);
    if let crate::owner_state_crdt::ApplyOutcome::Rejected(reason) = space_outcome {
        return Err(DmReceiveError::CrdtRejected(format!("{:?}", reason)));
    }

    Ok(DrainOutcome::default())
}
```

- [ ] **Step 7.4: Run tests to verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test handle_invite_ 2>&1 | tail -15
```

Expected: PASS — all five tests.

- [ ] **Step 7.5: Verification gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

Expected: all green.

- [ ] **Step 7.6: Commit**

```bash
git add src-tauri/src/dm_outbox.rs
git commit -m "$(cat <<'EOF'
feat(zeb-227): implement handle_invite — sanity gates + auto-accept

Phase 3b's inbound DmInvite handler. Runs the three structural sanity
gates from ZEB-216 §"Link-origin binding rule":

1. invite.inviter must be in invite.members
2. from_identity_hash must be in invite.sender_devices
3. self_owner_addr must be in invite.members

On all three passing, auto-accepts:
- apply_owner_device_update(invite.inviter, invite.sender_devices, ...)
- apply_space_with_canonicalization(Space { from invite })

The cache update is keyed by invite.inviter, NOT members[0]
(invite.members is sorted lex for canonical CBOR; members[0] is the
lex-smallest OwnerAddr — NOT the inviter — and binding to it would be
wrong for any group-DM where the inviter isn't lex-smallest).

Phase 3b ships auto-accept; the user-driven decline UX (modal + IPC)
is deferred to Phase 4 with a follow-up Linear ticket filed at
PR-creation time. Until then, structural-validity drops cover the
"no state written" spec test cases.

Tests added:
- handle_invite_writes_space_and_owner_device_cache_entry
- handle_invite_binds_inviter_field_not_members_zero (regression for
  the lex-vs-inviter binding bug surfaced in spec)
- handle_invite_inviter_not_in_members_drops
- handle_invite_sender_device_not_in_sender_devices_drops
- handle_invite_receiver_not_in_members_drops
EOF
)"
```

---

### Task 8: Implement `handle_cidnotify` (CAS fetch + decrypt + apply_inbox + ack fan-out)

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (replace handle_cidnotify stub with real body)
- Test: `src-tauri/src/dm_outbox.rs` test module — covers spec tests `handle_unicast_cidnotify_triggers_cas_fetch_decrypt_inbox_write`, `handle_unicast_cidnotify_duplicate_no_dm_received_emit`, `handle_unicast_cidnotify_sender_binding_mismatch_drops`, `handle_unicast_cidnotify_owner_field_mismatch_drops_no_cache_update`, `handle_unicast_cidnotify_unknown_link_origin_drops`, `handle_unicast_cidnotify_decrypt_failure_uses_prior_keys`

This is the largest single task — handles CAS fetch, decryption, sender-binding check, inbox write, and DmAck fan-out. Per spec Flow 2 steps 7-13.

- [ ] **Step 8.1: Write failing test for happy-path cidnotify**

Add a comprehensive happy-path test plus the five drop-path tests. The happy-path test exercises:
1. CAS pre-seeded with the encrypted blob: compute `message_cid` via `harmony_content::cid::ContentId::for_book(...)` (same call site as `dm_outbox.rs:234`), then `cas.put(message_cid, blob).await` (the trait's caller-provides-cid pattern).
2. handle_cidnotify is called with the notify
3. State now has an InboxEntry; the `outcome.newly_delivered` would be empty (sender doesn't get newly_delivered events; that's the recipient's IPC); the function emits via the unicast_send_tx an outbound DmAck for each device in notify.sender_devices

Sketch (the implementer fills in the missing fixtures, mirroring patterns from `dm_send_integration.rs`):

```rust
#[tokio::test]
async fn handle_cidnotify_happy_path_writes_inbox_and_fans_out_ack() {
    // 1. Set up state with: a DM Space (ck included), an OwnerDeviceCache
    //    entry mapping Alice's identity_hash to OwnerAddr Alice, our
    //    self_owner = Bob.
    // 2. Pre-seed CAS with encrypt_dm_message(...) so CasOp::GetOrFetch
    //    succeeds.
    // 3. Call handle_cidnotify with a DmCidNotify whose
    //    sender_owner_addr = Alice and sender_devices = [Alice's hashes].
    // 4. Assert: state.inbox now has an InboxEntry under (space_id, message_cid).
    // 5. Assert: rx (the ack channel) received K UnicastSendRequest entries,
    //    one per device in notify.sender_devices, each carrying a DmAck.
    // ... [full body — implementer expands ~80 lines following dm_send_integration.rs patterns]
}

// [Plus 5 drop-path tests per spec — each follows the structural pattern
// of Task 7's drop tests: build state + invite, call handle_cidnotify,
// assert specific Err variant + assert no state mutation + assert no
// outbound packet.]
```

The implementer reads `tests/dm_send_integration.rs` and `dm_outbox.rs:send_dm` for the encryption + CAS-write pattern they need to mirror in the test setup.

- [ ] **Step 8.2: Run tests to verify they fail**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test handle_cidnotify_ 2>&1 | tail -30
```

Expected: FAIL — handle_cidnotify is still the placeholder.

- [ ] **Step 8.3: Implement handle_cidnotify**

Replace the stub body. The structure mirrors spec Flow 2 steps 7-13:

```rust
pub async fn handle_cidnotify(
    &mut self,
    state: &mut OwnerState,
    cas: &dyn ContentStore,
    unicast_send_tx: &tokio::sync::mpsc::Sender<UnicastSendRequest>,
    notify: crate::dm_envelope::DmCidNotify,
    from_identity_hash: DeviceIdentityHash,
    wall_now_ms: u64,
) -> Result<DrainOutcome, DmReceiveError> {
    // Step 7a: resolve link origin to the OwnerAddr that owns from_identity_hash.
    let resolved_owner = resolve_link_origin_owner(&state.owner_device_cache, from_identity_hash)?;

    // Step 7b: verify notify.sender_owner_addr matches resolved owner.
    if notify.sender_owner_addr != resolved_owner {
        tracing::warn!("dropped DmCidNotify: notify.sender_owner_addr does not match resolved owner");
        return Err(DmReceiveError::OwnerFieldMismatch);
    }

    // Look up the Space for the AAD + content_key. If we're not a member,
    // no Space exists for us — drop with telemetry.
    let space = state.spaces.get(&notify.space_id)
        .ok_or(DmReceiveError::SpaceNotFound)?;
    let space_clone = space.clone();  // needed because we'll mutate state below

    // Step 8: refresh OwnerDeviceCache with notify.sender_devices (LWW HLC-bound).
    let _ = state.apply_owner_device_update(
        resolved_owner,  // NOT notify.sender_owner_addr (use resolved per spec)
        notify.sender_devices.clone(),
        crate::owner_state_types::Hlc {
            wall_ms: wall_now_ms,
            logical: 0,
            device_id: self.device_id.clone(),
        },
    );  // ApplyOutcome ignored — Rejected is acceptable here (stale HLC = our cache is fresher)

    // Step 9: fetch the storage_blob from CAS. The ContentStore trait's `get`
    // is the entry point — production impl wraps this as a CasOp::GetOrFetch
    // over the cas_op channel, which the runtime resolves locally then via
    // Zenoh DAG-sync; the 500ms timeout caps both legs.
    let blob = match tokio::time::timeout(
        std::time::Duration::from_millis(500),
        cas.get(&notify.message_cid),
    ).await {
        Ok(Ok(Some(bytes))) => bytes,
        Ok(Ok(None)) => return Err(DmReceiveError::CasFetchFailed("blob not found".into())),
        Ok(Err(e)) => return Err(DmReceiveError::CasFetchFailed(format!("{e:?}"))),
        Err(_) => return Err(DmReceiveError::CasFetchFailed("500ms fetch timeout".into())),
    };

    // Step 11: decrypt the blob (current key + prior keys fallback).
    let aad = crate::dm_crypto::compute_aad(&space_clone)
        .map_err(|e| DmReceiveError::AadCompute(e.to_string()))?;
    let prior_keys: Vec<DmContentKey> = space_clone.prior_content_keys.clone();
    let payload = crate::dm_crypto::decrypt_dm_message(
        space_clone.content_key.as_ref().expect("DM Space MUST have content_key (validate_invariants)"),
        &prior_keys,
        &aad,
        &blob,
    ).map_err(|_| DmReceiveError::DecryptFailed)?;

    // Step 12: sender-binding check.
    crate::dm_crypto::verify_sender_binding(&payload, resolved_owner)
        .map_err(|_| DmReceiveError::SenderImpersonation)?;

    // Step 13a: apply_inbox — atomic-emit semantics.
    let inbox_entry = crate::owner_state_types::InboxEntry {
        space_id: notify.space_id,
        message_cid: notify.message_cid,
        from: resolved_owner,
        received_at: crate::owner_state_types::Hlc {
            wall_ms: wall_now_ms,
            logical: 0,
            device_id: self.device_id.clone(),
        },
    };
    let outcome = state.apply_inbox(inbox_entry);
    let mut drain_outcome = DrainOutcome::default();
    if matches!(outcome, crate::owner_state_crdt::ApplyOutcome::Inserted) {
        // Caller (event_loop) will emit `dm-received` IPC for newly-applied entries.
        // Phase 3b: caller derives this from the apply_inbox return value, not from
        // a separate Inserted-vs-Merged signal in DrainOutcome. For simplicity, we
        // emit the dm-received here directly as a side effect — wait, no, that
        // requires AppHandle which we don't have. Plumb via DrainOutcome:
        drain_outcome.newly_delivered.push((
            crate::owner_state_types::OutboxEntryId(notify.message_cid.to_bytes()[..16].try_into().expect("32→16 truncate")),
            resolved_owner,
        ));
        // ^^^ TODO: that overload of newly_delivered is wrong shape — newly_delivered
        // is for OUTBOX deliveries (sender side). Receiver-side dm-received needs
        // its own DrainOutcome field. The implementer adds DrainOutcome.newly_received:
        // Vec<InboxEntry> (or similar) at this task and updates the event_loop arm
        // to emit dm-received from it.
    }

    // Step 13b: ack fan-out to all sender_devices.
    let ack = crate::dm_envelope::DmAck {
        space_id: notify.space_id,
        message_cid: notify.message_cid,
        ack_from_owner_addr: self.self_owner,
        ack_from_devices: vec![/* our own device hashes — needs OwnerDeviceCache lookup */],
    };
    let ack_packet = crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::Ack(ack))
        .map_err(|e| DmReceiveError::Decode(e.to_string()))?;

    for device in &notify.sender_devices {
        // Compute the destination_hash: SHA256(name_hash || identity_address_hash)[:16]
        // ... [implementer wires this up; might need a helper in lib.rs that uses
        //     NodeRuntime::local_identity_hash + a fixed DM destination name string]
        let dest_hash = compute_dm_destination_hash(device);
        let req = UnicastSendRequest { destination_hash: dest_hash, packet: ack_packet.clone() };
        let _ = unicast_send_tx.send(req).await;  // failed sends are silent per spec Flow 2 step 13
    }

    Ok(drain_outcome)
}

fn compute_dm_destination_hash(device: &DeviceIdentityHash) -> [u8; 16] {
    // SHA256("harmony.dm".as_bytes() concatenated with device identity hash)[:16]
    use sha2::{Sha256, Digest};
    let name_hash = Sha256::digest(b"harmony.dm");
    let mut h = Sha256::new();
    h.update(&name_hash[..16]);  // Reticulum name_hash is 16 bytes
    h.update(&device.0);
    let out = h.finalize();
    out[..16].try_into().expect("SHA256 output is 32 bytes")
}
```

**The DrainOutcome shape problem:** `DrainOutcome.newly_delivered` is currently typed as `Vec<(OutboxEntryId, OwnerAddr)>` for outbox-side delivery events. Receive-side `dm-received` is a different concept — it needs `Vec<InboxEntry>` or `Vec<(SpaceId, ContentId, OwnerAddr)>`.

**Decision:** widen `DrainOutcome` to add a `newly_received: Vec<InboxEntry>` field at this task. Update Phase 2's drain to leave it empty (drain doesn't produce inbox events). Update event_loop's drain arm AND the new handle_unicast arm to emit `dm-received` from this field. This is a small, targeted refactor, not a redesign.

**Decision on ack_from_devices:** for Phase 3b's first cut, populate `ack_from_devices` with whatever this device knows about its own bound devices (OwnerDeviceCache entry for `self.self_owner`). If the cache doesn't yet have the entry (first-ever DM), populate with just our own device — Phase 3b ships the minimal correct behavior; growing this list as more devices come online is automatic via Flow A.

- [ ] **Step 8.4: Run tests to verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test handle_cidnotify_ 2>&1 | tail -25
```

Expected: PASS — all six tests.

- [ ] **Step 8.5: Add the dm-received emit path in DrainOutcome**

The widening: add to `DrainOutcome`:

```rust
pub struct DrainOutcome {
    pub newly_delivered: Vec<(OutboxEntryId, OwnerAddr)>,
    pub newly_expired: Vec<OutboxEntryId>,
    /// Phase 3b: InboxEntries written by handle_cidnotify for which
    /// apply_inbox returned Inserted (not NoOp). Caller emits dm-received.
    pub newly_received: Vec<crate::owner_state_types::InboxEntry>,
}
```

Phase 2's `drain` leaves it empty. Task 9 wires the event_loop side to emit `dm-received` from this field.

- [ ] **Step 8.6: Verification gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

Expected: all green.

- [ ] **Step 8.7: Commit**

```bash
git add src-tauri/src/dm_outbox.rs
git commit -m "$(cat <<'EOF'
feat(zeb-227): implement handle_cidnotify — CAS fetch, decrypt, inbox write, ack fan-out

Phase 3b's inbound DmCidNotify handler — the largest single inbound
arm. Implements ZEB-216 spec Flow 2 steps 7-13:

7a. resolve_link_origin_owner(cache, from_identity_hash)
7b. verify notify.sender_owner_addr == resolved_owner (drop on mismatch
    — cache-poisoning attempt regression per spec)
8.  apply_owner_device_update(resolved_owner, notify.sender_devices,
    HLC) — uses RESOLVED owner, not payload field (load-bearing)
9.  CasOp::GetOrFetch(message_cid) with 500ms timeout
11. decrypt_dm_message with current + prior content_keys fallback
12. verify_sender_binding(payload.sender == resolved_owner) — drop on
    impersonation
13a. apply_inbox(InboxEntry); on ApplyOutcome::Inserted, push into
     DrainOutcome.newly_received (NEW field) so the caller emits
     dm-received IPC. NoOp duplicates are silent (atomic-emit semantics
     per spec — the inserted-vs-merged discriminant is the boundary,
     not a separate pre-write existence check).
13b. DmAck fan-out to all devices in notify.sender_devices (per spec
     "fan out ack to ALL sender devices, not just A1" — liveness benefit
     when the original sender's primary device went offline). Failed
     sends are silent per spec.

DrainOutcome widened with newly_received: Vec<InboxEntry>. Phase 2 drain
leaves it empty; Task 9 wires event_loop to emit dm-received from it.

Tests added (six, mirroring spec):
- handle_cidnotify_happy_path_writes_inbox_and_fans_out_ack
- handle_cidnotify_duplicate_no_dm_received_emit (atomic-emit regression)
- handle_cidnotify_sender_binding_mismatch_drops
- handle_cidnotify_owner_field_mismatch_drops_no_cache_update (cache-
  poisoning regression)
- handle_cidnotify_unknown_link_origin_drops
- handle_cidnotify_decrypt_failure_uses_prior_keys (prior-key fallback)
EOF
)"
```

---

### Task 9: Implement `handle_ack` (Phase 3b version) + wire event_loop interception

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (replace existing Phase 2 `handle_ack` with link-origin-binding version + tests)
- Modify: `src-tauri/src/event_loop.rs` (wire RuntimeAction::UnicastReceived interception, plumb cas handle through `event_loop::run`)
- Test: `src-tauri/src/dm_outbox.rs` test module — covers spec tests `handle_unicast_ack_updates_outbox_delivered_to`, `handle_unicast_ack_owner_field_mismatch_drops`, `handle_unicast_ack_from_non_recipient_drops`, `handle_unicast_ack_ambiguous_link_origin_drops`

This task subsumes:
1. Phase 3b version of handle_ack (link-origin binding + non-recipient drop check)
2. Wiring event_loop's interception of RuntimeAction::UnicastReceived → outbox.handle_unicast(...)
3. Plumbing the `Arc<dyn ContentStore>` handle through `event_loop::run` parameters
4. Wiring DrainOutcome.newly_received → `dm-received` IPC emit

- [ ] **Step 9.1: Write failing tests for handle_ack happy + drop paths**

Add the four spec-named tests, following the same fixture pattern as Task 7.

- [ ] **Step 9.2: Run tests to verify they fail**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test handle_ack_ 2>&1 | tail -20
```

Expected: FAIL — Phase 3b handle_ack signature has changed (now takes `from_identity_hash`).

- [ ] **Step 9.3: Replace Phase 2 handle_ack with Phase 3b version**

```rust
pub async fn handle_ack(
    &mut self,
    state: &mut OwnerState,
    ack: crate::dm_envelope::DmAck,
    from_identity_hash: DeviceIdentityHash,
    wall_now_ms: u64,
) -> Result<DrainOutcome, DmReceiveError> {
    // Resolve link origin → owner.
    let resolved_owner = resolve_link_origin_owner(&state.owner_device_cache, from_identity_hash)?;

    // Verify ack.ack_from_owner_addr matches resolved.
    if ack.ack_from_owner_addr != resolved_owner {
        tracing::warn!("dropped DmAck: ack_from_owner_addr does not match resolved owner");
        return Err(DmReceiveError::OwnerFieldMismatch);
    }

    // Find the OutboxEntry for this (space_id, message_cid).
    // outbox is keyed by entry_id, so iterate to find the match.
    let entry_id = state.outbox.iter()
        .find(|(_, e)| e.space_id == ack.space_id && e.message_cid == ack.message_cid)
        .map(|(id, _)| *id)
        .ok_or(DmReceiveError::OutboxEntryNotFound)?;

    let entry = state.outbox.get(&entry_id).expect("just looked it up");
    if !entry.recipient_owners.contains(&resolved_owner) {
        tracing::warn!("dropped DmAck: ack from non-recipient {resolved_owner:?}");
        return Err(DmReceiveError::AckFromNonRecipient);
    }

    // Update OwnerDeviceCache with ack.ack_from_devices.
    let _ = state.apply_owner_device_update(
        resolved_owner,
        ack.ack_from_devices.clone(),
        crate::owner_state_types::Hlc {
            wall_ms: wall_now_ms,
            logical: 0,
            device_id: self.device_id.clone(),
        },
    );

    // Mutate the OutboxEntry: insert into delivered_to, recompute status.
    let mut entry_mut = state.outbox.get(&entry_id).unwrap().clone();
    let was_already_delivered = !entry_mut.delivered_to.insert(resolved_owner);
    entry_mut.delivery_status = entry_mut.compute_status(false);

    let mut drain_outcome = DrainOutcome::default();
    if !was_already_delivered {
        // First time we've seen this recipient ack — emit dm-delivered.
        drain_outcome.newly_delivered.push((entry_id, resolved_owner));
    }
    // Re-write through CRDT for cross-device convergence.
    let _ = state.apply_outbox(entry_mut);

    Ok(drain_outcome)
}
```

(If the existing Phase 2 `handle_ack` had a different signature — search `dm_outbox.rs` for `fn handle_ack` — the implementer first checks whether any caller depends on the old signature. Per the Phase 2 dm_outbox.rs module doc at line 16, `handle_ack` is not yet wired into a public path; the only caller is the test module. So the signature change has bounded blast radius.)

- [ ] **Step 9.4: Run tests to verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test handle_ack_ 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 9.5: Wire event_loop interception of RuntimeAction::UnicastReceived**

This is the chunk Task 6 deferred. Update `event_loop::run` signature to add `cas_handle: Option<Arc<dyn crate::content_store::ContentStore>>`:

```rust
pub async fn run<R: Runtime>(
    // ... existing ...
    dm_outbox: Option<...>,
    dm_transport: Option<...>,
    crdt_state: Option<...>,
    unicast_send_rx: Option<...>,
    cas_handle: Option<std::sync::Arc<dyn crate::content_store::ContentStore>>,
)
```

In each `for action in runtime.tick()` loop site, intercept `RuntimeAction::UnicastReceived` BEFORE `dispatch_action`:

```rust
for action in runtime.tick() {
    if let RuntimeAction::UnicastReceived { destination_hash: _, source, ref packet } = action {
        if let (Some(outbox), Some(state), Some(unicast_send_tx), Some(cas)) =
            (dm_outbox.as_ref(), crdt_state.as_ref(), unicast_send_tx_for_loop.as_ref(), cas_handle.as_ref())
        {
            let outbox_try = outbox.try_lock();
            let state_try = state.try_lock();
            match (outbox_try, state_try) {
                (Ok(mut outbox_g), Ok(mut state_g)) => {
                    let wall_now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let result = outbox_g.handle_unicast(
                        &mut state_g,
                        cas.as_ref(),
                        unicast_send_tx,
                        source,
                        packet.clone(),
                        wall_now_ms,
                    ).await;
                    drop(state_g);
                    drop(outbox_g);
                    match result {
                        Ok(outcome) => {
                            for entry in outcome.newly_received {
                                let _ = app.emit("dm-received", serde_json::json!({
                                    "spaceId": hex::encode(entry.space_id.0),
                                    "messageCid": hex::encode(entry.message_cid.to_bytes()),
                                    "from": hex::encode(entry.from.0),
                                    "receivedAt": entry.received_at.wall_ms,
                                }));
                            }
                            for (entry_id, recipient) in outcome.newly_delivered {
                                let _ = app.emit("dm-delivered", serde_json::json!({
                                    "messageId": hex::encode(entry_id.0),
                                    "recipient": hex::encode(recipient.0),
                                }));
                            }
                            for entry_id in outcome.newly_expired {
                                let _ = app.emit("dm-expired", serde_json::json!({
                                    "messageId": hex::encode(entry_id.0),
                                }));
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "handle_unicast dropped packet");
                        }
                    }
                }
                _ => {
                    tracing::debug!("handle_unicast skipped this tick (locks contended); packet dropped");
                }
            }
            continue;
        }
    }
    dispatch_action(action, /* ... */).await;
}
```

There are three `runtime.tick()` sites in `event_loop.rs` (per the earlier grep — line 871, 898, 1137). Apply the same intercept block to all three. Consider extracting the intercept logic into a helper function `async fn handle_runtime_action_or_dispatch(action, ...)` to avoid the 3× duplication.

- [ ] **Step 9.6: Update event_loop::run callers**

Same as Task 5: three callers (lib.rs, content_index_integration.rs, folder_primitive_integration.rs). Append `None` to the test files; pass `Some(cas_handle.clone())` from lib.rs (lift NodeState's content_store Arc up — Phase 2 already added a content_store field, so the Arc is reachable).

- [ ] **Step 9.7: Verification gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

Expected: all green.

- [ ] **Step 9.8: Commit**

```bash
git add src-tauri/src/dm_outbox.rs src-tauri/src/event_loop.rs src-tauri/src/lib.rs \
        src-tauri/tests/content_index_integration.rs \
        src-tauri/tests/folder_primitive_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-227): implement handle_ack + wire event_loop UnicastReceived interception

Phase 3b's inbound DmAck handler. Replaces the Phase 2 placeholder
signature with the link-origin-binding shape:
- Resolve from_identity_hash → OwnerAddr via resolve_link_origin_owner.
- Verify ack.ack_from_owner_addr matches resolved (drop on impersonation).
- Find OutboxEntry by (space_id, message_cid); drop if not present
  (OutboxEntryNotFound).
- Drop if resolved owner not in OutboxEntry.recipient_owners
  (AckFromNonRecipient — forged-ack regression per spec).
- apply_owner_device_update with ack.ack_from_devices.
- delivered_to.insert(resolved); idempotent if already present.
- compute_status; if newly delivered, push into DrainOutcome.newly_delivered
  for caller's dm-delivered emit.
- apply_outbox writes back through CRDT for cross-device convergence.

Wires event_loop's interception of RuntimeAction::UnicastReceived BEFORE
dispatch_action (the dispatch_action catch-all _ => {} arm at line 1351
would silently drop UnicastReceived otherwise). Same try_lock + skip-this-
tick pattern as the dm_outbox drain block. Plumbs cas_handle through
event_loop::run as a new Optional Arc parameter.

Three runtime.tick() sites updated (line 871, 898, 1137) — extracted
the intercept into a helper to avoid 3× duplication.

DrainOutcome.newly_received → app.emit("dm-received", ...) wired here;
newly_delivered + newly_expired emit unchanged from Phase 2.

Tests added (four, mirroring spec):
- handle_ack_updates_outbox_delivered_to
- handle_ack_owner_field_mismatch_drops
- handle_ack_from_non_recipient_drops
- handle_ack_ambiguous_link_origin_drops
EOF
)"
```

---

### Task 10: Wire DM destination registration in lib.rs start_node

**Files:**
- Modify: `src-tauri/src/lib.rs` (in start_node, compute the local DM destination hash and call `runtime.register_local_destination(dm_dest)`)

- [ ] **Step 10.1: Compute the local DM destination hash**

Per Reticulum: `destination_hash = SHA256(name_hash || identity_address_hash)[:16]`. The `name_hash` for the DM destination is `SHA256("harmony.dm")[:16]` (or whatever convention the spec lands on; the spec is open about the exact destination naming). Add a helper near `start_node`:

```rust
/// Compute the 16-byte Reticulum destination hash for our local DM inbox.
/// `identity_hash` comes from `NodeRuntime::local_identity_hash()`.
fn compute_dm_destination_hash(identity_hash: [u8; 16]) -> [u8; 16] {
    use sha2::{Sha256, Digest};
    let name_hash = Sha256::digest(b"harmony.dm");
    let mut h = Sha256::new();
    h.update(&name_hash[..16]);
    h.update(&identity_hash);
    let out = h.finalize();
    out[..16].try_into().expect("SHA256 output is 32 bytes")
}
```

In `start_node`, after `NodeRuntime::new`, register the DM destination:

```rust
let local_identity = runtime.local_identity_hash();
let dm_dest = compute_dm_destination_hash(local_identity);
runtime.register_local_destination(dm_dest);
tracing::info!(
    "registered DM destination hash {} for inbound DmInvite/DmCidNotify/DmAck",
    hex::encode(dm_dest)
);
```

The `compute_dm_destination_hash` function is also needed inside `dm_outbox.rs` for the ack fan-out path (Task 8, Step 8.3). Move it to a shared location — e.g., `src-tauri/src/dm_envelope.rs` or a new `dm_destination.rs` — so both call sites use the same canonical implementation.

- [ ] **Step 10.2: Add a unit test for compute_dm_destination_hash**

```rust
#[test]
fn dm_destination_hash_is_deterministic_per_identity() {
    let identity = [0xa1u8; 16];
    let dest1 = compute_dm_destination_hash(identity);
    let dest2 = compute_dm_destination_hash(identity);
    assert_eq!(dest1, dest2);
}

#[test]
fn dm_destination_hash_differs_per_identity() {
    let dest_alice = compute_dm_destination_hash([0xa1; 16]);
    let dest_bob = compute_dm_destination_hash([0xb2; 16]);
    assert_ne!(dest_alice, dest_bob);
}
```

- [ ] **Step 10.3: Verification gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

Expected: all green.

- [ ] **Step 10.4: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/dm_destination.rs  # (or wherever the helper lives)
git commit -m "$(cat <<'EOF'
feat(zeb-227): register local DM destination on NodeRuntime at startup

Computes destination_hash = SHA256("harmony.dm" name_hash || local_identity_hash)[:16]
in start_node and calls runtime.register_local_destination(dm_dest).

Without this registration, every inbound DmInvite/DmCidNotify/DmAck
addressed to our DM destination would drop in the runtime as
NoLocalDestination before reaching the link-origin-binding check.

Helper compute_dm_destination_hash extracted to a shared module so the
ack-fan-out path in handle_cidnotify (Task 8) uses the canonical
implementation.

Two unit tests pin determinism + identity-sensitivity.
EOF
)"
```

---

### Task 11: Replace `StubTransport` with `RuntimeUnicastTransport` in production wiring

**Files:**
- Modify: `src-tauri/src/lib.rs` (replace `StubTransport::new()` at line 843-844 with the real adapter)

- [ ] **Step 11.1: Replace the production transport instantiation**

In `src-tauri/src/lib.rs`, around line 835-844 (where Phase 2's StubTransport is instantiated), replace:

```rust
let transport: std::sync::Arc<dyn crate::dm_outbox::DmTransport> =
    std::sync::Arc::new(crate::dm_outbox::StubTransport::new());
```

with:

```rust
// Production resolver: looks up recipient device hashes from OwnerDeviceCache.
let resolver = std::sync::Arc::new(crate::dm_outbox::OwnerDeviceCacheResolver::new(
    crdt_state.clone(),
));
// Sender device list: our own device's identity hash. Phase 3b ships with
// a single-device sender_devices list; cross-device piggyback grows
// automatically as more of our devices come online and Flow A propagates
// the OwnerDeviceCache entry.
let our_device_hash = DeviceIdentityHash(local_identity);
let transport: std::sync::Arc<dyn crate::dm_outbox::DmTransport> =
    std::sync::Arc::new(crate::dm_outbox::RuntimeUnicastTransport::new(
        unicast_send_tx.clone(),
        resolver,
        self_owner,
        vec![our_device_hash],
    ));
```

`OwnerDeviceCacheResolver` is a thin trait impl that holds `Arc<Mutex<OwnerState>>` and reads `owner_device_cache.devices.get(&recipient).devices` to produce the list of destination hashes (one per device). Add it to `dm_outbox.rs`:

```rust
pub struct OwnerDeviceCacheResolver {
    state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
}

impl OwnerDeviceCacheResolver {
    pub fn new(state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>) -> Self {
        Self { state }
    }
}

impl DestinationResolver for OwnerDeviceCacheResolver {
    fn resolve(&self, recipient: OwnerAddr) -> Vec<[u8; 16]> {
        // try_lock is intentional — we're called from the transport's send()
        // path which may be invoked from event_loop's drain block while
        // dm_outbox + crdt_state are already locked. If the lock is contended
        // we return [] which surfaces as TransportError::Transient (the drain
        // retries on next tick). Avoiding await/blocking here is critical to
        // prevent the same lock-during-await deadlock chain we hit in Phase 2.
        let state = match self.state.try_lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        state.owner_device_cache.devices.get(&recipient)
            .map(|entry| {
                entry.devices.iter()
                    .map(|d| compute_dm_destination_hash(d.0))
                    .collect()
            })
            .unwrap_or_default()
    }
}
```

(Wait — `compute_dm_destination_hash` takes `[u8; 16]` and that's the `DeviceIdentityHash.0` field. The spec specifies destination_hash = SHA256(name_hash || identity_address_hash)[:16] where `identity_address_hash` IS the 16-byte device identity hash. So the helper signature is right; just adjust naming so it's obvious the input is the recipient's device identity, not a destination hash already.)

- [ ] **Step 11.2: Verification gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

Expected: all green. Phase 2's existing dm_outbox tests still pass — they explicitly construct `StubTransport` (which is preserved for test use) so production replacing the wired transport doesn't affect them.

- [ ] **Step 11.3: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/dm_outbox.rs
git commit -m "$(cat <<'EOF'
feat(zeb-227): replace StubTransport with RuntimeUnicastTransport in production

The Phase 2 wiring at lib.rs:843-844 instantiated StubTransport directly
in start_node — that's now swapped for the real adapter:

- OwnerDeviceCacheResolver: a thin DestinationResolver impl that reads
  OwnerDeviceCache via try_lock (avoiding the lock-during-await deadlock
  chain we hit in Phase 2). Returns [] on contention; transport then
  surfaces TransportError::Transient and the drain retries.
- RuntimeUnicastTransport wired with our_device_hash (single-device
  sender_devices for now; cross-device piggyback grows via Flow A).

StubTransport is preserved for test use. Phase 2's dm_outbox tests
explicitly construct StubTransport so this swap is transparent to them.

Production sends now go: send_dm IPC → DmOutbox → drain → transport
→ unicast_send_tx → event_loop arm → runtime.push_event(SendUnicast...)
→ runtime.tick() → SendOnInterface → UDP. Real Reticulum delivery.
EOF
)"
```

---

### Task 12: End-to-end integration test at the RuntimeAction-channel boundary

**Files:**
- Create: `src-tauri/tests/dm_unicast_integration.rs` (new file, ~250 lines)

The test stands up two `DmOutbox` instances + two `OwnerState`s + a fake `NodeRuntime` channel pair. Sends a DM from outbox A; intercepts the outbound `RuntimeEvent::SendUnicastToDevice` from the channel; converts it to an inbound `RuntimeAction::UnicastReceived` for outbox B; calls `B.handle_unicast(...)`; asserts InboxEntry written + DmAck queued; loops back the ack to outbox A; asserts OutboxEntry.delivered_to updated.

This is the Phase 3b acceptance criterion: spec §"Acceptance criteria / Phase 3b" — "real-transport tests pass at the RuntimeAction-channel boundary."

- [ ] **Step 12.1: Write the test file**

```rust
//! ZEB-227 / Phase 3b end-to-end integration test.
//!
//! Mocks at the RuntimeAction channel boundary (no real Reticulum wire).
//! Exercises: send_dm IPC → outbox.send_dm → drain → transport
//! → unicast_send_tx (channel) → handle_unicast (B) → CAS fetch → decrypt
//! → apply_inbox → ack queue → handle_ack (A) → delivered_to updated.

use harmony_client::dm_outbox::{
    /* ... */
};
// [implementer fills in the full body — single test of ~150 lines plus
//  one test for the offline-recipient → online-recipient case (~100 lines)]
```

The implementer writes this as two test functions:
1. `dm_full_round_trip_through_unicast_channel` — happy path.
2. `dm_offline_recipient_then_online_delivers` — exercises the drain backoff path: first send fails (resolver returns []), recipient comes online (cache populated), next drain succeeds.

- [ ] **Step 12.2: Run tests to verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test --test dm_unicast_integration 2>&1 | tail -20
```

Expected: PASS.

- [ ] **Step 12.3: Verification gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

Expected: all green.

- [ ] **Step 12.4: Commit**

```bash
git add src-tauri/tests/dm_unicast_integration.rs
git commit -m "$(cat <<'EOF'
test(zeb-227): end-to-end DM round-trip at RuntimeAction-channel boundary

Phase 3b acceptance criterion (spec §Acceptance criteria / Phase 3b):
"real-transport tests pass at the RuntimeAction-channel boundary."

Two test functions in the new dm_unicast_integration.rs file:

1. dm_full_round_trip_through_unicast_channel
   - Stand up outbox A + outbox B with their own OwnerStates.
   - Pre-populate B's cache with A's identity_hash → A's OwnerAddr.
   - Pre-populate A's cache with B's identity_hash → B's OwnerAddr.
   - Pre-create the DM Space on both sides (Invite-bypass for test brevity;
     handle_invite's coverage is in dm_outbox unit tests).
   - Call A.send_dm(...). Drain A's outbox. Intercept the outbound
     UnicastSendRequest from the channel. Convert to RuntimeAction::
     UnicastReceived addressed to B. Call B.handle_unicast(...).
   - Assert: B.state.inbox now has the InboxEntry; outbox B's channel
     emitted a DmAck; B's outcome.newly_received is non-empty.
   - Intercept the DmAck UnicastSendRequest. Call A.handle_unicast(...).
   - Assert: A.state.outbox[entry_id].delivered_to includes B; A's
     outcome.newly_delivered is non-empty.

2. dm_offline_recipient_then_online_delivers
   - Same setup but A's resolver returns [] (B has no known devices).
   - First drain: TransportError::Transient; backoff schedules retry.
   - Populate A's cache with B's devices. Wait past backoff window.
   - Next drain: succeeds. End-to-end as above.

Mocks at the channel boundary, NOT the wire — no UDP, no real Reticulum.
Real Reticulum coverage lives in the manual two-device LAN smoke
follow-up (filed at PR creation in Task 13).
EOF
)"
```

---

### Task 13: Open the harmony-client PR + file follow-ups

**Process task — no code changes. Push the branch, open the PR, file follow-ups for any deferred work.**

- [ ] **Step 13.1: Push the branch**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git push -u origin zeb-227-dm-transport-phase3b
```

- [ ] **Step 13.2: Open the PR**

```bash
gh pr create --title "feat(zeb-227): DM transport Phase 3b — real Reticulum unicast adapter + inbound demux" --body "$(cat <<'EOF'
## Summary
- Replaces the Phase 2 `StubTransport` with a real `RuntimeUnicastTransport` adapter that pushes `RuntimeEvent::SendUnicastToDevice` into `NodeRuntime`.
- Adds inbound `RuntimeAction::UnicastReceived` interception in `event_loop`, dispatching to `dm_outbox::handle_invite` / `handle_cidnotify` / `handle_ack` per packet discriminant.
- Implements link-origin binding (per ZEB-216 §"Link-origin binding rule") — every inbound DM packet's payload-controlled owner field is verified against the resolved owner from the inbound link's identity_hash.
- Auto-accepts valid `DmInvite` packets (sanity-gated per spec); user-driven decline UX deferred to Phase 4.
- Registers the local DM destination on `NodeRuntime` at startup so inbound packets surface.

## Why
ZEB-216 Phase 3b — the umbrella DM-transport feature's last big infra step. Phase 4 (UI) consumes the IPC events this PR adds (`dm-received`, `dm-delivered`).

## Cross-repo
- Companion harmony PR #<task-1-pr-num> shipped first: terminal-link → identity binding (so `DeliverLocally.source` is `Some(_)`) + public `NodeRuntime::register_local_destination`. Both blocking deps for this PR.

## Test plan
- [ ] `cargo fmt --all -- --check` — clean.
- [ ] `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings` — clean.
- [ ] `cargo test --manifest-path src-tauri/Cargo.toml` — all green, including:
  - `dm_outbox::resolve_link_origin_owner_*` (3 tests)
  - `dm_outbox::runtime_unicast_transport_*` (2 tests)
  - `dm_outbox::handle_unicast_*` (2 tests)
  - `dm_outbox::handle_invite_*` (5 tests)
  - `dm_outbox::handle_cidnotify_*` (6 tests)
  - `dm_outbox::handle_ack_*` (4 tests)
  - `dm_outbox::dm_destination_hash_*` (2 tests)
  - `tests/dm_unicast_integration.rs` (2 tests)
- [ ] `npx tsc --noEmit` — clean.
- [ ] `npx vitest run` — all green.
- [ ] Manual two-device LAN round-trip — deferred to follow-up (filed below).

## Follow-ups filed
- ZEB-? — Phase 3b user-driven decline UX (modal + accept_dm_invite/decline_dm_invite IPC). Phase 3b ships auto-accept.
- ZEB-? — Manual two-device LAN smoke test scenarios (per spec §"Manual testing — deferred to follow-up"). Includes 30-day expiration via sim clock, group-DM 3-5 members, sender-online recipient-offline → online, ack lost → retry, multi-device receiver via Flow A, dedupe collision + prior_content_keys merge, DmInvite at-16/at-17.
- ZEB-? — Inbound packet drop on lock contention (handle_unicast skipped this tick) currently loses the event. Investigate buffering vs. higher-priority lock ordering. Phase 3b ships with the drop behavior.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 13.3: File the three follow-up Linear tickets**

Per user memory rule "never invent Linear IDs" — file the issues, then update the PR body with the assigned IDs.

For each follow-up listed in the PR body, use `mcp__plugin_linear_linear__save_issue` (or `gh issue create` if Linear MCP isn't reachable in this session) with a descriptive title:

1. "ZEB-216 Phase 4 — DM invite decline UX (modal + accept/decline IPC)"
2. "ZEB-216 Phase 3b — manual two-device LAN smoke scenarios (30-day expiration via sim clock, group-DM, multi-device, dedupe collision, at-17 invite block)"
3. "ZEB-227 follow-up — inbound DM packet drop on lock contention (investigate buffering vs lock-ordering)"

After Linear assigns IDs, update the PR body via `gh pr edit <pr-num> --body "..."`.

- [ ] **Step 13.4: Print the PR URL for the user**

Print the URL returned by `gh pr create` so the user can review.

---

## Verification gates (run at every commit, per user memory)

```bash
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
npx tsc --noEmit  # if frontend types touched
npx vitest run    # if frontend code touched
```

Per pipe-exit-codes-lie rule: any verification command that pipes through `tail`/`grep` MUST set `pipefail` first or the failure exit code is silently lost.

---

## Spec coverage cross-check

| Spec test name | Plan task |
|---|---|
| `handle_unicast_invite_creates_space` | Task 7, Step 7.1 (`handle_invite_writes_space_and_owner_device_cache_entry`) |
| `handle_unicast_invite_binds_inviter_field_not_members_zero` | Task 7, Step 7.1 (`handle_invite_binds_inviter_field_not_members_zero`) |
| `handle_unicast_invite_inviter_not_in_members_drops` | Task 7, Step 7.1 (`handle_invite_inviter_not_in_members_drops`) |
| `handle_unicast_invite_sender_device_not_in_sender_devices_drops` | Task 7, Step 7.1 (`handle_invite_sender_device_not_in_sender_devices_drops`) |
| `handle_unicast_invite_receiver_not_in_members_drops` | Task 7, Step 7.1 (`handle_invite_receiver_not_in_members_drops`) |
| `handle_unicast_invite_decline_writes_no_state` | Deferred to Phase 4 (no UI in 3b); structural-validity drops above already cover "no state written" cases. Phase 4 follow-up filed at Task 13. |
| `handle_unicast_cidnotify_triggers_cas_fetch_decrypt_inbox_write` | Task 8, Step 8.1 (`handle_cidnotify_happy_path_writes_inbox_and_fans_out_ack`) |
| `handle_unicast_cidnotify_duplicate_no_dm_received_emit` | Task 8, Step 8.1 (`handle_cidnotify_duplicate_no_dm_received_emit`) |
| `handle_unicast_cidnotify_sender_binding_mismatch_drops` | Task 8, Step 8.1 (`handle_cidnotify_sender_binding_mismatch_drops`) |
| `handle_unicast_cidnotify_owner_field_mismatch_drops_no_cache_update` | Task 8, Step 8.1 (`handle_cidnotify_owner_field_mismatch_drops_no_cache_update`) |
| `handle_unicast_cidnotify_unknown_link_origin_drops` | Task 8, Step 8.1 (`handle_cidnotify_unknown_link_origin_drops`) |
| `handle_unicast_cidnotify_decrypt_failure_uses_prior_keys` | Task 8, Step 8.1 (`handle_cidnotify_decrypt_failure_uses_prior_keys`) |
| `handle_unicast_ack_updates_outbox_delivered_to` | Task 9, Step 9.1 (`handle_ack_updates_outbox_delivered_to`) |
| `handle_unicast_ack_owner_field_mismatch_drops` | Task 9, Step 9.1 (`handle_ack_owner_field_mismatch_drops`) |
| `handle_unicast_ack_from_non_recipient_drops` | Task 9, Step 9.1 (`handle_ack_from_non_recipient_drops`) |
| `handle_unicast_ack_ambiguous_link_origin_drops` | Task 9, Step 9.1 (`handle_ack_ambiguous_link_origin_drops`) |
| `expiration_at_30day_boundary_marks_expired` | Already covered by Phase 2 tests (drain handles expiration; Task 12's offline-recipient test exercises the drain path through Phase 3b's transport). |
| `expiration_29day_old_entry_stays_pending` | Already covered by Phase 2 tests. |
| `expiration_30day_real_transport_path` | Manual LAN follow-up (Task 13.3). |

All Phase 3b spec tests are mapped to plan tasks (one deferred per the auto-accept scope decision, one deferred to manual LAN, two existing-coverage).

---

## Out of scope (intentional)

- User-driven DmInvite decline UX (modal + IPC) — Phase 4 follow-up.
- Forward secrecy / content-key rotation — ZEB-219 deferral.
- Per-device delivery lease in OutboxEntry — v1 tolerates cross-device duplicate sends.
- HLC-monotonic per-OwnerAddr `device_list_version` to suppress redundant `sender_devices` piggyback.
- DmReactions, DmReadReceipts.
- Voice/video DM transport.
