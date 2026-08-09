# ZEB-889 — Joiner-side mint reuse on retry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop a legitimate joiner's failed-delivery retry from creating a permanent unredeemable zombie invite, by caching the joiner's minted redemption per invite-token and reusing it on retry so the host's existing `AlreadyKnown`-retransmit path re-delivers the countersign.

**Architecture:** A process-lifetime `HashMap<[u8;64], MintedCommunity>` on `CommunitySyncRegistry`, keyed by `InviteToken.sig`. `connectivity_redeem_invite_iroh_inner` reuses a cached mint instead of minting fresh; evicts on a `joined` outcome. No host-side or wire-format change — the acceptor already re-delivers a countersign on an `AlreadyKnown` (same-id) insert.

**Tech Stack:** Rust, tokio, `cargo nextest`. Design: `docs/specs/2026-08-08-zeb-889-invite-countersign-redelivery-design.md`.

## Global Constraints

- All cargo commands run from `src-tauri/`.
- Full gate: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; lint `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; format `cargo fmt --all -- --check`. Iterative runs may use `scripts/test-select --context task` — when you do, copy the emitted `round=… bucket=…` summary line into the task report so the selection is auditable/reproducible.
- `--locked` and `--features test-fixtures` are load-bearing; never drop them.
- No change to `verify_event`, P6, the acceptor, or the wire format.
- Cache is in-memory only (no disk persistence).

---

### Task 1: In-flight-redemption mint cache — type, `MintedCommunity: Clone`, registry plumbing, unit tests

**Files:**
- Modify: `src-tauri/src/lib.rs` (add `#[derive(Clone)]` to `MintedCommunity`, ~`:35402`)
- Modify: `src-tauri/src/community_state_sync.rs` (new `InFlightRedemptionMints` type; new field on `CommunitySyncRegistry` ~`:5367`; init in `CommunitySyncRegistry::new`; three delegating methods near `:5850`; unit tests in the file's `mod tests`)

**Interfaces:**
- Produces:
  - `MintedCommunity: Clone` (crate-visible struct in `lib.rs`).
  - `CommunitySyncRegistry::get_redemption_mint(&self, token_sig: [u8; 64]) -> Option<crate::MintedCommunity>`
  - `CommunitySyncRegistry::store_redemption_mint(&self, token_sig: [u8; 64], mint: crate::MintedCommunity)`
  - `CommunitySyncRegistry::evict_redemption_mint(&self, token_sig: &[u8; 64])`

- [ ] **Step 1: Add `#[derive(Clone)]` to `MintedCommunity`**

In `src-tauri/src/lib.rs` at the `pub struct MintedCommunity` definition (~`:35402`), add the derive:

```rust
#[derive(Clone)]
pub struct MintedCommunity {
    pub community_id: crate::owner_state_types::SpaceId,
    pub membership_key: crate::owner_state_types::EpochKey,
    pub space: crate::owner_state_types::Space,
    pub bootstrap_join: crate::community_membership::SignedMembershipEvent,
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cd src-tauri && cargo check --locked --features test-fixtures`
Expected: compiles (all four fields are `Clone`: `SpaceId` is `Copy`, `EpochKey`/`Space`/`SignedMembershipEvent` are `Clone`). If `EpochKey` or `Space` is not `Clone`, stop and reassess (the design assumes they are).

- [ ] **Step 3: Write the failing cache unit tests**

Add to the `mod tests` in `src-tauri/src/community_state_sync.rs`. These test the standalone cache type directly (no registry construction needed). Build a throwaway `MintedCommunity` from existing membership test helpers — reuse whatever the file's tests already use to mint a `SignedMembershipEvent` (e.g. `mint_test_owner` + `sign_event`, or the `zeb875_*` helpers). Helper sketch:

```rust
fn mint_stub(id_byte: u8) -> crate::MintedCommunity {
    // A stored MintedCommunity is opaque to the cache (key/value map), so any
    // well-formed value works. Build a PendingJoin-shaped SignedMembershipEvent
    // via the module's existing test helpers and vary its id by id_byte so two
    // stubs are distinguishable.
    // ... construct community_id / membership_key / space / bootstrap_join ...
}

#[tokio::test]
async fn redemption_mint_cache_store_get_evict() {
    let cache = InFlightRedemptionMints::default();
    let sig = [7u8; 64];
    assert!(cache.get(sig).await.is_none(), "empty cache misses");
    let m = mint_stub(0x11);
    let want_id = m.bootstrap_join.id;
    cache.store(sig, m).await;
    let got = cache.get(sig).await.expect("stored mint is retrievable");
    assert_eq!(got.bootstrap_join.id, want_id, "get returns the stored value");
    // still present on a second get (get does not consume)
    assert!(cache.get(sig).await.is_some(), "get is non-consuming");
    cache.evict(&sig).await;
    assert!(cache.get(sig).await.is_none(), "evicted entry is gone");
}

#[tokio::test]
async fn redemption_mint_cache_keys_are_independent() {
    let cache = InFlightRedemptionMints::default();
    let (a, b) = ([1u8; 64], [2u8; 64]);
    let (ma, mb) = (mint_stub(0xAA), mint_stub(0xBB));
    let (ida, idb) = (ma.bootstrap_join.id, mb.bootstrap_join.id);
    cache.store(a, ma).await;
    cache.store(b, mb).await;
    assert_eq!(cache.get(a).await.unwrap().bootstrap_join.id, ida);
    assert_eq!(cache.get(b).await.unwrap().bootstrap_join.id, idb);
    cache.evict(&a).await;
    assert!(cache.get(a).await.is_none(), "evicting a leaves b");
    assert!(cache.get(b).await.is_some());
}
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(redemption_mint_cache)'`
Expected: FAIL to compile — `InFlightRedemptionMints` does not exist yet.

- [ ] **Step 5: Implement the cache type + registry plumbing**

In `src-tauri/src/community_state_sync.rs`:

Define the cache type (near the other registry-support types):

```rust
/// ZEB-889: process-lifetime cache of a joiner's minted redemption, keyed by
/// the invite-token signature. A failed-delivery retry reuses the cached mint
/// (same bootstrap_join id/bytes) so the host's AlreadyKnown-retransmit path
/// re-delivers the countersign, instead of minting a fresh id P6 then rejects.
/// Same lock-discipline as the other registry maps (guard never held across an
/// unrelated `.await`).
#[derive(Default)]
pub(crate) struct InFlightRedemptionMints {
    by_token: tokio::sync::Mutex<std::collections::HashMap<[u8; 64], crate::MintedCommunity>>,
}

impl InFlightRedemptionMints {
    async fn get(&self, token_sig: [u8; 64]) -> Option<crate::MintedCommunity> {
        self.by_token.lock().await.get(&token_sig).cloned()
    }
    async fn store(&self, token_sig: [u8; 64], mint: crate::MintedCommunity) {
        self.by_token.lock().await.insert(token_sig, mint);
    }
    async fn evict(&self, token_sig: &[u8; 64]) {
        self.by_token.lock().await.remove(token_sig);
    }
}
```

(If `tokio::sync::Mutex<HashMap<..>>` does not satisfy `#[derive(Default)]`, drop the derive and initialize the field explicitly in `new` with `InFlightRedemptionMints { by_token: Default::default() }`.)

Add the field to `struct CommunitySyncRegistry` (after `root_fetch_shutdowns`):

```rust
    /// ZEB-889: joiner-side minted-redemption cache (see InFlightRedemptionMints).
    in_flight_redemption_mints: InFlightRedemptionMints,
```

Initialize it in `CommunitySyncRegistry::new` (find the struct literal that builds `Self { .. }`):

```rust
    in_flight_redemption_mints: InFlightRedemptionMints::default(),
```

Add the three delegating methods next to `register_pending_redemption` (~`:5850`):

```rust
    /// ZEB-889: return the cached minted redemption for `token_sig`, if a prior
    /// (failed-delivery) attempt for this invite stored one.
    pub async fn get_redemption_mint(&self, token_sig: [u8; 64]) -> Option<crate::MintedCommunity> {
        self.in_flight_redemption_mints.get(token_sig).await
    }

    /// ZEB-889: cache the minted redemption for `token_sig` so a retry reuses it.
    pub async fn store_redemption_mint(&self, token_sig: [u8; 64], mint: crate::MintedCommunity) {
        self.in_flight_redemption_mints.store(token_sig, mint).await;
    }

    /// ZEB-889: drop the cached mint for `token_sig` (join succeeded).
    pub async fn evict_redemption_mint(&self, token_sig: &[u8; 64]) {
        self.in_flight_redemption_mints.evict(token_sig).await;
    }
```

Update the unit tests to call the type directly (they already use `InFlightRedemptionMints::default()`); no registry construction is required.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(redemption_mint_cache)'`
Expected: PASS (both tests).

- [ ] **Step 7: Lint + format**

Run: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo fmt --all`
Expected: clean. (`get`/`store`/`evict` are used by the registry delegations; the delegations are `pub` so not dead-code even before Task 2.)

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/community_state_sync.rs
git commit -m "ZEB-889: in-flight redemption mint cache on the community registry

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D"
```

---

### Task 2: Reuse the mint on retry + evict on success, with an end-to-end retry-converges integration test

**Files:**
- Modify: `src-tauri/src/lib.rs` (`connectivity_redeem_invite_iroh_inner`: reuse at the mint site ~`:62816-62844`; evict in the `Ok(dto)` arm ~`:63173`)
- Test: `src-tauri/tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs` (new integration test)

**Interfaces:**
- Consumes: `CommunitySyncRegistry::{get_redemption_mint, store_redemption_mint, evict_redemption_mint}` (Task 1); `mint_redemption` (`lib.rs:39907`); `CommunityState::insert_verified_for_test` (`community_state_crdt.rs:~800`); `community_membership::{sign_event, EventPayload, MembershipEventKind::JoinCountersign}`.

- [ ] **Step 1: Write the failing integration test**

Add to `src-tauri/tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs`, modeled on `invite_not_burned_when_handshake_fails_after_insert` (`:1929`). It seeds the "first attempt succeeded host-side but delivery failed" state deterministically (single setup, **default** acceptor config so the already-present countersign is delivered on the first poll scan), then drives one retry.

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zeb889_retry_reuses_mint_and_redeems_zombie_invite() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        // Default (nonzero) poll_deadline so the acceptor delivers the
        // already-present countersign on its first poll scan.
        let s = setup_two_party_iroh_handshake().await;

        // Build the SAME targeted invite-only URL + token as the negative test
        // (copy lines ~1955-2009: token_minted_at, invite_token sig, sealed
        // epoch key, invite_payload, invite_url). Keep `invite_token` in scope.
        // ... (identical construction) ...

        // Register the case-A invite so active_handles carries it (assert-burn target).
        s.invite_pub.register_invite(&invite_payload).await;
        let _probe = await_pkarr_record_visible(&s.pkarr_resolver, &token_sig).await;
        let invite_handle = format!("invite:{}", hex::encode(token_sig));
        assert!(
            s.pkarr_publisher.active_handles().await.contains(&invite_handle),
            "precondition: invite handle registered"
        );

        // --- Seed the "first attempt landed host-side, delivery failed" state. ---
        // 1. Mint P1 for Bob exactly as connectivity_redeem_invite_iroh_inner would,
        //    with a FIXED join_hlc so it is reproducible.
        let join_hlc = Hlc { wall_ms: 100_600, logical: 0, device_id: "bob-dev".into() };
        let p1_mint = harmony_app::mint_redemption(
            &invite_payload, s.bob_addr, s.bob_comm_sk.as_ref(), &s.bob_comm.cert, join_hlc,
        ).expect("mint P1 for bob");
        let p1 = p1_mint.bootstrap_join.clone();

        // 2. Seed Bob's registry cache with P1's mint (models the first attempt
        //    having stored it before its delivery failed).
        s.registry_bob.store_redemption_mint(token_sig, p1_mint.clone()).await;

        // 3. Seed Alice's engine with P1 + a genuine CS1 (Alice's JoinCountersign
        //    targeting P1.id, signed with her device key). insert_verified_for_test
        //    bypasses verify/precheck; that is fine — we are reproducing committed
        //    state, and CS1 carries a real Alice signature so Bob's engine accepts
        //    it on delivery (Alice's admin bootstrap is inserted by Bob's redeem).
        let cs1 = {
            use harmony_app::community_membership::{sign_event, EventPayload, MembershipEventKind};
            let payload = EventPayload {
                community_id: s.community_id,
                actor: s.alice_addr,
                at: Hlc { wall_ms: 100_700, logical: 0, device_id: "alice-dev".into() },
                kind: MembershipEventKind::JoinCountersign { target_event_id: p1.id },
                // fill remaining fields to match the module's EventPayload shape
                // (enrollment/prev/etc.) as the auto-countersign path builds them.
            };
            sign_event(&payload, s.alice_comm_sk.as_ref())
        };
        {
            let state = s.registry_alice.state_for(&s.community_id).await
                .expect("alice state exists");
            let g = state.lock().await;
            g.insert_verified_for_test(p1.clone());
            g.insert_verified_for_test(cs1);
        }

        // --- Drive the retry. Bob reuses P1 from cache → sends P1 → Alice
        //     AlreadyKnown → poll finds the seeded CS1 → delivers → Bob joins,
        //     and the acceptor burns the invite. ---
        let outcome = harmony_app::connectivity_redeem_invite_iroh_inner(
            invite_url,
            Some(Arc::clone(&s.pkarr_resolver)),
            Some(s.bob_reachability.clone()),
            Some(Arc::clone(&s.bob_ep)),
            Arc::clone(&s.bob_crdt_state),
            Arc::clone(&s.bob_hlc_tracker),
            s.bob_adopt_floor.clone(),
            "bob-dev".to_string(),
            s.bob_addr,
            Arc::clone(&s.bob_comm_sk),
            s.bob_comm.cert.clone(),
            Arc::clone(&s.registry_bob),
            s.bob_adapter_tx.clone(),
            None,
            Arc::clone(&s.bob_dm_outbox),
            Arc::clone(&s.bob_channel_log_registry),
            None,
            None,
            |_| {},
            |_p: harmony_app::NavUpdatedPayload| {},
            harmony_app::HandshakeDialConfig {
                connect_timeout: Duration::from_millis(10_000),
                open_bi_timeout: Duration::from_millis(10_000),
                response_read_timeout: Duration::from_millis(10_000),
                write_timeout: Duration::from_millis(10_000),
            },
            || Ok(()),
        ).await.expect("redeem inner must Ok");

        // Load-bearing: only reuse-of-P1 lets this converge. A fresh mint would
        // hit P6 -> EngineRejected -> no delivery -> not joined + invite live.
        assert_eq!(outcome.status, "joined", "retry converges via reused mint");
        tokio::time::sleep(Duration::from_millis(300)).await; // let burn settle
        assert!(
            !s.pkarr_publisher.active_handles().await.contains(&invite_handle),
            "ZEB-889: the single-use invite is burned once the reused-mint retry \
             delivers the countersign — no longer a permanent zombie"
        );

        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("zeb889_retry_reuses_mint timed out at 60s");
}
```

Note during implementation: match `EventPayload`'s exact field set (copy the shape the auto-counter-sign helper builds a `JoinCountersign` with — grep `MembershipEventKind::JoinCountersign` construction in `community_state_sync.rs`). If `insert_verified_for_test` is not importable from an integration test, insert via the engine's public insert path instead (e.g. drive P1 through `community_invite::handle_unicast` for a genuine auto-countersign, exposing `alice_dm_outbox`/`alice_crdt_state` on `TwoPartySetup` if needed).

- [ ] **Step 2: Run the test to verify it fails against current code**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zeb889_retry_reuses_mint)'`
Expected: FAIL — the current `connectivity_redeem_invite_iroh_inner` ignores the cache and mints fresh, so the retry mints a new id, Alice's `handle_unicast` hits P6 → `EngineRejected`, no countersign is delivered → `outcome.status != "joined"` (and the invite is not burned).

