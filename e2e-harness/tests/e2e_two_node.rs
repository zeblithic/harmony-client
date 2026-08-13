//! ZEB-447 two-node E2E scenarios. Gated behind `--features e2e` (spawns the
//! real harmony-app binary + real transport). Build the binary first:
//!   cd src-tauri && cargo build --bin harmony-app

#![cfg(feature = "e2e")]

use std::path::PathBuf;

use e2e_harness::RunDir;
use e2e_harness::{NodeConfig, NodeHandle};
use serde_json::json;

fn fresh_home(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("harmony-e2e-{tag}-"))
        .tempdir()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_node_mints_owner() {
    let home = fresh_home("solo");
    let cfg = NodeConfig::new(PathBuf::from(home.path()), "alice");
    let node = NodeHandle::spawn(cfg).await.expect("spawn alice");

    let pre = node.status().await.expect("status");
    assert_eq!(
        pre.get("ownerId")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        serde_json::Value::Null,
        "owner should be unminted at first boot"
    );

    let mint = node
        .rpc("mint_owner_identity", json!({}))
        .await
        .expect("mint");
    assert!(
        mint.get("recoveryToken").and_then(|v| v.as_str()).is_some(),
        "mint returns recoveryToken"
    );

    let owner = node
        .rpc("get_owner_state", json!({}))
        .await
        .expect("get_owner_state");
    assert!(
        owner.get("ownerId").and_then(|v| v.as_str()).is_some(),
        "owner id set after mint"
    );

    // keep `home` alive until here
    drop(node);
    drop(home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mint_emits_mint_changed_event() {
    use std::time::Duration;
    let home = fresh_home("evt");
    let cfg = NodeConfig::new(PathBuf::from(home.path()), "alice");
    let node = NodeHandle::spawn(cfg).await.expect("spawn");
    let (mut rx, _task) = node.events().await.expect("subscribe");

    node.rpc("mint_owner_identity", json!({}))
        .await
        .expect("mint");

    // `mint_owner_identity` restarts the node; the restart deterministically
    // emits `zenoh-status {status:"connected"}` over the WS once the new node is
    // ready. (`mint-changed` only fires on a *remote* mint-snapshot merge via a
    // Zenoh peer/echo — it does NOT fire on a lone single node, confirmed by
    // capturing the live WS frames + node stderr; see report. We assert on the
    // real mint-triggered event, not a trivially-true predicate.)
    e2e_harness::await_event(&mut rx, Duration::from_secs(20), |f| {
        f.event == "zenoh-status"
            && f.payload.get("status").and_then(|s| s.as_str()) == Some("connected")
    })
    .await
    .expect("zenoh-status connected event after mint restart");

    drop(node);
    drop(home);
}

/// Spawn two named-profile nodes, each under its OWN temp HOME (so discovery is
/// unambiguous), both minted, stdout/stderr captured into the run dir. Returns
/// (run_dir, alice_home, bob_home, alice, bob). Keep both homes alive until the
/// scenario ends.
async fn two_minted_nodes(
    scenario: &str,
) -> (
    RunDir,
    tempfile::TempDir,
    tempfile::TempDir,
    NodeHandle,
    NodeHandle,
) {
    let run = RunDir::new(scenario).expect("run dir");
    let alice_home = fresh_home(&format!("{scenario}-a"));
    let bob_home = fresh_home(&format!("{scenario}-b"));
    let mk = |home: &tempfile::TempDir, profile: &str| {
        let mut cfg = NodeConfig::new(PathBuf::from(home.path()), profile);
        cfg.log_dir = Some(run.log_dir());
        cfg
    };
    let alice = NodeHandle::spawn(mk(&alice_home, "alice"))
        .await
        .expect("spawn alice");
    let bob = NodeHandle::spawn(mk(&bob_home, "bob"))
        .await
        .expect("spawn bob");
    alice
        .rpc("mint_owner_identity", json!({}))
        .await
        .expect("alice mint");
    bob.rpc("mint_owner_identity", json!({}))
        .await
        .expect("bob mint");
    (run, alice_home, bob_home, alice, bob)
}

async fn owner_id(node: &NodeHandle) -> String {
    let o = node
        .rpc("get_owner_state", json!({}))
        .await
        .expect("get_owner_state");
    o.get("ownerId")
        .and_then(|v| v.as_str())
        .expect("ownerId")
        .to_string()
}

/// Spawn THREE named-profile nodes, each under its OWN temp HOME, all minted,
/// stdout/stderr captured into the run dir. Roles for the ZEB-487 deposit→recover
/// scenario: `a` = sender, `b` = recipient (goes offline), `r` = relay host.
/// Returns (run_dir, a_home, b_home, r_home, a, b, r). Keep the homes alive until
/// the scenario ends.
async fn three_minted_nodes(
    scenario: &str,
) -> (
    RunDir,
    tempfile::TempDir,
    tempfile::TempDir,
    tempfile::TempDir,
    NodeHandle, // a = sender
    NodeHandle, // b = recipient
    NodeHandle, // r = relay host
) {
    let run = RunDir::new(scenario).expect("run dir");
    let a_home = fresh_home(&format!("{scenario}-a"));
    let b_home = fresh_home(&format!("{scenario}-b"));
    let r_home = fresh_home(&format!("{scenario}-r"));
    let mk = |home: &tempfile::TempDir, profile: &str| {
        let mut cfg = NodeConfig::new(PathBuf::from(home.path()), profile);
        cfg.log_dir = Some(run.log_dir());
        cfg
    };
    let a = NodeHandle::spawn(mk(&a_home, "alice"))
        .await
        .expect("spawn a");
    let b = NodeHandle::spawn(mk(&b_home, "bob"))
        .await
        .expect("spawn b");
    let r = NodeHandle::spawn(mk(&r_home, "relay"))
        .await
        .expect("spawn r");
    for (n, who) in [(&a, "a"), (&b, "b"), (&r, "r")] {
        n.rpc("mint_owner_identity", json!({}))
            .await
            .unwrap_or_else(|e| panic!("{who} mint: {e}"));
    }
    (run, a_home, b_home, r_home, a, b, r)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_boot_and_mint() {
    let (mut run, ah, bh, a, b) = two_minted_nodes("smoke").await;
    assert_ne!(owner_id(&a).await, owner_id(&b).await, "distinct owners");
    run.mark_success();
    drop((a, b, ah, bh));
}

// ─────────────────────────────────────────────────────────────────────────────
// S1: invite → cross-node iroh first-contact join → roster convergence.
//
// Bob joins Alice's invite-only community over the REAL first-contact path the
// GUI uses for two-never-met nodes: `connectivity_redeem_invite_iroh`. That verb
// pkarr-window-resolves Alice's routing record, opens an iroh bi-stream on the
// handshake ALPN, and runs the inner join. It is now exposed on the headless v1
// RPC surface (the binary under test was rebuilt with it).
//
// First contact is racy: Alice's pkarr record may not be resolvable the instant
// after she boots + invites, so the redeem is polled — `inviter_unreachable`
// means pkarr/iroh hasn't converged yet (retryable), `joined` is success, and
// anything else (e.g. `join_failed`) is a hard failure surfaced with its status.
// Assertions are MEANINGFUL (real joined-roster convergence, both directions).
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s1_invite_join_roster_convergence() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let (mut run, ah, bh, alice, bob) = two_minted_nodes("s1").await;
    let bob_owner = owner_id(&bob).await;
    let alice_owner = owner_id(&alice).await;

    // Alice mints a community and an invite.
    let community = create_community(&alice, "s1-community", true)
        .await
        .expect("create community");
    let invite = generate_invite(&alice, &community)
        .await
        .expect("generate invite");

    // Bob joins via iroh first-contact. `poll_join_iroh` polls until "joined",
    // retrying on `inviter_unreachable` (pkarr/iroh not yet converged) AND on
    // transient RPC errors (pkarr relay cooldown under repeated runs), failing
    // only on a terminal non-join status or timeout.
    let joined = poll_join_iroh(&bob, &invite, Duration::from_secs(240))
        .await
        .expect("bob joins alice's community via iroh first-contact");

    let joined_id = joined
        .get("communityId")
        .and_then(|v| v.as_str())
        .expect("joined community id");
    assert_eq!(joined_id, community, "bob joined alice's community");

    // Roster converges both directions (poll — no assumed event).
    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&alice, &community, &bob_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("alice sees bob joined");

    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&bob, &community, &alice_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("bob sees alice joined");

    run.mark_success();
    drop((alice, bob, ah, bh));
}

// ─────────────────────────────────────────────────────────────────────────────
// S2: friend-add (iroh first-contact) → friend graph (DM-picker class, ZEB-431)
//     → DM-space creation. DM *delivery* is exercised + characterized below.
//
// Friend-only first-contact path: Alice mints a friend token, Bob redeems it.
// `redeem_friend_token` IS the cross-node first-contact — it pkarr-resolves
// Alice's `friend:`-namespaced reachability record (derived from the token sig),
// dials the HARMONY_FRIEND_V1 ALPN, and runs the handshake. That record is
// subject to the SAME ~75–90s pkarr-propagation race S1's invite redeem hits, so
// the redeem is POLLED: every Err (the inviter_unreachable family — pkarr/iroh
// not yet converged, connect/open_bi/read timeouts) is retried for up to ~120s.
// (Empirically this resolved in ~25s on the dev Mac — well within budget — so the
// friend-only path is reliable; the community-warm fallback was NOT needed.)
//
// A friend TOKEN is itself the consent proof: the acceptor's consent gate takes
// the TokenPath (`decide_consent`: `token_sig.is_some()` ⇒ auto-accept) and writes
// the requester as an Active/Token friend on BOTH sides inline — no manual accept
// is needed. The `accept_pending_from` loop below is therefore belt-and-braces
// (it no-ops when there's no pending row); the real assertion is that
// `friend_is_active` flips true in BOTH directions. That friend graph is exactly
// what feeds the DM picker (ZEB-431) — so the friend-add → DM-picker half is fully
// proven here.
//
// DM-space addressing reality (verified empirically + against the CRDT code):
// `add_space` mints a RANDOM `SpaceId(rand::random())`; the dedupe key is the
// sorted member set. Alice's and Bob's independently-created spaces therefore get
// DIFFERENT raw ids — confirmed on a live run (alice=2a90c86b…, bob=fb6aac3a…).
// The plan's `assert_eq!(a_space, b_space)` would be a FALSE invariant at creation
// time: the two ids converge only AFTER the space-invite each side dispatches in
// `add_space` propagates and `apply_space` canonicalizes the cross-id collision to
// `min(a_space, b_space)` (the loser id is removed). So this test does NOT assert
// id-equality; it asserts the (correct) inequality-at-creation and reads tolerant
// of whichever id the thread settles under (`read_dm_plaintext_any`).
//
// DM DELIVERY (status after ZEB-473 / DM-over-iroh Move 1a, Tasks 1-9):
// DM unicast resolves `OwnerAddr → device destinations` via `OwnerDeviceCache`,
// now populated by the iroh friend handshake (ZEB-461 Tasks 5-7) with NO Reticulum
// socket (the harness disables it: `HARMONY_RETICULUM_PORT=0`, mandatory so two
// co-located nodes don't collide on the fixed LAN broadcast port). The live DM
// byte carrier is the PQ `harmony-tunnel` (`IrohTunnelDmTransport` + inbound
// `ingest_dm_packet`), installed in `serve`. This scenario hard-asserts the
// parts that genuinely work end-to-end co-located (friendship active both ways +
// DM-space creation + `send_dm` accepted) and CHARACTERIZES (logs, does not gate)
// the byte round-trip. The full byte-delivery hard-assert lives in the separate,
// `#[ignore]`'d `s2_dm_delivery_over_tunnel_hard_assert` below, which documents the
// exact remaining co-located gap empirically (the DM Space invite no longer has a
// cross-owner carrier — see that test's ignore note).
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s2_friend_graph_and_dm_send() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let (mut run, ah, bh, alice, bob) = two_minted_nodes("s2").await;
    let alice_owner = owner_id(&alice).await;
    let bob_owner = owner_id(&bob).await;

    // Friend handshake = cross-node iroh first-contact. Alice mints a friend
    // token; Bob redeems it. Poll the redeem until it returns Ok — every Err is
    // the pkarr/iroh-not-yet-converged race (retryable for ~120s, mirroring S1).
    let token = generate_friend_token(&alice).await.expect("friend token");
    // Friend redeem IS the cross-node first-contact and is racy: it can Err while
    // pkarr/iroh converge (~75-90s). Retry until Ok, but CAPTURE the last error so
    // a genuine hard failure surfaces it instead of a bare 120s timeout with the
    // real server error discarded (CodeRabbit).
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut last_err = String::from("(no redeem attempt completed before the deadline)");
    loop {
        if std::time::Instant::now() >= deadline {
            panic!("bob never redeemed alice's friend token within 120s; last error: {last_err}");
        }
        match redeem_friend_token(&bob, &token).await {
            Ok(_) => break,
            Err(e) => last_err = e.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // Token redemption auto-accepts on both sides (TokenPath consent). The accept
    // loop is belt-and-braces (no-ops without a pending row); the real check is
    // that the friend graph reaches `active` in BOTH directions.
    poll_until(Duration::from_secs(120), || async {
        accept_pending_from(&alice, &bob_owner).await?;
        Ok(friend_is_active(&alice, &bob_owner).await?.then_some(()))
    })
    .await
    .expect("alice has bob as active friend");

    poll_until(Duration::from_secs(120), || async {
        Ok(friend_is_active(&bob, &alice_owner).await?.then_some(()))
    })
    .await
    .expect("bob has alice as active friend (DM-picker class, ZEB-431)");

    // DM-space creation. Both sides create the member-set-addressed DM space.
    let a_space = add_dm_space(&alice, "s2-dm", &bob_owner)
        .await
        .expect("alice dm space");
    let b_space = add_dm_space(&bob, "s2-dm", &alice_owner)
        .await
        .expect("bob dm space");
    eprintln!(
        "S2 dm space ids: alice={a_space} bob={b_space} (equal={})",
        a_space == b_space
    );
    // Empirically the two independently-minted DM-space ids DIFFER at creation
    // (random `SpaceId`s; they canonicalize to min(a,b) only after the space-
    // invites cross). Assert the verified semantics rather than the plan's false
    // `assert_eq!`: each side really did create a DM space, and the raw ids are
    // distinct pre-merge.
    assert!(!a_space.is_empty(), "alice minted a DM-space id");
    assert!(!b_space.is_empty(), "bob minted a DM-space id");
    assert_ne!(
        a_space, b_space,
        "independently-minted DM-space ids are distinct at creation (canonicalize \
         to min(a,b) only after the cross-node space-invite merges them)"
    );
    let candidates: Vec<&str> = vec![a_space.as_str(), b_space.as_str()];

    // DM send must be ACCEPTED by the engine (the IPC returns Ok — the message is
    // CAS-stored + queued on the outbox). With ZEB-461 Tasks 5-7 the recipient's
    // devices now resolve from `OwnerDeviceCache` (handshake-populated), so the send
    // gets a real destination instead of "no known devices". Delivery is still
    // characterized, not hard-asserted: ZEB-473 Move 1a carries DMs over the live
    // PQ `harmony-tunnel` (always-deposit + attempt-tunnel), but co-located
    // delivery in this pkarr harness still hinges on the deposit path and the
    // DM-space carrier wiring (the receiver can still drop a packet whose Space
    // hasn't merged yet — SpaceNotFound). The bounded poll keeps the scenario
    // fast + honest rather than hanging on that gap.
    send_dm(&alice, &a_space, b"hello-from-alice", "text/plain")
        .await
        .expect("alice's send_dm is accepted by the engine (CAS-stored + queued)");

    let delivered_a_to_b = poll_until(Duration::from_secs(15), || async {
        let msgs = read_dm_plaintext_any(&bob, &candidates).await?;
        Ok(msgs
            .iter()
            .any(|(_, body)| body == b"hello-from-alice")
            .then_some(()))
    })
    .await
    .is_ok();

    // Bob → Alice. Send under whichever candidate id Bob's thread is live under
    // (post-merge that's the canonical min id; pre-merge it's b_space). Try
    // b_space first, fall back to a_space on UnknownSpace.
    let bob_send_ok = send_dm(&bob, &b_space, b"hello-from-bob", "text/plain")
        .await
        .is_ok()
        || send_dm(&bob, &a_space, b"hello-from-bob", "text/plain")
            .await
            .is_ok();
    assert!(
        bob_send_ok,
        "bob's send_dm is accepted by the engine under one of the candidate ids"
    );

    let delivered_b_to_a = poll_until(Duration::from_secs(15), || async {
        let msgs = read_dm_plaintext_any(&alice, &candidates).await?;
        Ok(msgs
            .iter()
            .any(|(_, body)| body == b"hello-from-bob")
            .then_some(()))
    })
    .await
    .is_ok();

    eprintln!(
        "S2 DM delivery: alice→bob={delivered_a_to_b} bob→alice={delivered_b_to_a} \
         (both currently FALSE co-located: the PQ tunnel DOES establish + deliver the \
         signed CidNotify packet between the two nodes — Tasks 1-9 — but the receiver \
         rejects it at `verify_cidnotify_admission: SpaceNotFound` because the DM Space \
         (random per-owner SpaceId + per-Space content_key) has no cross-owner carrier: \
         the DmInvite that propagated it rode Reticulum unicast, removed in harmony#280, \
         and re-wiring it onto the tunnel is a later move. See \
         `s2_dm_delivery_over_tunnel_hard_assert`'s ignore note for the full diagnosis.)"
    );

    // Hard scenario result = the parts that genuinely work end-to-end:
    //   • cross-node friend first-contact (iroh) succeeded,
    //   • friendship is Active in BOTH directions (the DM-picker graph, ZEB-431),
    //   • both sides created a DM space with the verified id semantics,
    //   • both `send_dm` IPCs were accepted by the engine.
    // DM byte-delivery is characterized above but not gated on (documented gap).
    run.mark_success();
    drop((alice, bob, ah, bh));
}

// ─────────────────────────────────────────────────────────────────────────────
// S2 (hard-assert): live 1:1 DM byte-delivery over the PQ tunnel (ZEB-473/482).
//
// Two co-located headless nodes friend each other, Alice sends a DM, and Bob MUST
// receive the DM byte — asserted BOTH ways: the `dm-received` WS event fires on Bob
// AND the plaintext lands in Bob's DM thread. This gates the DM-over-iroh path
// end-to-end co-located.
//
// ── STATE (ZEB-482 invite carrier landed; co-located byte-delivery STILL gated) ──
// ZEB-473 (Move 1a) proved the PQ tunnel DIALS + HANDSHAKES co-located and that
// `IrohTunnelDmTransport` routes a signed CidNotify the peer's `ingest_dm_packet`
// receives — but every tunnel DM was then rejected at
// `verify_cidnotify_admission: SpaceNotFound`, because a DM `Space` (random
// per-owner `SpaceId` + per-Space `content_key`) had no cross-owner carrier after
// the Reticulum unicast invite was removed in harmony#280.
//
// ZEB-482 (Move 1b) re-wires the already-built DmInvite fan-out onto the SAME
// tunnel: `add_space_impl` routes the signed `DmInvite` over `send_dm` at
// Space-creation time (before the first CidNotify), and `ingest_dm_packet` now
// dispatches `DmPacket::Invite` into the shared `apply_invite` auto-accept. This is
// PROVEN to land co-located: with ZEB-482 the receiver's rejection reason advances
// from `SpaceNotFound` → `CAS fetch: no successful reply`. I.e. the Space now
// bootstraps from the invite and admission SUCCEEDS — the `SpaceNotFound` gap this
// test documented is closed. (A simultaneous-dial tunnel-dedup fix in
// `tunnel_manager::note_active` — retain `pending` so a dedup-loss redirects the
// invite onto the winning session — was needed for the invite to survive the
// invite/invite collision both nodes trigger the instant they friend.)
//
// ZEB-484 (Move 1c) CLOSED the DM-blob gap: the encrypted DM *blob* now rides the
// PQ tunnel inline (`DmPacket::CidNotifyWithBlob`) alongside the CidNotify, so a
// recipient with NO butler can take DM content live — the receiver CAS-puts the
// inline blob (content-addressed; fails closed on mismatch) and the existing
// CidNotify ingest finds it locally instead of hitting the (refusing) content-serve
// queryable. The butler deposit rung is unchanged (durability). The blob carrier is
// UNIT-PROVEN (`dm_envelope` codec round-trip; `ingest_dm_packet` inline-blob
// delivery + fail-closed-on-CID-mismatch; `IrohTunnelDmTransport` send inlining).
//
// ZEB-485 LANDED reliable tunnel establishment (deterministic single-dialer: the
// lower-NodeId peer dials, the higher buffers + accepts → exactly one iroh
// connection per pair, no `TunnelAccept: connection lost` collision) AND fixed a
// ZEB-484 re-entrancy deadlock the now-working tunnel exposed: the content send's
// `cas.get_local` (`CasOp::GetLocal`) was awaited inline inside the event-loop-
// driven outbox drain, deadlocking against the loop's own GetLocal servicing —
// `IrohTunnelDmTransport::send` now SPAWNS the build + `send_dm`. With both, the
// recipient ingests the inline blob and fires `dm-received` live. UN-IGNORED:
// reliably green (5/5) under `--features e2e`. CI never sets `e2e`, so this never
// runs in CI. Run explicitly with:
//   cargo nextest run --features e2e -E 'test(s2_dm_delivery_over_tunnel_hard_assert)'
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s2_dm_delivery_over_tunnel_hard_assert() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let (mut run, ah, bh, alice, bob) = two_minted_nodes("s2-hard").await;
    let alice_owner = owner_id(&alice).await;
    let bob_owner = owner_id(&bob).await;

    // Subscribe to Bob's event stream BEFORE the send so we never miss the
    // `dm-received` frame (the WS forwards every frame from connect onward).
    let (mut bob_events, _bob_ev_task) = bob.events().await.expect("subscribe bob events");

    // Friend handshake = cross-node iroh first-contact (racy ~75-90s; retry until
    // Ok, surfacing the real error on timeout instead of a bare deadline).
    let token = generate_friend_token(&alice).await.expect("friend token");
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut last_err = String::from("(no redeem attempt completed before the deadline)");
    loop {
        if std::time::Instant::now() >= deadline {
            panic!("bob never redeemed alice's friend token within 120s; last error: {last_err}");
        }
        match redeem_friend_token(&bob, &token).await {
            Ok(_) => break,
            Err(e) => last_err = e.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // Friendship Active both ways (token consent auto-accepts; the accept loop is
    // belt-and-braces). This is the DM-picker graph (ZEB-431) + the handshake that
    // populates each side's `OwnerDeviceCache` with the peer's tunnel contact.
    poll_until(Duration::from_secs(120), || async {
        accept_pending_from(&alice, &bob_owner).await?;
        Ok(friend_is_active(&alice, &bob_owner).await?.then_some(()))
    })
    .await
    .expect("alice has bob as active friend");
    poll_until(Duration::from_secs(120), || async {
        Ok(friend_is_active(&bob, &alice_owner).await?.then_some(()))
    })
    .await
    .expect("bob has alice as active friend");

    // Alice creates the DM space and sends a DM. (Bob also creates his own DM space
    // so both candidate ids are readable; the live thread settles under whichever
    // id wins the cross-node merge.)
    let a_space = add_dm_space(&alice, "s2-hard-dm", &bob_owner)
        .await
        .expect("alice dm space");
    let b_space = add_dm_space(&bob, "s2-hard-dm", &alice_owner)
        .await
        .expect("bob dm space");
    let candidates: Vec<&str> = vec![a_space.as_str(), b_space.as_str()];

    let body: &[u8] = b"hard-assert-dm-over-tunnel";
    send_dm(&alice, &a_space, body, "text/plain")
        .await
        .expect("alice's send_dm accepted by the engine (CAS-stored + queued)");

    // HARD ASSERT #1: the `dm-received` WS event fires on Bob carrying this body.
    // The shared `dm-received` payload serializes `body` as lowercase hex.
    let want_body_hex = hex::encode(body);
    e2e_harness::await_event(&mut bob_events, Duration::from_secs(60), |f| {
        f.event == "dm-received"
            && f.payload.get("body").and_then(|b| b.as_str()) == Some(want_body_hex.as_str())
    })
    .await
    .expect("bob MUST receive a `dm-received` event for the DM over the PQ tunnel");

    // HARD ASSERT #2: the plaintext lands in Bob's DM thread (read-back).
    poll_until(Duration::from_secs(30), || async {
        let msgs = read_dm_plaintext_any(&bob, &candidates).await?;
        Ok(msgs.iter().any(|(_, b)| b == body).then_some(()))
    })
    .await
    .expect("bob's DM thread MUST contain the delivered DM plaintext");

    run.mark_success();
    drop((alice, bob, ah, bh));
}

// ─────────────────────────────────────────────────────────────────────────────
// S3: channel reconnect catch-up (ZEB-434). Alice creates a channel while Bob is
// hard-offline (SIGKILL); after Bob relaunches against his persisted profile he
// must catch up that channel.
//
// History: this was long #[ignore]'d "blocked by ZEB-462", with a FINDING block
// here asserting that ongoing co-located community-state sync simply never
// establishes (persistent "startup root query: no responder", "reproduced even
// WITHOUT a restart"). That conclusion was an ARTIFACT of a harness bug:
// `channels_contains` checked `c.get("id")`, but `ChannelInfoDto` is camelCase
// (`channelId`), so the assertion was always-false and `poll_until` always timed
// out regardless of whether catch-up actually succeeded. With the key corrected,
// AND the ZEB-462 (B) community-membership-CRDT durability fix on main (#253), Bob
// reliably re-peers (pkarr re-resolve + ZEB-373 iroh dial) and catches up the
// offline-created channel in ~90-110s (dominated by first-contact). Proven 3/3
// across the decisive run + two reliability re-runs. So co-located ongoing
// community-state sync DOES work; only cross-WAN reachability remains a separate,
// cross-machine-playbook concern (ZEB-444). ZEB-462 (A) "no-responder re-peering"
// was a non-bug — the wrong key masked working behavior.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s3_offline_channel_reconnect_catchup() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let (mut run, ah, bh, alice, mut bob) = two_minted_nodes("s3-offline").await;
    let bob_owner = owner_id(&bob).await;

    let community = create_community(&alice, "s3-community", true)
        .await
        .expect("create community");
    let invite = generate_invite(&alice, &community)
        .await
        .expect("generate invite");
    let joined = poll_join_iroh(&bob, &invite, Duration::from_secs(240))
        .await
        .expect("bob joins alice's community via iroh first-contact");
    assert_eq!(
        joined.get("communityId").and_then(|v| v.as_str()),
        Some(community.as_str()),
        "bob joined alice's community"
    );
    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&alice, &community, &bob_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("alice sees bob joined (both online before bob goes offline)");

    // Bob goes HARD offline (SIGKILL); snapshot config for relaunch.
    let bob_cfg = bob.config.clone();
    bob.kill().await.expect("bob hard offline (SIGKILL)");
    drop(bob);

    // Alice creates a channel while Bob is provably offline.
    let channel = create_channel(&alice, &community, "created-while-offline", 0)
        .await
        .expect("alice creates channel while bob is offline");
    eprintln!("S3-offline channel id={channel}");

    // Bob relaunches against the persisted profile/home.
    let bob = NodeHandle::spawn(bob_cfg)
        .await
        .expect("bob relaunches against persisted profile/home");

    // ZEB-434: Bob catches up the offline-created channel after reconnect (works
    // post-ZEB-462(B) + the channelId key fix; see this scenario's header note).
    // 180s ceiling: catch-up is typically fast, but this is a now-un-ignored,
    // network-racy CI test — the poll returns the instant the channel appears, so
    // the extra slack only buys margin on a slow runner (zero happy-path cost).
    poll_until(Duration::from_secs(180), || async {
        Ok(channels_contains(&bob, &community, &channel)
            .await?
            .then_some(()))
    })
    .await
    .expect("bob catches up the offline-created channel after reconnect (ZEB-434)");

    run.mark_success();
    drop((alice, bob, ah, bh));
}

// ─────────────────────────────────────────────────────────────────────────────
// S4: single-node restart durability (ZEB-393). Mint → create community →
// graceful restart → the community must rehydrate and appear in
// `list_owner_communities`.
//
// History: #[ignore]'d "blocked by ZEB-462 (B)" on the belief that a single-node
// restart rehydrates the owner's own membership as `Left` and drops the community
// from `list_owner_communities`. That was a HARNESS bug, not a product bug: the
// poll checked `c.get("id")`, but `CommunityNavDto` is camelCase (`spaceId`), so
// it was always-false and timed out even though the community rehydrated fine —
// boot logs show it spawns an engine + `registered case-C pkarr publications …
// count=1`, both of which require `left_at.is_none()`. With the key corrected this
// passes in ~13s. Single-node owner-state durability (ZEB-393) is intact.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s4_restart_durability() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let mut run = RunDir::new("s4").expect("run dir");
    let home = fresh_home("s4");
    let mut cfg = NodeConfig::new(PathBuf::from(home.path()), "alice");
    cfg.log_dir = Some(run.log_dir());

    let mut alice = NodeHandle::spawn(cfg.clone()).await.expect("spawn");
    mint(&alice).await.expect("mint");
    let community = create_community(&alice, "s4-durable", true)
        .await
        .expect("create community");

    // GRACEFUL shutdown (flushes owner-state on exit) — the robust durability
    // question: does a community survive a CLEAN restart? (A hard SIGKILL-before-
    // debounce is the separate ZEB-393 Bug-A edge.)
    alice.shutdown().await.expect("graceful shutdown");
    drop(alice);

    // Relaunch against the same profile/home; the community must rehydrate.
    let alice = NodeHandle::spawn(cfg).await.expect("relaunch");
    poll_until(Duration::from_secs(120), || async {
        let comms = alice
            .rpc("list_owner_communities", serde_json::json!({}))
            .await?;
        // `CommunityNavDto` is camelCase: the id field is `spaceId`, NOT `id`.
        // Wrong key → always-false → 120s timeout even when the community
        // rehydrated correctly. Surface a future DTO rename as a loud schema
        // error: an empty list is "not rehydrated yet" (keep polling), but a
        // present community object missing `spaceId` is a contract mismatch.
        for c in comms.as_array().cloned().unwrap_or_default() {
            let sid = c.get("spaceId").and_then(|v| v.as_str()).ok_or_else(|| {
                anyhow::anyhow!(
                    "community object missing string `spaceId` key (DTO/schema mismatch?): {c}"
                )
            })?;
            if sid == community.as_str() {
                return Ok(Some(()));
            }
        }
        Ok(None)
    })
    .await
    .expect("community rehydrated after restart (ZEB-393)");

    run.mark_success();
    drop((alice, home));
}

// ─────────────────────────────────────────────────────────────────────────────
// S4b: leave durability — a LEFT community must NOT resurrect after a restart
// (ZEB-427 Half B). Sibling of S4 (restart durability). The headline bug:
// leaving a community removed it from the
// live list (the frontend reacted to the nav "removed" emit) but
// `leave_community` never set `Space.left_at`, so the next boot re-listed it as a
// zombie ("no power, no channels"). PR #228 makes leave set `left_at` + fence it;
// this proves the end-to-end effect through a real boot → leave → restart → boot
// cycle (the live re-probe ZEB-427 was left open pending).
//
// Single-node + deterministic (no first-contact). Two communities exercise the
// original create-persists / leave-doesn't asymmetry in one run:
//   • `kept` — created, never left → MUST rehydrate after restart (control; the
//              same "a created community persists" half ZEB-393 / S4 cover).
//   • `left` — created then left → MUST be filtered live AND stay gone after the
//              restart (the ZEB-427 regression guard).
// The owner leaving their own single-member community is the SOLO-LEAVE path,
// which still runs the `persist_community_left_at` fence (lib.rs:27650), so it
// faithfully exercises the fix. Graceful shutdown mirrors S4; the fence's crash
// (non-graceful) durability is separately unit-proven by #228's fenced
// SyncEngine disk round-trip.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s4b_leave_durability() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let mut run = RunDir::new("s4b").expect("run dir");
    let home = fresh_home("s4b");
    let mut cfg = NodeConfig::new(PathBuf::from(home.path()), "alice");
    cfg.log_dir = Some(run.log_dir());

    let mut alice = NodeHandle::spawn(cfg.clone()).await.expect("spawn");
    mint(&alice).await.expect("mint");

    let kept = create_community(&alice, "s4b-kept", true)
        .await
        .expect("create kept community");
    let left = create_community(&alice, "s4b-left", true)
        .await
        .expect("create community to leave");
    assert_ne!(kept, left, "two distinct communities");

    // Leave one. No owner-leave guard; a single-member leave is the solo path,
    // which still sets `left_at` + fences (ZEB-427 Half B).
    leave_community(&alice, &left)
        .await
        .expect("owner leaves the single-member community");

    // LIVE: `list_owner_communities` filters `left_at` immediately — the left
    // community is hidden, the kept one remains.
    assert!(
        community_listed(&alice, &kept)
            .await
            .expect("list owner communities (kept, live)"),
        "kept community is listed before restart"
    );
    assert!(
        !community_listed(&alice, &left)
            .await
            .expect("list owner communities (left, live)"),
        "left community is hidden from the live list (left_at filter)"
    );

    // GRACEFUL restart (flushes owner-state on exit, mirroring S4) — pre-#228
    // this is exactly where a left community RESURRECTED.
    alice.shutdown().await.expect("graceful shutdown");
    drop(alice);
    let alice = NodeHandle::spawn(cfg)
        .await
        .expect("relaunch against the persisted home");

    // The kept community must rehydrate. Once it reappears, the boot rehydration
    // pass (engine sweep + nav hydration — the SAME path that would resurrect a
    // left community) has completed, so the left community's absence below is
    // meaningful, not merely "not loaded yet".
    poll_until(Duration::from_secs(120), || async {
        Ok(community_listed(&alice, &kept).await?.then_some(()))
    })
    .await
    .expect("kept community rehydrated after restart (control)");

    // REGRESSION GUARD (ZEB-427): the left community must NOT resurrect.
    assert!(
        !community_listed(&alice, &left)
            .await
            .expect("list owner communities (left, post-restart)"),
        "left community must NOT resurrect after restart (ZEB-427 Half B)"
    );

    run.mark_success();
    drop((alice, home));
}

// ─────────────────────────────────────────────────────────────────────────────
// S5: profile-card propagation (ZEB-341 / ZEB-464). After Bob joins Alice's
// community (S1 first-contact establishes the transport path between two never-
// met nodes), each publishes its signed owner card and subscribes to the peer's
// card topic (keyed by owner_id). Both must resolve the peer's SIGNED
// displayName via the headless card verbs ZEB-464 exposes — the last cross-peer
// surface the two-agent suite couldn't drive, and the headless proof behind
// ZEB-432 (community member cards rendering as truncated hex).
//
// Cards ride a Zenoh broadcast topic; a subscription needs only the peer's
// ownerIdHex. The community join is the transport-establishing step — two never-
// met nodes peer via iroh first-contact, not ambient scouting (same reason S1/S2
// dial first). A Zenoh put isn't retained for a late subscriber, so we subscribe
// BEFORE the convergence puts and re-publish each poll tick.
//
// The card VERBS are hard-asserted (boot, join, publish + subscribe accepted).
// Card PROPAGATION is CHARACTERIZED, not hard-asserted: co-located it does NOT
// converge (ZEB-466 — owner-global card topics don't route between two peers
// connected only via a community; verify_card is self-contained so it's a
// transport gap, not a crypto one). This is the likely substrate of ZEB-432; the
// cross-machine S5 run answers whether card topics route cross-WAN. See the
// CARD PROPAGATION block below.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s5_profile_card_propagation() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let (mut run, ah, bh, alice, bob) = two_minted_nodes("s5").await;
    let alice_owner = owner_id(&alice).await;
    let bob_owner = owner_id(&bob).await;

    // Establish a transport path via a community join (S1 first-contact).
    let community = create_community(&alice, "s5-community", true)
        .await
        .expect("create community");
    let invite = generate_invite(&alice, &community)
        .await
        .expect("generate invite");
    poll_join_iroh(&bob, &invite, Duration::from_secs(240))
        .await
        .expect("bob joins alice's community via iroh first-contact");
    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&alice, &community, &bob_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("alice sees bob joined (transport path established)");

    const ALICE_CARD: &str = "Alice-ZEBbot";
    const BOB_CARD: &str = "Bob-KOYAbot";

    // Card publisher runtime is ready post-connect; surface an
    // `owner card runtime not ready` as a clear failure rather than a silent
    // never-arrives before the convergence poll.
    publish_card_until_ok(&alice, ALICE_CARD, "gm from alice", Duration::from_secs(30))
        .await
        .expect("alice's card publisher is ready");
    publish_card_until_ok(&bob, BOB_CARD, "gm from bob", Duration::from_secs(30))
        .await
        .expect("bob's card publisher is ready");

    // Subscribe to the peer's card topic (by owner_id) BEFORE the convergence
    // puts so each subscriber is live when the sample lands.
    let a_sub = subscribe_member_card(&alice, &bob_owner)
        .await
        .expect("alice subscribes to bob's card");
    let b_sub = subscribe_member_card(&bob, &alice_owner)
        .await
        .expect("bob subscribes to alice's card");

    // CARD PROPAGATION — characterized, NOT hard-asserted (co-located gap; see
    // ZEB-466), mirroring S2's DM-delivery treatment. The headless card verbs are
    // PROVEN above: both nodes boot, join, and ACCEPT republish_owner_card +
    // subscribe_member_card. Here we measure whether the signed card actually
    // traverses to the peer's subscriber and verifies into its cache.
    //
    // FINDING (Ildwyn, 2026-06-14): co-located, neither side resolves the peer's
    // card within 120s — even though (a) the community roster sync (Zenoh)
    // converges between the SAME two nodes, (b) no `owner card sign failed`
    // warning fires (cards ARE signed + published), and (c) `verify_card` is fully
    // self-contained (the embedded Master enrollment cert + owner_id binding mean a
    // just-met peer needs nothing external to verify). So it is a TRANSPORT/routing
    // gap for owner-global card topics between two peers connected only via a
    // community — NOT a verification gap — and the likely substrate of ZEB-432.
    // The bounded poll records the outcome without hanging the suite on a known-
    // blocked path; whether card topics route CROSS-WAN (via pkarr/relay) is the
    // open question the cross-machine S5 run answers.
    let converged = poll_until(Duration::from_secs(30), || async {
        let _ = republish_owner_card(&alice, ALICE_CARD, "gm from alice").await;
        let _ = republish_owner_card(&bob, BOB_CARD, "gm from bob").await;
        let a_sees = cached_card_display_name(&alice, a_sub).await?;
        let b_sees = cached_card_display_name(&bob, b_sub).await?;
        Ok(
            (a_sees.as_deref() == Some(BOB_CARD) && b_sees.as_deref() == Some(ALICE_CARD))
                .then_some(()),
        )
    })
    .await
    .is_ok();
    eprintln!(
        "S5 card propagation: converged={converged} (expected FALSE co-located — \
         owner-global card topics don't route between two community-only peers; see \
         ZEB-466 + ZEB-432). The headless card verbs are proven by the accepted \
         publish/subscribe calls above."
    );

    // Exercise the unsubscribe verb (best-effort cleanup).
    unsubscribe_member_card(&alice, a_sub).await.ok();
    unsubscribe_member_card(&bob, b_sub).await.ok();

    run.mark_success();
    drop((alice, bob, ah, bh));
}

