//! ZEB-343 Phase 0: two-node fetch-by-CID proof. Node A declares the content
//! serve queryable backed by a stub store holding one blob; node B issues a
//! Zenoh GET on harmony/content/{prefix}/{cid_hex} and must receive the exact
//! bytes. This is the prove-first gate: it validates the Zenoh GET serve/fetch
//! round-trip end-to-end, the unknown that blocked every prior CAS attempt.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use harmony_app::content_store::CommunityServeAllowlist;
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
    let _serve = spawn_content_serve_queryable(
        Arc::clone(&session_a),
        lookup,
        Arc::clone(&closing),
        CommunityServeAllowlist::new(),
    )
    .await
    .expect("declare content-serve queryable");

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
    tokio::time::timeout(Duration::from_secs(30), encrypted_inner())
        .await
        .expect("encrypted-gate test must complete within 30s");
}

async fn encrypted_inner() {
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B"));

    // Seed the store with TWO CIDs:
    //   1. A PUBLIC control CID — used to prove the Zenoh channel is live.
    //   2. An ENCRYPTED CID     — must NOT be served (the gate under test).
    let pub_blob = b"public-control".to_vec();
    let pub_cid = ContentId::for_book(&pub_blob, ContentFlags::default()).expect("public cid");

    let enc_blob = b"secret".to_vec();
    let enc_flags = ContentFlags {
        encrypted: true,
        ..ContentFlags::default()
    };
    let enc_cid = ContentId::for_book(&enc_blob, enc_flags).expect("encrypted cid");

    let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
    store.insert(pub_cid, pub_blob.clone());
    store.insert(enc_cid, enc_blob.clone());
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
    let _serve = spawn_content_serve_queryable(
        Arc::clone(&session_a),
        lookup,
        Arc::clone(&closing),
        CommunityServeAllowlist::new(),
    )
    .await
    .expect("declare content-serve queryable");

    // --- Step 1: Liveness proof ---
    // Retry-loop (mirrors serve_inner) until the PUBLIC control CID is served,
    // proving that Zenoh discovery has completed and the queryable is reachable.
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
        Some(pub_blob.as_slice()),
        "public control CID must be served (liveness proof — Zenoh channel is live)"
    );

    // --- Step 2: Encrypted gate check ---
    // The channel is now provably live. A missing reply for the encrypted CID
    // can ONLY mean the !cid.flags().encrypted gate blocked it.
    let enc_hex = hex::encode(enc_cid.to_bytes());
    let enc_key = format!("harmony/content/{}/{}", &enc_hex[1..2], enc_hex);

    let replies = session_b.get(&enc_key).await.expect("get encrypted");

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

// ── ZEB-535 Task 7: subtree serve-authorization, end-to-end ──────────────────
//
// Phase 0 above proves a single served CID round-trips. Task 7 proves the SAME
// gate over a *real, multi-book, encrypted* artifact: node A ingests it through
// the production `streaming_ingest_with_options` pipeline (the same call the
// channel-artifact ingest IPC uses), so the chunker emits ≥2 leaves + a root
// bundle, every CID stamped `encrypted` and `serveable`. Node B then walks the
// Merkle DAG over the serve queryable — fetching the root bundle, parsing it,
// recursing into every interior/leaf CID — reassembles the ciphertext, and
// decrypts it with the shared epoch key.
//
// The negative case re-ingests the identical bytes with `serveable: false`: now
// NO CID is allowlisted, so the encrypted ROOT BUNDLE itself is refused by the
// serve gate and B's walk stalls at the very first hop. That is the precise
// silent-stall failure mode this primitive exists to prevent — an un-authorized
// interior node makes the whole subtree unreachable with no error.

