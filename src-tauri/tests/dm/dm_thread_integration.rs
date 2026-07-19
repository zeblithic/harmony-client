//! Phase 4 integration test for the `read_dm_thread` IPC.
//!
//! Seeds a DM Space with a content_key, writes 3 self-sent messages via the
//! existing `DmOutbox::send_dm` path (which lands a self-InboxEntry per the
//! Phase-4 widening), then calls the pure inner function
//! `read_dm_thread_inner` and asserts the bodies + mime_types come back in
//! reverse-chronological order. A second test exercises pagination via the
//! `before_hlc` cursor.
//!
//! These tests target the inner pure function rather than the
//! `#[tauri::command]` shim because spinning a fully-populated `NodeState`
//! (event-loop thread, pairing handle, follow_mgr, etc.) is an order of
//! magnitude more setup than a Phase-4 IPC behavior test warrants. The
//! `tauri::command` wrapper is a thin adapter — its contract is "snapshot
//! handles under sync mutex, drop, call inner".

use std::sync::Arc;

use harmony_app::content_store::{ContentStore, InMemoryStub};
use harmony_app::dm_outbox::DmOutbox;
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{
    DeliveryStatus, DeviceIdentityHash, DmContentKey, Hlc, OwnerAddr, Space, SpaceId, SpaceKind,
};
use harmony_app::read_dm_thread_inner;

fn make_dm_space(space_id: SpaceId, alice: OwnerAddr, bob: OwnerAddr) -> Space {
    let mut members = vec![alice, bob];
    members.sort();
    Space {
        id: space_id,
        kind: SpaceKind::Dm,
        parent: None,
        community_id: None,
        name: "Alice <-> Bob".into(),
        transport: None,
        members,
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "alice-device".into(),
        },
        updated_at: Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "alice-device".into(),
        },
        content_key: Some(DmContentKey::new([0xAB; 32])),
        prior_content_keys: vec![],
        current_epoch: None,
        current_epoch_key: None,
        old_epoch_keys: std::collections::BTreeMap::new(),
        admin_addr: None,
        is_invite_only: None,
        shared_in_profile: false,
        pending_join_at: None,
    }
}

/// Construct an `OwnerState` + `DmOutbox` + CAS triple seeded with a DM
/// Space. Returns `(state, outbox, cas, alice_owner, space_id)`.
async fn fixture() -> (
    OwnerState,
    DmOutbox,
    Arc<dyn ContentStore>,
    OwnerAddr,
    SpaceId,
) {
    let alice = OwnerAddr([0x01; 16]);
    let bob = OwnerAddr([0x02; 16]);
    let mut state = OwnerState::default();
    let space = make_dm_space(SpaceId([7u8; 16]), alice, bob);
    let space_id = space.id;
    state.apply_space_with_canonicalization(space);

    let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());

    // Phase 3b: DmOutbox::new takes signing_key + signing_device_hash for
    // ack fan-out. Send-only path here doesn't exercise signing.
    //
    // ZEB-262 Phase 4 Task 2: DmOutbox::new also takes Arc<PrivateIdentity>
    // for the receive-side counter-sign path (not exercised here).
    let signing_key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]));
    let our_device_hash = DeviceIdentityHash([0xaa; 16]);
    let private_identity = Arc::new(harmony_identity::PrivateIdentity::from_seed(&[0x55; 32]));
    // ZEB-339: use new_synthetic because alice is an arbitrary address not
    // matching any mint_test_owner cert. This test only exercises DM send/read
    // paths, not community-signing.
    let test_owner_thread = harmony_app::community_membership::mint_test_owner(0xD2);
    let community_signing_key_thread = Arc::new(ed25519_dalek::SigningKey::from_bytes(
        &test_owner_thread.device_key.to_bytes(),
    ));
    let enrollment_cert_thread = test_owner_thread.cert;
    let outbox = DmOutbox::new_synthetic(
        "alice-device".into(),
        alice,
        our_device_hash,
        signing_key,
        private_identity,
        community_signing_key_thread,
        enrollment_cert_thread,
    );

    (state, outbox, cas, alice, space_id)
}

