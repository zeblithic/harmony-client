# ZEB-483 DM Invite Deposit Durability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the DM-Space `DmInvite` the same always-deposit + attempt-tunnel durability as the `CidNotify`, by piggybacking the signed invite inside the end-to-end sealed `DepositPayload`, so an offline-at-create recipient bootstraps the DM Space from the deposit rung.

**Architecture:** A new `invite_packet: Option<Vec<u8>>` rides inside the sealed `DepositPayload` (and the persisted `DmInboxEntry`, and the in-process `ButlerDepositRequest`). The sender rebuilds+signs the invite from the persisted `Space` record at deposit time (mirroring how it rebuilds the CidNotify). The butler acceptor passes the invite through to the persisted entry untouched (its CidNotify+blob validation is unchanged) behind a size bound. The community relay needs no acceptor change (it holds the sealed blob opaquely). On recover, both entry points (butler `ProdDmInboxIngestCtx::verify` and relay `ProdRelayIngestCtx::ingest_recovered`) call a shared `apply_deposited_invite` helper to bootstrap the Space *before* `verify_cidnotify_admission`, binding the invite's inviter to the CidNotify's claimed sender (which admission then cryptographically pins).

**Tech Stack:** Rust, `serde` + canonical CBOR (`serde_bytes` for byte fields), `ed25519_dalek` signing, `cargo nextest`. All changes in `harmony-client/src-tauri/src` (crate `harmony-app`). No harmony-core PR.

**Spec:** `docs/specs/2026-06-16-zeb-483-dm-invite-deposit-durability-design.md` (commit `eff59a7e`).

**Test/lint commands** (run from `src-tauri/`, per `CLAUDE.md`):
- One test: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(=TEST_NAME)'`
- Lint (scoped, lib only — avoids the integration-test relink cost): `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
- Format: `cargo fmt --all -- --check`
- Final full sweep (Task 6 only): `cargo nextest run --locked --workspace --all-targets --features test-fixtures` + `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`

---

## File Structure

| File | Responsibility / change |
|---|---|
| `src-tauri/src/butler_deposit.rs` | Add `MAX_DEPOSIT_INVITE_BYTES` const; `DepositPayload.invite_packet`; `ButlerDepositRequest.invite_packet`; `IrohButlerDepositClient::deposit` copies `req.invite_packet` → payload. |
| `src-tauri/src/dm_inbox_crdt.rs` | `DmInboxEntry.invite_packet` (serde-default). |
| `src-tauri/src/community_relay_prod.rs` | `ProdCommunityRelayDepositClient::deposit` copies `req.invite_packet` → payload; `ProdRelayIngestCtx::ingest_recovered` applies invite-before-notify (+ `self_owner` field if absent). |
| `src-tauri/src/iroh_butler_acceptor.rs` | `handle_deposit_core` size-bounds + carries `payload.invite_packet` into the persisted `DmInboxEntry`. |
| `src-tauri/src/dm_outbox.rs` | `build_invite_packet_bytes`; `push_deposit_candidate` attaches it; shared `apply_deposited_invite` recover helper. |
| `src-tauri/src/dm_inbox_ingest.rs` | `ProdDmInboxIngestCtx::verify` applies invite-before-notify (lock `&mut`); `+ self_owner` field if absent. |
| `src-tauri/src/iroh_community_relay_acceptor.rs` | (compile-only) add `invite_packet: None` to its test `DepositPayload` literal. |

---

## Task 1: Add `invite_packet` fields + size const + plumb through deposit clients

**Files:**
- Modify: `src-tauri/src/butler_deposit.rs` (`DepositPayload` ~:144, `ButlerDepositRequest` ~:296, const block ~:50, `IrohButlerDepositClient::deposit` payload literal ~:512)
- Modify: `src-tauri/src/dm_inbox_crdt.rs` (`DmInboxEntry` ~:14)
- Modify: `src-tauri/src/community_relay_prod.rs` (deposit payload literal ~:738)
- Modify: `src-tauri/src/iroh_butler_acceptor.rs` (test payload literals ~:951, :1329, :1632-1702)
- Modify: `src-tauri/src/iroh_community_relay_acceptor.rs` (test payload literal ~:1057)
- Test: `src-tauri/src/butler_deposit.rs` (`#[cfg(test)] mod tests`)

This task adds the field everywhere and keeps the tree compiling; the field flows request → sealed payload but stays `None` until Task 2 populates it at the source.

- [ ] **Step 1: Write the failing backward-compat round-trip test**

In `butler_deposit.rs` test module, add:

```rust
    #[test]
    fn deposit_payload_round_trips_invite_packet_and_decodes_legacy_as_none() {
        // (a) Some(invite) round-trips.
        let with_invite = DepositPayload {
            cidnotify_packet: vec![1, 2, 3],
            storage_blob: vec![4, 5, 6],
            invite_packet: Some(vec![7, 8, 9]),
        };
        let bytes = encode_deposit_payload(&with_invite).expect("encode");
        assert_eq!(decode_deposit_payload(&bytes).expect("decode"), with_invite);

        // (b) None round-trips and (skip_serializing_if) omits the key.
        let without = DepositPayload {
            cidnotify_packet: vec![1],
            storage_blob: vec![2],
            invite_packet: None,
        };
        let bytes_none = encode_deposit_payload(&without).expect("encode none");
        assert_eq!(decode_deposit_payload(&bytes_none).expect("decode none"), without);

        // (c) A LEGACY payload (encoded by a struct WITHOUT invite_packet) decodes
        //     to invite_packet: None — proving forward-compat for old senders.
        #[derive(serde::Serialize)]
        struct LegacyDepositPayload {
            #[serde(rename = "cn", with = "serde_bytes")]
            cidnotify_packet: Vec<u8>,
            #[serde(rename = "pl", with = "serde_bytes")]
            storage_blob: Vec<u8>,
        }
        let legacy = LegacyDepositPayload { cidnotify_packet: vec![1], storage_blob: vec![2] };
        let legacy_bytes = crate::owner_state_crypto::canonical_cbor_encode(&legacy).expect("legacy encode");
        let decoded = decode_deposit_payload(&legacy_bytes).expect("legacy decode");
        assert_eq!(decoded.invite_packet, None);
        assert_eq!(decoded.cidnotify_packet, vec![1]);
    }
```

- [ ] **Step 2: Run it — confirm it fails to compile (`invite_packet` field absent)**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(=deposit_payload_round_trips_invite_packet_and_decodes_legacy_as_none)'`
Expected: FAIL — compile error `struct DepositPayload has no field named invite_packet`.

- [ ] **Step 3: Add `MAX_DEPOSIT_INVITE_BYTES` const** near `DEPOSIT_MAX_FRAME_BYTES` (`butler_deposit.rs:50`):

```rust
/// ZEB-483: per-deposit cap on the piggybacked DmInvite packet bytes. A DM
/// invite is a few hundred bytes (member set + 32-byte content key + 64-byte
/// identity pub + 64-byte sig); 4 KiB is a generous ceiling that still bars a
/// malicious sender from inflating butler/relay storage via the invite field.
pub const MAX_DEPOSIT_INVITE_BYTES: usize = 4096;
```

- [ ] **Step 4: Add the field to `DepositPayload`** (`butler_deposit.rs:144`), after `storage_blob`:

```rust
    /// ZEB-483: optional signed DmInvite packet bytes (a `DmPacket::Invite`),
    /// piggybacked so an offline-at-create recipient bootstraps the DM Space
    /// from the deposit rung. Opaque to the butler/relay (sealed end-to-end);
    /// applied + verified on recover. `None` for non-DM deposits and legacy
    /// senders.
    #[serde(rename = "iv", default, skip_serializing_if = "Option::is_none", with = "serde_bytes")]
    pub invite_packet: Option<Vec<u8>>,
```

- [ ] **Step 5: Add the field to `ButlerDepositRequest`** (`butler_deposit.rs:296`), after `cidnotify_packet`:

```rust
    /// ZEB-483: signed DmInvite packet bytes for the recipient to bootstrap the
    /// DM Space from the deposit rung; `None` for non-DM Spaces. Copied verbatim
    /// into the sealed `DepositPayload` by each deposit client.
    pub invite_packet: Option<Vec<u8>>,
```

- [ ] **Step 6: Add the field to `DmInboxEntry`** (`dm_inbox_crdt.rs:14`), after `storage_blob`:

```rust
    /// ZEB-483: optional signed DmInvite packet bytes, carried through from the
    /// sealed `DepositPayload` by the butler acceptor. Applied on recover to
    /// bootstrap the DM Space before CidNotify admission. `None` for non-DM /
    /// legacy deposits.
    #[serde(rename = "iv", default, skip_serializing_if = "Option::is_none", with = "serde_bytes")]
    pub invite_packet: Option<Vec<u8>>,
```

- [ ] **Step 7: Thread request → payload in `IrohButlerDepositClient::deposit`** (`butler_deposit.rs:512`):

```rust
        let payload = DepositPayload {
            cidnotify_packet: req.cidnotify_packet.clone(),
            storage_blob,
            invite_packet: req.invite_packet.clone(),
        };
```

- [ ] **Step 8: Thread request → payload in `ProdCommunityRelayDepositClient::deposit`** (`community_relay_prod.rs:738`):

```rust
        let payload = DepositPayload {
            cidnotify_packet: req.cidnotify_packet.clone(),
            storage_blob,
            invite_packet: req.invite_packet.clone(),
        };
```

