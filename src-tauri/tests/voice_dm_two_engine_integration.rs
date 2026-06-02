//! ZEB-352 Voice V4 DM media seal/relay two-engine integration test.
//!
//! Proves the DM voice-call media path works peer-to-peer: a frame sealed
//! under the derived DM voice key and published on the Zenoh DM media topic
//! can be opened by a subscriber using the same key.
//!
//! # Transport-flake class
//!
//! This test exercises a Zenoh loopback relay and belongs to the same known
//! transport-flake class as `voice_presence_two_engine_integration` and the
//! iroh-zenoh loopback tests. It passes on CI; it may flake on a
//! loopback-restricted local box. Do NOT add retries that mask real failures.
//! If the test fails locally but the seal/open assertion path is sound, note
//! it and let CI be authoritative.

#![cfg(feature = "test-fixtures")]

use std::time::Duration;

use harmony_app::community_channel_log::derive_dm_voice_key;
use harmony_app::owner_state_types::DmContentKey;
use harmony_app::voice_crypto::{
    decrypt_dm_voice_packet, encrypt_dm_voice_packet, VoiceCryptoError, VOICE_DM_PACKET_AAD,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dm_voice_two_engine_seal_relay_and_negative_call_id() {
    // Outer timeout: surfaces a hang as a clear failure, not an indefinite stall.
    tokio::time::timeout(Duration::from_secs(30), run_inner())
        .await
        .expect("DM voice two-engine test timed out");
}

async fn run_inner() {
    // ── Two real Zenoh sessions on the shared loopback peer router ──────────
    let cfg = zenoh::Config::default();
    let session_a = zenoh::open(cfg.clone()).await.expect("session A");
    let session_b = zenoh::open(cfg).await.expect("session B");

    // ── Shared DM content key + call_id ─────────────────────────────────────
    // Both sides share the DM content key (simulating two devices in the same
    // DM space). Each derives the same per-call voice key from the call_id.
    let dm_content_key = DmContentKey::new([0xAB; 32]);
    let call_id: [u8; 16] = [0xCD; 16];
    let k_voice = derive_dm_voice_key(&dm_content_key, &call_id);

    // A second, distinct call_id for the negative assertion: a packet sealed
    // under K_voice(call_id=A) must fail to open under K_voice(call_id=B).
    let call_id_other: [u8; 16] = [0xEF; 16];
    let k_voice_other = derive_dm_voice_key(&dm_content_key, &call_id_other);

    // ── Wire topics ─────────────────────────────────────────────────────────
    let call_id_hex = hex::encode(call_id);
    let device_a_hex = "aaaaaaaaaaaaaaaa"; // opaque device token in topic path
    let media_topic_a = format!("harmony/voice/dm/{}/{}", call_id_hex, device_a_hex);
    let media_sub_key = format!("harmony/voice/dm/{}/*", call_id_hex);

    // ── Declare subscriber on B before A publishes (settle ~1s) ─────────────
    let media_sub = session_b
        .declare_subscriber(&media_sub_key)
        .await
        .expect("declare DM media subscriber");
    tokio::time::sleep(Duration::from_secs(1)).await;

    // ── Positive assertion: A seals a frame, B opens it ─────────────────────
    let original_frame: Vec<u8> = (0u8..40).collect();
    let sealed_frame =
        encrypt_dm_voice_packet(&k_voice, &call_id, VOICE_DM_PACKET_AAD, &original_frame)
            .expect("seal DM media frame");

    session_a
        .put(&media_topic_a, sealed_frame)
        .await
        .expect("publish sealed DM media frame");

    let recovered = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let sample = media_sub.recv_async().await.expect("DM media recv");
            let bytes = sample.payload().to_bytes().to_vec();
            if let Ok(frame) =
                decrypt_dm_voice_packet(&k_voice, &call_id, VOICE_DM_PACKET_AAD, &bytes)
            {
                break frame;
            }
        }
    })
    .await
    .expect("B should receive + open the sealed DM media frame");

    assert_eq!(
        recovered, original_frame,
        "recovered DM media frame must equal the original"
    );

    // ── Negative assertion: wrong call_id key must reject ────────────────────
    // Seal a packet under k_voice (call_id A), then try to open it under
    // k_voice_other (call_id B). This must fail — the call_id is bound in the
    // AAD, so a cross-call replay is detectable.
    let frame2: Vec<u8> = (0u8..20).collect();
    let sealed_for_a = encrypt_dm_voice_packet(&k_voice, &call_id, VOICE_DM_PACKET_AAD, &frame2)
        .expect("seal frame under call_id A");

    let open_under_b = decrypt_dm_voice_packet(
        &k_voice_other,
        &call_id_other,
        VOICE_DM_PACKET_AAD,
        &sealed_for_a,
    );
    assert_eq!(
        open_under_b,
        Err(VoiceCryptoError::OpenFailed),
        "a packet sealed under K_voice(call_id=A) must FAIL to open under K_voice(call_id=B)"
    );

    drop(media_sub);
}
