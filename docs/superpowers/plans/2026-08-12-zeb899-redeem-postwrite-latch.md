# ZEB-899 Redeem Post-Write Latch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop the iroh first-contact redeem from reporting `inviter_unreachable` when the join request was fully written (the acceptor may have committed the join) — instead commit the ZEB-501 latched-pending Space and return `joined` + `pending: true`, which converges to full membership over CRDT sync with zero user action.

**Architecture:** Restructure steps 10–11 of `connectivity_redeem_invite_iroh_inner` (read + decode + verify the countersign response) into a single labeled block producing `delivered: Option<(chain, countersign)>`; every current post-write `return Ok(post_dial_failure_outcome())` becomes `break 'delivered None`. The one existing `redeem_invite_inner_with_overrides` call then serves both modes — `Some` is today's success path verbatim; `None` (latch mode) is the inner's existing ZEB-501/902 machinery (local PendingJoin insert + DM-outbox deposit + countersign-oneshot wait + latched-pending commit). The tail keeps the ZEB-889 mint cache in latch mode (retry idempotency) and degrades a latch-mode inner `Err` to today's outcome.

**Tech Stack:** Rust (tokio, iroh QUIC bi-streams, ciborium), existing two-party integration harness in `src-tauri/tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs`.

**Spec:** `docs/superpowers/specs/2026-08-12-zeb899-redeem-postwrite-latch-design.md`

## Global Constraints

- Cargo commands run from `src-tauri/`; always `--locked` and `--features test-fixtures` (CLAUDE.md).
- Clippy gate: `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; fmt gate: `cargo fmt --all -- --check`.
- The post-write boundary (spec §2.1): a branch latches iff BOTH `write_all`s returned `Ok`. `send.finish()` failure latches. Either `write_all` failure/timeout stays `inviter_unreachable` — do NOT move those two branches into the funnel.
- Latch mode must NOT evict the ZEB-889 mint-cache entry and must NOT change acceptor-side code.
- `redeem_timeout: None` (env-or-5s default) in BOTH modes.
- No frontend changes; no RPC/IPC signature changes; no new `RedemptionOutcome` status strings.
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D`.

---

### Task 1: Post-write latch funnel + tail + existing-pin updates

**Files:**
- Modify: `src-tauri/src/lib.rs` — steps 10–11 + tail of `connectivity_redeem_invite_iroh_inner` (currently ~64559–64847), doc comments on `RedemptionOutcome` (~62167–62240) and the fn-level flow doc (~63583–63600)
- Test (modify): `src-tauri/tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs` — `zeb889_first_attempt_caches_minted_redemption` (~2263) and `invite_not_burned_when_handshake_fails_after_insert` (~2057)

**Interfaces:**
- Consumes: `RedeemInviteOverrides` (fields `pre_minted`, `pre_delivered_countersign`, `pre_delivered_chain`, `admin_identity_pub`, `redeem_timeout`, `open_join_iroh`), `redeem_invite_inner_with_overrides` (unchanged signature), `post_dial_failure_outcome` closure, `redemption_cache_key`/`registry_evict` (already captured before the dial).
- Produces: latch-mode behavior contract for Tasks 2–3 — post-write failure ⇒ `outcome.status == "joined"`, `outcome.pending == true`, Bob Space row `pending_join_at.is_some()`, mint cache retained.

- [ ] **Step 1: Update `zeb889_first_attempt_caches_minted_redemption` to the new latch expectations**

Replace (at ~2263):

```rust
        assert_ne!(
            outcome.status, "joined",
            "the first attempt must NOT join when the acceptor CountersignTimeouts; \
             got status={:?}",
            outcome.status
        );
```

with:

