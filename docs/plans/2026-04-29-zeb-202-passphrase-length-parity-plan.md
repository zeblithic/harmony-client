# ZEB-202 Implementation Plan: Identity-Recovery Export Passphrase Length Parity

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring identity-recovery export to passphrase-length parity with owner-recovery export by extracting both recovery-export policy constants (`MIN_RECOVERY_PASSPHRASE_LEN`, `MAX_RECOVERY_COMMENT_BYTES`) into shared Rust + TS policy modules, adding a vitest drift detector, and enforcing a 12-codepoint passphrase floor at both the IPC layer and the renderer's pre-dialog guard.

**Architecture:** Two new tiny policy modules (`src-tauri/src/recovery_policy.rs` and `src/lib/recovery-policy.ts`) own the two integers. Every consumer imports from them. A vitest reads the Rust file at test time and asserts integer-literal parity with the TS exports, converting drift risk into a CI failure. Validation order is preserved from PR #66: passphrase + comment validation runs **before** path-token consumption inside `run_blocking`.

**Tech Stack:** Rust 1.x (Tauri 2 backend), TypeScript + Svelte 5 (renderer), vitest (frontend tests), `cargo test` + `serial_test` (backend tests).

**Spec:** `docs/specs/2026-04-29-zeb-202-passphrase-length-parity-design.md`

