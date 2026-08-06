//! ZEB-811 Task 7: `vine_pull_driver.rs` — follower-side cadenced pull
//! driver.
//!
//! The FOLLOWER half of ZEB-811's relay fan-out: for every creator this
//! node follows, dial a relay from that creator's pkarr-published relay set
//! (`pkarr_vines::resolve_vine_relays`, Task 2) and page the creator's
//! signed descriptor feed over `harmony/vine-relay/v1` (Task 6), advancing a
//! durable `(created_at, id)` cursor per creator. Modeled closely on
//! [`crate::community_relay_pull_driver::CommunityRelayPullDriver`] — see
//! that module for the wake/interval spawn shape and the three
//! load-bearing telemetry conventions this copies verbatim (see
//! [`crate::network_health::VinePullTelemetry`]).
//!
//! ## Why a pure client-session loop
//!
//! [`run_vine_pull_client_session`] is generic over any
//! `AsyncRead`/`AsyncWrite` pair — mirrors `vine_relay::run_vine_relay_session`'s
//! pure/production-shell split — so the cursor-advance rules (the
//! [`IngestVerdict`] mapping, and the "unparseable JSON never moves the
//! cursor" guard) are unit-testable over `tokio::io::duplex` without a real
//! iroh connection. [`IrohVinePullTransport`] is the thin production shell
//! that dials the relay and drives this loop under one overall deadline.
//!
//! ## Bounded mesh-live skip (recency is not completeness)
//!
//! A creator's descriptors also arrive live over the existing
//! zenoh-over-iroh publish/subscribe mesh, so most pull passes would find
//! nothing new. [`VinePullDriver::run_one_pass`] skips a creator whose mesh
//! delivery looks fresher than the last pull attempt — but only for
//! [`VINE_PULL_SKIP_MAX_CONSECUTIVE`] consecutive passes, then forces a
//! pull anyway. Live delivery fans out over a lossy gossip mesh: it proves
//! *something* arrived recently, never that *everything* did, so an
//! unbounded skip could leave a relay-only descriptor page (one the mesh
//! never carried) unpulled forever.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Notify;

use crate::iroh_framing::{read_len_prefixed, write_len_prefixed, Endian};
use crate::pkarr_vines::{resolve_vine_relays, VineRelayEntry};
use crate::vine_relay::{
    decode_vine_pull_response, encode_vine_pull_request, VinePullQuery, VinePullRequest,
    VINE_CONTENT_MAX_FRAME_BYTES, VINE_PULL_PAGE_LIMIT_MAX, VINE_QUERY_MAX_FRAME_BYTES,
    VINE_RELAY_ALPN,
};

/// Periodic floor between vine pull passes. Reuses the community-relay
/// advertisement refresh cadence (~7.5 min, `community_relay_pull_driver::
/// COMMUNITY_RELAY_PULL_INTERVAL_MS`'s sibling constant) — a creator's relay
/// ad refreshes about this often, so re-checking on the same cadence keeps
/// the pull driver in step without inventing a bespoke interval. The driver
/// is also woken immediately via [`VinePullDriver::wake_handle`]; this is
/// only the idle backstop.
pub const VINE_PULL_INTERVAL_MS: u64 =
    crate::community_relay_announce::COMMUNITY_RELAY_AD_REFRESH_MS;

/// Minimum spacing between pkarr resolves of the SAME creator's vine relay
/// set. Mirrors `PKARR_REFRESH_COOLDOWN` (`reachability_resolver.rs:37`,
/// `pub(crate)` to that module's own tree, hence a same-value alias here
/// rather than a re-export) — a creator's relay set changes about as
/// rarely as a reachability record, so re-resolving it on every pull pass
/// would hammer pkarr for no benefit. The cached `relay_set` hint carries
/// the driver between resolves, including across resolve failures.
pub const VINE_PKARR_RESOLVE_COOLDOWN_MS: u64 = 15 * 60 * 1000;

/// ZEB-818: an unverifiable row may advance the pull cursor only within a
/// plausible clock window. Rows that fail ingest AND claim a `created_at`
/// further than this ahead of local time are treated as hostile-relay
/// cursor poisoning and do not advance. Seconds domain (descriptor
/// `created_at` is seconds; the session clock is ms). 30 min = the display-tier
/// house default (`clock_trust::DISPLAY_SKEW_TOLERANCE_SECS`).
pub const VINE_PULL_INVALID_FORWARD_SKEW_SECS: u64 =
    crate::clock_trust::DISPLAY_SKEW_TOLERANCE_SECS;

/// Consecutive passes a creator may be skipped while live mesh delivery
/// looks fresher than the last pull attempt, before the driver forces a
/// pull anyway. See the module doc's "bounded mesh-live skip" section.
pub const VINE_PULL_SKIP_MAX_CONSECUTIVE: u32 = 4;

/// Sync closure over `NodeState.followed_set`'s own mutex (an
/// `Arc<Mutex<HashSet<String>>>`, `lib.rs`). Unlike
/// `community_relay_pull_driver::JoinedCommunitiesFn`'s analogous seam, no
/// refresher task is needed here — the follow set is already continuously
/// maintained by the follow/unfollow IPCs.
pub type FollowedCreatorsFn = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

/// Reads `VineFeedCache::last_received_ms_for_creator` (Task 5) as an
/// injectable seam — like [`FollowedCreatorsFn`], a plain sync closure over
/// the SAME `Arc<Mutex<VineFeedCache>>` the event loop and the production
/// [`VineIngestCtx`] share, so the driver's mesh-live skip logic is
/// unit-testable without a real cache. Not itself part of the ctx trait:
/// the brief's interface list names this a direct dependency, separate from
/// `on_descriptor_sample` (which only reaches the driver through
/// [`VineIngestCtx::ingest_descriptor`]).
pub type LastReceivedMsFn = Arc<dyn Fn(&str) -> Option<u64> + Send + Sync>;

// =====================================================================
// Transport + ingest seams
// =====================================================================

/// One full pull session against a relay for one creator: page the
/// descriptor feed starting after `cursor`, ingesting each row, until a page
/// comes back shorter than the wire limit or an [`IngestVerdict::Halt`]
/// stops the session early. Seam so [`VinePullDriver`] is unit-testable
/// without iroh. The prod impl is [`IrohVinePullTransport`].
#[async_trait::async_trait]
pub trait VinePullTransport: Send + Sync {
    async fn pull_pages(
        &self,
        relay: &VineRelayEntry,
        creator: &str,
        cursor: (u64, String),
        ingest: &dyn VineIngestCtx,
        progress: PullProgressSink,
    ) -> Result<PullSessionResult, String>;
}

/// Injectable ingest seam for one descriptor row's raw JSON bytes. Sync (not
/// async) because the production impl only ever needs a `std::sync::Mutex`
/// lock, mirroring `VineFeedCache::on_descriptor_sample`'s own signature.
pub trait VineIngestCtx: Send + Sync {
    fn ingest_descriptor(&self, creator: &str, json_bytes: &[u8], now_ms: u64) -> IngestVerdict;
}

/// Outcome of ingesting one descriptor row, and what it does to the pull
/// cursor (see [`run_vine_pull_client_session`] for the exact mapping).
///
/// ZEB-811 fix round 1: originally 3 variants (Advance/SkipInvalid/Halt),
/// with `Advance` covering both a genuinely new insert AND a mesh-delivered
/// duplicate (`DescriptorOutcome::AlreadyPresent`). Review caught that this
/// collapsed the two into one counter: `PullSessionResult.ingested` would
/// count every all-duplicate page as "ingested" activity, over-reporting
/// pull-driver contribution when the mesh had already delivered everything.
/// [`AdvanceDuplicate`](Self::AdvanceDuplicate) splits the duplicate case out
/// so the cursor still advances (a duplicate is still a fully valid,
/// already-durable row) without inflating the ingest telemetry a fleet
/// operator reads to tell "the pull driver backfilled N rows" apart from
/// "the mesh already had it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestVerdict {
    /// A genuinely new insert. The cursor advances past this row, and it
    /// counts toward [`PullSessionResult::ingested`].
    Advance,
    /// Accepted, but the mesh already delivered this exact row first (a
    /// cheap no-op, not a fault) — `DescriptorOutcome::AlreadyPresent`.
    /// The cursor still advances past this row (it's fully valid and
    /// already durable), but it does NOT count toward
    /// [`PullSessionResult::ingested`]: a duplicate must never inflate the
    /// telemetry that distinguishes "the pull backfilled this" from "the
    /// mesh already had it".
    AdvanceDuplicate,
    /// Rejected on its own merits (bad signature, stale, malformed field) —
    /// each descriptor is independently dual-signed, so skipping this row
    /// can never forge or hide a later one. The cursor still advances past
    /// it: re-fetching a row that will never validate would loop forever.
    SkipInvalid,
    /// Infrastructure failure (a poisoned cache lock, or the ctx receiving
    /// a topic it cannot map at all) rather than anything about this row.
    /// The session stops immediately with the cursor left at the last
    /// durable row, so the next session retries this row from scratch.
    Halt,
}

/// Outcome of one full pull session against one relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullSessionResult {
    pub cursor: (u64, String),
    pub ingested: u32,
    pub skipped_invalid: u32,
}

/// ZEB-819: caller-owned pull-progress slot. The pull session commits after
/// each fully processed page; the driver reads it even when the IO deadline
/// drops the session future mid-flight, so completed pages are never
/// re-downloaded. Tuple order (created_at, id) matches the cursor.
///
/// ZEB-826: the same slot also carries two drop-surviving counters —
/// `ingested` (rows durably backfilled) and `refused_forward_skew` (rows the
/// ZEB-818 clamp refused) — for the same reason the cursor lives here. A
/// candidate that committed pages then died (dropped future / early `Err`)
/// otherwise takes its counts to the grave, and those are exactly the rows a
/// fleet operator most needs counted: `record_session_ok` sees only the
/// winning candidate's return value.
///
/// Why a side channel rather than the return value: a dropped future never
/// returns anything at all. [`PullSessionResult`] can only report progress
/// the session survived long enough to hand back, and the whole point of
/// the failure this guards is that it does not get that far.
#[derive(Clone, Default)]
pub struct PullProgressSink(Arc<std::sync::Mutex<PullProgress>>);

/// Inner state of [`PullProgressSink`]. All three fields share one lock: they
/// are written at the same page-boundary commit points and read together once
/// per creator per pass.
#[derive(Default)]
struct PullProgress {
    cursor: Option<(u64, String)>,
    /// ZEB-826: rows durably ingested this pass. Summed, not max-merged —
    /// each candidate re-pulls from the same starting cursor and sees earlier
    /// candidates' rows as `AdvanceDuplicate` (cursor advances, ingest count
    /// does not), so per-candidate contributions are disjoint and add to the
    /// pass total rather than overlapping.
    ingested: u32,
    /// ZEB-826: rows the ZEB-818 skew clamp refused to advance the cursor
    /// past, summed across the pass's candidates.
    refused_forward_skew: u32,
}

impl PullProgressSink {
    /// Monotone: only advances (strictly greater tuple order), so a stale
    /// commit from a failed earlier candidate cannot regress progress.
    ///
    /// Poison is RECOVERED, not propagated (every method): the sink exists to
    /// preserve progress across another task's failure, and the slot holds
    /// only a cursor `Option` and two `u32` counters — valid at every
    /// instruction, so a panic while the lock was held cannot leave it torn.
    /// Panicking here would turn one earlier panic into a permanently broken
    /// pull path (every later commit/take repanics until restart), which is
    /// strictly worse than the stale-progress cost it guards against (Qodo
    /// PR #564 round 1).
    pub fn commit(&self, cursor: (u64, String)) {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match slot.cursor.as_ref() {
            Some(cur) if *cur >= cursor => {}
            _ => slot.cursor = Some(cursor),
        }
    }

    /// ZEB-826: add rows ingested since the last commit to the drop-surviving
    /// pass total. Called at the same page-boundary points as [`commit`], so
    /// an ingest count survives a dropped session exactly as its committed
    /// cursor does. Saturating: the count is telemetry, never a correctness
    /// input, so an implausible wrap is capped rather than aliased low.
    pub fn add_ingested(&self, n: u32) {
        if n == 0 {
            return;
        }
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        slot.ingested = slot.ingested.saturating_add(n);
    }

