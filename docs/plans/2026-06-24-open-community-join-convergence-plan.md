# Open-Community Join Convergence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make open-community join-by-URL converge between two nodes by relaxing the community-state membership gate, for open communities only, to admit an unknown publisher whose publish blob carries a valid self-signed `Join`.

**Architecture:** `handle_incoming_publish` (`community_state_sync.rs`) currently rejects any publish whose publisher isn't already `Joined` in local state — pre-decrypt, so a brand-new open joiner's self-`Join` (which lives only inside their own encrypted blob) can never authorize their publish. We add a *deferred bootstrap-admission* path: for `!is_invite_only` + entirely-unknown publishers, skip the early reject + early sig-verify, decrypt/decode the blob (already done later in the fn), validate the publisher's in-blob self-`Join` via a new pure helper `bootstrap_admit_open_publisher` (reusing `verify_event` + `materialize_with_now`), seed the resulting enrolled keys into a synthetic `MemberState`, verify the root `publisher_sig` against it, then admit. The normal merge re-validates and inserts the Join. Invite-only and known-but-not-`Joined` publishers are untouched.

**Tech Stack:** Rust 1.88+, tokio, serde/ciborium (canonical CBOR), ed25519-dalek. Tests: `cargo nextest`, `tempfile`, in-memory CAS + channel-forwarder two-engine harness. No new dependencies.

## Global Constraints

- **No worktrees.** Work in the main repo on branch `open-community-gate-self-join-admission` (already created off `main` @ `2a5b65e6`; spec committed `17c606e2`).
- **Keep ZEB IDs out of branch/commit/PR names.** Reference ZEB-558 descriptively in the PR body (no Linear magic-word close), never in commit subjects or the branch.
- **Scope strictly to open communities + unknown publishers.** Every new branch is gated on `!ctx.is_invite_only` AND `members.get(publisher).is_none()`. Invite-only retains the strict pre-decrypt reject; known-but-`Left`/`Banned` publishers retain the strict reject.
- **Pipe exit codes lie.** Use `set -o pipefail` (bash) / `${pipestatus[1]}` (zsh) when piping cargo through `tail`.
- **Gate commands** (from `CLAUDE.md`, run from `src-tauri/`): fmt `cargo fmt --all -- --check`; clippy `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; tests `cargo nextest run --locked -p harmony-app --features test-fixtures`. Scope to touched tests during dev; run `--all-targets` only in the final sweep (Task 3) — harmony-app `--all-targets` relinks ~97 integration binaries (~25 min).
- **Test drift is our fault.** Any pre-existing breakage surfaced during this work gets fixed or filed, not externalized.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src-tauri/src/community_membership.rs` | Membership CRDT + verify rules | **Add** `pub fn bootstrap_admit_open_publisher(...)` near `verify_event` (~line 2755). Pure, unit-testable, reuses `verify_event` + `materialize_with_now` + `enrolled_key_from_cert`. No rule changes. |
| `src-tauri/src/community_state_sync.rs` | Engine receive path | **Modify** `handle_incoming_publish`: step-2 gate (~3127) yields a `deferred_open_bootstrap` flag instead of always rejecting; the sig-verify (~3178) is skipped for the deferred case; after blob decode+sort (~3273) the deferred case runs `bootstrap_admit_open_publisher` + `verify_publisher_sig`; the TOCTOU re-check (~3308) skips its assert for the deferred case. |
| `src-tauri/tests/community_sync/community_open_flow_integration.rs` | Two-engine open-flow integration tests | **Add** the no-preseed wire-convergence test (Task 2) + the helper unit tests (Task 1). Already imports the needed harness + `mint_*` API. |

No new files; no module-registration changes (both additions land in files already compiled into the `harmony-app` lib + the `community_sync` integration target).

---

## Task 1: `bootstrap_admit_open_publisher` helper + unit tests

**Files:**
- Modify: `src-tauri/src/community_membership.rs` — add `bootstrap_admit_open_publisher` after `verify_event` (`~2890`, i.e. just past the end of `verify_event`).
- Test: `src-tauri/tests/community_sync/community_open_flow_integration.rs` — append a `#[tokio::test]`-free `#[test]` unit block.

