# ZEB-227 — ZEB-216 Sub-B Phase 3b: Real Reticulum DM Transport (Path B — Application-Signature Binding) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Phase 2 `StubTransport` with a real harmony-runtime adapter (raw Type1 Data via `path_table`, NOT Reticulum links) and add inbound `UnicastReceived` demux (`DmInvite` / `DmCidNotify` / `DmAck`) with **per-device Ed25519 signature binding on every packet body** so DMs work end-to-end with full sender-impersonation defense.

**Architecture (Path B):** The original spec assumed Reticulum link-layer ECDH would provide authenticated source identity. Investigation revealed harmony's `Node` has no terminal-link state at endpoint destinations — `Link::respond` is unwired. Wiring it would be a multi-PR feature in its own right. **Path B** instead authenticates sender identity at the application layer: every Reticulum DM packet body carries a `signing_device_hash` field and an appended Ed25519 signature. Sender signs with their device-Identity Ed25519 key. Receiver verifies against the public key looked up via OwnerDeviceCache (post-bootstrap) or inline `inviter_signing_pub` (DmInvite bootstrap). On verification success, `signing_device_hash` IS the authenticated `from_identity_hash`; downstream checks use the OwnerAddr resolved from this hash. See `docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md` §"Application-signature binding rule" (commit `ea38132`).

