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
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// ZEB-147: max descriptors retained in the cache. On insert into a full
/// cache, the oldest descriptor (lowest `created_at`) is dropped, along
/// with its reactions. Viewed-set entries are NOT dropped (low byte cost).
pub const MAX_DESCRIPTORS: usize = 5000;

/// ZEB-147: max age of a descriptor in seconds. Applied ONCE on `load()`;
/// descriptors with `created_at < now_secs - MAX_AGE_SECS` are dropped
/// along with their reactions. Runtime mutations do not re-age-prune.
pub const MAX_AGE_SECS: u64 = 90 * 86_400;

/// ZEB-671: max addresses in one follow list — enforced on BOTH sides:
/// publish truncates, ingest rejects larger lists outright.
pub const MAX_FOLLOWS_PER_LIST: usize = 1000;

/// ZEB-671: max cached follow lists (one per owner). On overflow the
/// stalest list (lowest `updated_at`) is evicted.
pub const MAX_FOLLOW_LISTS: usize = 5000;

/// ZEB-671: byte ceiling for a follow-list wire payload, checked BEFORE
/// JSON parsing (CodeRabbit PR #447 — the topic is peer-writable, and
/// `MAX_FOLLOWS_PER_LIST` alone only rejects after serde has already
/// paid for an arbitrarily large document). A maximal legitimate record
/// is ~68 KB (1000 × 64-hex addresses + envelope + sig fields); 96 KB
/// leaves comfortable slack.
pub const MAX_FOLLOW_LIST_WIRE_BYTES: usize = 96 * 1024;

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
    /// ZEB-670: applied tombstones. `#[serde(default)]` so pre-tombstone
    /// v1 files load unchanged (and older builds ignore the field —
    /// serde_json tolerates unknown fields by default).
    #[serde(default)]
    tombstones: Vec<TombstoneOnDisk>,
    /// ZEB-671: cached peer follow lists. `#[serde(default)]` — same
    /// additive-evolution posture as `tombstones`.
    #[serde(default)]
    follow_lists: Vec<FollowListOnDisk>,
}

/// ZEB-671: on-disk follow-list row. Signature/pub are NOT retained —
/// verification happens once at ingest (same posture as descriptors).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FollowListOnDisk {
    owner: String,
    follows: Vec<String>,
    updated_at: u64,
}

/// ZEB-670: on-disk applied-tombstone row. Signature/pub are NOT retained —
/// verification happens once at ingest; persisted tombstones are trusted
/// local state (same posture as descriptors).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TombstoneOnDisk {
    vine_id: String,
    video_cid: String,
    creator_address: String,
    deleted_at: u64,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reshare_of: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    original_creator_address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    original_creator_name: Option<String>,
    /// ZEB-811: retained wire signature fields so a relay can serve
    /// verifiable descriptors after a restart (previously dropped on
    /// disk under a verify-once-at-ingest posture — see the doc comment
    /// on `VineDescriptorPayload::identity_pub`). `#[serde(default)]` so
    /// rows persisted before ZEB-811 deserialize with `None`; such
    /// legacy rows still load and are listed by `list_descriptors()`
    /// (local display never needed the signature) but are excluded from
    /// `descriptors_for_creator_page` — a relay must not serve an
    /// unverifiable row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity_pub: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sig: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_sig: Option<String>,
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
    Inserted { dto: Box<VineVideoDtoWithSource> },
    AlreadyPresent,
    Rejected(String),
}

/// Outcome of `on_reaction_sample`. The receive path re-emits to the
/// frontend only on `Inserted` or `UpdatedNewer` (idempotent re-arrivals
/// and stale samples are absorbed silently). `Rejected` carries the
/// reason (parse / signature / topic-binding failure — ZEB-673).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactionOutcome {
    Inserted,
    UpdatedNewer,
    Stale,
    Rejected(String),
}

/// ZEB-670: in-memory applied tombstone, keyed by `vine_id` in
/// `VineFeedCache::tombstones`.
#[derive(Debug, Clone)]
struct TombstoneRecord {
    video_cid: String,
    creator_address: String,
    deleted_at: u64,
}

/// ZEB-671: in-memory verified follow list, keyed by owner address in
/// `VineFeedCache::follow_lists`. Signature already checked at ingest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowListEntry {
    pub follows: Vec<String>,
    pub updated_at: u64,
}

/// Outcome of `on_follow_list_sample`. The receive path recomputes the
/// Discover graph only on `Inserted` / `UpdatedNewer` (stale re-arrivals
/// and rejected records must have zero state effect).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowListOutcome {
    Inserted,
    UpdatedNewer,
    IgnoredOlder,
    Rejected(String),
}

/// ZEB-670: `vine-removed` frontend event payload, produced for every
/// newly applied tombstone — even when no original descriptor was cached
/// (a subscriber may hold only reshares, which still need stub-marking;
/// CodeRabbit PR #445 round 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovedVine {
    pub vine_id: String,
    pub video_cid: String,
    pub creator_address: String,
}

/// Outcome of `on_tombstone_sample`.
///
/// `Applied.removed` is always present (built from the verified tombstone,
/// with the cached descriptor's CID as the canonical `video_cid` when one
/// was cached). `Applied.evict_cid` is `Some` only when no remaining live
/// (non-stub) descriptor references that CID — the caller (event loop)
/// may then evict the blob; eviction of never-held bytes is a no-op.
#[derive(Debug, Clone, PartialEq)]
pub enum TombstoneOutcome {
    Applied {
        removed: RemovedVine,
        evict_cid: Option<String>,
    },
    AlreadyApplied,
    Rejected(String),
}

/// ZEB-670: cap on retained tombstones (CodeRabbit PR #445 round 1 —
/// unbounded growth was a disk/scan DoS path: every valid-signed tombstone
/// persisted forever, `save()` rewrites the file per insert). Generous
/// relative to MAX_DESCRIPTORS because rows are tiny and every legit
/// delete needs its guard; oldest-by-`deleted_at` trims first. Adversarial
/// far-future timestamps can push out honest guards — signing (ZEB-678)
/// authenticates a tombstone's author but does not bound its self-asserted
/// `deleted_at`, so a validly-signed tombstone can still carry an
/// attacker-chosen far-future stamp.
pub const MAX_TOMBSTONES: usize = 10_000;

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

/// Flat reaction row for the `list_vine_reactions` IPC (ZEB-672).
/// Mirrors the `vine-reaction-received` event payload shape so the
/// frontend hydrate merge and the live handler share one wire model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VineReactionRow {
    pub vine_id: String,
    pub reactor_address: String,
    pub reactor_name: String,
    pub liked: bool,
    pub timestamp: u64,
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
    pub source: VineSource,
    pub viewed: bool,
    /// See VineDescriptorPayload::original_creator_address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_creator_address: Option<String>,
    /// See VineDescriptorPayload::original_creator_name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_creator_name: Option<String>,
    /// ZEB-670: true when this row is a reshare whose original was
    /// tombstoned by its creator — render as a "Removed by creator" stub.
    pub original_removed: bool,
    /// ZEB-671: graph distance (2 or 3) for Discover provenance. See
    /// `VineVideoDto::degree`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degree: Option<u8>,
    /// ZEB-671: follow chain that reached the creator. See
    /// `VineVideoDto::via`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
struct CachedVine {
    descriptor: VineDescriptorPayload,
    received_at_ms: u64,
    source: VineSource,
}

/// One candidate row in `descriptors_for_creator_page`'s bounded top-k
/// max-heap, ordered solely by the `(created_at, id)` cursor tuple — never
/// derive `Ord`/`PartialOrd` off the full `CachedVine` (it has no natural
/// order and its `descriptor.id` isn't unique enough alone to compare
/// against `after`).
struct PageCandidate<'a> {
    key: (u64, &'a str),
    cv: &'a CachedVine,
}

impl PartialEq for PageCandidate<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for PageCandidate<'_> {}

impl PartialOrd for PageCandidate<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PageCandidate<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key.cmp(&other.key)
    }
}

