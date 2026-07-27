# Vine relay hardening — ZEB-817 / ZEB-818 / ZEB-819 / ZEB-822

Post-merge hardening bundle for the ZEB-811 vine relay fan-out (PR #563, main
@ `05abc4fe`). Two PRs:

- **PR A** — `zeblithic/harmony` (harmony-pkarr): verified-resolve variant
  (ZEB-817 core half). Branch `zeb-817-resolver-verify-callback` off
  main @ `e64e267`.
- **PR B** — `harmony-client`: pin bump + verified-resolve adoption (ZEB-817
  client half), unverified-cursor skew clamp (ZEB-818), page-boundary cursor
  progress (ZEB-819), publisher retraction-on-zero-vines (ZEB-822). Branch
  `zeb-817-vine-pull-hardening` off main @ `05abc4fe`.

All file:line references below were verified against those revisions.

## 1. ZEB-817 core — verified freshest-resolve (harmony-pkarr)

### Problem (verified)

`resolve_freshest` (`resolver.rs:209-258`) keeps a single freshest-by-seq
winner among candidates that pass only outer sig + inner sig + freshness
(`resolver.rs:224-238`). The inner sig is self-certifying — it verifies
against the record's own embedded `harmony_identity_pub` (`record.rs:92`) —
so an attacker publishing to a vines slot (key derived from fully public
inputs, `derive.rs:43-53`) passes every gate. The unverified winner then:

- pins the seq-highwater (`resolver.rs:251`) → the genuine lower-seq record
  is subsequently rejected as rollback (`resolver.rs:248`) — sticky squat
  for process lifetime (LRU 4096, boot-lifetime, not persisted);
- poisons the 15-min positive cache (`resolver.rs:253`), a second surface;
- shadows the genuine record entirely: the loop discards non-winners, so the
  caller's post-resolve verification has no fallback candidate.

Publisher is fully decoupled (never reads the highwater; seq = strictly
monotonic wall-clock µs from `SignedPacket` timestamps), so no publisher
changes.

### Fix

Add **new variant fns** (do not change existing signatures —
`resolve_window_freshest` has two non-vines client call sites,
`lib.rs:59162` and `lib.rs:64284`, that verify differently):

```rust
pub async fn resolve_freshest_with<F>(
    &self,
    pk: &VerifyingKey,
    verify: &F,
) -> Result<Option<PkarrRoutingRecord>, PkarrError>
where
    F: Fn(&PkarrRoutingRecord) -> bool + Sync;

pub async fn resolve_window_freshest_with<F>(
    &self,
    keys: &[VerifyingKey],
    verify: &F,
) -> Result<Option<PkarrRoutingRecord>, PkarrError>
where
    F: Fn(&PkarrRoutingRecord) -> bool + Sync;
```

Semantics of `resolve_freshest_with`:

1. Gather candidates exactly as today (full relay fan-out via `get_all`,
   outer-sig parse, `verify_inner_sig`, `verify_freshness`).
2. **Collect all surviving candidates** instead of reducing to one; sort
   descending by `(seq, announced_at_ms)`.
3. Return the first candidate for which `verify(&record)` is true.
4. Only that verified winner passes through the highwater gate (reject if
   `seq < seen` as rollback → `Ok(None)`; else `hw.put`) and `cache_put`.
   Candidates failing `verify` never touch the highwater or the cache.
5. No verified candidate → `Ok(None)`. No `PkarrError` change (the enum is
   not `#[non_exhaustive]`; a bool callback needs no new variant).

`resolve_window_freshest_with` mirrors the existing wrapper: concurrent
`join_all` of `resolve_freshest_with` per key, cross-key winner by
`announced_at_ms`, same error-precedence match (`resolver.rs:288-293`).
`join_all` borrows (no spawn), so `F: Sync` suffices; `&F` is `Send` when
`F: Sync`, which keeps the client's `tokio::spawn`-ed callers `Send`.

Callback type is a generic param, not a stored field — `PkarrResolver`
stays non-generic (it is held as `Arc<PkarrResolver>` by
`PkarrSlotResolver`, `rendezvous.rs:185`). Crate precedent for both shapes
exists (`publisher.rs:29` Arc alias; `rendezvous.rs:183-189` generic `F`).

