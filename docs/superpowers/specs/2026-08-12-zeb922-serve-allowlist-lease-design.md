# ZEB-922 — Serve-allowlist lease discipline (R5-A) design

**Ticket:** ZEB-922 (child of ZEB-913, Freenet review R5). **Date:** 2026-08-12.
**Pattern:** serving effort is renewed by demonstrated demand or authoritative
reference and collapses by default (Freenet interest-gated leases — the
discipline, not the parameters).

## 1. Verified premises (2026-08-12 audit, main `6d42382e`)

1. `CommunityServeAllowlist` is `Arc<RwLock<HashSet<ContentId>>>` with exactly
   `new`/`allow`/`contains` (`content_store.rs:31-53`). No removal API, zero
   removal call sites; entries live until the next `start_node_inner`.
2. The serve gate is `content_cid_servable` (`event_loop.rs:11435-11440`):
   encrypted CIDs are served iff allowlisted; refusal is a silent non-reply.
3. Insert sites: `put_serveable` (community segments
   `community_state_sync.rs:3564`, community manifest `:3668`, fleet root
   `fleet_sync.rs:1283`), encrypted ingest (`event_loop.rs:4935`, `:4980`),
   grant subtree walk (`event_loop.rs:5036`), artifact download completion
   (`lib.rs:34603`).
4. Successful serves are unobservable: the queryable logs only failures
   (`event_loop.rs:11498-11520`).
5. Revocation deliberately does NOT de-allowlist (design constraint 3,
   `lib.rs:23585-23588`; pinned by `revoke_read_is_lazy_and_keeps_allowlist`).
6. **Renewal-relevant publish mechanics** (verified this session):
   - A community republish re-`put_serveable`s ONLY newly-sealed segments
     (O(delta), `community_state_sync.rs:3558-3566`); reused segments are
     never re-put. The manifest IS re-put on every publish AND on every peer
     root GET, because `encode_root_packet` runs on the query-serve path too
     (`community_state_sync.rs:3450`, `:3154`, comment at `:3670-3673`).
   - The fleet root is a single self-contained blob re-`put_serveable`d on
     every publish and every peer root PULL (`fleet_sync.rs:1049-1067`,
     `:1272-1284`). It needs no new renewal machinery.
   - EncryptedDurable bytes ARE disk-persisted (`production_content_policy`,
     `lib.rs:13011-13013`, pinned at `lib.rs:86-91`).

## 2. Latent bug found and fixed by this design

Because bytes persist but the allowlist does not, and reused segments are never
re-inserted, **a restarted publisher cannot serve its previously-sealed
segments**: a joiner fetches the manifest fine (re-put on the root GET) but
every reused-segment GET is refused by the gate until that segment happens to
be re-sealed — for old immutable segments, potentially never. Single-publisher
communities are exposed hardest (another online member's own root heals
multi-publisher communities). Same family as ZEB-706/ZEB-398. The
`affirm_serveable` hook in §3.2 closes this: every root GET re-affirms every
current segment just-in-time, before the requester fetches them.

## 3. Design

### 3.1 Data structure (`content_store.rs`)

`CommunityServeAllowlist(Arc<RwLock<HashMap<ContentId, u64>>>)` — value is
`last_affirmed_ms` (wall clock; stamps are never persisted and never compared
across processes, but multiple modules stamp the same map, so the one shared
clock everyone already uses — `crate::wall_clock_ms()` — wins over a private
monotonic. A backwards clock jump delays expiry; a forwards jump expires
early; both are acceptable for hygiene state that §3.2 re-affirms on demand).

API (all lock-scoped, sync, no guard across await — preserving the documented
`std::sync::RwLock` rationale at `content_store.rs:25-29`):

- `allow(&self, cid)` — existing signature, now = `allow_at(cid, wall_clock_ms())`.
  Insert-or-refresh. All existing callers compile unchanged.
- `allow_at(&self, cid, now_ms)` — pure core, testable.
- `contains(&self, &cid) -> bool` — UNCHANGED and read-pure (a request must
  never renew a lease; only a successful serve or an authoritative affirm may).
  Poisoned-lock semantics preserved: fail closed.
- `touch(&self, &cid)` / `touch_at(&self, &cid, now_ms) -> bool` — refresh iff
  present; never inserts (so a served unencrypted CID cannot enter the map).
- `sweep_expired(&self, now_ms, ttl_ms) -> usize` — removes entries where
  `last_affirmed.saturating_add(ttl_ms) < now_ms` (strict `<`: boundary
  survives, mirroring `RelayHoldDoc::gc`). Returns removed count.
- `last_affirmed_ms(&self, &cid) -> Option<u64>` + `len()` — observability/tests.

Constants: `SERVE_ALLOWLIST_TTL_MS = 30 * 24 * 60 * 60 * 1000` (30 days,
matching `RELAY_HOLD_TTL_MS` precedent — the goal is bounding unbounded growth
on long-uptime nodes, not aggressive collapse) and
`SERVE_ALLOWLIST_SWEEP_INTERVAL_MS = 3_600_000` (1 h; expiry precision is
irrelevant at a 30-day TTL).

