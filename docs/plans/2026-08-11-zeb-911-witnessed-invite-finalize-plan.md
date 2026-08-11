# ZEB-911 Witnessed Invite Finalize — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Invite-only redemption completes through any reachable Joined member (witness), not just the admin/inviter — per `docs/specs/2026-08-11-zeb-911-witnessed-invite-finalize-design.md`.

**Architecture:** Slice 1 relaxes the acceptor's inviter-identity policy (`verify_packet_pure` step 4) to the standing Joined+power eligibility and retargets the token-sig check to the admin's enrolled keys resolved from materialized membership. Slice 2 adds a witness-discovery ladder to `connectivity_redeem_invite_iroh_inner`: admin Case-A dial first (unchanged), then per-slot rendezvous resolution using the same `resolve_window_freshest_with` API the Case-A path already uses. No new event kinds, no CRDT changes, no harmony-pkarr changes.

**Tech stack:** Rust (src-tauri), existing `harmony_pkarr` public API, vitest for the frontend branch.

## Global constraints

- All cargo commands from `src-tauri/`, always `--locked`, tests with `--features test-fixtures`, clippy with `--all-targets` (CLAUDE.md).
- Per-task gates via `scripts/test-select --context task`; final pre-PR sweep is the full `--workspace --all-targets` run + `cargo fmt --all -- --check` + clippy.
- P2 (`invite_token.inviter == ctx.admin_addr`, `community_membership.rs:4368-4371`) and the ZEB-888 canonical-claimant fence are UNTOUCHED — any diff touching them is a plan violation.
- Commit per task; branch `zeblith/zeb-911-witnessed-invite-finalize`.

## Verified baseline (read 2026-08-11, main @ 011995ad)

- `verify_packet_pure(signed, self_owner, now_fn, self_device_ed25519)` — `community_invite.rs:2203-2302`. Step 4 identity check at 2271-2277 (`InviteSignerMismatch`); step 6 token-sig vs the acceptor's own device key at 2292-2299 (`verify_invite_token_sig_device_key`).
- `handle_unicast` — `community_invite.rs:2511+`. Order: decode → outbox snapshot → envelope sig → **pure verify (3b)** → engine/state resolve (4) → materialize + eligibility with **hardcoded `invite_threshold: u8 = 0`** (5, 2611-2647) → claim-bound insert (ZEB-875) → burn deferred to acceptor (ZEB-874).
- `verify_invite_token_sig_with_enrolled(token, prior_state)` — `community_membership.rs:1788-1807`: looks up `prior_state.members[token.inviter].enrolled_device_keys`, tries each against `canonical_invite_token_bytes`.
- Dial: single `alice_addr` (Case-A, `lib.rs:63009`), `for attempt in 0..2` with ZEB-908 merge (63038) + B4 diverse-relay re-resolve (63078-63179); failure → `RedemptionOutcome::unreachable()` (63181-63183). Resolver seeds under `payload.admin_addr` at 62989 and 63137. Shared tail (mint w/ ZEB-889 cache → packet build → write → countersign read → commit) from 63207.
- `resolve_window_freshest_with(&verifying_keys, &verify)` — the per-identity multi-epoch resolve used by Case-A (62964, 63104); returns the full `PkarrRoutingRecord` (identity available via `rec` accessors).
- `rendezvous_slot_verifying_key(epoch_key, slot_index, epoch_id)` — `community_rendezvous.rs:116-122`, public. `RENDEZVOUS_SLOT_COUNT = 4` (= `COMMUNITY_RELAY_ADVERTISERS_MAX`).
- Open-join precedent: `open_join_dial.rs:97-204` (resolve → synthesize `endpoint_addr_from_routing` → single bounded dial; retryable non-error outcomes).
- `verify_packet_pure` external callers: `handle_unicast` + 14 call sites in `tests/community_misc/community_invite_unit.rs`.

---

### Task 1: Retarget `verify_packet_pure` (pure layer)

**Files:** Modify `src-tauri/src/community_invite.rs` (fn at 2203, enum at 1310, `reason_tag` at ~1419); Test `src-tauri/tests/community_misc/community_invite_unit.rs`.

**Interfaces — produces:**
```rust
pub fn verify_packet_pure<F>(
    signed: &CommunityInviteSigned,
    now_fn: F,
    token_signer_keys: &[[u8; 32]],   // admin's enrolled device keys, caller-resolved
) -> Result<SignedMembershipEvent, CommunityInviteVerifyError>
```
(`self_owner` dropped — only step 4 used it. `token_signer_keys` replaces `self_device_ed25519`.)

