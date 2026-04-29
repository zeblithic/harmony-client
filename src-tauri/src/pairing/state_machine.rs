use crate::pairing::cert::{sign_enrollment_for_joiner, verify_cert_for_self};
use crate::pairing::sas::derive_sas;
use crate::pairing::session::{decrypt as session_decrypt, encrypt as session_encrypt};
use crate::pairing::transport::PairingTransport;
use crate::pairing::types::{
    DiscoveredPeer, EncryptedPayload, PairingRole, PairingState, PairingWireMessage,
};
use ed25519_dalek::SigningKey;
use harmony_owner::certs::EnrollmentCert;
use harmony_owner::pubkey_bundle::PubKeyBundle;
use harmony_owner::state::OwnerState;
use harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

/// Default interval between DISCOVER re-broadcasts while we're still in
/// the discovery phase (no peer selected yet). Zenoh pub/sub doesn't
/// replay past publications to late-joining subscribers — without periodic
/// re-emit, an Inviter that started its wizard before the Joiner subscribed
/// would be invisible to the Joiner forever, breaking the documented
/// "Inviter clicks Add another device first, Joiner second" flow. Tests
/// use shorter values via [`spawn_state_machine`]'s parameter.
pub const DEFAULT_DISCOVER_REBROADCAST_INTERVAL: Duration = Duration::from_millis(2500);
use x25519_dalek::{PublicKey as X25519Pub, StaticSecret as X25519Sec};
use zeroize::Zeroizing;

/// Inputs to the state machine from the UI layer.
pub enum PairingCommand {
    StartInviter {
        display_name: String,
        owner_state: OwnerState,
        master_seed: Zeroizing<[u8; 32]>,
    },
    StartJoiner {
        display_name: String,
        signing_key: SigningKey,
    },
    SelectPeer {
        peer_session_id: Uuid,
    },
    ConfirmSas,
    Cancel,
}

/// Output from the state machine for the Joiner side, when enrollment succeeds.
/// The persistence layer (Task 7) consumes this to write keys + state to disk.
pub struct JoinerEnrollResult {
    pub our_signing_key: SigningKey,
    pub owner_state: OwnerState,
    pub our_device_id: [u8; 16],
}

/// Output from the state machine for the Inviter side, when enrollment of a
/// new peer device succeeds. The persistence layer consumes this to merge
/// the new enrollment into the freshest on-disk `OwnerState` and write back
/// to disk so the new enrollment survives restarts. Unlike the Joiner
/// result, this carries no signing key — the Inviter already has its own
/// signing key persisted on disk; the persistence layer reloads it via
/// `load_owner_state` and rewrites the state file atomically alongside the
/// existing key.
///
/// ZEB-199: this carries the signed cert + the timestamp the SM used at
/// signing time, NOT the SM's mutated `OwnerState`. The persist layer
/// (`install_inviter_state_inner`) re-applies the cert against the freshly-
/// loaded disk state under `OWNER_STATE_WRITE_LOCK`. Doing so on a snapshot
/// the SM produced earlier would silently drop any other enrollment that
/// landed on disk between the SM's snapshot and our write — see ZEB-199 for
/// the corresponding test that pins this invariant.
pub struct InviterEnrollResult {
    /// The freshly-signed enrollment cert for the new peer device.
    pub cert: EnrollmentCert,
    /// Timestamp (unix seconds) the SM used when signing the cert. Reused
    /// at persist time so `add_enrollment`'s active-window calculation is
    /// consistent with the cert's `issued_at`.
    pub now: u64,
    /// The master seed — Inviter HAS this (cert-only model only restricts
    /// the Joiner). Persisting it via `Some(seed)` keeps `canBackUp: true`.
    pub master_seed: Zeroizing<[u8; 32]>,
}

/// Handle the UI talks to. Drops the state machine on drop.
pub struct PairingHandle {
    pub state_rx: watch::Receiver<PairingState>,
    pub cmd_tx: mpsc::Sender<PairingCommand>,
    pub joiner_result_rx: Option<mpsc::Receiver<JoinerEnrollResult>>,
    pub inviter_result_rx: Option<mpsc::Receiver<InviterEnrollResult>>,
    _shutdown: tokio::task::JoinHandle<()>,
}

pub fn spawn_state_machine(
    transport: Arc<dyn PairingTransport>,
    now_fn: Arc<dyn Fn() -> u64 + Send + Sync>,
    discover_rebroadcast_interval: Duration,
) -> PairingHandle {
    let (state_tx, state_rx) = watch::channel(PairingState::Idle);
    let (cmd_tx, cmd_rx) = mpsc::channel::<PairingCommand>(16);
    let (result_tx, result_rx) = mpsc::channel::<JoinerEnrollResult>(1);
    let (inviter_result_tx, inviter_result_rx) = mpsc::channel::<InviterEnrollResult>(1);

    let task = tokio::spawn(run_state_machine(
        transport,
        state_tx,
        cmd_rx,
        result_tx,
        inviter_result_tx,
        now_fn,
        discover_rebroadcast_interval,
    ));

    PairingHandle {
        state_rx,
        cmd_tx,
        joiner_result_rx: Some(result_rx),
        inviter_result_rx: Some(inviter_result_rx),
        _shutdown: task,
    }
}

async fn run_state_machine(
    transport: Arc<dyn PairingTransport>,
    state_tx: watch::Sender<PairingState>,
    mut cmd_rx: mpsc::Receiver<PairingCommand>,
    result_tx: mpsc::Sender<JoinerEnrollResult>,
    inviter_result_tx: mpsc::Sender<InviterEnrollResult>,
    now_fn: Arc<dyn Fn() -> u64 + Send + Sync>,
    discover_rebroadcast_interval: Duration,
) {
    // Per-session local context. Reset each time we leave a session.
    let mut ctx: Option<SessionCtx> = None;

    // Periodic DISCOVER re-broadcast: see DEFAULT_DISCOVER_REBROADCAST_INTERVAL.
    // `tokio::time::interval` fires the first tick immediately by default;
    // we consume that tick before entering the loop because start_inviter /
    // start_joiner already publishes the initial DISCOVER. MissedTickBehavior::Skip
    // ensures a sleeping system that wakes up doesn't burst many catch-up emits.
    let mut rebroadcast = tokio::time::interval(discover_rebroadcast_interval);
    rebroadcast.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    rebroadcast.tick().await;

    loop {
        // Race fix: when ctx is None, do NOT poll transport.recv. Otherwise an
        // early-arriving wire message (e.g. peer's Discover landing in our
        // transport buffer before our own Start* cmd has been processed) gets
        // grabbed by the transport branch, sees ctx=None, and is silently
        // dropped — leaving us deaf to the peer for the rest of the session.
        //
        // The tokio::select! `if` guard disables the transport branch entirely
        // when ctx is None, so cmd_rx is the only ready branch.
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { return; };
                match cmd {
                    PairingCommand::StartInviter { display_name, owner_state, master_seed } => {
                        // start_inviter returns None on publish failure (state already
                        // pushed to Failed); leave ctx as None so the user must Cancel.
                        ctx = start_inviter(&transport, &state_tx, display_name, owner_state, master_seed, &now_fn).await;
                    }
                    PairingCommand::StartJoiner { display_name, signing_key } => {
                        ctx = start_joiner(&transport, &state_tx, display_name, signing_key, &now_fn).await;
                    }
                    PairingCommand::SelectPeer { peer_session_id } => {
                        if let Some(c) = ctx.as_mut() {
                            on_select_peer(&transport, &state_tx, c, peer_session_id).await;
                        }
                    }
                    PairingCommand::ConfirmSas => {
                        if let Some(c) = ctx.as_mut() {
                            on_confirm_sas(
                                &transport,
                                &state_tx,
                                c,
                                &now_fn,
                                &result_tx,
                                &inviter_result_tx,
                            )
                            .await;
                        }
                    }
                    PairingCommand::Cancel => {
                        if let Some(c) = ctx.as_mut() {
                            let _ = transport.publish(PairingWireMessage::Cancel {
                                my_session_id: c.session_id,
                                peer_session_id: c.selected_peer_session_id,
                                reason: "user cancelled".to_string(),
                            }).await;
                        }
                        ctx = None;
                        let _ = state_tx.send(PairingState::Idle);
                    }
                }
            }
            msg = transport.recv(), if ctx.is_some() => {
                let Some(msg) = msg else { return; };
                handle_wire_message(
                    &transport,
                    &state_tx,
                    &mut ctx,
                    msg,
                    &now_fn,
                    &result_tx,
                    &inviter_result_tx,
                )
                .await;
            }
            // Periodic DISCOVER re-broadcast for late-joining peers (P1 fix).
            // Gate: only fire while we're in a session AND haven't selected a
            // peer yet. Once selected_peer_session_id is set, continued
            // re-broadcast would mislead other LAN devices into thinking we're
            // still available. Best-effort publish — transport errors here are
            // non-fatal because the periodic retry handles transient drops.
            _ = rebroadcast.tick(), if ctx.as_ref().is_some_and(|c| c.selected_peer_session_id.is_none()) => {
                if let Some(c) = ctx.as_ref() {
                    let _ = transport.publish(build_discover(c)).await;
                }
            }
        }
    }
}

/// Reconstruct the DISCOVER wire message from the current SessionCtx.
/// Used both by start_inviter / start_joiner for the initial publish and
/// by the periodic rebroadcast in run_state_machine.
fn build_discover(ctx: &SessionCtx) -> PairingWireMessage {
    let owner_id_if_inviter = match ctx.role {
        PairingRole::Inviter => ctx.owner_state.as_ref().map(|s| hex::encode(s.owner_id)),
        PairingRole::Joiner => None,
    };
    let joiner_ed25519_verify_hex = match ctx.role {
        PairingRole::Joiner => ctx
            .our_signing_key
            .as_ref()
            .map(|sk| hex::encode(sk.verifying_key().to_bytes())),
        PairingRole::Inviter => None,
    };
    PairingWireMessage::Discover {
        session_id: ctx.session_id,
        role: ctx.role,
        ephemeral_pubkey_hex: hex::encode(ctx.eph_pk.as_bytes()),
        display_name: ctx.display_name.clone(),
        owner_id_if_inviter,
        joiner_ed25519_verify_hex,
    }
}

struct SessionCtx {
    role: PairingRole,
    session_id: Uuid,
    /// Our local display name. Stored on the ctx so the periodic DISCOVER
    /// rebroadcast in `run_state_machine` (and `build_discover`) can
    /// re-emit it without the caller re-supplying.
    display_name: String,
    eph_sk: X25519Sec,
    eph_pk: X25519Pub,

    // Joiner-only:
    our_signing_key: Option<SigningKey>,
    our_pubkey: Option<PubKeyBundle>,

    // Inviter-only:
    owner_state: Option<OwnerState>,
    master_seed: Option<Zeroizing<[u8; 32]>>,

    // After Discovery:
    discovered_peers: Vec<DiscoveredPeer>,

    // After Select (mutual):
    selected_peer_session_id: Option<Uuid>,
    selected_peer_pubkey: Option<X25519Pub>,
    selected_peer_display_name: Option<String>,
    /// Inviter-side: the Joiner's ed25519 verifying key (decoded from
    /// `DiscoveredPeer::joiner_ed25519_verify_hex`). Used to sign the
    /// EnrollmentCert. None on the Joiner's own session ctx.
    selected_peer_ed25519_verify: Option<[u8; 32]>,
    /// Joiner-side: the owner_id (16 bytes, decoded from
    /// `DiscoveredPeer::owner_id_if_inviter`) the Inviter advertised at
    /// DISCOVER time. The Joiner cross-checks the ENROLL payload's
    /// `owner_state.owner_id` against this on receipt — defense in depth
    /// alongside the SAS, ensuring the identity we agreed to enroll under
    /// matches the one advertised at discovery. None on the Inviter's
    /// own session ctx, and None when SELECTing a peer with no
    /// `owner_id_if_inviter` (which the SM rejects elsewhere).
    selected_peer_owner_id: Option<[u8; 16]>,
    sent_select: bool,
    received_select: bool,
    /// All peers that have published a SELECT addressed to our session_id.
    /// Tracked per-peer so a multi-peer LAN race can't false-trigger
    /// `received_select`: if peer B selects us while we have chosen peer A,
    /// the SELECT from B lands here but does not flip the boolean. Once
    /// the local user runs `SelectPeer` against a peer present in this list,
    /// `received_select` becomes true and `maybe_advance_to_handshake`
    /// uses the correct mutually-selected pubkey.
    received_selects_from: Vec<Uuid>,

