//! ZEB-343 Phase 5: cross-peer avatar e2e. Owner A signs a ProfileCardBroadcast
//! carrying avatar_cid; the card verifies (verify_card -> owner); A's content-serve
//! queryable serves the avatar bytes; node B fetches the avatar CID over Zenoh and
//! verifies hash==CID. Composes: card signing/verify + serve queryable + fetch +
//! verify-on-fetch — the full avatar-over-CAS path.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use harmony_app::community_membership::mint_test_owner;
use harmony_app::event_loop::spawn_content_serve_queryable;
use harmony_app::owner_state_types::Hlc;
use harmony_app::profile_card_broadcast::{sign_card, verify_card};
use harmony_content::cid::{ContentFlags, ContentId};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_a_avatar_card_resolves_for_peer_b() {
    tokio::time::timeout(Duration::from_secs(30), inner())
        .await
        .expect("avatar cross-peer e2e must complete within 30s");
}

async fn inner() {
    // ---- Owner A: ingest an avatar blob -> CID, sign a card carrying it ----
    let owner = mint_test_owner(0xA1);
    let avatar_blob = b"fake-normalized-256x256-png-bytes".to_vec();
    let avatar_cid = ContentId::for_book(&avatar_blob, ContentFlags::default()).expect("cid");

    let card = sign_card(
        &owner.device_key,
        owner.owner.0,
        "Ann".into(),
        "hi".into(),
        Some(avatar_cid.to_bytes()),
        owner.cert.clone(),
        Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "d".into(),
        },
    )
    .expect("sign");

    // ---- Peer B receives + verifies the card, extracts the avatar CID ----
    let verified_owner = verify_card(&card).expect("card verifies");
    assert_eq!(verified_owner, owner.owner.0);
    let avatar_cid_bytes = card.avatar_cid.expect("card carries avatar_cid");
    let cid_to_fetch = ContentId::from_bytes(avatar_cid_bytes);

    // ---- A serves the avatar over Zenoh; B fetches by CID ----
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B"));

    let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
    store.insert(cid_to_fetch, avatar_blob.clone());
    let store = Arc::new(store);
    let lookup = {
        let store = Arc::clone(&store);
        Arc::new(move |cid: ContentId| {
            let store = Arc::clone(&store);
            Box::pin(async move { store.get(&cid).cloned() })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send>>
        })
    };
    let closing = Arc::new(AtomicBool::new(false));
    let _serve =
        spawn_content_serve_queryable(Arc::clone(&session_a), lookup, Arc::clone(&closing))
            .await
            .expect("declare content-serve queryable");

    let cid_hex = hex::encode(cid_to_fetch.to_bytes());
    let key = format!("harmony/content/{}/{}", &cid_hex[1..2], cid_hex);

    let mut got: Option<Vec<u8>> = None;
    for _ in 0..60 {
        let replies = session_b.get(&key).await.expect("get");
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.result() {
                got = Some(sample.payload().to_bytes().to_vec());
                break;
            }
        }
        if got.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let fetched = got.expect("peer B must fetch the avatar bytes");
    // verify-on-fetch property: fetched bytes hash to the requested CID.
    assert!(
        cid_to_fetch.verify_hash(&fetched),
        "fetched avatar must hash to its CID"
    );
    assert_eq!(fetched, avatar_blob, "fetched bytes must equal A's avatar");

    closing.store(true, std::sync::atomic::Ordering::SeqCst);
}