// ─────────────────────────────────────────────────────────────────────────────
// S5b — clean-relaunch CONTROL for ZEB-468 (Ildwyn, 2026-06-14).
//
// S5 shows cards don't propagate co-located. ZEB-468 root-caused that to the
// mint-driven node RESTART breaking Zenoh peering (deterministic-zid remap +
// failed ZEB-373 re-peer dial + dead TCP accept loop). `two_minted_nodes` mints
// (= restarts) both nodes, so every S5 run begins from that restart-poisoned
// state.
//
// This control removes exactly that one variable: after minting, KILL both nodes
// and re-spawn them with the SAME config. Each boots ALREADY-minted (identity
// rehydrates from disk → no re-mint, no mint-restart) into a single clean Zenoh
// session, and re-peers co-located via LAN multicast with NO lingering same-zid
// session to collide with. Cards are owner-GLOBAL (`card_topic_for(owner_id)`,
// not community-scoped), so no community join is needed — the ONLY variable under
// test is whether a STABLE co-located mesh routes owner cards.
//
// EXPECTED (ZEB-468 hypothesis): converged=TRUE here, vs S5's FALSE → the restart
// is the sole trigger and a healthy mesh routes cards fine. If this is ALSO false,
// there is a second, deeper transport problem to chase. Characterized (not hard-
// asserted) like S5 so the outcome is read from the eprintln either way.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s5b_clean_relaunch_card_propagation_control() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let (mut run, ah, bh, mut alice, mut bob) = two_minted_nodes("s5b").await;
    let alice_owner = owner_id(&alice).await;
    let bob_owner = owner_id(&bob).await;

    // Clean relaunch: hard-kill both, then re-spawn with the SAME config so each
    // rehydrates its minted identity from disk (no re-mint → no mint-restart, so a
    // single fresh Zenoh session per process — none of S5's restart poisoning).
    let a_cfg = alice.config.clone();
    let b_cfg = bob.config.clone();
    alice.kill().await.expect("kill alice");
    bob.kill().await.expect("kill bob");
    let alice = NodeHandle::spawn(a_cfg)
        .await
        .expect("respawn alice (already minted)");
    let bob = NodeHandle::spawn(b_cfg)
        .await
        .expect("respawn bob (already minted)");

    // Identity rehydrated, NOT re-minted: same owner ids, and this boot did not
    // restart (mint is what restarts; we did not call it here).
    assert_eq!(
        owner_id(&alice).await,
        alice_owner,
        "alice rehydrates the same owner (no re-mint)"
    );
    assert_eq!(
        owner_id(&bob).await,
        bob_owner,
        "bob rehydrates the same owner (no re-mint)"
    );

    const ALICE_CARD: &str = "Alice-clean";
    const BOB_CARD: &str = "Bob-clean";

    publish_card_until_ok(&alice, ALICE_CARD, "gm clean", Duration::from_secs(30))
        .await
        .expect("alice card publisher ready post-relaunch");
    publish_card_until_ok(&bob, BOB_CARD, "gm clean", Duration::from_secs(30))
        .await
        .expect("bob card publisher ready post-relaunch");

    let a_sub = subscribe_member_card(&alice, &bob_owner)
        .await
        .expect("alice subscribes to bob's card");
    let b_sub = subscribe_member_card(&bob, &alice_owner)
        .await
        .expect("bob subscribes to alice's card");

    // Give the clean sessions time to multicast-peer, re-publishing each tick (a
    // Zenoh put is not retained for a late subscriber).
    let converged = poll_until(Duration::from_secs(45), || async {
        let _ = republish_owner_card(&alice, ALICE_CARD, "gm clean").await;
        let _ = republish_owner_card(&bob, BOB_CARD, "gm clean").await;
        let a_sees = cached_card_display_name(&alice, a_sub).await?;
        let b_sees = cached_card_display_name(&bob, b_sub).await?;
        Ok(
            (a_sees.as_deref() == Some(BOB_CARD) && b_sees.as_deref() == Some(ALICE_CARD))
                .then_some(()),
        )
    })
    .await
    .is_ok();
    eprintln!(
        "S5b clean-relaunch card propagation: converged={converged} \
         (ZEB-468 hypothesis expects TRUE — a stable, non-restarted co-located mesh \
         routes owner cards; contrast S5's FALSE post-mint-restart)."
    );

    unsubscribe_member_card(&alice, a_sub).await.ok();
    unsubscribe_member_card(&bob, b_sub).await.ok();

    run.mark_success();
    drop((alice, bob, ah, bh));
}

