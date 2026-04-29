use crate::pairing::cert::{sign_enrollment_for_joiner, verify_cert_for_self};
use crate::pairing::sas::derive_sas;
use crate::pairing::session::{decrypt as session_decrypt, encrypt as session_encrypt};
use crate::pairing::transport::PairingTransport;
use crate::pairing::types::{
    DiscoveredPeer, EncryptedPayload, PairingRole, PairingState, PairingWireMessage,
};
use ed25519_dalek::SigningKey;
use harmony_owner::pubkey_bundle::PubKeyBundle;
use harmony_owner::state::OwnerState;
use harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;
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
/// new peer device succeeds. The persistence layer consumes this to write
/// the freshly-mutated `OwnerState` to disk so the new enrollment survives
/// restarts. Unlike the Joiner result, this carries no signing key — the
/// Inviter already has its own signing key persisted on disk; the persistence
/// layer reloads it via `load_owner_state` and rewrites the state file
/// atomically alongside the existing key.
pub struct InviterEnrollResult {
    pub owner_state: OwnerState,
    /// The master seed — Inviter HAS this (cert-only model only restricts the
    /// Joiner). Persisting it via `Some(seed)` keeps `canBackUp: true`.
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
) {
    // Per-session local context. Reset each time we leave a session.
    let mut ctx: Option<SessionCtx> = None;

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
                        ctx = Some(start_inviter(&transport, &state_tx, display_name, owner_state, master_seed, &now_fn).await);
                    }
                    PairingCommand::StartJoiner { display_name, signing_key } => {
                        ctx = Some(start_joiner(&transport, &state_tx, display_name, signing_key, &now_fn).await);
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
        }
    }
}

struct SessionCtx {
    role: PairingRole,
    session_id: Uuid,
    /// Our local display name (for diagnostics + future advisory display in
    /// follow-up tasks). Stored on the ctx so future commands can re-emit
    /// without the caller re-supplying it.
    #[allow(dead_code)]
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
) -> SessionCtx {
    let mut ctx = SessionCtx::new(PairingRole::Inviter, display_name.clone());
    ctx.owner_state = Some(owner_state.clone());
    ctx.master_seed = Some(master_seed);

    let _ = state_tx.send(PairingState::Discovering {
        role: PairingRole::Inviter,
        ephemeral_pubkey_hex: hex::encode(ctx.eph_pk.as_bytes()),
        session_id: ctx.session_id,
    });

    let _ = transport
        .publish(PairingWireMessage::Discover {
            session_id: ctx.session_id,
            role: PairingRole::Inviter,
            ephemeral_pubkey_hex: hex::encode(ctx.eph_pk.as_bytes()),
            display_name,
            owner_id_if_inviter: Some(hex::encode(owner_state.owner_id)),
            joiner_ed25519_verify_hex: None,
        })
        .await;

    ctx
}

