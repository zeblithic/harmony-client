//! ZEB-345 Task 6: cross-peer long-form profile doc over CAS. Node A encodes a
//! `ProfilePageDoc` (bio + links + fields) to canonical CBOR via the public
//! `encode_profile_doc` codec, addresses the bytes as a PublicDurable CID, and
//! serves them on its content-serve queryable. Node B fetches that CID over the
//! real two-node Zenoh transport and `decode_profile_doc`s the bytes back to a
//! doc that equals A's input. This proves a profile document rides the ZEB-343
//! public-serve path end-to-end.
//!
//! Reachability note: `ingest_profile_doc_inner` (pub(crate)) and
//! `profile_doc_dto_from_bytes` (private fn) are NOT visible to the integration
//! test crate, which compiles against the public API + `test-fixtures`. The
//! public codec — `profile_page_doc::{encode_profile_doc, decode_profile_doc}` —
//! is what we exercise here (it is the exact encode/decode authority
//! `ingest_profile_doc_inner` / `profile_doc_dto_from_bytes` delegate to), with
//! the single-blob serve harness shared with `cas_serve_two_node_integration.rs`
//! and `profile_card_avatar_cross_peer_integration.rs`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use harmony_app::event_loop::spawn_content_serve_queryable;
use harmony_app::profile_page_doc::{
    decode_profile_doc, encode_profile_doc, ProfileField, ProfileLink, ProfilePageDoc,
    PROFILE_DOC_VERSION,
};
use harmony_content::cid::{ContentFlags, ContentId};

/// A representative profile doc: non-empty bio (with a newline), >=1 link, >=1
/// field — the shape the happy-path round-trip must preserve exactly.
fn sample_doc() -> ProfilePageDoc {
    ProfilePageDoc {
        version: PROFILE_DOC_VERSION,
        bio: "long-form bio line one\nline two".into(),
        links: vec![
            ProfileLink {
                label: "Homepage".into(),
                url: "https://example.com/me".into(),
            },
            ProfileLink {
                label: "Community".into(),
                url: "harmony:community/abc".into(),
            },
        ],
        fields: vec![
            ProfileField {
                key: "Pronouns".into(),
                value: "she/her".into(),
            },
            ProfileField {
                key: "Location".into(),
                value: "Earth".into(),
            },
        ],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn profile_doc_resolves_for_peer_b() {
    tokio::time::timeout(Duration::from_secs(30), happy_inner())
        .await
        .expect("profile-doc cross-peer e2e must complete within 30s");
}

async fn happy_inner() {
    // ---- Node A: encode the doc -> canonical CBOR -> public-durable CID ----
    let doc = sample_doc();
    let bytes = encode_profile_doc(&doc).expect("encode profile doc");
    let cid = ContentId::for_book(&bytes, ContentFlags::default()).expect("cid");

    let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
    store.insert(cid, bytes.clone());
    let store = Arc::new(store);

    // ---- A serves the doc over Zenoh; B fetches by CID ----
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B"));

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

    let cid_hex = hex::encode(cid.to_bytes());
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

    let fetched = got.expect("peer B must fetch the profile doc bytes");
    // verify-on-fetch property: fetched bytes hash to the requested CID.
    assert!(
        cid.verify_hash(&fetched),
        "fetched profile doc bytes must hash to their CID"
    );
    assert_eq!(fetched, bytes, "fetched bytes must equal A's encoded doc");

    // Decode through the public codec and assert full structural equality with
    // A's input — bio, links, and fields all round-trip across the transport.
    let decoded = decode_profile_doc(&fetched).expect("decode fetched profile doc");
    assert_eq!(decoded, doc, "decoded doc must equal A's input doc");
    assert_eq!(decoded.bio, doc.bio, "bio must match");
    assert_eq!(decoded.links, doc.links, "links must match");
    assert_eq!(decoded.fields, doc.fields, "fields must match");

    closing.store(true, Ordering::SeqCst);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn does_not_serve_encrypted_profile_doc() {
    tokio::time::timeout(Duration::from_secs(30), encrypted_inner())
        .await
        .expect("encrypted-gate test must complete within 30s");
}

async fn encrypted_inner() {
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B"));

    // Seed the store with TWO doc CIDs (mirrors cas_serve_two_node_integration's
    // encrypted-gate test, so a slow-discovery miss can't masquerade as a pass):
    //   1. A PUBLIC control doc CID  — proves the Zenoh channel is live.
    //   2. An ENCRYPTED doc CID      — must NOT be served (the gate under test).
    let pub_doc = sample_doc();
    let pub_bytes = encode_profile_doc(&pub_doc).expect("encode public control doc");
    let pub_cid = ContentId::for_book(&pub_bytes, ContentFlags::default()).expect("public cid");

    let enc_doc = sample_doc();
    let enc_bytes = encode_profile_doc(&enc_doc).expect("encode encrypted doc");
    let enc_flags = ContentFlags {
        encrypted: true,
        ..ContentFlags::default()
    };
    let enc_cid = ContentId::for_book(&enc_bytes, enc_flags).expect("encrypted cid");

    let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
    store.insert(pub_cid, pub_bytes.clone());
    store.insert(enc_cid, enc_bytes.clone());
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

    // --- Step 1: Liveness proof ---
    // Retry-loop (mirrors happy_inner) until the PUBLIC control CID is served,
    // proving Zenoh discovery completed and the queryable is reachable.
    let pub_hex = hex::encode(pub_cid.to_bytes());
    let pub_key = format!("harmony/content/{}/{}", &pub_hex[1..2], pub_hex);

    let mut pub_got: Option<Vec<u8>> = None;
    for _ in 0..60 {
        let replies = session_b.get(&pub_key).await.expect("get public control");
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
        Some(pub_bytes.as_slice()),
        "public control doc CID must be served (liveness proof — Zenoh channel is live)"
    );

    // --- Step 2: Encrypted gate check ---
    // The channel is now provably live. A missing reply for the encrypted CID
    // can ONLY mean the !cid.flags().encrypted gate blocked it.
    let enc_hex = hex::encode(enc_cid.to_bytes());
    let enc_key = format!("harmony/content/{}/{}", &enc_hex[1..2], enc_hex);

    let replies = session_b.get(&enc_key).await.expect("get encrypted");

    // Arc<AtomicBool> to avoid a borrow-checker conflict between the async
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
        "encrypted profile doc CID must not be served"
    );
    closing.store(true, Ordering::SeqCst);
}