    // After Handshake:
    session_key: Option<[u8; 32]>,
    sas_digits: Option<String>,

    // After Confirm:
    our_confirmed: bool,
    peer_confirmed: bool,

    // Inviter idempotency: true once ENROLL has been signed and published.
    cert_sent: bool,
}

impl SessionCtx {
    fn new(role: PairingRole, display_name: String) -> Self {
        let eph_sk = X25519Sec::random_from_rng(rand::rngs::OsRng);
        let eph_pk = X25519Pub::from(&eph_sk);
        Self {
            role,
            session_id: Uuid::new_v4(),
            display_name,
            eph_sk,
            eph_pk,
            our_signing_key: None,
            our_pubkey: None,
            owner_state: None,
            master_seed: None,
            discovered_peers: Vec::new(),
            selected_peer_session_id: None,
            selected_peer_pubkey: None,
            selected_peer_display_name: None,
            selected_peer_ed25519_verify: None,
            selected_peer_owner_id: None,
            sent_select: false,
            received_select: false,
            received_selects_from: Vec::new(),
            session_key: None,
            sas_digits: None,
            our_confirmed: false,
            peer_confirmed: false,
            cert_sent: false,
        }
    }
}

async fn start_inviter(
    transport: &Arc<dyn PairingTransport>,
    state_tx: &watch::Sender<PairingState>,
    display_name: String,
    owner_state: OwnerState,
    master_seed: Zeroizing<[u8; 32]>,
    _now_fn: &Arc<dyn Fn() -> u64 + Send + Sync>,
) -> Option<SessionCtx> {
    let mut ctx = SessionCtx::new(PairingRole::Inviter, display_name);
    ctx.owner_state = Some(owner_state);
    ctx.master_seed = Some(master_seed);

    let _ = state_tx.send(PairingState::Discovering {
        role: PairingRole::Inviter,
        ephemeral_pubkey_hex: hex::encode(ctx.eph_pk.as_bytes()),
        session_id: ctx.session_id,
    });

    // PR #63 review: if DISCOVER publish fails, peers can't see us — there
    // is no recovery from inside the SM (no retry, no alternate transport),
    // so emit Failed and return None. The caller leaves ctx as None, and
    // the user must Cancel to clear and retry.
    if let Err(e) = transport.publish(build_discover(&ctx)).await {
        let _ = state_tx.send(PairingState::Failed {
            reason: format!("publish discover: {e}"),
        });
        return None;
    }

    Some(ctx)
}

async fn start_joiner(
    transport: &Arc<dyn PairingTransport>,
    state_tx: &watch::Sender<PairingState>,
    display_name: String,
    signing_key: SigningKey,
    _now_fn: &Arc<dyn Fn() -> u64 + Send + Sync>,
) -> Option<SessionCtx> {
    let mut ctx = SessionCtx::new(PairingRole::Joiner, display_name);
    let verify_bytes = signing_key.verifying_key().to_bytes();
    let pubkey = PubKeyBundle::classical_only(verify_bytes);
    ctx.our_signing_key = Some(signing_key);
    ctx.our_pubkey = Some(pubkey);

    let _ = state_tx.send(PairingState::Discovering {
        role: PairingRole::Joiner,
        ephemeral_pubkey_hex: hex::encode(ctx.eph_pk.as_bytes()),
        session_id: ctx.session_id,
    });

    // See start_inviter for rationale on returning None on publish failure.
    if let Err(e) = transport.publish(build_discover(&ctx)).await {
        let _ = state_tx.send(PairingState::Failed {
            reason: format!("publish discover: {e}"),
        });
        return None;
    }

    Some(ctx)
}

async fn on_select_peer(
    transport: &Arc<dyn PairingTransport>,
    state_tx: &watch::Sender<PairingState>,
    ctx: &mut SessionCtx,
    peer_session_id: Uuid,
) {
    // Find the peer in the discovered list.
    let Some(peer) = ctx
        .discovered_peers
        .iter()
        .find(|p| p.session_id == peer_session_id)
        .cloned()
    else {
        let _ = state_tx.send(PairingState::Failed {
            reason: format!("unknown peer session_id: {peer_session_id}"),
        });
        return;
    };
    ctx.selected_peer_session_id = Some(peer_session_id);
    ctx.selected_peer_display_name = Some(peer.display_name.clone());
    let pk_bytes = hex::decode(&peer.ephemeral_pubkey_hex).unwrap_or_default();
    if pk_bytes.len() != 32 {
        let _ = state_tx.send(PairingState::Failed {
            reason: format!("peer pubkey has wrong length: {}", pk_bytes.len()),
        });
        return;
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&pk_bytes);
    ctx.selected_peer_pubkey = Some(X25519Pub::from(arr));

    // If our peer is an Inviter, capture their advertised owner_id so the
    // Joiner can cross-check it against the owner_id in the ENROLL payload
    // (defense in depth alongside the SAS — we want the identity we
    // enrolled under to match what was advertised at discovery time).
    if matches!(peer.role, PairingRole::Inviter) {
        match peer.owner_id_if_inviter.as_deref() {
            Some(hex_str) => match hex::decode(hex_str) {
                Ok(bytes) if bytes.len() == 16 => {
                    let mut oid = [0u8; 16];
                    oid.copy_from_slice(&bytes);
                    ctx.selected_peer_owner_id = Some(oid);
                }
                Ok(other) => {
                    let _ = state_tx.send(PairingState::Failed {
                        reason: format!(
                            "inviter owner_id wrong length: {} (expected 16)",
                            other.len()
                        ),
                    });
                    return;
                }
                Err(e) => {
                    let _ = state_tx.send(PairingState::Failed {
                        reason: format!("inviter owner_id hex decode: {e}"),
                    });
                    return;
                }
            },
            None => {
                let _ = state_tx.send(PairingState::Failed {
                    reason: "inviter peer missing owner_id_if_inviter".to_string(),
                });
                return;
            }
        }
    }

    // If our peer is a Joiner, capture their ed25519 verifying key so the
    // Inviter can sign the EnrollmentCert against it. (X25519 ephemeral above
    // is for SAS / session_key only.) The Joiner's own session has None here.
    if matches!(peer.role, PairingRole::Joiner) {
        match peer.joiner_ed25519_verify_hex.as_deref() {
            Some(hex_str) => match hex::decode(hex_str) {
                Ok(bytes) if bytes.len() == 32 => {
                    let mut vk = [0u8; 32];
                    vk.copy_from_slice(&bytes);
                    ctx.selected_peer_ed25519_verify = Some(vk);
                }
                Ok(other) => {
                    let _ = state_tx.send(PairingState::Failed {
                        reason: format!("joiner ed25519 verify key wrong length: {}", other.len()),
                    });
                    return;
                }
                Err(e) => {
                    let _ = state_tx.send(PairingState::Failed {
                        reason: format!("joiner ed25519 verify key hex decode: {e}"),
                    });
                    return;
                }
            },
            None => {
                let _ = state_tx.send(PairingState::Failed {
                    reason: "joiner peer missing ed25519 verifying key".to_string(),
                });
                return;
            }
        }
    }

    // Publish SELECT. PR #63 review: set `sent_select = true` ONLY after a
    // successful publish. Setting the flag first meant a transport failure
    // would leave us locally believing we'd told the peer; arrival of the
    // peer's matching SELECT would then advance us into Handshaking with a
    // SAS the peer never derived.
    if let Err(e) = transport
        .publish(PairingWireMessage::Select {
            my_session_id: ctx.session_id,
            peer_session_id,
        })
        .await
    {
        let _ = state_tx.send(PairingState::Failed {
            reason: format!("publish select: {e}"),
        });
        return;
    }
    ctx.sent_select = true;

    // If we previously saw the SELECT-of-us from THIS specific peer (the one
    // we just chose), set the mutual-selection flag. Per-peer matching is
    // load-bearing: a SELECT from a different LAN peer (e.g. B) that already
    // landed in `received_selects_from` does NOT count toward A — otherwise we
    // would derive SAS using A's pubkey while only B has actually selected us.
    if ctx.received_selects_from.contains(&peer_session_id) {
        ctx.received_select = true;
    }
    maybe_advance_to_handshake(state_tx, ctx);
}

fn maybe_advance_to_handshake(state_tx: &watch::Sender<PairingState>, ctx: &mut SessionCtx) {
    if ctx.sent_select && ctx.received_select && ctx.session_key.is_none() {
        let peer_pk = ctx
            .selected_peer_pubkey
            .as_ref()
            .expect("peer pubkey set on select");
        // `derive_sas` Errs on a low-order peer pubkey (non-contributory ECDH).
        // Surface as Failed — never derive a key from a publicly-known shared
        // secret.
        let derivation = match derive_sas(&ctx.eph_sk, peer_pk) {
            Ok(d) => d,
            Err(e) => {
                let _ = state_tx.send(PairingState::Failed {
                    reason: format!("derive sas: {e}"),
                });
                return;
            }
        };
        ctx.session_key = Some(derivation.session_key);
        ctx.sas_digits = Some(derivation.sas_digits.clone());
        let _ = state_tx.send(PairingState::Handshaking {
            peer_session_id: ctx.selected_peer_session_id.expect("set on select"),
            sas_digits: derivation.sas_digits,
        });
    }
}

async fn on_confirm_sas(
    transport: &Arc<dyn PairingTransport>,
    state_tx: &watch::Sender<PairingState>,
    ctx: &mut SessionCtx,
    now_fn: &Arc<dyn Fn() -> u64 + Send + Sync>,
    result_tx: &mpsc::Sender<JoinerEnrollResult>,
    inviter_result_tx: &mpsc::Sender<InviterEnrollResult>,
) {
    let Some(session_key) = ctx.session_key else {
        return;
    };
    let Some(sas_digits) = ctx.sas_digits.clone() else {
        return;
    };
    let Some(peer_session_id) = ctx.selected_peer_session_id else {
        return;
    };

    // Encrypt + publish CONFIRM. PR #63 review: set `our_confirmed = true`
    // ONLY after the publish succeeds. The previous flag-then-publish
    // ordering meant a transport failure would leave us locally believing
    // the peer had been told (so maybe_advance_to_enroll could fire on
    // peer-CONFIRM arrival) while the peer was still waiting for our SAS
    // confirmation.
    let payload = EncryptedPayload::Confirm {
        sas_digits: sas_digits.clone(),
    };
    let mut pt = Vec::new();
    if let Err(e) = ciborium::into_writer(&payload, &mut pt) {
        let _ = state_tx.send(PairingState::Failed {
            reason: format!("encode payload: {e}"),
        });
        return;
    }
    let (nonce, ct) = match session_encrypt(&session_key, &pt) {
        Ok(p) => p,
        Err(e) => {
            let _ = state_tx.send(PairingState::Failed {
                reason: format!("encrypt confirm: {e}"),
            });
            return;
        }
    };
    if let Err(e) = transport
        .publish(PairingWireMessage::Encrypted {
            my_session_id: ctx.session_id,
            peer_session_id,
            nonce_hex: hex::encode(nonce),
            ciphertext_hex: hex::encode(ct),
        })
        .await
    {
        let _ = state_tx.send(PairingState::Failed {
            reason: format!("publish confirm: {e}"),
        });
        return;
    }
    ctx.our_confirmed = true;

    // PR #63 review: only emit WaitingPeerConfirm if we are actually
    // waiting on the peer. When the peer's CONFIRM landed FIRST,
    // `peer_confirmed` is already true and `maybe_advance_to_enroll` will
    // immediately transition to Enrolling/Complete; emitting
    // WaitingPeerConfirm here would have made the UI flash through a
    // misleading intermediate state.
    if !ctx.peer_confirmed {
        let _ = state_tx.send(PairingState::WaitingPeerConfirm { peer_session_id });
    }

    maybe_advance_to_enroll(transport, state_tx, ctx, now_fn, inviter_result_tx).await;
    // Joiner waits for the ENROLL message; the receive path emits Enrolling and
    // then completes. result_tx is only consumed on the Joiner side via
    // on_encrypted_payload::Enroll.
    let _ = result_tx;
}

