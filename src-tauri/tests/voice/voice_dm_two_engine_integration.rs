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
//! iroh-zenoh loopback tests. It may flake on a loopback-restricted local
//! box. Do NOT add retries that mask real failures. If the test fails
//! locally but the seal/open assertion path is sound, note it and let CI be
//! authoritative.
//!
//! ZEB-675: the positive path re-publishes the sealed frame on a cadence
//! (delivery retry) instead of one put + one recv wait. That is NOT an
//! assertion retry — the seal/open assertions are unchanged, and no number
//! of re-puts makes a bad frame decrypt. See the comment at the publish
//! site for the CI drop-race this closes.

#![cfg(feature = "test-fixtures")]

use std::time::Duration;

use harmony_app::community_channel_log::derive_dm_voice_key;
use harmony_app::owner_state_types::DmContentKey;
use harmony_app::voice_crypto::{
    decrypt_dm_voice_packet, encrypt_dm_voice_packet, VoiceCryptoError, VOICE_DM_PACKET_AAD,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dm_voice_two_engine_seal_relay_and_negative_call_id() {
    // Outer timeout: surfaces a hang as a clear failure, not an indefinite
    // stall. 120s ≫ the 30s discovery + 60s delivery ceilings inside, so an
    // inner failure always reports its own specific message first (ZEB-675).
    tokio::time::timeout(Duration::from_secs(120), run_inner())
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

    // ── Declare subscriber on B before A publishes ──────────────────────────
    let media_sub = session_b
        .declare_subscriber(&media_sub_key)
        .await
        .expect("declare DM media subscriber");
    // ZEB-502: deterministically wait until A's session has discovered B's REMOTE
    // media subscriber before publishing, rather than a fixed 1s settle. Frames go
    // out live (no replay); a put that races B's subscriber declaration is silently
    // dropped — under the ZEB-675 delivery loop below that costs a 2s re-put round
    // trip, so the barrier still pays for itself by making the first put land. A
    // concrete-key publisher on `…/{deviceA}` intersects B's wildcard subscriber, so
    // a `matching_status(Locality::Remote)` barrier is well-defined (mirrors
    // `voice_presence_two_engine_integration.rs`).
    {
        let media_ready_pub = session_a
            .declare_publisher(
                zenoh::key_expr::KeyExpr::try_from(media_topic_a.clone()).expect("media topic key"),
            )
            .allowed_destination(zenoh::sample::Locality::Remote)
            .await
            .expect("declare media readiness publisher");
        // 30s is far above the sub-second loopback subscriber-discovery time but
        // comfortably under the test's 120s outer timeout, so a genuine discovery
        // failure surfaces as the informative assertion below rather than the
        // less-specific outer guard (and leaves budget for the delivery loop).
        // Widened from 10s for loaded CI runners (ZEB-675 wall-clock-budget rule:
        // ceiling ≫ healthy-path time, and this is a wait, not a perf assertion).
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if media_ready_pub
                .matching_status()
                .await
                .expect("media matching_status query failed")
                .matching()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "A never matched B's remote media subscriber within 30s"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // ── Positive assertion: A seals a frame, B opens it ─────────────────────
    let original_frame: Vec<u8> = (0u8..40).collect();
    let sealed_frame =
        encrypt_dm_voice_packet(&k_voice, &call_id, VOICE_DM_PACKET_AAD, &original_frame)
            .expect("seal DM media frame");

    // ZEB-675: publish-retry against a one-shot-put drop race. CI run
    // 29161978730 failed here at exactly the old 10s recv ceiling with
    // discovery already settled: `matching_status()` had reported B's remote
    // subscriber, yet the single put never reached B. "Matching" means A's
    // session knows a matching remote subscriber exists — not that the route
    // is fully wired end-to-end — and a live put dropped in that window is
    // gone (no replay on this topic). The sealed frame is idempotent (same
    // bytes every put; the recv loop ignores anything it cannot open), so
    // re-publishing is safe: re-put every 2s until B opens a copy, bounded
    // by a 60s ceiling ≫ the sub-second healthy path (wall-clock-budget
    // rule). A real seal/open regression still fails loudly — no number of
    // re-puts makes a bad frame decrypt.
    let recovered = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            session_a
                .put(&media_topic_a, sealed_frame.clone())
                .await
                .expect("publish sealed DM media frame");
            let attempt = tokio::time::timeout(Duration::from_secs(2), async {
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
            .await;
            match attempt {
                Ok(frame) => break frame,
                Err(_elapsed) => continue, // this copy dropped — re-put and keep listening
            }
        }
    })
    .await
    .expect("B should receive + open the sealed DM media frame within 60s of 2s re-puts");

    assert_eq!(
        recovered, original_frame,
        "recovered DM media frame must equal the original"
    );

    // ── Negative assertions: the two bindings, isolated ──────────────────────
    // A single packet sealed under K_voice(call_id=A) is opened under two
    // deliberately-different conditions, each varying ONE factor so the failure
    // can be attributed to that factor alone (the original test varied both key
    // AND call_id at once, so key-mismatch could mask a missing AAD binding).
    let frame2: Vec<u8> = (0u8..20).collect();
    let sealed_for_a = encrypt_dm_voice_packet(&k_voice, &call_id, VOICE_DM_PACKET_AAD, &frame2)
        .expect("seal frame under call_id A");

    // (1) Call-scope AAD binding: SAME key, different call_id in the AAD. The
    // key is held constant, so the only thing that changed is the AAD-bound
    // call_id — this proves a cross-call replay is rejected by the AAD itself.
    let open_wrong_call_id =
        decrypt_dm_voice_packet(&k_voice, &call_id_other, VOICE_DM_PACKET_AAD, &sealed_for_a);
    assert_eq!(
        open_wrong_call_id,
        Err(VoiceCryptoError::OpenFailed),
        "same key but wrong call_id in the AAD must FAIL (call-scope AAD binding)"
    );

    // (2) Per-call key binding: different derived key, MATCHING call_id. This
    // proves the per-call HKDF key (the production cross-call defense, since the
    // key is salted by call_id) also rejects the frame on its own.
    let open_wrong_key =
        decrypt_dm_voice_packet(&k_voice_other, &call_id, VOICE_DM_PACKET_AAD, &sealed_for_a);
    assert_eq!(
        open_wrong_key,
        Err(VoiceCryptoError::OpenFailed),
        "a different per-call key must FAIL to open (per-call key binding)"
    );

    drop(media_sub);
}
