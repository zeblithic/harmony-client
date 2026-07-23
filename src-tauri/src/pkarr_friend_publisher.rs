//! ZEB-371 Phase 1b: Case-D publisher. Publishes this node's iroh reachability
//! under a per-friend, secret-derived pkarr slot so each active friend can
//! resolve us across WAN without any global discoverability. One registered
//! handle per active friend; mirrors `PkarrIdentityPublisher`.

use crate::friend_rendezvous::{
    case_d_info, case_d_publish_key, open_case_d_payload, seal_case_d_payload,
};
use harmony_pkarr::{
    current_epoch_id, epoch_tolerance_window, resolve_rendezvous_with, EphemeralKeyBuilder,
    PkarrCase, PkarrPublisher, PkarrResolver, PkarrRoutingRecord, PkarrSlotResolver, RecordBuilder,
    RendezvousResolveConfig,
};
use std::sync::Arc;
use std::time::Duration;
use zeroize::Zeroizing;

/// Per-batch resolve deadline for friend Case-D (ZEB-743). Friend's old
/// `resolve_window` had no explicit deadline (it bounded on the pkarr resolver's
/// own per-query timeout); the kernel driver takes one, so set it >= the
/// community resolve deadline (2500 ms) so it never cuts a slow-but-valid
/// resolve before the resolver's own timeout in the common case.
const FRIEND_RESOLVE_DEADLINE: Duration = Duration::from_millis(3000);

pub struct PkarrFriendPublisher {
    publisher: Arc<PkarrPublisher>,
    self_owner: [u8; 16],
    /// Builds the raw (unsealed) iroh routing blob for the current reachability.
    routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
}

fn friend_handle(friend_owner: &[u8; 16]) -> String {
    format!("friend:{}", hex::encode(friend_owner))
}

impl PkarrFriendPublisher {
    pub fn new(
        publisher: Arc<PkarrPublisher>,
        self_owner: [u8; 16],
        routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    ) -> Self {
        Self {
            publisher,
            self_owner,
            routing_blob_builder,
        }
    }

    /// Begin (or refresh) Case-D publication for one active friend, keyed on the
    /// shared `secret`. Idempotent: re-registering the same friend replaces the
    /// builders (e.g. to pick up a changed reachability blob).
    ///
    /// `secret` stays wrapped in `Zeroizing` the whole way through — including
    /// inside the two long-lived closure captures via a shared `Arc` — so the
    /// friendship key material is scrubbed on drop and never lands in a plain
    /// `[u8; 32]` copy (which would never be zeroized).
    pub async fn register_friend(&self, friend_owner: [u8; 16], secret: Zeroizing<[u8; 32]>) {
        let self_owner = self.self_owner;
        let secret = Arc::new(secret);
        let key_secret = Arc::clone(&secret);
        let key_builder: EphemeralKeyBuilder = Arc::new(move |at_ms| {
            case_d_publish_key(&key_secret, current_epoch_id(at_ms), &self_owner)
        });
        let blob_builder = Arc::clone(&self.routing_blob_builder);
        let payload_secret = Arc::clone(&secret);
        let builder: RecordBuilder = Arc::new(move |at_ms| {
            let epoch = current_epoch_id(at_ms);
            let cd_key = case_d_publish_key(&payload_secret, epoch, &self_owner);
            let mut id_pub = [0u8; 64];
            id_pub[32..].copy_from_slice(&cd_key.verifying_key().to_bytes());
            let sealed = seal_case_d_payload(&payload_secret, epoch, &blob_builder())
                .expect("case-d payload seal");
            PkarrRoutingRecord::sign_new(
                sealed,
                id_pub,
                at_ms,
                at_ms + crate::reachability_record::REACHABILITY_RECORD_TTL_MS,
                &cd_key,
            )
            .expect("sign — derived key matches embedded id_pub by construction")
        });
        self.publisher
            .register(friend_handle(&friend_owner), key_builder, builder)
            .await;
    }

