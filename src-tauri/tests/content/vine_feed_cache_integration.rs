//! ZEB-286: Integration tests for VineFeedCache.
//!
//! Models `profile_broadcast_integration.rs` — hands canonical wire
//! bytes (built via `serde_json::to_vec` on the same `VineDescriptorPayload`
//! / `VineReactionPayload` types that production `publish_vine` emits)
//! directly to the recipient's `VineFeedCache`. The Zenoh transport
//! layer is covered by codebase-wide "real Zenoh too heavy" precedent.
//!
//! Full design: `docs/specs/2026-05-13-zeb-286-vine-integration-test-design.md`.

use crate::vine_signing_testutil::{addr, reaction_topic, signer_for, topic};
use harmony_app::vine_feed_cache::{DescriptorOutcome, ReactionOutcome, VineFeedCache, VineSource};
use harmony_app::{VineDescriptorPayload, VineReactionPayload};
use std::collections::HashSet;

// ── Fixture helpers ─────────────────────────────────────────────────

/// `creator` is a signer NAME — translated to its derived address and
/// creator-signed (ZEB-673 strict wire admission).
fn make_descriptor(
    vine_id: &str,
    creator: &str,
    creator_name: &str,
    video_cid: &str,
    title: Option<&str>,
    reshare_of: Option<&str>,
    created_at: u64,
) -> VineDescriptorPayload {
    let mut v = VineDescriptorPayload {
        id: vine_id.to_string(),
        creator_address: addr(creator),
        creator_name: creator_name.to_string(),
        created_at,
        video_cid: video_cid.to_string(),
        title: title.map(String::from),
        reshare_of: reshare_of.map(String::from),
        original_creator_address: None,
        original_creator_name: None,
        identity_pub: None,
        sig: None,
        device_sig: None,
    };
    harmony_app::vine_signing::sign_descriptor(&signer_for(creator), &mut v);
    v
}

fn descriptor_bytes(d: &VineDescriptorPayload) -> Vec<u8> {
    serde_json::to_vec(d).expect("descriptor to_vec")
}

/// `reactor` is a signer NAME — translated and reactor-signed (ZEB-673).
fn make_reaction(
    vine_id: &str,
    reactor: &str,
    reactor_name: &str,
    liked: bool,
    timestamp: u64,
) -> VineReactionPayload {
    let mut v = VineReactionPayload {
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
    harmony_app::vine_signing::sign_reaction(&signer_for(reactor), &mut v);
    v
}

fn reaction_bytes(r: &VineReactionPayload) -> Vec<u8> {
    serde_json::to_vec(r).expect("reaction to_vec")
}

/// Followed-set of named test signers (translated to derived addresses).
fn followed(names: &[&str]) -> HashSet<String> {
    names.iter().map(|s| addr(s)).collect()
}

// ── Category 1: Descriptor publish/receive + follow-set filtering ──

#[test]
fn descriptor_from_followed_creator_lands_in_followed_bucket() {
    let mut cache = VineFeedCache::new();
    let d = make_descriptor(
        "vine-1",
        "alice-addr",
        "Alice",
        "cid-a",
        Some("hi"),
        None,
        100,
    );
    let bytes = descriptor_bytes(&d);

    let outcome = cache.on_descriptor_sample(
        &topic("alice-addr"),
        &bytes,
        &followed(&["alice-addr"]),
        1_000,
    );

    match outcome {
        Some(DescriptorOutcome::Inserted { dto }) => {
            assert_eq!(dto.id, "vine-1");
            assert_eq!(dto.creator_address, addr("alice-addr"));
            assert_eq!(dto.creator_name, "Alice");
            assert_eq!(dto.video_cid, "cid-a");
            assert_eq!(dto.title.as_deref(), Some("hi"));
            assert_eq!(dto.source, VineSource::Followed);
            assert!(!dto.viewed);
        }
        other => panic!("expected Inserted/Followed, got {:?}", other),
    }

    let listed = cache.list_descriptors();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "vine-1");
}