/// Inviter-only: once both sides have confirmed the SAS, sign the
/// EnrollmentCert + ship ENROLL. The Joiner's path through both-confirmed
/// emits `Enrolling` from the receiving handler and then transitions to
/// `Complete` when ENROLL arrives.
async fn maybe_advance_to_enroll(
    transport: &Arc<dyn PairingTransport>,
    state_tx: &watch::Sender<PairingState>,
    ctx: &mut SessionCtx,
    now_fn: &Arc<dyn Fn() -> u64 + Send + Sync>,
    inviter_result_tx: &mpsc::Sender<InviterEnrollResult>,
) {
    if !(ctx.our_confirmed && ctx.peer_confirmed) {
        return;
    }
    if !matches!(ctx.role, PairingRole::Inviter) {
        // Joiner waits for the ENROLL wire message; surface Enrolling so the
        // UI reflects progress.
        let _ = state_tx.send(PairingState::Enrolling);
        return;
    }
    // Idempotency: if we've already shipped the cert, do not re-enter.
    // (Once the Inviter has signed + sent ENROLL, owner_state has the new
    // enrollment installed; arriving late peer-CONFIRM should be a no-op.)
    if ctx.cert_sent {
        return;
    }
    let _ = state_tx.send(PairingState::Enrolling);

    // PR #63 review: build a PROSPECTIVE state on a clone, publish first,
    // then commit on success. Mutating ctx.owner_state before publish meant
    // a transport failure left the Inviter persistently bound to a Joiner
    // device that never received the cert (a "phantom device" on disk).
    let mut prospective_state = ctx.owner_state.clone().expect("inviter has owner_state");
    let master_seed = ctx.master_seed.as_ref().expect("inviter has master_seed");

    // Caveat 2: real ed25519 verify key from the Joiner's DISCOVER message.
    // Fail-fast if missing — the Inviter cannot sign without it.
    let Some(joiner_ed25519_verify) = ctx.selected_peer_ed25519_verify else {
        let _ = state_tx.send(PairingState::Failed {
            reason: "missing joiner ed25519 verifying key — cannot sign cert".to_string(),
        });
        return;
    };
    let joiner_pubkey = PubKeyBundle::classical_only(joiner_ed25519_verify);

    let now = (now_fn)();
    let cert = match sign_enrollment_for_joiner(
        master_seed,
        &prospective_state,
        joiner_pubkey.clone(),
        now,
    ) {
        Ok(c) => c,
        Err(e) => {
            let _ = state_tx.send(PairingState::Failed {
                reason: format!("sign cert: {e}"),
            });
            return;
        }
    };

    if let Err(e) = prospective_state.add_enrollment(cert.clone(), now, DEFAULT_ACTIVE_WINDOW_SECS)
    {
        let _ = state_tx.send(PairingState::Failed {
            reason: format!("add enrollment: {e}"),
        });
        return;
    }

    let mut cert_cbor = Vec::new();
    if let Err(e) = ciborium::into_writer(&cert, &mut cert_cbor) {
        let _ = state_tx.send(PairingState::Failed {
            reason: format!("encode cert: {e}"),
        });
        return;
    }
    let mut state_cbor = Vec::new();
    if let Err(e) = ciborium::into_writer(&prospective_state, &mut state_cbor) {
        let _ = state_tx.send(PairingState::Failed {
            reason: format!("encode owner_state: {e}"),
        });
        return;
    }
    let payload = EncryptedPayload::Enroll {
        enrollment_cert_cbor_hex: hex::encode(&cert_cbor),
        owner_state_cbor_hex: hex::encode(&state_cbor),
        joiner_advisory_display_name: ctx.selected_peer_display_name.clone().unwrap_or_default(),
    };
    let mut pt = Vec::new();
    if let Err(e) = ciborium::into_writer(&payload, &mut pt) {
        let _ = state_tx.send(PairingState::Failed {
            reason: format!("encode payload: {e}"),
        });
        return;
    }
    let session_key = ctx.session_key.expect("session key after handshake");
    let (nonce, ct) = match session_encrypt(&session_key, &pt) {
        Ok(p) => p,
        Err(e) => {
            let _ = state_tx.send(PairingState::Failed {
                reason: format!("encrypt enroll: {e}"),
            });
            return;
        }
    };
    if let Err(e) = transport
        .publish(PairingWireMessage::Encrypted {
            my_session_id: ctx.session_id,
            peer_session_id: ctx.selected_peer_session_id.expect("set"),
            nonce_hex: hex::encode(nonce),
            ciphertext_hex: hex::encode(ct),
        })
        .await
    {
        // Publish failed → DO NOT commit. The Joiner never received the
        // cert; persisting the new enrollment locally would leave the
        // Inviter bound to a phantom device.
        let _ = state_tx.send(PairingState::Failed {
            reason: format!("publish enroll: {e}"),
        });
        return;
    }

    // Publish succeeded → commit prospective state into ctx.
    ctx.owner_state = Some(prospective_state.clone());
    ctx.cert_sent = true;

    // Emit the InviterEnrollResult so the persistence layer (drained in
    // start_node) can merge the new enrollment into the freshest on-disk
    // OwnerState. Without this, the new enrollment lives only in RAM and
    // the Inviter's DevicesPanel reverts to showing only itself on next
    // start_node.
    //
    // PR #63 review: a closed channel here means the drainer task is gone
    // (start_node teardown, identity_dir resolution failure, etc). If we
    // ignored the error and proceeded to Complete, the user would be told
    // pairing succeeded but the on-disk state would never update — the
    // new enrollment would silently disappear on next launch. Surface as
    // Failed instead.
    //
    // ZEB-199: send the cert + signing-time `now`, NOT the mutated
    // owner_state. The persist layer applies the cert to the freshest
    // disk state under OWNER_STATE_WRITE_LOCK so a concurrent writer
    // (mint, future parallel pair flows) cannot silently lose this
    // enrollment by overwriting with a stale snapshot.
    if let Err(e) = inviter_result_tx
        .send(InviterEnrollResult {
            cert,
            now,
            master_seed: master_seed.clone(),
        })
        .await
    {
        let _ = state_tx.send(PairingState::Failed {
            reason: format!("inviter persistence channel closed: {e}"),
        });
        return;
    }

    let device_id = joiner_pubkey.identity_hash();
    let _ = state_tx.send(PairingState::Complete {
        device_id_hex: hex::encode(device_id),
    });
}

async fn handle_wire_message(
    transport: &Arc<dyn PairingTransport>,
    state_tx: &watch::Sender<PairingState>,
    ctx: &mut Option<SessionCtx>,
    msg: PairingWireMessage,
    now_fn: &Arc<dyn Fn() -> u64 + Send + Sync>,
    result_tx: &mpsc::Sender<JoinerEnrollResult>,
    inviter_result_tx: &mpsc::Sender<InviterEnrollResult>,
) {
    // Safety: callers only invoke this when ctx.is_some() (enforced by the
    // select! guard), EXCEPT the Cancel arm which explicitly sets ctx to None.
    // We borrow the inner ctx for all non-Cancel arms.
    match msg {
        PairingWireMessage::Discover {
            session_id,
            role,
            ephemeral_pubkey_hex,
            display_name,
            owner_id_if_inviter,
            joiner_ed25519_verify_hex,
        } => {
            let Some(c) = ctx.as_mut() else {
                return;
            };
            // Ignore our own discoveries (echo).
            if session_id == c.session_id {
                return;
            }
            // Only collect peers of the OPPOSITE role.
            if role == c.role {
                return;
            }
            // De-dup by session_id.
            if c.discovered_peers
                .iter()
                .any(|p| p.session_id == session_id)
            {
                return;
            }
            let now = (now_fn)();
            c.discovered_peers.push(DiscoveredPeer {
                session_id,
                role,
                display_name,
                owner_id_if_inviter,
                ephemeral_pubkey_hex,
                joiner_ed25519_verify_hex,
                seen_at_unix: now,
            });
            let _ = state_tx.send(PairingState::Discovered {
                peers: c.discovered_peers.clone(),
            });
        }
        PairingWireMessage::Select {
            my_session_id,
            peer_session_id,
        } => {
            let Some(c) = ctx.as_mut() else {
                return;
            };
            // Only act if the peer is selecting US.
            if peer_session_id != c.session_id {
                return;
            }
            // Only act if we have already discovered this peer.
            if !c
                .discovered_peers
                .iter()
                .any(|p| p.session_id == my_session_id)
            {
                return;
            }
            // Record the SELECT per-peer. The mutual-selection flag
            // (`received_select`) is only flipped if this peer is the one we
            // chose ourselves; without that gate, a multi-peer LAN race —
            // peer B selects us while we selected peer A — would prematurely
            // flip the flag and `maybe_advance_to_handshake` would derive the
            // SAS using A's pubkey while only B has actually selected us.
            //
            // De-dup defensively in case a peer re-emits SELECT (network
            // retries / re-runs of their wizard).
            if !c.received_selects_from.contains(&my_session_id) {
                c.received_selects_from.push(my_session_id);
            }
            if c.selected_peer_session_id == Some(my_session_id) {
                c.received_select = true;
            }
            maybe_advance_to_handshake(state_tx, c);
        }
        PairingWireMessage::Encrypted {
            my_session_id,
            peer_session_id,
            nonce_hex,
            ciphertext_hex,
        } => {
            let Some(c) = ctx.as_mut() else {
                return;
            };
            if peer_session_id != c.session_id {
                return;
            }
            if Some(my_session_id) != c.selected_peer_session_id {
                return;
            }
            let Some(session_key) = c.session_key else {
                return;
            };
            let nonce = match hex::decode(&nonce_hex) {
                Ok(n) => n,
                Err(e) => {
                    let _ = state_tx.send(PairingState::Failed {
                        reason: format!("nonce hex: {e}"),
                    });
                    return;
                }
            };
            let ct = match hex::decode(&ciphertext_hex) {
                Ok(ct) => ct,
                Err(e) => {
                    let _ = state_tx.send(PairingState::Failed {
                        reason: format!("ct hex: {e}"),
                    });
                    return;
                }
            };
            let pt = match session_decrypt(&session_key, &nonce, &ct) {
                Ok(p) => p,
                Err(e) => {
                    let _ = state_tx.send(PairingState::Failed {
                        reason: format!("decrypt: {e}"),
                    });
                    return;
                }
            };
            let payload: EncryptedPayload = match ciborium::from_reader(pt.as_slice()) {
                Ok(p) => p,
                Err(e) => {
                    let _ = state_tx.send(PairingState::Failed {
                        reason: format!("payload decode: {e}"),
                    });
                    return;
                }
            };
            on_encrypted_payload(
                transport,
                state_tx,
                c,
                payload,
                now_fn,
                result_tx,
                inviter_result_tx,
            )
            .await;
        }
        PairingWireMessage::Cancel { my_session_id, .. } => {
            // PR #63 review: only the SELECTED peer can tear down the session.
            // Treating any discovered peer's Cancel as a session kill let an
            // unselected LAN device grief our active handshake just by having
            // been visible in our discovered list. A non-selected peer
            // cancelling means "I'm leaving the discovery pool" — we drop
            // them from our list and re-emit Discovered, but stay in-session.
            let Some(c) = ctx.as_mut() else {
                return;
            };
            if c.selected_peer_session_id == Some(my_session_id) {
                // Selected peer is leaving: drop ctx (mirrors local-Cancel).
                *ctx = None;
                let _ = state_tx.send(PairingState::Idle);
                return;
            }
            // Non-selected peer leaving: prune from discovery list.
            let before = c.discovered_peers.len();
            c.discovered_peers.retain(|p| p.session_id != my_session_id);
            if c.discovered_peers.len() < before {
                // Also forget any pending received-SELECT from that peer.
                c.received_selects_from.retain(|s| *s != my_session_id);
                let _ = state_tx.send(PairingState::Discovered {
                    peers: c.discovered_peers.clone(),
                });
            }
        }
    }
}