async fn start_joiner(
    transport: &Arc<dyn PairingTransport>,
    state_tx: &watch::Sender<PairingState>,
    display_name: String,
    signing_key: SigningKey,
    _now_fn: &Arc<dyn Fn() -> u64 + Send + Sync>,
) -> SessionCtx {
    let mut ctx = SessionCtx::new(PairingRole::Joiner, display_name.clone());
    let verify_bytes = signing_key.verifying_key().to_bytes();
    let pubkey = PubKeyBundle::classical_only(verify_bytes);
    ctx.our_signing_key = Some(signing_key);
    ctx.our_pubkey = Some(pubkey);

    let _ = state_tx.send(PairingState::Discovering {
        role: PairingRole::Joiner,
        ephemeral_pubkey_hex: hex::encode(ctx.eph_pk.as_bytes()),
        session_id: ctx.session_id,
    });

    let _ = transport
        .publish(PairingWireMessage::Discover {
            session_id: ctx.session_id,
            role: PairingRole::Joiner,
            ephemeral_pubkey_hex: hex::encode(ctx.eph_pk.as_bytes()),
            display_name,
            owner_id_if_inviter: None,
            joiner_ed25519_verify_hex: Some(hex::encode(verify_bytes)),
        })
        .await;

    ctx
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

    // Publish SELECT.
    ctx.sent_select = true;
    let _ = transport
        .publish(PairingWireMessage::Select {
            my_session_id: ctx.session_id,
            peer_session_id,
        })
        .await;

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

    ctx.our_confirmed = true;

    // Encrypt + publish CONFIRM.
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
    let _ = transport
        .publish(PairingWireMessage::Encrypted {
            my_session_id: ctx.session_id,
            peer_session_id,
            nonce_hex: hex::encode(nonce),
            ciphertext_hex: hex::encode(ct),
        })
        .await;

    let _ = state_tx.send(PairingState::WaitingPeerConfirm { peer_session_id });

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

    let owner_state = ctx.owner_state.as_mut().expect("inviter has owner_state");
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
    let cert =
        match sign_enrollment_for_joiner(master_seed, owner_state, joiner_pubkey.clone(), now) {
            Ok(c) => c,
            Err(e) => {
                let _ = state_tx.send(PairingState::Failed {
                    reason: format!("sign cert: {e}"),
                });
                return;
            }
        };

    if let Err(e) = owner_state.add_enrollment(cert.clone(), now, DEFAULT_ACTIVE_WINDOW_SECS) {
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
    if let Err(e) = ciborium::into_writer(&*owner_state, &mut state_cbor) {
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
    let _ = transport
        .publish(PairingWireMessage::Encrypted {
            my_session_id: ctx.session_id,
            peer_session_id: ctx.selected_peer_session_id.expect("set"),
            nonce_hex: hex::encode(nonce),
            ciphertext_hex: hex::encode(ct),
        })
        .await;

    ctx.cert_sent = true;

    // Emit the InviterEnrollResult so the persistence layer (drained in
    // start_node) can write the freshly-mutated OwnerState back to disk.
    // Without this, the new enrollment lives only in RAM and the Inviter's
    // DevicesPanel reverts to showing only itself on next start_node.
    let _ = inviter_result_tx
        .send(InviterEnrollResult {
            owner_state: owner_state.clone(),
            master_seed: master_seed.clone(),
        })
        .await;

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
            // React if the sender is our selected peer OR a peer we have
            // discovered but not yet selected (pre-selection cancel).
            let is_our_peer = ctx
                .as_ref()
                .map(|c| {
                    // Post-selection: they are our chosen peer.
                    c.selected_peer_session_id == Some(my_session_id)
                    // Pre-selection: they appear in our discovered list.
                    || c.discovered_peers.iter().any(|p| p.session_id == my_session_id)
                })
                .unwrap_or(false);
            if is_our_peer {
                // Drop the session context (mirrors the local-Cancel path).
                *ctx = None;
                let _ = state_tx.send(PairingState::Idle);
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
            let _ = result_tx
                .send(JoinerEnrollResult {
                    our_signing_key: our_sk,
                    owner_state,
                    our_device_id,
                })
                .await;
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
    use ed25519_dalek::SigningKey;
    use harmony_owner::lifecycle::{mint_owner, MintResult};
    use rand::rngs::OsRng;
    use std::time::Duration;
    use tokio::time::timeout;
    use zeroize::Zeroizing;

    fn fixed_clock(t: u64) -> Arc<dyn Fn() -> u64 + Send + Sync> {
        Arc::new(move || t)
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
        let inviter_handle = spawn_state_machine(Arc::new(inviter_t), fixed_clock(1_700_000_001));
        let joiner_handle = spawn_state_machine(Arc::new(joiner_t), fixed_clock(1_700_000_002));

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
        use crate::pairing::transport::PairingTransport;
        use crate::pairing::types::PairingWireMessage;
        use async_trait::async_trait;
        use std::sync::Mutex as StdMutex;
        use tokio::sync::Mutex as AsyncMutex;
        use x25519_dalek::{PublicKey as X25519Pub, StaticSecret as X25519Sec};

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

        let inviter_handle = spawn_state_machine(transport.clone(), fixed_clock(1_700_000_001));

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

    /// Regression test for Fix 2: when the remote peer sends Cancel, the
    /// receiving state machine must clear its ctx so no further wire messages
    /// can advance it (e.g. re-emit Discovered from a stale peer list).
    #[tokio::test]
    async fn cancel_drops_ctx_on_remote_cancel() {
        let MintResult {
            state,
            recovery_artifact,
            ..
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
        let joiner_sk = SigningKey::generate(&mut OsRng);

        let (inviter_t, joiner_t) = InMemoryBroker::pair();
        let inviter_handle = spawn_state_machine(Arc::new(inviter_t), fixed_clock(1_700_000_001));
        let joiner_handle = spawn_state_machine(Arc::new(joiner_t), fixed_clock(1_700_000_002));

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

        // Joiner sends Cancel — this publishes a CANCEL wire message that the
        // Inviter will receive.
        joiner_handle
            .cmd_tx
            .send(PairingCommand::Cancel)
            .await
            .unwrap();

        // Inviter must transition to Idle.
        timeout(Duration::from_secs(2), async {
            inviter_state
                .wait_for(|s| matches!(s, PairingState::Idle))
                .await
                .unwrap();
        })
        .await
        .expect("inviter reaches Idle after remote cancel");

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
}
