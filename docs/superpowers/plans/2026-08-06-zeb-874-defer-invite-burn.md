# ZEB-874 Tier 1: defer the invite burn — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (inline) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Move the single-use-invite burn from `community_invite::handle_unicast` (on local insert) to `iroh_invite_acceptor::handle_invite_handshake_inbound` (after the countersign response is successfully written), so a post-insert delivery failure no longer consumes the invite.

**Architecture:** `handle_unicast` becomes verify+insert only (drops its publisher param and both `unregister_invite` blocks). The acceptor — its sole live caller — burns the invite after `write_len_prefixed_cbor(...).await?`, using the publisher field and `signed.invite_token.sig` already in scope.

**Tech Stack:** Rust, tokio, iroh QUIC, pkarr, `cargo nextest`.

## Global Constraints

- All Rust commands run from `src-tauri/`. CI gates: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Frontend (repo root): `npx tsc --noEmit` (no frontend change here, but the gate stays green).
- No wire-protocol change; no phantom-member reversal (those are ZEB-874 Tier 2/3, out of scope).
- Burn observability in tests is `pkarr_publisher.active_handles()` NOT containing `invite:{hex(sig)}` — never relay re-resolve (the PUT record lingers past TTL).

---

### Task 1: Move the burn to post-delivery + happy-path regression coverage