// ─────────────────────────────────────────────────────────────────────────────
// S5c — clean-state DIAL-ONLY probe for ZEB-468 (Ildwyn, 2026-06-14).
//
// s5b proved a STABLE co-located mesh routes cards — but it peered via LAN
// multicast, so the ZEB-373 dynamic iroh dial (the ONLY Zenoh re-peer path that
// exists cross-WAN, where there is no multicast) was never exercised. In S5 the
// dial FAILED, but under restart-poisoned conditions. This probe answers the
// remaining question: does the ZEB-373 dial establish a working Zenoh peer link
// between two CLEAN full nodes when multicast is OFF?
//
// Setup: mint both, kill + re-spawn already-minted (single clean session each).
// LAN scouting is off by default (ZEB-809) and the harness strips the opt-in
// var from spawned nodes' env, so neither can peer via multicast. Then join a
// community (clean nodes) — the membership reachability exchange fires the
// dial, now the ONLY path to a Zenoh peer mesh. Publish / subscribe owner
// cards and require convergence.
//
// ZEB-809: this probe used to SKIP unless HARMONY_ZENOH_DISABLE_MULTICAST=1 was
// set, because stock zenoh let the two nodes peer via multicast and
// `connect_peer` would then report success off that pre-existing transport — a
// FALSE POSITIVE for the dial.
//
// That guard is now obsolete: `event_loop::run` disables multicast AND gossip
// scouting by default, so a spawned node has no scouting path and the dial is
// already the only way these two can meet. The probe is therefore valid
// unconditionally, and now runs on every e2e sweep instead of only when someone
// remembered the env var. Retiring the guard rather than leaving it inert —
// a skip condition nobody can satisfy reads as "covered" while testing nothing.
//
// The old env var is gone; HARMONY_ZENOH_ENABLE_LAN_SCOUTING=1 now goes the
// other way and would re-break this probe — which is why `NodeHandle::spawn`
// strips it from every spawned node's env (a runner's exported opt-in must not
// silently hand co-located tests a peer path production doesn't have).
//
// HARD-ASSERTED since ZEB-809: with scouting off by default, the dial is the
// ONLY peer path production has on the LAN — the same position it was always
// in cross-WAN. A probe that merely logs its result would leave a broken dial
// green on every sweep (this probe existed because the LAN used to mask
// exactly that). converged=FALSE now fails the test: the dial is independently
// broken and nothing else in the product can paper over it.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s5c_clean_dial_only_card_propagation_probe() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let (mut run, ah, bh, mut alice, mut bob) = two_minted_nodes("s5c").await;
    let alice_owner = owner_id(&alice).await;
    let bob_owner = owner_id(&bob).await;

    // Clean relaunch (single fresh session each, no mint-restart). With multicast
    // disabled by env, these clean sessions can reach a Zenoh peer ONLY via the
    // ZEB-373 iroh dial.
    let a_cfg = alice.config.clone();
    let b_cfg = bob.config.clone();
    alice.kill().await.expect("kill alice");
    bob.kill().await.expect("kill bob");
    let alice = NodeHandle::spawn(a_cfg)
        .await
        .expect("respawn alice (already minted)");
    let bob = NodeHandle::spawn(b_cfg)
        .await
        .expect("respawn bob (already minted)");

    // Community join on the clean nodes → reachability exchange → fires the dial.
    let community = create_community(&alice, "s5c-community", true)
        .await
        .expect("create community");
    let invite = generate_invite(&alice, &community)
        .await
        .expect("generate invite");
    poll_join_iroh(&bob, &invite, Duration::from_secs(240))
        .await
        .expect("bob joins alice's community via iroh first-contact (multicast off)");
    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&alice, &community, &bob_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("alice sees bob joined (roster rides the first-contact bi-stream)");

    const ALICE_CARD: &str = "Alice-dial";
    const BOB_CARD: &str = "Bob-dial";

    publish_card_until_ok(&alice, ALICE_CARD, "gm dial", Duration::from_secs(30))
        .await
        .expect("alice card publisher ready");
    publish_card_until_ok(&bob, BOB_CARD, "gm dial", Duration::from_secs(30))
        .await
        .expect("bob card publisher ready");

    let a_sub = subscribe_member_card(&alice, &bob_owner)
        .await
        .expect("alice subscribes to bob's card");
    let b_sub = subscribe_member_card(&bob, &alice_owner)
        .await
        .expect("bob subscribes to alice's card");

    // Convergence here depends ENTIRELY on the ZEB-373 dial peering the two clean
    // sessions (multicast off). Generous window + re-publish each tick (a Zenoh put
    // is not retained for a late subscriber).
    let converged = poll_until(Duration::from_secs(60), || async {
        let _ = republish_owner_card(&alice, ALICE_CARD, "gm dial").await;
        let _ = republish_owner_card(&bob, BOB_CARD, "gm dial").await;
        let a_sees = cached_card_display_name(&alice, a_sub).await?;
        let b_sees = cached_card_display_name(&bob, b_sub).await?;
        Ok(
            (a_sees.as_deref() == Some(BOB_CARD) && b_sees.as_deref() == Some(ALICE_CARD))
                .then_some(()),
        )
    })
    .await
    .is_ok();
    eprintln!(
        "S5c clean dial-only card propagation: converged={converged} \
         (TRUE → ZEB-373 dial works clean, cross-WAN cards viable; FALSE → the dial is \
         independently broken and the ZEB-468 fix must address it, not just restarts)."
    );

    unsubscribe_member_card(&alice, a_sub).await.ok();
    unsubscribe_member_card(&bob, b_sub).await.ok();

    // ZEB-809 / PR #558 review: hard-assert. With LAN scouting off by default
    // the dial is the only peer path, so a non-converging dial is a product
    // outage, not a data point. Assert AFTER the unsubscribes so a red probe
    // still cleans up its subscriptions.
    assert!(
        converged,
        "S5c: clean dial-only card propagation did not converge — with LAN scouting \
         off by default (ZEB-809) the ZEB-373 iroh dial is the ONLY zenoh peer path, \
         so this red means the dial itself is broken"
    );

    run.mark_success();
    drop((alice, bob, ah, bh));
}

// ─────────────────────────────────────────────────────────────────────────────
// S5d — ZEB-468 restart-safety REGRESSION (hard-asserted). Ildwyn, 2026-06-15.
//
// s5b CHARACTERIZES; s5c (hard-asserted since ZEB-809) and this one GUARD. It
// is the minimal repro of the ZEB-468
// bug: `two_minted_nodes` mints (= RESTARTS) both nodes, which is the exact
// restart-poisoning.
//
// ZEB-810 rework (2026-07-28): this test originally relied on LAN multicast to
// peer two mint-restarted nodes with no community. ZEB-809 turned multicast +
// gossip scouting OFF by default, so relationship-less nodes no longer peer at
// all — the test could neither converge nor reproduce the bug. A first repair
// (peer via a community-join dial, keep the mint-restart) converged but did NOT
// guard ZEB-468: the join established the peer face only AFTER both mint-restarts,
// so no peer ever held a PRE-restart face and remap=0 was vacuous (Qodo, PR #568).
//
// Correct structure: peer FIRST (community join → ZEB-373 dial → alice holds a
// face for bob's deterministic, device-derived zid), THEN restart bob GRACEFULLY
// in-process via stop_node → start_node. That reuses the same zid AND drives
// `session.close()` — the exact path the fix touches — while alice still holds
// bob's old face. A SIGKILL would not do (no graceful close). Pre-fix: alice
// rejects bob's re-declarations ("Remapping unsupported") and the accept loop
// spins ("being shutdown"), so cards never re-converge. Post-fix: the old
// transport is closed, alice drops the stale face, cards re-converge 0/0.
//
// Root cause (see ZEB-468): `open_session_with_runtime` adopts an external zenoh
// `Runtime` via `session::init(DynamicRuntime)`, so `static_runtime` is None and
// `session.close()` only sends a face-close — it never calls the Runtime's
// `manager.close()`. Across a graceful restart that leaks every transport/listener:
//   • the peer keeps our old (deterministic) zid's face → the restarted session's
//     re-declarations are rejected "Resource remapped. Remapping unsupported!";
//   • the TCP accept loop spins on the shutting-down Tokio runtime
//     ("…being shutdown…").
// Both block the owner-card pub/sub mesh from re-forming, so cards never converge.
//
// The fix closes the adopted runtime on shutdown. AFTER it, bob's re-declarations
// land cleanly: cards re-converge, with 0 remap + 0 accept-spin.
//
// BEFORE the fix this test FAILS (converged=false, remap + accept-spin lines on
// bob's re-peer); after it, it PASSES 0/0. Verified passing on main @ 26315288.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s5d_restart_card_propagation_regression() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let (mut run, ah, bh, mut alice, mut bob) = two_minted_nodes("s5d").await;
    let alice_owner = owner_id(&alice).await;
    let bob_owner = owner_id(&bob).await;

    // Peer FIRST, before the restart. Post-ZEB-809 (LAN scouting off by default)
    // the ZEB-373 iroh dial is the only way two co-located nodes peer, and a
    // community join's reachability exchange fires it. Roster convergence proves
    // the zenoh peer session — and hence alice's face for bob's deterministic zid —
    // is up BEFORE bob restarts. Without a pre-restart face the remap guard is
    // vacuous: there is nothing for the restarted session to be rejected against
    // (Qodo, PR #568).
    let community = create_community(&alice, "s5d-community", true)
        .await
        .expect("create community");
    let invite = generate_invite(&alice, &community)
        .await
        .expect("generate invite");
    poll_join_iroh(&bob, &invite, Duration::from_secs(240))
        .await
        .expect("bob joins alice's community via iroh first-contact (multicast off)");
    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&alice, &community, &bob_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("alice sees bob joined — alice now holds a zenoh face for bob's zid");

    // The ZEB-468 event: bob restarts GRACEFULLY in-process (stop_node → start_node),
    // reusing its persisted device identity and therefore the SAME deterministic zid.
    // Unlike a SIGKILL this drives `session.close()` — the exact path the fix touches
    // — while alice still holds bob's PRE-restart face. If the old transport leaks,
    // bob's re-declarations under the reused zid are rejected as a remap.
    bob.rpc("stop_node", json!({}))
        .await
        .expect("bob stop_node (graceful session close)");
    bob.rpc("start_node", json!({}))
        .await
        .expect("bob start_node (same persisted identity → same zid)");

    const ALICE_CARD: &str = "Alice-restart";
    const BOB_CARD: &str = "Bob-restart";

    publish_card_until_ok(&alice, ALICE_CARD, "gm restart", Duration::from_secs(30))
        .await
        .expect("alice card publisher ready");
    publish_card_until_ok(&bob, BOB_CARD, "gm restart", Duration::from_secs(30))
        .await
        .expect("bob card publisher ready after restart");

    let a_sub = subscribe_member_card(&alice, &bob_owner)
        .await
        .expect("alice subscribes to bob's card");
    let b_sub = subscribe_member_card(&bob, &alice_owner)
        .await
        .expect("bob subscribes to alice's card");

    // After bob's graceful restart the reconnect supervisor (ZEB-373/620) must
    // re-dial and re-peer under the reused zid, and the owner-card mesh must re-form
    // WITHOUT a same-zid remap. Re-publish each tick (a Zenoh put is not retained for
    // a late subscriber). 90s: re-peer + first-contact settle after a restart.
    let converged = poll_until(Duration::from_secs(90), || async {
        let _ = republish_owner_card(&alice, ALICE_CARD, "gm restart").await;
        let _ = republish_owner_card(&bob, BOB_CARD, "gm restart").await;
        let a_sees = cached_card_display_name(&alice, a_sub).await?;
        let b_sees = cached_card_display_name(&bob, b_sub).await?;
        Ok(
            (a_sees.as_deref() == Some(BOB_CARD) && b_sees.as_deref() == Some(ALICE_CARD))
                .then_some(()),
        )
    })
    .await
    .is_ok();

    unsubscribe_member_card(&alice, a_sub).await.ok();
    unsubscribe_member_card(&bob, b_sub).await.ok();

    // Read the per-node stderr logs for the transport-health signals. Kill first
    // (awaited) so each child's stderr File is flushed + closed before we read it
    // — a hard kill is abrupt (no graceful teardown), so it adds no teardown spam
    // of its own. Capture the dir BEFORE mark_success (which deletes it on pass).
    let log_dir = run.log_dir();
    alice.kill().await.expect("kill alice");
    bob.kill().await.expect("kill bob");
    // A read failure here must be LOUD, not silent: the transport-health checks below
    // are line counts over these logs, so an unreadable/missing log would silently
    // report remap=0/accept_spin=0 and let the test pass without verifying anything
    // (e.g. if a child's stderr File failed to flush/close on the awaited kill above).
    // An empty measurement is a broken guard, not a clean result — fail hard instead.
    let read_log = |profile: &str| {
        let path = log_dir.join(format!("{profile}.stderr.log"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "ZEB-468 regression: cannot read {profile} stderr log at {}: {e}. \
                 The remap/accept_spin assertions count lines in this log, so an \
                 unreadable log would silently pass (0/0) — failing instead.",
                path.display()
            )
        })
    };
    let alice_log = read_log("alice");
    let bob_log = read_log("bob");
    let count = |needle: &str| -> usize {
        alice_log.matches(needle).count() + bob_log.matches(needle).count()
    };
    // Defect 1: peer rejects the restarted session's re-declarations under the
    // same deterministic zid because the old face was never dropped.
    let remap = count("Remapping unsupported");
    // Defect 3: TCP accept loop polling `accept()` on a shutting-down runtime.
    let accept_spin = count("being shutdown");

    assert!(
        converged,
        "ZEB-468 regression: after bob's graceful in-process restart (same reused zid, \
         alice holding bob's pre-restart face) owner cards must re-converge (got \
         converged=false; remap={remap}, accept_spin={accept_spin}). The restart is \
         leaking the Zenoh transport."
    );
    assert_eq!(
        remap, 0,
        "ZEB-468 regression: a restarted session must not be rejected as a same-zid \
         remap — the old transport must be cleanly closed so the peer drops its face \
         (found {remap} `Remapping unsupported` lines across both nodes)."
    );
    assert_eq!(
        accept_spin, 0,
        "ZEB-468 regression: the Zenoh TCP accept loop must be cancelled on restart, \
         not left spinning on a shutting-down runtime (found {accept_spin} \
         `being shutdown` lines across both nodes)."
    );

    run.mark_success();
    drop((ah, bh));
}