**Tech Stack:** Rust (`tokio`, `async-trait`, `chacha20poly1305`, `ed25519-dalek`, `ciborium`, `tracing`), Tauri 2 IPC, harmony-runtime + harmony-content (cross-repo deps), Reticulum unicast plane B (via harmony-runtime's existing `RuntimeAction::SendUnicastToDevice` / `RuntimeAction::UnicastReceived` per ZEB-226 Phase 3a).

**Cross-repo:** A small companion PR in `~/work/zeblithic/harmony` lands first (Task 1) — exposes public `NodeRuntime::register_local_destination` + `NodeRuntime::lookup_destination_identity` accessors. harmony-client then bumps the dep pin to that SHA (Task 2) and proceeds. Phase 3a's `RuntimeAction::UnicastReceived.source: Option<[u8; 16]>` field stays `None` forever in Path B; harmony-client does not read it.

**Spec:** `docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md` (commit `ea38132`).

**Branch:** `zeb-227-dm-transport-phase3b` (branched from `origin/main` at `97c2e90`, the just-merged Phase 2 PR #79).

**Important note on dropped Path A:** The branch `zeb-227-runtime-link-identity-binding` exists in `~/work/zeblithic/harmony` from the previous Path A attempt with no commits. Discard it before starting Task 1: `cd ~/work/zeblithic/harmony && git checkout main && git branch -D zeb-227-runtime-link-identity-binding`.

---

## File Structure

### Cross-repo: harmony companion PR (Task 1, branch `zeb-227-runtime-destination-api` in `~/work/zeblithic/harmony`)

| File | Change | Why |
|---|---|---|
| `crates/harmony-runtime/src/runtime.rs` | Add public `NodeRuntime::register_local_destination(&mut self, dest_hash: [u8; 16])` and `unregister_local_destination(&mut self, dest_hash: &[u8; 16]) -> bool` delegating to the private `router.register_destination` / `router.unregister_destination`. Add public `NodeRuntime::lookup_destination_identity(&self, dest_hash: &[u8; 16]) -> Option<&Identity>` that reads from the existing announce / path table. Add unit tests for all three. | harmony-client must register its DM destination so inbound packets surface as `UnicastReceived`. It must also look up sender public keys for signature verification — both APIs are gated behind the private `router: Node` field today (`runtime.rs:754`). |

Total harmony delta: ~60-90 lines + tests. Single squash-merge PR.

### harmony-client (Tasks 2-13, branch `zeb-227-dm-transport-phase3b`)

| File | Action | Estimated lines |
|---|---|---|
| `src-tauri/Cargo.toml` | Modify: bump `harmony-runtime` and `harmony-content` git revs to the harmony companion-PR merge SHA. Confirm `ed25519-dalek` is in deps (it likely already is via `harmony-identity` or `harmony-crypto` workspace re-export; verify and add direct dep if needed). | +4 modified |
| `src-tauri/src/owner_state_types.rs` | Modify: extend `OwnerDeviceEntry` to store per-device Ed25519 verifying keys alongside identity hashes. Two parallel sorted vecs (preserves binary-search invariant) OR a single `Vec<(DeviceIdentityHash, [u8; 32])>` — pick the lower-friction option after reading the existing struct. Add `serde(default)` so persisted Phase 1/2 OwnerDeviceCache snapshots load with empty pubkey vec (graceful upgrade). | +~50 |
| `src-tauri/src/dm_envelope.rs` | Modify: rename `DmInvite`/`DmCidNotify`/`DmAck` to `DmInviteSigned`/`DmCidNotifySigned`/`DmAckSigned` (the wire CBOR body); add `signing_device_hash` field to all three; add `inviter_signing_pub: [u8; 32]` to DmInviteSigned. Replace single-struct `DmPacket` variants with `{ signed: ..., signature: [u8; 64], signed_bytes: Vec<u8> }` (the receive handler needs the bytes the signature covers without re-encoding). Update `encode_packet` to canonical-CBOR-encode the body, append the 64-byte signature, prepend the discriminant. Update `decode_packet` to split `[disc][body][sig:64]` and capture `signed_bytes`. Add `DecodeError::TooShortForSignature`. | +~150 |
| `src-tauri/src/dm_crypto.rs` | Modify: parameter rename `link_origin` → `resolved_owner` on `verify_sender_binding` (semantics unchanged; only the parameter name and doc comment). | +~5 modified |
| `src-tauri/src/dm_signing.rs` | Create: pure-function module with `sign_dm_packet(body_bytes: &[u8], signing_key: &SigningKey) -> [u8; 64]` and `verify_dm_packet_signature(body_bytes: &[u8], signature: &[u8; 64], signing_pub: &VerifyingKey, expected_signing_device_hash: DeviceIdentityHash) -> Result<(), DmReceiveError>`. Plus `derive_device_hash_from_pubkey(pub: &VerifyingKey) -> DeviceIdentityHash` matching whatever scheme `harmony-identity` already uses for device hashes (read it; do not invent). Pure functions, no state, no I/O. | +~120 (new file) |
| `src-tauri/src/dm_outbox.rs` | Modify: add `DmReceiveError` enum (Phase 3b-scoped; distinct from `dm_crypto::DmReceiveError`). Add `resolve_signed_origin_owner` helper. Add `lookup_pubkey_for_device` helper (reads OwnerDeviceCache). Add `handle_unicast` dispatcher that decodes + verifies signature + dispatches by discriminant. Add `handle_invite`, `handle_cidnotify`, `handle_ack`. Add `RuntimeUnicastTransport` adapter struct + `DmTransport` impl. Trim `StubTransport` documentation to note it's test-cfg surface (still needed by Phase 2 tests). | ~1129 → ~2000 |
| `src-tauri/src/event_loop.rs` | Modify: add an mpsc channel for outbound `RuntimeEvent::SendUnicastToDevice` requests (`unicast_send_rx` parameter on `event_loop::run`). Add a `RuntimeAction::UnicastReceived` interception block in each `for action in runtime.tick()` loop site, before `dispatch_action`. Pass through `cas_handle`, `dm_outbox`, `crdt_state`, `app` to the new handler. | +~120 |
| `src-tauri/src/lib.rs` | Modify: replace `StubTransport::new()` at line 843-844 with `RuntimeUnicastTransport::new(...)`. Construct `unicast_send_tx/rx` mpsc channel near `cas_op_tx` (line 572). Compute the local DM destination hash at start_node and call `runtime.register_local_destination(dm_dest)` once at startup. Wire NodeState fields for `unicast_send_tx`. Pull the device-Identity SigningKey from existing identity-management code (likely `harmony_identity` integration site near where `device_id` and `self_owner` come from) and inject into `RuntimeUnicastTransport`. | +~80 |
| `src-tauri/tests/dm_unicast_integration.rs` | Create: end-to-end integration test exercising `RuntimeUnicastTransport` outbound + inbound `UnicastReceived` re-entry via a fake runtime-channel pair, including signature verification on both sides. Mocks at the channel boundary, NOT the wire. | +~280 (new) |
| `src-tauri/tests/dm_send_integration.rs` | Modify: light update to confirm Phase 2 round-trip still works against the new `RuntimeUnicastTransport` (or remains valid against StubTransport which is preserved for tests). | +~10 |

Total harmony-client delta: ~800-1000 lines spread across 8 files, plus a new integration test.

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

### Task 1: harmony companion PR — public destination + identity-lookup APIs

**Repo:** `~/work/zeblithic/harmony`
**Branch:** `zeb-227-runtime-destination-api` (branched from `origin/main`)

**Files:**
- Modify: `crates/harmony-runtime/src/runtime.rs` — add the three public methods
- Test: `crates/harmony-runtime/src/runtime.rs` test module

This is a much smaller PR than the original Path A Task 1. No `link.rs` changes, no `Node` state changes — just three thin accessors.

**Pre-flight:** Discard the abandoned Path A branch first.

- [ ] **Step 1.1: Clean up old Path A branch + branch off origin/main**

```bash
cd ~/work/zeblithic/harmony
git checkout main
git branch -D zeb-227-runtime-link-identity-binding 2>/dev/null || true
git fetch origin
git checkout -b zeb-227-runtime-destination-api origin/main
git log --oneline -3   # confirm starts at e25a696 or later
```

Expected: branch tracks `origin/main`. The commit log should include the Phase 3a merge `b721148`.

- [ ] **Step 1.2: Write a failing test for register_local_destination**

Add to the test module at the end of `crates/harmony-runtime/src/runtime.rs`:

```rust
#[test]
fn node_runtime_register_local_destination_accepts_inbound_to_that_dest() {
    // harmony-client (ZEB-227) needs a public API to register the DM
    // destination. Without it the router is unreachable from outside the
    // crate. This test pins the API shape and round-trips through tick().
    let config = NodeConfig::default();
    let store = MemoryBookStore::default();
    let (mut runtime, _startup) = NodeRuntime::new(config, store);

    let dm_dest = [0xd1u8; 16];
    runtime.register_local_destination(dm_dest);

    // Build a Type1/Single/Data Reticulum packet addressed to dm_dest.
    // (Reuse the same packet-builder helper that
    // unicast_round_trip_a_to_b_surfaces_as_unicast_received uses —
    // search for it in the runtime test module; it was added in ZEB-226
    // Phase 3a, commit b721148.)
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

If `build_type1_data_packet_for_test` doesn't already exist as a runtime-test helper, search for it in `harmony-reticulum` test modules (per `unicast_round_trip_a_to_b_surfaces_as_unicast_received`'s setup) and pull it in via `pub(crate) use` or copy inline.

- [ ] **Step 1.3: Run tests to verify they fail**

```bash
cd ~/work/zeblithic/harmony
set -o pipefail
cargo test --manifest-path crates/harmony-runtime/Cargo.toml node_runtime_register_local_destination 2>&1 | tail -20
cargo test --manifest-path crates/harmony-runtime/Cargo.toml node_runtime_unregister_local_destination 2>&1 | tail -10
```

Expected: FAIL with `no method named 'register_local_destination' found` (and similarly for unregister).

- [ ] **Step 1.4: Implement register/unregister API**

In `crates/harmony-runtime/src/runtime.rs`, add to the `impl<B: BookStore> NodeRuntime<B>` block (near the existing `local_identity_hash` / `set_local_*_announce` methods around `runtime.rs:1623-1644`):

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

The methods on `Node` they delegate to are confirmed to exist at `crates/harmony-reticulum/src/node.rs:454` (`register_destination`) and `:461` (`unregister_destination` returning `bool`).

- [ ] **Step 1.5: Run tests to verify they pass**

```bash
cd ~/work/zeblithic/harmony
set -o pipefail
cargo test --manifest-path crates/harmony-runtime/Cargo.toml node_runtime_register_local_destination 2>&1 | tail -10
cargo test --manifest-path crates/harmony-runtime/Cargo.toml node_runtime_unregister_local_destination 2>&1 | tail -5
```

Expected: PASS for both.

- [ ] **Step 1.6: Write a failing test for lookup_destination_identity**

This is the API harmony-client uses to fetch a sender's public key for signature verification. Investigation needed: where does `Node` currently store identity material from announces? Search `crates/harmony-reticulum/src/node.rs` for `announce_table`, `path_table`, `ValidatedAnnounce`. Identity material (Ed25519 + X25519 public keys) propagates via announces and is accessible from one of these structures.

Add the test:

```rust
#[test]
fn node_runtime_lookup_destination_identity_returns_announced_identity() {
    // harmony-client (ZEB-227) verifies inbound DM packet signatures by
    // looking up the sender's Ed25519 verifying key. Public keys arrive
    // via announces and live in the runtime's announce/path table.
    let config = NodeConfig::default();
    let store = MemoryBookStore::default();
    let (mut runtime, _) = NodeRuntime::new(config, store);

    // Inject a known announce. (Use the existing test helper for
    // constructing + delivering an announce; mirror whatever
    // `runtime::announces_*` tests do — search for them.)
    let identity = NodeIdentity::generate_for_tests();
    let dest_hash = inject_test_announce(&mut runtime, &identity);

    let looked_up = runtime.lookup_destination_identity(&dest_hash);
    assert!(looked_up.is_some(), "after announce, lookup must return Some(Identity)");
    let id = looked_up.unwrap();
    assert_eq!(id.address_hash(), identity.address_hash());
}

#[test]
fn node_runtime_lookup_destination_identity_unknown_returns_none() {
    let config = NodeConfig::default();
    let store = MemoryBookStore::default();
    let (runtime, _) = NodeRuntime::new(config, store);

    let unknown = [0xff; 16];
    assert!(runtime.lookup_destination_identity(&unknown).is_none());
}
```

If the existing test infrastructure does not have `inject_test_announce` or similar, the implementer reuses whatever announce-injection pattern is used in `runtime::announces_*` or `runtime::path_table_*` tests. Do NOT invent new helpers; mirror existing ones.

- [ ] **Step 1.7: Run tests to verify they fail**

```bash
cd ~/work/zeblithic/harmony
set -o pipefail
cargo test --manifest-path crates/harmony-runtime/Cargo.toml node_runtime_lookup_destination_identity 2>&1 | tail -20
```

Expected: FAIL with `no method named 'lookup_destination_identity' found`.

- [ ] **Step 1.8: Implement lookup_destination_identity**

INVESTIGATE FIRST: read the announce-table / path-table structure in `crates/harmony-reticulum/src/node.rs` to find where `Identity` is stored. The `Identity` type itself is in `crates/harmony-reticulum/src/identity.rs` (or similar — search for `pub struct Identity`). Identity material persists at the path-table level for routed destinations; it's also kept on `announce_table` while the announce is "fresh."

Add to the same `impl<B: BookStore> NodeRuntime<B>` block:

```rust
/// Look up the announced Identity for a destination hash. Returns Some
/// when an announce has been received for this destination and the
/// identity material (Ed25519 verifying key + X25519 ECDH key) is
/// available locally, None otherwise.
///
/// Used by harmony-client (ZEB-216 Sub-B Phase 3b) to resolve
/// `DmCidNotify.signing_device_hash` to a public key for application-
/// signature verification. The identity hash and public keys are
/// related by `Identity::address_hash() == truncated_hash(verifying_key)`,
/// which is the same scheme harmony-identity already uses.
///
/// Note: for first-contact DmInvite, harmony-client carries the
/// inviter's signing pubkey inline (`inviter_signing_pub`) so this
/// lookup is unnecessary at bootstrap. Subsequent DmCidNotify / DmAck
/// from the inviter's already-cached devices use this lookup against
/// pubkeys harmony-client cached locally on DmInvite accept — NOT
/// against this announce-table identity (which may not have arrived
/// yet for new contacts). See spec §"Application-signature binding rule"
/// for the bootstrap detail.
pub fn lookup_destination_identity(&self, dest_hash: &[u8; 16]) -> Option<&Identity> {
    self.router.lookup_identity(dest_hash)
}
```

If `Node` doesn't currently expose a `lookup_identity` helper, add a thin one in `crates/harmony-reticulum/src/node.rs`:

```rust
/// Returns the announced Identity for `dest_hash` if known. Reads from
/// path_table (preferred — long-lived) falling back to announce_table
/// (transient).
pub fn lookup_identity(&self, dest_hash: &[u8; 16]) -> Option<&Identity> {
    self.path_table.get(dest_hash)
        .and_then(|entry| entry.identity.as_ref())
        .or_else(|| self.announce_table.get(dest_hash)
            .and_then(|entry| entry.identity.as_ref()))
}
```

If the path_table entries don't carry full Identity (they may carry only the truncated hash), the implementer adapts: either propagate Identity into the path_table entry (small `path_table.rs` change), or rely solely on announce_table (acceptable if announces are stored long enough for the use case). **Investigate path_table and announce_table struct definitions before deciding** — file:line in your implementer notes.

- [ ] **Step 1.9: Run tests to verify they pass**

```bash
cd ~/work/zeblithic/harmony
set -o pipefail
cargo test --manifest-path crates/harmony-runtime/Cargo.toml node_runtime_lookup_destination_identity 2>&1 | tail -10
```

Expected: PASS for both.

- [ ] **Step 1.10: Run full workspace verification**

```bash
cd ~/work/zeblithic/harmony
set -o pipefail
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all green. Investigate and fix any breakage caused by the new APIs. Per "test drift is our fault" — broken tests on main caused by your changes belong in this PR.

If pre-existing fmt drift or clippy warnings outside your diff are in the way, do NOT include them in this PR — leave them as their own follow-up. Phase 3a's PR #267 set the precedent of explicitly stating which pre-existing warnings are NOT in scope.

- [ ] **Step 1.11: Commit**

```bash
cd ~/work/zeblithic/harmony
git add crates/harmony-runtime/src/runtime.rs crates/harmony-reticulum/src/node.rs
# (only add files you actually changed; the node.rs add is conditional
#  on whether you needed to add the lookup_identity helper)
git commit -m "$(cat <<'EOF'
feat(zeb-227): expose public NodeRuntime destination + identity APIs

Three accessors that harmony-client (ZEB-216 Sub-B Phase 3b, ZEB-227)
needs to consume the existing private router state:

1. NodeRuntime::register_local_destination(dest_hash) — register a
   16-byte Reticulum destination hash for local delivery. Without this,
   harmony-client cannot tell the runtime to accept inbound packets at
   the DM destination hash.

2. NodeRuntime::unregister_local_destination(dest_hash) -> bool —
   matching cleanup API.

3. NodeRuntime::lookup_destination_identity(dest_hash) -> Option<&Identity> —
   read announced Identity (Ed25519 verifying key + X25519 ECDH key)
   for a known destination. harmony-client uses this to look up sender
   public keys for application-layer Ed25519 signature verification on
   inbound DM packet bodies (Path B in the ZEB-216 spec — see spec
   §"Application-signature binding rule" at commit ea38132).

All three delegate to existing private state on the inner Node. This
PR is intentionally minimal; no Link state, no handshake wiring, no
protocol changes. The earlier-attempted Path A (terminal-link state +
responder-side handshake) was confirmed during ZEB-227 investigation
to be a multi-PR feature in its own right and is filed as a separate
future ticket for when voice / file sync / streaming features need it.

Test drift policy applied: any pre-existing fmt drift / clippy warnings
in the surrounding tree are NOT addressed here; only the diff covered
by this PR's TDD steps.
EOF
)"
```

Pass it via heredoc to preserve formatting. Do NOT use `--no-verify` or skip hooks.

- [ ] **Step 1.12: Push and open PR**

```bash
cd ~/work/zeblithic/harmony
git push -u origin zeb-227-runtime-destination-api
gh pr create --title "feat(zeb-227): expose public NodeRuntime destination + identity APIs" --body "$(cat <<'EOF'
## Summary
Three public accessors on \`NodeRuntime\` delegating to existing private router state:
- \`register_local_destination(dest_hash)\` / \`unregister_local_destination(dest_hash)\`
- \`lookup_destination_identity(dest_hash) -> Option<&Identity>\`

## Why
Blocking deps for **harmony-client ZEB-227** (DM transport Phase 3b — Path B). Without (1)/(2), harmony-client cannot register the DM destination, so the runtime would drop every inbound DM packet as \`NoLocalDestination\`. Without (3), harmony-client cannot look up sender public keys to verify per-device Ed25519 application-signature binding on inbound DM packets (the spec's "Application-signature binding rule" — see harmony-client spec at commit ea38132).

The earlier-attempted Path A (terminal-link state + responder-side Reticulum handshake wiring) was confirmed during ZEB-227 investigation to be a multi-PR feature in its own right (terminal_links map, Link::respond wiring, handshake completion, link expiration, plus initiator-side link cache + runtime API redesign for "establish-then-send" semantics). It's filed as a separate future ticket for when voice / file sync / streaming features need it. Path B (this PR + harmony-client signature work) ships DM end-to-end without requiring any of that Reticulum link wiring.

Companion PR pattern mirrors Phase 3a (PR #267) — small, targeted runtime-side surface change that the client-side PR consumes.

## Test plan
- [ ] \`cargo test -p harmony-runtime node_runtime_register_local_destination\` — green
- [ ] \`cargo test -p harmony-runtime node_runtime_unregister_local_destination\` — green
- [ ] \`cargo test -p harmony-runtime node_runtime_lookup_destination_identity\` — green
- [ ] \`cargo clippy --workspace --all-targets -- -D warnings\` — clean
- [ ] \`cargo fmt --all -- --check\` — clean

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Capture the PR URL printed by `gh pr create`. **Do NOT proceed past this step.** Task 2 starts after a human merges this PR; that's not your job.

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
git log --oneline -2   # confirm current HEAD includes the spec-update + plan commits
grep -E "harmony-runtime|harmony-content" src-tauri/Cargo.toml
```

Expected current pin (per the existing Phase 2 work): `ddf2ce07109eb30526a10bd37af3b0ddc901faa8` for both deps.

- [ ] **Step 2.2: Resolve the Task 1 merge SHA**

```bash
cd ~/work/zeblithic/harmony
git fetch origin main
git log origin/main --oneline -3
```

The first commit listed should be the squash-merge of Task 1 (subject prefix `feat(zeb-227): expose public NodeRuntime destination + identity APIs`). Capture its 40-char SHA. Referred to as `<TASK_1_SHA>` below.

- [ ] **Step 2.3: Bump both deps to TASK_1_SHA**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
```

Edit `src-tauri/Cargo.toml`, replacing `ddf2ce07109eb30526a10bd37af3b0ddc901faa8` (both occurrences) with the Task 1 merge SHA. Use the Edit tool with `replace_all` on the substring.

- [ ] **Step 2.4: Confirm ed25519-dalek is reachable for signing**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo tree -e normal --depth 3 2>&1 | grep -E "ed25519|dalek" | head
```

If `ed25519-dalek` is already pulled in transitively by harmony deps (`harmony-identity` or `harmony-crypto` likely re-export the SigningKey type), no Cargo.toml addition needed — just `use harmony_identity::ed25519::SigningKey;` (or whatever the re-export path is). If NOT reachable, add as a direct dep with the version that matches the harmony workspace's pin. Do not pull a different version than the workspace uses — version skew on signing primitives is dangerous.

- [ ] **Step 2.5: Resolve dependencies and verify the build**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo update -p harmony-runtime -p harmony-content
set -o pipefail
cargo build --tests 2>&1 | tail -30
```

Expected: clean build. If clippy warnings appear after the dep bump, fix them — they belong in this PR per "test drift is our fault."

- [ ] **Step 2.6: Smoke check the new APIs**

Quick verification that the new symbols are reachable:

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo doc --no-deps --document-private-items 2>&1 | grep -E "register_local_destination|unregister_local_destination|lookup_destination_identity" | head -10
```

If symbols don't appear, the dep bump didn't pick up Task 1 — recheck `cargo update` and `Cargo.lock`.

- [ ] **Step 2.7: Run the full verification quartet**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
npx tsc --noEmit
```

Expected: all green. (vitest can be skipped on dep-only bumps.)

- [ ] **Step 2.8: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "$(cat <<'EOF'
chore(zeb-227): bump harmony deps to absorb Path-B accessors

harmony PR <task-1-pr-num> merged at <TASK_1_SHA>:
- pub NodeRuntime::register_local_destination / unregister_local_destination
- pub NodeRuntime::lookup_destination_identity

These unblock DM destination registration and per-device Ed25519
application-signature verification (Path B per spec ea38132). Path A
(Reticulum terminal-link wiring) is deferred to a separate future
ticket and does NOT block this PR.
EOF
)"
```

(Replace `<task-1-pr-num>` and `<TASK_1_SHA>` with the actual values captured in Steps 1.12 and 2.2.)

---

### Task 3: Add `dm_signing.rs` module — sign + verify primitives

**Files:**
- Create: `src-tauri/src/dm_signing.rs`
- Modify: `src-tauri/src/lib.rs` — add `pub mod dm_signing;`
- Modify: `src-tauri/src/dm_outbox.rs` — add `DmReceiveError` enum (referenced by `verify_dm_packet_signature` return)

Pure-function module. Easiest piece to TDD.

- [ ] **Step 3.1: Add DmReceiveError enum to dm_outbox.rs**

In `src-tauri/src/dm_outbox.rs`, add near the existing `SendDmError`:

```rust
/// Inbound-DM packet handling errors. Each variant maps to a "drop +
/// telemetry" decision in handle_unicast per ZEB-216 §"Application-
/// signature binding rule". Distinct from dm_crypto::DmReceiveError
/// which only carries the SenderImpersonation case for the encrypted-
/// payload-layer check.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DmReceiveError {
    #[error("signing_device_hash not present in any OwnerDeviceCache entry")]
    UnknownSigningDevice,
    #[error("signing_device_hash claimed by multiple OwnerDeviceCache entries (corrupted state or cache-poisoning attempt)")]
    AmbiguousSigningDevice,
    #[error("no public key cached for signing_device_hash (pre-bootstrap)")]
    UnknownSigningKey,
    #[error("signature does not verify against the provided public key")]
    SignatureVerificationFailed,
    #[error("public key does not match claimed signing_device_hash (key-substitution attempt)")]
    SigningKeyDoesNotMatchDeviceHash,
    #[error("payload owner field does not match signed-origin-resolved owner")]
    OwnerFieldMismatch,
    #[error("DmInvite.inviter must be in DmInvite.members")]
    InviterNotInMembers,
    #[error("signing_device_hash must be in DmInvite.sender_devices")]
    SigningDeviceNotInSenderDevices,
    #[error("self_owner_addr must be in DmInvite.members")]
    ReceiverNotInMembers,
    #[error("ack from owner not in OutboxEntry.recipient_owners")]
    AckFromNonRecipient,
    #[error("OutboxEntry not found for (space_id, message_cid)")]
    OutboxEntryNotFound,
    #[error("Space not found for incoming DmCidNotify (we are not a member?)")]
    SpaceNotFound,
    #[error("CAS fetch failed or timed out: {0}")]
    CasFetchFailed(String),
    #[error("DM blob decryption failed under all candidate keys")]
    DecryptFailed,
    #[error("payload sender does not match resolved owner (impersonation)")]
    SenderImpersonation,
    #[error("packet decode failed: {0}")]
    Decode(String),
    #[error("AAD compute failed: {0}")]
    AadCompute(String),
    #[error("CRDT rejected the apply (invariant violation): {0}")]
    CrdtRejected(String),
}
```

- [ ] **Step 3.2: Investigate the device-hash-from-pubkey scheme**

Read `harmony_identity` (or whatever harmony crate owns `Identity::address_hash`) to find how device identity hashes are derived from Ed25519 verifying keys. Likely scheme: `address_hash = SHA256(verifying_key_bytes)[:16]` or similar. Use file:line investigation; do NOT invent. Phase 1's `DeviceIdentityHash` is `[u8; 16]` so the truncation is to 16 bytes.

Document the discovered scheme inline at the top of `dm_signing.rs` so future readers know the source of truth.

- [ ] **Step 3.3: Create dm_signing.rs with failing tests**

```rust
//! ZEB-216 Sub-B Phase 3b: per-device Ed25519 signing primitives for
//! Reticulum DM packet bodies (Path B per spec ea38132).
//!
//! Pure functions over (body_bytes, key, signature). No state, no I/O.
//!
//! Device-hash-from-pubkey scheme: SHA256(verifying_key_bytes)[:16].
//! This MUST match harmony-identity's `Identity::address_hash` derivation
//! for a verifying key — verified during ZEB-227 implementation (see
//! harmony commit <SHA from Step 3.2 investigation>).

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::dm_outbox::DmReceiveError;
use crate::owner_state_types::DeviceIdentityHash;

/// Compute the DeviceIdentityHash for a given Ed25519 verifying key.
/// MUST match the scheme harmony-identity uses for Identity::address_hash.
pub fn derive_device_hash_from_pubkey(pub_key: &VerifyingKey) -> DeviceIdentityHash {
    let bytes = pub_key.to_bytes();
    let hash = Sha256::digest(bytes);
    let mut truncated = [0u8; 16];
    truncated.copy_from_slice(&hash[..16]);
    DeviceIdentityHash(truncated)
}

/// Sign a Reticulum DM packet body. The signature is applied to the
/// canonical CBOR encoding of the body (which includes
/// `signing_device_hash` to prevent key-substitution attacks).
///
/// Caller computes `body_bytes` once, passes here for signing. The
/// resulting 64-byte Ed25519 signature is appended after `body_bytes`
/// in the wire packet by encode_packet.
pub fn sign_dm_packet(body_bytes: &[u8], signing_key: &SigningKey) -> [u8; 64] {
    let sig: Signature = signing_key.sign(body_bytes);
    sig.to_bytes()
}

/// Verify a Reticulum DM packet signature.
///
/// `body_bytes`: canonical CBOR encoding of the signed body (NOT
/// including the discriminant byte or the appended signature).
/// `signature`: 64-byte Ed25519 signature appended after body_bytes.
/// `signing_pub`: verifying key looked up by the caller (from
/// OwnerDeviceCache for CidNotify/Ack post-bootstrap, or from the
/// inline `inviter_signing_pub` for DmInvite).
/// `expected_signing_device_hash`: the body's `signing_device_hash`
/// field; this function verifies the public key actually corresponds
/// to that hash (defeats key-substitution attacks where an attacker
/// presents pubkey K but claims signing_device_hash from a different key).
///
/// Returns Ok on success; Err on signature mismatch OR pubkey-doesn't-
/// match-claimed-device-hash.
pub fn verify_dm_packet_signature(
    body_bytes: &[u8],
    signature: &[u8; 64],
    signing_pub: &VerifyingKey,
    expected_signing_device_hash: DeviceIdentityHash,
) -> Result<(), DmReceiveError> {
    let computed_hash = derive_device_hash_from_pubkey(signing_pub);
    if computed_hash != expected_signing_device_hash {
        return Err(DmReceiveError::SigningKeyDoesNotMatchDeviceHash);
    }
    let sig = Signature::from_bytes(signature);
    signing_pub
        .verify(body_bytes, &sig)
        .map_err(|_| DmReceiveError::SignatureVerificationFailed)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn fixed_key() -> SigningKey {
        // Deterministic key for tests. NOT for production.
        SigningKey::from_bytes(&[0x42u8; 32])
    }

    #[test]
    fn sign_then_verify_round_trip() {
        let sk = fixed_key();
        let pk = sk.verifying_key();
        let device_hash = derive_device_hash_from_pubkey(&pk);
        let body = b"hello world body bytes";
        let sig = sign_dm_packet(body, &sk);
        assert!(verify_dm_packet_signature(body, &sig, &pk, device_hash).is_ok());
    }

    #[test]
    fn verify_tampered_body_rejects() {
        let sk = fixed_key();
        let pk = sk.verifying_key();
        let device_hash = derive_device_hash_from_pubkey(&pk);
        let body = b"hello world body bytes";
        let sig = sign_dm_packet(body, &sk);
        let mut tampered = body.to_vec();
        tampered[0] ^= 0xff;
        let err = verify_dm_packet_signature(&tampered, &sig, &pk, device_hash).unwrap_err();
        assert!(matches!(err, DmReceiveError::SignatureVerificationFailed));
    }

    #[test]
    fn verify_wrong_pubkey_rejects() {
        let sk1 = fixed_key();
        let sk2 = SigningKey::from_bytes(&[0x99u8; 32]);
        let pk2 = sk2.verifying_key();
        let device_hash_2 = derive_device_hash_from_pubkey(&pk2);
        let body = b"hello world body bytes";
        let sig = sign_dm_packet(body, &sk1);  // signed by sk1
        // Verify with pk2 + claim sk2's device hash → first check passes
        // (pk2 matches device_hash_2), then signature verification fails.
        let err = verify_dm_packet_signature(body, &sig, &pk2, device_hash_2).unwrap_err();
        assert!(matches!(err, DmReceiveError::SignatureVerificationFailed));
    }

    #[test]
    fn verify_pubkey_does_not_match_device_hash_rejects() {
        let sk1 = fixed_key();
        let pk1 = sk1.verifying_key();
        let sk2 = SigningKey::from_bytes(&[0x99u8; 32]);
        let pk2 = sk2.verifying_key();
        let device_hash_2 = derive_device_hash_from_pubkey(&pk2);
        let body = b"hello world body bytes";
        let sig = sign_dm_packet(body, &sk1);
        // Present pk1 but claim device_hash_2 (which is for pk2).
        // Key-substitution attack defense: this MUST reject before
        // even attempting signature verification.
        let err = verify_dm_packet_signature(body, &sig, &pk1, device_hash_2).unwrap_err();
        assert!(matches!(err, DmReceiveError::SigningKeyDoesNotMatchDeviceHash));
    }

    #[test]
    fn derive_device_hash_is_deterministic() {
        let sk = fixed_key();
        let pk = sk.verifying_key();
        let h1 = derive_device_hash_from_pubkey(&pk);
        let h2 = derive_device_hash_from_pubkey(&pk);
        assert_eq!(h1, h2);
    }

    #[test]
    fn derive_device_hash_differs_per_key() {
        let pk1 = SigningKey::from_bytes(&[0x11u8; 32]).verifying_key();
        let pk2 = SigningKey::from_bytes(&[0x22u8; 32]).verifying_key();
        assert_ne!(
            derive_device_hash_from_pubkey(&pk1),
            derive_device_hash_from_pubkey(&pk2)
        );
    }
}
```

Add to `src-tauri/src/lib.rs`:

```rust
pub mod dm_signing;
```

(Place near the existing `pub mod dm_outbox;`.)

- [ ] **Step 3.4: Run tests to verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test dm_signing 2>&1 | tail -20
```

Expected: PASS for all six tests.

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
git add src-tauri/src/dm_signing.rs src-tauri/src/lib.rs src-tauri/src/dm_outbox.rs
git commit -m "$(cat <<'EOF'
feat(zeb-227): dm_signing module — Ed25519 sign + verify primitives

Pure-function module for Path B's per-device application-signature
binding. Three exports:

- sign_dm_packet(body_bytes, signing_key) -> [u8; 64]
- verify_dm_packet_signature(body_bytes, signature, signing_pub,
    expected_signing_device_hash) -> Result<(), DmReceiveError>
- derive_device_hash_from_pubkey(pub_key) -> DeviceIdentityHash

The verify step does TWO checks:
1. The provided pubkey actually hashes to the claimed signing_device_hash
   (defeats key-substitution attacks where an attacker presents pubkey K
   but claims a different device's hash).
2. The Ed25519 signature verifies against (pubkey, body_bytes).

Device-hash-from-pubkey scheme is SHA256(verifying_key_bytes)[:16],
matching harmony-identity's Identity::address_hash derivation for a
verifying key. Documented inline in the module header so the source
of truth is loud at the call site.

Six unit tests pin happy path + tampered body + wrong pubkey +
key-substitution + determinism + per-key uniqueness.

Also adds Phase 3b's DmReceiveError enum to dm_outbox.rs (16 variants
spanning packet decode, signature verification, link-origin resolution,
CAS fetch, decrypt, sender-binding, CRDT rejection). Distinct from
dm_crypto::DmReceiveError which only covers the encrypted-payload
sender-impersonation case.
EOF
)"
```

---

### Task 4: Extend `OwnerDeviceEntry` with per-device verifying keys

**Files:**
- Modify: `src-tauri/src/owner_state_types.rs` — extend `OwnerDeviceEntry` to carry `Vec<[u8; 32]>` of verifying keys
- Test: round-trip + invariant tests

The cache stores per-OwnerAddr lists of `(DeviceIdentityHash, signing_pub)` pairs. The implementer chooses representation:
- **Option A:** `Vec<[u8; 32]>` parallel to `devices: Vec<DeviceIdentityHash>` (same indexing). Preserves the existing binary_search invariant on `devices`.
- **Option B:** Replace `devices` with `Vec<(DeviceIdentityHash, [u8; 32])>`. Cleaner but breaks binary_search-on-DeviceIdentityHash; would need to switch to manual partition_point.

Option A is lower friction. Implementer picks A unless they discover a reason during reading.

- [ ] **Step 4.1: Write failing tests for the new field**

In `src-tauri/src/owner_state_types.rs` test module:

```rust
#[test]
fn owner_device_entry_serialize_includes_signing_pubs() {
    let entry = OwnerDeviceEntry {
        devices: vec![DeviceIdentityHash([0xa1; 16])],
        device_signing_pubs: vec![[0x42u8; 32]],
        learned_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
    };
    let bytes = canonical_cbor_encode(&entry).unwrap();
    let recovered: OwnerDeviceEntry = canonical_cbor_decode(&bytes).unwrap();
    assert_eq!(entry, recovered);
}

#[test]
fn owner_device_entry_loads_pre_phase3b_snapshot_with_default_pubs() {
    // Phase 1/2 snapshots stored only `devices` and `learned_at`.
    // Phase 3b adds `device_signing_pubs` with #[serde(default)] so old
    // snapshots load with an empty pubkey vec — the cache then drops
    // signature-verification packets as UnknownSigningKey until the next
    // DmInvite repopulates the pubkeys.
    let pre_phase3b_cbor = b"\xa2\x61v\x81\x50\xa1\xa1\xa1\xa1\xa1\xa1\xa1\xa1\xa1\xa1\xa1\xa1\xa1\xa1\xa1\xa1\x61l\xa3\x61w\x01\x61l\x00\x61d\x61d";
    // ^ map(2) { "v": [bstr(16) [0xa1; 16]], "l": { "w": 1, "l": 0, "d": "d" } }
    // (No "device_signing_pubs" key.)
    // The decoder must accept this and produce an entry with
    // `device_signing_pubs: vec![]` via #[serde(default)].
    // The implementer verifies the byte-exact CBOR with a small one-off
    // canonical_cbor_encode + grep before pinning the bytes here.
    let recovered: OwnerDeviceEntry = canonical_cbor_decode(pre_phase3b_cbor).unwrap();
    assert_eq!(recovered.devices.len(), 1);
    assert!(recovered.device_signing_pubs.is_empty());
}
```

(The byte literal in the second test is illustrative; the implementer derives the actual canonical CBOR bytes for a Phase 1/2 entry by encoding one without the new field, capturing the bytes, and pinning them. This locks in the graceful-upgrade contract.)

- [ ] **Step 4.2: Run tests to verify they fail**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test owner_device_entry 2>&1 | tail -20
```

Expected: FAIL — `device_signing_pubs` field doesn't exist.

- [ ] **Step 4.3: Add the field**

In `src-tauri/src/owner_state_types.rs`, modify `OwnerDeviceEntry`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerDeviceEntry {
    /// Sorted ascending lex, deduped, capped at MAX_DEVICES_PER_OWNER.
    /// Sorted invariant means binary_search works for lookup
    /// (used by resolve_signed_origin_owner in Phase 3b).
    /// ... (existing doc) ...
    #[serde(rename = "v", deserialize_with = "deserialize_device_identities")]
    pub devices: Vec<DeviceIdentityHash>,
    /// Per-device Ed25519 verifying keys, parallel to `devices` (same
    /// length; element i is the pubkey for devices[i]). Phase 3b
    /// (Path B per ZEB-216 spec) uses these to verify per-device
    /// application-layer signatures on inbound DM packets. Pre-Phase-3b
    /// snapshots load with `vec![]` via #[serde(default)] — the receive
    /// path then drops signature-verification packets as
    /// UnknownSigningKey until the next DmInvite repopulates these.
    #[serde(rename = "p", default, deserialize_with = "deserialize_device_signing_pubs")]
    pub device_signing_pubs: Vec<[u8; 32]>,
    /// HLC of when this entry was learned. LWW key for merge.
    #[serde(rename = "l")]
    pub learned_at: Hlc,
}
```

Add `deserialize_device_signing_pubs` helper that mirrors `deserialize_device_identities`'s cap behavior (truncate to `MAX_DEVICES_PER_OWNER`, but on the pubkey list — no sort/dedup since order is meaningful for parallel-vec correspondence).

The new field uses `serde(default)` so old snapshots without the field deserialize to `vec![]`. Phase 3b's signature verification then fails with `UnknownSigningKey` for any device that doesn't have a corresponding pubkey — which is correct behavior: the receiver hasn't been told the pubkey yet, so it can't verify, so it drops.

Update `apply_owner_device_update` (in `owner_state_crdt.rs:453`) to also accept and store `device_signing_pubs` parallel to `devices`. Signature:

```rust
pub fn apply_owner_device_update(
    &mut self,
    addr: OwnerAddr,
    devices: Vec<DeviceIdentityHash>,
    device_signing_pubs: Vec<[u8; 32]>,  // NEW — parallel to devices
    learned_at: Hlc,
) -> ApplyOutcome {
    // sanitize: sort+dedup devices BUT also reorder pubs to match
    // ... [implementer carefully maintains parallelism through sort+dedup] ...
}
```

The sanitization is delicate: `devices.sort()` invalidates the parallel-vec correspondence. Two ways to handle:
- **Option A:** Build `Vec<(DeviceIdentityHash, [u8; 32])>`, sort by .0, dedup by .0, then split back into two vecs. Cleanest.
- **Option B:** Sort with a permutation, apply the same permutation to pubs. More efficient for very large vecs but harder to read.

Use Option A. The vecs are at most 32 elements (MAX_DEVICES_PER_OWNER); efficiency doesn't matter.

Implementer also updates all callers of `apply_owner_device_update` to pass the new arg. Phase 1/2 callers don't have pubkeys; pass `vec![]` for now. Phase 3b adds real pubkeys at the call sites in `handle_invite` (Task 7) and `handle_cidnotify` (Task 8) — not yet wired in this task.

- [ ] **Step 4.4: Run tests to verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test owner_device_entry 2>&1 | tail -10
cargo test apply_owner_device_update 2>&1 | tail -20
```

Expected: PASS.

- [ ] **Step 4.5: Verification gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

Expected: all green. Existing Phase 1/2 tests that call `apply_owner_device_update` need their call sites updated with `vec![]` for the new pubkey arg.

- [ ] **Step 4.6: Commit**

```bash
git add src-tauri/src/owner_state_types.rs src-tauri/src/owner_state_crdt.rs
# (plus any test files updated for the apply_owner_device_update signature change)
git commit -m "$(cat <<'EOF'
feat(zeb-227): extend OwnerDeviceEntry with per-device Ed25519 pubkeys

OwnerDeviceEntry gains a `device_signing_pubs: Vec<[u8; 32]>` field
parallel to the existing `devices: Vec<DeviceIdentityHash>` — element i
is the Ed25519 verifying key for devices[i]. Phase 3b's signature
verification (per ZEB-216 spec §"Application-signature binding rule"
at commit ea38132, Path B) looks up the pubkey via this parallel-index
lookup.

Wire format addition: CBOR map gains key "p" (matches the existing
two-char rename pattern). The new field uses #[serde(default)] so
Phase 1/2 snapshots without the key load with `vec![]` — graceful
upgrade. The receive path then drops any signature-verification packet
from a device whose pubkey isn't cached as UnknownSigningKey, until
the next DmInvite repopulates it.

apply_owner_device_update widens its signature with the new pubkey
vec. Sanitization carefully maintains parallel-vec correspondence
through sort + dedup by zipping into Vec<(hash, pub)>, sorting by
hash, dedup'ing by hash, then splitting back. Phase 1/2 call sites
pass vec![] (no pubkey known yet); Phase 3b's handle_invite +
handle_cidnotify pass real pubkeys.
EOF
)"
```

---

### Task 5: Reshape `dm_envelope.rs` for the appended-signature wire layout

**Files:**
- Modify: `src-tauri/src/dm_envelope.rs` — rename existing structs to `*Signed`, add `signing_device_hash` + `inviter_signing_pub`, change `DmPacket` variants to carry signature + signed_bytes, update encode_packet + decode_packet

This is the largest single mechanical change in the plan. TDD applies but the structural refactor is contiguous.

- [ ] **Step 5.1: Write failing tests for the new wire layout**

In `src-tauri/src/dm_envelope.rs` test module:

```rust
#[test]
fn dm_packet_invite_round_trip_with_signature() {
    let sk = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]);
    let pk = sk.verifying_key();
    let device_hash = crate::dm_signing::derive_device_hash_from_pubkey(&pk);

    let signed = DmInviteSigned {
        space_id: SpaceId([1; 16]),
        kind: SpaceKind::Dm,
        members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
        inviter: OwnerAddr([1; 16]),
        content_key: DmContentKey::new([0xaa; 32]),
        sender_devices: vec![device_hash],
        created_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        signing_device_hash: device_hash,
        inviter_signing_pub: pk.to_bytes(),
    };

    let body_bytes = canonical_cbor_encode(&signed).unwrap();
    let signature = crate::dm_signing::sign_dm_packet(&body_bytes, &sk);

    let packet = DmPacket::Invite { signed: signed.clone(), signature, signed_bytes: body_bytes.clone() };
    let wire = encode_packet(&packet).unwrap();
    assert_eq!(wire[0], 0x01);
    assert_eq!(wire.len(), 1 + body_bytes.len() + 64);

    let decoded = decode_packet(&wire).unwrap();
    match decoded {
        DmPacket::Invite { signed: d_signed, signature: d_sig, signed_bytes: d_bytes } => {
            assert_eq!(d_signed, signed);
            assert_eq!(d_sig, signature);
            assert_eq!(d_bytes, body_bytes);
            // Verify signature round-trips through decode.
            assert!(crate::dm_signing::verify_dm_packet_signature(
                &d_bytes,
                &d_sig,
                &pk,
                device_hash,
            ).is_ok());
        }
        other => panic!("expected Invite, got {:?}", other),
    }
}