    /// ZEB-826: count `n` rows the ZEB-818 clamp refused. Written the instant
    /// a row is refused (not deferred to a page boundary), so even a session
    /// dropped mid-page still surfaces the refusals it saw.
    pub fn add_refused_forward_skew(&self, n: u32) {
        if n == 0 {
            return;
        }
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        slot.refused_forward_skew = slot.refused_forward_skew.saturating_add(n);
    }

    /// Read and clear the committed cursor. The driver calls this once per
    /// creator per pass, after every candidate relay has had its turn.
    pub fn take(&self) -> Option<(u64, String)> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cursor
            .take()
    }

    /// ZEB-826: read and clear the pass's ingest total — the ingest-count
    /// sibling of [`take`], recording rows a failed candidate committed
    /// before dying (plus the winning candidate's own, which the driver
    /// nets out so `record_session_ok` isn't double-counted).
    pub fn take_ingested(&self) -> u32 {
        std::mem::take(&mut self.0.lock().unwrap_or_else(|e| e.into_inner()).ingested)
    }

    /// ZEB-826: read and clear the pass's refused-advance total.
    pub fn take_refused_forward_skew(&self) -> u32 {
        std::mem::take(
            &mut self
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .refused_forward_skew,
        )
    }
}

/// Minimal fields parsed out of a descriptor's raw JSON *before* handing it
/// to [`VineIngestCtx::ingest_descriptor`] — the cursor-advance decision
/// needs `(created_at, id)` regardless of whether ingest accepts, rejects,
/// or halts on the row, and a row whose JSON doesn't even parse this far
/// must not advance the cursor at all (`descriptor.rs`'s wire shape is
/// `#[serde(rename_all = "camelCase")]`, so `created_at` unmarshals from the
/// wire key `createdAt`; extra fields on the object are ignored by serde_json
/// by default, so this doesn't need the descriptor's full shape).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorFields {
    id: String,
    created_at: u64,
}

/// The pure vine-pull client session loop: request pages starting at
/// `cursor`, ingest each row, and stop when a page is shorter than the wire
/// limit or ingest returns [`IngestVerdict::Halt`]. Generic over any
/// `AsyncRead`/`AsyncWrite` pair (mirrors `vine_relay::run_vine_relay_session`)
/// so it is directly unit-testable over `tokio::io::duplex` without a real
/// iroh connection. [`IrohVinePullTransport::pull_pages`] is the production
/// shell that dials the relay and drives this loop under one overall
/// deadline.
async fn run_vine_pull_client_session<R, W>(
    recv: &mut R,
    send: &mut W,
    creator: &str,
    mut cursor: (u64, String),
    ingest: &dyn VineIngestCtx,
    now_ms: u64,
    progress: PullProgressSink,
) -> Result<PullSessionResult, String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut ingested: u32 = 0;
    // ZEB-826: how much of `ingested` has already been pushed to the sink at a
    // prior page boundary, so each commit point pushes only its page's delta.
    let mut ingested_committed: u32 = 0;
    let mut skipped_invalid: u32 = 0;
    loop {
        let query = VinePullRequest::Query(VinePullQuery {
            creator_addr: creator.to_string(),
            after_created_at: cursor.0,
            after_id: cursor.1.clone(),
            limit: VINE_PULL_PAGE_LIMIT_MAX,
        });
        let req_bytes =
            encode_vine_pull_request(&query).map_err(|e| format!("encode query: {e}"))?;
        write_len_prefixed(
            send,
            &req_bytes,
            VINE_QUERY_MAX_FRAME_BYTES,
            Endian::Le,
            false,
        )
        .await
        .map_err(|e| format!("write query: {e}"))?;

        let resp_bytes = read_len_prefixed(recv, VINE_CONTENT_MAX_FRAME_BYTES, Endian::Le, true)
            .await
            .map_err(|e| format!("read response: {e}"))?;
        let resp =
            decode_vine_pull_response(&resp_bytes).map_err(|e| format!("decode response: {e}"))?;
        let page_len = resp.descriptors.len();
        let cursor_before = cursor.clone();

        for row in &resp.descriptors {
            let bytes = row.as_slice();
            let Ok(fields) = serde_json::from_slice::<CursorFields>(bytes) else {
                // JSON doesn't even parse this far: cannot derive a cursor
                // tuple for it, so it is dropped without ever reaching
                // ingest and the cursor stays at the last good row.
                skipped_invalid += 1;
                continue;
            };
            let candidate = (fields.created_at, fields.id);
            match ingest.ingest_descriptor(creator, bytes, now_ms) {
                IngestVerdict::Advance => {
                    ingested += 1;
                    cursor = candidate;
                }
                IngestVerdict::AdvanceDuplicate => {
                    // Fully valid and already durable — the cursor still
                    // advances past it, but it must not inflate the ingest
                    // counter (see IngestVerdict's doc comment).
                    cursor = candidate;
                }
                IngestVerdict::SkipInvalid => {
                    skipped_invalid += 1;
                    // ZEB-818: this row is unverifiable, so its claimed
                    // `created_at` is attacker-chosen. Advancing to an
                    // implausibly future one would let a hostile relay poison
                    // the persisted cursor past every genuine descriptor
                    // forever. `created_at` is seconds, `now_ms` is ms — the
                    // comparison is in the seconds domain. A full page of
                    // these advances nothing and ends the session via the
                    // zero-advance guard below.
                    if candidate.0 > now_ms / 1000 + VINE_PULL_INVALID_FORWARD_SKEW_SECS {
                        // ZEB-826: broken out of the generic `skipped_invalid`
                        // into its own drop-surviving counter so a
                        // cursor-poisoning relay is visible in a default `info`
                        // build (the debug! below is filtered out there) —
                        // without a spammable `warn!`.
                        progress.add_refused_forward_skew(1);
                        tracing::debug!(
                            creator,
                            created_at = candidate.0,
                            "ZEB-818 pull: refusing cursor advance to an implausibly \
                             future-dated unverifiable row"
                        );
                    } else {
                        cursor = candidate;
                    }
                }
                IngestVerdict::Halt => {
                    // ZEB-819: the rows BEFORE this one did ingest durably,
                    // so publish exactly what `cursor` holds — the halting
                    // row itself never moved it (see the variant's doc).
                    progress.commit(cursor.clone());
                    // ZEB-826: those same durable rows must count toward the
                    // drop-surviving ingest total (sibling of the commit).
                    progress.add_ingested(ingested - ingested_committed);
                    return Ok(PullSessionResult {
                        cursor,
                        ingested,
                        skipped_invalid,
                    });
                }
            }
        }

        // ZEB-819: page boundary. Every row of this page has been through
        // ingest, so `cursor` is durable progress the caller may keep even
        // if the next read never returns. One call site covers the loop's
        // continue and both break paths below; the Halt arm above has its
        // own. This needs no ordering logic of its own because `commit` is
        // MONOTONE — that, and nothing else, is what makes the call site
        // ordering-safe. `cursor` itself is not guaranteed forward-moving:
        // the ZEB-818 skew clamp governs only the `SkipInvalid` arm, while
        // `Advance`/`AdvanceDuplicate` assign `cursor = candidate`
        // order-blind, so a hostile relay serving a below-cursor row does
        // move it backward (final review M5, pre-existing; cost is
        // re-download work only). `commit`'s monotonicity absorbs that
        // here, and the driver's success-path assignment is monotone for
        // the same reason — the durable cursor never follows it backward.
        progress.commit(cursor.clone());
        // ZEB-826: page committed — fold this page's new ingests into the
        // drop-surviving total (the ingest-count sibling of the commit above).
        progress.add_ingested(ingested - ingested_committed);
        ingested_committed = ingested;

        if page_len < VINE_PULL_PAGE_LIMIT_MAX as usize {
            break;
        }
        if cursor == cursor_before {
            // A full page that advanced nothing (e.g. every row is
            // unparseable) would otherwise re-request the identical page
            // forever — treat it like Halt: stop, cursor stays at the last
            // durable tuple.
            tracing::debug!(
                creator,
                "ZEB-811 pull: full page advanced no cursor; ending session"
            );
            break;
        }
    }
    Ok(PullSessionResult {
        cursor,
        ingested,
        skipped_invalid,
    })
}

// =====================================================================
// Production transport
// =====================================================================

/// Production [`VinePullTransport`]: dial the relay's
/// `harmony/vine-relay/v1` ALPN and drive [`run_vine_pull_client_session`]
/// over the opened bi-stream, bounded by `io_deadline` for the WHOLE
/// dial + session + finish exchange (mirrors
/// `community_relay_pull_driver::IrohRelayPullTransport::pull_session`'s
/// single overall deadline).
pub struct IrohVinePullTransport {
    pub endpoint: Arc<crate::iroh_endpoint::IrohEndpoint>,
    pub io_deadline: Duration,
}

#[async_trait::async_trait]
impl VinePullTransport for IrohVinePullTransport {
    async fn pull_pages(
        &self,
        relay: &VineRelayEntry,
        creator: &str,
        cursor: (u64, String),
        ingest: &dyn VineIngestCtx,
        progress: PullProgressSink,
    ) -> Result<PullSessionResult, String> {
        let exchange = async {
            let ep_id = iroh::EndpointId::from_bytes(&relay.iroh_endpoint_id)
                .map_err(|e| format!("relay endpoint id: {e}"))?;
            let mut addr = iroh::EndpointAddr::new(ep_id);
            if !relay.home_relay.is_empty() {
                match relay.home_relay.parse::<iroh::RelayUrl>() {
                    Ok(url) => addr = addr.with_relay_url(url),
                    Err(e) => tracing::trace!(
                        relay = %relay.home_relay,
                        "ZEB-811 pull: skip malformed relay home_relay: {e}"
                    ),
                }
            }
            let conn = self
                .endpoint
                .inner()
                .connect(addr, VINE_RELAY_ALPN)
                .await
                .map_err(|e| format!("connect: {e}"))?;
            let (mut send, mut recv) = conn.open_bi().await.map_err(|e| format!("open_bi: {e}"))?;

            // `progress` goes INSIDE the timeout-wrapped future on purpose:
            // when the deadline fires, this whole future is dropped and its
            // return value is lost, but the sink is the caller's — every
            // page boundary the session reached before the drop is still
            // readable through it.
            let result = run_vine_pull_client_session(
                &mut recv,
                &mut send,
                creator,
                cursor,
                ingest,
                now_ms(),
                progress,
            )
            .await?;

            send.finish().map_err(|e| format!("finish: {e}"))?;
            // Dialer-driven close, mirroring the community-relay pull
            // transport: the relay's serve loop waits on the peer to close.
            conn.close(0u32.into(), b"");
            Ok::<PullSessionResult, String>(result)
        };
        tokio::time::timeout(self.io_deadline, exchange)
            .await
            .map_err(|_| "vine pull IO timeout".to_string())?
    }
}

// =====================================================================
// Production ingest ctx
// =====================================================================

/// Production [`VineIngestCtx`]: locks the node's `VineFeedCache` +
/// followed-set and routes each descriptor through
/// `VineFeedCache::on_descriptor_sample` under a synthetic
/// `harmony/vines/{creator}` topic (the SAME topic shape the live mesh
/// receive path uses, so admission — signature, tombstone, age-window
/// checks — is identical whether a descriptor arrived over the mesh or a
/// relay pull).
pub struct ProdVineIngestCtx {
    pub cache: Arc<std::sync::Mutex<crate::vine_feed_cache::VineFeedCache>>,
    pub followed_set: Arc<std::sync::Mutex<HashSet<String>>>,
}

