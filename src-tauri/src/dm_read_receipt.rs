//! ZEB-214 — opt-in per-DM read receipts (ephemeral, live-only). This module
//! owns the `dm-read-receipt` UI event shape and (in later tasks) the emit
//! decision + packet build. Receipts never touch the outbox/deposit rung.

use crate::owner_state_types::{
    DeviceIdentityHash, Hlc, OwnerAddr, ReadReceiptPref, Space, SpaceId, SpaceKind,
};

/// The `dm-read-receipt` UI event name — a peer told us they've read our DM up
/// to a watermark. Emitted from the tunnel ingest path only (receipts are
/// live-only, so there is no deposit/sweeper path to duplicate).
pub(crate) const DM_READ_RECEIPT_EVENT: &str = "dm-read-receipt";

/// Shared payload builder — single source of truth for the event shape.
/// `readUpTo` and `at` are exposed as wall-ms (like `sentAt` on `dm-received`)
/// so the frontend compares them against `Message.timestamp` directly.
/// `readUpTo` = the watermark (which of the viewer's sent messages are seen);
/// `at` = the receipt's send time (the "Seen HH:MM" clock).
pub(crate) fn dm_read_receipt_event_payload(
    space_id: SpaceId,
    from: OwnerAddr,
    read_up_to: &Hlc,
    at_ms: u64,
) -> serde_json::Value {
    serde_json::json!({
        "spaceId": hex::encode(space_id.0),
        "from": hex::encode(from.0),
        "readUpTo": read_up_to.wall_ms,
        "at": at_ms,
    })
}

/// The single other member of a 1:1 `Dm` space, or `None` for any other shape
/// (group DM, channel, or a degenerate member set). 1:1 only this cut.
pub(crate) fn dm_peer_of(space: &Space, self_owner: OwnerAddr) -> Option<OwnerAddr> {
    if space.kind != SpaceKind::Dm || space.members.len() != 2 {
        return None;
    }
    space.members.iter().copied().find(|m| *m != self_owner)
}

/// Decide whether to emit a read receipt for `space_id` and, if so, build the
/// signed wire packet. `Some((peer, wire))` iff the space is a 1:1 `Dm` with
/// `read_receipt_pref == Broadcast`. Pure: reads state, mints no HLC, touches
/// no outbox — the ephemerality invariant is structural (there is simply no
/// outbox write anywhere in this path).
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_read_receipt(
    state: &crate::owner_state_crdt::OwnerState,
    self_owner: OwnerAddr,
    signing_device_hash: DeviceIdentityHash,
    signing_key: &ed25519_dalek::SigningKey,
    device_id: &str,
    space_id: SpaceId,
    up_to_ms: u64,
    now_ms: u64,
) -> Option<(OwnerAddr, Vec<u8>)> {
    let space = state.spaces.get(&space_id)?;
    if space.read_receipt_pref != Some(ReadReceiptPref::Broadcast) {
        return None;
    }
    let peer = dm_peer_of(space, self_owner)?;
    let signed = crate::dm_envelope::DmReadReceiptSigned {
        space_id,
        sender_owner_addr: self_owner,
        signing_device_hash,
        read_up_to: Hlc {
            wall_ms: up_to_ms,
            logical: 0,
            device_id: device_id.to_string(),
        },
        sent_at: Hlc {
            wall_ms: now_ms,
            logical: 0,
            device_id: device_id.to_string(),
        },
    };
    let packet = crate::dm_envelope::build_signed_read_receipt(signed, signing_key).ok()?;
    let wire = crate::dm_envelope::encode_packet(&packet).ok()?;
    Some((peer, wire))
}

