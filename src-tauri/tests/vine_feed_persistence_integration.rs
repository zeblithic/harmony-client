//! ZEB-147: black-box integration test for disk-backed VineFeedCache.
//!
//! Validates the full app-reload cycle:
//! 1. Construct a cache via `VineFeedCache::load(tempdir)`.
//! 2. Mutate it through the three public mutators (descriptor sample,
//!    reaction sample, mark_viewed).
//! 3. Drop the cache.
//! 4. Re-load from the same tempdir.
//! 5. Assert all three pieces of state are preserved.

use harmony_app::vine_feed_cache::{DescriptorOutcome, ReactionOutcome, VineFeedCache};
use harmony_app::{VineDescriptorPayload, VineReactionPayload};
use std::collections::HashSet;

fn canonical_descriptor_bytes(
    vine_id: &str,
    creator_address: &str,
    creator_name: &str,
    video_cid: &str,
    title: Option<&str>,
    reshare_of: Option<&str>,
    created_at: u64,
) -> Vec<u8> {
    let v = VineDescriptorPayload {
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

fn canonical_reaction_bytes(
    vine_id: &str,
    reactor_address: &str,
    reactor_name: &str,
    liked: bool,
    timestamp: u64,
) -> Vec<u8> {
    let v = VineReactionPayload {
        vine_id: vine_id.to_string(),
        reactor_address: reactor_address.to_string(),
        reactor_name: reactor_name.to_string(),
        liked,
        timestamp,
    };
    serde_json::to_vec(&v).unwrap()
}

#[test]
fn cache_survives_reload() {
    let dir = tempfile::tempdir().expect("create tempdir");

    // Use timestamps inside the 90-day window so age-pruning on reload
    // does not strip the inserted state.
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let recent_a = now_secs.saturating_sub(120); // 2 min old
    let recent_b = now_secs.saturating_sub(60); // 1 min old

    // Phase 1: build state and let save-on-mutation persist it
    {
        let mut cache = VineFeedCache::load(dir.path());

        let followed: HashSet<String> = ["alice-addr".to_string()].into_iter().collect();

        // Insert a descriptor with a reshare-of pointer (preserves the
        // optional field through round-trip).
        let desc = canonical_descriptor_bytes(
            "vine-A",
            "alice-addr",
            "Alice",
            "cid-aaa",
            Some("hello"),
            Some("vine-prev"),
            recent_a,
        );
        let out = cache.on_descriptor_sample("harmony/vines/alice-addr", &desc, &followed, 10_000);
        assert!(
            matches!(out, Some(DescriptorOutcome::Inserted { .. })),
            "first descriptor must Insert; got {out:?}"
        );

        // Insert a second descriptor with no title and no reshare (covers
        // skip_serializing_if = "Option::is_none" round-trip).
        let desc2 = canonical_descriptor_bytes(
            "vine-B",
            "alice-addr",
            "Alice",
            "cid-bbb",
            None,
            None,
            recent_b,
        );
        let out2 =
            cache.on_descriptor_sample("harmony/vines/alice-addr", &desc2, &followed, 10_500);
        assert!(matches!(out2, Some(DescriptorOutcome::Inserted { .. })));

        // Insert two reactions on vine-A — different reactors, both liked
        let r1 = canonical_reaction_bytes("vine-A", "bob-addr", "Bob", true, recent_a + 5);
        let r2 = canonical_reaction_bytes("vine-A", "carol-addr", "Carol", true, recent_a + 10);
        assert_eq!(
            cache.on_reaction_sample("harmony/vines/alice-addr/reactions/vine-A/bob-addr", &r1),
            Some(ReactionOutcome::Inserted)
        );
        assert_eq!(
            cache.on_reaction_sample("harmony/vines/alice-addr/reactions/vine-A/carol-addr", &r2),
            Some(ReactionOutcome::Inserted)
        );

        // Mark vine-A viewed
        assert!(cache.mark_viewed("vine-A".to_string()));

        // cache drops here — save-on-mutation ensured everything is on disk
    }

    // Phase 2: reload from the same tempdir
    let cache2 = VineFeedCache::load(dir.path());

    // Descriptors: both present, sorted DESC by created_at
    let dtos = cache2.list_descriptors();
    assert_eq!(dtos.len(), 2, "both descriptors should survive reload");
    assert_eq!(dtos[0].id, "vine-B"); // newer
    assert!(dtos[0].title.is_none());
    assert!(dtos[0].reshare_of.is_none());
    assert_eq!(dtos[1].id, "vine-A");
    assert_eq!(dtos[1].title.as_deref(), Some("hello"));
    assert_eq!(dtos[1].reshare_of.as_deref(), Some("vine-prev"));
    assert!(dtos[1].viewed, "vine-A viewed flag must survive reload");
    assert!(!dtos[0].viewed, "vine-B was never viewed");

    // Reactions: both present (count == 2, neither liked_by_me from
    // dave-addr's perspective)
    let summary = cache2.get_reaction("vine-A", "dave-addr");
    assert_eq!(summary.count, 2);
    assert!(!summary.liked_by_me);

    // From Bob's perspective: liked_by_me must be true (he's one of the
    // two who liked it)
    let bob_view = cache2.get_reaction("vine-A", "bob-addr");
    assert_eq!(bob_view.count, 2);
    assert!(bob_view.liked_by_me);

    // Viewed-state survives independently
    assert!(cache2.is_viewed("vine-A"));
    assert!(!cache2.is_viewed("vine-B"));
}