**Interfaces:**
- Consumes: `verify_event(&SignedMembershipEvent, &MaterializedMembership, &VerifyContext) -> Result<(), VerifyError>` (community_membership.rs:2755); `materialize_with_now(&[SignedMembershipEvent], OwnerAddr, Option<u64>) -> MaterializedMembership` (2725); `enrolled_key_from_cert` (1314) indirectly via materialize/verify; types `MemberState`, `MaterializedMembership`, `VerifyContext`, `MembershipEventKind`, `OwnerAddr`, `SpaceId`.
- Produces: `pub fn bootstrap_admit_open_publisher(incoming_events: &[SignedMembershipEvent], publisher_addr: OwnerAddr, admin_addr: OwnerAddr, expected_community_id: SpaceId) -> Option<MemberState>` — `Some(MemberState{status: Joined, enrolled_device_keys: …})` iff `incoming_events` carries a signature-valid open self-`Join` for `publisher_addr`; else `None`. Consumed by Task 2's gate.

- [ ] **Step 1: Write the failing unit tests**

Append to `src-tauri/tests/community_sync/community_open_flow_integration.rs` (the file already imports `mint_community_creation`, `mint_redemption`, `OwnerAddr`, `Hlc`, and the community types):

```rust
#[cfg(test)]
mod bootstrap_admit_open_publisher_tests {
    use super::*;
    use harmony_app::community_membership::{bootstrap_admit_open_publisher, MemberStatus};

    // A freshly-minted open community: returns (admin_owner, community_id,
    // admin's signature-valid open bootstrap Join event).
    fn mint_open_admin_join(
        seed: u8,
    ) -> (
        OwnerAddr,
        harmony_app::owner_state_types::SpaceId,
        harmony_app::community_membership::SignedMembershipEvent,
    ) {
        let t = harmony_app::community_membership::mint_test_owner(seed);
        let minted = mint_community_creation(
            "BootstrapTest",
            false, // open
            t.owner,
            &t.device_key,
            &t.cert,
            Hlc { wall_ms: 100_000, logical: 0, device_id: format!("dev-{seed}") },
        )
        .expect("mint create");
        (t.owner, minted.community_id, minted.bootstrap_join)
    }

    #[test]
    fn admits_publisher_with_valid_open_self_join() {
        let (admin, community_id, admin_join) = mint_open_admin_join(0xC1);
        // Publisher == admin (the creator-publish direction): the admin's own
        // self-Join is in the blob; admit with their enrolled key seeded.
        let got = bootstrap_admit_open_publisher(
            std::slice::from_ref(&admin_join),
            admin,
            admin,
            community_id,
        );
        let ms = got.expect("valid open self-Join must admit");
        assert!(matches!(ms.status, MemberStatus::Joined));
        assert!(!ms.enrolled_device_keys.is_empty(), "enrolled keys must be seeded from the cert");
    }

    #[test]
    fn admits_joiner_self_join_from_redemption() {
        // The joiner direction: a redemption-minted self-Join for a DIFFERENT
        // owner against the same open community must admit.
        let (admin, community_id, _admin_join) = mint_open_admin_join(0xC2);
        let joiner = harmony_app::community_membership::mint_test_owner(0xD3);
        let invite = harmony_app::community_invite::CommunityInvitePayload {
            community_id,
            epoch_snapshot: harmony_app::community_invite::InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: vec![0u8; 32],
                sealed_epoch_keys: Vec::new(),
                state_snapshot:
                    harmony_app::community_invite::MaterializedCommunityState::default(),
            },
            admin_addr: admin,
            community_name: "BootstrapTest".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: None,
            pre_fork_snapshot: None,
            inviter_enrollment: None,
            untargeted_decrypt_key: None,
        };
        let minted = mint_redemption(
            &invite,
            joiner.owner,
            &joiner.device_key,
            &joiner.cert,
            Hlc { wall_ms: 200_000, logical: 0, device_id: "joiner-dev".into() },
        )
        .expect("mint redeem");
        let got = bootstrap_admit_open_publisher(
            std::slice::from_ref(&minted.bootstrap_join),
            joiner.owner,
            admin,
            community_id,
        );
        let ms = got.expect("joiner self-Join must admit");
        assert!(matches!(ms.status, MemberStatus::Joined));
        assert!(!ms.enrolled_device_keys.is_empty());
    }

    #[test]
    fn rejects_when_no_self_join_for_publisher() {
        // Blob carries the admin's Join but the publisher is a stranger with
        // no Join present → None.
        let (admin, community_id, admin_join) = mint_open_admin_join(0xC4);
        let stranger = harmony_app::community_membership::mint_test_owner(0xE5).owner;
        let got = bootstrap_admit_open_publisher(
            std::slice::from_ref(&admin_join),
            stranger,
            admin,
            community_id,
        );
        assert!(got.is_none(), "no self-Join for the publisher ⇒ reject");
    }

    #[test]
    fn rejects_self_join_for_wrong_community() {
        // A valid Join, but for a different community_id than the gate expects
        // ⇒ verify_event's WrongCommunity guard fires ⇒ None.
        let (admin, _community_id, admin_join) = mint_open_admin_join(0xC6);
        let other_community = harmony_app::owner_state_types::SpaceId([0x99; 16]);
        let got = bootstrap_admit_open_publisher(
            std::slice::from_ref(&admin_join),
            admin,
            admin,
            other_community,
        );
        assert!(got.is_none(), "Join for a different community must not admit");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd src-tauri
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(bootstrap_admit_open_publisher_tests)' 2>&1 | tail -20
```