```rust
        // ZEB-899: the request was fully written and Alice's handle_unicast
        // committed the PendingJoin (poll_deadline=0 only suppresses the
        // RESPONSE) — a post-write failure now latches the join as pending
        // instead of falsely reporting the inviter unreachable.
        assert_eq!(
            outcome.status, "joined",
            "ZEB-899: a post-write failure (no countersign response) must latch, \
             not report unreachable; got status={:?}",
            outcome.status
        );
        assert!(
            outcome.pending,
            "ZEB-899: the latched join must report pending=true (no countersign \
             was applied in-band)"
        );
        {
            let g = s.bob_crdt_state.lock().await;
            let row = g
                .spaces
                .get(&s.community_id)
                .expect("ZEB-899: the latch must commit Bob's owner-state Space row");
            assert!(
                row.pending_join_at.is_some(),
                "ZEB-899: the latched Space row must carry pending_join_at (greyed \
                 until the JoinCountersign converges); got {:?}",
                row.pending_join_at
            );
        }
```

- [ ] **Step 2: Update `invite_not_burned_when_handshake_fails_after_insert` the same way**

Replace (at ~2057):

```rust
        assert_ne!(
            outcome.status, "joined",
            "the redeem must NOT report joined when the acceptor CountersignTimeouts \
             before delivering the countersign; got status={:?}",
            outcome.status
        );
```

with:

```rust
        // ZEB-899: post-write failure now latches (joined + pending) instead of
        // reporting unreachable. The ZEB-874 burn assertions below are
        // unchanged — the latch is joiner-local and must not burn the invite.
        assert_eq!(
            outcome.status, "joined",
            "ZEB-899: a post-write failure must latch as joined+pending; got status={:?}",
            outcome.status
        );
        assert!(
            outcome.pending,
            "ZEB-899: the latched join must report pending=true"
        );
```

Leave every other assertion in that test (Alice's PendingJoin insert proof, invite-still-active) untouched.

- [ ] **Step 3: Run both tests to verify they fail on current code**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zeb889_first_attempt_caches_minted_redemption) | test(invite_not_burned_when_handshake_fails_after_insert)'`
Expected: both FAIL — current code returns `inviter_unreachable` for the no-response handshake.

- [ ] **Step 4: Implement the funnel in `connectivity_redeem_invite_iroh_inner`**

Replace the region from `// 10. Read response, bounded by dial_config.response_read_timeout.` through the `conn` cleanup + `overrides` construction + final `match result` (currently ~64568–64847) with the following. The read/decode/verify logic is byte-identical except each post-write `return Ok(post_dial_failure_outcome())` becomes `break 'delivered None` (with a labelled `conn.close` added to the branches that previously relied on Drop):

