# ZEB-882 + ZEB-884: member-card name resolution — reliable publish + fast late-join

Date: 2026-08-08
Tickets: ZEB-882 (D1 — a peer's card is never published), ZEB-884 (D2 — a published card
resolves slowly for later-joining members). Bundled: both halves of the same user-visible
symptom ("a connected peer shows a hex address instead of a name"), same subsystem.

## Problem

Surfaced during the ZEB-878 v0.2.4 fleet validation (Koya third-vantage, verified in code + live).
A peer's display name rides a Zenoh pub/sub topic `harmony/discovery/profile/owner/<owner_id_hex>`
(`card_topic_for`). Two independent defects each break name resolution:

- **D1 (publish-side — the card is never published).** The only thing that sets the publisher's
  cached `latest` is a successful `publish_owner_card`. On boot that call is **frontend-driven**
  (`App.svelte:806` `tryBootPublishCard`, retried on the `zenoh-status: connected` event); there is
  **no Rust-side boot publish**. `publish_owner_card` returns `Err("owner card runtime not ready")`
  (`lib.rs:15016`) until the owner runtime is fully wired. If the frontend's retry doesn't fire
  (state/timing race — AVALON exhibited exactly this), `latest` stays `None`. Because the 600s
  refresh and the 3/10/30s boot burst both re-emit `latest` and are **no-ops while it is `None`**,
  the node is then silently nameless to every peer on every network, forever. Headless `serve` is a
  permanent instance of this: it calls neither publish path and has no display name, so a `serve`
  node is *always* nameless.
- **D2 (subscribe-side — a published card resolves slowly).** The card subscriber is a plain
  `session.declare_subscriber(...)` (`event_loop.rs:2987`) — no query-on-subscribe, no retained
  value. A member who subscribes *after* the publisher's boot burst (any later-joining community
  member, any mid-session subscribe) receives nothing until the publisher's next steady refresh —
  **up to 600s**. This is the un-shipped subscriber-side residual of ZEB-568 (which shipped the boot
  burst + push-on-observed-Join train, but never the recommended query-on-subscribe / request-on-miss
  subscriber pull). It affects the GUI roster path too, not just headless.

D1 and D2 compounded to produce the reported "Ildwyn can't see AVALON's name": AVALON never
published (D1), and even a published card would have taken up to 600s to reach a late subscriber (D2).

## Scope

- **A. D1 publish robustness (GUI):** an in-memory pending-card latch so a boot-race
  `"runtime not ready"` failure auto-completes once the runtime is wired — no dependency on the
  frontend re-detecting `connected`.
- **B. D1 headless parity (`serve`):** a `--display-name` flag and a serve-boot card publish, so
  headless nodes are nameable. No flag → fall back to the active profile name.
- **C. D2 query-on-subscribe fast path:** a publisher-side Zenoh **queryable** answering the cached
  card bytes, and a **query-on-subscribe** `get` on the subscriber side, so a late joiner converges
  in <1s instead of ≤600s.

Out of scope (explicitly not in this bundle):
- No placeholder/empty-name card is ever published (decision below). A node with no real display
  name publishes nothing; peers keep the hex fallback until a real name exists.
- The display name stays **not persisted backend-side** (deliberate; the publisher caches signed
  bytes precisely because name/status live only in the frontend). The latch is in-memory only.
- ZEB-879b (runtime enable→republish stall for *discoverability*) is a sibling timing bug in a
  different publisher (`PkarrIdentityPublisher`), not this one. Not addressed here, though D1's latch
  is the same *shape* of fix and may inform it.

## Design

### A. D1 publish robustness — in-memory pending-card latch

The failure is a boot race: the frontend calls `republish_owner_card` before the owner runtime is
wired, gets `Err("owner card runtime not ready")`, and the retry-on-`connected` is not guaranteed to
fire. Fix it **by construction** on the Rust side, without persisting the name and without depending
on the frontend retry.

- Add a `NodeState` field `pending_card: Option<PendingCard>` where `PendingCard` carries exactly the
  arguments `republish_owner_card_impl` needs: `{ display_name: String, status_text: String,
  avatar_cid: Option<[u8;32]>, profile_page_root: Option<[u8;32]> }` (mirrors the current IPC args).
- In `publish_owner_card` (`lib.rs:14982`), when the runtime-not-ready branch (`lib.rs:15016`) is
  hit, **stash** the caller's card params into `pending_card` instead of only returning `Err`. Still
  return the same `Err` to the caller (behavior-compatible; the frontend's existing flow is
  unaffected — it simply no longer *needs* to retry).
- At the end of `start_node_inner`, after all owner-runtime components are `Some` (the point where
  `profile_card_publisher` is stored, `lib.rs:13186`), **drain the latch**: if `pending_card` is
  `Some`, take it and call `publish_owner_card` with those params. This runs on every start
  (boot and post-`stop_node` restart). Publishing here still relies on the boot burst / refresh to
  cover the Zenoh-session-connect timing — which is exactly what the burst is for; the point is that
  `latest` is now reliably set.
