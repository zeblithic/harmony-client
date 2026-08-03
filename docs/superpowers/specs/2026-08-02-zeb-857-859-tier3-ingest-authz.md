# Tier-3 Voting Ingest-Authz Completion (ZEB-857 / ZEB-858 / ZEB-859) — Design

**Date:** 2026-08-02
**Tickets:** ZEB-859 (kd=cl ungated), ZEB-858 (se-mode verify_sr decrypt DoS), ZEB-857 (local/peer authz asymmetry)
**Branch:** `zeblith/zeb-857-859-tier3-ingest-authz`
**Base:** main @ `57a0d309`

## Goal

Complete the tier-3 community-voting ingest-authz surface that ZEB-850 (T-VOTE-LANE) established: close the one remaining ungated forgeable kind (`kd=cl`), bound the per-event cost of the se-mode result verifier so an insider can't force repeated threshold-decrypts, and make the node's own local-publish path verify the user-action kinds symmetrically with the peer-ingest path.

## Architecture

All three changes live in the tier-3 voting subsystem:
- `src-tauri/src/community_voting_log_engine.rs` — the ingest seam (`inbound_eligibility_check`), the engine-auto orchestration (`trigger_kd_cl`, `try_finalize_secret_tally`), the local `publish_event`, and the `VotingReplayTracker`.
- `src-tauri/src/community_voting_tier3.rs` — the `verify_*` family, `Tier3PollState`, `VerifyError`, `Stage`, `recover_secret_tally`, and `apply_event`.

The unifying invariant this set completes:

> **Every forgeable tier-3 voting event is authorized identically on the local-publish and peer-ingest paths, with bounded work per event, and every lifecycle transition a peer can inject is locally recomputable so a forged one is rejected fail-closed.**

The design principle throughout mirrors ZEB-850/ZEB-846: **each node recomputes the legitimate value from state it already holds** (window elapse, tally) and rejects anything that doesn't match, clamping any peer-supplied wall-clock to `receiver-now + MAX_FORWARD_SKEW_MS` (never trusting a peer stamp).

**Landing order (matters):** 859 → 858 → 857. 858's memo is keyed on `close_event_hash`, which 859's `verify_cl` governs; 857 is independent but lands last as the lowest-risk additive change.

---

## Component 1 — ZEB-859: `verify_cl` for `kd=cl` (PollClose)

### Problem

`inbound_eligibility_check`'s Sortition arm treats `kd=cl` as engine-signed and does nothing (`PollClose => {}`, `community_voting_log_engine.rs:3428`). No `verify_cl` exists. A peer `kd=cl` is bounded only by membership-V6 (`verify_voting_event`), so **any community member can inject a forged `kd=cl`**, setting `close_event_hash` and prematurely closing a poll (cuts off ratification early — a griefing vector). It cannot forge a *result* (that's `verify_sr`-gated), but `close_event_hash` is `verify_sr`'s R1 precondition, so a forged close changes downstream gating.

### Fix

The legitimate close condition is **locally recomputable** — it is exactly what the engine-auto close trigger computes (`trigger_kd_cl`, `community_voting_log_engine.rs:1141`–`1157`):
- stage at the relevant HLC is `Ratification`, AND
- `wall_ms >= poll_create_hlc.wall_ms + total_window_ms`, where `total_window_ms = (deliberation + drafting + ratification)_window_seconds * 1000`.

**Extract that predicate into ONE shared helper** on `Tier3PollState` (in `community_voting_tier3.rs`), e.g.:

```rust
/// True iff `at_wall_ms` (already clamped to receiver-now by the caller) is
/// at/after the poll's full-window close boundary AND the stage at that wall
/// is Ratification. Shared by the engine-auto close trigger and verify_cl so
/// a node can never reject its own legitimately-triggered close.
pub fn close_condition_met(&self, at_wall_ms: u64) -> bool
```

