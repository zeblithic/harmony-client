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
// DM DELIVERY GAP (documented product/harness limitation — see report).
// DM unicast resolves `OwnerAddr → device destinations` via `OwnerDeviceCache`,
// populated by Reticulum announce propagation ("Flow A"). The harness sets
// `HARMONY_RETICULUM_PORT=0` (`e2e-harness/src/node.rs`), DISABLING Reticulum LAN
// discovery — mandatory for two co-located nodes, which would otherwise collide on
// the single fixed broadcast/bind port (`255.255.255.255:{port}`). With no live
// Reticulum socket the two nodes never exchange the announces that fill
// `OwnerDeviceCache`, so `resolve_destinations` returns empty and `send_dm` retries
// forever with `transport temporarily unavailable: no known devices for recipient`
// (observed in alice.stderr.log). The iroh-based friend handshake does NOT populate
// a DM transport destination. Net: DM *delivery* between two same-host headless
// nodes is unreachable in this harness regardless of how first-contact happens
// (the community-warm fallback hits the same wall — the blocker is the disabled
// Reticulum transport, not the friend path). So below we PROVE the DM send is
// accepted by the engine, then *characterize* (not hard-assert) delivery with a
// short bounded poll, recording whether bytes round-tripped. The hard assertions
// are the parts that genuinely work end-to-end: real friendship active both ways +
// real DM-space creation with the verified id semantics.
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
    // CAS-stored + queued on the outbox). Delivery is then characterized, not
    // hard-asserted (see the DM DELIVERY GAP note above): a bounded poll records
    // whether the bytes round-trip. In this harness they do not (Reticulum LAN
    // transport disabled → "no known devices for recipient"); on a transport that
    // populates OwnerDeviceCache (real LAN / two hosts) the same poll would observe
    // delivery. The bounded poll keeps the scenario fast + honest rather than
    // hanging 120s on a known-blocked path.
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
         (both expected FALSE in this harness: Reticulum LAN transport disabled, \
         OwnerDeviceCache never populated — see DM DELIVERY GAP note + report)"
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
// Setup: mint both, kill + re-spawn already-minted (single clean session each),
// with HARMONY_ZENOH_DISABLE_MULTICAST=1 in the env so neither can peer via
// multicast. Then join a community (clean nodes) — the membership reachability
// exchange fires the dial, now the ONLY path to a Zenoh peer mesh. Publish /
// subscribe owner cards and measure convergence.
//
// SKIPS (no-op) unless HARMONY_ZENOH_DISABLE_MULTICAST=1 is set — without it the
// nodes peer via multicast and `connect_peer` reports success off that transport,
// a FALSE POSITIVE for the dial. The harness inherits the parent env, so run as:
//   HARMONY_ZENOH_DISABLE_MULTICAST=1 cargo nextest run --features e2e \
//     -E 'test(s5c_clean_dial_only_card_propagation_probe)'
//
// EXPECTED: converged=TRUE → the clean dial works; the ZEB-468 fix is "just
// restart-safety" and cross-WAN cards are viable. converged=FALSE → the dial is
// independently broken and the fix must address it too.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s5c_clean_dial_only_card_propagation_probe() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    if !std::env::var("HARMONY_ZENOH_DISABLE_MULTICAST")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
    {
        eprintln!(
            "s5c SKIPPED: requires HARMONY_ZENOH_DISABLE_MULTICAST=1 (dial-only probe; \
             without it nodes peer via multicast → false positive). Re-run with the env set."
        );
        return;
    }

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

    run.mark_success();
    drop((alice, bob, ah, bh));
}

// ─────────────────────────────────────────────────────────────────────────────
// S5d — ZEB-468 restart-safety REGRESSION (hard-asserted). Ildwyn, 2026-06-15.
//
// s5b/s5c CHARACTERIZE; this one GUARDS. It is the minimal repro of the ZEB-468
// bug: `two_minted_nodes` mints (= RESTARTS) both nodes, which is the exact
// restart-poisoning. No community is needed — owner cards are global and s5b
// proved a *clean* co-located mesh routes them with no community. The ONLY
// difference from s5b's passing control is that here we KEEP the mint-restart
// instead of doing a clean relaunch.
//
// Root cause (see ZEB-468): `open_session_with_runtime` adopts an external zenoh
// `Runtime` via `session::init(DynamicRuntime)`, so `static_runtime` is None and
// `session.close()` only sends a face-close — it never calls the Runtime's
// `manager.close()`. Across the mint-restart that leaks every transport/listener:
//   • the peer keeps our old (deterministic) zid's face → the restarted session's
//     re-declarations are rejected "Resource remapped. Remapping unsupported!";
//   • the TCP accept loop spins on the shutting-down Tokio runtime
//     ("…being shutdown…").
// Both block the owner-card pub/sub mesh from re-forming, so cards never converge.
//
// The fix closes the adopted runtime on shutdown. AFTER it, this restarted mesh
// behaves like s5b's clean one: cards converge, with 0 remap + 0 accept-spin.
//
// BEFORE the fix this test FAILS (converged=false, dozens of remap + hundreds of
// accept-spin lines); after it, it PASSES. The 0/0 baseline matches s5b's
// measured clean-relaunch control.
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s5d_restart_card_propagation_regression() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let (mut run, ah, bh, mut alice, mut bob) = two_minted_nodes("s5d").await;
    let alice_owner = owner_id(&alice).await;
    let bob_owner = owner_id(&bob).await;

    const ALICE_CARD: &str = "Alice-restart";
    const BOB_CARD: &str = "Bob-restart";

    publish_card_until_ok(&alice, ALICE_CARD, "gm restart", Duration::from_secs(30))
        .await
        .expect("alice card publisher ready");
    publish_card_until_ok(&bob, BOB_CARD, "gm restart", Duration::from_secs(30))
        .await
        .expect("bob card publisher ready");

    let a_sub = subscribe_member_card(&alice, &bob_owner)
        .await
        .expect("alice subscribes to bob's card");
    let b_sub = subscribe_member_card(&bob, &alice_owner)
        .await
        .expect("bob subscribes to alice's card");

    // Convergence depends on the restarted co-located mesh re-forming cleanly.
    // Re-publish each tick (a Zenoh put is not retained for a late subscriber).
    let converged = poll_until(Duration::from_secs(60), || async {
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
        "ZEB-468 regression: owner cards must propagate between two co-located nodes \
         after the mint-driven restart (got converged=false; remap={remap}, \
         accept_spin={accept_spin}). The mint-restart is leaking the Zenoh transport."
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