**Files:**
- Modify: `src-tauri/src/community_invite.rs` (`handle_unicast`: drop param + both burn blocks)
- Modify: `src-tauri/src/iroh_invite_acceptor.rs` (call site, burn-after-write, field doc)
- Modify: `src-tauri/tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs` (add burn assertion to the targeted happy-path test; relabel the untargeted test's existing assertion)

**Interfaces:**
- Produces: `handle_unicast<H>(community_registry, dm_outbox, _crdt_state, packet_bytes, app)` — the `pkarr_invite_publisher` param is removed. Sole caller is the acceptor.

- [ ] **Step 1: `community_invite.rs` — drop both burn blocks.** Both `InsertOutcome::Inserted` arms (new-shape ~`:2303`, legacy ~`:2374`) are byte-identical; replace both:

  Replace (all occurrences):
  ```rust
              Ok(crate::community_state_crdt::InsertOutcome::Inserted) => {
                  // ZEB-367: invite consumed — stop the case-A pkarr publication
                  // (single-use; frees the DHT slot) now that the Join is inserted.
                  if let Some(pubr) = pkarr_invite_publisher {
                      pubr.unregister_invite(&signed.invite_token.sig).await;
                  }
                  Ok(())
              }
  ```
  with:
  ```rust
              Ok(crate::community_state_crdt::InsertOutcome::Inserted) => {
                  // ZEB-874: the single-use invite is NO LONGER burned here. The
                  // burn moved to the acceptor, gated behind a successful
                  // countersign-response write, so a post-insert delivery failure
                  // leaves the invite live for retry. See
                  // iroh_invite_acceptor::handle_invite_handshake_inbound.
                  Ok(())
              }
  ```

- [ ] **Step 2: `community_invite.rs` — drop the now-unused param + its `#[allow]`.**

  Replace:
  ```rust
  #[allow(clippy::too_many_arguments)] // 5 args — clippy default is 7; kept here for symmetry with future expansion.
  pub async fn handle_unicast<H: AppHandleEmit>(
      community_registry: &std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
      dm_outbox: &std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
      _crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
      packet_bytes: Vec<u8>,
      app: Option<&H>,
      pkarr_invite_publisher: Option<
          &std::sync::Arc<crate::pkarr_invite_publisher::PkarrInvitePublisher>,
      >,
  ) -> Result<(), CommunityInviteVerifyError> {
  ```
  with:
  ```rust
  pub async fn handle_unicast<H: AppHandleEmit>(
      community_registry: &std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
      dm_outbox: &std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
      _crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
      packet_bytes: Vec<u8>,
      app: Option<&H>,
  ) -> Result<(), CommunityInviteVerifyError> {
  ```

- [ ] **Step 3: `iroh_invite_acceptor.rs` — drop the arg at the call site (~`:454`).**

  Replace:
  ```rust
          let unicast_result = community_invite::handle_unicast(
              &self.community_registry,
              &self.dm_outbox,
              &self.crdt_state,
              packet_bytes,
              self.app.as_deref(),
              self.pkarr_invite_publisher.as_ref(),
          )
          .await;
  ```
  with:
  ```rust
          let unicast_result = community_invite::handle_unicast(
              &self.community_registry,
              &self.dm_outbox,
              &self.crdt_state,
              packet_bytes,
              self.app.as_deref(),
          )
          .await;
  ```

- [ ] **Step 4: `iroh_invite_acceptor.rs` — burn after the write (~`:518`).**

  Replace:
  ```rust
          self.write_len_prefixed_cbor(&mut send, &countersign)
              .await?;

          Ok(bootstrap_join_id)
  ```
  with:
  ```rust
          self.write_len_prefixed_cbor(&mut send, &countersign)
              .await?;

          // ZEB-874: burn the single-use invite ONLY now that the countersign
          // response has been handed to the transport. The `?` above means any
          // delivery failure (CountersignTimeout returns earlier; a failed
          // write / io-timeout / lost connection returns here) leaves the invite
          // live for the joiner to retry. Fires on both Inserted and
          // AlreadyKnown (a retransmit re-delivers the countersign); the
          // unregister is idempotent, so a repeat burn is a safe no-op. Moved
          // here from `community_invite::handle_unicast`, which used to burn on
          // local insert before delivery was known.
          if let Some(pubr) = self.pkarr_invite_publisher.as_ref() {
              pubr.unregister_invite(&signed.invite_token.sig).await;
          }

          Ok(bootstrap_join_id)
  ```

- [ ] **Step 5: `iroh_invite_acceptor.rs` — correct the field doc (~`:203`).**

  Replace:
  ```rust
      /// ZEB-367: case-A pkarr publisher handle. When `Some`, a successful
      /// invite consumption (PendingJoin / counter-signed Join `Inserted`)
      /// unregisters the invite's case-A publication via
      /// `handle_unicast`, freeing the DHT slot. `None` in tests.
  ```
  with:
  ```rust
      /// ZEB-367 / ZEB-874: case-A pkarr publisher handle. When `Some`, the
      /// invite's case-A publication is unregistered (freeing the DHT slot) once
      /// the countersign response has been successfully written back to the
      /// joiner — see `handle_invite_handshake_inbound`. ZEB-874 moved this burn
      /// off `handle_unicast`'s local insert so a failed delivery no longer
      /// consumes the single-use invite. `None` in tests.
  ```

- [ ] **Step 6: add the burn regression assertion to the targeted happy-path test.** In `bob_joins_alice_via_iroh_handshake_option_a`, immediately before `s.publisher_handle.abort();` (~`:1036`), insert:
  ```rust
          // ZEB-874 regression: the single-use invite must be burned once the
          // handshake completes — now the burn fires in the acceptor AFTER the
          // countersign response is written, not in handle_unicast on insert. The
          // deterministic signal is the publisher dropping the case-A handle from
          // its active set (re-resolving the mock relay is unreliable; the PUT
          // record lingers until TTL — see the untargeted roundtrip's note).
          let invite_handle = format!("invite:{}", hex::encode(token_sig));
          let mut handle_gone = false;
          for _ in 0..50 {
              tokio::time::sleep(Duration::from_millis(100)).await;
              if !s
                  .pkarr_publisher
                  .active_handles()
                  .await
                  .contains(&invite_handle)
              {
                  handle_gone = true;
                  break;
              }
          }
          assert!(
              handle_gone,
              "ZEB-874: the case-A invite publication must be unregistered (handle \
               {invite_handle:?} dropped from active_handles) within 5s of the \
               successful handshake (acceptor burns after the countersign write)"
          );
  ```

- [ ] **Step 7: relabel the untargeted test's existing burn note.** In `invite_only_untargeted_generate_then_redeem_roundtrip`, update the comment that currently attributes the unregister to `handle_unicast → unregister_invite on Inserted`.

  Replace:
  ```rust
          // ZEB-367 unregister-on-consume (e2e). Alice's acceptor was built with
          // Some(invite_pub); once Bob's PendingJoin lands as `Inserted` on
          // Alice's accept side, handle_unicast calls
          // unregister_invite(&invite_token.sig), which removes the case-A
          // publication from the publisher's active set so it stops republishing.
  ```
  with:
  ```rust
          // ZEB-367 / ZEB-874 unregister-on-consume (e2e). Alice's acceptor was
          // built with Some(invite_pub); once the handshake completes and the
          // acceptor has written the countersign back, it calls
          // unregister_invite(&invite_token.sig) (ZEB-874 moved this burn off
          // handle_unicast's insert), removing the case-A publication from the
          // publisher's active set so it stops republishing.
  ```
  Also update the trailing assertion message in the same block: change `(handle_unicast → unregister_invite on Inserted)` to `(acceptor unregisters after the countersign write)`.

- [ ] **Step 8: gate + commit.** From `src-tauri/`: `cargo fmt --all`, then the three CI gates (clippy `--all-targets`, `cargo check`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`). Scope the nextest run during iteration to `-E 'test(pkarr_iroh_redeem_full)'` first, then the full sweep. Commit:
  ```bash
  git add -A && git commit -m "ZEB-874: defer invite burn to after countersign delivery"
  ```

---

### Task 2: Deterministic negative test — a post-insert failure must not burn

**Files:**
- Modify: `src-tauri/tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs` (parameterize the harness config; add the negative test)

**Interfaces:**
- Consumes: `setup_two_party_iroh_handshake_with_config(HandshakeAcceptorConfig) -> TwoPartySetup` (new); `setup_two_party_iroh_handshake()` becomes a thin wrapper over it with `default_acceptor_config()`.

> **Amendment (review, PR #619):** the original `poll_deadline = 0` test relied on the async auto-countersign not landing before the acceptor's first poll scan — a genuine scheduler race that CI reproduced. Fixed deterministically by a **production change in Task 1's acceptor**: the poll loop now checks the deadline **before** scanning state, so a zero deadline times out on the first iteration without ever scanning (production's 10s deadline is unaffected — the ordering is a no-op there). The negative test additionally asserts Alice inserted Bob's `PendingJoin`, proving the failure is genuinely post-insert. The embedded comment/asserts below reflect the pre-amendment draft; the authoritative code is the merged test.

- [ ] **Step 1: parameterize the harness on the acceptor config.** Replace the helper opener:
  ```rust
  /// Stand up the full two-party iroh-handshake harness (identities, endpoints,
  /// Alice's engine + acceptor, Bob's redeem deps, mock pkarr relay). Identical
  /// for the targeted and untargeted roundtrip tests; the only thing that
  /// differs between them is the invite payload each constructs afterward.
  async fn setup_two_party_iroh_handshake() -> TwoPartySetup {
  ```
  with:
  ```rust
  /// The acceptor config the four happy-path roundtrips use (short wall-clock
  /// budgets so a stall surfaces as a timeout in seconds, not at the outer 60s).
  fn default_acceptor_config() -> harmony_app::iroh_invite_acceptor::HandshakeAcceptorConfig {
      harmony_app::iroh_invite_acceptor::HandshakeAcceptorConfig {
          io_deadline: Duration::from_millis(10_000),
          poll_deadline: Duration::from_millis(10_000),
          poll_interval: Duration::from_millis(20),
      }
  }

  /// Thin wrapper: the happy-path roundtrips use the default acceptor config.
  async fn setup_two_party_iroh_handshake() -> TwoPartySetup {
      setup_two_party_iroh_handshake_with_config(default_acceptor_config()).await
  }

  /// Stand up the full two-party iroh-handshake harness (identities, endpoints,
  /// Alice's engine + acceptor, Bob's redeem deps, mock pkarr relay). The
  /// `acceptor_config` lets the ZEB-874 negative test force a deterministic
  /// post-insert failure (`poll_deadline = 0` → CountersignTimeout before the
  /// countersign write).
  async fn setup_two_party_iroh_handshake_with_config(
      acceptor_config: harmony_app::iroh_invite_acceptor::HandshakeAcceptorConfig,
  ) -> TwoPartySetup {
  ```

- [ ] **Step 2: thread the config into the acceptor.** Replace the inline config struct in the `with_config` call:
  ```rust
              harmony_app::iroh_invite_acceptor::HandshakeAcceptorConfig {
                  io_deadline: Duration::from_millis(10_000),
                  poll_deadline: Duration::from_millis(10_000),
                  poll_interval: Duration::from_millis(20),
              },
  ```
  with:
  ```rust
              acceptor_config,
  ```

- [ ] **Step 3: add the negative test at end of file.** Append:
  ```rust
  // ────────────────────────────────────────────────────────────────────────────
  // ZEB-874 Tier 1: a redeem that fails AFTER the host's local insert must NOT
  // burn the single-use invite. Alice's acceptor runs with poll_deadline=0, so it
  // returns CountersignTimeout immediately after handle_unicast inserts Bob's
  // PendingJoin — the auto-counter-sign task spawns but cannot land in the
  // microseconds before the first poll check. This is exactly a post-insert,
  // pre-delivery failure: pre-ZEB-874 handle_unicast had already burned the invite
  // on the insert; post-ZEB-874 the burn lives after the countersign write, which
  // is never reached, so the invite stays live.
  //
  // Determinism/no-flake contract: the ONLY way the race is lost is if the async
  // resolve+sign+insert completes within microseconds of tokio::spawn. If it ever
  // did, the acceptor would write, burn, and Bob would join — tripping BOTH asserts
  // LOUDLY. On broken (pre-fix) code the handle is burned on insert, also tripping
  // the assert loudly. There is no path where this test passes while the code is
  // wrong; the worst case is a loud, re-runnable flake, never a false green.
  // ────────────────────────────────────────────────────────────────────────────

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn invite_not_burned_when_handshake_fails_after_insert() {
      let _ = tracing_subscriber::fmt()
          .with_env_filter(
              tracing_subscriber::EnvFilter::try_from_default_env()
                  .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
          )
          .with_test_writer()
          .try_init();

      harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

      tokio::time::timeout(Duration::from_secs(60), async {
          // poll_deadline = 0 → the acceptor CountersignTimeouts right after the
          // insert, before writing any countersign back.
          let s = setup_two_party_iroh_handshake_with_config(
              harmony_app::iroh_invite_acceptor::HandshakeAcceptorConfig {
                  io_deadline: Duration::from_millis(10_000),
                  poll_deadline: Duration::ZERO,
                  poll_interval: Duration::from_millis(20),
              },
          )
          .await;

          // Targeted invite-only URL — same construction as
          // `bob_joins_alice_via_iroh_handshake_option_a`.
          let token_minted_at = Hlc {
              wall_ms: 100_500,
              logical: 0,
              device_id: "alice-dev".into(),
          };
          let invite_token_unsigned = InviteToken {
              inviter: s.alice_addr,
              invitee_hint: Some(s.bob_addr),
              minted_at: token_minted_at.clone(),
              expires_at: None,
              sig: [0u8; 64],
          };
          let token_payload_bytes =
              canonical_invite_token_bytes(&invite_token_unsigned).expect("canonical token bytes");
          let token_sig: [u8; 64] = s.alice_comm_sk.sign(&token_payload_bytes).to_bytes();
          let invite_token = InviteToken {
              inviter: s.alice_addr,
              invitee_hint: Some(s.bob_addr),
              minted_at: token_minted_at,
              expires_at: None,
              sig: token_sig,
          };
          let bob_x25519_pub = {
              let verifying_bytes = s.bob_comm_sk.verifying_key().to_bytes();
              harmony_app::dm_signing::ed25519_pub_to_x25519(&verifying_bytes)
                  .expect("bob_comm ed25519→x25519")
          };
          let sealed_epoch_key = harmony_app::dm_signing::seal_to_owner(
              &bob_x25519_pub,
              s.alice_minted.membership_key.as_bytes(),
          )
          .expect("seal epoch key to bob");
          let invite_payload = CommunityInvitePayload {
              inviter_signer_certs: Vec::new(),
              community_id: s.community_id,
              epoch_snapshot: InviteEpochSnapshot {
                  epoch: 0,
                  sealed_epoch_key: Vec::new(),
                  sealed_epoch_keys: vec![sealed_epoch_key],
                  state_snapshot: MaterializedCommunityState::default(),
              },
              admin_addr: s.alice_addr,
              community_name: "OptionAHandshakeCommunity".into(),
              is_invite_only: true,
              expires_at: None,
              invite_token: Some(invite_token),
              admin_bootstrap: Some(s.alice_minted.bootstrap_join.clone()),
              admin_identity_pub: Some(s.alice_pub),
              forked_from: None,
              pre_fork_snapshot: None,
              inviter_enrollment: Some(s.alice_comm.cert.clone()),
              untargeted_decrypt_key: None,
          };
          let invite_url =
              community_invite::encode_invite_url(&invite_payload).expect("encode invite");

          s.invite_pub.register_invite(&invite_payload).await;
          let _probe = await_pkarr_record_visible(&s.pkarr_resolver, &token_sig).await;
          let invite_handle = format!("invite:{}", hex::encode(token_sig));
          assert!(
              s.pkarr_publisher
                  .active_handles()
                  .await
                  .contains(&invite_handle),
              "precondition: the case-A invite handle must be registered before the redeem"
          );

          // Drive Bob's redeem. The acceptor inserts Bob's PendingJoin, then
          // CountersignTimeouts (poll_deadline=0) before writing anything back.
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
              |_payload: harmony_app::NavUpdatedPayload| {},
              harmony_app::HandshakeDialConfig {
                  connect_timeout: Duration::from_millis(10_000),
                  open_bi_timeout: Duration::from_millis(10_000),
                  response_read_timeout: Duration::from_millis(10_000),
                  write_timeout: Duration::from_millis(10_000),
              },
              || Ok(()),
          )
          .await
          .expect("connectivity_redeem_invite_iroh_inner must Ok (errors → non-joined status)");

          assert_ne!(
              outcome.status, "joined",
              "the redeem must NOT report joined when the acceptor CountersignTimeouts \
               before delivering the countersign; got status={:?}",
              outcome.status
          );

          // THE load-bearing assertion: the single-use invite must remain live.
          // Grace window for any spawned tasks to settle, then assert the handle
          // is STILL registered — pre-ZEB-874 handle_unicast burned it on insert.
          tokio::time::sleep(Duration::from_millis(500)).await;
          assert!(
              s.pkarr_publisher
                  .active_handles()
                  .await
                  .contains(&invite_handle),
              "ZEB-874: a redeem that fails after the host's insert (CountersignTimeout) \
               must NOT burn the single-use invite — handle {invite_handle:?} must still \
               be in active_handles so the legitimate joiner can retry"
          );

          s.publisher_handle.abort();
          s.alice_ep.shutdown().await;
          s.bob_ep.shutdown().await;
      })
      .await
      .expect("invite_not_burned_when_handshake_fails_after_insert timed out at 60s");
  }
  ```

- [ ] **Step 4: gate + commit.** From `src-tauri/`: `cargo fmt --all`; clippy `--all-targets`; run the file (`-E 'test(pkarr_iroh_redeem_full)'`) to confirm all roundtrips + the new negative test pass; then the full `--workspace --all-targets` sweep + `npx tsc --noEmit` (repo root). Commit:
  ```bash
  git add -A && git commit -m "ZEB-874: add deterministic negative test — post-insert failure keeps invite live"
  ```

---

## Self-review

- **Spec coverage:** production burn-move (Task 1 Steps 1-5), happy-path regression (Task 1 Steps 6-7), deterministic negative (Task 2). The honest scope boundary (write-success-but-joiner-never-reads, phantom member) is documented as out-of-scope in the spec + code comments, not implemented — matches the Tier-1 decision.
- **Type consistency:** `handle_unicast` param removal is reflected at its one call site (Task 1 Step 3). `setup_two_party_iroh_handshake_with_config` is introduced before its use in Task 2 Step 3; the four existing callers keep calling the untouched `setup_two_party_iroh_handshake()` wrapper.
- **No placeholders:** every step carries the exact before/after text.