#[test]
fn descriptor_from_unfollowed_creator_lands_in_discover_bucket() {
    let mut cache = VineFeedCache::new();
    let d = make_descriptor(
        "vine-2",
        "stranger-addr",
        "Stranger",
        "cid-b",
        None,
        None,
        200,
    );
    let bytes = descriptor_bytes(&d);

    let outcome = cache.on_descriptor_sample(
        &topic("stranger-addr"),
        &bytes,
        &followed(&["alice-addr"]),
        2_000,
    );

    match outcome {
        Some(DescriptorOutcome::Inserted { dto }) => {
            assert_eq!(dto.source, VineSource::Discover);
        }
        other => panic!("expected Inserted/Discover, got {:?}", other),
    }
}

#[test]
fn re_arrival_of_same_descriptor_is_idempotent() {
    let mut cache = VineFeedCache::new();
    let d = make_descriptor("vine-3", "alice-addr", "Alice", "cid-c", None, None, 300);
    let bytes = descriptor_bytes(&d);

    let first = cache.on_descriptor_sample(
        &topic("alice-addr"),
        &bytes,
        &followed(&["alice-addr"]),
        1_000,
    );
    assert!(matches!(first, Some(DescriptorOutcome::Inserted { .. })));

    // Second arrival of identical bytes. Even if the followed set has
    // since changed (alice no longer followed), source decision must
    // be preserved from the first arrival.
    let second = cache.on_descriptor_sample(&topic("alice-addr"), &bytes, &followed(&[]), 2_000);
    assert_eq!(second, Some(DescriptorOutcome::AlreadyPresent));

    assert_eq!(cache.len_descriptors(), 1);
}

#[test]
fn descriptor_with_malformed_payload_is_rejected() {
    let mut cache = VineFeedCache::new();
    let bad = b"\x00\x01\x02not valid json";

    let outcome =
        cache.on_descriptor_sample(&topic("alice-addr"), bad, &followed(&["alice-addr"]), 1_000);

    match outcome {
        Some(DescriptorOutcome::Rejected(reason)) => {
            assert!(!reason.trim().is_empty(), "unexpected empty reason");
        }
        other => panic!("expected Rejected, got {:?}", other),
    }
    assert_eq!(cache.len_descriptors(), 0);
    assert!(cache.list_descriptors().is_empty());
}

// ── Category 2: Reaction publish/receive + LWW aggregation ─────────

#[test]
fn two_reactors_like_same_vine_count_is_two() {
    let mut cache = VineFeedCache::new();
    let alice = make_reaction("vine-1", "alice-addr", "Alice", true, 100);
    let bob = make_reaction("vine-1", "bob-addr", "Bob", true, 110);

    let r1 = cache.on_reaction_sample(
        &reaction_topic("creator-addr", "vine-1", "alice-addr"),
        &reaction_bytes(&alice),
        0,
    );
    let r2 = cache.on_reaction_sample(
        &reaction_topic("creator-addr", "vine-1", "bob-addr"),
        &reaction_bytes(&bob),
        0,
    );
    assert_eq!(r1, Some(ReactionOutcome::Inserted));
    assert_eq!(r2, Some(ReactionOutcome::Inserted));

    let summary = cache.get_reaction("vine-1", &addr("carol-addr"));
    assert_eq!(summary.count, 2);
    assert!(!summary.liked_by_me);
}

#[test]
fn same_reactor_unlikes_then_likes_lww_wins() {
    let mut cache = VineFeedCache::new();
    let unlike = make_reaction("vine-1", "alice-addr", "Alice", false, 100);
    let like = make_reaction("vine-1", "alice-addr", "Alice", true, 200);

    cache.on_reaction_sample(
        &reaction_topic("creator-addr", "vine-1", "alice-addr"),
        &reaction_bytes(&unlike),
        0,
    );
    let outcome = cache.on_reaction_sample(
        &reaction_topic("creator-addr", "vine-1", "alice-addr"),
        &reaction_bytes(&like),
        0,
    );
    assert_eq!(outcome, Some(ReactionOutcome::UpdatedNewer));

    let summary = cache.get_reaction("vine-1", &addr("alice-addr"));
    assert_eq!(summary.count, 1);
    assert!(summary.liked_by_me);
}

