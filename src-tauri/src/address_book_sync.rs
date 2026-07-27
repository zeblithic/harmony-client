//! ZEB-815 Task 3: sealed wire codec for the community address book.
//!
//! One codec serves both a live single-row publish and a full snapshot: a
//! live record publish is [`seal_records`] with a 1-element slice; a snapshot
//! is the same codec over the full row set. Mirrors the presence beacon seal
//! (`community_presence.rs`'s `seal_presence_beacon`/`open_presence_beacon`)
//! over `encrypt_voice_packet`/`decrypt_voice_packet`, with its own HKDF
//! `info` label, sentinel channel, and AAD domain so an address-book packet
//! can never be confused with (or opened as) a presence beacon.

use crate::community_address_book::{
    save_addrbook, AddressBookEntry, AddressBookRow, CommunityAddressBook, UpsertOutcome,
};
use crate::community_channel_log::ChannelKey;
use crate::community_membership::ChannelId;
use crate::community_relay_resolver::CommunityRelayResolver;
use crate::community_state_sync::CommunitySyncRegistry;
use crate::iroh_friend_acceptor::wall_now_ms;
use crate::owner_state_types::{EpochKey, SpaceId};
use crate::reachability_resolver::ReachabilityResolver;
use crate::voice_crypto::{decrypt_voice_packet, encrypt_voice_packet};
use hkdf::Hkdf;
use sha2::Sha256;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Domain separator for sealed address-book packets (records + snapshots
/// alike — one codec, no second format).
pub const ADDRBOOK_AAD: &[u8] = b"harmony-addrbook-v1";

/// The address book has no channel, so the AEAD seam (which is
/// `(community, channel)` scoped) is bound with this sentinel. Distinct from
/// `community_presence.rs`'s `PRESENCE_SENTINEL_CHANNEL` ([0u8; 16]) so the
/// two domains never collide even before `ADDRBOOK_AAD` is considered.
pub const ADDRBOOK_SENTINEL_CHANNEL: ChannelId = ChannelId([1u8; 16]);

/// Minimum interval between full-snapshot publishes for a given community.
pub const ADDRBOOK_SNAPSHOT_COOLDOWN_MS: u64 = 60_000;

/// Upper bound on a sealed address-book packet (record or snapshot) accepted
/// for decryption. Enforced before any AEAD open to bound allocation from a
/// peer flooding the topic.
pub const ADDRBOOK_SNAPSHOT_MAX_BYTES: usize = 1_048_576;

/// Idle backstop between snapshot queries when nothing wakes the requester.
/// The event-driven fires (spawn, presence roster change) are the real
/// catch-up path; this only bounds how long a node that missed every edge
/// stays behind.
pub const ADDRBOOK_SNAPSHOT_IDLE_REQUERY_MS: u64 = 30 * 60_000;

/// Budget for draining one snapshot query's replies. Generous relative to a
/// KB-scale reply because a cross-WAN responder may be several hops out; the
/// requester is off the critical path (live records keep filling the book).
pub const ADDRBOOK_SNAPSHOT_GET_TIMEOUT_MS: u64 = 10_000;

/// Debounce between an ingest marking the book dirty and the sidecar write.
/// Coalesces a snapshot's row burst into one file write.
pub const ADDRBOOK_PERSIST_DEBOUNCE_MS: u64 = 2_000;

/// Width of the random delay applied before a WOKEN snapshot query.
///
/// A roster change is a shared edge: every member's presence subscriber sees
/// it at about the same instant and wakes its requester, so without a spread
/// one edge produces N simultaneous queries, each answered by every other node
/// sealing its ENTIRE book — up to N×(N−1) sealed replies landing at once.
/// Same thundering-herd reasoning (and same fix) as
/// `channel_backfill::PERIODIC_RESYNC_JITTER_MS`.
pub const ADDRBOOK_SNAPSHOT_JITTER_MS: u64 = 5_000;

/// Map a raw random value onto `[0, ADDRBOOK_SNAPSHOT_JITTER_MS)`. Pure, so
/// the bound is unit-testable without a clock or an RNG; the caller draws
/// `rng_val` from entropy per fire (never a fixed seed — a seeded draw would
/// leave every node in a freshly-rebuilt fleet correlated again).
pub fn jitter_ms(rng_val: u64) -> u64 {
    rng_val % ADDRBOOK_SNAPSHOT_JITTER_MS
}

/// Live per-record topic for a community's address book.
pub fn addrbook_records_topic(community: &SpaceId) -> String {
    format!("harmony/addrbook/{}/records", hex::encode(community.0))
}

/// Full-snapshot queryable topic for a community's address book.
pub fn addrbook_snapshot_topic(community: &SpaceId) -> String {
    format!("harmony/addrbook/{}/snapshot", hex::encode(community.0))
}

/// Has the snapshot cooldown window closed since `last_fire_ms`?
///
/// Factored out of [`spawn_addrbook_snapshot_requester`] so the rate limiter
/// is unit-testable without a zenoh session. `saturating_sub` matters in both
/// directions: `last_fire_ms == 0` (never fired) always elapses, and a
/// backwards clock step reads as "not elapsed" rather than underflowing into
/// a permanently-open gate.
pub fn cooldown_elapsed(last_fire_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(last_fire_ms) >= ADDRBOOK_SNAPSHOT_COOLDOWN_MS
}

/// HKDF-SHA256 derivation of the per-community address-book key from the
/// community epoch (membership) key. Mirrors `derive_presence_key` — same
/// salt (`community_id`), distinct `info` label — so the address-book key is
/// independent of the presence key (and every channel key) for the same
/// `(mk, community_id)`.
pub fn derive_addrbook_key(mk: &EpochKey, community_id: &SpaceId) -> ChannelKey {
    let salt = community_id.0;
    let info = b"addrbook:";
    let mut out = zeroize::Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(&salt), mk.as_bytes())
        .expand(info, out.as_mut())
        .expect("32 <= 8160");
    ChannelKey::from_bytes(*out)
}

/// Seal `rows` (a single record or a full snapshot — same codec either way)
/// under the per-community address-book key.
pub fn seal_records(
    key: &ChannelKey,
    community: &SpaceId,
    rows: &[AddressBookRow],
) -> Result<Vec<u8>, String> {
    let mut plain = Vec::new();
    ciborium::into_writer(&rows, &mut plain).map_err(|e| format!("addrbook encode: {e}"))?;
    encrypt_voice_packet(
        key,
        community,
        &ADDRBOOK_SENTINEL_CHANNEL,
        ADDRBOOK_AAD,
        &plain,
    )
    .map_err(|e| format!("addrbook seal: {e}"))
}

/// Open + decode a sealed address-book packet. Returns `None` on any failure
/// (wrong key, wrong scope, tamper, or oversize) — callers drop silently.
pub fn open_records(
    key: &ChannelKey,
    community: &SpaceId,
    packet: &[u8],
) -> Option<Vec<AddressBookRow>> {
    if packet.len() > ADDRBOOK_SNAPSHOT_MAX_BYTES {
        return None;
    }
    let plain = decrypt_voice_packet(
        key,
        community,
        &ADDRBOOK_SENTINEL_CHANNEL,
        ADDRBOOK_AAD,
        packet,
    )
    .ok()?;
    ciborium::from_reader(plain.as_slice()).ok()
}

/// Result of ingesting one address-book row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestOutcome {
    Applied(UpsertOutcome),
    BadSignature,
    NotMember,
    Malformed,
}

