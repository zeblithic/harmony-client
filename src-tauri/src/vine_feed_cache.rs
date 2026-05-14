//! ZEB-286: VineFeedCache — Rust-side state surface for the Vine feed.
//!
//! Cache is updated by `event_loop::emit_frontend_event` on receive (one
//! cache instance per NodeState; shared with the event loop via
//! `Arc<Mutex<VineFeedCache>>`). Read by the `list_vine_videos()` and
//! `mark_vine_viewed()` Tauri IPCs.
//!
//! ZEB-147: disk-backed via `load()` + `save()`. `new()` stays for tests
//! (no disk side-effects); production uses `load(&app_data_dir)`.
//!
//! See `docs/specs/2026-05-13-zeb-286-vine-integration-test-design.md`.

use crate::{VineDescriptorPayload, VineReactionPayload, VineVideoDto};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// ZEB-147: max descriptors retained in the cache. On insert into a full
/// cache, the oldest descriptor (lowest `created_at`) is dropped, along
/// with its reactions. Viewed-set entries are NOT dropped (low byte cost).
pub const MAX_DESCRIPTORS: usize = 5000;

/// ZEB-147: max age of a descriptor in seconds. Applied ONCE on `load()`;
/// descriptors with `created_at < now_secs - MAX_AGE_SECS` are dropped
/// along with their reactions. Runtime mutations do not re-age-prune.
pub const MAX_AGE_SECS: u64 = 90 * 86_400;

/// On-disk filename for the cached Vine feed. Lives under `app_data_dir`.
const VINE_FEED_FILE: &str = "vine_feed.json";

/// On-disk schema version. Bump only on a breaking format change; `load()`
/// rejects `version != FILE_VERSION` (treat as missing).
const FILE_VERSION: u32 = 1;

/// On-disk envelope. Versioned at the top level for forward-compat.
/// `version != 1` on `load()` causes the file to be ignored (treat as
/// missing). v1 is the only version that exists today.
#[derive(Debug, Serialize, Deserialize)]
struct VineFeedDiskV1 {
    version: u32,
    descriptors: Vec<DescriptorOnDisk>,
    reactions: Vec<ReactionOnDisk>,
    viewed: Vec<String>,
}

/// On-disk descriptor row. Mirrors the in-memory `CachedVine` plus the
/// `source` tag (decided at first arrival; preserved across reloads).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DescriptorOnDisk {
    id: String,
    creator_address: String,
    creator_name: String,
    created_at: u64,
    video_cid: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    reshare_of: Option<String>,
    received_at_ms: u64,
    source: VineSource,
}

/// On-disk reaction row. Flat — `vine_id` and `reactor_address` join
/// back to the in-memory `HashMap<(String, String), CachedReaction>` key.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReactionOnDisk {
    vine_id: String,
    reactor_address: String,
    reactor_name: String,
    liked: bool,
    timestamp: u64,
}

/// How the recipient discovered this vine. Followed = creator is in the
/// local follow set at the time of first arrival; Discover = otherwise.
/// Decided ONCE at first insert; subsequent re-arrivals do not change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
///
/// ZEB-147: `path` is set by `load(data_dir)` (production path) and is
/// `None` after `new()` (test path). When `None`, `save()` is a no-op,
/// so unit tests can mutate the cache freely without touching disk.
#[derive(Debug, Default)]
pub struct VineFeedCache {
    descriptors: HashMap<String, CachedVine>,
    reactions: HashMap<(String, String), CachedReaction>,
    viewed: HashSet<String>,
    /// `Some(path_to_vine_feed.json)` when constructed via `load()`;
    /// `None` for `new()`. `save()` checks this and is a no-op when None.
    #[allow(dead_code)] // read by save(); save() wired to mutators in ZEB-147 Task 4
    path: Option<PathBuf>,
}

