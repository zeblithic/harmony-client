//! ZEB-286: VineFeedCache — Rust-side state surface for the Vine feed.
//!
//! Cache is updated by `event_loop::emit_frontend_event` on receive (one
//! cache instance per NodeState; shared with the event loop via
//! `Arc<Mutex<VineFeedCache>>`). Read by the `list_vine_videos()` and
//! `mark_vine_viewed()` Tauri IPCs.
//!
//! In-memory only in this PR — disk persistence is deferred to ZEB-147.
//!
//! See `docs/specs/2026-05-13-zeb-286-vine-integration-test-design.md`.

use crate::{VineDescriptorPayload, VineReactionPayload, VineVideoDto};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// How the recipient discovered this vine. Followed = creator is in the
/// local follow set at the time of first arrival; Discover = otherwise.
/// Decided ONCE at first insert; subsequent re-arrivals do not change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VineSource {
    Followed,
    Discover,
}

/// Outcome of `on_descriptor_sample`. `Inserted` carries the DTO ready
/// for the frontend `vine-received` emit so the caller does not have
/// to re-walk the cache.
#[derive(Debug, Clone, PartialEq)]
pub enum DescriptorOutcome {
    Inserted { dto: VineVideoDtoWithSource },
    AlreadyPresent,
    Rejected(String),
}

/// Outcome of `on_reaction_sample`. The receive path re-emits to the
/// frontend only on `Inserted` or `UpdatedNewer` (idempotent re-arrivals
/// and stale samples are absorbed silently).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionOutcome {
    Inserted,
    UpdatedNewer,
    Stale,
    Rejected,
}

/// Aggregated reaction view for a vine from the local viewer's
/// perspective. `count` is the number of `liked == true` reactions
/// across all reactors; `liked_by_me` is whether `viewer_addr` itself
/// has a `liked == true` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReactionSummary {
    pub count: usize,
    pub liked_by_me: bool,
}

/// Frontend-facing DTO carrying the `source` tag. Mirrors `VineVideoDto`
/// plus the `source` discriminator the frontend already consumes from
/// the `vine-received` Tauri event payload.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VineVideoDtoWithSource {
    pub id: String,
    pub creator_address: String,
    pub creator_name: String,
    pub created_at: u64,
    pub video_cid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reshare_of: Option<String>,
    pub viewed: bool,
    pub source: VineSource,
}

#[derive(Debug, Clone)]
struct CachedVine {
    descriptor: VineDescriptorPayload,
    #[allow(dead_code)] // recorded for future use (ZEB-147 may surface received-at in UI)
    received_at_ms: u64,
    #[allow(dead_code)]
    // source decision is preserved here for future use; emit returns it via DTO
    source: VineSource,
}

#[derive(Debug, Clone)]
struct CachedReaction {
    liked: bool,
    timestamp: u64,
    #[allow(dead_code)] // recorded for future UI surfacing (reactor display name)
    reactor_name: String,
}

/// In-memory, single-peer view of the Vine network. Owned by NodeState;
/// updated by the event loop on receive; queried by IPCs.
#[derive(Debug, Default)]
pub struct VineFeedCache {
    descriptors: HashMap<String, CachedVine>,
    reactions: HashMap<(String, String), CachedReaction>,
    viewed: HashSet<String>,
}

