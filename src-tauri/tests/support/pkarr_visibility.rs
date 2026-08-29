//! ZEB-1021: shared mock-relay visibility barrier for pkarr-publishing tests.
//!
//! Included via `#[path]` by the `misc_tests`, `dm_tests`, and
//! `pkarr_net_tests` umbrella binaries. Not every including binary uses every
//! item (misc uses only the generic barrier; dm/pkarr_net use the invite
//! wrapper), hence the module-level `allow(dead_code)`.
//!
//! The naive barrier this replaces — derive ONE current-epoch probe key, then
//! poll a single `PkarrResolver` 50×100ms — carried two independent flake
//! mechanisms, each able to hard-fail a test that would have passed moments
//! later:
//!
//! 1. **Epoch-boundary hard miss.** Ephemeral pkarr keys rotate on the weekly
//!    epoch (`current_epoch_id`, boundary every Thursday 00:00 UTC). A record
//!    published moments before the rollover sits under the PREVIOUS epoch's
//!    key; a barrier that derives only the current epoch's key polls a key the
//!    publisher never wrote, and no deadline helps. Production resolvers are
//!    immune — they always query the 3-key `epoch_tolerance_window` (see
//!    `pkarr_resolver_adapter`) — so the barrier now mirrors that window.
//!    (CI run 32989223004, the ZEB-1021 flake, executed across the boundary.)
//! 2. **Negative-cache single-shot.** `PkarrResolver` caches a miss for 60s —
//!    longer than the whole poll budget — so polling one resolver in a loop
//!    makes exactly ONE real relay query: a publish landing 200ms after the
//!    first poll still fails the barrier at the deadline. Rebuilding the
//!    (cheap, stateless-over-the-store) resolver each attempt makes every
//!    poll hit the relay.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::VerifyingKey;

/// Poll (≤10s) until a record is visible in the mock relay under ANY of
/// `probe_vks` (the caller's epoch tolerance window). Panics with `what` in
/// the message on deadline. Each attempt uses a fresh `PkarrResolver` so the
/// 60s negative cache can never mask a late publish (mechanism 2 above).
///
/// 10s rather than the historical 5s: headroom for scheduler jitter on the
/// loaded 4-vCPU CI runners (ZEB-1013 precedent). The loop exits on first
/// success, so the pass path never pays for it. The budget is a wall-clock
/// deadline (`tokio::time::timeout`), not an iteration count — a slow
/// `resolve_window` (each GET carries its own 5s relay timeout) counts
/// against it rather than silently stretching the barrier.
pub(crate) async fn await_record_visible_any(
    relay_client: &Arc<harmony_pkarr::RelayClient>,
    probe_vks: &[VerifyingKey],
    what: &str,
) {
    let poll = async {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let fresh = harmony_pkarr::PkarrResolver::new(Arc::clone(relay_client));
            if let Ok(Some(_)) = fresh.resolve_window(probe_vks).await {
                return;
            }
        }
    };
    if tokio::time::timeout(Duration::from_secs(10), poll)
        .await
        .is_err()
    {
        panic!("{what} did not appear in the mock relay within 10s (any window epoch)");
    }
}

/// Case-A invite probe keys for `token_sig`: one per epoch in the ±1
/// tolerance window, mirroring the production redeem resolve.
pub(crate) fn invite_probe_vks(token_sig: &[u8; 64]) -> Vec<VerifyingKey> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_millis() as u64;
    harmony_pkarr::epoch_tolerance_window(now_ms)
        .into_iter()
        .map(|epoch_id| {
            harmony_pkarr::derive_ephemeral_key(
                harmony_pkarr::PkarrCase::Invite,
                token_sig,
                &epoch_id.to_be_bytes(),
            )
            .verifying_key()
        })
        .collect()
}

/// Wait until Alice's Case-A pkarr record — keyed on the friend/invite
/// token's signature — is visible in the mock relay under any window epoch.
pub(crate) async fn await_invite_record_visible(
    relay_client: &Arc<harmony_pkarr::RelayClient>,
    token_sig: &[u8; 64],
) {
    let probe_vks = invite_probe_vks(token_sig);
    await_record_visible_any(
        relay_client,
        &probe_vks,
        "alice's friend-token pkarr record",
    )
    .await;
}