- **`trigger_kd_cl`** (engine) calls it with its already-clamped `last_wall` (`t3.last_hlc.wall_ms` clamped to `now + MAX_FORWARD_SKEW_MS`, existing ZEB-846 Layer-2 clamp at `:1123`–`1135`).
- **`verify_cl(event, poll_state, receiver_now_ms) -> Result<(), VerifyError>`** (new, in `community_voting_tier3.rs` next to the other verifiers) clamps the **incoming close event's own** `event.hlc.wall_ms` to `min(event.hlc.wall_ms, receiver_now_ms + MAX_FORWARD_SKEW_MS)` and calls the same helper. A future-stamped close that claims the window elapsed is clamped back to receiver-now and rejected unless the window has genuinely elapsed by the receiver's clock.

Enforce it in the `kd=cl` ingest arm (replace `PollClose => {}` at `:3428`). On failure return a new `VerifyError::CloseConditionNotMet`.

### Invariants to honor

- **Validate the *condition*, not a specific hash.** `close_event_hash` legitimately differs across replicas under reordering (see the comment at `community_voting_log_engine.rs:1167`–`1174`, "not in any cross-peer state-root; do NOT treat a peer mismatch as corruption"). `verify_cl` must never require a particular hash — only that *some* legitimate close is warranted by the timeline.
- **Never trust the peer wall.** Clamp to receiver-now; use the receiver's own clock via the same mechanism the trigger uses (`SystemTime::now()`-derived, ZEB-846 pattern). Follow the established `clock_trust` fail-open contract if receiver-now is unavailable (theoretical: clock before UNIX_EPOCH) — do not reject in that case, consistent with the rest of ZEB-831.
- **Shared predicate is load-bearing.** Any drift between the trigger's condition and `verify_cl`'s condition would make a node reject its own legitimate close. They MUST call the same helper.
- `kd=cl` is not proposer-gated (any signer may publish it — `:1068`); `verify_cl` gates on the *timeline*, not the author, preserving that.

---

## Component 2 — ZEB-858: bound se-mode `verify_sr` threshold-decrypt cost

### Problem

`verify_sr`'s se-branch (`community_voting_tier3.rs:1441`) runs `recover_secret_tally` — a Lagrange-combine + BSGS discrete-log over the electorate, cost ~ `committee_threshold × candidate_pairs × √electorate` — **on the ingest path, once per inbound `kd=rs`**. There is no post-finalize early-out (terminal rejection is downstream in `apply_event:447`), and distinct-signed `kd=rs` from different members escape the coordinate dedup (`(actor, device_id, wall, logical)`) by construction (multi-writer engine-auto lane, intentional for liveness). So an authenticated member can publish many distinct-signed `kd=rs` for one poll — even an already-finalized one — and force a fresh threshold-decrypt each time. Insider DoS. (The in-tree comment at `community_voting_log_engine.rs:3479` already names this fix: "post-finalize early-out + memoization.")

Additionally, `verify_sr` returns `VerifyError::TallyMismatch` for BOTH "shares not yet available" (`recover_secret_tally` → `None`, benign fail-closed) and "genuine forgery" (recomputed ≠ claimed) — an overloaded error.

### Fix (three parts)

**(a) Post-finalize early-out.** In the `kd=rs` ingest arm, before calling `verify_sr`, if the poll is already terminal (`stage == Finalized`, i.e. a result is set), reject cheaply — the event is terminal-rejected at apply anyway. Return a new benign `VerifyError::PollAlreadyFinalized` (dropped silently at ingest, not treated as an attack). This closes the spam-after-finalization window for free (poll_state is already cloned under the `with_tier3` guard).