#[derive(Debug, Clone)]
struct CachedReaction {
    liked: bool,
    timestamp: u64,
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
    /// ZEB-670: applied tombstones keyed by vine_id. Retained (and
    /// persisted) even for vine ids never seen, so a descriptor arriving
    /// AFTER its tombstone — or re-arriving from a still-holding peer
    /// after a restart — cannot resurrect the vine.
    tombstones: HashMap<String, TombstoneRecord>,
    /// ZEB-671: verified peer follow lists keyed by owner address —
    /// the raw edges the Discover graph is computed from. LWW by
    /// `updated_at`; bounded by `MAX_FOLLOW_LISTS`.
    follow_lists: HashMap<String, FollowListEntry>,
    /// ZEB-671: the viewer's own address — BFS root for the Discover
    /// graph. Set via `set_graph_inputs` (start_node + follow/unfollow).
    graph_me: String,
    /// ZEB-671: the viewer's LOCAL follows (degree-1 ground truth; never
    /// the own published echo).
    graph_my_follows: Vec<String>,
    /// ZEB-671: derived reachability (creator → degree + via-path).
    /// Recomputed on graph-input or follow-list change; NOT persisted.
    reach: HashMap<String, crate::vine_follow_graph::Reach>,
    /// ZEB-678 S2: per-feed authority records (owner-anchoring + revocation),
    /// fed by `on_authority_sample`. In-memory only (like `reach`); the content
    /// verifiers consult it to pick the dual-path (authority cached ⇒ require
    /// `#2` + `!revoked`; else legacy `#3`).
    authority: crate::feed_authority::FeedAuthorityCache,
    /// `Some(path_to_vine_feed.json)` when constructed via `load()`;
    /// `None` for `new()`. `save()` checks this and is a no-op when None.
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
            tombstones: HashMap::new(),
            follow_lists: HashMap::new(),
            graph_me: String::new(),
            graph_my_follows: Vec::new(),
            reach: HashMap::new(),
            authority: crate::feed_authority::FeedAuthorityCache::default(),
            path: Some(path.clone()),
        };
        Self::populate_from_disk(&mut cache, &path);
        cache
    }

    /// Read `path` (if it exists) and populate `cache`. A missing file
    /// (`NotFound`) silently produces an empty cache — the expected first-run
    /// path. Other IO errors (permissions, etc.), version mismatches, and
    /// malformed JSON log a `tracing::warn!` and fall back to an empty cache,
    /// matching `follows.rs::FollowManager::load`'s graceful-degrade.
    fn populate_from_disk(cache: &mut Self, path: &Path) {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    path = %path.display(),
                    "vine_feed_cache: load() failed; starting with empty cache",
                );
                return;
            }
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
        // Backward age bound ONLY. The ZEB-831/VF forward-skew bound is
        // deliberately NOT applied here: load must never delete a persisted
        // descriptor on a clock reading (Greptile PR #580 P1). `save()` rewrites
        // `vine_feed.json` from the in-memory set, so a load-time drop is made
        // permanent by the next mutation — and a local clock set *behind* real
        // time makes every honest descriptor look future-dated, which would purge
        // the entire feed irreversibly (the `now_secs == 0` guard only catches an
        // epoch-zero clock, not a slow one). Unlike the ingest gate in
        // `on_descriptor_sample`, whose rejection is recoverable via re-gossip, a
        // load-delete is not. The forward gate instead lives in `list_descriptors`
        // as a non-destructive, live-re-evaluated display filter: a future-dated
        // descriptor is hidden from the feed but retained on disk, and reappears
        // once the clock is corrected.
        let mut descriptors: Vec<DescriptorOnDisk> = file
            .descriptors
            .into_iter()
            .filter(|d| d.created_at >= age_cutoff)
            .collect();

        // Capacity-trim (defensive — production write path enforces cap
        // on insert, but persisted state from a future version with a
        // higher cap could exceed ours).
        if descriptors.len() > MAX_DESCRIPTORS {
            // Match runtime trim: sort oldest-first by created_at ASC,
            // ties by id ASC, drop from the front. Symmetric with the
            // runtime path in on_descriptor_sample so a restart retains
            // exactly the same descriptor set as live mutation would.
            descriptors.sort_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
            let drop_count = descriptors.len() - MAX_DESCRIPTORS;
            descriptors.drain(0..drop_count);
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
                original_creator_address: d.original_creator_address,
                original_creator_name: d.original_creator_name,
                // ZEB-811: signatures ARE retained on disk (see
                // `DescriptorOnDisk`'s doc comment) so a relay can serve
                // verifiable descriptors after a restart. `#[serde(default)]`
                // means legacy pre-ZEB-811 rows restore `None` here — those
                // rows still load and list, but `descriptors_for_creator_page`
                // filters them out.
                identity_pub: d.identity_pub,
                sig: d.sig,
                device_sig: d.device_sig,
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

        // ZEB-671: follow lists restore verbatim (already verified at
        // ingest — sigs are never persisted). Defensive bounds mirror
        // the ingest path in case a future/foreign build wrote more.
        let mut lists = file.follow_lists;
        if lists.len() > MAX_FOLLOW_LISTS {
            // Keep the freshest MAX_FOLLOW_LISTS (ties by owner).
            lists.sort_by(|a, b| {
                b.updated_at
                    .cmp(&a.updated_at)
                    .then_with(|| a.owner.cmp(&b.owner))
            });
            lists.truncate(MAX_FOLLOW_LISTS);
        }
        for mut l in lists {
            l.follows.truncate(MAX_FOLLOWS_PER_LIST);
            cache.follow_lists.insert(
                l.owner,
                FollowListEntry {
                    follows: l.follows,
                    updated_at: l.updated_at,
                },
            );
        }

        // ZEB-670: tombstones age-prune on the same 90-day window as
        // descriptors (by deleted_at) — past it, the descriptor they
        // guard against would have age-pruned anyway.
        for t in file.tombstones {
            if t.deleted_at < age_cutoff {
                continue;
            }
            cache.tombstones.insert(
                t.vine_id,
                TombstoneRecord {
                    video_cid: t.video_cid,
                    creator_address: t.creator_address,
                    deleted_at: t.deleted_at,
                },
            );
        }
    }

    /// Parse + insert a vine descriptor.
    ///
    /// Returns `None` if `key_expr` is not a vine-descriptor topic.
    /// Returns `Some(Rejected(reason))` on JSON parse failure.
    /// Idempotent: re-arrival of an already-cached `vine_id` returns
    /// `AlreadyPresent` and does NOT mutate the cache. Source decision
    /// ZEB-678 S2: ingest a feed authority record from
    /// `harmony/vines/{N}/authority`. Parses the record, binds it to the topic
    /// segment `N`, and feeds it to the LWW/sticky-`revoked` `FeedAuthorityCache`
    /// (first-write-wins binding, monotonic revocation). `now_secs` is the
    /// verifier's wall clock (never a record field). A malformed record or a
    /// topic/`feed_id` mismatch is dropped with a warn — authority ingest never
    /// degrades existing cached state.
    pub fn on_authority_sample(&mut self, key_expr: &str, payload: &[u8], now_secs: u64) {
        let feed = match key_expr
            .strip_prefix("harmony/vines/")
            .and_then(|s| s.strip_suffix("/authority"))
        {
            Some(f) if !f.contains('/') => f,
            _ => return, // not a `harmony/vines/{N}/authority` topic
        };
        let rec: crate::feed_authority::FeedAuthorityRecord = match serde_json::from_slice(payload)
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "dropping malformed feed authority record");
                return;
            }
        };
        if rec.feed_id != feed {
            tracing::warn!(
                topic_feed = feed,
                record_feed = %rec.feed_id,
                "feed authority record feed_id does not match its topic; dropping"
            );
            return;
        }
        let _ = self.authority.ingest(&rec, now_secs);
    }

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
        if key_expr.contains("/reactions/") || key_expr.contains("/tombstones/") {
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

        // ZEB-673: strict wire admission, before any state effect. The
        // record must be creator-signed (pubkey→address binding +
        // verify_strict) and published on the creator's own topic — the
        // key is not authoritative, so both directions matter: a valid
        // record replayed onto another topic AND a forged record on the
        // right topic are rejected. Disk loads bypass this path
        // (populate_from_disk) and stay tolerant of pre-ZEB-673 rows.
        let topic_creator = key_expr.strip_prefix("harmony/vines/").unwrap_or("");
        // ZEB-678 S2: dual-path admission. Authority cached for this feed ⇒
        // require a valid `#2` device_sig against the pinned publisher_key AND a
        // non-revoked device (legacy `#3`-only records are rejected once the
        // feed has migrated). No authority cached ⇒ verify the legacy `#3` way.
        let verify_result = match self.authority.get(topic_creator) {
            Some(p) if p.revoked => {
                Err("descriptor rejected: publisher device revoked".to_string())
            }
            Some(p) => crate::vine_signing::verify_descriptor_v2(&descriptor, &p.publisher_key),
            None => crate::vine_signing::verify_descriptor(&descriptor),
        };
        if let Err(e) = verify_result {
            return Some(DescriptorOutcome::Rejected(e));
        }
        if topic_creator != descriptor.creator_address {
            return Some(DescriptorOutcome::Rejected(format!(
                "descriptor topic creator '{topic_creator}' does not match payload creator '{}'",
                descriptor.creator_address
            )));
        }

        if self.descriptors.contains_key(&descriptor.id) {
            return Some(DescriptorOutcome::AlreadyPresent);
        }

        // ZEB-670: a tombstoned vine id can never (re-)insert — the
        // pre-arrival / re-arrival resurrection guard. CREATOR-BOUND
        // (CodeRabbit PR #445 round 1, Critical): the tombstone's signer
        // must match the descriptor's creator, otherwise an attacker who
        // observed a victim's vine id could pre-publish a tombstone
        // validly signed under the ATTACKER's own address and censor the
        // victim's descriptor. A mismatched tombstone simply doesn't
        // apply to this descriptor.
        if let Some(t) = self.tombstones.get(&descriptor.id) {
            if t.creator_address == descriptor.creator_address {
                return Some(DescriptorOutcome::Rejected(format!(
                    "descriptor {} is tombstoned",
                    descriptor.id
                )));
            }
        }

        // ZEB-670: runtime age gate, mirroring the load-time prune (which
        // previously ran only in populate_from_disk — a live re-delivery
        // of an ancient descriptor could outlive its tombstone's 90-day
        // window and resurrect; CodeRabbit PR #445 round 1). Invariant:
        // `deleted_at >= created_at`, so by the time a tombstone
        // age-prunes, every descriptor it guarded is rejected here anyway.
        let now_secs = now_ms / 1000;
        if descriptor.created_at < now_secs.saturating_sub(MAX_AGE_SECS) {
            return Some(DescriptorOutcome::Rejected(format!(
                "descriptor {} is older than the retention window",
                descriptor.id
            )));
        }

        // ZEB-831 / VF (ZEB-818): forward-skew gate. The age gate above is a
        // BACKWARD bound only; without a forward bound a future-dated
        // `created_at` survives ingest, is immune to the capacity-trim below
        // (which drops the *oldest* `created_at`, so honest entries are evicted
        // instead), and pins the top of the feed forever (`list_descriptors`
        // sorts `created_at` DESC). Reject anything dated further ahead than the
        // display-tier tolerance, mirroring the sibling pull-cursor bound
        // (`vine_pull_driver::VINE_PULL_INVALID_FORWARD_SKEW_SECS`). Seconds domain.
        if crate::clock_trust::reject_future_logged(
            descriptor.created_at.saturating_mul(1000),
            now_secs.saturating_mul(1000),
            crate::clock_trust::DISPLAY_SKEW_TOLERANCE_MS,
            "vine_feed.descriptor.created_at",
        ) {
            return Some(DescriptorOutcome::Rejected(format!(
                "descriptor {} is dated further ahead than the plausible clock window",
                descriptor.id
            )));
        }

        let source = if followed_set.contains(&descriptor.creator_address) {
            VineSource::Followed
        } else {
            VineSource::Discover
        };

        let vine_id = descriptor.id.clone();
        let dto = self.build_dto(&descriptor, source);
        self.descriptors.insert(
            vine_id.clone(),
            CachedVine {
                descriptor,
                received_at_ms: now_ms,
                source,
            },
        );

        // Runtime capacity-trim: if insert exceeded the cap, drop the
        // oldest descriptor(s) by `created_at` ascending (ties broken by
        // id ascending for cross-replica determinism), and drop their
        // reactions. Single-pass; runs only when len > MAX_DESCRIPTORS.
        if self.descriptors.len() > MAX_DESCRIPTORS {
            let mut entries: Vec<(u64, String)> = self
                .descriptors
                .iter()
                .map(|(id, cv)| (cv.descriptor.created_at, id.clone()))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            let drop_count = self.descriptors.len() - MAX_DESCRIPTORS;
            for (_, id) in entries.into_iter().take(drop_count) {
                self.descriptors.remove(&id);
                self.reactions.retain(|(vid, _), _| vid != &id);
            }
        }

        // If the capacity-trim removed the descriptor we just inserted
        // (older than every existing entry), the cache state is identical
        // to pre-insert. Return Rejected so the event loop does NOT emit
        // `vine-received` for a vine that isn't actually retained.
        // Skip save() — no net state change to persist.
        if !self.descriptors.contains_key(&vine_id) {
            return Some(DescriptorOutcome::Rejected(format!(
                "descriptor {vine_id} older than cache window (capacity-trim victim)"
            )));
        }

        self.save();
        Some(DescriptorOutcome::Inserted { dto: Box::new(dto) })
    }

    /// Return all cached descriptors as `VineVideoDto`, sorted by
    /// `created_at` DESC. `viewed` is populated by joining with the
    /// `self.viewed` HashSet (local-only viewed-state).
    pub fn list_descriptors(&self) -> Vec<VineVideoDto> {
        // ZEB-831/VF: forward-skew display gate, re-evaluated against the LIVE
        // clock on every call. A descriptor dated further ahead than the display
        // tolerance must not squat the feed head (`created_at` DESC) or displace
        // honest entries — but this is the *non-destructive* counterpart to the
        // ingest gate in `on_descriptor_sample`: hiding a future-dated descriptor
        // from the view never removes it from the store, so a corrected clock (or
        // a since-elapsed `created_at`) restores it (Greptile PR #580 P1 — a load-
        // time drop would let `save()` purge it permanently). Broken clock
        // (now_secs == 0) shows everything, matching the load path's age-filter
        // fallback ("keep data over hiding it").
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut out: Vec<VineVideoDto> = self
            .descriptors
            .values()
            .filter(|cv| {
                now_secs == 0
                    || !crate::clock_trust::reject_future(
                        cv.descriptor.created_at,
                        now_secs,
                        crate::clock_trust::DISPLAY_SKEW_TOLERANCE_SECS,
                    )
            })
            .map(|cv| VineVideoDto {
                id: cv.descriptor.id.clone(),
                creator_address: cv.descriptor.creator_address.clone(),
                creator_name: cv.descriptor.creator_name.clone(),
                created_at: cv.descriptor.created_at,
                video_cid: cv.descriptor.video_cid.clone(),
                title: cv.descriptor.title.clone(),
                reshare_of: cv.descriptor.reshare_of.clone(),
                viewed: self.viewed.contains(&cv.descriptor.id),
                original_creator_address: cv.descriptor.original_creator_address.clone(),
                original_creator_name: cv.descriptor.original_creator_name.clone(),
                original_removed: self.original_removed(&cv.descriptor),
                degree: self
                    .reach
                    .get(&cv.descriptor.creator_address)
                    .map(|r| r.degree),
                via: self
                    .reach
                    .get(&cv.descriptor.creator_address)
                    .map(|r| r.via.clone()),
            })
            .collect();
        out.sort_by_key(|v| std::cmp::Reverse(v.created_at));
        out
    }

    /// Cheap presence probe by vine id. `delete_vine_impl`'s post-tombstone
    /// reconcile hook polls this under the cache lock on a 25ms cadence —
    /// `list_descriptors` (full DTO clone + sort, up to MAX_DESCRIPTORS
    /// rows) is far too heavy for that loop (Qodo PR #564 round 1).
    /// Equivalent to `list_descriptors().iter().any(|v| v.id == vine_id)`:
    /// that listing maps over this same map unfiltered, and tombstone
    /// application removes the entry outright rather than marking it.
    pub fn has_descriptor(&self, vine_id: &str) -> bool {
        self.descriptors.contains_key(vine_id)
    }

    /// ZEB-811: paginated per-creator descriptors for vine-relay serving.
    /// Ascending `(created_at, id)` tuple order; returns only rows
    /// strictly greater than the `after` cursor, up to `limit` rows —
    /// the caller is responsible for clamping `limit` to a sane wire cap.
    /// Only rows carrying a retained `sig` or `device_sig` are served: a
    /// relay must not hand out an unverifiable row (legacy pre-ZEB-811
    /// disk rows are silently excluded rather than served bare).
    ///
    /// Uses a bounded top-k max-heap (`PageCandidate`) rather than
    /// collect-all-then-sort — see the method body's comment.
    pub fn descriptors_for_creator_page(
        &self,
        creator: &str,
        after: &(u64, String),
        limit: usize,
    ) -> Vec<VineDescriptorPayload> {
        if limit == 0 {
            return Vec::new();
        }
        // Bounded top-k via a size-`limit` max-heap (O(n log limit)) rather
        // than collect-all-then-full-sort (O(n log n)): vine-relay is
        // deliberately unauthenticated (module doc), so this page query is
        // an attacker-reachable hot path and a large cache must not turn a
        // small wire `limit` into an O(n log n) scan+sort on every request.
        // The heap holds at most `limit` candidates at any time; only the
        // final `limit`-sized result is sorted.
        let mut heap: BinaryHeap<PageCandidate> = BinaryHeap::with_capacity(limit + 1);
        for cv in self.descriptors.values() {
            if cv.descriptor.creator_address != creator {
                continue;
            }
            if cv.descriptor.sig.is_none() && cv.descriptor.device_sig.is_none() {
                continue;
            }
            let key = (cv.descriptor.created_at, cv.descriptor.id.as_str());
            if key <= (after.0, after.1.as_str()) {
                continue;
            }
            if heap.len() < limit {
                heap.push(PageCandidate { key, cv });
            } else if heap.peek().is_some_and(|max| key < max.key) {
                heap.pop();
                heap.push(PageCandidate { key, cv });
            }
        }
        heap.into_sorted_vec()
            .into_iter()
            .map(|c| c.cv.descriptor.clone())
            .collect()
    }

    /// ZEB-811: true when `cid_hex` is the `video_cid` of at least one
    /// signature-retaining cached descriptor from `creator` SPECIFICALLY.
    /// Backs the vine-relay serve ctx's content allowlist — resolved from
    /// THIS node's own cache, never from a requester's claim, so an
    /// anonymous peer cannot turn this node into an open blob-serving proxy
    /// for arbitrary CIDs. Mirrors the same sig-retained filter as
    /// `descriptors_for_creator_page`.
    ///
    /// ZEB-811 final review: scoped to `creator` (the caller passes its own
    /// address) rather than "any cached creator" — a v1 vine relay serves
    /// only its own creator's content, and the caller (`ProdVineRelayServeCtx`)
    /// already enforces the same scope on `descriptors_for_creator_page`;
    /// this scope must match or the content path would leak a broader
    /// allowlist than the descriptor path.
    pub fn video_cid_is_served(&self, creator: &str, cid_hex: &str) -> bool {
        self.descriptors.values().any(|cv| {
            cv.descriptor.creator_address == creator
                && (cv.descriptor.sig.is_some() || cv.descriptor.device_sig.is_some())
                && cv.descriptor.video_cid == cid_hex
        })
    }

    /// ZEB-811: max `received_at_ms` across `creator`'s cached rows — feeds
    /// the vine-relay pull driver's per-creator freshness check. `None`
    /// when no descriptor from `creator` is cached (never pulled, or all
    /// age/capacity-pruned).
    pub fn last_received_ms_for_creator(&self, creator: &str) -> Option<u64> {
        self.descriptors
            .values()
            .filter(|cv| cv.descriptor.creator_address == creator)
            .map(|cv| cv.received_at_ms)
            .max()
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
            original_creator_address: descriptor.original_creator_address.clone(),
            original_creator_name: descriptor.original_creator_name.clone(),
            original_removed: self.original_removed(descriptor),
            degree: self
                .reach
                .get(&descriptor.creator_address)
                .map(|r| r.degree),
            via: self
                .reach
                .get(&descriptor.creator_address)
                .map(|r| r.via.clone()),
        }
    }

    /// Parse + verify + insert/LWW-update a reaction.
    ///
    /// Returns `None` if `key_expr` is not a vine-reaction topic.
    /// ZEB-673: the record must be REACTOR-signed and the topic's
    /// `{vine_id}`/`{reactor}` segments must match the (verified)
    /// payload. LWW per (vine_id, reactor_addr) by `timestamp`. Stale
    /// samples (timestamp older than existing entry) return `Stale` and
    /// do NOT mutate the cache.
    pub fn on_reaction_sample(
        &mut self,
        key_expr: &str,
        payload: &[u8],
        now_secs: u64,
    ) -> Option<ReactionOutcome> {
        if !(key_expr.starts_with("harmony/vines/") && key_expr.contains("/reactions/")) {
            return None;
        }

        let reaction: VineReactionPayload = match serde_json::from_slice(payload) {
            Ok(r) => r,
            Err(e) => {
                return Some(ReactionOutcome::Rejected(format!(
                    "reaction parse failed: {e}"
                )))
            }
        };

        // ZEB-673: strict wire admission — reactor signature first (no
        // state effects on failure), then bind the topic's trailing
        // `{vine_id}/{reactor}` segments to the verified payload so a
        // valid record can't be replayed under another vine's key. The
        // leading `{creator}` segment names the topic OWNER, which the
        // payload doesn't carry — cross-creator replay of the same
        // (vine_id, reactor) tuple is idempotent by construction.
        // ZEB-678 S2 (review-fix, CodeRabbit security): the `#3` signature binds
        // `reactor_address` and is ALWAYS required. `verify_reaction_v2` alone
        // recovers an enrolled device from the carried enrollment but never ties
        // it to `reactor_address`, so any enrolled device could mint a reaction
        // for an arbitrary address (dedup + revocation key off `reactor_address`).
        // When a migrated reaction also carries `device_sig`, the `#2` layer is
        // verified as defense-in-depth. Revocation is best-effort (§3.3): rejected
        // only when we DO hold the reactor's authority record marking its device
        // revoked — keyed by the now `#3`-bound reactor_address.
        let verify_result = crate::vine_signing::verify_reaction(&reaction)
            .and_then(|()| {
                if reaction.device_sig.is_some() {
                    crate::vine_signing::verify_reaction_v2(&reaction, now_secs)
                } else {
                    Ok(())
                }
            })
            .and_then(|()| match self.authority.get(&reaction.reactor_address) {
                Some(p) if p.revoked => {
                    Err("reaction rejected: reactor device revoked".to_string())
                }
                _ => Ok(()),
            });
        if let Err(e) = verify_result {
            return Some(ReactionOutcome::Rejected(e));
        }
        let mut tail = key_expr
            .split("/reactions/")
            .nth(1)
            .unwrap_or("")
            .split('/');
        let topic_vine = tail.next().unwrap_or("");
        let topic_reactor = tail.next().unwrap_or("");
        // Exact shape: no segments may follow `{vine_id}/{reactor}` — a
        // valid record replayed under a deeper non-canonical key must not
        // pass the binding gate (Qodo PR #446 round 1; the subscription
        // is narrowed to `*/*` too, this is defense in depth at the
        // admission authority).
        if topic_vine != reaction.vine_id
            || topic_reactor != reaction.reactor_address
            || tail.next().is_some()
        {
            return Some(ReactionOutcome::Rejected(format!(
                "reaction topic '{key_expr}' does not match payload '{}/{}'",
                reaction.vine_id, reaction.reactor_address
            )));
        }

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
                self.save();
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
                self.save();
                Some(ReactionOutcome::UpdatedNewer)
            }
        }
    }

    /// ZEB-670: parse + verify + apply a creator tombstone.
    ///
    /// Returns `None` if `key_expr` is not a vine-tombstone topic.
    /// Verification: signature (pubkey→address binding + `verify_strict`,
    /// see `vine_tombstone::verify_tombstone`), the topic's `{creator}`
    /// segment must match the payload's `creator_address`, and — when the
    /// target vine is cached — the tombstone's creator must own it.
    ///
    /// Applying removes the descriptor, its reactions, and its viewed
    /// entry, and records the tombstone (persisted; pre-arrival-proof).
    /// See `TombstoneOutcome` for the removed/evict_cid contract.
    pub fn on_tombstone_sample(
        &mut self,
        key_expr: &str,
        payload: &[u8],
    ) -> Option<TombstoneOutcome> {
        if !(key_expr.starts_with("harmony/vines/") && key_expr.contains("/tombstones/")) {
            return None;
        }

        let t: crate::vine_tombstone::VineTombstonePayload = match serde_json::from_slice(payload) {
            Ok(t) => t,
            Err(e) => {
                return Some(TombstoneOutcome::Rejected(format!(
                    "tombstone parse failed: {e}"
                )))
            }
        };

        // ZEB-678 S2: dual-path. Authority cached for this feed ⇒ require the
        // `#2` device_sig against the pinned publisher_key AND a non-revoked
        // device; else verify the legacy `#3` tombstone. A migrated tombstone
        // dual-signs, so both branches have material to verify.
        let tomb_feed = key_expr
            .strip_prefix("harmony/vines/")
            .unwrap_or_default()
            .split('/')
            .next()
            .unwrap_or_default();
        let verify_result = match self.authority.get(tomb_feed) {
            Some(p) if p.revoked => Err("tombstone rejected: publisher device revoked".to_string()),
            Some(p) => crate::vine_tombstone::verify_tombstone_v2(&t, &p.publisher_key),
            None => crate::vine_tombstone::verify_tombstone(&t),
        };
        if let Err(e) = verify_result {
            return Some(TombstoneOutcome::Rejected(e));
        }

        // Both topic segments must match the (verified) payload — the key
        // is not authoritative, so a tombstone re-published under someone
        // else's topic (or a mismatched vine-id segment) is dropped rather
        // than trusted on payload content alone (Qodo PR #445 round 1).
        let mut segments = key_expr
            .strip_prefix("harmony/vines/")
            .unwrap_or_default()
            .split('/');
        let topic_creator = segments.next().unwrap_or_default();
        let topic_vine_id = segments.nth(1).unwrap_or_default(); // skip "tombstones"
        if topic_creator != t.creator_address {
            return Some(TombstoneOutcome::Rejected(format!(
                "tombstone topic creator {topic_creator} != payload creator {}",
                t.creator_address
            )));
        }
        if topic_vine_id != t.vine_id {
            return Some(TombstoneOutcome::Rejected(format!(
                "tombstone topic vine id {topic_vine_id} != payload vine id {}",
                t.vine_id
            )));
        }

        if self.tombstones.contains_key(&t.vine_id) {
            return Some(TombstoneOutcome::AlreadyApplied);
        }

        // A tombstone can only remove a vine its signer authored. (For
        // uncached vines this check is vacuous — but the signature still
        // binds the tombstone to the creator address, and insert-time
        // spoofing is ZEB-673's problem, not ours.)
        if let Some(cached) = self.descriptors.get(&t.vine_id) {
            if cached.descriptor.creator_address != t.creator_address {
                return Some(TombstoneOutcome::Rejected(format!(
                    "tombstone creator {} does not own vine {}",
                    t.creator_address, t.vine_id
                )));
            }
        }

        // Canonical CID: prefer the cached descriptor's — that's what our
        // local rows (chain-stub matching) and blob state actually
        // reference; the unsigned-descriptor world means the two can
        // disagree (Qodo PR #445 round 1). Falls back to the signed
        // payload's CID for pre-arrival tombstones.
        let removed_descriptor = self.descriptors.remove(&t.vine_id);
        let canonical_cid = removed_descriptor
            .as_ref()
            .map(|cv| cv.descriptor.video_cid.clone())
            .unwrap_or_else(|| t.video_cid.clone());

        self.tombstones.insert(
            t.vine_id.clone(),
            TombstoneRecord {
                video_cid: canonical_cid.clone(),
                creator_address: t.creator_address.clone(),
                deleted_at: t.deleted_at,
            },
        );
        // Cap enforcement — oldest-by-deleted_at trims first (ties by
        // vine_id for determinism), mirroring the descriptor cap.
        if self.tombstones.len() > MAX_TOMBSTONES {
            let mut entries: Vec<(u64, String)> = self
                .tombstones
                .iter()
                .map(|(id, rec)| (rec.deleted_at, id.clone()))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            let drop_count = self.tombstones.len() - MAX_TOMBSTONES;
            for (_, id) in entries.into_iter().take(drop_count) {
                self.tombstones.remove(&id);
            }
        }

        // Emitted for EVERY newly applied tombstone — a subscriber holding
        // only reshares still needs the event to mark stubs (CodeRabbit
        // PR #445 round 1).
        let removed = RemovedVine {
            vine_id: t.vine_id.clone(),
            video_cid: canonical_cid.clone(),
            creator_address: t.creator_address.clone(),
        };
        self.reactions.retain(|(vid, _), _| vid != &t.vine_id);
        self.viewed.remove(&t.vine_id);

        // Evict the blob only when every remaining descriptor referencing
        // the CID is a stub (original_removed). Stubs never play, so they
        // don't need the bytes; what blocks eviction is any LIVE
        // reference — an unrelated original with the same content, the
        // still-live original when a user deletes their own reshare, or
        // a re-published original (same creator + CID, new id). Computed
        // even for pre-arrival tombstones: reshares may hold the bytes,
        // and evicting never-held bytes is a no-op.
        let live_ref = self.descriptors.values().any(|cv| {
            cv.descriptor.video_cid == canonical_cid && !self.original_removed(&cv.descriptor)
        });
        let evict_cid = (!live_ref).then_some(canonical_cid);

        self.save();
        Some(TombstoneOutcome::Applied { removed, evict_cid })
    }

    /// ZEB-671: admit a follow-list sample from `harmony/vines/*/follows`.
    /// Verify-first — signature, pubkey→address binding, and exact topic
    /// shape all precede ANY state effect. LWW by `updated_at` per owner;
    /// oversized lists are rejected; the map is bounded by
    /// `MAX_FOLLOW_LISTS` (stalest evicted).
    pub fn on_follow_list_sample(&mut self, key_expr: &str, payload: &[u8]) -> FollowListOutcome {
        // Byte cap BEFORE parsing — the topic is peer-writable, so an
        // oversized document must not get free serde work.
        if payload.len() > MAX_FOLLOW_LIST_WIRE_BYTES {
            return FollowListOutcome::Rejected(format!(
                "follow list payload exceeds wire cap: {} > {MAX_FOLLOW_LIST_WIRE_BYTES} bytes",
                payload.len()
            ));
        }
        let list: crate::VineFollowListPayload = match serde_json::from_slice(payload) {
            Ok(l) => l,
            Err(e) => return FollowListOutcome::Rejected(format!("follow list parse failed: {e}")),
        };

        // ZEB-678 S2: dual-path. Authority cached for this owner's feed ⇒
        // require the `#2` device_sig against the pinned publisher_key AND a
        // non-revoked device; else verify the legacy `#3`. A migrated follow
        // list dual-signs (best-effort via try_lock at publish), so the `#2`
        // branch normally has material; a rare `#3`-only list on a migrated feed
        // is rejected here and re-migrates on the next publish.
        let verify_result = match self.authority.get(&list.owner_address) {
            Some(p) if p.revoked => Err("follow list rejected: owner device revoked".to_string()),
            Some(p) => crate::vine_signing::verify_follow_list_v2(&list, &p.publisher_key),
            None => crate::vine_signing::verify_follow_list(&list),
        };
        if let Err(e) = verify_result {
            return FollowListOutcome::Rejected(e);
        }

        // Exact topic shape: harmony/vines/{owner}/follows — the owner
        // segment must equal the (verified) payload owner, and no extra
        // segments are tolerated (same posture as reactions, ZEB-673).
        let mut segments = key_expr
            .strip_prefix("harmony/vines/")
            .unwrap_or_default()
            .split('/');
        let topic_owner = segments.next().unwrap_or_default();
        let follows_seg = segments.next().unwrap_or_default();
        if topic_owner != list.owner_address
            || follows_seg != "follows"
            || segments.next().is_some()
        {
            return FollowListOutcome::Rejected(format!(
                "follow-list topic '{key_expr}' does not match payload owner '{}'",
                list.owner_address
            ));
        }

        if list.follows.len() > MAX_FOLLOWS_PER_LIST {
            return FollowListOutcome::Rejected(format!(
                "follow list exceeds cap: {} > {MAX_FOLLOWS_PER_LIST}",
                list.follows.len()
            ));
        }

        let outcome = match self.follow_lists.get(&list.owner_address) {
            // `>=`: an equal-timestamp re-arrival is a no-op, so replayed
            // records can never churn state or trigger a save.
            Some(existing) if existing.updated_at >= list.updated_at => {
                return FollowListOutcome::IgnoredOlder;
            }
            Some(_) => FollowListOutcome::UpdatedNewer,
            None => FollowListOutcome::Inserted,
        };

        self.follow_lists.insert(
            list.owner_address,
            FollowListEntry {
                follows: list.follows,
                updated_at: list.updated_at,
            },
        );

        // Cap enforcement — stalest updated_at evicted first (ties by
        // owner for determinism), mirroring the tombstone cap.
        while self.follow_lists.len() > MAX_FOLLOW_LISTS {
            let Some(stalest) = self
                .follow_lists
                .iter()
                .map(|(owner, e)| (e.updated_at, owner.clone()))
                .min()
                .map(|(_, owner)| owner)
            else {
                break;
            };
            self.follow_lists.remove(&stalest);
        }

        self.save();
        outcome
    }

    /// ZEB-671: verified follow lists keyed by owner — the edge source
    /// for the Discover graph computation.
    pub fn follow_lists(&self) -> &HashMap<String, FollowListEntry> {
        &self.follow_lists
    }

    /// ZEB-671: install the viewer-side graph inputs (own address +
    /// LOCAL follows) and recompute reachability. Returns `true` when
    /// the derived reach map changed. Called from start_node (once the
    /// node identity is known) and from the follow/unfollow IPCs.
    pub fn set_graph_inputs(&mut self, me: String, my_follows: Vec<String>) -> bool {
        self.graph_me = me;
        self.graph_my_follows = my_follows;
        self.recompute_reach()
    }

    /// ZEB-671: recompute Discover reachability from the stored graph
    /// inputs over the cached follow lists. Returns `true` on change.
    /// Cheap no-op before `set_graph_inputs` (empty root set → empty
    /// reach).
    pub fn recompute_reach(&mut self) -> bool {
        let next = crate::vine_follow_graph::compute_reach(
            &self.graph_me,
            &self.graph_my_follows,
            |owner| self.follow_lists.get(owner).map(|e| e.follows.as_slice()),
        );
        if next == self.reach {
            return false;
        }
        self.reach = next;
        true
    }

    /// ZEB-670: true when descriptor `d` is a reshare whose original was
    /// tombstoned by its creator. Two matches:
    ///   1. direct — `reshare_of` names a tombstoned vine id;
    ///   2. chain — the origin creator (original_creator_address, falling
    ///      back to creator_address) has a tombstone for this exact
    ///      video_cid AND no live original for it. The live-original
    ///      exception un-stubs reshares of a RE-published original (same
    ///      creator, same CID, new id); old reshares of the tombstoned id
    ///      stay stubbed via the direct match.
    fn original_removed(&self, d: &VineDescriptorPayload) -> bool {
        let Some(src_id) = d.reshare_of.as_deref() else {
            return false;
        };
        if self.tombstones.contains_key(src_id) {
            return true;
        }
        let origin = d
            .original_creator_address
            .as_deref()
            .unwrap_or(&d.creator_address);
        let content_retracted = self
            .tombstones
            .values()
            .any(|t| t.creator_address == origin && t.video_cid == d.video_cid);
        content_retracted
            && !self.descriptors.values().any(|cv| {
                cv.descriptor.reshare_of.is_none()
                    && cv.descriptor.creator_address == origin
                    && cv.descriptor.video_cid == d.video_cid
            })
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

    /// All cached reaction rows (liked and unliked), sorted by
    /// `(vine_id, reactor_address)` for deterministic output. Orphan rows
    /// never appear: `load()` prunes reactions whose descriptor was
    /// dropped. ZEB-672: feeds the `list_vine_reactions` IPC so a
    /// restarted frontend can rebuild its reaction map — live
    /// `vine-reaction-received` events never re-fire for reloaded rows.
    pub fn list_reactions(&self) -> Vec<VineReactionRow> {
        let mut rows: Vec<VineReactionRow> = self
            .reactions
            .iter()
            .map(|((vine_id, reactor_address), r)| VineReactionRow {
                vine_id: vine_id.clone(),
                reactor_address: reactor_address.clone(),
                reactor_name: r.reactor_name.clone(),
                liked: r.liked,
                timestamp: r.timestamp,
            })
            .collect();
        rows.sort_by(|a, b| {
            (a.vine_id.as_str(), a.reactor_address.as_str())
                .cmp(&(b.vine_id.as_str(), b.reactor_address.as_str()))
        });
        rows
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
        let newly_added = self.viewed.insert(vine_id);
        if newly_added {
            self.save();
        }
        newly_added
    }

    /// Atomic save: serialize cache state, write to `<path>.tmp`, rename
    /// to `<path>`. No-op when `self.path.is_none()` (test path).
    ///
    /// Errors are logged via `tracing::warn!` but never propagated — this
    /// matches the `follows.rs` / `content_index.rs` philosophy: persistence
    /// is best-effort; a failed save must not crash the dispatch loop.
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
                    original_creator_address: cv.descriptor.original_creator_address.clone(),
                    original_creator_name: cv.descriptor.original_creator_name.clone(),
                    identity_pub: cv.descriptor.identity_pub.clone(),
                    sig: cv.descriptor.sig.clone(),
                    device_sig: cv.descriptor.device_sig.clone(),
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
            tombstones: self
                .tombstones
                .iter()
                .map(|(vine_id, t)| TombstoneOnDisk {
                    vine_id: vine_id.clone(),
                    video_cid: t.video_cid.clone(),
                    creator_address: t.creator_address.clone(),
                    deleted_at: t.deleted_at,
                })
                .collect(),
            follow_lists: self
                .follow_lists
                .iter()
                .map(|(owner, e)| FollowListOnDisk {
                    owner: owner.clone(),
                    follows: e.follows.clone(),
                    updated_at: e.updated_at,
                })
                .collect(),
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

    /// Test-only bulk tombstone seeding (bypasses crypto) so the
    /// MAX_TOMBSTONES trim can be exercised without 10k Ed25519 ops.
    #[cfg(test)]
    pub fn seed_tombstone_for_test(&mut self, vine_id: &str, deleted_at: u64) {
        self.tombstones.insert(
            vine_id.to_string(),
            TombstoneRecord {
                video_cid: format!("cid-{vine_id}"),
                creator_address: "seeded".into(),
                deleted_at,
            },
        );
    }

    /// Test-only tombstone-count probe.
    #[cfg(test)]
    pub fn len_tombstones(&self) -> usize {
        self.tombstones.len()
    }

    /// ZEB-811: test-only descriptor seeder — inserts `descriptor` directly
    /// into the cache, bypassing `on_descriptor_sample`'s signature-
    /// verification pipeline entirely. Callers set `sig`/`device_sig` on the
    /// payload themselves to control whether a row is signature-retaining
    /// (served) or not — this exists to test the SERVE-SIDE filters
    /// (`descriptors_for_creator_page`, `video_cid_is_served`), not
    /// signature correctness (covered separately by the real-ingest tests).
    #[cfg(test)]
    pub fn seed_descriptor_for_test(&mut self, descriptor: VineDescriptorPayload) {
        self.descriptors.insert(
            descriptor.id.clone(),
            CachedVine {
                descriptor,
                received_at_ms: 0,
                source: VineSource::Followed,
            },
        );
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

    /// ZEB-673: strict wire admission requires real signer identities,
    /// but the tests' readable creator names ("alice-addr") are worth
    /// keeping. This registry lazily mints one identity per name;
    /// `addr()` translates a name to its signer's derived hex address.
    /// Process-global so every mention of a name agrees on the identity.
    /// Identities minted by `tombstone_identity()` self-register under
    /// their DERIVED address, so a derived address passed back in as a
    /// "name" maps to itself.
    type SignerRegistry =
        std::collections::HashMap<String, std::sync::Arc<harmony_identity::PrivateIdentity>>;

    fn signers() -> &'static std::sync::Mutex<SignerRegistry> {
        static SIGNERS: std::sync::OnceLock<std::sync::Mutex<SignerRegistry>> =
            std::sync::OnceLock::new();
        SIGNERS.get_or_init(Default::default)
    }

    fn signer_for(name: &str) -> std::sync::Arc<harmony_identity::PrivateIdentity> {
        let mut guard = signers().lock().unwrap();
        guard
            .entry(name.to_string())
            .or_insert_with(|| {
                std::sync::Arc::new(harmony_identity::PrivateIdentity::generate(
                    &mut rand::rngs::OsRng,
                ))
            })
            .clone()
    }

    /// Derived hex address for a named test signer.
    fn addr(name: &str) -> String {
        hex::encode(signer_for(name).public_identity().address_hash)
    }

    /// Descriptor topic for a named test signer.
    fn topic(name: &str) -> String {
        format!("harmony/vines/{}", addr(name))
    }

    /// Build a canonical descriptor JSON payload for `creator` + `vine_id`.
    ///
    /// Mirrors the bytes that production `publish_vine` produces: the
    /// payload carries the DERIVED address of the named signer (see
    /// `addr`) and is creator-signed (ZEB-673). `original_creator` is
    /// also a signer NAME, translated the same way.
    #[allow(clippy::too_many_arguments)] // test helper — readable named-positional args beat a struct here
    fn canonical_descriptor_bytes(
        vine_id: &str,
        creator: &str,
        creator_name: &str,
        video_cid: &str,
        title: Option<&str>,
        reshare_of: Option<&str>,
        created_at: u64,
        original_creator: Option<&str>,
        original_creator_name: Option<&str>,
    ) -> Vec<u8> {
        let signer = signer_for(creator);
        let mut v = crate::VineDescriptorPayload {
            id: vine_id.to_string(),
            creator_address: addr(creator),
            creator_name: creator_name.to_string(),
            created_at,
            video_cid: video_cid.to_string(),
            title: title.map(String::from),
            reshare_of: reshare_of.map(String::from),
            original_creator_address: original_creator.map(addr),
            original_creator_name: original_creator_name.map(String::from),
            identity_pub: None,
            sig: None,
            device_sig: None,
        };
        crate::vine_signing::sign_descriptor(&signer, &mut v);
        serde_json::to_vec(&v).unwrap()
    }

    /// Followed-set of named test signers (translated to derived addresses).
    fn followed_set_with(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| addr(s)).collect()
    }

    // ── ZEB-673: strict wire admission ───────────────────────────────

    #[test]
    fn unsigned_wire_descriptor_is_rejected() {
        let mut cache = VineFeedCache::new();
        let v = crate::VineDescriptorPayload {
            id: "vine-unsigned-1".into(),
            creator_address: addr("alice-addr"),
            creator_name: "Alice".into(),
            created_at: 1_700_000_000,
            video_cid: "cid-aaa".into(),
            title: None,
            reshare_of: None,
            original_creator_address: None,
            original_creator_name: None,
            identity_pub: None,
            sig: None,
            device_sig: None,
        };
        let out = cache.on_descriptor_sample(
            &topic("alice-addr"),
            &serde_json::to_vec(&v).unwrap(),
            &HashSet::new(),
            1_000,
        );
        assert!(
            matches!(out, Some(DescriptorOutcome::Rejected(ref r)) if r.contains("unsigned")),
            "got {out:?}"
        );
        assert!(cache.list_descriptors().is_empty());
    }

    #[test]
    fn tampered_descriptor_signature_is_rejected() {
        let mut cache = VineFeedCache::new();
        let bytes = canonical_descriptor_bytes(
            "vine-tamper-1",
            "alice-addr",
            "Alice",
            "cid-aaa",
            Some("original title"),
            None,
            1_700_000_000,
            None,
            None,
        );
        let mut v: crate::VineDescriptorPayload = serde_json::from_slice(&bytes).unwrap();
        v.title = Some("attacker title".into());
        let out = cache.on_descriptor_sample(
            &topic("alice-addr"),
            &serde_json::to_vec(&v).unwrap(),
            &HashSet::new(),
            1_000,
        );
        assert!(
            matches!(out, Some(DescriptorOutcome::Rejected(ref r)) if r.contains("signature invalid")),
            "got {out:?}"
        );
    }

    #[test]
    fn descriptor_replayed_on_foreign_topic_is_rejected() {
        let mut cache = VineFeedCache::new();
        let bytes = canonical_descriptor_bytes(
            "vine-replay-1",
            "alice-addr",
            "Alice",
            "cid-aaa",
            None,
            None,
            1_700_000_000,
            None,
            None,
        );
        // Valid signature, wrong topic: republished under Bob's key.
        let out = cache.on_descriptor_sample(&topic("bob-addr"), &bytes, &HashSet::new(), 1_000);
        assert!(
            matches!(out, Some(DescriptorOutcome::Rejected(ref r)) if r.contains("does not match payload creator")),
            "got {out:?}"
        );
    }

    // ── ZEB-811: signature retention + per-creator relay pages ─────────

    #[test]
    fn disk_round_trip_retains_wire_signatures() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let followed = followed_set_with(&["alice-addr"]);
        // `load()` age-prunes against the REAL wall clock (unlike
        // on_descriptor_sample's synthetic now_ms), so created_at must be
        // recent — see descriptor_insert_persists_to_disk for precedent.
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        {
            let mut cache = VineFeedCache::load(dir.path());
            let desc = canonical_descriptor_bytes(
                "vine-sig-rt",
                "alice-addr",
                "Alice",
                "cid-sig-rt",
                None,
                None,
                now_secs.saturating_sub(60),
                None,
                None,
            );
            let out =
                cache.on_descriptor_sample(&topic("alice-addr"), &desc, &followed, now_secs * 1000);
            assert!(
                matches!(out, Some(DescriptorOutcome::Inserted { .. })),
                "got {out:?}"
            );
        }

        let cache2 = VineFeedCache::load(dir.path());
        assert_eq!(cache2.len_descriptors(), 1);
        let page =
            cache2.descriptors_for_creator_page(&addr("alice-addr"), &(0, String::new()), 10);
        assert_eq!(
            page.len(),
            1,
            "signed row must survive the reload and be served"
        );
        assert_eq!(page[0].id, "vine-sig-rt");
        assert!(
            page[0].sig.is_some(),
            "sig must survive the disk round trip"
        );
        assert!(
            page[0].identity_pub.is_some(),
            "identity_pub must survive the disk round trip"
        );
    }

    #[test]
    fn legacy_disk_rows_without_sigs_still_load_and_are_not_served() {
        let dir = tempfile::tempdir().expect("create tempdir");
        // `load()` age-prunes against the REAL wall clock, so createdAt must
        // be recent (see disk_round_trip_retains_wire_signatures).
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let disk = serde_json::json!({
            "version": 1,
            "descriptors": [
                {
                    "id": "vine-legacy",
                    "creatorAddress": addr("alice-addr"),
                    "creatorName": "Alice",
                    "createdAt": now_secs.saturating_sub(60),
                    "videoCid": "cid-legacy",
                    "receivedAtMs": 500,
                    "source": "followed"
                    // no identityPub/sig/deviceSig — pre-ZEB-811 row shape
                }
            ],
            "reactions": [],
            "viewed": []
        });
        std::fs::write(
            dir.path().join(VINE_FEED_FILE),
            serde_json::to_vec_pretty(&disk).unwrap(),
        )
        .unwrap();

        let cache = VineFeedCache::load(dir.path());
        assert_eq!(cache.len_descriptors(), 1);
        assert_eq!(
            cache.list_descriptors()[0].id,
            "vine-legacy",
            "legacy row still loads and lists for local display"
        );

        let page = cache.descriptors_for_creator_page(&addr("alice-addr"), &(0, String::new()), 10);
        assert!(
            page.is_empty(),
            "legacy unsigned row must not be served by the relay page accessor"
        );
    }

    #[test]
    fn creator_page_orders_by_tuple_and_breaks_created_at_ties_by_id() {
        let mut cache = VineFeedCache::new();
        let followed = followed_set_with(&["alice-addr", "bob-addr"]);

        for (id, created_at) in [("b", 10u64), ("a", 10u64), ("c", 11u64)] {
            let desc = canonical_descriptor_bytes(
                id,
                "alice-addr",
                "Alice",
                &format!("cid-{id}"),
                None,
                None,
                created_at,
                None,
                None,
            );
            let out = cache.on_descriptor_sample(&topic("alice-addr"), &desc, &followed, 1_000);
            assert!(
                matches!(out, Some(DescriptorOutcome::Inserted { .. })),
                "insert {id} failed: {out:?}"
            );
        }
        // A second creator's row must never appear in Alice's page.
        let bob_desc = canonical_descriptor_bytes(
            "bob-1",
            "bob-addr",
            "Bob",
            "cid-bob-1",
            None,
            None,
            10,
            None,
            None,
        );
        let out = cache.on_descriptor_sample(&topic("bob-addr"), &bob_desc, &followed, 1_000);
        assert!(matches!(out, Some(DescriptorOutcome::Inserted { .. })));

        let alice = addr("alice-addr");
        let page1 = cache.descriptors_for_creator_page(&alice, &(0, String::new()), 2);
        let ids1: Vec<&str> = page1.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids1, vec!["a", "b"]);

        let page2 = cache.descriptors_for_creator_page(&alice, &(10, "b".to_string()), 2);
        let ids2: Vec<&str> = page2.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids2, vec!["c"]);

        assert!(
            page1
                .iter()
                .chain(page2.iter())
                .all(|d| d.creator_address == alice),
            "Bob's descriptor must never appear on Alice's page"
        );
    }

    #[test]
    fn last_received_ms_tracks_the_freshest_row_per_creator() {
        let mut cache = VineFeedCache::new();
        let followed = followed_set_with(&["alice-addr"]);

        let d1 = canonical_descriptor_bytes(
            "vine-lr-1",
            "alice-addr",
            "Alice",
            "cid-lr-1",
            None,
            None,
            10,
            None,
            None,
        );
        let d2 = canonical_descriptor_bytes(
            "vine-lr-2",
            "alice-addr",
            "Alice",
            "cid-lr-2",
            None,
            None,
            11,
            None,
            None,
        );
        // Insert the higher `received_at_ms` FIRST so a naive "last inserted
        // wins" implementation would report the wrong (lower) value.
        let out1 = cache.on_descriptor_sample(&topic("alice-addr"), &d1, &followed, 5_000);
        assert!(matches!(out1, Some(DescriptorOutcome::Inserted { .. })));
        let out2 = cache.on_descriptor_sample(&topic("alice-addr"), &d2, &followed, 1_000);
        assert!(matches!(out2, Some(DescriptorOutcome::Inserted { .. })));

        assert_eq!(
            cache.last_received_ms_for_creator(&addr("alice-addr")),
            Some(5_000),
            "max received_at_ms must win, not the most recently inserted row"
        );
        assert_eq!(
            cache.last_received_ms_for_creator(&addr("bob-addr")),
            None,
            "a creator with no cached rows must report None"
        );
    }

    #[test]
    fn unsigned_wire_reaction_is_rejected() {
        let mut cache = VineFeedCache::new();
        let r = crate::VineReactionPayload {
            vine_id: "vine-1".into(),
            reactor_address: addr("bob-addr"),
            reactor_name: "Bob".into(),
            liked: true,
            timestamp: 100,
            identity_pub: None,
            sig: None,
            owner_id: None,
            enrollment_cbor_hex: None,
            signer_certs_cbor_hex: String::new(),
            device_sig: None,
        };
        let out = cache.on_reaction_sample(
            &reaction_topic("alice-addr", "vine-1", "bob-addr"),
            &serde_json::to_vec(&r).unwrap(),
            0,
        );
        assert!(
            matches!(out, Some(ReactionOutcome::Rejected(ref e)) if e.contains("unsigned")),
            "got {out:?}"
        );
        assert_eq!(cache.len_reactions(), 0);
    }

    #[test]
    fn reaction_replayed_under_other_vine_or_reactor_is_rejected() {
        let mut cache = VineFeedCache::new();
        let bytes = canonical_reaction_bytes("vine-1", "bob-addr", "Bob", true, 100);
        // Valid signature, topic vine segment differs from payload.
        let out = cache.on_reaction_sample(
            &reaction_topic("alice-addr", "vine-OTHER", "bob-addr"),
            &bytes,
            0,
        );
        assert!(
            matches!(out, Some(ReactionOutcome::Rejected(ref e)) if e.contains("does not match payload")),
            "got {out:?}"
        );
        // Valid signature, topic reactor segment names someone else.
        let out = cache.on_reaction_sample(
            &reaction_topic("alice-addr", "vine-1", "carol-addr"),
            &bytes,
            0,
        );
        assert!(
            matches!(out, Some(ReactionOutcome::Rejected(ref e)) if e.contains("does not match payload")),
            "got {out:?}"
        );
        // Valid signature, matching first two segments, but replayed
        // under a DEEPER non-canonical key (the old `**` subscription
        // delivered these; Qodo PR #446 round 1).
        let deep = format!(
            "{}/extra",
            reaction_topic("alice-addr", "vine-1", "bob-addr")
        );
        let out = cache.on_reaction_sample(&deep, &bytes, 0);
        assert!(
            matches!(out, Some(ReactionOutcome::Rejected(ref e)) if e.contains("does not match payload")),
            "got {out:?}"
        );
        assert_eq!(cache.len_reactions(), 0);
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
            None,
            None,
        );
        let followed = followed_set_with(&["alice-addr"]);

        let outcome = cache.on_descriptor_sample(
            &topic("alice-addr"),
            &payload,
            &followed,
            1_700_000_000_000,
        );

        match outcome {
            Some(DescriptorOutcome::Inserted { dto }) => {
                assert_eq!(dto.id, "vine-1");
                assert_eq!(dto.creator_address, addr("alice-addr"));
                assert_eq!(dto.source, VineSource::Followed);
                assert!(!dto.viewed);
            }
            other => panic!("expected Inserted, got {:?}", other),
        }
        assert_eq!(cache.len_descriptors(), 1);
    }

    #[test]
    fn future_dated_descriptor_beyond_display_skew_is_rejected() {
        // ZEB-831 / VF (ZEB-818): a descriptor whose created_at is further ahead
        // than the display-tier tolerance must be rejected at ingest — otherwise
        // it pins the top of the feed forever and is immune to capacity-trim.
        let mut cache = VineFeedCache::new();
        let now_secs: u64 = 1_700_000_000;
        let now_ms = now_secs * 1000;
        let followed = followed_set_with(&["alice-addr"]);

        // A legit, recent descriptor inserts.
        let legit = canonical_descriptor_bytes(
            "vine-legit",
            "alice-addr",
            "Alice",
            "cid-legit",
            None,
            None,
            now_secs,
            None,
            None,
        );
        assert!(matches!(
            cache.on_descriptor_sample(&topic("alice-addr"), &legit, &followed, now_ms),
            Some(DescriptorOutcome::Inserted { .. })
        ));

        // A poisoned descriptor dated one second past the display tolerance is rejected.
        let poisoned = canonical_descriptor_bytes(
            "vine-poison",
            "alice-addr",
            "Alice",
            "cid-poison",
            None,
            None,
            now_secs + crate::clock_trust::DISPLAY_SKEW_TOLERANCE_SECS + 1,
            None,
            None,
        );
        match cache.on_descriptor_sample(&topic("alice-addr"), &poisoned, &followed, now_ms) {
            Some(DescriptorOutcome::Rejected(_)) => {}
            other => panic!("expected Rejected for future-dated descriptor, got {other:?}"),
        }

        // The poisoned descriptor never entered the cache; the legit one still leads.
        let listed = cache.list_descriptors();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "vine-legit");

        // Boundary: exactly at the tolerance ceiling is accepted.
        let edge = canonical_descriptor_bytes(
            "vine-edge",
            "alice-addr",
            "Alice",
            "cid-edge",
            None,
            None,
            now_secs + crate::clock_trust::DISPLAY_SKEW_TOLERANCE_SECS,
            None,
            None,
        );
        assert!(matches!(
            cache.on_descriptor_sample(&topic("alice-addr"), &edge, &followed, now_ms),
            Some(DescriptorOutcome::Inserted { .. })
        ));
    }

    #[test]
    fn load_retains_future_dated_descriptor_and_display_excludes_it() {
        // ZEB-831/VF (Greptile PR #580 P1): a future-dated descriptor persisted by
        // a pre-gate build — or by this build under a since-corrected clock — must
        // be RETAINED across reload and write-back, never deleted on a clock
        // reading. It is hidden from the displayed feed (`list_descriptors`) while
        // it looks future-dated and reappears once the clock is corrected. The old
        // load-time drop let the next `save()` rewrite `vine_feed.json` from the
        // filtered state, permanently purging it (and, on a clock set *behind* real
        // time, the entire honest feed).
        let dir = tempfile::tempdir().expect("tempdir");
        let now_real = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let followed = followed_set_with(&["alice-addr"]);

        {
            let mut cache = VineFeedCache::load(dir.path());

            // A far-future descriptor (400 days ahead). It only reaches disk
            // because at insert time we feed a matching future `now_ms`, so the
            // runtime gate passes it — modeling state a pre-gate build persisted.
            let future_secs = now_real + 400 * 86_400;
            let future = canonical_descriptor_bytes(
                "vine-future",
                "alice-addr",
                "Alice",
                "cid-fut",
                None,
                None,
                future_secs,
                None,
                None,
            );
            assert!(matches!(
                cache.on_descriptor_sample(
                    &topic("alice-addr"),
                    &future,
                    &followed,
                    future_secs * 1000
                ),
                Some(DescriptorOutcome::Inserted { .. })
            ));

            // A legit, current descriptor.
            let legit = canonical_descriptor_bytes(
                "vine-legit",
                "alice-addr",
                "Alice",
                "cid-legit",
                None,
                None,
                now_real,
                None,
                None,
            );
            assert!(matches!(
                cache.on_descriptor_sample(
                    &topic("alice-addr"),
                    &legit,
                    &followed,
                    now_real * 1000
                ),
                Some(DescriptorOutcome::Inserted { .. })
            ));
            assert_eq!(cache.len_descriptors(), 2);
        }

        // Reload uses the REAL wall clock. The future-dated descriptor is NOT
        // deleted — it stays in the store — but is excluded from the display feed;
        // the legit one is both retained and shown.
        let cache2 = VineFeedCache::load(dir.path());
        assert!(
            cache2.has_descriptor("vine-legit"),
            "legit descriptor survives reload"
        );
        assert!(
            cache2.has_descriptor("vine-future"),
            "future-dated descriptor is RETAINED on load, not deleted"
        );
        assert_eq!(cache2.len_descriptors(), 2);

        let listed = cache2.list_descriptors();
        assert_eq!(
            listed.len(),
            1,
            "display feed excludes the future-dated row"
        );
        assert_eq!(listed[0].id, "vine-legit");

        // The P1 regression guard: a write-back (the path any later mutation
        // takes) must NOT purge the future-dated descriptor from disk.
        cache2.save_for_test();
        let cache3 = VineFeedCache::load(dir.path());
        assert!(
            cache3.has_descriptor("vine-future"),
            "future-dated descriptor survives write-back — no permanent deletion (P1)"
        );
        assert_eq!(cache3.len_descriptors(), 2);
    }

    #[test]
    fn on_descriptor_sample_unfollowed_creator_inserts_with_discover_source() {
        let mut cache = VineFeedCache::new();
        let payload = canonical_descriptor_bytes(
            "vine-2", "bob-addr", "Bob", "cid-bbb", None, None, 1700000100, None, None,
        );
        let followed = followed_set_with(&["someone-else"]);

        let outcome =
            cache.on_descriptor_sample(&topic("bob-addr"), &payload, &followed, 1_700_000_100_000);

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
            None,
            None,
        );
        let followed = followed_set_with(&["alice-addr"]);

        // First arrival
        let first = cache.on_descriptor_sample(
            &topic("alice-addr"),
            &payload,
            &followed,
            1_700_000_200_000,
        );
        assert!(matches!(first, Some(DescriptorOutcome::Inserted { .. })));

        // Same vine_id arrives again — even if followed_set changed
        let followed2 = followed_set_with(&[]); // empty
        let second = cache.on_descriptor_sample(
            &topic("alice-addr"),
            &payload,
            &followed2,
            1_700_000_201_000,
        );
        assert_eq!(second, Some(DescriptorOutcome::AlreadyPresent));
        assert_eq!(cache.len_descriptors(), 1);

        // Third arrival: alice is now followed again. The cache must
        // still report AlreadyPresent (i.e., source decision is frozen
        // at first insert and CANNOT be flipped by re-arrival, regardless
        // of current followed_set membership). If the cache ever decided
        // to re-insert based on a "now followed" signal, this third call
        // would return Inserted{Followed}.
        let followed3 = followed_set_with(&["alice-addr"]);
        let third = cache.on_descriptor_sample(
            &topic("alice-addr"),
            &payload,
            &followed3,
            1_700_000_202_000,
        );
        assert_eq!(third, Some(DescriptorOutcome::AlreadyPresent));
        assert_eq!(cache.len_descriptors(), 1);
    }

    #[test]
    fn on_descriptor_sample_malformed_payload_rejected() {
        let mut cache = VineFeedCache::new();
        let bad = b"not valid json {{{";
        let followed = followed_set_with(&[]);

        let outcome = cache.on_descriptor_sample(&topic("alice-addr"), bad, &followed, 5_000);

        match outcome {
            Some(DescriptorOutcome::Rejected(_)) => {}
            other => panic!("expected Rejected, got {:?}", other),
        }
        assert_eq!(cache.len_descriptors(), 0);
    }

    #[test]
    fn on_descriptor_sample_wrong_topic_returns_none() {
        let mut cache = VineFeedCache::new();
        let payload = canonical_descriptor_bytes(
            "vine-9",
            "alice-addr",
            "Alice",
            "cid",
            None,
            None,
            1,
            None,
            None,
        );
        let followed = followed_set_with(&[]);

        // The descriptor branch must NOT match reaction topics (they
        // contain `/reactions/`).
        let outcome = cache.on_descriptor_sample(
            &reaction_topic("alice-addr", "vine-9", "bob-addr"),
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
            let payload = canonical_descriptor_bytes(
                id,
                "alice-addr",
                "Alice",
                "cid",
                None,
                None,
                t,
                None,
                None,
            );
            cache.on_descriptor_sample(&topic("alice-addr"), &payload, &followed, 1_000);
        }

        let dtos = cache.list_descriptors();
        assert_eq!(dtos.len(), 3);
        // Newest first
        assert_eq!(dtos[0].id, "v-300");
        assert_eq!(dtos[1].id, "v-200");
        assert_eq!(dtos[2].id, "v-100");
    }

    /// Reactor-signed reaction bytes; `reactor` is a signer NAME
    /// (translated to its derived address — see `addr`).
    fn canonical_reaction_bytes(
        vine_id: &str,
        reactor: &str,
        reactor_name: &str,
        liked: bool,
        timestamp: u64,
    ) -> Vec<u8> {
        let signer = signer_for(reactor);
        let mut v = crate::VineReactionPayload {
            vine_id: vine_id.to_string(),
            reactor_address: addr(reactor),
            reactor_name: reactor_name.to_string(),
            liked,
            timestamp,
            identity_pub: None,
            sig: None,
            owner_id: None,
            enrollment_cbor_hex: None,
            signer_certs_cbor_hex: String::new(),
            device_sig: None,
        };
        crate::vine_signing::sign_reaction(&signer, &mut v);
        serde_json::to_vec(&v).unwrap()
    }

    /// Reaction topic for a vine by named creator, reacted by named reactor.
    fn reaction_topic(creator: &str, vine_id: &str, reactor: &str) -> String {
        format!(
            "harmony/vines/{}/reactions/{vine_id}/{}",
            addr(creator),
            addr(reactor)
        )
    }

    #[test]
    fn two_reactors_like_same_vine_count_is_two() {
        let mut cache = VineFeedCache::new();
        let alice_likes = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", true, 100);
        let bob_likes = canonical_reaction_bytes("vine-1", "bob-addr", "Bob", true, 110);

        let r1 = cache.on_reaction_sample(
            &reaction_topic("creator-addr", "vine-1", "alice-addr"),
            &alice_likes,
            0,
        );
        let r2 = cache.on_reaction_sample(
            &reaction_topic("creator-addr", "vine-1", "bob-addr"),
            &bob_likes,
            0,
        );

        assert_eq!(r1, Some(ReactionOutcome::Inserted));
        assert_eq!(r2, Some(ReactionOutcome::Inserted));

        let summary = cache.get_reaction("vine-1", &addr("anyone-addr"));
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
            &reaction_topic("creator-addr", "vine-1", "alice-addr"),
            &alice_unlikes,
            0,
        );
        let r2 = cache.on_reaction_sample(
            &reaction_topic("creator-addr", "vine-1", "alice-addr"),
            &alice_likes,
            0,
        );
        assert_eq!(r2, Some(ReactionOutcome::UpdatedNewer));

        let summary = cache.get_reaction("vine-1", &addr("alice-addr"));
        assert_eq!(summary.count, 1);
        assert!(summary.liked_by_me);
    }

    #[test]
    fn stale_reaction_does_not_overwrite_newer() {
        let mut cache = VineFeedCache::new();
        // First: like at t=200
        let alice_likes = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", true, 200);
        cache.on_reaction_sample(
            &reaction_topic("creator-addr", "vine-1", "alice-addr"),
            &alice_likes,
            0,
        );

        // Stale unlike at t=100 (lower timestamp, must be rejected)
        let stale_unlike = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", false, 100);
        let outcome = cache.on_reaction_sample(
            &reaction_topic("creator-addr", "vine-1", "alice-addr"),
            &stale_unlike,
            0,
        );
        assert_eq!(outcome, Some(ReactionOutcome::Stale));

        // Newer like still wins
        let summary = cache.get_reaction("vine-1", &addr("alice-addr"));
        assert_eq!(summary.count, 1);
        assert!(summary.liked_by_me);
    }

    #[test]
    fn liked_by_me_reflects_viewer_addr() {
        let mut cache = VineFeedCache::new();
        let alice_likes = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", true, 100);
        let bob_likes = canonical_reaction_bytes("vine-1", "bob-addr", "Bob", true, 110);

        cache.on_reaction_sample(
            &reaction_topic("creator-addr", "vine-1", "alice-addr"),
            &alice_likes,
            0,
        );
        cache.on_reaction_sample(
            &reaction_topic("creator-addr", "vine-1", "bob-addr"),
            &bob_likes,
            0,
        );

        // From Alice's perspective: liked_by_me=true (she liked it)
        let a = cache.get_reaction("vine-1", &addr("alice-addr"));
        assert_eq!(a.count, 2);
        assert!(a.liked_by_me);

        // From Carol's perspective: she did not react
        let c = cache.get_reaction("vine-1", &addr("carol-addr"));
        assert_eq!(c.count, 2);
        assert!(!c.liked_by_me);
    }

    #[test]
    fn on_reaction_sample_wrong_topic_returns_none() {
        let mut cache = VineFeedCache::new();
        let payload = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", true, 100);

        // Descriptor topic — must NOT match the reaction branch
        let outcome = cache.on_reaction_sample(&topic("creator-addr"), &payload, 0);
        assert_eq!(outcome, None);

        // Unrelated topic
        let outcome2 = cache.on_reaction_sample("harmony/profile/alice-addr", &payload, 0);
        assert_eq!(outcome2, None);
    }

    #[test]
    fn on_reaction_sample_malformed_payload_rejected() {
        let mut cache = VineFeedCache::new();
        let bad = b"{{{not json";

        let outcome = cache.on_reaction_sample(
            &reaction_topic("creator-addr", "vine-1", "alice-addr"),
            bad,
            0,
        );
        assert!(
            matches!(outcome, Some(ReactionOutcome::Rejected(ref e)) if e.contains("parse failed")),
            "got {outcome:?}"
        );
        assert_eq!(cache.len_reactions(), 0);
    }

    #[test]
    fn get_reaction_for_unknown_vine_id_returns_zero() {
        let cache = VineFeedCache::new();
        let summary = cache.get_reaction("nonexistent-vine", &addr("anyone-addr"));
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
            &reaction_topic("creator-addr", "vine-1", "alice-addr"),
            &alice_unlike,
            0,
        );

        // Bob likes (so count > 0, isolating the liked_by_me=false invariant)
        let bob_like = canonical_reaction_bytes("vine-1", "bob-addr", "Bob", true, 110);
        cache.on_reaction_sample(
            &reaction_topic("creator-addr", "vine-1", "bob-addr"),
            &bob_like,
            0,
        );

        let summary = cache.get_reaction("vine-1", &addr("alice-addr"));
        assert_eq!(summary.count, 1);
        assert!(!summary.liked_by_me);
    }

    #[test]
    fn mark_viewed_idempotent_and_local_only() {
        let mut cache = VineFeedCache::new();
        let payload = canonical_descriptor_bytes(
            "vine-1",
            "alice-addr",
            "Alice",
            "cid",
            None,
            None,
            100,
            None,
            None,
        );
        let followed = followed_set_with(&["alice-addr"]);
        cache.on_descriptor_sample(&topic("alice-addr"), &payload, &followed, 0);
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
            None,
            None,
        );
        cache.on_descriptor_sample(&topic("alice-addr"), &payload, &followed, 0);

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
                None,
                None,
            );
            let out =
                cache.on_descriptor_sample(&topic("alice-addr"), &desc, &followed, now_secs * 1000);
            assert!(matches!(out, Some(DescriptorOutcome::Inserted { .. })));

            let react =
                canonical_reaction_bytes("vine-rt", "bob-addr", "Bob", true, recent_created_at + 1);
            let out2 = cache.on_reaction_sample(
                &reaction_topic("alice-addr", "vine-rt", "bob-addr"),
                &react,
                0,
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
        assert_eq!(dtos[0].creator_address, addr("alice-addr"));
        assert_eq!(dtos[0].title.as_deref(), Some("title-x"));
        assert!(dtos[0].viewed);

        // Verify reaction is correctly reconstructed (count + liked_by_me)
        let summary = cache2.get_reaction("vine-rt", &addr("bob-addr"));
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
        let summary = cache.get_reaction("vine-new", &addr("bob-addr"));
        assert_eq!(summary.count, 1);
    }

    #[test]
    fn descriptor_insert_persists_to_disk() {
        let dir = tempfile::tempdir().expect("create tempdir");
        {
            let mut cache = VineFeedCache::load(dir.path());
            let followed = followed_set_with(&["alice-addr"]);
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let desc = canonical_descriptor_bytes(
                "vine-p1",
                "alice-addr",
                "Alice",
                "cid-1",
                None,
                None,
                now_secs.saturating_sub(60), // recent (within age cutoff),
                None,
                None,
            );
            let out =
                cache.on_descriptor_sample(&topic("alice-addr"), &desc, &followed, now_secs * 1000);
            assert!(matches!(out, Some(DescriptorOutcome::Inserted { .. })));
            // No explicit save_for_test() call — Task 4 wires save() into
            // on_descriptor_sample, so the disk must already reflect this.
        }
        let cache2 = VineFeedCache::load(dir.path());
        assert_eq!(cache2.len_descriptors(), 1);
        assert_eq!(cache2.list_descriptors()[0].id, "vine-p1");
    }

    #[test]
    fn reaction_update_persists_to_disk() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let recent = now_secs.saturating_sub(60);
        {
            let mut cache = VineFeedCache::load(dir.path());
            let followed = followed_set_with(&["alice-addr"]);
            // Need a descriptor first (otherwise reaction is orphaned and
            // load() drops it).
            let desc = canonical_descriptor_bytes(
                "vine-r1",
                "alice-addr",
                "Alice",
                "cid",
                None,
                None,
                recent,
                None,
                None,
            );
            cache.on_descriptor_sample(&topic("alice-addr"), &desc, &followed, now_secs * 1000);

            // Insert reaction
            let react = canonical_reaction_bytes("vine-r1", "bob-addr", "Bob", true, recent + 10);
            let out = cache.on_reaction_sample(
                &reaction_topic("alice-addr", "vine-r1", "bob-addr"),
                &react,
                0,
            );
            assert_eq!(out, Some(ReactionOutcome::Inserted));

            // Update reaction (LWW newer timestamp)
            let react2 = canonical_reaction_bytes("vine-r1", "bob-addr", "Bob", false, recent + 20);
            let out2 = cache.on_reaction_sample(
                &reaction_topic("alice-addr", "vine-r1", "bob-addr"),
                &react2,
                0,
            );
            assert_eq!(out2, Some(ReactionOutcome::UpdatedNewer));
        }
        // Reload — both descriptor and updated reaction must be persisted
        let cache2 = VineFeedCache::load(dir.path());
        assert_eq!(cache2.len_reactions(), 1);
        // The final reaction value should be liked=false at recent+20
        let summary = cache2.get_reaction("vine-r1", &addr("bob-addr"));
        assert_eq!(summary.count, 0); // liked=false → not counted
    }

    #[test]
    fn reaction_insert_persists_to_disk_without_update() {
        // Regression guard: ensures the Inserted save path stands on its
        // own. `reaction_update_persists_to_disk` mutates the same key
        // twice, so the final UpdatedNewer save would mask a missing
        // save() on the Inserted path. This test inserts ONCE.
        let dir = tempfile::tempdir().expect("create tempdir");
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let recent = now_secs.saturating_sub(60);
        {
            let mut cache = VineFeedCache::load(dir.path());
            let followed = followed_set_with(&["alice-addr"]);
            let desc = canonical_descriptor_bytes(
                "vine-ri",
                "alice-addr",
                "Alice",
                "cid",
                None,
                None,
                recent,
                None,
                None,
            );
            cache.on_descriptor_sample(&topic("alice-addr"), &desc, &followed, now_secs * 1000);
            let react = canonical_reaction_bytes("vine-ri", "bob-addr", "Bob", true, recent + 10);
            let out = cache.on_reaction_sample(
                &reaction_topic("alice-addr", "vine-ri", "bob-addr"),
                &react,
                0,
            );
            assert_eq!(out, Some(ReactionOutcome::Inserted));
            // No further mutations — the Inserted save must be sufficient.
        }
        let cache2 = VineFeedCache::load(dir.path());
        assert_eq!(cache2.len_reactions(), 1);
        let summary = cache2.get_reaction("vine-ri", &addr("bob-addr"));
        assert_eq!(summary.count, 1);
        assert!(summary.liked_by_me);
    }

    #[test]
    fn list_reactions_returns_rows_after_reload() {
        // ZEB-672: the frontend rebuilds its reaction map from these rows
        // at boot — live vine-reaction-received events never re-fire for
        // cache-reloaded reactions, so this query is the only recovery
        // path. Unliked rows are included (the frontend merge skips them).
        let dir = tempfile::tempdir().expect("create tempdir");
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let recent = now_secs.saturating_sub(60);
        {
            let mut cache = VineFeedCache::load(dir.path());
            let followed = followed_set_with(&["alice-addr"]);
            let desc = canonical_descriptor_bytes(
                "vine-lr",
                "alice-addr",
                "Alice",
                "cid",
                None,
                None,
                recent,
                None,
                None,
            );
            cache.on_descriptor_sample(&topic("alice-addr"), &desc, &followed, now_secs * 1000);
            let r1 = canonical_reaction_bytes("vine-lr", "bob-addr", "Bob", true, recent + 10);
            cache.on_reaction_sample(&reaction_topic("alice-addr", "vine-lr", "bob-addr"), &r1, 0);
            let r2 = canonical_reaction_bytes("vine-lr", "ann-addr", "Ann", false, recent + 11);
            cache.on_reaction_sample(&reaction_topic("alice-addr", "vine-lr", "ann-addr"), &r2, 0);
        }
        let cache2 = VineFeedCache::load(dir.path());
        let rows = cache2.list_reactions();
        assert_eq!(rows.len(), 2);
        // Rows sort by (vine_id, reactor_address); derived signer
        // addresses (ZEB-673) make the relative order arbitrary, so look
        // each row up by reactor instead of by index.
        let ann = rows
            .iter()
            .find(|r| r.reactor_address == addr("ann-addr"))
            .expect("ann row");
        assert_eq!(
            *ann,
            VineReactionRow {
                vine_id: "vine-lr".to_string(),
                reactor_address: addr("ann-addr"),
                reactor_name: "Ann".to_string(),
                liked: false,
                timestamp: recent + 11,
            }
        );
        let bob = rows
            .iter()
            .find(|r| r.reactor_address == addr("bob-addr"))
            .expect("bob row");
        assert!(bob.liked);
        assert_eq!(bob.timestamp, recent + 10);
    }

    #[test]
    fn vine_reaction_row_serializes_camel_case() {
        // Pins the wire shape the frontend hydrate merge consumes —
        // must match the vine-reaction-received event payload keys.
        let row = VineReactionRow {
            vine_id: "v1".to_string(),
            reactor_address: "addr-1".to_string(),
            reactor_name: "Nia".to_string(),
            liked: true,
            timestamp: 42,
        };
        let v = serde_json::to_value(&row).expect("serialize");
        assert_eq!(
            v,
            serde_json::json!({
                "vineId": "v1",
                "reactorAddress": "addr-1",
                "reactorName": "Nia",
                "liked": true,
                "timestamp": 42,
            })
        );
    }

    #[test]
    fn mark_viewed_persists_to_disk() {
        let dir = tempfile::tempdir().expect("create tempdir");
        {
            let mut cache = VineFeedCache::load(dir.path());
            let first = cache.mark_viewed("v-mv".to_string());
            assert!(first);
            // Second call returns false; the disk write side-effect must
            // be skipped on the no-op path (we can't directly observe
            // "no write happened" from this test, but the next reload
            // confirms the viewed set has exactly the one entry).
            let second = cache.mark_viewed("v-mv".to_string());
            assert!(!second);
        }
        let cache2 = VineFeedCache::load(dir.path());
        assert!(cache2.is_viewed("v-mv"));
    }

    #[test]
    fn cached_descriptor_round_trips_original_creator_fields_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // First boot: insert a reshare with full attribution, drop the cache.
        {
            let mut cache = VineFeedCache::load(dir.path());
            let payload = canonical_descriptor_bytes(
                "vine-reshare",
                "addr-resharer",
                "Resharer",
                "cid-r",
                None,
                Some("vine-orig"),
                now_secs.saturating_sub(1),
                Some("addr-original"),
                Some("Original"),
            );
            let followed = followed_set_with(&["addr-resharer"]);
            let outcome = cache.on_descriptor_sample(
                &topic("addr-resharer"),
                &payload,
                &followed,
                now_secs * 1000,
            );
            assert!(
                matches!(outcome, Some(DescriptorOutcome::Inserted { .. })),
                "expected Inserted, got {outcome:?}"
            );
        }

        // Second boot: reload, verify attribution survived.
        {
            let cache = VineFeedCache::load(dir.path());
            let dtos = cache.list_descriptors();
            let dto = dtos
                .iter()
                .find(|d| d.id == "vine-reshare")
                .expect("reshare should survive reload");
            assert_eq!(
                dto.original_creator_address.as_deref(),
                Some(addr("addr-original").as_str())
            );
            assert_eq!(dto.original_creator_name.as_deref(), Some("Original"));
        }
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
            None,
            None,
        );
        let payload2 = canonical_descriptor_bytes(
            "vine-2",
            "alice-addr",
            "Alice",
            "cid-b",
            None,
            Some("vine-1"), // reshare
            600,
            None,
            None,
        );
        let followed = followed_set_with(&["alice-addr"]);

        cache.on_descriptor_sample(&topic("alice-addr"), &payload, &followed, 0);
        cache.on_descriptor_sample(&topic("alice-addr"), &payload2, &followed, 0);
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

    #[test]
    fn capacity_trim_on_insert_drops_oldest_when_over_max() {
        // Insert MAX_DESCRIPTORS + 5 descriptors with strictly increasing
        // created_at. After all inserts, exactly MAX_DESCRIPTORS remain,
        // and the oldest 5 (created_at 0..4) are gone — only created_at
        // 5..MAX_DESCRIPTORS+5 should remain.
        let mut cache = VineFeedCache::new();
        let followed = followed_set_with(&["alice-addr"]);
        let total = MAX_DESCRIPTORS + 5;
        for i in 0..total {
            let id = format!("v-{i:05}");
            let payload = canonical_descriptor_bytes(
                &id,
                "alice-addr",
                "Alice",
                "cid",
                None,
                None,
                i as u64, // created_at = i,
                None,
                None,
            );
            // Clock consistent with the largest created_at so the forward-skew
            // gate rejects none of these inserts (all still within the window).
            cache.on_descriptor_sample(
                &topic("alice-addr"),
                &payload,
                &followed,
                (total as u64) * 1_000,
            );
        }
        assert_eq!(cache.len_descriptors(), MAX_DESCRIPTORS);

        // The 5 oldest (created_at 0..4) must be gone
        let dtos = cache.list_descriptors();
        let ids: HashSet<&str> = dtos.iter().map(|d| d.id.as_str()).collect();
        for i in 0..5 {
            let dropped = format!("v-{i:05}");
            assert!(
                !ids.contains(dropped.as_str()),
                "v-{i:05} (oldest) should have been trimmed"
            );
        }
        // The newest one (v-MAX_DESCRIPTORS+4) must be present
        let newest = format!("v-{:05}", total - 1);
        assert!(
            ids.contains(newest.as_str()),
            "newest descriptor should remain"
        );
    }

    #[test]
    fn descriptor_older_than_cache_window_is_rejected_when_at_capacity() {
        // Regression for Qodo PR #119 finding: if the cache is at
        // MAX_DESCRIPTORS and a new descriptor arrives with `created_at`
        // older than every existing entry, the capacity-trim will drop
        // the just-inserted vine. The outcome must be Rejected (NOT
        // Inserted), so the event loop does not emit vine-received to
        // the frontend for a vine that wasn't actually retained.
        let mut cache = VineFeedCache::new();
        let followed = followed_set_with(&["alice-addr"]);

        // Fill cache to capacity with strictly increasing created_at
        // starting at 1_000 (so created_at = 0 is reliably older).
        for i in 0..MAX_DESCRIPTORS {
            let id = format!("v-{i:05}");
            let payload = canonical_descriptor_bytes(
                &id,
                "alice-addr",
                "Alice",
                "cid",
                None,
                None,
                (i + 1_000) as u64,
                None,
                None,
            );
            // Clock large enough that every inserted created_at is in-window for
            // the forward-skew gate, yet still well under MAX_AGE_SECS so the
            // deliberately-ancient v-ancient below reaches the capacity-trim
            // (not the backward age gate).
            cache.on_descriptor_sample(
                &topic("alice-addr"),
                &payload,
                &followed,
                (MAX_DESCRIPTORS as u64 + 1_000) * 1_000,
            );
        }
        assert_eq!(cache.len_descriptors(), MAX_DESCRIPTORS);

        // Now insert one with created_at = 0 (older than all existing).
        let old_payload = canonical_descriptor_bytes(
            "v-ancient",
            "alice-addr",
            "Alice",
            "cid",
            None,
            None,
            0, // older than every entry in the cache,
            None,
            None,
        );
        let outcome = cache.on_descriptor_sample(
            &topic("alice-addr"),
            &old_payload,
            &followed,
            (MAX_DESCRIPTORS as u64 + 1_000) * 1_000,
        );

        // Must NOT be Inserted — the vine was trimmed away before
        // persistence. Must be Rejected so the event loop does not emit.
        match outcome {
            Some(DescriptorOutcome::Rejected(reason)) => {
                assert!(
                    reason.contains("v-ancient"),
                    "rejection reason should name the dropped vine; got: {reason}"
                );
            }
            other => panic!("expected Rejected (capacity-trim victim), got {:?}", other),
        }

        // Cache state is unchanged: still at capacity, v-ancient absent.
        assert_eq!(cache.len_descriptors(), MAX_DESCRIPTORS);
        let dtos = cache.list_descriptors();
        let ids: HashSet<&str> = dtos.iter().map(|d| d.id.as_str()).collect();
        assert!(
            !ids.contains("v-ancient"),
            "v-ancient must not be in the cache (was capacity-trimmed)"
        );
    }

    #[test]
    fn ninety_day_boundary_keeps_recent_drops_past() {
        // Verifies the spec §5 algorithm's `created_at >= age_cutoff`
        // semantics: a descriptor RECENT enough (just inside the 90-day
        // window) survives load; one PAST the window is dropped.
        //
        // Why not test the literal-second cutoff? `load()` calls
        // SystemTime::now() independently from the test's `now`, so a
        // 1-second clock tick between the two would silently flake the
        // boundary case. A 60-second margin makes the test robust on
        // loaded CI machines while still exercising the same predicate.
        let dir = tempfile::tempdir().expect("create tempdir");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let just_inside = now.saturating_sub(MAX_AGE_SECS).saturating_add(60); // 60 seconds INSIDE the window — safely kept
        let just_past = now.saturating_sub(MAX_AGE_SECS).saturating_sub(60); // 60 seconds PAST the window — safely dropped

        let disk = serde_json::json!({
            "version": FILE_VERSION,
            "descriptors": [
                {
                    "id": "recent",
                    "creatorAddress": "a",
                    "creatorName": "A",
                    "createdAt": just_inside,
                    "videoCid": "cid",
                    "receivedAtMs": 0,
                    "source": "followed"
                },
                {
                    "id": "too-old",
                    "creatorAddress": "a",
                    "creatorName": "A",
                    "createdAt": just_past,
                    "videoCid": "cid",
                    "receivedAtMs": 0,
                    "source": "followed"
                }
            ],
            "reactions": [],
            "viewed": []
        });
        std::fs::write(
            dir.path().join(VINE_FEED_FILE),
            serde_json::to_vec_pretty(&disk).unwrap(),
        )
        .unwrap();

        let cache = VineFeedCache::load(dir.path());
        let ids: HashSet<String> = cache
            .list_descriptors()
            .iter()
            .map(|d| d.id.clone())
            .collect();
        assert!(
            ids.contains("recent"),
            "descriptor inside the 90-day window must be KEPT (>= cutoff)"
        );
        assert!(
            !ids.contains("too-old"),
            "descriptor past the 90-day window must be DROPPED (< cutoff)"
        );
    }

    #[test]
    fn load_and_runtime_capacity_trim_use_same_tiebreak() {
        // Cross-replica determinism: when multiple descriptors share a
        // created_at and the cap is exceeded, LOAD and RUNTIME must both
        // retain the same set. Without symmetric tiebreaks, a restart
        // could retain a different subset than live mutation would.
        //
        // Strategy: build a state with MAX_DESCRIPTORS + 1 descriptors
        // all sharing the same created_at, and unique ids "a", "b", ...
        // Both code paths should drop the SAME id.

        // === Phase 1: RUNTIME path ===
        let mut runtime_cache = VineFeedCache::new();
        let followed = followed_set_with(&["alice-addr"]);
        // Use a timestamp well inside the 90-day window so the load-path
        // age filter does not silently discard everything before
        // capacity-trim has a chance to run.
        let shared_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(MAX_AGE_SECS)
            .saturating_add(86_400); // 1 day inside the window
        let total = MAX_DESCRIPTORS + 1;
        for i in 0..total {
            // Fixed-width zero-padded ids so lexicographic ordering
            // matches numerical ordering.
            let id = format!("v-{i:08}");
            let payload = canonical_descriptor_bytes(
                &id,
                "alice-addr",
                "Alice",
                "cid",
                None,
                None,
                shared_ts,
                None,
                None,
            );
            runtime_cache.on_descriptor_sample(
                &topic("alice-addr"),
                &payload,
                &followed,
                shared_ts * 1_000,
            );
        }
        // The runtime trim should have dropped exactly one descriptor.
        assert_eq!(runtime_cache.len_descriptors(), MAX_DESCRIPTORS);
        let runtime_ids: HashSet<String> = runtime_cache
            .list_descriptors()
            .iter()
            .map(|d| d.id.clone())
            .collect();

        // === Phase 2: LOAD path ===
        // Construct the same input as an on-disk envelope, load it.
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut disk_descriptors = Vec::with_capacity(total);
        for i in 0..total {
            let id = format!("v-{i:08}");
            disk_descriptors.push(serde_json::json!({
                "id": id,
                "creatorAddress": "alice-addr",
                "creatorName": "Alice",
                "createdAt": shared_ts,
                "videoCid": "cid",
                "receivedAtMs": 0,
                "source": "followed",
            }));
        }
        let disk = serde_json::json!({
            "version": FILE_VERSION,
            "descriptors": disk_descriptors,
            "reactions": [],
            "viewed": []
        });
        std::fs::write(
            dir.path().join(VINE_FEED_FILE),
            serde_json::to_vec_pretty(&disk).unwrap(),
        )
        .unwrap();

        let load_cache = VineFeedCache::load(dir.path());
        assert_eq!(load_cache.len_descriptors(), MAX_DESCRIPTORS);
        let load_ids: HashSet<String> = load_cache
            .list_descriptors()
            .iter()
            .map(|d| d.id.clone())
            .collect();

        // === Phase 3: assert the two retained sets match ===
        assert_eq!(
            runtime_ids, load_ids,
            "load and runtime capacity-trim must retain the same descriptor set on ties"
        );

        // Also verify the specific dropped id is the lexicographically
        // smallest one (defensive: locks in the actual tiebreak direction).
        let dropped = format!("v-{:08}", 0); // "v-00000000"
        assert!(
            !runtime_ids.contains(&dropped),
            "runtime should drop the smallest id on tie; got: {runtime_ids:?}"
        );
        assert!(
            !load_ids.contains(&dropped),
            "load should drop the smallest id on tie; got: {load_ids:?}"
        );
    }

    // ── ZEB-670: tombstone tests ─────────────────────────────────────

    /// Generate a real identity and return (identity, hex address). The
    /// identity self-registers in the signer registry under its DERIVED
    /// address, so passing the returned address into name-translating
    /// helpers (`insert_descriptor`, `canonical_descriptor_bytes`, …)
    /// signs with THIS identity and maps the address to itself (ZEB-673).
    fn tombstone_identity() -> (std::sync::Arc<harmony_identity::PrivateIdentity>, String) {
        let id = std::sync::Arc::new(harmony_identity::PrivateIdentity::generate(
            &mut rand::rngs::OsRng,
        ));
        let addr = hex::encode(id.public_identity().address_hash);
        signers().lock().unwrap().insert(addr.clone(), id.clone());
        (id, addr)
    }

    fn signed_tombstone_bytes(
        id: &harmony_identity::PrivateIdentity,
        vine_id: &str,
        video_cid: &str,
        creator_address: &str,
    ) -> Vec<u8> {
        // Stamp "now" — the load-path age-prune drops tombstones older
        // than the 90-day window, so a fixed 2023 epoch would (correctly)
        // vanish across the persistence-roundtrip test.
        let deleted_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let t = crate::vine_tombstone::sign_tombstone(
            id,
            vine_id.to_string(),
            video_cid.to_string(),
            creator_address.to_string(),
            deleted_at,
        );
        serde_json::to_vec(&t).unwrap()
    }

    fn insert_descriptor(cache: &mut VineFeedCache, vine_id: &str, addr: &str, cid: &str) {
        let payload = canonical_descriptor_bytes(
            vine_id,
            addr,
            "Creator",
            cid,
            None,
            None,
            1_700_000_000,
            None,
            None,
        );
        let outcome = cache.on_descriptor_sample(
            &topic(addr),
            &payload,
            &followed_set_with(&[]),
            1_700_000_000_000,
        );
        assert!(
            matches!(outcome, Some(DescriptorOutcome::Inserted { .. })),
            "fixture insert failed: {outcome:?}"
        );
    }

    fn insert_reshare(
        cache: &mut VineFeedCache,
        vine_id: &str,
        resharer: &str,
        reshare_of: &str,
        origin_addr: &str,
        cid: &str,
    ) {
        let payload = canonical_descriptor_bytes(
            vine_id,
            resharer,
            "Resharer",
            cid,
            None,
            Some(reshare_of),
            1_700_000_100,
            Some(origin_addr),
            Some("Creator"),
        );
        let outcome = cache.on_descriptor_sample(
            &topic(resharer),
            &payload,
            &followed_set_with(&[]),
            1_700_000_100_000,
        );
        assert!(
            matches!(outcome, Some(DescriptorOutcome::Inserted { .. })),
            "fixture reshare insert failed: {outcome:?}"
        );
    }

    #[test]
    fn tombstone_removes_descriptor_reactions_viewed_and_reports_evict() {
        let (id, addr) = tombstone_identity();
        let mut cache = VineFeedCache::new();
        insert_descriptor(&mut cache, "vine-1", &addr, "cid-aaa");
        cache.on_reaction_sample(
            &format!("harmony/vines/{addr}/reactions/vine-1/alice-addr"),
            &canonical_reaction_bytes("vine-1", "alice-addr", "Alice", true, 100),
            0,
        );
        cache.on_reaction_sample(
            &format!("harmony/vines/{addr}/reactions/vine-1/bob-addr"),
            &canonical_reaction_bytes("vine-1", "bob-addr", "Bob", true, 110),
            0,
        );
        assert!(cache.mark_viewed("vine-1".into()));

        let outcome = cache.on_tombstone_sample(
            &format!("harmony/vines/{addr}/tombstones/vine-1"),
            &signed_tombstone_bytes(&id, "vine-1", "cid-aaa", &addr),
        );

        match outcome {
            Some(TombstoneOutcome::Applied { removed, evict_cid }) => {
                assert_eq!(removed.vine_id, "vine-1");
                assert_eq!(removed.video_cid, "cid-aaa");
                assert_eq!(removed.creator_address, addr);
                assert_eq!(evict_cid.as_deref(), Some("cid-aaa"));
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        assert!(cache.list_descriptors().is_empty());
        assert!(cache.list_reactions().is_empty());
        assert!(!cache.is_viewed("vine-1"));
    }

    #[test]
    fn tombstone_keeps_blob_when_an_unrelated_live_vine_references_cid() {
        let (id, addr) = tombstone_identity();
        let mut cache = VineFeedCache::new();
        insert_descriptor(&mut cache, "vine-1", &addr, "cid-shared");
        // Unrelated ORIGINAL by another creator, same content bytes.
        insert_descriptor(&mut cache, "vine-2", "other-addr", "cid-shared");

        let outcome = cache.on_tombstone_sample(
            &format!("harmony/vines/{addr}/tombstones/vine-1"),
            &signed_tombstone_bytes(&id, "vine-1", "cid-shared", &addr),
        );

        match outcome {
            Some(TombstoneOutcome::Applied { removed, evict_cid }) => {
                assert_eq!(removed.vine_id, "vine-1");
                assert_eq!(evict_cid, None, "live unrelated reference must block evict");
            }
            other => panic!("expected Applied, got {other:?}"),
        }
    }

    #[test]
    fn own_reshare_delete_removes_it_and_keeps_original_playable() {
        // Bob reshared Alice's vine, then deletes HIS OWN reshare: the
        // reshare goes away outright, Alice's original must neither be
        // stubbed nor lose its blob.
        let (bob_id, bob_addr) = tombstone_identity();
        let mut cache = VineFeedCache::new();
        insert_descriptor(&mut cache, "vine-orig", "alice-addr", "cid-aaa");
        insert_reshare(
            &mut cache,
            "vine-reshare",
            &bob_addr,
            "vine-orig",
            "alice-addr",
            "cid-aaa",
        );

        let outcome = cache.on_tombstone_sample(
            &format!("harmony/vines/{bob_addr}/tombstones/vine-reshare"),
            &signed_tombstone_bytes(&bob_id, "vine-reshare", "cid-aaa", &bob_addr),
        );

        match outcome {
            Some(TombstoneOutcome::Applied { removed, evict_cid }) => {
                assert_eq!(removed.vine_id, "vine-reshare");
                assert_eq!(evict_cid, None, "original still references the blob");
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        let rows = cache.list_descriptors();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "vine-orig");
        assert!(!rows[0].original_removed, "original must not be stubbed");
    }

    #[test]
    fn pre_arrival_tombstone_blocks_later_descriptor() {
        let (id, addr) = tombstone_identity();
        let mut cache = VineFeedCache::new();

        let outcome = cache.on_tombstone_sample(
            &format!("harmony/vines/{addr}/tombstones/vine-1"),
            &signed_tombstone_bytes(&id, "vine-1", "cid-aaa", &addr),
        );
        assert_eq!(
            outcome,
            Some(TombstoneOutcome::Applied {
                // Emitted even pre-arrival (reshare-only subscribers need
                // it); nothing live references the content, so evict too.
                removed: RemovedVine {
                    vine_id: "vine-1".into(),
                    video_cid: "cid-aaa".into(),
                    creator_address: addr.clone(),
                },
                evict_cid: Some("cid-aaa".into()),
            })
        );

        // The descriptor arrives late (e.g. relayed by a peer that never
        // saw the tombstone) — it must NOT resurrect.
        let payload = canonical_descriptor_bytes(
            "vine-1",
            &addr,
            "Creator",
            "cid-aaa",
            None,
            None,
            1_700_000_000,
            None,
            None,
        );
        let outcome =
            cache.on_descriptor_sample(&topic(&addr), &payload, &followed_set_with(&[]), 1_000);
        assert!(
            matches!(outcome, Some(DescriptorOutcome::Rejected(ref r)) if r.contains("tombstoned")),
            "expected tombstone rejection, got {outcome:?}"
        );
        assert_eq!(cache.len_descriptors(), 0);
    }

    #[test]
    fn tombstone_rejects_bad_signature_and_wrong_creator() {
        let (id, addr) = tombstone_identity();
        let (attacker_id, attacker_addr) = tombstone_identity();
        let mut cache = VineFeedCache::new();
        insert_descriptor(&mut cache, "vine-1", &addr, "cid-aaa");

        // (a) Tampered signature.
        let mut t: crate::vine_tombstone::VineTombstonePayload =
            serde_json::from_slice(&signed_tombstone_bytes(&id, "vine-1", "cid-aaa", &addr))
                .unwrap();
        t.sig = hex::encode([0u8; 64]);
        let outcome = cache.on_tombstone_sample(
            &format!("harmony/vines/{addr}/tombstones/vine-1"),
            &serde_json::to_vec(&t).unwrap(),
        );
        assert!(
            matches!(outcome, Some(TombstoneOutcome::Rejected(_))),
            "tampered sig must reject, got {outcome:?}"
        );

        // (b) Valid-signed tombstone by an attacker for someone else's
        // vine (attacker signs THEIR OWN address — passes crypto, fails
        // ownership).
        let outcome = cache.on_tombstone_sample(
            &format!("harmony/vines/{attacker_addr}/tombstones/vine-1"),
            &signed_tombstone_bytes(&attacker_id, "vine-1", "cid-aaa", &attacker_addr),
        );
        assert!(
            matches!(outcome, Some(TombstoneOutcome::Rejected(ref r)) if r.contains("does not own")),
            "cross-creator tombstone must reject, got {outcome:?}"
        );

        // (c) Valid tombstone published on the WRONG topic.
        let outcome = cache.on_tombstone_sample(
            &format!("harmony/vines/{attacker_addr}/tombstones/vine-1"),
            &signed_tombstone_bytes(&id, "vine-1", "cid-aaa", &addr),
        );
        assert!(
            matches!(outcome, Some(TombstoneOutcome::Rejected(ref r)) if r.contains("topic creator")),
            "topic/payload creator mismatch must reject, got {outcome:?}"
        );

        // Vine untouched throughout.
        assert_eq!(cache.len_descriptors(), 1);
    }

    #[test]
    fn tombstone_is_idempotent() {
        let (id, addr) = tombstone_identity();
        let mut cache = VineFeedCache::new();
        insert_descriptor(&mut cache, "vine-1", &addr, "cid-aaa");
        let key = format!("harmony/vines/{addr}/tombstones/vine-1");
        let bytes = signed_tombstone_bytes(&id, "vine-1", "cid-aaa", &addr);

        assert!(matches!(
            cache.on_tombstone_sample(&key, &bytes),
            Some(TombstoneOutcome::Applied { .. })
        ));
        assert_eq!(
            cache.on_tombstone_sample(&key, &bytes),
            Some(TombstoneOutcome::AlreadyApplied)
        );
    }

    #[test]
    fn reshare_of_tombstoned_original_lists_original_removed() {
        let (id, addr) = tombstone_identity();
        let mut cache = VineFeedCache::new();
        insert_descriptor(&mut cache, "vine-orig", &addr, "cid-aaa");
        // Direct reshare by Bob.
        insert_reshare(
            &mut cache,
            "vine-b",
            "bob-addr",
            "vine-orig",
            &addr,
            "cid-aaa",
        );
        // Chain reshare by Carol (reshare of Bob's reshare; origin = creator).
        insert_reshare(
            &mut cache,
            "vine-c",
            "carol-addr",
            "vine-b",
            &addr,
            "cid-aaa",
        );

        let outcome = cache.on_tombstone_sample(
            &format!("harmony/vines/{addr}/tombstones/vine-orig"),
            &signed_tombstone_bytes(&id, "vine-orig", "cid-aaa", &addr),
        );
        match outcome {
            Some(TombstoneOutcome::Applied { removed, evict_cid }) => {
                assert_eq!(removed.vine_id, "vine-orig");
                // Both survivors are stubs — nothing live references the
                // blob, so it evicts.
                assert_eq!(evict_cid.as_deref(), Some("cid-aaa"));
            }
            other => panic!("expected Applied, got {other:?}"),
        }

        let rows = cache.list_descriptors();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert!(
                row.original_removed,
                "reshare {} must be stubbed (direct or chain match)",
                row.id
            );
        }
    }

    #[test]
    fn republished_original_unstubs_new_reshares_but_not_old_ones() {
        let (id, addr) = tombstone_identity();
        let mut cache = VineFeedCache::new();
        insert_descriptor(&mut cache, "vine-old", &addr, "cid-aaa");
        insert_reshare(
            &mut cache, "vine-b", "bob-addr", "vine-old", &addr, "cid-aaa",
        );
        cache.on_tombstone_sample(
            &format!("harmony/vines/{addr}/tombstones/vine-old"),
            &signed_tombstone_bytes(&id, "vine-old", "cid-aaa", &addr),
        );

        // Creator re-publishes the same content under a new id; Dave
        // reshares the NEW original.
        insert_descriptor(&mut cache, "vine-new", &addr, "cid-aaa");
        insert_reshare(
            &mut cache,
            "vine-d",
            "dave-addr",
            "vine-new",
            &addr,
            "cid-aaa",
        );

        let rows = cache.list_descriptors();
        let by_id = |id: &str| rows.iter().find(|r| r.id == id).unwrap();
        assert!(
            by_id("vine-b").original_removed,
            "old reshare stays stubbed via direct id match"
        );
        assert!(
            !by_id("vine-d").original_removed,
            "reshare of the re-published original must not be stubbed"
        );
        assert!(!by_id("vine-new").original_removed);
    }

    #[test]
    fn attackers_pre_arrival_tombstone_does_not_censor_victims_descriptor() {
        // CodeRabbit PR #445 round 1 (Critical): an attacker who observes
        // a victim's vine id publishes a tombstone validly signed under
        // the ATTACKER's own address before the victim's descriptor
        // arrives. The guard is creator-bound, so the descriptor must
        // still insert.
        let (attacker_id, attacker_addr) = tombstone_identity();
        let mut cache = VineFeedCache::new();

        let outcome = cache.on_tombstone_sample(
            &format!("harmony/vines/{attacker_addr}/tombstones/vine-victim"),
            &signed_tombstone_bytes(&attacker_id, "vine-victim", "cid-aaa", &attacker_addr),
        );
        assert!(matches!(outcome, Some(TombstoneOutcome::Applied { .. })));

        // Victim's descriptor arrives — different creator, must insert.
        let payload = canonical_descriptor_bytes(
            "vine-victim",
            "victim-addr",
            "Victim",
            "cid-aaa",
            None,
            None,
            1_700_000_000,
            None,
            None,
        );
        let outcome = cache.on_descriptor_sample(
            &topic("victim-addr"),
            &payload,
            &followed_set_with(&[]),
            1_700_000_000_000,
        );
        assert!(
            matches!(outcome, Some(DescriptorOutcome::Inserted { .. })),
            "attacker tombstone must not block the victim, got {outcome:?}"
        );
        // A tombstone from the vine's REAL creator still applies normally
        // (ownership check compares against the now-cached descriptor).
        assert_eq!(cache.len_descriptors(), 1);
    }

    #[test]
    fn runtime_descriptor_insert_rejects_ancient_created_at() {
        // CodeRabbit PR #445 round 1: the age prune ran only on load, so a
        // live re-delivery of an ancient descriptor could outlive its
        // tombstone's retention window and resurrect. Runtime now mirrors
        // the load gate; with `deleted_at >= created_at`, a pruned
        // tombstone's descriptor is always rejected here too.
        let mut cache = VineFeedCache::new();
        let now_secs: u64 = 2_000_000_000;
        let ancient = now_secs - MAX_AGE_SECS - 10;
        let payload = canonical_descriptor_bytes(
            "vine-old", "a-addr", "A", "cid-old", None, None, ancient, None, None,
        );
        let outcome = cache.on_descriptor_sample(
            &topic("a-addr"),
            &payload,
            &followed_set_with(&[]),
            now_secs * 1000,
        );
        assert!(
            matches!(outcome, Some(DescriptorOutcome::Rejected(ref r)) if r.contains("retention")),
            "ancient descriptor must reject at runtime, got {outcome:?}"
        );
    }

    #[test]
    fn tombstone_cap_trims_oldest() {
        let (id, addr) = tombstone_identity();
        let mut cache = VineFeedCache::new();
        for i in 0..MAX_TOMBSTONES {
            cache.seed_tombstone_for_test(&format!("seed-{i:05}"), 1_000 + i as u64);
        }
        // One real apply pushes past the cap; the oldest seeded entry
        // (deleted_at = 1000) trims, the fresh one stays.
        let outcome = cache.on_tombstone_sample(
            &format!("harmony/vines/{addr}/tombstones/vine-new"),
            &signed_tombstone_bytes(&id, "vine-new", "cid-new", &addr),
        );
        assert!(matches!(outcome, Some(TombstoneOutcome::Applied { .. })));
        assert_eq!(cache.len_tombstones(), MAX_TOMBSTONES);

        // The freshly applied tombstone survived the trim (its guard works).
        let payload = canonical_descriptor_bytes(
            "vine-new",
            &addr,
            "Creator",
            "cid-new",
            None,
            None,
            1_700_000_000,
            None,
            None,
        );
        let outcome =
            cache.on_descriptor_sample(&topic(&addr), &payload, &followed_set_with(&[]), 1_000);
        assert!(matches!(outcome, Some(DescriptorOutcome::Rejected(_))));
    }

    #[test]
    fn tombstone_rejects_topic_vine_id_mismatch() {
        // Qodo PR #445 round 1: the key's vine-id segment is bound to the
        // signed payload — a misrouted publish must not apply.
        let (id, addr) = tombstone_identity();
        let mut cache = VineFeedCache::new();
        insert_descriptor(&mut cache, "vine-1", &addr, "cid-aaa");

        let outcome = cache.on_tombstone_sample(
            &format!("harmony/vines/{addr}/tombstones/vine-OTHER"),
            &signed_tombstone_bytes(&id, "vine-1", "cid-aaa", &addr),
        );
        assert!(
            matches!(outcome, Some(TombstoneOutcome::Rejected(ref r)) if r.contains("topic vine id")),
            "got {outcome:?}"
        );
        assert_eq!(cache.len_descriptors(), 1);
    }

    #[test]
    fn tombstone_canonicalizes_cid_from_cached_descriptor() {
        // Qodo PR #445 round 1: when the payload's CID disagrees with the
        // cached descriptor's (unsigned-descriptor world), the CACHED CID
        // is what local rows and blob state reference — persist that one
        // for stub matching and evict decisions.
        let (id, addr) = tombstone_identity();
        let mut cache = VineFeedCache::new();
        insert_descriptor(&mut cache, "vine-1", &addr, "cid-real");
        insert_reshare(
            &mut cache, "vine-b", "bob-addr", "vine-1", &addr, "cid-real",
        );

        let outcome = cache.on_tombstone_sample(
            &format!("harmony/vines/{addr}/tombstones/vine-1"),
            &signed_tombstone_bytes(&id, "vine-1", "cid-DIFFERENT", &addr),
        );
        match outcome {
            Some(TombstoneOutcome::Applied { removed, evict_cid }) => {
                assert_eq!(removed.video_cid, "cid-real");
                // Bob's reshare is a stub (direct id match) — nothing live
                // references cid-real, so the canonical CID evicts.
                assert_eq!(evict_cid.as_deref(), Some("cid-real"));
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        let rows = cache.list_descriptors();
        assert!(rows[0].original_removed, "direct reshare must stub");
    }

    #[test]
    fn tombstones_persist_and_block_after_reload() {
        let (id, addr) = tombstone_identity();
        let dir = tempfile::tempdir().expect("create tempdir");
        {
            let mut cache = VineFeedCache::load(dir.path());
            insert_descriptor(&mut cache, "vine-1", &addr, "cid-aaa");
            cache.on_tombstone_sample(
                &format!("harmony/vines/{addr}/tombstones/vine-1"),
                &signed_tombstone_bytes(&id, "vine-1", "cid-aaa", &addr),
            );
        }

        let mut reloaded = VineFeedCache::load(dir.path());
        assert_eq!(reloaded.len_descriptors(), 0);
        // Descriptor re-arrival after restart must still be blocked.
        let payload = canonical_descriptor_bytes(
            "vine-1",
            &addr,
            "Creator",
            "cid-aaa",
            None,
            None,
            1_700_000_000,
            None,
            None,
        );
        let outcome =
            reloaded.on_descriptor_sample(&topic(&addr), &payload, &followed_set_with(&[]), 1_000);
        assert!(
            matches!(outcome, Some(DescriptorOutcome::Rejected(ref r)) if r.contains("tombstoned")),
            "reloaded tombstone must still block, got {outcome:?}"
        );
    }

    #[test]
    fn legacy_v1_file_without_tombstones_field_loads() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let legacy = serde_json::json!({
            "version": 1,
            "descriptors": [],
            "reactions": [],
            "viewed": ["vine-x"],
        });
        std::fs::write(
            dir.path().join("vine_feed.json"),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let cache = VineFeedCache::load(dir.path());
        assert!(cache.is_viewed("vine-x"), "legacy file must load unchanged");
    }

    // ── ZEB-671: follow-list ingest ────────────────────────────────────

    /// Follow-list topic for a named test signer.
    fn follows_topic(owner: &str) -> String {
        format!("harmony/vines/{}/follows", addr(owner))
    }

    /// Signed follow-list JSON for a named owner; `follows` are signer
    /// NAMES translated to derived addresses (mirroring production).
    fn follow_list_bytes(owner: &str, follows: &[&str], updated_at: u64) -> Vec<u8> {
        let mut payload = crate::VineFollowListPayload {
            owner_address: addr(owner),
            follows: follows.iter().map(|n| addr(n)).collect(),
            updated_at,
            identity_pub: None,
            sig: None,
            device_sig: None,
        };
        crate::vine_signing::sign_follow_list(&signer_for(owner), &mut payload);
        serde_json::to_vec(&payload).unwrap()
    }

    #[test]
    fn signed_follow_list_inserts() {
        let mut cache = VineFeedCache::new();
        let outcome = cache.on_follow_list_sample(
            &follows_topic("fl-alice"),
            &follow_list_bytes("fl-alice", &["fl-bob", "fl-carol"], 100),
        );
        assert_eq!(outcome, FollowListOutcome::Inserted);
        let entry = cache.follow_lists().get(&addr("fl-alice")).expect("stored");
        assert_eq!(entry.follows, vec![addr("fl-bob"), addr("fl-carol")]);
        assert_eq!(entry.updated_at, 100);
    }

    #[test]
    fn unsigned_follow_list_is_rejected() {
        let mut cache = VineFeedCache::new();
        let bare = serde_json::json!({
            "ownerAddress": addr("fl-alice"),
            "follows": [addr("fl-bob")],
            "updatedAt": 100u64
        });
        let outcome = cache.on_follow_list_sample(
            &follows_topic("fl-alice"),
            &serde_json::to_vec(&bare).unwrap(),
        );
        assert!(
            matches!(outcome, FollowListOutcome::Rejected(ref r) if r.contains("unsigned")),
            "got {outcome:?}"
        );
        assert!(cache.follow_lists().is_empty(), "no state effect");
    }

    #[test]
    fn tampered_follow_list_is_rejected() {
        let mut cache = VineFeedCache::new();
        let mut payload: crate::VineFollowListPayload =
            serde_json::from_slice(&follow_list_bytes("fl-alice", &["fl-bob"], 100)).unwrap();
        payload.follows.push(addr("fl-mallory"));
        let outcome = cache.on_follow_list_sample(
            &follows_topic("fl-alice"),
            &serde_json::to_vec(&payload).unwrap(),
        );
        assert!(
            matches!(outcome, FollowListOutcome::Rejected(ref r) if r.contains("signature invalid")),
            "got {outcome:?}"
        );
        assert!(cache.follow_lists().is_empty(), "no state effect");
    }

    #[test]
    fn follow_list_replayed_on_foreign_or_deeper_topic_is_rejected() {
        let mut cache = VineFeedCache::new();
        let bytes = follow_list_bytes("fl-alice", &["fl-bob"], 100);

        // Foreign owner topic.
        let outcome = cache.on_follow_list_sample(&follows_topic("fl-eve"), &bytes);
        assert!(
            matches!(outcome, FollowListOutcome::Rejected(ref r) if r.contains("does not match payload owner")),
            "got {outcome:?}"
        );

        // Deeper key below the canonical shape.
        let deeper = format!("harmony/vines/{}/extra/follows", addr("fl-alice"));
        let outcome = cache.on_follow_list_sample(&deeper, &bytes);
        assert!(
            matches!(outcome, FollowListOutcome::Rejected(_)),
            "got {outcome:?}"
        );

        assert!(cache.follow_lists().is_empty(), "no state effect");
    }

    #[test]
    fn follow_list_lww_keeps_newest() {
        let mut cache = VineFeedCache::new();
        cache.on_follow_list_sample(
            &follows_topic("fl-alice"),
            &follow_list_bytes("fl-alice", &["fl-bob"], 100),
        );

        // Older ignored.
        let outcome = cache.on_follow_list_sample(
            &follows_topic("fl-alice"),
            &follow_list_bytes("fl-alice", &["fl-eve"], 50),
        );
        assert_eq!(outcome, FollowListOutcome::IgnoredOlder);

        // Equal timestamp ignored (replay is a no-op).
        let outcome = cache.on_follow_list_sample(
            &follows_topic("fl-alice"),
            &follow_list_bytes("fl-alice", &["fl-eve"], 100),
        );
        assert_eq!(outcome, FollowListOutcome::IgnoredOlder);

        // Newer replaces — including the empty-list retraction.
        let outcome = cache.on_follow_list_sample(
            &follows_topic("fl-alice"),
            &follow_list_bytes("fl-alice", &[], 200),
        );
        assert_eq!(outcome, FollowListOutcome::UpdatedNewer);
        let entry = cache.follow_lists().get(&addr("fl-alice")).unwrap();
        assert!(entry.follows.is_empty(), "retraction clears the edges");
        assert_eq!(entry.updated_at, 200);
    }

    #[test]
    fn oversized_wire_payload_is_rejected_before_parse() {
        let mut cache = VineFeedCache::new();
        let blob = vec![b'x'; MAX_FOLLOW_LIST_WIRE_BYTES + 1];
        let outcome = cache.on_follow_list_sample(&follows_topic("fl-alice"), &blob);
        assert!(
            matches!(outcome, FollowListOutcome::Rejected(ref r) if r.contains("wire cap")),
            "got {outcome:?}"
        );
        assert!(cache.follow_lists().is_empty(), "no state effect");
    }

    #[test]
    fn oversized_follow_list_is_rejected() {
        let mut cache = VineFeedCache::new();
        let owner = signer_for("fl-alice");
        let mut payload = crate::VineFollowListPayload {
            owner_address: addr("fl-alice"),
            follows: (0..=MAX_FOLLOWS_PER_LIST)
                .map(|i| format!("{i:064x}"))
                .collect(),
            updated_at: 100,
            identity_pub: None,
            sig: None,
            device_sig: None,
        };
        crate::vine_signing::sign_follow_list(&owner, &mut payload);
        let outcome = cache.on_follow_list_sample(
            &follows_topic("fl-alice"),
            &serde_json::to_vec(&payload).unwrap(),
        );
        assert!(
            matches!(outcome, FollowListOutcome::Rejected(ref r) if r.contains("exceeds cap")),
            "got {outcome:?}"
        );
        assert!(cache.follow_lists().is_empty(), "no state effect");
    }

    #[test]
    fn follow_lists_survive_disk_reload() {
        let dir = tempfile::tempdir().expect("create tempdir");
        {
            let mut cache = VineFeedCache::load(dir.path());
            let outcome = cache.on_follow_list_sample(
                &follows_topic("fl-alice"),
                &follow_list_bytes("fl-alice", &["fl-bob"], 100),
            );
            assert_eq!(outcome, FollowListOutcome::Inserted);
        }
        let reloaded = VineFeedCache::load(dir.path());
        let entry = reloaded
            .follow_lists()
            .get(&addr("fl-alice"))
            .expect("follow list must survive reload");
        assert_eq!(entry.follows, vec![addr("fl-bob")]);
        assert_eq!(entry.updated_at, 100);

        // Disk shape pin: sigs are NOT retained (verify-once-at-ingest).
        let raw = std::fs::read_to_string(dir.path().join("vine_feed.json")).unwrap();
        assert!(
            raw.contains("follow_lists"),
            "envelope key is snake_case like its siblings"
        );
        assert!(!raw.contains("identityPub"), "sigs never persist");
    }

    #[test]
    fn set_graph_inputs_reports_reach_changes() {
        let mut cache = VineFeedCache::new();
        // Inputs with no cached lists → reach stays empty → no change.
        assert!(!cache.set_graph_inputs(addr("fl-me"), vec![addr("fl-devin")]));

        // devin → ravi arrives: recompute flips ravi to 2°.
        cache.on_follow_list_sample(
            &follows_topic("fl-devin"),
            &follow_list_bytes("fl-devin", &["fl-ravi"], 100),
        );
        assert!(cache.recompute_reach(), "new 2° edge must report change");
        assert!(!cache.recompute_reach(), "idempotent recompute");

        // ravi → ada extends to 3°.
        cache.on_follow_list_sample(
            &follows_topic("fl-ravi"),
            &follow_list_bytes("fl-ravi", &["fl-ada"], 100),
        );
        assert!(cache.recompute_reach(), "new 3° edge must report change");

        // Unfollow devin: reach collapses to empty.
        assert!(cache.set_graph_inputs(addr("fl-me"), vec![]));
    }

    #[test]
    fn reach_annotations_populate_discover_dtos() {
        let mut cache = VineFeedCache::new();

        // A vine from ravi arrives as Discover (nobody followed).
        let payload = canonical_descriptor_bytes(
            "vine-reach-1",
            "fl-ravi",
            "Ravi",
            &"aa".repeat(32),
            None,
            None,
            1_000,
            None,
            None,
        );
        let outcome =
            cache.on_descriptor_sample(&topic("fl-ravi"), &payload, &followed_set_with(&[]), 1_000);
        assert!(matches!(outcome, Some(DescriptorOutcome::Inserted { .. })));

        // No graph yet → no annotation.
        let rows = cache.list_descriptors();
        assert_eq!(rows[0].degree, None);

        // devin (my follow) publishes a list containing ravi → 2°.
        cache.on_follow_list_sample(
            &follows_topic("fl-devin"),
            &follow_list_bytes("fl-devin", &["fl-ravi"], 100),
        );
        cache.set_graph_inputs(addr("fl-me"), vec![addr("fl-devin")]);

        let rows = cache.list_descriptors();
        let row = rows.iter().find(|r| r.id == "vine-reach-1").unwrap();
        assert_eq!(row.degree, Some(2));
        assert_eq!(row.via, Some(vec![addr("fl-devin")]));
    }

    #[test]
    fn follow_list_cap_evicts_stalest() {
        let mut cache = VineFeedCache::new();
        // Fill to cap + 1 with direct (non-registry) identities — the
        // registry would otherwise balloon for every later test.
        for i in 0..=MAX_FOLLOW_LISTS {
            let private = harmony_identity::PrivateIdentity::generate(&mut rand::rngs::OsRng);
            let owner_addr = hex::encode(private.public_identity().address_hash);
            let mut payload = crate::VineFollowListPayload {
                owner_address: owner_addr.clone(),
                follows: vec![],
                updated_at: i as u64, // strictly increasing — entry 0 is stalest
                identity_pub: None,
                sig: None,
                device_sig: None,
            };
            crate::vine_signing::sign_follow_list(&private, &mut payload);
            let outcome = cache.on_follow_list_sample(
                &format!("harmony/vines/{owner_addr}/follows"),
                &serde_json::to_vec(&payload).unwrap(),
            );
            assert_eq!(outcome, FollowListOutcome::Inserted, "insert #{i}");
        }
        assert_eq!(
            cache.follow_lists().len(),
            MAX_FOLLOW_LISTS,
            "cap holds after overflow"
        );
        assert!(
            !cache.follow_lists().values().any(|e| e.updated_at == 0),
            "stalest entry evicted"
        );
    }

    // ── ZEB-678 S2: dual-path (authority-aware) admission ────────────────

    /// Authority sub-topic for a named test feed.
    fn authority_topic(creator: &str) -> String {
        format!("harmony/vines/{}/authority", addr(creator))
    }

    /// Serialized `FeedAuthorityRecord` binding `creator`'s feed to the enrolled
    /// `#2` device `world.a`. `revoke` layers an A+B quorum revocation of that
    /// device (with the signer bundle needed to verify it). Reuses the public
    /// builder for the (unchanged) `#3` binding, then attaches the optional
    /// signer-bundle / revocation fields — neither is covered by `n_sig`, so the
    /// binding stays valid.
    fn authority_record(
        creator: &str,
        world: &crate::enrollment_verify::quorum_fixtures::QuorumWorld,
        updated_at_ms: u64,
        revoke: bool,
    ) -> Vec<u8> {
        use crate::enrollment_verify::quorum_fixtures::WORLD_NOW;
        let n = signer_for(creator);
        let mut rec = crate::feed_authority::build_active_authority(
            &n,
            &world.a_sk,
            &world.a_cert,
            &[],
            updated_at_ms,
        )
        .expect("build authority record");
        assert_eq!(
            rec.feed_id,
            addr(creator),
            "authority feed_id == creator feed"
        );
        if revoke {
            let rev = crate::enrollment_verify::quorum_fixtures::mint_quorum_revocation(
                world,
                world.a_cert.device_id,
                WORLD_NOW,
            );
            rec.signer_certs_cbor_hex =
                crate::feed_authority::encode_certs(&world.bundle).expect("encode signer bundle");
            rec.revocation_cbor_hex =
                Some(crate::feed_authority::encode_revocation(&rev).expect("encode revocation"));
        }
        serde_json::to_vec(&rec).unwrap()
    }

    /// A `#2`-signed descriptor for `creator`'s feed, signed by device key `sk`.
    /// Leaves the legacy `#3` fields (`identity_pub`/`sig`) `None` — post-migration
    /// records are pure `#2`.
    fn v2_descriptor_bytes(
        vine_id: &str,
        creator: &str,
        sk: &ed25519_dalek::SigningKey,
    ) -> Vec<u8> {
        let mut d = crate::VineDescriptorPayload {
            id: vine_id.to_string(),
            creator_address: addr(creator),
            creator_name: "Creator".into(),
            created_at: 1_700_000_000,
            video_cid: format!("cid-{vine_id}"),
            title: None,
            reshare_of: None,
            original_creator_address: None,
            original_creator_name: None,
            identity_pub: None,
            sig: None,
            device_sig: None,
        };
        crate::vine_signing::sign_descriptor_v2(sk, &mut d);
        serde_json::to_vec(&d).unwrap()
    }

    /// The S2 cutover: a legacy `#3`-signed descriptor is admitted while the feed
    /// has no authority record, then rejected the instant the feed's authority
    /// record migrates it to the `#2` device key; a `#2`-signed descriptor is
    /// accepted post-migration.
    #[test]
    fn descriptor_legacy_accepted_pre_authority_rejected_post_authority() {
        use crate::enrollment_verify::quorum_fixtures::{mint_quorum_world, WORLD_NOW};
        let world = mint_quorum_world(0xE0);
        let creator = "s2-migrating-feed";
        let feed_topic = topic(creator);
        let followed = followed_set_with(&[creator]);
        let now_ms = WORLD_NOW * 1000;

        let mut cache = VineFeedCache::new();

        // 1) Legacy #3 descriptor, no authority cached → accepted.
        let d1 = canonical_descriptor_bytes(
            "vine-s2-legacy",
            creator,
            "Creator",
            "cid-legacy",
            None,
            None,
            WORLD_NOW,
            None,
            None,
        );
        assert!(
            matches!(
                cache.on_descriptor_sample(&feed_topic, &d1, &followed, now_ms),
                Some(DescriptorOutcome::Inserted { .. })
            ),
            "legacy #3 descriptor accepted before the feed migrates"
        );

        // 2) Ingest the feed's authority record → the feed migrates to #2.
        cache.on_authority_sample(
            &authority_topic(creator),
            &authority_record(creator, &world, now_ms, false),
            WORLD_NOW,
        );

        // 3) A #3-only descriptor is now rejected (authority cached ⇒ #2 required).
        let d3 = canonical_descriptor_bytes(
            "vine-s2-post-legacy",
            creator,
            "Creator",
            "cid-post-legacy",
            None,
            None,
            WORLD_NOW + 1,
            None,
            None,
        );
        assert!(
            matches!(
                cache.on_descriptor_sample(&feed_topic, &d3, &followed, now_ms),
                Some(DescriptorOutcome::Rejected(_))
            ),
            "post-migration a #3-only descriptor is rejected"
        );

        // 4) A #2-signed descriptor from the enrolled device is accepted.
        let d2 = v2_descriptor_bytes("vine-s2-v2", creator, &world.a_sk);
        assert!(
            matches!(
                cache.on_descriptor_sample(&feed_topic, &d2, &followed, now_ms),
                Some(DescriptorOutcome::Inserted { .. })
            ),
            "post-migration a #2-signed descriptor is accepted"
        );
    }

    /// Once a feed's authority carries a valid revocation of its publisher
    /// device, even a correctly `#2`-signed descriptor is rejected — this is the
    /// revocation-aware admission that is the point of ZEB-678.
    #[test]
    fn descriptor_rejected_when_feed_authority_revoked() {
        use crate::enrollment_verify::quorum_fixtures::{mint_quorum_world, WORLD_NOW};
        let world = mint_quorum_world(0xE4);
        let creator = "s2-revoked-feed";
        let feed_topic = topic(creator);
        let followed = followed_set_with(&[creator]);
        let now_ms = WORLD_NOW * 1000;
        let mut cache = VineFeedCache::new();

        // Migrate the feed, then revoke the publisher device (sticky).
        cache.on_authority_sample(
            &authority_topic(creator),
            &authority_record(creator, &world, now_ms, false),
            WORLD_NOW,
        );
        cache.on_authority_sample(
            &authority_topic(creator),
            &authority_record(creator, &world, now_ms + 10_000, true),
            WORLD_NOW,
        );

        // A correctly #2-signed descriptor is rejected — the device is revoked.
        let d = v2_descriptor_bytes("vine-after-revoke", creator, &world.a_sk);
        let out = cache.on_descriptor_sample(&feed_topic, &d, &followed, now_ms);
        assert!(
            matches!(&out, Some(DescriptorOutcome::Rejected(e)) if e.contains("revoked")),
            "a revoked publisher's descriptor is rejected: {out:?}"
        );
    }

    /// Defense-in-depth for the event-loop router (Task 8): the `emit_frontend_event`
    /// dispatch only forwards `.../authority` keys here, but `on_authority_sample`
    /// re-validates the topic shape so a misroute can never pin a feed from a
    /// reaction / descriptor / malformed key.
    #[test]
    fn on_authority_sample_ignores_non_authority_topics() {
        use crate::enrollment_verify::quorum_fixtures::{mint_quorum_world, WORLD_NOW};
        let world = mint_quorum_world(0xE8);
        let creator = "s2-filter-feed";
        let payload = authority_record(creator, &world, WORLD_NOW * 1000, false);
        let mut cache = VineFeedCache::new();

        // Wrong sub-topic: a reactions key (never routed here in production).
        cache.on_authority_sample(
            &format!("harmony/vines/{}/reactions/r1", addr(creator)),
            &payload,
            WORLD_NOW,
        );
        // Bare descriptor topic — no `/authority` suffix.
        cache.on_authority_sample(&topic(creator), &payload, WORLD_NOW);
        // Nested feed segment: feed id `{addr}/nested` contains '/'.
        cache.on_authority_sample(
            &format!("harmony/vines/{}/nested/authority", addr(creator)),
            &payload,
            WORLD_NOW,
        );
        assert!(
            cache.authority.get(&addr(creator)).is_none(),
            "no feed pinned from a non-authority or malformed topic"
        );

        // The correctly-addressed authority record DOES pin — proving the
        // negatives above are the topic filter, not a broken payload.
        cache.on_authority_sample(&authority_topic(creator), &payload, WORLD_NOW);
        assert!(
            cache.authority.get(&addr(creator)).is_some(),
            "the correctly-addressed authority record pins the feed"
        );
    }

    // ── ZEB-678 S2 review-fix: dual-signing (out-of-order + reactor bind) ──

    /// A DUAL-signed descriptor: `#3` (binds `creator_address`) + `#2` device_sig.
    fn dual_signed_descriptor_bytes(
        vine_id: &str,
        creator: &str,
        sk: &ed25519_dalek::SigningKey,
    ) -> Vec<u8> {
        let signer = signer_for(creator);
        let mut d = crate::VineDescriptorPayload {
            id: vine_id.to_string(),
            creator_address: addr(creator),
            creator_name: "Creator".into(),
            created_at: 1_700_000_000,
            video_cid: format!("cid-{vine_id}"),
            title: None,
            reshare_of: None,
            original_creator_address: None,
            original_creator_name: None,
            identity_pub: None,
            sig: None,
            device_sig: None,
        };
        crate::vine_signing::sign_descriptor(&signer, &mut d);
        crate::vine_signing::sign_descriptor_v2(sk, &mut d);
        serde_json::to_vec(&d).unwrap()
    }

    /// The out-of-order fix (Qodo #1): a dual-signed descriptor is admitted BOTH
    /// before its feed's authority is cached (legacy `#3` path — the record can
    /// arrive before `/authority` over unordered Zenoh) AND after migration (via
    /// the `#2` path).
    #[test]
    fn dual_signed_descriptor_accepted_pre_and_post_authority() {
        use crate::enrollment_verify::quorum_fixtures::{mint_quorum_world, WORLD_NOW};
        let world = mint_quorum_world(0xEE);
        let creator = "s2-dual-desc-feed";
        let feed_topic = topic(creator);
        let followed = followed_set_with(&[creator]);
        let now_ms = WORLD_NOW * 1000;
        let mut cache = VineFeedCache::new();

        // Pre-authority: accepted via the legacy `#3` binding.
        let d1 = dual_signed_descriptor_bytes("vine-dual-1", creator, &world.a_sk);
        assert!(
            matches!(
                cache.on_descriptor_sample(&feed_topic, &d1, &followed, now_ms),
                Some(DescriptorOutcome::Inserted { .. })
            ),
            "dual-signed descriptor accepted before its authority arrives"
        );

        // Migrate the feed, then a new dual-signed descriptor is accepted via `#2`.
        cache.on_authority_sample(
            &authority_topic(creator),
            &authority_record(creator, &world, now_ms, false),
            WORLD_NOW,
        );
        let d2 = dual_signed_descriptor_bytes("vine-dual-2", creator, &world.a_sk);
        assert!(
            matches!(
                cache.on_descriptor_sample(&feed_topic, &d2, &followed, now_ms),
                Some(DescriptorOutcome::Inserted { .. })
            ),
            "dual-signed descriptor accepted after migration via #2"
        );
    }

    /// The impersonation fix (CodeRabbit security): a reaction carrying a valid
    /// `#2` `device_sig` + enrollment but NO `#3` binding for its claimed
    /// `reactor_address` is rejected — an enrolled device must not be able to
    /// mint a reaction for an arbitrary address.
    #[test]
    fn reaction_with_v2_sig_but_unbound_reactor_address_is_rejected() {
        use crate::enrollment_verify::quorum_fixtures::{mint_quorum_world, WORLD_NOW};
        let world = mint_quorum_world(0xEC);
        let mut cache = VineFeedCache::new();
        let victim = "s2-victim-addr";
        let mut wire = crate::VineReactionPayload {
            vine_id: "vine-imp".into(),
            reactor_address: addr(victim),
            reactor_name: "Victim".into(),
            liked: true,
            timestamp: 100,
            identity_pub: None,
            sig: None,
            owner_id: Some(hex::encode(world.a_cert.owner_id)),
            enrollment_cbor_hex: Some(crate::feed_authority::encode_cert(&world.a_cert).unwrap()),
            signer_certs_cbor_hex: String::new(),
            device_sig: None,
        };
        // Attacker signs with its own enrolled `#2` key only (no victim `#3` key).
        crate::vine_signing::sign_reaction_v2(&world.a_sk, &mut wire);
        let bytes = serde_json::to_vec(&wire).unwrap();
        let out = cache.on_reaction_sample(
            &reaction_topic("s2-creator", "vine-imp", victim),
            &bytes,
            WORLD_NOW,
        );
        assert!(
            matches!(out, Some(ReactionOutcome::Rejected(ref e)) if e.contains("unsigned")),
            "a #2-only reaction with an unbound reactor_address is rejected: {out:?}"
        );
        assert_eq!(cache.len_reactions(), 0);
    }

    /// A properly dual-signed reaction (`#3` binds `reactor_address`, `#2` rides
    /// on top) is admitted.
    #[test]
    fn dual_signed_reaction_is_accepted() {
        use crate::enrollment_verify::quorum_fixtures::{mint_quorum_world, WORLD_NOW};
        let world = mint_quorum_world(0xED);
        let mut cache = VineFeedCache::new();
        let reactor = "s2-dual-reactor";
        let signer = signer_for(reactor);
        let mut wire = crate::VineReactionPayload {
            vine_id: "vine-dual".into(),
            reactor_address: addr(reactor),
            reactor_name: "Reactor".into(),
            liked: true,
            timestamp: 100,
            identity_pub: None,
            sig: None,
            owner_id: Some(hex::encode(world.a_cert.owner_id)),
            enrollment_cbor_hex: Some(crate::feed_authority::encode_cert(&world.a_cert).unwrap()),
            signer_certs_cbor_hex: String::new(),
            device_sig: None,
        };
        crate::vine_signing::sign_reaction(&signer, &mut wire); // #3 binds reactor_address
        crate::vine_signing::sign_reaction_v2(&world.a_sk, &mut wire); // #2 defense-in-depth
        let bytes = serde_json::to_vec(&wire).unwrap();
        let out = cache.on_reaction_sample(
            &reaction_topic("s2-vine-creator", "vine-dual", reactor),
            &bytes,
            WORLD_NOW,
        );
        assert!(
            matches!(out, Some(ReactionOutcome::Inserted)),
            "dual-signed reaction accepted: {out:?}"
        );
    }
}