```rust
    // 10.–11. Read + decode + verify the response, collapsed into `delivered`.
    // ZEB-899: `None` = post-write failure — the request was FULLY written
    // (both write_alls Ok), so the acceptor may already have committed the
    // join host-side (it inserts the PendingJoin and countersign BEFORE
    // writing the response). These branches therefore no longer return
    // `inviter_unreachable`; they fall through to the latch call below.
    let delivered: Option<(
        Vec<crate::community_membership::SignedMembershipEvent>,
        crate::community_membership::SignedMembershipEvent,
    )> = 'delivered: {
        let read_response = async {
            let mut len_buf = [0u8; 4];
            recv.read_exact(&mut len_buf)
                .await
                .map_err(|e| format!("read length-prefix: {e}"))?;
            let len = crate::iroh_framing::decode_len_prefix(
                len_buf,
                crate::iroh_invite_acceptor::HANDSHAKE_MAX_PACKET_LEN,
                crate::iroh_framing::Endian::Le,
                false,
            )
            .map_err(|e| format!("response length out of bounds: len={} max={}", e.len, e.max))?;
            let mut body = vec![0u8; len];
            recv.read_exact(&mut body)
                .await
                .map_err(|e| format!("read response body: {e}"))?;
            Ok::<Vec<u8>, String>(body)
        };
        let response_bytes =
            match tokio::time::timeout(dial_config.response_read_timeout, read_response).await {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => {
                    tracing::warn!(
                        error = %e,
                        "ZEB-325 Phase 2c option A: handshake response read failed"
                    );
                    conn.close(0u32.into(), b"response-read-failed");
                    break 'delivered None;
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        timeout_ms = dial_config.response_read_timeout.as_millis() as u64,
                        "ZEB-325 Phase 2c option A: handshake response timeout (read)"
                    );
                    conn.close(0u32.into(), b"response-read-timeout");
                    break 'delivered None;
                }
            };

        // A legacy/admin acceptor returns a single SignedMembershipEvent (CBOR
        // map, major type 5); a ZEB-911 witness returns an array (major type 4)
        // of [admission chain ..., countersign]. (Bounds rationale unchanged —
        // see ZEB-911 / Qodo r3.)
        const ZEB911_MAX_CHAIN_EVENTS: usize =
            crate::iroh_invite_acceptor::HANDSHAKE_MAX_PACKET_LEN
                / crate::community_membership::MIN_SIGNED_EVENT_ENCODED_LEN;
        let (chain, countersign): (
            Vec<crate::community_membership::SignedMembershipEvent>,
            crate::community_membership::SignedMembershipEvent,
        ) = if response_bytes.first().map(|b| b >> 5) == Some(4) {
            let mut events: Vec<crate::community_membership::SignedMembershipEvent> =
                match ciborium::from_reader(response_bytes.as_slice()) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "ZEB-911: chain response CBOR decode failed");
                        conn.close(0u32.into(), b"response-decode-failed");
                        break 'delivered None;
                    }
                };
            if events.len() < 2 || events.len() > ZEB911_MAX_CHAIN_EVENTS {
                tracing::warn!(
                    len = events.len(),
                    "ZEB-911: chain response length out of bounds"
                );
                conn.close(0u32.into(), b"response-decode-failed");
                break 'delivered None;
            }
            if events.iter().any(|e| e.community_id != minted.community_id) {
                tracing::warn!("ZEB-911: chain response carries a foreign community_id");
                conn.close(0u32.into(), b"response-decode-failed");
                break 'delivered None;
            }
            let cs = events.pop().expect("len >= 2 checked above");
            (events, cs)
        } else {
            let cs: crate::community_membership::SignedMembershipEvent =
                match ciborium::from_reader(response_bytes.as_slice()) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "ZEB-325 Phase 2c option A: response CBOR decode failed"
                        );
                        conn.close(0u32.into(), b"response-decode-failed");
                        break 'delivered None;
                    }
                };
            (Vec::new(), cs)
        };
        let target_ok = matches!(
            &countersign.kind,
            crate::community_membership::MembershipEventKind::JoinCountersign { target_event_id }
            if *target_event_id == minted.bootstrap_join.id
        );
        if !target_ok {
            tracing::warn!(
                "ZEB-325 Phase 2c option A: response is not a JoinCountersign for our bootstrap_join.id"
            );
            conn.close(0u32.into(), b"countersign-mismatch");
            break 'delivered None;
        }
        if countersign.community_id != minted.community_id {
            tracing::warn!("ZEB-325 Phase 2c option A: countersign community_id mismatch");
            conn.close(0u32.into(), b"countersign-mismatch");
            break 'delivered None;
        }
        Some((chain, countersign))
    };

    // Connection cleanup. On the delivered path this is the existing
    // "handshake-complete" close (rationale comment unchanged — see ZEB-325
    // PR #159 R4-4); the None branches each closed with their own label above
    // (a repeat close would be a no-op anyway).
    drop(send);
    drop(recv);
    if delivered.is_some() {
        conn.close(0u32.into(), b"handshake-complete");
    }
    drop(conn);

    // 12. ONE call site for both modes (ZEB-899). `delivered == Some`: the
    // pre-delivered countersign resolves the inner's oneshot immediately —
    // today's success path, unchanged. `delivered == None` (LATCH MODE): the
    // request was fully written, so the acceptor may have committed; run the
    // SAME inner without a countersign — it inserts the local PendingJoin,
    // deposits it via the DM outbox (the host can recover it even if the
    // handshake request never arrived), awaits the countersign oneshot for
    // the redeem window (a live Zenoh session can still complete the join
    // in-band → pending=false), and otherwise commits the ZEB-501
    // latched-pending Space (pending=true, greyed in nav) that converges to
    // full membership over CRDT sync — instead of falsely reporting
    // `inviter_unreachable` while the community already contains us.
    let latch_mode = delivered.is_none();
    let overrides = RedeemInviteOverrides {
        pre_minted: Some(minted),
        pre_delivered_countersign: delivered.as_ref().map(|(_, cs)| cs.clone()),
        pre_delivered_chain: delivered.map(|(chain, _)| chain).unwrap_or_default(),
        admin_identity_pub: Some(admin_id_pub),
        // ZEB-501: production uses the env-or-5s default in both modes (the
        // pre-delivered countersign resolves well within it; latch mode gives
        // an existing Zenoh session that long to complete the join in-band).
        redeem_timeout: None,
        // Invite-only path — no open first-contact dial.
        open_join_iroh: None,
    };
    let result = redeem_invite_inner_with_overrides(
        invite_url,
        crdt_state,
        hlc_tracker,
        adopt_floor,
        device_id,
        self_owner,
        signing_key,
        enrollment_cert,
        community_registry,
        community_adapter_tx,
        transport_epoch_rx,
        dm_outbox,
        channel_log_registry,
        fence_check,
        identity_dir,
        overrides,
    )
    .await;

    match result {
        Ok(dto) => {
            if latch_mode {
                // ZEB-899: do NOT evict the ZEB-889 mint cache — no countersign
                // was applied and the invite was not burned (ZEB-874 burns only
                // after a delivered response). The cached mint is what makes a
                // later manual retry re-send the SAME bootstrap_join id and hit
                // the host's AlreadyKnown-retransmit path, instead of minting
                // fresh and dying on the verify_event P6 already-engaged reject.
            } else if let Some(key) = redemption_cache_key {
                // ZEB-889: the join committed — the acceptor delivered the
                // countersign (this arm is reached only after the pre-delivered
                // countersign was applied) and burned the single-use invite.
                // Drop the cached mint; no further retry is possible or needed.
                // (A never-completing redemption keeps its entry until the TTL
                // window elapses — bounded, one per distinct invite redeemed
                // this session.)
                registry_evict.evict_redemption_mint(&key).await;
            }
            // ZEB-427: durable-on-commit fence (rationale unchanged) — in latch
            // mode this is what makes the latched-pending Space survive a
            // non-graceful exit, exactly like a full join.
            match sync_engine.as_ref() {
                Some(engine) => {
                    fence_owner_state_flush(
                        engine,
                        OWNER_STATE_FENCE_TIMEOUT,
                        "connectivity_redeem_invite_iroh",
                        &dto.community_id,
                    )
                    .await;
                }
                None => {
                    tracing::warn!(
                        community_id = %dto.community_id,
                        "connectivity_redeem_invite_iroh: no SyncEngine handle — the joined \
                         community's owner-state Space is NOT persisted until the next \
                         unrelated owner-state flush (ZEB-427)"
                    );
                }
            }
            // Stage 5/5 + nav emit (rationale unchanged; `pending` carries the
            // latch state — the dialog renders the honest ZEB-902 copy).
            emit_stage(RedemptionStage::Joined);
            nav_emit_sink(NavUpdatedPayload {
                action: "added",
                space_id: dto.community_id.clone(),
                kind: "community",
                name: dto.community_name.clone(),
                members: None,
                parent_id: None,
                pending: Some(dto.pending),
            });
            Ok(RedemptionOutcome::joined(dto.community_id, dto.pending))
        }
        Err(e) if latch_mode => {
            // ZEB-899: the latch could not commit — nothing landed locally, so
            // degrade to the honest pre-write outcome (which keeps the LAN
            // fallback affordance). NOT `join_failed`: that status asserts the
            // inviter was reached and suppresses the fallback, neither of which
            // is known on this arm.
            tracing::warn!(
                error = %e,
                community_id = %hex::encode(payload.community_id.0),
                "ZEB-899: post-write latch commit failed; degrading to the unreachable outcome"
            );
            Ok(post_dial_failure_outcome())
        }
        Err(e) => {
            // ZEB-325 PR #159 F1 (rationale unchanged): countersign was
            // delivered and verified, so the failure is local.
            tracing::warn!(
                error = %e,
                community_id = %hex::encode(payload.community_id.0),
                "ZEB-325 Phase 2c option A: redeem_invite_inner_with_overrides failed \
                 after iroh handshake countersign delivery"
            );
            Ok(RedemptionOutcome::join_failed(hex::encode(
                payload.community_id.0,
            )))
        }
    }
}
```

