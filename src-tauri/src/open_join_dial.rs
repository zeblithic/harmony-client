//! Joiner-side open-community first contact: resolve a live beacon from the
//! DHT rendezvous slots, dial it on `HARMONY_HANDSHAKE_V1`, send an
//! [`OpenJoinRequest`], and apply the admitted membership snapshot.
//!
//! This is the open/tokenless counterpart to the invite-only
//! `connectivity_redeem_invite_iroh_inner` (`lib.rs`). It mirrors that
//! function's iroh dial idioms (addr synthesis from a
//! [`ReachabilityAnnouncePayload`], bounded `connect`/`open_bi`/write/read via
//! [`HandshakeDialConfig`], `[u32 LE len-prefix][wire]` framing) but swaps the
//! invite-token packet for a capability-proven open-join packet and decodes an
//! [`OpenJoinResponse`] instead of a `JoinCountersign`.
//!
//! Cold start (no live beacon resolved) or any dial/transport failure returns a
//! RETRYABLE non-error (`status = "no_beacon_reachable"`), mirroring the
//! invite-only path's `"inviter_unreachable"` non-error returns — the open
//! redeem flow re-attempts first contact on the next transport-epoch re-arm.

use std::sync::Arc;

use crate::community_invite::{
    build_signed_open_join_packet, device_hash_from_identity_pub, encode_packet, OpenJoinRequest,
    OpenJoinResponse,
};
use crate::community_rendezvous::{resolve_rendezvous, RendezvousResolveConfig};
use crate::community_state_sync::CommunitySyncEngine;
use crate::iroh_endpoint::IrohEndpoint;
use crate::open_join_auth::mint_epoch_auth;
use crate::owner_state_types::{DeviceIdentityHash, EpochKey, Hlc, SpaceId};
use crate::reachability_record::ReachabilityAnnouncePayload;

/// Outcome of a joiner open-join attempt. Mirrors `RedemptionOutcome`'s shape
/// (a status string + optional hex community id) so the open-redeem wiring
/// (Task 11) can map it to the redeem DTO uniformly.
///
/// Statuses:
/// * `"joined"` — a beacon admitted us and we applied its membership snapshot.
/// * `"no_beacon_reachable"` — no live beacon resolved (cold start) OR the dial
///   / open_bi / write / read failed before a response landed. RETRYABLE.
/// * `"beacon_rejected"` — a beacon was reached but rejected the request
///   (capability/freshness/ban/rate-limit). The `reason` is logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenJoinOutcome {
    pub status: String,
    pub community_id: Option<String>,
}

impl OpenJoinOutcome {
    fn no_beacon(community_id: &SpaceId) -> Self {
        Self {
            status: "no_beacon_reachable".to_string(),
            community_id: Some(hex::encode(community_id.0)),
        }
    }
}

/// Dial-side context for [`open_join_after_resolve`] / [`connectivity_open_join_iroh_inner`].
///
/// The cold-start branch (`beacon = None`) reads only `community_id`, so the
/// iroh endpoint and engine handle are `Option` — a unit test can build a
/// minimal `ctx` with both `None` and never construct an iroh endpoint or a
/// `CommunitySyncEngine`. Production always supplies both.
pub struct OpenJoinDialCtx {
    /// The open community we're joining (its 16-byte `SpaceId`).
    pub community_id: SpaceId,
    /// The community `epoch_key` (the link capability). Used to mint
    /// `epoch_auth` and (upstream) to resolve the rendezvous slots.
    pub epoch_key: EpochKey,
    /// The joiner's self-signed bootstrap Join event (minted by the open
    /// redeem path; Task 11 passes it in — we do NOT re-mint here).
    pub bootstrap_join: crate::community_membership::SignedMembershipEvent,
    /// The joiner's identity signing key (the dm_outbox signing key in
    /// production). Signs the open-join packet envelope; its X25519/Ed25519
    /// derivation must hash to `bootstrap_join.actor` so the beacon's
    /// device-hash binding (Task 5) passes.
    pub signing_key: Arc<ed25519_dalek::SigningKey>,
    /// iroh endpoint used to dial the beacon. `None` → treated as
    /// unreachable (no way to dial), mirroring the invite-only path.
    pub iroh_endpoint: Option<Arc<IrohEndpoint>>,
    /// The joiner's `CommunitySyncEngine` for this community. The admitted
    /// snapshot is applied event-by-event via `insert_local_event`. `None`
    /// only in the cold-start unit test (which never reaches the apply).
    pub engine: Option<Arc<CommunitySyncEngine>>,
    /// Per-await dial timeouts (production: `HandshakeDialConfig::from_env`).
    pub dial_config: crate::HandshakeDialConfig,
}

