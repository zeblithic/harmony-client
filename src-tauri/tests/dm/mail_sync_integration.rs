//! End-to-end mail sync test.
//!
//! Spins up an in-process Zenoh session, registers a stub CAS queryable
//! backed by a HashMap of pre-built Merkle-tree blobs, and bridges
//! MailSync's fetch channel through real Zenoh `get` calls. Verifies
//! that the walker registers the message as Pending and that a
//! subsequent fetch_body promotes it to Local and persists the blob
//! on disk.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harmony_app::mail::{BodyState, MailManager};
use harmony_app::mail_sync::{FetchRequest, MailSync, RefreshRequest};
use harmony_mailbox::mailbox::{
    FolderKind, MailFolder, MailPage, MailRoot, MessageEntry, MAILBOX_VERSION,
};
use harmony_mailbox::message::{
    unique_message_id, HarmonyMessage, MailMessageType, MessageFlags, Recipient, RecipientType,
    ADDRESS_HASH_LEN,
};
use tokio::sync::mpsc;

fn make_test_harmony_message(subject: &str, sender: [u8; ADDRESS_HASH_LEN]) -> HarmonyMessage {
    HarmonyMessage {
        version: 0x01,
        message_type: MailMessageType::Email,
        flags: MessageFlags::new(false, false, false),
        timestamp: 1_744_403_200,
        message_id: unique_message_id(),
        in_reply_to: None,
        sender_address: sender,
        recipients: vec![Recipient {
            address_hash: [0xBB; ADDRESS_HASH_LEN],
            recipient_type: RecipientType::To,
        }],
        subject: subject.to_string(),
        body: "test body".to_string(),
        attachments: vec![],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn end_to_end_walks_tree_and_lazy_fetches_body() {
    // ── Build a small mailbox: 1 message, 1 page, 1 folder, 1 root ──
    let msg = make_test_harmony_message("test subject", [0xAA; 16]);
    let msg_bytes = msg.to_bytes().unwrap();
    let msg_cid: [u8; 32] = *blake3::hash(&msg_bytes).as_bytes();

    let entry = MessageEntry {
        message_cid: msg_cid,
        message_id: msg.message_id,
        sender_address: msg.sender_address,
        timestamp: msg.timestamp,
        subject_snippet: "test subject".to_string(),
        read: false,
    };

    let page = MailPage {
        version: MAILBOX_VERSION,
        entries: vec![entry],
    };
    let page_bytes = page.to_bytes().unwrap();
    let page_cid: [u8; 32] = *blake3::hash(&page_bytes).as_bytes();

    let folder = MailFolder {
        version: MAILBOX_VERSION,
        message_count: 1,
        unread_count: 1,
        page_cids: vec![page_cid],
    };
    let folder_bytes = folder.to_bytes().unwrap();
    let folder_cid: [u8; 32] = *blake3::hash(&folder_bytes).as_bytes();

    let root = MailRoot::new_empty([0u8; 16], 1_700_000_000).with_folder(
        FolderKind::Inbox,
        folder_cid,
        1_700_000_001,
    );
    let root_bytes = root
        .to_bytes()
        .expect("test fixture root encodes at MAILBOX_VERSION");
    let root_cid: [u8; 32] = *blake3::hash(&root_bytes).as_bytes();

    // ── Bring up an in-process Zenoh session ──
    let session = zenoh::open(zenoh::Config::default()).await.unwrap();

    // Stub gateway: register CAS queryable backed by our HashMap.
    // The client builds the CAS fetch key as `harmony/content/{prefix}/{cid_hex}`
    // where prefix is the SECOND hex nibble of the CID (matches
    // `fetch_key()` in harmony-zenoh). We declare the queryable as a
    // single wildcard that matches any shard prefix.
    let blobs: Arc<Mutex<HashMap<[u8; 32], Vec<u8>>>> = Arc::new(Mutex::new(HashMap::from([
        (root_cid, root_bytes.clone()),
        (folder_cid, folder_bytes.clone()),
        (page_cid, page_bytes.clone()),
        (msg_cid, msg_bytes.clone()),
    ])));

    let blobs_clone = Arc::clone(&blobs);
    let cas_session = session.clone();
    tokio::spawn(async move {
        let q = cas_session
            .declare_queryable("harmony/content/*/*")
            .await
            .unwrap();
        while let Ok(query) = q.recv_async().await {
            let key = query.key_expr().as_str().to_string();
            let cid_hex = key.rsplit('/').next().unwrap_or("");
            let Ok(cid_bytes) = hex::decode(cid_hex) else {
                let _ = query.reply_err(b"invalid cid hex".to_vec()).await;
                continue;
            };
            let Ok(cid_arr) = <[u8; 32]>::try_from(cid_bytes.as_slice()) else {
                let _ = query.reply_err(b"cid wrong length".to_vec()).await;
                continue;
            };
            let bytes = blobs_clone.lock().unwrap().get(&cid_arr).cloned();
            match bytes {
                Some(b) => {
                    let _ = query.reply(query.key_expr().clone(), b).await;
                }
                None => {
                    let _ = query.reply_err(b"not found".to_vec()).await;
                }
            }
        }
    });

    // Bounded readiness probe — replaces a blind 100ms sleep so the test
    // fails fast on a real propagation issue and doesn't flake when
    // declare_queryable is slower than expected. Hits the queryable's
    // wildcard with a probe CID; any reply (success or err) proves the
    // queryable is discoverable. Both `session.get` AND `recv_async` are
    // bounded by the deadline so a hung receiver can't deadlock the test.
    let probe_deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = probe_deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            panic!("CAS queryable never became reachable within 5s");
        }
        let Ok(replies) = session.get("harmony/content/0/probe").await else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let recv_remaining = probe_deadline.saturating_duration_since(std::time::Instant::now());
        if recv_remaining.is_zero() {
            panic!("CAS queryable never replied within 5s");
        }
        match tokio::time::timeout(recv_remaining, replies.recv_async()).await {
            Ok(Ok(_)) => break,
            Ok(Err(_)) => {
                // Reply stream closed without yielding — try another probe.
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(_) => panic!("CAS queryable never replied within 5s"),
        }
    }

    // ── Bring up MailSync ──
    let tmp = tempfile::tempdir().unwrap();
    let mail_mgr = Arc::new(Mutex::new(MailManager::load(
        Some(&harmony_app::device_dataset_file::test_cipher()),
        &tmp.path().join("mail"),
        [0u8; 16],
    )));

    let (fetch_tx, mut fetch_rx) = mpsc::channel::<FetchRequest>(16);
    // Keep the refresh sender alive for the lifetime of the test even
    // though we never exercise refresh_now — dropping it early would
    // close the channel and break consistency with production use.
    let (_refresh_tx, _refresh_rx) = mpsc::channel::<RefreshRequest>(8);

    // Bridge: pull FetchRequests and issue real Zenoh gets against the
    // stub CAS queryable declared above. Matches the shard-prefix
    // computation used by the real client in event_loop.rs (the second
    // hex nibble of `cid_hex`).
    let fetch_session = session.clone();
    tokio::spawn(async move {
        while let Some(req) = fetch_rx.recv().await {
            let session = fetch_session.clone();
            tokio::spawn(async move {
                let prefix = req.cid_hex.get(1..2).unwrap_or("");
                let key = format!("harmony/content/{prefix}/{}", req.cid_hex);
                let result = match session.get(&key).await {
                    Ok(replies) => {
                        let mut bytes: Option<Vec<u8>> = None;
                        while let Ok(reply) = replies.recv_async().await {
                            if let Ok(sample) = reply.result() {
                                bytes = Some(sample.payload().to_bytes().to_vec());
                                break;
                            }
                        }
                        bytes.ok_or_else(|| "empty reply".to_string())
                    }
                    Err(e) => Err(format!("get: {e}")),
                };
                let _ = req.reply.send(result);
            });
        }
    });

    // Construct MailSync with a don't-care event sink (ZEB-445: MailSync
    // emits via NodeEventSink; this test asserts via MailManager state).
    let sync = Arc::new(MailSync::new(
        fetch_tx,
        _refresh_tx.clone(),
        Arc::clone(&mail_mgr),
        Arc::new(harmony_app::node_event_sink::FanoutSink(vec![])),
    ));

    // Trigger the walk by directly handing MailSync a root.
    // (Equivalent to what event_loop does on /root Zenoh sub events.)
    Arc::clone(&sync).handle_root_push(&root_cid).await;

    // Bounded polling: wait for the walker to complete and the entry to
    // appear. Polling beats fixed sleeps because Zenoh propagation time
    // varies under CI load.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
        if !inbox.is_empty() {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("walker did not register the entry within 10s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // ── Assert: walker registered the entry as Pending ──
    let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
    assert_eq!(inbox.len(), 1, "walker should register exactly 1 entry");
    assert_eq!(
        inbox[0].body_state,
        BodyState::Pending,
        "walker-registered entries start Pending"
    );
    assert_eq!(inbox[0].subject_snippet, "test subject");

    // ── Trigger lazy body fetch ──
    let result = Arc::clone(&sync).fetch_body(msg_cid).await.unwrap();
    assert_eq!(result, msg_bytes);

    // ── Assert: entry promoted to Local and blob persisted ──
    let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
    assert_eq!(
        inbox[0].body_state,
        BodyState::Local,
        "fetch_body should promote the entry to Local"
    );
    let blob_path = tmp
        .path()
        .join("mail")
        .join("blobs")
        .join(format!("{}.bin", hex::encode(msg_cid)));
    assert!(blob_path.exists(), "body blob should be persisted");
    // ZEB-984: the blob is sealed at rest, not plaintext on disk.
    let on_disk = std::fs::read(&blob_path).unwrap();
    assert_eq!(
        on_disk[0],
        harmony_app::device_dataset_file::SEALED_DEVICE_SCHEMA_V3,
        "persisted blob is sealed at rest"
    );
    assert_ne!(on_disk, msg_bytes, "plaintext body must not be on disk");
    // …and it unseals + integrity-verifies back to the original message.
    let detail = mail_mgr
        .lock()
        .unwrap()
        .get_message(&hex::encode(msg_cid))
        .unwrap();
    assert_eq!(detail.subject, "test subject");
    assert_eq!(detail.body, "test body");
}