Preserve the long-form rationale comments from the current code where the plan block abbreviates them (`read_response` framing, ZEB911 bounds, R4-4 close rationale, ZEB-427 fence, R3-1 nav emit) — the abbreviations above mark them "(rationale unchanged)".

- [ ] **Step 5: Update the doc comments to match the new contract**

1. `RedemptionOutcome.status` doc (~62175): change the `"inviter_unreachable"` bullet's tail to say the status now means the request was NOT fully written (pkarr/dial/open_bi/request-write failed) — post-write failures latch as `"joined"` + `pending` (ZEB-899).
2. Same for the `"no_member_reachable"` bullet (witness rung, pre-write only).
3. `RedemptionOutcome.pending` doc (~62203): add the iroh post-write latch (ZEB-899) as a second source alongside the ZEB-902 deposit.
4. `unreachable()` / `no_member_reachable()` helper docs (~62214–62240): note "pre-write only since ZEB-899".
5. Fn-level flow doc steps 10–12 (~63583–63600): describe `delivered` and latch mode.

- [ ] **Step 6: Run the updated tests + neighboring pins**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zeb889_) | test(invite_not_burned_when_handshake_fails_after_insert) | test(zeb911_) | test(bob_joins_alice_via_iroh_handshake_option_a) | test(zeb427_iroh_redeem_fences_owner_state_space_to_disk)'`
Expected: ALL PASS — the two updated tests go green on the latch; `zeb889_retry_reuses_mint_and_redeems_zombie_invite` (seeded, delivered-mode retry) stays green; the zeb911 pre-write classification pins stay green; the option-A full roundtrip and ZEB-427 fence tests stay green.

