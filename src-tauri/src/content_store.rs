//! Content-addressed storage trait + in-memory stub (ZEB-215 Sub-A Phase 3a)
//! and async-trait migration (Phase 3b).
//!
//! See `docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md`
//! §"ContentStore trait" and `docs/specs/2026-05-01-zeb-215-sub-a-phase3b-content-cas-design.md`.
//!
//! Phase 3a shipped a sync trait + `InMemoryStub`. Phase 3b makes the trait
//! async so the real `RuntimeContentStore` adapter can await network fetches
//! through the harmony-runtime event loop. `InMemoryStub` keeps in-process
//! semantics for unit tests; the new `RuntimeContentStore` (Task 4) wires
//! through the new `cas_op` mpsc channel into `event_loop::run`.

use crate::owner_state_types::ContentId;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

/// Set of community state-root CIDs this node is willing to serve over CAS even
/// though they carry the `encrypted` flag (ZEB-395). Community roots are
/// epoch-key ciphertext shared among members; serving them by CID is safe (see
/// `docs/specs/2026-06-07-zeb-395-community-content-serve-policy-design.md` §3).
/// Private encrypted blobs (DMs, private profiles) are never inserted, so the
/// content-serve queryable keeps refusing them.
///
/// `std::sync::RwLock` (not tokio) is intentional: `allow`/`contains` lock,
/// mutate/read, and drop the guard synchronously — no guard is ever held across
/// an `.await`. The handle is `Clone` (Arc bump) and shared between the
/// production `RuntimeContentStore` (registration) and the content-serve
/// queryable (lookup).
#[derive(Clone, Default)]
pub struct CommunityServeAllowlist(Arc<RwLock<HashSet<ContentId>>>);

impl CommunityServeAllowlist {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a community-root CID serveable. Idempotent. The write guard is held
    /// only across a non-panicking `HashSet::insert`, so poisoning cannot occur
    /// in practice; a poisoned lock is nonetheless handled by skipping the insert
    /// (best-effort, never a panic) rather than by promising later recovery.
    pub fn allow(&self, cid: ContentId) {
        if let Ok(mut g) = self.0.write() {
            g.insert(cid);
        }
    }