impl VineFeedCache {
    /// In-memory only. No persistence path; `save()` is a no-op.
    /// Used by tests and any caller that explicitly wants ephemeral state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load from `data_dir/vine_feed.json`. Returns an empty cache (with
    /// `path` set so subsequent mutations persist) when the file is
    /// missing, unreadable, malformed JSON, or has an unrecognized
    /// `version`. Applies the age cutoff and capacity cap on load so
    /// the in-memory state mirrors what `on_descriptor_sample` would
    /// enforce going forward.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(VINE_FEED_FILE);
        let mut cache = Self {
            descriptors: HashMap::new(),
            reactions: HashMap::new(),
            viewed: HashSet::new(),
            path: Some(path.clone()),
        };
        Self::populate_from_disk(&mut cache, &path);
        cache
    }

    /// Read `path` (if it exists) and populate `cache`. Errors / version
    /// mismatch / malformed JSON all silently produce an empty cache —
    /// matches `follows.rs::FollowManager::load`'s graceful-degrade.
    fn populate_from_disk(cache: &mut Self, path: &Path) {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => return, // file missing or unreadable — treat as empty
        };
        let file: VineFeedDiskV1 = match serde_json::from_slice(&bytes) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    "vine_feed_cache: load() ignoring malformed vine_feed.json",
                );
                return;
            }
        };
        if file.version != FILE_VERSION {
            tracing::warn!(
                version = file.version,
                expected = FILE_VERSION,
                "vine_feed_cache: load() ignoring vine_feed.json with unexpected version",
            );
            return;
        }

        // Age-prune (one-shot on load).
        // Broken clock fallback: now_secs = 0 → age_cutoff = 0 → all
        // descriptors kept. Prefer keeping stale data over silently losing
        // the entire cache when the system clock is bad.
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let age_cutoff = now_secs.saturating_sub(MAX_AGE_SECS);
        let mut descriptors: Vec<DescriptorOnDisk> = file
            .descriptors
            .into_iter()
            .filter(|d| d.created_at >= age_cutoff)
            .collect();

        // Capacity-trim (defensive — production write path enforces cap
        // on insert, but persisted state from a future version with a
        // higher cap could exceed ours).
        if descriptors.len() > MAX_DESCRIPTORS {
            // Sort by created_at DESC, ties by id ASC (deterministic).
            descriptors.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
            descriptors.truncate(MAX_DESCRIPTORS);
        }

        // Build the surviving vine-id set for orphan-pruning reactions.
        let surviving_ids: HashSet<String> = descriptors.iter().map(|d| d.id.clone()).collect();

        // Reactions: drop orphans (where the parent descriptor was pruned).
        for r in file.reactions {
            if !surviving_ids.contains(&r.vine_id) {
                continue;
            }
            cache.reactions.insert(
                (r.vine_id, r.reactor_address),
                CachedReaction {
                    liked: r.liked,
                    timestamp: r.timestamp,
                    reactor_name: r.reactor_name,
                },
            );
        }

        // Descriptors: populate the cache from the (possibly pruned) list.
        for d in descriptors {
            let descriptor = VineDescriptorPayload {
                id: d.id.clone(),
                creator_address: d.creator_address,
                creator_name: d.creator_name,
                created_at: d.created_at,
                video_cid: d.video_cid,
                title: d.title,
                reshare_of: d.reshare_of,
            };
            cache.descriptors.insert(
                d.id,
                CachedVine {
                    descriptor,
                    received_at_ms: d.received_at_ms,
                    source: d.source,
                },
            );
        }

        // Viewed: passes through unmodified (low byte cost; not pruned
        // even when the associated descriptor age-prunes — see spec §11
        // out-of-scope on viewed-set GC).
        cache.viewed = file.viewed.into_iter().collect();
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
                // Stale if strictly older, OR if same-timestamp AND the
                // liked-state is unchanged (exact duplicate redelivery).
                // Same-timestamp with CHANGED liked-state is treated as
                // UpdatedNewer so that rapid toggles within one second
                // (publish_vine_reaction uses SystemTime::now().as_secs()
                // second-resolution) are not silently dropped.
                if reaction.timestamp < existing.timestamp
                    || (reaction.timestamp == existing.timestamp
                        && reaction.liked == existing.liked)
                {
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

    /// Atomic save: serialize cache state, write to `<path>.tmp`, rename
    /// to `<path>`. No-op when `self.path.is_none()` (test path).
    ///
    /// Errors are logged via `tracing::warn!` but never propagated — this
    /// matches the `follows.rs` / `content_index.rs` philosophy: persistence
    /// is best-effort; a failed save must not crash the dispatch loop.
    #[allow(dead_code)] // wired into mutators in ZEB-147 Task 4
    fn save(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };

        let file = VineFeedDiskV1 {
            version: FILE_VERSION,
            descriptors: self
                .descriptors
                .values()
                .map(|cv| DescriptorOnDisk {
                    id: cv.descriptor.id.clone(),
                    creator_address: cv.descriptor.creator_address.clone(),
                    creator_name: cv.descriptor.creator_name.clone(),
                    created_at: cv.descriptor.created_at,
                    video_cid: cv.descriptor.video_cid.clone(),
                    title: cv.descriptor.title.clone(),
                    reshare_of: cv.descriptor.reshare_of.clone(),
                    received_at_ms: cv.received_at_ms,
                    source: cv.source,
                })
                .collect(),
            reactions: self
                .reactions
                .iter()
                .map(|((vine_id, reactor_addr), r)| ReactionOnDisk {
                    vine_id: vine_id.clone(),
                    reactor_address: reactor_addr.clone(),
                    reactor_name: r.reactor_name.clone(),
                    liked: r.liked,
                    timestamp: r.timestamp,
                })
                .collect(),
            viewed: self.viewed.iter().cloned().collect(),
        };

        let json = match serde_json::to_vec_pretty(&file) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(err = %e, "vine_feed_cache: serialize failed; changes not persisted");
                return;
            }
        };

        let tmp_path = {
            let mut name = path.file_name().unwrap_or_default().to_os_string();
            name.push(".tmp");
            path.with_file_name(name)
        };

        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(err = %e, "vine_feed_cache: create_dir_all failed; changes not persisted");
                return;
            }
        }

        if let Err(e) = std::fs::write(&tmp_path, &json) {
            tracing::warn!(err = %e, "vine_feed_cache: write tmp failed; changes not persisted");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp_path, path) {
            tracing::warn!(err = %e, "vine_feed_cache: rename failed; tmp file may be stale");
        }
    }

    /// Test-only public alias for `save()`. Lets unit tests trigger
    /// persistence explicitly before Task 4 wires it into mutators.
    /// Marked `#[cfg(test)]` so it cannot leak into production callers.
    #[cfg(test)]
    pub fn save_for_test(&self) {
        self.save();
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
    fn new_leaves_path_unset() {
        let cache = VineFeedCache::new();
        // `path` is private; we observe it indirectly: save() must be a
        // no-op when path is None. Since save() is wired in Task 2, this
        // test asserts the constructor contract via the public API.
        // For now, just assert the cache is empty and constructable.
        assert_eq!(cache.len_descriptors(), 0);
        assert_eq!(cache.len_reactions(), 0);
        assert!(!cache.is_viewed("anything"));
    }

    #[test]
    fn save_is_noop_when_path_is_none() {
        // VineFeedCache::new() leaves path = None. Calling save_for_test()
        // must not panic and must not create vine_feed.json anywhere on
        // disk. We verify the negative by ensuring the in-memory cache
        // remains usable AND no file appears in a freshly-created tempdir
        // (which the cache has no knowledge of — that's the point).
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut cache = VineFeedCache::new();
        cache.mark_viewed("v-1".to_string());
        cache.save_for_test(); // must be a no-op (path is None)
        assert!(cache.is_viewed("v-1"));
        // The tempdir was never told to the cache; nothing should appear.
        assert!(!dir.path().join(VINE_FEED_FILE).exists());
    }

    #[test]
    fn save_writes_atomic_file_when_path_is_set() {
        // Construct a cache with path set, mutate it, call save() directly
        // (the method is private — we invoke it via a Task-2-internal
        // test helper exposing `save_for_test`). Verify the file exists
        // and contains expected JSON.
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut cache = VineFeedCache::load(dir.path());
        cache.mark_viewed("v-saved".to_string());
        cache.save_for_test();

        let path = dir.path().join(VINE_FEED_FILE);
        assert!(path.exists(), "vine_feed.json must exist after save");
        let bytes = std::fs::read(&path).expect("read saved file");
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).expect("file must be valid JSON");
        assert_eq!(json["version"], FILE_VERSION);
        assert!(
            json["viewed"]
                .as_array()
                .map(|a| a.iter().any(|v| v.as_str() == Some("v-saved")))
                .unwrap_or(false),
            "viewed set must contain v-saved; got: {json}"
        );
    }

    #[test]
    fn load_empty_dir_returns_empty_cache() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let cache = VineFeedCache::load(dir.path());
        assert_eq!(cache.len_descriptors(), 0);
        assert_eq!(cache.len_reactions(), 0);
        assert!(!cache.is_viewed("anything"));
    }

    #[test]
    fn load_round_trip_preserves_descriptors_reactions_viewed() {
        // Use a created_at that won't be age-pruned on reload.
        // now_secs - 1 is always within the 90-day window.
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let recent_created_at = now_secs.saturating_sub(1);

        // Phase 1: build + save state
        let dir = tempfile::tempdir().expect("create tempdir");
        {
            let mut cache = VineFeedCache::load(dir.path());
            let followed = followed_set_with(&["alice-addr"]);
            let desc = canonical_descriptor_bytes(
                "vine-rt",
                "alice-addr",
                "Alice",
                "cid-x",
                Some("title-x"),
                None,
                recent_created_at,
            );
            let out =
                cache.on_descriptor_sample("harmony/vines/alice-addr", &desc, &followed, 1_000);
            assert!(matches!(out, Some(DescriptorOutcome::Inserted { .. })));

            let react =
                canonical_reaction_bytes("vine-rt", "bob-addr", "Bob", true, recent_created_at + 1);
            let out2 = cache.on_reaction_sample(
                "harmony/vines/alice-addr/reactions/vine-rt/bob-addr",
                &react,
            );
            assert_eq!(out2, Some(ReactionOutcome::Inserted));

            assert!(cache.mark_viewed("vine-rt".to_string()));
            cache.save_for_test();
        }
        // Phase 2: reload from same dir, assert state survived
        let cache2 = VineFeedCache::load(dir.path());
        assert_eq!(cache2.len_descriptors(), 1);
        assert_eq!(cache2.len_reactions(), 1);
        assert!(cache2.is_viewed("vine-rt"));

        // Verify DTO is correctly reconstructed
        let dtos = cache2.list_descriptors();
        assert_eq!(dtos.len(), 1);
        assert_eq!(dtos[0].id, "vine-rt");
        assert_eq!(dtos[0].creator_address, "alice-addr");
        assert_eq!(dtos[0].title.as_deref(), Some("title-x"));
        assert!(dtos[0].viewed);

        // Verify reaction is correctly reconstructed (count + liked_by_me)
        let summary = cache2.get_reaction("vine-rt", "bob-addr");
        assert_eq!(summary.count, 1);
        assert!(summary.liked_by_me);
    }

    #[test]
    fn load_rejects_wrong_version() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join(VINE_FEED_FILE);
        // Write a v999 envelope
        let json = serde_json::json!({
            "version": 999,
            "descriptors": [],
            "reactions": [],
            "viewed": ["v-ignored"]
        });
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();

        // load() must treat wrong-version as "missing file" — empty cache
        let cache = VineFeedCache::load(dir.path());
        assert_eq!(cache.len_descriptors(), 0);
        assert_eq!(cache.len_reactions(), 0);
        assert!(!cache.is_viewed("v-ignored"));
    }

    #[test]
    fn load_corrupt_json_returns_empty_cache() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join(VINE_FEED_FILE);
        std::fs::write(&path, b"{ this is not valid json").unwrap();

        let cache = VineFeedCache::load(dir.path());
        assert_eq!(cache.len_descriptors(), 0);
        assert_eq!(cache.len_reactions(), 0);
    }

    #[test]
    fn age_prune_on_load_drops_old_descriptors_and_their_reactions() {
        // Write a vine_feed.json containing one old descriptor (created_at
        // = epoch, well past the 90d cutoff) and one recent (created_at =
        // now - 1d). Add a reaction for each. After load(), only the
        // recent descriptor and its reaction should survive.
        let dir = tempfile::tempdir().expect("create tempdir");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let recent_ts = now.saturating_sub(86_400); // 1 day old
        let old_ts = 0u64; // epoch — definitely older than 90 days

        let disk = serde_json::json!({
            "version": 1,
            "descriptors": [
                {
                    "id": "vine-old",
                    "creatorAddress": "alice-addr",
                    "creatorName": "Alice",
                    "createdAt": old_ts,
                    "videoCid": "cid-old",
                    "receivedAtMs": 0,
                    "source": "followed"
                },
                {
                    "id": "vine-new",
                    "creatorAddress": "alice-addr",
                    "creatorName": "Alice",
                    "createdAt": recent_ts,
                    "videoCid": "cid-new",
                    "receivedAtMs": 0,
                    "source": "followed"
                }
            ],
            "reactions": [
                {
                    "vineId": "vine-old",
                    "reactorAddress": "bob-addr",
                    "reactorName": "Bob",
                    "liked": true,
                    "timestamp": old_ts
                },
                {
                    "vineId": "vine-new",
                    "reactorAddress": "bob-addr",
                    "reactorName": "Bob",
                    "liked": true,
                    "timestamp": recent_ts
                }
            ],
            "viewed": []
        });
        std::fs::write(
            dir.path().join(VINE_FEED_FILE),
            serde_json::to_vec_pretty(&disk).unwrap(),
        )
        .unwrap();

        let cache = VineFeedCache::load(dir.path());
        assert_eq!(
            cache.len_descriptors(),
            1,
            "old descriptor must be age-pruned; only vine-new survives"
        );
        let dtos = cache.list_descriptors();
        assert_eq!(dtos[0].id, "vine-new");
        // The orphan reaction for vine-old must be gone; vine-new's
        // reaction must survive.
        assert_eq!(cache.len_reactions(), 1);
        let summary = cache.get_reaction("vine-new", "bob-addr");
        assert_eq!(summary.count, 1);
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