#[test]
fn dm_packet_decode_too_short_for_signature_rejects() {
    // Body would need to be at least 1 byte (CBOR map header) + 64 byte
    // signature = 65 bytes total minimum after the discriminant.
    let bytes = vec![0x02, 0xa0]; // disc=0x02, body = empty CBOR map (1 byte), no signature
    let err = decode_packet(&bytes).unwrap_err();
    assert!(matches!(err, DecodeError::TooShortForSignature));
}

#[test]
fn dm_packet_signature_does_not_cover_discriminant() {
    // Same body bytes, swap the discriminant byte → signature should
    // still verify (discriminant is routing-only, not signed). This
    // pins the wire-format contract.
    let sk = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]);
    let body = b"\xa1\x61x\x00".to_vec(); // map(1) {"x": 0}
    let sig = crate::dm_signing::sign_dm_packet(&body, &sk);

    let mut wire_a = vec![0x01];
    wire_a.extend_from_slice(&body);
    wire_a.extend_from_slice(&sig);

    let mut wire_b = vec![0x02];  // different discriminant
    wire_b.extend_from_slice(&body);
    wire_b.extend_from_slice(&sig);

    // Both should parse the same body bytes (and thus the signature
    // should verify against both), differing only in their dispatch.
    // Note: actual decode would fail here because the body isn't valid
    // for the discriminant's schema — but the bytes-extraction is what
    // we're pinning. The implementer adapts the test to assert on the
    // body-extraction step specifically.
}
```

(The third test is illustrative; if the encode_packet API doesn't expose a "extract body bytes" function, the implementer reformulates it as a documentation-only assertion of the design decision.)

Plus the existing tests in dm_envelope.rs (round-trip for each variant) need updating to match the new struct names and field additions. Implementer batches these updates.

- [ ] **Step 5.2: Run tests to verify they fail**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test --lib dm_packet 2>&1 | tail -30
```