#[test]
fn stale_reaction_does_not_overwrite_newer() {
    let mut cache = VineFeedCache::new();
    let recent_like = make_reaction("vine-1", "alice-addr", "Alice", true, 200);
    cache.on_reaction_sample(
        &reaction_topic("creator-addr", "vine-1", "alice-addr"),
        &reaction_bytes(&recent_like),
        0,
    );

    // Late-arriving unlike (older timestamp) — must NOT overwrite.
    let stale_unlike = make_reaction("vine-1", "alice-addr", "Alice", false, 100);
    let outcome = cache.on_reaction_sample(
        &reaction_topic("creator-addr", "vine-1", "alice-addr"),
        &reaction_bytes(&stale_unlike),
        0,
    );
    assert_eq!(outcome, Some(ReactionOutcome::Stale));

    let summary = cache.get_reaction("vine-1", &addr("alice-addr"));
    assert_eq!(summary.count, 1);
    assert!(summary.liked_by_me);
}

#[test]
fn liked_by_me_reflects_viewer_addr() {
    let mut cache = VineFeedCache::new();
    let alice = make_reaction("vine-1", "alice-addr", "Alice", true, 100);
    let bob = make_reaction("vine-1", "bob-addr", "Bob", true, 110);

    cache.on_reaction_sample(
        &reaction_topic("creator-addr", "vine-1", "alice-addr"),
        &reaction_bytes(&alice),
        0,
    );
    cache.on_reaction_sample(
        &reaction_topic("creator-addr", "vine-1", "bob-addr"),
        &reaction_bytes(&bob),
        0,
    );

    let alice_view = cache.get_reaction("vine-1", &addr("alice-addr"));
    assert_eq!(alice_view.count, 2);
    assert!(alice_view.liked_by_me);

    let carol_view = cache.get_reaction("vine-1", &addr("carol-addr"));
    assert_eq!(carol_view.count, 2);
    assert!(!carol_view.liked_by_me);
}

#[test]
fn same_second_toggle_overwrites_not_dropped() {
    // Regression test for the Qodo finding: publish_vine_reaction uses
    // SystemTime::now().as_secs() (second resolution), so two genuine
    // user toggles within the same second share a timestamp. With the
    // old `<=` check, the second toggle would be dropped. With the
    // new `<` check, it correctly overwrites.
    let mut cache = VineFeedCache::new();
    let like = make_reaction("vine-1", "alice-addr", "Alice", true, 100);
    let unlike = make_reaction("vine-1", "alice-addr", "Alice", false, 100); // SAME timestamp

    let r1 = cache.on_reaction_sample(
        &reaction_topic("creator-addr", "vine-1", "alice-addr"),
        &reaction_bytes(&like),
        0,
    );
    let r2 = cache.on_reaction_sample(
        &reaction_topic("creator-addr", "vine-1", "alice-addr"),
        &reaction_bytes(&unlike),
        0,
    );

    assert_eq!(r1, Some(ReactionOutcome::Inserted));
    // Same-timestamp now classifies as UpdatedNewer (was Stale, which
    // dropped the second toggle silently)
    assert_eq!(r2, Some(ReactionOutcome::UpdatedNewer));

    let summary = cache.get_reaction("vine-1", &addr("alice-addr"));
    assert_eq!(summary.count, 0); // unlike won, so no liked count
    assert!(!summary.liked_by_me);
}

#[test]
fn same_timestamp_same_liked_is_stale_no_reemit() {
    // Regression test for CodeRabbit Major on commit d6c7792.
    // Exact-duplicate Zenoh redelivery (same timestamp AND same liked
    // value) must be classified as Stale, NOT UpdatedNewer — otherwise
    // every duplicate sample re-emits to the frontend.
    let mut cache = VineFeedCache::new();
    let like1 = make_reaction("vine-1", "alice-addr", "Alice", true, 100);
    // Identical second sample (Zenoh redelivery)
    let like2 = make_reaction("vine-1", "alice-addr", "Alice", true, 100);

    let r1 = cache.on_reaction_sample(
        &reaction_topic("creator-addr", "vine-1", "alice-addr"),
        &reaction_bytes(&like1),
        0,
    );
    let r2 = cache.on_reaction_sample(
        &reaction_topic("creator-addr", "vine-1", "alice-addr"),
        &reaction_bytes(&like2),
        0,
    );

    assert_eq!(r1, Some(ReactionOutcome::Inserted));
    // Exact-duplicate redelivery: Stale (no overwrite, no re-emit)
    assert_eq!(r2, Some(ReactionOutcome::Stale));

    let summary = cache.get_reaction("vine-1", &addr("alice-addr"));
    assert_eq!(summary.count, 1);
    assert!(summary.liked_by_me);
}