async fn on_encrypted_payload(
    transport: &Arc<dyn PairingTransport>,
    state_tx: &watch::Sender<PairingState>,
    ctx: &mut SessionCtx,
    payload: EncryptedPayload,
    now_fn: &Arc<dyn Fn() -> u64 + Send + Sync>,
    result_tx: &mpsc::Sender<JoinerEnrollResult>,
    inviter_result_tx: &mpsc::Sender<InviterEnrollResult>,
) {
    match payload {
        EncryptedPayload::Confirm { sas_digits } => {
            // Defense-in-depth: the SAS in the message must match what we
            // computed locally. (Session_key already authenticates this, but
            // the explicit equality check makes the intent obvious.)
            if Some(&sas_digits) != ctx.sas_digits.as_ref() {
                let _ = state_tx.send(PairingState::Failed {
                    reason: "SAS mismatch in CONFIRM".to_string(),
                });
                return;
            }
            ctx.peer_confirmed = true;
            // Caveat 1: when we receive peer-CONFIRM AFTER we have already
            // locally confirmed, we must drive the post-confirm transition
            // ourselves. Previously the scaffold tried to do this without
            // `transport` in scope; we now thread it through.
            if ctx.our_confirmed {
                maybe_advance_to_enroll(transport, state_tx, ctx, now_fn, inviter_result_tx).await;
            }
        }
        EncryptedPayload::Enroll {
            enrollment_cert_cbor_hex,
            owner_state_cbor_hex,
            ..
        } => {
            // Joiner-side: install the cert and state.
            if !matches!(ctx.role, PairingRole::Joiner) {
                // Inviter doesn't accept ENROLL.
                return;
            }
            // PR #63 review: gate ENROLL on mutual SAS confirmation. The
            // session_key already authenticates that the sender holds the
            // shared key, but that is NOT a substitute for the user-driven
            // SAS step — without this gate, a selected peer could complete
            // pairing without the user ever seeing or confirming the SAS,
            // collapsing protocol verification into a pure transport check.
            if !(ctx.our_confirmed && ctx.peer_confirmed) {
                let _ = state_tx.send(PairingState::Failed {
                    reason: "received ENROLL before mutual SAS confirm".to_string(),
                });
                return;
            }
            // Idempotency: a duplicate ENROLL (network re-delivery, replay)
            // arriving after we've already installed must be a no-op. We
            // already took our_signing_key on the first install, so its
            // absence is the natural sentinel — without this guard, the
            // earlier `take().expect()` would panic the SM task on a second
            // ENROLL even though all prior guards (role, mutual confirm)
            // still pass.
            if ctx.our_signing_key.is_none() {
                return;
            }
            let cert_bytes = match hex::decode(&enrollment_cert_cbor_hex) {
                Ok(b) => b,
                Err(e) => {
                    let _ = state_tx.send(PairingState::Failed {
                        reason: format!("cert hex: {e}"),
                    });
                    return;
                }
            };
            let state_bytes = match hex::decode(&owner_state_cbor_hex) {
                Ok(b) => b,
                Err(e) => {
                    let _ = state_tx.send(PairingState::Failed {
                        reason: format!("state hex: {e}"),
                    });
                    return;
                }
            };
            let cert: harmony_owner::certs::EnrollmentCert =
                match ciborium::from_reader(cert_bytes.as_slice()) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = state_tx.send(PairingState::Failed {
                            reason: format!("cert decode: {e}"),
                        });
                        return;
                    }
                };
            let owner_state: OwnerState = match ciborium::from_reader(state_bytes.as_slice()) {
                Ok(s) => s,
                Err(e) => {
                    let _ = state_tx.send(PairingState::Failed {
                        reason: format!("state decode: {e}"),
                    });
                    return;
                }
            };

            // Cross-check: the owner_id in the ENROLL payload MUST match the
            // owner_id_if_inviter the Inviter advertised at DISCOVER time.
            // PR #63 review (Greptile): the SAS catches MitM attacks at the
            // session level, but binding ENROLL's identity to the discovery-
            // time advertisement closes the residual gap where a selected
            // Inviter could (after passing SAS) try to enroll us under a
            // DIFFERENT owner — defense in depth. Failure here is a hard
            // protocol violation, not a network glitch.
            if let Some(advertised_owner_id) = ctx.selected_peer_owner_id {
                if owner_state.owner_id != advertised_owner_id {
                    let _ = state_tx.send(PairingState::Failed {
                        reason: format!(
                            "ENROLL owner_id mismatch: advertised {} at DISCOVER, got {} in ENROLL",
                            hex::encode(advertised_owner_id),
                            hex::encode(owner_state.owner_id),
                        ),
                    });
                    return;
                }
            }

            let our_pubkey = ctx.our_pubkey.as_ref().expect("joiner has pubkey");
            let now = (now_fn)();
            if let Err(e) = verify_cert_for_self(
                &cert,
                owner_state.owner_id,
                our_pubkey,
                now,
                DEFAULT_ACTIVE_WINDOW_SECS,
            ) {
                let _ = state_tx.send(PairingState::Failed {
                    reason: format!("verify cert: {e}"),
                });
                return;
            }

            let our_sk = ctx.our_signing_key.take().expect("joiner has signing key");
            let our_device_id = our_pubkey.identity_hash();
            // See the Inviter side for rationale on send() error → Failed:
            // a closed channel here means the persistence drainer is gone,
            // and reporting Complete would tell the user pairing worked
            // when the on-disk state will silently revert on next launch.
            if let Err(e) = result_tx
                .send(JoinerEnrollResult {
                    our_signing_key: our_sk,
                    owner_state,
                    our_device_id,
                })
                .await
            {
                let _ = state_tx.send(PairingState::Failed {
                    reason: format!("joiner persistence channel closed: {e}"),
                });
                return;
            }
            let _ = state_tx.send(PairingState::Complete {
                device_id_hex: hex::encode(our_device_id),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::transport::InMemoryBroker;
    use async_trait::async_trait;
    use ed25519_dalek::SigningKey;
    use harmony_owner::lifecycle::{mint_owner, MintResult};
    use rand::rngs::OsRng;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use tokio::sync::Mutex as AsyncMutex;
    use tokio::time::timeout;
    use zeroize::Zeroizing;

    fn fixed_clock(t: u64) -> Arc<dyn Fn() -> u64 + Send + Sync> {
        Arc::new(move || t)
    }

    /// Test transport that lets the test inject inbound wire messages
    /// (via `publish_tx` outside) and capture every outbound publish (via
    /// `published`). Used in tests where the SM needs to run against a
    /// scripted peer rather than another live SM.
    struct ScriptedTransport {
        publish_tx: mpsc::Sender<PairingWireMessage>,
        recv_rx: AsyncMutex<mpsc::Receiver<PairingWireMessage>>,
        published: StdMutex<Vec<PairingWireMessage>>,
    }
    #[async_trait]
    impl PairingTransport for ScriptedTransport {
        async fn publish(&self, message: PairingWireMessage) -> Result<(), String> {
            self.published.lock().unwrap().push(message.clone());
            let _ = self.publish_tx.send(message).await;
            Ok(())
        }
        async fn recv(&self) -> Option<PairingWireMessage> {
            self.recv_rx.lock().await.recv().await
        }
    }

    #[tokio::test]
    async fn happy_path_two_devices_pair() {
        // Setup: mint owner on Inviter side; generate a Joiner signing key.
        let MintResult {
            state,
            recovery_artifact,
            ..
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());

        let joiner_sk = SigningKey::generate(&mut OsRng);

        // Two transports linked back-to-back.
        let (inviter_t, joiner_t) = InMemoryBroker::pair();
        // Long rebroadcast interval so periodic re-emit doesn't fire during
        // these short-lived tests (tests complete in ~1-3s; pickng 60s ensures
        // we never see more than the initial DISCOVER).
        let test_interval = Duration::from_secs(60);
        let inviter_handle = spawn_state_machine(
            Arc::new(inviter_t),
            fixed_clock(1_700_000_001),
            test_interval,
        );
        let joiner_handle = spawn_state_machine(
            Arc::new(joiner_t),
            fixed_clock(1_700_000_002),
            test_interval,
        );

        // Both start.
        inviter_handle
            .cmd_tx
            .send(PairingCommand::StartInviter {
                display_name: "KRILE".to_string(),
                owner_state: state.clone(),
                master_seed,
            })
            .await
            .unwrap();
        joiner_handle
            .cmd_tx
            .send(PairingCommand::StartJoiner {
                display_name: "AVALON".to_string(),
                signing_key: joiner_sk,
            })
            .await
            .unwrap();

        // Wait for both to discover each other. Use `wait_for` which checks
        // the current value first (handles the case where the SM has already
        // transitioned past Idle by the time we start waiting).
        let mut inviter_state = inviter_handle.state_rx.clone();
        let mut joiner_state = joiner_handle.state_rx.clone();

        timeout(Duration::from_secs(2), async {
            inviter_state
                .wait_for(|s| matches!(s, PairingState::Discovered { .. }))
                .await
                .unwrap();
        })
        .await
        .expect("inviter sees joiner within 2s");

        timeout(Duration::from_secs(2), async {
            joiner_state
                .wait_for(|s| matches!(s, PairingState::Discovered { .. }))
                .await
                .unwrap();
        })
        .await
        .expect("joiner sees inviter within 2s");

        // Each side selects the other.
        let inviter_peer_id = match &*inviter_handle.state_rx.borrow() {
            PairingState::Discovered { peers } => peers[0].session_id,
            _ => panic!(),
        };
        let joiner_peer_id = match &*joiner_handle.state_rx.borrow() {
            PairingState::Discovered { peers } => peers[0].session_id,
            _ => panic!(),
        };
        inviter_handle
            .cmd_tx
            .send(PairingCommand::SelectPeer {
                peer_session_id: inviter_peer_id,
            })
            .await
            .unwrap();
        joiner_handle
            .cmd_tx
            .send(PairingCommand::SelectPeer {
                peer_session_id: joiner_peer_id,
            })
            .await
            .unwrap();

        // Wait for both to reach Handshaking with same SAS.
        timeout(Duration::from_secs(2), async {
            inviter_state
                .wait_for(|s| matches!(s, PairingState::Handshaking { .. }))
                .await
                .unwrap();
        })
        .await
        .expect("inviter handshakes within 2s");
        timeout(Duration::from_secs(2), async {
            joiner_state
                .wait_for(|s| matches!(s, PairingState::Handshaking { .. }))
                .await
                .unwrap();
        })
        .await
        .expect("joiner handshakes within 2s");

        let inviter_sas = match &*inviter_handle.state_rx.borrow() {
            PairingState::Handshaking { sas_digits, .. } => sas_digits.clone(),
            _ => panic!(),
        };
        let joiner_sas = match &*joiner_handle.state_rx.borrow() {
            PairingState::Handshaking { sas_digits, .. } => sas_digits.clone(),
            _ => panic!(),
        };
        assert_eq!(inviter_sas, joiner_sas, "both sides see same SAS");

        // Both confirm.
        inviter_handle
            .cmd_tx
            .send(PairingCommand::ConfirmSas)
            .await
            .unwrap();
        joiner_handle
            .cmd_tx
            .send(PairingCommand::ConfirmSas)
            .await
            .unwrap();

        // Joiner reaches Complete.
        timeout(Duration::from_secs(3), async {
            joiner_state
                .wait_for(|s| matches!(s, PairingState::Complete { .. }))
                .await
                .unwrap();
        })
        .await
        .expect("joiner completes within 3s");

        // The joiner_result_rx should have a JoinerEnrollResult.
        let mut jrx = joiner_handle.joiner_result_rx.expect("joiner result rx");
        let result = timeout(Duration::from_secs(1), jrx.recv())
            .await
            .expect("joiner result arrives")
            .expect("result not None");

        // The Joiner's OwnerState now contains its enrollment.
        let our_id = result.our_device_id;
        assert!(result.owner_state.enrollments.contains_key(&our_id));
        // And contains the original Inviter's enrollment.
        let original_inviter_device_id = *state.enrollments.keys().next().unwrap();
        assert!(result
            .owner_state
            .enrollments
            .contains_key(&original_inviter_device_id));
    }

    /// Multi-peer LAN race regression (PR #63 review): a SELECT addressed to
    /// us from a peer we did NOT select must NOT flip `received_select` and
    /// advance to Handshaking. The bug let peer B's SELECT count toward our
    /// having-chosen-A state, producing a SAS derived from A's pubkey while
    /// only B had mutually selected us.
    ///
    /// We model this with a one-way "scripted" transport: we drive the SM
    /// from outside by injecting wire messages directly, so we can simulate
    /// two distinct peers without spawning two more state machines.
    #[tokio::test]
    async fn select_from_unchosen_peer_does_not_advance() {
        use x25519_dalek::{PublicKey as X25519Pub, StaticSecret as X25519Sec};

        let (in_tx, in_rx) = mpsc::channel::<PairingWireMessage>(16);
        // out_tx is the SM's publish sink; we don't actually consume from
        // out_rx — `published` captures the messages directly.
        let (out_tx, _out_rx) = mpsc::channel::<PairingWireMessage>(16);
        let transport = Arc::new(ScriptedTransport {
            publish_tx: out_tx,
            recv_rx: AsyncMutex::new(in_rx),
            published: StdMutex::new(Vec::new()),
        });

        let MintResult {
            state,
            recovery_artifact,
            ..
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());

        let inviter_handle = spawn_state_machine(
            transport.clone(),
            fixed_clock(1_700_000_001),
            Duration::from_secs(60),
        );

        inviter_handle
            .cmd_tx
            .send(PairingCommand::StartInviter {
                display_name: "krile".to_string(),
                owner_state: state,
                master_seed,
            })
            .await
            .unwrap();

        // Wait for our DISCOVER to be published, then read our session_id
        // out of it.
        let our_session_id = timeout(Duration::from_secs(2), async {
            loop {
                if let Some(PairingWireMessage::Discover { session_id, .. }) =
                    transport.published.lock().unwrap().first().cloned()
                {
                    return session_id;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("inviter publishes its DISCOVER within 2s");

        // Inject DISCOVER from two distinct Joiner peers.
        let peer_a_session = Uuid::new_v4();
        let peer_b_session = Uuid::new_v4();
        let peer_a_x_sk = X25519Sec::random_from_rng(rand::rngs::OsRng);
        let peer_b_x_sk = X25519Sec::random_from_rng(rand::rngs::OsRng);
        let peer_a_x_pk = X25519Pub::from(&peer_a_x_sk);
        let peer_b_x_pk = X25519Pub::from(&peer_b_x_sk);
        let peer_a_ed = SigningKey::generate(&mut OsRng);
        let peer_b_ed = SigningKey::generate(&mut OsRng);

        in_tx
            .send(PairingWireMessage::Discover {
                session_id: peer_a_session,
                role: PairingRole::Joiner,
                ephemeral_pubkey_hex: hex::encode(peer_a_x_pk.as_bytes()),
                display_name: "alpha".to_string(),
                owner_id_if_inviter: None,
                joiner_ed25519_verify_hex: Some(hex::encode(peer_a_ed.verifying_key().to_bytes())),
            })
            .await
            .unwrap();
        in_tx
            .send(PairingWireMessage::Discover {
                session_id: peer_b_session,
                role: PairingRole::Joiner,
                ephemeral_pubkey_hex: hex::encode(peer_b_x_pk.as_bytes()),
                display_name: "bravo".to_string(),
                owner_id_if_inviter: None,
                joiner_ed25519_verify_hex: Some(hex::encode(peer_b_ed.verifying_key().to_bytes())),
            })
            .await
            .unwrap();

        // Wait for both peers in our discovered list.
        let mut state_rx = inviter_handle.state_rx.clone();
        timeout(Duration::from_secs(2), async {
            state_rx
                .wait_for(|s| matches!(s, PairingState::Discovered { peers } if peers.len() == 2))
                .await
                .unwrap();
        })
        .await
        .expect("both peers visible within 2s");

        // SELECT peer A locally.
        inviter_handle
            .cmd_tx
            .send(PairingCommand::SelectPeer {
                peer_session_id: peer_a_session,
            })
            .await
            .unwrap();
        // Let the SM process the cmd.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // BUG SCENARIO: peer B sends SELECT addressed to us, but we chose A.
        in_tx
            .send(PairingWireMessage::Select {
                my_session_id: peer_b_session,
                peer_session_id: our_session_id,
            })
            .await
            .unwrap();

        // Give the SM enough time that, with the bug, it would have advanced
        // to Handshaking.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let observed = inviter_handle.state_rx.borrow().clone();
        assert!(
            !matches!(observed, PairingState::Handshaking { .. }),
            "must NOT advance to Handshaking from peer B's SELECT — we chose A; got: {observed:?}"
        );

        // Sanity: peer A's SELECT (the legitimate one) MUST advance us.
        in_tx
            .send(PairingWireMessage::Select {
                my_session_id: peer_a_session,
                peer_session_id: our_session_id,
            })
            .await
            .unwrap();
        timeout(Duration::from_secs(2), async {
            state_rx
                .wait_for(|s| matches!(s, PairingState::Handshaking { .. }))
                .await
                .unwrap();
        })
        .await
        .expect("must advance once peer A (chosen) selects us");
    }

    /// Regression test: when the SELECTED remote peer sends Cancel, the
    /// receiving state machine must clear its ctx and emit Idle so no
    /// further wire messages can advance it (e.g. re-emit Discovered from
    /// a stale peer list). The "selected" gate is load-bearing — see
    /// `unselected_peer_cancel_only_prunes_discovered` below for why an
    /// unselected peer must NOT have this power.
    #[tokio::test]
    async fn cancel_drops_ctx_on_selected_peer_cancel() {
        let MintResult {
            state,
            recovery_artifact,
            ..
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
        let joiner_sk = SigningKey::generate(&mut OsRng);

        let (inviter_t, joiner_t) = InMemoryBroker::pair();
        // Long rebroadcast interval so periodic re-emit doesn't fire during
        // these short-lived tests (tests complete in ~1-3s; pickng 60s ensures
        // we never see more than the initial DISCOVER).
        let test_interval = Duration::from_secs(60);
        let inviter_handle = spawn_state_machine(
            Arc::new(inviter_t),
            fixed_clock(1_700_000_001),
            test_interval,
        );
        let joiner_handle = spawn_state_machine(
            Arc::new(joiner_t),
            fixed_clock(1_700_000_002),
            test_interval,
        );

        inviter_handle
            .cmd_tx
            .send(PairingCommand::StartInviter {
                display_name: "KRILE".to_string(),
                owner_state: state,
                master_seed,
            })
            .await
            .unwrap();
        joiner_handle
            .cmd_tx
            .send(PairingCommand::StartJoiner {
                display_name: "AVALON".to_string(),
                signing_key: joiner_sk,
            })
            .await
            .unwrap();

        let mut inviter_state = inviter_handle.state_rx.clone();

        // Wait for Inviter to reach Discovered.
        timeout(Duration::from_secs(2), async {
            inviter_state
                .wait_for(|s| matches!(s, PairingState::Discovered { .. }))
                .await
                .unwrap();
        })
        .await
        .expect("inviter reaches Discovered");

        // Inviter SELECTs the Joiner — making the Joiner the selected peer.
        let inviter_peer_session = match &*inviter_handle.state_rx.borrow() {
            PairingState::Discovered { peers } => peers[0].session_id,
            _ => panic!("expected Discovered"),
        };
        inviter_handle
            .cmd_tx
            .send(PairingCommand::SelectPeer {
                peer_session_id: inviter_peer_session,
            })
            .await
            .unwrap();
        // Give the SM a moment to process the SELECT cmd before the Cancel
        // arrives (otherwise selected_peer_session_id may not be set yet).
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Now Joiner cancels. Joiner is the SELECTED peer → Inviter must Idle.
        joiner_handle
            .cmd_tx
            .send(PairingCommand::Cancel)
            .await
            .unwrap();

        timeout(Duration::from_secs(2), async {
            inviter_state
                .wait_for(|s| matches!(s, PairingState::Idle))
                .await
                .unwrap();
        })
        .await
        .expect("inviter reaches Idle after selected-peer cancel");

        // Verify ctx has been cleared: send a local Cancel from the Inviter.
        // Because ctx is None, the Cancel handler should skip publishing and
        // the state stays Idle (no panic, no state change).
        inviter_handle
            .cmd_tx
            .send(PairingCommand::Cancel)
            .await
            .unwrap();

        // Give the state machine a moment to process the command.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // State must still be Idle — not Discovered, not Failed.
        assert!(
            matches!(*inviter_handle.state_rx.borrow(), PairingState::Idle),
            "inviter remains Idle after no-op Cancel (ctx was already None)"
        );
    }

    /// Multi-peer LAN regression (PR #63 review): an UNSELECTED peer
    /// cancelling must NOT tear down our session. Otherwise any device
    /// that ever appeared in our discovered list could grief our active
    /// pairing just by sending Cancel. Instead, the unselected peer must
    /// only be pruned from our discovered list.
    #[tokio::test]
    async fn unselected_peer_cancel_only_prunes_discovered() {
        use x25519_dalek::{PublicKey as X25519Pub, StaticSecret as X25519Sec};

        let (in_tx, in_rx) = mpsc::channel::<PairingWireMessage>(16);
        let (out_tx, _out_rx) = mpsc::channel::<PairingWireMessage>(16);
        let transport = Arc::new(ScriptedTransport {
            publish_tx: out_tx,
            recv_rx: AsyncMutex::new(in_rx),
            published: StdMutex::new(Vec::new()),
        });

        let MintResult {
            state,
            recovery_artifact,
            ..
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
        let inviter_handle = spawn_state_machine(
            transport.clone(),
            fixed_clock(1_700_000_001),
            Duration::from_secs(60),
        );
        inviter_handle
            .cmd_tx
            .send(PairingCommand::StartInviter {
                display_name: "krile".to_string(),
                owner_state: state,
                master_seed,
            })
            .await
            .unwrap();

        // Inject DISCOVER from two distinct Joiner peers.
        let peer_a_session = Uuid::new_v4();
        let peer_b_session = Uuid::new_v4();
        let peer_a_x_pk = X25519Pub::from(&X25519Sec::random_from_rng(rand::rngs::OsRng));
        let peer_b_x_pk = X25519Pub::from(&X25519Sec::random_from_rng(rand::rngs::OsRng));
        let peer_a_ed = SigningKey::generate(&mut OsRng);
        let peer_b_ed = SigningKey::generate(&mut OsRng);
        for (sid, pk, ed, name) in [
            (peer_a_session, peer_a_x_pk, &peer_a_ed, "alpha"),
            (peer_b_session, peer_b_x_pk, &peer_b_ed, "bravo"),
        ] {
            in_tx
                .send(PairingWireMessage::Discover {
                    session_id: sid,
                    role: PairingRole::Joiner,
                    ephemeral_pubkey_hex: hex::encode(pk.as_bytes()),
                    display_name: name.to_string(),
                    owner_id_if_inviter: None,
                    joiner_ed25519_verify_hex: Some(hex::encode(ed.verifying_key().to_bytes())),
                })
                .await
                .unwrap();
        }

        let mut state_rx = inviter_handle.state_rx.clone();
        timeout(Duration::from_secs(2), async {
            state_rx
                .wait_for(|s| matches!(s, PairingState::Discovered { peers } if peers.len() == 2))
                .await
                .unwrap();
        })
        .await
        .expect("both peers discovered within 2s");

        // Select peer A.
        inviter_handle
            .cmd_tx
            .send(PairingCommand::SelectPeer {
                peer_session_id: peer_a_session,
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Peer B (unselected) sends Cancel. Must NOT tear down our session
        // with peer A; must only prune peer B from the discovered list.
        in_tx
            .send(PairingWireMessage::Cancel {
                my_session_id: peer_b_session,
                peer_session_id: None,
                reason: "unrelated session ending".to_string(),
            })
            .await
            .unwrap();

        // Wait for the Discovered re-emit to land.
        timeout(Duration::from_secs(1), async {
            state_rx
                .wait_for(|s| {
                    matches!(s, PairingState::Discovered { peers }
                                 if peers.len() == 1 && peers[0].session_id == peer_a_session)
                })
                .await
                .unwrap();
        })
        .await
        .expect("peer B pruned, peer A still in list");

        // Critical: state must NOT be Idle. Our session with peer A survived.
        assert!(
            !matches!(*inviter_handle.state_rx.borrow(), PairingState::Idle),
            "unselected peer's Cancel must not drop our ctx"
        );
    }

    /// P1 regression (Greptile): DISCOVER must be re-broadcast periodically
    /// while we're in pre-selection state. Zenoh pub/sub does NOT replay past
    /// publications, so without periodic re-emit, a Joiner that subscribes
    /// after the Inviter's initial DISCOVER would never see it. This test
    /// captures every published message and asserts that within a short
    /// window we observe MULTIPLE DISCOVERs from a still-discovering Inviter,
    /// then SELECTs a peer and asserts NO further DISCOVER fires (continued
    /// broadcast post-selection would mislead other LAN devices).
    #[tokio::test]
    async fn discover_is_rebroadcast_until_select() {
        use x25519_dalek::{PublicKey as X25519Pub, StaticSecret as X25519Sec};

        let (in_tx, in_rx) = mpsc::channel::<PairingWireMessage>(16);
        let (out_tx, _out_rx) = mpsc::channel::<PairingWireMessage>(64);
        let transport = Arc::new(ScriptedTransport {
            publish_tx: out_tx,
            recv_rx: AsyncMutex::new(in_rx),
            published: StdMutex::new(Vec::new()),
        });

        let MintResult {
            state,
            recovery_artifact,
            ..
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());

        // Short interval so the test is fast; stays >> tokio scheduler jitter.
        let rebroadcast_interval = Duration::from_millis(80);
        let inviter_handle = spawn_state_machine(
            transport.clone(),
            fixed_clock(1_700_000_001),
            rebroadcast_interval,
        );

        inviter_handle
            .cmd_tx
            .send(PairingCommand::StartInviter {
                display_name: "krile".to_string(),
                owner_state: state,
                master_seed,
            })
            .await
            .unwrap();

        // Wait ~5x the interval so we'd expect ≥4 rebroadcasts on top of the
        // initial publish. The exact number depends on scheduling, but ≥3
        // total Discover messages is a safe lower bound — the bug version
        // would publish exactly 1.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let discover_count_pre_select = transport
            .published
            .lock()
            .unwrap()
            .iter()
            .filter(|m| matches!(m, PairingWireMessage::Discover { .. }))
            .count();
        assert!(
            discover_count_pre_select >= 3,
            "expected ≥3 DISCOVER messages over ~500ms with 80ms rebroadcast interval, \
             got {discover_count_pre_select} (one-shot DISCOVER bug?)"
        );

        // Inject a peer DISCOVER so we have someone to SELECT.
        let peer_session = Uuid::new_v4();
        let peer_x_pk = X25519Pub::from(&X25519Sec::random_from_rng(rand::rngs::OsRng));
        let peer_ed = SigningKey::generate(&mut OsRng);
        in_tx
            .send(PairingWireMessage::Discover {
                session_id: peer_session,
                role: PairingRole::Joiner,
                ephemeral_pubkey_hex: hex::encode(peer_x_pk.as_bytes()),
                display_name: "avalon".to_string(),
                owner_id_if_inviter: None,
                joiner_ed25519_verify_hex: Some(hex::encode(peer_ed.verifying_key().to_bytes())),
            })
            .await
            .unwrap();

        let mut state_rx = inviter_handle.state_rx.clone();
        timeout(Duration::from_secs(2), async {
            state_rx
                .wait_for(|s| matches!(s, PairingState::Discovered { peers } if !peers.is_empty()))
                .await
                .unwrap();
        })
        .await
        .expect("peer discovered");

        inviter_handle
            .cmd_tx
            .send(PairingCommand::SelectPeer {
                peer_session_id: peer_session,
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Snapshot the count immediately after SELECT lands.
        let discover_count_at_select = transport
            .published
            .lock()
            .unwrap()
            .iter()
            .filter(|m| matches!(m, PairingWireMessage::Discover { .. }))
            .count();

        // Wait again — rebroadcast should now be SUPPRESSED (selected_peer_session_id
        // is Some, so the select! `if` guard skips the tick branch).
        tokio::time::sleep(Duration::from_millis(500)).await;
        let discover_count_after_select = transport
            .published
            .lock()
            .unwrap()
            .iter()
            .filter(|m| matches!(m, PairingWireMessage::Discover { .. }))
            .count();
        assert_eq!(
            discover_count_at_select, discover_count_after_select,
            "DISCOVER must NOT continue broadcasting after SelectPeer; \
             was {discover_count_at_select}, after another 500ms it's \
             {discover_count_after_select} — rebroadcast guard regressed?"
        );
    }

    /// ZEB-198: Cancel from every reachable phase must clean up ctx and
    /// emit Idle. Today the Cancel handler is uniform (`run_state_machine`
    /// at L166-176: clear ctx, send Idle, regardless of current state) — but
    /// that uniformity is load-bearing for safety. This regression test
    /// pins it down so any future per-phase Cancel logic is forced to
    /// preserve the no-stale-ctx invariant.
    ///
    /// The "ctx is clean" check exploits the *only* observable side effect
    /// that distinguishes Cancel-with-ctx from Cancel-without-ctx: at
    /// L166-176 the handler publishes a wire `Cancel` only when
    /// `ctx.is_some()`. We wrap the cancelled side's transport with
    /// `CountCancelTransport`, snapshot the publish count, send a follow-up
    /// no-op Cancel, and assert the count is unchanged. Asserting only
    /// that the *state* stayed Idle is too weak — the SM emits Idle on
    /// Cancel either way, even with stale ctx (per CodeRabbit/Qodo review
    /// on PR #64).
    #[tokio::test]
    async fn state_machine_cancel_at_each_stage() {
        use crate::pairing::transport::PairingTransport;
        use crate::pairing::types::PairingWireMessage;
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountCancelTransport {
            inner: Arc<dyn PairingTransport>,
            cancel_count: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl PairingTransport for CountCancelTransport {
            async fn publish(&self, message: PairingWireMessage) -> Result<(), String> {
                if matches!(message, PairingWireMessage::Cancel { .. }) {
                    self.cancel_count.fetch_add(1, Ordering::Relaxed);
                }
                self.inner.publish(message).await
            }
            async fn recv(&self) -> Option<PairingWireMessage> {
                self.inner.recv().await
            }
        }

        async fn assert_idle_and_clean(
            handle: &mut PairingHandle,
            label: &str,
            cancel_count: &AtomicUsize,
        ) {
            let mut rx = handle.state_rx.clone();
            timeout(Duration::from_secs(2), async {
                rx.wait_for(|s| matches!(s, PairingState::Idle))
                    .await
                    .unwrap();
            })
            .await
            .unwrap_or_else(|_| panic!("[{label}] SM did not reach Idle within 2s"));

            // Snapshot AFTER first Cancel has settled. The first Cancel
            // (ctx was Some) should already have bumped the count; the
            // second Cancel (ctx is None) must NOT bump it again.
            let count_before = cancel_count.load(Ordering::Relaxed);

            handle.cmd_tx.send(PairingCommand::Cancel).await.unwrap();
            // Deterministic ack: Cancel handler at L175 unconditionally calls
            // state_tx.send(Idle), even when ctx is None. tokio::watch bumps
            // its version on every send (regardless of value equality), so
            // rx.changed() fires once the SM has actually processed this
            // command. After wait_for above marked the post-first-Cancel
            // Idle as "seen", the next changed() blocks until the second
            // send — proving the SM picked up our message before we sample.
            timeout(Duration::from_secs(2), rx.changed())
                .await
                .unwrap_or_else(|_| panic!("[{label}] SM did not process second Cancel within 2s"))
                .unwrap();

            assert!(
                matches!(*handle.state_rx.borrow(), PairingState::Idle),
                "[{label}] no-op second Cancel must leave state Idle"
            );
            let count_after = cancel_count.load(Ordering::Relaxed);
            assert_eq!(
                count_before, count_after,
                "[{label}] no-op second Cancel must NOT publish a wire \
                 Cancel (ctx must already be None); was {count_before} \
                 cancels, now {count_after} — stale-ctx regression?"
            );
        }

        // Phase 1: Discovering. Start a solo Inviter — drop the joiner side
        // of the broker pair so the SM never receives a peer DISCOVER and
        // sits at Discovering until we Cancel.
        {
            let MintResult {
                state,
                recovery_artifact,
                ..
            } = mint_owner(1_700_000_000).unwrap();
            let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
            // CRITICAL: bind the joiner side to a named keepalive — the
            // underscore prefix silences "unused" but the variable is
            // load-bearing. Dropping it closes the broker's joiner→inviter
            // channel, which makes inviter.recv() return None and trips
            // run_state_machine's `return` arm at L180, killing the SM
            // before we can observe the cancel sequence below.
            let (inviter_t, _joiner_broker_keepalive) = InMemoryBroker::pair();
            let cancel_count = Arc::new(AtomicUsize::new(0));
            let inviter_t_wrapped: Arc<dyn PairingTransport> = Arc::new(CountCancelTransport {
                inner: Arc::new(inviter_t),
                cancel_count: cancel_count.clone(),
            });
            let mut inviter = spawn_state_machine(
                inviter_t_wrapped,
                fixed_clock(1_700_000_001),
                Duration::from_secs(60),
            );
            inviter
                .cmd_tx
                .send(PairingCommand::StartInviter {
                    display_name: "krile".to_string(),
                    owner_state: state,
                    master_seed,
                })
                .await
                .unwrap();
            timeout(Duration::from_secs(2), async {
                inviter
                    .state_rx
                    .clone()
                    .wait_for(|s| matches!(s, PairingState::Discovering { .. }))
                    .await
                    .unwrap();
            })
            .await
            .expect("Phase 1: reach Discovering");

            inviter.cmd_tx.send(PairingCommand::Cancel).await.unwrap();
            assert_idle_and_clean(&mut inviter, "Discovering", &cancel_count).await;
        }

        // Phase 2: Discovered. Pair two SMs; cancel the Inviter once it
        // sees the Joiner.
        {
            let MintResult {
                state,
                recovery_artifact,
                ..
            } = mint_owner(1_700_000_000).unwrap();
            let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
            let joiner_sk = SigningKey::generate(&mut OsRng);
            let (inviter_t, joiner_t) = InMemoryBroker::pair();
            let cancel_count = Arc::new(AtomicUsize::new(0));
            let inviter_t_wrapped: Arc<dyn PairingTransport> = Arc::new(CountCancelTransport {
                inner: Arc::new(inviter_t),
                cancel_count: cancel_count.clone(),
            });
            let mut inviter = spawn_state_machine(
                inviter_t_wrapped,
                fixed_clock(1_700_000_001),
                Duration::from_secs(60),
            );
            let _joiner = spawn_state_machine(
                Arc::new(joiner_t),
                fixed_clock(1_700_000_002),
                Duration::from_secs(60),
            );
            inviter
                .cmd_tx
                .send(PairingCommand::StartInviter {
                    display_name: "krile".to_string(),
                    owner_state: state,
                    master_seed,
                })
                .await
                .unwrap();
            _joiner
                .cmd_tx
                .send(PairingCommand::StartJoiner {
                    display_name: "avalon".to_string(),
                    signing_key: joiner_sk,
                })
                .await
                .unwrap();
            timeout(Duration::from_secs(2), async {
                inviter
                    .state_rx
                    .clone()
                    .wait_for(|s| matches!(s, PairingState::Discovered { .. }))
                    .await
                    .unwrap();
            })
            .await
            .expect("Phase 2: reach Discovered");

            inviter.cmd_tx.send(PairingCommand::Cancel).await.unwrap();
            assert_idle_and_clean(&mut inviter, "Discovered", &cancel_count).await;
        }

        // Phase 3: Handshaking. Drive both sides through SELECT so each
        // derives the SAS, then cancel the Inviter.
        {
            let MintResult {
                state,
                recovery_artifact,
                ..
            } = mint_owner(1_700_000_000).unwrap();
            let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
            let joiner_sk = SigningKey::generate(&mut OsRng);
            let (inviter_t, joiner_t) = InMemoryBroker::pair();
            let cancel_count = Arc::new(AtomicUsize::new(0));
            let inviter_t_wrapped: Arc<dyn PairingTransport> = Arc::new(CountCancelTransport {
                inner: Arc::new(inviter_t),
                cancel_count: cancel_count.clone(),
            });
            let mut inviter = spawn_state_machine(
                inviter_t_wrapped,
                fixed_clock(1_700_000_001),
                Duration::from_secs(60),
            );
            let joiner = spawn_state_machine(
                Arc::new(joiner_t),
                fixed_clock(1_700_000_002),
                Duration::from_secs(60),
            );
            inviter
                .cmd_tx
                .send(PairingCommand::StartInviter {
                    display_name: "krile".to_string(),
                    owner_state: state,
                    master_seed,
                })
                .await
                .unwrap();
            joiner
                .cmd_tx
                .send(PairingCommand::StartJoiner {
                    display_name: "avalon".to_string(),
                    signing_key: joiner_sk,
                })
                .await
                .unwrap();

            timeout(Duration::from_secs(2), async {
                inviter
                    .state_rx
                    .clone()
                    .wait_for(|s| matches!(s, PairingState::Discovered { .. }))
                    .await
                    .unwrap();
                joiner
                    .state_rx
                    .clone()
                    .wait_for(|s| matches!(s, PairingState::Discovered { .. }))
                    .await
                    .unwrap();
            })
            .await
            .expect("Phase 3: both reach Discovered");

            let inv_peer = match &*inviter.state_rx.borrow() {
                PairingState::Discovered { peers } => peers[0].session_id,
                _ => panic!("inviter not Discovered"),
            };
            let joi_peer = match &*joiner.state_rx.borrow() {
                PairingState::Discovered { peers } => peers[0].session_id,
                _ => panic!("joiner not Discovered"),
            };
            inviter
                .cmd_tx
                .send(PairingCommand::SelectPeer {
                    peer_session_id: inv_peer,
                })
                .await
                .unwrap();
            joiner
                .cmd_tx
                .send(PairingCommand::SelectPeer {
                    peer_session_id: joi_peer,
                })
                .await
                .unwrap();
            timeout(Duration::from_secs(2), async {
                inviter
                    .state_rx
                    .clone()
                    .wait_for(|s| matches!(s, PairingState::Handshaking { .. }))
                    .await
                    .unwrap();
            })
            .await
            .expect("Phase 3: reach Handshaking");

            inviter.cmd_tx.send(PairingCommand::Cancel).await.unwrap();
            assert_idle_and_clean(&mut inviter, "Handshaking", &cancel_count).await;
        }

        // Phase 4: WaitingPeerConfirm. Drive both to Handshaking; only the
        // Inviter ConfirmsSas, so it sits at WaitingPeerConfirm. Cancel
        // from there.
        {
            let MintResult {
                state,
                recovery_artifact,
                ..
            } = mint_owner(1_700_000_000).unwrap();
            let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
            let joiner_sk = SigningKey::generate(&mut OsRng);
            let (inviter_t, joiner_t) = InMemoryBroker::pair();
            let cancel_count = Arc::new(AtomicUsize::new(0));
            let inviter_t_wrapped: Arc<dyn PairingTransport> = Arc::new(CountCancelTransport {
                inner: Arc::new(inviter_t),
                cancel_count: cancel_count.clone(),
            });
            let mut inviter = spawn_state_machine(
                inviter_t_wrapped,
                fixed_clock(1_700_000_001),
                Duration::from_secs(60),
            );
            let joiner = spawn_state_machine(
                Arc::new(joiner_t),
                fixed_clock(1_700_000_002),
                Duration::from_secs(60),
            );
            inviter
                .cmd_tx
                .send(PairingCommand::StartInviter {
                    display_name: "krile".to_string(),
                    owner_state: state,
                    master_seed,
                })
                .await
                .unwrap();
            joiner
                .cmd_tx
                .send(PairingCommand::StartJoiner {
                    display_name: "avalon".to_string(),
                    signing_key: joiner_sk,
                })
                .await
                .unwrap();
            timeout(Duration::from_secs(2), async {
                inviter
                    .state_rx
                    .clone()
                    .wait_for(|s| matches!(s, PairingState::Discovered { .. }))
                    .await
                    .unwrap();
                joiner
                    .state_rx
                    .clone()
                    .wait_for(|s| matches!(s, PairingState::Discovered { .. }))
                    .await
                    .unwrap();
            })
            .await
            .expect("Phase 4: both reach Discovered");
            let inv_peer = match &*inviter.state_rx.borrow() {
                PairingState::Discovered { peers } => peers[0].session_id,
                _ => panic!(),
            };
            let joi_peer = match &*joiner.state_rx.borrow() {
                PairingState::Discovered { peers } => peers[0].session_id,
                _ => panic!(),
            };
            inviter
                .cmd_tx
                .send(PairingCommand::SelectPeer {
                    peer_session_id: inv_peer,
                })
                .await
                .unwrap();
            joiner
                .cmd_tx
                .send(PairingCommand::SelectPeer {
                    peer_session_id: joi_peer,
                })
                .await
                .unwrap();
            timeout(Duration::from_secs(2), async {
                inviter
                    .state_rx
                    .clone()
                    .wait_for(|s| matches!(s, PairingState::Handshaking { .. }))
                    .await
                    .unwrap();
            })
            .await
            .expect("Phase 4: inviter reaches Handshaking");

            // Inviter alone confirms — Joiner remains at Handshaking, so
            // the Inviter sits at WaitingPeerConfirm.
            inviter
                .cmd_tx
                .send(PairingCommand::ConfirmSas)
                .await
                .unwrap();
            timeout(Duration::from_secs(2), async {
                inviter
                    .state_rx
                    .clone()
                    .wait_for(|s| matches!(s, PairingState::WaitingPeerConfirm { .. }))
                    .await
                    .unwrap();
            })
            .await
            .expect("Phase 4: inviter reaches WaitingPeerConfirm");

            inviter.cmd_tx.send(PairingCommand::Cancel).await.unwrap();
            assert_idle_and_clean(&mut inviter, "WaitingPeerConfirm", &cancel_count).await;
        }

        // Phase 5: Enrolling — cancel the JOINER from Enrolling.
        //
        // Pick the Joiner side because it sits stably at Enrolling waiting
        // for ENROLL: the Inviter's "Enrolling" is transient (auto-advances
        // to Complete the moment publish() returns Ok). To keep the Joiner
        // at Enrolling deterministically we silently drop the Inviter's
        // ENROLL — the second `Encrypted` payload it publishes (the first
        // is CONFIRM, which must reach the Joiner so peer_confirmed flips).
        {
            struct DropNthEncryptedTransport {
                inner: Arc<dyn PairingTransport>,
                encrypted_count: Arc<AtomicUsize>,
                drop_at: usize,
            }
            #[async_trait]
            impl PairingTransport for DropNthEncryptedTransport {
                async fn publish(&self, message: PairingWireMessage) -> Result<(), String> {
                    if matches!(message, PairingWireMessage::Encrypted { .. }) {
                        let n = self.encrypted_count.fetch_add(1, Ordering::Relaxed);
                        if n == self.drop_at {
                            // Silent drop: report success so the Inviter
                            // commits + emits Complete normally; only the
                            // Joiner side is starved of the wire message.
                            return Ok(());
                        }
                    }
                    self.inner.publish(message).await
                }
                async fn recv(&self) -> Option<PairingWireMessage> {
                    self.inner.recv().await
                }
            }

            let MintResult {
                state,
                recovery_artifact,
                ..
            } = mint_owner(1_700_000_000).unwrap();
            let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
            let joiner_sk = SigningKey::generate(&mut OsRng);
            let (inviter_t, joiner_t) = InMemoryBroker::pair();
            let wrapped_inviter_t: Arc<dyn PairingTransport> =
                Arc::new(DropNthEncryptedTransport {
                    inner: Arc::new(inviter_t),
                    encrypted_count: Arc::new(AtomicUsize::new(0)),
                    drop_at: 1, // drop the SECOND Encrypted publish == ENROLL
                });
            let cancel_count = Arc::new(AtomicUsize::new(0));
            let joiner_t_wrapped: Arc<dyn PairingTransport> = Arc::new(CountCancelTransport {
                inner: Arc::new(joiner_t),
                cancel_count: cancel_count.clone(),
            });
            let inviter = spawn_state_machine(
                wrapped_inviter_t,
                fixed_clock(1_700_000_001),
                Duration::from_secs(60),
            );
            let mut joiner = spawn_state_machine(
                joiner_t_wrapped,
                fixed_clock(1_700_000_002),
                Duration::from_secs(60),
            );
            inviter
                .cmd_tx
                .send(PairingCommand::StartInviter {
                    display_name: "krile".to_string(),
                    owner_state: state,
                    master_seed,
                })
                .await
                .unwrap();
            joiner
                .cmd_tx
                .send(PairingCommand::StartJoiner {
                    display_name: "avalon".to_string(),
                    signing_key: joiner_sk,
                })
                .await
                .unwrap();
            timeout(Duration::from_secs(2), async {
                inviter
                    .state_rx
                    .clone()
                    .wait_for(|s| matches!(s, PairingState::Discovered { .. }))
                    .await
                    .unwrap();
                joiner
                    .state_rx
                    .clone()
                    .wait_for(|s| matches!(s, PairingState::Discovered { .. }))
                    .await
                    .unwrap();
            })
            .await
            .expect("Phase 5: both reach Discovered");
            let inv_peer = match &*inviter.state_rx.borrow() {
                PairingState::Discovered { peers } => peers[0].session_id,
                _ => panic!(),
            };
            let joi_peer = match &*joiner.state_rx.borrow() {
                PairingState::Discovered { peers } => peers[0].session_id,
                _ => panic!(),
            };
            inviter
                .cmd_tx
                .send(PairingCommand::SelectPeer {
                    peer_session_id: inv_peer,
                })
                .await
                .unwrap();
            joiner
                .cmd_tx
                .send(PairingCommand::SelectPeer {
                    peer_session_id: joi_peer,
                })
                .await
                .unwrap();
            timeout(Duration::from_secs(2), async {
                joiner
                    .state_rx
                    .clone()
                    .wait_for(|s| matches!(s, PairingState::Handshaking { .. }))
                    .await
                    .unwrap();
            })
            .await
            .expect("Phase 5: joiner reaches Handshaking");

            inviter
                .cmd_tx
                .send(PairingCommand::ConfirmSas)
                .await
                .unwrap();
            joiner
                .cmd_tx
                .send(PairingCommand::ConfirmSas)
                .await
                .unwrap();
            // Joiner reaches Enrolling once both confirms have processed.
            // ENROLL never arrives (dropped), so this is stable.
            timeout(Duration::from_secs(3), async {
                joiner
                    .state_rx
                    .clone()
                    .wait_for(|s| matches!(s, PairingState::Enrolling))
                    .await
                    .unwrap();
            })
            .await
            .expect("Phase 5: joiner reaches Enrolling (and stays there)");

            joiner.cmd_tx.send(PairingCommand::Cancel).await.unwrap();
            assert_idle_and_clean(&mut joiner, "Enrolling", &cancel_count).await;
        }
    }

    /// ZEB-198: SAS-mismatch user flow (the MitM-detection path).
    ///
    /// In a real MitM, the attacker runs two separate ECDHs (one with each
    /// real peer). Each real peer's `derive_sas` against the attacker's
    /// pubkey yields a different shared secret, and therefore a different
    /// 6-digit SAS. The user sees DIFFERENT digits on the two screens and
    /// clicks "no don't match" — both sides Cancel and return to Idle.
    ///
    /// We simulate this by running two independent SMs, each driven via a
    /// ScriptedTransport with a *different* fake peer X25519 pubkey. ECDH
    /// is mathematically the same operation as in a real MitM:
    ///   SM_A's session_key = ECDH(SM_A.eph_sk, fake_peer_A.pk)
    ///   SM_B's session_key = ECDH(SM_B.eph_sk, fake_peer_B.pk)
    /// With high probability these differ, so the SAS digits differ.
    ///
    /// `sas.rs::sas_differs_under_mitm` already proves the cryptographic
    /// property; this test pins down the *state machine's* end-to-end UX:
    /// reach Handshaking, observe the mismatch, Cancel-on-both-sides
    /// returns both to Idle without leaving stale ctx.
    #[tokio::test]
    async fn sas_mismatch_path() {
        use x25519_dalek::{PublicKey as X25519Pub, StaticSecret as X25519Sec};

        // Drive a SM to Handshaking by injecting a fake peer (with a known
        // X25519 secret so the test could derive the same session_key, though
        // we don't need it here — we only need the SAS to be observable).
        // Returns (handle, in_tx, sas_digits, transport). The transport is
        // returned as `Arc<ScriptedTransport>` (not the trait object) so the
        // caller can read `transport.published` to verify wire-publish side
        // effects of subsequent commands.
        async fn drive_to_handshaking(
            role: PairingRole,
            fake_x_sk: &X25519Sec,
            clock_t: u64,
        ) -> (
            PairingHandle,
            mpsc::Sender<PairingWireMessage>,
            String,
            Arc<ScriptedTransport>,
        ) {
            let (in_tx, in_rx) = mpsc::channel::<PairingWireMessage>(16);
            let (out_tx, _out_rx) = mpsc::channel::<PairingWireMessage>(16);
            let transport = Arc::new(ScriptedTransport {
                publish_tx: out_tx,
                recv_rx: AsyncMutex::new(in_rx),
                published: StdMutex::new(Vec::new()),
            });

            let handle = spawn_state_machine(
                transport.clone(),
                fixed_clock(clock_t),
                Duration::from_secs(60),
            );

            // Start the SM in the requested role.
            match role {
                PairingRole::Inviter => {
                    let MintResult {
                        state,
                        recovery_artifact,
                        ..
                    } = mint_owner(clock_t).unwrap();
                    let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
                    handle
                        .cmd_tx
                        .send(PairingCommand::StartInviter {
                            display_name: "krile".to_string(),
                            owner_state: state,
                            master_seed,
                        })
                        .await
                        .unwrap();
                }
                PairingRole::Joiner => {
                    let joiner_sk = SigningKey::generate(&mut OsRng);
                    handle
                        .cmd_tx
                        .send(PairingCommand::StartJoiner {
                            display_name: "avalon".to_string(),
                            signing_key: joiner_sk,
                        })
                        .await
                        .unwrap();
                }
            }

            // Capture our session_id from the SM's own published DISCOVER.
            let our_session_id = timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(PairingWireMessage::Discover { session_id, .. }) =
                        transport.published.lock().unwrap().first().cloned()
                    {
                        return session_id;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("SM publishes its DISCOVER within 2s");

            // Inject a fake peer DISCOVER. The fake peer is the OPPOSITE role.
            let fake_session = Uuid::new_v4();
            let fake_x_pk = X25519Pub::from(fake_x_sk);
            let fake_ed = SigningKey::generate(&mut OsRng);
            let (fake_role, fake_owner_id_if_inviter, fake_joiner_ed25519_verify_hex) = match role {
                PairingRole::Inviter => (
                    PairingRole::Joiner,
                    None,
                    Some(hex::encode(fake_ed.verifying_key().to_bytes())),
                ),
                PairingRole::Joiner => {
                    (PairingRole::Inviter, Some(hex::encode([0xABu8; 16])), None)
                }
            };
            in_tx
                .send(PairingWireMessage::Discover {
                    session_id: fake_session,
                    role: fake_role,
                    ephemeral_pubkey_hex: hex::encode(fake_x_pk.as_bytes()),
                    display_name: "fake-peer".to_string(),
                    owner_id_if_inviter: fake_owner_id_if_inviter,
                    joiner_ed25519_verify_hex: fake_joiner_ed25519_verify_hex,
                })
                .await
                .unwrap();

            // Wait for SM to observe the fake peer in Discovered.
            let mut state_rx = handle.state_rx.clone();
            timeout(Duration::from_secs(2), async {
                state_rx
                    .wait_for(
                        |s| matches!(s, PairingState::Discovered { peers } if !peers.is_empty()),
                    )
                    .await
                    .unwrap();
            })
            .await
            .expect("SM reaches Discovered with fake peer");

            // SelectPeer + inject the matching fake SELECT to advance to
            // Handshaking (which triggers SAS derivation).
            handle
                .cmd_tx
                .send(PairingCommand::SelectPeer {
                    peer_session_id: fake_session,
                })
                .await
                .unwrap();
            in_tx
                .send(PairingWireMessage::Select {
                    my_session_id: fake_session,
                    peer_session_id: our_session_id,
                })
                .await
                .unwrap();

            timeout(Duration::from_secs(2), async {
                state_rx
                    .wait_for(|s| matches!(s, PairingState::Handshaking { .. }))
                    .await
                    .unwrap();
            })
            .await
            .expect("SM reaches Handshaking");

            let sas = match &*handle.state_rx.borrow() {
                PairingState::Handshaking { sas_digits, .. } => sas_digits.clone(),
                other => panic!("expected Handshaking, got {other:?}"),
            };
            (handle, in_tx, sas, transport)
        }

        // Two independent SMs each talking to their own fake peer with a
        // fresh X25519 keypair. SAS = uniform-random mod 1_000_000, so a
        // single random pair has ~10⁻⁶ chance of accidentally colliding.
        // PR #64 review (Qodo/CodeRabbit) flagged the original single-shot
        // assert_ne! as a low-rate flake; a bounded retry pushes the
        // collision probability after N=5 retries to ~10⁻³⁰, effectively
        // zero. A regression that fixed SAS to a constant would collide
        // every time and panic with the message below.
        //
        // CRITICAL: each ScriptedTransport's `in_tx` (the inbound channel
        // sender) MUST stay alive until after the Cancel assertions
        // complete. Dropping it closes the transport's recv channel, which
        // makes `transport.recv()` return None. In `run_state_machine`'s
        // tokio::select!, `transport.recv() = None` causes the SM task to
        // `return` (L179-191), racing with the about-to-be-processed
        // Cancel — the CI failure on commit 47fb953 was exactly this race.
        // We keep the senders alongside the handles in the Option tuple so
        // they live for the rest of the test.
        const MAX_ATTEMPTS: usize = 5;
        type SmFixture = (
            PairingHandle,
            mpsc::Sender<PairingWireMessage>,
            Arc<ScriptedTransport>,
        );
        let mut sm_inviter_opt: Option<SmFixture> = None;
        let mut sm_joiner_opt: Option<SmFixture> = None;
        let mut sas_inviter = String::new();
        let mut sas_joiner = String::new();
        for attempt in 0..MAX_ATTEMPTS {
            let fake_peer_a = X25519Sec::random_from_rng(OsRng);
            let fake_peer_b = X25519Sec::random_from_rng(OsRng);
            let (h_a, in_tx_a, sas_a, transport_a) = drive_to_handshaking(
                PairingRole::Inviter,
                &fake_peer_a,
                1_700_000_001 + attempt as u64,
            )
            .await;
            let (h_b, in_tx_b, sas_b, transport_b) = drive_to_handshaking(
                PairingRole::Joiner,
                &fake_peer_b,
                1_700_000_500 + attempt as u64,
            )
            .await;
            if sas_a != sas_b {
                sm_inviter_opt = Some((h_a, in_tx_a, transport_a));
                sm_joiner_opt = Some((h_b, in_tx_b, transport_b));
                sas_inviter = sas_a;
                sas_joiner = sas_b;
                break;
            }
            // Collision (~10⁻⁶) — drop SMs (and their senders) and retry.
            // On the last attempt, exhausting the retry budget means SAS
            // is always equal, which is the regression we want to catch.
            if attempt == MAX_ATTEMPTS - 1 {
                panic!(
                    "SAS values collided across {MAX_ATTEMPTS} attempts \
                     (probability ~10⁻{} under correct derive_sas) — likely \
                     derive_sas regressed to ignore the peer key. Both = {sas_a}",
                    MAX_ATTEMPTS * 6
                );
            }
        }
        let (mut sm_inviter, _inviter_in_tx, inviter_transport) =
            sm_inviter_opt.expect("inviter handle assigned in loop");
        let (mut sm_joiner, _joiner_in_tx, joiner_transport) =
            sm_joiner_opt.expect("joiner handle assigned in loop");
        // Belt-and-suspenders: the loop only exits via `break` when SAS
        // values differ, but make the invariant explicit.
        assert_ne!(sas_inviter, sas_joiner);

        // User clicks "no don't match" on each device. Both must Cancel
        // cleanly back to Idle. The first Cancel publishes a wire Cancel
        // (ctx is Some); a follow-up no-op Cancel must NOT (ctx is None,
        // and the handler at L166-176 gates the publish on ctx.is_some()).
        // Mirrors the assert_idle_and_clean pattern in
        // state_machine_cancel_at_each_stage.
        sm_inviter
            .cmd_tx
            .send(PairingCommand::Cancel)
            .await
            .unwrap();
        sm_joiner.cmd_tx.send(PairingCommand::Cancel).await.unwrap();

        for (label, handle, transport) in [
            ("inviter", &mut sm_inviter, &inviter_transport),
            ("joiner", &mut sm_joiner, &joiner_transport),
        ] {
            let mut rx = handle.state_rx.clone();
            timeout(Duration::from_secs(2), async {
                rx.wait_for(|s| matches!(s, PairingState::Idle))
                    .await
                    .unwrap();
            })
            .await
            .unwrap_or_else(|_| panic!("{label} SM did not reach Idle within 2s"));

            // Snapshot the count of wire Cancels published so far. The
            // first user-Cancel above should already have produced one (ctx
            // was Some). The follow-up no-op Cancel below must NOT bump it.
            let cancels_before = transport
                .published
                .lock()
                .unwrap()
                .iter()
                .filter(|m| matches!(m, PairingWireMessage::Cancel { .. }))
                .count();

            handle.cmd_tx.send(PairingCommand::Cancel).await.unwrap();
            // Deterministic ack: see assert_idle_and_clean for the
            // watch::Sender version-bump rationale. Cancel handler always
            // calls state_tx.send(Idle), so rx.changed() fires when the SM
            // has actually picked up this no-op command — no fixed sleep
            // and no race even on a loaded CI runner.
            timeout(Duration::from_secs(2), rx.changed())
                .await
                .unwrap_or_else(|_| panic!("{label}: SM did not process second Cancel within 2s"))
                .unwrap();

            assert!(
                matches!(*handle.state_rx.borrow(), PairingState::Idle),
                "{label}: state must remain Idle"
            );
            let cancels_after = transport
                .published
                .lock()
                .unwrap()
                .iter()
                .filter(|m| matches!(m, PairingWireMessage::Cancel { .. }))
                .count();
            assert_eq!(
                cancels_before, cancels_after,
                "{label}: no-op second Cancel must NOT publish a wire Cancel \
                 (ctx must already be None); was {cancels_before} cancels, \
                 now {cancels_after} — stale-ctx regression?"
            );
        }
    }
}
