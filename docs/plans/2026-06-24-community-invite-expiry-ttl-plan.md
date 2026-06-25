# Community-invite expiry: absolute epoch → server-computed TTL (ms)

**Ticket:** ZEB-564 — direct fast-follow to ZEB-507 (#342, merged `47380942`).
`generate_invite` shares the identical absolute-`expiresAt` footgun: a caller
who passes a *seconds* value mints a dead-on-arrival community invite
(`lib.rs:22328` uses `expires_at` verbatim).

**Decision (Jake, 2026-06-24):** Mirror the ZEB-507 fix — replace the absolute
`expiresAt` argument with a **TTL duration in ms (`ttlMs`)**, computed
server-side. **Difference from ZEB-507:** community invites keep a **7-day
default** when omitted (friend tokens default to no expiry), so the resolver
must apply that default rather than returning `None`.

## What does NOT change

- `mint_invite_token` keeps taking absolute `expires_at: Option<u64>` (ms) — only
  the API ingestion point changes.
- GUI: `community-service.ts generateInvite()` already hardcodes the expiry field
  to `null` (no exposed param) and `App.svelte:2976` passes nothing extra → only
  the JSON key is renamed. The headless `e2e-harness` driver sends no expiry
  field → unaffected.

## Tasks

### Task 1: Pure resolver helper (TDD seam)

- **File:** `src-tauri/src/lib.rs` (free fn near `generate_invite`, ~line 22063)
- Add `const INVITE_DEFAULT_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;` and
  `fn resolve_invite_expiry_ms(ttl_ms: Option<u64>, now_ms: u64) -> u64`
  returning `now_ms.saturating_add(ttl_ms.unwrap_or(INVITE_DEFAULT_TTL_MS))`.
  Returns a concrete `u64` (invites always carry an expiry).
- **Tests** (in the existing `generate_invite_helper_tests` mod, `lib.rs:50667`):
  `None → now + 7d`; `Some(4h) → now + 4h`; saturating at `u64::MAX`;
  `Some(0) → now` (immediate-expiry edge, the caller's explicit choice).

### Task 2: Wire impl + API boundary (Rust)

- **`lib.rs`** — `generate_invite` command + `_impl`: rename `expires_at` →
  `ttl_ms`. At `:22328` replace
  `let effective_expiry = expires_at.or(Some(wall_now_ms + 7d));`
  with `let effective_expiry = Some(resolve_invite_expiry_ms(ttl_ms, wall_now_ms));`.
- **`rpc.rs`** — `GenerateInviteArgs`: `expires_at` → `ttl_ms` (+ doc); verb
  closure `a.expires_at` → `a.ttl_ms`.

### Task 3: Frontend key rename

- **`src/lib/community-service.ts`** — `generateInvite`: `expiresAt: null` →
  `ttlMs: null`. (No exposed param; no frontend test references this verb.)

## Gates

- `cargo fmt --all -- --check`
- `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
- `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(generate_invite) or test(resolve_invite_expiry)'`
- `npx tsc --noEmit` ; `npx vitest run` (sanity)
- CI runs the full `--all-targets` sweep.