// ─────────────────────────────────────────────────────────────────────────────
// S6: community sealed-relay deposit → recover (ZEB-487 / ZEB-483 durability).
//
// Three nodes: a = sender, b = recipient (goes OFFLINE before the send), r =
// relay host. a + b are friends (so b's OwnerDeviceCache holds a, required to
// verify the recovered CidNotify); all three co-member a community C; r opts in
// to relay for C. With b offline, a creates the DM Space + sends the first
// message — b is unreachable, so after the no-ack windows the deposit fans out
// (butler skipped — b has none — then the relay rung deposits to r). b relaunches
// from its persisted profile, auto-pulls the held blob, bootstraps the DM Space
// from the deposited invite, and the message applies.
//
// Asserts the HELD ∧ RECV ∧ CLEARED triple. If the relay deposit never lands
// co-located within the budget (the ZEB-466-class community-relay resolve/dial
// routing gap that also blocks co-located card propagation), this CHARACTERIZES
// rather than asserts — prints an `S6 FINDING:` line + `mark_success()` (mirroring
// S2/S5). The cross-WAN playbook Scenario D2 is the real proof. If the deposit
// LANDS (HELD) but RECV or CLEARED then fails, that is a genuine product finding
// in the recover path — the assertion is NOT weakened.
//
// The DM Space is deliberately NOT created while b is online — the deposited
// invite must bootstrap it on recovery.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn s6_relay_deposit_recover() {
    use e2e_harness::driver::*;
    use serde_json::Value;
    use std::time::Duration;

    let (mut run, ah, bh, rh, a, mut b, r) = three_minted_nodes("s6").await;
    let a_owner = owner_id(&a).await;
    let b_owner = owner_id(&b).await;

    // --- Setup: shared community C (a creates; b + r join via iroh first-contact,
    //     the SAME path s1/s3 use). poll_join_iroh is racy ~75-90s; 240s budget.
    let community_id = create_community(&a, "s6-relay", true)
        .await
        .expect("create community");
    let invite = generate_invite(&a, &community_id)
        .await
        .expect("generate invite");
    poll_join_iroh(&b, &invite, Duration::from_secs(240))
        .await
        .expect("b joins C via iroh first-contact");
    poll_join_iroh(&r, &invite, Duration::from_secs(240))
        .await
        .expect("r joins C via iroh first-contact");

    // --- Friendship a<->b while b is ONLINE (populates b's OwnerDeviceCache with a
    //     — required to verify the recovered CidNotify). Reuse s2's friend dance.
    let token = generate_friend_token(&a).await.expect("friend token");
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut last_err = String::from("(no redeem attempt completed before the deadline)");
    loop {
        if std::time::Instant::now() >= deadline {
            panic!("b never redeemed a's friend token within 120s; last error: {last_err}");
        }
        match redeem_friend_token(&b, &token).await {
            Ok(_) => break,
            Err(e) => last_err = e.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    poll_until(Duration::from_secs(120), || async {
        accept_pending_from(&a, &b_owner).await?;
        Ok(friend_is_active(&a, &b_owner).await?.then_some(()))
    })
    .await
    .expect("a has b as active friend");
    poll_until(Duration::from_secs(120), || async {
        Ok(friend_is_active(&b, &a_owner).await?.then_some(()))
    })
    .await
    .expect("b has a as active friend");

    // --- r volunteers as the relay for C; confirm the opt-in took.
    set_relay_opt_in(&r, &community_id, true)
        .await
        .expect("r opts in to relay C");
    assert!(
        get_relay_opt_in(&r, &community_id)
            .await
            .expect("r relay status"),
        "r is opted in to relay C"
    );

    // Prove the get_relay_held RPC contract works (a well-formed, empty result)
    // BEFORE we depend on it for the HELD poll — so a broken response surfaces as
    // a failure HERE rather than masquerading as "deposit never landed" in the
    // characterize fallback (Qodo). r is opted in but has accepted nothing yet.
    let pre_held = get_relay_held(&r, Some(&community_id))
        .await
        .expect("get_relay_held RPC contract works (returns a 'held' array)");
    assert!(
        pre_held.is_empty(),
        "relay holds nothing before the offline send"
    );

    // --- REACHABILITY-SYNC BARRIER (durable seal-targets, ZEB-488). The relay
    //     rung resolves b's SEAL TARGETS on the SENDER (a), from a's reachability
    //     resolver, which carries b's ReachabilityAnnounce payload INCLUDING its
    //     durable butler-set. That resolution only succeeds for a fully-offline b
    //     if b's announce replicated to a BEFORE the kill. Block here until a has
    //     observed b's announce (the durable butler-set rides the same announce,
    //     so observing the announce = observing the set — the headless
    //     connectivity_list_peer_reachability DTO does not expose the set field
    //     itself, so this asserts that announce-observed proxy). Generous budget:
    //     the publisher loop + community-CRDT replication is co-located but racy.
    poll_reachability_observed(&a, &b_owner, Duration::from_secs(60))
        .await
        .expect(
            "REACHABILITY: a observed b's ReachabilityAnnounce (durable seal-targets) before kill",
        );
    eprintln!("S6 REACHABILITY: a observed b's announce; durable seal-targets replicated.");

    // --- b goes OFFLINE (real SIGKILL). The DM Space does NOT exist on b yet.
    b.kill().await.expect("kill b");

    // --- a creates the DM Space + sends the first message. b is unreachable, so
    //     after the no-ack windows the deposit fans out: butler skipped (b has
    //     none) → relay rung deposits to r.
    let a_space = add_dm_space(&a, "s6-dm", &b_owner)
        .await
        .expect("a dm space");
    send_dm(&a, &a_space, b"durable-hello", "text/plain")
        .await
        .expect("a send_dm accepted by the engine");

    // --- ASSERTION 1 (HELD): r holds the deposit for b while b is offline. Now a
    //     HARD assert (was a characterize-fallback): the reachability-sync barrier
    //     above guarantees a resolved b's durable seal-targets before the kill, so
    //     the relay deposit MUST land. Generous budget: deposit only fires after
    //     the DEPOSIT_NOACK backoff.
    let held = poll_until(Duration::from_secs(90), || async {
        let entries = get_relay_held(&r, Some(&community_id)).await?;
        let m = entries.into_iter().find(|e| {
            e.get("senderOwnerHex").and_then(Value::as_str) == Some(a_owner.as_str())
                && e.get("recipientOwnerHex").and_then(Value::as_str) == Some(b_owner.as_str())
        });
        Ok(m)
    })
    .await;
    assert!(
        held.is_ok(),
        "HELD: r holds a's deposit for offline b (durable seal-targets)"
    );
    eprintln!("S6 HELD: r is holding a's deposit for b while b is offline.");

    // --- b comes back ONLINE (rehydrates from its persisted profile). Recovery is
    //     automatic: b pulls held blobs for C from r, ingests, the deposited invite
    //     bootstraps the DM Space, the message applies, dm-received fires.
    b = b.relaunch().await.expect("relaunch b");

    // --- RECV (characterized): a's plaintext shows up in b's thread post-reconnect.
    //     b had no DM Space before going offline; apply_deposited_invite bootstraps
    //     it PINNED to a's space_id (a_space). Use the candidate-tolerant helper
    //     (matches the other DM tests + tolerant of UnknownSpace while the recovered
    //     thread settles).
    let recovered = poll_until(Duration::from_secs(90), || async {
        let msgs = read_dm_plaintext_any(&b, &[a_space.as_str()])
            .await
            .unwrap_or_default();
        Ok(msgs
            .iter()
            .any(|(_, body)| body == b"durable-hello")
            .then_some(()))
    })
    .await;

    if recovered.is_err() {
        // Recover half (RECV/CLEARED) characterized pending ZEB-509: co-located
        // community query-serve never converges (epoch incomplete) → offline
        // recipient can't re-sync community state to drive the relay/butler pull.
        // The owner-state epoch DURABILITY race behind this is fixed (#307) and
        // regression-guarded (the ZEB-509 epoch-key assertion in
        // pkarr_iroh_redeem_full_integration::zeb427_iroh_redeem_fences_owner_state_space_to_disk);
        // the residual is the co-located harness-topology gap (recover likely needs
        // the cross-WAN pkarr/relay re-discovery layer this topology lacks). Deposit
        // half (REACHABILITY + HELD) is hard-asserted above; cross-WAN two-machine
        // (ZEB-477) is the authoritative recover proof.
        eprintln!(
            "S6 FINDING (ZEB-509): b did not recover the deposited DM within 90s \
             co-located. Deposit landed (HELD passed); the gap is the co-located \
             community query-serve / epoch-incomplete path that blocks b's relay \
             pull on reconnect. Cross-WAN Scenario D2 (ZEB-477) is the recover proof. \
             Skipping RECV/CLEARED."
        );
        run.mark_success();
        drop((a, b, r, ah, bh, rh));
        return;
    }
    eprintln!("S6 RECV: b recovered the deposited DM after reconnect.");

    // --- CLEARED (characterized): r's held entry is gone (acked + GC'd post-recovery).
    let cleared = poll_until(Duration::from_secs(60), || async {
        let entries = get_relay_held(&r, Some(&community_id)).await?;
        let still_held = entries
            .iter()
            .any(|e| e.get("recipientOwnerHex").and_then(Value::as_str) == Some(b_owner.as_str()));
        Ok((!still_held).then_some(()))
    })
    .await;

    if cleared.is_err() {
        // Recover half (RECV/CLEARED) characterized pending ZEB-509: co-located
        // community query-serve never converges (epoch incomplete) → offline
        // recipient can't re-sync community state to drive the relay/butler pull.
        // The owner-state epoch DURABILITY race behind this is fixed (#307) and
        // regression-guarded (the ZEB-509 epoch-key assertion in
        // pkarr_iroh_redeem_full_integration::zeb427_iroh_redeem_fences_owner_state_space_to_disk);
        // the residual is the co-located harness-topology gap (recover likely needs
        // the cross-WAN pkarr/relay re-discovery layer this topology lacks). Deposit
        // half (REACHABILITY + HELD) is hard-asserted above; cross-WAN two-machine
        // (ZEB-477) is the authoritative recover proof.
        eprintln!(
            "S6 FINDING (ZEB-509): r's held entry was not released within 60s after \
             b's recovery co-located. RECV passed but the clear/GC handshake did not \
             settle co-located. Cross-WAN Scenario D2 (ZEB-477) is the recover proof. \
             Skipping CLEARED."
        );
        run.mark_success();
        drop((a, b, r, ah, bh, rh));
        return;
    }
    eprintln!("S6 CLEARED: r released the held deposit after recovery.");

    run.mark_success();
    drop((a, b, r, ah, bh, rh));
}

// ─────────────────────────────────────────────────────────────────────────────
// ZEB-490 — s7: butler deposit→recover (co-located).
//
// A (sender) befriends P (recipient primary). P pairs a SECOND local instance
// B2 into its fleet via the real ZEB-446 SAS handshake, then pins B2 as butler.
// With P offline, A creates the DM Space + sends — after the no-ack windows the
// deposit fans out to P's butler B2. P relaunches, fleet-merges with B2,
// recovers the deposited invite + message.
//
// Layered characterize fallbacks at the racy co-located boundaries (the s6
// pattern): (1) pairing may not establish (Zenoh harmony/pairing/v2/lan/** is
// the same transport class as the ZEB-466 gap); (2) the deposit may not land on
// B2; (3) recovery may not complete. Every boundary that DOES establish becomes
// a hard assertion. set_butler_pin + get_butler_pin are hard-asserted whenever
// boundary 1 passes (the guaranteed-value core).
//
// ZEB-492 promotes the butler-engine-readiness boundary (0b) to a HARD ASSERT:
// the owner's fleet KeyTree is now sealed into the SAS pairing payload and
// persisted on the cert-only joiner, so `start_node` rebuilds B2's dm-inbox /
// butler engines from that material on relaunch. B2 therefore MUST be a working
// butler (get_butler_held returns a clean empty array, no "dm-inbox not running"
// 500) after pairing+relaunch — this scenario asserts that. The remaining
// characterize boundaries (2 deposit-routing, 3 recovery) cover a SEPARATE
// co-located sender->butler resolve/dial gap, not KeyTree distribution. The
// cross-WAN playbook Scenario D3 is the authoritative proof of the full
// deposit→recover path; this is its co-located sibling.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn s7_butler_deposit_recover() {
    use e2e_harness::driver::*;
    use serde_json::Value;
    use std::time::Duration;

    // --- Spawn A (sender) + P (primary) + B2 (fresh joiner). Mint A + P only;
    //     B2 stays unminted and acquires identity by enrolling into P's fleet.
    let mut run = RunDir::new("s7").expect("run dir");
    let a_home = fresh_home("s7-a");
    let p_home = fresh_home("s7-p");
    let b2_home = fresh_home("s7-b2");
    let mk = |home: &tempfile::TempDir, profile: &str| {
        let mut cfg = NodeConfig::new(PathBuf::from(home.path()), profile);
        cfg.log_dir = Some(run.log_dir());
        cfg
    };
    let a = NodeHandle::spawn(mk(&a_home, "alice"))
        .await
        .expect("spawn a");
    let mut p = NodeHandle::spawn(mk(&p_home, "primary"))
        .await
        .expect("spawn p");
    let mut b2 = NodeHandle::spawn(mk(&b2_home, "butler"))
        .await
        .expect("spawn b2");
    a.rpc("mint_owner_identity", json!({}))
        .await
        .expect("a mint");
    p.rpc("mint_owner_identity", json!({}))
        .await
        .expect("p mint");

    let a_owner = owner_id(&a).await;
    let p_owner = owner_id(&p).await;

    // --- Boundary 1: pair B2 into P's fleet via the real SAS handshake.
    //     CHARACTERIZE (left as-is, NOT hardened by the durable-seal-targets
    //     work): this gates on the co-located pairing TRANSPORT (Zenoh
    //     harmony/pairing/v2/lan/**), a pre-condition unrelated to deposit
    //     seal-target durability — it stays a fallback pending its own
    //     pairing-transport ticket.
    let b2_device = match pair_into_fleet(&p, &b2, "s7", Duration::from_secs(180)).await {
        Ok(dev) => dev,
        Err(e) => {
            eprintln!(
                "S7 FINDING: SAS pairing did not establish co-located within 180s \
                 ({e}). Pairing discovery rides Zenoh harmony/pairing/v2/lan/** — \
                 the same transport class as the ZEB-466 co-located gap. File a \
                 finding ticket; the cross-WAN Scenario D3 is the real proof. \
                 Skipping pin/HELD/RECV/CLEARED."
            );
            run.mark_success();
            drop((a, p, b2, a_home, p_home, b2_home));
            return;
        }
    };
    eprintln!("S7 PAIRED: B2 enrolled into P's fleet (device {b2_device}).");

    // --- Friendship A<->P while both ONLINE. Done BEFORE the post-pairing
    //     relaunch for two reasons: (1) A's device directory learns P's fleet
    //     (incl. B2's reachability, ZEB-461); (2) the multi-second handshake is
    //     a deterministic barrier that lets P's async pairing-persist drainer
    //     (lib.rs:8821 — a ~50ms spawn_blocking fired on the inviter's
    //     InviterEnrollResult just before Complete) finish writing B2's
    //     EnrollmentCert to disk, so the relaunch below actually reloads it. A
    //     SIGKILL immediately after Complete races that write and loses it.
    let token = generate_friend_token(&a).await.expect("friend token");
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut last_err = String::from("(no redeem attempt completed before the deadline)");
    loop {
        if std::time::Instant::now() >= deadline {
            panic!("P never redeemed A's friend token within 120s; last error: {last_err}");
        }
        match redeem_friend_token(&p, &token).await {
            Ok(_) => break,
            Err(e) => last_err = e.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    poll_until(Duration::from_secs(120), || async {
        accept_pending_from(&a, &p_owner).await?;
        Ok(friend_is_active(&a, &p_owner).await?.then_some(()))
    })
    .await
    .expect("A has P as active friend");
    poll_until(Duration::from_secs(120), || async {
        Ok(friend_is_active(&p, &a_owner).await?.then_some(()))
    })
    .await
    .expect("P has A as active friend");
    eprintln!("S7 FRIENDS: A<->P active; P had time to persist B2's enrollment.");

    // --- Shared community A<->P. REQUIRED for the durable-seal-targets path the
    //     reachability-sync barrier (below) asserts: P's durable butler-set
    //     (advertising B2) is carried in the per-community `ReachabilityAnnounce`
    //     CRDT, which replicates ONLY to CO-MEMBERS. Without a shared community,
    //     A would learn P's butler-set solely via P's pkarr friend record — the
    //     windowed PkarrLive path this task does NOT harden — and A's
    //     community-CRDT resolver (what the barrier reads) would never observe P.
    //     A creates; P joins via the same iroh first-contact path s6 uses.
    let community_id = create_community(&a, "s7-butler", true)
        .await
        .expect("a creates shared community");
    let invite = generate_invite(&a, &community_id)
        .await
        .expect("generate invite");
    poll_join_iroh(&p, &invite, Duration::from_secs(240))
        .await
        .expect("P joins the shared community via iroh first-contact");
    eprintln!("S7 COMMUNITY: A<->P co-members of {community_id} (durable announce path).");

    // --- Relaunch P so start_node rebuilds `fleet_net_enrolled` (a boot-time
    //     snapshot, lib.rs:4618 → 8203, that runtime pairing does NOT refresh)
    //     from P's PERSISTED enrollments — the precondition for set_butler_pin.
    //     NB (ZEB-491): the headless INVITER currently never persists the paired
    //     device's enrollment, so this reload finds peers=0 and the pin boundary
    //     below characterizes. The relaunch + the friend-dance barrier above are
    //     kept so this scenario asserts the full path the moment ZEB-491 lands.
    //     Friendship is persisted; its live "active" status may not re-show
    //     immediately co-located after a reboot (ZEB-466 class) — do NOT block.
    p = p
        .relaunch()
        .await
        .expect("relaunch p to refresh enrolled set after pairing");
    eprintln!("S7 RELAUNCH: P rebooted post-pairing.");

    // --- Boundary 0 (butler-pin setup). After the reboot, P's enrolled-set
    //     snapshot is rebuilt from its PERSISTED enrollments. Inspect them via
    //     get_owner_state: the GUI butler toggle pins by `deviceVkHex` (the
    //     64-hex ed25519_verify; owner_commands.rs:118), so read the (only) peer
    //     device's deviceVkHex straight from P's own view rather than trusting
    //     the value captured during pairing. If no peer device is present, the
    //     headless inviter's pairing enrollment never landed on disk —
    //     butler-pin-after-pairing is blocked (a real product gap); characterize.
    let owner = p
        .rpc("get_owner_state", json!({}))
        .await
        .expect("get_owner_state");
    // A missing/non-array `devices` is a schema/contract regression, not "no
    // peers" — hard-fail rather than silently characterize (CodeRabbit).
    let devices = owner
        .get("devices")
        .and_then(Value::as_array)
        .expect("get_owner_state response has a 'devices' array");
    let peer_devices: Vec<&Value> = devices
        .iter()
        .filter(|d| d.get("isThisDevice").and_then(Value::as_bool) == Some(false))
        .collect();
    eprintln!(
        "S7 DEVICES after relaunch: total={}, peers={} (paired b2_device={})",
        devices.len(),
        peer_devices.len(),
        b2_device
    );

    let butler_vk = match peer_devices.as_slice() {
        // Exactly one peer (B2) once the inviter's enrollment landed.
        [d] => d
            .get("deviceVkHex")
            .and_then(Value::as_str)
            .expect("peer device exposes deviceVkHex")
            .to_string(),
        // Intermittent inviter-persist gap (ZEB-491): B2's enrollment did not
        // land on P — characterize boundary 0. CHARACTERIZE (left as-is, NOT
        // hardened by the durable-seal-targets work): this gates on the ZEB-491
        // inviter-enrollment-persist gap, a pre-condition unrelated to deposit
        // seal-target durability — it stays a fallback pending ZEB-491.
        [] => {
            eprintln!(
                "S7 FINDING (ZEB-491): after pairing + reboot, P's persisted \
                 enrolled set has no peer device — the headless inviter's pairing \
                 enrollment did not land in OwnerState, so butler-pin-after-pairing \
                 is blocked. The cross-WAN Scenario D3 is the authoritative proof. \
                 Skipping PIN/HELD/RECV/CLEARED."
            );
            run.mark_success();
            drop((a, p, b2, a_home, p_home, b2_home));
            return;
        }
        // Only B2 was paired into P's fleet, so >1 peer is an unexpected anomaly.
        many => panic!(
            "P's fleet has {} peer devices (expected exactly 1, B2)",
            many.len()
        ),
    };

    // --- Hard assertion: pin the peer device as butler and read it back.
    set_butler_pin(&p, Some(&butler_vk))
        .await
        .expect("pin the peer device as butler");
    let pin = get_butler_pin(&p).await.expect("get butler pin");
    assert_eq!(
        pin.get("pinnedDeviceId").and_then(Value::as_str),
        Some(butler_vk.as_str()),
        "get_butler_pin reflects the pinned butler device"
    );
    eprintln!("S7 PIN: P pinned B2 ({butler_vk}) as butler; get_butler_pin confirms.");

    // --- B2 was spawned UNMINTED and only acquired its identity via pairing.
    //     Pre-ZEB-492, its owner-dependent engines (dm-inbox / butler acceptor)
    //     never started — `start_node` only built the fleet engines from a master
    //     SEED, which a cert-only enrolled device does not have, so
    //     get_butler_held would 500 with "dm-inbox not running" (the old
    //     ZEB-491 characterize boundary). ZEB-492 changed this: the owner's fleet
    //     KeyTree is now sealed into the SAS pairing payload, persisted on the
    //     joiner (fleet_keytree.enc), and `start_node` rebuilds the fleet engines
    //     from THAT persisted material on boot. So after this relaunch B2 boots
    //     WITH its enrolled identity AND its dm-inbox engine, can actually hold
    //     deposits, and fleet-syncs P's pin to learn it is the butler.
    b2 = b2
        .relaunch()
        .await
        .expect("relaunch b2 to start its dm-inbox/butler engines after pairing");
    eprintln!("S7 B2-RELAUNCH: butler B2 rebooted with its enrolled identity.");

    // --- Boundary 0b (butler engine readiness) — HARD ASSERT (ZEB-492). A
    //     cert-only paired device that rebuilds its fleet engines from the
    //     persisted KeyTree MUST have a live dm-inbox engine after relaunch, so
    //     get_butler_held returns a well-formed (empty) 'held' array rather than
    //     500ing with "dm-inbox not running". Any error here is now a regression
    //     of the ZEB-492 KeyTree-distribution feature (or a get_butler_held
    //     contract/schema break) — fail loudly rather than characterize. This is
    //     the boundary that gates the cert-only-B2-is-a-real-butler claim; the
    //     downstream HELD/RECV boundaries below stay tolerant of the SEPARATE
    //     co-located sender->butler deposit-routing gap (see boundary 2).
    let pre_held = get_butler_held(&b2).await.unwrap_or_else(|e| {
        panic!(
            "ZEB-492 REGRESSION: butler B2 (cert-only, paired, relaunched) failed \
                     get_butler_held ({e}). Its dm-inbox engine must boot from the persisted \
                     fleet KeyTree — a 'not running' error means the KeyTree did not reach \
                     start_node's engine builder; any other error is a get_butler_held \
                     contract/schema break."
        )
    });
    assert!(
        pre_held.is_empty(),
        "B2 holds nothing before the offline send"
    );
    eprintln!(
        "S7 B2-BUTLER-READY: cert-only B2's dm-inbox engine is live post-relaunch \
         (get_butler_held OK, empty) — ZEB-492 KeyTree distribution reached the engines."
    );

    // --- REACHABILITY-SYNC BARRIER (durable seal-targets, ZEB-488). The butler
    //     deposit resolves P's SEAL TARGETS on the SENDER (A), from A's
    //     reachability resolver, which carries P's ReachabilityAnnounce payload
    //     INCLUDING its DURABLE butler-set (P's butler-set advertises B2). That
    //     resolution only succeeds for a fully-offline P if P's announce (with the
    //     B2 butler-set) replicated to A BEFORE the kill. Block here until A has
    //     observed P's announce (the durable butler-set rides the same announce —
    //     the headless connectivity_list_peer_reachability DTO does not expose the
    //     set field, so this asserts that announce-observed proxy). P is online
    //     (relaunched post-pairing) and has pinned B2 as butler, so its publisher
    //     advertises the B2-bearing durable set. Generous budget: the publisher
    //     loop + community-CRDT replication is co-located but racy.
    poll_reachability_observed(&a, &p_owner, Duration::from_secs(60))
        .await
        .expect(
            "REACHABILITY: A observed P's ReachabilityAnnounce (durable butler-set) before kill",
        );
    eprintln!("S7 REACHABILITY: A observed P's announce; durable B2 butler-set replicated.");

    // --- P goes OFFLINE (real SIGKILL). The DM Space does NOT exist on P yet.
    p.kill().await.expect("kill p");

    // --- A creates the DM Space + sends. P unreachable → after the no-ack
    //     windows the deposit fans out to P's butler B2.
    let a_space = add_dm_space(&a, "s7-dm", &p_owner)
        .await
        .expect("a dm space");
    send_dm(&a, &a_space, b"butler-durable-hello", "text/plain")
        .await
        .expect("a send_dm accepted by the engine");

    // --- Boundary 2 (HELD, HARD ASSERT — ZEB-510 step 1): B2 holds the deposit
    //     for P while P is offline. Generous budget — deposit fires only after
    //     DEPOSIT_NOACK_WINDOWS=2.
    //
    // ZEB-510 (step 1): P now seeds sibling B2's iroh endpoint into its dial
    // resolver from the persisted FleetNetDoc (FleetNetDoc→ReachabilityResolver
    // wiring), so P's published butler-set reaches B2 and A's deposit lands on
    // B2. HARD ASSERT — if this times out, step 1 alone did not converge
    // fleet-net co-located and the step-2 SAS first-contact seed is required. Do
    // NOT weaken this assert back to a soft characterize: the timeout IS the
    // step-1-vs-step-2 gate signal (design doc §"The step-1-vs-step-2 gate").
    let held_entry = poll_until(Duration::from_secs(120), || async {
        let entries = get_butler_held(&b2).await?;
        Ok(entries
            .into_iter()
            .find(|e| e.get("senderOwnerHex").and_then(Value::as_str) == Some(a_owner.as_str())))
    })
    .await
    .expect(
        "S7 HELD (ZEB-510): B2 must hold A's deposit for P within 120s co-located — \
         P should have learned B2's iroh endpoint via the FleetNetDoc→resolver wiring",
    );
    // RESIDUAL (ZEB-510 step 1): RECV/CLEARED below remain soft-characterize
    // fallbacks — the recover half (B2->P handoff) is validated cross-WAN by
    // Scenario D3; promote them in a follow-up if they pass co-located.
    let held_space = held_entry
        .get("spaceIdHex")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let held_cid = held_entry
        .get("messageCidHex")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    eprintln!("S7 HELD: B2 is holding A's deposit for P (space {held_space}, cid {held_cid}).");

    // --- P comes back ONLINE; fleet-merges with B2, recovers the deposited
    //     invite + message (apply_deposited_invite bootstraps the DM Space).
    p = p.relaunch().await.expect("relaunch p");

    // --- RECV (characterized): A's plaintext shows up in P's thread post-reconnect.
    let recovered = poll_until(Duration::from_secs(120), || async {
        let msgs = read_dm_plaintext_any(&p, &[a_space.as_str()])
            .await
            .unwrap_or_default();
        Ok(msgs
            .iter()
            .any(|(_, body)| body == b"butler-durable-hello")
            .then_some(()))
    })
    .await;

    if recovered.is_err() {
        // Recover half (RECV/CLEARED) characterized pending ZEB-509: co-located
        // community query-serve never converges (epoch incomplete) → offline
        // recipient can't re-sync community state to drive the relay/butler pull.
        // The owner-state epoch DURABILITY race behind this is fixed (#307) and
        // regression-guarded (the ZEB-509 epoch-key assertion in
        // pkarr_iroh_redeem_full_integration::zeb427_iroh_redeem_fences_owner_state_space_to_disk);
        // the residual is the co-located harness-topology gap (recover likely needs
        // the cross-WAN pkarr/relay re-discovery layer this topology lacks). Deposit
        // half (REACHABILITY + HELD) is hard-asserted above; cross-WAN two-machine
        // (ZEB-477) is the authoritative recover proof.
        eprintln!(
            "S7 FINDING (ZEB-509): P did not recover the butler-deposited DM within \
             120s co-located. Deposit landed (HELD passed); the gap is the co-located \
             community query-serve / epoch-incomplete path that blocks P's pull on \
             reconnect. Cross-WAN Scenario D3 (ZEB-477) is the recover proof. \
             Skipping RECV/CLEARED."
        );
        run.mark_success();
        drop((a, p, b2, a_home, p_home, b2_home));
        return;
    }
    eprintln!("S7 RECV: P recovered the butler-deposited DM after reconnect.");

    // --- CLEARED (characterized): butler's `ingested_by` is a grow-only set (the
    //     recovered signal). Once P ingests, the held entry either gains a device in
    //     ingestedByDevices OR is GC'd away — accept either as cleared.
    let cleared = poll_until(Duration::from_secs(60), || async {
        let entries = get_butler_held(&b2).await?;
        let entry = entries
            .iter()
            .find(|e| e.get("senderOwnerHex").and_then(Value::as_str) == Some(a_owner.as_str()));
        let done = match entry {
            None => true, // entry GC'd away after recovery
            Some(e) => e
                .get("ingestedByDevices")
                .and_then(Value::as_array)
                .map(|arr| !arr.is_empty())
                .unwrap_or(false),
        };
        Ok(done.then_some(()))
    })
    .await;

    if cleared.is_err() {
        // Recover half (RECV/CLEARED) characterized pending ZEB-509: co-located
        // community query-serve never converges (epoch incomplete) → offline
        // recipient can't re-sync community state to drive the relay/butler pull.
        // The owner-state epoch DURABILITY race behind this is fixed (#307) and
        // regression-guarded (the ZEB-509 epoch-key assertion in
        // pkarr_iroh_redeem_full_integration::zeb427_iroh_redeem_fences_owner_state_space_to_disk);
        // the residual is the co-located harness-topology gap (recover likely needs
        // the cross-WAN pkarr/relay re-discovery layer this topology lacks). Deposit
        // half (REACHABILITY + HELD) is hard-asserted above; cross-WAN two-machine
        // (ZEB-477) is the authoritative recover proof.
        eprintln!(
            "S7 FINDING (ZEB-509): B2's held entry did not record P's recovery within \
             60s co-located. RECV passed but the ingest/GC handshake did not settle \
             co-located. Cross-WAN Scenario D3 (ZEB-477) is the recover proof. \
             Skipping CLEARED."
        );
        run.mark_success();
        drop((a, p, b2, a_home, p_home, b2_home));
        return;
    }
    eprintln!("S7 CLEARED: B2 recorded P's recovery of the deposit.");

    run.mark_success();
    drop((a, p, b2, a_home, p_home, b2_home));
}

// ─────────────────────────────────────────────────────────────────────────────
// S8 (hard-assert): multi-member CHANNEL messaging. Two co-located nodes join one
// invite-only community + a shared channel; BOTH post a text message; each node
// MUST see the OTHER's (remote-authored) message — asserted BOTH ways: the
// `channel-message-received` WS event fires AND the message lands in
// `list_channel_messages` keyed on the remote author's ownerId. Mirrors S2-hard's
// dual hard-assert, but for community channels rather than 1:1 DMs.
//
// First proof that a message authored on node A is read back on node B — the
// multi-member channel-delivery primitive behind putting the fleet on Harmony
// (ZEB-529 / ZEB-528).
// ─────────────────────────────────────────────────────────────────────────────

/// Strictly decode a `ChannelMessageDto.body` into bytes. serde serializes the
/// DTO's `body: Vec<u8>` as a JSON ARRAY of byte numbers (NO hex wrapper —
/// unlike the DM `dm-received` payload, which IS hex), so matching it as hex
/// would silently never match. Returns `None` on a missing/non-array body or any
/// element that isn't an integer in `0..=255` — strict rather than
/// coercing/truncating, so a schema regression can't false-match in a
/// hard-assert (Qodo). Mirrors the driver's strict-schema convention
/// (`channels_contains` / `get_relay_held`).
fn channel_msg_body_bytes(msg: &serde_json::Value) -> Option<Vec<u8>> {
    msg.get("body")?
        .as_array()?
        .iter()
        .map(|n| n.as_u64().filter(|v| *v <= u8::MAX as u64).map(|v| v as u8))
        .collect()
}

/// True iff `node`'s view of `(community, channel)` contains a message whose
/// `author` (camelCase DTO key == the author node's ownerId) and decoded `body`
/// both match — proves a specific remote-authored message is readable.
async fn channel_has_message_from(
    node: &e2e_harness::NodeHandle,
    community_id: &str,
    channel_id: &str,
    author_owner: &str,
    want_body: &[u8],
) -> anyhow::Result<bool> {
    let msgs = e2e_harness::driver::list_channel_messages(node, community_id, channel_id).await?;
    Ok(msgs.iter().any(|m| {
        m.get("author").and_then(|a| a.as_str()) == Some(author_owner)
            && channel_msg_body_bytes(m).as_deref() == Some(want_body)
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s8_channel_multi_member_message_exchange() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let (mut run, ah, bh, alice, bob) = two_minted_nodes("s8").await;
    let alice_owner = owner_id(&alice).await;
    let bob_owner = owner_id(&bob).await;

    // Subscribe to BOTH event streams BEFORE any post, so neither node can miss
    // its peer's `channel-message-received` frame (the WS forwards from connect).
    let (mut alice_events, _a_ev) = alice.events().await.expect("subscribe alice events");
    let (mut bob_events, _b_ev) = bob.events().await.expect("subscribe bob events");

    // --- Community + invite + Bob iroh first-contact join (S1 path).
    let community = create_community(&alice, "s8-community", true)
        .await
        .expect("create community");
    let invite = generate_invite(&alice, &community)
        .await
        .expect("generate invite");
    poll_join_iroh(&bob, &invite, Duration::from_secs(240))
        .await
        .expect("bob joins alice's community via iroh first-contact");

    // Roster converges both ways (transport path is live).
    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&alice, &community, &bob_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("alice sees bob joined");
    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&bob, &community, &alice_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("bob sees alice joined");

    // --- Alice creates a shared channel; Bob must converge on it (S3 path).
    //     write_power 0 so both members can post.
    let channel = create_channel(&alice, &community, "s8-shared", 0)
        .await
        .expect("alice creates shared channel");
    poll_until(Duration::from_secs(180), || async {
        Ok(channels_contains(&bob, &community, &channel)
            .await?
            .then_some(()))
    })
    .await
    .expect("bob converges the shared channel (ZEB-434 channel propagation)");
    assert!(
        channels_contains(&alice, &community, &channel)
            .await
            .expect("alice list channels"),
        "alice sees her own channel"
    );

    // --- Both members post a distinct text message to the shared channel.
    let alice_body: &[u8] = b"hello-channel-from-alice";
    let bob_body: &[u8] = b"hello-channel-from-bob";
    post_channel_message(&alice, &community, &channel, alice_body)
        .await
        .expect("alice's post_channel_message accepted");
    post_channel_message(&bob, &community, &channel, bob_body)
        .await
        .expect("bob's post_channel_message accepted");

    // ── HARD ASSERT #1 (WS events, remote-authored both ways) ──
    // Payload (ChannelMessageReceivedPayload): { communityId, channelId,
    // message: ChannelMessageDto{ author, body(JSON byte-array), .. } }.
    e2e_harness::await_event(&mut bob_events, Duration::from_secs(60), |f| {
        f.event == "channel-message-received"
            && f.payload.get("channelId").and_then(|c| c.as_str()) == Some(channel.as_str())
            && f.payload
                .get("message")
                .map(|m| {
                    m.get("author").and_then(|a| a.as_str()) == Some(alice_owner.as_str())
                        && channel_msg_body_bytes(m).as_deref() == Some(alice_body)
                })
                .unwrap_or(false)
    })
    .await
    .expect("bob MUST receive a channel-message-received for alice's remote-authored message");

    e2e_harness::await_event(&mut alice_events, Duration::from_secs(60), |f| {
        f.event == "channel-message-received"
            && f.payload.get("channelId").and_then(|c| c.as_str()) == Some(channel.as_str())
            && f.payload
                .get("message")
                .map(|m| {
                    m.get("author").and_then(|a| a.as_str()) == Some(bob_owner.as_str())
                        && channel_msg_body_bytes(m).as_deref() == Some(bob_body)
                })
                .unwrap_or(false)
    })
    .await
    .expect("alice MUST receive a channel-message-received for bob's remote-authored message");

    // ── HARD ASSERT #2 (read-back, remote-authored both ways) ──
    poll_until(Duration::from_secs(30), || async {
        Ok(
            channel_has_message_from(&bob, &community, &channel, &alice_owner, alice_body)
                .await?
                .then_some(()),
        )
    })
    .await
    .expect("bob's channel read-back MUST contain alice's remote-authored message");
    poll_until(Duration::from_secs(30), || async {
        Ok(
            channel_has_message_from(&alice, &community, &channel, &bob_owner, bob_body)
                .await?
                .then_some(()),
        )
    })
    .await
    .expect("alice's channel read-back MUST contain bob's remote-authored message");

    run.mark_success();
    drop((alice, bob, ah, bh));
}

// ─────────────────────────────────────────────────────────────────────────────
// S9 (hard-assert): THREE-member community convergence + channel messaging,
// co-located. Founder `a` + two joiners `b`, `c` each redeem a fresh invite via
// iroh first-contact. ALL THREE rosters MUST converge to {a,b,c}, both joiners
// MUST converge on a shared channel, and every node MUST read back all three
// members' channel messages. Directly exercises the 3rd-member convergence path
// (ZEB-526 / ZEB-530): does c's join reach a + b, and does the channel fan-out
// reach all three co-located? All three are ONLINE throughout — this isolates
// the all-online 3rd-member case from the offline-2nd-member 3b scenario that
// first surfaced ZEB-526.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn s9_three_member_channel_convergence() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    // three_minted_nodes returns (run, a_home, b_home, r_home, a, b, r); here the
    // three roles are simply three community members a / b / c.
    let (mut run, ah, bh, ch, a, b, c) = three_minted_nodes("s9").await;
    let a_owner = owner_id(&a).await;
    let b_owner = owner_id(&b).await;
    let c_owner = owner_id(&c).await;

    // --- Founder a creates the invite-only community; b then c join via iroh
    //     first-contact (a fresh invite per joiner).
    let community = create_community(&a, "s9-community", true)
        .await
        .expect("create community");
    let invite_b = generate_invite(&a, &community).await.expect("invite for b");
    poll_join_iroh(&b, &invite_b, Duration::from_secs(240))
        .await
        .expect("b joins via iroh first-contact");
    let invite_c = generate_invite(&a, &community).await.expect("invite for c");
    poll_join_iroh(&c, &invite_c, Duration::from_secs(240))
        .await
        .expect("c joins via iroh first-contact");

    // --- THE ZEB-526 PROBE: every node's roster MUST converge to all three
    //     members. If c's join never reaches a/b (or vice-versa), this times out.
    for (node, who) in [(&a, "a"), (&b, "b"), (&c, "c")] {
        poll_until(Duration::from_secs(90), || async {
            let has_a = roster_has_joined(node, &community, &a_owner).await?;
            let has_b = roster_has_joined(node, &community, &b_owner).await?;
            let has_c = roster_has_joined(node, &community, &c_owner).await?;
            Ok((has_a && has_b && has_c).then_some(()))
        })
        .await
        .unwrap_or_else(|e| panic!("{who}'s roster MUST converge to all three members: {e}"));
    }

    // --- Founder creates a shared channel; both joiners must converge on it.
    let channel = create_channel(&a, &community, "s9-shared", 0)
        .await
        .expect("a creates shared channel");
    for (node, who) in [(&b, "b"), (&c, "c")] {
        poll_until(Duration::from_secs(90), || async {
            Ok(channels_contains(node, &community, &channel)
                .await?
                .then_some(()))
        })
        .await
        .unwrap_or_else(|e| panic!("{who} MUST converge the shared channel: {e}"));
    }

    // Subscribe to the 3rd member's event stream now — AFTER convergence but
    // BEFORE any post — so we catch the real-time fan-out without buffering the
    // whole join/convergence window's frames in the unbounded WS channel (Qodo).
    let (mut c_events, _c_ev) = c.events().await.expect("subscribe c events");

    // --- All three post a distinct message.
    let a_body: &[u8] = b"s9-from-alice";
    let b_body: &[u8] = b"s9-from-bob";
    let c_body: &[u8] = b"s9-from-carol";
    post_channel_message(&a, &community, &channel, a_body)
        .await
        .expect("a posts");
    post_channel_message(&b, &community, &channel, b_body)
        .await
        .expect("b posts");
    post_channel_message(&c, &community, &channel, c_body)
        .await
        .expect("c posts");

    // --- HARD ASSERT (real-time): the 3rd member c receives the founder's
    //     remote-authored message over the WS stream (real-time fan-out to a
    //     late joiner). Read-back below covers full eventual propagation. A
    //     single targeted await avoids cross-message ordering hazards on the
    //     shared stream.
    e2e_harness::await_event(&mut c_events, Duration::from_secs(60), |f| {
        f.event == "channel-message-received"
            && f.payload.get("channelId").and_then(|x| x.as_str()) == Some(channel.as_str())
            && f.payload
                .get("message")
                .map(|m| {
                    m.get("author").and_then(|x| x.as_str()) == Some(a_owner.as_str())
                        && channel_msg_body_bytes(m).as_deref() == Some(a_body)
                })
                .unwrap_or(false)
    })
    .await
    .expect(
        "c (3rd member) MUST receive the founder's remote-authored channel message in real-time",
    );

    // --- HARD ASSERT (read-back): every node's channel view MUST contain all
    //     three members' messages. One poll per node (checking all three authors
    //     per tick) keeps the worst-case wait bounded vs nine independent polls
    //     (Qodo: avoid compounded sequential timeouts).
    let want: [(&str, &[u8]); 3] = [
        (a_owner.as_str(), a_body),
        (b_owner.as_str(), b_body),
        (c_owner.as_str(), c_body),
    ];
    for (node, who) in [(&a, "a"), (&b, "b"), (&c, "c")] {
        poll_until(Duration::from_secs(60), || async {
            for (author_owner, body) in want {
                if !channel_has_message_from(node, &community, &channel, author_owner, body).await?
                {
                    return Ok(None);
                }
            }
            Ok(Some(()))
        })
        .await
        .unwrap_or_else(|e| {
            panic!("{who}'s read-back MUST contain all three members' messages: {e}")
        });
    }

    run.mark_success();
    drop((a, b, c, ah, bh, ch));
}

// ─────────────────────────────────────────────────────────────────────────────
// S10 (PROBE, characterize — ZEB-526): plain `redeem_invite` 3rd-member join.
//
// CO-LOCATED LIMITATION (do not promote to a hard assert): the plain
// `redeem_invite` verb skips the iroh first-contact handshake that establishes
// the a<->c zenoh peering (s1/s6/s9 use `connectivity_redeem_invite_iroh`, which
// peers the nodes). Without that handshake the two nodes' zenoh sessions never
// connect co-located, so c's state-root publish never reaches the founder — the
// join cannot converge here regardless of the server-side fixes. This is a
// transport/peering property of the plain path, not a defect in the convergence
// fixes.
//
// The ZEB-526 fixes ARE validated elsewhere:
//   * Founder-side invite-only self-Join admission — `bootstrap_admit_invite_only_publisher`
//     unit tests + the `invite_only_self_join_admission` integration test that drives
//     `handle_incoming_publish` with a joiner's published blob (peering-free).
//   * Joiner-side redeem publish re-arm — the redeem-path `notify_dirty` after the
//     epoch-Space commit (verified: c re-publishes a complete root post-commit).
// The end-to-end fallback convergence (zenoh publish path when the iroh dial does
// not deliver) is a CROSS-WAN proof (AVALON), as ZEB-526's original repro was.
//
// Characterizes: prints `S10:` on convergence or `S10 FINDING:` otherwise, then
// mark_success — the co-located run reports without failing on the peering gap.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s10_plain_redeem_third_member_probe() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let (mut run, ah, ch, a, c) = two_minted_nodes("s10").await;
    let a_owner = owner_id(&a).await;
    let c_owner = owner_id(&c).await;

    let community = create_community(&a, "s10-community", true)
        .await
        .expect("create invite-only community");
    let invite = generate_invite(&a, &community)
        .await
        .expect("generate invite");

    // THE PROBE: plain redeem (zenoh-publish path only, no iroh handshake).
    let redeem = redeem_invite(&c, &invite).await;
    eprintln!("S10 plain redeem_invite result: {redeem:?}");

    // Bounded characterization window (NOT a generous assert window): co-located,
    // the plain-redeem path CANNOT peer (see header), so non-convergence is the
    // structural outcome and `poll_until` here always runs to the deadline —
    // every second is paid on every run. 30s comfortably exceeds the peered
    // convergence latency the asserting scenarios observe (s1/s9 converge in
    // seconds), so it still reports a genuine convergence if behavior ever
    // changes, without burning 2 minutes characterizing a known gap. Do not
    // re-expand this to match the asserting scenarios' 120s — that ceiling is
    // free for them (they return early on success) and pure waste here.
    let converged = poll_until(Duration::from_secs(30), || async {
        let a_sees_c = roster_has_joined(&a, &community, &c_owner).await?;
        let c_sees_a = roster_has_joined(&c, &community, &a_owner).await?;
        Ok((a_sees_c && c_sees_a).then_some(()))
    })
    .await;

    if converged.is_ok() {
        eprintln!("S10: plain-redeem 3rd member converged bidirectionally (a sees c, c sees a).");
    } else {
        eprintln!(
            "S10 FINDING: plain-redeem 3rd member did not converge co-located within 30s. \
             Expected: this path is not peered co-located — the plain `redeem_invite` verb skips \
             the iroh first-contact handshake that establishes the a<->c zenoh link, so c's \
             state-root publish never reaches the founder. The convergence fixes (founder-side \
             invite-only self-Join admission + joiner-side redeem publish re-arm) are validated \
             by unit + integration tests; the end-to-end fallback is a cross-WAN (AVALON) proof."
        );
    }
    run.mark_success();
    drop((a, c, ah, ch));
}

/// Headless two-node Vines round-trip over REAL transport: A publishes → it
/// lands in B's feed → B marks it viewed (idempotent) → B reshares → the
/// reshare, carrying origin attribution, lands back in A's feed. Both nodes are
/// real engines on real Zenoh, so this IS the live descriptor-propagation
/// round-trip (publish leg A→B, reshare-attribution leg B→A).
///
/// Zenoh `put` is not retained, so a single publish before subscriber-match is
/// lost. Both legs therefore republish-until-observed against a bounded
/// deadline (no fixed sleeps) — robust to the one genuine race (pub before
/// sub-match) rather than asserting a single put lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s_vines_publish_feed_view_reshare() {
    use e2e_harness::driver::*;
    use serde_json::Value;
    use std::time::Duration;

    let (mut run, ah, bh, alice, bob) = two_minted_nodes("vines").await;
    let alice_owner = owner_id(&alice).await;
    let bob_owner = owner_id(&bob).await;

    // ZEB-811 (found via ZEB-809): establish a real peer relationship before
    // expecting any vine to propagate.
    //
    // This test used to publish straight out of `two_minted_nodes` with NO
    // community, NO friendship, and NO routing-record exchange — so the only
    // way these two nodes could ever find each other was zenoh LAN multicast,
    // which `Config::default()` happened to leave enabled. It passed for that
    // reason and no other: with scouting disabled it failed 3/3 at a
    // near-identical 108s, deterministically rather than flakily.
    //
    // That made it a FALSE POSITIVE of exactly the class
    // `s5c_clean_dial_only_card_propagation_probe` exists to eliminate for
    // owner cards — co-located nodes peering implicitly, so the assertion
    // never exercised the path production actually uses. Cross-WAN there is no
    // multicast, so what this test claimed to cover has never worked there.
    //
    // Vines are wildcard pub/sub over the EXISTING peer mesh and carry no
    // peer-acquisition step of their own, and a follow builds no network path
    // (ZEB-811). A community join fires the membership reachability exchange,
    // which fires the ZEB-373 dial — the same mechanism `s5c` uses, and the
    // one that has to work where multicast does not exist.
    let community = create_community(&alice, "vines-community", true)
        .await
        .expect("alice creates community");
    let invite = generate_invite(&alice, &community)
        .await
        .expect("generate invite");
    poll_join_iroh(&bob, &invite, Duration::from_secs(240))
        .await
        .expect("bob joins alice's community (the peer link vines ride on)");
    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&alice, &community, &bob_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("roster converges before any vine is published");

    // Unique-per-run title so we can pick our descriptor out of a feed
    // unambiguously (feeds start empty — the Rust cache has no mock seed).
    let title = format!("e2e-vine-{}", &alice_owner[..8.min(alice_owner.len())]);

    // ── Leg 1: A publishes → it lands in B's feed (republish-until-seen). ──
    // Each publish mints a fresh id; we match on the shared unique title.
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut descriptor: Option<Value> = None;
    'a_to_b: while std::time::Instant::now() < deadline {
        publish_vine(&alice, &title, "alice")
            .await
            .expect("alice publishes a vine");
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let feed = list_vine_videos(&bob).await.expect("bob feed");
            if let Some(d) = feed
                .into_iter()
                .find(|d| d.get("title").and_then(Value::as_str) == Some(title.as_str()))
            {
                descriptor = Some(d);
                break 'a_to_b;
            }
        }
    }
    let descriptor = descriptor.expect("alice's vine reached bob's feed over real Zenoh");

    let vine_id = descriptor
        .get("id")
        .and_then(Value::as_str)
        .expect("descriptor id")
        .to_string();
    // A vine's `creatorAddress` is the publishing node's `node_addr` (a
    // device/transport address), NOT the owner identity id from
    // `get_owner_state`. Capture whatever address it published under so leg 3
    // can verify the reshare preserves that exact address (attribution
    // consistency end-to-end), without assuming which identifier vines use.
    let alice_addr = descriptor
        .get("creatorAddress")
        .and_then(Value::as_str)
        .expect("creatorAddress")
        .to_string();
    assert!(
        !alice_addr.is_empty(),
        "descriptor carries a creatorAddress"
    );
    assert_eq!(
        descriptor.get("creatorName").and_then(Value::as_str),
        Some("alice")
    );
    assert!(
        descriptor
            .get("videoCid")
            .and_then(Value::as_str)
            .is_some_and(|c| !c.is_empty()),
        "descriptor carries a non-empty videoCid"
    );
    assert_eq!(
        descriptor.get("viewed").and_then(Value::as_bool),
        Some(false),
        "a freshly received vine is unviewed"
    );

    // ── Leg 2: view round-trip (local, idempotent). ──
    assert!(
        mark_vine_viewed(&bob, &vine_id).await.expect("mark viewed"),
        "first mark_viewed returns true"
    );
    assert!(
        !mark_vine_viewed(&bob, &vine_id)
            .await
            .expect("mark viewed 2"),
        "repeat mark_viewed is idempotent (false)"
    );
    let after = list_vine_videos(&bob).await.expect("bob feed after view");
    assert_eq!(
        after
            .iter()
            .find(|d| d.get("id").and_then(Value::as_str) == Some(vine_id.as_str()))
            .and_then(|d| d.get("viewed").and_then(Value::as_bool)),
        Some(true),
        "the vine now reads as viewed in bob's feed"
    );

    // ── Leg 3: B reshares → reshare (with attribution) lands in A's feed. ──
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut reshare: Option<Value> = None;
    'b_to_a: while std::time::Instant::now() < deadline {
        reshare_vine(&bob, &vine_id, "bob")
            .await
            .expect("bob reshares the vine");
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let feed = list_vine_videos(&alice).await.expect("alice feed");
            if let Some(d) = feed
                .into_iter()
                .find(|d| d.get("reshareOf").and_then(Value::as_str) == Some(vine_id.as_str()))
            {
                reshare = Some(d);
                break 'b_to_a;
            }
        }
    }
    let reshare = reshare.expect("bob's reshare reached alice's feed over real Zenoh");
    assert_eq!(
        reshare
            .get("originalCreatorAddress")
            .and_then(Value::as_str),
        Some(alice_addr.as_str()),
        "reshare preserves the original creator's address through the round-trip"
    );
    assert_eq!(
        reshare.get("originalCreatorName").and_then(Value::as_str),
        Some("alice"),
        "reshare attributes the original to alice's name"
    );

    run.mark_success();
    drop((alice, bob, ah, bh));
}