Expected: many existing tests fail (they reference the old struct names + decode_packet signature) and the new tests fail.

- [ ] **Step 5.3: Refactor dm_envelope.rs**

The plan won't reproduce the entire ~600-line file. Key edits:

1. **Rename structs:** `DmInvite` → `DmInviteSigned`, etc. Add `signing_device_hash: DeviceIdentityHash` to all three (with `#[serde(rename = "dh")]`). Add `inviter_signing_pub: [u8; 32]` to `DmInviteSigned` (rename `"sp"`).

2. **Restructure DmPacket enum:**

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DmPacket {
    Invite {
        signed: DmInviteSigned,
        signature: [u8; 64],
        /// The canonical CBOR bytes of `signed` — the bytes the
        /// signature was computed over. Captured on decode so the
        /// receive handler can verify without re-encoding (re-encoding
        /// would require canonical determinism guarantees that we have,
        /// but capturing is cheaper than recomputing + risk-free).
        signed_bytes: Vec<u8>,
    },
    CidNotify {
        signed: DmCidNotifySigned,
        signature: [u8; 64],
        signed_bytes: Vec<u8>,
    },
    Ack {
        signed: DmAckSigned,
        signature: [u8; 64],
        signed_bytes: Vec<u8>,
    },
}
```

3. **Update encode_packet:** caller passes the variant with `signed`, `signature`, and `signed_bytes` already populated. Wire output is `[disc][signed_bytes][signature]` (signed_bytes is already CBOR; we don't re-encode):

```rust
pub fn encode_packet(packet: &DmPacket) -> Result<Vec<u8>, EncodeError> {
    let (disc, signed_bytes, signature) = match packet {
        DmPacket::Invite { signed_bytes, signature, .. } => (0x01, signed_bytes, signature),
        DmPacket::CidNotify { signed_bytes, signature, .. } => (0x02, signed_bytes, signature),
        DmPacket::Ack { signed_bytes, signature, .. } => (0x03, signed_bytes, signature),
    };
    let mut out = Vec::with_capacity(1 + signed_bytes.len() + 64);
    out.push(disc);
    out.extend_from_slice(signed_bytes);
    out.extend_from_slice(signature);
    Ok(out)
}
```

4. **Add helper for the sender-side common path (sign + build):**

```rust
/// Builder helper: take an unsigned struct, sign it, return a complete
/// `DmPacket` ready for encode_packet. Hides the canonical-CBOR-encode
/// + sign + bundle dance from senders.
pub fn build_signed_invite(
    signed: DmInviteSigned,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<DmPacket, EncodeError> {
    let signed_bytes = canonical_cbor_encode(&signed)
        .map_err(|e| EncodeError::Cbor(e.to_string()))?;
    let signature = crate::dm_signing::sign_dm_packet(&signed_bytes, signing_key);
    Ok(DmPacket::Invite { signed, signature, signed_bytes })
}
// Plus build_signed_cidnotify, build_signed_ack — same shape.
```

5. **Update decode_packet:**

```rust
pub fn decode_packet(bytes: &[u8]) -> Result<DmPacket, DecodeError> {
    let (disc, rest) = bytes.split_first().ok_or(DecodeError::Empty)?;
    if rest.len() < 64 + 1 {
        // Need at least 1 byte of body + 64 bytes of signature.
        return Err(DecodeError::TooShortForSignature);
    }
    let split_at = rest.len() - 64;
    let (body_bytes, signature_bytes) = rest.split_at(split_at);
    let signature: [u8; 64] = signature_bytes.try_into().expect("just split at len-64");
    let signed_bytes = body_bytes.to_vec();
    match disc {
        0x01 => {
            let signed: DmInviteSigned = canonical_cbor_decode(body_bytes)?;
            // Phase 1 wire-decoder invariant checks remain (sorted members,
            // member counts, inviter ∈ members, sender_devices.len() cap).
            // Plus new check: signing_device_hash MUST be in sender_devices.
            if !signed.sender_devices.contains(&signed.signing_device_hash) {
                return Err(DecodeError::Invalid(
                    "DmInvite.signing_device_hash must be in sender_devices",
                ));
            }
            // Plus the existing checks (kind ∈ {Dm, GroupDm}, member-count match,
            // sorted, inviter ∈ members, oversized sender_devices) — keep them.
            Ok(DmPacket::Invite { signed, signature, signed_bytes })
        }
        0x02 => {
            let signed: DmCidNotifySigned = canonical_cbor_decode(body_bytes)?;
            if !signed.sender_devices.contains(&signed.signing_device_hash) {
                return Err(DecodeError::Invalid(
                    "DmCidNotify.signing_device_hash must be in sender_devices",
                ));
            }
            Ok(DmPacket::CidNotify { signed, signature, signed_bytes })
        }
        0x03 => {
            let signed: DmAckSigned = canonical_cbor_decode(body_bytes)?;
            if !signed.ack_from_devices.contains(&signed.signing_device_hash) {
                return Err(DecodeError::Invalid(
                    "DmAck.signing_device_hash must be in ack_from_devices",
                ));
            }
            Ok(DmPacket::Ack { signed, signature, signed_bytes })
        }
        other => Err(DecodeError::UnknownDiscriminant(*other)),
    }
}
```

6. **Add `DecodeError::TooShortForSignature` variant** to the error enum.

The implementer updates ALL existing tests in `dm_envelope.rs` test module to:
- Use the `*Signed` struct names
- Populate `signing_device_hash` (and `inviter_signing_pub` for invites)
- Build packets via the `build_signed_*` helpers (which compute the signature)
- Match on the new variant shape `{ signed, signature, signed_bytes }`

- [ ] **Step 5.4: Run tests to verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test --lib dm_packet 2>&1 | tail -30
cargo test --lib dm_envelope 2>&1 | tail -30
```

Expected: PASS for the new tests + the updated existing tests.

- [ ] **Step 5.5: Verification gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

Expected: all green. Other modules that import `DmInvite` / `DmCidNotify` / `DmAck` (just `dm_outbox.rs` per Phase 2) need their imports updated to `DmInviteSigned` / etc.

- [ ] **Step 5.6: Commit**

```bash
git add src-tauri/src/dm_envelope.rs
# (plus any cross-module import updates in dm_outbox.rs if needed)
git commit -m "$(cat <<'EOF'
feat(zeb-227): reshape DM wire format for appended Ed25519 signature

Wire layout per spec ea38132 §"Wire format":
[u8 disc][CBOR(signed_body)][bstr(64) signature]

The signature lives outside the CBOR map (no chicken-and-egg with
computing it inside) and outside the discriminant (which is routing-
only — same body could in principle be reused under a different
discriminant; the signature pins the body, not the routing tag).

Struct renames + field additions:
- DmInvite → DmInviteSigned. Adds `signing_device_hash: DeviceIdentityHash`
  (inside signed body — prevents key-substitution attacks where an
  attacker swaps which device claims authorship) and `inviter_signing_pub:
  [u8; 32]` (the inviter's Ed25519 verifying key, inline so bootstrap
  signature verification is self-contained).
- DmCidNotify → DmCidNotifySigned. Adds signing_device_hash.
- DmAck → DmAckSigned. Adds signing_device_hash.

DmPacket variants restructured to `{ signed: ..., signature: [u8; 64],
signed_bytes: Vec<u8> }`. signed_bytes is captured on decode so the
receive handler can call dm_signing::verify_dm_packet_signature without
re-encoding (re-encoding would work given canonical CBOR determinism,
but capturing is cheaper and risk-free).

encode_packet now expects `signed_bytes` + `signature` to be already
populated (caller goes through build_signed_invite / build_signed_cidnotify /
build_signed_ack helpers that wrap canonical_cbor_encode + sign).

decode_packet adds:
- DecodeError::TooShortForSignature when the post-discriminant slice
  is < 65 bytes (body min 1 + sig 64).
- New invariant check per packet type: signing_device_hash MUST be
  present in sender_devices (Invite/CidNotify) or ack_from_devices
  (Ack). Catches structurally-inconsistent packets before signature
  verification is even attempted.

Wire-size cost: +80 bytes per packet (signing_device_hash 16 + appended
signature 64) and +32 bytes for DmInvite (inviter_signing_pub).
Reticulum MTU is ~500 bytes effective; new packet sizes ~140-280 bytes.

dm_outbox imports updated to use the new struct names; rest of dm_outbox
is unchanged in this commit (signature verification + handle_unicast
land in subsequent tasks).
EOF
)"
```

---

### Task 6: Add `RuntimeUnicastTransport` adapter struct + `DmTransport` impl

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` — add the new adapter type, `DestinationResolver` trait, and tests

The adapter signs outbound `DmCidNotify` packets and pushes them via mpsc. `StubTransport` is preserved for tests.

- [ ] **Step 6.1: Write failing test for RuntimeUnicastTransport::send**

```rust
#[tokio::test]
async fn runtime_unicast_transport_send_pushes_signed_event_into_channel() {
    use tokio::sync::mpsc;
    let (tx, mut rx) = mpsc::channel::<UnicastSendRequest>(8);

    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]);
    let signing_pub = signing_key.verifying_key();
    let our_device = crate::dm_signing::derive_device_hash_from_pubkey(&signing_pub);

    let resolver = std::sync::Arc::new(StaticDestResolver::new([
        (OwnerAddr([1; 16]), vec![[0xd1u8; 16]]),
    ]));

    let transport = RuntimeUnicastTransport::new(
        tx,
        resolver,
        OwnerAddr([0xff; 16]),  // self_owner
        our_device,
        std::sync::Arc::new(signing_key),
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

    transport.send(&entry, OwnerAddr([1; 16])).await.unwrap();

    let req = rx.recv().await.expect("channel produced no event");
    assert_eq!(req.destination_hash, [0xd1u8; 16]);

    // The packet body decodes as a signed DmCidNotify; the signature
    // verifies against our signing pubkey.
    let packet = crate::dm_envelope::decode_packet(&req.packet).unwrap();
    match packet {
        crate::dm_envelope::DmPacket::CidNotify { signed, signature, signed_bytes } => {
            assert_eq!(signed.space_id, SpaceId([0xcc; 16]));
            assert_eq!(signed.message_cid, ContentId::from_bytes([0xee; 32]));
            assert_eq!(signed.signing_device_hash, our_device);
            // Signature must verify against our pubkey + claimed device hash.
            assert!(crate::dm_signing::verify_dm_packet_signature(
                &signed_bytes,
                &signature,
                &signing_pub,
                our_device,
            ).is_ok());
        }
        other => panic!("expected CidNotify, got {:?}", other),
    }
}