/// Pure ingest core for one already-unsealed, already-membership-checked row:
/// verify the entry's inner signature against `row.device` (the device's
/// Ed25519 verifying-key bytes), then upsert into `book` and — on a
/// materialized change (`Inserted`/`Replaced`) — fan out to the matching
/// resolver, mirroring the ReachabilityAnnounce/CommunityRelayAnnounce delta
/// arms this replaces (`lib.rs`).
///
/// Membership is NOT checked here — the caller ([`ingest_sealed_packet`])
/// gates on it per-row before calling in.
pub fn ingest_verified_row(
    book: &CommunityAddressBook,
    reachability_resolver: &ReachabilityResolver,
    community_relay_resolver: &CommunityRelayResolver,
    community: SpaceId,
    row: AddressBookRow,
    now_ms: u64,
) -> IngestOutcome {
    let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&row.device) else {
        return IngestOutcome::BadSignature;
    };
    let verified = match &row.entry {
        AddressBookEntry::Reachability(p) => {
            crate::reachability_record::verify_inner_signature(p, &row.actor, &row.at, &vk).is_ok()
        }
        AddressBookEntry::Relay(p) => {
            crate::community_relay_announce::verify_inner_signature(p, &row.actor, &row.at, &vk)
                .is_ok()
        }
    };
    if !verified {
        return IngestOutcome::BadSignature;
    }

    let outcome = book.upsert(community, row.clone(), now_ms);
    if matches!(outcome, UpsertOutcome::Inserted | UpsertOutcome::Replaced) {
        match row.entry {
            AddressBookEntry::Reachability(p) => {
                reachability_resolver.update(row.actor, p, row.at);
            }
            AddressBookEntry::Relay(p) => {
                community_relay_resolver.update(community, row.actor, p, row.at);
            }
        }
    }
    IngestOutcome::Applied(outcome)
}

/// Async wrapper: unseal `packet` under the community's current membership
/// key, then per-row gate on live membership before dispatching to
/// [`ingest_verified_row`]. Used by the event-loop wiring (Tasks 5/6).
///
/// A missing engine or an unseal/decode failure yields one `Malformed`
/// outcome for the whole packet (nothing to iterate); a row whose signer
/// is not a currently-enrolled, Joined member yields `NotMember` for that
/// row and is skipped (never reaches signature verification or the store).
pub async fn ingest_sealed_packet(
    registry: &Arc<CommunitySyncRegistry>,
    book: &CommunityAddressBook,
    reachability_resolver: &ReachabilityResolver,
    community_relay_resolver: &CommunityRelayResolver,
    community: SpaceId,
    packet: &[u8],
    now_ms: u64,
) -> Vec<IngestOutcome> {
    let Some(engine) = registry.engine_arc(&community).await else {
        return vec![IngestOutcome::Malformed];
    };
    let mk = engine.membership_key();
    let key = derive_addrbook_key(&mk, &community);
    let Some(rows) = open_records(&key, &community, packet) else {
        return vec![IngestOutcome::Malformed];
    };

    let mut outcomes = Vec::with_capacity(rows.len());
    for row in rows {
        let is_member = crate::voice_presence::beacon_signer_is_member(
            registry,
            &community,
            &row.actor,
            &row.device,
        )
        .await;
        if !is_member {
            outcomes.push(IngestOutcome::NotMember);
            continue;
        }
        outcomes.push(ingest_verified_row(
            book,
            reachability_resolver,
            community_relay_resolver,
            community,
            row,
            now_ms,
        ));
    }
    outcomes
}

/// Did this batch of outcomes materially change the book? Only `Inserted` /
/// `Replaced` did — an `IgnoredOlder`/`IgnoredCapped`/rejected row leaves the
/// store byte-identical, so neither the sidecar nor a re-persist is warranted.
fn batch_changed_book(outcomes: &[IngestOutcome]) -> bool {
    outcomes.iter().any(|o| {
        matches!(
            o,
            IngestOutcome::Applied(UpsertOutcome::Inserted | UpsertOutcome::Replaced)
        )
    })
}

// ── ZEB-815 Task 5: per-community sync tasks ────────────────────────
//
// Four tasks per subscribed community, spawned + owned by the event loop's
// address-book pool (`event_loop.rs`, mirroring the ZEB-537 presence pool):
// live subscriber, snapshot queryable (serve), snapshot requester (catch-up),
// and the sidecar persist task. They coordinate through two `Notify`s:
// `dirty` (ingest → persist) and `resync` (presence roster change → requester).
//
// None of the four takes a `closing` flag: shutdown is abort-driven. The pool
// owns all four `JoinHandle`s and aborts them on `Unsubscribe` and when its
// request channel closes (stop_node drops the sender), which is the same
// terminal path the presence pool's `handles.drain()` uses.

/// Per-community `Notify` registry shared by two sides that can reach a given
/// community in EITHER order — the shared mechanics behind
/// [`AddrbookResyncHub`] and [`AddrbookDirtyHub`].
///
/// A hub rather than a plain `Arc<Notify>` per side because producer and
/// consumer are independent tasks draining independent channels: either can
/// process its `Subscribe` for a community first. `handle()` creates the
/// `Notify` on first call and returns the same one to the second caller, so
/// the wiring is order-independent. A fire with no listener yet (or ever — the
/// pool is unwired in test callers that bypass `start_node`) is a no-op beyond
/// leaving one permit, exactly like `send_modify` on a receiver-less watch.
///
/// Entries are deliberately never removed. Dropping one on unsubscribe would
/// reintroduce exactly the ordering bug the hub exists to prevent: across an
/// unsubscribe→resubscribe the two sides can interleave such that the producer
/// re-takes the OLD handle before the pool clears it, and the new consumer
/// then waits on a `Notify` nobody fires. The map is bounded by the number of
/// communities the node has ever subscribed to, and a stale permit costs at
/// most one extra round of whatever the handle drives.
#[derive(Clone, Default)]
struct NotifyHub {
    inner: Arc<std::sync::Mutex<HashMap<[u8; 16], Arc<Notify>>>>,
}

impl NotifyHub {
    fn handle(&self, community_id: [u8; 16]) -> Arc<Notify> {
        Arc::clone(
            self.inner
                .lock()
                .expect("addrbook notify hub poisoned")
                .entry(community_id)
                .or_insert_with(|| Arc::new(Notify::new())),
        )
    }
}

/// Per-community resync handles, shared between the producer (the community
/// presence subscriber, which fires one on a roster device-set change) and the
/// consumer (that community's snapshot requester).
#[derive(Clone, Default)]
pub struct AddrbookResyncHub(NotifyHub);

impl AddrbookResyncHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// The resync handle for `community_id`, created on first call and stable
    /// for the process lifetime thereafter.
    pub fn handle(&self, community_id: [u8; 16]) -> Arc<Notify> {
        self.0.handle(community_id)
    }
}

/// Per-community dirty handles, shared between the producers (every path that
/// materially changes the book — the live subscriber, the snapshot requester,
/// and [`publish_own_rows`] for our OWN rows) and the consumer (that
/// community's sidecar persist task).
///
/// The pool's per-community `dirty` was originally minted locally inside the
/// `Subscribe` arm, which made it unreachable from anywhere outside the pool:
/// a LOCAL publish would upsert into the book and never wake the persist task,
/// so our own reachability/relay rows would survive a restart only if a peer
/// happened to echo them back. Routing `dirty` through a hub — created beside
/// the resync hub and threaded to the pool AND the publisher closures — closes
/// that seam with the same order-independence the resync hub already has.
#[derive(Clone, Default)]
pub struct AddrbookDirtyHub(NotifyHub);

impl AddrbookDirtyHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// The dirty handle for `community_id`, created on first call and stable
    /// for the process lifetime thereafter.
    pub fn handle(&self, community_id: [u8; 16]) -> Arc<Notify> {
        self.0.handle(community_id)
    }
}

