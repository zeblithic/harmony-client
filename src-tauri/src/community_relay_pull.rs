//! ZEB-458 Phase A Task 5: recipient open-and-ingest core for community relay
//! blobs.
//!
//! After the recipient pulls [`crate::community_relay::RelayHeldBlob`]s from a
//! relay (Phase A Tasks 2-4), each blob is sealed opaquely to one of the
//! recipient's enrolled device keys.  This module provides the pure
//! open+ingest unit:
//!
//! 1. Derive the [`harmony_content::cid::ContentId`] of the sealed blob (to
//!    produce the relay ack list later).
//! 2. Try opening the blob with every device X25519 private key the owner
//!    possesses; if none opens it, skip (wrong owner/device).
//! 3. Decode the plaintext as a [`crate::butler_deposit::DepositPayload`].
//! 4. Delegate to [`RelayIngestCtx::ingest_recovered`] — the seam that
//!    production (a LATER task) wires to the existing DM receive path
//!    (`verify_cidnotify_admission`, `decrypt_and_bind_dm_blob`, `apply_inbox`,
//!    `emit_dm_received`).
//!
//! The background polling driver is a later phase; this module is the pure
//! open+ingest unit only.

use async_trait::async_trait;
use harmony_content::cid::{ContentFlags, ContentId};
use zeroize::Zeroizing;

use crate::butler_deposit::{decode_deposit_payload, DepositPayload};
use crate::community_relay::{RelayHeldBlob, COMMUNITY_RELAY_SEAL_INFO};
use crate::dm_signing::open_from_owner_with_info;

/// Injectable context for [`open_and_ingest`]: provides per-device X25519
/// private keys and the normal-receive-path ingest seam.
///
/// Production (a LATER task) implements this over `NodeState` handles, wiring
/// `ingest_recovered` to the existing DM inbox primitives:
/// `verify_cidnotify_admission`, `decrypt_and_bind_dm_blob`, `apply_inbox`,
/// `emit_dm_received`. Tests mock it with a simple probe struct.
#[async_trait]
pub trait RelayIngestCtx: Send + Sync {
    /// X25519 private keys for each of this owner's enrolled devices (to
    /// try-open each blob). In practice a single device holds one key, but the
    /// trait models ≤N keys for multi-device owners.
    fn device_x25519_privs(&self) -> Vec<Zeroizing<[u8; 32]>>;

    /// Ingest a recovered [`DepositPayload`] via the NORMAL receive path
    /// (verify_cidnotify_admission, decrypt_and_bind_dm_blob, apply_inbox,
    /// emit_dm_received). `sender_owner` is the AUTHENTICATED depositor the
    /// relay recorded ([`RelayHeldBlob::sender_owner`]); the ZEB-505 invite-only
    /// branch binds the invite's `expected_inviter` to it (the payload's own
    /// `signed.inviter` is untrusted). Returns `Ok(())` on success; `Err(reason)`
    /// causes the blob to be dropped (content_id NOT returned to the relay).
    async fn ingest_recovered(
        &self,
        sender_owner: [u8; 16],
        payload: DepositPayload,
    ) -> Result<(), String>;
}