/// Joiner entry point: resolve a live beacon from the DHT rendezvous slots,
/// then dial + send + apply via [`open_join_after_resolve`].
///
/// `pkarr` is the joiner's resolver; `now_ms` is the wall clock used for both
/// the rendezvous freshness check and the `epoch_auth` timestamp binding.
///
/// On empty resolve → `Ok(OpenJoinOutcome { status: "no_beacon_reachable" })`
/// (RETRYABLE, NOT `Err`). The dial branch returns `Err` only on an internal
/// build/encode invariant violation; transport failures are non-error
/// retryable outcomes.
pub async fn connectivity_open_join_iroh_inner(
    pkarr: Arc<harmony_pkarr::PkarrResolver>,
    ctx: OpenJoinDialCtx,
    now_ms: u64,
) -> Result<OpenJoinOutcome, String> {
    let resolve = resolve_rendezvous(
        &pkarr,
        &ctx.epoch_key,
        now_ms,
        &RendezvousResolveConfig::from_env(),
    )
    .await;
    open_join_after_resolve(resolve.payload, ctx, now_ms).await
}

/// Post-resolve dial logic, factored out so the cold-start branch is
/// unit-testable without iroh: `beacon = None` returns the retryable
/// `"no_beacon_reachable"` outcome and never dials. The `Some` branch is
/// covered by the Task 12 integration test (live loopback beacon).
pub async fn open_join_after_resolve(
    beacon: Option<ReachabilityAnnouncePayload>,
    ctx: OpenJoinDialCtx,
    now_ms: u64,
) -> Result<OpenJoinOutcome, String> {
    // 1. Cold start: no live beacon resolved → retryable non-error.
    let Some(beacon) = beacon else {
        tracing::info!(
            community_id = %hex::encode(ctx.community_id.0),
            "open-join: no live beacon resolved (cold start) — retryable"
        );
        return Ok(OpenJoinOutcome::no_beacon(&ctx.community_id));
    };

    // 2. We have a beacon. Acquire the iroh endpoint; absent = unreachable.
    let Some(iroh_endpoint) = ctx.iroh_endpoint.as_ref() else {
        tracing::warn!(
            community_id = %hex::encode(ctx.community_id.0),
            "open-join: resolved a beacon but no iroh endpoint to dial — treating as unreachable"
        );
        return Ok(OpenJoinOutcome::no_beacon(&ctx.community_id));
    };

    // 3. Synthesize the beacon's EndpointAddr from the verified reachability
    // payload (iroh_node_id + home_relay_url + direct_addresses). Mirrors
    // `endpoint_addr_from_routing` in lib.rs.
    let beacon_addr = match crate::endpoint_addr_from_routing(&beacon) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(
                error = %e,
                community_id = %hex::encode(ctx.community_id.0),
                "open-join: failed to synthesize beacon EndpointAddr — treating as unreachable"
            );
            return Ok(OpenJoinOutcome::no_beacon(&ctx.community_id));
        }
    };

    // 4. Dial the beacon on the handshake ALPN, bounded by connect_timeout.
    let conn = match tokio::time::timeout(
        ctx.dial_config.connect_timeout,
        iroh_endpoint.inner().connect(
            beacon_addr,
            crate::iroh_endpoint::alpn::HARMONY_HANDSHAKE_V1,
        ),
    )
    .await
    {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "open-join: iroh connect failed");
            return Ok(OpenJoinOutcome::no_beacon(&ctx.community_id));
        }
        Err(_elapsed) => {
            tracing::warn!(
                timeout_ms = ctx.dial_config.connect_timeout.as_millis() as u64,
                "open-join: iroh connect timeout (dial)"
            );
            return Ok(OpenJoinOutcome::no_beacon(&ctx.community_id));
        }
    };

    // 5. Open the bi-directional stream, bounded by open_bi_timeout.
    let (mut send, mut recv) =
        match tokio::time::timeout(ctx.dial_config.open_bi_timeout, conn.open_bi()).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "open-join: open_bi failed");
                conn.close(0u32.into(), b"open_bi-failed");
                return Ok(OpenJoinOutcome::no_beacon(&ctx.community_id));
            }
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_ms = ctx.dial_config.open_bi_timeout.as_millis() as u64,
                    "open-join: open_bi timeout"
                );
                conn.close(0u32.into(), b"open_bi-timeout");
                return Ok(OpenJoinOutcome::no_beacon(&ctx.community_id));
            }
        };

    // 6. Derive the joiner identity composite the same way the invite-only
    // sender does (X25519-from-Ed25519 birational map || Ed25519 pub), so the
    // device-hash binding the beacon checks (Task 5) reproduces `actor`.
    let joiner_identity_pub = joiner_identity_pub_from_signing_key(ctx.signing_key.as_ref());
    let signing_device_hash =
        DeviceIdentityHash(device_hash_from_identity_pub(&joiner_identity_pub));

    // 7. Build the capability-proven OpenJoinRequest. The `epoch_auth`
    // timestamp MUST be `created_at.wall_ms` — the beacon's verify recomputes
    // the MAC over exactly that field (Task 5/6).
    let nonce: [u8; 16] = {
        use rand::RngCore;
        let mut n = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut n);
        n
    };
    let created_at = Hlc {
        wall_ms: now_ms,
        logical: 0,
        device_id: ctx.bootstrap_join.at.device_id.clone(),
    };
    let epoch_auth = mint_epoch_auth(
        &ctx.epoch_key,
        &ctx.community_id,
        &joiner_identity_pub,
        &nonce,
        created_at.wall_ms,
    );
    let req = OpenJoinRequest {
        community_id: ctx.community_id,
        join_event: ctx.bootstrap_join.clone(),
        joiner_identity_pub,
        signing_device_hash,
        epoch_auth,
        nonce,
        created_at,
    };
    let packet = build_signed_open_join_packet(req, ctx.signing_key.as_ref())
        .map_err(|e| format!("build_signed_open_join_packet: {e}"))?;
    let wire = encode_packet(&packet).map_err(|e| format!("encode_packet: {e}"))?;

    // 8. Write [u32 LE len][wire] then finish(), each bounded by write_timeout.
    let wire_len = wire.len() as u32;
    let write_prefix = async {
        send.write_all(&wire_len.to_le_bytes())
            .await
            .map_err(|e| format!("write length-prefix: {e}"))
    };
    match tokio::time::timeout(ctx.dial_config.write_timeout, write_prefix).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "open-join: request length-prefix write failed");
            conn.close(0u32.into(), b"request-write-failed");
            return Ok(OpenJoinOutcome::no_beacon(&ctx.community_id));
        }
        Err(_elapsed) => {
            tracing::warn!(
                timeout_ms = ctx.dial_config.write_timeout.as_millis() as u64,
                "open-join: request length-prefix write timeout"
            );
            conn.close(0u32.into(), b"write-timeout");
            return Ok(OpenJoinOutcome::no_beacon(&ctx.community_id));
        }
    }
    let write_body = async {
        send.write_all(&wire)
            .await
            .map_err(|e| format!("write packet body: {e}"))
    };
    match tokio::time::timeout(ctx.dial_config.write_timeout, write_body).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "open-join: request body write failed");
            conn.close(0u32.into(), b"request-write-failed");
            return Ok(OpenJoinOutcome::no_beacon(&ctx.community_id));
        }
        Err(_elapsed) => {
            tracing::warn!(
                timeout_ms = ctx.dial_config.write_timeout.as_millis() as u64,
                "open-join: request body write timeout"
            );
            conn.close(0u32.into(), b"write-timeout");
            return Ok(OpenJoinOutcome::no_beacon(&ctx.community_id));
        }
    }
    if let Err(e) = send.finish() {
        tracing::warn!(error = %e, "open-join: send.finish() failed");
        conn.close(0u32.into(), b"send-finish-failed");
        return Ok(OpenJoinOutcome::no_beacon(&ctx.community_id));
    }

    // 9. Read [u32 LE len][CBOR OpenJoinResponse], bounded by response_read_timeout.
    let read_response = async {
        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf)
            .await
            .map_err(|e| format!("read length-prefix: {e}"))?;
        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 || len > crate::iroh_invite_acceptor::HANDSHAKE_MAX_PACKET_LEN {
            return Err(format!(
                "response length out of bounds: len={} max={}",
                len,
                crate::iroh_invite_acceptor::HANDSHAKE_MAX_PACKET_LEN
            ));
        }
        let mut body = vec![0u8; len];
        recv.read_exact(&mut body)
            .await
            .map_err(|e| format!("read response body: {e}"))?;
        Ok::<Vec<u8>, String>(body)
    };
    let response_bytes =
        match tokio::time::timeout(ctx.dial_config.response_read_timeout, read_response).await {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "open-join: response read failed");
                conn.close(0u32.into(), b"response-read-failed");
                return Ok(OpenJoinOutcome::no_beacon(&ctx.community_id));
            }
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_ms = ctx.dial_config.response_read_timeout.as_millis() as u64,
                    "open-join: response read timeout"
                );
                conn.close(0u32.into(), b"response-read-timeout");
                return Ok(OpenJoinOutcome::no_beacon(&ctx.community_id));
            }
        };

    // 10. Decode the OpenJoinResponse.
    let response: OpenJoinResponse = match ciborium::from_reader(response_bytes.as_slice()) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "open-join: response CBOR decode failed");
            conn.close(0u32.into(), b"response-decode-failed");
            return Ok(OpenJoinOutcome::no_beacon(&ctx.community_id));
        }
    };

    // The dialer drove the handshake to completion; close the connection so the
    // beacon's symmetric `conn.closed()` wait releases. (The beacon bounds
    // itself by its io_deadline regardless.)
    drop(send);
    drop(recv);
    conn.close(0u32.into(), b"open-join-complete");
    drop(conn);

    // 11. Apply the response.
    match response {
        OpenJoinResponse::Admitted { member_events } => {
            let Some(engine) = ctx.engine.as_ref() else {
                // A reached-and-admitted beacon with no local engine to apply
                // into is an internal misconfiguration (production always wires
                // the engine). Surface loudly rather than silently dropping the
                // admitted snapshot.
                return Err(
                    "open-join: admitted but no engine handle to apply the snapshot".to_string(),
                );
            };
            // Apply each event to the joiner's engine so it materializes the
            // live membership. `insert_local_event` runs `verify_event` (each
            // event is self-authorizing via its carried cert / materialized
            // membership); a duplicate (e.g. our own Join, if already inserted
            // by the open-redeem local arm) is an idempotent no-op.
            let mut applied = 0usize;
            for ev in member_events {
                match engine.insert_local_event(ev).await {
                    Ok(_) => applied += 1,
                    Err(e) => {
                        // Non-fatal: a single bad/duplicate event must not abort
                        // the bootstrap. Log and continue; the engine converges
                        // the rest over Zenoh.
                        tracing::warn!(
                            error = %e,
                            "open-join: applying an admitted member event failed (continuing)"
                        );
                    }
                }
            }
            tracing::info!(
                community_id = %hex::encode(ctx.community_id.0),
                applied,
                "open-join: admitted — applied membership snapshot"
            );
            Ok(OpenJoinOutcome {
                status: "joined".to_string(),
                community_id: Some(hex::encode(ctx.community_id.0)),
            })
        }
        OpenJoinResponse::Rejected { reason } => {
            tracing::warn!(
                community_id = %hex::encode(ctx.community_id.0),
                reason = %reason,
                "open-join: beacon rejected the request"
            );
            Ok(OpenJoinOutcome {
                status: "beacon_rejected".to_string(),
                community_id: Some(hex::encode(ctx.community_id.0)),
            })
        }
    }
}

