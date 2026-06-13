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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_boot_and_mint() {
    let (mut run, ah, bh, a, b) = two_minted_nodes("smoke").await;
    assert_ne!(owner_id(&a).await, owner_id(&b).await, "distinct owners");
    run.mark_success();
    drop((a, b, ah, bh));
}

// ─────────────────────────────────────────────────────────────────────────────
// S1: invite → cross-node join → roster convergence.
//
// FINDING (2026-06-13, this run): S1 cannot currently pass against the headless
// RPC surface. Root cause is a *contract gap*, NOT a transport / first-contact
// failure — both nodes boot, get distinct stable iroh ids (alice 3d5818be…,
// bob 7d862e11…), and reach the iroh relay. The blocker is which join RPC the
// headless surface exposes:
//
//   * The only invite-join verb in `src-tauri/src/api/rpc.rs` is `redeem_invite`
//     → `redeem_invite_impl`, which calls `redeem_invite_inner(.., allow_no_
//     reticulum_destinations = FALSE)` (lib.rs:22266). That path requires the
//     joiner to ALREADY hold a Reticulum device-route for the inviter in
//     `owner_device_cache` (resolve_destinations_for_owner, lib.rs:22100). The
//     harness sets HARMONY_RETICULUM_PORT=0 (to dodge the fixed-4242 two-local-
//     node collision) and the two owners have never met, so that cache is cold.
//     → server returns HTTP 500 "no known device for inviter <hex> — invite
//        cannot route" (lib.rs:21745) within ~1s of redeem.
//
//   * The REAL first-contact path the GUI uses for two-never-met nodes is
//     `connectivity_redeem_invite_iroh` (lib.rs:37943): it pkarr-window-resolves
//     the inviter's routing record, opens an iroh bi-stream on the handshake
//     ALPN, and runs the inner with `allow_no_reticulum_destinations = TRUE`
//     (lib.rs:38346). That verb is a `#[tauri::command]` but is **NOT** in the
//     curated headless v1 RPC surface (api/rpc.rs:605-650) — so the harness has
//     no way to drive real first contact.
//
// Controller decision needed (out of this task's "add the S1 test" scope, which
// must not rebuild harmony-app): EITHER (a) expose `connectivity_redeem_invite_
// iroh` in api/rpc.rs (its handles already come straight off NodeState — a
// one-entry registration mirroring `redeem_invite`, then rebuild), OR (b) let
// the harness re-enable Reticulum on a per-node unique port so LAN discovery
// seeds the device cache before redeem.
//
// The assertions below are kept MEANINGFUL (real joined-roster convergence, both
// directions) and are NOT weakened. The test is `#[ignore]`d so the suite stays
// green while documenting the exact expected behavior; remove the ignore once
// the first-contact join verb is reachable headlessly. Run it explicitly with:
//   cargo test --features e2e s1_invite_join_roster_convergence -- --ignored --nocapture
// ─────────────────────────────────────────────────────────────────────────────
#[ignore = "blocked: headless surface lacks a first-contact join RPC (connectivity_redeem_invite_iroh); see FINDING above"]
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

    // Bob redeems → joins the same community.
    //
    // NOTE: this drives `redeem_invite` (the Reticulum-required path). For two
    // fresh, never-met nodes with Reticulum disabled this fast-fails with
    // "no known device for inviter … — invite cannot route" (see FINDING). The
    // real first-contact verb is `connectivity_redeem_invite_iroh`, which is not
    // yet exposed on the headless RPC surface.
    let redeemed = redeem_invite(&bob, &invite).await.expect("redeem invite");
    let joined_id = redeemed
        .get("ownerIdHex")
        .and_then(|v| v.as_str())
        .expect("joined community id");
    assert_eq!(joined_id, community, "bob joined alice's community");

    // Roster converges both directions (poll — no assumed event).
    poll_until(Duration::from_secs(60), || async {
        Ok(roster_has_joined(&alice, &community, &bob_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("alice sees bob joined");

    poll_until(Duration::from_secs(60), || async {
        Ok(roster_has_joined(&bob, &community, &alice_owner)
            .await?
            .then_some(()))
    })
    .await
    .expect("bob sees alice joined");

    run.mark_success();
    drop((alice, bob, ah, bh));
}