/// Publish this node's OWN freshly-minted address-book rows.
///
/// Each row takes the SAME gate a peer's row takes ([`ingest_verified_row`]),
/// so `self` lands in our own book and resolvers exactly where the CRDT
/// membership-delta hook used to put it — one trust path, no self-shaped
/// bypass — and is then sealed onto the community's live record topic for
/// every other member.
///
/// `key_fn` resolves the per-community seal key synchronously: the callers
/// already hold each community's engine inside their own publish loop and
/// pre-collect `(community, key)` there, so an async lookup here would only
/// re-take the registry lock mid-publish. `None` means we hold no key for that
/// community — the row stays in our local book, but nothing goes on the wire
/// (a member we cannot seal for could not read it anyway).
///
/// Every failure is per-row: one community's missing key, failed seal, or
/// closed publish channel must not stop the other communities' rows — the same
/// per-community `continue` discipline as the publish loops this replaces.
pub async fn publish_own_rows(
    key_fn: &(dyn Fn(&SpaceId) -> Option<ChannelKey> + Send + Sync),
    book: &CommunityAddressBook,
    rr: &ReachabilityResolver,
    crr: &CommunityRelayResolver,
    publish_tx: &tokio::sync::mpsc::Sender<crate::event_loop::PublishRequest>,
    rows: Vec<(SpaceId, AddressBookRow)>,
    dirty: &AddrbookDirtyHub,
) {
    for (community, row) in rows {
        let community_hex = hex::encode(&community.0[..4]);
        match ingest_verified_row(book, rr, crr, community, row.clone(), wall_now_ms()) {
            IngestOutcome::Applied(UpsertOutcome::Inserted | UpsertOutcome::Replaced) => {
                dirty.handle(community.0).notify_one();
            }
            // Nothing materially changed (a same-millisecond republish, or the
            // community's row cap already full) — no sidecar rewrite is owed,
            // but the row still goes out: peers keep their own books.
            IngestOutcome::Applied(_) => {}
            // Our own row failing the gate we hold every peer to is a bug in
            // the minting path, and a row WE reject is one every member would
            // reject too — so it is never put on the wire.
            bad => {
                tracing::warn!(
                    community = %community_hex,
                    outcome = ?bad,
                    "own addrbook row failed the local ingest gate; not published"
                );
                continue;
            }
        }

        let Some(key) = key_fn(&community) else {
            tracing::warn!(
                community = %community_hex,
                "no addrbook key for community; own row stays local"
            );
            continue;
        };
        let payload = match seal_records(&key, &community, std::slice::from_ref(&row)) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    community = %community_hex,
                    err = %e,
                    "addrbook record seal failed; not published"
                );
                continue;
            }
        };

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if publish_tx
            .send(crate::event_loop::PublishRequest {
                key_expr: addrbook_records_topic(&community),
                payload,
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            tracing::warn!(
                community = %community_hex,
                "event loop gone; addrbook record not published"
            );
            continue;
        }
        match reply_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(
                community = %community_hex,
                err = %e,
                "addrbook record publish failed"
            ),
            Err(_) => tracing::warn!(
                community = %community_hex,
                "event loop dropped the addrbook publish request"
            ),
        }
    }
}

/// Live-record subscriber: every sealed packet on
/// `harmony/addrbook/{hex}/records` goes through [`ingest_sealed_packet`]
/// (unseal under the community's CURRENT membership key → per-row membership
/// gate → signature → LWW upsert → resolver fan-out). A batch that materially
/// changed the book marks it dirty so the sidecar is rewritten.
///
/// `session` is an owned, cheaply-cloned `zenoh::Session` (call sites pass
/// `session.clone()`), matching
/// [`crate::community_presence::spawn_community_presence_subscriber`], whose
/// declare-inside-the-task shape this mirrors.
pub fn spawn_addrbook_subscriber(
    session: zenoh::Session,
    registry: Arc<CommunitySyncRegistry>,
    book: Arc<CommunityAddressBook>,
    rr: Arc<ReachabilityResolver>,
    crr: Arc<CommunityRelayResolver>,
    community: SpaceId,
    dirty: Arc<Notify>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let topic = addrbook_records_topic(&community);
        let sub = match session.declare_subscriber(&topic).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(%topic, err = %e, "addrbook record subscribe failed");
                return;
            }
        };
        while let Ok(sample) = sub.recv_async().await {
            // Bound the copy BEFORE materializing the payload — `open_records`
            // re-checks, but only after `to_vec` has already allocated.
            if sample.payload().len() > ADDRBOOK_SNAPSHOT_MAX_BYTES {
                tracing::warn!(
                    %topic,
                    len = sample.payload().len(),
                    max = ADDRBOOK_SNAPSHOT_MAX_BYTES,
                    "oversized addrbook record dropped"
                );
                continue;
            }
            let bytes = sample.payload().to_bytes().to_vec();
            let outcomes = ingest_sealed_packet(
                &registry,
                &book,
                &rr,
                &crr,
                community,
                &bytes,
                wall_now_ms(),
            )
            .await;
            if batch_changed_book(&outcomes) {
                dirty.notify_one();
            }
        }
    })
}

/// Seal `rows` for a snapshot reply, dropping the oldest half until the sealed
/// packet fits `max_bytes`. A reply over the receiver's cap is rejected before
/// decryption (see [`open_records`]), so serving one is pure waste — a partial
/// newest-first snapshot is strictly better, and the omitted rows still arrive
/// via live records or another responder. Terminates: each iteration halves
/// the row count, and an empty row set always seals small.
fn seal_snapshot_bounded(
    key: &ChannelKey,
    community: &SpaceId,
    mut rows: Vec<AddressBookRow>,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    rows.sort_by(|a, b| b.stamped_at_ms.cmp(&a.stamped_at_ms));
    loop {
        let packet = seal_records(key, community, &rows)?;
        if packet.len() <= max_bytes || rows.is_empty() {
            return Ok(packet);
        }
        let keep = rows.len() / 2;
        tracing::warn!(
            community = %hex::encode(&community.0[..4]),
            len = packet.len(),
            max = max_bytes,
            rows = rows.len(),
            keep,
            "addrbook snapshot over cap; serving the newest rows only"
        );
        rows.truncate(keep);
    }
}

/// Snapshot queryable (the serve side of catch-up): reply to a query on
/// `harmony/addrbook/{hex}/snapshot` with this node's full TTL-filtered book
/// for the community, sealed under the community's current address-book key.
///
/// `async` so the declare is AWAITED before the serve loop is spawned — a
/// caller that sequences a subscribe against readiness must not race the
/// declare (`event_loop.rs`'s `spawn_content_serve_queryable`). A declare
/// failure returns an already-finished handle, which the pool's self-healing
/// `retain` treats as a dead entry and restarts on the next `Subscribe`.
pub async fn spawn_addrbook_snapshot_queryable(
    session: zenoh::Session,
    registry: Arc<CommunitySyncRegistry>,
    book: Arc<CommunityAddressBook>,
    community: SpaceId,
) -> JoinHandle<()> {
    let topic = addrbook_snapshot_topic(&community);
    let qbl = match session.declare_queryable(&topic).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(%topic, err = %e, "addrbook snapshot queryable declare failed");
            return tokio::spawn(async {});
        }
    };
    tokio::spawn(async move {
        while let Ok(query) = qbl.recv_async().await {
            // Re-derive per query so the reply follows epoch rotation; a
            // missing engine means we hold no key for this community and
            // simply do not answer (the querier logs a no-responder INFO).
            let Some(engine) = registry.engine_arc(&community).await else {
                continue;
            };
            let key = derive_addrbook_key(&engine.membership_key(), &community);
            // An EMPTY book still replies: "I am here and hold nothing" is a
            // real answer, and suppressing it would make an all-empty
            // community look like a no-responder failure forever.
            let rows = book.rows_for_community(&community, wall_now_ms());
            let packet =
                match seal_snapshot_bounded(&key, &community, rows, ADDRBOOK_SNAPSHOT_MAX_BYTES) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(%topic, err = %e, "addrbook snapshot seal failed");
                        continue;
                    }
                };
            if let Err(e) = query.reply(query.key_expr(), packet).await {
                tracing::warn!(%topic, err = %e, "addrbook snapshot reply failed");
            }
        }
    })
}