impl VineIngestCtx for ProdVineIngestCtx {
    fn ingest_descriptor(&self, creator: &str, json_bytes: &[u8], now_ms: u64) -> IngestVerdict {
        let Ok(mut cache) = self.cache.lock() else {
            return IngestVerdict::Halt;
        };
        let Ok(followed) = self.followed_set.lock() else {
            return IngestVerdict::Halt;
        };
        let key_expr = format!("harmony/vines/{creator}");
        match cache.on_descriptor_sample(&key_expr, json_bytes, &followed, now_ms) {
            Some(crate::vine_feed_cache::DescriptorOutcome::Inserted { .. }) => {
                IngestVerdict::Advance
            }
            // The mesh already delivered this exact row: a cheap no-op, not
            // a fault, but it must not inflate the ingest counter (ZEB-811
            // fix round 1 — see IngestVerdict's doc comment).
            Some(crate::vine_feed_cache::DescriptorOutcome::AlreadyPresent) => {
                IngestVerdict::AdvanceDuplicate
            }
            Some(crate::vine_feed_cache::DescriptorOutcome::Rejected(_)) => {
                IngestVerdict::SkipInvalid
            }
            // Only reachable if `key_expr` somehow doesn't parse as a vine
            // descriptor topic, which cannot happen for a synthetic
            // `harmony/vines/{creator}` string we just built ourselves —
            // treated conservatively as an infra fault rather than silently
            // accepted or rejected.
            None => IngestVerdict::Halt,
        }
    }
}

// =====================================================================
// Sidecar persistence
// =====================================================================

/// ZEB-811 Task 7: per-node sidecar tracking pull progress for every
/// followed creator. Lives at `vine_pull.cbor` in `app_data_dir`, beside
/// `follows.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VinePullSidecar {
    pub per_creator: BTreeMap<String, CreatorPullState>,
}

/// One followed creator's pull-driver bookkeeping.
///
/// The cached `relay_set` is a DIALING HINT ONLY: every descriptor pulled
/// through it is independently re-verified on arrival
/// (`vine_signing::verify_descriptor[_v2]`, inside
/// `VineFeedCache::on_descriptor_sample`) exactly like a fresh mesh delivery
/// would be. This mirrors the ZEB-815 boot-seed rule for the address-book
/// sidecar (`lib.rs`, the "BOOT-PROBE 10" address-book routing seed block):
/// rows loaded from a locally-written cache are re-verified through the
/// SAME gate a peer's live data takes, so a tampered or truncated sidecar
/// can misdirect a dial at worst, never inject trusted state as a second
/// path around verification.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CreatorPullState {
    pub cursor: (u64, String),
    pub last_pull_attempt_ms: u64,
    pub consecutive_skips: u32,
    pub relay_set: Vec<VineRelayEntry>,
    pub relays_fetched_at_ms: u64,
}

/// Persist the sidecar via temp-file + rename — same durability posture as
/// `community_address_book::save_addrbook` (no fsync: pull progress is
/// fully re-derivable from a slower next pull if lost, so the extra
/// durability cost doesn't pencil out at this granularity). Like
/// `save_addrbook`, the fixed `.tmp` name is only race-free with a single
/// writer per `path` (true here — one `VinePullDriver` per process owns it).
pub fn save_vine_pull(path: &Path, sidecar: &VinePullSidecar) -> Result<(), String> {
    let mut bytes = Vec::new();
    ciborium::into_writer(sidecar, &mut bytes)
        .map_err(|e| format!("encode vine pull sidecar: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create_dir_all: {e}"))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename to {}: {e}", path.display()))?;
    Ok(())
}

/// Load the sidecar. Missing or corrupt (truncated/undecodable) loads as an
/// empty default — loss-safe: a lost sidecar just means the next pull pass
/// re-derives cursors from scratch (a slower re-pull, never a boot-time
/// hard failure), and every row it seeds is re-verified on arrival anyway
/// (see [`CreatorPullState`]'s doc comment).
pub fn load_vine_pull(path: &Path) -> VinePullSidecar {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return VinePullSidecar::default(),
    };
    ciborium::from_reader(bytes.as_slice()).unwrap_or_default()
}

// =====================================================================
// VinePullDriver
// =====================================================================

/// Cadenced follower-side pull driver: for every followed creator, pull
/// fresh descriptor pages from one of that creator's advertised relays,
/// skipping creators the live mesh appears to have already delivered for
/// (bounded — see the module doc). Spawned by Task 8 via [`Self::spawn`].
pub struct VinePullDriver {
    /// This node's own iroh endpoint id — a creator's relay set can list
    /// this node itself (e.g. a creator who is also a vine relay for their
    /// own feed); ZEB-806 already taught the community-relay driver that
    /// iroh's self-connection rejection turns such an entry into a
    /// permanent doomed-dial loop if not filtered before dialing.
    self_endpoint_id: [u8; 32],
    pkarr_resolver: Arc<harmony_pkarr::PkarrResolver>,
    transport: Arc<dyn VinePullTransport>,
    ingest: Arc<dyn VineIngestCtx>,
    followed_creators: FollowedCreatorsFn,
    last_received_ms: LastReceivedMsFn,
    sidecar_path: PathBuf,
    sidecar: std::sync::Mutex<VinePullSidecar>,
    wake: Arc<Notify>,
    interval: Duration,
    /// ZEB-811: pull-side health telemetry, shared with
    /// `network_health_snapshot` (Task 8). `None` when nothing installed a
    /// source (unit tests, pre-boot); recording is then a no-op.
    telemetry: Option<Arc<crate::network_health::VinePullTelemetry>>,
}

impl VinePullDriver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        self_endpoint_id: [u8; 32],
        pkarr_resolver: Arc<harmony_pkarr::PkarrResolver>,
        transport: Arc<dyn VinePullTransport>,
        ingest: Arc<dyn VineIngestCtx>,
        followed_creators: FollowedCreatorsFn,
        last_received_ms: LastReceivedMsFn,
        sidecar_path: PathBuf,
    ) -> Self {
        let sidecar = load_vine_pull(&sidecar_path);
        Self {
            self_endpoint_id,
            pkarr_resolver,
            transport,
            ingest,
            followed_creators,
            last_received_ms,
            sidecar_path,
            sidecar: std::sync::Mutex::new(sidecar),
            wake: Arc::new(Notify::new()),
            interval: Duration::from_millis(VINE_PULL_INTERVAL_MS),
            telemetry: None,
        }
    }

    /// ZEB-811: install the pull health telemetry sink. Builder-style so
    /// boot wiring can attach it without growing `new`'s already-long
    /// arity (mirrors `CommunityRelayPullDriver::with_telemetry`).
    pub fn with_telemetry(
        mut self,
        telemetry: Arc<crate::network_health::VinePullTelemetry>,
    ) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Clone the wake handle so external callers (a follow/unfollow IPC, or
    /// a test) can trigger an immediate pull pass without holding the whole
    /// `Arc<Self>`.
    pub fn wake_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.wake)
    }

    /// ZEB-811 Task 9: read `creator`'s cached relay-set hint out of the
    /// sidecar (`CreatorPullState::relay_set`) for the video-fetch fallback.
    /// Returns the RAW list — unfiltered, including a possible self-entry —
    /// mirroring what `pull_one_creator` sees before ITS OWN self-filter
    /// step (`lib.rs`'s `plan_video_fetch` does that filtering on the
    /// caller's side; this accessor stays a plain read with no policy of its
    /// own). Empty for a creator with no sidecar row yet (never pulled, or
    /// pruned as unfollowed).
    pub fn cached_relays_for(&self, creator: &str) -> Vec<VineRelayEntry> {
        self.sidecar
            .lock()
            .expect("vine pull sidecar lock")
            .per_creator
            .get(creator)
            .map(|st| st.relay_set.clone())
            .unwrap_or_default()
    }

    /// One pull pass: prune sidecar state for creators no longer followed,
    /// then attempt a pull for every followed creator. Errors are logged
    /// and skipped — one bad relay/creator never aborts the pass.
    pub async fn run_one_pass(&self, now_ms: u64) {
        // Recorded FIRST and unconditionally — before the followed-set
        // read, before any relay work — so this counter can prove the loop
        // is alive even on a pass with nothing to do (mirrors
        // `CommunityRelayPullTelemetry::record_pass_start`'s doc comment).
        if let Some(t) = self.telemetry.as_ref() {
            t.record_pass_start();
        }

        let followed = (self.followed_creators)();
        let followed_set: HashSet<&str> = followed.iter().map(String::as_str).collect();
        {
            let mut sc = self.sidecar.lock().expect("vine pull sidecar lock");
            sc.per_creator
                .retain(|creator, _| followed_set.contains(creator.as_str()));
        }

        for creator in &followed {
            self.pull_one_creator(creator, now_ms).await;
        }

        let snapshot = self.sidecar.lock().expect("vine pull sidecar lock").clone();
        let path = self.sidecar_path.clone();
        match tokio::task::spawn_blocking(move || save_vine_pull(&path, &snapshot)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "ZEB-811 pull: sidecar save failed"),
            Err(e) => tracing::warn!(error = %e, "ZEB-811 pull: sidecar save task panicked"),
        }
    }

    async fn pull_one_creator(&self, creator: &str, now_ms: u64) {
        let mut st = {
            let mut sc = self.sidecar.lock().expect("vine pull sidecar lock");
            sc.per_creator
                .entry(creator.to_string())
                .or_default()
                .clone()
        };

        // Bounded mesh-live skip — recency is not completeness. First
        // follow (genesis cursor) always pulls: it backfills history the
        // mesh never carried, so the skip condition never applies to it.
        let first_follow = st.cursor == (0, String::new());
        let mesh_is_fresh = !first_follow
            && (self.last_received_ms)(creator)
                .map(|last_ms| last_ms > st.last_pull_attempt_ms)
                .unwrap_or(false);
        if mesh_is_fresh && st.consecutive_skips < VINE_PULL_SKIP_MAX_CONSECUTIVE {
            st.consecutive_skips += 1;
            self.store_creator_state(creator, st);
            return;
        }
        st.consecutive_skips = 0;
        st.last_pull_attempt_ms = now_ms;

        if now_ms.saturating_sub(st.relays_fetched_at_ms) >= VINE_PKARR_RESOLVE_COOLDOWN_MS {
            match resolve_vine_relays(&self.pkarr_resolver, creator, now_ms).await {
                Ok(rs) => {
                    st.relay_set = rs;
                    st.relays_fetched_at_ms = now_ms;
                }
                Err(e) => {
                    // Keep the cached hint — a stale relay set is still
                    // better than none, and the next cooldown window will
                    // retry the resolve.
                    tracing::debug!(
                        creator,
                        error = %e,
                        "ZEB-811 pull: relay resolve failed; using cached hint"
                    );
                }
            }
        }

        // ZEB-806 lesson, day one: a creator's own advertised relay set can
        // list this node itself. iroh rejects self-connections, so an
        // unfiltered self-entry becomes a permanent doomed-dial. Filter
        // BEFORE picking a candidate, never after a failed dial.
        let candidates: Vec<VineRelayEntry> = st
            .relay_set
            .iter()
            .filter(|r| r.iroh_endpoint_id != self.self_endpoint_id)
            .cloned()
            .collect();

        if candidates.is_empty() {
            if let Some(t) = self.telemetry.as_ref() {
                t.record_no_relay();
            }
            self.store_creator_state(creator, st);
            return;
        }

        // ZEB-819: one sink per creator per pass, shared by every candidate.
        // A session killed by the IO deadline never returns a
        // `PullSessionResult`, so the pages it DID complete would otherwise
        // be re-downloaded on every future pass; the sink carries them out.
        // Sharing one sink across candidates is safe because `commit` is
        // monotone — a candidate that got less far cannot rewind one that
        // got further.
        let progress = PullProgressSink::default();
        // ZEB-826: the winning candidate's own ingests, recorded by
        // `record_session_ok`; netted out of the sink's pass total below so a
        // rescued failed candidate's rows are counted once and the winner is
        // not counted twice.
        let mut recorded_ingested: u32 = 0;

        // Try every candidate in order within this pass, stopping at the
        // first success — a dead head-of-list relay must not block the
        // creator indefinitely just because the set is only re-resolved
        // every VINE_PKARR_RESOLVE_COOLDOWN_MS and pkarr order is stable.
        // Mirrors `fetch_vine_video_impl`'s multi-relay iteration (`lib.rs`).
        for relay in &candidates {
            match self
                .transport
                .pull_pages(
                    relay,
                    creator,
                    st.cursor.clone(),
                    self.ingest.as_ref(),
                    progress.clone(),
                )
                .await
            {
                Ok(res) => {
                    // Monotone, mirroring the sink merge below: a hostile
                    // relay serving below-cursor rows makes the session
                    // return a REWOUND final cursor (the order-blind
                    // `Advance` assignment — see the session's page-boundary
                    // comment), and taking that at face value was the one
                    // remaining path that could rewind the durable cursor
                    // (CodeRabbit PR #564 round 1).
                    if res.cursor > st.cursor {
                        st.cursor = res.cursor;
                    }
                    // ZEB-826: stash the winner's ingests to net out of the
                    // sink pass total (which includes them) at the merge below.
                    recorded_ingested = res.ingested;
                    if let Some(t) = self.telemetry.as_ref() {
                        t.record_session_ok(creator, &relay.iroh_endpoint_id, res.ingested);
                    }
                    break;
                }
                Err(e) => {
                    tracing::debug!(
                        creator,
                        error = %e,
                        "ZEB-811 pull: pull session failed; trying next relay"
                    );
                    if let Some(t) = self.telemetry.as_ref() {
                        t.record_session_failed(creator, &relay.iroh_endpoint_id);
                    }
                }
            }
        }

        // ZEB-819: single merge point for BOTH outcomes, so page-boundary
        // progress has exactly one path into the durable cursor. The rule is
        // MAX(cursor returned by the candidate that succeeded, highest
        // tuple any candidate committed) — not "rescue only on Err":
        //   * `Err` (including the IO deadline dropping the session future,
        //     which returns nothing at all) — this is what rescues the pages
        //     that did complete;
        //   * `Ok` — a no-op when the successful candidate is the only one
        //     that got anywhere, but it still bites when an EARLIER
        //     candidate committed further before failing and the one that
        //     finally succeeded returned a LOWER cursor (a partial-mirror
        //     relay). Those rows were already ingested durably this pass, so
        //     taking the lower result at face value would rewind past them.
        // Guarded so a stale commit can never rewind the durable cursor.
        // Pinned by `failover_keeps_the_higher_committed_cursor_over_a_lower_success`.
        if let Some(p) = progress.take() {
            if p > st.cursor {
                // ZEB-826: the rescue actually bit — a candidate committed a
                // page (or the winner returned a lower cursor than an earlier
                // failed candidate reached) that the loop's own assignment did
                // not carry. Module convention logs each unusual cursor move.
                tracing::debug!(
                    creator,
                    rescued_created_at = p.0,
                    "ZEB-819 pull: rescued a committed cursor the session loop did not return"
                );
                st.cursor = p;
            }
        }

        // ZEB-826: record ingests the sink rescued from candidates that
        // committed pages then failed — invisible to `record_session_ok`,
        // which sees only the winner's return value. The winner's own ingests
        // are already in the sink total, so net them out; any positive
        // remainder is rows a FAILED candidate committed durably this pass.
        // Refusals ride the same sink so a poisoning relay that also fails the
        // session is still counted.
        if let Some(t) = self.telemetry.as_ref() {
            let rescued = progress.take_ingested().saturating_sub(recorded_ingested);
            if rescued > 0 {
                t.record_rescued_ingested(rescued);
            }
            let refused = progress.take_refused_forward_skew();
            if refused > 0 {
                t.record_refused_forward_skew(refused);
            }
        }

        self.store_creator_state(creator, st);
    }

    fn store_creator_state(&self, creator: &str, st: CreatorPullState) {
        let mut sc = self.sidecar.lock().expect("vine pull sidecar lock");
        sc.per_creator.insert(creator.to_string(), st);
    }

    /// Spawn the driver loop: one immediate startup pass, then a pass on
    /// every `wake` notification or `interval` tick. Returns the
    /// `JoinHandle` the caller may `abort()` on shutdown.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run_one_pass(now_ms()).await;
            let mut ticker = tokio::time::interval(self.interval);
            // `Skip` collapses a backlog of missed ticks (e.g. after a slow
            // pass) into a single tick at the next period boundary, rather
            // than firing every missed tick back-to-back.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // `interval` fires the first tick immediately; the startup
            // pass above already covered it.
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = self.wake.notified() => {
                        self.run_one_pass(now_ms()).await;
                    }
                    _ = ticker.tick() => {
                        self.run_one_pass(now_ms()).await;
                    }
                }
            }
        })
    }
}

