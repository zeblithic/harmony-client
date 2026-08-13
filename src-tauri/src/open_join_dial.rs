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
use crate::community_rendezvous::{rendezvous_config_from_env, resolve_rendezvous};
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
        &rendezvous_config_from_env(),
    )
    .await;
    // Surface the resolve instrumentation at `info` so the success-rate/latency
    // tuning data (the spec's open question) is visible without `RUST_LOG=debug`.
    tracing::info!(
        winning_slot = ?resolve.winning_slot,
        elapsed_ms = resolve.elapsed_ms,
        batches_tried = resolve.batches_tried,
        found_beacon = resolve.payload.is_some(),
        "open-join: rendezvous resolve complete"
    );
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
    //
    // Re-sample the wall clock HERE (after the awaited rendezvous resolution +
    // dial/open_bi) rather than reusing the `now_ms` captured before resolve:
    // a slow resolve/dial would otherwise stamp a stale `created_at`, which the
    // beacon's freshness window could reject as stale despite the join being
    // valid. The cold-start branch above (beacon = None) returns before reaching
    // this point and is unaffected.
    let fresh_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(now_ms);
    let nonce: [u8; 16] = {
        use rand::RngCore;
        let mut n = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut n);
        n
    };
    let created_at = Hlc {
        wall_ms: fresh_now_ms,
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
    let write_prefix = async {
        // Caller-bounded packet; no historical write cap, so cap at the wire-
        // representable max (u32::MAX) — preserves behavior, keeps the prefix
        // u32-safe, and unifies byte order.
        let prefix = crate::iroh_framing::encode_len_prefix(
            wire.len(),
            u32::MAX as usize,
            crate::iroh_framing::Endian::Le,
            false,
        )
        .map_err(|e| format!("length-prefix out of bounds: {e}"))?;
        send.write_all(&prefix)
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
        let len = crate::iroh_framing::decode_len_prefix(
            len_buf,
            crate::iroh_invite_acceptor::HANDSHAKE_MAX_PACKET_LEN,
            crate::iroh_framing::Endian::Le,
            false,
        )
        .map_err(|e| format!("response length out of bounds: len={} max={}", e.len, e.max))?;
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
            // Apply the admitted snapshot in dependency order. The beacon
            // harvests its event log in `HashMap` (hash) order, so an
            // admin-signed steady-state event (e.g. ChannelCreate) can precede
            // the admin's enrollment-introducing Join; applied naively in one
            // pass it would fail `verify_event` with `SignerNotEnrolledForActor`
            // and be dropped, leaving the joiner channel-less (ZEB-573).
            // `apply_admitted_snapshot` retries the rejected remainder until it
            // stabilizes, so enrollment-dependent events land once their
            // dependency is materialized. A duplicate (e.g. our own Join already
            // inserted by the open-redeem local arm) is an idempotent no-op.
            let applied = apply_admitted_snapshot(engine.as_ref(), member_events).await;
            tracing::info!(
                community_id = %hex::encode(ctx.community_id.0),
                applied,
                "open-join: admitted — applied membership snapshot (dependency-ordered)"
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

/// Applies one admitted membership event to the joiner's local state.
/// Implemented by [`CommunitySyncEngine`] in production; a unit-test double
/// models the enrollment-ordering dependency. The `String` error is the
/// non-ordering failure class (transport / wrong-community) — distinct from a
/// semantic `InsertOutcome::Rejected`, which `apply_admitted_snapshot` may
/// resolve on a later round.
#[async_trait::async_trait]
pub(crate) trait SnapshotEventApplier {
    async fn apply_one(
        &self,
        ev: crate::community_membership::SignedMembershipEvent,
    ) -> Result<crate::community_state_crdt::InsertOutcome, String>;
}

#[async_trait::async_trait]
impl SnapshotEventApplier for CommunitySyncEngine {
    async fn apply_one(
        &self,
        ev: crate::community_membership::SignedMembershipEvent,
    ) -> Result<crate::community_state_crdt::InsertOutcome, String> {
        self.insert_local_event(ev)
            .await
            .map_err(|e| format!("{e:?}"))
    }
}

/// Apply an admitted open-join membership snapshot in dependency order.
///
/// ZEB-573: the beacon harvests its community event log in `HashMap` (hash)
/// order, so an admin-signed steady-state event (channel-create, config) can
/// land *before* the admin's enrollment-introducing Join. `verify_event` then
/// rejects it with `SignerNotEnrolledForActor` (the actor's enrolled device key
/// isn't materialized yet). A single naive pass would drop such events
/// permanently — the joiner would converge into membership but never receive the
/// channels.
///
/// Two-tier ordering. First sort by [`event_sort_key`] — the canonical replay
/// comparator the inbound merge path (`community_state_sync`) sorts by to apply
/// a partially-ordered DAG-sync blob without order-caused verify rejections — so
/// the common case (an actor's Join carries an earlier HLC than its dependent
/// events) converges in a single pass with no re-verification. Then
/// retry-until-stable as a fallback for any residual dependency the sort key
/// doesn't capture: re-apply the rejected remainder each round; every round that
/// materializes a new enrollment unblocks its dependents, bounded by
/// `member_events.len()` rounds. A round with zero progress means the remainder
/// is genuinely unverifiable — logged per-event with its `VerifyError` (matching
/// the merge path's diagnostics) and dropped. A duplicate (e.g. the joiner's own
/// Join, already inserted by the open-redeem local arm) is an idempotent
/// `AlreadyKnown`. Returns the number of events that landed.
///
/// ZEB-927: this is also the invite-only redeem path's snapshot applier. The
/// invite acceptor now ships the FULL membership roster in its handshake
/// response (not just the countersigner's narrow admission chain), so a joiner
/// with no Zenoh path to its inviter still converges the whole roster instead of
/// islanding. The acceptor's roster arrives in canonical (EventId-hash) order,
/// so the same order-tolerant fixpoint merge is required — a single naive pass
/// would drop (or, on the old redeem loop, hard-fail) any event that precedes
/// its dependency. Same drop-on-unresolvable semantics.
pub(crate) async fn apply_admitted_snapshot<A: SnapshotEventApplier>(
    applier: &A,
    member_events: Vec<crate::community_membership::SignedMembershipEvent>,
) -> usize {
    use crate::community_membership::event_sort_key;
    use crate::community_state_crdt::InsertOutcome;
    // Canonical replay order first (mirrors the inbound merge path); the retry
    // loop below is the fallback for anything the sort key can't capture.
    let mut pending = member_events;
    pending.sort_by(|a, b| event_sort_key(a).cmp(&event_sort_key(b)));
    let mut applied = 0usize;
    loop {
        if pending.is_empty() {
            break;
        }
        let applied_before = applied;
        // Carry the `VerifyError` with each deferred event so the no-progress
        // path can report which events failed and why (Qodo: parity with the
        // merge path's per-event verify diagnostics).
        let mut deferred: Vec<(
            crate::community_membership::SignedMembershipEvent,
            crate::community_membership::VerifyError,
        )> = Vec::new();
        for ev in std::mem::take(&mut pending) {
            match applier.apply_one(ev.clone()).await {
                Ok(InsertOutcome::Inserted) | Ok(InsertOutcome::AlreadyKnown) => applied += 1,
                // A semantic rejection may just be an ordering miss (a dependency
                // not yet materialized) — keep the reason and defer to a later round.
                Ok(InsertOutcome::Rejected(v)) => deferred.push((ev, v)),
                // A non-ordering failure (transport / wrong-community) can't be
                // resolved by re-application; log and drop.
                Err(e) => tracing::warn!(
                    error = %e,
                    "open-join: applying an admitted member event failed (continuing)"
                ),
            }
        }
        if applied == applied_before {
            // No event landed this round — the remainder can't be unblocked by
            // re-application. Log each with its rejection reason, then stop.
            for (ev, reason) in &deferred {
                tracing::warn!(
                    community_id = %hex::encode(ev.community_id.0),
                    event_id = %hex::encode(ev.id),
                    kind = ?ev.kind,
                    ?reason,
                    "open-join: admitted snapshot event never verified (dropping)"
                );
            }
            break;
        }
        pending = deferred.into_iter().map(|(ev, _)| ev).collect();
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_membership::{MembershipEventKind, SignedMembershipEvent};
    use crate::owner_state_types::OwnerAddr;

    fn sample_bootstrap_join() -> SignedMembershipEvent {
        SignedMembershipEvent {
            signer_certs: Vec::new(),
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

    /// Test double modeling `verify_event`'s enrollment dependency: a Join is
    /// self-authorizing (always lands, materializing the actor's enrollment); a
    /// steady-state event (e.g. ChannelCreate) is Rejected with
    /// `SignerNotEnrolledForActor` until its actor's Join has been applied.
    struct OrderingMockApplier {
        enrolled: tokio::sync::Mutex<std::collections::HashSet<OwnerAddr>>,
        apply_order: tokio::sync::Mutex<Vec<[u8; 16]>>,
    }

    #[async_trait::async_trait]
    impl SnapshotEventApplier for OrderingMockApplier {
        async fn apply_one(
            &self,
            ev: SignedMembershipEvent,
        ) -> Result<crate::community_state_crdt::InsertOutcome, String> {
            use crate::community_membership::VerifyError;
            use crate::community_state_crdt::InsertOutcome;
            match &ev.kind {
                MembershipEventKind::Join => {
                    self.enrolled.lock().await.insert(ev.actor);
                    self.apply_order.lock().await.push(ev.id);
                    Ok(InsertOutcome::Inserted)
                }
                _ => {
                    if self.enrolled.lock().await.contains(&ev.actor) {
                        self.apply_order.lock().await.push(ev.id);
                        Ok(InsertOutcome::Inserted)
                    } else {
                        Ok(InsertOutcome::Rejected(
                            VerifyError::SignerNotEnrolledForActor,
                        ))
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn admitted_snapshot_applies_enrollment_dependent_events_out_of_order() {
        // ZEB-573 regression. `apply_admitted_snapshot` first sorts by
        // `event_sort_key` (which fixes the common hash-order case in one pass),
        // then retries-until-stable as a fallback. To exercise that FALLBACK,
        // this fixture inverts the HLCs so the sort still orders the admin
        // ChannelCreate (wall_ms 10) BEFORE the admin Join (wall_ms 20) — an
        // enrollment dependency the sort key can't capture. A naive single pass
        // drops the ChannelCreate (SignerNotEnrolledForActor); the apply must
        // still land both, with the Join materialized first.
        let admin = OwnerAddr([0x21; 16]);
        let community = SpaceId([0x42; 16]);
        let admin_join = SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0xA0; 16],
            community_id: community,
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc {
                wall_ms: 20,
                logical: 0,
                device_id: "admin".into(),
            },
            sig: [0u8; 64],
            countersig: None,
            enrollment: None,
        };
        let channel_create = SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0xC0; 16],
            community_id: community,
            kind: MembershipEventKind::ChannelCreate {
                channel_id: crate::community_membership::ChannelId([0x01; 16]),
                name: "general".into(),
                write_power: 0,
                kind: crate::community_membership::ChannelKind::Text,
            },
            actor: admin,
            at: Hlc {
                wall_ms: 10,
                logical: 0,
                device_id: "admin".into(),
            },
            sig: [0u8; 64],
            countersig: None,
            enrollment: None,
        };
        // Input order is irrelevant (the apply sorts first); the HLC inversion
        // above is what forces the channel-create ahead of the Join post-sort.
        let events = vec![channel_create.clone(), admin_join.clone()];

        let mock = OrderingMockApplier {
            enrolled: tokio::sync::Mutex::new(std::collections::HashSet::new()),
            apply_order: tokio::sync::Mutex::new(Vec::new()),
        };
        let applied = apply_admitted_snapshot(&mock, events).await;

        assert_eq!(
            applied, 2,
            "both events must land once the snapshot is applied in dependency order"
        );
        let order = mock.apply_order.lock().await.clone();
        assert_eq!(
            order,
            vec![admin_join.id, channel_create.id],
            "the admin Join must materialize before the channel-create lands"
        );
    }
}
