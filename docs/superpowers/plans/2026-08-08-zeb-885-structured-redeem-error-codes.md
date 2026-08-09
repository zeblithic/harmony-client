# ZEB-885 — Structured redeem-invite error codes: Implementation Plan

> **For agentic workers:** executed inline by the author (Koya). Single-file compile-cascade in `lib.rs` (72k lines) — sequential, not parallel. Gate incrementally.

**Goal:** Route the redeem-invite UI's error copy off a typed backend error code instead of regex-matching prose; repair the latent bug where bootstrap failures currently hit the generic fallback.

**Design:** `docs/superpowers/specs/2026-08-08-zeb-885-structured-redeem-error-codes-design.md`

**Tech Stack:** Rust (Tauri backend), Svelte 5 + TypeScript (frontend), serde, vitest, nextest.

## Global Constraints

- Cargo from `src-tauri/`; frontend from repo root.
- Gates: `cargo fmt --all -- --check` · `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` · `cargo nextest run --locked --workspace --all-targets --features test-fixtures` · `npx tsc --noEmit` · `npx vitest run`.
- `RedeemInviteErrorCode` serializes `#[serde(rename_all = "snake_case")]`; the 17 codes are fixed by the spec's taxonomy table.
- **No `RedemptionOutcome.status` retype** (non-goal) — iroh Ok-side status strings stay `String`.
- **RPC wire unchanged** — flatten `RedeemInviteError → e.to_string()` at `api/rpc.rs` redeem sites.
- Second-order: preserve error *precedence* (a not-loaded node still reports node-not-ready first, etc.); the code assignment must not reorder which failure a caller hits first.

## File Structure

- `src-tauri/src/community_invite.rs` — **new** `RedeemInviteErrorCode` enum + `RedeemInviteError` struct + `From`/`as_str`/`Display` impls, adjacent to the existing `RedeemBootstrapVerifyError` (keeps the redeem error taxonomy in one module). Unit tests inline.
- `src-tauri/src/lib.rs` — LAN seam return-type cascade + per-site code assignment; iroh seam hard-error cascade; boundary tests in `mod redeem_invite_inner_tests`.
- `src-tauri/src/iroh_invite_acceptor.rs` — one call site of `redeem_invite_inner_with_overrides` (maps error → `join_failed` status).
- `src-tauri/src/api/rpc.rs` — two `.map_err(|e| e.to_string())` flattens.
- `src/lib/redeem-invite-errors.ts` — regex table → `switch(code)`.
- `src/App.svelte` — structured catch extraction; `redeemError` type `string → RedeemInviteError`.
- `src/lib/components/RedeemInviteDialog.svelte` — `error` prop type; iroh `catch` routes code.
- `src/lib/community-service.ts` — return/throw typing only.
- `src/lib/__tests__/redeem-invite-errors.test.ts` + `src/lib/components/__tests__/RedeemInviteDialog.test.ts` — rewrite to codes.

---

### Task 1: Backend error types (standalone, no cascade)

**Files:** Create in `community_invite.rs` (after `RedeemBootstrapVerifyError`, ~line 1490). Test inline.

**Produces:** `RedeemInviteErrorCode` (17 variants, `Copy + Serialize + snake_case`), `RedeemInviteError { code, message }` (`Serialize + Display + Error`), `RedeemInviteError::new`, `RedeemInviteErrorCode::as_str() -> &'static str`, `From<RedeemBootstrapVerifyError>`, `From<InviteUrlError>`, `From<CommunityInviteVerifyError>`, `From<String>`/`From<&str>` (→ `internal`).

- [ ] **Step 1 — Failing tests first.** Add `#[cfg(test)] mod redeem_invite_error_code_tests`: (a) `serde_json::to_value(RedeemInviteError::new(BootstrapSignatureInvalid, "m"))` == `{"code":"bootstrap_signature_invalid","message":"m"}`; (b) each `RedeemBootstrapVerifyError` variant `.into::<RedeemInviteError>().code` == the matching code (5 cases) and `.message` == the variant's Display; (c) `InviteUrlError::WrongScheme(..).into().code == InviteUrlMalformed`; (d) `RedeemInviteErrorCode::BootstrapMissing.as_str() == "bootstrap_missing"` for all 17; (e) `String → internal`. Run: expect FAIL (types absent).
- [ ] **Step 2 — Define the types + impls.** Enum, struct, `Display` (writes `message`), `Error`, `as_str` (matches serde strings), the three typed `From`s (bootstrap via variant match reusing the `reason_tag()` mapping; url/enrollment → their single codes with `message = e.to_string()`), `From<String>`/`From<&str>`.
- [ ] **Step 3 — Run tests → PASS.** `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(redeem_invite_error_code)'`.
- [ ] **Step 4 — Gate + commit.** fmt + clippy (`--all-targets`) on the new module. Commit: "ZEB-885: RedeemInviteError{Code} types + From mappings".