Doc-comment the unverified `resolve_freshest`/`resolve_window_freshest`
with a pointer: callers whose slot keys derive from public inputs must use
the `_with` variants (ZEB-817).

### Core tests (inline in `resolver.rs` — `build_relay_payload_with_seq` is `#[cfg(test)] pub(crate)`)

Follow `resolve_freshest_beats_stale_relay` (`resolver.rs:528-585`): two
`MockPkarrRelay`s (each stores one envelope per key), per-relay single-relay
`RelayClient`s for targeted PUTs, records built with
`PkarrRoutingRecord::sign_new(...)` + 7-day TTL, seqs chosen via
`build_relay_payload_with_seq`.

1. **Squat defeated:** attacker record (distinct identity key, seq 200) on
   relay 1; genuine record (seq 100) on relay 2. `verify` accepts only the
   genuine identity. `resolve_freshest_with` returns the genuine record.
2. **Highwater not pinned by unverified record:** after test 1, call
   `invalidate_for_test` (clears positive cache, NOT the highwater), resolve
   again → genuine record still returned (would be `Ok(None)` rollback if
   the attacker's seq 200 had pinned the highwater).
3. **Verified rollback still rejected:** genuine seq 200 accepted, then
   relay replays genuine seq 100 only → `Ok(None)` (highwater still guards
   real rollback among verified records).
4. **All candidates unverified → `Ok(None)`**, and a subsequent resolve of a
   newly published genuine record succeeds (nothing was cached or pinned).

Gate: `cargo nextest run -p harmony-pkarr --features test-fixtures` + fmt +
clippy as per repo CI.

## 2. ZEB-817 client — adopt the verified resolve

The verify-after-resolve gap is entirely inside
`pkarr_vines::resolve_vine_relays` (`pkarr_vines.rs:134-151`): it calls
`resolver.resolve_window_freshest(&keys)` then `verify_vines_record` on the
single winner (`:149`). Change to:

```rust
let verify = |rec: &harmony_pkarr::PkarrRoutingRecord| {
    verify_vines_record(rec, creator_addr_hex, now_ms).is_ok()
};
let rec = resolver
    .resolve_window_freshest_with(&verifying_keys, &verify)
    .await
    .map_err(|e| format!("pkarr resolve: {e}"))?
    .ok_or_else(|| "no vines record found for creator".to_string())?;
let payload = verify_vines_record(&rec, creator_addr_hex, now_ms)?;
```

The closure borrows `creator_addr_hex`/`now_ms`; the final
`verify_vines_record` call re-runs the (pure, cheap) chain to obtain the
decoded payload. `vine_pull_driver.rs` does not change for this ticket.

Pin bump: `harmony-pkarr` is deliberately on its **own rev**, not the
13-crate lockstep — exactly two lines (`src-tauri/Cargo.toml:145` and
`:262`) move from `e64e2671…` to the PR A rev, plus fix the stale comment
at `:138` ("currently 80f6d80" → actual). During development pin the PR A
branch head; re-pin to the merge commit when Jake merges PR A.

## 3. ZEB-818 — unverified-cursor forward-skew clamp

### Problem (verified)

Session row loop `vine_pull_driver.rs:232-265`: the `SkipInvalid` arm
(`:253-256`) advances the cursor to the relay-supplied
`(created_at, id)` tuple. `SkipInvalid` covers signature-failure rows
(`vine_feed_cache.rs:664`), and `on_descriptor_sample` has **no forward
bound** on `created_at` (only the 90-day age lower bound at `:701`). A
hostile relay row with `created_at = u64::MAX` therefore poisons the
strictly-greater cursor; it persists in the sidecar and survives relay
switches.

### Fix

**Units:** `created_at` is **seconds** (`lib.rs:16636` `now_secs`);
the session's `now_ms` param is milliseconds. The clamp is in the seconds
domain.

In the session row loop, `SkipInvalid` arm only:

```rust
const VINE_PULL_INVALID_FORWARD_SKEW_SECS: u64 = 30 * 60;
// SkipInvalid arm:
if candidate.0 > now_ms / 1000 + VINE_PULL_INVALID_FORWARD_SKEW_SECS {
    // implausibly future-dated AND unverifiable: refuse the advance
    skipped_invalid += 1;
} else {
    cursor = candidate;
    skipped_invalid += 1;
}
```

30 min matches the house forward-skew default
(`INTRODUCTION_MAX_FORWARD_SKEW_MS`, `friend_intro.rs:62`;
`REACHABILITY_TIMESTAMP_SKEW_MAX_MS`, `community_membership.rs:5516`).

- `Advance`/`AdvanceDuplicate` (verified rows) advance unclamped — a
  future-dated *verified* descriptor is the creator's own doing (v1 relay
  sets are creator-signed; self-harm is out of scope per the ticket).