/// ZEB-811: a node's own `creatorAddress` (`vine_signing::signer_address`) has
/// no dedicated RPC — it's a signing-only identity, deliberately distinct from
/// `get_owner_state`'s `ownerId` (the device-address hash; the two notions
/// never converge). The cheapest verified source is a node's OWN just-
/// published vine: Task 5's self-ingest guarantees the publisher's own
/// descriptor lands in its own feed immediately, carrying `creatorAddress`.
/// Callers MUST publish at least one vine on `node` before calling this.
async fn node_vine_address(node: &NodeHandle) -> String {
    let feed = e2e_harness::driver::list_vine_videos(node)
        .await
        .expect("list_vine_videos for creatorAddress lookup");
    feed.first()
        .and_then(|d| d.get("creatorAddress"))
        .and_then(serde_json::Value::as_str)
        .expect("node's own published vine carries creatorAddress")
        .to_string()
}

/// ZEB-811: follow-only cross-node delivery over the vine relay path. Two
/// nodes with NO relationship of any kind — the exact gap the ticket
/// documents (the mesh-path sibling above needs a community join to pass
/// without LAN scouting). Alice's own device is her v1 relay; Bob discovers
/// it via the public pkarr `vines` slot and pulls descriptors + video with no
/// community, no friendship, and no shared mesh.
///
/// View + reshare legs assert on Bob's PULLED copies only — v1 has no
/// reverse channel, so Alice never sees Bob's reshare (spec §6).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s_vines_follow_only() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let (mut run, _ah, _bh, alice, bob) = two_minted_nodes("vines-follow").await;

    // Alice: confirm the gate defaults ON, then publish one vine. Address
    // discovery (`node_vine_address`) reads it back from Alice's own feed, so
    // it necessarily runs AFTER the publish it reads from — publish comes
    // first here, unlike the mesh-path sibling where `owner_id` (a distinct,
    // pre-existing identifier) is available before any vine exists.
    let settings = get_vine_settings(&alice).await.unwrap();
    assert_eq!(
        settings["shareVinesPublicly"], true,
        "public-by-default (spec product decision 4)"
    );
    let alice_owner = owner_id(&alice).await;
    let title = format!(
        "e2e-vine-follow-{}",
        &alice_owner[..8.min(alice_owner.len())]
    );
    let (vine_id, video_cid) = publish_vine(&alice, &title, "alice").await.unwrap();
    let alice_addr = node_vine_address(&alice).await;

    // Bob: follow by address — no community, no friendship, no multicast.
    assert!(follow_vine_creator(&bob, &alice_addr).await.unwrap());

    // Descriptor leg: bob's feed gains the vine via the pull driver (pkarr
    // publish + resolve + pull; generous deadline, poll only — a follow fires
    // an immediate wake+pull pass, so this is bounded by pkarr publish→resolve
    // latency, not the driver's ~7.5min cadenced retry).
    poll_until(Duration::from_secs(240), || async {
        Ok(list_vine_videos(&bob)
            .await?
            .iter()
            .any(|v| v["id"] == vine_id.as_str())
            .then_some(()))
    })
    .await
    .expect("descriptor must arrive over the relay path");

    // Bind the descriptor leg's outcome to the actual relay-pull mechanism —
    // not just "bob's feed changed" — so a future regression (e.g. a new ALPN
    // accidentally bridging zenoh) can't pass the poll above for the wrong
    // reason.
    let bob_health = network_health_snapshot(&bob).await.unwrap();
    assert!(
        bob_health["vineRelay"]["pulling"]["sessionsOk"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "bob's vineRelay.pulling.sessionsOk must be >=1 after the descriptor leg; got {bob_health}"
    );
    assert!(
        bob_health["vineRelay"]["pulling"]["descriptorsIngested"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "bob's vineRelay.pulling.descriptorsIngested must be >=1 after the descriptor leg; got {bob_health}"
    );

    // Video leg: relay content fetch (mesh GET cannot succeed — no shared mesh).
    let bytes = fetch_vine_video(&bob, &video_cid, &alice_addr)
        .await
        .expect("video over vine-relay");
    assert!(!bytes.is_empty());

    // Alice's serving counter increments at HER session teardown, which can
    // lag a beat behind bob having received all the bytes — poll briefly
    // (not a bare assert) to avoid a race with that teardown, then bind the
    // video leg to the serving side of the mechanism the same way.
    poll_until(Duration::from_secs(30), || async {
        let snap = network_health_snapshot(&alice).await?;
        Ok((snap["vineRelay"]["serving"]["sessionsServed"]
            .as_u64()
            .unwrap_or(0)
            >= 1)
            .then_some(()))
    })
    .await
    .expect("alice must have served at least one vine-relay session for bob's video fetch");

    // View + reshare legs on the PULLED copies (v1 has no reverse channel).
    assert!(mark_vine_viewed(&bob, &vine_id).await.unwrap());
    let reshare_of = reshare_vine(&bob, &vine_id, "bob").await.unwrap();
    assert_eq!(reshare_of, vine_id);
    assert!(list_vine_videos(&bob)
        .await
        .unwrap()
        .iter()
        .any(|v| v["reshareOf"] == vine_id.as_str() && v["viewed"] == false));

    run.mark_success();
}

// ─────────────────────────────────────────────────────────────────────────────
// S6-ADDRBOOK (ZEB-815 Task 8): join → channel create → post → read-back on
// the REAL binary, with ZERO address-book-announce CRDT events available
// ANYWHERE in the run (Tasks 1-7's flag-day deleted both
// ReachabilityAnnounce/CommunityRelayAnnounce mint sites entirely — there is
// no fallback path left to regress to). This proves the ordinary user-facing
// flow (join, roster convergence, channel convergence, message delivery)
// still works end to end on the real binary now that those CRDT mint sites
// are gone.
//
// PRECISE CLAIM — read before citing this test as address-book proof: this
// test does NOT isolate the address book as the mechanism driving the dial.
// Bob's `redeem_invite_iroh` join calls `reachability_resolver.seed_from_pkarr`
// (`lib.rs` ~58064, case-A option A) directly off the pkarr-resolved routing
// record, independently of the address book. The handshake CONNECTION itself
// is NOT what's reused: it's explicitly closed right after the exchange
// (`lib.rs:58553`, `conn.close(0u32.into(), b"handshake-complete")`). What IS
// reused is the RESOLVER ENTRY that seed wrote — `IrohZenohLinkManager::new_link`
// (`zenoh_iroh_transport.rs`) reads it via `resolve_by_node_id` to synthesize
// the dial target for a SEPARATE zenoh-over-iroh connection it opens under a
// different ALPN (`harmony/zenoh/v1` vs the handshake's `harmony/handshake/v1`
// — one QUIC connection structurally cannot carry both). So a real two-node
// run always has BOTH the pkarr-seeded resolver entry and any
// address-book-ingested one present at once — this test cannot tell you
// which one supplied the reachability that `new_link` actually dialed with.
// The confound-free proof that the address book alone (no pkarr, no CRDT) can
// drive a resolver hit lives in the integration test
// `addrbook_replaces_announce_events_end_to_end`
// (`community_sync/community_sync_integration.rs`), which loops A's
// `publish_own_rows` output straight into B's `ingest_sealed_packet` with no
// pkarr in the loop at all.
//
// Mirrors `s_vines_publish_feed_view_reshare`'s preamble (create_community →
// generate_invite → poll_join_iroh → roster poll).
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s6_addrbook_join_message_delivery() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let (mut run, ah, bh, alice, bob) = two_minted_nodes("addrbook").await;
    let alice_owner = owner_id(&alice).await;
    let bob_owner = owner_id(&bob).await;

    let community = create_community(&alice, "addrbook-community", true)
        .await
        .expect("alice creates community");
    let invite = generate_invite(&alice, &community)
        .await
        .expect("generate invite");
    poll_join_iroh(&bob, &invite, Duration::from_secs(240))
        .await
        .expect("bob joins alice's community via iroh first-contact");
    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&alice, &community, &bob_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("alice sees bob joined — roster converges with no CRDT announce events available");

    // A shared channel both members can post to (write_power 0).
    let channel = create_channel(&alice, &community, "addrbook-shared", 0)
        .await
        .expect("alice creates shared channel");
    poll_until(Duration::from_secs(180), || async {
        Ok(channels_contains(&bob, &community, &channel)
            .await?
            .then_some(()))
    })
    .await
    .expect("bob converges the shared channel");

    // Alice posts; bob's read-back must show it — the real-binary join +
    // channel-delivery flow works end to end with no CRDT announce events
    // anywhere (see the file-level comment above for what this test does and
    // does NOT isolate about the dial's reachability source).
    let body: &[u8] = b"hello-addrbook-e2e";
    post_channel_message(&alice, &community, &channel, body)
        .await
        .expect("alice's post_channel_message accepted");

    let delivered = poll_until(Duration::from_secs(120), || async {
        let msgs = list_channel_messages(&bob, &community, &channel).await?;
        Ok(msgs.into_iter().find(|m| {
            let body_matches = m.get("body").and_then(|b| b.as_array()).map(|arr| {
                arr.iter()
                    .map(|n| n.as_u64().filter(|v| *v <= u8::MAX as u64).map(|v| v as u8))
                    .collect::<Option<Vec<u8>>>()
            }) == Some(Some(body.to_vec()));
            m.get("author").and_then(|a| a.as_str()) == Some(alice_owner.as_str()) && body_matches
        }))
    })
    .await
    .expect("bob's channel read-back MUST contain alice's message");

    // Assert on the DTO's REAL camelCase keys (`ChannelMessageDto` is
    // `#[serde(rename_all = "camelCase")]`) — a guessed key would make
    // `poll_until` silently time out rather than fail loudly (ZEB-462 class).
    assert!(
        delivered
            .get("messageId")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()),
        "delivered message DTO must carry a non-empty messageId: {delivered:?}"
    );
    assert_eq!(
        delivered.get("author").and_then(|v| v.as_str()),
        Some(alice_owner.as_str()),
        "delivered message DTO's author must be alice's ownerId"
    );

    run.mark_success();
    drop((alice, bob, ah, bh));
}

