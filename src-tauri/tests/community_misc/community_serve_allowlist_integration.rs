//! ZEB-395 regression: the content-serve queryable serves an ENCRYPTED CID iff
//! it is in the CommunityServeAllowlist. This is the case the existing
//! community-sync test (shared CAS) cannot exercise: separate stores reachable
//! only over the serve queryable. Models on cas_serve_two_node_integration.rs.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use harmony_app::content_store::CommunityServeAllowlist;
use harmony_app::event_loop::spawn_content_serve_queryable;
use harmony_content::cid::{ContentFlags, ContentId};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serves_allowlisted_encrypted_cid_but_not_others() {
    tokio::time::timeout(Duration::from_secs(30), inner())
        .await
        .expect("allowlist serve test must complete within 30s");
}

async fn inner() {
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B"));

    // Public control CID (liveness proof) + an allowlisted encrypted CID
    // (must serve) + a non-allowlisted encrypted CID (must NOT serve).
    let pub_blob = b"public-control".to_vec();
    let pub_cid = ContentId::for_book(&pub_blob, ContentFlags::default()).expect("public cid");

    let enc_flags = ContentFlags {
        encrypted: true,
        ..ContentFlags::default()
    };
    let allowed_blob = b"community-root-ciphertext".to_vec();
    let allowed_cid = ContentId::for_book(&allowed_blob, enc_flags).expect("allowed enc cid");
    let denied_blob = b"private-dm-ciphertext".to_vec();
    let denied_cid = ContentId::for_book(&denied_blob, enc_flags).expect("denied enc cid");

    let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
    store.insert(pub_cid, pub_blob.clone());
    store.insert(allowed_cid, allowed_blob.clone());
    store.insert(denied_cid, denied_blob.clone());
    let store = Arc::new(store);

    let lookup = {
        let store = Arc::clone(&store);
        Arc::new(move |cid: ContentId| {
            let store = Arc::clone(&store);
            Box::pin(async move { store.get(&cid).cloned() })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send>>
        })
    };

    let allowlist = CommunityServeAllowlist::new();
    // Only this encrypted CID is serveable. Stamp 0 so ANY lease renewal by
    // the serve path is visible (ZEB-922).
    allowlist.allow_at(allowed_cid, 0);
    let allowlist_probe = allowlist.clone();

    let closing = Arc::new(AtomicBool::new(false));
    let _serve = spawn_content_serve_queryable(
        Arc::clone(&session_a),
        lookup,
        Arc::clone(&closing),
        allowlist,
    )
    .await
    .expect("declare content-serve queryable");

    let key_for = |c: &ContentId| {
        let hex = hex::encode(c.to_bytes());
        format!("harmony/content/{}/{}", &hex[1..2], hex)
    };

    // --- Step 1: liveness via public control CID ---
    let pub_key = key_for(&pub_cid);
    let mut pub_got: Option<Vec<u8>> = None;
    for _ in 0..60 {
        let replies = session_b.get(&pub_key).await.expect("get public");
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.result() {
                pub_got = Some(sample.payload().to_bytes().to_vec());
                break;
            }
        }
        if pub_got.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(
        pub_got.as_deref(),
        Some(pub_blob.as_slice()),
        "public control CID must serve (liveness)"
    );

    // --- Step 2: the allowlisted encrypted CID MUST serve ---
    let allowed_key = key_for(&allowed_cid);
    let mut allowed_got: Option<Vec<u8>> = None;
    for _ in 0..60 {
        let replies = session_b.get(&allowed_key).await.expect("get allowed enc");
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.result() {
                allowed_got = Some(sample.payload().to_bytes().to_vec());
                break;
            }
        }
        if allowed_got.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(
        allowed_got.as_deref(),
        Some(allowed_blob.as_slice()),
        "allowlisted encrypted CID must be served"
    );

    // --- Step 2b (ZEB-922): a successful serve must refresh the lease ---
    let mut renewed = false;
    for _ in 0..40 {
        if allowlist_probe.last_affirmed_ms(&allowed_cid) > Some(0) {
            renewed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(renewed, "successful serve must touch the lease stamp");

    // --- Step 3: the non-allowlisted encrypted CID MUST NOT serve ---
    let denied_key = key_for(&denied_cid);
    let replies = session_b.get(&denied_key).await.expect("get denied enc");
    let served_flag = Arc::new(AtomicBool::new(false));
    let served_flag2 = Arc::clone(&served_flag);
    let _ = tokio::time::timeout(Duration::from_secs(3), async move {
        while let Ok(reply) = replies.recv_async().await {
            if reply.result().is_ok() {
                served_flag2.store(true, Ordering::SeqCst);
            }
        }
    })
    .await;
    assert!(
        !served_flag.load(Ordering::SeqCst),
        "non-allowlisted encrypted CID must NOT be served"
    );

    // ZEB-922: refused and never-allowlisted CIDs must not gain lease entries
    // from mere requests — demand can renew intent but never create it.
    assert_eq!(allowlist_probe.last_affirmed_ms(&denied_cid), None);
    assert_eq!(allowlist_probe.last_affirmed_ms(&pub_cid), None);

    closing.store(true, Ordering::SeqCst);
}
