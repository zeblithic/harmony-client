//! ZEB-371 Phase 1b: Case-D publisher. Publishes this node's iroh reachability
//! under a per-friend, secret-derived pkarr slot so each active friend can
//! resolve us across WAN without any global discoverability. One registered
//! handle per active friend; mirrors `PkarrIdentityPublisher`.

use crate::friend_rendezvous::{
    case_d_publish_key, case_d_resolve_key, open_case_d_payload, seal_case_d_payload,
};
use harmony_pkarr::{
    current_epoch_id, epoch_tolerance_window, EphemeralKeyBuilder, PkarrPublisher, PkarrResolver,
    PkarrRoutingRecord, RecordBuilder,
};
use std::sync::Arc;

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
    pub async fn register_friend(&self, friend_owner: [u8; 16], secret: [u8; 32]) {
        let self_owner = self.self_owner;
        let key_builder: EphemeralKeyBuilder = Arc::new(move |at_ms| {
            case_d_publish_key(&secret, current_epoch_id(at_ms), &self_owner)
        });
        let blob_builder = Arc::clone(&self.routing_blob_builder);
        let builder: RecordBuilder = Arc::new(move |at_ms| {
            let epoch = current_epoch_id(at_ms);
            let cd_key = case_d_publish_key(&secret, epoch, &self_owner);
            let mut id_pub = [0u8; 64];
            id_pub[32..].copy_from_slice(&cd_key.verifying_key().to_bytes());
            let sealed =
                seal_case_d_payload(&secret, epoch, &blob_builder()).expect("case-d payload seal");
            PkarrRoutingRecord::sign_new(sealed, id_pub, at_ms, &cd_key)
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

/// Resolve `friend_owner`'s current Case-D routing blob (UNSEALED) using the
/// shared `secret`. Queries the ±1 epoch tolerance window in parallel; on a hit,
/// tries each window epoch to unseal (the record could be from any of the three).
/// Returns `Ok(None)` if no record is found OR a record is found but cannot be
/// unsealed (wrong secret/epoch — treated as a miss, not an error).
pub async fn resolve_friend_case_d(
    resolver: &PkarrResolver,
    secret: &[u8; 32],
    friend_owner: &[u8; 16],
) -> Result<Option<Vec<u8>>, String> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let window = epoch_tolerance_window(now_ms);
    let keys: Vec<_> = window
        .iter()
        .map(|&e| case_d_resolve_key(secret, e, friend_owner).verifying_key())
        .collect();
    let Some(rec) = resolver
        .resolve_window(&keys)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    for &e in &window {
        if let Ok(blob) = open_case_d_payload(secret, e, &rec.routing_blob) {
            return Ok(Some(blob));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_pkarr::testing::MockPkarrRelay;
    use harmony_pkarr::{PkarrPublisher, PkarrResolver, RelayClient, RelayPool};
    use std::sync::Arc;

    #[tokio::test]
    async fn case_d_publish_then_resolve_round_trip() {
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(Arc::clone(&client)));
        let _ph = Arc::clone(&publisher).spawn();
        let resolver = PkarrResolver::new(Arc::clone(&client));

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
        fp.register_friend([0xBB; 16], secret).await;

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
        fp.register_friend(friend, secret).await;
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
}