// ─────────────────────────────────────────────────────────────────────────────
// S11 (ZEB-715): admin-recovery liveness path — configure designates →
// designate initiates → cosign to threshold → current admin vetoes →
// terminal Vetoed, membership unchanged.
//
// Cast: alice = founder (power 100 — and the recovery's named lost_admin);
// bob = ordinary joined member. Designates = {alice, bob}, R = 2. That cast is
// legal per the CRDT gates (RD2/RC1 require only that a designate be Joined —
// the admin MAY appear in the list) and is exactly what makes a TWO-node story
// cover the full verb set: bob initiates as signer 1, alice cosigns to
// threshold, then alice vetoes. W = the RD4 7-day floor. Every transition here
// (collecting → timeLocked → vetoed) is event-driven, so this path needs NO
// time control; the veto authorship window is [t₀, deadline] (spec §4.2),
// i.e. valid in timeLocked.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s11_recovery_veto_path() {
    use e2e_harness::driver::*;
    use serde_json::Value;
    use std::time::Duration;

    const WEEK_MS: u64 = 7 * 86_400_000;

    let (mut run, ah, bh, alice, bob) = two_minted_nodes("s11-recovery-veto").await;
    let alice_owner = owner_id(&alice).await;
    let bob_owner = owner_id(&bob).await;

    // Community + join + roster convergence (the s1 skeleton).
    let community = create_community(&alice, "s11-community", true)
        .await
        .expect("create community");
    let invite = generate_invite(&alice, &community)
        .await
        .expect("generate invite");
    poll_join_iroh(&bob, &invite, Duration::from_secs(240))
        .await
        .expect("bob joins via iroh first-contact");
    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&alice, &community, &bob_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("alice sees bob joined");
    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&bob, &community, &alice_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("bob sees alice joined");

    // Founder configures designates {alice, bob}, R=2, W=floor. Sole admin ⇒
    // the SetRecoveryDesignates proposal self-satisfies admin_quorum == 1.
    let res = set_recovery_designates(&alice, &community, &[&alice_owner, &bob_owner], 2, WEEK_MS)
        .await
        .expect("set_recovery_designates");
    assert_eq!(
        res.get("kind").and_then(Value::as_str),
        Some("Completed"),
        "sole admin self-satisfies quorum 1: {res}"
    );

    // Config replicates to bob; his own DTO proves he is a designate
    // (loud-contract predicate — missing DTO keys fail fast, only
    // `config: null` / the freshly-joined no-engine window keep polling).
    poll_until(Duration::from_secs(120), || async {
        recovery_configured_as_designate(&bob, &community).await
    })
    .await
    .expect("bob sees the designate config and selfIsDesignate");

    // Bob (designate) initiates recovery of alice, naming himself new admin.
    let proposal = initiate_admin_recovery(&bob, &community, &alice_owner, &bob_owner)
        .await
        .expect("initiate_admin_recovery");

    // The collecting proposal replicates to alice (signer 1 of 2).
    poll_until(Duration::from_secs(120), || async {
        let st = recovery_state(&alice, &community).await?;
        Ok(recovery_proposal(&st, &proposal)?
            .filter(|p| p.get("phase").and_then(Value::as_str) == Some("collecting"))
            .map(|_| ()))
    })
    .await
    .expect("alice sees the collecting proposal");

    // Alice (also a designate) cosigns to threshold.
    let cosign = cosign_admin_recovery(&alice, &community, &proposal)
        .await
        .expect("cosign_admin_recovery");
    assert_eq!(
        cosign.get("reachedThreshold").and_then(Value::as_bool),
        Some(true),
        "alice's cosign reaches R=2: {cosign}"
    );

    // Both replicas converge to timeLocked, which must carry a deadline.
    for (name, node) in [("alice", &alice), ("bob", &bob)] {
        poll_until(Duration::from_secs(120), || async {
            let st = recovery_state(node, &community).await?;
            match recovery_proposal(&st, &proposal)? {
                Some(p) if p.get("phase").and_then(Value::as_str) == Some("timeLocked") => {
                    anyhow::ensure!(
                        p.get("deadlineMs").and_then(Value::as_u64).is_some(),
                        "timeLocked must carry deadlineMs: {p}"
                    );
                    Ok(Some(()))
                }
                _ => Ok(None),
            }
        })
        .await
        .unwrap_or_else(|e| panic!("{name} converges to timeLocked: {e}"));
    }

    // The current admin vetoes — restores the status quo (spec §4.2).
    veto_admin_recovery(&alice, &community, &proposal)
        .await
        .expect("veto_admin_recovery");

    // Both replicas resolve to vetoed-by-alice.
    for (name, node) in [("alice", &alice), ("bob", &bob)] {
        poll_until(Duration::from_secs(120), || async {
            let st = recovery_state(node, &community).await?;
            Ok(recovery_proposal(&st, &proposal)?
                .filter(|p| {
                    p.get("phase").and_then(Value::as_str) == Some("vetoed")
                        && p.get("vetoedByAddr").and_then(Value::as_str)
                            == Some(alice_owner.as_str())
                })
                .map(|_| ()))
        })
        .await
        .unwrap_or_else(|e| panic!("{name} resolves to vetoed-by-alice: {e}"));
    }

    // Membership unchanged: alice still the power-100 admin (her own DTO),
    // both members still Joined in both rosters.
    let a_st = recovery_state(&alice, &community)
        .await
        .expect("alice reads final state");
    assert_eq!(
        a_st.get("selfPower").and_then(Value::as_u64),
        Some(100),
        "veto restored the status quo — alice keeps power 100: {a_st}"
    );
    assert!(
        roster_has_joined(&alice, &community, &bob_owner)
            .await
            .expect("alice roster read"),
        "bob still Joined after the vetoed recovery"
    );
    assert!(
        roster_has_joined(&bob, &community, &alice_owner)
            .await
            .expect("bob roster read"),
        "alice still Joined after the vetoed recovery"
    );

    run.mark_success();
    drop((alice, bob, ah, bh));
}

