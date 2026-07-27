# Vine Relay Hardening Implementation Plan (ZEB-817/818/819/822)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the vines pkarr slot-squat DoS (resolver-side verified resolve), the pull-cursor poisoning hole, the lost-page-progress re-download loop, and the stale-relay-set TTL-decay corner.

**Architecture:** Two PRs. PR A (`zeblithic/harmony`, crate `harmony-pkarr`) adds `resolve_freshest_with`/`resolve_window_freshest_with` — candidate-list resolution with a caller verification callback, verified-only highwater/cache writes. PR B (`harmony-client`) bumps the pkarr pin, routes `resolve_vine_relays` through the verified variant, clamps unverified cursor advances to a 30-min forward skew, adds a caller-owned page-progress sink to the pull session, and mirrors the disable-path retraction for the share-ON/zero-vines corner.

**Tech Stack:** Rust, tokio, ciborium, ed25519-dalek, cargo-nextest. Spec: `docs/superpowers/specs/2026-07-27-vine-relay-hardening-design.md` (readable copy also applies to the core repo task).

## Global Constraints

- Repos/branches: PR A on `zeblithic/harmony` branch `zeb-817-resolver-verify-callback` off main @ `e64e267`; PR B on `harmony-client` branch `zeb-817-vine-pull-hardening` off main @ `05abc4fe`. Never commit to main.
- Do NOT change the signatures of existing `resolve`, `resolve_window`, `resolve_freshest`, `resolve_window_freshest` — `resolve_window_freshest` has non-vines client callers (`lib.rs:59162`, `lib.rs:64284`).
- No `PkarrError` variant additions (enum is not `#[non_exhaustive]`).
- `created_at` in vine descriptors is SECONDS; session `now_ms` is MILLISECONDS. Any comparison converts: `now_ms / 1000`.
- Skew constant: `VINE_PULL_INVALID_FORWARD_SKEW_SECS: u64 = 30 * 60` (house default, cf. `friend_intro.rs:62`).
- No cursor reset on relay-set change (LWW device flip-flop would churn; documented in spec §3).
- ZEB-822 must NOT use `register(retraction)` → `unregister()` — `unregister` sets `cancelled` and the publish short-circuits (`publisher.rs:127-131`, `:220-227`); mirror the share==false path (`pkarr_vines_publisher.rs:302-318`) and leave the retraction builder registered.
- Core tests live INLINE in `resolver.rs` (`build_relay_payload_with_seq` is `#[cfg(test)] pub(crate)`); one `MockPkarrRelay` holds ONE envelope per key — competing records need two relays.
- Gates are run un-piped with sentinel echoes (`&& echo GATE_OK`); on macOS there is no `timeout` binary.
- Client repo: `cargo fmt --all -- --check` and `cargo clippy --all-targets` are CI gates.

---

### Task 1: Core verified-resolve variants (harmony repo — PR A)

**Files:**
- Modify: `/Users/zeblith/work/zeblithic/harmony/crates/harmony-pkarr/src/resolver.rs`

**Interfaces:**
- Consumes: existing `resolve_freshest` body (`resolver.rs:209-258`), `resolve_window_freshest` body (`:262-294`), `seq_highwater`, `cache_put`, `crate::wire::parse_relay_payload`.
- Produces (Task 2 consumes):
  ```rust
  pub async fn resolve_freshest_with<F>(&self, pk: &VerifyingKey, verify: &F)
      -> Result<Option<PkarrRoutingRecord>, PkarrError>
  where F: Fn(&PkarrRoutingRecord) -> bool + Sync;

  pub async fn resolve_window_freshest_with<F>(&self, keys: &[VerifyingKey], verify: &F)
      -> Result<Option<PkarrRoutingRecord>, PkarrError>
  where F: Fn(&PkarrRoutingRecord) -> bool + Sync;
  ```

- [ ] **Step 1: Branch.** In `/Users/zeblith/work/zeblithic/harmony`: `git checkout main && git pull --ff-only && git checkout -b zeb-817-resolver-verify-callback`.

- [ ] **Step 2: Write the failing squat test** (inline in `resolver.rs` `mod tests`, modeled on `resolve_freshest_beats_stale_relay` at `:528-585`):