/// Wall-clock now in epoch-ms.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tokio::io::AsyncWriteExt;

    // ── Pure client-session-loop tests (mock ingest, no driver) ──

    struct ScriptedIngest {
        verdicts: std::sync::Mutex<VecDeque<IngestVerdict>>,
    }

    impl ScriptedIngest {
        fn new(verdicts: Vec<IngestVerdict>) -> Self {
            Self {
                verdicts: std::sync::Mutex::new(verdicts.into()),
            }
        }
    }

    impl VineIngestCtx for ScriptedIngest {
        fn ingest_descriptor(
            &self,
            _creator: &str,
            _json_bytes: &[u8],
            _now_ms: u64,
        ) -> IngestVerdict {
            self.verdicts
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected extra ingest_descriptor call")
        }
    }

    fn descriptor_json(id: &str, created_at: u64) -> Vec<u8> {
        format!(r#"{{"id":"{id}","createdAt":{created_at}}}"#).into_bytes()
    }

    async fn read_fake_query<R: AsyncRead + Unpin>(r: &mut R) -> VinePullQuery {
        let bytes = read_len_prefixed(r, VINE_QUERY_MAX_FRAME_BYTES, Endian::Le, false)
            .await
            .expect("read query frame");
        match crate::vine_relay::decode_vine_pull_request(&bytes).expect("decode query") {
            VinePullRequest::Query(q) => q,
            VinePullRequest::Content(_) => panic!("expected a Query request"),
        }
    }

    async fn write_fake_page_response<W: AsyncWrite + Unpin>(w: &mut W, rows: Vec<Vec<u8>>) {
        let resp = crate::vine_relay::VinePullResponse {
            descriptors: rows.into_iter().map(serde_bytes::ByteBuf::from).collect(),
        };
        let bytes = crate::vine_relay::encode_vine_pull_response(&resp).expect("encode response");
        write_len_prefixed(w, &bytes, VINE_CONTENT_MAX_FRAME_BYTES, Endian::Le, true)
            .await
            .expect("write response frame");
    }

    #[tokio::test]
    async fn cursor_advances_past_invalid_but_not_past_halt() {
        let (client, server) = tokio::io::duplex(1 << 16);
        let (mut client_read, mut client_write) = tokio::io::split(client);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        let rows = vec![
            descriptor_json("row1", 100),
            descriptor_json("row2", 200), // bad-sig, but JSON parses fine
            descriptor_json("row3", 300),
            descriptor_json("row4", 400), // triggers Halt
        ];
        let server_task = tokio::spawn(async move {
            let _query = read_fake_query(&mut server_read).await;
            write_fake_page_response(&mut server_write, rows).await;
            // `tokio::io::split` shares the underlying stream via an
            // internal Arc — a bare `drop` does NOT half-close it, so the
            // client's read would otherwise never see EOF if it tried a
            // second page. This session never does (Halt returns first),
            // but shutting down here keeps the fixture correct regardless.
            let _ = server_write.shutdown().await;
        });

        let ingest = ScriptedIngest::new(vec![
            IngestVerdict::Advance,
            IngestVerdict::SkipInvalid,
            IngestVerdict::Advance,
            IngestVerdict::Halt,
        ]);

        let result = run_vine_pull_client_session(
            &mut client_read,
            &mut client_write,
            "creator-x",
            (0, String::new()),
            &ingest,
            1_700_000_000_000,
            PullProgressSink::default(),
        )
        .await
        .expect("session must not error");

        server_task.await.expect("server task must not panic");

        assert_eq!(
            result.cursor,
            (300, "row3".to_string()),
            "cursor must land on row 3's tuple, never row 4's (the Halt row)"
        );
        assert_eq!(result.ingested, 2);
        assert_eq!(result.skipped_invalid, 1);
    }

    #[tokio::test]
    async fn unparseable_row_does_not_advance_cursor() {
        let (client, server) = tokio::io::duplex(1 << 16);
        let (mut client_read, mut client_write) = tokio::io::split(client);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        let rows = vec![
            descriptor_json("row1", 100),
            descriptor_json("row2", 200),
            b"not even json{{{".to_vec(),
        ];
        let server_task = tokio::spawn(async move {
            let _query = read_fake_query(&mut server_read).await;
            write_fake_page_response(&mut server_write, rows).await;
            let _ = server_write.shutdown().await;
        });

        // Only two entries: the garbage row's JSON fails to parse before
        // ever reaching ingest, so a third call would be a test bug.
        let ingest = ScriptedIngest::new(vec![IngestVerdict::Advance, IngestVerdict::Advance]);

        let result = run_vine_pull_client_session(
            &mut client_read,
            &mut client_write,
            "creator-y",
            (0, String::new()),
            &ingest,
            1_700_000_000_000,
            PullProgressSink::default(),
        )
        .await
        .expect("session must not error");

        server_task.await.expect("server task must not panic");

        assert_eq!(result.cursor, (200, "row2".to_string()));
        assert_eq!(result.ingested, 2);
        assert_eq!(result.skipped_invalid, 1);
    }

    #[tokio::test]
    async fn full_page_of_unparseable_rows_ends_session_without_looping() {
        // ZEB-811 review fix round 1: a FULL page (== VINE_PULL_PAGE_LIMIT_MAX)
        // whose rows are all unparseable advances the cursor nowhere —
        // without the fix, the loop would re-request the identical page
        // forever. The server task here answers exactly ONE query and then
        // asserts a second one never arrives; the whole session is also
        // wrapped in an outer timeout so a regression fails fast instead of
        // hanging the suite.
        let (client, server) = tokio::io::duplex(1 << 20);
        let (mut client_read, mut client_write) = tokio::io::split(client);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        let rows: Vec<Vec<u8>> = (0..VINE_PULL_PAGE_LIMIT_MAX)
            .map(|_| b"not even json{{{".to_vec())
            .collect();
        assert_eq!(rows.len(), VINE_PULL_PAGE_LIMIT_MAX as usize);

        let server_task = tokio::spawn(async move {
            let _query = read_fake_query(&mut server_read).await;
            write_fake_page_response(&mut server_write, rows).await;
            let _ = server_write.shutdown().await;

            // Proves the client never re-requests the identical page: a
            // second query would be readable well within this bound if the
            // loop-forever bug regressed.
            let second = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                read_fake_query(&mut server_read),
            )
            .await;
            assert!(
                second.is_err(),
                "the client must not re-request an identical full page"
            );
        });

        // No row ever parses far enough to reach ingest.
        let ingest = ScriptedIngest::new(vec![]);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_vine_pull_client_session(
                &mut client_read,
                &mut client_write,
                "creator-z",
                (0, String::new()),
                &ingest,
                1_700_000_000_000,
                PullProgressSink::default(),
            ),
        )
        .await
        .expect("session must terminate promptly, not loop forever")
        .expect("session must not error");

        server_task.await.expect("server task must not panic");

        assert_eq!(
            result.cursor,
            (0, String::new()),
            "cursor must stay at the initial tuple — nothing advanced it"
        );
        assert_eq!(result.ingested, 0);
        assert_eq!(result.skipped_invalid, VINE_PULL_PAGE_LIMIT_MAX as u32);
    }

    #[tokio::test]
    async fn mesh_delivered_duplicate_is_a_cheap_no_op() {
        // ZEB-811 fix round 1: exercises the REAL session loop (not a
        // scripted `PullSessionResult`) against `IngestVerdict::AdvanceDuplicate`
        // — the verdict `ProdVineIngestCtx` maps `DescriptorOutcome::AlreadyPresent`
        // to. This is what actually proves a mesh-delivered duplicate is a
        // cheap no-op: the cursor advances past it, but it never inflates
        // `ingested`.
        let (client, server) = tokio::io::duplex(1 << 16);
        let (mut client_read, mut client_write) = tokio::io::split(client);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        let rows = vec![descriptor_json("dup1", 100), descriptor_json("dup2", 200)];
        let server_task = tokio::spawn(async move {
            let _query = read_fake_query(&mut server_read).await;
            write_fake_page_response(&mut server_write, rows).await;
            let _ = server_write.shutdown().await;
        });

        let ingest = ScriptedIngest::new(vec![
            IngestVerdict::AdvanceDuplicate,
            IngestVerdict::AdvanceDuplicate,
        ]);

        let result = run_vine_pull_client_session(
            &mut client_read,
            &mut client_write,
            "creator-dup",
            (0, String::new()),
            &ingest,
            1_700_000_000_000,
            PullProgressSink::default(),
        )
        .await
        .expect("an all-duplicate page must still be a successful session");

        server_task.await.expect("server task must not panic");

        assert_eq!(
            result.cursor,
            (200, "dup2".to_string()),
            "the cursor must still advance past mesh-delivered duplicates"
        );
        assert_eq!(
            result.ingested, 0,
            "a duplicate must never inflate the ingest telemetry counter"
        );
        assert_eq!(result.skipped_invalid, 0);
    }

    // ── ZEB-818: unverified-cursor forward-skew clamp ──

    /// The session clock every skew test runs against. `now_ms` is
    /// MILLISECONDS; a descriptor's `created_at` is SECONDS — the clamp
    /// compares in the seconds domain, so both are pinned here rather than
    /// spelled inline where a factor of 1000 could hide.
    const TEST_NOW_MS: u64 = 1_700_000_000_000;
    const TEST_NOW_SECS: u64 = TEST_NOW_MS / 1000;

    /// The `(after_created_at, after_id)` cursor a query carries — the wire
    /// form of the tuple the row loop produced.
    fn query_cursor(q: &VinePullQuery) -> (u64, &str) {
        (q.after_created_at, q.after_id.as_str())
    }

    /// Pads `tail` out to a page of exactly `VINE_PULL_PAGE_LIMIT_MAX` rows
    /// with ordinary past-dated rows the ingest accepts, returning the rows
    /// and the matching verdict script. The page must be exactly full for
    /// the session to issue a SECOND query — the only place the cursor the
    /// row loop produced becomes observable on the wire.
    fn full_page_with_tail(
        tail: Vec<(Vec<u8>, IngestVerdict)>,
    ) -> (Vec<Vec<u8>>, Vec<IngestVerdict>) {
        let filler = VINE_PULL_PAGE_LIMIT_MAX as usize - tail.len();
        let mut rows: Vec<Vec<u8>> = (0..filler)
            .map(|i| {
                descriptor_json(
                    &format!("fill{i:03}"),
                    TEST_NOW_SECS - filler as u64 + i as u64,
                )
            })
            .collect();
        let mut verdicts = vec![IngestVerdict::Advance; filler];
        for (row, verdict) in tail {
            rows.push(row);
            verdicts.push(verdict);
        }
        assert_eq!(rows.len(), VINE_PULL_PAGE_LIMIT_MAX as usize);
        (rows, verdicts)
    }

    /// Drives one session over duplex: the server answers the first query
    /// with `rows`, captures the SECOND query (the one carrying the cursor
    /// the row loop just produced), then answers it with an empty page so
    /// the session ends. Both halves are under an outer timeout so a
    /// regression fails fast instead of hanging the suite.
    async fn run_session_capturing_second_query(
        rows: Vec<Vec<u8>>,
        verdicts: Vec<IngestVerdict>,
    ) -> (PullSessionResult, VinePullQuery, PullProgressSink) {
        let (client, server) = tokio::io::duplex(1 << 20);
        let (mut client_read, mut client_write) = tokio::io::split(client);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        let server_task = tokio::spawn(async move {
            let _first = read_fake_query(&mut server_read).await;
            write_fake_page_response(&mut server_write, rows).await;
            let second = read_fake_query(&mut server_read).await;
            // Short page ends the session.
            write_fake_page_response(&mut server_write, vec![]).await;
            // `tokio::io::split` shares the stream via an internal Arc — a
            // bare drop does NOT half-close it (see the note at the top of
            // `cursor_advances_past_invalid_but_not_past_halt`).
            let _ = server_write.shutdown().await;
            second
        });

        let ingest = ScriptedIngest::new(verdicts);
        // ZEB-826: kept (Arc clone shares state) so callers can assert the
        // drop-surviving refused/ingested counters the session wrote.
        let sink = PullProgressSink::default();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_vine_pull_client_session(
                &mut client_read,
                &mut client_write,
                "creator-skew",
                (0, String::new()),
                &ingest,
                TEST_NOW_MS,
                sink.clone(),
            ),
        )
        .await
        .expect("session must terminate promptly, not hang")
        .expect("session must not error");

        let second = tokio::time::timeout(std::time::Duration::from_secs(5), server_task)
            .await
            .expect("server task must finish promptly")
            .expect("server task must not panic");

        (result, second, sink)
    }

    /// ZEB-818: an unverifiable row with an implausibly future `created_at`
    /// must not advance the cursor — a hostile relay could otherwise poison
    /// the persisted cursor past all genuine descriptors forever.
    #[tokio::test]
    async fn skip_invalid_refuses_cursor_advance_past_forward_skew() {
        let (rows, verdicts) = full_page_with_tail(vec![
            (descriptor_json("ok", 1_700_000_100), IngestVerdict::Advance),
            (
                descriptor_json("evil", u64::MAX),
                IngestVerdict::SkipInvalid,
            ),
        ]);

        let (result, second_query, sink) = run_session_capturing_second_query(rows, verdicts).await;

        assert_eq!(
            query_cursor(&second_query),
            (1_700_000_100, "ok"),
            "the next query must resume from the last genuine row, never from \
             the poisoned far-future one"
        );
        assert_eq!(result.cursor, (1_700_000_100, "ok".to_string()));
        assert_eq!(result.ingested, VINE_PULL_PAGE_LIMIT_MAX as u32 - 1);
        assert_eq!(
            result.skipped_invalid, 1,
            "the refused row is still counted as skipped-invalid"
        );
        // ZEB-826: the refused row is ALSO counted in the dedicated
        // drop-surviving counter, so a poisoning relay is visible without the
        // debug-level `skipped_invalid` conflation.
        assert_eq!(
            sink.take_refused_forward_skew(),
            1,
            "the refused row must increment refused_forward_skew"
        );
    }

    /// Plausibly-timed invalid rows must STILL advance the cursor
    /// (tombstones, trim victims — refusing them would livelock the driver
    /// on any ordinary invalid region).
    #[tokio::test]
    async fn skip_invalid_within_skew_still_advances() {
        let (rows, verdicts) = full_page_with_tail(vec![(
            descriptor_json("dead", 1_700_000_050),
            IngestVerdict::SkipInvalid,
        )]);

        let (result, second_query, _sink) =
            run_session_capturing_second_query(rows, verdicts).await;

        assert_eq!(
            query_cursor(&second_query),
            (1_700_000_050, "dead"),
            "an ordinary invalid row must still move the cursor past itself"
        );
        assert_eq!(result.cursor, (1_700_000_050, "dead".to_string()));
        assert_eq!(result.skipped_invalid, 1);
    }

    /// Boundary: `created_at == now_secs + SKEW` advances;
    /// `created_at == now_secs + SKEW + 1` is refused.
    #[tokio::test]
    async fn skip_invalid_skew_boundary_is_exact() {
        let just_inside = TEST_NOW_SECS + VINE_PULL_INVALID_FORWARD_SKEW_SECS;
        let (rows, verdicts) = full_page_with_tail(vec![(
            descriptor_json("edge", just_inside),
            IngestVerdict::SkipInvalid,
        )]);
        let (result, second_query, _sink) =
            run_session_capturing_second_query(rows, verdicts).await;
        assert_eq!(
            query_cursor(&second_query),
            (just_inside, "edge"),
            "exactly `SKEW` seconds ahead is still inside the window"
        );
        assert_eq!(result.cursor, (just_inside, "edge".to_string()));

        let just_outside = TEST_NOW_SECS + VINE_PULL_INVALID_FORWARD_SKEW_SECS + 1;
        // Above the filler block's max (`TEST_NOW_SECS - 1`): a real relay
        // page is strictly ascending by `(created_at, id)`, so the anchor
        // must not sort behind the rows preceding it. Dating it below them
        // would make this test assert a BACKWARDS cursor move.
        let anchor_created_at = TEST_NOW_SECS + 1;
        let (rows, verdicts) = full_page_with_tail(vec![
            (
                descriptor_json("anchor", anchor_created_at),
                IngestVerdict::Advance,
            ),
            (
                descriptor_json("edge-plus-one", just_outside),
                IngestVerdict::SkipInvalid,
            ),
        ]);
        let (result, second_query, _sink) =
            run_session_capturing_second_query(rows, verdicts).await;
        assert_eq!(
            query_cursor(&second_query),
            (anchor_created_at, "anchor"),
            "one second past `SKEW` is outside the window and must not advance"
        );
        assert_eq!(result.cursor, (anchor_created_at, "anchor".to_string()));
    }

    /// A page whose rows are ALL refused by the clamp advances nothing, so
    /// the session must end via the zero-cursor-advance guard rather than
    /// re-request the identical page forever. This is a distinct control
    /// flow into that guard from
    /// `full_page_of_unparseable_rows_ends_session_without_looping`: those
    /// rows `continue` before ingest, whereas these parse, reach ingest,
    /// consume a verdict, and fall through the clamp's refusal branch.
    #[tokio::test]
    async fn full_page_of_refused_rows_ends_session_without_looping() {
        let (client, server) = tokio::io::duplex(1 << 20);
        let (mut client_read, mut client_write) = tokio::io::split(client);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        // Strictly ascending, as a real relay page is — and every row far
        // beyond `now_secs + SKEW`.
        let rows: Vec<Vec<u8>> = (0..VINE_PULL_PAGE_LIMIT_MAX)
            .map(|i| {
                descriptor_json(
                    &format!("evil{i:03}"),
                    u64::MAX - VINE_PULL_PAGE_LIMIT_MAX as u64 + i as u64,
                )
            })
            .collect();

        let server_task = tokio::spawn(async move {
            let _query = read_fake_query(&mut server_read).await;
            write_fake_page_response(&mut server_write, rows).await;
            let _ = server_write.shutdown().await;

            // The session must be over: a second query would arrive well
            // within this bound if a fully-refused page looped.
            let second = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                read_fake_query(&mut server_read),
            )
            .await;
            assert!(
                second.is_err(),
                "a fully-refused page must end the session, not re-request itself"
            );
        });

        let ingest = ScriptedIngest::new(vec![
            IngestVerdict::SkipInvalid;
            VINE_PULL_PAGE_LIMIT_MAX as usize
        ]);

        // A non-empty starting cursor, so the assertion below proves the
        // durable tuple is PRESERVED rather than merely still at its default.
        let seed_cursor = (1_699_999_000u64, "seed".to_string());
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_vine_pull_client_session(
                &mut client_read,
                &mut client_write,
                "creator-refused",
                seed_cursor.clone(),
                &ingest,
                TEST_NOW_MS,
                PullProgressSink::default(),
            ),
        )
        .await
        .expect("session must terminate promptly, not loop forever")
        .expect("session must not error");

        server_task.await.expect("server task must not panic");

        assert_eq!(
            result.cursor, seed_cursor,
            "no refused row may move the cursor off the last durable tuple"
        );
        assert_eq!(result.ingested, 0);
        assert_eq!(result.skipped_invalid, VINE_PULL_PAGE_LIMIT_MAX as u32);
    }

    // ── ZEB-819: page-boundary cursor progress sink ──

    /// The sink only ever moves forward: a stale commit (an earlier
    /// candidate relay that got further than a later one) can never regress
    /// progress, and `take()` clears the slot.
    #[test]
    fn progress_sink_is_monotone() {
        let s = PullProgressSink::default();
        s.commit((10, "b".into()));
        s.commit((5, "a".into()));
        assert_eq!(s.take(), Some((10, "b".to_string())));
        assert_eq!(s.take(), None, "take() clears the slot");
    }

    /// ZEB-819: the IO deadline dropping the session future mid-page must
    /// not discard the pages that already completed. The session commits at
    /// every page boundary, so the caller-owned sink still holds page 1's
    /// last tuple even though the future was never polled to completion and
    /// no `PullSessionResult` was ever returned.
    ///
    /// `start_paused` puts the deadline on LOGICAL time: the clock only
    /// jumps when every task is parked, so the 200ms budget can never
    /// expire while the client is still working through page 1 — the one
    /// way a wall-clock budget could make this test flaky on a loaded host.
    #[tokio::test(start_paused = true)]
    async fn deadline_drop_preserves_page_boundary_progress() {
        let (client, server) = tokio::io::duplex(1 << 20);
        let (mut client_read, mut client_write) = tokio::io::split(client);
        let (mut server_read, mut server_write) = tokio::io::split(server);

        // Page 1 must be exactly FULL: a short page ends the session
        // normally, and then there is no second query to strand.
        let (rows, verdicts) = full_page_with_tail(Vec::new());
        let last: CursorFields = serde_json::from_slice(rows.last().expect("a full page"))
            .expect("the helper's filler rows parse");
        let page_one_end = (last.created_at, last.id);

        let server_task = tokio::spawn(async move {
            let _first = read_fake_query(&mut server_read).await;
            write_fake_page_response(&mut server_write, rows).await;
            // The second query is deliberately NEVER answered, and both
            // duplex halves stay alive so the client blocks on the read
            // instead of seeing EOF — EOF would be an ordinary `Err`
            // return, not the dropped-future path under test.
            std::future::pending::<()>().await;
        });

        let ingest = ScriptedIngest::new(verdicts);
        let sink = PullProgressSink::default();

        let dropped = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            run_vine_pull_client_session(
                &mut client_read,
                &mut client_write,
                "creator-deadline",
                (0, String::new()),
                &ingest,
                TEST_NOW_MS,
                sink.clone(),
            ),
        )
        .await;

        assert!(
            dropped.is_err(),
            "the session must still be blocked on the unanswered second query"
        );
        assert_eq!(
            sink.take(),
            Some(page_one_end),
            "the completed first page's cursor must survive the dropped future"
        );

        server_task.abort();
    }

    #[test]
    fn sidecar_round_trip_and_corrupt_file_loads_empty() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("vine_pull.cbor");

        let sidecar = VinePullSidecar {
            per_creator: BTreeMap::from([(
                "addr1".to_string(),
                CreatorPullState {
                    cursor: (42, "v1".to_string()),
                    last_pull_attempt_ms: 100,
                    consecutive_skips: 2,
                    relay_set: vec![VineRelayEntry {
                        iroh_endpoint_id: [9u8; 32],
                        home_relay: "https://r".to_string(),
                    }],
                    relays_fetched_at_ms: 50,
                },
            )]),
        };
        save_vine_pull(&path, &sidecar).expect("save");
        assert_eq!(load_vine_pull(&path), sidecar);

        // A truncated/garbage file must load as an empty default, not error.
        std::fs::write(&path, [0xFFu8, 0x00]).expect("write garbage");
        assert_eq!(load_vine_pull(&path), VinePullSidecar::default());

        // A missing file must also load as an empty default.
        let missing = dir.path().join("does_not_exist.cbor");
        assert_eq!(load_vine_pull(&missing), VinePullSidecar::default());
    }

    // ── Driver-level tests (mock transport + mock ingest) ──

    /// One recorded `pull_pages` call: the relay dialed, the creator, and
    /// the cursor it was invoked with. Named alias so the field/method
    /// types stay readable and clippy's `type_complexity` lint (which
    /// otherwise fires on the raw nested-tuple-in-`Vec`-in-`Mutex` shape)
    /// has nothing to flag.
    type PullCall = (VineRelayEntry, String, (u64, String));

    /// ZEB-819: one scripted page-boundary commit — the cursor a single
    /// `pull_pages` call publishes to the sink before returning, or `None`
    /// for a call that commits nothing. Aliased so the queue's type stays
    /// readable (and clippy's `type_complexity` has nothing to flag).
    type ScriptedCommit = Option<(u64, String)>;

    #[derive(Default)]
    struct MockTransport {
        calls: std::sync::Mutex<Vec<PullCall>>,
        script: std::sync::Mutex<VecDeque<Result<PullSessionResult, String>>>,
        /// ZEB-819: page-boundary progress this mock commits into the
        /// caller's sink BEFORE returning each scripted result — the mock's
        /// stand-in for a real session that finished pages and only then hit
        /// the IO deadline. Positionally aligned with `script`; an exhausted
        /// queue commits nothing, so tests that don't care omit it and drive
        /// the identical code path they did before ZEB-819.
        commits: std::sync::Mutex<VecDeque<ScriptedCommit>>,
        /// ZEB-826: rows this mock adds to the caller's sink BEFORE returning
        /// each scripted result — the mock's stand-in for a session that
        /// backfilled N rows into the durable store (and the drop-surviving
        /// sink) before its outcome, including an `Err` whose real session
        /// would carry no count. Positionally aligned with `script`; an
        /// exhausted queue adds nothing.
        ingesting: std::sync::Mutex<VecDeque<u32>>,
    }

    impl MockTransport {
        fn with_results(results: Vec<Result<PullSessionResult, String>>) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                script: std::sync::Mutex::new(results.into()),
                commits: std::sync::Mutex::new(VecDeque::new()),
                ingesting: std::sync::Mutex::new(VecDeque::new()),
            }
        }

        /// Script one page-boundary commit PER CALL, aligned with
        /// `with_results`' outcomes: entry *i* lands in the caller's sink
        /// just before outcome *i* is returned. Per-call rather than a
        /// single shared value on purpose — it is what lets a test attribute
        /// committed progress to one specific candidate relay.
        fn committing(mut self, commits: Vec<ScriptedCommit>) -> Self {
            self.commits = std::sync::Mutex::new(commits.into());
            self
        }

        /// ZEB-826: script one `add_ingested(n)` PER CALL, aligned with
        /// `with_results` — entry *i* lands in the caller's sink just before
        /// outcome *i* returns. Lets a test attribute rescued ingests to a
        /// specific candidate (notably one whose scripted outcome is `Err`).
        fn ingesting(mut self, ingests: Vec<u32>) -> Self {
            self.ingesting = std::sync::Mutex::new(ingests.into());
            self
        }

        fn calls(&self) -> Vec<PullCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl VinePullTransport for MockTransport {
        async fn pull_pages(
            &self,
            relay: &VineRelayEntry,
            creator: &str,
            cursor: (u64, String),
            _ingest: &dyn VineIngestCtx,
            progress: PullProgressSink,
        ) -> Result<PullSessionResult, String> {
            self.calls
                .lock()
                .unwrap()
                .push((relay.clone(), creator.to_string(), cursor.clone()));
            if let Some(Some(committed)) = self.commits.lock().unwrap().pop_front() {
                progress.commit(committed);
            }
            if let Some(n) = self.ingesting.lock().unwrap().pop_front() {
                progress.add_ingested(n);
            }
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(PullSessionResult {
                    cursor,
                    ingested: 0,
                    skipped_invalid: 0,
                }))
        }
    }

    struct StubIngest;

    impl VineIngestCtx for StubIngest {
        fn ingest_descriptor(
            &self,
            _creator: &str,
            _json_bytes: &[u8],
            _now_ms: u64,
        ) -> IngestVerdict {
            IngestVerdict::Advance
        }
    }

    /// A `PkarrResolver` wired to an empty relay pool. Any test that keeps
    /// the sidecar's resolve cooldown active never actually calls it, so
    /// this avoids spinning up a mock pkarr relay server in every test —
    /// only `resolve_cooldown_uses_cached_relay_hint` needs a live one.
    fn inert_pkarr_resolver() -> Arc<harmony_pkarr::PkarrResolver> {
        Arc::new(harmony_pkarr::PkarrResolver::new(Arc::new(
            harmony_pkarr::RelayClient::new(harmony_pkarr::RelayPool::new(Vec::new())),
        )))
    }

    fn temp_sidecar_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("vine_pull.cbor")
    }

    #[tokio::test]
    async fn first_follow_always_pulls_and_backfills() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sidecar_path = temp_sidecar_path(&dir);
        let creator = "aa".repeat(16);
        let now = 1_700_000_000_000u64;
        let relay = VineRelayEntry {
            iroh_endpoint_id: [0x11; 32],
            home_relay: String::new(),
        };

        // Relays already resolved recently (cooldown active, so this pass
        // never touches the resolver) — genesis cursor, so this is a
        // creator that's never had a successful pull yet.
        let seeded = VinePullSidecar {
            per_creator: BTreeMap::from([(
                creator.clone(),
                CreatorPullState {
                    cursor: (0, String::new()),
                    last_pull_attempt_ms: 0,
                    consecutive_skips: 0,
                    relay_set: vec![relay.clone()],
                    relays_fetched_at_ms: now,
                },
            )]),
        };
        save_vine_pull(&sidecar_path, &seeded).expect("seed sidecar");

        let transport = Arc::new(MockTransport::with_results(vec![Ok(PullSessionResult {
            cursor: (now, "v1".to_string()),
            ingested: 1,
            skipped_invalid: 0,
        })]));
        let followed = creator.clone();
        let followed_fn: FollowedCreatorsFn = Arc::new(move || vec![followed.clone()]);
        // Mesh recency looks fresher than last_pull_attempt_ms (0) — this
        // WOULD trigger a skip if first-follow didn't override it.
        let last_received: LastReceivedMsFn = Arc::new(|_| Some(999_999_999));

        let driver = VinePullDriver::new(
            [0xFF; 32],
            inert_pkarr_resolver(),
            transport.clone(),
            Arc::new(StubIngest),
            followed_fn,
            last_received,
            sidecar_path.clone(),
        );

        driver.run_one_pass(now).await;

        assert_eq!(
            transport.calls().len(),
            1,
            "first follow must always pull, never skip"
        );
        let st = load_vine_pull(&sidecar_path)
            .per_creator
            .get(&creator)
            .cloned()
            .expect("creator state must persist");
        assert_eq!(st.cursor, (now, "v1".to_string()));
    }

    #[tokio::test]
    async fn mesh_live_skip_is_bounded_at_four() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sidecar_path = temp_sidecar_path(&dir);
        let creator = "bb".repeat(16);
        let now0 = 1_700_000_000_000u64;
        let relay = VineRelayEntry {
            iroh_endpoint_id: [0x22; 32],
            home_relay: String::new(),
        };

        let seeded = VinePullSidecar {
            per_creator: BTreeMap::from([(
                creator.clone(),
                CreatorPullState {
                    cursor: (500, "seed".to_string()), // NOT first-follow
                    last_pull_attempt_ms: now0,
                    consecutive_skips: 0,
                    relay_set: vec![relay.clone()],
                    relays_fetched_at_ms: now0,
                },
            )]),
        };
        save_vine_pull(&sidecar_path, &seeded).expect("seed sidecar");

        let transport = Arc::new(MockTransport::with_results(vec![Ok(PullSessionResult {
            cursor: (now0 + 100, "repair".to_string()),
            ingested: 1,
            skipped_invalid: 0,
        })]));
        let followed = creator.clone();
        let followed_fn: FollowedCreatorsFn = Arc::new(move || vec![followed.clone()]);
        // Mesh always looks fresher than last_pull_attempt_ms, which stays
        // pinned at now0 across every skipped pass.
        let last_received: LastReceivedMsFn = Arc::new(move |_| Some(now0 + 1));

        let driver = VinePullDriver::new(
            [0xFF; 32],
            inert_pkarr_resolver(),
            transport.clone(),
            Arc::new(StubIngest),
            followed_fn,
            last_received,
            sidecar_path.clone(),
        );

        for i in 1..=4u64 {
            driver.run_one_pass(now0 + 10 * i).await;
            assert!(
                transport.calls().is_empty(),
                "skip {i} must not dial the relay"
            );
        }
        driver.run_one_pass(now0 + 1000).await;
        assert_eq!(
            transport.calls().len(),
            1,
            "the 5th pass must force a repair pull"
        );

        let st = load_vine_pull(&sidecar_path)
            .per_creator
            .get(&creator)
            .cloned()
            .expect("creator state must persist");
        assert_eq!(
            st.consecutive_skips, 0,
            "a successful repair pull resets the skip counter"
        );
    }

    #[tokio::test]
    async fn self_relay_entry_is_never_dialed() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sidecar_path = temp_sidecar_path(&dir);
        let creator = "cc".repeat(16);
        let now = 1_700_000_000_000u64;
        let self_ep = [0xAA; 32];
        let mine = VineRelayEntry {
            iroh_endpoint_id: self_ep,
            home_relay: "https://self".to_string(),
        };
        let other = VineRelayEntry {
            iroh_endpoint_id: [0xBB; 32],
            home_relay: "https://other".to_string(),
        };

        let seeded = VinePullSidecar {
            per_creator: BTreeMap::from([(
                creator.clone(),
                CreatorPullState {
                    cursor: (0, String::new()), // first follow: always pulls
                    last_pull_attempt_ms: 0,
                    consecutive_skips: 0,
                    relay_set: vec![mine.clone(), other.clone()],
                    relays_fetched_at_ms: now,
                },
            )]),
        };
        save_vine_pull(&sidecar_path, &seeded).expect("seed sidecar");

        let transport = Arc::new(MockTransport::with_results(vec![Ok(PullSessionResult {
            cursor: (now, "v".to_string()),
            ingested: 0,
            skipped_invalid: 0,
        })]));
        let followed = creator.clone();
        let followed_fn: FollowedCreatorsFn = Arc::new(move || vec![followed.clone()]);
        let last_received: LastReceivedMsFn = Arc::new(|_| None);

        let driver = VinePullDriver::new(
            self_ep,
            inert_pkarr_resolver(),
            transport.clone(),
            Arc::new(StubIngest),
            followed_fn,
            last_received,
            sidecar_path,
        );

        driver.run_one_pass(now).await;

        let calls = transport.calls();
        assert_eq!(calls.len(), 1, "exactly one relay must be dialed");
        assert_eq!(
            calls[0].0.iroh_endpoint_id, other.iroh_endpoint_id,
            "self relay entry must be filtered before dialing"
        );
    }

    #[tokio::test]
    async fn relay_failover_tries_next_candidate_on_failure() {
        // ZEB-811 review fix round 1: only the first relay was ever dialed
        // per pass — a dead head-of-list relay would block the creator
        // indefinitely. This proves the pass fails over to the next
        // candidate within the SAME pass, mirroring
        // `fetch_vine_video_impl`'s multi-relay iteration (`lib.rs`).
        let dir = tempfile::tempdir().expect("create tempdir");
        let sidecar_path = temp_sidecar_path(&dir);
        let creator = "dd".repeat(16);
        let now = 1_700_000_000_000u64;
        let dead = VineRelayEntry {
            iroh_endpoint_id: [0x44; 32],
            home_relay: "https://dead".to_string(),
        };
        let alive = VineRelayEntry {
            iroh_endpoint_id: [0x55; 32],
            home_relay: "https://alive".to_string(),
        };

        let seeded = VinePullSidecar {
            per_creator: BTreeMap::from([(
                creator.clone(),
                CreatorPullState {
                    cursor: (0, String::new()), // first follow: always pulls
                    last_pull_attempt_ms: 0,
                    consecutive_skips: 0,
                    relay_set: vec![dead.clone(), alive.clone()],
                    relays_fetched_at_ms: now,
                },
            )]),
        };
        save_vine_pull(&sidecar_path, &seeded).expect("seed sidecar");

        // First candidate (dead) fails; the pass must fail over to the
        // second (alive) rather than giving up after index 0.
        let transport = Arc::new(MockTransport::with_results(vec![
            Err("connection refused".to_string()),
            Ok(PullSessionResult {
                cursor: (now, "v".to_string()),
                ingested: 1,
                skipped_invalid: 0,
            }),
        ]));
        let followed = creator.clone();
        let followed_fn: FollowedCreatorsFn = Arc::new(move || vec![followed.clone()]);
        let last_received: LastReceivedMsFn = Arc::new(|_| None);

        let driver = VinePullDriver::new(
            [0xFF; 32],
            inert_pkarr_resolver(),
            transport.clone(),
            Arc::new(StubIngest),
            followed_fn,
            last_received,
            sidecar_path.clone(),
        );

        driver.run_one_pass(now).await;

        let calls = transport.calls();
        assert_eq!(
            calls.len(),
            2,
            "both candidates must be dialed: the dead one, then the failover"
        );
        assert_eq!(calls[0].0.iroh_endpoint_id, dead.iroh_endpoint_id);
        assert_eq!(calls[1].0.iroh_endpoint_id, alive.iroh_endpoint_id);

        let st = load_vine_pull(&sidecar_path)
            .per_creator
            .get(&creator)
            .cloned()
            .expect("creator state must persist");
        assert_eq!(
            st.cursor,
            (now, "v".to_string()),
            "cursor must reflect the SUCCESSFUL relay's result"
        );
    }

    #[tokio::test]
    async fn resolve_cooldown_uses_cached_relay_hint() {
        // Nothing published for this creator, so a live resolve attempt
        // would error — proving the cached hint wins on its own merits,
        // not merely because resolve was skipped by luck.
        let relay_srv = harmony_pkarr::testing::MockPkarrRelay::start().await;
        let pool = harmony_pkarr::RelayPool::new(vec![relay_srv.base_url.clone()]);
        let client = Arc::new(harmony_pkarr::RelayClient::new(pool));
        let resolver = Arc::new(harmony_pkarr::PkarrResolver::new(client));

        let dir = tempfile::tempdir().expect("create tempdir");
        let sidecar_path = temp_sidecar_path(&dir);
        let creator = "ee".repeat(16); // valid hex address
        let now = 1_700_000_000_000u64;
        let cached_relay = VineRelayEntry {
            iroh_endpoint_id: [0x33; 32],
            home_relay: "https://cached".to_string(),
        };

        let seeded = VinePullSidecar {
            per_creator: BTreeMap::from([(
                creator.clone(),
                CreatorPullState {
                    cursor: (0, String::new()),
                    last_pull_attempt_ms: 0,
                    consecutive_skips: 0,
                    relay_set: vec![cached_relay.clone()],
                    relays_fetched_at_ms: 0, // outside cooldown: resolve WILL be attempted
                },
            )]),
        };
        save_vine_pull(&sidecar_path, &seeded).expect("seed sidecar");

        let transport = Arc::new(MockTransport::with_results(vec![Ok(PullSessionResult {
            cursor: (now, "v".to_string()),
            ingested: 0,
            skipped_invalid: 0,
        })]));
        let followed = creator.clone();
        let followed_fn: FollowedCreatorsFn = Arc::new(move || vec![followed.clone()]);
        let last_received: LastReceivedMsFn = Arc::new(|_| None);

        let driver = VinePullDriver::new(
            [0; 32],
            resolver,
            transport.clone(),
            Arc::new(StubIngest),
            followed_fn,
            last_received,
            sidecar_path.clone(),
        );

        driver.run_one_pass(now).await;

        let calls = transport.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].0, cached_relay,
            "a failed resolve must fall back to the cached relay hint"
        );

        let st = load_vine_pull(&sidecar_path)
            .per_creator
            .get(&creator)
            .cloned()
            .expect("creator state must persist");
        assert_eq!(
            st.relay_set,
            vec![cached_relay],
            "the cached relay_set must be preserved (not wiped) on a resolve error"
        );
    }

    #[tokio::test]
    async fn unfollowed_creator_state_is_pruned() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sidecar_path = temp_sidecar_path(&dir);
        let stale_creator = "dd".repeat(16);
        let now = 1_700_000_000_000u64;

        let seeded = VinePullSidecar {
            per_creator: BTreeMap::from([(stale_creator.clone(), CreatorPullState::default())]),
        };
        save_vine_pull(&sidecar_path, &seeded).expect("seed sidecar");

        let transport = Arc::new(MockTransport::with_results(Vec::new()));
        let followed_fn: FollowedCreatorsFn = Arc::new(Vec::new);
        let last_received: LastReceivedMsFn = Arc::new(|_| None);

        let driver = VinePullDriver::new(
            [0; 32],
            inert_pkarr_resolver(),
            transport.clone(),
            Arc::new(StubIngest),
            followed_fn,
            last_received,
            sidecar_path.clone(),
        );

        driver.run_one_pass(now).await;

        assert!(
            transport.calls().is_empty(),
            "an unfollowed creator must never be dialed"
        );
        let loaded = load_vine_pull(&sidecar_path);
        assert!(
            loaded.per_creator.is_empty(),
            "the unfollowed creator's state must be pruned"
        );
    }

    #[tokio::test]
    async fn telemetry_pass_counter_beats_before_target_read() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sidecar_path = temp_sidecar_path(&dir);

        let transport = Arc::new(MockTransport::with_results(Vec::new()));
        let followed_fn: FollowedCreatorsFn = Arc::new(Vec::new);
        let last_received: LastReceivedMsFn = Arc::new(|_| None);
        let telemetry = Arc::new(crate::network_health::VinePullTelemetry::new());

        let driver = VinePullDriver::new(
            [0; 32],
            inert_pkarr_resolver(),
            transport,
            Arc::new(StubIngest),
            followed_fn,
            last_received,
            sidecar_path,
        )
        .with_telemetry(Arc::clone(&telemetry));

        driver.run_one_pass(1_700_000_000_000).await;
        driver.run_one_pass(1_700_000_000_001).await;

        let s = telemetry.summary();
        assert_eq!(
            s.passes_run, 2,
            "the loop must prove liveness even with zero followed creators"
        );
        assert!(s.last_pass_ms.is_some());
        assert_eq!(s.sessions_ok, 0);
        assert_eq!(s.sessions_failed, 0);
        assert_eq!(
            s.passes_no_relay, 0,
            "no followed creators ⇒ no no-relay rows"
        );
    }

    #[tokio::test]
    async fn cached_relays_for_reads_the_sidecar_hint_raw() {
        // ZEB-811 Task 9: the accessor must return exactly what the sidecar
        // holds — including a self-entry, since self-filtering is the
        // caller's job (`plan_video_fetch`), not this accessor's.
        let dir = tempfile::tempdir().expect("create tempdir");
        let sidecar_path = temp_sidecar_path(&dir);
        let creator = "ff".repeat(16);
        let relays = vec![
            VineRelayEntry {
                iroh_endpoint_id: [0x11; 32],
                home_relay: "https://one".to_string(),
            },
            VineRelayEntry {
                iroh_endpoint_id: [0x22; 32],
                home_relay: "https://two".to_string(),
            },
        ];
        let seeded = VinePullSidecar {
            per_creator: BTreeMap::from([(
                creator.clone(),
                CreatorPullState {
                    cursor: (0, String::new()),
                    last_pull_attempt_ms: 0,
                    consecutive_skips: 0,
                    relay_set: relays.clone(),
                    relays_fetched_at_ms: 0,
                },
            )]),
        };
        save_vine_pull(&sidecar_path, &seeded).expect("seed sidecar");

        let driver = VinePullDriver::new(
            [0; 32],
            inert_pkarr_resolver(),
            Arc::new(MockTransport::with_results(Vec::new())),
            Arc::new(StubIngest),
            Arc::new(Vec::new),
            Arc::new(|_| None),
            sidecar_path,
        );

        assert_eq!(driver.cached_relays_for(&creator), relays);
        assert_eq!(
            driver.cached_relays_for("never-followed"),
            Vec::new(),
            "an unknown creator must read as an empty hint, not panic"
        );
    }

    /// ZEB-819: a session that completed pages and only THEN failed (IO
    /// deadline, dropped connection) returns no `PullSessionResult` at all,
    /// so without the sink merge on the `Err` arm those pages are silently
    /// re-downloaded on every subsequent pass. The driver merges the sink,
    /// so the persisted cursor advances to the last committed page boundary.
    #[tokio::test]
    async fn failed_session_persists_committed_page_progress() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sidecar_path = temp_sidecar_path(&dir);
        let creator = "ab".repeat(16);
        let now = 1_700_000_000_000u64;
        let relay = VineRelayEntry {
            iroh_endpoint_id: [0x66; 32],
            home_relay: "https://slow".to_string(),
        };

        let seeded = VinePullSidecar {
            per_creator: BTreeMap::from([(
                creator.clone(),
                CreatorPullState {
                    cursor: (0, String::new()), // first follow: always pulls
                    last_pull_attempt_ms: 0,
                    consecutive_skips: 0,
                    relay_set: vec![relay.clone()],
                    relays_fetched_at_ms: now, // cooldown active: no resolve
                },
            )]),
        };
        save_vine_pull(&sidecar_path, &seeded).expect("seed sidecar");

        // Pages 1..n landed durably (committed), then the session died.
        let transport = Arc::new(
            MockTransport::with_results(vec![Err("vine pull IO timeout".to_string())])
                .committing(vec![Some((7, "g".to_string()))]),
        );
        let followed = creator.clone();
        let followed_fn: FollowedCreatorsFn = Arc::new(move || vec![followed.clone()]);
        let last_received: LastReceivedMsFn = Arc::new(|_| None);

        let driver = VinePullDriver::new(
            [0xFF; 32],
            inert_pkarr_resolver(),
            transport.clone(),
            Arc::new(StubIngest),
            followed_fn,
            last_received,
            sidecar_path.clone(),
        );

        driver.run_one_pass(now).await;

        assert_eq!(transport.calls().len(), 1, "the one candidate is dialed");
        let st = load_vine_pull(&sidecar_path)
            .per_creator
            .get(&creator)
            .cloned()
            .expect("creator state must persist");
        assert_eq!(
            st.cursor,
            (7, "g".to_string()),
            "a failed session's committed page progress must still be persisted"
        );
    }

    /// ZEB-819, the design's least obvious consequence: ONE sink is shared
    /// across the candidate failover loop and merged as a MAX, so a FAILED
    /// candidate's committed progress outranks a LATER candidate's
    /// successful-but-lower returned cursor.
    ///
    /// Candidate A completes pages (committing `(9,"x")`) and then dies;
    /// candidate B succeeds, but its relay's mirror only reaches `(5,"y")`.
    /// Taking B's result at face value would rewind the durable cursor past
    /// rows THIS VERY PASS already ingested durably. Only A commits here
    /// (the mock's commit script is per-call), so this also pins that the
    /// sink is shared across candidates rather than scoped to one, and that
    /// the merge runs AFTER the loop's `st.cursor = res.cursor` assignment.
    #[tokio::test]
    async fn failover_keeps_the_higher_committed_cursor_over_a_lower_success() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sidecar_path = temp_sidecar_path(&dir);
        let creator = "ba".repeat(16);
        let now = 1_700_000_000_000u64;
        let died_ahead = VineRelayEntry {
            iroh_endpoint_id: [0x99; 32],
            home_relay: "https://ahead-then-died".to_string(),
        };
        let lagging = VineRelayEntry {
            iroh_endpoint_id: [0xAB; 32],
            home_relay: "https://lagging-mirror".to_string(),
        };

        let seeded = VinePullSidecar {
            per_creator: BTreeMap::from([(
                creator.clone(),
                CreatorPullState {
                    cursor: (0, String::new()), // first follow: always pulls
                    last_pull_attempt_ms: 0,
                    consecutive_skips: 0,
                    relay_set: vec![died_ahead.clone(), lagging.clone()],
                    relays_fetched_at_ms: now, // cooldown active: no resolve
                },
            )]),
        };
        save_vine_pull(&sidecar_path, &seeded).expect("seed sidecar");

        let transport = Arc::new(
            MockTransport::with_results(vec![
                Err("vine pull IO timeout".to_string()),
                Ok(PullSessionResult {
                    cursor: (5, "y".to_string()),
                    ingested: 1,
                    skipped_invalid: 0,
                }),
            ])
            .committing(vec![Some((9, "x".to_string())), None]),
        );
        let followed = creator.clone();
        let followed_fn: FollowedCreatorsFn = Arc::new(move || vec![followed.clone()]);
        let last_received: LastReceivedMsFn = Arc::new(|_| None);

        let driver = VinePullDriver::new(
            [0xFF; 32],
            inert_pkarr_resolver(),
            transport.clone(),
            Arc::new(StubIngest),
            followed_fn,
            last_received,
            sidecar_path.clone(),
        );

        driver.run_one_pass(now).await;

        let calls = transport.calls();
        assert_eq!(calls.len(), 2, "the failed candidate must fail over to B");
        assert_eq!(calls[0].0.iroh_endpoint_id, died_ahead.iroh_endpoint_id);
        assert_eq!(calls[1].0.iroh_endpoint_id, lagging.iroh_endpoint_id);

        let st = load_vine_pull(&sidecar_path)
            .per_creator
            .get(&creator)
            .cloned()
            .expect("creator state must persist");
        assert_eq!(
            st.cursor,
            (9, "x".to_string()),
            "the merge is a MAX: a succeeding candidate's LOWER cursor must not \
             rewind progress an earlier failed candidate already committed"
        );
    }

    /// ZEB-826: a candidate that ingested pages then FAILED (its `Err` carries
    /// no count, and a dropped future returns nothing at all) still has those
    /// rows counted toward `descriptors_ingested`, because they rode the
    /// drop-surviving sink out. `record_session_ok` sees only the winning
    /// candidate's ingests; before this fix the failed candidate's were lost,
    /// leaving fleet-health blind on exactly the ZEB-819 failover path.
    #[tokio::test]
    async fn rescued_ingests_from_a_failed_candidate_are_counted() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sidecar_path = temp_sidecar_path(&dir);
        let creator = "ca".repeat(16);
        let now = 1_700_000_000_000u64;
        let died_after_ingesting = VineRelayEntry {
            iroh_endpoint_id: [0x21; 32],
            home_relay: "https://ingested-then-died".to_string(),
        };
        let winner = VineRelayEntry {
            iroh_endpoint_id: [0x22; 32],
            home_relay: "https://the-winner".to_string(),
        };

        let seeded = VinePullSidecar {
            per_creator: BTreeMap::from([(
                creator.clone(),
                CreatorPullState {
                    cursor: (0, String::new()), // first follow: always pulls
                    last_pull_attempt_ms: 0,
                    consecutive_skips: 0,
                    relay_set: vec![died_after_ingesting.clone(), winner.clone()],
                    relays_fetched_at_ms: now, // cooldown active: no resolve
                },
            )]),
        };
        save_vine_pull(&sidecar_path, &seeded).expect("seed sidecar");

        // Candidate A backfilled 5 rows into the sink, then its session failed
        // (the `Err` carries no count). Candidate B succeeded with 3 more.
        let transport = Arc::new(
            MockTransport::with_results(vec![
                Err("vine pull IO timeout".to_string()),
                Ok(PullSessionResult {
                    cursor: (8, "h".to_string()),
                    ingested: 3,
                    skipped_invalid: 0,
                }),
            ])
            .ingesting(vec![5, 3]),
        );
        let followed = creator.clone();
        let followed_fn: FollowedCreatorsFn = Arc::new(move || vec![followed.clone()]);
        let last_received: LastReceivedMsFn = Arc::new(|_| None);
        let telemetry = Arc::new(crate::network_health::VinePullTelemetry::new());

        let driver = VinePullDriver::new(
            [0xFF; 32],
            inert_pkarr_resolver(),
            transport.clone(),
            Arc::new(StubIngest),
            followed_fn,
            last_received,
            sidecar_path.clone(),
        )
        .with_telemetry(Arc::clone(&telemetry));

        driver.run_one_pass(now).await;

        assert_eq!(transport.calls().len(), 2, "A must fail over to winner B");
        let s = telemetry.summary();
        assert_eq!(
            s.descriptors_ingested, 8,
            "the failed candidate's 5 committed ingests must be rescued and \
             added to the winner's 3 — not silently dropped"
        );
        assert_eq!(s.sessions_ok, 1, "only B completed a session");
        assert_eq!(s.sessions_failed, 1, "A's session failed");
    }

    /// ZEB-826 guard: a lone successful candidate's ingests are counted
    /// exactly ONCE. Moving ingest accounting onto the sink must not
    /// double-count the common no-failover path — the merge nets the winner's
    /// own `record_session_ok` contribution out of the sink pass total.
    #[tokio::test]
    async fn single_successful_candidate_counts_ingests_once() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sidecar_path = temp_sidecar_path(&dir);
        let creator = "da".repeat(16);
        let now = 1_700_000_000_000u64;
        let relay = VineRelayEntry {
            iroh_endpoint_id: [0x31; 32],
            home_relay: "https://sole-relay".to_string(),
        };

        let seeded = VinePullSidecar {
            per_creator: BTreeMap::from([(
                creator.clone(),
                CreatorPullState {
                    cursor: (0, String::new()),
                    last_pull_attempt_ms: 0,
                    consecutive_skips: 0,
                    relay_set: vec![relay.clone()],
                    relays_fetched_at_ms: now,
                },
            )]),
        };
        save_vine_pull(&sidecar_path, &seeded).expect("seed sidecar");

        // One candidate: backfills 4 rows into the sink AND returns them as
        // its result — exactly how a real single-candidate pass behaves.
        let transport = Arc::new(
            MockTransport::with_results(vec![Ok(PullSessionResult {
                cursor: (4, "d".to_string()),
                ingested: 4,
                skipped_invalid: 0,
            })])
            .ingesting(vec![4]),
        );
        let followed = creator.clone();
        let followed_fn: FollowedCreatorsFn = Arc::new(move || vec![followed.clone()]);
        let last_received: LastReceivedMsFn = Arc::new(|_| None);
        let telemetry = Arc::new(crate::network_health::VinePullTelemetry::new());

        let driver = VinePullDriver::new(
            [0xFF; 32],
            inert_pkarr_resolver(),
            transport.clone(),
            Arc::new(StubIngest),
            followed_fn,
            last_received,
            sidecar_path.clone(),
        )
        .with_telemetry(Arc::clone(&telemetry));

        driver.run_one_pass(now).await;

        let s = telemetry.summary();
        assert_eq!(
            s.descriptors_ingested, 4,
            "a lone winner's ingests must be counted once, not doubled by the \
             sink pass-total"
        );
    }

    /// A SUCCESSFUL session whose relay handed back a below-cursor final
    /// result (order-blind `Advance` on hostile below-cursor rows) must not
    /// rewind the durable cursor: the driver's success-path assignment is
    /// monotone, same as the sink merge (CodeRabbit PR #564 round 1 — the
    /// plain `st.cursor = res.cursor` overwrite was the one remaining
    /// rewind path).
    #[tokio::test]
    async fn successful_session_below_cursor_result_does_not_rewind_durable_progress() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sidecar_path = temp_sidecar_path(&dir);
        let creator = "ba".repeat(16);
        let now = 1_700_000_000_000u64;
        let relay = VineRelayEntry {
            iroh_endpoint_id: [0xAB; 32],
            home_relay: "https://rewinding-relay".to_string(),
        };

        let seeded = VinePullSidecar {
            per_creator: BTreeMap::from([(
                creator.clone(),
                CreatorPullState {
                    cursor: (1_699_999_000, "durable".to_string()),
                    last_pull_attempt_ms: 0,
                    consecutive_skips: 0,
                    relay_set: vec![relay.clone()],
                    relays_fetched_at_ms: now, // cooldown active: no resolve
                },
            )]),
        };
        save_vine_pull(&sidecar_path, &seeded).expect("seed sidecar");

        let transport = Arc::new(MockTransport::with_results(vec![Ok(PullSessionResult {
            cursor: (5, "rewound".to_string()),
            ingested: 1,
            skipped_invalid: 0,
        })]));
        let followed = creator.clone();
        let followed_fn: FollowedCreatorsFn = Arc::new(move || vec![followed.clone()]);
        let last_received: LastReceivedMsFn = Arc::new(|_| None);

        let driver = VinePullDriver::new(
            [0xFF; 32],
            inert_pkarr_resolver(),
            transport.clone(),
            Arc::new(StubIngest),
            followed_fn,
            last_received,
            sidecar_path.clone(),
        );
        driver.run_one_pass(now).await;

        assert_eq!(
            transport.calls().len(),
            1,
            "the seeded relay must be pulled"
        );
        let st = load_vine_pull(&sidecar_path)
            .per_creator
            .get(&creator)
            .cloned()
            .expect("creator state must persist");
        assert_eq!(
            st.cursor,
            (1_699_999_000, "durable".to_string()),
            "a successful session's below-cursor result must not rewind the \
             durable cursor"
        );
    }
}
