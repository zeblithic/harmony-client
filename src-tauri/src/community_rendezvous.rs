//! Open-community cross-WAN first-contact: enumerated DHT rendezvous slots.
//!
//! A community-keyed rendezvous record lets a link-only joiner (holding the
//! community `epoch_key`) resolve a live serving "beacon" from the DHT. Slot
//! keys reuse `PkarrCase::Community` (its `ikm` contract IS the epoch key) with
//! a distinct, length-disjoint info-prefix so they can never collide with the
//! member-keyed records (`info = identity_pub(64) || epoch_id(8)`, 72 bytes).

use crate::community_relay_announce::COMMUNITY_RELAY_ADVERTISERS_MAX;
use crate::owner_state_types::EpochKey;
use crate::owner_state_types::OwnerAddr;
use crate::reachability_record::ReachabilityAnnouncePayload;
use harmony_pkarr::derive::{derive_ephemeral_key, PkarrCase};
use harmony_pkarr::epoch::epoch_tolerance_window;
use harmony_pkarr::PkarrResolver;
use std::sync::Arc;
use std::time::Duration;

/// Number of enumerated rendezvous slots == the relay-advertiser cap.
pub const RENDEZVOUS_SLOT_COUNT: usize = COMMUNITY_RELAY_ADVERTISERS_MAX;

/// Domain-separation prefix for rendezvous slot derivation. Length-disjoint
/// from the 72-byte member-keyed `info`, so rendezvous keys can never alias a
/// member's reachability key even though both reuse the `Community` salt.
pub const RENDEZVOUS_INFO_PREFIX: &[u8] = b"harmony.rendezvous.v1";

/// `info = RENDEZVOUS_INFO_PREFIX || slot_index_be(2) || epoch_id_be(8)`.
fn rendezvous_info(slot_index: u16, epoch_id: u64) -> Vec<u8> {
    let mut info = Vec::with_capacity(RENDEZVOUS_INFO_PREFIX.len() + 2 + 8);
    info.extend_from_slice(RENDEZVOUS_INFO_PREFIX);
    info.extend_from_slice(&slot_index.to_be_bytes());
    info.extend_from_slice(&epoch_id.to_be_bytes());
    info
}

pub fn rendezvous_slot_key(
    epoch_key: &EpochKey,
    slot_index: u16,
    epoch_id: u64,
) -> ed25519_dalek::SigningKey {
    let info = rendezvous_info(slot_index, epoch_id);
    derive_ephemeral_key(PkarrCase::Community, epoch_key.as_bytes(), &info)
}

pub fn rendezvous_slot_verifying_key(
    epoch_key: &EpochKey,
    slot_index: u16,
    epoch_id: u64,
) -> ed25519_dalek::VerifyingKey {
    rendezvous_slot_key(epoch_key, slot_index, epoch_id).verifying_key()
}

/// Deterministic slot claim: sort the advertiser set ascending by address; the
/// position of `me` is its slot index. `None` if `me` is absent or ranks at/
/// beyond the slot cap. Because the advertiser set is CRDT-replicated, every
/// member computes the same ordering, so each slot has exactly one writer.
pub fn slot_for_advertiser(advertisers: &[OwnerAddr], me: &OwnerAddr) -> Option<u16> {
    let mut sorted: Vec<OwnerAddr> = advertisers.to_vec();
    sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    sorted.dedup_by(|a, b| a.0 == b.0);
    let rank = sorted.iter().position(|a| a.0 == me.0)?;
    if rank >= RENDEZVOUS_SLOT_COUNT {
        return None;
    }
    Some(rank as u16)
}

/// Tunable widening schedule + per-batch deadline for the joiner's escalating
/// rendezvous resolve.
pub struct RendezvousResolveConfig {
    /// Widening curve of batch widths, e.g. `[1, 2, N]`: probe slot 0, then
    /// 0..1, then all. Tunable so the success-rate/latency trade can be set
    /// from data (the spec's open question).
    pub batch_curve: Vec<usize>,
    /// Per-batch resolve deadline. Reserved for the production resolver's
    /// network timeout; the pure driver records timing but never sleeps.
    pub per_batch_deadline: Duration,
}