Expected: **compile error** — `bootstrap_admit_open_publisher` not found in `harmony_app::community_membership`.

- [ ] **Step 3: Implement the helper**

In `src-tauri/src/community_membership.rs`, immediately after the end of `verify_event` (the function starting at line 2755; insert after its closing `}`), add:

```rust
/// ZEB-558 — bootstrap-admission for an OPEN-community publisher we don't yet
/// know locally. Given the membership events carried in an incoming publish
/// blob, return the publisher's `MemberState` (with enrolled device keys)
/// IFF the blob carries a signature-valid OPEN self-`Join` for them — the
/// exact authorization `verify_event` applies on the merge path (cert +
/// signer key + open-Join rule). Returns `None` when no such valid self-Join
/// is present, so the caller rejects.
///
/// OPEN communities only: the caller (`handle_incoming_publish`) gates this
/// on `!is_invite_only` AND an entirely-unknown publisher. The returned
/// `MemberState` is used solely to verify the root `publisher_sig`; the
/// authoritative merge re-validates and inserts the Join via `insert_event`,
/// so this helper never widens what actually lands in the CRDT.
pub fn bootstrap_admit_open_publisher(
    incoming_events: &[SignedMembershipEvent],
    publisher_addr: OwnerAddr,
    admin_addr: OwnerAddr,
    expected_community_id: SpaceId,
) -> Option<MemberState> {
    let ctx = VerifyContext {
        expected_community_id,
        admin_addr,
        is_invite_only: false,
    };
    // Empty prior: an unknown publisher has no local membership, so the
    // banned-status guard in `verify_event` sees no prior entry (not banned),
    // and an open Join needs no power/countersig. This validates signature +
    // EnrollmentCert exactly as the merge path will.
    let prior = MaterializedMembership::default();
    let verified: Vec<SignedMembershipEvent> = incoming_events
        .iter()
        .filter(|e| {
            e.actor == publisher_addr && matches!(e.kind, MembershipEventKind::Join)
        })
        .filter(|e| verify_event(e, &prior, &ctx).is_ok())
        .cloned()
        .collect();
    if verified.is_empty() {
        return None;
    }
    // Materialize the verified self-Join(s) to derive the canonical
    // MemberState (status + enrolled_device_keys from the cert) the merge
    // will produce. `None` now-floor: we only need the resulting status/keys.
    let mat = materialize_with_now(&verified, admin_addr, None);
    mat.members
        .get(&publisher_addr)
        .filter(|s| matches!(s.status, MemberStatus::Joined))
        .cloned()
}
```

If `materialize_with_now` is at a slightly different line than 2725, find it with `grep -n "pub fn materialize_with_now" src/community_membership.rs`; the signature is `materialize_with_now(events: &[SignedMembershipEvent], admin_addr: OwnerAddr, now_ms: Option<u64>) -> MaterializedMembership`.

- [ ] **Step 4: Run tests to verify they pass**