**Note:** keep `RedeemBootstrapVerifyError::reason_tag()` as the single source for the 5 bootstrap strings — `From` and `as_str` must agree with it (add a test asserting `code.as_str() == variant.reason_tag()` for the five).

---

### Task 2: LAN seam cascade + per-site codes

**Files:** `lib.rs` (`redeem_invite` 41517, `redeem_invite_impl` 41528, `redeem_invite_inner` 40327/40354, `redeem_invite_inner_with_overrides` 40382, `join_open_community_inner`), `iroh_invite_acceptor.rs:~745`, `api/rpc.rs:836`.

**Consumes:** Task 1 types.

- [ ] **Step 1 — Flip the inner's return type.** `redeem_invite_inner_with_overrides`: `Result<Dto, String> → Result<Dto, RedeemInviteError>`. Compile (`cargo check`) to get the exhaustive list of error sites the compiler flags; that list IS the work-queue (scope-by-compiler, not eyeball).
- [ ] **Step 2 — Assign a code per flagged site** per the spec table: bootstrap/url/enrollment via `?` + `From` (or `.map_err(RedeemInviteError::from)`); snapshot guards (`owner_not_loaded`, missing handles, poisoned) → `node_not_ready` (poisoned → `internal`); generation fence → `generation_changed`; token-missing → `invite_token_missing`; engine insert → `engine_insert_failed`; oneshot closed → `internal`. **Preserve ordering** — do not move checks, only change the `Err(...)` construction.
- [ ] **Step 3 — Cascade the wrappers.** `redeem_invite_inner`, `redeem_invite_impl`, `redeem_invite` command, `join_open_community_inner`: propagate `Result<_, RedeemInviteError>`. Command return type → `Result<Dto, RedeemInviteError>` (Tauri serializes structured).
- [ ] **Step 4 — Fix the two non-command consumers.** `iroh_invite_acceptor.rs` call site: it discards the specific error into `join_failed` status — use `e.message`/`e.to_string()` where it logs. `api/rpc.rs:836`: `.map_err(|e| e.to_string())`.
- [ ] **Step 5 — Boundary tests.** Extend `mod redeem_invite_inner_tests` (lib.rs:41973): drive each reachable site, assert `err.code == Expected` (node_not_ready, generation_changed, invite_token_missing, engine_insert_failed, bootstrap_* via a fixture with a bad bootstrap, internal). Assert precedence unchanged (not-loaded → node_not_ready before any decode).
- [ ] **Step 6 — Gate + commit.** `cargo check` clean, targeted nextest for the redeem tests, clippy `--all-targets`. Commit: "ZEB-885: thread RedeemInviteError through the LAN redeem seam".

---

### Task 3: iroh seam hard-error cascade

**Files:** `lib.rs` (`connectivity_redeem_invite_iroh` 61237, `_impl` 61257, inner `61989`, open counterpart `61505`), `api/rpc.rs:847,857`.

**Consumes:** Task 1 types.

- [ ] **Step 1 — Flip hard-error type.** `connectivity_redeem_invite_iroh_impl` (+ inner + open counterpart): `Result<RedemptionOutcome, String> → Result<RedemptionOutcome, RedeemInviteError>`. `cargo check` → flagged Err sites.
- [ ] **Step 2 — Code the internal Err sites.** poisoned → `internal`; generation-changed (`61399`) → `generation_changed`; registry-torn-down → `generation_changed`; other internal → `internal`. **Leave every `Ok(RedemptionOutcome { status: ... })` untouched** (Ok-side vocabulary is a non-goal to retype).
- [ ] **Step 3 — Command + RPC.** Command return → `Result<RedemptionOutcome, RedeemInviteError>` (Tauri structured). `api/rpc.rs:847` (+ `857` open verb if it shares the type): `.map_err(|e| e.to_string())`.
- [ ] **Step 4 — Vocabulary drift-guard test.** Assert the iroh failure statuses that overlap the code space (`inviter_unreachable`, `join_failed`, `missing_admin_identity_pub`) equal `RedeemInviteErrorCode::_.as_str()` — pins the shared vocabulary without retyping the sites.
- [ ] **Step 5 — Gate + commit.** Commit: "ZEB-885: code the iroh redeem hard-error branch; share status vocabulary".