impl Default for RendezvousResolveConfig {
    fn default() -> Self {
        Self {
            batch_curve: vec![1, 2, RENDEZVOUS_SLOT_COUNT],
            per_batch_deadline: Duration::from_millis(2_500),
        }
    }
}

impl RendezvousResolveConfig {
    /// Override from `HARMONY_OPEN_JOIN_RESOLVE_CURVE` (comma-separated batch
    /// widths, each clamped to `1..=RENDEZVOUS_SLOT_COUNT`) and
    /// `HARMONY_OPEN_JOIN_RESOLVE_DEADLINE_MS` (clamped `>= 1`).
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(curve) = std::env::var("HARMONY_OPEN_JOIN_RESOLVE_CURVE") {
            let parsed: Vec<usize> = curve
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .filter(|w| *w >= 1 && *w <= RENDEZVOUS_SLOT_COUNT)
                .collect();
            if !parsed.is_empty() {
                cfg.batch_curve = parsed;
            }
        }
        if let Ok(ms) = std::env::var("HARMONY_OPEN_JOIN_RESOLVE_DEADLINE_MS") {
            if let Ok(ms) = ms.parse::<u64>() {
                cfg.per_batch_deadline = Duration::from_millis(ms.max(1));
            }
        }
        cfg
    }
}

/// Result of a joiner's escalating-batch resolve, carrying the instrumentation
/// the spec's tuning open-question needs: which slot answered, how long it took,
/// and how many widening batches were probed.
#[derive(Debug, Default)]
pub struct RendezvousResolveOutcome {
    pub payload: Option<ReachabilityAnnouncePayload>,
    pub winning_slot: Option<u16>,
    pub elapsed_ms: u64,
    pub batches_tried: usize,
}

/// Abstraction over "probe one rendezvous slot at one epoch". The production
/// impl ([`PkarrSlotResolver`]) derives the slot verifying-key and queries
/// pkarr; tests inject a deterministic stub. Returns `Some` only for a live,
/// freshness-valid record.
#[async_trait::async_trait]
pub trait SlotResolver {
    async fn resolve_slot(
        &self,
        slot_index: u16,
        epoch_id: u64,
    ) -> Option<ReachabilityAnnouncePayload>;
}

/// Escalating-batch rendezvous resolve over any [`SlotResolver`] (`now_ms` is
/// supplied so the driver itself stays clock-free; the only I/O is the per-batch
/// deadline). For each width `w` in `cfg.batch_curve`, probe slots `0..w` across
/// the epoch-tolerance window CONCURRENTLY and return on the FIRST live record —
/// the first beacon to respond wins (not strictly the lowest slot), so one
/// hung/slow probe can never stall discovery. Each batch is bounded by
/// `cfg.per_batch_deadline`: on the deadline elapsing OR all probes returning
/// `None`, widen to the next batch width. Records `winning_slot` +
/// `batches_tried` + `elapsed_ms`. Returns an empty outcome (cold start) if no
/// slot answers across all batches.
pub async fn resolve_rendezvous_with<R: SlotResolver + Sync>(
    resolver: &R,
    now_ms: u64,
    cfg: &RendezvousResolveConfig,
) -> RendezvousResolveOutcome {
    use futures::stream::{FuturesUnordered, StreamExt};

    let started = std::time::Instant::now();
    let epoch_window = epoch_tolerance_window(now_ms);
    let mut outcome = RendezvousResolveOutcome::default();

    for &width in &cfg.batch_curve {
        outcome.batches_tried += 1;
        let capped = width.min(RENDEZVOUS_SLOT_COUNT);
        // Probe every (slot, epoch) pair in this batch concurrently, draining
        // them as they complete so the FIRST live beacon wins without waiting
        // on slower/hung probes. Bounded by the per-batch deadline.
        let mut probes: FuturesUnordered<_> = (0..capped as u16)
            .flat_map(|slot| {
                epoch_window.iter().map(move |&epoch_id| async move {
                    resolver
                        .resolve_slot(slot, epoch_id)
                        .await
                        .map(|payload| (slot, payload))
                })
            })
            .collect();

        let winner = tokio::time::timeout(cfg.per_batch_deadline, async {
            while let Some(result) = probes.next().await {
                if let Some((slot, payload)) = result {
                    return Some((slot, payload));
                }
            }
            None
        })
        .await
        // On the batch deadline elapsing (Err), treat the batch as exhausted
        // and widen to the next width rather than hanging.
        .unwrap_or(None);

        if let Some((slot, payload)) = winner {
            outcome.winning_slot = Some(slot);
            outcome.payload = Some(payload);
            outcome.elapsed_ms = started.elapsed().as_millis() as u64;
            tracing::info!(
                winning_slot = slot,
                elapsed_ms = outcome.elapsed_ms,
                batches_tried = outcome.batches_tried,
                "open-join rendezvous resolved (tuning metric)"
            );
            return outcome;
        }
    }

    outcome.elapsed_ms = started.elapsed().as_millis() as u64;
    tracing::info!(
        winning_slot = tracing::field::Empty,
        elapsed_ms = outcome.elapsed_ms,
        batches_tried = outcome.batches_tried,
        "open-join rendezvous resolve found no live beacon (cold start)"
    );
    outcome
}