/// Orchestrate a single read-receipt push: prepare (pref gate + packet build),
/// record the watermark for later reconnect re-sends, and push over the live
/// tunnel. No-op when `prepare_read_receipt` returns `None`. Never writes the
/// outbox (ephemeral).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn maybe_send_read_receipt(
    crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    mgr: &std::sync::Arc<crate::tunnel_manager::TunnelManager>,
    signing_key: &std::sync::Arc<ed25519_dalek::SigningKey>,
    signing_device_hash: DeviceIdentityHash,
    self_owner: OwnerAddr,
    device_id: &str,
    watermarks: &std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<SpaceId, u64>>>,
    space_id: SpaceId,
    up_to_ms: u64,
    now_ms: u64,
) {
    let prepared = {
        let state = crdt_state.lock().await;
        prepare_read_receipt(
            &state,
            self_owner,
            signing_device_hash,
            signing_key,
            device_id,
            space_id,
            up_to_ms,
            now_ms,
        )
    };
    let Some((peer, wire)) = prepared else {
        return;
    };
    watermarks.lock().await.insert(space_id, up_to_ms);
    crate::iroh_tunnel_dm_transport::send_packet_to_owner_tunnels(crdt_state, mgr, peer, &wire).await;
}

/// The 1:1 DM spaces with `peer` that are opted in (Broadcast) AND have a
/// stored watermark to re-send. Used when `peer` proves reachable (an inbound
/// DM just arrived over a live tunnel).
pub(crate) fn plan_reconnect_resends(
    state: &crate::owner_state_crdt::OwnerState,
    self_owner: OwnerAddr,
    peer: OwnerAddr,
    watermarks: &std::collections::HashMap<SpaceId, u64>,
) -> Vec<SpaceId> {
    state
        .spaces
        .values()
        .filter(|s| s.read_receipt_pref == Some(ReadReceiptPref::Broadcast))
        .filter(|s| dm_peer_of(s, self_owner) == Some(peer))
        .filter(|s| watermarks.contains_key(&s.id))
        .map(|s| s.id)
        .collect()
}