```bash
cd src-tauri
cargo fmt --all
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(bootstrap_admit_open_publisher_tests)' 2>&1 | tail -20
```

Expected: 4 tests PASS.

- [ ] **Step 5: Scoped gates**

```bash
cd src-tauri
cargo fmt --all -- --check; echo "fmt=$?"
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5; echo "clippy=${pipestatus[1]}"
```

Expected: `fmt=0`, `clippy=0`. (Lib-scoped clippy here; the `--all-targets` sweep is Task 3.)

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/community_membership.rs src-tauri/tests/community_sync/community_open_flow_integration.rs
git commit -m "$(cat <<'EOF'
feat(community): bootstrap_admit_open_publisher helper for open-join

Pure helper that validates an open-community publisher's in-blob self-Join
(reusing verify_event + materialize_with_now) and returns the MemberState
with seeded enrolled keys, or None. Unit-tested for admit/reject paths.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
EOF
)"
```

---

## Task 2: Deferred bootstrap-admission in the gate + two-node wire-convergence test

**Files:**
- Test: `src-tauri/tests/community_sync/community_open_flow_integration.rs` — add `open_community_two_node_wire_convergence_no_preseed`.
- Modify: `src-tauri/src/community_state_sync.rs` — `handle_incoming_publish` (gate ~3127, sig-verify ~3178, post-decode ~3273, TOCTOU ~3308).

**Interfaces:**
- Consumes: `bootstrap_admit_open_publisher` (Task 1); existing `verify_publisher_sig(&CommunityRootPublishPayload, &MemberState)` (community_state_sync.rs:2964); `ctx.is_invite_only` (InternalCtx:1645); `ctx.community_id`; `ctx.admin_addr`; `payload.publisher_addr`; `payload.at`; the decoded+sorted `resolved: Vec<SignedMembershipEvent>` (~3269).
- Produces: open-community publishes from unknown-but-self-Join-carrying publishers are admitted + merged; invite-only and known-but-not-Joined paths unchanged.

- [ ] **Step 1: Write the failing convergence test**

Append to `src-tauri/tests/community_sync/community_open_flow_integration.rs`. This mirrors the setup of `open_community_create_redeem_leave_round_trip` (lines 71–231) but **omits the two manual cross-seed `insert_local_event` calls** (the ones the existing test documents as "without this seed B would reject `publisher_not_joined`") and asserts convergence over the real wire:

```rust
/// ZEB-558: open-community join must converge over the WIRE with no manual
/// cross-seeding. This is the faithful repro of the production deadlock that
/// `open_community_create_redeem_leave_round_trip` masks with its explicit
/// `insert_local_event` pre-seeds. Pre-fix this DEADLOCKS (each engine
/// rejects the other's publish `publisher_not_joined`); post-fix the gate's
/// deferred bootstrap-admission converges both.
#[tokio::test]
async fn open_community_two_node_wire_convergence_no_preseed() {
    let owner_a_test = harmony_app::community_membership::mint_test_owner(0x5A);
    let owner_b_test = harmony_app::community_membership::mint_test_owner(0x5B);
    let owner_a = owner_a_test.owner;
    let owner_b = owner_b_test.owner;
    let signing_a = owner_a_test.device_key.clone();
    let signing_b = owner_b_test.device_key.clone();

    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        a: (owner_a, [0u8; 64]),
        b: (owner_b, [0u8; 64]),
    });

    // Shared in-memory CAS (verbatim from the round-trip test).
    let cas: Arc<Mutex<HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(64);
    let cas_for_servicer = Arc::clone(&cas);
    tokio::spawn(async move {
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal { cid, blob, reply, .. } => {
                    cas_for_servicer.lock().await.insert(cid, blob);
                    if let Some(r) = reply { let _ = r.send(Ok(())); }
                }
                CasOp::GetOrFetch { cid, timeout: _, reply } => {
                    let v = cas_for_servicer.lock().await.get(&cid).cloned();
                    let _ = reply.send(Ok(v));
                }
                CasOp::GetLocal { cid, reply } => {
                    let v = cas_for_servicer.lock().await.get(&cid).cloned();
                    let _ = reply.send(v);
                }
                CasOp::AllowServeSubtree { reply, .. } => { let _ = reply.send(Ok(0)); }
            }
        }
    });

    // Bidirectional wire forwarders (verbatim).
    let (a_out_tx, mut a_out_rx) = mpsc::channel::<Vec<u8>>(64);
    let (a_in_tx, a_in_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_out_tx, mut b_out_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_in_tx, b_in_rx) = mpsc::channel::<Vec<u8>>(64);
    let a_in_for_fwd = a_in_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = b_out_rx.recv().await { let _ = a_in_for_fwd.send(bytes).await; }
    });
    let b_in_for_fwd = b_in_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = a_out_rx.recv().await { let _ = b_in_for_fwd.send(bytes).await; }
    });

    let minted_a = mint_community_creation(
        "WireConvergence", false, owner_a, &signing_a, &owner_a_test.cert,
        Hlc { wall_ms: 100_000, logical: 0, device_id: "a-dev".to_string() },
    ).expect("mint create");
    let community_id = minted_a.community_id;

    let cs_a: Arc<dyn ContentStore> =
        Arc::new(RuntimeContentStore::new(cas_op_tx.clone(), Duration::from_secs(2)));
    let cs_b: Arc<dyn ContentStore> =
        Arc::new(RuntimeContentStore::new(cas_op_tx.clone(), Duration::from_secs(2)));

    let state_a = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let state_b = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker_a = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let tracker_b = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let (delta_a_tx, mut _delta_a_rx) = mpsc::channel::<CommunityMembershipDelta>(32);
    let (delta_b_tx, mut _delta_b_rx) = mpsc::channel::<CommunityMembershipDelta>(32);
    let tmp_a = tempfile::tempdir().expect("tmp a");
    let tmp_b = tempfile::tempdir().expect("tmp b");

    let engine_a = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a,
        is_invite_only: false,
        device_id: "a-dev".into(),
        self_owner: owner_a,
        signing_key: Arc::new(signing_a.clone()),
        state: Arc::clone(&state_a),
        tracker: Arc::clone(&tracker_a),
        content_store: cs_a,
        publisher_tx: a_out_tx,
        subscriber_rx: a_in_rx,
        paths: PersistPaths {
            crdt: tmp_a.path().join("crdt.cbor"),
            replay: tmp_a.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(Arc::clone(&resolver)),
        error_tx: None,
        delta_tx: Some(delta_a_tx),
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });
    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a,
        is_invite_only: false,
        device_id: "b-dev".into(),
        self_owner: owner_b,
        signing_key: Arc::new(signing_b.clone()),
        state: Arc::clone(&state_b),
        tracker: Arc::clone(&tracker_b),
        content_store: cs_b,
        publisher_tx: b_out_tx,
        subscriber_rx: b_in_rx,
        paths: PersistPaths {
            crdt: tmp_b.path().join("crdt.cbor"),
            replay: tmp_b.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(Arc::clone(&resolver)),
        error_tx: None,
        delta_tx: Some(delta_b_tx),
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    // A inserts its bootstrap Join → publishes over the wire.
    engine_a.insert_local_event(minted_a.bootstrap_join.clone())
        .await.expect("A bootstrap insert");

    // B redeems the open invite + inserts ONLY its own Join → publishes.
    // NO manual cross-seed of A's Join into B, and NO seed of B's Join into A.
    let invite_payload = harmony_app::community_invite::CommunityInvitePayload {
        community_id,
        epoch_snapshot: harmony_app::community_invite::InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: minted_a.membership_key.as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: harmony_app::community_invite::MaterializedCommunityState::default(),
        },
        admin_addr: owner_a,
        community_name: "WireConvergence".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    };
    let minted_b = mint_redemption(
        &invite_payload, owner_b, &signing_b, &owner_b_test.cert,
        Hlc { wall_ms: 200_000, logical: 0, device_id: "b-dev".to_string() },
    ).expect("mint redeem");
    engine_b.insert_local_event(minted_b.bootstrap_join.clone())
        .await.expect("B redemption insert");

    // Convergence over the WIRE: each engine must learn the other's Join with
    // no pre-seed. Pre-fix this times out (mutual publisher_not_joined reject).
    let a_has_both = wait_until(
        || async { state_a.lock().await.events.len() == 2 },
        Duration::from_secs(10),
    ).await;
    let b_has_both = wait_until(
        || async { state_b.lock().await.events.len() == 2 },
        Duration::from_secs(10),
    ).await;
    assert!(a_has_both, "A must learn B's Join over the wire (no pre-seed)");
    assert!(b_has_both, "B must learn A's Join over the wire (no pre-seed)");

    // Both materialize the same 2-member roster.
    let dto_a = member_info_for(&state_a.lock().await.materialize_now(owner_a));
    let dto_b = member_info_for(&state_b.lock().await.materialize_now(owner_a));
    assert_eq!(dto_a.len(), 2, "A roster = {{admin, joiner}}");
    assert_eq!(dto_b.len(), 2, "B roster = {{admin, joiner}}");
    assert_eq!(dto_a, dto_b, "rosters must agree");

    engine_a.shutdown().await.expect("shutdown a");
    engine_b.shutdown().await.expect("shutdown b");
}
```

- [ ] **Step 2: Run the test to verify it FAILS (proves the deadlock)**

```bash
cd src-tauri
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(open_community_two_node_wire_convergence_no_preseed)' 2>&1 | tail -25
```

Expected: **FAIL** — `wait_until` times out, panic `A must learn B's Join over the wire (no pre-seed)` (and/or the B assertion). This confirms the deadlock is real and the test has teeth.

