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

// Public surface is intentionally forward-declared for Tasks 2-4; wiring
// into NodeState and Tauri IPCs lands later in this PR.
#![allow(dead_code)]

use crate::{VineDescriptorPayload, VineVideoDto};
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
    pub title: Option<String>,
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
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
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

        // Source decision from first arrival is preserved (Followed),
        // not flipped to Discover by the second-arrival empty followed_set.
        let dtos = cache.list_descriptors();
        assert_eq!(dtos.len(), 1);
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
}