    /// True if `cid` is an allowlisted community-root CID. A poisoned lock reads
    /// as "not allowlisted" (fail closed — never serve on a poisoned guard).
    pub fn contains(&self, cid: &ContentId) -> bool {
        self.0.read().map(|g| g.contains(cid)).unwrap_or(false)
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ContentStoreError {
    #[error("content store I/O: {0}")]
    Io(String),
}

#[async_trait]
pub trait ContentStore: Send + Sync {
    async fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError>;
    async fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError>;

    /// Local-only read: returns bytes already present in the local cache, NEVER
    /// triggering a network fetch. The default delegates to `get` (correct for
    /// stores whose `get` is already local-only, e.g. `InMemoryStub`);
    /// `RuntimeContentStore` overrides it with the local-cache-only
    /// `CasOp::GetLocal`. Use this on latency/privacy-sensitive paths that must
    /// NOT emit a network CID lookup on a local miss — e.g. the tunnel send rung
    /// (ZEB-484): the sender's own DM blob is always local, and a miss must fall
    /// back to a bare CidNotify rather than fetch (and leak) an encrypted DM CID
    /// over the network.
    async fn get_local(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError> {
        self.get(cid).await
    }

    /// Like `get`, but with a caller-declared network budget instead of the
    /// store's default. Callers fetching a known-large payload class — the
    /// community / fleet / mint state-root blobs — MUST use this: the default
    /// budget is tuned for small, latency-sensitive reads, and a state root has
    /// not fit inside it since communities grew past a few hundred members
    /// (ZEB-805).
    ///
    /// Deliberately per-call rather than a raised global: `lib.rs` builds ONE
    /// `RuntimeContentStore` and clones the `Arc` to ~10 consumers, so raising
    /// `DEFAULT_FETCH_TIMEOUT_MS` would silently re-tune every latency-sensitive
    /// path that was never part of the incident.
    ///
    /// INVARIANT: `budget` must stay below `fetch_via_zenoh`'s own internal
    /// deadline (`event_loop.rs`), which is the hard backstop — a larger budget
    /// silently becomes that deadline instead. Enforced at compile time by the
    /// `const _: () = assert!(...)` pair beside `STATE_ROOT_FETCH_TIMEOUT_MS`.
    ///
    /// The default body ignores the budget and delegates, which is correct for
    /// stores whose `get` is already local-only (e.g. `InMemoryStub`).
    async fn get_with_budget(
        &self,
        cid: &ContentId,
        budget: std::time::Duration,
    ) -> Result<Option<Vec<u8>>, ContentStoreError> {
        let _ = budget;
        self.get(cid).await
    }

    /// Like `put`, but also marks `cid` serveable to peers over CAS even though
    /// it carries the `encrypted` flag (ZEB-395 community-root sharing). The
    /// default impl is identical to `put`; only `RuntimeContentStore` registers
    /// the CID in its shared `CommunityServeAllowlist`. Callers use this ONLY
    /// for content that is safe to serve to any requester who can name the CID
    /// (community state-root ciphertext) — never for private blobs.
    async fn put_serveable(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
        self.put(cid, blob).await
    }

    /// ZEB-539: after a download validates, allowlist the artifact's whole local
    /// CID subtree for member-to-member re-serve. Sends CasOp::AllowServeSubtree
    /// and returns the number of CIDs allowlisted.
    async fn allow_serve_subtree(&self, root: ContentId) -> Result<usize, ContentStoreError> {
        // default: no-op for stub impls; only the runtime store re-serves.
        let _ = root;
        Ok(0)
    }
}

#[derive(Default)]
pub struct InMemoryStub {
    inner: Mutex<HashMap<ContentId, Vec<u8>>>,
}

#[async_trait]
impl ContentStore for InMemoryStub {
    async fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
        self.inner
            .lock()
            .map_err(|e| ContentStoreError::Io(format!("lock poisoned: {e}")))?
            .insert(cid, blob);
        Ok(())
    }

    async fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError> {
        Ok(self
            .inner
            .lock()
            .map_err(|e| ContentStoreError::Io(format!("lock poisoned: {e}")))?
            .get(cid)
            .cloned())
    }
}

/// Channel-protocol message between `RuntimeContentStore` (in
/// `SyncEngine`'s tokio task) and the harmony-runtime event loop.
///
/// The event loop owns the only `&mut NodeRuntime`, so the adapter
/// can't admit/fetch directly — it sends one of these messages and
/// awaits a oneshot reply. See spec §"Event loop handler" and
/// §"Re-entry" for the full protocol including the second-mpsc-hop
/// admit pattern used by `GetOrFetch` after a successful network GET.
#[derive(Debug)]
pub enum CasOp {
    /// Admit `blob` to the local StorageTier cache under `cid`.
    /// Reply `Ok(())` once `runtime.tick()` has drained the
    /// resulting actions; reply `Err(...)` if the channel layer
    /// itself failed (StorageTier silently drops corrupted bytes —
    /// matches the existing ingest_rx pattern — so callers treat
    /// `Ok(())` as "we tried" rather than as proof of admit).
    PutLocal {
        cid: ContentId,
        blob: Vec<u8>,
        /// ZEB-535: allowlist this CID for member-to-member serve once admitted.
        /// `true` for encrypted-artifact subtree authorization (the sharer's
        /// ingest hop and the fetcher's re-serve hop); `false` for the avatar /
        /// file-vault / GetOrFetch fire-and-forget paths, which never serve
        /// encrypted CIDs (avatars are unencrypted) or never re-serve.
        serveable: bool,
        /// `Some` for synchronous round-trip callers (e.g.
        /// `RuntimeContentStore::put`); `None` for fire-and-forget
        /// admit hops from the spawned-fetch task in `event_loop.rs`'s
        /// `GetOrFetch` arm. The PutLocal handler only replies if
        /// `Some`, avoiding wasted work on already-dropped receivers.
        reply: Option<tokio::sync::oneshot::Sender<Result<(), ContentStoreError>>>,
    },
    /// Cache check, then on miss spawn a Zenoh GET wrapped in
    /// `tokio::time::timeout(timeout, ...)`. On fetch success,
    /// admit via a second `CasOp::PutLocal` hop before replying
    /// `Ok(Some(bytes))`. On timeout: `Ok(None)`. On hard transport
    /// error (zenoh::open failure, malformed key_expr): `Err(...)`.
    ///
    /// `timeout` is the CALLER's budget and is the binding deadline —
    /// `fetch_via_zenoh` has its own, larger internal backstop, so a caller
    /// budget above that one silently becomes it. ZEB-805: `Ok(None)` means
    /// "not fetchable in the time you allowed", NOT "does not exist"; callers
    /// must treat it as retryable rather than terminal.
    GetOrFetch {
        cid: ContentId,
        timeout: std::time::Duration,
        reply: tokio::sync::oneshot::Sender<Result<Option<Vec<u8>>, ContentStoreError>>,
    },
    /// Read-only local-cache lookup: return the bytes held under `cid` in the
    /// StorageTier cache, or `None` on a cache miss. Unlike `GetOrFetch`, this
    /// NEVER triggers a network fetch — it is the lookup path for the
    /// content-serve queryable, which must not recursively fetch while
    /// answering a peer's GET (that would invert the serve relationship and
    /// could deadlock the event loop). The cache only holds bytes that passed
    /// `hash==cid` verification at admit time (StorageTier::verify_cid), so a
    /// `Some(bytes)` reply is already integrity-checked.
    GetLocal {
        cid: ContentId,
        reply: tokio::sync::oneshot::Sender<Option<Vec<u8>>>,
    },
    /// ZEB-539: allowlist an artifact's full local CID subtree for
    /// member-to-member re-serve, AFTER the download has been validated.
    /// Walks the Merkle DAG locally (no network fetch — every node is already
    /// in the local cache post-fetch) starting from `root`, and calls
    /// `serve_allowlist.allow(cid)` for `root` plus every descendant. Reply is
    /// the count of CIDs allowlisted, or an error if `root` is missing locally.
    AllowServeSubtree {
        root: ContentId,
        reply: tokio::sync::oneshot::Sender<Result<usize, ContentStoreError>>,
    },
}

/// Default fetch budget for `RuntimeContentStore::get`. Wraps the Zenoh GET in
/// `tokio::time::timeout`; on miss the caller sees `Ok(None)`.
///
/// Tuned for SMALL, latency-sensitive reads and nothing else. State-root blobs
/// must use [`STATE_ROOT_FETCH_TIMEOUT_MS`] via `get_with_budget` instead.
///
/// ZEB-805 removed a claim that used to live here — that a miss was harmless
/// because "CRDT eventual consistency carries recovery via the next state-root
/// from any peer". It does not. Attempts are independent; what is not
/// independent is their difficulty. State only grows, so the "recovery" blob is
/// LARGER than the one that just failed and is fetched under the SAME budget:
/// the named recovery path is anti-correlated with the failure it is supposed
/// to repair. Do not reintroduce that reasoning to justify dropping on a miss.
pub const DEFAULT_FETCH_TIMEOUT_MS: u64 = 500;

/// Fetch budget for state-root blobs (community / fleet / mint). These are a
/// different payload class from the 500 ms default: ~100 KB and growing with
/// membership, and frequently fetched cross-WAN.
///
/// ZEB-805: the default was set in 2026-05 for small local reads and never
/// revisited while the community-state root grew past 100 KB and the fleet went
/// cross-WAN. A state root has not fit inside 500 ms since; the four blobs
/// dropped in the incident were 101084 B, 101682 B, and 109151 B twice.
///
/// 5 s sits ~3x above a pessimistic real fetch (100 KB at 500 kbps ≈ 1.6 s plus
/// query round-trips at the fleet's worst measured 121 ms RTT), and well below
/// both `fetch_via_zenoh`'s 30 s backstop and the 450 s relay-pull cadence — so
/// a retry chain cannot overlap the next pull pass.
pub const STATE_ROOT_FETCH_TIMEOUT_MS: u64 = 5_000;

/// `fetch_via_zenoh` (`event_loop.rs`) wraps its reply loop in its own 30 s
/// deadline, which is the hard backstop under every caller budget.
const ZENOH_FETCH_BACKSTOP_MS: u64 = 30_000;

// ZEB-805 budget invariants, enforced at COMPILE time rather than by a test —
// these relate two constants, so there is no runtime state that could make them
// true or false, and a build is a stronger gate than a test someone can skip.
//
// The first is the one that bit us: a caller budget at or above the backstop
// silently BECOMES the backstop, so the caller's number stops meaning anything.
// That is precisely the 60x nested-timeout confusion this ticket found — a
// 500 ms outer under a 30 s inner, where the inner was unreachable dead code.
const _: () = assert!(
    STATE_ROOT_FETCH_TIMEOUT_MS < ZENOH_FETCH_BACKSTOP_MS,
    "state-root budget must stay strictly under the fetch_via_zenoh backstop, \
     or the caller's budget silently becomes that backstop"
);
const _: () = assert!(
    STATE_ROOT_FETCH_TIMEOUT_MS > DEFAULT_FETCH_TIMEOUT_MS,
    "the state-root budget exists because the default is too small for this \
     payload class; collapsing them re-creates ZEB-805"
);

/// ZEB-805 (r1): bounded fetch-retry budget for a state-root publish whose CAS
/// fetch missed. Lives here, beside the budget it retries under, because the
/// two are one policy: how this codebase fetches a state-root blob.
///
/// All three sync engines — `community_state_sync`, `fleet_sync`, `mint_sync` —
/// read these. They were three copies of the same literals until a reviewer
/// pointed out the obvious: cross-engine consistency was the stated deliverable
/// of ZEB-805, and three private copies enforce it by good intentions rather
/// than by construction. Tuning one engine in isolation is exactly the drift
/// this prevents; if an engine ever genuinely needs a different budget, give it
/// a distinct named constant here rather than editing this one.
///
/// Values are ZEB-705's, which survived review plus adversarial flood analysis.
pub mod fetch_retry {
    /// Attempts after the initial fetch, per inbound publish.
    ///
    /// The miss is usually transient: a fresh zenoh link delivers the root put
    /// before the publisher's content-queryable declaration has propagated back
    /// (~1 s). Without a retry the publish is dropped permanently — fatal for a
    /// paired butler whose primary (the only blob holder) departs seconds later.
    ///
    /// Retries re-inject the ORIGINAL wire frame through the FULL inbound
    /// pipeline, so the replay check naturally kills a retry that a newer root
    /// has since superseded. That re-injection is admissible only because every
    /// engine's replay check is read-only until a successful apply — community
    /// via ZEB-750's `CommitTicket`, mint via its step-3/step-6 split. Breaking
    /// that invariant would silently turn every retry into a rejected duplicate.
    pub const ATTEMPTS: u8 = 3;

    /// Delay between retries. Comfortably above declaration-propagation latency
    /// on a fresh link, and small enough that the whole chain
    /// (3 x (5 s budget + 2 s delay) ~= 21 s worst case) lands far inside the
    /// 450 s relay-pull cadence, so retries can never overlap the next pass.
    pub const DELAY_MS: u64 = 2000;

    /// Max retry sleepers in flight per engine, and the re-injection channel's
    /// capacity. Each pending retry is a detached task retaining its wire frame
    /// for [`DELAY_MS`]; a burst of misses — or a hostile publish flood — must
    /// not pile those up unbounded. A permit is acquired BEFORE the sleeper is
    /// spawned and held for its whole lifetime, so concurrent retries (and thus
    /// retained wire buffers) are hard-capped.
    ///
    /// Excess misses are dropped and COUNTED — at both drop sites, permit
    /// exhaustion and a full re-injection channel. An uncounted drop on the
    /// surface built to diagnose stalls is the ZEB-805 failure mode in
    /// miniature.
    pub const MAX_INFLIGHT: usize = 8;
}

/// Production `ContentStore` impl that delegates to the harmony-runtime
/// event loop via `cas_op_tx`. Used at SyncEngine construction in
/// `lib.rs::start_node` (Task 8); tests still use `InMemoryStub` for
/// in-process flows.
pub struct RuntimeContentStore {
    cas_op_tx: tokio::sync::mpsc::Sender<CasOp>,
    fetch_timeout: std::time::Duration,
    /// ZEB-395: when set, `put_serveable` records the put CID here so the
    /// content-serve queryable will serve it despite the `encrypted` flag.
    /// `None` for the legacy/test constructions that don't serve community
    /// roots. Shared (Arc clone) with `event_loop::run`'s serve queryable.
    serve_allowlist: Option<CommunityServeAllowlist>,
}

impl RuntimeContentStore {
    pub fn new(
        cas_op_tx: tokio::sync::mpsc::Sender<CasOp>,
        fetch_timeout: std::time::Duration,
    ) -> Self {
        Self {
            cas_op_tx,
            fetch_timeout,
            serve_allowlist: None,
        }
    }

    /// ZEB-395: attach the shared serve-allowlist so `put_serveable` registers
    /// community-root CIDs. Chained builder so the existing
    /// `RuntimeContentStore::new(...)` call sites stay untouched.
    pub fn with_serve_allowlist(mut self, allowlist: CommunityServeAllowlist) -> Self {
        self.serve_allowlist = Some(allowlist);
        self
    }
}

#[async_trait]
impl ContentStore for RuntimeContentStore {
    async fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cas_op_tx
            .send(CasOp::PutLocal {
                cid,
                blob,
                serveable: false,
                reply: Some(reply_tx),
            })
            .await
            .map_err(|_| ContentStoreError::Io("event loop unavailable (send)".into()))?;
        reply_rx
            .await
            .map_err(|_| ContentStoreError::Io("event loop unavailable (reply)".into()))?
    }

    async fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cas_op_tx
            .send(CasOp::GetOrFetch {
                cid: *cid,
                timeout: self.fetch_timeout,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ContentStoreError::Io("event loop unavailable (send)".into()))?;
        reply_rx
            .await
            .map_err(|_| ContentStoreError::Io("event loop unavailable (reply)".into()))?
    }

    async fn get_with_budget(
        &self,
        cid: &ContentId,
        budget: std::time::Duration,
    ) -> Result<Option<Vec<u8>>, ContentStoreError> {
        // Identical to `get` except the timeout source: the caller's budget
        // rather than `self.fetch_timeout`. See the trait doc for why this is
        // per-call and not a raised global (ZEB-805).
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cas_op_tx
            .send(CasOp::GetOrFetch {
                cid: *cid,
                timeout: budget,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ContentStoreError::Io("event loop unavailable (send)".into()))?;
        reply_rx
            .await
            .map_err(|_| ContentStoreError::Io("event loop unavailable (reply)".into()))?
    }

    async fn get_local(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError> {
        // ZEB-484: local-cache-only — NEVER a network fetch (unlike `get`'s
        // `GetOrFetch`). `CasOp::GetLocal` replies the cached bytes (already
        // integrity-checked at admit) or `None` on a miss; it never spawns a
        // Zenoh GET, so a caller on the send path can't be turned into a
        // blocking/leaky network lookup of an encrypted DM CID.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cas_op_tx
            .send(CasOp::GetLocal {
                cid: *cid,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ContentStoreError::Io("event loop unavailable (send)".into()))?;
        reply_rx
            .await
            .map_err(|_| ContentStoreError::Io("event loop unavailable (reply)".into()))
    }

    async fn put_serveable(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
        // Admit first; only record as serveable after a successful put.
        // ZEB-535: send `serveable: true` so the event-loop PutLocal arm
        // allowlists the CID even when this store has no local `serve_allowlist`
        // (e.g. legacy/test constructions). The direct `allowlist.allow(cid)`
        // below stays for the SyncEngine path that DOES hold a `serve_allowlist`
        // (community-root sharing) — both writes target the same shared set.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cas_op_tx
            .send(CasOp::PutLocal {
                cid,
                blob,
                serveable: true,
                reply: Some(reply_tx),
            })
            .await
            .map_err(|_| ContentStoreError::Io("event loop unavailable (send)".into()))?;
        reply_rx
            .await
            .map_err(|_| ContentStoreError::Io("event loop unavailable (reply)".into()))??;
        if let Some(allowlist) = &self.serve_allowlist {
            allowlist.allow(cid);
        }
        Ok(())
    }

    async fn allow_serve_subtree(&self, root: ContentId) -> Result<usize, ContentStoreError> {
        // ZEB-539: hand the local DAG walk to the event loop (it owns the
        // StorageTier cache + the serve_allowlist). Mirrors `put` /
        // `put_serveable`'s send-then-await-oneshot shape.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cas_op_tx
            .send(CasOp::AllowServeSubtree {
                root,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ContentStoreError::Io("event loop unavailable (send)".into()))?;
        reply_rx
            .await
            .map_err(|_| ContentStoreError::Io("event loop unavailable (reply)".into()))?
    }
}

#[cfg(any(test, feature = "test-fixtures"))]
impl InMemoryStub {
    pub async fn debug_count(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub async fn debug_all_cids(&self) -> Vec<crate::owner_state_types::ContentId> {
        self.inner.lock().unwrap().keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(byte: u8) -> ContentId {
        ContentId::from_bytes([byte; 32])
    }

    #[tokio::test]
    async fn put_then_get_returns_blob() {
        let store = InMemoryStub::default();
        store.put(cid(1), vec![10, 20, 30]).await.unwrap();
        let blob = store.get(&cid(1)).await.unwrap().expect("blob present");
        assert_eq!(blob, vec![10, 20, 30]);
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let store = InMemoryStub::default();
        assert!(store.get(&cid(99)).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn runtime_get_local_routes_getlocal_and_returns_cached_blob() {
        // ZEB-484: RuntimeContentStore::get_local must send the local-cache-only
        // `CasOp::GetLocal` (NEVER `GetOrFetch`) and return its reply. Simulate the
        // event loop receiving exactly one GetLocal for the asked CID and replying
        // with cached bytes; assert get_local plumbs them through.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<CasOp>(4);
        let store = RuntimeContentStore::new(tx, std::time::Duration::from_millis(500));
        let want_cid = cid(0x7a);
        let want_blob = vec![1u8, 2, 3, 4];

        let blob_for_task = want_blob.clone();
        let task = tokio::spawn(async move {
            match rx.recv().await.expect("a CasOp was sent") {
                CasOp::GetLocal { cid, reply } => {
                    assert_eq!(cid, want_cid, "get_local must request the asked CID");
                    reply
                        .send(Some(blob_for_task))
                        .expect("reply receiver alive");
                }
                other => panic!("get_local must send CasOp::GetLocal, got {other:?}"),
            }
        });

        let got = store.get_local(&want_cid).await.expect("get_local ok");
        assert_eq!(got, Some(want_blob), "get_local returns the GetLocal reply");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn get_with_budget_passes_the_caller_budget_not_the_store_default() {
        // ZEB-805: the whole point of the per-call budget is that the op carries
        // what the CALLER asked for. If this ever regresses to `self.fetch_timeout`
        // the state-root fetches silently return to the 500 ms budget that caused
        // the incident, and nothing else would notice.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<CasOp>(4);
        let store = RuntimeContentStore::new(
            tx,
            std::time::Duration::from_millis(DEFAULT_FETCH_TIMEOUT_MS),
        );
        let want_cid = cid(0x11);
        let budget = std::time::Duration::from_millis(STATE_ROOT_FETCH_TIMEOUT_MS);

        let task = tokio::spawn(async move {
            match rx.recv().await.expect("a CasOp was sent") {
                CasOp::GetOrFetch {
                    cid,
                    timeout,
                    reply,
                } => {
                    assert_eq!(cid, want_cid, "must request the asked CID");
                    assert_eq!(
                        timeout, budget,
                        "op must carry the caller's budget, not the store default"
                    );
                    assert_ne!(
                        timeout,
                        std::time::Duration::from_millis(DEFAULT_FETCH_TIMEOUT_MS),
                        "guard: the budget under test must differ from the default, \
                         or this test proves nothing"
                    );
                    reply.send(Ok(None)).expect("reply receiver alive");
                }
                other => panic!("get_with_budget must send CasOp::GetOrFetch, got {other:?}"),
            }
        });

        let got = store
            .get_with_budget(&want_cid, budget)
            .await
            .expect("get_with_budget ok");
        assert_eq!(got, None, "plumbs the op reply through unchanged");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn get_with_budget_default_body_delegates_to_get() {
        // The trait's default body ignores the budget — correct for stores whose
        // `get` is already local-only. Pins that `InMemoryStub` (and every other
        // stub) keeps working without an override.
        let store = InMemoryStub::default();
        store.put(cid(2), vec![7, 8, 9]).await.unwrap();
        let blob = store
            .get_with_budget(&cid(2), std::time::Duration::from_millis(1))
            .await
            .unwrap()
            .expect("blob present");
        assert_eq!(blob, vec![7, 8, 9], "default body delegates to `get`");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_puts_all_land() {
        use std::sync::Arc;

        let store = Arc::new(InMemoryStub::default());
        let mut handles = vec![];
        for i in 0..50u8 {
            let s = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                s.put(cid(i), vec![i]).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        for i in 0..50u8 {
            let blob = store.get(&cid(i)).await.unwrap().expect("blob present");
            assert_eq!(blob, vec![i]);
        }
    }

    #[tokio::test]
    async fn runtime_content_store_put_round_trip() {
        // RuntimeContentStore sends CasOp::PutLocal; stub receiver replies Ok(()).
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));

        // Stub receiver: handle exactly one PutLocal then exit.
        let stub = tokio::spawn(async move {
            if let Some(CasOp::PutLocal {
                cid,
                blob,
                reply: Some(reply),
                ..
            }) = cas_op_rx.recv().await
            {
                assert_eq!(cid, ContentId::from_bytes([0x42; 32]));
                assert_eq!(blob, vec![1, 2, 3]);
                let _ = reply.send(Ok(()));
            } else {
                panic!("expected CasOp::PutLocal with reply");
            }
        });

        store
            .put(ContentId::from_bytes([0x42; 32]), vec![1, 2, 3])
            .await
            .unwrap();
        stub.await.unwrap();
    }

    #[tokio::test]
    async fn runtime_content_store_get_round_trip() {
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));

        let stub = tokio::spawn(async move {
            if let Some(CasOp::GetOrFetch {
                cid,
                timeout,
                reply,
            }) = cas_op_rx.recv().await
            {
                assert_eq!(cid, ContentId::from_bytes([0x99; 32]));
                assert_eq!(timeout, std::time::Duration::from_millis(500));
                let _ = reply.send(Ok(Some(vec![7, 8, 9])));
            } else {
                panic!("expected CasOp::GetOrFetch");
            }
        });

        let blob = store.get(&ContentId::from_bytes([0x99; 32])).await.unwrap();
        assert_eq!(blob, Some(vec![7, 8, 9]));
        stub.await.unwrap();
    }

    #[tokio::test]
    async fn runtime_content_store_put_propagates_error() {
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));

        let stub = tokio::spawn(async move {
            if let Some(CasOp::PutLocal {
                reply: Some(reply), ..
            }) = cas_op_rx.recv().await
            {
                let _ = reply.send(Err(ContentStoreError::Io("admit rejected".into())));
            }
        });

        let err = store
            .put(ContentId::from_bytes([1; 32]), vec![1])
            .await
            .unwrap_err();
        match err {
            ContentStoreError::Io(msg) => assert!(msg.contains("admit rejected")),
        }
        stub.await.unwrap();
    }

    #[tokio::test]
    async fn runtime_content_store_channel_closed_returns_io_error() {
        let (cas_op_tx, cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        // Drop the receiver immediately. Subsequent sends fail.
        drop(cas_op_rx);

        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));
        let err = store
            .put(ContentId::from_bytes([0; 32]), vec![])
            .await
            .unwrap_err();
        match err {
            ContentStoreError::Io(msg) => {
                assert!(msg.contains("(send)"), "got msg: {msg}");
            }
        }
    }

    #[tokio::test]
    async fn runtime_content_store_get_returns_none_for_timeout_signal() {
        // The actual tokio::time::timeout enforcement lives in the event-loop
        // arm; this test verifies that whatever the event loop replies (here:
        // Ok(None) simulating a timeout) is propagated unchanged.
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));

        let stub = tokio::spawn(async move {
            if let Some(CasOp::GetOrFetch { reply, .. }) = cas_op_rx.recv().await {
                let _ = reply.send(Ok(None));
            }
        });

        let blob = store.get(&ContentId::from_bytes([0xAA; 32])).await.unwrap();
        assert_eq!(blob, None);
        stub.await.unwrap();
    }

    #[tokio::test]
    async fn runtime_content_store_reply_dropped_returns_io_error() {
        // Stub receives the message but drops the reply sender without
        // replying. RuntimeContentStore.put should then surface the
        // distinct (reply) error message — the spec calls out the
        // distinction between (send) and (reply) lifecycle failures
        // because they implicate different root causes (receiver gone
        // before delivery vs receiver panicked after delivery).
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));

        let stub = tokio::spawn(async move {
            // Receive the message but drop the reply sender without replying.
            if let Some(CasOp::PutLocal {
                reply: Some(reply), ..
            }) = cas_op_rx.recv().await
            {
                drop(reply);
            }
        });

        let err = store
            .put(ContentId::from_bytes([0; 32]), vec![])
            .await
            .unwrap_err();
        match err {
            ContentStoreError::Io(msg) => {
                assert!(msg.contains("(reply)"), "got msg: {msg}");
            }
        }
        stub.await.unwrap();
    }

    #[test]
    fn allowlist_allow_then_contains() {
        let a = CommunityServeAllowlist::new();
        let c = cid(7);
        assert!(!a.contains(&c), "fresh allowlist contains nothing");
        a.allow(c);
        assert!(a.contains(&c), "allowed CID is contained");
        assert!(!a.contains(&cid(8)), "un-added CID is not contained");
    }

    #[test]
    fn allowlist_clone_shares_state() {
        // Arc-backed: a clone observes inserts made via the original.
        let a = CommunityServeAllowlist::new();
        let b = a.clone();
        let c = cid(42);
        a.allow(c);
        assert!(b.contains(&c), "clone shares the underlying set");
    }

    #[tokio::test]
    async fn put_serveable_registers_cid_in_allowlist() {
        // RuntimeContentStore.with_serve_allowlist: put_serveable admits AND records
        // the CID; plain put admits but does NOT record.
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let allowlist = CommunityServeAllowlist::new();
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500))
            .with_serve_allowlist(allowlist.clone());

        // Stub receiver: ack every PutLocal reply so put()/put_serveable() return Ok.
        let stub = tokio::spawn(async move {
            while let Some(op) = cas_op_rx.recv().await {
                if let CasOp::PutLocal {
                    reply: Some(reply), ..
                } = op
                {
                    let _ = reply.send(Ok(()));
                }
            }
        });

        let served = ContentId::from_bytes([0x11; 32]);
        let private = ContentId::from_bytes([0x22; 32]);
        store.put_serveable(served, vec![1, 2, 3]).await.unwrap();
        store.put(private, vec![4, 5, 6]).await.unwrap();

        assert!(
            allowlist.contains(&served),
            "put_serveable registers the CID"
        );
        assert!(
            !allowlist.contains(&private),
            "plain put does NOT register the CID"
        );
        drop(store);
        stub.await.unwrap();
    }