#[tokio::test]
async fn read_dm_thread_returns_decrypted_messages_reverse_chronological() {
    let (mut state, mut outbox, cas, alice, space_id) = fixture().await;

    // Three sends with strictly increasing wall_ms — ensures a deterministic
    // chronological order independent of HLC logical-tick collisions.
    let bodies: [&[u8]; 3] = [b"hello", b"world", b"phase4"];
    for (i, body) in bodies.iter().enumerate() {
        outbox
            .send_dm(
                &mut state,
                cas.as_ref(),
                space_id,
                body.to_vec(),
                "text/plain".into(),
                1_000_000 + (i as u64) * 1_000,
                None,
            )
            .await
            .expect("send_dm ok");
    }

    let result = read_dm_thread_inner(&state, cas.as_ref(), space_id, 50, None, alice)
        .await
        .expect("read_dm_thread_inner ok");

    assert_eq!(result.len(), 3, "all 3 self-InboxEntries surface");

    // Reverse-chronological: newest (i=2, wall_ms 1_002_000) first.
    assert_eq!(
        hex::decode(&result[0].body).unwrap(),
        b"phase4",
        "newest first"
    );
    assert_eq!(hex::decode(&result[1].body).unwrap(), b"world");
    assert_eq!(hex::decode(&result[2].body).unwrap(), b"hello");

    // received_at is monotonically decreasing.
    assert!(result[0].received_at > result[1].received_at);
    assert!(result[1].received_at > result[2].received_at);

    // sent_at on each message must match the wall_now_ms passed to send_dm
    // (encrypted payload survives the round-trip).
    assert_eq!(result[0].sent_at, 1_002_000);
    assert_eq!(result[1].sent_at, 1_001_000);
    assert_eq!(result[2].sent_at, 1_000_000);

    // All three are self-sent.
    assert!(
        result.iter().all(|m| m.is_self_outbound),
        "all 3 are self-outbound"
    );
    assert!(result.iter().all(|m| m.mime_type == "text/plain"));
    assert!(
        result.iter().all(|m| m.from == hex::encode(alice.0)),
        "from == self_owner hex on all self-sent entries"
    );

    // ── delivery_state check ──
    // All three were freshly sent without acks, so the outbox status
    // is `Pending` for each → delivery_state = "sending".
    assert!(
        result
            .iter()
            .all(|m| m.delivery_state.as_deref() == Some("sending")),
        "self-entries with Pending outbox status surface as delivery_state='sending'"
    );
}

#[tokio::test]
async fn read_dm_thread_paginates_via_before_hlc_cursor() {
    let (mut state, mut outbox, cas, alice, space_id) = fixture().await;

    // Seed 5 messages at strictly increasing wall_ms.
    for i in 0..5u64 {
        outbox
            .send_dm(
                &mut state,
                cas.as_ref(),
                space_id,
                format!("msg-{i}").into_bytes(),
                "text/plain".into(),
                1_000_000 + i * 1_000,
                None,
            )
            .await
            .expect("send_dm ok");
    }

    // Page 1: newest 2.
    let page1 = read_dm_thread_inner(&state, cas.as_ref(), space_id, 2, None, alice)
        .await
        .expect("page 1 ok");
    assert_eq!(page1.len(), 2);
    assert_eq!(hex::decode(&page1[0].body).unwrap(), b"msg-4");
    assert_eq!(hex::decode(&page1[1].body).unwrap(), b"msg-3");

    // Page 2: cursor = oldest entry's opaque cursor on page 1 (msg-3).
    // ZEB-244: the cursor is now a full-HLC token, not a bare wall_ms.
    let cursor = page1[1].cursor.clone();
    let page2 = read_dm_thread_inner(&state, cas.as_ref(), space_id, 2, Some(cursor), alice)
        .await
        .expect("page 2 ok");
    assert_eq!(page2.len(), 2);
    assert_eq!(hex::decode(&page2[0].body).unwrap(), b"msg-2");
    assert_eq!(hex::decode(&page2[1].body).unwrap(), b"msg-1");

    // Page 3: cursor = oldest entry's cursor on page 2 (msg-1).
    let cursor = page2[1].cursor.clone();
    let page3 = read_dm_thread_inner(&state, cas.as_ref(), space_id, 2, Some(cursor), alice)
        .await
        .expect("page 3 ok");
    assert_eq!(page3.len(), 1, "only msg-0 left");
    assert_eq!(hex::decode(&page3[0].body).unwrap(), b"msg-0");

    // Page 4: cursor = oldest entry's cursor on page 3 → empty.
    let cursor = page3[0].cursor.clone();
    let page4 = read_dm_thread_inner(&state, cas.as_ref(), space_id, 2, Some(cursor), alice)
        .await
        .expect("page 4 ok");
    assert!(page4.is_empty(), "no entries past the oldest message");

    // Pagination invariant: pages 1+2+3 cover all 5 messages, no overlap, no
    // skips. Concatenate decoded bodies and assert the full descending
    // sequence.
    let mut all_bodies: Vec<Vec<u8>> = Vec::new();
    for m in page1.iter().chain(page2.iter()).chain(page3.iter()) {
        all_bodies.push(hex::decode(&m.body).unwrap());
    }
    assert_eq!(
        all_bodies,
        vec![
            b"msg-4".to_vec(),
            b"msg-3".to_vec(),
            b"msg-2".to_vec(),
            b"msg-1".to_vec(),
            b"msg-0".to_vec(),
        ],
        "pagination covers all 5 in descending order, no overlap/skips"
    );
}

