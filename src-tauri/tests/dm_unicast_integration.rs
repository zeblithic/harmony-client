//! ZEB-227 Path B end-to-end integration test (Phase 3b acceptance criterion).
//!
//! Mocks at the `UnicastSendRequest` channel boundary (no real Reticulum
//! wire, no real runtime tick, no real UDP). Exercises:
//!
//!   send_dm → drain → RuntimeUnicastTransport → unicast_send_tx
//!     → (test intercepts) → handle_unicast on receiver → CAS fetch
//!     → decrypt → apply_inbox → DmAck queued → handle_unicast on sender
//!     → delivered_to updated.
//!
//! The integration test uses `make_party()` to construct matched
//! `(OwnerAddr, DeviceIdentityHash, identity_pub, SigningKey)` tuples
//! from `harmony_identity::PrivateIdentity::from_seed` — same pattern as
//! Task 11's `lib.rs` production wiring, so this test exercises the
//! production cryptographic shape (NOT synthetic identity_pub fixtures).
//!
//! Real Reticulum wire coverage lives in the manual two-device LAN smoke
//! tests filed as a follow-up Linear ticket at PR-creation time (Task 13).

use std::sync::Arc;
use tokio::sync::{mpsc, Mutex as TokioMutex};

use ed25519_dalek::SigningKey;

use harmony_app::content_store::{ContentStore, InMemoryStub};
use harmony_app::dm_outbox::{
    resolve_destinations, DmOutbox, DmTransport, RuntimeUnicastTransport, TransportError,
    UnicastSendRequest,
};
use harmony_app::dm_signing;
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{
    DeviceIdentityHash, DmContentKey, Hlc, InboxKey, OwnerAddr, OwnerDeviceEntry, Space, SpaceId,
    SpaceKind, TransportBinding,
};

/// Build a complete identity for one side of the test:
/// `OwnerAddr` (independent bytes) + `DeviceIdentityHash` (== identity
/// `address_hash`) + 64-byte combined `identity_pub` (for caching) +
/// Ed25519 `SigningKey` (extracted from `PrivateIdentity::to_private_bytes()[32..64]`
/// per Task 11's lib.rs pattern) + `Arc<PrivateIdentity>` (the same
/// material, snapshotted for `DmOutbox::new` per ZEB-262 Phase 4 Task 2).
///
/// `PrivateIdentity` does not implement `Clone` (it carries `ZeroizeOnDrop`),
/// so we round-trip through `to_private_bytes` / `from_private_bytes` to
/// hand back two byte-identical instances: the `signing_key` derived from
/// the seed half lives in one `Arc`, and the second `PrivateIdentity` lives
/// in another. The `dm_outbox_holds_private_identity_for_countersign`
/// unit test in dm_outbox.rs proves the two are signature-equivalent.
fn make_party(
    seed_byte: u8,
) -> (
    OwnerAddr,
    DeviceIdentityHash,
    [u8; 64],
    Arc<SigningKey>,
    Arc<harmony_identity::PrivateIdentity>,
) {
    let seed = [seed_byte; 32];
    let private = harmony_identity::PrivateIdentity::from_seed(&seed);
    let public = private.public_identity();
    let identity_pub = public.to_public_bytes();
    let device_hash = DeviceIdentityHash(public.address_hash);
    // Owner addr is independent of device — pick a different bytes pattern
    // so a bug that confuses owner ↔ device doesn't accidentally pass.
    let owner = OwnerAddr([seed_byte ^ 0xff; 16]);
    // Extract Ed25519 SigningKey from private bytes [32..64] per Task 11's
    // lib.rs pattern (mirrored in dm_signing::sign_dm_packet_matches_private_identity_sign).
    let private_bytes = private.to_private_bytes();
    let ed25519_seed: [u8; 32] = private_bytes[32..64]
        .try_into()
        .expect("PRIVATE_KEY_LENGTH - 32 == 32");
    let signing_key = Arc::new(SigningKey::from_bytes(&ed25519_seed));
    // ZEB-262 Phase 4 Task 2: round-trip a second `PrivateIdentity` from
    // the same private bytes for `DmOutbox.private_identity`. Since
    // `PrivateIdentity` is not Clone, we have to derive it; round-trip is
    // bit-identical.
    let private_identity = Arc::new(
        harmony_identity::PrivateIdentity::from_private_bytes(&private_bytes)
            .expect("private bytes round-trip"),
    );
    (
        owner,
        device_hash,
        identity_pub,
        signing_key,
        private_identity,
    )
}

