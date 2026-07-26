//! ZEB-458 P4 Phase B: in-memory resolver for CommunityRelayAnnounce ads.
//! Fed by an event-loop hook on applied CommunityRelayAnnounce events
//! (mirrors `ReachabilityResolver`); read by senders (D40) and the pull
//! driver (D39). Freshness + per-community cap applied on READ.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::community_relay_announce::{
    fresh_relay_entry, CommunityRelayAnnouncePayload, CommunityRelayEntry,
    COMMUNITY_RELAY_ADVERTISERS_MAX,
};
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

#[derive(Default)]
pub struct CommunityRelayResolver {
    inner: Mutex<BTreeMap<(SpaceId, OwnerAddr), CommunityRelayAnnouncePayload>>,
}

impl CommunityRelayResolver {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    /// LWW by `ad_at`: a newer stamp replaces; older/equal is ignored.
    /// `_hlc` accepted for signature-parity with the reachability hook.
    pub fn update(
        &self,
        community_id: SpaceId,
        advertiser: OwnerAddr,
        payload: CommunityRelayAnnouncePayload,
        _hlc: Hlc,
    ) {
        let mut g = self.inner.lock().unwrap();
        let k = (community_id, advertiser);
        match g.get(&k) {
            Some(existing) if existing.ad_at >= payload.ad_at => {}
            _ => {
                g.insert(k, payload);
            }
        }
    }

    /// Fresh, capped advertiser entries for a community (freshest first).
    ///
    /// ZEB-806: the returned set may include THIS node's OWN advertisement —
    /// deliberately unfiltered, because the two existing callers need self
    /// present for different things: the deposit client orders self LAST and
    /// falls back to a local self-hold write (ZEB-524), and the pull driver
    /// drains self through the local acceptor pipeline instead of dialing
    /// (iroh rejects self-connections). A new caller must decide what "self"
    /// means for it — never dial an entry whose
    /// `relay_device_ed25519_verify` equals this device's verify key.
    pub fn relays_for_community(
        &self,
        community_id: &SpaceId,
        now_ms: u64,
    ) -> Vec<CommunityRelayEntry> {
        let g = self.inner.lock().unwrap();
        let mut fresh: Vec<(u64, CommunityRelayEntry)> = g
            .iter()
            .filter(|((c, _), _)| c == community_id)
            .filter_map(|(_, p)| fresh_relay_entry(p, now_ms).map(|e| (p.ad_at, e)))
            .collect();
        fresh.sort_by(|a, b| b.0.cmp(&a.0));
        fresh.truncate(COMMUNITY_RELAY_ADVERTISERS_MAX);
        fresh.into_iter().map(|(_, e)| e).collect()
    }

    /// The owner addresses of the FRESH relay advertisers for a community, the
    /// input `slot_for_advertiser` ranks into rendezvous slots. Mirrors the
    /// freshness filtering of [`Self::relays_for_community`] but returns the
    /// advertiser identities (the `BTreeMap` key's `OwnerAddr`), which
    /// `relays_for_community`'s `CommunityRelayEntry` does not expose.
    ///
    /// NOTE: unlike `relays_for_community` this does NOT apply the
    /// `COMMUNITY_RELAY_ADVERTISERS_MAX` read cap — the rendezvous slot
    /// assignment (`slot_for_advertiser`) ranks the full fresh advertiser set
    /// and bounds itself by `RENDEZVOUS_SLOT_COUNT`, so capping here would drop
    /// advertisers that legitimately claim a slot.
    pub fn advertiser_addrs_for_community(
        &self,
        community_id: &SpaceId,
        now_ms: u64,
    ) -> Vec<OwnerAddr> {
        let g = self.inner.lock().unwrap();
        g.iter()
            .filter(|((c, _), _)| c == community_id)
            .filter(|(_, p)| fresh_relay_entry(p, now_ms).is_some())
            .map(|((_, a), _)| *a)
            .collect()
    }