    /// Stop Case-D publication for a friend (on revoke / secret cleared).
    pub async fn unregister_friend(&self, friend_owner: &[u8; 16]) {
        self.publisher
            .unregister(&friend_handle(friend_owner))
            .await;
    }
}

/// Reconcile the friend publisher's active Case-D handles with the friend graph:
/// register every `Active` friend that has a `sealed_secret` (opened with `kt`),
/// unregister everyone else (`Pending`/`Revoked`, missing secret, or a secret that
/// fails to open). Idempotent; safe to call on every reachability tick.
///
/// `register_friend` re-registers with the current `routing_blob_builder`, so
/// re-calling this on an address change refreshes every active friend's published
/// blob. The caller is responsible for snapshotting `friends` out from under any
/// owner-state lock BEFORE calling this — it performs network `.await`s.
pub async fn sync_case_d_handles(
    friend_pub: &PkarrFriendPublisher,
    friends: &std::collections::BTreeMap<
        crate::owner_state_types::OwnerAddr,
        crate::friend_graph::FriendEntry,
    >,
    kt: &crate::owner_state_crypto::KeyTree,
) {
    use crate::friend_graph::FriendStatus;
    for (owner, entry) in friends.iter() {
        let should_publish = entry.status == FriendStatus::Active;
        let secret = if should_publish {
            if let Some(sealed) = entry.sealed_secret.as_deref() {
                match crate::owner_state_crypto::decrypt_friend_secret(kt, &owner.0, sealed) {
                    Ok(s) => Some(s),
                    Err(_) => {
                        tracing::warn!(
                            friend = %hex::encode(owner.0),
                            "ZEB-371: friend rendezvous secret failed to decrypt; skipping Case-D publish"
                        );
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        match secret {
            // Move the `Zeroizing` wrapper through — never deref into a plain
            // `[u8; 32]` copy that would escape zeroization.
            Some(secret) => friend_pub.register_friend(owner.0, secret).await,
            None => friend_pub.unregister_friend(&owner.0).await,
        }
    }
}

/// Resolve `friend_owner`'s current Case-D routing blob (UNSEALED) using the
/// shared `secret`, via the core `harmony_pkarr::rendezvous` driver (ZEB-743).
///
/// Friend Case-D is the degenerate N=1 rendezvous shape: one slot per
/// friendship, keyed by `case_d_info(epoch, friend_owner)`. We drive it through
/// the shared kernel (`resolve_rendezvous_with` with a `[1]` batch curve) so
/// friend and community resolve through one code path. The `PkarrSlotResolver`
/// derives the exact same per-epoch slot key `case_d_resolve_key` does
/// (case = `PkarrCase::Friend`, ikm = `secret`, info = `case_d_info`), so nothing
/// on the wire changes.
///
/// The kernel's decode closure is epoch-blind (`Fn(&[u8]) -> Option<P>`) but the
/// payload is AEAD-sealed with the epoch in the AAD, so the closure tries every
/// epoch in the tolerance window — a wrong-epoch attempt fails instantly on the
/// AAD mismatch, and N=1 keeps it to at most three trivial unseal attempts.
///
/// Returns `Ok(None)` when no live record is found OR a record is found but
/// cannot be unsealed. Per the driver's best-effort, first-responder-wins
/// contract a transient pkarr failure surfaces as `Ok(None)` (a miss this
/// round), not `Err` — matching the community resolve path.
pub async fn resolve_friend_case_d(
    resolver: &Arc<PkarrResolver>,
    secret: &[u8; 32],
    friend_owner: &[u8; 16],
) -> Result<Option<Vec<u8>>, String> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    // The decode closure unseals across the same epoch window the driver probes
    // (both derive it from `now_ms`), since the epoch-blind decode seam can't
    // tell which window epoch a given blob was sealed under.
    let window = epoch_tolerance_window(now_ms);
    let decode_secret = *secret;
    let info_owner = *friend_owner;
    let slot_resolver = PkarrSlotResolver::new(
        Arc::clone(resolver),
        PkarrCase::Friend,
        secret.to_vec(),
        Arc::new(move |_slot: u16, epoch: u64| case_d_info(epoch, &info_owner)),
        move |blob: &[u8]| {
            window
                .iter()
                .find_map(|&e| open_case_d_payload(&decode_secret, e, blob).ok())
        },
    );
    let outcome = resolve_rendezvous_with(
        &slot_resolver,
        now_ms,
        &RendezvousResolveConfig {
            batch_curve: vec![1],
            per_batch_deadline: FRIEND_RESOLVE_DEADLINE,
        },
    )
    .await;
    Ok(outcome.payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_pkarr::testing::MockPkarrRelay;
    use harmony_pkarr::{PkarrPublisher, PkarrResolver, RelayClient, RelayPool};
    use std::sync::Arc;
    use zeroize::Zeroizing;

    #[tokio::test]
    async fn case_d_publish_then_resolve_round_trip() {
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(Arc::clone(&client)));
        let _ph = Arc::clone(&publisher).spawn();
        let resolver = Arc::new(PkarrResolver::new(Arc::clone(&client)));

        let secret = [9u8; 32];
        let a_owner = [0xAA; 16]; // the publisher's own owner_id
        let raw = b"alice-iroh-routing".to_vec();
        let fp = PkarrFriendPublisher::new(
            Arc::clone(&publisher),
            a_owner,
            Arc::new(move || raw.clone()),
        );
        // A publishes its own Case-D slot (info = a_owner). The friend_owner handle
        // key is arbitrary here; A is making itself findable.
        fp.register_friend([0xBB; 16], Zeroizing::new(secret)).await;

        // B (who shares `secret`) resolves A under info = a_owner, unseals, gets raw.
        let mut attempts = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            attempts += 1;
            assert!(attempts < 80, "case-d resolve timed out");
            if let Some(blob) = resolve_friend_case_d(&resolver, &secret, &a_owner)
                .await
                .expect("resolve")
            {
                assert_eq!(blob, b"alice-iroh-routing");
                return;
            }
        }
    }

    #[tokio::test]
    async fn resolve_friend_case_d_cold_start_returns_none() {
        // No record published for this friendship → the driver's cold-start path
        // yields an empty outcome, and the resolve reports Ok(None) (a miss for
        // this round), never Err — the best-effort first-responder-wins contract.
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let resolver = Arc::new(PkarrResolver::new(client));
        let out = resolve_friend_case_d(&resolver, &[7u8; 32], &[0xCC; 16])
            .await
            .expect("resolve must not error on a cold start");
        assert!(out.is_none(), "cold start must resolve to None");
    }

    #[tokio::test]
    async fn register_then_unregister_friend_slot() {
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(client));
        let _ph = Arc::clone(&publisher).spawn();

        let secret = [5u8; 32];
        let self_owner = [0xAA; 16];
        let friend = [0xBB; 16];
        let fp = PkarrFriendPublisher::new(
            Arc::clone(&publisher),
            self_owner,
            Arc::new(|| b"routing".to_vec()),
        );
        fp.register_friend(friend, Zeroizing::new(secret)).await;
        assert!(publisher
            .active_handles()
            .await
            .iter()
            .any(|h| h.starts_with("friend:")));
        fp.unregister_friend(&friend).await;
        assert!(!publisher
            .active_handles()
            .await
            .iter()
            .any(|h| h.starts_with("friend:")));
    }

    #[tokio::test]
    async fn sync_case_d_handles_registers_only_active_with_secret() {
        use crate::friend_graph::{FriendEntry, FriendGraph, FriendOrigin, FriendStatus};
        use crate::owner_state_crypto::{encrypt_friend_secret, KeyTree};
        use crate::owner_state_types::{Hlc, OwnerAddr};
        use std::collections::BTreeMap;

        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(client));
        let _ph = Arc::clone(&publisher).spawn();

        let self_owner = [0xAA; 16];
        let fp = PkarrFriendPublisher::new(
            Arc::clone(&publisher),
            self_owner,
            Arc::new(|| b"routing".to_vec()),
        );
        let kt = KeyTree::derive(&[0x42; 32]).expect("derive keytree");

        fn entry(status: FriendStatus, sealed_secret: Option<Vec<u8>>) -> FriendEntry {
            FriendEntry {
                master_ed25519: [0x11; 32],
                display: None,
                status,
                established_via: FriendOrigin::Token,
                referrable: false,
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "d".into(),
                },
                sealed_secret,
            }
        }

        // (a) Active WITH a sealed secret → should publish.
        let a_owner = OwnerAddr([0xA1; 16]);
        let a_secret = [0xCC; 32];
        let a_sealed = encrypt_friend_secret(&kt, &a_owner.0, &a_secret).expect("seal a");
        // (b) Active WITHOUT a secret → must NOT publish.
        let b_owner = OwnerAddr([0xB2; 16]);
        // (c) Revoked WITH a (stale) secret → must NOT publish.
        let c_owner = OwnerAddr([0xC3; 16]);
        let c_sealed = encrypt_friend_secret(&kt, &c_owner.0, &[0xDD; 32]).expect("seal c");

        let mut friends: BTreeMap<OwnerAddr, FriendEntry> = FriendGraph::default().friends;
        friends.insert(a_owner, entry(FriendStatus::Active, Some(a_sealed)));
        friends.insert(b_owner, entry(FriendStatus::Active, None));
        friends.insert(c_owner, entry(FriendStatus::Revoked, Some(c_sealed)));

        sync_case_d_handles(&fp, &friends, &kt).await;

        let a_handle = friend_handle(&a_owner.0);
        let b_handle = friend_handle(&b_owner.0);
        let c_handle = friend_handle(&c_owner.0);
        let handles = publisher.active_handles().await;
        assert!(
            handles.contains(&a_handle),
            "Active+secret friend (a) must be published; got {handles:?}"
        );
        assert!(
            !handles.contains(&b_handle),
            "Active-without-secret friend (b) must NOT be published; got {handles:?}"
        );
        assert!(
            !handles.contains(&c_handle),
            "Revoked friend (c) must NOT be published; got {handles:?}"
        );
        assert_eq!(
            handles.iter().filter(|h| h.starts_with("friend:")).count(),
            1,
            "exactly one friend slot (only a) must be active; got {handles:?}"
        );

        // Idempotent: re-syncing the same map leaves exactly (a) published.
        sync_case_d_handles(&fp, &friends, &kt).await;
        let handles = publisher.active_handles().await;
        assert_eq!(
            handles.iter().filter(|h| h.starts_with("friend:")).count(),
            1,
            "re-sync must be idempotent; got {handles:?}"
        );
        assert!(handles.contains(&a_handle));

        // Flip (a) to Revoked and re-sync → its handle is dropped. (Re-seal
        // because encrypt_friend_secret moved the earlier blob into the map;
        // the secret bytes are what matter, not the specific ciphertext.)
        let a_resealed = encrypt_friend_secret(&kt, &a_owner.0, &a_secret).expect("re-seal a");
        friends.insert(a_owner, entry(FriendStatus::Revoked, Some(a_resealed)));
        sync_case_d_handles(&fp, &friends, &kt).await;
        let handles = publisher.active_handles().await;
        assert!(
            !handles.contains(&a_handle),
            "after flipping (a) to Revoked its slot must be gone; got {handles:?}"
        );
        assert_eq!(
            handles.iter().filter(|h| h.starts_with("friend:")).count(),
            0,
            "no friend slots should remain after revoking (a); got {handles:?}"
        );
    }
}