/// Open each blob in `blobs` (trying every device priv from
/// [`RelayIngestCtx::device_x25519_privs`] against
/// [`COMMUNITY_RELAY_SEAL_INFO`]); on success decode the [`DepositPayload`]
/// and ingest it via [`RelayIngestCtx::ingest_recovered`].
///
/// Returns the `content_id` bytes of every blob that was successfully opened
/// **and** successfully ingested.  These are the blobs the recipient should
/// acknowledge back to the relay (the relay may GC them).
///
/// Blobs that cannot be opened (sealed to a different owner/device), fail to
/// decode, or whose ingest returns `Err` are silently skipped — they do NOT
/// produce a content_id in the return value.  Failed blobs do not abort
/// processing of subsequent blobs.
pub async fn open_and_ingest(blobs: &[RelayHeldBlob], ctx: &dyn RelayIngestCtx) -> Vec<[u8; 32]> {
    let device_privs = ctx.device_x25519_privs();
    let mut acked = Vec::new();

    for blob in blobs {
        // 1. Compute the content_id of the sealed blob — needed for the ack
        //    list even before we know whether we can open it.  Skip on error
        //    (malformed blob); the relay can GC it on TTL expiry.
        let content_id = match ContentId::for_book(
            &blob.sealed_blob,
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        ) {
            Ok(cid) => cid,
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    "community_relay_pull: for_book failed on sealed blob, skipping"
                );
                continue;
            }
        };

        // 2. Try opening: iterate over enrolled device privs and use the
        //    first one that succeeds.  If none opens it, the blob was sealed
        //    to a different owner/device — not ours, skip it silently.
        let plaintext = {
            let mut opened: Option<Vec<u8>> = None;
            for priv_key in &device_privs {
                match open_from_owner_with_info(
                    priv_key,
                    &blob.sealed_blob,
                    COMMUNITY_RELAY_SEAL_INFO,
                ) {
                    Ok(pt) => {
                        opened = Some(pt);
                        break;
                    }
                    Err(_) => continue,
                }
            }
            match opened {
                Some(pt) => pt,
                None => {
                    tracing::debug!(
                        "community_relay_pull: sealed blob could not be opened by any \
                         enrolled device key; blob sealed to a different owner/device"
                    );
                    continue;
                }
            }
        };

        // 3. Decode the plaintext as a DepositPayload.
        let payload = match decode_deposit_payload(&plaintext) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "community_relay_pull: decode_deposit_payload failed; skipping blob"
                );
                continue;
            }
        };

        // 4. Ingest via the normal receive path.  Only ack on success.
        //    Pass the relay-authenticated sender so the ZEB-505 invite-only
        //    branch binds the invite to it (not the payload's own claim).
        match ctx.ingest_recovered(blob.sender_owner, payload).await {
            Ok(()) => {
                acked.push(content_id.to_bytes());
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "community_relay_pull: ingest_recovered failed; blob dropped (not acked)"
                );
            }
        }
    }

    acked
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use crate::community_membership::mint_test_owner;
    use crate::community_relay::build_relay_deposit_frame;
    use crate::dm_signing::ed25519_priv_to_x25519;
    use crate::owner_state_types::SpaceId;

    // ------------------------------------------------------------------
    // Probe mock for RelayIngestCtx
    // ------------------------------------------------------------------

    /// Test mock for [`RelayIngestCtx`]:
    /// * holds a set of device X25519 privs (the ones "we" own);
    /// * records every [`DepositPayload`] passed to `ingest_recovered`;
    /// * returns a configurable `Ok(())`/`Err` result from `ingest_recovered`.
    struct MockCtx {
        /// X25519 privs for the enrolled devices.
        privs: Vec<Zeroizing<[u8; 32]>>,
        /// `true` → `ingest_recovered` returns `Ok(())`; `false` → `Err`.
        ingest_ok: bool,
        /// `(sender_owner, payload)` recorded by `ingest_recovered`, in call
        /// order — the sender lets a test assert `open_and_ingest` threads the
        /// relay-authenticated `blob.sender_owner` (ZEB-505 / Qodo).
        ingested: StdMutex<Vec<([u8; 16], DepositPayload)>>,
    }

    impl MockCtx {
        fn new(privs: Vec<Zeroizing<[u8; 32]>>, ingest_ok: bool) -> Self {
            Self {
                privs,
                ingest_ok,
                ingested: StdMutex::new(Vec::new()),
            }
        }

        fn ingested(&self) -> Vec<([u8; 16], DepositPayload)> {
            self.ingested.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl RelayIngestCtx for MockCtx {
        fn device_x25519_privs(&self) -> Vec<Zeroizing<[u8; 32]>> {
            self.privs.clone()
        }

        async fn ingest_recovered(
            &self,
            sender_owner: [u8; 16],
            payload: DepositPayload,
        ) -> Result<(), String> {
            self.ingested.lock().unwrap().push((sender_owner, payload));
            if self.ingest_ok {
                Ok(())
            } else {
                Err("simulated ingest failure".into())
            }
        }
    }

    // ------------------------------------------------------------------
    // Helpers: build a RelayHeldBlob sealed to a specific device VK
    // ------------------------------------------------------------------

    fn dummy_payload(tag: u8) -> DepositPayload {
        DepositPayload {
            cidnotify_packet: Some(vec![tag; 4]),
            storage_blob: vec![tag + 1; 5],
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
        }
    }

    /// Build a `RelayHeldBlob` by constructing a full `RelayDepositFrame`
    /// (which does the sealing) and extracting its `sealed_blob`.
    fn make_blob_for_device(recipient_ed_vk: &[u8; 32], payload: &DepositPayload) -> RelayHeldBlob {
        // Use a dummy sender.
        let sender = mint_test_owner(0xFE);
        let community_id = SpaceId([0xCC; 16]);
        let frame = build_relay_deposit_frame(
            [0x11; 16], // recipient_owner — not checked by open_and_ingest
            recipient_ed_vk,
            sender.owner.0,
            community_id,
            vec![0xDE],
            &sender.device_key,
            payload,
        )
        .expect("build_relay_deposit_frame must succeed");
        RelayHeldBlob {
            sender_owner: sender.owner.0,
            sealed_blob: frame.sealed_blob,
        }
    }

    // ------------------------------------------------------------------
    // Test 1: blob sealed to one of our devices is opened, decoded,
    //         ingested, and its content_id is returned.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn open_and_ingest_opens_own_blob_and_returns_content_id() {
        let recipient = mint_test_owner(0x01);
        let x_priv = ed25519_priv_to_x25519(&recipient.device_key);
        let payload = dummy_payload(0xAA);
        let blob = make_blob_for_device(
            &recipient.cert.device_pubkeys.classical.ed25519_verify,
            &payload,
        );

        // Compute expected content_id independently.
        let expected_cid = ContentId::for_book(
            &blob.sealed_blob,
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .expect("for_book must succeed")
        .to_bytes();

        let expected_sender = blob.sender_owner;
        let ctx = MockCtx::new(vec![x_priv], true);
        let acked = open_and_ingest(&[blob], &ctx).await;

        // The content_id of the opened blob is returned.
        assert_eq!(
            acked,
            vec![expected_cid],
            "content_id must be returned for a successfully ingested blob"
        );

        // ingest_recovered was called with the EXACT DepositPayload, and with
        // the blob's relay-authenticated sender_owner threaded through (ZEB-505).
        let ingested = ctx.ingested();
        assert_eq!(ingested.len(), 1, "ingest_recovered must be called once");
        assert_eq!(
            ingested[0].1, payload,
            "ingest_recovered must receive the exact decoded DepositPayload"
        );
        assert_eq!(
            ingested[0].0, expected_sender,
            "open_and_ingest must thread the blob's authenticated sender_owner"
        );
    }

    // ------------------------------------------------------------------
    // Test 2: blob sealed to a NON-owner device is skipped entirely.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn open_and_ingest_skips_foreign_blob() {
        let recipient = mint_test_owner(0x01);
        let other = mint_test_owner(0x02); // different device — we don't own its priv

        let recipient_priv = ed25519_priv_to_x25519(&recipient.device_key);
        let payload = dummy_payload(0xBB);

        // Blob sealed to `other`'s device — we only have `recipient`'s priv.
        let blob = make_blob_for_device(
            &other.cert.device_pubkeys.classical.ed25519_verify,
            &payload,
        );

        let ctx = MockCtx::new(vec![recipient_priv], true);
        let acked = open_and_ingest(&[blob], &ctx).await;

        assert!(acked.is_empty(), "foreign blob must not be acked");
        assert!(
            ctx.ingested().is_empty(),
            "ingest_recovered must NOT be called for a foreign blob"
        );
    }

    // ------------------------------------------------------------------
    // Test 3: ingest_recovered returning Err drops the blob (not acked)
    //         but does not abort other blobs.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn open_and_ingest_drops_on_ingest_err() {
        let recipient = mint_test_owner(0x01);
        let x_priv = ed25519_priv_to_x25519(&recipient.device_key);
        let payload = dummy_payload(0xCC);
        let blob = make_blob_for_device(
            &recipient.cert.device_pubkeys.classical.ed25519_verify,
            &payload,
        );

        // Mock returns Err from ingest_recovered.
        let ctx = MockCtx::new(vec![x_priv], false);
        let acked = open_and_ingest(&[blob], &ctx).await;

        assert!(
            acked.is_empty(),
            "content_id must NOT be returned when ingest_recovered fails"
        );
        // ingest_recovered WAS called (we opened it), but failed.
        assert_eq!(ctx.ingested().len(), 1);
    }

    // ------------------------------------------------------------------
    // Test 4: multiple blobs processed independently:
    //   blob A — openable, ingest Ok  → content_id returned
    //   blob B — foreign               → skipped
    //   blob C — openable, ingest Err → not returned
    //
    // Only blob A's content_id is returned.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn open_and_ingest_processes_blobs_independently() {
        let recipient = mint_test_owner(0x01);
        let other = mint_test_owner(0x02);

        let x_priv = ed25519_priv_to_x25519(&recipient.device_key);
        let vk = recipient.cert.device_pubkeys.classical.ed25519_verify;

        let payload_a = dummy_payload(0xA1);
        let payload_c = dummy_payload(0xC1);

        let blob_a = make_blob_for_device(&vk, &payload_a);
        let blob_b = make_blob_for_device(
            &other.cert.device_pubkeys.classical.ed25519_verify,
            &dummy_payload(0xB1),
        );
        let blob_c = make_blob_for_device(&vk, &payload_c);

        // Compute expected content_id for blob_a.
        let expected_cid_a = ContentId::for_book(
            &blob_a.sealed_blob,
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .expect("for_book")
        .to_bytes();

        // Use a ctx that returns Ok then Err alternately: we override by using
        // a counter-based mock.  Simpler: use two separate single-blob passes
        // with the same mock but different ingest_ok; instead, build a
        // BehaviorMockCtx that fails on the second call.
        let ctx = AlternatingCtx::new(vec![x_priv]);
        let acked = open_and_ingest(&[blob_a, blob_b, blob_c], &ctx).await;

        // Only blob_a's content_id is in the ack list.
        assert_eq!(acked, vec![expected_cid_a]);
        // ingest_recovered was called for blobs A and C (not B — foreign).
        assert_eq!(ctx.call_count(), 2);
    }

    /// A mock that returns Ok on the first `ingest_recovered` call, Err on
    /// all subsequent calls.
    struct AlternatingCtx {
        privs: Vec<Zeroizing<[u8; 32]>>,
        call_count: StdMutex<usize>,
    }

    impl AlternatingCtx {
        fn new(privs: Vec<Zeroizing<[u8; 32]>>) -> Self {
            Self {
                privs,
                call_count: StdMutex::new(0),
            }
        }

        fn call_count(&self) -> usize {
            *self.call_count.lock().unwrap()
        }
    }

    #[async_trait]
    impl RelayIngestCtx for AlternatingCtx {
        fn device_x25519_privs(&self) -> Vec<Zeroizing<[u8; 32]>> {
            self.privs.clone()
        }

        async fn ingest_recovered(
            &self,
            _sender_owner: [u8; 16],
            _payload: DepositPayload,
        ) -> Result<(), String> {
            let mut n = self.call_count.lock().unwrap();
            let result = if *n == 0 {
                Ok(())
            } else {
                Err("second call fails".into())
            };
            *n += 1;
            result
        }
    }

    // ------------------------------------------------------------------
    // Sanity: encode_deposit_payload + decode_deposit_payload round-trips
    // (confirms the sealing path produces a decodable plaintext).
    // ------------------------------------------------------------------

    #[test]
    fn relay_deposit_frame_plaintext_decodes_to_original_payload() {
        let recipient = mint_test_owner(0x10);
        let payload = dummy_payload(0xDD);

        let blob = make_blob_for_device(
            &recipient.cert.device_pubkeys.classical.ed25519_verify,
            &payload,
        );

        let x_priv = ed25519_priv_to_x25519(&recipient.device_key);
        let plaintext = crate::dm_signing::open_from_owner_with_info(
            &x_priv,
            &blob.sealed_blob,
            COMMUNITY_RELAY_SEAL_INFO,
        )
        .expect("should open");
        let decoded = decode_deposit_payload(&plaintext).expect("should decode");
        assert_eq!(decoded, payload);
    }
}