mod task7 {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use harmony_app::community_state_sync::{decrypt_blob, encrypt_blob};
    use harmony_app::content_store::CommunityServeAllowlist;
    use harmony_app::event_loop::{spawn_content_serve_queryable, IngestRequest};
    use harmony_app::owner_state_types::EpochKey;
    use harmony_app::{streaming_ingest_with_options, IngestOptions};
    use harmony_content::bundle::parse_bundle;
    use harmony_content::chunker::ChunkerConfig;
    use harmony_content::cid::{CidType, ContentFlags, ContentId};
    use tokio::sync::Mutex;

    /// Shared local store: `cid_hex` → bytes. Wraps a Mutex so the ingest-drain
    /// task (writer) and the serve queryable's lookup closure (reader) share it.
    type Store = Arc<Mutex<HashMap<ContentId, Vec<u8>>>>;

    /// A running node-A side: a serve queryable backed by a store that an ingest
    /// pipeline fills. Holds the pieces a test drives + tears down.
    struct NodeA {
        ingest_tx: tokio::sync::mpsc::Sender<IngestRequest>,
        closing: Arc<AtomicBool>,
        // Keep these alive for the lifetime of the test; dropping them tears the
        // serve queryable + ingest drain down.
        _serve: tokio::task::JoinHandle<()>,
        _drain: tokio::task::JoinHandle<()>,
        _session: Arc<zenoh::Session>,
    }

    /// Stand up node A: an ingest pipeline whose drain task replicates EXACTLY
    /// what the production event loop's `ingest_rx` arm does (event_loop.rs:3275)
    /// — store the bytes keyed by CID, and `serve_allowlist.allow(cid)` iff the
    /// request is `serveable`. The same store + allowlist back the serve
    /// queryable. This is the smallest faithful slice of the event loop that
    /// exercises serveable→allowlist→serve without booting a whole NodeRuntime.
    async fn spawn_node_a(session: Arc<zenoh::Session>) -> NodeA {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        let allowlist = CommunityServeAllowlist::new();

        let (ingest_tx, mut ingest_rx) = tokio::sync::mpsc::channel::<IngestRequest>(4096);

        // Ingest drain — mirrors the event loop's ingest_rx handler contract.
        let drain = {
            let store = Arc::clone(&store);
            let allowlist = allowlist.clone();
            tokio::spawn(async move {
                while let Some(req) = ingest_rx.recv().await {
                    match hex::decode(&req.cid_hex)
                        .ok()
                        .and_then(|b| <[u8; 32]>::try_from(b).ok())
                        .map(ContentId::from_bytes)
                    {
                        Some(cid) => {
                            store.lock().await.insert(cid, req.data);
                            if req.serveable {
                                allowlist.allow(cid);
                            }
                            let _ = req.reply.send(Ok(()));
                        }
                        None => {
                            let _ = req
                                .reply
                                .send(Err(format!("invalid CID hex: {}", req.cid_hex)));
                        }
                    }
                }
            })
        };

        let lookup = {
            let store = Arc::clone(&store);
            Arc::new(move |cid: ContentId| {
                let store = Arc::clone(&store);
                Box::pin(async move { store.lock().await.get(&cid).cloned() })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send>>
            })
        };

        let closing = Arc::new(AtomicBool::new(false));
        let serve = spawn_content_serve_queryable(
            Arc::clone(&session),
            lookup,
            Arc::clone(&closing),
            allowlist,
        )
        .await
        .expect("declare content-serve queryable");

        NodeA {
            ingest_tx,
            closing,
            _serve: serve,
            _drain: drain,
            _session: session,
        }
    }

    /// The zenoh serve key for a CID: `harmony/content/{shard}/{cid_hex}` where
    /// the shard is the CID's 2nd hex nibble (the sharding invariant the serve
    /// queryable enforces — see parse_content_serve_cid).
    fn serve_key(cid: &ContentId) -> String {
        let hex = hex::encode(cid.to_bytes());
        format!("harmony/content/{}/{}", &hex[1..2], hex)
    }