// ── Category 3: Reshare wire path ───────────────────────────────────

#[test]
fn reshare_descriptor_carries_reshare_of_link() {
    let mut cache = VineFeedCache::new();
    let original = make_descriptor(
        "vine-original",
        "alice-addr",
        "Alice",
        "cid-orig",
        Some("Alice's original"),
        None,
        100,
    );
    let reshare = make_descriptor(
        "vine-reshare",
        "bob-addr",
        "Bob",
        "cid-orig", // reshares point to the same video_cid as the original
        None,
        Some("vine-original"),
        200,
    );

    // C (viewer) follows both Alice and Bob.
    let f = followed(&["alice-addr", "bob-addr"]);
    cache.on_descriptor_sample(&topic("alice-addr"), &descriptor_bytes(&original), &f, 0);
    cache.on_descriptor_sample(&topic("bob-addr"), &descriptor_bytes(&reshare), &f, 0);

    let dtos = cache.list_descriptors();
    assert_eq!(dtos.len(), 2);

    // Sorted by created_at DESC: vine-reshare (200) first
    assert_eq!(dtos[0].id, "vine-reshare");
    assert_eq!(dtos[0].reshare_of.as_deref(), Some("vine-original"));
    assert_eq!(dtos[0].creator_address, addr("bob-addr"));

    assert_eq!(dtos[1].id, "vine-original");
    assert_eq!(dtos[1].reshare_of, None);
}

#[test]
fn reshare_of_unknown_vine_id_still_accepted() {
    // Recipient never saw the original; the reshare descriptor still
    // lands with a dangling reshare_of pointer (no FK constraint in
    // this CRDT-style append-only cache).
    let mut cache = VineFeedCache::new();
    let dangling = make_descriptor(
        "vine-reshare",
        "bob-addr",
        "Bob",
        "cid-orig",
        None,
        Some("vine-never-seen"),
        300,
    );

    let outcome = cache.on_descriptor_sample(
        &topic("bob-addr"),
        &descriptor_bytes(&dangling),
        &followed(&["bob-addr"]),
        1_000,
    );
    assert!(matches!(outcome, Some(DescriptorOutcome::Inserted { .. })));

    let dtos = cache.list_descriptors();
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, "vine-reshare");
    assert_eq!(dtos[0].reshare_of.as_deref(), Some("vine-never-seen"));
}

// ── Category 4: Viewed-state ────────────────────────────────────────

#[test]
fn mark_viewed_idempotent_and_local_only() {
    let mut cache = VineFeedCache::new();
    let d = make_descriptor("v-1", "alice-addr", "Alice", "cid", None, None, 100);
    cache.on_descriptor_sample(
        &topic("alice-addr"),
        &descriptor_bytes(&d),
        &followed(&[]),
        0,
    );

    let first = cache.mark_viewed("v-1".to_string());
    assert!(first);
    let second = cache.mark_viewed("v-1".to_string());
    assert!(!second);

    let dtos = cache.list_descriptors();
    assert_eq!(dtos.len(), 1);
    assert!(dtos[0].viewed);
    // Descriptor count is unchanged — mark_viewed never inserts a descriptor.
    assert_eq!(cache.len_descriptors(), 1);
}

#[test]
fn viewed_state_survives_descriptor_insertion_order() {
    let mut cache = VineFeedCache::new();

    // mark_viewed BEFORE the descriptor exists
    cache.mark_viewed("v-future".to_string());
    assert!(cache.is_viewed("v-future"));

    let d = make_descriptor("v-future", "alice-addr", "Alice", "cid", None, None, 100);
    cache.on_descriptor_sample(
        &topic("alice-addr"),
        &descriptor_bytes(&d),
        &followed(&[]),
        0,
    );

    let dtos = cache.list_descriptors();
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, "v-future");
    assert!(dtos[0].viewed);
}

// ── Category 5: Wire format pinning (drift detector) ────────────────