/// Re-send our stored watermark to `peer` for each opted-in 1:1 DM. Best-effort
/// (a live tunnel to `peer` was just proven by the inbound DM that triggered
/// this). Never writes the outbox (ephemeral).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resend_watermarks_to_peer(
    crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    mgr: &std::sync::Arc<crate::tunnel_manager::TunnelManager>,
    signing_key: &std::sync::Arc<ed25519_dalek::SigningKey>,
    signing_device_hash: DeviceIdentityHash,
    self_owner: OwnerAddr,
    device_id: &str,
    watermarks: &std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<SpaceId, u64>>>,
    peer: OwnerAddr,
    now_ms: u64,
) {
    let plan = {
        let state = crdt_state.lock().await;
        let wm = watermarks.lock().await;
        plan_reconnect_resends(&state, self_owner, peer, &wm)
    };
    for space_id in plan {
        let up_to_ms = match watermarks.lock().await.get(&space_id) {
            Some(v) => *v,
            None => continue,
        };
        maybe_send_read_receipt(
            crdt_state,
            mgr,
            signing_key,
            signing_device_hash,
            self_owner,
            device_id,
            watermarks,
            space_id,
            up_to_ms,
            now_ms,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_crdt::OwnerState;
    use crate::owner_state_types::DmContentKey;

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "d".into(),
        }
    }

    fn state_with_space(
        kind: SpaceKind,
        pref: Option<ReadReceiptPref>,
        me: OwnerAddr,
        peer: OwnerAddr,
    ) -> (OwnerState, SpaceId) {
        let space_id = SpaceId([0x5A; 16]);
        let mut st = OwnerState::default();
        let mut members = vec![me, peer];
        members.sort();
        let content_key = if matches!(kind, SpaceKind::Dm | SpaceKind::GroupDm) {
            Some(DmContentKey::new([0x22; 32]))
        } else {
            None
        };
        st.spaces.insert(
            space_id,
            Space {
                id: space_id,
                kind,
                parent: None,
                community_id: None,
                name: "s".into(),
                transport: None,
                members,
                custom_name: None,
                notification_pref: None,
                read_receipt_pref: pref,
                left_at: None,
                created_at: hlc(1),
                updated_at: hlc(1),
                content_key,
                prior_content_keys: vec![],
                current_epoch: None,
                current_epoch_key: None,
                old_epoch_keys: std::collections::BTreeMap::new(),
                admin_addr: None,
                is_invite_only: None,
                shared_in_profile: false,
                pending_join_at: None,
            },
        );
        (st, space_id)
    }

    #[test]
    fn prepares_only_for_1to1_dm_broadcast() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let me = OwnerAddr([1; 16]);
        let peer = OwnerAddr([2; 16]);
        let (st, space_id) =
            state_with_space(SpaceKind::Dm, Some(ReadReceiptPref::Broadcast), me, peer);
        let (got_peer, wire) =
            prepare_read_receipt(&st, me, DeviceIdentityHash([1; 16]), &sk, "dev", space_id, 1234, 5678)
                .expect("Broadcast 1:1 DM prepares a receipt");
        assert_eq!(got_peer, peer);
        match crate::dm_envelope::decode_packet(&wire).unwrap() {
            crate::dm_envelope::DmPacket::ReadReceipt { signed, .. } => {
                assert_eq!(signed.read_up_to.wall_ms, 1234);
                assert_eq!(signed.sent_at.wall_ms, 5678);
                assert_eq!(signed.space_id, space_id);
                assert_eq!(signed.sender_owner_addr, me);
            }
            other => panic!("expected ReadReceipt, got {other:?}"),
        }
    }

    #[test]
    fn no_receipt_when_off_group_or_channel() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let me = OwnerAddr([1; 16]);
        let peer = OwnerAddr([2; 16]);
        for (pref, kind) in [
            (Some(ReadReceiptPref::Off), SpaceKind::Dm),
            (None, SpaceKind::Dm),
            (Some(ReadReceiptPref::Broadcast), SpaceKind::GroupDm), // 1:1 only this cut
            (Some(ReadReceiptPref::Broadcast), SpaceKind::Channel),
        ] {
            let (st, space_id) = state_with_space(kind, pref, me, peer);
            assert!(
                prepare_read_receipt(&st, me, DeviceIdentityHash([1; 16]), &sk, "dev", space_id, 1, 2)
                    .is_none(),
                "pref={pref:?} kind={kind:?} must not prepare a receipt"
            );
        }
    }

    fn dm_between(
        id: SpaceId,
        me: OwnerAddr,
        peer: OwnerAddr,
        pref: Option<ReadReceiptPref>,
    ) -> Space {
        let mut members = vec![me, peer];
        members.sort();
        Space {
            id,
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "dm".into(),
            transport: None,
            members,
            custom_name: None,
            notification_pref: None,
            read_receipt_pref: pref,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(1),
            content_key: Some(DmContentKey::new([0x22; 32])),
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

    #[test]
    fn plan_reconnect_resends_only_opted_in_dms_with_stored_watermark() {
        let me = OwnerAddr([1; 16]);
        let peer = OwnerAddr([2; 16]);
        let other = OwnerAddr([3; 16]);
        let mut st = OwnerState::default();
        let a = dm_between(SpaceId([0xA0; 16]), me, peer, Some(ReadReceiptPref::Broadcast));
        let b = dm_between(SpaceId([0xB0; 16]), me, peer, Some(ReadReceiptPref::Off));
        let c = dm_between(SpaceId([0xC0; 16]), me, other, Some(ReadReceiptPref::Broadcast));
        let (a_id, b_id, c_id) = (a.id, b.id, c.id);
        for s in [a, b, c] {
            st.spaces.insert(s.id, s);
        }
        let mut wm = std::collections::HashMap::new();
        wm.insert(a_id, 999u64); // a: opted-in + stored watermark → included
        wm.insert(b_id, 5u64); // b: Off → excluded regardless of a stored watermark
        // c: opted-in but a different peer AND no stored watermark → excluded
        let plan = plan_reconnect_resends(&st, me, peer, &wm);
        assert_eq!(plan, vec![a_id]);
        let _ = (b_id, c_id);
    }
}