#[tokio::test]
async fn read_dm_thread_self_message_delivery_state_reflects_outbox_status() {
    // Phase 4 / PR #81 review fix: read_dm_thread must populate
    // DmThreadMessage.delivery_state for self entries by joining
    // against the outbox status. A stuck-sending self-message must
    // surface as "sending" — pre-fix, the frontend hardcoded
    // "delivered" on every self-InboxEntry and stuck/expired
    // messages were silently misrepresented in scrollback.
    //
    // Strategy: send 4 messages, then mutate their outbox entries
    // to land in each of the four observable states (Pending,
    // Partial, Complete, Expired). Plus delete the outbox entry
    // entirely on a 5th (simulating post-Complete GC) and assert
    // that the InboxEntry-only path also surfaces "delivered".
    let (mut state, mut outbox, cas, alice, space_id) = fixture().await;
    let bob = OwnerAddr([0x02; 16]);
    // The third recipient lets us realize a Partial state: 1-of-2
    // acked. The DM Space we created in fixture() has only alice+bob
    // as members, so we directly mutate the outbox entry's
    // recipient list to add a synthetic peer for the Partial test.
    let charlie = OwnerAddr([0x03; 16]);

    let mut msg_ids = Vec::with_capacity(5);
    for i in 0..5u64 {
        let (msg_id, _msg_cid) = outbox
            .send_dm(
                &mut state,
                cas.as_ref(),
                space_id,
                format!("msg-{i}").into_bytes(),
                "text/plain".into(),
                1_000_000 + i * 1_000,
                None,
            )
            .await
            .expect("send_dm ok");
        msg_ids.push(msg_id);
    }

    // msg-0: leave Pending (default post-send_dm).
    // msg-1: Partial — bump recipient list to {bob, charlie}, ack bob.
    {
        let e = state.outbox.get_mut(&msg_ids[1]).unwrap();
        e.recipient_owners = vec![bob, charlie];
        e.recipient_owners.sort();
        e.delivered_to.insert(bob);
        e.delivery_status = e.compute_status(false);
        assert!(matches!(e.delivery_status, DeliveryStatus::Partial));
    }
    // msg-2: Complete — ack bob (the only recipient).
    {
        let e = state.outbox.get_mut(&msg_ids[2]).unwrap();
        e.delivered_to.insert(bob);
        e.delivery_status = e.compute_status(false);
        assert!(matches!(e.delivery_status, DeliveryStatus::Complete));
    }
    // msg-3: Expired — flip status directly (mirrors what drain's
    // wall-clock sweep does after EXPIRATION_MS elapses).
    {
        let e = state.outbox.get_mut(&msg_ids[3]).unwrap();
        e.delivery_status = DeliveryStatus::Expired;
    }
    // msg-4: simulate post-Complete GC — remove the OutboxEntry
    // entirely. The InboxEntry remains; the helper's missing-outbox
    // fallback should produce "delivered" (the entry was Complete
    // before being collected; GC is the only realistic reason for
    // a self-InboxEntry without a matching outbox entry).
    state.outbox.remove(&msg_ids[4]);

    // Read back. Newest-first ordering means msg-4, msg-3, msg-2, msg-1, msg-0.
    let result = read_dm_thread_inner(&state, cas.as_ref(), space_id, 50, None, alice)
        .await
        .expect("read_dm_thread_inner ok");
    assert_eq!(result.len(), 5);

    // msg-4 (outbox GC'd → "delivered"), msg-3 (Expired), msg-2 (Complete),
    // msg-1 (Partial), msg-0 (Pending).
    assert_eq!(result[0].delivery_state.as_deref(), Some("delivered"));
    assert_eq!(result[1].delivery_state.as_deref(), Some("expired"));
    assert_eq!(result[2].delivery_state.as_deref(), Some("delivered"));
    assert_eq!(result[3].delivery_state.as_deref(), Some("sending"));
    assert_eq!(result[4].delivery_state.as_deref(), Some("sending"));

    // Sanity: every entry IS self-outbound (we sent all five).
    assert!(result.iter().all(|m| m.is_self_outbound));

    // Cursor Bugbot finding (PR #81 round 2): self entries with a
    // surviving OutboxEntry must surface the OutboxEntryId so the
    // frontend's TextMessage.canDelete (which requires
    // messageId !== undefined) can render the inline ⓧ on
    // stuck/expired scrollback entries. Entries whose OutboxEntry
    // was already GC'd post-Complete have message_id == None
    // (delivery_state == "delivered" — nothing to delete).
    assert_eq!(result[0].message_id, None, "msg-4: GC'd → no messageId");
    assert!(
        result[1].message_id.is_some(),
        "msg-3 (Expired): messageId surfaces so user can delete the expired entry"
    );
    assert!(
        result[2].message_id.is_some(),
        "msg-2 (Complete but not GC'd): messageId surfaces"
    );
    assert!(
        result[3].message_id.is_some(),
        "msg-1 (Partial): messageId surfaces so user can delete a stuck entry"
    );
    assert!(
        result[4].message_id.is_some(),
        "msg-0 (Pending): messageId surfaces so user can delete a stuck entry"
    );

    // Each surfaced messageId is the hex of an OutboxEntryId — verify
    // it's the right one (parses to OutboxEntryId, matches the entry).
    for (i, m) in result.iter().enumerate() {
        let inbox_idx = 4 - i; // result is newest-first; msg-4 → result[0]
        if let Some(mid_hex) = &m.message_id {
            let bytes = hex::decode(mid_hex).expect("message_id is valid hex");
            let arr: [u8; 16] = bytes.as_slice().try_into().expect("16 bytes");
            assert_eq!(
                arr, msg_ids[inbox_idx].0,
                "result[{i}].message_id matches msg_ids[{inbox_idx}]"
            );
        }
    }
}