- [ ] **1.1 Write failing tests** in `community_invite_unit.rs`: (a) packet accepted when token signed by a key in `token_signer_keys` that is NOT the acceptor's own (the witness case — reuse the existing fixture builder, pass the minting key's pub in the slice); (b) rejected `InviteTokenSigInvalid` when no key in the slice verifies; (c) accepted when the slice has several keys and only the last matches (mirrors `verify_invite_token_sig_with_enrolled`'s loop). Delete the `InviteSignerMismatch` expectation test (line ~815) — its semantic is removed.
- [ ] **1.2 Run** `cargo nextest run --locked --features test-fixtures -E 'test(community_invite)'` — expect compile failure/red on new tests.
- [ ] **1.3 Implement:** delete step 4 (2271-2277); rewrite step 6 as a try-each loop over `token_signer_keys` via `verify_invite_token_sig_device_key`, any success → Ok, none → `InviteTokenSigInvalid`. Remove the `self_owner` param; remove `CommunityInviteVerifyError::InviteSignerMismatch` + its `reason_tag` arm. Update the fn's doc comment (steps renumber; note the ZEB-911 witness model and that P5-parity now means "admin enrolled keys," not "self device key"). Update the 14 test call sites.
- [ ] **1.4 Run** the same filter — green. **1.5 Commit** `feat(invite): verify_packet_pure accepts witness-resolved token-signer keys (ZEB-911)`.

### Task 2: `handle_unicast` witness path

**Files:** Modify `src-tauri/src/community_invite.rs` (`handle_unicast` 2511+, enum/reason_tag); Test: same unit file + existing `handle_unicast`-level tests (locate via `grep -rn "handle_unicast" tests/`).

**Interfaces — consumes Task 1's signature. Produces:** new `CommunityInviteVerifyError::InviteTokenSignerUnknown` (reason tag `community_invite_token_signer_unknown`).

- [ ] **2.1 Write failing tests** (engine-backed, mirroring existing handle_unicast tests): (a) **witness accept** — community with admin + Joined member B; B's node receives a valid redeem packet (token minted by admin) → `Ok`, PendingJoin inserted in B's engine, auto-countersign observed (poll B's state for the JoinCountersign, as ZEB-254 tests do); (b) **non-member reject** — node C not in the community → `SelfNotJoined` (existing semantics, now must fire where step 4 used to); (c) **admin self-accept regression** — existing admin-path test still green unchanged; (d) **signer-unknown** — packet whose token.inviter is absent from the receiver's materialized members → `InviteTokenSignerUnknown`.
- [ ] **2.2 Run targeted filter** — red.
- [ ] **2.3 Implement — reorder `handle_unicast`:** decode → outbox snapshot (keep; still needs `self_owner` for eligibility + `community_signing_key` no longer needed for verify — drop if now unused) → envelope sig → resolve engine+state (existing step 4 code) → **single materialize** (reuse existing step-5 block) → eligibility: `self_status == Joined` and power via `crate::community_membership::actor_power_meets_invite_tier(&mat, &self_owner)` (replaces the hardcoded `invite_threshold: u8 = 0` — parity with the countersign gate, per spec §3.1) → resolve `signed.invite_token.inviter`'s `enrolled_device_keys` from `mat.members` (absent → `InviteTokenSignerUnknown`, emit degraded) → `verify_packet_pure(&signed, now_fn, &keys)` → claim-bound insert (UNCHANGED, 2660-2730). Note in a comment that error precedence moved: membership-state errors now precede pure-verify errors (was reversed); acceptable — both are degraded-telemetry-only surfaces.
- [ ] **2.4 Run filter** — green. **2.5 Commit** `feat(invite): any Joined member accepts the redeem handshake (ZEB-911 slice 1)`.

### Task 3: Witness discovery ladder (joiner side)

**Files:** Modify `src-tauri/src/lib.rs` (`connectivity_redeem_invite_iroh_inner` 62717+, `RedemptionOutcome` constructors, `RedeemInviteErrorCode` if the outcome maps through it); locate the epoch-key unseal helper (`grep -n "sealed_epoch_key" src/community_invite.rs src/lib.rs` — reuse whatever `redeem_invite_inner` calls; do NOT duplicate unseal logic). Test: new unit tests near `zeb908_reuse_live_session_tests` (62534) + ladder helper units.

**Interfaces — produces:** `RedemptionOutcome::no_member_reachable()` (status string `no_member_reachable`), emitted only when the witness phase ran and no candidate connected.

- [ ] **3.1 Write failing unit tests** for two new pure helpers: (a) `witness_slot_keys(epoch_key, now_ms) -> Vec<Vec<VerifyingKey>>` — 4 slots × epoch-tolerance-window, assert derivation matches `rendezvous_slot_verifying_key` for each (slot, epoch) pair (publisher parity — the same fixture discipline `community_rendezvous.rs:422-424` uses); (b) `dedup_witness_candidates(candidates, exclude_node_id) -> Vec<Candidate>` — drops records whose `iroh_node_id` equals the rung-0-dialed admin node id or a prior candidate's.
- [ ] **3.2 Red.** **3.3 Implement helpers** (+ a `WitnessCandidate { routing: ReachabilityAnnouncePayload, owner: OwnerAddr }` local struct; derive `owner` from the resolved record's identity the same way the gateway driver derives `beacon_owner` — `OwnerAddr(identity.address_hash)`, `community_gateway_dial_driver.rs:518-521`).
- [ ] **3.4 Green.** **3.5 Commit** `feat(redeem): witness slot derivation + candidate dedup helpers (ZEB-911)`.
- [ ] **3.6 Restructure the dial phase:** wrap the existing Case-A block (63009-63183) so failure falls through instead of early-returning; add the witness phase:
  1. Unseal the epoch key from `payload.epoch_snapshot` (reuse located helper; on unseal failure log + skip witness phase → preserve today's `unreachable()`).
  2. For each slot 0..4: `resolver.resolve_window_freshest_with(&slot_window_keys, &verify)` with predicate `rec.verify_inner_sig() && rec.verify_freshness(now)` — **no identity-match** (joiner doesn't know witnesses; spec §6 decoy posture). Collect + decode routing blobs.
  3. Dedup (3.3 helper) against the rung-0 node id; for each surviving candidate: seed `reachability_resolver.seed_from_pkarr(candidate.owner, DeviceIdentityHash([0u8;16]), routing)`, then one bounded `connect` (same timeout shape as rung 0; no B4, no ZEB-908 merge — cold identities by construction). First success wins.
  4. Track `dialed_owner: OwnerAddr` (admin for rung 0, candidate owner for witness rungs); the shared tail's log fields switch from `inviter_addr` to it. The tail from `open_bi` (63184) onward is UNCHANGED in behavior.
  5. All candidates exhausted (and witness phase ran) → `Ok(RedemptionOutcome::no_member_reachable())`; witness phase skipped (no epoch key / zero slots resolved) → existing `unreachable()`.
- [ ] **3.7 Extend `zeb908`-adjacent unit coverage** where the harness allows (outcome mapping: ladder-exhausted → `no_member_reachable`; guards → unchanged codes). **3.8 Green + commit** `feat(redeem): rendezvous witness dial ladder (ZEB-911 slice 2)`.

### Task 4: Frontend outcome mapping

**Files:** locate the redeem outcome switch (`grep -rn "inviter_unreachable\|relays_warming_up" src/ --include="*.ts" --include="*.svelte"`); Test: sibling vitest file.

- [ ] **4.1 Failing vitest:** `no_member_reachable` renders the same retry affordance ("Try via local network" + retry) as `inviter_unreachable`, with copy naming the community, not the inviter ("No community member is currently reachable").
- [ ] **4.2 Red → implement branch → green** (`npx vitest run` scoped; `npx tsc --noEmit`). **4.3 Commit** `feat(ui): ladder-exhausted redeem outcome (ZEB-911)`.

### Task 5: E2E + full gates

**Files:** new `src-tauri/tests/` e2e alongside the existing redeem/pending-join integration tests (find via `grep -rln "connectivity_redeem_invite" tests/`); follow the headless-profile harness pattern (ZEB-254/889 tests).

- [ ] **5.1 Witness-redeem e2e:** admin + member B (relay-opted-in, Joined, publishing slots) + cold joiner; stop the admin's acceptor; joiner redeems via the iroh path → asserts Joined via B, countersign author = B, joiner's replica materializes Joined. (If full pkarr loopback is impractical in-harness, drive `open_join_after_resolve`-style seams: inject the witness candidate list and assert the ladder + handshake + countersign end-to-end; note which variant shipped in the task report.)
- [ ] **5.2 Negative e2e:** no advertisers → outcome `no_member_reachable`, DTO `pending` flow intact (camelCase key assertion per e2e conventions).
- [ ] **5.3 Full pre-PR sweep:** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; `npx tsc --noEmit && npx vitest run`. Git status clean.
- [ ] **5.4 Commit** remaining test work; push branch; open PR (`Closes ZEB-911`), fire CodeRabbit once, converge.

## Self-review notes

- Spec coverage: §3.1→T1/T2, §3.2→T1 (key retarget)+T2 (resolution), §3.3→no-op by design (asserted in T2a's countersign observation), §3.4→T2 (acceptor errors)+T3 (joiner code), §4.1-4.3→T3, §4.4→no-op, §9→T1/T2/T3/T5. Gap check: spec §3.4's "witness power-insufficient" variant — covered by reusing `SelfPowerInsufficient` (already exists, now reachable on witnesses); only `InviteTokenSignerUnknown` is genuinely new. Spec's "raised threshold reject" test requires a community with a non-zero materialized threshold — if no event kind can set it yet (ZEB-251 unshipped), test via a direct `MaterializedMembership` fixture against the eligibility helper instead of event replay; note in T2's report.
- Type consistency: `token_signer_keys: &[[u8; 32]]` in T1 == the slice T2 builds from `member.enrolled_device_keys` (`Vec<[u8;32]>` → `.as_slice()`); `WitnessCandidate.owner: OwnerAddr` == what `seed_from_pkarr` takes in T3.
- The B4 retry and ZEB-908 merge stay rung-0-only, verbatim.