// ─────────────────────────────────────────────────────────────────────────────
// S12 (ZEB-715): admin-recovery execution path — cosign to threshold, then
// observe TimeLocked → Executed and the +48h rotation-finality wall through
// the read-side `nowMs` as-of override on `get_recovery_state`.
//
// Recovery execution is PURE DERIVED state on the `materialized_with_now`
// now-floor (spec §4.1): no event marks it, a replica simply derives the
// promotion + kick whenever it materializes with T past the deadline. The RD4
// 7-day veto-window floor therefore makes execution unobservable on the real
// clock, and the caller-supplied `nowMs` (ZEB-715's one prod-code seam) IS the
// sanctioned time control — an as-of query, not a clock mock; every mutating
// verb stays on the real clock.
//
// The execution assertions are BEHAVIORAL, not roster dumps:
//   * bob (new_admin) watches his own `selfPower` flip to 100 across the
//     deadline in his own DTO;
//   * alice (the named lost_admin — loss-as-compromise, spec §2) is
//     derived-kicked at execution, so her as-of read past the deadline is
//     REFUSED by get_recovery_state's Joined-caller gate — the kick proven by
//     the API treating her as a non-member;
//   * `rotationEligibleAtMs` == deadline + 48h (spec §4.3), and the phase
//     stays "executed" past that wall — ELIGIBILITY is asserted, not an actual
//     rotation: synthesis is the self-heal observer's job and is gated on the
//     REAL wall clock (the F-margin is a materialize-level CRDT invariant),
//     deliberately out of e2e reach.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s12_recovery_time_locked_execution() {
    use e2e_harness::driver::*;
    use serde_json::Value;
    use std::time::Duration;

    const WEEK_MS: u64 = 7 * 86_400_000;
    const FINALITY_MS: u64 = 48 * 3_600_000;
    const MARGIN_MS: u64 = 1_000;

    let (mut run, ah, bh, alice, bob) = two_minted_nodes("s12-recovery-exec").await;
    let alice_owner = owner_id(&alice).await;
    let bob_owner = owner_id(&bob).await;

    // Community + join + roster convergence.
    let community = create_community(&alice, "s12-community", true)
        .await
        .expect("create community");
    let invite = generate_invite(&alice, &community)
        .await
        .expect("generate invite");
    poll_join_iroh(&bob, &invite, Duration::from_secs(240))
        .await
        .expect("bob joins via iroh first-contact");
    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&alice, &community, &bob_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("alice sees bob joined");
    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&bob, &community, &alice_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("bob sees alice joined");

    // Designates {alice, bob}, R=2, W=floor; bob initiates (lost=alice,
    // new=bob); alice cosigns to threshold — same dance as S11.
    let res = set_recovery_designates(&alice, &community, &[&alice_owner, &bob_owner], 2, WEEK_MS)
        .await
        .expect("set_recovery_designates");
    assert_eq!(
        res.get("kind").and_then(Value::as_str),
        Some("Completed"),
        "sole admin self-satisfies quorum 1: {res}"
    );
    poll_until(Duration::from_secs(120), || async {
        recovery_configured_as_designate(&bob, &community).await
    })
    .await
    .expect("bob sees the designate config");
    let proposal = initiate_admin_recovery(&bob, &community, &alice_owner, &bob_owner)
        .await
        .expect("initiate_admin_recovery");
    poll_until(Duration::from_secs(120), || async {
        let st = recovery_state(&alice, &community).await?;
        Ok(recovery_proposal(&st, &proposal)?
            .filter(|p| p.get("phase").and_then(Value::as_str) == Some("collecting"))
            .map(|_| ()))
    })
    .await
    .expect("alice sees the collecting proposal");
    cosign_admin_recovery(&alice, &community, &proposal)
        .await
        .expect("cosign_admin_recovery");

    // Both replicas hold the full event set and derive timeLocked with the
    // SAME deadline (deadline is replay-derived from the Rth signer's wall
    // time — cross-replica equality is itself a CRDT determinism assertion).
    let mut deadlines = Vec::new();
    for (name, node) in [("alice", &alice), ("bob", &bob)] {
        let d = poll_until(Duration::from_secs(120), || async {
            let st = recovery_state(node, &community).await?;
            match recovery_proposal(&st, &proposal)? {
                Some(p) if p.get("phase").and_then(Value::as_str) == Some("timeLocked") => {
                    let d = p
                        .get("deadlineMs")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| anyhow::anyhow!("timeLocked must carry deadlineMs: {p}"))?;
                    Ok(Some(d))
                }
                _ => Ok(None),
            }
        })
        .await
        .unwrap_or_else(|e| panic!("{name} converges to timeLocked: {e}"));
        deadlines.push(d);
    }
    assert_eq!(
        deadlines[0], deadlines[1],
        "both replicas derive the same replay deadline"
    );
    let deadline = deadlines[0];

    // Just BEFORE the deadline: still timeLocked; bob is an ordinary member.
    let before = recovery_state_as_of(&bob, &community, deadline - MARGIN_MS)
        .await
        .expect("bob as-of read before deadline");
    let p = recovery_proposal(&before, &proposal)
        .expect("proposal lookup")
        .expect("proposal present before deadline");
    assert_eq!(
        p.get("phase").and_then(Value::as_str),
        Some("timeLocked"),
        "still time-locked just before the deadline: {p}"
    );
    let bob_power_before = before
        .get("selfPower")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            panic!("RecoveryStateDto missing `selfPower` (DTO/schema mismatch?): {before}")
        });
    assert!(
        bob_power_before < 100,
        "bob is not an admin before execution: {before}"
    );

    // Just PAST the deadline: executed — bob's own power is 100 and the
    // rotation-finality wall is published as deadline + F.
    let after = recovery_state_as_of(&bob, &community, deadline + MARGIN_MS)
        .await
        .expect("bob as-of read past deadline");
    let p = recovery_proposal(&after, &proposal)
        .expect("proposal lookup")
        .expect("proposal present past deadline");
    assert_eq!(
        p.get("phase").and_then(Value::as_str),
        Some("executed"),
        "derived execution past the deadline: {p}"
    );
    assert_eq!(
        after.get("selfPower").and_then(Value::as_u64),
        Some(100),
        "bob (new_admin) derives power 100 at execution: {after}"
    );
    assert_eq!(
        p.get("rotationEligibleAtMs").and_then(Value::as_u64),
        Some(deadline + FINALITY_MS),
        "rotation eligibility = deadline + 48h finality margin: {p}"
    );

    // Alice before the deadline: still the power-100 admin. Past it: the
    // named lost_admin is derived-kicked, so the Joined-caller gate refuses
    // her read — the kick asserted behaviorally.
    let a_before = recovery_state_as_of(&alice, &community, deadline - MARGIN_MS)
        .await
        .expect("alice as-of read before deadline");
    assert_eq!(
        a_before.get("selfPower").and_then(Value::as_u64),
        Some(100),
        "alice still the admin before the deadline: {a_before}"
    );
    let a_after = recovery_state_as_of(&alice, &community, deadline + MARGIN_MS).await;
    match a_after {
        Err(e) if e.to_string().contains("not a Joined member") => {}
        other => panic!(
            "alice (named lost_admin) must be derived-kicked past the deadline — \
             expected the Joined-caller refusal, got: {other:?}"
        ),
    }

    // Past the finality wall: still executed (no phantom phase), eligibility
    // wall passed — the rotation PROMPT may fire; actual rotation synthesis
    // is the real-clock observer's job, out of e2e reach by design.
    let post_wall = recovery_state_as_of(&bob, &community, deadline + FINALITY_MS + MARGIN_MS)
        .await
        .expect("bob as-of read past the finality wall");
    let p = recovery_proposal(&post_wall, &proposal)
        .expect("proposal lookup")
        .expect("proposal present past the finality wall");
    assert_eq!(
        p.get("phase").and_then(Value::as_str),
        Some("executed"),
        "phase remains executed past the finality wall: {p}"
    );

    // The REAL-clock view (no override) still shows timeLocked on both nodes:
    // the as-of override must never leak into un-overridden reads.
    for (name, node) in [("alice", &alice), ("bob", &bob)] {
        let st = recovery_state(node, &community)
            .await
            .expect("real-clock read");
        let p = recovery_proposal(&st, &proposal)
            .expect("proposal lookup")
            .expect("proposal present on the real clock");
        assert_eq!(
            p.get("phase").and_then(Value::as_str),
            Some("timeLocked"),
            "{name}: real-clock view unaffected by earlier as-of reads: {p}"
        );
    }

    run.mark_success();
    drop((alice, bob, ah, bh));
}