struct StaticDestResolver { /* ... as in plan section above ... */ }
// trait DestinationResolver { ... }
```

(The `StaticDestResolver` test helper is the same as in the original plan — fixed-table impl of `DestinationResolver`.)

- [ ] **Step 6.2: Run test to verify it fails**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test runtime_unicast_transport_send_pushes_signed_event 2>&1 | tail -20
```

Expected: FAIL — `RuntimeUnicastTransport`, `UnicastSendRequest`, `DestinationResolver` don't exist yet.

- [ ] **Step 6.3: Implement RuntimeUnicastTransport**

In `src-tauri/src/dm_outbox.rs`, after the existing `StubTransport`:

```rust
#[derive(Debug, Clone)]
pub struct UnicastSendRequest {
    pub destination_hash: [u8; 16],
    pub packet: Vec<u8>,
}

pub trait DestinationResolver: Send + Sync {
    fn resolve(&self, recipient: OwnerAddr) -> Vec<[u8; 16]>;
}

pub struct RuntimeUnicastTransport {
    tx: tokio::sync::mpsc::Sender<UnicastSendRequest>,
    resolver: std::sync::Arc<dyn DestinationResolver>,
    self_owner: OwnerAddr,
    our_signing_device_hash: DeviceIdentityHash,
    signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
}

impl RuntimeUnicastTransport {
    pub fn new(
        tx: tokio::sync::mpsc::Sender<UnicastSendRequest>,
        resolver: std::sync::Arc<dyn DestinationResolver>,
        self_owner: OwnerAddr,
        our_signing_device_hash: DeviceIdentityHash,
        signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    ) -> Self {
        Self { tx, resolver, self_owner, our_signing_device_hash, signing_key }
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

        let signed = crate::dm_envelope::DmCidNotifySigned {
            space_id: entry.space_id,
            message_cid: entry.message_cid,
            sender_owner_addr: self.self_owner,
            sender_devices: vec![self.our_signing_device_hash],
            signing_device_hash: self.our_signing_device_hash,
        };
        let packet = crate::dm_envelope::build_signed_cidnotify(signed, &self.signing_key)
            .map_err(|e| TransportError::Permanent(format!("build_signed_cidnotify: {e}")))?;
        let wire = crate::dm_envelope::encode_packet(&packet)
            .map_err(|e| TransportError::Permanent(format!("encode_packet: {e}")))?;

        for dest_hash in destinations {
            self.tx.send(UnicastSendRequest {
                destination_hash: dest_hash,
                packet: wire.clone(),
            }).await.map_err(|e| {
                TransportError::Transient(format!("event-loop channel closed: {e}"))
            })?;
        }
        Ok(())
    }
}
```