- [ ] **Step 7: fmt + clippy + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
git add src-tauri/src/lib.rs src-tauri/tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs
git commit -m "ZEB-899: latch post-write iroh redeem failures as pending joins

A fully-written join request may already be committed host-side (the
acceptor inserts PendingJoin + countersign before responding), so
post-write failures no longer report inviter_unreachable: they run the
same inner without a countersign, committing the ZEB-501 latched-pending
Space (joined+pending=true, ZEB-902 rendering) that converges over CRDT
sync. Mint cache retained in latch mode for retry idempotency."
```

(with the standard trailers)

### Task 2: Retry-after-latch pin (extend `zeb889_retry_reuses_mint_and_redeems_zombie_invite`)

**Files:**
- Test (modify): `src-tauri/tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs` — `zeb889_retry_reuses_mint_and_redeems_zombie_invite` (~2300)

**Interfaces:**
- Consumes: Task 1's latch contract; `redeem_invite_inner_with_overrides` + `RedeemInviteOverrides { pre_minted, redeem_timeout, ..Default::default() }` (the ZEB-501 LAN-latch seam, pinned by `redeem_invite_only_commits_pending_join_when_inviter_unreachable`).
- Produces: the spec §2.4 guarantee — a retry over an existing latched-pending Space completes to a full member with the cached mint.

- [ ] **Step 1: Seed Bob's latched-pending Space between the existing cache/Alice seeds and the retry**

Insert after the Alice-engine seed block (`g.insert_verified_for_test(cs1);` + closing brace, ~2398) and before the `// --- Drive the retry` comment:

```rust
        // --- ZEB-899: seed Bob's LATCHED-PENDING Space — the state the
        //     post-write latch now commits on a failed first attempt (drive
        //     the same inner the latch call uses: pre_minted, no countersign,
        //     short redeem window). The retry below then runs over an
        //     EXISTING pending Space + spawned engine, which is the real
        //     retry-after-latch shape.
        let latch_dto = harmony_app::redeem_invite_inner_with_overrides(
            invite_url.clone(),
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
            || Ok(()),
            None,
            harmony_app::RedeemInviteOverrides {
                pre_minted: Some(p1_mint.clone()),
                redeem_timeout: Some(Duration::from_secs(1)),
                ..Default::default()
            },
        )
        .await
        .expect("ZEB-899: the latch seed must commit a pending Space, not Err");
        assert!(
            latch_dto.pending,
            "ZEB-899 precondition: the seeded latch must be pending; got {latch_dto:?}"
        );
        {
            let g = s.bob_crdt_state.lock().await;
            let row = g
                .spaces
                .get(&s.community_id)
                .expect("ZEB-899 precondition: latched Space row exists before the retry");
            assert!(row.pending_join_at.is_some());
        }
```

(`invite_url` gains a `.clone()` at the seed call; the retry keeps consuming the original.)

- [ ] **Step 2: Extend the post-retry assertions**

After the existing `assert_eq!(outcome.status, "joined", ...)` add:

```rust
        // ZEB-899: the retry delivered the countersign in-band over the
        // EXISTING latched Space/engine — the join is fully ratified now.
        assert!(
            !outcome.pending,
            "ZEB-899: the reused-mint retry must complete the latched join \
             (pending=false); got {outcome:?}"
        );
```

The existing burn + cache-evict assertions stay — they now also pin that completion (not the latch) is what evicts.

- [ ] **Step 3: Run the test**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zeb889_retry_reuses_mint_and_redeems_zombie_invite)'`
Expected: PASS. If it fails inside the retry's inner (e.g., a reject on the pre-existing Space row or a pending-redemption oneshot conflict), that is an implementation gap in the retry-over-latched-state path — fix it in `redeem_invite_inner_with_overrides`'s invite-only branch (treat AlreadyKnown/existing-Space as convergent, mirroring the ZEB-436 adoption tolerance), then re-run.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs
git commit -m "ZEB-899: pin retry-after-latch — cached mint completes a latched-pending join"
```

(with the standard trailers)

### Task 3: Latch-mode local failure degrades to today's outcome

**Files:**
- Test (create in existing file): `src-tauri/tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs` — new test after `zeb889_first_attempt_caches_minted_redemption`

**Interfaces:**
- Consumes: Task 1's `Err(e) if latch_mode` arm; the harness (`setup_two_party_iroh_handshake_with_config`, `zeb889_build_targeted_invite`, `await_pkarr_record_visible`).
- Produces: pin for spec §2.3's degrade rule (latch `Err` → `inviter_unreachable`, NOT `join_failed`/`joined`; nothing committed; mint stays cached).

- [ ] **Step 1: Write the test**

```rust
/// ZEB-899: when the post-write LATCH itself cannot commit (here: the
/// generation fence rejects — node stopped mid-redeem), nothing landed
/// locally, so the outcome degrades to the honest legacy classification
/// (`inviter_unreachable`, which keeps the LAN-fallback affordance) — NOT
/// `join_failed` (which asserts the inviter was reached and suppresses the
/// fallback) and NOT `joined`. The mint stays cached for a later retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zeb899_latch_commit_failure_degrades_to_unreachable() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        // poll_deadline = 0 → no response is written → the joiner enters latch
        // mode after its response read fails.
        let s = setup_two_party_iroh_handshake_with_config(
            harmony_app::iroh_invite_acceptor::HandshakeAcceptorConfig {
                io_deadline: Duration::from_millis(10_000),
                poll_deadline: Duration::ZERO,
                poll_interval: Duration::from_millis(20),
            },
        )
        .await;

        let (invite_payload, invite_url, token_sig) = zeb889_build_targeted_invite(&s);
        let cache_key = invite_payload
            .redemption_mint_cache_key()
            .expect("payload cache key");
        s.invite_pub.register_invite(&invite_payload).await;
        let _probe = await_pkarr_record_visible(&s.pkarr_resolver, &token_sig).await;

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
                response_read_timeout: Duration::from_millis(2_000),
                write_timeout: Duration::from_millis(10_000),
            },
            // The generation fence rejects: the ONLY fence evaluation on this
            // run happens inside the latch-mode inner (the handshake never
            // reaches the delivered path), so a constant Err drives the
            // latch-commit-failure arm deterministically.
            || {
                Err(harmony_app::RedeemInviteError::new(
                    harmony_app::RedeemInviteErrorCode::GenerationChanged,
                    "forced fence failure (ZEB-899 latch-degrade test)".to_string(),
                ))
            },
        )
        .await
        .expect("connectivity_redeem_invite_iroh_inner must Ok (errors → non-joined status)");

        assert_eq!(
            outcome.status, "inviter_unreachable",
            "ZEB-899: a failed latch commit must degrade to the legacy unreachable \
             outcome (fallback affordance intact), not join_failed/joined; got {:?}",
            outcome.status
        );
        assert!(
            s.bob_crdt_state
                .lock()
                .await
                .spaces
                .get(&s.community_id)
                .is_none(),
            "ZEB-899: a failed latch must not leave a Space row (rollback)"
        );
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(
            s.registry_bob
                .get_redemption_mint(cache_key, now_ms)
                .await
                .is_some(),
            "ZEB-899: the mint stays cached for a later retry even when the latch fails"
        );

        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("zeb899_latch_commit_failure_degrades_to_unreachable timed out at 60s");
}
```

If `RedeemInviteError::new` / `RedeemInviteErrorCode` are not visible to integration tests, add them to the crate-root re-exports in `src-tauri/src/lib.rs` next to the existing `pub use` items rather than changing their definitions.

- [ ] **Step 2: Run the test**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zeb899_latch_commit_failure_degrades_to_unreachable)'`
Expected: PASS (pins the Task-1 arm; a `join_failed` result here means the `Err(e) if latch_mode` guard is missing/misordered).