```rust
    /// ZEB-817: an attacker record with a higher seq but failing caller
    /// verification must not shadow the genuine record, must not pin the
    /// seq-highwater, and must not enter the positive cache.
    #[tokio::test]
    async fn verified_resolve_defeats_higher_seq_squat() {
        let squat = MockPkarrRelay::start();
        let genuine = MockPkarrRelay::start();
        let pool = crate::relay::RelayPool::new(vec![
            squat.base_url.clone(),
            genuine.base_url.clone(),
        ]);
        let client = Arc::new(crate::relay::RelayClient::new(pool));
        let resolver = PkarrResolver::new(client);

        let ephemeral = SigningKey::generate(&mut OsRng);
        let vk = ephemeral.verifying_key();

        let genuine_sk = SigningKey::generate(&mut OsRng);
        let genuine_pub = fixture_identity_pubkey(&genuine_sk);
        let attacker_sk = SigningKey::generate(&mut OsRng);
        let attacker_pub = fixture_identity_pubkey(&attacker_sk);

        let now = 1_700_000_000_000u64;
        let genuine_rec = PkarrRoutingRecord::sign_new(
            b"genuine".to_vec(), genuine_pub, now, now + 604_800_000, &genuine_sk,
        ).unwrap();
        let attacker_rec = PkarrRoutingRecord::sign_new(
            b"squat".to_vec(), attacker_pub, now + 1, now + 604_800_000, &attacker_sk,
        ).unwrap();

        // Genuine record seq 100 on relay 2; attacker seq 200 on relay 1.
        let genuine_c = crate::relay::RelayClient::new(
            crate::relay::RelayPool::new(vec![genuine.base_url.clone()]));
        let squat_c = crate::relay::RelayClient::new(
            crate::relay::RelayPool::new(vec![squat.base_url.clone()]));
        let key_z32 = crate::wire::z32_key(&vk.to_bytes());
        let (_, genuine_payload) =
            (key_z32.clone(), crate::wire::build_relay_payload_with_seq(&ephemeral, &genuine_rec, 100).unwrap());
        genuine_c.put(&key_z32, &genuine_payload).await.unwrap();
        let squat_payload =
            crate::wire::build_relay_payload_with_seq(&ephemeral, &attacker_rec, 200).unwrap();
        squat_c.put(&key_z32, &squat_payload).await.unwrap();

        let verify = |rec: &PkarrRoutingRecord| rec.harmony_identity_pub == genuine_pub;

        // 1. Squat defeated: genuine (lower-seq) record wins.
        let got = resolver.resolve_freshest_with(&vk, &verify).await.unwrap().unwrap();
        assert_eq!(got.routing_blob, b"genuine");

        // 2. Highwater NOT pinned by the unverified seq-200 record: bypass the
        // positive cache and resolve again — still the genuine record (a pinned
        // highwater would reject seq 100 as rollback and return None).
        resolver.invalidate_for_test(&vk.to_bytes());
        let again = resolver.resolve_freshest_with(&vk, &verify).await.unwrap();
        assert_eq!(again.unwrap().routing_blob, b"genuine");
    }
```

  NOTE: `crate::wire::z32_key` above stands for however `resolve_freshest` derives `key_z32` from `pk` — read `resolver.rs:213-215` and reuse the exact same helper; adjust the PUT plumbing to match `resolve_freshest_beats_stale_relay` (`:558-577`), which is the authoritative pattern. `fixture_identity_pubkey` (`:342-346`) packs only bytes 32..64; if `sign_new` validation requires it, keep the same helper the existing tests use.

- [ ] **Step 3: Add two more failing tests** in the same module:

```rust
    /// Verified rollback is still rejected: highwater must keep guarding
    /// against replay of an older VERIFIED record.
    #[tokio::test]
    async fn verified_resolve_still_rejects_verified_rollback() {
        // Single relay (latest write wins): PUT genuine seq 200, resolve
        // (accepts, highwater=200), then PUT genuine seq 100 (overwrites),
        // invalidate_for_test, resolve again => Ok(None).
    }

    /// All candidates failing verification => Ok(None), and nothing is
    /// cached or pinned: a genuine record published afterwards resolves.
    #[tokio::test]
    async fn all_unverified_resolves_none_without_pinning() {
        // PUT attacker seq 200 only; resolve with genuine-only verify =>
        // Ok(None). Then PUT genuine seq 100 on a second relay,
        // invalidate_for_test, resolve => genuine record.
    }
```

  Flesh both out with the same fixtures as Step 2 (they share the two-relay setup; write real bodies, the comments above are the scenarios to encode).