    /// Drop all ads from an advertiser in a community (opt-out / leave). Returns count removed.
    pub fn remove_advertiser(&self, community_id: &SpaceId, advertiser: &OwnerAddr) -> usize {
        let mut g = self.inner.lock().unwrap();
        let before = g.len();
        g.retain(|(c, a), _| !(c == community_id && a == advertiser));
        before - g.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_relay_announce::{CommunityRelayAnnouncePayload, CommunityRelayEntry};
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

    fn hlc(ms: u64) -> Hlc {
        Hlc {
            wall_ms: ms,
            logical: 0,
            device_id: "d".into(),
        }
    }
    fn payload(seed: u8, ad_at: u64) -> CommunityRelayAnnouncePayload {
        CommunityRelayAnnouncePayload {
            relay: CommunityRelayEntry {
                relay_device_id: [seed; 16],
                iroh_endpoint_id: [seed; 32],
                relay_device_ed25519_verify: [seed; 32],
                home_relay: "https://r/".into(),
            },
            ad_at,
            identity_signature: [0; 64],
        }
    }

    #[test]
    fn update_then_read_returns_fresh_entries() {
        let r = CommunityRelayResolver::new();
        let c = SpaceId([0xCC; 16]);
        let now = 1_700_000_000_000;
        r.update(c, OwnerAddr([0x01; 16]), payload(0x01, now), hlc(now));
        let got = r.relays_for_community(&c, now);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].relay_device_id, [0x01; 16]);
    }

    #[test]
    fn newer_ad_at_replaces_older_for_same_advertiser() {
        let r = CommunityRelayResolver::new();
        let c = SpaceId([0xCC; 16]);
        let a = OwnerAddr([0x01; 16]);
        let now = 1_700_000_000_000;
        r.update(c, a, payload(0x01, now), hlc(now));
        r.update(c, a, payload(0x02, now + 1), hlc(now + 1));
        r.update(c, a, payload(0x03, now - 1), hlc(now - 1));
        let got = r.relays_for_community(&c, now + 1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].relay_device_id, [0x02; 16]);
    }

    #[test]
    fn stale_entries_filtered_on_read() {
        let r = CommunityRelayResolver::new();
        let c = SpaceId([0xCC; 16]);
        let old = 1_000_000_000_000;
        r.update(c, OwnerAddr([0x01; 16]), payload(0x01, old), hlc(old));
        let way_later = old + crate::community_relay_announce::COMMUNITY_RELAY_AD_FRESHNESS_MS + 1;
        assert!(r.relays_for_community(&c, way_later).is_empty());
    }

    #[test]
    fn read_caps_to_max_advertisers_keeping_freshest() {
        let r = CommunityRelayResolver::new();
        let c = SpaceId([0xCC; 16]);
        let now = 1_700_000_000_000;
        for i in 0..(crate::community_relay_announce::COMMUNITY_RELAY_ADVERTISERS_MAX as u8 + 2) {
            r.update(
                c,
                OwnerAddr([i; 16]),
                payload(i, now + i as u64),
                hlc(now + i as u64),
            );
        }
        let got = r.relays_for_community(&c, now + 100);
        assert_eq!(
            got.len(),
            crate::community_relay_announce::COMMUNITY_RELAY_ADVERTISERS_MAX
        );
    }

    #[test]
    fn advertiser_addrs_for_community_returns_only_fresh_and_scopes_to_community() {
        let r = CommunityRelayResolver::new();
        let c = SpaceId([0xCC; 16]);
        let other = SpaceId([0xDD; 16]);
        let now = 1_700_000_000_000;
        let fresh_addr = OwnerAddr([0x01; 16]);
        let stale_addr = OwnerAddr([0x02; 16]);
        let other_addr = OwnerAddr([0x03; 16]);
        // Fresh advertiser in `c`.
        r.update(c, fresh_addr, payload(0x01, now), hlc(now));
        // Stale advertiser in `c` (ad_at older than the freshness window).
        let stale_at = now - crate::community_relay_announce::COMMUNITY_RELAY_AD_FRESHNESS_MS - 1;
        r.update(c, stale_addr, payload(0x02, stale_at), hlc(stale_at));
        // Fresh advertiser, but in a DIFFERENT community.
        r.update(other, other_addr, payload(0x03, now), hlc(now));

        let got = r.advertiser_addrs_for_community(&c, now);
        assert_eq!(got, vec![fresh_addr], "only the fresh advertiser of `c`");
        assert!(
            !got.contains(&stale_addr),
            "stale advertiser must be filtered out"
        );
        assert!(
            !got.contains(&other_addr),
            "another community's advertiser must be excluded"
        );
    }

    #[test]
    fn remove_advertiser_drops_its_entries() {
        let r = CommunityRelayResolver::new();
        let c = SpaceId([0xCC; 16]);
        let a = OwnerAddr([0x01; 16]);
        let now = 1_700_000_000_000;
        r.update(c, a, payload(0x01, now), hlc(now));
        assert_eq!(r.remove_advertiser(&c, &a), 1);
        assert!(r.relays_for_community(&c, now).is_empty());
    }
}