`OwnerDeviceCacheResolver` (the production resolver that reads from `OwnerState`) is added in Task 11.

- [ ] **Step 6.4: Run test to verify it passes**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test runtime_unicast_transport 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 6.5: Add a no-known-devices test**

```rust
#[tokio::test]
async fn runtime_unicast_transport_no_known_devices_is_transient_error() {
    use tokio::sync::mpsc;
    let (tx, _rx) = mpsc::channel::<UnicastSendRequest>(8);
    let resolver = std::sync::Arc::new(StaticDestResolver::new(std::iter::empty()));
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]);
    let our_device = crate::dm_signing::derive_device_hash_from_pubkey(&signing_key.verifying_key());

    let transport = RuntimeUnicastTransport::new(
        tx, resolver,
        OwnerAddr([0xff; 16]),
        our_device,
        std::sync::Arc::new(signing_key),
    );

    let entry = /* ... same fixture as above ... */;
    let err = transport.send(&entry, OwnerAddr([1; 16])).await.unwrap_err();
    assert!(matches!(err, TransportError::Transient(_)));
}
```

- [ ] **Step 6.6: Verification gates + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml

git add src-tauri/src/dm_outbox.rs
git commit -m "$(cat <<'EOF'
feat(zeb-227): RuntimeUnicastTransport adapter — sign + dispatch via mpsc

Phase 3b's production DmTransport. Per send():
1. Resolve recipient OwnerAddr → list of destination hashes via the
   injected DestinationResolver (production impl OwnerDeviceCacheResolver
   lands in Task 11; tests use a StaticDestResolver fixed-table impl).
2. Build a DmCidNotifySigned with signing_device_hash = our device.
3. Sign + encode via dm_envelope::build_signed_cidnotify + encode_packet.
4. Push one UnicastSendRequest per recipient device hash into the
   channel that the event-loop drains and forwards into NodeRuntime
   as RuntimeEvent::SendUnicastToDevice.

Cross-device fan-out is per spec (Flow 2 step 5): every known device
of the recipient gets its own SendUnicastToDevice. The runtime's
per-destination FIFO and cross-destination best-effort ordering
guarantees apply (per ZEB-226 round-13 doc).

DmInvite outbound is Phase 4's add_space IPC for DM kinds (spec Flow 1).
DmAck outbound is built directly by the receive-side handle_cidnotify
(Task 8) — not through DmTransport::send because acks aren't tied to
OutboxEntry retry.

StubTransport is preserved for test use. Phase 2's dm_outbox tests
explicitly construct StubTransport so this addition is transparent
to them.
EOF
)"
```

---

### Task 7: Wire outbound mpsc channel + event_loop arm

**Files:**
- Modify: `src-tauri/src/event_loop.rs` — add `unicast_send_rx` parameter and select arm
- Modify: `src-tauri/src/lib.rs` — construct the channel near `cas_op_tx`; thread through
- Modify: test files (`tests/content_index_integration.rs`, `tests/folder_primitive_integration.rs`) — append `None` to event_loop::run call sites

This is mechanical wiring. No new tests in this task — coverage lives in Task 12's end-to-end integration test.

- [ ] **Step 7.1: Add the mpsc channel parameter to event_loop::run**

Edit `src-tauri/src/event_loop.rs:134-162`. Add a new parameter `unicast_send_rx: Option<mpsc::Receiver<crate::dm_outbox::UnicastSendRequest>>` after `crdt_state`.

- [ ] **Step 7.2: Add the new select arm**

Inside the `tokio::select!` block at `event_loop.rs:584`, after the existing `Some(op) = cas_op_rx.recv()` arm:

```rust
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

Same pattern as the existing optional channels.

- [ ] **Step 7.3: Update the three event_loop::run callers**

```bash
grep -nE "event_loop::run\b" src-tauri/src/lib.rs src-tauri/tests/*.rs
```

Test files: append `None` to each call site. Production caller in `lib.rs`: pass `Some(unicast_send_rx)` — wire its construction in the next step.

- [ ] **Step 7.4: Construct the channel in lib.rs and lift to NodeState**

Near `cas_op_tx, cas_op_rx` construction (~line 572):

```rust
let (cas_op_tx, cas_op_rx) = tokio::sync::mpsc::channel::<crate::content_store::CasOp>(8);
// ZEB-227: outbound DM unicast channel. Sized at 64 to accommodate
// group-DM fan-out (16 members × 4 devices = 64 worst-case).
let (unicast_send_tx, unicast_send_rx) =
    tokio::sync::mpsc::channel::<crate::dm_outbox::UnicastSendRequest>(64);
```

Add to `NodeState`:

```rust
struct NodeState {
    // ... existing ...
    unicast_send_tx: Option<tokio::sync::mpsc::Sender<crate::dm_outbox::UnicastSendRequest>>,
}
```

Update `stop_inner` and the restart path to take + drop this field. Mirror the Phase 2 pattern for `dm_outbox` / `dm_transport` / `crdt_state`.

Pass `Some(unicast_send_rx)` to `event_loop::run`.

- [ ] **Step 7.5: Verification gates + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml

git add src-tauri/src/event_loop.rs src-tauri/src/lib.rs \
        src-tauri/tests/content_index_integration.rs \
        src-tauri/tests/folder_primitive_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-227): wire outbound RuntimeEvent::SendUnicastToDevice channel

New mpsc channel unicast_send_tx/rx (capacity 64 — group-DM fan-out
worst case is 16 members × 4 devices) constructed in start_node. New
tokio::select! arm in event_loop drains the receiver and forwards each
UnicastSendRequest into NodeRuntime as RuntimeEvent::SendUnicastToDevice.
The runtime queues it in pending_unicast_sends and resolves on next
tick against the path table (per ZEB-226's defer-then-drop semantics).

NodeState gains an unicast_send_tx field so RuntimeUnicastTransport
(Task 11) can be instantiated holding a clone of the sender. Stop +
restart cleanup mirrors the Phase 2 dm_outbox/dm_transport/crdt_state
pattern.

Test call sites (content_index_integration.rs, folder_primitive_integration.rs)
get `None` appended to event_loop::run.

The arm has no producers yet — Task 11 wires up RuntimeUnicastTransport.
EOF
)"
```

---

### Task 8: `handle_unicast` skeleton + `resolve_signed_origin_owner` + `lookup_pubkey_for_device`

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` — add helpers + handle_unicast skeleton (placeholder bodies for handle_invite/handle_cidnotify/handle_ack)

- [ ] **Step 8.1: Write failing tests**

```rust
#[test]
fn resolve_signed_origin_owner_single_match_returns_ok() { /* like Path A's test */ }

#[test]
fn resolve_signed_origin_owner_no_matches_is_unknown_signing_device() { /* like Path A */ }

#[test]
fn resolve_signed_origin_owner_multiple_matches_is_ambiguous() { /* like Path A */ }

#[tokio::test]
async fn handle_unicast_invalid_packet_returns_decode_error() {
    use crate::owner_state_crdt::OwnerState;
    let mut state = OwnerState::default();
    let mut outbox = DmOutbox::new("device".into(), OwnerAddr([0xff; 16]));
    let cas = crate::content_store::InMemoryStub::default();
    let (tx, _rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);

    let bogus = vec![0xff, 0xa0]; // invalid discriminant
    let err = outbox.handle_unicast(
        &mut state, &cas, &tx, bogus, 100,
    ).await.unwrap_err();
    assert!(matches!(err, DmReceiveError::Decode(_)));
}
```

Note: `handle_unicast`'s signature in Path B is *simpler* than Path A — no `source: Option<[u8;16]>` parameter. The signing device hash comes from the packet body itself (after signature verification).

- [ ] **Step 8.2: Run tests to verify they fail**

- [ ] **Step 8.3: Implement helpers + handle_unicast skeleton**

```rust
pub(crate) fn resolve_signed_origin_owner(
    cache: &OwnerDeviceCache,
    signing_device_hash: DeviceIdentityHash,
) -> Result<OwnerAddr, DmReceiveError> {
    let matches: Vec<OwnerAddr> = cache.devices.iter()
        .filter(|(_, entry)| entry.devices.binary_search(&signing_device_hash).is_ok())
        .map(|(addr, _)| *addr)
        .collect();
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(DmReceiveError::UnknownSigningDevice),
        _ => Err(DmReceiveError::AmbiguousSigningDevice),
    }
}

/// Look up the cached Ed25519 verifying key for a known device. Reads
/// from OwnerDeviceCache via the parallel-vec correspondence between
/// `devices[i]` and `device_signing_pubs[i]`.
///
/// Returns Some(pubkey) only if the device hash is in the cache AND
/// the cache has a pubkey at the corresponding index. Returns None
/// for either: device unknown, or device known but pubkey not yet
/// cached (pre-bootstrap state — handler treats as UnknownSigningKey).
pub(crate) fn lookup_pubkey_for_device(
    cache: &OwnerDeviceCache,
    signing_device_hash: DeviceIdentityHash,
) -> Option<ed25519_dalek::VerifyingKey> {
    for entry in cache.devices.values() {
        if let Ok(idx) = entry.devices.binary_search(&signing_device_hash) {
            if idx < entry.device_signing_pubs.len() {
                return ed25519_dalek::VerifyingKey::from_bytes(&entry.device_signing_pubs[idx]).ok();
            }
            return None;  // device present but no pubkey cached
        }
    }
    None
}

impl DmOutbox {
    /// Inbound DM packet entry point. Decodes, verifies signature,
    /// dispatches by discriminant. Per spec §"Application-signature
    /// binding rule", every dispatched arm uses the verified
    /// signing_device_hash from the packet body (NOT a payload-controlled
    /// owner field).
    pub async fn handle_unicast(
        &mut self,
        state: &mut OwnerState,
        cas: &dyn ContentStore,
        unicast_send_tx: &tokio::sync::mpsc::Sender<UnicastSendRequest>,
        packet_bytes: Vec<u8>,
        wall_now_ms: u64,
    ) -> Result<DrainOutcome, DmReceiveError> {
        let packet = crate::dm_envelope::decode_packet(&packet_bytes)
            .map_err(|e| DmReceiveError::Decode(e.to_string()))?;

        match packet {
            crate::dm_envelope::DmPacket::Invite { signed, signature, signed_bytes } => {
                self.handle_invite(state, signed, signature, &signed_bytes, wall_now_ms).await
            }
            crate::dm_envelope::DmPacket::CidNotify { signed, signature, signed_bytes } => {
                self.handle_cidnotify(state, cas, unicast_send_tx, signed, signature, &signed_bytes, wall_now_ms).await
            }
            crate::dm_envelope::DmPacket::Ack { signed, signature, signed_bytes } => {
                self.handle_ack(state, signed, signature, &signed_bytes, wall_now_ms).await
            }
        }
    }

    pub async fn handle_invite(
        &mut self,
        _state: &mut OwnerState,
        _signed: crate::dm_envelope::DmInviteSigned,
        _signature: [u8; 64],
        _signed_bytes: &[u8],
        _wall_now_ms: u64,
    ) -> Result<DrainOutcome, DmReceiveError> {
        Err(DmReceiveError::Decode("Task 9 implements handle_invite".into()))
    }

    pub async fn handle_cidnotify(
        &mut self,
        _state: &mut OwnerState,
        _cas: &dyn ContentStore,
        _unicast_send_tx: &tokio::sync::mpsc::Sender<UnicastSendRequest>,
        _signed: crate::dm_envelope::DmCidNotifySigned,
        _signature: [u8; 64],
        _signed_bytes: &[u8],
        _wall_now_ms: u64,
    ) -> Result<DrainOutcome, DmReceiveError> {
        Err(DmReceiveError::Decode("Task 10 implements handle_cidnotify".into()))
    }

    pub async fn handle_ack(
        &mut self,
        _state: &mut OwnerState,
        _signed: crate::dm_envelope::DmAckSigned,
        _signature: [u8; 64],
        _signed_bytes: &[u8],
        _wall_now_ms: u64,
    ) -> Result<DrainOutcome, DmReceiveError> {
        Err(DmReceiveError::Decode("Task 11 implements handle_ack".into()))
    }
}
```

