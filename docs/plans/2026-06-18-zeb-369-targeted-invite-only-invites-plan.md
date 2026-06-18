# ZEB-369 Targeted invite-only invites — implementation plan

> **For agentic workers:** execute task-by-task; commit per task. Full design + code sketches in `docs/specs/2026-06-18-zeb-369-targeted-invite-only-invites-design.md`. Recon (exact file:line anchors) is the second comment on ZEB-369.

**Goal:** Add a targeted branch to invite-only `generate_invite` that seals the epoch key to ALL of the invitee's enrolled device-#2 keys (resolved from materialized membership), carried in an additive `InviteEpochSnapshot` field; `mint_redemption` tries each envelope.

**Architecture:** additive optional wire field (untargeted byte-identical) + membership-scan resolver + seal-to-all loop + try-all redeem. X25519 derived birationally from ed25519 (no PubKeyBundle dependency).

**Tech stack:** Rust (Tauri `harmony-app`), serde_cbor canonical wire, ed25519↔x25519 birational seal (`dm_signing.rs`).

---

## File map

| File | Change |
|---|---|
| `src-tauri/src/owner_state_types.rs` | new `serialize_vec_of_vec_as_bstr` / `deserialize_vec_of_vec_from_bstr` helper pair |
| `src-tauri/src/community_invite.rs` | add `sealed_epoch_keys: Vec<Vec<u8>>` field (key `"se"`) to `InviteEpochSnapshot` |
| `src-tauri/src/lib.rs` | `resolve_invitee_device_keys` fn; `invite_only_generation_guard` drop targeted-reject; `generate_invite_impl` targeted branch; `mint_redemption` try-all + invitee-binding check; update existing guard test (`:46464`) |
| `src-tauri/tests/wire_format_*` | new targeted-snapshot fixture + back-compat decode test |
| `src-tauri/tests/pkarr_iroh_redeem_full_integration.rs` | `targeted_invite_only_generate_then_redeem_roundtrip` (+ optional multi-device variant) |

---

## Dispatch 1 — core Rust + unit tests (gate: `-p harmony-app --lib`)

**Task 1.1 — wire helper + field (TDD).**
- Add the `Vec<Vec<u8>>`↔CBOR-array-of-bstr helper pair in `owner_state_types.rs` (mirror existing single-vec helpers).
- Add `sealed_epoch_keys` field to `InviteEpochSnapshot` with `rename="se", default, skip_serializing_if="Vec::is_empty"` + the new helpers. Confirm no test pins a uniform-key-length invariant beyond 2 chars.
- Tests: (a) round-trip a snapshot with 2 envelopes; (b) **back-compat**: decode an old-format CBOR (only `ep`/`sk`/`ss`) → `sealed_epoch_keys` defaults empty; (c) untargeted snapshot (empty field) encodes byte-identically to a pre-change fixture byte vector.

**Task 1.2 — resolver (TDD).**
- `resolve_invitee_device_keys(crdt_state, community_registry, inviter_admin_addr, invitee_addr) -> BTreeSet<[u8;32]>` per spec §Component 1: scan `crdt_state.spaces` (Community kind), materialize each, union `enrolled_device_keys` for invitee where `status==Joined`.
- Tests: invitee Joined in one community → its keys; invitee in two communities w/ distinct devices → union; invitee `Left`/absent → empty; multi-device member → all keys.

**Task 1.3 — generate branch + guard (TDD).**
- `invite_only_generation_guard`: remove `invitee_hint.is_some()` reject; keep admin-only check.
- `generate_invite_impl` `is_invite_only` block: `Some(hint)` → decode addr, resolve keys, empty→shipped "can't target … use an untargeted link" Err, else seal-to-all (`ed25519_pub_to_x25519`+`seal_to_owner`) → `sealed_epoch_keys`, `sealed_epoch_key=vec![]`, token `invitee_hint=Some(addr)`, `untargeted_decrypt_key=None`. `None` → unchanged untargeted.
- Tests: update `:46464` guard test (targeted now allowed; non-admin still rejected); targeted generation yields non-empty `sealed_epoch_keys` + token hint; unresolvable invitee → Err.

**Task 1.4 — redeem try-all + invitee binding (TDD).**
- `mint_redemption` invite-only branch: build candidate list from `sealed_epoch_keys` (non-empty) else `[sealed_epoch_key]`; length-guard + `find_map(open_from_owner)`; clear Err if none open. Add the `invitee_hint != self_owner` early Err (targeted only).
- Tests: 2-envelope snapshot, device key matches 2nd → opens; matches none → Err; untargeted single-blob fallback still opens; invitee-mismatch → Err; open-community raw-key path unchanged.

**Dispatch 1 gate (commit first):**
```
cd src-tauri
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```

## Dispatch 2 — fixtures + E2E (gate: `--test` scoped)

**Task 2.1 — targeted wire fixture.** Pin the targeted `InviteEpochSnapshot` CBOR (deterministic-nonce seal via `test-fixtures`); assert stable bytes.

**Task 2.2 — E2E roundtrip.** `targeted_invite_only_generate_then_redeem_roundtrip` in `tests/pkarr_iroh_redeem_full_integration.rs`, mirroring `invite_only_untargeted_generate_then_redeem_roundtrip` (:1040) + `bob_joins_alice_via_iroh_handshake_option_a` (:718): seed Bob as a Joined member of a community Alice shares, generate a targeted invite (resolver finds Bob's key), redeem with Bob's real device key, assert `status=="joined"`. Optional multi-device variant via `mint_second_device`/`make_device_announce` (seal to 2, redeem on 2nd).

**Dispatch 2 gate (commit first):**
```
cd src-tauri
cargo fmt --all -- --check
cargo nextest run --locked -p harmony-app --features test-fixtures \
  --test pkarr_iroh_redeem_full_integration -E 'test(wire_format)'
```
(plus the specific fixture test target)

## Final sweep (controller, CI parity)

```
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Then push, open PR (`Closes ZEB-369`, related refs in a comment not the body), bot loop.

## Acceptance

- Targeted invite-only invite generates (seals to all known devices), redeems on any of them; unresolvable invitee → clear "use untargeted" Err.
- Untargeted/open invites unchanged (byte-identical wire; existing tests green).
- All gates green under `--all-targets --features test-fixtures`.