/// One snapshot query round: GET the community's snapshot topic, then run
/// EVERY responder's reply through the same per-row ingest gate live records
/// use — a hostile or stale responder can therefore only contribute rows that
/// verify individually (spec §6).
#[allow(clippy::too_many_arguments)]
async fn request_snapshot_once(
    session: &zenoh::Session,
    topic: &str,
    registry: &Arc<CommunitySyncRegistry>,
    book: &CommunityAddressBook,
    rr: &ReachabilityResolver,
    crr: &CommunityRelayResolver,
    community: SpaceId,
    dirty: &Notify,
) {
    let community_hex = hex::encode(community.0);
    // Mirrors the channel-log/voting backfill requester's get shape:
    // - ConsolidationMode::None — every member's book is a legitimate partial
    //   view, so we want ALL replies, not zenoh's newest-per-key pick.
    // - Locality::Remote — exclude our OWN snapshot queryable. Ingesting our
    //   own book back is pure work, and (worse) a self-reply would make the
    //   `responders == 0` branch below unreachable, silently retiring the
    //   spec §6 no-responder signal on exactly the solo node it is for.
    // - `.timeout` — bound the query zenoh-side too, so the stream closes on
    //   its own rather than relying only on our drain budget.
    let replies = match session
        .get(topic)
        .consolidation(zenoh::query::ConsolidationMode::None)
        .allowed_destination(zenoh::sample::Locality::Remote)
        .timeout(Duration::from_millis(ADDRBOOK_SNAPSHOT_GET_TIMEOUT_MS))
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(%topic, err = %e, "addrbook snapshot get failed");
            return;
        }
    };

    let mut responders = 0usize;
    let mut changed = false;
    let drain = async {
        while let Ok(reply) = replies.recv_async().await {
            match reply.result() {
                Ok(sample) => {
                    responders += 1;
                    if sample.payload().len() > ADDRBOOK_SNAPSHOT_MAX_BYTES {
                        tracing::warn!(
                            %topic,
                            len = sample.payload().len(),
                            max = ADDRBOOK_SNAPSHOT_MAX_BYTES,
                            "oversized addrbook snapshot reply dropped"
                        );
                        continue;
                    }
                    let bytes = sample.payload().to_bytes().to_vec();
                    let outcomes = ingest_sealed_packet(
                        registry,
                        book,
                        rr,
                        crr,
                        community,
                        &bytes,
                        wall_now_ms(),
                    )
                    .await;
                    changed |= batch_changed_book(&outcomes);
                }
                Err(err) => {
                    let msg = String::from_utf8_lossy(&err.payload().to_bytes()).into_owned();
                    tracing::info!(%topic, err = %msg, "addrbook snapshot reply error");
                }
            }
        }
    };
    if tokio::time::timeout(
        Duration::from_millis(ADDRBOOK_SNAPSHOT_GET_TIMEOUT_MS),
        drain,
    )
    .await
    .is_err()
    {
        tracing::info!(
            community = %community_hex,
            %topic,
            responders,
            "addrbook snapshot query budget elapsed; keeping what arrived"
        );
    }

    if responders == 0 {
        // Spec §6: INFO with community context, NOT a warn. A solo node — or
        // the first of a fleet to rebuild — legitimately has nobody to ask;
        // live records still fill the book. ZEB-813's lesson was that a WARN
        // here trains operators to ignore the line that matters.
        tracing::info!(
            community = %community_hex,
            %topic,
            "addrbook snapshot: no responder (not an error — live records still fill the book)"
        );
    }
    if changed {
        dirty.notify_one();
    }
}

/// Snapshot requester (the catch-up side): fire one query immediately on
/// spawn (join / resubscribe / reconnect), then on every `resync` fire
/// (presence roster change) and on a `ADDRBOOK_SNAPSHOT_IDLE_REQUERY_MS` idle
/// backstop, rate-limited to one query per `ADDRBOOK_SNAPSHOT_COOLDOWN_MS`.
///
/// A wake inside the cooldown is DEFERRED, never dropped: the first roster
/// change after a join typically lands seconds after the spawn-time query
/// (which found nobody connected yet), and dropping it would leave the node
/// waiting out the idle backstop with an empty book.
///
/// Every WOKEN fire is additionally jittered by `[0,
/// ADDRBOOK_SNAPSHOT_JITTER_MS)` (after any cooldown deferral, so the two
/// compose rather than cancel). The spawn-time fire is deliberately NOT
/// jittered: it is triggered locally by this node's own subscribe, not by an
/// edge every member observes at once, and it is the bootstrap-latency path a
/// joiner's first dial waits on.
#[allow(clippy::too_many_arguments)]
pub fn spawn_addrbook_snapshot_requester(
    session: zenoh::Session,
    registry: Arc<CommunitySyncRegistry>,
    book: Arc<CommunityAddressBook>,
    rr: Arc<ReachabilityResolver>,
    crr: Arc<CommunityRelayResolver>,
    community: SpaceId,
    resync: Arc<Notify>,
    dirty: Arc<Notify>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let topic = addrbook_snapshot_topic(&community);
        let mut idle =
            tokio::time::interval(Duration::from_millis(ADDRBOOK_SNAPSHOT_IDLE_REQUERY_MS));
        // `interval` fires its first tick immediately; the spawn-time query
        // below IS that fire, so consume it rather than querying twice.
        idle.tick().await;

        let mut last_fire_ms = wall_now_ms();
        request_snapshot_once(
            &session, &topic, &registry, &book, &rr, &crr, community, &dirty,
        )
        .await;

        loop {
            tokio::select! {
                _ = resync.notified() => {}
                _ = idle.tick() => {}
            }
            let now = wall_now_ms();
            if !cooldown_elapsed(last_fire_ms, now) {
                let remaining =
                    ADDRBOOK_SNAPSHOT_COOLDOWN_MS.saturating_sub(now.saturating_sub(last_fire_ms));
                tokio::time::sleep(Duration::from_millis(remaining)).await;
            }
            // Decorrelate this fire from every other member woken by the same
            // roster edge. Drawn fresh per fire from entropy, and applied AFTER
            // the deferral so a herd that all defer to the same instant still
            // spreads. `cooldown_elapsed` stays pure — the jitter is timing,
            // not a rate-limit decision.
            tokio::time::sleep(Duration::from_millis(jitter_ms(rand::random::<u64>()))).await;
            last_fire_ms = wall_now_ms();
            request_snapshot_once(
                &session, &topic, &registry, &book, &rr, &crr, community, &dirty,
            )
            .await;
        }
    })
}