/// ZEB-357: the call-event feature rests on `send_dm` → `read_dm_thread`
/// carrying an arbitrary (non-text) mime type through unchanged — the mime is
/// the frontend's ONLY discriminator for call-event DM messages (no field was
/// added to the durable types). Pins the round-trip for the exact production
/// mime + a JSON body.
#[tokio::test]
async fn read_dm_thread_preserves_call_event_mime_type_zeb357() {
    let (mut state, mut outbox, cas, alice, space_id) = fixture().await;

    const CALL_EVENT_MIME: &str = "application/x-harmony-call-event+json";
    let body: &[u8] =
        br#"{"v":1,"callId":"abababababababababababababababab","outcome":"no_answer"}"#;
    outbox
        .send_dm(
            &mut state,
            cas.as_ref(),
            space_id,
            body.to_vec(),
            CALL_EVENT_MIME.into(),
            1_000_000,
            None,
        )
        .await
        .expect("send_dm ok");

    let result = read_dm_thread_inner(&state, cas.as_ref(), space_id, 50, None, alice)
        .await
        .expect("read_dm_thread_inner ok");

    assert_eq!(result.len(), 1);
    assert_eq!(
        result[0].mime_type, CALL_EVENT_MIME,
        "mime type must round-trip unchanged — it is the call-event discriminator"
    );
    assert_eq!(
        hex::decode(&result[0].body).unwrap(),
        body,
        "JSON body survives encrypt/decrypt unchanged"
    );
}