### 3.2 Renewal paths

1. **Producer re-affirmation (authoritative reference).** New `ContentStore`
   trait method `fn affirm_serveable(&self, cid: ContentId)` with a default
   no-op body (same idiom as `get_local`/`put_serveable` defaults,
   `content_store.rs:61-113`); `RuntimeContentStore` implements it as
   `allowlist.allow(cid)` (no-op when constructed without an allowlist).
   `encode_root_packet` calls it for **every segment ref in the manifest**
   after the manifest `put_serveable` (covers reused segments; newly-sealed
   ones were just re-put). Since that function runs on every publish AND every
   peer root GET, current segments are re-affirmed exactly when a requester is
   about to fetch them — this is both the lease renewal and the §2 restart
   fix. The fleet root and community manifest already self-affirm via re-put.
2. **Demand renewal.** The content-serve queryable calls
   `serve_allowlist.touch(&cid)` after a successful reply, and emits a
   `tracing::debug!` serve-success line (closing the observability gap — serves
   become visible in logs and in `last_affirmed_ms`).
3. **Collapse.** A dedicated sweep task in `lib.rs` beside the relay-hold GC
   (`lib.rs:12770-12841` precedent): `tokio::interval` 1 h,
   `MissedTickBehavior::Skip`, first tick consumed, wall-clock `now`, calls
   `sweep_expired`, logs removed/remaining at debug. Abort handle stored and
   aborted on node stop exactly like `community_relay_gc_handle`
   (`lib.rs:1992`, `:14750`). Nothing else consults an oracle: renewal is
   entirely push-based, so the sweep needs only the allowlist handle.

### 3.3 What each entry class experiences

| Class | Renewal | Post-decay recovery |
|---|---|---|
| Community manifest | re-put on publish + every root GET | self-healing (next GET re-puts) |
| Community segments | `affirm_serveable` on publish + every root GET | self-healing just-in-time (root GET precedes segment fetches) |
| Fleet root | re-put on publish + every root PULL | self-healing |
| Grant subtrees, shared/downloaded artifacts | `allow()` at grant/ingest/download + `touch` on every successful serve | none from the requester side — bounded by the 30-day idle TTL (see §5.2) |

### 3.4 Explicitly preserved semantics

- Revoke still never de-allowlists; collapse comes only from 30 days of zero
  affirmation. `revoke_read_is_lazy_and_keeps_allowlist` stays green.
- `contains` fail-closed on poison; `allow` best-effort on poison — unchanged.
- Gate predicate `content_cid_servable` — unchanged.
- ~10 test files constructing `CommunityServeAllowlist::new()` positionally —
  compile unchanged (no signature changes to `new`/`allow`/`contains`).

## 4. Tests (TDD)

- **U1** (`content_store.rs`): `allow_at` inserts + refreshes; `touch_at`
  refreshes-iff-present and never inserts; `sweep_expired` removes only
  expired entries with a strict boundary (entry at exactly `now - ttl`
  survives); refreshed entries survive a sweep that removes their cohort.
- **U2** (`content_store.rs`): `put_serveable` still registers;
  `affirm_serveable` default is a no-op; `RuntimeContentStore::affirm_serveable`
  inserts-or-refreshes; without an allowlist it is a no-op.
- **U3** (`event_loop.rs` queryable tests): a successful serve refreshes
  `last_affirmed_ms`; a refused or byte-missing request does not.
- **U4** (`community_state_sync.rs`): after a segmented publish, wiping the
  allowlist (simulated restart) and driving the root query-serve path
  re-affirms every manifest segment CID — the §2 regression pin.
- **U5**: existing suites stay green untouched, specifically
  `revoke_read_is_lazy_and_keeps_allowlist`, the `content_serve_gate_tests`,
  the `allow_serve_subtree_tests`, and the ZEB-706/707 fleet-root tests.

## 5. Declined / out of scope

1. **Registry-driven keep-set oracle sweep** — unnecessary once renewal rides
   `encode_root_packet` (which already enumerates exactly the authoritative
   segment set on exactly the right trigger); avoids threading registry
   handles into a GC task.
2. **Lazy authoritative recheck on gate miss** (`find_attachment` on refused
   CIDs) — converts the miss path into an O(segments) disk scan reachable by
   any peer naming random CIDs (DoS surface) for a marginal UX win over the
   30-day TTL. Revisit only if idle-decay stalls are observed in practice.
3. **Persisting the allowlist across restarts** — the restart amnesia for
   grant/artifact classes predates this change and is a separate feature
   (serve-intent durability); §3.2's affirm hook already restores the one
   class where restart amnesia provably stalls peers (segments).
4. **Metrics-pipeline serve counters** (`network_health` integration) — the
   debug log line + `last_affirmed_ms` satisfy the observability acceptance;
   a counter plumb is pure scope growth here.
5. **Buddy pins / relay holds** — ZEB-923 / ZEB-924.
