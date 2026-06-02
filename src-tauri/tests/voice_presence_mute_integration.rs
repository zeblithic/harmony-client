//! ZEB-351 Voice V3 dynamic-mute integration test.
//!
//! Proves that the presence publisher's mute state is driven by a shared
//! `Arc<AtomicBool>` (the flag `set_voice_muted` → `VoiceChannelRequest::SetMuted`
//! flips) rather than the old hardcoded `muted: true`:
//!
//! 1. A real `spawn_voice_presence_publisher` runs over a real Zenoh session,
//!    started with the flag = `true`. A subscriber recovers its beacons and the
//!    first one it sees has `muted == true`.
//! 2. The flag is flipped to `false` (simulating the `SetMuted` handler). A
//!    subsequent heartbeat beacon the subscriber recovers has `muted == false`.
//!
//! This drives the publisher's real beacon-construction + sign + seal path end
//! to end across the flip — the same `build_heartbeat_beacon` site the event
//! loop's immediate-beacon helper (`publish_presence_once`) shares. A second,
//! transport-free assertion pins `build_heartbeat_beacon` directly across the
//! atomic so the test stays meaningful even if the loopback router is flaky.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use harmony_app::community_channel_log::derive_channel_key;
use harmony_app::community_membership::{mint_test_owner, ChannelId};
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
use harmony_app::voice_presence::{
    build_heartbeat_beacon, open_presence_beacon, spawn_voice_presence_publisher,
};

/// Poll an async predicate until it returns true or `timeout` elapses.
async fn wait_until<F, Fut>(timeout: Duration, mut predicate: F) -> Result<(), ()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if predicate().await {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Pure path: `build_heartbeat_beacon` reflects the mute bool — the exact value
/// the publisher reads from the shared atomic each heartbeat. Transport-free, so
/// the dynamic-mute contract is pinned even if the loopback router is flaky.
#[test]
fn build_heartbeat_beacon_reflects_atomic_across_flip() {
    let hlc = Hlc {
        wall_ms: 1,
        logical: 0,
        device_id: "aa".repeat(32),
    };
    let owner = OwnerAddr([0xa1; 16]);
    let device = [0x22; 32];
    let flag = Arc::new(AtomicBool::new(true));

    let muted_beacon = build_heartbeat_beacon(owner, device, &hlc, 0, flag.load(Ordering::SeqCst));
    assert!(
        muted_beacon.muted,
        "flag=true ⇒ beacon muted (start-muted join state)"
    );

    flag.store(false, Ordering::SeqCst);
    let unmuted_beacon =
        build_heartbeat_beacon(owner, device, &hlc, 1, flag.load(Ordering::SeqCst));
    assert!(
        !unmuted_beacon.muted,
        "after the flag flips, the next heartbeat tracks muted=false"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publisher_mute_flag_drives_beacon_muted_over_zenoh() {
    // Outer guard so a hang surfaces as a failure, not an indefinite stall.
    tokio::time::timeout(Duration::from_secs(30), run_inner())
        .await
        .expect("voice presence mute integration test timed out");
}

async fn run_inner() {
    // ── Two real Zenoh sessions on the shared loopback peer router ──────────
    let cfg = zenoh::Config::default();
    let session_a = zenoh::open(cfg.clone()).await.expect("session A");
    let session_b = zenoh::open(cfg).await.expect("session B");

    // ── Identity A: owner + enrolled device key, consistently bound ─────────
    let owner_a_seed = mint_test_owner(0xA1);
    let owner_a = owner_a_seed.owner;
    let signing_a = Arc::new(owner_a_seed.device_key);
    let device_a: [u8; 32] = signing_a.verifying_key().to_bytes();

    // ── Shared community + channel + derived ChannelKey ─────────────────────
    let community = SpaceId([0xc0; 16]);
    let channel = ChannelId([0xc1; 16]);
    let membership_key = EpochKey::new([0x77; 32]);
    let channel_key = Arc::new(derive_channel_key(&membership_key, &community, &channel));

    let pres_topic = format!(
        "harmony/voice-presence/{}/{}",
        hex::encode(community.0),
        hex::encode(channel.0),
    );

    // ── A plain Zenoh subscriber on B that opens beacons under the channel
    //    key. We don't need the full membership-verifying subscriber here — the
    //    contract under test is "publisher's beacon `muted` tracks the atomic",
    //    so opening + reading `muted` is sufficient and keeps the test light.
    let sub = session_b
        .declare_subscriber(&pres_topic)
        .await
        .expect("declare presence subscriber");
    // Loopback subscriber declaration + peer discovery settle (~1 s), matching
    // the two-engine template.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // ── Shared mute flag, started muted (V3 start-muted join). A's publisher
    //    reads this each heartbeat; flipping it simulates the `SetMuted` arm.
    let mute_flag = Arc::new(AtomicBool::new(true));
    let closing = Arc::new(AtomicBool::new(false));

    // Short interval so the test sees multiple heartbeats quickly.
    let joined_hlc = Hlc {
        wall_ms: 1_000,
        logical: 0,
        device_id: hex::encode(device_a),
    };
    let pub_handle = spawn_voice_presence_publisher(
        session_a.clone(),
        pres_topic.clone(),
        Arc::clone(&channel_key),
        community,
        channel,
        Arc::clone(&signing_a),
        owner_a,
        device_a,
        joined_hlc.clone(),
        Arc::clone(&mute_flag),
        Duration::from_millis(200),
        Arc::clone(&closing),
    );

    // Collect the latest recovered `muted` value as beacons arrive. We open each
    // sample under the channel key and read `beacon.muted`.
    let latest_muted: Arc<tokio::sync::Mutex<Option<bool>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let latest_for_task = Arc::clone(&latest_muted);
    let ck_for_task = Arc::clone(&channel_key);
    let reader = tokio::spawn(async move {
        while let Ok(sample) = sub.recv_async().await {
            let bytes = sample.payload().to_bytes().to_vec();
            if let Some(signed) = open_presence_beacon(&ck_for_task, &community, &channel, &bytes) {
                *latest_for_task.lock().await = Some(signed.beacon.muted);
            }
        }
    });

    // ── Assertion 1: while the flag is true, recovered beacons show muted=true.
    wait_until(Duration::from_secs(10), || {
        let latest = Arc::clone(&latest_muted);
        async move { *latest.lock().await == Some(true) }
    })
    .await
    .expect("a beacon published while flag=true must show muted=true");

    // ── Flip the flag (simulating set_voice_muted → SetMuted) ───────────────
    mute_flag.store(false, Ordering::SeqCst);

    // ── Assertion 2: a subsequent heartbeat shows muted=false ───────────────
    wait_until(Duration::from_secs(10), || {
        let latest = Arc::clone(&latest_muted);
        async move { *latest.lock().await == Some(false) }
    })
    .await
    .expect("after the flag flips to false, a subsequent beacon must show muted=false");

    // ── Teardown ────────────────────────────────────────────────────────────
    closing.store(true, Ordering::SeqCst);
    pub_handle.abort();
    reader.abort();
}