- [ ] **Step 3: Edit the step-2 membership gate to defer for open + unknown**

In `src-tauri/src/community_state_sync.rs`, replace the step-2 gate block (the `let publisher_member_state: ... = { ... };` spanning ~3127–3159) with a version that yields an `Option` + a deferral flag:

```rust
    // ZEB-558: the gate now yields (publisher_member_state, deferred_open_bootstrap).
    // For an OPEN community + entirely-unknown publisher we DEFER the reject:
    // the publisher's self-Join lives only inside the (not-yet-fetched) blob,
    // so we validate it post-decode via `bootstrap_admit_open_publisher`.
    let (publisher_member_state, deferred_open_bootstrap): (
        Option<crate::community_membership::MemberState>,
        bool,
    ) = {
        let state = ctx.state.lock().await;
        let events: Vec<SignedMembershipEvent> = state.events.values().cloned().collect();
        drop(state);
        let materialized =
            crate::community_membership::prior_state_at_hlc(&events, &payload.at, ctx.admin_addr);
        let member_state = materialized.members.get(&payload.publisher_addr).cloned();
        let status_now = member_state.as_ref().map(|s| s.status);
        if matches!(status_now, Some(MemberStatus::Joined)) {
            (member_state, false)
        } else if !ctx.is_invite_only && member_state.is_none() {
            // OPEN + entirely-unknown publisher → defer. We do NOT run the
            // prior-state publisher-sig check below (no enrolled keys yet);
            // `bootstrap_admit_open_publisher` (post-decode) supplies them.
            (None, true)
        } else {
            // invite-only, OR known-but-Left/Banned → strict reject (unchanged).
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::PublisherNotJoined {
                addr: payload.publisher_addr,
                status: status_now.unwrap_or(MemberStatus::Left),
                left_at: member_state.and_then(|s| s.left_at),
            });
        }
    };
```