- [ ] **Step 3: Wire the mint reuse at the mint site**

In `src-tauri/src/lib.rs`, `connectivity_redeem_invite_iroh_inner`, replace the step-8' reserve-HLC+mint block (`:62816-62844`) so it reuses a cached mint. `token` is already in scope (`payload.invite_token`, unwrapped at ~`:62511`). Capture `token_sig` and an evict handle up front (before `community_registry` is moved at `:63153`):

```rust
    // ZEB-889: reuse a cached mint for this invite token if a prior attempt
    // stored one, so a failed-delivery retry re-sends the SAME bootstrap_join id
    // (the host's AlreadyKnown path then re-delivers the countersign). Only mint
    // fresh — and cache it — on the first attempt for this token.
    let token_sig = token.sig;
    let registry_evict = std::sync::Arc::clone(&community_registry);
    let minted = if let Some(cached) = community_registry.get_redemption_mint(token_sig).await {
        cached
    } else {
        let wall_now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let join_hlc = crate::dm_outbox::reserve_next_hlc_for_device(
            &hlc_tracker, &adopt_floor, &device_id, wall_now_ms,
        ).await;
        let m = match mint_redemption(&payload, self_owner, signing_key.as_ref(), &enrollment_cert, join_hlc) {
            Ok(m) => m,
            Err(e) => return Err(RedeemInviteError::new(
                RedeemInviteErrorCode::Internal, format!("mint_redemption: {e}"))),
        };
        community_registry.store_redemption_mint(token_sig, m.clone()).await;
        m
    };
```