/// Sidecar persist task: on `dirty`, debounce
/// `ADDRBOOK_PERSIST_DEBOUNCE_MS`, then snapshot the community's rows and
/// write `addrbook.cbor` off the async runtime. Snapshot-then-`spawn_blocking`
/// mirrors `community_state_sync::persist_crdt_only` — the (sync) store lock
/// is never held across the file write.
///
/// A failed write is logged and dropped: the book is fully peer-recoverable
/// (spec §2), so a persist failure must not take down the sync tasks.
pub fn spawn_addrbook_persist_task(
    book: Arc<CommunityAddressBook>,
    path: PathBuf,
    community: SpaceId,
    dirty: Arc<Notify>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            dirty.notified().await;
            tokio::time::sleep(Duration::from_millis(ADDRBOOK_PERSIST_DEBOUNCE_MS)).await;
            let rows = book.rows_for_community(&community, wall_now_ms());
            let path_for_write = path.clone();
            let result =
                tokio::task::spawn_blocking(move || save_addrbook(&path_for_write, &rows)).await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(
                    community = %hex::encode(&community.0[..4]),
                    path = %path.display(),
                    err = %e,
                    "addrbook sidecar write failed"
                ),
                Err(e) => tracing::warn!(
                    community = %hex::encode(&community.0[..4]),
                    err = %e,
                    "addrbook sidecar write task join failed"
                ),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_address_book::AddressBookEntry;
    use crate::community_channel_log::derive_presence_key;
    use crate::owner_state_types::{Hlc, OwnerAddr};
    use crate::reachability_record::ReachabilityAnnouncePayload;

    fn hlc(ms: u64) -> Hlc {
        Hlc {
            wall_ms: ms,
            logical: 0,
            device_id: "d".into(),
        }
    }

    fn row(seed: u8, ts: u64) -> AddressBookRow {
        AddressBookRow {
            entry: AddressBookEntry::Reachability(ReachabilityAnnouncePayload {
                iroh_node_id: [seed; 32],
                home_relay_url: "https://derp.example/".into(),
                direct_addresses: vec![],
                announced_at_ms: ts,
                identity_signature: [0; 64],
                butler_set: Vec::new(),
                bs_at: 0,
            }),
            actor: OwnerAddr([seed; 16]),
            device: [seed; 32],
            at: hlc(ts),
            stamped_at_ms: ts,
        }
    }

    fn fixture_key(community: &SpaceId) -> ChannelKey {
        derive_addrbook_key(&EpochKey::new([7u8; 32]), community)
    }

    #[test]
    fn seal_open_round_trip_single_and_many() {
        let c = SpaceId([0xc0; 16]);
        let key = fixture_key(&c);

        let one = vec![row(1, 1_000)];
        let sealed_one = seal_records(&key, &c, &one).unwrap();
        assert_eq!(open_records(&key, &c, &sealed_one), Some(one));

        let many: Vec<AddressBookRow> = (1..=5u8).map(|i| row(i, 1_000 + i as u64)).collect();
        let sealed_many = seal_records(&key, &c, &many).unwrap();
        assert_eq!(open_records(&key, &c, &sealed_many), Some(many));
    }

    #[test]
    fn wrong_key_fails_open() {
        let c = SpaceId([0xc0; 16]);
        let key = fixture_key(&c);
        let other = derive_addrbook_key(&EpochKey::new([9u8; 32]), &c);

        let rows = vec![row(1, 1_000)];
        let sealed = seal_records(&key, &c, &rows).unwrap();
        assert_eq!(open_records(&other, &c, &sealed), None);
    }

    #[test]
    fn tampered_packet_fails_open() {
        let c = SpaceId([0xc0; 16]);
        let key = fixture_key(&c);

        let rows = vec![row(1, 1_000)];
        let mut sealed = seal_records(&key, &c, &rows).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        assert_eq!(open_records(&key, &c, &sealed), None);
    }

    #[test]
    fn oversize_packet_rejected_before_decrypt() {
        let c = SpaceId([0xc0; 16]);
        let key = fixture_key(&c);
        let oversize = vec![0u8; ADDRBOOK_SNAPSHOT_MAX_BYTES + 1];
        assert_eq!(open_records(&key, &c, &oversize), None);
    }

    #[test]
    fn distinct_from_presence_seal() {
        let c = SpaceId([0xc0; 16]);
        let mk = EpochKey::new([7u8; 32]);
        let presence_key = derive_presence_key(&mk, &c);
        let addrbook_key = derive_addrbook_key(&mk, &c);
        assert_ne!(presence_key.as_bytes(), addrbook_key.as_bytes());
    }

    // ── Task 4: ingest gate (pure core) ─────────────────────────────

    use crate::community_relay_announce::{
        build_signed_community_relay_announce, CommunityRelayEntry,
    };
    use crate::reachability_record::build_signed_payload_with_key;

    #[test]
    fn verified_reachability_row_lands_in_book_and_resolver() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x11; 32]);
        let vk = signing_key.verifying_key();
        let actor = OwnerAddr([0x01; 16]);
        let community = SpaceId([0xC0; 16]);
        let ts = 1_700_000_000_000u64;
        let at = hlc(ts);

        let payload = build_signed_payload_with_key(
            [0xAB; 32],
            "https://derp.example/".into(),
            vec![],
            ts,
            &actor,
            &at,
            Vec::new(),
            0,
            &signing_key,
        )
        .expect("build signed reachability payload");

        let row = AddressBookRow {
            entry: AddressBookEntry::Reachability(payload),
            actor,
            device: vk.to_bytes(),
            at,
            stamped_at_ms: ts,
        };

        let book = CommunityAddressBook::new();
        let reach_resolver = ReachabilityResolver::new();
        let relay_resolver = CommunityRelayResolver::new();

        let outcome =
            ingest_verified_row(&book, &reach_resolver, &relay_resolver, community, row, ts);
        assert_eq!(outcome, IngestOutcome::Applied(UpsertOutcome::Inserted));
        assert!(!reach_resolver.resolve(&actor).is_empty());
    }

    #[test]
    fn verified_relay_row_lands_in_book_and_relay_resolver() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x22; 32]);
        let vk = signing_key.verifying_key();
        let actor = OwnerAddr([0x02; 16]);
        let community = SpaceId([0xC1; 16]);
        let ts = 1_700_000_000_000u64;
        let at = hlc(ts);

        let relay_entry = CommunityRelayEntry {
            relay_device_id: [0x44; 16],
            iroh_endpoint_id: [0x55; 32],
            relay_device_ed25519_verify: vk.to_bytes(),
            home_relay: "https://r.example/".into(),
        };
        let payload =
            build_signed_community_relay_announce(relay_entry, ts, &actor, &at, &signing_key)
                .expect("build signed relay payload");

        let row = AddressBookRow {
            entry: AddressBookEntry::Relay(payload),
            actor,
            device: vk.to_bytes(),
            at,
            stamped_at_ms: ts,
        };

        let book = CommunityAddressBook::new();
        let reach_resolver = ReachabilityResolver::new();
        let relay_resolver = CommunityRelayResolver::new();

        let outcome =
            ingest_verified_row(&book, &reach_resolver, &relay_resolver, community, row, ts);
        assert_eq!(outcome, IngestOutcome::Applied(UpsertOutcome::Inserted));
        assert!(!relay_resolver
            .relays_for_community(&community, ts)
            .is_empty());
    }

    #[test]
    fn bad_signature_rejected_nothing_stored() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x33; 32]);
        let vk = signing_key.verifying_key();
        let actor = OwnerAddr([0x03; 16]);
        let community = SpaceId([0xC2; 16]);
        let ts = 1_700_000_000_000u64;
        let at = hlc(ts);

        let mut payload = build_signed_payload_with_key(
            [0xCD; 32],
            "https://derp.example/".into(),
            vec![],
            ts,
            &actor,
            &at,
            Vec::new(),
            0,
            &signing_key,
        )
        .expect("build signed reachability payload");
        payload.identity_signature[0] ^= 0xFF; // corrupt

        let row = AddressBookRow {
            entry: AddressBookEntry::Reachability(payload),
            actor,
            device: vk.to_bytes(),
            at,
            stamped_at_ms: ts,
        };

        let book = CommunityAddressBook::new();
        let reach_resolver = ReachabilityResolver::new();
        let relay_resolver = CommunityRelayResolver::new();

        let outcome =
            ingest_verified_row(&book, &reach_resolver, &relay_resolver, community, row, ts);
        assert_eq!(outcome, IngestOutcome::BadSignature);
        assert!(book.rows_for_community(&community, ts).is_empty());
        assert!(reach_resolver.resolve(&actor).is_empty());
    }

    #[test]
    fn older_row_ignored_resolver_not_double_fed() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x44; 32]);
        let vk = signing_key.verifying_key();
        let actor = OwnerAddr([0x04; 16]);
        let community = SpaceId([0xC3; 16]);
        let node_id = [0xEE; 32];

        let book = CommunityAddressBook::new();
        let reach_resolver = ReachabilityResolver::new();
        let relay_resolver = CommunityRelayResolver::new();

        let now_ms = 1_700_000_000_000u64;
        let at1 = hlc(now_ms);
        let payload1 = build_signed_payload_with_key(
            node_id,
            "https://derp.example/".into(),
            vec![],
            now_ms,
            &actor,
            &at1,
            Vec::new(),
            0,
            &signing_key,
        )
        .expect("build signed reachability payload (first)");
        let row1 = AddressBookRow {
            entry: AddressBookEntry::Reachability(payload1),
            actor,
            device: vk.to_bytes(),
            at: at1,
            stamped_at_ms: now_ms,
        };
        let outcome1 = ingest_verified_row(
            &book,
            &reach_resolver,
            &relay_resolver,
            community,
            row1,
            now_ms,
        );
        assert_eq!(outcome1, IngestOutcome::Applied(UpsertOutcome::Inserted));

        // Second ingest, same key (actor, node_id), OLDER stamp — must be
        // ignored by the store and never reach the resolver.
        let older_ts = now_ms - 5_000;
        let at2 = hlc(older_ts);
        let payload2 = build_signed_payload_with_key(
            node_id,
            "https://derp.example/".into(),
            vec![],
            older_ts,
            &actor,
            &at2,
            Vec::new(),
            0,
            &signing_key,
        )
        .expect("build signed reachability payload (second, older)");
        let row2 = AddressBookRow {
            entry: AddressBookEntry::Reachability(payload2),
            actor,
            device: vk.to_bytes(),
            at: at2,
            stamped_at_ms: older_ts,
        };
        let outcome2 = ingest_verified_row(
            &book,
            &reach_resolver,
            &relay_resolver,
            community,
            row2,
            now_ms,
        );
        assert_eq!(
            outcome2,
            IngestOutcome::Applied(UpsertOutcome::IgnoredOlder)
        );

        // The resolver must still show the FIRST (newer) announced_at_ms —
        // the ignored second row was never fanned out to it.
        let resolved = reach_resolver.resolve(&actor);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].announced_at_ms, now_ms);
    }

    // ── Task 7: membership eviction ─────────────────────────────────

    /// The three stores `ingest_verified_row` writes into, bundled so the seed
    /// helpers below read as one call rather than a wall of arguments.
    struct Sinks {
        book: CommunityAddressBook,
        reach: ReachabilityResolver,
        relay: CommunityRelayResolver,
    }

    impl Sinks {
        fn new() -> Self {
            Self {
                book: CommunityAddressBook::new(),
                reach: ReachabilityResolver::new(),
                relay: CommunityRelayResolver::new(),
            }
        }

        /// Seed one reachability row through the real ingest gate (signature
        /// and all), asserting it landed — the eviction test below needs a
        /// book whose contents got there the way production's does.
        fn seed_reach(
            &self,
            community: SpaceId,
            actor: OwnerAddr,
            sk: &ed25519_dalek::SigningKey,
            node_id: [u8; 32],
            ts: u64,
        ) {
            let at = hlc(ts);
            let payload = build_signed_payload_with_key(
                node_id,
                "https://derp.example/".into(),
                vec![],
                ts,
                &actor,
                &at,
                Vec::new(),
                0,
                sk,
            )
            .expect("build signed reachability payload");
            let row = AddressBookRow {
                entry: AddressBookEntry::Reachability(payload),
                actor,
                device: sk.verifying_key().to_bytes(),
                at,
                stamped_at_ms: ts,
            };
            assert_eq!(
                ingest_verified_row(&self.book, &self.reach, &self.relay, community, row, ts),
                IngestOutcome::Applied(UpsertOutcome::Inserted),
            );
        }

        /// Relay-row counterpart of [`Sinks::seed_reach`].
        fn seed_relay(
            &self,
            community: SpaceId,
            actor: OwnerAddr,
            sk: &ed25519_dalek::SigningKey,
            relay_device_id: [u8; 16],
            ts: u64,
        ) {
            let at = hlc(ts);
            let entry = CommunityRelayEntry {
                relay_device_id,
                iroh_endpoint_id: [0x55; 32],
                relay_device_ed25519_verify: sk.verifying_key().to_bytes(),
                home_relay: "https://r.example/".into(),
            };
            let payload = build_signed_community_relay_announce(entry, ts, &actor, &at, sk)
                .expect("build signed relay payload");
            let row = AddressBookRow {
                entry: AddressBookEntry::Relay(payload),
                actor,
                device: sk.verifying_key().to_bytes(),
                at,
                stamped_at_ms: ts,
            };
            assert_eq!(
                ingest_verified_row(&self.book, &self.reach, &self.relay, community, row, ts),
                IngestOutcome::Applied(UpsertOutcome::Inserted),
            );
        }

        fn book_actors(&self, community: &SpaceId, ts: u64) -> Vec<OwnerAddr> {
            self.book
                .rows_for_community(community, ts)
                .into_iter()
                .map(|r| r.actor)
                .collect()
        }
    }

    /// The eviction half of the ZEB-815 flag-day: the calls the delta hook's
    /// Kick and Leave arms make must clear the departed member from the
    /// address book as well as from the two resolvers.
    ///
    /// The book is COMMUNITY-scoped and `ReachabilityResolver::remove_owner`
    /// is OWNER-global — a difference the arms depend on (the book eviction
    /// runs ungated while the resolver eviction sits behind the ZEB-634
    /// co-membership consult). Both departures here are from community A
    /// while the same members hold rows in community B, which is exactly the
    /// case that tells the two scopes apart.
    #[test]
    fn kick_evicts_book_rows_and_leave_evicts_book_rows() {
        let community_a = SpaceId([0xA1; 16]);
        let community_b = SpaceId([0xB2; 16]);
        let ts = 1_700_000_000_000u64;

        let s = Sinks::new();

        let kicked_sk = ed25519_dalek::SigningKey::from_bytes(&[0x51; 32]);
        let leaver_sk = ed25519_dalek::SigningKey::from_bytes(&[0x52; 32]);
        let bystander_sk = ed25519_dalek::SigningKey::from_bytes(&[0x53; 32]);
        let kicked = OwnerAddr([0x51; 16]);
        let leaver = OwnerAddr([0x52; 16]);
        let bystander = OwnerAddr([0x53; 16]);

        // Community A: all three, with the kicked member also advertising a
        // relay (both entry kinds must go).
        s.seed_reach(community_a, kicked, &kicked_sk, [0x01; 32], ts);
        s.seed_relay(community_a, kicked, &kicked_sk, [0xD1; 16], ts);
        s.seed_reach(community_a, leaver, &leaver_sk, [0x02; 32], ts);
        s.seed_reach(community_a, bystander, &bystander_sk, [0x03; 32], ts);
        // Community B: the same two departing members still hold rows there.
        s.seed_reach(community_b, kicked, &kicked_sk, [0x04; 32], ts);
        s.seed_reach(community_b, leaver, &leaver_sk, [0x05; 32], ts);

        assert_eq!(s.book_actors(&community_a, ts).len(), 4);
        assert_eq!(s.book_actors(&community_b, ts).len(), 2);

        // ── Kick arm, community A ──
        // The relay retraction + book eviction run unconditionally; the
        // owner-global resolver eviction is what the consult gates (the
        // kicked member is a co-member of B, so a faithful arm SKIPS it).
        assert_eq!(s.relay.remove_advertiser(&community_a, &kicked), 1);
        assert_eq!(
            s.book.remove_owner(&community_a, &kicked),
            2,
            "both the reachability and the relay row for the kicked member"
        );

        assert!(
            !s.book_actors(&community_a, ts).contains(&kicked),
            "kicked member's rows are gone from community A"
        );
        assert!(
            s.book_actors(&community_b, ts).contains(&kicked),
            "community-scoped: the kicked member's community B rows survive"
        );
        assert!(
            s.relay
                .relays_for_community(&community_a, ts)
                .iter()
                .all(|e| e.relay_device_id != [0xD1; 16]),
            "kicked member's relay ad retracted from community A"
        );
        assert!(
            !s.reach.resolve(&kicked).is_empty(),
            "co-member of B: the owner-global reachability records survive"
        );

        // ── Leave arm, community A, then the leaver's LAST community ──
        assert_eq!(s.book.remove_owner(&community_a, &leaver), 1);
        assert!(
            !s.book_actors(&community_a, ts).contains(&leaver),
            "leaver's rows are gone from community A"
        );
        assert!(
            s.book_actors(&community_b, ts).contains(&leaver),
            "community-scoped: the leaver's community B rows survive"
        );

        // Leaving B too — now the consult finds no other shared community, so
        // the arm also runs the owner-global resolver eviction. It returns
        // BOTH of the leaver's node-ids: the resolver never knew which
        // community each was announced in, which is precisely why the arm has
        // to consult the projection before calling it.
        assert_eq!(s.book.remove_owner(&community_b, &leaver), 1);
        assert_eq!(s.reach.remove_owner(&leaver).len(), 2);
        assert!(
            s.book_actors(&community_b, ts).iter().all(|a| *a != leaver),
            "leaver evicted from every community's book"
        );
        assert!(
            s.reach.resolve(&leaver).is_empty(),
            "last shared community: reachability records evicted"
        );

        // The bystander is untouched throughout.
        assert!(s.book_actors(&community_a, ts).contains(&bystander));
        assert!(!s.reach.resolve(&bystander).is_empty());
    }

    // ── Task 5: sync-task decision cores ────────────────────────────

    /// The snapshot requester's rate limiter, exercised on BOTH sides of the
    /// boundary: a rapid second notify inside the window must not fire a
    /// second GET, and the instant the window closes it must.
    #[test]
    fn requester_cooldown_gates_rapid_notifies() {
        let first_fire = 1_700_000_000_000u64;

        assert!(
            !cooldown_elapsed(first_fire, first_fire),
            "a notify in the same millisecond as the last fire is gated"
        );
        assert!(
            !cooldown_elapsed(first_fire, first_fire + ADDRBOOK_SNAPSHOT_COOLDOWN_MS - 1),
            "one millisecond BEFORE the window closes is still gated"
        );
        assert!(
            cooldown_elapsed(first_fire, first_fire + ADDRBOOK_SNAPSHOT_COOLDOWN_MS),
            "exactly at the window boundary the next fire is allowed"
        );
        assert!(
            cooldown_elapsed(first_fire, first_fire + ADDRBOOK_SNAPSHOT_COOLDOWN_MS + 1),
            "past the window the next fire is allowed"
        );

        // A never-fired requester (last_fire_ms == 0) fires immediately, and a
        // backwards clock step saturates to "not elapsed" rather than
        // underflowing into a permanently-open gate.
        assert!(cooldown_elapsed(0, first_fire), "first fire is never gated");
        assert!(
            !cooldown_elapsed(first_fire, first_fire - 5_000),
            "a backwards clock step keeps the gate closed (saturating), not open"
        );

        // The woken-fire jitter that decorrelates a roster-edge herd is bounded
        // by the window and never negative-equivalent (it is added to a delay,
        // so a value at or over the window would stretch the fire past the
        // cooldown it composes with). Extremes plus a spread check — no tokio
        // time travel needed, the helper is pure.
        assert_eq!(jitter_ms(0), 0, "the low extreme is a zero-delay fire");
        assert_eq!(
            jitter_ms(ADDRBOOK_SNAPSHOT_JITTER_MS - 1),
            ADDRBOOK_SNAPSHOT_JITTER_MS - 1,
            "the high extreme maps to the top of the window"
        );
        assert_eq!(
            jitter_ms(ADDRBOOK_SNAPSHOT_JITTER_MS),
            0,
            "the window wraps rather than escaping it"
        );
        for raw in [1u64, 4_321, u64::MAX / 3, u64::MAX] {
            assert!(
                jitter_ms(raw) < ADDRBOOK_SNAPSHOT_JITTER_MS,
                "raw {raw} must land inside the jitter window"
            );
        }
        // Distinct raw draws must land on distinct delays — a helper that
        // collapsed everything onto one value would compile, pass the bound
        // checks above, and still leave the herd perfectly synchronized.
        assert_ne!(
            jitter_ms(7),
            jitter_ms(1_234),
            "the jitter must actually spread, not map every draw to one delay"
        );
    }

    /// The queryable↔requester contract minus the wire: the responder seals
    /// its full row set under the community address-book key; the requester
    /// opens it and runs every row through the SAME per-row ingest gate live
    /// records use, landing them in the book and fanning out to both
    /// resolvers.
    #[test]
    fn snapshot_reply_and_ingest_round_trip() {
        let community = SpaceId([0xC5; 16]);
        let ts = 1_700_000_000_000u64;
        let at = hlc(ts);
        let key = fixture_key(&community);

        // Responder side: two members, one reachability row and one relay row.
        let reach_sk = ed25519_dalek::SigningKey::from_bytes(&[0x51; 32]);
        let reach_actor = OwnerAddr([0x05; 16]);
        let reach_payload = build_signed_payload_with_key(
            [0xA5; 32],
            "https://derp.example/".into(),
            vec![],
            ts,
            &reach_actor,
            &at,
            Vec::new(),
            0,
            &reach_sk,
        )
        .expect("build signed reachability payload");

        let relay_sk = ed25519_dalek::SigningKey::from_bytes(&[0x52; 32]);
        let relay_actor = OwnerAddr([0x06; 16]);
        let relay_payload = build_signed_community_relay_announce(
            CommunityRelayEntry {
                relay_device_id: [0x77; 16],
                iroh_endpoint_id: [0x88; 32],
                relay_device_ed25519_verify: relay_sk.verifying_key().to_bytes(),
                home_relay: "https://r.example/".into(),
            },
            ts,
            &relay_actor,
            &at,
            &relay_sk,
        )
        .expect("build signed relay payload");

        let served = vec![
            AddressBookRow {
                entry: AddressBookEntry::Reachability(reach_payload),
                actor: reach_actor,
                device: reach_sk.verifying_key().to_bytes(),
                at: at.clone(),
                stamped_at_ms: ts,
            },
            AddressBookRow {
                entry: AddressBookEntry::Relay(relay_payload),
                actor: relay_actor,
                device: relay_sk.verifying_key().to_bytes(),
                at,
                stamped_at_ms: ts,
            },
        ];
        let packet = seal_records(&key, &community, &served).expect("seal snapshot");

        // Requester side: open under the same community key, then per-row gate.
        let opened = open_records(&key, &community, &packet).expect("open snapshot");
        assert_eq!(opened.len(), 2, "both rows survive the snapshot round trip");

        let book = CommunityAddressBook::new();
        let reach_resolver = ReachabilityResolver::new();
        let relay_resolver = CommunityRelayResolver::new();
        for row in opened {
            assert_eq!(
                ingest_verified_row(&book, &reach_resolver, &relay_resolver, community, row, ts),
                IngestOutcome::Applied(UpsertOutcome::Inserted),
                "a snapshot row goes through the same gate as a live record"
            );
        }

        assert_eq!(book.rows_for_community(&community, ts).len(), 2);
        assert_eq!(
            reach_resolver.resolve(&reach_actor).len(),
            1,
            "the reachability row fanned out to the dial resolver"
        );
        assert_eq!(
            relay_resolver.relays_for_community(&community, ts).len(),
            1,
            "the relay row fanned out to the community relay resolver"
        );

        // A responder sealing under a DIFFERENT epoch key contributes nothing:
        // the requester cannot open the packet at all.
        let foreign = derive_addrbook_key(&EpochKey::new([0xEE; 32]), &community);
        assert!(
            open_records(&foreign, &community, &packet).is_none(),
            "a snapshot sealed under another epoch key never reaches the gate"
        );
    }

    // ── Task 6: publisher swap ──────────────────────────────────────

    /// Our OWN rows take the same path a peer's row takes — local gate → book
    /// + resolvers — AND go out sealed on the live record topic, with the
    /// per-community dirty handle fired so the sidecar picks the change up.
    /// Two communities in one call so the per-community keying, topic, and
    /// dirty handle are each exercised against a second value rather than a
    /// single one that any constant would satisfy.
    #[tokio::test]
    async fn publish_own_rows_feeds_book_resolvers_and_wire() {
        use futures::FutureExt;

        let reach_community = SpaceId([0xD0; 16]);
        let relay_community = SpaceId([0xD1; 16]);
        let ts = 1_700_000_000_000u64;
        let at = hlc(ts);

        let reach_sk = ed25519_dalek::SigningKey::from_bytes(&[0x61; 32]);
        let reach_actor = OwnerAddr([0x0A; 16]);
        let reach_payload = build_signed_payload_with_key(
            [0xB1; 32],
            "https://derp.example/".into(),
            vec![],
            ts,
            &reach_actor,
            &at,
            Vec::new(),
            0,
            &reach_sk,
        )
        .expect("build signed reachability payload");
        let reach_row = AddressBookRow {
            entry: AddressBookEntry::Reachability(reach_payload),
            actor: reach_actor,
            device: reach_sk.verifying_key().to_bytes(),
            at: at.clone(),
            stamped_at_ms: ts,
        };

        let relay_sk = ed25519_dalek::SigningKey::from_bytes(&[0x62; 32]);
        let relay_actor = OwnerAddr([0x0B; 16]);
        let relay_payload = build_signed_community_relay_announce(
            CommunityRelayEntry {
                relay_device_id: [0x99; 16],
                iroh_endpoint_id: [0xAA; 32],
                relay_device_ed25519_verify: relay_sk.verifying_key().to_bytes(),
                home_relay: "https://r.example/".into(),
            },
            ts,
            &relay_actor,
            &at,
            &relay_sk,
        )
        .expect("build signed relay payload");
        let relay_row = AddressBookRow {
            entry: AddressBookEntry::Relay(relay_payload),
            actor: relay_actor,
            device: relay_sk.verifying_key().to_bytes(),
            at,
            stamped_at_ms: ts,
        };

        let book = CommunityAddressBook::new();
        let rr = ReachabilityResolver::new();
        let crr = CommunityRelayResolver::new();
        let dirty = AddrbookDirtyHub::new();
        let (publish_tx, mut publish_rx) =
            tokio::sync::mpsc::channel::<crate::event_loop::PublishRequest>(8);

        // Stand in for the event loop's `publish_rx` arm: answer the oneshot
        // (the publisher AWAITS it, so an undrained channel would hang) and
        // keep what was sent. Drains until `publish_tx` is dropped below.
        let drain = tokio::spawn(async move {
            let mut seen: Vec<(String, Vec<u8>)> = Vec::new();
            while let Some(req) = publish_rx.recv().await {
                let _ = req.reply.send(Ok(()));
                seen.push((req.key_expr, req.payload));
            }
            seen
        });

        let epoch = EpochKey::new([7u8; 32]);
        let key_fn = |c: &SpaceId| Some(derive_addrbook_key(&epoch, c));
        publish_own_rows(
            &key_fn,
            &book,
            &rr,
            &crr,
            &publish_tx,
            vec![
                (reach_community, reach_row.clone()),
                (relay_community, relay_row.clone()),
            ],
            &dirty,
        )
        .await;
        drop(publish_tx);
        let seen = drain.await.expect("publish drain task");

        // (a) both rows are in OUR OWN book, under their own communities.
        assert_eq!(
            book.rows_for_community(&reach_community, ts),
            vec![reach_row.clone()],
            "our own reachability row lands in our own book"
        );
        assert_eq!(
            book.rows_for_community(&relay_community, ts),
            vec![relay_row.clone()],
            "our own relay row lands in our own book"
        );

        // (b) both resolvers see them — the dial + deposit consumers read
        //     these, and before Task 6 the CRDT delta hook is what fed them.
        assert_eq!(
            rr.resolve(&reach_actor).len(),
            1,
            "our own reachability row fanned out to the dial resolver"
        );
        assert_eq!(
            crr.relays_for_community(&relay_community, ts).len(),
            1,
            "our own relay row fanned out to the community relay resolver"
        );

        // (c) both went out on the wire, on the RIGHT topic, and open back to
        //     the exact row under that community's own derived key.
        assert_eq!(seen.len(), 2, "one publish per row");
        for (community, row) in [(reach_community, reach_row), (relay_community, relay_row)] {
            let topic = addrbook_records_topic(&community);
            let (_, packet) = seen
                .iter()
                .find(|(expr, _)| *expr == topic)
                .unwrap_or_else(|| panic!("no publish on {topic}"));
            let opened = open_records(&derive_addrbook_key(&epoch, &community), &community, packet)
                .expect("own record opens under the community's addrbook key");
            assert_eq!(
                opened,
                vec![row],
                "the sealed record round-trips to the row"
            );
        }

        // (d) the sidecar wakes for BOTH communities — a local publish that
        //     never marked the book dirty would survive only if a peer echoed
        //     it back. The unrelated community is the control: a hub that
        //     handed out pre-notified handles would satisfy the first two
        //     asserts for the wrong reason.
        assert!(
            dirty
                .handle(reach_community.0)
                .notified()
                .now_or_never()
                .is_some(),
            "the reachability community's persist task was woken"
        );
        assert!(
            dirty
                .handle(relay_community.0)
                .notified()
                .now_or_never()
                .is_some(),
            "the relay community's persist task was woken"
        );
        assert!(
            dirty.handle([0xEE; 16]).notified().now_or_never().is_none(),
            "a community we published nothing for is NOT woken"
        );
    }

    /// Build a correctly-signed reachability row plus the wiring
    /// `publish_own_rows` needs, for the two policy-branch tests below.
    /// Returns the row and a drain task that answers (and records) publishes.
    fn own_row_fixture(seed: u8) -> (AddressBookRow, OwnerAddr) {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let actor = OwnerAddr([seed; 16]);
        let ts = 1_700_000_000_000u64;
        let at = hlc(ts);
        let payload = build_signed_payload_with_key(
            [seed; 32],
            "https://derp.example/".into(),
            vec![],
            ts,
            &actor,
            &at,
            Vec::new(),
            0,
            &sk,
        )
        .expect("build signed reachability payload");
        (
            AddressBookRow {
                entry: AddressBookEntry::Reachability(payload),
                actor,
                device: sk.verifying_key().to_bytes(),
                at,
                stamped_at_ms: ts,
            },
            actor,
        )
    }

    /// A row that fails OUR OWN gate is one every member would reject at
    /// theirs, so it must not reach the wire — and must leave the book, the
    /// resolver, and the persist task untouched. Guards the `bad => continue`
    /// branch: publishing first and gating second would still pass the happy
    /// path test above.
    #[tokio::test]
    async fn rejected_own_row_is_not_published() {
        use futures::FutureExt;

        let community = SpaceId([0xD2; 16]);
        let (mut row, actor) = own_row_fixture(0x71);
        // Corrupt the inner signature — the same failure a broken minting
        // path would produce.
        if let AddressBookEntry::Reachability(p) = &mut row.entry {
            p.identity_signature[0] ^= 0xFF;
        }

        let book = CommunityAddressBook::new();
        let rr = ReachabilityResolver::new();
        let crr = CommunityRelayResolver::new();
        let dirty = AddrbookDirtyHub::new();
        let (publish_tx, mut publish_rx) =
            tokio::sync::mpsc::channel::<crate::event_loop::PublishRequest>(8);

        let key_fn = |c: &SpaceId| Some(derive_addrbook_key(&EpochKey::new([7u8; 32]), c));
        publish_own_rows(
            &key_fn,
            &book,
            &rr,
            &crr,
            &publish_tx,
            vec![(community, row)],
            &dirty,
        )
        .await;

        assert!(
            publish_rx.try_recv().is_err(),
            "a row that fails our own gate is never sent"
        );
        assert!(
            book.rows_for_community(&community, 1_700_000_000_000)
                .is_empty(),
            "and never stored"
        );
        assert!(rr.resolve(&actor).is_empty(), "and never fanned out");
        assert!(
            dirty
                .handle(community.0)
                .notified()
                .now_or_never()
                .is_none(),
            "and never marks the book dirty"
        );
    }

    /// No key for a community means we cannot seal anything a member could
    /// open — the row still belongs in OUR book (it is our own address, and
    /// the sidecar should keep it), but nothing goes on the wire. Guards the
    /// `key_fn` → `None` branch against both failure directions: dropping the
    /// row entirely, or publishing it unsealed.
    #[tokio::test]
    async fn own_row_without_a_key_stays_local() {
        let community = SpaceId([0xD3; 16]);
        let ts = 1_700_000_000_000u64;
        let (row, actor) = own_row_fixture(0x72);

        let book = CommunityAddressBook::new();
        let rr = ReachabilityResolver::new();
        let crr = CommunityRelayResolver::new();
        let dirty = AddrbookDirtyHub::new();
        let (publish_tx, mut publish_rx) =
            tokio::sync::mpsc::channel::<crate::event_loop::PublishRequest>(8);

        let key_fn = |_: &SpaceId| None;
        publish_own_rows(
            &key_fn,
            &book,
            &rr,
            &crr,
            &publish_tx,
            vec![(community, row.clone())],
            &dirty,
        )
        .await;

        assert!(
            publish_rx.try_recv().is_err(),
            "a community we hold no key for gets nothing on the wire"
        );
        assert_eq!(
            book.rows_for_community(&community, ts),
            vec![row],
            "but the row is still in our own book"
        );
        assert_eq!(
            rr.resolve(&actor).len(),
            1,
            "and still reached the resolver"
        );
    }

    /// A book too large to seal under the receiver's cap is served
    /// newest-first and truncated, never as an over-cap packet the receiver
    /// would reject before decrypting. `max_bytes` is a parameter (not the
    /// constant) so the boundary is testable without building a megabyte.
    #[test]
    fn oversize_snapshot_truncates_to_newest_rows() {
        let community = SpaceId([0xC6; 16]);
        let key = fixture_key(&community);
        let rows: Vec<AddressBookRow> = (1..=8u8).map(|i| row(i, 1_000 + i as u64)).collect();

        let full = seal_snapshot_bounded(&key, &community, rows.clone(), usize::MAX)
            .expect("seal under an unbounded cap");
        assert_eq!(
            open_records(&key, &community, &full)
                .expect("open full")
                .len(),
            8,
            "an unbounded cap serves every row"
        );

        // A cap under the full size forces truncation; whatever survives must
        // still open, and must be the NEWEST rows.
        let cap = full.len() / 2;
        let trimmed = seal_snapshot_bounded(&key, &community, rows, cap).expect("seal under a cap");
        assert!(
            trimmed.len() <= cap,
            "the served packet fits the receiver's cap ({} <= {cap})",
            trimmed.len()
        );
        let served = open_records(&key, &community, &trimmed).expect("open trimmed");
        assert!(
            !served.is_empty() && served.len() < 8,
            "a partial snapshot is served, not an empty or full one (got {})",
            served.len()
        );
        let oldest_kept = served
            .iter()
            .map(|r| r.stamped_at_ms)
            .min()
            .expect("non-empty");
        assert!(
            oldest_kept > 1_000,
            "truncation drops the OLDEST rows first (kept floor {oldest_kept})"
        );
    }
}