**(b) Memoize the recompute by `(poll_id, close_event_hash)`.** The expensive part is `recover_secret_tally`. Cache its **output** (the recomputed tally), not a pass/fail bit:
- Split the recompute out of `verify_sr` into a pure function, e.g. `recompute_expected_result(poll_state, &ordered_candidates) -> Option<Tally>` (se → `recover_secret_tally`; pu → `Some(tally_star(...))`).
- Add a memo to `VotingReplayTracker`: `verify_sr_memo: HashMap<(PollId, [u8;32]), Tally>` (keyed on poll id + `close_event_hash`), holding the recomputed expected result. It is **ephemeral in-memory dedup state, NOT CRDT/`voting_log` state** — never persisted, never replicated, rebuilt-empty-on-restart is correct. The `notify_dirty` rule does NOT apply (do not route it through owner-state persistence).
- On `kd=rs` ingest: compute the memo key; `expected = memo.get_or_insert_with(key, || recompute_expected_result(...))`; then the comparison `expected != payload.result` still runs for **every** event (so a forged result is always caught), but the decrypt runs at most once per `(poll, close_hash)`.
- **Correctness note:** caching pass/fail would be wrong — a later distinct-signed `kd=rs` could carry a *different, forged* result for the same `(poll, close_hash)`; caching the recomputed *tally* and re-comparing catches it.
- **Threading:** `inbound_eligibility_check` does not currently receive the tracker. Either thread the tracker (or just the memo handle) into it, or lift the memo/early-out into the `kd=rs` arm within `process_inbound`/`apply_backfilled_event` which already hold the tracker. Prefer threading a `&Mutex<VotingReplayTracker>` (or a dedicated memo handle) into `inbound_eligibility_check` to keep the dispatch table in one place; the memo lock is held only for the map lookup/insert, never across the decrypt.

**(c) Disambiguate the overloaded error.** Split `TallyMismatch`:
- `recover_secret_tally`/`recompute_expected_result` → `None` ⇒ `VerifyError::TallySharesNotReady` (fail-closed, benign — backfill/retry will supply shares).
- recomputed `Some(expected)` and `expected != payload.result` ⇒ keep `VerifyError::TallyMismatch` (genuine forgery).

### Coupling with 859

The memo key is `close_event_hash`. Since 859's `verify_cl` rejects forged closes, a rejected close never becomes a poll's `close_event_hash`, so the memo never caches an entry for an illegitimate close. Land 859 first. The memo treats the hash as opaque (a local CPU-cache key), consistent with "close_event_hash may differ across replicas."

---

## Component 3 — ZEB-857: surgical local-path verify for user-action kinds

**Chosen approach (Jake, 2026-08-02): surgical — verify user-action kinds at publish, skip engine-auto self-mints.** (Rejected: full symmetry in `publish_event` — too invasive/stall-prone; doc+debug-assert only — no prod protection; defer — unnecessary given the surgical path is contained.)

### Problem

`publish_event` (`community_voting_log_engine.rs:1893`) runs no verifiers (its doc delegates verify to the caller); only `process_inbound`/`apply_backfilled_event` call `inbound_eligibility_check`. Local origination is gated by ad-hoc per-IPC `check_eligibility` (membership only), not the symmetric kind-specific verifier table. A buggy client that self-authors an illegitimate user action (e.g. a mini-public member who already DECLINED submits a `kd=da`) applies it locally with no error, while every peer's ingest gate rejects it — silent local/peer divergence.

### Fix

On the local-publish path, run the **kind-specific SYNC verifier** for the **user-originated forgeable kinds only**, turning a self-authored illegitimate event into a clean local error before apply.

Kinds and verifiers (all sync; none needs `snapshot` or `beacon_oracle`):
- `kd=md` (MiniPublicDecline), `kd=dc` (DraftCandidate) → `verify_sd`
- `kd=da` (DraftApproval) → `verify_sd` + `verify_da_candidate_exists` (mirror the ingest arm at `:3500`–`3507`)
- `kd=rb` (RatificationBallot) → `verify_ratification_ballot`

**Why this is safe and needs no self-mint exemption:** these four kinds are **disjoint** from the engine-auto self-mints (`kd=cl/rs/sf/ss/ts`), so gating on kind automatically excludes every self-mint. The engine-auto kinds (which swallow publish errors and could stall a poll if fail-closed) are never touched. The async/oracle-dependent verifiers (`verify_ss`, se `verify_sr`) are only ever needed for engine-auto kinds, so they are out of scope here.

**Insertion point:** inside `publish_event`, after the event kind is known and before `apply_with_snapshot` (`:2002`). Acquire the tier3 poll-state clone via `with_tier3` (same mechanism the ingest arm uses at `:3220`), run the matching verifier, and on `Err` return `Err(String)` (surfaces to the IPC caller / Tauri rejection). Only the four user-action kinds take this path; all other kinds (creates, engine-auto, tier1/2, ungated ds/dv/ts) are unchanged.