/// Pre-populate `state`'s `OwnerDeviceCache` with `(owner → [device])` and
/// the matching `identity_pub` cached at index 0. This bypasses
/// `apply_owner_device_update`'s LWW HLC + sanitize path (a single-device
/// vec is already sorted/deduped), giving direct control over the test
/// fixture without reaching for `apply_owner_device_update` boilerplate.
fn cache_party(
    state: &mut OwnerState,
    owner: OwnerAddr,
    device: DeviceIdentityHash,
    identity_pub: [u8; 64],
    learned_at_ms: u64,
    learner_dev: &str,
) {
    state.owner_device_cache.devices.insert(
        owner,
        OwnerDeviceEntry {
            devices: vec![device],
            device_identity_pubs: vec![Some(identity_pub)],
            learned_at: Hlc {
                wall_ms: learned_at_ms,
                logical: 0,
                device_id: learner_dev.into(),
            },
        },
    );
}

/// Build the shared DM Space fixture used by both parties. Sorted-ascending
/// members per the `dm` invariant; Reticulum transport with empty
/// participants per Task 9's `handle_invite` pattern (we don't have real
/// `ReticulumDest` values without going through a real announce flow).
fn make_dm_space(
    space_id: SpaceId,
    alice: OwnerAddr,
    bob: OwnerAddr,
    content_key: DmContentKey,
) -> Space {
    let mut members = vec![alice, bob];
    members.sort();
    Space {
        id: space_id,
        kind: SpaceKind::Dm,
        parent: None,
        community_id: None,
        name: "DM Alice <-> Bob".into(),
        transport: Some(TransportBinding::Reticulum {
            participants: vec![],
        }),
        members,
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "alice".into(),
        },
        updated_at: Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "alice".into(),
        },
        content_key: Some(content_key),
        prior_content_keys: vec![],
        current_epoch: None,
        current_epoch_key: None,
        old_epoch_keys: std::collections::BTreeMap::new(),
        admin_addr: None,
        is_invite_only: None,
        shared_in_profile: false,
    }
}