- [ ] **Step 4: Skip the early sig-verify for the deferred case**

Replace the existing sig-verify call (~3178):

```rust
    if let Err(e) = verify_publisher_sig(&payload, &publisher_member_state) {
        return IncomingOutcome::ErrPreMutation(e);
    }
```

with:

```rust
    // ZEB-558: for the deferred open-bootstrap case we have no enrolled keys
    // yet — the publisher-sig check runs post-decode against keys derived from
    // the in-blob self-Join. For all other (known-Joined) publishers, verify
    // now against their materialized enrolled keys exactly as before.
    if !deferred_open_bootstrap {
        let pms = publisher_member_state
            .as_ref()
            .expect("non-deferred publisher ⇒ Some(member_state)");
        if let Err(e) = verify_publisher_sig(&payload, pms) {
            return IncomingOutcome::ErrPreMutation(e);
        }
    }
```

- [ ] **Step 5: Run the deferred bootstrap-admission after decode + sort**

In `src-tauri/src/community_state_sync.rs`, immediately after the `resolved.sort_by(...)` block (~3273, after `resolved` is built) and BEFORE the `// Phase B` state lock (~3292), insert:

```rust
    // ZEB-558: deferred open-bootstrap admission. The publisher was unknown at
    // the gate; validate the open self-Join they carry in this blob (cert +
    // signer key + open-Join rule, via `bootstrap_admit_open_publisher`),
    // derive their enrolled keys, and verify the root publisher_sig against
    // them. The authoritative merge below re-validates and inserts the Join.
    if deferred_open_bootstrap {
        match crate::community_membership::bootstrap_admit_open_publisher(
            &resolved,
            payload.publisher_addr,
            ctx.admin_addr,
            ctx.community_id,
        ) {
            Some(bootstrap_member_state) => {
                if let Err(e) = verify_publisher_sig(&payload, &bootstrap_member_state) {
                    return IncomingOutcome::ErrPreMutation(e);
                }
            }
            None => {
                // No signature-valid open self-Join for the publisher in this
                // blob → the publish is unauthorized; reject as before.
                return IncomingOutcome::ErrPreMutation(CommunitySyncError::PublisherNotJoined {
                    addr: payload.publisher_addr,
                    status: MemberStatus::Left,
                    left_at: None,
                });
            }
        }
    }
```