### Out of scope for 857

- `kd=ds`/`dv`/`ts` (Deliberation/DraftVote/TallyShare) — ungated at ingest too (`_ => {}`, inline-checked in `apply_event`); no ingest verifier to mirror, so not part of this symmetry pass.
- A malicious (not merely buggy) local client can bypass any local check by editing its own code — this fix targets the realistic case (our own IPC bug / a buggy client silently desyncing), converting it to a surfaced local error.

---

## Error handling

New `VerifyError` variants (in `community_voting_tier3.rs`):
- `CloseConditionNotMet` — 859: forged/premature `kd=cl`.
- `PollAlreadyFinalized` — 858(a): `kd=rs` for a terminal poll (benign; dropped at ingest).
- `TallySharesNotReady` — 858(c): se recompute returned `None` (benign fail-closed; distinct from forgery).

All map through the existing `inbound_eligibility_check` → `Result<(), String>` → `process_inbound` drop path. `PollAlreadyFinalized` and `TallySharesNotReady` are benign (expected under normal operation / backfill); `CloseConditionNotMet` and `TallyMismatch` indicate a forged event.

## Testing strategy

Follow the existing conventions (recon Q5):
- **Verifier unit tests** inline in `community_voting_tier3.rs` (`#[cfg(test)] mod tests`, next to the `verify_*` family; fixtures `make_event`, `ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee`, `build_real_se_ballot_payload`, `rb_event_at_with_actor`):
  - `verify_cl`: accepts a legitimate close (Ratification + window elapsed); rejects premature (stage < Ratification), rejects window-not-elapsed, rejects future-stamped-wall (clamp defeats it); a node accepts its OWN trigger-minted close (shared-predicate parity test).
  - `verify_sr` error split: `TallySharesNotReady` when shares below threshold; `TallyMismatch` only on genuine forgery.
- **Ingest-dispatch + tracker tests** inline in `community_voting_log_engine.rs`:
  - `kd=cl` ingest arm now rejects a forged premature close and accepts a legitimate one.
  - `kd=rs` post-finalize early-out: an already-finalized poll rejects a new `kd=rs` **without** running `recover_secret_tally` (assert via a counter/spy or by asserting the cheap-reject error variant).
  - memo: two distinct-signed `kd=rs` for the same `(poll, close_hash)` recompute the tally once (assert one decrypt); a second `kd=rs` with a **forged** result for the same key is still rejected (`TallyMismatch`).
- **857 local-path** unit/integration: `publish_event` of a `kd=da` from a declined member returns `Err` locally (no silent apply); a legitimate `kd=da` still applies. Covered best by the in-process multi-engine integration style under `tests/community_voting/` (e.g. mirror `community_voting_tier3_ipc_integration.rs`), asserting engine_a no longer diverges from engine_b on a self-authored illegitimate approval (the exact ZEB-850 `ipc_full_lifecycle` observation, now a local error).

## Global constraints

- **Rust gates (from `src-tauri/`):** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast`. Iterative test runs use `--lib` (a lib change relinks ~97 integration binaries); full `--all-targets --workspace` sweep before the push.
- **Frontend:** untouched by this work (backend-only). No `npm test` script exists; frontend gate is `npx tsc --noEmit` + `npx vitest run` from repo root — not expected to be needed here.
- **No persisted/replicated state added.** The 858 memo lives on the ephemeral `VotingReplayTracker`; it must never reach `voting_log`/owner-state persistence or `notify_dirty`.
- **Clock discipline:** every wall comparison clamps a peer stamp to `receiver-now + MAX_FORWARD_SKEW_MS`; the receiver clock is read once and threaded (testable), never a peer value.
- **Shared-predicate rule:** 859's close condition is a single helper called by both the trigger and `verify_cl`.

## Out of scope / follow-ups (already filed)

- ZEB-864/865/866 (open-join acceptor follow-ups) — unrelated.
- ZEB-860 (tier-3 poll projection replay order), ZEB-861 (unbounded watermark lane map), ZEB-862 (restart-durable first-observation clock) — separate tier-3 items, not part of this authz-completion set.