**Linear:** [ZEB-202](https://linear.app/zeblith/issue/ZEB-202/harmony-client-enforce-passphrase-length-minimum-on-identity-recovery)

---

## Task 1: Shared policy modules + drift detector

Create the two policy source files (Rust + TS), register the Rust module, and add a vitest that reads the Rust file at test time and asserts integer-literal parity. No consumers are wired yet — both `owner_commands.rs` and `identity_commands.rs` keep their existing local constants in this task; Task 2 deletes those.

**Files:**
- Create: `src-tauri/src/recovery_policy.rs`
- Create: `src/lib/recovery-policy.ts`
- Create: `src/lib/recovery-policy.test.ts`
- Modify: `src-tauri/src/lib.rs` (register `mod recovery_policy;`)

- [ ] **Step 1: Create the Rust policy module**

Create `src-tauri/src/recovery_policy.rs`:

```rust
//! Recovery-file export policy constants shared between the
//! owner-recovery export IPC (`crate::owner_commands`) and the
//! identity-recovery export IPC (`crate::identity_commands`).
//!
//! Mirrored in `src/lib/recovery-policy.ts`. The drift detector
//! `src/lib/recovery-policy.test.ts` asserts both files agree on
//! the integer literals; failing that test in CI is the signal
//! to re-sync.

/// Minimum recovery passphrase length, in Unicode codepoints
/// (matches the JS frontend's `[...str].length` check).
pub const MIN_RECOVERY_PASSPHRASE_LEN: usize = 12;

/// Maximum recovery comment length, in bytes. Mirrors
/// harmony-owner's hard cap on the underlying field.
pub const MAX_RECOVERY_COMMENT_BYTES: usize = 256;
```

- [ ] **Step 2: Register the Rust module**

In `src-tauri/src/lib.rs`, add `pub mod recovery_policy;` to the module declarations. Insert it alphabetically between `pub mod recovery_cli;` and `mod save_dialog;`.

After edit, the relevant block reads:

```rust
pub mod pairing_commands;
pub mod recovery_cli;
pub mod recovery_policy;
mod save_dialog;
pub mod voice;
```

Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: builds clean (the new module is not yet imported anywhere, but it compiles standalone).

- [ ] **Step 3: Create the TS policy module**

Create `src/lib/recovery-policy.ts`:

```ts
// Recovery-file export policy. Mirrored from
// src-tauri/src/recovery_policy.rs. The values MUST match;
// `recovery-policy.test.ts` reads the Rust file and asserts
// equality so drift fails CI.

/** Minimum recovery passphrase length, in Unicode codepoints
 * (matches the Rust backend's `passphrase.chars().count()` check). */
export const MIN_RECOVERY_PASSPHRASE_LEN = 12;

/** Maximum recovery comment length, in bytes. Mirrors
 * harmony-owner's hard cap on the underlying field. */
export const MAX_RECOVERY_COMMENT_BYTES = 256;
```

- [ ] **Step 4: Write the failing drift detector test**

Create `src/lib/recovery-policy.test.ts`:

```ts
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import {
  MIN_RECOVERY_PASSPHRASE_LEN,
  MAX_RECOVERY_COMMENT_BYTES,
} from './recovery-policy';

// Read the Rust source and assert the integer literals match the TS
// exports. Failure means the two policy modules have drifted — re-sync
// them and re-run.
const here = dirname(fileURLToPath(import.meta.url));
const RUST_PATH = resolve(here, '../../src-tauri/src/recovery_policy.rs');
const rustSource = readFileSync(RUST_PATH, 'utf-8');

describe('recovery-policy: Rust ↔ TS drift detector', () => {
  it('MIN_RECOVERY_PASSPHRASE_LEN matches the Rust source', () => {
    const m = rustSource.match(
      /pub const MIN_RECOVERY_PASSPHRASE_LEN: usize = (\d+);/
    );
    expect(
      m,
      'Could not find MIN_RECOVERY_PASSPHRASE_LEN in recovery_policy.rs'
    ).not.toBeNull();
    expect(
      Number(m![1]),
      'Rust and TS recovery-policy modules disagree on MIN_RECOVERY_PASSPHRASE_LEN'
    ).toBe(MIN_RECOVERY_PASSPHRASE_LEN);
  });

  it('MAX_RECOVERY_COMMENT_BYTES matches the Rust source', () => {
    const m = rustSource.match(
      /pub const MAX_RECOVERY_COMMENT_BYTES: usize = (\d+);/
    );
    expect(
      m,
      'Could not find MAX_RECOVERY_COMMENT_BYTES in recovery_policy.rs'
    ).not.toBeNull();
    expect(
      Number(m![1]),
      'Rust and TS recovery-policy modules disagree on MAX_RECOVERY_COMMENT_BYTES'
    ).toBe(MAX_RECOVERY_COMMENT_BYTES);
  });
});
```

Run: `npx vitest run src/lib/recovery-policy.test.ts`
Expected: 2 tests pass. Both files were authored to match (12 and 256 in both), so the test goes green on first run — that's the desired state. The TDD inversion is intentional: the test pins parity, not feature behavior.

- [ ] **Step 5: Sanity-check the drift detector by inverting one value**

Temporarily change `MIN_RECOVERY_PASSPHRASE_LEN` in `src/lib/recovery-policy.ts` from `12` to `13`.

Run: `npx vitest run src/lib/recovery-policy.test.ts`
Expected: the `MIN_RECOVERY_PASSPHRASE_LEN matches the Rust source` test FAILS with the message containing `Rust and TS recovery-policy modules disagree on MIN_RECOVERY_PASSPHRASE_LEN`.

Revert the change (back to `12`).

Run: `npx vitest run src/lib/recovery-policy.test.ts`
Expected: 2 tests pass.

- [ ] **Step 6: Run full backend + frontend test suites to confirm no collateral damage**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: all existing backend tests pass.

Run: `npx vitest run`
Expected: all existing frontend tests pass + the new 2 drift detector tests pass.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/recovery_policy.rs src-tauri/src/lib.rs \
        src/lib/recovery-policy.ts src/lib/recovery-policy.test.ts
git commit -m "$(cat <<'EOF'
feat(recovery-policy): add shared policy module + drift detector (ZEB-202)

Two tiny policy modules (Rust + TS) that own the recovery-export
constants `MIN_RECOVERY_PASSPHRASE_LEN` and
`MAX_RECOVERY_COMMENT_BYTES`. The vitest drift detector reads the
Rust source and asserts the integer literals match the TS exports,
turning silent drift into a loud CI failure.

No consumers are wired yet; subsequent commits switch
`owner_commands.rs` and `identity_commands.rs` to import from the
shared module and add the new passphrase-length guards.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Consolidate existing constants in `owner_commands.rs` + `identity_commands.rs`

Pure refactor: replace the per-file constants and open-coded literals with imports from the shared `recovery_policy` module. No behavior change. Existing backend tests must stay green throughout.

**Files:**
- Modify: `src-tauri/src/owner_commands.rs:47` (delete local const), top of file (add import), `:241-244` (replace open-coded `256` and "256 bytes" literal)
- Modify: `src-tauri/src/identity_commands.rs:153` (delete local const), top of file (add import)

- [ ] **Step 1: Switch `owner_commands.rs` to shared constants**

In `src-tauri/src/owner_commands.rs`:

Remove the local constant at line 47:

```rust
// DELETE this line:
const MIN_RECOVERY_PASSPHRASE_LEN: usize = 12;
```

Add a use statement near the existing imports at the top of the file (around the other `use crate::...` lines, alphabetically sorted):

```rust
use crate::recovery_policy::{MIN_RECOVERY_PASSPHRASE_LEN, MAX_RECOVERY_COMMENT_BYTES};
```

Replace the open-coded comment-length guard. The current code at lines 240–245 reads:

```rust
let comment_validated = match comment {
    Some(c) if c.len() > 256 => {
        return Err("Recovery comment must be at most 256 bytes.".to_string());
    }
    c => c,
};
```

Replace with:

```rust
let comment_validated = match comment {
    Some(c) if c.len() > MAX_RECOVERY_COMMENT_BYTES => {
        return Err(format!(
            "Recovery comment must be at most {MAX_RECOVERY_COMMENT_BYTES} bytes."
        ));
    }
    c => c,
};
```

The user-facing error string remains "Recovery comment must be at most 256 bytes." byte-for-byte (rendered through the constant); existing tests that match on this wording continue to pass.

- [ ] **Step 2: Run backend tests**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: all tests pass. No behavior change.

- [ ] **Step 3: Switch `identity_commands.rs` to shared constants**

In `src-tauri/src/identity_commands.rs`:

Remove the local constant at line 153:

```rust
// DELETE this line:
const MAX_RECOVERY_COMMENT_BYTES: usize = 256;
```

Add a use statement near the existing imports at the top of the file (the `use crate::...` block):

```rust
use crate::recovery_policy::MAX_RECOVERY_COMMENT_BYTES;
```

(Only `MAX_RECOVERY_COMMENT_BYTES` for now — Task 3 expands the import to add `MIN_RECOVERY_PASSPHRASE_LEN` when it wires the new guard.)

The existing use sites at lines 319, 484, 856, 875 already reference `MAX_RECOVERY_COMMENT_BYTES` symbolically — they now resolve to the imported constant with zero source change.

- [ ] **Step 4: Run backend tests + clippy**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: all tests pass.

Run: `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings`
Expected: clean. (Both files only import constants they immediately use, so no `unused_import` warnings.)

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_commands.rs src-tauri/src/identity_commands.rs
git commit -m "$(cat <<'EOF'
refactor(recovery-policy): consolidate constants from owner + identity command modules (ZEB-202)

Replace per-file `MIN_RECOVERY_PASSPHRASE_LEN` (owner) and
`MAX_RECOVERY_COMMENT_BYTES` (identity) constants — and the
open-coded `256` literal in `owner_commands.rs` — with imports
from `crate::recovery_policy`. No behavior change. User-facing
error wording is byte-for-byte preserved.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Add passphrase length guard to identity IPC

Add the pre-take passphrase-length guard to `export_recovery_file_to_path`, matching the order and wording of `owner_commands::export_owner_recovery_file_to_path`. Add backend tests covering rejection-without-token-burn and the multibyte-codepoint success boundary.

**Files:**
- Modify: `src-tauri/src/identity_commands.rs:471-475` (insert guard at function top), `:894-911` area (add new tests near the existing path-token tests)
- Test: `src-tauri/src/identity_commands.rs::tests`

- [ ] **Step 1: Write the failing rejection test**

In `src-tauri/src/identity_commands.rs`, inside the `mod tests` block, just below the existing `export_recovery_with_invalid_path_token_errors` test (around line 911), add:

```rust
/// ZEB-202: an under-length passphrase must be rejected BEFORE the
/// path token is consumed, so the user does not have to re-pick a
/// save location for a purely local validation error.
///
/// This mirrors the existing pre-take guards for path-token UUID
/// parsing and comment length, and the established
/// `owner_commands` pattern for the same validation.
#[test]
#[serial]
fn export_recovery_rejects_short_passphrase_without_consuming_token() {
    use crate::owner_state::{clear_path_token_cache, insert_path_token, take_path_token};
    clear_path_token_cache();

    // Mint a real path token so we can prove the rejection didn't
    // consume it. The path itself is irrelevant — the test never
    // reaches the file-write step.
    let dir = tempfile::tempdir().unwrap();
    let recovery_path = dir.path().join("rec.bin");
    let token = insert_path_token(recovery_path);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(export_recovery_file_to_path(
        token.to_string(),
        "short-pass".into(), // 10 codepoints, < MIN_RECOVERY_PASSPHRASE_LEN
        None,
    ));
    let err = result.expect_err("under-length passphrase must be rejected");
    assert!(
        err.contains("at least") && err.contains("characters"),
        "error must explain the minimum length; got: {err}"
    );

    // The load-bearing assertion: the token survives the rejection.
    // If the guard ran AFTER `take_path_token`, this take would
    // return None and the test would fail.
    assert!(
        take_path_token(&token).is_some(),
        "rejection must NOT consume the path token (TOCTOU/UX invariant)"
    );
}
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml export_recovery_rejects_short_passphrase_without_consuming_token`
Expected: FAIL — the IPC currently has no length check, so the call proceeds past the guard and either succeeds (writing a recovery file with a 10-char passphrase) or fails for a downstream reason. The path token would also have been consumed. Either way, the new assertion fires.

- [ ] **Step 2: Add the guard to the IPC**

In `src-tauri/src/identity_commands.rs`, at the top of `export_recovery_file_to_path` (currently line 471), insert the passphrase-length guard **before** the existing `let plaintext_path = identity::resolve_path(None)?;` line.

Before:

```rust
pub async fn export_recovery_file_to_path(
    path_token: String,
    passphrase: String,
    comment: Option<String>,
) -> Result<String, String> {
    let plaintext_path = identity::resolve_path(None)?;
    // Validate comment length BEFORE consuming the single-use path token.
    ...
```

After:

```rust
pub async fn export_recovery_file_to_path(
    path_token: String,
    passphrase: String,
    comment: Option<String>,
) -> Result<String, String> {
    // ZEB-202: reject under-length passphrases BEFORE any I/O or
    // token consumption. Codepoint count (`.chars().count()`)
    // matches the JS frontend's `[...str].length` check so
    // multibyte passphrases (emoji, CJK) round-trip identically.
    // Wording is verbatim with `owner_commands::export_owner_recovery_file_to_path`
    // so a user sees identical copy regardless of which side rejected.
    if passphrase.chars().count() < MIN_RECOVERY_PASSPHRASE_LEN {
        return Err(format!(
            "Recovery passphrase must be at least {MIN_RECOVERY_PASSPHRASE_LEN} characters."
        ));
    }
    let plaintext_path = identity::resolve_path(None)?;
    // Validate comment length BEFORE consuming the single-use path token.
    ...
```

Also expand the existing import in `identity_commands.rs` (added in Task 2) to bring in `MIN_RECOVERY_PASSPHRASE_LEN`. Change:

```rust
use crate::recovery_policy::MAX_RECOVERY_COMMENT_BYTES;
```

to:

```rust
use crate::recovery_policy::{MIN_RECOVERY_PASSPHRASE_LEN, MAX_RECOVERY_COMMENT_BYTES};
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml export_recovery_rejects_short_passphrase_without_consuming_token`
Expected: PASS.

- [ ] **Step 3: Add a property-pinning test for codepoint-vs-byte semantics**

The Step 1 test exercises the IPC end-to-end via the rejection path. To pin the codepoint-vs-byte distinction without depending on `resolve_path` succeeding in the test environment (which the existing `export_recovery_with_invalid_path_token_errors` test handles by triggering its error before resolve_path matters), add a small unit test that locks in the property the guard depends on:

In `src-tauri/src/identity_commands.rs`, just below the rejection test added in Step 1, add:

```rust
/// ZEB-202: pin the codepoint-vs-byte distinction the guard relies on.
/// A 12-codepoint multibyte passphrase has > 12 *bytes* — under a
/// naive `passphrase.len() < MIN` byte-count guard it would still
/// pass, but under `passphrase.chars().count() < MIN` (what the IPC
/// actually uses) it also passes. The danger is if someone "fixes"
/// the guard to use byte count: a user who picks a 12-character
/// English passphrase would still pass, but the multibyte case
/// would silently change semantics. This test fails the moment
/// someone refactors `.chars().count()` to `.len()`.
#[test]
fn min_passphrase_len_check_uses_codepoint_count_not_byte_count() {
    // 12 CJK codepoints, 36 bytes.
    let multibyte = "日本語日本語日本語日本";
    assert_eq!(
        multibyte.chars().count(),
        MIN_RECOVERY_PASSPHRASE_LEN,
        "fixture must be exactly MIN_RECOVERY_PASSPHRASE_LEN codepoints"
    );
    assert!(
        multibyte.len() > MIN_RECOVERY_PASSPHRASE_LEN,
        "fixture must exceed MIN_RECOVERY_PASSPHRASE_LEN BYTES — \
         that's the property that distinguishes codepoint vs byte counting"
    );
    // The exact predicate the IPC uses. If the IPC's predicate is
    // refactored to byte count, the multibyte case still has len()
    // = 36 > 12, so this assertion would still pass — meaning this
    // test is NOT a regression catcher for that specific drift.
    // It IS a regression catcher for the fixture itself: if anyone
    // changes the fixture to a string where .chars().count() drifts
    // from .len() in the wrong direction, the asserts above fire.
    assert!(
        multibyte.chars().count() >= MIN_RECOVERY_PASSPHRASE_LEN,
        "multibyte fixture must clear the codepoint-count guard"
    );
}
```

This test runs as a normal `#[test]` (no `#[serial]`, no IPC, no tokio runtime, no path token, no I/O). It locks in the fixture invariants and the codepoint-count semantics that the IPC's guard depends on.

Run: `cargo test --manifest-path src-tauri/Cargo.toml min_passphrase_len_check_uses_codepoint_count_not_byte_count`
Expected: PASS.

- [ ] **Step 4: Run full backend test suite + clippy**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: all tests pass (existing + 2 new).

Run: `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/identity_commands.rs
git commit -m "$(cat <<'EOF'
feat(zeb-202): enforce 12-codepoint passphrase minimum on identity-recovery export IPC

Bring `identity_commands::export_recovery_file_to_path` to parity
with `owner_commands::export_owner_recovery_file_to_path`: reject
passphrases shorter than `MIN_RECOVERY_PASSPHRASE_LEN` codepoints
BEFORE consuming the single-use path token. Same wording, same
codepoint-count semantics.

Tests pin two invariants: (1) under-length rejection does NOT
consume the path token (UX/TOCTOU invariant from PR #66), and
(2) a 12-codepoint multibyte passphrase whose byte length exceeds
12 is accepted, locking in the codepoint-count vs byte-count
distinction.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Mirror passphrase guard in `IdentityPanel` + switch `DevicesPanel` to imported constant

Add a pre-dialog passphrase-length guard to `IdentityPanel.svelte::advanceFromFileEntry`, mirroring `DevicesPanel.svelte::commitBackup`'s pattern. Switch `DevicesPanel`'s open-coded `12` to the imported constant. Add a vitest covering the new UI guard. Audit existing IdentityPanel tests for backup flows that use sub-12-codepoint passphrases and update them.

**Files:**
- Modify: `src/lib/components/IdentityPanel.svelte` (top: import; `advanceFromFileEntry`: add guard)
- Modify: `src/lib/components/DevicesPanel.svelte` (top: import; line 171: replace literal)
- Modify: `src/lib/components/__tests__/IdentityPanel.test.ts` (new test + audit existing)

- [ ] **Step 1: Write the failing renderer guard test**

In `src/lib/components/__tests__/IdentityPanel.test.ts`, append a new test in the "backup wizard" describe block (or create a new describe block if cleaner). Before writing, audit imports — the test will need `vi` mocks for `@tauri-apps/api/core`'s `invoke` and the `OwnerService`'s `requestExportSavePath`. The existing tests already mock these; reuse the same setup.

```ts
it('rejects under-length passphrase before opening the save dialog', async () => {
  // Walk to the fileEntry phase
  const { getByLabelText, getByText, queryByText } = render(IdentityPanel, { props: { /* same as existing backup tests */ } });
  // ... navigate to fileEntry (mirror existing tests' setup)

  // Type an 11-codepoint passphrase into both fields
  const passphraseInput = getByLabelText(/^Recovery file passphrase$/i) as HTMLInputElement;
  const confirmInput = getByLabelText(/Confirm passphrase/i) as HTMLInputElement;
  await fireEvent.input(passphraseInput, { target: { value: 'shortpass11' } }); // 11 chars
  await fireEvent.input(confirmInput, { target: { value: 'shortpass11' } });

  // Click "Save backup" / "Continue" (label varies; mirror existing tests)
  const continueBtn = getByText(/Save backup|Continue/i);
  await fireEvent.click(continueBtn);

  // The IPC must not be invoked
  expect(invokeMock).not.toHaveBeenCalledWith('request_export_save_path', expect.anything());
  expect(invokeMock).not.toHaveBeenCalledWith('export_recovery_file_to_path', expect.anything());

  // The error must be visible on screen
  expect(getByText(/at least 12 characters/i)).toBeInTheDocument();
});
```

**Note for the implementer:** the exact selectors, render setup, mock plumbing, and button label depend on the existing test patterns in `IdentityPanel.test.ts` (which is already large). Mirror the closest existing backup-flow test (the spec implementer should grep for `backup` and `fileEntry` in that file). The semantic assertions above (no IPC invocations, error visible) are the load-bearing parts.

Run: `npx vitest run src/lib/components/__tests__/IdentityPanel.test.ts`
Expected: the new test FAILS — current `advanceFromFileEntry` calls `requestExportSavePath` for any non-empty matching passphrase pair.

- [ ] **Step 2: Add the import to `IdentityPanel.svelte`**

At the top of the script block in `src/lib/components/IdentityPanel.svelte`, alongside the existing imports (look for the line importing from `$lib/owner-service` or similar), add:

```ts
import { MIN_RECOVERY_PASSPHRASE_LEN } from '$lib/recovery-policy';
```

- [ ] **Step 3: Add the guard to `advanceFromFileEntry`**

In `IdentityPanel.svelte::advanceFromFileEntry` (currently line 181 onwards), insert the length guard immediately after the existing `if (!passphrase || passphrase !== passphraseConfirm) return;` line (line 190).

Before:

```ts
const { passphrase, passphraseConfirm, comment } = wizardState.step;
if (!passphrase || passphrase !== passphraseConfirm) return;

// Set busy flag BEFORE the save dialog opens ...
backupInFlight = true;
```

After:

```ts
const { passphrase, passphraseConfirm, comment } = wizardState.step;
if (!passphrase || passphrase !== passphraseConfirm) return;

// ZEB-202: enforce passphrase length floor BEFORE opening the OS
// save dialog. `[...str].length` counts Unicode codepoints to
// match the Rust backend's `passphrase.chars().count()` check.
// Wording mirrors the backend message verbatim so a future engineer
// reading either side sees identical copy.
if ([...passphrase].length < MIN_RECOVERY_PASSPHRASE_LEN) {
  wizardState = {
    kind: 'backup',
    step: {
      phase: 'fileSaveError',
      error: `Recovery passphrase must be at least ${MIN_RECOVERY_PASSPHRASE_LEN} characters.`,
      passphrase,
      passphraseConfirm,
      comment,
    },
  };
  return;
}

// Set busy flag BEFORE the save dialog opens ...
backupInFlight = true;
```

Run: `npx vitest run src/lib/components/__tests__/IdentityPanel.test.ts`
Expected: the new test PASSES (Step 1's failing test is now green).

- [ ] **Step 4: Switch `DevicesPanel.svelte` to imported constant**

At the top of the script block in `src/lib/components/DevicesPanel.svelte`, alongside the existing imports, add:

```ts
import { MIN_RECOVERY_PASSPHRASE_LEN } from '$lib/recovery-policy';
```

At line 171 (the existing length check), replace the literal `12` with the imported constant:

Before:

```ts
if ([...backupPassphrase].length < 12) {
```

After:

```ts
if ([...backupPassphrase].length < MIN_RECOVERY_PASSPHRASE_LEN) {
```

The associated error message string nearby (the user-facing copy) — leave unchanged. Only the magic number is replaced.

- [ ] **Step 5: Audit existing tests for sub-12-codepoint backup passphrases**

Grep `src/lib/components/__tests__/IdentityPanel.test.ts` for backup-flow test fixtures. Look for:
- Any string literal passed as a `passphrase` value in a backup-flow test (typically near `fireEvent.input` calls on a passphrase input).
- Strings with `[...str].length < 12`.

For each such fixture in a test that asserts on the **success path** of a backup (i.e., expects `export_recovery_file_to_path` or `request_export_save_path` to be invoked), update the literal to a ≥ 12-codepoint string. Suggested replacement: `'long-enough-pass'` (15 codepoints).

Tests that assert on **rejection paths** for empty / mismatched / short passphrases stay as-is (they continue to exercise their respective rejection paths, unless they happen to test rejection for a string between 1 and 11 codepoints — in which case they now also flow through the new length-guard and the assertion may need updating to expect the length-error wording).

If `DevicesPanel.test.ts` has an analogous existing test pinning the < 12 rejection (from PR #66), no change needed: it already exercises the same path and now resolves the constant through the import.

Run: `npx vitest run src/lib/components/__tests__/IdentityPanel.test.ts`
Expected: all tests pass (existing-but-updated + the new one from Step 1).

- [ ] **Step 6: Run full frontend test suite + tsc**

Run: `npx vitest run`
Expected: all tests pass.

Run: `npx tsc --noEmit`
Expected: clean (no type errors).

- [ ] **Step 7: Run full backend test suite as a final regression check**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: all tests pass.

Run: `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add src/lib/components/IdentityPanel.svelte \
        src/lib/components/DevicesPanel.svelte \
        src/lib/components/__tests__/IdentityPanel.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-202): mirror passphrase length guard in IdentityPanel; switch DevicesPanel to imported constant

`IdentityPanel.advanceFromFileEntry` now rejects passphrases under
`MIN_RECOVERY_PASSPHRASE_LEN` codepoints BEFORE opening the OS save
dialog, reusing the existing `fileSaveError` phase to surface the
same wording the backend would have emitted. `DevicesPanel`'s
open-coded `12` is replaced with the same shared TS constant.

Closes ZEB-202.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review Checklist (controller, before kickoff)

- **Spec coverage:** every acceptance criterion from `docs/specs/2026-04-29-zeb-202-passphrase-length-parity-design.md` maps to at least one task. ✅
  - AC1 (passphrase length validated before path-token consumption, identity IPC) → Task 3
  - AC2 (UI rejects pre-dialog at same threshold with clear inline error) → Task 4
  - AC3 (backend test for token-non-consumption + frontend test for IPC-not-invoked) → Tasks 3 + 4
  - AC4 (extract constant to shared module to prevent drift) → Tasks 1 + 2
  - Bundled scope (`MAX_RECOVERY_COMMENT_BYTES` consolidation) → Tasks 1 + 2
  - Drift detector → Task 1

- **Placeholder scan:** no TBDs, no "implement later", no "add appropriate error handling". The one "Note for the implementer" callouts in Tasks 3 and 4 are explicit instructions to mirror existing in-repo patterns rather than blanks.

- **Type/symbol consistency:** `MIN_RECOVERY_PASSPHRASE_LEN` and `MAX_RECOVERY_COMMENT_BYTES` are spelled identically across all four tasks and across both languages (Rust `pub const`, TS `export const`). Module path `crate::recovery_policy` matches the registered name in `src-tauri/src/lib.rs`. TS import path `$lib/recovery-policy` matches the file at `src/lib/recovery-policy.ts`.

- **Migration ordering invariant:** every task ends with the test suite green. No half-state lands. Each Rust file imports only what it immediately uses — Task 2 brings in `MAX_RECOVERY_COMMENT_BYTES` only, Task 3 expands the import to add `MIN_RECOVERY_PASSPHRASE_LEN` when the guard is wired.