- [ ] **Step 8.4: Run tests to verify they pass + verification gates + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo test resolve_signed_origin_owner handle_unicast_invalid 2>&1 | tail -10

cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml

git add src-tauri/src/dm_outbox.rs
git commit -m "$(cat <<'EOF'
feat(zeb-227): handle_unicast skeleton + resolve_signed_origin_owner

Phase 3b's inbound DM entry point. Decodes the wire bytes into a
DmPacket discriminant + signed body + signature + signed_bytes, then
dispatches to handle_invite / handle_cidnotify / handle_ack by variant.

The Path B signature verification happens INSIDE each handler (not
in handle_unicast) because the pubkey-lookup strategy differs by
discriminant: DmInvite uses the inline inviter_signing_pub field;
CidNotify / Ack use lookup_pubkey_for_device against OwnerDeviceCache.
Centralizing verification in handle_unicast would force a generic
"first try inline pubkey, fallback to cache lookup" pattern that's
less expressive than per-discriminant handling.

Two helpers added:
- resolve_signed_origin_owner(cache, hash) -> Result<OwnerAddr> —
  same shape as Path A's resolve_link_origin_owner: single match Ok,
  zero match UnknownSigningDevice, multi-match AmbiguousSigningDevice
  (cache-poisoning regression).
- lookup_pubkey_for_device(cache, hash) -> Option<VerifyingKey> —
  reads OwnerDeviceCache's parallel-vec correspondence between
  devices[i] and device_signing_pubs[i]. Returns None if device
  unknown OR device present but pubkey not yet cached (pre-bootstrap).

The three handler stubs ship as Err("Task N implements") placeholders;
Tasks 9-11 fill them in.
EOF
)"
```

---

### Task 9: `handle_invite` — signature verification + sanity gates + auto-accept

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` — replace handle_invite stub with real body + 5+ tests

Per spec scope decision: Phase 3b auto-accepts; user-driven decline UX deferred to Phase 4.

- [ ] **Step 9.1: Write failing tests for handle_invite**

Five tests covering:
- Happy path: writes Space + cache + cached pubkey
- Inviter ≠ members[0] (lex-largest-inviter regression)
- inviter ∉ members → InviterNotInMembers
- signing_device_hash ∉ sender_devices → SigningDeviceNotInSenderDevices
- self_owner ∉ members → ReceiverNotInMembers
- Tampered body / forged signature → SignatureVerificationFailed

The happy path test must:
1. Build a real signed invite via `build_signed_invite`
2. Verify after `handle_invite` runs: `state.spaces` has the new Space, `state.owner_device_cache.devices.get(&inviter_addr).device_signing_pubs[0] == inviter_signing_pub`

- [ ] **Step 9.2: Run tests to verify they fail**

- [ ] **Step 9.3: Implement handle_invite**

```rust
pub async fn handle_invite(
    &mut self,
    state: &mut OwnerState,
    signed: crate::dm_envelope::DmInviteSigned,
    signature: [u8; 64],
    signed_bytes: &[u8],
    _wall_now_ms: u64,
) -> Result<DrainOutcome, DmReceiveError> {
    // Sanity gate 1: inviter ∈ members
    if !signed.members.contains(&signed.inviter) {
        return Err(DmReceiveError::InviterNotInMembers);
    }
    // Sanity gate 2: signing_device_hash ∈ sender_devices
    // (decode_packet already enforces this — defense-in-depth)
    if !signed.sender_devices.contains(&signed.signing_device_hash) {
        return Err(DmReceiveError::SigningDeviceNotInSenderDevices);
    }
    // Sanity gate 3: self_owner ∈ members
    if !signed.members.contains(&self.self_owner) {
        return Err(DmReceiveError::ReceiverNotInMembers);
    }
    // Verify signature using inline inviter_signing_pub.
    let signing_pub = ed25519_dalek::VerifyingKey::from_bytes(&signed.inviter_signing_pub)
        .map_err(|_| DmReceiveError::SignatureVerificationFailed)?;
    crate::dm_signing::verify_dm_packet_signature(
        signed_bytes,
        &signature,
        &signing_pub,
        signed.signing_device_hash,
    )?;

    // Phase 3b auto-accept: write Space + cache + cached pubkey.
    // (Phase 4 replaces with stage-pending-invite + UI prompt path.)

    // Pubkey list is parallel to sender_devices; we know the inviter's
    // signing pubkey for the device that signed THIS invite. For the
    // other devices in sender_devices we have no pubkeys yet — they
    // remain pre-bootstrap until the next invite-equivalent flow.
    // Build a parallel vec of length sender_devices.len() with the
    // signing pubkey at the correct index, [0u8; 32] elsewhere.
    // (lookup_pubkey_for_device treats [0u8; 32] as "no pubkey cached"
    // because it parses to the all-zeros point which fails validation —
    // wait, ed25519_dalek::VerifyingKey::from_bytes accepts arbitrary
    // 32-byte values; we need a sentinel. Use Option<[u8; 32]> in the
    // OwnerDeviceEntry instead — implementer adjusts Task 4's design
    // here if not already done.)
    // SCOPE NOTE: revisit Task 4's representation if needed: Option<[u8; 32]>
    // is more honest than [u8; 32] sentinel. Implementer decides.
    let mut device_signing_pubs: Vec<[u8; 32]> = vec![[0u8; 32]; signed.sender_devices.len()];
    let signer_idx = signed.sender_devices.iter()
        .position(|d| *d == signed.signing_device_hash)
        .expect("sanity gate 2 already verified this");
    device_signing_pubs[signer_idx] = signed.inviter_signing_pub;

    let cache_outcome = state.apply_owner_device_update(
        signed.inviter,
        signed.sender_devices.clone(),
        device_signing_pubs,
        signed.created_at.clone(),
    );
    if let crate::owner_state_crdt::ApplyOutcome::Rejected(reason) = cache_outcome {
        return Err(DmReceiveError::CrdtRejected(format!("{:?}", reason)));
    }

    let space = crate::owner_state_types::Space {
        id: signed.space_id,
        kind: signed.kind,
        parent: None,
        community_id: None,
        name: format!("DM with {:?}", signed.inviter),
        transport: Some(crate::owner_state_types::TransportBinding::Reticulum {
            participants: signed.members.clone(),
        }),
        members: signed.members,
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: signed.created_at.clone(),
        updated_at: signed.created_at,
        content_key: Some(signed.content_key),
        prior_content_keys: vec![],
    };
    let space_outcome = state.apply_space_with_canonicalization(space);
    if let crate::owner_state_crdt::ApplyOutcome::Rejected(reason) = space_outcome {
        return Err(DmReceiveError::CrdtRejected(format!("{:?}", reason)));
    }

    Ok(DrainOutcome::default())
}
```

**Note on the all-zeros sentinel comment:** if Task 4's representation used `Vec<[u8; 32]>` and the implementer realizes the all-zeros sentinel is fragile (a valid Ed25519 verifying key COULD be all-zeros — the curve allows it, even if cryptographically odd), revisit Task 4 to use `Vec<Option<[u8; 32]>>` instead. Don't ship a brittle sentinel.

- [ ] **Step 9.4: Run tests to verify they pass + gates + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
cargo fmt --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml

git add src-tauri/src/dm_outbox.rs src-tauri/src/owner_state_types.rs src-tauri/src/owner_state_crdt.rs
git commit -m "$(cat <<'EOF'
feat(zeb-227): handle_invite — signature verification + sanity gates + auto-accept

Phase 3b's inbound DmInvite handler. Per spec ea38132 §"Application-
signature binding rule":

1. Sanity gates (cheap, run before signature verification):
   - inviter ∈ members
   - signing_device_hash ∈ sender_devices (defense-in-depth; decode_packet
     also enforces)
   - self_owner ∈ members
2. Verify signature using inline inviter_signing_pub (DmInvite is the
   bootstrap exception — receiver doesn't yet have OwnerDeviceCache
   entry for the inviter).
3. Auto-accept (Phase 3b ships no UI; user-driven decline deferred
   to Phase 4 with follow-up Linear ticket):
   - apply_owner_device_update with sender_devices + parallel pubkey
     vec (signing pubkey at signer's index, all-zeros elsewhere — TODO
     if all-zeros is rejected as not-a-valid-key on lookup, fold into
     a None sentinel rather than zero bytes).
   - apply_space_with_canonicalization for the new DM Space.

Five tests added:
- handle_invite_writes_space_and_cache_with_signing_pub
- handle_invite_binds_inviter_field_not_members_zero (lex-largest-inviter
  regression)
- handle_invite_inviter_not_in_members_drops
- handle_invite_signing_device_not_in_sender_devices_drops
- handle_invite_receiver_not_in_members_drops
- handle_invite_tampered_signature_drops
EOF
)"
```

---

### Task 10: `handle_cidnotify` — signature verify + CAS fetch + decrypt + inbox + ack fan-out

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` — replace handle_cidnotify stub with real body + 6+ tests

Largest task. Per spec Flow 2 steps 7-13 (with Path B signature verification replacing link-origin binding).

- [ ] **Step 10.1: Widen DrainOutcome with `newly_received: Vec<InboxEntry>`**

```rust
pub struct DrainOutcome {
    pub newly_delivered: Vec<(OutboxEntryId, OwnerAddr)>,
    pub newly_expired: Vec<OutboxEntryId>,
    /// Phase 3b: InboxEntries written by handle_cidnotify for which
    /// apply_inbox returned Inserted (not NoOp). Caller emits dm-received
    /// IPC events from this field.
    pub newly_received: Vec<crate::owner_state_types::InboxEntry>,
}
```

Phase 2's `drain` leaves it empty; only `handle_cidnotify` populates it.

- [ ] **Step 10.2: Write failing tests** (6+ tests, see spec test list)

- [ ] **Step 10.3: Implement handle_cidnotify**