- Plausibly-timed invalid rows (tombstones, capacity-trim victims, benign
  rejects) still advance — no livelock on ordinary invalid regions.
- A full page of refused rows no longer advances the cursor → the existing
  zero-cursor-advance guard (`:270-280`) ends the session. A hostile relay
  can thereby stall pulls **from itself**, which it could already do by
  withholding data; it can no longer sabotage pulls from other relays in
  the set.

**Rejected alternative — cursor reset on relay-set change:** the ticket
lists it as optional. Rejected: multi-device publishing is currently
last-writer-wins per device (ZEB-820), so a creator's relay set flip-flops
routinely between devices; resetting on change would re-download the full
backlog on every flip. The clamp alone caps poison damage at 30 min ahead
of real time and self-heals as the clock catches up.

### Tests (session-level, existing `duplex` harness)

1. Page `[valid row (Advance), row with created_at = u64::MAX
   (SkipInvalid)]` → next query's `after_*` (via `read_fake_query`) equals
   the valid row's tuple, not the poisoned one.
2. Plausible invalid row (created_at ≈ now) still advances (guards the
   no-livelock property).
3. Boundary: `created_at = now_secs + SKEW + 1` refused,
   `= now_secs + SKEW` advanced.

## 4. ZEB-819 — page-boundary cursor progress (vine driver only)

### Problem (verified, rescoped)

One IO deadline (prod: `DEFAULT_BUTLER_IO_DEADLINE_MS` = 30 s, wired at
`lib.rs:10937-10942` — note: NOT `VINE_RELAY_IO_DEADLINE_MS`, which is the
acceptor-side constant) wraps dial + all pages
(`vine_pull_driver.rs:306-352`). On expiry the session future is dropped;
`pull_one_creator` only updates `st.cursor` from a returned
`PullSessionResult` (`:670`), and the `Err` arm (`:676`) discards
everything — N completed pages of progress lost, re-downloaded next pass.
A backlog too large for 30 s re-downloads the same prefix forever.

**Scope correction (posted to ZEB-819):** the community relay pull driver
has no cursor and no paging — hold-drain + ack; a lost ack only causes
redundant re-transfer. No community-side change.

### Fix

A caller-owned progress sink the dropped future cannot take with it:

```rust
/// Shared cursor-progress slot: the session commits after each fully
/// processed page; the driver reads it even when the session errs or the
/// IO deadline drops the future mid-flight.
#[derive(Clone, Default)]
pub struct PullProgressSink(Arc<std::sync::Mutex<Option<(u64, String)>>>);

impl PullProgressSink {
    pub fn commit(&self, cursor: (u64, String));  // monotone: only advances
    pub fn take(&self) -> Option<(u64, String)>;
}
```

- `VinePullTransport::pull_pages` (`:105-112`) gains a
  `progress: PullProgressSink` parameter (trait + prod transport + test
  mocks).
- `run_vine_pull_client_session` calls `progress.commit(cursor.clone())`
  once per page, after the row loop completes for that page. On the `Halt`
  early-return, commit whatever `cursor` holds at that point: the rows
  before the halt did ingest durably, and this matches the cursor the
  `Halt` arm already returns in its `PullSessionResult`.