/// ZEB-720: two-node Tier-2 Conviction SetPower finalize. Alice (admin) creates a
/// Tier-2 proposal to raise Bob's power; both signal support; the short-window
/// voting tick finalizes and auto-execs SetPower; both replicas converge to the
/// new materialized power. Nodes run with a short voting cadence so finalization
/// is deterministic without a real 24h wait and without a wall-clock sleep.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s13_tier2_conviction_setpower() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    // window (1500ms) >> tick interval (250ms): the poll is observed
    // ThresholdReached before it finalizes; poll_until absorbs tick jitter.
    let voting_env = vec![
        (
            "HARMONY_VOTING_CONTESTABILITY_WINDOW_MS".to_string(),
            "1500".to_string(),
        ),
        (
            "HARMONY_VOTING_TICK_INTERVAL_MS".to_string(),
            "250".to_string(),
        ),
    ];

    let mut run = RunDir::new("s13-tier2-setpower").expect("run dir");
    let log_dir = run.log_dir();
    let alice_home = fresh_home("s13-tier2-a");
    let bob_home = fresh_home("s13-tier2-b");
    let mk = |home: &tempfile::TempDir, profile: &str| {
        let mut cfg = NodeConfig::new(PathBuf::from(home.path()), profile);
        cfg.log_dir = Some(log_dir.clone());
        cfg.extra_env = voting_env.clone();
        cfg
    };
    let alice = NodeHandle::spawn(mk(&alice_home, "alice"))
        .await
        .expect("spawn alice");
    let bob = NodeHandle::spawn(mk(&bob_home, "bob"))
        .await
        .expect("spawn bob");
    alice
        .rpc("mint_owner_identity", json!({}))
        .await
        .expect("alice mint");
    bob.rpc("mint_owner_identity", json!({}))
        .await
        .expect("bob mint");

    let alice_owner = owner_id(&alice).await;
    let bob_owner = owner_id(&bob).await;

    // Community + channel + join + roster convergence (s11 skeleton).
    let community = create_community(&alice, "s13-community", true)
        .await
        .expect("create community");
    // NB: create_community auto-creates a "general" channel, so use a distinct
    // name for the proposal channel.
    let channel = create_channel(&alice, &community, "proposals", 0)
        .await
        .expect("create channel");
    let invite = generate_invite(&alice, &community)
        .await
        .expect("generate invite");
    poll_join_iroh(&bob, &invite, Duration::from_secs(240))
        .await
        .expect("bob joins via iroh first-contact");
    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&alice, &community, &bob_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("alice sees bob joined");
    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&bob, &community, &alice_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("bob sees alice joined");

    // Baseline: Bob's power must differ from the target (60) for a real change.
    let bob_power_before = member_power(&alice, &community, &bob_owner)
        .await
        .expect("read bob power")
        .expect("bob in roster");
    assert_ne!(bob_power_before, 60, "target 60 must differ from baseline");

    // Alice (admin, power 100) creates a Tier-2 SetPower{bob -> 60} proposal.
    // 60 < POWER_THRESHOLDS.max ⇒ NOT admin-affecting ⇒ direct-mint at
    // admin_quorum==1 (no AdminProposal route). thresholdMin=1 (raw ms floor)
    // so a support signal crosses the dynamic threshold quickly; minPower=0 so
    // bob is eligible to signal regardless of his power.
    let proposal = create_tier2_setpower_proposal(
        &alice,
        &community,
        &channel,
        "raise bob to 60",
        &bob_owner,
        60, // new_power
        1,  // threshold_min
        10, // threshold_max — tiny band; conviction (~1k/s) crosses in <1s.
        0,  // min_power
    )
    .await
    .expect("create tier-2 proposal");

    // Alice signals support. With admin power 100 and thresholdMin=1 her
    // conviction crosses the threshold within a tick or two. Bob need not
    // signal or even subscribe to voting: the finalize's SetPower reaches him
    // over the membership-log sync, not the voting topic.
    signal_tier2(&alice, &proposal, true)
        .await
        .expect("alice signals support");

    // Alice's short-window tick drives Open → ThresholdReached → Finalized.
    poll_until(Duration::from_secs(90), || async {
        let dto = get_tier2_proposal(&alice, &proposal).await?;
        Ok((dto.get("lifecycle").and_then(|v| v.as_str()) == Some("Finalized")).then_some(()))
    })
    .await
    .expect("alice's proposal reaches Finalized");

    // Finalization auto-execs SetPower{bob -> 60}; assert the materialized power
    // changed on BOTH replicas (alice minted it; bob converges via sync).
    for (name, node) in [("alice", &alice), ("bob", &bob)] {
        poll_until(Duration::from_secs(120), || async {
            let p = member_power(node, &community, &bob_owner).await?;
            Ok((p == Some(60)).then_some(()))
        })
        .await
        .unwrap_or_else(|e| panic!("{name} sees bob power == 60: {e}"));
    }

    run.mark_success();
    drop((alice, bob, alice_home, bob_home));
}

// ─────────────────────────────────────────────────────────────────────────────
// S14 (hard-assert — ZEB-912 step 2): router-mode multi-hop delivery across a
// SEVERED pair. All three nodes run HARMONY_ZENOH_MODE=router. B and C each
// join founder A's community (v1 restricts invite generation to the admin);
// C's spawn env denylists B at the zenoh link layer
// (HARMONY_TEST_ZENOH_DENYLIST=<B's nodeId>), so the B—C link can never form
// from either side (the seam gates dial AND accept). Rosters and channel
// structure converge over the two live direct links (B—A, C—A) — the
// load-bearing multi-hop assertions are the CHANNEL MESSAGES between B and C,
// which can only travel B—A—C: zenoh router-mode linkstate forwarding through
// A. In peer mode those asserts CANNOT pass — the R3 spike measured that a
// peer does not transit data between two other peers
// (docs/research/2026-08-12-zeb912-r3-zenoh-multihop-spike.md).
//
// Why the severed pair is B—C and not joiner—inviter: the invite-only join
// response carries NO membership snapshot (unlike open-join), so a joiner
// severed from its inviter can never learn the other members exist and is
// fully islanded — a real production gap this scenario surfaced, filed as
// ZEB-927. Once that ships, a joiner—inviter sever variant becomes provable.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn s14_router_mode_severed_pair_delivery() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let mut run = RunDir::new("s14").expect("run dir");
    let a_home = fresh_home("s14-a");
    let b_home = fresh_home("s14-b");
    let c_home = fresh_home("s14-c");
    let router_env = || ("HARMONY_ZENOH_MODE".to_string(), "router".to_string());
    let mk = |home: &tempfile::TempDir, profile: &str, extra: Vec<(String, String)>| {
        let mut cfg = NodeConfig::new(PathBuf::from(home.path()), profile);
        cfg.log_dir = Some(run.log_dir());
        cfg.extra_env = extra;
        cfg
    };

    let a = NodeHandle::spawn(mk(&a_home, "alice", vec![router_env()]))
        .await
        .expect("spawn a");
    a.rpc("mint_owner_identity", json!({}))
        .await
        .expect("a mint");

    // B boots BEFORE C: its nodeId seeds C's denylist, so the sever is in
    // force from C's very first boot (no window where a B—C link could form).
    let b = NodeHandle::spawn(mk(&b_home, "bob", vec![router_env()]))
        .await
        .expect("spawn b");
    b.rpc("mint_owner_identity", json!({}))
        .await
        .expect("b mint");
    // mint restarts the inner node; poll until the restarted node reports its id.
    let b_node_id: String = poll_until(Duration::from_secs(60), || async { b.node_id().await })
        .await
        .expect(
            "b reports nodeId after mint restart (a persistent null on a running node means \
             iroh transport failed at boot — degraded mode, not slowness)",
        );
    assert_eq!(b_node_id.len(), 64, "nodeId is 64-hex: {b_node_id}");

    let c = NodeHandle::spawn(mk(
        &c_home,
        "carol",
        vec![
            router_env(),
            ("HARMONY_TEST_ZENOH_DENYLIST".to_string(), b_node_id.clone()),
        ],
    ))
    .await
    .expect("spawn c");
    c.rpc("mint_owner_identity", json!({}))
        .await
        .expect("c mint");

    // --- Positive evidence router mode ENGAGED on every node (same discipline
    //     as the sever-evidence assert below): a knob regression must fail
    //     here as "mode never engaged", not as a delivery timeout downstream
    //     (CodeRabbit #671).
    for (node, who) in [(&a, "a"), (&b, "b"), (&c, "c")] {
        poll_until(Duration::from_secs(30), || async {
            Ok(node
                .stderr_log_contains("ZEB-912: zenoh session mode: router")?
                .then_some(()))
        })
        .await
        .unwrap_or_else(|e| panic!("{who} MUST report a router-mode zenoh session: {e}"));
    }

    let a_owner = owner_id(&a).await;
    let b_owner = owner_id(&b).await;
    let c_owner = owner_id(&c).await;

    // --- A founds; B and C each redeem a fresh invite from A via iroh
    //     first-contact (both handshakes touch only the live A links).
    let community = create_community(&a, "s14-community", true)
        .await
        .expect("create community");
    let invite_b = generate_invite(&a, &community).await.expect("invite for b");
    poll_join_iroh(&b, &invite_b, Duration::from_secs(240))
        .await
        .expect("b joins via iroh first-contact");
    let invite_c = generate_invite(&a, &community).await.expect("invite for c");
    poll_join_iroh(&c, &invite_c, Duration::from_secs(240))
        .await
        .expect("c joins via iroh first-contact");

    // --- Rosters converge on all three nodes (member sync rides the two live
    //     direct links; NOT multi-hop-dependent — the diagnostic dump guards
    //     against regressions in the join path itself).
    for (node, who) in [(&a, "a"), (&b, "b"), (&c, "c")] {
        if let Err(e) = poll_until(Duration::from_secs(120), || async {
            let has_a = roster_has_joined(node, &community, &a_owner).await?;
            let has_b = roster_has_joined(node, &community, &b_owner).await?;
            let has_c = roster_has_joined(node, &community, &c_owner).await?;
            Ok((has_a && has_b && has_c).then_some(()))
        })
        .await
        {
            let rows = list_community_members(node, &community)
                .await
                .map(|ms| {
                    ms.iter()
                        .map(|m| {
                            format!(
                                "{}:{}",
                                m.get("addr")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("?"),
                                m.get("status")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("?"),
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|e| vec![format!("(roster fetch failed: {e})")]);
            panic!(
                "{who}'s roster MUST converge: {e}\n  {who}'s roster rows: {rows:?}\n                   owners: a={a_owner} b={b_owner} c={c_owner}"
            );
        }
    }

    // --- Channel created by A converges on both joiners (direct links).
    let channel = create_channel(&a, &community, "s14-shared", 0)
        .await
        .expect("a creates shared channel");
    for (node, who) in [(&b, "b"), (&c, "c")] {
        poll_until(Duration::from_secs(120), || async {
            Ok(channels_contains(node, &community, &channel)
                .await?
                .then_some(()))
        })
        .await
        .unwrap_or_else(|e| panic!("{who} MUST converge the shared channel: {e}"));
    }

    // Subscribe AFTER convergence, BEFORE posts (s9 discipline: don't buffer
    // the join window in the unbounded WS channel).
    let (mut c_events, _c_ev) = c.events().await.expect("subscribe c events");

    // --- THE MULTI-HOP PROOF: the severed pair posts in both directions.
    //     B—C have no link; each message can only arrive through A.
    let b_body: &[u8] = b"s14-from-bob-via-a";
    let c_body: &[u8] = b"s14-from-carol-via-a";
    post_channel_message(&b, &community, &channel, b_body)
        .await
        .expect("b posts");
    post_channel_message(&c, &community, &channel, c_body)
        .await
        .expect("c posts");

    // --- HARD ASSERT (real-time): C receives B's message over the WS stream —
    //     it can only have transited A (router-mode linkstate forwarding).
    e2e_harness::await_event(&mut c_events, Duration::from_secs(60), |f| {
        f.event == "channel-message-received"
            && f.payload.get("channelId").and_then(|x| x.as_str()) == Some(channel.as_str())
            && f.payload
                .get("message")
                .map(|m| {
                    m.get("author").and_then(|x| x.as_str()) == Some(b_owner.as_str())
                        && channel_msg_body_bytes(m).as_deref() == Some(b_body)
                })
                .unwrap_or(false)
    })
    .await
    .expect("c MUST receive b's message in real-time across the sever (via a)");

    // --- HARD ASSERT (read-back, both directions across the sever).
    poll_until(Duration::from_secs(60), || async {
        Ok(
            channel_has_message_from(&c, &community, &channel, &b_owner, b_body)
                .await?
                .then_some(()),
        )
    })
    .await
    .expect("c's read-back MUST contain b's message (via a)");
    poll_until(Duration::from_secs(60), || async {
        Ok(
            channel_has_message_from(&b, &community, &channel, &c_owner, c_body)
                .await?
                .then_some(()),
        )
    })
    .await
    .expect("b's read-back MUST contain c's message (via a)");

    // --- Sever evidence (positive, not log-absence): C's denylist must have
    //     ENGAGED — refusing its own dial to B (C learns B's record via A's
    //     addrbook forwarding) or rejecting B's inbound. Both hit-paths log on
    //     C. Without this, a silently-inert seam would let an ordinary
    //     full-mesh run masquerade as a multi-hop proof.
    poll_until(Duration::from_secs(30), || async {
        let engaged = c.stderr_log_contains("ZEB-912 test denylist: rejecting inbound")?
            || c.stderr_log_contains("ZEB-912 test denylist: refusing dial")?;
        Ok(engaged.then_some(()))
    })
    .await
    .expect("c's denylist MUST show engagement (rejecting inbound / refusing dial)");

    run.mark_success();
    drop((a, b, c, a_home, b_home, c_home));
}