/// Production [`SlotResolver`]: derives the slot verifying-key from `epoch_key`,
/// queries pkarr, checks freshness, and decodes the routing blob into a
/// [`ReachabilityAnnouncePayload`]. The BEP44 envelope already proves the writer
/// held `epoch_key`, so the inner identity signature is intentionally NOT
/// verified here — the joiner does not know the beacon's identity; trust is
/// established at the handshake/admission layer.
pub struct PkarrSlotResolver {
    pub pkarr: Arc<PkarrResolver>,
    pub epoch_key: EpochKey,
}

#[async_trait::async_trait]
impl SlotResolver for PkarrSlotResolver {
    async fn resolve_slot(
        &self,
        slot_index: u16,
        epoch_id: u64,
    ) -> Option<ReachabilityAnnouncePayload> {
        let vk = rendezvous_slot_verifying_key(&self.epoch_key, slot_index, epoch_id);
        let rec = self.pkarr.resolve(&vk).await.ok()??;
        // Re-sample the wall clock AFTER the awaited resolve so freshness is
        // checked against "now", not a timestamp captured before a possibly
        // long network round-trip (the stale-clock bug fixed in PR#306).
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        rec.verify_freshness(now_ms).ok()?;
        ciborium::from_reader::<ReachabilityAnnouncePayload, _>(rec.routing_blob.as_slice()).ok()
    }
}