- `pull_one_creator` creates one sink per creator per pass, shared across
  the candidate failover loop (progress is per-creator and monotone), and
  on **both** `Ok` and `Err` arms merges `sink.take()` into `st.cursor`
  (monotone max; `Ok`'s returned cursor equals the last commit).
- Disk cadence unchanged: sidecar still written once per pass
  (`:586-592`). Crash-durability is a non-goal — the fix targets
  deadline/error survival within a running process; a crash loses at most
  one pass's progress, and ingest is idempotent.

### Tests

1. Session-level (duplex): serve 2 full pages then hang (no response to the
   third query); outer `tokio::time::timeout` kills it → sink holds the
   page-2 boundary cursor.
2. Driver-level: `MockTransport` scripted to commit a cursor to the sink
   then return `Err` → after `run_one_pass`, `load_vine_pull` shows the
   committed cursor persisted (proves the `Err` arm merges progress).
3. Monotonicity: `commit` with a smaller tuple does not regress the sink.

## 5. ZEB-822 — retract instead of TTL-decay on share-ON/zero-vines

### Problem (verified)

`reconcile_locked` branch structure (`pkarr_vines_publisher.rs:232-318`):
share ON + zero own vines → plain `unregister` (`:290-293`), leaving any
previously published relay-set record to 7-day TTL decay. The disable path
(`:302-318`, round-1 fix on #563) already solves the identical problem by
registering a retraction-only `RecordBuilder` and leaving it registered.

**Hazard (verified):** the naive "publish retraction once, then
unregister" sequence publishes nothing — `unregister` sets `cancelled`
(`publisher.rs:127-131`) and `publish_one` short-circuits on it
(`publisher.rs:220-227`). There is no one-shot publish API and
`wire::build_relay_payload` is `pub(crate)`.

### Fix

Mirror the disable path exactly. Branch 4 becomes:

- share ON, zero vines, handle **active** (`active_handles` contains
  `HANDLE`) → register the retraction-only builder (same
  `build_retraction_blob` + `sign_new` shape as `:302-318`), leave it
  registered;
- share ON, zero vines, handle **not** active → return (nothing to
  retract; parity with branch 5).

Notes:

- The gate-open record builder (`:252-273`) already re-reads share/count
  live and emits `build_blob_or_retraction`, so a registered builder
  flips to retraction content on the publish cadence too; this change makes
  the flip happen on the reconcile that observes zero vines instead of
  waiting for the next scheduled publish tick.
- **Restart hole (accepted):** `active_handles` is process-local; a record
  published last boot with share-ON/zero-vines at next boot won't be
  retracted. The disable path has the identical hole today. Closing it
  needs a persisted "record live" flag — out of scope, parity is the goal.
- Branch 1 (endpoint `None` → plain unregister, `:233-236`) is left
  unchanged **intentionally**: endpoint-None is a transient boot state;
  retracting a valid record because iroh isn't up yet would be wrong.

### Tests

Pure tier: branch decision on (share=true, count=0, active) →
retraction-builder registration; (share=true, count=0, inactive) → no-op.
Resolve-asserting tier (pattern of
`stale_reconcile_never_reopens_serving_after_a_later_disable`,
`:558-635`): real identity (`vine_signing::test_identity`), enable with
`has_own_vines` backed by `Arc<AtomicUsize>` = 1 → poll for non-empty relay
set → set count 0 → `reconcile()` → poll until `resolve_vine_relays`
yields the **empty-set retraction** (not merely an unregistered handle).

## Non-goals

- Migrating Cases A/B/D call sites (`lib.rs:59162`, `:64284`, rendezvous
  `resolve` path) to `_with` — follow-up candidates; the rendezvous layer
  explicitly defers trust to the consumer handshake (`rendezvous.rs:169-174`).
- Community relay pull driver changes (no cursor exists; ack chunking not
  worth it).
- Persisted seq-highwater, persisted "record live" flag, cursor reset on
  relay-set change (rationales above).
- ZEB-820 multi-device relay-set aggregation (needs its own design note).

## Gates

- Core: `cargo fmt --all -- --check`, clippy per repo CI,
  `cargo nextest run -p harmony-pkarr --features test-fixtures`.
- Client: fmt + `clippy --all-targets` sentinel-gated un-piped, module
  suites (`vine_pull_driver`, `pkarr_vines`, `pkarr_vines_publisher`,
  `vine_relay`, rpc), `scripts/test-select --full` (dep-graph changes via
  pin bump force it), fresh `cargo build --bin harmony-app` +
  `s_vines_follow_only` e2e before PR.