(If the relay literal at `:738` doesn't have `req` in scope under that exact name, use the local request binding the function already holds; the field name is `invite_packet`.)

- [ ] **Step 9: Add `invite_packet: None` to every remaining struct literal so the tree compiles.** Build and fix each error. Known sites:
  - `iroh_butler_acceptor.rs`: the `DmInboxEntry { … }` built in `handle_deposit_core` (~:670) → add `invite_packet: None,` (Task 3 replaces with the real passthrough); test `DepositPayload` literals at `:951`, `:1329`, and the `reframe` literals `:1649`, `:1670`, `:1693` → `invite_packet: None,`.
  - `community_relay_prod.rs` / `iroh_community_relay_acceptor.rs`: any test `DepositPayload` literal (e.g. `:1057`) → `invite_packet: None,`.
  - `dm_inbox_crdt.rs`, `dm_inbox_ingest.rs`: test `DmInboxEntry` literals (`make_entry` ~:829, `build_dm_ingest_fixture` ~:2194, and any others) → `invite_packet: None,`.
  - `dm_outbox.rs`: any `ButlerDepositRequest { … }` literals in tests → `invite_packet: None,`.

  Run `cargo build --locked -p harmony-app --features test-fixtures` and add `invite_packet: None,` at each remaining literal the compiler flags.

- [ ] **Step 10: Run the test — confirm it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(=deposit_payload_round_trips_invite_packet_and_decodes_legacy_as_none)'`
Expected: PASS.

If `#[serde(with = "serde_bytes")]` fails to compile on `Option<Vec<u8>>` (it should not — serde_bytes supports `Option<Vec<u8>>`), fall back to removing `with = "serde_bytes"` (plain `#[serde(rename = "iv", default, skip_serializing_if = "Option::is_none")]`); the bytes then encode as a CBOR array (correct, marginally larger) and the test still passes.

- [ ] **Step 11: Lint + format + commit**

```bash
cd src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings && cargo fmt --all
git add -A && git commit -m "feat(zeb-483): add invite_packet to DepositPayload/DmInboxEntry/ButlerDepositRequest

Backward-compatible serde-default field + MAX_DEPOSIT_INVITE_BYTES; plumb it
request->sealed payload in both deposit clients. Always None until the send
side populates it.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Send side — rebuild + sign the invite and attach to the deposit candidate

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (add `build_invite_packet_bytes` near `build_cidnotify_packet_bytes` :1361; populate the request in `push_deposit_candidate` :1379)
- Test: `src-tauri/src/dm_outbox.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test** — the deposit request carries a valid signed invite that `apply_invite` accepts, and a non-DM space yields `None`.

Mirror the existing `deposit_rung_fixture` (`dm_outbox.rs:3667`) which installs a DM space + outbox entry. Add:

```rust
    #[tokio::test]
    async fn deposit_candidate_attaches_signed_invite_for_dm_space() {
        let (mut state, transport, mut o, mock, entry_id, bob) =
            deposit_rung_fixture(DepositRungOutcome::Failed("butlers unreachable".into()));
        // deposit_rung_fixture installs a DM Space for the entry; ensure it has
        // a content_key + both members so the invite rebuild has its inputs.
        let space_id = SpaceId([1u8; 16]);
        install_space(&mut state, make_dm_space(1, vec![o.self_owner, bob]));

        // Drive two transient failures to trip the deposit rung (the first never
        // deposits, the second does — matches the existing rung tests).
        let _ = drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 10_000).await;
        let _ = drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 15_000).await;

        let calls = mock.calls();
        assert_eq!(calls.len(), 1, "deposit rung fires once");
        let invite_bytes = calls[0]
            .invite_packet
            .as_ref()
            .expect("DM-space deposit must carry a piggybacked invite");

        // The invite decodes to a DmPacket::Invite for the same Space, inviter == sender.
        let packet = crate::dm_envelope::decode_packet(invite_bytes).expect("decode invite");
        let crate::dm_envelope::DmPacket::Invite { signed, signature, signed_bytes } = packet else {
            panic!("expected Invite");
        };
        assert_eq!(signed.space_id, space_id);
        assert_eq!(signed.inviter, o.self_owner);
        assert!(signed.members.contains(&o.self_owner) && signed.members.contains(&bob));

        // And a FRESH recipient state applies it (signature + admission gates pass).
        let mut rx = OwnerState::default();
        let outcome = crate::dm_outbox::apply_invite(
            &mut rx,
            bob,                 // recipient self
            "bob-dev",
            signed,
            signature,
            &signed_bytes,
            20_000,
            Some(o.self_owner),  // expected inviter
        );
        assert!(outcome.is_ok(), "rebuilt invite must apply on a fresh recipient: {outcome:?}");
        assert!(rx.spaces.contains_key(&space_id), "Space bootstrapped from the deposited invite");
    }

    #[tokio::test]
    async fn deposit_candidate_omits_invite_for_non_dm_space() {
        let (mut state, transport, mut o, mock, entry_id, bob) =
            deposit_rung_fixture(DepositRungOutcome::Failed("x".into()));
        // Replace the space with a community (non-DM) space sharing the entry's id.
        let mut community = make_dm_space(1, vec![o.self_owner, bob]);
        community.kind = SpaceKind::Community;
        community.content_key = None;
        install_space(&mut state, community);

        let _ = drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 10_000).await;
        let _ = drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 15_000).await;

        assert_eq!(mock.calls()[0].invite_packet, None, "non-DM deposit carries no invite");
    }
```

(If `deposit_rung_fixture` already installs a DM space for `entry.space_id`, the explicit `install_space` in the first test is a harmless overwrite ensuring members+content_key; verify the space id matches `entry.space_id` = `SpaceId([1u8;16])`.)

- [ ] **Step 2: Run it — confirm failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(deposit_candidate_attaches_signed_invite_for_dm_space) + test(deposit_candidate_omits_invite_for_non_dm_space)'`
Expected: FAIL — `invite_packet` is always `None` (no builder yet) → first test panics on `.expect("…carry a piggybacked invite")`.

- [ ] **Step 3: Add `build_invite_packet_bytes`** to `impl DmOutbox` next to `build_cidnotify_packet_bytes` (`dm_outbox.rs:1361`):

```rust
    /// ZEB-483: rebuild + sign the DmInvite wire bytes for a DM-Space deposit —
    /// the SAME `DmInviteSigned` `add_space_dm_inner` builds for the tunnel
    /// carrier (lib.rs:10410), reconstructed from the persisted `Space` record so
    /// a deposited invite bootstraps the Space exactly like a tunnel arrival.
    /// Returns `None` for non-DM Spaces, a missing Space record, or a Space with
    /// no content_key (the CidNotify still deposits without it).
    fn build_invite_packet_bytes(&self, state: &OwnerState, space_id: &SpaceId) -> Option<Vec<u8>> {
        let space = state.spaces.get(space_id)?;
        if !matches!(space.kind, SpaceKind::Dm | SpaceKind::GroupDm) {
            return None;
        }
        let content_key = space.content_key.clone()?;
        let signed = crate::dm_envelope::DmInviteSigned {
            space_id: space.id,
            kind: space.kind,
            members: space.members.clone(),
            inviter: self.self_owner,
            content_key,
            sender_devices: vec![self.our_signing_device_hash],
            created_at: space.created_at.clone(),
            signing_device_hash: self.our_signing_device_hash,
            inviter_identity_pub: self.private_identity.public_identity().to_public_bytes(),
        };
        match crate::dm_envelope::build_signed_invite(signed, &self.signing_key)
            .and_then(|p| crate::dm_envelope::encode_packet(&p))
        {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                tracing::warn!(error = %e, space_id = ?space_id, "ZEB-483: invite rebuild failed; depositing CidNotify without invite");
                None
            }
        }
    }
```

- [ ] **Step 4: Populate the request in `push_deposit_candidate`** (`dm_outbox.rs:1390`) — pass `state` through and set the field:

```rust
        match self.build_cidnotify_packet_bytes(entry) {
            Ok(cidnotify_packet) => out.push(crate::butler_deposit::ButlerDepositRequest {
                entry_id,
                recipient_owner: recipient,
                space_id: entry.space_id,
                message_cid: entry.message_cid,
                cidnotify_packet,
                invite_packet: self.build_invite_packet_bytes(state, &entry.space_id),
                now_ms,
            }),
            Err(err) => tracing::warn!(
                entry_id = ?entry_id, recipient = ?recipient, error = %err,
                "ZEB-418: CidNotify build failed; skipping deposit candidate"
            ),
        }
```

(`state` is already a parameter of `push_deposit_candidate`; `entry` is already bound. Confirm `to_public_bytes()` is the method name on `private_identity.public_identity()` — the test `handle_invite_writes_space_and_cache_with_signing_pub` at `dm_outbox.rs:4943` uses exactly `private.public_identity().to_public_bytes()`.)

- [ ] **Step 5: Run the tests — confirm pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(deposit_candidate_attaches_signed_invite_for_dm_space) + test(deposit_candidate_omits_invite_for_non_dm_space)'`
Expected: PASS both.

- [ ] **Step 6: Guard against regressions — run the existing deposit-rung tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(deposit) + test(butler)'`
Expected: PASS (the existing CidNotify-deposit tests are unaffected; their `invite_packet` is now populated for DM spaces but they assert on the CidNotify only).

- [ ] **Step 7: Lint + format + commit**

```bash
cd src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings && cargo fmt --all
git add -A && git commit -m "feat(zeb-483): rebuild+sign DmInvite and attach to DM-space deposit candidates

build_invite_packet_bytes reconstructs the DmInviteSigned from the persisted
Space record (mirrors build_cidnotify_packet_bytes); push_deposit_candidate
always-attaches it for Dm/GroupDm spaces, None otherwise.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Butler acceptor — size-bound + carry invite into the persisted entry

**Files:**
- Modify: `src-tauri/src/iroh_butler_acceptor.rs` (`handle_deposit_core` ~:601-686)
- Test: `src-tauri/src/iroh_butler_acceptor.rs` (`#[cfg(test)] mod tests`, near `valid_fixture` :937 / `deposit_from_active_friend_is_accepted_persisted_then_acked` :1142)

- [ ] **Step 1: Write the failing tests** — invite carried through to the persisted entry; oversized invite rejected. Add a fixture variant + two tests:

```rust
    fn valid_fixture_with_invite(invite_packet: Option<Vec<u8>>) -> Fixture {
        // Same as valid_fixture() but the sealed DepositPayload carries an invite.
        let so = sender();
        let space_id = SpaceId([0x77; 16]);
        let storage_blob = b"encrypted-dm-storage-blob-bytes".to_vec();
        let message_cid = ContentId::for_book(
            &storage_blob,
            ContentFlags { encrypted: true, ..Default::default() },
        ).expect("cid for blob");
        let (cidnotify_packet, identity_pub, dm_device_hash) =
            build_cidnotify(so.owner, space_id, message_cid);
        let payload = DepositPayload {
            cidnotify_packet: cidnotify_packet.clone(),
            storage_blob: storage_blob.clone(),
            invite_packet,
        };
        let payload_bytes = encode_deposit_payload(&payload).expect("encode payload");
        let sealed = seal_payload_bytes(&payload_bytes);
        let sig = sign_frame(&BUTLER_OWNER, &sealed, &so.device_key);
        let cert_bytes = harmony_owner::cbor::to_canonical(&so.cert).expect("encode cert");
        Fixture {
            frame: DepositFrame {
                recipient_owner: BUTLER_OWNER,
                sender_owner: so.owner.0,
                sender_enrollment_cert: cert_bytes,
                sig,
                sealed_blob: sealed,
            },
            sender_owner: so.owner.0,
            sender_master: master_from_cert(&so.cert),
            space_id, message_cid, cidnotify_packet, storage_blob,
            dm_device_hash, identity_pub,
        }
    }

    #[tokio::test]
    async fn deposit_carries_invite_packet_into_persisted_entry() {
        let invite = vec![0xABu8; 200];
        let f = valid_fixture_with_invite(Some(invite.clone()));
        let ctx = TestCtx::for_fixture(&f);

        handle_deposit_core(&f.frame, &ctx).await.expect("accepted");

        let key = DmInboxDoc::key(&f.space_id.0, &f.message_cid.to_bytes());
        let store = ctx.store.lock().unwrap();
        let entry = store.get(&key).expect("persisted");
        assert_eq!(entry.invite_packet, Some(invite), "invite carried through verbatim");
        assert_eq!(entry.cidnotify_packet, f.cidnotify_packet, "CidNotify validation/persist unchanged");
    }

    #[tokio::test]
    async fn deposit_with_oversized_invite_is_rejected() {
        let f = valid_fixture_with_invite(Some(vec![0u8; MAX_DEPOSIT_INVITE_BYTES + 1]));
        let ctx = TestCtx::for_fixture(&f);
        let err = handle_deposit_core(&f.frame, &ctx).await.expect_err("must reject");
        assert!(matches!(err, DepositReject::BadPayload), "oversized invite => BadPayload, got {err:?}");
    }
```

(Import `MAX_DEPOSIT_INVITE_BYTES` in the test module: `use crate::butler_deposit::MAX_DEPOSIT_INVITE_BYTES;`.)

- [ ] **Step 2: Run them — confirm failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(deposit_carries_invite_packet_into_persisted_entry) + test(deposit_with_oversized_invite_is_rejected)'`
Expected: FAIL — entry has `invite_packet: None` (Task 1 hard-coded it), and no size check exists yet.

- [ ] **Step 3: Add the size bound + passthrough in `handle_deposit_core`.** Right after the `let payload = decode_deposit_payload(...)?;` line (`iroh_butler_acceptor.rs:601`), insert the bound:

```rust
    if let Some(inv) = payload.invite_packet.as_ref() {
        if inv.len() > crate::butler_deposit::MAX_DEPOSIT_INVITE_BYTES {
            return Err(DepositReject::BadPayload);
        }
    }
```

Then in the `DmInboxEntry { … }` literal (~:670) set the field (replacing the `invite_packet: None,` added in Task 1):

```rust
    let entry = DmInboxEntry {
        sender_owner: frame.sender_owner,
        cidnotify_packet: payload.cidnotify_packet,
        storage_blob: payload.storage_blob,
        invite_packet: payload.invite_packet,
        deposited_at: ctx.mint_hlc().await,
        deposited_by: ctx.device_id(),
        ingested_by: BTreeSet::new(),
    };
```

(Order the `invite_packet` move BEFORE `payload.storage_blob`/`payload.cidnotify_packet` are moved out, or take the field first into a local — Rust will reject moving `payload.invite_packet` after a partial move only if the others are moved by value first. Simplest: bind `let invite_packet = payload.invite_packet.take();` is not available on a non-mut payload — instead make `payload` mut, or read `invite_packet` in the literal before the other two. The literal above moves all three fields out of `payload` in one struct expression, which is allowed.)

- [ ] **Step 4: Run the tests — confirm pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(deposit_carries_invite_packet_into_persisted_entry) + test(deposit_with_oversized_invite_is_rejected)'`
Expected: PASS.

- [ ] **Step 5: Regression — existing acceptor tests still green**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(deposit_from_active_friend_is_accepted_persisted_then_acked)'`
Expected: PASS (CidNotify+blob validation path unchanged; the existing test's payload has `invite_packet: None`).

- [ ] **Step 6: Lint + format + commit**

```bash
cd src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings && cargo fmt --all
git add -A && git commit -m "feat(zeb-483): butler acceptor carries deposited invite into DmInboxEntry

handle_deposit_core size-bounds payload.invite_packet (BadPayload over
MAX_DEPOSIT_INVITE_BYTES) and persists it verbatim; CidNotify+blob validation
unchanged.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: Recover (butler) — `apply_deposited_invite` helper + apply before notify

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (add `apply_deposited_invite` near `apply_invite` :1971)
- Modify: `src-tauri/src/dm_inbox_ingest.rs` (`ProdDmInboxIngestCtx::verify` :702 — lock `&mut`, apply invite before admission; add `self_owner` field to the ctx if absent)
- Test: `src-tauri/src/dm_inbox_ingest.rs` (live `ProdDmInboxIngestCtx` ingest tests, near `:1329` / fixture `build_dm_ingest_fixture` :2194)

- [ ] **Step 1: Write the failing test** — a deposited entry carrying invite + CidNotify for a NOT-yet-bootstrapped Space verifies (Space bootstrapped, then admits); inviter-mismatch leaves it unverified.

Use `build_dm_ingest_fixture` (`dm_inbox_ingest.rs:2194`) but DO NOT pre-install the Space; instead attach the signed invite to the entry. Add (in the live-ctx test module):

```rust
    #[tokio::test]
    async fn deposited_invite_bootstraps_space_then_cidnotify_admits() {
        // Fixture that seeds Alice's device in Bob's cache + builds a signed
        // CidNotify + storage blob, but does NOT install the DM Space in Bob's
        // state (simulating offline-at-create). It also yields a signed invite.
        let fx = build_dm_ingest_fixture_without_space_with_invite();
        let ctx = fx.prod_ctx; // ProdDmInboxIngestCtx over Bob's state

        // Sanity: the Space is absent → a plain CidNotify ingest would fail.
        {
            let st = ctx.crdt_state_for_test().lock().await;
            assert!(!st.spaces.contains_key(&fx.space_id), "space absent pre-recover");
        }

        let verified = ctx.verify(&fx.entry).await.expect("invite bootstraps space, notify admits");
        assert_eq!(verified.space_id, fx.space_id);
        assert_eq!(verified.body, fx.expected_body);

        let st = ctx.crdt_state_for_test().lock().await;
        assert!(st.spaces.contains_key(&fx.space_id), "Space bootstrapped from the deposited invite");
    }

    #[tokio::test]
    async fn deposited_invite_with_wrong_inviter_is_rejected_and_space_absent() {
        let fx = build_dm_ingest_fixture_without_space_with_invite();
        // Tamper: re-sign an invite whose inviter != the CidNotify sender.
        let entry = fx.entry_with_mismatched_inviter();
        let ctx = fx.prod_ctx;
        let err = ctx.verify(&entry).await.expect_err("mismatched inviter must fail-closed");
        assert!(err.contains("apply_invite") || err.contains("InviterMismatch"), "got {err}");
        let st = ctx.crdt_state_for_test().lock().await;
        assert!(!st.spaces.contains_key(&fx.space_id), "no Space bootstrapped on reject");
    }
```

(The fixture helpers `build_dm_ingest_fixture_without_space_with_invite`, `entry_with_mismatched_inviter`, and a `crdt_state_for_test()` accessor are test-only; build them by adapting `build_dm_ingest_fixture` (`:2194`): construct the signed invite exactly as `dm_outbox.rs:4943` does (a `PrivateIdentity::from_seed` for Alice, `DmInviteSigned { … kind: Dm, members: [alice, bob] sorted, content_key, inviter: alice … }`, `build_signed_invite` + `encode_packet`), set `entry.invite_packet = Some(invite_wire)`, and DON'T call `apply_space_with_canonicalization`. For the mismatch case, set `inviter` to a third owner and re-sign.)

- [ ] **Step 2: Run them — confirm failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(deposited_invite_bootstraps_space_then_cidnotify_admits) + test(deposited_invite_with_wrong_inviter_is_rejected_and_space_absent)'`
Expected: FAIL — `verify` ignores `entry.invite_packet`, so admission fails with `SpaceNotFound`.

- [ ] **Step 3: Add the shared `apply_deposited_invite` helper** to `dm_outbox.rs` (near `apply_invite` :1971):

```rust
/// ZEB-483: apply a deposited DmInvite (if present) to bootstrap the DM Space
/// before CidNotify admission, on the deposit-recover path (no authenticated
/// tunnel peer). Binds the invite's inviter to the CidNotify's claimed
/// `sender_owner_addr`; the caller's subsequent `verify_cidnotify_admission`
/// cryptographically pins that claimed sender to the resolved device owner, so a
/// forged invite that doesn't match the verified sender never admits (the Space
/// it bootstraps stays inert). Size-bounded; fail-closed.
pub(crate) fn apply_deposited_invite(
    state: &mut OwnerState,
    self_owner: OwnerAddr,
    device_id: &str,
    invite_packet: &[u8],
    expected_inviter: OwnerAddr,
    wall_now_ms: u64,
) -> Result<(), String> {
    if invite_packet.len() > crate::butler_deposit::MAX_DEPOSIT_INVITE_BYTES {
        return Err(format!("deposited invite too large: {} bytes", invite_packet.len()));
    }
    let packet = crate::dm_envelope::decode_packet(invite_packet)
        .map_err(|e| format!("decode invite: {e}"))?;
    let crate::dm_envelope::DmPacket::Invite { signed, signature, signed_bytes } = packet else {
        return Err("deposited invite_packet is not an Invite".into());
    };
    apply_invite(
        state, self_owner, device_id, signed, signature, &signed_bytes, wall_now_ms,
        Some(expected_inviter),
    )
    .map(|_| ())
    .map_err(|e| format!("apply_invite: {e:?}"))
}
```

- [ ] **Step 4: Apply the invite before admission in `ProdDmInboxIngestCtx::verify`** (`dm_inbox_ingest.rs:702`). Change the state lock to `mut` and insert the invite apply before `verify_cidnotify_admission`:

```rust
        let (space, resolved_owner) = {
            let mut state = self.crdt_state.lock().await;
            if let Some(inv) = entry.invite_packet.as_ref() {
                crate::dm_outbox::apply_deposited_invite(
                    &mut state,
                    self.self_owner,            // recipient's own owner (see Step 5)
                    &self.self_device_id(),
                    inv,
                    signed.sender_owner_addr,   // CidNotify's claimed sender
                    self.now_ms(),
                )?;
            }
            let (space, resolved_owner, _identity_pub) =
                crate::dm_outbox::verify_cidnotify_admission(
                    &state, &signed, &signature, &signed_bytes,
                )
                .map_err(|e| format!("verify_cidnotify_admission: {e:?}"))?;
            (space, resolved_owner)
        };
```

- [ ] **Step 5: Ensure `ProdDmInboxIngestCtx` exposes `self_owner`.** If the struct lacks a `self_owner: OwnerAddr` field, add one and set it at construction (the start_node wiring knows the local owner; grep for `ProdDmInboxIngestCtx {` to find the constructor and the test builders, and thread the recipient's `OwnerAddr`). The test fixture sets it to Bob's owner.

- [ ] **Step 6: Run the tests — confirm pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(deposited_invite_bootstraps_space_then_cidnotify_admits) + test(deposited_invite_with_wrong_inviter_is_rejected_and_space_absent)'`
Expected: PASS.

- [ ] **Step 7: Regression — existing live-ingest tests (incl. the pre-installed-space path)**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(ingest_dm_packet_applies_inbox_and_emits_dm_received) + test(ingest_puts_blob_verifies_and_applies_inbox)'`
Expected: PASS (entries with `invite_packet: None` skip the new branch and behave exactly as before).

- [ ] **Step 8: Lint + format + commit**

```bash
cd src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings && cargo fmt --all
git add -A && git commit -m "feat(zeb-483): recover-side applies deposited invite before CidNotify admission (butler)

apply_deposited_invite shared helper bootstraps the DM Space from entry.invite_packet
(inviter bound to the CidNotify sender, which admission then pins); ProdDmInboxIngestCtx::verify
applies it under a mut state lock before verify_cidnotify_admission. Fail-closed.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: Recover (relay) — `ingest_recovered` applies the invite before admission

**Files:**
- Modify: `src-tauri/src/community_relay_prod.rs` (`ProdRelayIngestCtx::ingest_recovered` :386; add `self_owner` field if absent)
- Test: `src-tauri/src/community_relay_prod.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test** — `ingest_recovered` with `payload.invite_packet` for a not-bootstrapped Space bootstraps + emits `dm-received`; mismatch rejected.

Adapt the existing `ingest_recovered` tests in `community_relay_prod.rs` (grep `ingest_recovered` in its test module for the happy-path fixture that builds a `DepositPayload`). Add:

```rust
    #[tokio::test]
    async fn ingest_recovered_invite_bootstraps_space_then_delivers() {
        let fx = relay_ingest_fixture_without_space_with_invite(); // builds payload incl. invite_packet
        let ctx = fx.prod_ctx;

        ctx.ingest_recovered(fx.payload.clone()).await.expect("invite bootstraps + delivers");

        let st = fx.state.lock().await;
        assert!(st.spaces.contains_key(&fx.space_id), "Space bootstrapped from relay-held invite");
        // dm-received emitted exactly once
        assert_eq!(fx.sink.emitted_dm_received(), 1);
    }

    #[tokio::test]
    async fn ingest_recovered_invite_wrong_inviter_rejected() {
        let fx = relay_ingest_fixture_without_space_with_invite();
        let payload = fx.payload_with_mismatched_inviter();
        let ctx = fx.prod_ctx;
        let err = ctx.ingest_recovered(payload).await.expect_err("fail-closed");
        assert!(err.contains("apply_invite") || err.contains("InviterMismatch"), "got {err}");
        let st = fx.state.lock().await;
        assert!(!st.spaces.contains_key(&fx.space_id));
    }
```

(Build `relay_ingest_fixture_without_space_with_invite` by adapting the existing relay ingest happy-path fixture: seed the sender's device in the recipient's `owner_device_cache`, build the signed CidNotify + storage blob as today, build the signed invite as in `dm_outbox.rs:4943`, set `payload.invite_packet = Some(invite_wire)`, and DON'T pre-install the Space.)

- [ ] **Step 2: Run them — confirm failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(ingest_recovered_invite_bootstraps_space_then_delivers) + test(ingest_recovered_invite_wrong_inviter_rejected)'`
Expected: FAIL — `ingest_recovered` ignores `payload.invite_packet` → `verify_cidnotify_admission` fails `SpaceNotFound`.

- [ ] **Step 3: Apply the invite before admission in `ingest_recovered`** (`community_relay_prod.rs:386`). Inside the existing `let mut state = self.crdt_state.lock().await;` block, before `verify_cidnotify_admission`:

```rust
            if let Some(inv) = payload.invite_packet.as_ref() {
                crate::dm_outbox::apply_deposited_invite(
                    &mut state,
                    self.self_owner,           // recipient's own owner (see Step 4)
                    &self.device_id,
                    inv,
                    signed.sender_owner_addr,  // CidNotify's claimed sender
                    now_epoch_ms(),
                )?;
            }
            let (space, resolved_owner, _identity_pub) =
                crate::dm_outbox::verify_cidnotify_admission(&state, &signed, &signature, &signed_bytes)
                    .map_err(|e| format!("verify_cidnotify_admission: {e:?}"))?;
```

(`apply_deposited_invite` returns `Result<(), String>`; `ingest_recovered` already returns `Result<(), String>`, so `?` propagates directly — fail-closed, the relay drain leaves it for retry.)

- [ ] **Step 4: Ensure `ProdRelayIngestCtx` exposes `self_owner`.** The struct (`community_relay_prod.rs:361`) has `device_id`, `crdt_state`, etc. If it lacks `self_owner: OwnerAddr`, add it and set it at construction (start_node knows the owner). The test fixture sets it to the recipient's owner.

- [ ] **Step 5: Run the tests — confirm pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(ingest_recovered_invite_bootstraps_space_then_delivers) + test(ingest_recovered_invite_wrong_inviter_rejected)'`
Expected: PASS.

- [ ] **Step 6: Regression — existing relay ingest tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(ingest_recovered)'`
Expected: PASS (payloads with `invite_packet: None` skip the new branch).

- [ ] **Step 7: Lint + format + commit**

```bash
cd src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings && cargo fmt --all
git add -A && git commit -m "feat(zeb-483): relay recover applies deposited invite before admission

ingest_recovered bootstraps the DM Space from payload.invite_packet (inviter
bound to the CidNotify sender) before verify_cidnotify_admission. Fail-closed.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: Full-workspace gates + final commit

**Files:** none (verification only)

- [ ] **Step 1: Format check**

Run: `cd src-tauri && cargo fmt --all -- --check`
Expected: clean (exit 0).

- [ ] **Step 2: Full clippy (all targets)**

Run: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: 0 warnings.

- [ ] **Step 3: Full test suite**

Run: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
Expected: all pass (the ZEB-483 tests + the unchanged suite). Note: some iroh/zenoh transport tests are known orphan-flakes (first-bind init); re-run a failing one once to confirm it's a flake, not a regression.

- [ ] **Step 4: Confirm no stray `invite_packet: None` was left where a real value belongs.** Grep:

Run: `cd src-tauri && grep -rn "invite_packet" src/ | grep -v test`
Expected: real values at the two deposit clients (`req.invite_packet.clone()`), the acceptor persist (`payload.invite_packet`), the send builder (`build_invite_packet_bytes`), and the two recover sites; struct defs; no production `invite_packet: None` placeholder except where genuinely correct.

- [ ] **Step 5: Final commit (if any fmt/regression fixes were needed)**

```bash
cd src-tauri && git add -A && git commit -m "test(zeb-483): full-workspace gates green for invite deposit durability

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>" || echo "nothing to commit"
```

---

## Out of scope (per spec)

- Live co-located / cross-WAN offline→recover e2e (needs AVALON; ZEB-444/447). No new harness deposit rung in this PR.
- Relaxing the butler acceptor's CidNotify-only contract / a separate invite-deposit item.
- An ack-gated "omit invite once bootstrapped" optimization (always-attach for DM spaces).

## Self-review checklist (run before handing off)

- **Spec coverage:** Task 1 = struct/wire + size const (spec §1, §3 size-bound); Task 2 = send-side always-attach (spec §2); Task 3 = butler passthrough (spec §3); Task 4 = butler recover (spec §5); Task 5 = relay recover (spec §5); relay deposit zero-change (spec §4) = the `:738` literal copy in Task 1. Idempotency/dedup (spec §6) relies on unchanged `apply_invite` LWW — no task needed, asserted indirectly by the recover tests. Error handling (spec table) = size-bound (Task 1/3), fail-closed apply (Task 4/5), legacy-None (Task 1 round-trip). All covered.
- **Type consistency:** `invite_packet: Option<Vec<u8>>` identical across `DepositPayload`, `DmInboxEntry`, `ButlerDepositRequest`; `MAX_DEPOSIT_INVITE_BYTES` defined once in `butler_deposit.rs`; `apply_deposited_invite` signature identical at both call sites; `build_invite_packet_bytes(&self, state, &SpaceId)` matches its one caller.
- **Placeholder scan:** every code step shows real code; test fixtures reference real existing helpers (`deposit_rung_fixture`, `valid_fixture`, `build_dm_ingest_fixture`, `make_dm_space`) with explicit adaptation notes where a new test-only helper is built.