/// Derive the 64-byte joiner identity composite (`X25519_pub(32) ||
/// Ed25519_pub(32)`) from the signing key, via the SAME RFC 7748 §5
/// birational map the invite-only sender uses (`lib.rs` option-A derivation).
/// The composite must hash (via `device_hash_from_identity_pub`) to the
/// device hash the beacon binds against the Join's actor.
fn joiner_identity_pub_from_signing_key(sk: &ed25519_dalek::SigningKey) -> [u8; 64] {
    let x25519_priv = crate::dm_signing::ed25519_priv_to_x25519(sk);
    let x25519_pub = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*x25519_priv));
    let ed25519_pub = sk.verifying_key().to_bytes();
    let mut joiner_pub = [0u8; 64];
    joiner_pub[..32].copy_from_slice(x25519_pub.as_bytes());
    joiner_pub[32..].copy_from_slice(&ed25519_pub);
    joiner_pub
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_membership::{MembershipEventKind, SignedMembershipEvent};
    use crate::owner_state_types::OwnerAddr;

    fn sample_bootstrap_join() -> SignedMembershipEvent {
        SignedMembershipEvent {
            id: [1u8; 16],
            community_id: SpaceId([2u8; 16]),
            kind: MembershipEventKind::Join,
            actor: OwnerAddr([3u8; 16]),
            at: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "joiner-dev".into(),
            },
            sig: [0u8; 64],
            countersig: None,
            enrollment: None,
        }
    }

    /// Minimal cold-start context: no iroh endpoint, no engine — the
    /// `beacon = None` branch never dials, so neither is touched.
    fn test_ctx() -> OpenJoinDialCtx {
        OpenJoinDialCtx {
            community_id: SpaceId([2u8; 16]),
            epoch_key: EpochKey::new([7u8; 32]),
            bootstrap_join: sample_bootstrap_join(),
            signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[5u8; 32])),
            iroh_endpoint: None,
            engine: None,
            dial_config: crate::HandshakeDialConfig::default(),
        }
    }

    #[tokio::test]
    async fn cold_start_is_retryable_non_error() {
        // Inject a resolver outcome of "no live beacon" and assert the function
        // returns Ok(status="no_beacon_reachable"), not Err.
        let outcome = open_join_after_resolve(None, test_ctx(), 1_000_000).await;
        assert!(outcome.is_ok(), "cold start must be a non-error");
        let outcome = outcome.unwrap();
        assert_eq!(outcome.status, "no_beacon_reachable");
        assert_eq!(outcome.community_id, Some(hex::encode([2u8; 16])));
    }
}