    #[tokio::test]
    async fn put_serveable_sends_serveable_bit_put_does_not() {
        // ZEB-535: pin the authorization bit on the emitted CasOp::PutLocal —
        // `put_serveable` must send `serveable: true` (the event-loop arm
        // allowlists it) and plain `put` must send `serveable: false` (never
        // allowlisted). The allowlist-registration test above covers the
        // RuntimeContentStore-side effect; this one pins the wire bit the event
        // loop actually keys its hash-verify-then-allowlist decision off.
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));

        // Stub: record the serveable flag of each PutLocal, then ack.
        let (flag_tx, mut flag_rx) = tokio::sync::mpsc::channel::<bool>(8);
        let stub = tokio::spawn(async move {
            while let Some(op) = cas_op_rx.recv().await {
                if let CasOp::PutLocal {
                    serveable,
                    reply: Some(reply),
                    ..
                } = op
                {
                    let _ = flag_tx.send(serveable).await;
                    let _ = reply.send(Ok(()));
                }
            }
        });

        store
            .put_serveable(ContentId::from_bytes([0x11; 32]), vec![1])
            .await
            .unwrap();
        assert_eq!(
            flag_rx.recv().await,
            Some(true),
            "put_serveable emits serveable: true"
        );

        store
            .put(ContentId::from_bytes([0x22; 32]), vec![2])
            .await
            .unwrap();
        assert_eq!(
            flag_rx.recv().await,
            Some(false),
            "put emits serveable: false"
        );

        drop(store);
        stub.await.unwrap();
    }

    #[tokio::test]
    async fn put_serveable_default_impl_routes_to_put() {
        // The default trait impl (InMemoryStub) routes put_serveable to put with no
        // allowlist concept and no panic.
        let store = InMemoryStub::default();
        store
            .put_serveable(ContentId::from_bytes([9; 32]), vec![7, 8])
            .await
            .unwrap();
        let got = store
            .get(&ContentId::from_bytes([9; 32]))
            .await
            .unwrap()
            .expect("blob present");
        assert_eq!(got, vec![7, 8]);
    }

    #[tokio::test]
    async fn put_serveable_without_allowlist_is_just_put() {
        // RuntimeContentStore with no allowlist set: put_serveable behaves like put.
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));
        let stub = tokio::spawn(async move {
            if let Some(CasOp::PutLocal {
                reply: Some(reply), ..
            }) = cas_op_rx.recv().await
            {
                let _ = reply.send(Ok(()));
            }
        });
        store
            .put_serveable(ContentId::from_bytes([3; 32]), vec![1])
            .await
            .unwrap();
        stub.await.unwrap();
    }

    #[tokio::test]
    async fn put_serveable_failed_put_does_not_register() {
        // Failure contract: if the underlying put fails, `?` returns early and
        // the CID is NEVER added to the allowlist (serving an un-admitted CID
        // would be a dangling entry that can never be satisfied locally).
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let allowlist = CommunityServeAllowlist::new();
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500))
            .with_serve_allowlist(allowlist.clone());

        // Stub: reply Err to the PutLocal so put_serveable's `?` propagates.
        let stub = tokio::spawn(async move {
            if let Some(CasOp::PutLocal {
                reply: Some(reply), ..
            }) = cas_op_rx.recv().await
            {
                let _ = reply.send(Err(ContentStoreError::Io("admit rejected".into())));
            }
        });

        let cid = ContentId::from_bytes([0x55; 32]);
        let err = store.put_serveable(cid, vec![1, 2]).await.unwrap_err();
        match err {
            ContentStoreError::Io(msg) => assert!(msg.contains("admit rejected")),
        }
        assert!(
            !allowlist.contains(&cid),
            "a failed put must NOT register the CID as serveable"
        );
        stub.await.unwrap();
    }

    /// Guard the `CasOp::GetLocal` enum shape and oneshot reply plumbing.
    ///
    /// This test does NOT construct a `NodeRuntime` (no cheap harness exists;
    /// building one for a 3-line read-only handler is not warranted). Instead
    /// it constructs the variant, pattern-matches to extract `cid`/`reply`,
    /// manually sends a reply, and asserts the receiver gets it. The
    /// handler's runtime-cache read is covered end-to-end by the Task 14
    /// cross-peer e2e (owner A serving its cached avatar).
    #[tokio::test]
    async fn cas_getlocal_variant_shape_and_reply_plumbing() {
        use harmony_content::cid::{ContentFlags, ContentId as HcContentId};

        let cid = HcContentId::for_book(b"hi", ContentFlags::default()).unwrap();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<Option<Vec<u8>>>();

        let op = CasOp::GetLocal {
            cid,
            reply: reply_tx,
        };

        // Pattern-match to verify the variant exists and fields are accessible.
        match op {
            CasOp::GetLocal {
                cid: extracted_cid,
                reply,
            } => {
                assert_eq!(extracted_cid, cid);
                // Simulate the handler: send Some(bytes) through the oneshot.
                let _ = reply.send(Some(vec![104, 105]));
            }
            _ => panic!("expected CasOp::GetLocal"),
        }

        let result = reply_rx.await.expect("oneshot should not be dropped");
        assert_eq!(result, Some(vec![104, 105]));
    }
}
