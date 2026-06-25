# Friend-token expiry: absolute epoch → server-computed TTL (ms)

**Ticket:** ZEB-507 — `generate_friend_token` stamps `expiresAt` verbatim as an
absolute epoch-ms with no unit guard, so a caller who passes a *seconds* value
mints a dead-on-arrival (always-expired) token. The failure is silent at mint
and misattributed at redeem (instant `friend token expired`).

**Decision (Jake, 2026-06-24):** Option 1 — eliminate the ambiguity at the API
boundary. Replace the absolute `expiresAt` argument with a **TTL duration in
milliseconds (`ttlMs`)**; the server computes `expires_at = now_ms + ttlMs`.
Unit is ms to stay consistent with the codebase's all-ms expiry contract (no new
ms↔s seam). No residual guard (the epoch is no longer caller-supplied, so the
"wrong unit" class is structurally impossible). `None`/omitted → no expiry,
exactly as today.

**Scope:** `generate_friend_token` only. The sibling `generate_invite` verb
shares the identical absolute-`expiresAt` shape (`rpc.rs:433`,
`community-service.ts:353`) — flagged as a fast-follow in the PR, **not** folded
in here.

## What does NOT change

- `mint_friend_token` / `mint_invite_token` keep taking `expires_at: Option<u64>`
  as an **absolute wall-clock ms** — the signing contract is correct and stays.
  We only move the TTL→absolute conversion to the API ingestion point.
- Both verify sites (`redeem`, acceptor consent gate) — already correct ms.
- The single GUI caller `FriendsPanel.svelte:314` calls `generateFriendToken()`
  with no argument → unaffected.

## Tasks

### Task 1: Pure TTL→expiry helper (TDD seam)

`generate_friend_token_impl` needs a loaded owner (heavy integration setup), so
the unit-testable seam is a pure function.

- **File:** `src-tauri/src/friend_token.rs`
- Add `pub(crate) fn resolve_expiry_ms(ttl_ms: Option<u64>, now_ms: u64) -> Option<u64>`:
  returns `ttl_ms.map(|t| now_ms.saturating_add(t))` (saturating = overflow-safe;
  a u64-ms TTL can't realistically overflow, but it costs nothing to be exact).
- **Test** (in the existing `mod tests`): `None → None`; `Some(4h_ms)` at a fixed
  `now` → `now + 4h`; saturating at `u64::MAX`; `Some(0) → Some(now)` (documents
  that a zero TTL yields an immediately-expired token — the caller's explicit
  choice, distinct from the old unit footgun).

### Task 2: Wire the impl + API boundary (Rust)

- **`src-tauri/src/lib.rs`** — `generate_friend_token` command + `_impl`: rename
  param `expires_at: Option<u64>` → `ttl_ms: Option<u64>`. After `wall_now_ms` is
  computed (the same `now` used for `minted_at`), derive
  `let expires_at = crate::friend_token::resolve_expiry_ms(ttl_ms, wall_now_ms);`
  and pass `expires_at` into `mint_friend_token(...)`.
- **`src-tauri/src/api/rpc.rs`** — `GenerateFriendTokenArgs`: `expires_at` →
  `ttl_ms`; verb closure `a.expires_at` → `a.ttl_ms`.

### Task 3: Frontend service + tests

- **`src/lib/friend-service.ts`** — `generateFriendToken(expiresAt?)` →
  `generateFriendToken(ttlMs?)`; invoke `{ ttlMs: ttlMs ?? null }`; update the
  doc comment (absolute deadline → relative TTL in ms).
- **`src/lib/friend-service.test.ts`** — update the two assertions: default
  `{ ttlMs: null }`; forwarded value asserts `{ ttlMs: <ms> }`.

## Gates

- `cargo fmt --all -- --check`
- `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
- `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(friend_token)'` (+ `--lib` sweep)
- `npx tsc --noEmit` ; `npx vitest run src/lib/friend-service.test.ts` (+ full vitest before push)
- Final pre-push: full `--all-targets` nextest sweep is CI's job; locally run the scoped gates above.