- [ ] **Step 6: Make the TOCTOU re-check skip its assert for the deferred case**

In the Phase B block, wrap the existing TOCTOU re-check (the inner `{ let events_now ... }` block spanning ~3308–3330) so it only runs when NOT deferring. Replace the block's opening so it reads:

```rust
        // 12. TOCTOU re-check (unchanged rationale). Skipped for the ZEB-558
        //     deferred open-bootstrap case: the publisher is being admitted by
        //     THIS merge (their self-Join is in `resolved`), and an entirely-
        //     unknown publisher cannot have a concurrent local Leave/Kick, so
        //     there is no prior local membership for a race to invalidate.
        if !deferred_open_bootstrap {
            let events_now: Vec<SignedMembershipEvent> = state.events.values().cloned().collect();
            let mat_now = crate::community_membership::prior_state_at_hlc(
                &events_now,
                &payload.at,
                ctx.admin_addr,
            );
            let pub_state = mat_now.members.get(&payload.publisher_addr).cloned();
            let pub_status = pub_state.as_ref().map(|s| s.status);
            if !matches!(pub_status, Some(MemberStatus::Joined)) {
                drop(state);
                return IncomingOutcome::ErrPreMutation(CommunitySyncError::PublisherNotJoined {
                    addr: payload.publisher_addr,
                    status: pub_status.unwrap_or(MemberStatus::Left),
                    left_at: pub_state.and_then(|s| s.left_at),
                });
            }
        }
```

(Preserve the rest of the Phase B loop verbatim. Note `publisher_member_state` is no longer referenced after Step 4, so the `Option` is fine — if clippy flags it unused in the deferred path, the `.expect(...)` in Step 4 keeps the non-deferred read.)

- [ ] **Step 7: Run the convergence test — must now PASS**

```bash
cd src-tauri
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(open_community_two_node_wire_convergence_no_preseed)' 2>&1 | tail -20
```

Expected: PASS.

- [ ] **Step 8: Regression — existing open-flow + invite-only paths untouched**

```bash
cd src-tauri
cargo nextest run --locked -p harmony-app --features test-fixtures \
  -E 'test(open_community_create_redeem_leave_round_trip) + test(redeem_invite_twice_does_not_corrupt_state) + test(alice_redeems_invite_only_against_bob_admin)' 2>&1 | tail -20
```

Expected: all PASS (the pre-seeded open round-trip still works; invite-only is unchanged because `deferred_open_bootstrap` is only ever `true` for `!is_invite_only`).

- [ ] **Step 9: Scoped gates**

```bash
cd src-tauri
cargo fmt --all -- --check; echo "fmt=$?"
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5; echo "clippy=${pipestatus[1]}"
```

Expected: `fmt=0`, `clippy=0`.

- [ ] **Step 10: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/community_state_sync.rs src-tauri/tests/community_sync/community_open_flow_integration.rs
git commit -m "$(cat <<'EOF'
fix(community): converge open-community join via gate self-Join admission