- The frontend keeps calling `republish_owner_card` on boot (unchanged). Ordering is safe both ways:
  - Frontend call arrives *before* the runtime is ready → not-ready → latched → drained at
    `start_node_inner` end. ✅ (the previously-broken case)
  - Frontend call arrives *after* the runtime is ready → publishes immediately (today's happy path);
    the latch is empty, drain is a no-op. ✅
- **Latch/ready-publish synchronization (no stale regression).** A ready-path publish and a stale
  latch must never let older content win with a newer HLC. Two rules enforce this:
  - The ready branch of `republish_owner_card_impl` **clears** `pending_card` under the same lock —
    a fresh publish supersedes any older latched card, so the drain can't later republish stale
    name/avatar with a newer HLC and regress the display over what we just published.
  - `drain_pending_owner_card` snapshots the gating components **and** takes the latch under a
    **single** lock, and takes **only when the runtime is ready**. A concurrent `stop_node` therefore
    leaves the latch intact for the next start instead of dropping the card on a doomed publish.

This does **not** publish anything when there is no real display name (no frontend call, no
`--display-name`): `pending_card` stays `None` and the drain is a no-op. (Decision C.)

### B. D1 headless `serve` parity

- Add a `--display-name <NAME>` CLI flag to the `serve` entrypoint (`main.rs` arg parsing +
  `serve_cli` signature). Thread the value into `serve_cli`.
- After `start_node_inner` succeeds in `serve_cli` (`lib.rs:29355`, alongside the existing
  `auto_subscribe_presence_all_communities` parity hook at `:29360`), publish the owner card via
  `republish_owner_card_impl(state, display_name, status="", avatar=None, page=None)`.
- **Name resolution order:** explicit `--display-name` wins; if omitted, fall back to the active
  profile name (`crate::profile::active_profile()`, already used at `lib.rs:29288`); if there is no
  named profile either, publish **nothing** (stay nameless — consistent with Decision C, no
  placeholder). Log which name source was used at info level so an operator can see it in the serve
  log.
- **Blank normalization (single policy for both sources).** `resolve_serve_card_name` treats a
  blank/whitespace-only flag *or* profile value as absent (falls through), and **trims** the value
  it returns — so a `"  Ada  "` flag publishes `"Ada"`, never the padded string, and an empty
  `--display-name ""` does not defeat the no-placeholder rule.
- This reuses the same publish path as GUI, so the D2 queryable (below) automatically serves a serve
  node's card to late subscribers too.

### C. D2 query-on-subscribe fast path

Give a fresh subscriber the *current* card immediately instead of waiting for a refresh tick.

- **Publisher side — a queryable answering cached bytes.** Each node declares a Zenoh `queryable` on
  its **own** card topic `card_topic_for(own_owner_id)`. On a `Query`, it replies with the cached
  `latest` bytes if `Some`, else replies with nothing (a node that hasn't published has no card to
  serve). Reply via `query.reply(key_expr, bytes).await` — this back-pressures only the responding
  node, never an engine channel, so it is **not** subject to the reply-drain wedge.
  - The queryable needs read access to the publisher's cached `latest`. Expose
    `ProfileCardPublisher::latest_handle() -> Arc<Mutex<Option<CardWire>>>` (an `Arc::clone`, no
    re-sign) and pass that handle into the queryable task. Co-locate the queryable declaration where
    both the shared Zenoh session and the `latest` handle are reachable (the card subscriber-pool
    region, `event_loop.rs:2947`, or the publisher spawn site, `lib.rs:10738` — implementation
    choice, resolved in the plan).
- **Subscriber side — query-on-subscribe.** In the per-subscription task, immediately after the
  `declare_subscriber` (`event_loop.rs:2987`), fire one `session.get(card_topic)` in a **detached
  task** (not inline) so it never blocks the live recv loop or a shutdown for up to the GET budget —
  the recv loop starts immediately and both paths feed the same cache (newer-HLC-wins). The detached
  get drains the reply **locally** (mirror `fetch_via_zenoh`/`query_mail_root`,
  `event_loop.rs:7650`/`:7706`: a bounded `tokio::time::timeout` around `while replies.recv_async()
  .await`, storing the first valid reply into a local `Option<Vec<u8>>`). It **logs** per-reply
  errors and oversize skips (parity with `fetch_via_zenoh` and the live-PUT path), checks the
  `closing` flag before starting, and feeds the fetched bytes through the **same** decode →
  `verify_card` → attribution → `cache.insert_verified` pipeline (`ingest_card_bytes`) the subscriber
  sample handler uses. The live recv loop continues receiving PUTs concurrently.
  - **Reply-drain safety (HARD):** the fetched bytes land in `ProfileCardCache` (a `Mutex`), **not**
    a bounded engine channel, so the local-drain pattern is sufficient and safe. There must be **zero
    `.await` forwarding a `Reply` into a bounded channel** in the get arm (the ZEB-803/812 wedge).
    We deliberately do *not* use `ReplySpill` here — a single-card local drain is simpler and carries
    no head-of-line risk.
  - Refactor the sample-handler body (`event_loop.rs:3005-3102`) into a shared
    `ingest_card_bytes(bytes, subscription_id, owner_id, cache, event_sink) -> ...` helper so the
    live-PUT path and the query-on-subscribe path share one verify/attribute/cache/emit
    implementation (no divergence, one place to change).
  - Robust to old peers: if the owner has no queryable (older build) or hasn't published, the `get`
    yields nothing within the timeout → fall back to the existing behavior (wait for PUTs). No error.
- **Keep the 600s steady refresh** as the liveness/idempotent-republish backstop. Query-on-subscribe
  is the fast path; the refresh is the safety net.

## Testing

Follow the established harness shapes (no real two-node Zenoh session — the existing suite proves that
is flaky in CI; item 8 of the code map). Test each seam at the level it lives.

- **A. Pending-card latch (unit, `lib.rs` `pending_owner_card_tests`):**
  - `republish_owner_card_impl` on a not-ready runtime **stashes** `pending_card` (assert `Some` with
    the passed params) and still returns the `"owner card runtime not ready"` `Err`.
  - `drain_pending_owner_card` with `pending_card == None` is a no-op.
  - `drain_pending_owner_card` with the runtime **not ready** LEAVES the latch intact (single-lock,
    take-only-when-ready — a torn-down runtime can't drop the card). The ready-path drain/publish is
    exercised end-to-end by the integration suite (a full node boot publishes the card).
- **B. Serve name resolution (unit, `serve_card_name_tests`):** `resolve_serve_card_name(flag,
  profile)` returns flag > profile > `None`; blank/whitespace inputs fall through; a returned value
  is **trimmed** (`"  Ada  "` → `"Ada"`). All branches unit-tested without spawning serve.
- **C. Query-on-subscribe / queryable:**
  - Publisher `latest_handle()` returns a handle observing the same `Option<CardWire>` that
    `publish_now` sets (`profile_card_broadcast.rs` unit test).
  - `ingest_card_bytes` helper (`ingest_card_bytes_tests`): valid card → cached + `member-card-received`
    emitted; oversize (>4096) → dropped pre-decode; wrong-owner attribution → rejected. This is the
    shared pipeline BOTH the live-PUT arm and the query-on-subscribe path feed, so covering it proves
    the ingest half of the D2 fast path deterministically. The existing
    `profile_card_cross_peer_integration.rs` assertions continue to cover the verify/attribution
    pipeline through the refactor.
  - D2 wire-contract coverage: a real two-session Zenoh test is deliberately avoided (flaky in CI —
    item 8 of the code map). The queryable→get contract is instead guaranteed by construction:
    publisher and subscriber both key on `card_topic_for(owner_id)` (one function, symmetric), the
    queryable replies the exact `latest_handle` bytes, and those bytes flow through the same
    `ingest_card_bytes` the unit tests cover. The `session.get` drain runs in a detached task and
    contains no `.await` into a bounded channel (ZEB-803/812), enforced by construction + a code
    comment; not a runtime test.
- **Full CI-parity gate before PR** (from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy
  --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run
  --locked --workspace --all-targets --features test-fixtures`. From repo root: `npx tsc --noEmit`;
  `npx vitest run`.

## Risks / notes

- **No live fleet repro.** AVALON/Ildwyn are currently off, so the exact D1 boot race can't be
  reproduced end-to-end right now. The latch fixes it *by construction* (a not-ready failure can no
  longer strand `latest = None`), validated by unit/integration tests; a live cross-WAN confirmation
  is a follow-up when the fleet is back, not a blocker.
- **Reply-drain wedge (ZEB-803/812).** The one real hazard in D2. Mitigated by using the local-drain
  pattern (no bounded-channel forwarding) and a code comment at the get site. Any reviewer should
  confirm the get arm has zero forwarding awaits.
- **Queryable topic symmetry.** The queryable must be declared on `card_topic_for(own_owner_id)` so a
  subscriber's `get` on a peer's topic routes to that peer's queryable. A mismatch would silently
  yield no fast-path result (falls back to PUT-wait) — degrade, not break.
- **Idempotent double-publish.** The latch drain plus a later frontend success may both publish;
  equal-or-newer-HLC newer-wins makes the duplicate harmless.
- **Serve `--display-name` is opt-in-ish.** With no flag and no named profile, serve still publishes
  nothing — a bare anonymous `serve` stays nameless by design (Decision C).