impl VineFeedCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse + insert a vine descriptor.
    ///
    /// Returns `None` if `key_expr` is not a vine-descriptor topic.
    /// Returns `Some(Rejected(reason))` on JSON parse failure.
    /// Idempotent: re-arrival of an already-cached `vine_id` returns
    /// `AlreadyPresent` and does NOT mutate the cache. Source decision
    /// (Followed vs Discover) is frozen at first insert.
    pub fn on_descriptor_sample(
        &mut self,
        key_expr: &str,
        payload: &[u8],
        followed_set: &HashSet<String>,
        now_ms: u64,
    ) -> Option<DescriptorOutcome> {
        if !key_expr.starts_with("harmony/vines/") {
            return None;
        }
        if key_expr.contains("/reactions/") {
            return None;
        }

        let descriptor: VineDescriptorPayload = match serde_json::from_slice(payload) {
            Ok(d) => d,
            Err(e) => {
                return Some(DescriptorOutcome::Rejected(format!(
                    "descriptor parse failed: {e}"
                )))
            }
        };

        if self.descriptors.contains_key(&descriptor.id) {
            return Some(DescriptorOutcome::AlreadyPresent);
        }

        let source = if followed_set.contains(&descriptor.creator_address) {
            VineSource::Followed
        } else {
            VineSource::Discover
        };

        let vine_id = descriptor.id.clone();
        let dto = self.build_dto(&descriptor, source);
        self.descriptors.insert(
            vine_id,
            CachedVine {
                descriptor,
                received_at_ms: now_ms,
                source,
            },
        );

        Some(DescriptorOutcome::Inserted { dto })
    }

    /// Return all cached descriptors as `VineVideoDto`, sorted by
    /// `created_at` DESC. `viewed` is populated by joining with the
    /// `self.viewed` HashSet (local-only viewed-state).
    pub fn list_descriptors(&self) -> Vec<VineVideoDto> {
        let mut out: Vec<VineVideoDto> = self
            .descriptors
            .values()
            .map(|cv| VineVideoDto {
                id: cv.descriptor.id.clone(),
                creator_address: cv.descriptor.creator_address.clone(),
                creator_name: cv.descriptor.creator_name.clone(),
                created_at: cv.descriptor.created_at,
                video_cid: cv.descriptor.video_cid.clone(),
                title: cv.descriptor.title.clone(),
                reshare_of: cv.descriptor.reshare_of.clone(),
                viewed: self.viewed.contains(&cv.descriptor.id),
            })
            .collect();
        out.sort_by_key(|v| std::cmp::Reverse(v.created_at));
        out
    }

    /// Internal helper: build the `VineVideoDtoWithSource` for the
    /// `Inserted` outcome. Source is provided by the caller (it was
    /// just computed). Viewed-state is joined from `self.viewed`.
    fn build_dto(
        &self,
        descriptor: &VineDescriptorPayload,
        source: VineSource,
    ) -> VineVideoDtoWithSource {
        VineVideoDtoWithSource {
            id: descriptor.id.clone(),
            creator_address: descriptor.creator_address.clone(),
            creator_name: descriptor.creator_name.clone(),
            created_at: descriptor.created_at,
            video_cid: descriptor.video_cid.clone(),
            title: descriptor.title.clone(),
            reshare_of: descriptor.reshare_of.clone(),
            viewed: self.viewed.contains(&descriptor.id),
            source,
        }
    }

    /// Parse + insert/LWW-update a reaction.
    ///
    /// Returns `None` if `key_expr` is not a vine-reaction topic.
    /// LWW per (vine_id, reactor_addr) by `timestamp`. Stale samples
    /// (timestamp older than existing entry) return `Stale` and do
    /// NOT mutate the cache.
    ///
    /// `ReactionOutcome::Rejected` is a unit variant — the underlying
    /// `serde_json` parse error is discarded for now. Callers (currently
    /// `event_loop::emit_frontend_event`, Task 5) are responsible for
    /// any observability around malformed reaction payloads. If a future
    /// telemetry need surfaces, this can be lifted to `Rejected(String)`
    /// to match `DescriptorOutcome::Rejected`.
    pub fn on_reaction_sample(
        &mut self,
        key_expr: &str,
        payload: &[u8],
    ) -> Option<ReactionOutcome> {
        if !(key_expr.starts_with("harmony/vines/") && key_expr.contains("/reactions/")) {
            return None;
        }

        let reaction: VineReactionPayload = match serde_json::from_slice(payload) {
            Ok(r) => r,
            Err(_) => return Some(ReactionOutcome::Rejected),
        };

        let key = (reaction.vine_id.clone(), reaction.reactor_address.clone());
        match self.reactions.get(&key) {
            None => {
                self.reactions.insert(
                    key,
                    CachedReaction {
                        liked: reaction.liked,
                        timestamp: reaction.timestamp,
                        reactor_name: reaction.reactor_name,
                    },
                );
                Some(ReactionOutcome::Inserted)
            }
            Some(existing) => {
                if reaction.timestamp < existing.timestamp {
                    // Strictly older: stale, no-op.
                    // Same-timestamp is treated as UpdatedNewer so that
                    // rapid toggles within one second (publish_vine_reaction
                    // uses SystemTime::now().as_secs()) are not silently
                    // dropped.
                    return Some(ReactionOutcome::Stale);
                }
                self.reactions.insert(
                    key,
                    CachedReaction {
                        liked: reaction.liked,
                        timestamp: reaction.timestamp,
                        reactor_name: reaction.reactor_name,
                    },
                );
                Some(ReactionOutcome::UpdatedNewer)
            }
        }
    }

    /// Aggregate reaction state for `vine_id` from the local viewer's
    /// perspective. `count` is the number of `liked == true` reactions
    /// across all reactors; `liked_by_me` is true iff `viewer_addr` has
    /// a `liked == true` entry for this vine.
    pub fn get_reaction(&self, vine_id: &str, viewer_addr: &str) -> ReactionSummary {
        let mut count = 0usize;
        let mut liked_by_me = false;
        for ((vid, reactor), r) in &self.reactions {
            if vid != vine_id {
                continue;
            }
            if r.liked {
                count += 1;
                if reactor == viewer_addr {
                    liked_by_me = true;
                }
            }
        }
        ReactionSummary { count, liked_by_me }
    }

    /// Mark a vine viewed by this local peer. Local-only in this PR —
    /// cross-device sync deferred to ZEB-147.
    ///
    /// Returns `true` if the vine was newly added to the viewed set,
    /// `false` if it was already viewed. Matches `FollowManager::follow`'s
    /// "did this change anything" convention.
    ///
    /// Safe to call before the descriptor arrives — `list_descriptors`
    /// joins viewed-state at query time, so the order of `mark_viewed`
    /// + `on_descriptor_sample` does not matter.
    pub fn mark_viewed(&mut self, vine_id: String) -> bool {
        self.viewed.insert(vine_id)
    }

    /// Number of cached descriptors. Test helper.
    #[allow(dead_code)]
    pub fn len_descriptors(&self) -> usize {
        self.descriptors.len()
    }

    /// Number of cached reactions. Test helper.
    #[allow(dead_code)]
    pub fn len_reactions(&self) -> usize {
        self.reactions.len()
    }

    /// Whether `vine_id` has been locally marked viewed. Test helper.
    #[allow(dead_code)]
    pub fn is_viewed(&self, vine_id: &str) -> bool {
        self.viewed.contains(vine_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Build a canonical descriptor JSON payload for `creator_address` + `vine_id`.
    ///
    /// Mirrors the bytes that production `publish_vine` produces (the same
    /// `VineDescriptorPayload` serde::Serialize shape).
    fn canonical_descriptor_bytes(
        vine_id: &str,
        creator_address: &str,
        creator_name: &str,
        video_cid: &str,
        title: Option<&str>,
        reshare_of: Option<&str>,
        created_at: u64,
    ) -> Vec<u8> {
        let v = crate::VineDescriptorPayload {
            id: vine_id.to_string(),
            creator_address: creator_address.to_string(),
            creator_name: creator_name.to_string(),
            created_at,
            video_cid: video_cid.to_string(),
            title: title.map(String::from),
            reshare_of: reshare_of.map(String::from),
        };
        serde_json::to_vec(&v).unwrap()
    }

    fn followed_set_with(addrs: &[&str]) -> HashSet<String> {
        addrs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn on_descriptor_sample_followed_creator_inserts_with_followed_source() {
        let mut cache = VineFeedCache::new();
        let payload = canonical_descriptor_bytes(
            "vine-1",
            "alice-addr",
            "Alice",
            "cid-aaa",
            Some("hello"),
            None,
            1700000000,
        );
        let followed = followed_set_with(&["alice-addr"]);

        let outcome =
            cache.on_descriptor_sample("harmony/vines/alice-addr", &payload, &followed, 1_000);

        match outcome {
            Some(DescriptorOutcome::Inserted { dto }) => {
                assert_eq!(dto.id, "vine-1");
                assert_eq!(dto.creator_address, "alice-addr");
                assert_eq!(dto.source, VineSource::Followed);
                assert!(!dto.viewed);
            }
            other => panic!("expected Inserted, got {:?}", other),
        }
        assert_eq!(cache.len_descriptors(), 1);
    }

    #[test]
    fn on_descriptor_sample_unfollowed_creator_inserts_with_discover_source() {
        let mut cache = VineFeedCache::new();
        let payload = canonical_descriptor_bytes(
            "vine-2", "bob-addr", "Bob", "cid-bbb", None, None, 1700000100,
        );
        let followed = followed_set_with(&["someone-else"]);

        let outcome =
            cache.on_descriptor_sample("harmony/vines/bob-addr", &payload, &followed, 2_000);

        match outcome {
            Some(DescriptorOutcome::Inserted { dto }) => {
                assert_eq!(dto.source, VineSource::Discover);
            }
            other => panic!("expected Inserted/Discover, got {:?}", other),
        }
    }

    #[test]
    fn on_descriptor_sample_idempotent_on_rearrival() {
        let mut cache = VineFeedCache::new();
        let payload = canonical_descriptor_bytes(
            "vine-3",
            "alice-addr",
            "Alice",
            "cid-ccc",
            None,
            None,
            1700000200,
        );
        let followed = followed_set_with(&["alice-addr"]);

        // First arrival
        let first =
            cache.on_descriptor_sample("harmony/vines/alice-addr", &payload, &followed, 3_000);
        assert!(matches!(first, Some(DescriptorOutcome::Inserted { .. })));

        // Same vine_id arrives again — even if followed_set changed
        let followed2 = followed_set_with(&[]); // empty
        let second =
            cache.on_descriptor_sample("harmony/vines/alice-addr", &payload, &followed2, 4_000);
        assert_eq!(second, Some(DescriptorOutcome::AlreadyPresent));
        assert_eq!(cache.len_descriptors(), 1);

        // Third arrival: alice is now followed again. The cache must
        // still report AlreadyPresent (i.e., source decision is frozen
        // at first insert and CANNOT be flipped by re-arrival, regardless
        // of current followed_set membership). If the cache ever decided
        // to re-insert based on a "now followed" signal, this third call
        // would return Inserted{Followed}.
        let followed3 = followed_set_with(&["alice-addr"]);
        let third =
            cache.on_descriptor_sample("harmony/vines/alice-addr", &payload, &followed3, 5_000);
        assert_eq!(third, Some(DescriptorOutcome::AlreadyPresent));
        assert_eq!(cache.len_descriptors(), 1);
    }

    #[test]
    fn on_descriptor_sample_malformed_payload_rejected() {
        let mut cache = VineFeedCache::new();
        let bad = b"not valid json {{{";
        let followed = followed_set_with(&[]);

        let outcome = cache.on_descriptor_sample("harmony/vines/alice-addr", bad, &followed, 5_000);

        match outcome {
            Some(DescriptorOutcome::Rejected(_)) => {}
            other => panic!("expected Rejected, got {:?}", other),
        }
        assert_eq!(cache.len_descriptors(), 0);
    }

    #[test]
    fn on_descriptor_sample_wrong_topic_returns_none() {
        let mut cache = VineFeedCache::new();
        let payload =
            canonical_descriptor_bytes("vine-9", "alice-addr", "Alice", "cid", None, None, 1);
        let followed = followed_set_with(&[]);

        // The descriptor branch must NOT match reaction topics (they
        // contain `/reactions/`).
        let outcome = cache.on_descriptor_sample(
            "harmony/vines/alice-addr/reactions/vine-9/bob-addr",
            &payload,
            &followed,
            6_000,
        );
        assert_eq!(outcome, None);
        assert_eq!(cache.len_descriptors(), 0);

        // And must NOT match unrelated topics.
        let outcome2 =
            cache.on_descriptor_sample("harmony/profile/alice-addr", &payload, &followed, 7_000);
        assert_eq!(outcome2, None);
    }

    #[test]
    fn list_descriptors_sorted_by_created_at_desc() {
        let mut cache = VineFeedCache::new();
        let followed = followed_set_with(&["alice-addr"]);

        // Insert in mixed order: created_at 100, 300, 200
        for (id, t) in [("v-100", 100u64), ("v-300", 300), ("v-200", 200)] {
            let payload =
                canonical_descriptor_bytes(id, "alice-addr", "Alice", "cid", None, None, t);
            cache.on_descriptor_sample("harmony/vines/alice-addr", &payload, &followed, 1_000);
        }

        let dtos = cache.list_descriptors();
        assert_eq!(dtos.len(), 3);
        // Newest first
        assert_eq!(dtos[0].id, "v-300");
        assert_eq!(dtos[1].id, "v-200");
        assert_eq!(dtos[2].id, "v-100");
    }

    fn canonical_reaction_bytes(
        vine_id: &str,
        reactor_address: &str,
        reactor_name: &str,
        liked: bool,
        timestamp: u64,
    ) -> Vec<u8> {
        let v = crate::VineReactionPayload {
            vine_id: vine_id.to_string(),
            reactor_address: reactor_address.to_string(),
            reactor_name: reactor_name.to_string(),
            liked,
            timestamp,
        };
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn two_reactors_like_same_vine_count_is_two() {
        let mut cache = VineFeedCache::new();
        let alice_likes = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", true, 100);
        let bob_likes = canonical_reaction_bytes("vine-1", "bob-addr", "Bob", true, 110);

        let r1 = cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
            &alice_likes,
        );
        let r2 = cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/bob-addr",
            &bob_likes,
        );

        assert_eq!(r1, Some(ReactionOutcome::Inserted));
        assert_eq!(r2, Some(ReactionOutcome::Inserted));

        let summary = cache.get_reaction("vine-1", "anyone-addr");
        assert_eq!(summary.count, 2);
        assert!(!summary.liked_by_me);
    }

    #[test]
    fn same_reactor_unlike_then_like_lww_wins() {
        let mut cache = VineFeedCache::new();
        // First: alice unlikes at t=100
        let alice_unlikes = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", false, 100);
        // Then: alice likes at t=200 (newer, so LWW wins)
        let alice_likes = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", true, 200);

        cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
            &alice_unlikes,
        );
        let r2 = cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
            &alice_likes,
        );
        assert_eq!(r2, Some(ReactionOutcome::UpdatedNewer));

        let summary = cache.get_reaction("vine-1", "alice-addr");
        assert_eq!(summary.count, 1);
        assert!(summary.liked_by_me);
    }

    #[test]
    fn stale_reaction_does_not_overwrite_newer() {
        let mut cache = VineFeedCache::new();
        // First: like at t=200
        let alice_likes = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", true, 200);
        cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
            &alice_likes,
        );

        // Stale unlike at t=100 (lower timestamp, must be rejected)
        let stale_unlike = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", false, 100);
        let outcome = cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
            &stale_unlike,
        );
        assert_eq!(outcome, Some(ReactionOutcome::Stale));

        // Newer like still wins
        let summary = cache.get_reaction("vine-1", "alice-addr");
        assert_eq!(summary.count, 1);
        assert!(summary.liked_by_me);
    }

    #[test]
    fn liked_by_me_reflects_viewer_addr() {
        let mut cache = VineFeedCache::new();
        let alice_likes = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", true, 100);
        let bob_likes = canonical_reaction_bytes("vine-1", "bob-addr", "Bob", true, 110);

        cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
            &alice_likes,
        );
        cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/bob-addr",
            &bob_likes,
        );

        // From Alice's perspective: liked_by_me=true (she liked it)
        let a = cache.get_reaction("vine-1", "alice-addr");
        assert_eq!(a.count, 2);
        assert!(a.liked_by_me);

        // From Carol's perspective: she did not react
        let c = cache.get_reaction("vine-1", "carol-addr");
        assert_eq!(c.count, 2);
        assert!(!c.liked_by_me);
    }

    #[test]
    fn on_reaction_sample_wrong_topic_returns_none() {
        let mut cache = VineFeedCache::new();
        let payload = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", true, 100);

        // Descriptor topic — must NOT match the reaction branch
        let outcome = cache.on_reaction_sample("harmony/vines/creator-addr", &payload);
        assert_eq!(outcome, None);

        // Unrelated topic
        let outcome2 = cache.on_reaction_sample("harmony/profile/alice-addr", &payload);
        assert_eq!(outcome2, None);
    }

    #[test]
    fn on_reaction_sample_malformed_payload_rejected() {
        let mut cache = VineFeedCache::new();
        let bad = b"{{{not json";

        let outcome = cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
            bad,
        );
        assert_eq!(outcome, Some(ReactionOutcome::Rejected));
        assert_eq!(cache.len_reactions(), 0);
    }

    #[test]
    fn get_reaction_for_unknown_vine_id_returns_zero() {
        let cache = VineFeedCache::new();
        let summary = cache.get_reaction("nonexistent-vine", "anyone-addr");
        assert_eq!(summary.count, 0);
        assert!(!summary.liked_by_me);
    }

    #[test]
    fn viewer_with_only_unliked_entry_reports_not_liked_by_me() {
        // Regression test for a subtle invariant: liked_by_me must require
        // a `liked == true` entry from viewer_addr. A `liked = false`
        // (unlike) entry for the viewer must NOT set liked_by_me — the
        // viewer explicitly does NOT like the vine. The `same_reactor_unlike_
        // then_like_lww_wins` test does insert an unlike, but immediately
        // overwrites it; this test exercises unlike-as-final-state via
        // get_reaction.
        let mut cache = VineFeedCache::new();

        // Alice unlikes
        let alice_unlike = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", false, 100);
        cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
            &alice_unlike,
        );

        // Bob likes (so count > 0, isolating the liked_by_me=false invariant)
        let bob_like = canonical_reaction_bytes("vine-1", "bob-addr", "Bob", true, 110);
        cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/bob-addr",
            &bob_like,
        );

        let summary = cache.get_reaction("vine-1", "alice-addr");
        assert_eq!(summary.count, 1);
        assert!(!summary.liked_by_me);
    }

    #[test]
    fn mark_viewed_idempotent_and_local_only() {
        let mut cache = VineFeedCache::new();
        let payload =
            canonical_descriptor_bytes("vine-1", "alice-addr", "Alice", "cid", None, None, 100);
        let followed = followed_set_with(&["alice-addr"]);
        cache.on_descriptor_sample("harmony/vines/alice-addr", &payload, &followed, 0);
        assert_eq!(cache.len_descriptors(), 1);

        // First mark — newly added
        let first = cache.mark_viewed("vine-1".to_string());
        assert!(first);
        assert!(cache.is_viewed("vine-1"));

        // Second mark — already viewed
        let second = cache.mark_viewed("vine-1".to_string());
        assert!(!second);

        // Descriptor count unchanged (mark_viewed must NOT touch descriptors)
        assert_eq!(cache.len_descriptors(), 1);

        // list_descriptors reflects viewed=true
        let dtos = cache.list_descriptors();
        assert_eq!(dtos.len(), 1);
        assert!(dtos[0].viewed);
    }

    #[test]
    fn viewed_state_survives_descriptor_insertion_order() {
        let mut cache = VineFeedCache::new();
        let followed = followed_set_with(&["alice-addr"]);

        // Mark viewed BEFORE descriptor arrives (off-order)
        let first = cache.mark_viewed("vine-future".to_string());
        assert!(first);
        assert!(cache.is_viewed("vine-future"));

        // Descriptor arrives later
        let payload = canonical_descriptor_bytes(
            "vine-future",
            "alice-addr",
            "Alice",
            "cid",
            None,
            None,
            500,
        );
        cache.on_descriptor_sample("harmony/vines/alice-addr", &payload, &followed, 0);

        // list_descriptors must show viewed=true even though the mark
        // happened before insert
        let dtos = cache.list_descriptors();
        assert_eq!(dtos.len(), 1);
        assert_eq!(dtos[0].id, "vine-future");
        assert!(dtos[0].viewed);
    }

    #[test]
    fn mark_viewed_for_unknown_vine_id_is_still_tracked() {
        let mut cache = VineFeedCache::new();

        // No descriptor exists yet
        let first = cache.mark_viewed("vine-ghost".to_string());
        assert!(first);
        assert!(cache.is_viewed("vine-ghost"));

        // No descriptors are created by mark_viewed
        assert_eq!(cache.len_descriptors(), 0);

        // list_descriptors is empty because no descriptor was ever
        // inserted — viewed-state alone does not synthesize a DTO
        let dtos = cache.list_descriptors();
        assert_eq!(dtos.len(), 0);
    }

    #[test]
    fn vine_feed_cache_round_trip_through_arc_mutex_works() {
        use std::sync::{Arc, Mutex};
        let cache = Arc::new(Mutex::new(VineFeedCache::new()));

        // Independent borrow + mutation through the lock — same pattern
        // as event_loop's emit_frontend_event will use in Task 5.
        {
            let mut guard = cache.lock().unwrap();
            guard.mark_viewed("v-1".to_string());
        }
        {
            let guard = cache.lock().unwrap();
            assert!(guard.is_viewed("v-1"));
        }

        // Two Arc clones can both read without deadlock
        let c2 = cache.clone();
        let len = c2.lock().unwrap().len_descriptors();
        assert_eq!(len, 0);
    }

    #[test]
    fn list_descriptors_returns_dto_with_viewed_state_set() {
        // Test the full DTO shape exposed to the IPC, including the
        // viewed flag joining correctly and reshare_of preservation.
        let mut cache = VineFeedCache::new();
        let payload = canonical_descriptor_bytes(
            "vine-1",
            "alice-addr",
            "Alice",
            "cid-a",
            Some("title-a"),
            None,
            500,
        );
        let payload2 = canonical_descriptor_bytes(
            "vine-2",
            "alice-addr",
            "Alice",
            "cid-b",
            None,
            Some("vine-1"), // reshare
            600,
        );
        let followed = followed_set_with(&["alice-addr"]);

        cache.on_descriptor_sample("harmony/vines/alice-addr", &payload, &followed, 0);
        cache.on_descriptor_sample("harmony/vines/alice-addr", &payload2, &followed, 0);
        cache.mark_viewed("vine-1".to_string());

        let dtos = cache.list_descriptors();
        assert_eq!(dtos.len(), 2);
        // sorted by created_at DESC: vine-2 (600) first
        assert_eq!(dtos[0].id, "vine-2");
        assert_eq!(dtos[0].reshare_of.as_deref(), Some("vine-1"));
        assert!(!dtos[0].viewed);
        // vine-1 second, marked viewed
        assert_eq!(dtos[1].id, "vine-1");
        assert_eq!(dtos[1].title.as_deref(), Some("title-a"));
        assert!(dtos[1].viewed);
    }
}