```rust
pub async fn handle_cidnotify(
    &mut self,
    state: &mut OwnerState,
    cas: &dyn ContentStore,
    unicast_send_tx: &tokio::sync::mpsc::Sender<UnicastSendRequest>,
    signed: crate::dm_envelope::DmCidNotifySigned,
    signature: [u8; 64],
    signed_bytes: &[u8],
    wall_now_ms: u64,
) -> Result<DrainOutcome, DmReceiveError> {
    // Step 7a: look up the signing pubkey for signing_device_hash.
    let signing_pub = lookup_pubkey_for_device(&state.owner_device_cache, signed.signing_device_hash)
        .ok_or(DmReceiveError::UnknownSigningKey)?;

    // Step 7b: verify the signature.
    crate::dm_signing::verify_dm_packet_signature(
        signed_bytes,
        &signature,
        &signing_pub,
        signed.signing_device_hash,
    )?;

    // Step 7c: resolve signing_device_hash → OwnerAddr.
    let resolved_owner = resolve_signed_origin_owner(&state.owner_device_cache, signed.signing_device_hash)?;

    // Step 7d: verify notify.sender_owner_addr matches resolved owner.
    if signed.sender_owner_addr != resolved_owner {
        return Err(DmReceiveError::OwnerFieldMismatch);
    }

    // Look up the Space for AAD + content_key.
    let space_clone = state.spaces.get(&signed.space_id)
        .ok_or(DmReceiveError::SpaceNotFound)?
        .clone();

    // Step 8: refresh OwnerDeviceCache with notify.sender_devices (LWW HLC-bound).
    // We don't have signing pubs for the OTHER devices in sender_devices
    // (only for the one that signed THIS notify) — pass [0u8; 32] for the
    // others. Adjust to use Option<[u8; 32]> per Task 9's note.
    let mut updated_pubs = vec![[0u8; 32]; signed.sender_devices.len()];
    if let Some(idx) = signed.sender_devices.iter().position(|d| *d == signed.signing_device_hash) {
        updated_pubs[idx] = signing_pub.to_bytes();
    }
    let _ = state.apply_owner_device_update(
        resolved_owner,
        signed.sender_devices.clone(),
        updated_pubs,
        crate::owner_state_types::Hlc {
            wall_ms: wall_now_ms,
            logical: 0,
            device_id: self.device_id.clone(),
        },
    );

    // Step 9: fetch the storage_blob from CAS via cas.get with 500ms timeout.
    let blob = match tokio::time::timeout(
        std::time::Duration::from_millis(500),
        cas.get(&signed.message_cid),
    ).await {
        Ok(Ok(Some(bytes))) => bytes,
        Ok(Ok(None)) => return Err(DmReceiveError::CasFetchFailed("blob not found".into())),
        Ok(Err(e)) => return Err(DmReceiveError::CasFetchFailed(format!("{e:?}"))),
        Err(_) => return Err(DmReceiveError::CasFetchFailed("500ms fetch timeout".into())),
    };

    // Step 11: decrypt with prior-keys fallback.
    let aad = crate::dm_crypto::compute_aad(&space_clone)
        .map_err(|e| DmReceiveError::AadCompute(e.to_string()))?;
    let payload = crate::dm_crypto::decrypt_dm_message(
        space_clone.content_key.as_ref().expect("DM Space MUST have content_key"),
        &space_clone.prior_content_keys,
        &aad,
        &blob,
    ).map_err(|_| DmReceiveError::DecryptFailed)?;

    // Step 12: sender-binding check (encrypted-payload layer).
    crate::dm_crypto::verify_sender_binding(&payload, resolved_owner)
        .map_err(|_| DmReceiveError::SenderImpersonation)?;

    // Step 13a: apply_inbox — atomic-emit semantics.
    let inbox_entry = crate::owner_state_types::InboxEntry {
        space_id: signed.space_id,
        message_cid: signed.message_cid,
        from: resolved_owner,
        received_at: crate::owner_state_types::Hlc {
            wall_ms: wall_now_ms,
            logical: 0,
            device_id: self.device_id.clone(),
        },
    };
    let outcome = state.apply_inbox(inbox_entry.clone());
    let mut drain_outcome = DrainOutcome::default();
    if matches!(outcome, crate::owner_state_crdt::ApplyOutcome::Inserted) {
        drain_outcome.newly_received.push(inbox_entry);
    }

    // Step 13b: ack fan-out to all sender_devices.
    let our_device_hash = self.our_signing_device_hash();  // helper added on DmOutbox
    let our_ack_devices = state.owner_device_cache.devices.get(&self.self_owner)
        .map(|e| e.devices.clone())
        .unwrap_or_else(|| vec![our_device_hash]);

    let ack_signed = crate::dm_envelope::DmAckSigned {
        space_id: signed.space_id,
        message_cid: signed.message_cid,
        ack_from_owner_addr: self.self_owner,
        ack_from_devices: our_ack_devices,
        signing_device_hash: our_device_hash,
    };
    let ack_packet = crate::dm_envelope::build_signed_ack(ack_signed, &self.signing_key())
        .map_err(|e| DmReceiveError::Decode(e.to_string()))?;
    let ack_wire = crate::dm_envelope::encode_packet(&ack_packet)
        .map_err(|e| DmReceiveError::Decode(e.to_string()))?;

    for device in &signed.sender_devices {
        let dest_hash = compute_dm_destination_hash(device.0);
        let _ = unicast_send_tx.send(UnicastSendRequest {
            destination_hash: dest_hash,
            packet: ack_wire.clone(),
        }).await;  // failed sends are silent per spec
    }

    Ok(drain_outcome)
}
```

DmOutbox needs new fields to support this: `our_signing_device_hash: DeviceIdentityHash` and `signing_key: Arc<SigningKey>`. Add them in `DmOutbox::new` (signature widens — propagate to all callers). Phase 2 callers in tests pass dummy values; production caller in `lib.rs` (Task 11) passes the real key.

`compute_dm_destination_hash` is the same helper from the original plan; lives in `dm_signing.rs` or a new `dm_destination.rs` module.

- [ ] **Step 10.4: Tests + gates + commit**

```bash
git commit -m "$(cat <<'EOF'
feat(zeb-227): handle_cidnotify — signature verify + CAS fetch + decrypt + inbox + ack fan-out

[Full body similar to original plan but adapted for Path B signature
verification path. DrainOutcome widened with newly_received: Vec<InboxEntry>.
Six tests added per spec Phase 3b test list.]
EOF
)"
```

(Plan abbreviates the commit message detail since the structural pattern is the same as Task 9; implementer fills in the comprehensive description before committing.)

---

### Task 11: `handle_ack` + `OwnerDeviceCacheResolver` + replace `StubTransport` in production + event_loop interception

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` — handle_ack real body + OwnerDeviceCacheResolver
- Modify: `src-tauri/src/event_loop.rs` — wire UnicastReceived interception, plumb cas_handle through `event_loop::run` params
- Modify: `src-tauri/src/lib.rs` — replace StubTransport with RuntimeUnicastTransport using OwnerDeviceCacheResolver and the device's signing key

Combined task (was Tasks 9+11 in the original plan; merged because both touch lib.rs's start_node and a single commit is cleaner).

- [ ] **Step 11.1: Implement handle_ack** (4 tests; same shape as original plan but using `signing_device_hash` + signature verification)

- [ ] **Step 11.2: Add OwnerDeviceCacheResolver**

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
        let state = match self.state.try_lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),  // contention → transient transport error
        };
        state.owner_device_cache.devices.get(&recipient)
            .map(|entry| entry.devices.iter().map(|d| compute_dm_destination_hash(d.0)).collect())
            .unwrap_or_default()
    }
}
```

- [ ] **Step 11.3: Wire event_loop interception**

Per the original plan: extract a helper, intercept `RuntimeAction::UnicastReceived` BEFORE `dispatch_action` at all 3 `runtime.tick()` sites. Plumb `cas_handle: Option<Arc<dyn ContentStore>>` through `event_loop::run`. Wire dm-received / dm-delivered / dm-expired emits from `DrainOutcome`.

- [ ] **Step 11.4: Replace StubTransport in lib.rs**

```rust
// Acquire the device's signing key from the existing identity-management
// code path. (Implementer reads lib.rs:start_node to find where device_id
// + self_owner come from; the signing key lives there too — likely in
// harmony_identity::PrivateIdentity. Pull the Ed25519 SigningKey out.)
let signing_key = std::sync::Arc::new(extract_signing_key_from_identity(...));
let our_device_hash = crate::dm_signing::derive_device_hash_from_pubkey(&signing_key.verifying_key());

let resolver = std::sync::Arc::new(crate::dm_outbox::OwnerDeviceCacheResolver::new(crdt_state.clone()));
let transport: std::sync::Arc<dyn crate::dm_outbox::DmTransport> =
    std::sync::Arc::new(crate::dm_outbox::RuntimeUnicastTransport::new(
        unicast_send_tx.clone(),
        resolver,
        self_owner,
        our_device_hash,
        signing_key.clone(),
    ));

// And inject the same signing key into DmOutbox::new (Task 10's widening):
let outbox = crate::dm_outbox::DmOutbox::new(device_id.clone(), self_owner, signing_key, our_device_hash);
```

- [ ] **Step 11.5: Tests + gates + commit**

```bash
git commit -m "$(cat <<'EOF'
feat(zeb-227): handle_ack + OwnerDeviceCacheResolver + production transport swap + event_loop interception

[Combined task: handle_ack real body + OwnerDeviceCacheResolver +
replace StubTransport in production wiring + event_loop interception
of RuntimeAction::UnicastReceived. Four tests for handle_ack mirroring
spec Phase 3b test list.]
EOF
)"
```

---

### Task 12: Register local DM destination at startup + end-to-end integration test

**Files:**
- Modify: `src-tauri/src/lib.rs` — `runtime.register_local_destination(dm_dest)` at start_node
- Create: `src-tauri/tests/dm_unicast_integration.rs` — end-to-end test at the channel boundary

- [ ] **Step 12.1: Compute + register the DM destination at start_node**

```rust
let local_identity = runtime.local_identity_hash();
let dm_dest = compute_dm_destination_hash(local_identity);
runtime.register_local_destination(dm_dest);
```

- [ ] **Step 12.2: Write the integration test**

Two tests:
- `dm_full_round_trip_through_unicast_channel` — happy path, end-to-end, including signature verification on both sides.
- `dm_offline_recipient_then_online_delivers` — exercises drain backoff path.

Both mock at the channel boundary, NOT the wire.

- [ ] **Step 12.3: Tests + gates + commit**

---

### Task 13: Open PR + file follow-up Linear tickets

Push branch, open harmony-client PR, file follow-ups:
1. **Phase 4 user-driven invite decline UX** (modal + accept/decline IPC)
2. **harmony terminal-link state — responder-side handshake wiring for future link-using features (voice / file sync / streaming)** — the deferred Path A
3. **Per-device signing pubkey piggyback on every DmCidNotify / DmAck** (removes the bootstrap-incompleteness window where receiver knows the device hash but not the pubkey)
4. **Manual two-device LAN smoke scenarios**
5. **Inbound DM packet drop on lock contention** (handle_unicast skipped this tick) — investigate buffering vs higher-priority lock ordering

Per memory rule "never invent Linear IDs" — file each via the Linear MCP, capture the assigned ID, then update the PR body.

- [ ] **Step 13.1**: `git push -u origin zeb-227-dm-transport-phase3b`
- [ ] **Step 13.2**: `gh pr create` with the body listing all five follow-ups (placeholders, then fill in)
- [ ] **Step 13.3**: File the follow-ups via Linear MCP
- [ ] **Step 13.4**: Update PR body with assigned IDs

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

Per pipe-exit-codes-lie rule: any verification command that pipes through `tail`/`grep` MUST set `pipefail` first.

---

## Spec coverage cross-check

| Spec Phase 3b test | Plan task |
|---|---|
| `dm_envelope::dm_packet_signature_round_trip` | Task 5, Step 5.1 |
| `dm_envelope::dm_packet_decode_too_short_for_signature_rejects` | Task 5, Step 5.1 |
| `dm_envelope::dm_packet_signature_does_not_cover_discriminant` | Task 5, Step 5.1 |
| `dm_outbox::resolve_signed_origin_owner_*` (3 tests) | Task 8, Step 8.1 |
| `dm_outbox::verify_dm_packet_signature_*` (4 tests) | Task 3, Step 3.3 |
| `dm_outbox::handle_unicast_invite_creates_space` | Task 9, Step 9.1 |
| `dm_outbox::handle_unicast_invite_binds_inviter_field_not_members_zero` | Task 9, Step 9.1 |
| `dm_outbox::handle_unicast_invite_inviter_not_in_members_drops` | Task 9, Step 9.1 |
| `dm_outbox::handle_unicast_invite_signing_device_not_in_sender_devices_drops` | Task 9, Step 9.1 |
| `dm_outbox::handle_unicast_invite_receiver_not_in_members_drops` | Task 9, Step 9.1 |
| `dm_outbox::handle_unicast_invite_signature_invalid_drops` | Task 9, Step 9.1 |
| `dm_outbox::handle_unicast_invite_decline_writes_no_state` | Deferred to Phase 4 (no UI in 3b) |
| `dm_outbox::handle_unicast_cidnotify_*` (8 tests) | Task 10, Step 10.2 |
| `dm_outbox::handle_unicast_ack_*` (5 tests) | Task 11, Step 11.1 |
| `dm_outbox::expiration_*` (3 tests) | Already covered by Phase 2; Task 12's offline-recipient test exercises the drain through Phase 3b's transport |

---

## Out of scope (intentional)

- User-driven DmInvite decline UX — Phase 4 follow-up
- Forward secrecy / content-key rotation — ZEB-219 deferral
- Per-device delivery lease in OutboxEntry — v1 tolerates cross-device duplicate sends
- HLC-monotonic per-OwnerAddr `device_list_version` to suppress redundant `sender_devices` piggyback
- DmReactions, DmReadReceipts
- Voice/video DM transport
- Reticulum terminal-link state + responder-side handshake (Path A) — filed as separate ticket for when voice / file sync / streaming need it