---

### Task 4: Frontend — switch on code, rewrite tests

**Files:** `redeem-invite-errors.ts`, `App.svelte`, `RedeemInviteDialog.svelte`, `community-service.ts`, both `__tests__`.

**Consumes:** the wire shapes from Tasks 2–3 (`{code, message}` on both Tauri commands).

- [ ] **Step 1 — Rewrite `redeem-invite-errors.ts` (tests first).** Rewrite `redeem-invite-errors.test.ts` to feed **codes**: each code → expected `{summary, hint}`; `unknown`/absent → fallback; `raw`/`message` passthrough. Run → FAIL.
- [ ] **Step 2 — Implement.** `RedeemInviteErrorCode` TS union (17 + `unknown`); `RedeemInviteError` interface; `redeemInviteCopy(code)` switch (port the surviving summaries/hints from the current table, drop the dead ones — `bootstrap_invalid_pubkey`/`bootstrap_address_mismatch`/`bootstrap_insert_failed`/`already_member`/`malformed_url` regex entries gone); `mapRedeemInviteError(err: RedeemInviteError)`. Run → PASS.
- [ ] **Step 3 — `App.svelte` + `community-service.ts`.** Add `toRedeemInviteError(e): RedeemInviteError` (object with `code` → as-is; `Error`/string → `{code:'unknown', message}`). `redeemError: RedeemInviteError | null`. Dialog `error` prop type `string → RedeemInviteError | null`. Update `mapped = error ? mapRedeemInviteError(error) : null` and the disclosure (`Telemetry tag` → `{mapped.code}`, `Raw error` → `{mapped.raw}`).
- [ ] **Step 4 — `RedeemInviteDialog.svelte` iroh catch.** `catch (e) { const err = toRedeemInviteError(e); irohError = redeemInviteCopy(err.code).summary; ... }` (or keep raw message for `internal`). Leave the Ok-side `status` switch and its richer copy as-is (fallback button, joined-dismiss, community hint).
- [ ] **Step 5 — Dialog tests.** Update `RedeemInviteDialog.test.ts`: `error` prop is now `{code, message}`; assert code-sourced copy + disclosure shows the code; a legacy string/`Error` rejection → `unknown` copy.
- [ ] **Step 6 — Gate + commit.** `npx tsc --noEmit` + `npx vitest run`. Commit: "ZEB-885: frontend switches redeem copy on structured code".

---

### Task 5: Full sweep + PR

- [ ] **Step 1 — git-status clean check**, then full gates on the working tree: fmt, clippy `--all-targets`, `cargo nextest run --locked --workspace --all-targets`, tsc, vitest. (Local gates run the working tree — commit everything first.)
- [ ] **Step 2 — Self-review the diff** for second-order issues: any error-precedence change? any `Ok`-status accidentally reworded? dead frontend patterns fully removed? `unknown` fallback reachable?
- [ ] **Step 3 — Push branch, open PR** titled `ZEB-885: structured redeem-invite error codes (switch on code, not prose)`. Body: the reframe (latent bug), the two-path handling, the RPC-wire-unchanged decision, test summary. Attach to Linear.
- [ ] **Step 4 — Fire `@coderabbitai review` once**; converge all bot buckets → one push per round; never auto-merge; pushover at ready.

## Self-Review (plan vs spec)

- Spec coverage: taxonomy (T1), LAN structured error + bug repair (T2), iroh fold-in via shared vocab + coded hard-error (T3), frontend switch + dead-pattern prune + honest tests (T4), RPC-wire-unchanged (T2/T3 step 4). ✓
- Type consistency: `RedeemInviteError{code,message}` identical Rust↔TS; `as_str()` ↔ serde ↔ TS union pinned by tests. ✓
- No silent caps: dead frontend patterns are removed deliberately (documented in T4), not dropped silently.