- [ ] **Step 4: Run tests to verify they fail** (fns don't exist yet):
  `cargo nextest run --locked -p harmony-pkarr --features test-fixtures -E 'test(verified_resolve)' ; echo "EXPECT compile error: resolve_freshest_with not found"`

- [ ] **Step 5: Implement `resolve_freshest_with`.** Copy the body of `resolve_freshest` (`resolver.rs:209-258`) into the new generic fn and change exactly two regions:

  Candidate loop — collect instead of reduce (replaces `:222-239`):

```rust
        let mut candidates: Vec<(u64, PkarrRoutingRecord)> = Vec::new();
        for (_relay, envelope) in hits {
            let (record, seq) = match crate::wire::parse_relay_payload(&pk_bytes, &envelope) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if record.verify_inner_sig().is_err() {
                continue;
            }
            if record.verify_freshness(now_ms).is_err() {
                continue;
            }
            candidates.push((seq, record));
        }
        // Freshest-first: seq primary, announced_at_ms tiebreak.
        candidates.sort_by(|(sa, ra), (sb, rb)| {
            sb.cmp(sa).then(rb.announced_at_ms.cmp(&ra.announced_at_ms))
        });
        // First candidate passing FULL caller verification wins. Candidates
        // that fail verification never touch the highwater or the cache
        // (ZEB-817: an unverified record must not pin either surface).
        let best = candidates.into_iter().find(|(_, rec)| verify(rec));
```

  Accept path: keep the existing `match best` (`:241-257`) verbatim — highwater gate, `hw.put`, `cache_put` now run only for the verified winner.

- [ ] **Step 6: Implement `resolve_window_freshest_with`.** Copy `resolve_window_freshest` (`:262-294`) changing only the mapped call: `keys.iter().map(|pk| self.resolve_freshest_with(pk, verify))`. Same cross-key `announced_at_ms` winner and error-precedence match.

- [ ] **Step 7: Doc-comment the unverified fns.** On `resolve_freshest` and `resolve_window_freshest` add: `/// NOTE (ZEB-817): callers resolving slots whose keys derive from public inputs (e.g. Case E vines) MUST use the _with variant — this fn accepts the freshest self-certified record without caller verification.`

- [ ] **Step 8: Run the new tests + full crate suite:**
  `cargo nextest run --locked -p harmony-pkarr --features test-fixtures && echo NEXTEST_GATE_OK`
  Expected: all pass including the 3 new tests.

- [ ] **Step 9: fmt + clippy gates (un-piped):**
  `cargo fmt --all -- --check && echo FMT_GATE_OK`
  `cargo clippy --locked -p harmony-pkarr --all-targets -- -D warnings && echo CLIPPY_GATE_OK` (match the repo's CI clippy invocation if stricter).

- [ ] **Step 10: Commit:**
  `git add crates/harmony-pkarr/src/resolver.rs && git commit -m "pkarr: verified freshest-resolve variants — unverified records cannot win, pin the highwater, or enter the cache (ZEB-817)"`

### Task 2: Client pin bump + verified-resolve adoption (PR B starts)

**Files:**
- Modify: `src-tauri/Cargo.toml:138` (stale comment), `:145`, `:262` (pin rev)
- Modify: `src-tauri/src/pkarr_vines.rs:134-151` (`resolve_vine_relays`)
- Test: `src-tauri/src/pkarr_vines_publisher.rs` (new resolve-asserting test)

**Interfaces:**
- Consumes: Task 1's `resolve_window_freshest_with` (signature above); `verify_vines_record(rec, creator_addr_hex, now_ms) -> Result<VineRelayRecordPayload, String>` (`pkarr_vines.rs:105-128`).
- Produces: `resolve_vine_relays` keeps its exact public signature (`pkarr_vines.rs:134-138`) — no caller changes anywhere.

- [ ] **Step 1: Branch.** In `harmony-client`: `git checkout main && git pull --ff-only && git checkout -b zeb-817-vine-pull-hardening`.

- [ ] **Step 2: Bump the pkarr pin.** In `src-tauri/Cargo.toml` set BOTH `:145` (`[dependencies]`) and `:262` (`[dev-dependencies]`, keeps `features = ["test-fixtures"]`) to `rev = "<HEAD of zeb-817-resolver-verify-callback>"` (the controller supplies the exact rev in the dispatch). Fix the stale comment at `:138` to name the new rev. `harmony-pkarr` is deliberately NOT on the 13-crate lockstep rev — touch only these two lines + comment.

- [ ] **Step 3: Write the failing wiring test** in `pkarr_vines_publisher.rs` `mod tests` (resolve-asserting tier, pattern at `:472-483` + two-relay PUT pattern from core; real identity via `crate::vine_signing::test_identity` so the address binding passes):

```rust
    /// ZEB-817 wiring: a squatter record on one relay with a higher seq must
    /// not shadow the genuine relay-set record on another relay, because
    /// resolve_vine_relays now verifies per-candidate inside the resolver.
    #[tokio::test]
    async fn squatted_slot_still_resolves_genuine_relay_set() {
        // Two MockPkarrRelays. Genuine publisher (real test identity,
        // signer_address) registers the real relay-set record via a
        // single-relay PkarrPublisher wired to relay G. Attacker publisher
        // (a DIFFERENT test identity, same slot key via key_builder for the
        // genuine creator address) registers its own record via a publisher
        // wired to relay S, and is driven until its PUT lands (poll
        // MockPkarrRelay or use the polling idiom at :503-515).
        // Then: resolver over BOTH relays; poll resolve_vine_relays(&resolver,
        // &genuine_addr, now_ms()) until it yields the GENUINE endpoint set.
        // Assert the returned relay_set contains the genuine endpoint id and
        // not the attacker's.
    }
```

  Write the real body following the existing helpers (`test_publisher` `:414-421` builds a publisher against one relay; clone the pattern for the second relay). The attacker publisher's `RecordBuilder` signs with the attacker identity but publishes under the SAME ephemeral slot key (`vines_key_for_epoch(&genuine_addr, epoch)`).

- [ ] **Step 4: Run it — expect failure** (attacker's fresher record wins today, resolve errors on binding): `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(squatted_slot_still_resolves_genuine_relay_set)' ; echo "EXPECT FAIL"`
  NOTE: this requires the Step 2 pin bump to compile against the `_with` API only in Step 5 — if the test needs `_with` to even express, reorder: bump pin (Step 2), write test, watch it fail against the OLD `resolve_vine_relays` body, then fix in Step 5. The test as written calls only `resolve_vine_relays`, so it compiles either way.

- [ ] **Step 5: Adopt the verified variant.** In `pkarr_vines.rs` `resolve_vine_relays`, replace the resolve + post-verify (`:144-149`) with:

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

  (The second `verify_vines_record` re-runs a pure, cheap chain to get the decoded payload; keep the existing tail of the fn that maps `payload.relay_set`.)

- [ ] **Step 6: Run the new test + both module suites:**
  `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(pkarr_vines)' && echo VINES_TESTS_OK`
  Expected: new test passes; all existing `pkarr_vines`/`pkarr_vines_publisher` tests still pass.

- [ ] **Step 7: Commit:**
  `git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/pkarr_vines.rs src-tauri/src/pkarr_vines_publisher.rs && git commit -m "vines resolve: adopt verified freshest-resolve; squatter records can no longer shadow or pin (ZEB-817)"`

### Task 3: Unverified-cursor forward-skew clamp (ZEB-818)

**Files:**
- Modify: `src-tauri/src/vine_pull_driver.rs` (session row loop `:232-265`; new const near `:70`; tests `:743-1000`)

**Interfaces:**
- Consumes: session fn `run_vine_pull_client_session` (`:191-283`), `now_ms: u64` param already in scope (`:197`), test helpers `descriptor_json(id, created_at)` (`:772`), `ScriptedIngest` (`:745`), `read_fake_query` (`:776`), `write_fake_page_response` (`:786`).
- Produces: `pub const VINE_PULL_INVALID_FORWARD_SKEW_SECS: u64 = 30 * 60;`

- [ ] **Step 1: Write the failing tests** (session-level duplex harness, model on `cursor_advances_past_invalid_but_not_past_halt` `:797`):

```rust
    /// ZEB-818: an unverifiable row with an implausibly future created_at
    /// must not advance the cursor (a hostile relay could otherwise poison
    /// the persisted cursor past all genuine descriptors forever).
    #[tokio::test]
    async fn skip_invalid_refuses_cursor_advance_past_forward_skew() {
        // Page: [descriptor_json("ok", 1_700_000_100) -> Advance,
        //        descriptor_json("evil", u64::MAX)   -> SkipInvalid]
        // now_ms = 1_700_000_000_000.
        // Assert next query's after_created_at/after_id == ("ok" row tuple).
    }

    /// Plausibly-timed invalid rows must STILL advance (tombstones, trim
    /// victims — refusing them would livelock ordinary invalid regions).
    #[tokio::test]
    async fn skip_invalid_within_skew_still_advances() {
        // Row: descriptor_json("dead", 1_700_000_050) -> SkipInvalid.
        // Assert next query cursor == ("dead" tuple).
    }

    /// Boundary: created_at == now_secs + SKEW advances;
    /// created_at == now_secs + SKEW + 1 is refused.
    #[tokio::test]
    async fn skip_invalid_skew_boundary_is_exact() { }
```

  Write real bodies using the duplex fixture idiom (`tokio::io::duplex(1 << 16)`, server task, `server_write.shutdown().await`, outer 5 s timeout — see `:905-940`).

- [ ] **Step 2: Run to verify the first fails** (today the cursor advances to `u64::MAX`): `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(skip_invalid_refuses)' ; echo "EXPECT FAIL"`

- [ ] **Step 3: Implement.** Add near the other consts (`:70` area):

```rust
/// ZEB-818: an unverifiable row may advance the pull cursor only within a
/// plausible clock window. Rows that fail ingest AND claim a created_at
/// further than this ahead of local time are treated as hostile-relay
/// cursor poisoning and do not advance. Seconds domain (descriptor
/// created_at is seconds; the session clock is ms). 30 min matches the
/// house forward-skew defaults (cf. INTRODUCTION_MAX_FORWARD_SKEW_MS).
pub const VINE_PULL_INVALID_FORWARD_SKEW_SECS: u64 = 30 * 60;
```

  In the row loop, `SkipInvalid` arm only (`:253-256`):

```rust
            IngestVerdict::SkipInvalid => {
                if candidate.0 > now_ms / 1000 + VINE_PULL_INVALID_FORWARD_SKEW_SECS {
                    // Implausibly future-dated and unverifiable: refuse the
                    // advance. A full page of these ends the session via the
                    // zero-advance guard below.
                    skipped_invalid += 1;
                } else {
                    cursor = candidate;
                    skipped_invalid += 1;
                }
            }
```

- [ ] **Step 4: Run all session tests:** `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(vine_pull_driver)' && echo PULL_TESTS_OK` — new tests pass, existing `cursor_advances_past_invalid_but_not_past_halt` still passes (its invalid rows are plausibly timed; if it used far-future timestamps, fix the fixture timestamps, not the invariant).

- [ ] **Step 5: Commit:** `git add src-tauri/src/vine_pull_driver.rs && git commit -m "vine pull: unverifiable rows advance the cursor only within 30min forward skew (ZEB-818)"`

### Task 4: Page-boundary cursor progress sink (ZEB-819)

**Files:**
- Modify: `src-tauri/src/vine_pull_driver.rs` — new `PullProgressSink` type; `VinePullTransport::pull_pages` signature (`:105-112`); prod transport `IrohVinePullTransport::pull_pages` (`:306-352`); session (`:191-283`); `pull_one_creator` (`:595-690`); test mocks (`:1043-1085`).

**Interfaces:**
- Consumes: existing session/driver structure, `store_creator_state` (`:692`).
- Produces:
  ```rust
  #[derive(Clone, Default)]
  pub struct PullProgressSink(std::sync::Arc<std::sync::Mutex<Option<(u64, String)>>>);
  impl PullProgressSink {
      pub fn commit(&self, cursor: (u64, String));  // monotone
      pub fn take(&self) -> Option<(u64, String)>;
  }
  ```
  `pull_pages(&self, relay: &VineRelayEntry, creator: &str, cursor: (u64, String), progress: PullProgressSink) -> Result<PullSessionResult, String>` — all impls.

- [ ] **Step 1: Write the failing tests:**

```rust
    /// ZEB-819: the IO deadline dropping the session future must not discard
    /// completed pages — the sink holds the last page-boundary cursor.
    #[tokio::test]
    async fn deadline_drop_preserves_page_boundary_progress() {
        // Duplex server: serve one full page (VINE_PULL_PAGE_LIMIT_MAX rows,
        // Advance verdicts), then serve NOTHING for the second query (hold
        // the write half open, never respond).
        // Run the session inside tokio::time::timeout(200ms, ...) — it
        // times out. Assert sink.take() == Some(last row tuple of page 1).
    }

    /// Sink commits are monotone: a stale smaller tuple cannot regress it.
    #[test]
    fn progress_sink_is_monotone() {
        let s = PullProgressSink::default();
        s.commit((10, "b".into()));
        s.commit((5, "a".into()));
        assert_eq!(s.take(), Some((10, "b".into())));
    }

    /// Driver merges sink progress on the Err arm: a scripted transport
    /// that commits then fails still advances the persisted cursor.
    #[tokio::test]
    async fn failed_session_persists_committed_page_progress() {
        // MockTransport scripted: on pull_pages call, progress.commit((7, "g".into()))
        // then return Err("deadline"). Seed sidecar cursor (0, "").
        // run_one_pass, then load_vine_pull => cursor == (7, "g").
    }
```

- [ ] **Step 2: Run to verify failure** (no `PullProgressSink` yet — compile error): `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(progress_sink)' ; echo "EXPECT compile error"`

- [ ] **Step 3: Implement the sink** (near `PullSessionResult`):

```rust
/// ZEB-819: caller-owned cursor-progress slot. The pull session commits
/// after each fully processed page; the driver reads it even when the IO
/// deadline drops the session future mid-flight, so completed pages are
/// never re-downloaded. Tuple order (created_at, id) matches the cursor.
#[derive(Clone, Default)]
pub struct PullProgressSink(std::sync::Arc<std::sync::Mutex<Option<(u64, String)>>>);

impl PullProgressSink {
    /// Monotone: only advances (strictly greater tuple order), so a stale
    /// commit from a failed earlier candidate cannot regress progress.
    pub fn commit(&self, cursor: (u64, String)) {
        let mut slot = self.0.lock().expect("progress sink poisoned");
        match slot.as_ref() {
            Some(cur) if *cur >= cursor => {}
            _ => *slot = Some(cursor),
        }
    }
    pub fn take(&self) -> Option<(u64, String)> {
        self.0.lock().expect("progress sink poisoned").take()
    }
}
```

- [ ] **Step 4: Thread it through.**
  - Trait `VinePullTransport::pull_pages` gains `progress: PullProgressSink` (last param).
  - `IrohVinePullTransport::pull_pages` passes it into `run_vine_pull_client_session` (which gains the same param) INSIDE the timeout-wrapped future.
  - Session: after the row loop of each page — including just before the `Halt` early-return and before the short-page/no-advance breaks — `progress.commit(cursor.clone());` (one call site at the end of the row loop covers the loop-continue and break paths; add one in the `Halt` arm before `return`).
  - `pull_one_creator`: create `let progress = PullProgressSink::default();` before the candidate loop (`:663`), pass a clone to each `pull_pages` call; after the loop, in BOTH arms: `if let Some(p) = progress.take() { if p > st.cursor { st.cursor = p; } }` — on `Ok`, `res.cursor` equals the final commit so this is a no-op; on `Err` it rescues completed pages.
  - Update `MockTransport` (`:1062-1085`) to accept and (for the new test) drive the sink; update `StubIngest`-based session tests' direct `run_vine_pull_client_session` calls with a sink arg.

- [ ] **Step 5: Run the full module:** `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(vine_pull_driver)' && echo PULL_TESTS_OK` — all pass.

- [ ] **Step 6: Commit:** `git add src-tauri/src/vine_pull_driver.rs && git commit -m "vine pull: page-boundary progress sink survives the IO deadline (ZEB-819)"`

### Task 5: Publisher retraction on share-ON/zero-vines (ZEB-822)

**Files:**
- Modify: `src-tauri/src/pkarr_vines_publisher.rs` (`reconcile_locked` branch 4, `:281-293`; tests)
- Modify: `src-tauri/src/lib.rs` (`delete_vine_impl` — post-tombstone reconcile trigger, spec §5)

**Interfaces:**
- Consumes: existing retraction path (`:302-318`), `build_retraction_blob` (`:61-67`), `publisher.active_handles()` (`:294-299`), test tiers (`:399-483`, `:558-635`), `Arc<AtomicUsize>` count-swap pattern (`:664-673`).
- Produces: no API change — behavior only.

- [ ] **Step 1: Write the failing resolve-asserting test** (model: `stale_reconcile_never_reopens_serving_after_a_later_disable` `:558-635`):

```rust
    /// ZEB-822: deleting your last vine while sharing stays ON must actively
    /// retract the published relay set (empty-set record), not strand it to
    /// 7-day TTL decay.
    #[tokio::test]
    async fn zero_own_vines_with_share_on_retracts_instead_of_ttl_decay() {
        // Real test identity + signer_address (binding must pass).
        // has_own_vines backed by Arc<AtomicUsize> starting at 1.
        // enable().await; poll until resolve_vine_relays yields a NON-empty
        // set. Set count to 0; reconcile().await; poll until
        // resolve_vine_relays yields an EMPTY set (the retraction record) —
        // NOT an Err("no vines record"): the record must exist and be empty.
    }
```

  Note the empty-set retraction makes `resolve_vine_relays` return `Ok(vec![])` (payload decodes, `relay_set` empty) — assert exactly that.

- [ ] **Step 2: Run to verify failure** (today the handle is unregistered; the old non-empty record persists on the mock relay, so the poll for an empty set times out): `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zero_own_vines_with_share_on)' ; echo "EXPECT FAIL"`

- [ ] **Step 3: Implement.** Replace branch 4 (`:290-293`) with the disable-path shape:

```rust
        // share == true but zero own vines: if a record may be live
        // (process-local handle registered), actively retract — publish the
        // empty-set record on the same cadence the disable path uses
        // (ZEB-822). NEVER register-then-unregister: unregister cancels the
        // pending publish and nothing would land. If no handle is
        // registered there is nothing this process published — nothing to
        // retract (restart hole shared with the disable path, accepted).
        if self
            .publisher
            .active_handles()
            .await
            .contains(&HANDLE.to_string())
        {
            // fall through to the retraction-register below
        } else {
            return;
        }
```

  then reuse/share the retraction-only `RecordBuilder` registration currently at `:302-318` (extract it into a private helper `async fn register_retraction(&self, ...)` called from both the share==false branch and this one, so the two paths cannot drift).

- [ ] **Step 4: Wire the delete trigger** (spec §5 — without it the new branch only runs at the scheduled pkarr cadence, leaving the headline ZEB-822 scenario unresolved for up to ~3.5 days). In `src-tauri/src/lib.rs` `delete_vine_impl`, after the tombstone publish acks, spawn (never inline-await) a bounded wait — up to ~2s of 25ms polls — for the loopback echo to evict the deleted vine from the feed cache, then call `pkarr_vines_publisher.republish()`. The reconcile must OBSERVE zero own vines, and the count it reads is updated asynchronously by the loopback echo, so reconciling immediately races the eviction. Do NOT apply the tombstone locally to win that race: `on_tombstone_sample` returns `AlreadyApplied` for the echo, which suppresses its `vine-removed` emission and its ZEB-670 pin-guarded blob eviction. Exhausting the wait degrades to exactly the pre-hook cadence-tick behavior. Cover the trigger in the `delete_vine` test suite.

- [ ] **Step 5: Run the module suites:** `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(pkarr_vines_publisher) or test(delete_vine)' && echo PUB_TESTS_OK` — new test + all existing pass (esp. `enable_does_not_register_without_own_vines` `:638` — a fresh enable with zero vines has no active handle, so it must still register nothing).

- [ ] **Step 6: Commit:** `git add src-tauri/src/pkarr_vines_publisher.rs src-tauri/src/lib.rs && git commit -m "vines publisher: actively retract when the last own vine disappears while sharing is on (ZEB-822)"`

### Task 6: Full gates + branch finish (PR B)

**Files:** none new — verification only (controller runs these; listed for the record).

- [ ] **Step 1:** `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo fmt --all -- --check && echo FMT_GATE_OK`
- [ ] **Step 2:** `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && echo CLIPPY_GATE_OK` (CI's exact clippy line)
- [ ] **Step 3:** `cd /Users/zeblith/work/zeblithic/harmony-client && scripts/test-select --full` (pin bump changes the dep graph — full sweep is mandatory; read the summary line, un-piped)
- [ ] **Step 4:** `cd src-tauri && cargo build --bin harmony-app && echo BUILD_OK`, then run the follow-only e2e with the fresh binary pinned (`HARMONY_APP_BIN`): `s_vines_follow_only` must pass.
- [ ] **Step 5:** Frontend untouched — no tsc/vitest needed unless the sweep says otherwise.
- [ ] **Step 6:** Push branch, open PR B referencing ZEB-817/818/819/822; PR A merge → re-pin commit (Cargo.toml `:145`/`:262` + comment + Cargo.lock) lands during convergence.