/// Production entry point: build a [`PkarrSlotResolver`] over a live pkarr
/// resolver + the community `epoch_key`, then run the escalating-batch resolve.
pub async fn resolve_rendezvous(
    pkarr: &Arc<PkarrResolver>,
    epoch_key: &EpochKey,
    now_ms: u64,
    cfg: &RendezvousResolveConfig,
) -> RendezvousResolveOutcome {
    let resolver = PkarrSlotResolver {
        pkarr: Arc::clone(pkarr),
        epoch_key: epoch_key.clone(),
    };
    resolve_rendezvous_with(&resolver, now_ms, cfg).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ek() -> EpochKey {
        EpochKey::new([7u8; 32])
    }

    #[test]
    fn slot_key_is_deterministic() {
        let a = rendezvous_slot_verifying_key(&ek(), 0, 42);
        let b = rendezvous_slot_verifying_key(&ek(), 0, 42);
        assert_eq!(a.to_bytes(), b.to_bytes());
    }

    #[test]
    fn distinct_slots_and_epochs_give_distinct_keys() {
        let s0 = rendezvous_slot_verifying_key(&ek(), 0, 42).to_bytes();
        let s1 = rendezvous_slot_verifying_key(&ek(), 1, 42).to_bytes();
        let s0_next = rendezvous_slot_verifying_key(&ek(), 0, 43).to_bytes();
        assert_ne!(s0, s1, "different slot index must yield a different key");
        assert_ne!(s0, s0_next, "different epoch must yield a different key");
    }

    #[test]
    fn rendezvous_key_is_disjoint_from_member_keyed_record() {
        // Member-keyed info is identity_pub(64) || epoch_id(8) = 72 bytes under
        // the SAME Community salt. Reconstruct it and confirm no rendezvous slot
        // collides with it.
        let epoch_id = 42u64;
        let identity_pub = [9u8; 64];
        let mut member_info = Vec::with_capacity(72);
        member_info.extend_from_slice(&identity_pub);
        member_info.extend_from_slice(&epoch_id.to_be_bytes());
        let member_key = derive_ephemeral_key(PkarrCase::Community, ek().as_bytes(), &member_info)
            .verifying_key()
            .to_bytes();
        for slot in 0..RENDEZVOUS_SLOT_COUNT as u16 {
            let rk = rendezvous_slot_verifying_key(&ek(), slot, epoch_id).to_bytes();
            assert_ne!(rk, member_key, "slot {slot} aliased a member key");
        }
    }

    #[test]
    fn slot_count_tracks_advertiser_cap() {
        assert_eq!(RENDEZVOUS_SLOT_COUNT, COMMUNITY_RELAY_ADVERTISERS_MAX);
    }

    fn addr(b: u8) -> OwnerAddr {
        OwnerAddr([b; 16])
    }

    #[test]
    fn slot_assignment_is_deterministic_across_members() {
        // Two members compute the SAME ordering from the same (unordered) set.
        let set_a = vec![addr(3), addr(1), addr(2)];
        let set_b = vec![addr(2), addr(3), addr(1)];
        for who in [addr(1), addr(2), addr(3)] {
            assert_eq!(
                slot_for_advertiser(&set_a, &who),
                slot_for_advertiser(&set_b, &who),
                "ordering disagreed for {who:?}"
            );
        }
        // Sorted ascending: addr(1)->0, addr(2)->1, addr(3)->2.
        assert_eq!(slot_for_advertiser(&set_a, &addr(1)), Some(0));
        assert_eq!(slot_for_advertiser(&set_a, &addr(2)), Some(1));
        assert_eq!(slot_for_advertiser(&set_a, &addr(3)), Some(2));
    }

    #[test]
    fn not_in_set_returns_none() {
        let set = vec![addr(1), addr(2)];
        assert_eq!(slot_for_advertiser(&set, &addr(9)), None);
    }

    #[test]
    fn rank_beyond_cap_returns_none() {
        // RENDEZVOUS_SLOT_COUNT (=4) advertisers fill slots 0..3; a 5th (highest
        // address) ranks 4 >= cap and claims no slot.
        let set: Vec<OwnerAddr> = (1..=(RENDEZVOUS_SLOT_COUNT as u8 + 1)).map(addr).collect();
        let highest = addr(RENDEZVOUS_SLOT_COUNT as u8 + 1);
        assert_eq!(slot_for_advertiser(&set, &highest), None);
        // The one just under the cap still gets the last valid slot.
        let last_valid = addr(RENDEZVOUS_SLOT_COUNT as u8);
        assert_eq!(
            slot_for_advertiser(&set, &last_valid),
            Some((RENDEZVOUS_SLOT_COUNT - 1) as u16)
        );
    }

    #[test]
    fn duplicate_addresses_do_not_shift_ranks() {
        // Defensive: a duplicated advertiser must not change anyone's slot.
        let set = vec![addr(1), addr(2), addr(2), addr(3)];
        assert_eq!(slot_for_advertiser(&set, &addr(1)), Some(0));
        assert_eq!(slot_for_advertiser(&set, &addr(2)), Some(1));
        assert_eq!(slot_for_advertiser(&set, &addr(3)), Some(2));
    }

    /// Deterministic `SlotResolver`: answers only for a single configured live
    /// slot (or never). Ignores `epoch_id` (the escalating-batch logic is what's
    /// under test, not epoch derivation).
    struct StubResolver {
        live_slot: Option<u16>,
    }

    impl StubResolver {
        fn with_live_slot(slot: u16) -> Self {
            Self {
                live_slot: Some(slot),
            }
        }
        fn all_dead() -> Self {
            Self { live_slot: None }
        }
    }

    fn dummy_payload() -> ReachabilityAnnouncePayload {
        ReachabilityAnnouncePayload {
            iroh_node_id: [1u8; 32],
            home_relay_url: "https://relay.example/".into(),
            direct_addresses: Vec::new(),
            announced_at_ms: 1_000_000,
            identity_signature: [0u8; 64],
            butler_set: Vec::new(),
            bs_at: 0,
        }
    }

    #[async_trait::async_trait]
    impl SlotResolver for StubResolver {
        async fn resolve_slot(
            &self,
            slot_index: u16,
            _epoch_id: u64,
        ) -> Option<ReachabilityAnnouncePayload> {
            if Some(slot_index) == self.live_slot {
                Some(dummy_payload())
            } else {
                None
            }
        }
    }

    #[tokio::test]
    async fn returns_slot0_without_widening_when_slot0_is_live() {
        let stub = StubResolver::with_live_slot(0);
        let cfg = RendezvousResolveConfig::default();
        let out = resolve_rendezvous_with(&stub, 1_000_000, &cfg).await;
        assert_eq!(out.winning_slot, Some(0));
        assert_eq!(
            out.batches_tried, 1,
            "should not widen past the first batch"
        );
        assert!(out.payload.is_some());
    }

    #[tokio::test]
    async fn widens_to_find_a_live_slot_when_slot0_is_dead() {
        let stub = StubResolver::with_live_slot(2); // only slot 2 answers
        let cfg = RendezvousResolveConfig::default(); // curve [1, 2, N]
        let out = resolve_rendezvous_with(&stub, 1_000_000, &cfg).await;
        assert_eq!(out.winning_slot, Some(2));
        assert!(out.batches_tried >= 3, "had to widen to the full set");
        assert!(out.payload.is_some());
    }

    #[tokio::test]
    async fn cold_start_returns_none() {
        let stub = StubResolver::all_dead();
        let cfg = RendezvousResolveConfig::default();
        let out = resolve_rendezvous_with(&stub, 1_000_000, &cfg).await;
        assert_eq!(out.payload, None);
        assert_eq!(out.winning_slot, None);
    }

    /// `SlotResolver` whose slot 0 NEVER completes (hangs) but whose slot 1
    /// answers live. Proves the resolve returns the live higher slot via the
    /// first-responding-beacon path and never blocks on the hung probe.
    struct HungSlot0Resolver;

    #[async_trait::async_trait]
    impl SlotResolver for HungSlot0Resolver {
        async fn resolve_slot(
            &self,
            slot_index: u16,
            _epoch_id: u64,
        ) -> Option<ReachabilityAnnouncePayload> {
            if slot_index == 0 {
                // Never completes — models a hung/dropped DHT probe.
                std::future::pending::<()>().await;
                unreachable!("pending() never resolves");
            }
            if slot_index == 1 {
                Some(dummy_payload())
            } else {
                None
            }
        }
    }

    #[tokio::test]
    async fn hung_probe_does_not_block_a_live_higher_slot() {
        let resolver = HungSlot0Resolver;
        let cfg = RendezvousResolveConfig::default(); // curve [1, 2, N], 2500ms
                                                      // Wrap in a generous outer timeout: if the resolve hung on slot 0 this
                                                      // would elapse; instead the live slot-1 beacon answers in batch 2.
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            resolve_rendezvous_with(&resolver, 1_000_000, &cfg),
        )
        .await
        .expect("resolve must not hang on a stuck slot-0 probe");
        assert_eq!(out.winning_slot, Some(1));
        assert!(out.payload.is_some());
    }
}