- [ ] **Step 3: Commit**

```bash
git add src-tauri/tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs src-tauri/src/lib.rs
git commit -m "ZEB-899: pin latch-commit-failure degrade to the legacy unreachable outcome"
```

(with the standard trailers)

### Task 4: Outcome-pin audit, full gates, PR

**Files:**
- No planned source changes (audit may surface stragglers)

- [ ] **Step 1: Audit every remaining outcome pin against the new boundary**

Run: `rg -n "inviter_unreachable|no_member_reachable" src-tauri/tests src-tauri/src src e2e-harness`
For each hit, confirm it pins a PRE-write failure (resolve/connect/ladder/open_bi/write) or is a comment/copy string. Known-good: the zeb911 trio (pre-write), `redeem_invite_only_commits_pending_join_when_inviter_unreachable` (LAN path), `RedeemInviteDialog`/`redeem-invite-errors` copy (unchanged statuses), e2e-harness driver retry loop (a latched `joined`+pending ends its retry loop as success — acceptable and noted in the PR body). Fix anything that pins a post-write failure to the old outcome.

- [ ] **Step 2: Full workspace sweep + gates (working tree committed)**

```bash
cd src-tauri && cargo fmt --all -- --check \
  && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
  && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: clean fmt/clippy; full sweep green (~5 min shard-parallel locally; budget 10+). `git status` must be clean before declaring green.

- [ ] **Step 3: Push branch, open PR, trigger review**

PR title: `ZEB-899: latch post-write iroh redeem failures as pending joins (stop the false "inviter unreachable")`. Body: spec/plan links, the audit-comment link, the three behavior changes, testing summary, `Closes ZEB-899`, standard footer. Fire `@coderabbitai review` exactly once at open; then converge per the standing protocol (ALL three comment buckets, bundle fixes, ONE push per round, never merge).