#[tokio::test]
async fn dm_full_round_trip_through_unicast_channel() {
    // ── Setup: two parties (Alice + Bob), each with their own OwnerState. ──
    let (alice_owner, alice_device, alice_identity_pub, alice_signing_key, alice_private_identity) =
        make_party(0xa1);
    let (bob_owner, bob_device, bob_identity_pub, bob_signing_key, bob_private_identity) =
        make_party(0xb2);

    let alice_state = Arc::new(TokioMutex::new(OwnerState::default()));
    let bob_state = Arc::new(TokioMutex::new(OwnerState::default()));

    // Pre-populate caches: each side knows the other's device + cached pub,
    // AND their own device + own pub (the latter so handle_cidnotify's ack
    // fan-out can include our own devices in `ack_from_devices`).
    {
        let mut alice_g = alice_state.lock().await;
        cache_party(
            &mut alice_g,
            bob_owner,
            bob_device,
            bob_identity_pub,
            100,
            "alice",
        );
        cache_party(
            &mut alice_g,
            alice_owner,
            alice_device,
            alice_identity_pub,
            100,
            "alice",
        );
    }
    {
        let mut bob_g = bob_state.lock().await;
        cache_party(
            &mut bob_g,
            alice_owner,
            alice_device,
            alice_identity_pub,
            100,
            "bob",
        );
        cache_party(
            &mut bob_g,
            bob_owner,
            bob_device,
            bob_identity_pub,
            100,
            "bob",
        );
    }

    // Pre-create the DM Space on both sides (bypass DmInvite flow per
    // test-brevity scope decision — handle_invite has dedicated unit-test
    // coverage). content_key is shared so Bob can decrypt Alice's blob.
    let space_id = SpaceId([0xcc; 16]);
    let content_key = DmContentKey::new([0xaa; 32]);
    let space = make_dm_space(space_id, alice_owner, bob_owner, content_key.clone());
    {
        let mut alice_g = alice_state.lock().await;
        alice_g.spaces.insert(space_id, space.clone());
    }
    {
        let mut bob_g = bob_state.lock().await;
        bob_g.spaces.insert(space_id, space);
    }

    // ── CAS: shared between Alice + Bob for test simplicity (Alice puts,
    //    Bob gets). In production each side has its own CAS bridged via
    //    Zenoh; the test bypasses that wire by using one InMemoryStub. ──
    let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());

    // ── Channels: Alice's outbound, Bob's outbound. ──
    let (alice_unicast_tx, mut alice_unicast_rx) = mpsc::channel::<UnicastSendRequest>(64);
    let (bob_unicast_tx, mut bob_unicast_rx) = mpsc::channel::<UnicastSendRequest>(64);

    // ── Outboxes: each side gets its own DmOutbox. The transport no
    //    longer holds a resolver — `DmOutbox::drain` resolves
    //    destinations from `&state.owner_device_cache` directly, then
    //    passes the resulting Vec into `transport.send`. ──
    let alice_transport: Arc<dyn DmTransport> = Arc::new(RuntimeUnicastTransport::new(
        alice_unicast_tx.clone(),
        alice_owner,
        alice_device,
        alice_signing_key.clone(),
    ));
    let alice_outbox = Arc::new(TokioMutex::new(DmOutbox::new(
        "alice-device".into(),
        alice_owner,
        alice_device,
        alice_signing_key.clone(),
        Arc::clone(&alice_private_identity),
    )));

    let bob_outbox = Arc::new(TokioMutex::new(DmOutbox::new(
        "bob-device".into(),
        bob_owner,
        bob_device,
        bob_signing_key.clone(),
        Arc::clone(&bob_private_identity),
    )));

    // ── Send a DM from Alice → Bob. ──
    let body = b"hello bob".to_vec();
    let mime_type = "text/plain".to_string();
    let send_wall_ms: u64 = 200;
    let _msg_id = {
        let mut alice_g = alice_state.lock().await;
        let mut alice_outbox_g = alice_outbox.lock().await;
        let (msg_id, _msg_cid) = alice_outbox_g
            .send_dm(
                &mut alice_g,
                cas.as_ref(),
                space_id,
                body.clone(),
                mime_type.clone(),
                send_wall_ms,
                None,
            )
            .await
            .expect("send_dm ok");
        msg_id
    };

    // ── Drain Alice's outbox: drain resolves Bob's destinations from
    //    state.owner_device_cache (populated above) and hands them to
    //    RuntimeUnicastTransport::send → pushes UnicastSendRequest onto
    //    alice_unicast_tx (one per device in Bob's cached devices vec —
    //    single-device here). ──
    {
        let mut alice_g = alice_state.lock().await;
        let mut alice_outbox_g = alice_outbox.lock().await;
        let _outcome = alice_outbox_g
            .drain(&mut alice_g, alice_transport.as_ref(), 250)
            .await;
    }

    // ── Intercept Alice's outbound CidNotify. The destination_hash MUST
    //    equal `compute_dm_destination_hash(bob_device.0)` — same scheme
    //    Bob's lib.rs::start_node uses for register_local_destination.
    //    Wrap recv in a 5s timeout so a regression that fails to deliver
    //    surfaces as a fast-fail panic instead of hanging CI. ──
    let alice_to_bob =
        tokio::time::timeout(std::time::Duration::from_secs(5), alice_unicast_rx.recv())
            .await
            .expect("Alice's outbound UnicastSendRequest did not arrive within 5s")
            .expect("alice_unicast_rx closed before Alice's drain produced a UnicastSendRequest");
    assert_eq!(
        alice_to_bob.destination_hash,
        dm_signing::compute_dm_destination_hash(bob_device.0),
        "Alice's outbound dest_hash must match Bob's registered DM dest"
    );

    // ── Bob receives. ZEB-241: dispatch CidNotify through the lifted
    //    variant (production path: event_loop pre-decodes, spawns the
    //    lifted task). The lifted handler does signature verify + cas
    //    fetch + decrypt + apply_inbox + ack fan-out into bob_unicast_tx
    //    + emits `dm-received` via app.emit. We attach a listener on a
    //    mock AppHandle to capture the emitted payload (since the lifted
    //    variant returns `()` instead of DrainOutcome). ──
    let bob_app = tauri::test::mock_app();
    let bob_app_handle = bob_app.handle().clone();
    let received_payloads: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let received_payloads_for_listener = Arc::clone(&received_payloads);
    let _unlisten = tauri::Listener::listen(&bob_app_handle, "dm-received", move |event| {
        let payload: serde_json::Value =
            serde_json::from_str(event.payload()).expect("parse dm-received payload");
        received_payloads_for_listener
            .lock()
            .expect("received_payloads lock")
            .push(payload);
    });
    let (signed, signature, signed_bytes) =
        match harmony_app::dm_envelope::decode_packet(&alice_to_bob.packet)
            .expect("alice's outbound packet must decode")
        {
            harmony_app::dm_envelope::DmPacket::CidNotify {
                signed,
                signature,
                signed_bytes,
            } => (signed, signature, signed_bytes.to_vec()),
            other => panic!("expected CidNotify, got {other:?}"),
        };
    DmOutbox::handle_cidnotify_lifted(
        Arc::clone(&bob_outbox),
        Arc::clone(&bob_state),
        Arc::clone(&cas),
        bob_unicast_tx.clone(),
        bob_app_handle,
        signed,
        signature,
        signed_bytes,
        300,
    )
    .await;

    // State-observable assertion: InboxEntry installed.
    let received_cid = {
        let bob_g = bob_state.lock().await;
        let entry = bob_g
            .inbox
            .iter()
            .find(|(k, _)| k.space_id == space_id)
            .map(|(_, e)| e.clone())
            .expect("Bob's inbox must contain the freshly-applied entry");
        assert!(
            bob_g.inbox.contains_key(&InboxKey {
                space_id,
                message_cid: entry.message_cid,
            }),
            "Bob's inbox must contain the freshly-applied entry"
        );
        entry.message_cid
    };

    // Phase 4 IPC payload assertions: dm-received emit threads body +
    // mime_type + sent_at through to the frontend. The lifted variant
    // emits via app.emit — listener captures the payload. Tauri's
    // listener delivery is async on a separate task; allow a short window
    // for the event to land.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if !received_payloads.lock().unwrap().is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("dm-received listener did not fire within 2s");
    // Snapshot the payloads vec, then drop the guard before further
    // awaits — clippy::await_holding_lock requires no std::Mutex guard
    // outlives an `.await` point.
    let received = {
        let payloads = received_payloads.lock().unwrap();
        assert_eq!(payloads.len(), 1, "exactly one dm-received emit expected");
        payloads[0].clone()
    };
    assert_eq!(
        received["body"].as_str().unwrap(),
        hex::encode(&body),
        "decrypted body must match Alice's plaintext"
    );
    assert_eq!(
        received["mimeType"].as_str().unwrap(),
        mime_type,
        "mime_type must match Alice's send_dm input"
    );
    assert_eq!(
        received["sentAt"].as_u64().unwrap(),
        send_wall_ms,
        "sent_at.wall_ms must match the wall_now_ms Alice's send_dm stamped"
    );
    assert_eq!(
        received["messageCid"].as_str().unwrap(),
        hex::encode(received_cid.to_bytes()),
        "messageCid in dm-received emit must match Bob's inbox entry"
    );

    // ── Bob's ack got pushed onto bob_unicast_tx (one ack per device in
    //    `signed.sender_devices` — single device for Alice). ──
    let bob_to_alice =
        tokio::time::timeout(std::time::Duration::from_secs(5), bob_unicast_rx.recv())
            .await
            .expect("Bob's ack did not arrive within 5s — DM did not deliver back to Alice")
            .expect("bob_unicast_rx closed before Bob queued an ack to Alice");
    assert_eq!(
        bob_to_alice.destination_hash,
        dm_signing::compute_dm_destination_hash(alice_device.0),
        "Bob's ack dest_hash must match Alice's registered DM dest"
    );

    // ── Alice receives the ack. handle_unicast routes through handle_ack:
    //    signature verify + (space_id, message_cid) lookup + delivered_to. ──
    let alice_ack_outcome = {
        let mut alice_g = alice_state.lock().await;
        let mut alice_outbox_g = alice_outbox.lock().await;
        alice_outbox_g
            .handle_unicast(
                &mut alice_g,
                cas.as_ref(),
                &alice_unicast_tx,
                bob_to_alice.packet,
                400,
            )
            .await
            .expect("Alice's handle_unicast(Ack) ok")
    };
    assert_eq!(
        alice_ack_outcome.newly_delivered.len(),
        1,
        "Alice should have one newly-delivered entry"
    );
    // ZEB-231: newly_delivered tuples are now (space_id, message_cid, recipient).
    let (delivered_space_id, delivered_message_cid, recipient) =
        alice_ack_outcome.newly_delivered[0];
    assert_eq!(
        recipient, bob_owner,
        "delivered recipient must be bob_owner"
    );
    {
        let alice_g = alice_state.lock().await;
        let entry = alice_g
            .outbox
            .values()
            .find(|e| e.space_id == delivered_space_id && e.message_cid == delivered_message_cid)
            .expect("OutboxEntry must still exist");
        assert!(
            entry.delivered_to.contains(&bob_owner),
            "Alice's outbox entry must record bob_owner as delivered"
        );
    }
}