    /// Single-CID zenoh GET against node A, returning the served bytes (or None
    /// if no reply arrives inside `attempt_timeout`). Retries are the caller's
    /// job — this is one GET attempt.
    async fn fetch_one(
        session_b: &zenoh::Session,
        cid: &ContentId,
        attempt_timeout: Duration,
    ) -> Option<Vec<u8>> {
        let key = serve_key(cid);
        let replies = session_b.get(&key).await.expect("zenoh get");
        let got = tokio::time::timeout(attempt_timeout, async {
            while let Ok(reply) = replies.recv_async().await {
                if let Ok(sample) = reply.result() {
                    return Some(sample.payload().to_bytes().to_vec());
                }
            }
            None
        })
        .await;
        got.unwrap_or(None)
    }

    /// Fetch a single CID with bounded retries — the first GET can race Zenoh
    /// discovery/queryable-propagation (the same first-hop readiness race the
    /// Phase-0 tests above handle with a 60×250ms retry loop). Returns None if
    /// every attempt comes up empty within the overall budget.
    async fn fetch_one_retrying(
        session_b: &zenoh::Session,
        cid: &ContentId,
        attempts: usize,
        attempt_timeout: Duration,
    ) -> Option<Vec<u8>> {
        for _ in 0..attempts {
            if let Some(bytes) = fetch_one(session_b, cid, attempt_timeout).await {
                return Some(bytes);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        None
    }

    /// Walk the Merkle DAG from `root` over the serve queryable, collecting every
    /// fetched (interior + leaf) CID's bytes into `fetched`, and returning the
    /// ordered leaf CIDs (mirrors harmony_content::dag::walk, but each Bundle is
    /// fetched cross-node instead of read from a local BookStore). Returns
    /// Err(stuck_cid) the moment a referenced CID cannot be fetched — this is how
    /// the negative test observes the serve gate refusing an un-allowlisted CID.
    async fn walk_cross_node(
        session_b: &zenoh::Session,
        root: ContentId,
        attempts: usize,
        attempt_timeout: Duration,
        fetched: &mut HashMap<ContentId, Vec<u8>>,
    ) -> Result<Vec<ContentId>, ContentId> {
        let mut leaves = Vec::new();
        // Explicit DFS stack so this stays an async fn (no boxed recursion).
        let mut stack = vec![root];
        while let Some(cid) = stack.pop() {
            match cid.cid_type() {
                CidType::Book => leaves.push(cid),
                CidType::InlineData => {
                    if !cid.is_sentinel() {
                        leaves.push(cid);
                    }
                }
                CidType::Bundle(_) => {
                    let bytes = match fetch_one_retrying(session_b, &cid, attempts, attempt_timeout)
                        .await
                    {
                        Some(b) => b,
                        None => return Err(cid),
                    };
                    let children = parse_bundle(&bytes).expect("parse bundle");
                    // Push children in reverse so the stack yields them
                    // left-to-right (preserves leaf order for reassembly).
                    let children: Vec<ContentId> = children.to_vec();
                    fetched.insert(cid, bytes);
                    for child in children.into_iter().rev() {
                        stack.push(child);
                    }
                }
                CidType::Stream => return Err(cid), // structurally invalid in a DAG
            }
        }
        Ok(leaves)
    }

    /// Deterministic ~1.5 MB pseudo-random payload. A simple LCG keeps it
    /// reproducible run-to-run while being non-compressible enough that the
    /// chunker emits multiple ~1 MiB books (MAX_PAYLOAD_SIZE ≈ 1 MiB), so the
    /// artifact is genuinely multi-book (≥2 leaves + a root bundle).
    ///
    /// `seed` MUST differ per test: zenoh's default config uses LAN
    /// multicast scouting, so a parallel test process serving the SAME
    /// content-addressed CIDs (identical bytes → identical CIDs) would answer
    /// this test's GETs. Distinct seeds → distinct ciphertext → distinct CIDs →
    /// no cross-process serve contamination.
    fn deterministic_payload(len: usize, seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut state: u64 = seed;
        while out.len() < len {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// Ingest `bytes` through the production streaming-ingest pipeline with the
    /// given options, then drop the sender so the drain task settles all stores
    /// + allowlist entries before the serve queryable is queried.
    async fn ingest(node: &NodeA, bytes: Vec<u8>, opts: IngestOptions) -> (ContentId, u64) {
        streaming_ingest_with_options(
            tokio::io::BufReader::new(std::io::Cursor::new(bytes)),
            &node.ingest_tx,
            ChunkerConfig::DEFAULT,
            None,
            opts,
        )
        .await
        .expect("streaming ingest")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn encrypted_multi_book_artifact_round_trips_between_nodes() {
        tokio::time::timeout(Duration::from_secs(60), round_trip_inner())
            .await
            .expect("encrypted multi-book round-trip must complete within 60s");
    }

    async fn round_trip_inner() {
        let cfg = zenoh::Config::default();
        let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A"));
        let session_b = Arc::new(zenoh::open(cfg).await.expect("session B"));

        let node_a = spawn_node_a(Arc::clone(&session_a)).await;

        // Shared epoch key: the test holds the SAME key on both sides (B is the
        // test here), so the only thing under proof is that B can FETCH every
        // (encrypted, serveable) book from A. Deterministic key for repeatability;
        // distinct from the negative test's key so the two never share CIDs (see
        // deterministic_payload's seed note — content-addressing means identical
        // bytes would otherwise be served cross-process over LAN multicast).
        let epoch_key = EpochKey::new([0x5c; 32]);

        // ~1.5 MB plaintext → encrypt the WHOLE thing → ingest the ciphertext.
        let plaintext = deterministic_payload(1_500_000, 0x9E37_79B9_7F4A_7C15);
        let ciphertext = encrypt_blob(&epoch_key, &plaintext).expect("encrypt_blob");

        let (root, total) = ingest(
            &node_a,
            ciphertext.clone(),
            IngestOptions {
                flags: ContentFlags {
                    encrypted: true,
                    ..ContentFlags::default()
                },
                serveable: true,
            },
        )
        .await;
        assert_eq!(total as usize, ciphertext.len(), "ingest byte count");

        // The artifact MUST be multi-book: a Bundle root means the chunker
        // emitted ≥2 leaves under a bundle, so the walk below actually exercises
        // subtree authorization on an INTERIOR (bundle) node, not just one leaf.
        assert!(
            matches!(root.cid_type(), CidType::Bundle(_)),
            "1.5 MB artifact must produce a Bundle root (multi-book), got {:?}",
            root.cid_type()
        );
        assert!(
            root.flags().encrypted,
            "root CID must carry the encrypted flag"
        );

        // Node B walks the DAG cross-node. 60 attempts × 1s/attempt absorbs the
        // first-hop discovery race (matches the Phase-0 60×250ms budget, scaled
        // up because a 1 MiB book transfer is heavier than an 18-byte blob).
        let mut fetched: HashMap<ContentId, Vec<u8>> = HashMap::new();
        let leaves = walk_cross_node(&session_b, root, 60, Duration::from_secs(1), &mut fetched)
            .await
            .expect("every interior + leaf CID must be served by A");

        assert!(
            leaves.len() >= 2,
            "multi-book artifact must walk to ≥2 leaves, got {}",
            leaves.len()
        );

        // Fetch each leaf (bundles were fetched during the walk) and reassemble
        // in leaf order — mirrors harmony_content::dag::reassemble.
        let mut reassembled: Vec<u8> = Vec::with_capacity(ciphertext.len());
        for leaf in &leaves {
            if leaf.cid_type() == CidType::InlineData {
                reassembled.extend_from_slice(&leaf.extract_inline_data().expect("inline data"));
                continue;
            }
            let bytes = fetch_one_retrying(&session_b, leaf, 60, Duration::from_secs(1))
                .await
                .expect("leaf CID must be served by A");
            reassembled.extend_from_slice(&bytes);
        }

        assert_eq!(
            reassembled.len(),
            ciphertext.len(),
            "reassembled ciphertext length must match"
        );
        assert_eq!(
            reassembled, ciphertext,
            "reassembled ciphertext must match the ingested ciphertext"
        );

        // Decrypt with the shared epoch key and verify the original plaintext.
        let recovered = decrypt_blob(&epoch_key, &reassembled).expect("decrypt_blob");
        assert_eq!(
            recovered.len(),
            plaintext.len(),
            "decrypted plaintext length must match the 1.5 MB input"
        );
        assert_eq!(
            recovered, plaintext,
            "decrypted plaintext must equal the original input"
        );

        node_a.closing.store(true, Ordering::SeqCst);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unserved_encrypted_artifact_fetch_does_not_complete() {
        tokio::time::timeout(Duration::from_secs(60), unserved_inner())
            .await
            .expect("unserved-stall guard must complete within 60s");
    }

    async fn unserved_inner() {
        let cfg = zenoh::Config::default();
        let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A"));
        let session_b = Arc::new(zenoh::open(cfg).await.expect("session B"));

        let node_a = spawn_node_a(Arc::clone(&session_a)).await;
        // Distinct key + payload seed from the positive test so this test's CIDs
        // can never collide with another (possibly parallel) test process that
        // serves the same content-addressed bytes (see deterministic_payload).
        let epoch_key = EpochKey::new([0xa7; 32]);

        let plaintext = deterministic_payload(1_500_000, 0xD1B5_4A32_D192_ED03);
        let ciphertext = encrypt_blob(&epoch_key, &plaintext).expect("encrypt_blob");

        // serveable=false → NO CID is allowlisted. Every CID
        // is encrypted, so the serve gate (content_cid_servable = !encrypted ||
        // allowlisted) refuses ALL of them — including the root bundle.
        let (root, _total) = ingest(
            &node_a,
            ciphertext,
            IngestOptions {
                flags: ContentFlags {
                    encrypted: true,
                    ..ContentFlags::default()
                },
                serveable: false,
            },
        )
        .await;
        assert!(
            matches!(root.cid_type(), CidType::Bundle(_)),
            "negative case must also be a multi-book Bundle root, got {:?}",
            root.cid_type()
        );

        // Liveness control: prove the Zenoh channel between B and A is actually
        // live by serving a PUBLIC (unencrypted) CID through the SAME pipeline.
        // Without this, a stalled fetch below could be a dead channel rather than
        // the serve gate doing its job. The control is ingested serveable=false
        // too, but it's unencrypted so the gate serves it regardless.
        let control_blob = b"liveness-control-public".to_vec();
        let (control_root, _n) = ingest(
            &node_a,
            control_blob.clone(),
            IngestOptions::default(), // unencrypted, not serveable
        )
        .await;
        assert_eq!(
            control_root.cid_type(),
            CidType::Book,
            "small control payload is a single book"
        );
        let control_got =
            fetch_one_retrying(&session_b, &control_root, 60, Duration::from_secs(1)).await;
        assert_eq!(
            control_got.as_deref(),
            Some(control_blob.as_slice()),
            "public control CID must serve — proves the channel is live"
        );

        // Now the gate check: the channel is provably live, so a missing reply
        // for the encrypted ROOT BUNDLE can ONLY be the serve gate refusing it
        // (not allowlisted). 5s is well clear of the observed control latency
        // (single GET landed inside the retry loop, typically <1s once
        // discovery completed), so a 5s timeout is not flaky — it's >5× the
        // normal single-hop fetch time. A served reply would arrive immediately.
        let root_reply = fetch_one(&session_b, &root, Duration::from_secs(5)).await;
        assert!(
            root_reply.is_none(),
            "encrypted root bundle must NOT be served when serveable=false \
             (un-allowlisted interior CID → fetch stalls; the silent-stall mode)"
        );

        node_a.closing.store(true, Ordering::SeqCst);
    }
}