handle_incoming_publish now defers the publisher-not-joined reject for an
open community + entirely-unknown publisher, validates the open self-Join
carried in the decrypted blob, seeds enrolled keys, and verifies the root
publisher_sig against them before admitting. Fixes the mutual-rejection
deadlock that left open joins permanently "joined but empty". Invite-only and
known-member paths unchanged. Adds a no-preseed two-node wire-convergence test.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
EOF
)"
```

---

## Task 3: Full-sweep gates + frontend sanity

**Files:** none modified unless a sweep surfaces a fixup.

- [ ] **Step 1: Full Rust clippy + test sweep (`--all-targets`)**

```bash
cd src-tauri
cargo fmt --all -- --check; echo "fmt=$?"
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -8; echo "clippy=${pipestatus[1]}"
cargo nextest run --locked -p harmony-app --features test-fixtures 2>&1 | tail -12; echo "test=${pipestatus[1]}"
```

Expected: `fmt=0`, `clippy=0`, `test=0`. This relinks the integration binaries (~25 min) — run once here, not per task. If `XprotectService` makes it hang on macOS, confirm `spctl developer-mode enable-terminal` per `CLAUDE.md`.

- [ ] **Step 2: Frontend sanity (no frontend change expected; confirm no accidental coupling)**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit 2>&1 | tail -5; echo "tsc=$?"
npx vitest run 2>&1 | tail -8
```

Expected: tsc clean; vitest all pass (unchanged — this fix is backend-only).

- [ ] **Step 3: Commit any fmt/clippy fixups (only if the sweep produced changes)**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git status --short
# If and only if the sweep changed files:
git add -A && git commit -m "$(cat <<'EOF'
chore(community): fmt/clippy fixups from full-targets sweep

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
EOF
)"
```

If `git status` is clean, skip the commit — Task 3 is verification-only in that case.

---

## Self-Review

**1. Spec coverage:**
- Deferred bootstrap-admission branch (open + unknown) → Task 2 Steps 3–6. ✓
- `bootstrap_admit_open_publisher` helper → Task 1. ✓
- Reuse `enrolled_key_from_cert` / `verify_publisher_sig` / `prior_state_at_hlc` / `verify_event` / `materialize_with_now` → Task 1 (verify_event + materialize) + Task 2 (verify_publisher_sig + prior_state_at_hlc). ✓
- Strict scoping `!is_invite_only` AND unknown-publisher → Task 2 Step 3 gate predicate; helper hardcodes open ctx. ✓
- Two-node convergence test, must fail pre-fix → Task 2 Steps 1–2 (red) → Step 7 (green). ✓
- Unit tests incl. invite-only-never-admits regression → Task 1 (admit/reject unit tests) + Task 2 Step 8 (invite-only round-trip unchanged; the gate predicate guarantees invite-only never sets the deferral flag). ✓
- TOCTOU re-check handled → Task 2 Step 6. ✓
- All gates (fmt / clippy --all-targets / nextest / tsc / vitest) → Task 3. ✓
- ZEB IDs out of branch/commit/PR → Global Constraints + commit messages use no `ZEB-558`/magic words. ✓

**2. Placeholder scan:** No TBD/TODO; every code step has complete code; the integration-test setup is given in full (not "similar to"). ✓

**3. Type consistency:** `bootstrap_admit_open_publisher(&[SignedMembershipEvent], OwnerAddr, OwnerAddr, SpaceId) -> Option<MemberState>` — same signature in Task 1 (definition) and Task 2 Step 5 (call). `deferred_open_bootstrap: bool` introduced in Task 2 Step 3, consumed in Steps 4/5/6. `verify_publisher_sig(&payload, &MemberState)` matches the existing fn (community_state_sync.rs:2964). `VerifyContext { expected_community_id, admin_addr, is_invite_only }` matches the in-tree shape (community_state_sync.rs:3357). ✓

**Note for the implementer:** line numbers are anchors from `main` @ `2a5b65e6` + the spec commit; if a region has drifted, locate it by the quoted surrounding code (`grep`), not the bare line number. `handle_incoming_publish` is long — confirm each edit lands in the intended step by matching the adjacent comments quoted above.