#[tokio::test]
async fn dm_offline_recipient_then_online_delivers() {
    // Same setup as Test 1 BUT: Alice's resolver initially has no entry for
    // Bob (Bob's devices unknown). First drain → resolver returns [] →
    // RuntimeUnicastTransport surfaces TransportError::Transient → outbox
    // backoff scheduled. We then populate Alice's cache + advance
    // wall_now_ms past the 5s base backoff (BACKOFF_BASE_MS in dm_outbox.rs).
    // Next drain: succeeds. End-to-end as Test 1 from there.
    //
    // Backoff approach: drain takes wall_now_ms as a parameter and re-evaluates
    // on every call (no real time::sleep inside), so we just bump wall_now_ms
    // manually rather than using tokio::time::pause() — same approach the
    // existing dm_outbox unit test `drain_respects_backoff_skipping_recently_attempted`
    // uses (dm_outbox.rs:1821). Cleaner for this test because handle_unicast
    // contains a tokio::time::timeout(500ms, cas.get) that does interact with
    // real time but only matters once we get to Bob's CidNotify-receive step,
    // by which point we're in the happy-path tail.

    let (alice_owner, alice_device, alice_identity_pub, alice_signing_key, alice_private_identity) =
        make_party(0xa1);
    let (bob_owner, bob_device, bob_identity_pub, bob_signing_key, bob_private_identity) =
        make_party(0xb2);

    let alice_state = Arc::new(TokioMutex::new(OwnerState::default()));
    let bob_state = Arc::new(TokioMutex::new(OwnerState::default()));

    // Alice does NOT yet know Bob (the offline-then-online scenario).
    // Alice still caches HERSELF (so handle_unicast on the ack path can
    // validate things end-to-end). Bob also caches both sides up front.
    {
        let mut alice_g = alice_state.lock().await;
        cache_party(
            &mut alice_g,
            alice_owner,
            alice_device,
            alice_identity_pub,
            100,
            "alice",
        );
        // intentionally NOT inserting bob_owner yet
    }
    {
        let mut bob_g = bob_state.lock().await;
        cache_party(
            &mut bob_g,
            alice_owner,
            alice_device,
            alice_identity_pub,
            100,
            "bob",
        );
        cache_party(
            &mut bob_g,
            bob_owner,
            bob_device,
            bob_identity_pub,
            100,
            "bob",
        );
    }

    let space_id = SpaceId([0xcc; 16]);
    let content_key = DmContentKey::new([0xaa; 32]);
    let space = make_dm_space(space_id, alice_owner, bob_owner, content_key.clone());
    {
        let mut alice_g = alice_state.lock().await;
        alice_g.spaces.insert(space_id, space.clone());
    }
    {
        let mut bob_g = bob_state.lock().await;
        bob_g.spaces.insert(space_id, space);
    }

    let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());

    let (alice_unicast_tx, mut alice_unicast_rx) = mpsc::channel::<UnicastSendRequest>(64);
    let (bob_unicast_tx, mut bob_unicast_rx) = mpsc::channel::<UnicastSendRequest>(64);

    let alice_transport: Arc<dyn DmTransport> = Arc::new(RuntimeUnicastTransport::new(
        alice_unicast_tx.clone(),
        alice_owner,
        alice_device,
        alice_signing_key.clone(),
    ));
    let alice_outbox = Arc::new(TokioMutex::new(DmOutbox::new(
        "alice-device".into(),
        alice_owner,
        alice_device,
        alice_signing_key.clone(),
        Arc::clone(&alice_private_identity),
    )));
    let bob_outbox = Arc::new(TokioMutex::new(DmOutbox::new(
        "bob-device".into(),
        bob_owner,
        bob_device,
        bob_signing_key.clone(),
        Arc::clone(&bob_private_identity),
    )));

    // ── Pre-flight: confirm `resolve_destinations` returns [] for Bob
    //    right now. This nails down the "offline recipient" precondition
    //    and pins the behavior under test (drain → transport's
    //    emit-Transient-on-empty arm). Cheap to verify; failing here
    //    means the test setup itself is broken, not the code under test. ──
    {
        let alice_g = alice_state.lock().await;
        assert!(
            resolve_destinations(&alice_g.owner_device_cache, bob_owner).is_empty(),
            "destination resolver must return [] before Bob's cache entry is populated"
        );
    }

    // ── Send the DM (writes the OutboxEntry; doesn't try to deliver). ──
    let body = b"hello bob (offline-first)".to_vec();
    let mime_type = "text/plain".to_string();
    {
        let mut alice_g = alice_state.lock().await;
        let mut alice_outbox_g = alice_outbox.lock().await;
        alice_outbox_g
            .send_dm(
                &mut alice_g,
                cas.as_ref(),
                space_id,
                body.clone(),
                mime_type,
                200,
                None,
            )
            .await
            .expect("send_dm ok");
    }

    // ── First drain: resolver returns [] → TransportError::Transient → no
    //    UnicastSendRequest is pushed onto alice_unicast_tx. Backoff is
    //    recorded for (entry_id, bob_owner). ──
    {
        let mut alice_g = alice_state.lock().await;
        let mut alice_outbox_g = alice_outbox.lock().await;
        let _outcome = alice_outbox_g
            .drain(&mut alice_g, alice_transport.as_ref(), 250)
            .await;
    }
    // Sanity-check: the channel has nothing on it. try_recv to confirm.
    match alice_unicast_rx.try_recv() {
        Err(mpsc::error::TryRecvError::Empty) => {} // expected — backoff scheduled
        Ok(req) => panic!(
            "expected no UnicastSendRequest on first drain (resolver returned []), got {req:?}"
        ),
        Err(e) => panic!("alice_unicast_rx unexpectedly closed: {e:?}"),
    }
    // Pin the transport surface: directly invoke send with empty
    // destinations and confirm we get TransportError::Transient. Guards
    // against a future refactor accidentally turning empty-destinations
    // into Permanent (which would skip backoff entirely and silently
    // break offline-first delivery).
    {
        // Build a synthetic OutboxEntry to call send() with — we don't
        // need to install it in state, just exercise the transport.
        let alice_g = alice_state.lock().await;
        let entry_ref = alice_g
            .outbox
            .values()
            .next()
            .expect("OutboxEntry was just installed by send_dm");
        let entry_clone = entry_ref.clone();
        drop(alice_g);
        let res = alice_transport
            .send(&entry_clone, bob_owner, Vec::new())
            .await;
        match res {
            Err(TransportError::Transient(_)) => {} // expected
            other => {
                panic!("expected TransportError::Transient on empty destinations, got {other:?}")
            }
        }
    }

    // ── Now Bob comes online from Alice's perspective: populate the cache. ──
    {
        let mut alice_g = alice_state.lock().await;
        cache_party(
            &mut alice_g,
            bob_owner,
            bob_device,
            bob_identity_pub,
            500,
            "alice",
        );
    }

    // ── Second drain at wall_now_ms = 250 + 6_000 (past 5s base backoff
    //    window — BACKOFF_BASE_MS = 5_000ms in dm_outbox.rs). The drain
    //    re-evaluates is_due against wall_now_ms each call, no real time
    //    elapsed. ──
    {
        let mut alice_g = alice_state.lock().await;
        let mut alice_outbox_g = alice_outbox.lock().await;
        let _outcome = alice_outbox_g
            .drain(&mut alice_g, alice_transport.as_ref(), 6_250)
            .await;
    }

    // ── End-to-end as Test 1 from here: intercept → Bob.handle_unicast →
    //    intercept ack → Alice.handle_unicast → delivered_to. Wrap each
    //    recv in 5s timeout so CI fails fast on regression. ──
    let alice_to_bob = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        alice_unicast_rx.recv(),
    )
    .await
    .expect("second drain produced no UnicastSendRequest within 5s — backoff/cache-populate path regressed")
    .expect("alice_unicast_rx closed before second drain emitted a UnicastSendRequest");
    assert_eq!(
        alice_to_bob.destination_hash,
        dm_signing::compute_dm_destination_hash(bob_device.0)
    );

    // ZEB-241: dispatch CidNotify through the lifted variant (production
    // path: event_loop pre-decodes, spawns the lifted task). The lifted
    // handler emits dm-received via app.emit instead of returning
    // DrainOutcome — assert via state inspection (InboxEntry installed).
    let bob_app = tauri::test::mock_app();
    let bob_app_handle = bob_app.handle().clone();
    let (signed, signature, signed_bytes) =
        match harmony_app::dm_envelope::decode_packet(&alice_to_bob.packet)
            .expect("alice's outbound packet must decode")
        {
            harmony_app::dm_envelope::DmPacket::CidNotify {
                signed,
                signature,
                signed_bytes,
            } => (signed, signature, signed_bytes.to_vec()),
            other => panic!("expected CidNotify, got {other:?}"),
        };
    let bob_inbox_before = bob_state.lock().await.inbox.len();
    DmOutbox::handle_cidnotify_lifted(
        Arc::clone(&bob_outbox),
        Arc::clone(&bob_state),
        Arc::clone(&cas),
        bob_unicast_tx.clone(),
        bob_app_handle,
        signed,
        signature,
        signed_bytes,
        7_000,
    )
    .await;
    {
        let bob_g = bob_state.lock().await;
        assert_eq!(
            bob_g.inbox.len(),
            bob_inbox_before + 1,
            "Bob's inbox must have one new entry after lifted CidNotify dispatch"
        );
    }

    let bob_to_alice =
        tokio::time::timeout(std::time::Duration::from_secs(5), bob_unicast_rx.recv())
            .await
            .expect("Bob's ack did not arrive within 5s in the offline-then-online path")
            .expect("bob_unicast_rx closed before Bob queued an ack");
    assert_eq!(
        bob_to_alice.destination_hash,
        dm_signing::compute_dm_destination_hash(alice_device.0)
    );

    let alice_ack_outcome = {
        let mut alice_g = alice_state.lock().await;
        let mut alice_outbox_g = alice_outbox.lock().await;
        alice_outbox_g
            .handle_unicast(
                &mut alice_g,
                cas.as_ref(),
                &alice_unicast_tx,
                bob_to_alice.packet,
                7_500,
            )
            .await
            .expect("Alice's handle_unicast(Ack) ok")
    };
    assert_eq!(alice_ack_outcome.newly_delivered.len(), 1);
    // ZEB-231: newly_delivered tuples are now (space_id, message_cid, recipient).
    let (delivered_space_id, delivered_message_cid, recipient) =
        alice_ack_outcome.newly_delivered[0];
    assert_eq!(recipient, bob_owner);
    {
        let alice_g = alice_state.lock().await;
        let entry = alice_g
            .outbox
            .values()
            .find(|e| e.space_id == delivered_space_id && e.message_cid == delivered_message_cid)
            .expect("OutboxEntry must still exist");
        assert!(entry.delivered_to.contains(&bob_owner));
    }
}