/// CHANGE-DETECTION TEST: if this fails, treat the diff as a wire
/// protocol break — every existing peer expects this byte sequence
/// on `harmony/vines/{addr}` topics. Cross-version compatibility
/// requires the legacy fields stay present + same names + same order.
///
/// Mirrors `wire_format_profile_broadcast_fixtures::profile_broadcast_canonical_cbor_pinned`
/// in spirit; the Vine wire is JSON not CBOR, so this is a byte-level
/// JSON pin instead of a CBOR-prefix pin.
#[test]
fn descriptor_canonical_json_pinned() {
    let d = make_descriptor(
        "vine-fixture-1",
        "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a",
        "Fixture Alice",
        "cid-1234abcd",
        Some("hello fixture"),
        Some("vine-original-7"),
        1_700_000_000,
    );
    let actual = String::from_utf8(serde_json::to_vec(&d).unwrap()).unwrap();

    // The struct's field order is: id, creator_address, creator_name,
    // created_at, video_cid, title, reshare_of, identity_pub, sig
    // (ZEB-673). camelCase rename. The address/pub/sig VALUES are
    // derived from the per-process signer identity, so the pin embeds
    // them dynamically — key order and naming stay fully pinned.
    let expected = format!(
        concat!(
            "{{",
            r#""id":"vine-fixture-1","#,
            r#""creatorAddress":"{}","#,
            r#""creatorName":"Fixture Alice","#,
            r#""createdAt":1700000000,"#,
            r#""videoCid":"cid-1234abcd","#,
            r#""title":"hello fixture","#,
            r#""reshareOf":"vine-original-7","#,
            r#""identityPub":"{}","#,
            r#""sig":"{}""#,
            "}}",
        ),
        d.creator_address,
        d.identity_pub.as_deref().unwrap(),
        d.sig.as_deref().unwrap(),
    );
    assert_eq!(actual, expected, "descriptor wire format drifted");

    // ── ALSO pin the None-case wire format ────────────────────────
    // The struct uses #[serde(skip_serializing_if = "Option::is_none")]
    // on `title` and `reshare_of`, so a descriptor with both as None
    // serializes WITHOUT those fields (not as `"title":null`). If a
    // future change drops the attribute, the JSON shape changes silently
    // and peer interop breaks.
    let d_no_opts = make_descriptor(
        "vine-fixture-2",
        "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a",
        "Fixture Alice",
        "cid-1234abcd",
        None,
        None,
        1_700_000_000,
    );
    let actual_no_opts = String::from_utf8(serde_json::to_vec(&d_no_opts).unwrap()).unwrap();
    let expected_no_opts = format!(
        concat!(
            "{{",
            r#""id":"vine-fixture-2","#,
            r#""creatorAddress":"{}","#,
            r#""creatorName":"Fixture Alice","#,
            r#""createdAt":1700000000,"#,
            r#""videoCid":"cid-1234abcd","#,
            r#""identityPub":"{}","#,
            r#""sig":"{}""#,
            "}}",
        ),
        d_no_opts.creator_address,
        d_no_opts.identity_pub.as_deref().unwrap(),
        d_no_opts.sig.as_deref().unwrap(),
    );
    assert_eq!(
        actual_no_opts, expected_no_opts,
        "descriptor wire format (None fields) drifted — Option::is_none skip invariant broken"
    );
}

/// CHANGE-DETECTION TEST: see `descriptor_canonical_json_pinned`.
#[test]
fn reaction_canonical_json_pinned() {
    let r = make_reaction(
        "vine-fixture-1",
        "1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b",
        "Fixture Bob",
        true,
        1_700_000_500,
    );
    let actual = String::from_utf8(serde_json::to_vec(&r).unwrap()).unwrap();

    // Field order: vine_id, reactor_address, reactor_name, liked,
    // timestamp, identity_pub, sig (ZEB-673). Dynamic values embedded —
    // see descriptor_canonical_json_pinned.
    let expected = format!(
        concat!(
            "{{",
            r#""vineId":"vine-fixture-1","#,
            r#""reactorAddress":"{}","#,
            r#""reactorName":"Fixture Bob","#,
            r#""liked":true,"#,
            r#""timestamp":1700000500,"#,
            r#""identityPub":"{}","#,
            r#""sig":"{}""#,
            "}}",
        ),
        r.reactor_address,
        r.identity_pub.as_deref().unwrap(),
        r.sig.as_deref().unwrap(),
    );
    assert_eq!(actual, expected, "reaction wire format drifted");
}
