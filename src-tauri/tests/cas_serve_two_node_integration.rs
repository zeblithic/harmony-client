//! ZEB-343 Phase 0: two-node fetch-by-CID proof. Node A declares the content
//! serve queryable backed by a stub store holding one blob; node B issues a
//! Zenoh GET on harmony/content/{prefix}/{cid_hex} and must receive the exact
//! bytes. This is the prove-first gate: it validates the Zenoh GET serve/fetch
//! round-trip end-to-end, the unknown that blocked every prior CAS attempt.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use harmony_app::event_loop::spawn_content_serve_queryable;
use harmony_content::cid::{ContentFlags, ContentId};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serves_public_cid_to_a_second_zenoh_node() {
    tokio::time::timeout(Duration::from_secs(30), serve_inner())
        .await
        .expect("cas-serve two-node proof must complete within 30s");
}

async fn serve_inner() {
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B"));

    let blob = b"avatar-bytes-proof".to_vec();
    let cid = ContentId::for_book(&blob, ContentFlags::default()).expect("cid");
    let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
    store.insert(cid, blob.clone());
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
        spawn_content_serve_queryable(Arc::clone(&session_a), lookup, Arc::clone(&closing));

    let cid_hex = hex::encode(cid.to_bytes());
    let prefix = &cid_hex[1..2];
    let key = format!("harmony/content/{prefix}/{cid_hex}");

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

    assert_eq!(
        got.as_deref(),
        Some(blob.as_slice()),
        "node B must receive A's served bytes"
    );
    closing.store(true, Ordering::SeqCst);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn does_not_serve_encrypted_cid() {
    tokio::time::timeout(Duration::from_secs(15), encrypted_inner())
        .await
        .expect("encrypted-gate test must complete within 15s");
}

async fn encrypted_inner() {
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B"));

    // An ENCRYPTED CID held locally — must NOT be served.
    let blob = b"secret".to_vec();
    let enc_flags = ContentFlags {
        encrypted: true,
        ..ContentFlags::default()
    };
    let cid = ContentId::for_book(&blob, enc_flags).expect("cid");
    let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
    store.insert(cid, blob.clone());
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
        spawn_content_serve_queryable(Arc::clone(&session_a), lookup, Arc::clone(&closing));

    let cid_hex = hex::encode(cid.to_bytes());
    let key = format!("harmony/content/{}/{}", &cid_hex[1..2], cid_hex);

    // Give discovery time, then GET: expect NO successful reply.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let replies = session_b.get(&key).await.expect("get");

    // Use Arc<AtomicBool> to avoid borrow-checker conflict between the async
    // block's mutable capture and the post-drain read.
    let served_flag = Arc::new(AtomicBool::new(false));
    let served_flag2 = Arc::clone(&served_flag);
    let drain = tokio::time::timeout(Duration::from_secs(3), async move {
        while let Ok(reply) = replies.recv_async().await {
            if reply.result().is_ok() {
                served_flag2.store(true, Ordering::SeqCst);
            }
        }
    })
    .await;
    let _ = drain; // timeout is the expected "no more replies" terminator
    assert!(
        !served_flag.load(Ordering::SeqCst),
        "encrypted CID must not be served"
    );
    closing.store(true, Ordering::SeqCst);
}
