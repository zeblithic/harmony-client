# ZEB-184 — Identity Backup/Restore GUI Wizard — Design

**Status:** Approved 2026-04-28. Implementation pending.

## Goal

Wire the recovery primitives shipped in [ZEB-176](https://linear.app/zeblith/issue/ZEB-176) (PR #59) — `export-mnemonic`, `export-recovery-file`, `restore-mnemonic`, `restore-recovery-file` — into a Tauri/Svelte GUI flow under Settings → Identity. Mirrors how ZEB-176 turned the CLI into actual user value; this ticket does the same conversion for the desktop app.

## Why this is separate from ZEB-176

ZEB-176 deliberately scoped CLI-only because the surface area was small enough to land cleanly in one PR and headless installs (server, Docker, CI) need recovery flows too. The GUI flow was carved out for parallel design once the underlying machinery was proven end-to-end. With the CLI working in production, the GUI half is a thin Svelte shell over already-tested logic.

## Scope

A multi-step wizard inside the existing Settings panel, with **two top-level entry points** under a new **Identity** section:

1. **"Backup..."** — wizard internally branches between mnemonic and encrypted recovery file
2. **"Restore..."** — wizard internally branches between mnemonic textarea and recovery file picker

Each path round-trips through new Tauri commands that wrap the existing `recovery_cli::*_with_keychain` functions.

### Out of scope (separate sibling tickets)

- **First-launch onboarding flow.** This wizard is Settings-only — the user must already have an identity. Restoring on a fresh install continues to be done via the CLI on first launch (`harmony-app restore-mnemonic` / `harmony-app restore-recovery-file`). Per ZEB-176's `docs/headless-install.md`.
- Cloud backup destinations (Dropbox, iCloud, S3-style integrations).
- Automated periodic backup.
- Continuity-claiming (mint a fresh identity that says it's the successor of a lost one) — wider [ZEB-173](https://linear.app/zeblith/issue/ZEB-173) territory.
- Cross-device sync of the recovery artifact via the multi-device-binding CRDT (also ZEB-173 work).
- Tauri-webview Playwright e2e coverage (the ZEB-150 suite is local-only; a wizard-specific spec there would be a follow-up).

### Folded scope

[ZEB-180](https://linear.app/zeblith/issue/ZEB-180) (surface recovery-file metadata `mint_at` + `comment` on restore output) is **folded into this ticket**. The restore-recovery-file flow naturally surfaces the metadata one screen away from the work this wizard is already doing; shipping it together is cheaper than a separate follow-up.

## Architecture

### New components

- `src/lib/components/IdentityPanel.svelte` — the Settings entry point. Sibling of `NotificationSettingsPanel.svelte` and `ProfileEditor.svelte`, rendered when `App.svelte`'s `showSettings` is true. Hosts the wizard internally via local state machines (no separate wizard component — each flow is short enough to live inline).

### New Rust variants in `src-tauri/src/recovery_cli.rs`

- `export_mnemonic_words_with_keychain(identity_path, keychain) -> Result<Vec<String>, String>` — returns the 24 words. The existing `export_mnemonic_cli` and `export_mnemonic_to_writers` refactor to delegate to this.
- `restore_mnemonic_from_words_with_keychain(identity_path, words: &[String], force, keychain) -> Result<(), String>` — takes the words array directly. The existing `restore_mnemonic_with_keychain` (which reads from a file path) refactors to read the file then delegate to this.

These avoid leaking the mnemonic to a temp file in the GUI path.

### New Tauri commands in `src-tauri/src/lib.rs`

**Read-only (no side effects):**

```rust
#[tauri::command]
fn current_identity_hash() -> Result<String, String>;

#[tauri::command]
fn export_mnemonic_words() -> Result<Vec<String>, String>;

#[tauri::command]
fn preview_mnemonic_identity(words: Vec<String>) -> Result<String, String>;

#[tauri::command]
fn preview_recovery_file(in_path: PathBuf, passphrase: String) -> Result<RestoreInfo, String>;
```

**Mutating (called only after the GUI's confirmation step passes):**

```rust
#[tauri::command]
fn export_recovery_file_to_path(
    out_path: PathBuf,
    passphrase: String,
    comment: Option<String>,
) -> Result<(), String>;

#[tauri::command]
fn restore_mnemonic_from_words(words: Vec<String>) -> Result<String, String>;
// returns post-restore identity_hash; force=true on this path (pre-confirmed by GUI)

#[tauri::command]
fn restore_recovery_file_from_path(
    in_path: PathBuf,
    passphrase: String,
) -> Result<RestoreInfo, String>;
// also force=true
```

**Shared response type:**

```rust
#[derive(serde::Serialize)]
struct RestoreInfo {
    identity_hash: String,    // hex, 64 chars
    minted_at: String,         // RFC 3339
    comment: Option<String>,
}
```

Passphrases arrive as plain strings via Tauri IPC. The Rust side wraps them in `secrecy::SecretString` immediately for `Zeroize` semantics. Same exposure surface as any browser password field — acceptable for this feature.

### Reused

- `TypeToConfirmDialog.svelte` — for the restore overwrite confirmation step
- `ConfirmDialog.svelte` — for any non-typed confirmations (e.g., abandon-wizard prompt)
- All existing `recovery_cli::*_with_keychain` functions — no behavior changes beyond the two new word-array variants

## Two-phase restore: preview, then commit

The restore flows decrypt twice: once for *preview* (read-only, returns `RestoreInfo` for display), once for *commit* (writes to disk). Cost is one extra ~101-byte AEAD decryption per restore — negligible. Benefits:

- The user sees the current-vs-restored identity hash diff *before* the `TypeToConfirmDialog` step, so the dialog has accurate data
- A failed decrypt during preview surfaces inline at the passphrase entry step, not during the commit step where rollback is more annoying
- Clean separation between "show me what's in this artifact" and "actually apply it"

## Wizard flows

### Default `IdentityPanel` state (Settings → Identity)

```
Settings → Identity
─────────────────────────────────────
Identity hash:  0xa1b2c3d4…   ← click to copy full 64-char hex

[ Backup… ]  [ Restore… ]

Back up your identity to a 24-word phrase or an encrypted file.
Restore replaces your current identity — the current one becomes
unrecoverable.
```

### Backup wizard

**Step 1 — choose backup type**

Radio: "24-word recovery phrase" | "Encrypted recovery file" → Continue.

**Step 2a (mnemonic) — reveal**

```
Header: "Backing up identity 0xa1b2c3d4…"

Body:   "Write these 24 words down. Anyone with them can recover
         your identity. There is no way to retrieve them later."

[ blurred 4×6 grid of 24 numbered words ]
            [ Reveal ]   ← unblurs grid on click

After reveal:
☐ I've stored this safely
                                                    [ Done ]
                                                    (disabled until ☑)
```

No clipboard support on the mnemonic. No auto-hide timer.

**Step 2b (file) — passphrase + comment**

```
Header: "Backing up identity 0xa1b2c3d4…"

Passphrase:           [ ************ ] [👁]
Confirm passphrase:   [ ************ ] [👁]
Comment (optional):   [ laptop-2026-04-15      ]

                                            [ Cancel ] [ Continue ]
```

Confirm-passphrase is required (typing twice catches typos that would render the file undecryptable). No strength meter, no generator. Show/hide toggle defaults to hidden. No clipboard support.

**Step 3b — save dialog & success**

Tauri save dialog (`tauri-plugin-dialog`). On save:

```
✓ Wrote N bytes to /Users/.../identity.recovery
                                                    [ Done ]
```

### Restore wizard

**Step 1 — choose source**

Radio: "I have a 24-word recovery phrase" | "I have a recovery file" → Continue.

**Step 2a (mnemonic) — paste**

```
Header: "Restore identity from recovery phrase"

[ Multi-line textarea, monospace, 24-word capacity         ]
[ Paste your 24 words here. Spaces or newlines OK.          ]

Live validation:
  - Word count: 24 / 24 ✓
  - All words in BIP39 wordlist: ✓
  - Checksum: ✓

Restored identity hash: 0x9f8e7d6c…   ← appears once valid

                                            [ Cancel ] [ Continue ]
                                            (disabled until valid)
```

**Step 2b (file) — pick + decrypt**

```
[ Pick recovery file… ]   ← Tauri open dialog

After file picked:
File: /Users/.../identity.recovery
Passphrase:  [ ************ ] [👁]

                                            [ Decrypt ]

After decrypt:
✓ Decrypted /Users/.../identity.recovery
Restored identity hash: 0x9f8e7d6c…

Backup metadata:
  Minted:   2026-04-15T18:32:11Z
  Comment:  laptop-2026-04-15

                                            [ Cancel ] [ Continue ]
```

**Step 3 — confirm overwrite**

```
This will replace your current identity. Your current identity
will be unrecoverable after this step.

Current identity:    0xa1b2c3d4…
Restored identity:   0x9f8e7d6c…

Type the first 8 chars of your current identity hash to proceed:
   (a1b2c3d4)
[                                                              ]

                                            [ Cancel ] [ Replace identity ]
                                            (disabled until typed prefix matches)
```

The typed-prefix input is rendered **inline as part of step 3**, not as a separate modal — consistent with the rest of the wizard's in-place transformation (Q3=A). The wizard reuses `TypeToConfirmDialog`'s validation logic by either: (a) extracting the comparison helper from `TypeToConfirmDialog.svelte` into a small standalone util that both components consume, or (b) embedding the dialog's body without its modal chrome. Implementation can pick whichever has lower drift; either way, no second modal layer appears in the flow.

**Step 4 — done**

```
✓ Identity restored.

New identity hash: 0x9f8e7d6c…   ← click to copy full

Verify this matches what you expected. If it does not match
your backup's expected hash, restore again from the correct
backup before performing any other action.

                                                    [ Done ]
```

## Error handling

Errors render **inline** under the failing field/step, in red, with the previous step still navigable. No toasts (too transient for a serious operation), no modal interruptions (the wizard itself is the modal context).

| Flow / step | Error | Surfacing |
|---|---|---|
| Backup-mnemonic / Reveal | Identity store read failure | Full-panel error: "Could not read identity store: `<reason>`. The wizard cannot continue." (no Back) |
| Backup-file / Save dialog | User cancelled | Silent — return to passphrase entry |
| Backup-file / Save dialog | Write failed | Inline below filename: "Could not save to `<path>`: `<os_error>`. Try a different location." |
| Restore-mnemonic / textarea | Wrong word count | Inline: "Need exactly 24 words; you entered N." |
| Restore-mnemonic / textarea | Word not in BIP39 wordlist | Inline: "Word #N (`<word>`) is not a recognized recovery word." |
| Restore-mnemonic / textarea | BIP39 checksum failure | Inline: "These 24 words don't form a valid recovery phrase. Double-check your transcription." |
| Restore-file / passphrase | AEAD failure | Inline: "Could not decrypt — passphrase incorrect or file corrupted." (deliberately ambiguous) |
| Restore-file / passphrase | File missing / unreadable | Inline: "Could not read `<path>`: `<os_error>`." |
| Restore-confirm / typed prefix | Wrong prefix | Inline (within the field): "That doesn't match your current identity hash." (re-enable retry) |
| Restore-commit | Underlying recovery_cli failure | Inline at confirmation step + roll back: "Restore failed: `<reason>`. Your current identity is unchanged." |

### Principles

- **Precise > generic.** Map specific `RecoveryError` variants to specific messages. The user can usually act on a precise reason.
- **One exception:** the AEAD failure is deliberately ambiguous (passphrase vs malformed file) so we don't oracle the failure mode.
- **Roll-back-on-failure invariant.** If the commit step fails, the on-disk identity is unchanged — inherited from the existing `recovery_cli::*_with_keychain` atomic file write.
- **No retry loops.** A failed commit means "fix the input or cancel." Don't auto-retry.

## Testing

### Vitest — `IdentityPanel.svelte` component logic

- State machine transitions for both flows (idle → backup-step1 → step2a/2b → done; idle → restore-step1 → 2a/2b → 3 → 4)
- Button-disable invariants:
  - "Done" disabled until "I've stored this safely" checkbox ticked
  - "Continue" on mnemonic restore disabled until 24 valid BIP39 words
  - "Replace identity" disabled until typed prefix matches current hash
- Mnemonic-validation logic: word count, BIP39 wordlist membership, checksum, identity_hash preview rendering
- Identity-hash truncation/copy: 8-char prefix shown; full 64-char hex copied to clipboard on click
- Blur-to-reveal: pre-reveal state hides words, post-reveal state shows them
- Error-rendering per the error-handling table
- Tauri `invoke` calls mocked via the existing pattern in other vitest specs

### Rust unit tests — new `recovery_cli` variants

- `export_mnemonic_words_with_keychain` returns 24 words; round-trip through `restore_mnemonic_from_words_with_keychain` yields the original `identity_hash`
- The existing `_cli` and `_to_writers` functions still produce identical output after the refactor (regression coverage on the delegation)
- All hermetic via `_with_keychain(None)` injection

### Integration — round-trip parity with CLI

Add to `src-tauri/tests/recovery_cli_integration.rs`:

- Export-via-Tauri-command → restore-via-CLI yields original `identity_hash`
- Export-via-CLI → restore-via-Tauri-command yields original `identity_hash`

This proves the GUI/Rust seam doesn't drift from CLI behavior.

### Manual smoke (one-shot, before merge)

A short checklist in the PR description:

1. **macOS:** real Keychain write/read, real Tauri save/open dialogs, mnemonic round-trip, recovery-file round-trip with comment field
2. **Linux** (libsecret available): same checklist
3. **Windows** (Credential Manager): same checklist
4. **Cross-platform:** export on macOS, restore on Linux. Same `identity_hash`.

## Definition of done

1. Settings → Identity panel exposes "Backup..." and "Restore..." controls plus the truncated identity_hash with click-to-copy.
2. Each of the four sub-flows works end-to-end against `~/.harmony/identity.{enc,key}` on macOS, Linux, and Windows.
3. Round-trip parity with the CLI: GUI-exported artifact restores via `harmony-app restore-*` and vice versa.
4. ZEB-180's metadata-on-restore display ships in this PR (`minted_at` + `comment` shown on restore-file step 2b and step 4).
5. Vitest coverage for the wizard component logic; Rust unit tests for the new word-array variants; integration round-trip with the CLI.
6. `harmony-client/docs/headless-install.md` (or a new `docs/identity-backup.md`) updated with screenshots of the GUI flow alongside the existing CLI examples.

## Resolved design decisions (audit trail)

| Question | Choice | Reason |
|---|---|---|
| Wizard scope | Settings-only (no first-launch flow) | Tighter scope; matches ZEB-176's `docs/headless-install.md` posture (CLI handles fresh-install restore) |
| Entry-point structure | Two buttons: Backup… / Restore… | Honest grouping (mnemonic/file are alternatives, not separate features); clean panel |
| Wizard placement | In-place transformation inside Settings panel | Consistent with existing Settings UX; no extra modal layer |
| Mnemonic display | Click-to-reveal (blurred default), no clipboard, no auto-hide timer | Defends against incidental shoulder-surf; near-zero implementation cost; auto-hide annoys real transcribers |
| Close confirmation | "I've stored this safely" checkbox | Signals seriousness without punishing power users; matches password-manager industry standard |
| Restore overwrite friction | TypeToConfirmDialog requiring first 8 chars of current `identity_hash` | Strongest existing primitive; user can't comply unless they understand which identity they're overwriting |
| Recovery-file passphrase UX | Standard: masked + show/hide + confirm-on-export, no strength meter, no generator | Strength meters are misleading; auto-generators move the storage problem rather than solving it; confirm-on-export is the load-bearing piece |
| `identity_hash` placement | At decision points only (8-char prefix + click-to-copy-full) | Matters at decisions, noise elsewhere |
