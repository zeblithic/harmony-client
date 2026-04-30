# ZEB-202: Identity-Recovery Export Passphrase Length Parity

**Linear:** [ZEB-202](https://linear.app/zeblith/issue/ZEB-202/harmony-client-enforce-passphrase-length-minimum-on-identity-recovery)

**Status:** Design

**Date:** 2026-04-29

## Problem

Two recovery-file export IPCs share the same UI metaphor (Settings → Identity wizard, with passphrase + comment + dialog-picked path) but enforce **asymmetric** passphrase policy:

- **Owner-recovery export** (`owner_commands::export_owner_recovery_file_to_path`) requires the recovery passphrase to be ≥ 12 codepoints. Mirrored in `DevicesPanel.svelte::commitBackup` with `[...backupPassphrase].length < 12`.
- **Identity-recovery export** (`identity_commands::export_recovery_file_to_path`) accepts any non-empty passphrase. `IdentityPanel.svelte::advanceFromFileEntry` only checks non-empty + match.

A user backing up their per-device transport identity can therefore use a 1-character passphrase, while the same UI applied to the owner identity rejects anything shorter than 12. This is asymmetric security policy on two backups produced by the same wizard, and it surfaced from PR #66 review (Cursor Bugbot, 2026-04-29).

A secondary drift hazard sits in the same area: `MAX_RECOVERY_COMMENT_BYTES = 256` lives in `identity_commands.rs` only; `owner_commands.rs` open-codes the same `256` literal in two places (the guard and the error string). Both export commands consume the same underlying field with the same hard cap from `harmony-owner`, but the constants live in two files and one file's constant shadows the other's literal.

## Goals

1. Identity-recovery export rejects passphrases below the same 12-codepoint floor as owner-recovery export, **before** consuming the single-use path token.
2. The 12-codepoint floor lives in exactly one Rust source location and one TypeScript source location, with a runtime drift detector that fails CI if the two diverge.
3. The 256-byte comment cap collapses to the same shared module (the drift hazard is identical and the consolidation is trivially in-scope while we are touching this surface).
4. UI mirrors the backend guard: `IdentityPanel`'s "next" button rejects too-short passphrases inline, **before** the OS save dialog opens, matching `DevicesPanel`'s pattern.

## Non-Goals

- Re-tuning the 12-codepoint floor itself (separate policy decision).
- Touching the headless `harmony-owner::recovery_cli` API. Validation lives in the Tauri IPC layer because that is where the policy is enforced for GUI consumers; the headless CLI has its own policy-enforcement story (passphrase prompts via stdin) and is out of scope here.
- Adding a runtime IPC for the renderer to fetch policy. Hardcoded TS constants + a static drift detector are simpler and sufficient.
- Cross-language constant generation (build script, codegen). Two manually-maintained source-of-truth files with a drift test is cheaper and good enough for two integers.

## Architecture

Two new tiny policy modules — one Rust, one TypeScript — own the recovery-export policy constants. Every consumer (two backend IPCs, two UI panels) imports from the shared module instead of carrying a local literal. A vitest reads the Rust source file at test time and asserts the integer literals match the TS exports, catching drift in CI.

```
                    recovery_policy.rs                  recovery-policy.ts
                    ┌─────────────────────┐             ┌──────────────────────┐
                    │ MIN_PASSPHRASE = 12 │ ←── drift ──→ │ MIN_PASSPHRASE = 12  │
                    │ MAX_COMMENT  = 256  │      test    │ MAX_COMMENT  = 256   │
                    └──────────┬──────────┘             └──────────┬───────────┘
                               │                                   │
              ┌────────────────┼────────────────┐    ┌─────────────┴──────────┐
              ▼                ▼                ▼    ▼                        ▼
   owner_commands.rs   identity_commands.rs    DevicesPanel.svelte    IdentityPanel.svelte
   (passphrase + cmt)  (+ NEW passphrase grd)  (already had 12)       (NEW pre-dialog grd)
```

## File Structure

### New files

#### `src-tauri/src/recovery_policy.rs` (~10 lines)

```rust
//! Recovery-file export policy constants shared between the
//! owner-recovery export IPC (owner_commands) and the
//! identity-recovery export IPC (identity_commands).
//!
//! Mirrored in src/lib/recovery-policy.ts. The drift detector
//! `src/lib/recovery-policy.test.ts` asserts both files agree on
//! the integer literals; failing that test in CI is the signal
//! to re-sync.

pub const MIN_RECOVERY_PASSPHRASE_LEN: usize = 12;
pub const MAX_RECOVERY_COMMENT_BYTES: usize = 256;
```

#### `src/lib/recovery-policy.ts` (~10 lines)

```ts
// Recovery-file export policy. Mirrored from
// src-tauri/src/recovery_policy.rs. The values MUST match;
// `recovery-policy.test.ts` reads the Rust file and asserts
// equality so drift fails CI.

export const MIN_RECOVERY_PASSPHRASE_LEN = 12;
export const MAX_RECOVERY_COMMENT_BYTES = 256;
```

### Modified files

#### `src-tauri/src/lib.rs`

Add `mod recovery_policy;` alongside the existing module declarations. No public re-export needed — consumers `use crate::recovery_policy::...` directly.

#### `src-tauri/src/owner_commands.rs`

- Line 47: delete `const MIN_RECOVERY_PASSPHRASE_LEN: usize = 12;`. Add `use crate::recovery_policy::{MIN_RECOVERY_PASSPHRASE_LEN, MAX_RECOVERY_COMMENT_BYTES};` at the top of the file.
- Line 232: existing `passphrase.chars().count() < MIN_RECOVERY_PASSPHRASE_LEN` guard — no behavior change, just resolves to the imported constant.
- Lines 240–245 (the comment cap guard): replace the open-coded `256` and `"Recovery comment must be at most 256 bytes."` literals with the imported `MAX_RECOVERY_COMMENT_BYTES` constant and the same error wording rendered via `format!`. Pattern:

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

  The user-facing string remains "Recovery comment must be at most 256 bytes." byte-for-byte, just rendered through the constant.

#### `src-tauri/src/identity_commands.rs`

- Line 153: delete `const MAX_RECOVERY_COMMENT_BYTES: usize = 256;`. Add `use crate::recovery_policy::{MIN_RECOVERY_PASSPHRASE_LEN, MAX_RECOVERY_COMMENT_BYTES};` at the top of the file.
- Line 471 (`export_recovery_file_to_path`): insert passphrase-length guard at the **very top** of the function body, ahead of the existing comment-length guard. Wording verbatim with owner IPC:

  ```rust
  if passphrase.chars().count() < MIN_RECOVERY_PASSPHRASE_LEN {
      return Err(format!(
          "Recovery passphrase must be at least {MIN_RECOVERY_PASSPHRASE_LEN} characters."
      ));
  }
  ```

  Codepoint count (`.chars().count()`) — not byte count — so multibyte passphrases (emoji, CJK) round-trip identically with the JS frontend's `[...str].length` check, mirroring the comment in `owner_commands.rs:228-231`.

  The check goes before path-token consumption (which still happens inside `run_blocking` via `take_path_token`), preserving the "validation never burns the token" invariant established in PR #66.
- Other internal use sites of `MAX_RECOVERY_COMMENT_BYTES` (line 319, line 484, the test at 856/875) — no behavior change, just resolve to the imported constant.

#### `src/lib/components/DevicesPanel.svelte`

- Line 171: replace `[...backupPassphrase].length < 12` with `[...backupPassphrase].length < MIN_RECOVERY_PASSPHRASE_LEN`. Add `import { MIN_RECOVERY_PASSPHRASE_LEN } from '$lib/recovery-policy';` at the top of the script block.
- Error message string: keep the existing copy verbatim — only the magic number is replaced.

#### `src/lib/components/IdentityPanel.svelte`

- Add `import { MIN_RECOVERY_PASSPHRASE_LEN } from '$lib/recovery-policy';` at the top of the script block.
- `advanceFromFileEntry` (line 189-onwards): insert a passphrase-length guard immediately after the existing non-empty + match check, **before** the call to `svc.requestExportSavePath`. On rejection, transition the wizard to `fileSaveError` phase carrying the prior input fields so Back restores the form, with the inline message:

  > `Recovery passphrase must be at least 12 characters.`

  (Same wording the backend would emit; eyeballs of a future engineer reading either message see the same string.)

  ```ts
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
  ```

  This reuses the existing `fileSaveError` phase, which already has a "Back" affordance that restores the form. No new wizard phase needed.

## Data Flow

The validation cascade for `export_recovery_file_to_path`:

```
renderer (IdentityPanel.advanceFromFileEntry)
  ├─ passphrase non-empty + match?            → no: stay on fileEntry
  ├─ passphrase ≥ 12 codepoints?              → no: → fileSaveError
  ├─ request_export_save_path() → path token  → null (cancelled): stay
  └─ export_recovery_file_to_path(path_token, passphrase, comment)
        │
        ▼
     backend (identity_commands::export_recovery_file_to_path)
       ├─ passphrase ≥ MIN_RECOVERY_PASSPHRASE_LEN?    → no: Err, token untouched
       ├─ comment ≤ MAX_RECOVERY_COMMENT_BYTES?        → no: Err, token untouched
       ├─ parse path_token UUID                        → no: Err, token untouched
       ├─ run_blocking { take_path_token(uuid) }       → consume! (success path only)
       └─ write encrypted file → return saved path
```

Owner IPC follows the identical cascade (already implemented).

## Error Handling

- Backend rejection messages are word-for-word identical between owner and identity IPCs. A user who has seen one error message recognises the other.
- The renderer surfaces backend errors verbatim in the existing `fileSaveError` phase. There is no client-side translation that could drift from the backend wording — the user sees whatever the IPC returned.
- Renderer pre-checks emit identical wording to what the backend would have returned, so the experience does not betray which side did the rejection.
- Path-token semantics unchanged: every renderer-side rejection path before `requestExportSavePath` means no token was minted; every backend-side rejection before `take_path_token` means the token sits unconsumed in the cache and is reusable on retry. PR #66 already established this invariant; this change extends it without altering the contract.

## Testing

### Backend tests (`src-tauri/src/identity_commands.rs::tests`)

- **`export_recovery_file_to_path_rejects_short_passphrase`**
  - Set up a path token via `cache_path_token`, capturing the UUID.
  - Call `export_recovery_file_to_path` with an 11-codepoint passphrase.
  - Assert `Err` with message containing `"at least 12 characters"`.
  - Assert the path token is still in the cache (e.g., `take_path_token(uuid)` succeeds), proving the rejection happened before token consumption.

- **`export_recovery_file_to_path_accepts_min_length_passphrase`** (multibyte case)
  - Set up a path token.
  - Call with a 12-codepoint passphrase that is also > 12 *bytes* (e.g., `"日本語日本語日本語日本"` — 12 codepoints, 36 bytes).
  - Assert `Ok` (the codepoint check passes; no spurious byte-length false reject).

- Existing comment-length tests at lines ~843–880: keep their assertions, just confirm they continue to compile against the imported constant.

### Frontend tests (`src/lib/components/__tests__/IdentityPanel.test.ts`)

- **`backup wizard rejects short passphrase before opening save dialog`**
  - Render the panel and walk to `fileEntry`.
  - Type an 11-codepoint passphrase into both passphrase fields.
  - Click "Save backup".
  - Assert: `request_export_save_path` was never invoked; `export_recovery_file_to_path` was never invoked; the displayed error reads `"Recovery passphrase must be at least 12 characters."`.

- Audit existing tests for any that use < 12-codepoint passphrases for the **backup** flow specifically. Update those to ≥ 12 codepoints (likely 1–3 line changes per test) so they continue to exercise the success path.

### Frontend test for `DevicesPanel`

- The existing `DevicesPanel.test.ts` short-passphrase test (already present from PR #66) keeps its semantic, just resolves the literal `12` through the imported constant. No new test needed.

### Drift detector (`src/lib/recovery-policy.test.ts`)

- Read `src-tauri/src/recovery_policy.rs` from disk via `node:fs`.
- Regex-extract the integer literal from `pub const MIN_RECOVERY_PASSPHRASE_LEN: usize = (\d+);` and `pub const MAX_RECOVERY_COMMENT_BYTES: usize = (\d+);`.
- Assert each parsed integer equals the corresponding TS export. Failure message points at "the Rust and TS recovery policy modules disagree — re-sync them and re-run".

This detector is the load-bearing part: it converts the residual drift risk between two manually-maintained constant files into a CI failure rather than a deferred bug.

## Migration Order

Single PR, four commits each landing a tested green state:

1. **`feat(recovery-policy): add shared policy module + drift detector`** — create `recovery_policy.rs`, `recovery-policy.ts`, register the Rust module in `lib.rs`, add the drift-detection vitest. No consumers wired yet.
2. **`refactor(recovery-policy): consolidate constants from owner_commands + identity_commands`** — switch all three existing use sites to the shared imports. No behavior change. `cargo test` and `vitest` both stay green.
3. **`feat(zeb-202): enforce passphrase length on identity-recovery export IPC`** — add the pre-take guard to `export_recovery_file_to_path`, add the two backend tests covering rejection-without-burn and multibyte success.
4. **`feat(zeb-202): mirror passphrase length guard in IdentityPanel`** — add the renderer-side guard, switch `DevicesPanel`'s literal to the imported constant, add the new IdentityPanel test, fix any existing tests that used short passphrases.

## Open Questions

None. The ticket's acceptance criteria fully constrain the design; this spec just makes the file-level decisions concrete.

## References

- [ZEB-194 PR #66](https://github.com/zeblithic/harmony-client/pull/66) — the ticket that surfaced this asymmetry; commit `b3c8513` introduced the comment-length guard pattern that this work mirrors for passphrase length.
- `src-tauri/src/owner_commands.rs:46-47` and `:227-235` — the existing owner-side guard this work brings to parity.
- `src-tauri/src/identity_commands.rs:470-494` — the IPC that gains the new guard.
- `src/lib/components/IdentityPanel.svelte::advanceFromFileEntry` — the UI mirror.
- `src/lib/components/DevicesPanel.svelte::commitBackup` lines 159-171 — the existing UI pattern this work mirrors for `IdentityPanel`.