- [ ] **Step 4: Wire eviction on a joined outcome**

In the `match result { Ok(dto) => { .. } }` arm (~`:63173`), after the DTO is known to be a joined success (before returning it), evict the cache entry using the pre-captured handle:

```rust
    // ZEB-889: the invite is burned once the join commits; drop the cached mint.
    if dto.status == "joined" {
        registry_evict.evict_redemption_mint(&token_sig).await;
    }
```

Place this so it runs on the success path regardless of the ZEB-427 fence outcome (the fence is non-fatal). Confirm `dto`'s joined sentinel string by checking how `status` is set on the returned DTO in `redeem_invite_inner_with_overrides`; use the exact value (`"joined"`).

- [ ] **Step 5: Run the integration test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zeb889_retry_reuses_mint)'`
Expected: PASS — the retry reuses P1, Alice re-delivers the seeded CS1, `outcome.status == "joined"`, and the invite handle is gone.

- [ ] **Step 6: Guard against reuse regressions on the happy path**

Run the existing iroh-redeem tests to confirm the reuse gate is transparent to first-time joins and the defer-burn negative test:

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(bob_joins_alice_via_iroh_handshake_option_a) + test(invite_not_burned_when_handshake_fails_after_insert) + test(targeted_invite_only_generate_then_redeem_roundtrip) + test(invite_only_untargeted_generate_then_redeem_roundtrip)'`
Expected: PASS (first-time redeem still mints fresh + stores; the negative test still leaves the invite live).

- [ ] **Step 7: Lint + format**

Run: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo fmt --all -- --check`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs
git commit -m "ZEB-889: reuse the minted redemption on retry so a failed-delivery join is redeemable

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D"
```

---

### Final gate (before PR)

- [ ] Full CI-parity sweep: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- [ ] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] `cargo fmt --all -- --check`
- [ ] `git status` clean (all changes committed) before declaring green.

## Self-Review notes

- **Spec coverage:** Task 1 = fix-shape §1–2 (Clone + cache + methods) + tests §1–2; Task 2 = fix-shape §3–4 (reuse + evict) + tests §3. Test §4 (different-actor-still-refused) is covered by the untouched ZEB-875 path and the existing `bob_joins_alice…` / claim tests re-run in Task 2 Step 6; no new code path weakens it, so no dedicated new test is required — but add one if the reviewer wants the regression pinned explicitly.
- **No verify_event/P6/acceptor/wire changes** — confirmed; the fix is entirely the joiner reusing its mint.
- **Type consistency:** `MintedCommunity` (not "MintedRedemption"); cache key `[u8; 64]` = `InviteToken.sig`; methods `get_redemption_mint`/`store_redemption_mint`/`evict_redemption_mint` used identically in Task 1 (define) and Task 2 (consume).
