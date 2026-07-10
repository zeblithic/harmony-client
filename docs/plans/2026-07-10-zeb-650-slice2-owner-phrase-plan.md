# ZEB-650 Slice 2 — Owner Recovery Phrase (Option A) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Surface the 24-word owner recovery phrase in the GUI behind a deliberate-reveal flow — the first (and only) IPC returning owner seed material to the webview.

**Architecture:** A thin `#[tauri::command] export_owner_mnemonic_words` wraps the existing, fully-gated `recovery_cli::export_owner_mnemonic_words_with_keychain` via a testable `*_dto` seam (ZEB-428). A new shared `OwnerPhraseReveal.svelte` mirrors IdentityPanel's `mnemonicReveal` blur-grid state machine with a fetch gate (IPC fires only on explicit confirm — never on mount), cross-checks `dto.ownerId` against the host surface's owner, and is mounted in WelcomeModal's backup stage and a DevicesPanel modal.

**Tech Stack:** Rust/Tauri 2 (src-tauri), Svelte 5 runes + vitest/@testing-library, existing `onboarding-backup-flags` module (slice 1).

**Spec:** `docs/specs/2026-07-09-zeb-650-commons-g-deferred-data-design.md` §3 + §5 + §6 (approved; Option A).

## Global Constraints

- **Redaction invariant (spec §3.3):** `OwnerMnemonicDto.ownerId` (exactly 32 hex chars) exists ONLY for the cross-check and must NEVER be rendered — it would trip the WelcomeModal invariant `expect(container.innerHTML).not.toMatch(/[0-9a-f]{32,}/)`. Both existing WelcomeModal redaction tests stay byte-identical and passing.
- **Reveal invariant (spec §3.3, restated in the component header):** owner seed material may exist in the webview only as BIP39 words, only inside `OwnerPhraseReveal`, only after an explicit user reveal action, never past the component's visible lifetime. The export IPC fires only on that action — never on mount.
- **ZEB-428:** no `KeychainStore::new()` reachable from tests — tests exercise `export_owner_mnemonic_dto(dir, None)` with `HARMONY_PASSPHRASE` set; only the `#[tauri::command]` wrapper constructs the real keychain.
- **DTO naming:** `#[serde(rename_all = "camelCase")]`; TS reads `{ words, ownerId }`.
- Svelte 5 runes only; budget-0 color tokens (no hex literals in styles — `commons-hex-guard` stays empty); existing testids/aria/copy pins preserved byte-identical.
- New testids (exact): `phrase-reveal-open`, `phrase-reveal-warning`, `phrase-reveal-error`, `phrase-reveal-confirm`, `phrase-reveal-cancel`, `phrase-grid`, `phrase-reveal-unblur`, `phrase-copy`, `phrase-written-down`, `phrase-reveal-hide`, `devices-view-phrase`.
- Warning copy (exact): "Anyone who sees these 24 words controls your identity. Make sure no one is watching your screen."
- Frontend gates per task: `npx tsc --noEmit` + targeted `npx vitest run <file>`. Rust gates per task: `cargo fmt --all` + targeted `cargo nextest run --locked -p harmony-app --features test-fixtures -E '<filter>'`. Final sweep (Task 5): full clippy `--all-targets` + `scripts/test-select --full` + full vitest.
- Commit per task with trailers:
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` and
  `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`.
- Branch: `zeb-650-slice2-owner-recovery` (already cut from `9846f1b1`).

---

### Task 1: Rust — `OwnerMnemonicDto` + `export_owner_mnemonic_words` command

**Files:**
- Modify: `src-tauri/src/owner_commands.rs` (DTO + seam helper + command + tests; insert after `issue_owner_recovery_token`, ~line 494)
- Modify: `src-tauri/src/lib.rs:52944` (register command in the owner cluster; also inspect the test-builder `generate_handler!` at `lib.rs:53147` and mirror there IF it enumerates owner commands)

**Interfaces:**
- Consumes: `crate::recovery_cli::export_owner_mnemonic_words_with_keychain(identity_dir: &Path, keychain: Option<KeychainStore>) -> Result<(Vec<String>, [u8; 16]), String>` (`recovery_cli.rs:239`, all three gates + exact error strings live there); `resolve_identity_dir()` (`owner_commands.rs:145`); `run_blocking` (`identity_commands.rs:540`, already imported at `owner_commands.rs:7`).
- Produces: IPC command `export_owner_mnemonic_words` (no args) → `{ words: string[], ownerId: string }` — Task 2's TS caller depends on these exact key names.

- [ ] **Step 1: Write the failing tests** — append to `owner_commands.rs` (add the `mod` at end of file if no `#[cfg(test)]` mod exists; if one exists, add these tests inside it):

```rust
#[cfg(test)]
mod owner_mnemonic_dto_tests {
    use super::*;
    use serial_test::serial;

    /// Plant a minted owner (with master seed) in `dir`. Mirrors
    /// recovery_cli.rs::plant_owner_and_export_words minus the export.
    fn plant_owner(dir: &std::path::Path) -> (harmony_owner::OwnerState, [u8; 32]) {
        use harmony_owner::lifecycle::{mint_owner, MintResult};
        let MintResult {
            state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed = *recovery_artifact.as_bytes();
        crate::owner_state::save_owner_state_atomic(
            dir,
            &state,
            &device_signing_key,
            Some(&master_seed),
            None,
        )
        .unwrap();
        (state, master_seed)
    }

    #[test]
    #[serial]
    fn export_owner_mnemonic_dto_round_trips_words_and_owner_id() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HARMONY_PASSPHRASE", "owner-mnemonic-dto-test");
        let (state, master_seed) = plant_owner(dir.path());

        let dto = export_owner_mnemonic_dto(dir.path(), None).expect("export must succeed");
        assert_eq!(dto.words.len(), 24, "owner mnemonic is 24 words");
        assert_eq!(dto.owner_id, hex::encode(state.owner_id));
        // Words must round-trip to the same master seed (same invariant the
        // recovery_cli tests pin; kept here so the DTO layer cannot drift).
        // NOTE to implementer: copy the exact from_mnemonic round-trip calls
        // from recovery_cli.rs::export_owner_mnemonic_writes_words_to_stdout_
        // and_owner_id_to_stderr (~lines 1490-1520) if the two lines below
        // don't match the real RecoveryArtifact API.
        use harmony_owner::lifecycle::RecoveryArtifact;
        let restored = RecoveryArtifact::from_mnemonic(&dto.words.join(" "))
            .expect("24 exported words must round-trip");
        assert_eq!(restored.as_bytes(), &master_seed);
        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn export_owner_mnemonic_dto_errors_when_seed_wiped() {
        use harmony_owner::lifecycle::{mint_owner, MintResult};
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HARMONY_PASSPHRASE", "owner-mnemonic-dto-wiped");
        let MintResult {
            state,
            device_signing_key,
            ..
        } = mint_owner(1_700_000_000).unwrap();
        // Persist WITHOUT the master seed — the wiped/joiner model.
        crate::owner_state::save_owner_state_atomic(
            dir.path(),
            &state,
            &device_signing_key,
            None,
            None,
        )
        .unwrap();
        let err = export_owner_mnemonic_dto(dir.path(), None).unwrap_err();
        assert!(err.contains("wiped"), "wiped-seed error must surface: {err}");
        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    fn owner_mnemonic_dto_serializes_camel_case() {
        let dto = OwnerMnemonicDto {
            words: vec!["abandon".into()],
            owner_id: "ab".into(),
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains("\"ownerId\""), "camelCase key required: {json}");
        assert!(json.contains("\"words\""));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(owner_mnemonic_dto)'`
Expected: COMPILE FAILURE — `export_owner_mnemonic_dto` / `OwnerMnemonicDto` not found.

- [ ] **Step 3: Implement** — insert after `issue_owner_recovery_token` (~`owner_commands.rs:494`), before `preview_owner_mnemonic_identity`. Ensure `use std::path::Path;` is in scope (add to the existing `std::path` import if only `PathBuf` is imported):

```rust
/// Wire DTO for the owner recovery-phrase reveal (ZEB-650 slice 2).
///
/// `owner_id` exists ONLY so the webview can cross-check the words against
/// the owner it is currently displaying. It must never be rendered: it is a
/// 32-hex-char run, which the WelcomeModal redaction invariant
/// (`/[0-9a-f]{32,}/` never in `innerHTML`) forbids in the DOM.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerMnemonicDto {
    pub words: Vec<String>,
    pub owner_id: String,
}

/// Testable core of [`export_owner_mnemonic_words`] (ZEB-428 seam: tests
/// inject `keychain: None` + `HARMONY_PASSPHRASE`; only the command wrapper
/// constructs the real keychain).
pub(crate) fn export_owner_mnemonic_dto(
    identity_dir: &Path,
    keychain: Option<KeychainStore>,
) -> Result<OwnerMnemonicDto, String> {
    let (words, owner_id) =
        crate::recovery_cli::export_owner_mnemonic_words_with_keychain(identity_dir, keychain)?;
    Ok(OwnerMnemonicDto {
        words,
        owner_id: hex::encode(owner_id),
    })
}

/// Return the 24 BIP39 owner-mnemonic words + owner id for the GUI reveal
/// (ZEB-650 slice 2). The first and only command returning owner seed
/// material to the webview; the renderer shows the words only behind an
/// explicit user reveal action (`OwnerPhraseReveal.svelte`), and the IPC
/// fires only on that action — never on mount.
///
/// Inherits all three gates from
/// [`crate::recovery_cli::export_owner_mnemonic_words_with_keychain`]:
/// owner minted; master seed still on device (the `canBackUp` condition);
/// seed↔owner-id invariant.
#[tauri::command]
pub async fn export_owner_mnemonic_words(
    _app: tauri::AppHandle,
) -> Result<OwnerMnemonicDto, String> {
    let identity_dir = resolve_identity_dir()?;
    run_blocking(move || export_owner_mnemonic_dto(&identity_dir, KeychainStore::new().ok())).await
}
```

Register in `src-tauri/src/lib.rs` — in the owner cluster (after line `owner_commands::issue_owner_recovery_token,` ~52944):

```rust
            owner_commands::export_owner_mnemonic_words,
```

Then Read the test-builder block around `lib.rs:53147`: if that `generate_handler!` enumerates `owner_commands::*` entries, add the same line there; if it does not (different command set), leave it untouched.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(owner_mnemonic_dto)'`
Expected: 3 tests PASS.

- [ ] **Step 5: Gates + commit**

Run: `cd src-tauri && cargo fmt --all && cargo clippy --locked -p harmony-app --features test-fixtures --no-deps -- -D warnings`
Expected: clean.

```bash
git add src-tauri/src/owner_commands.rs src-tauri/src/lib.rs
git commit -m "ZEB-650 slice 2: export_owner_mnemonic_words command via ZEB-428 dto seam"
```

---

### Task 2: `OwnerPhraseReveal.svelte` shared component

**Files:**
- Create: `src/lib/components/OwnerPhraseReveal.svelte`
- Test: `src/lib/components/__tests__/OwnerPhraseReveal.test.ts`

**Interfaces:**
- Consumes: IPC `export_owner_mnemonic_words` → `{ words: string[], ownerId: string }` (Task 1); `markRecoveryBackedUp(ownerId)` from `../onboarding-backup-flags` (slice 1 — also stamps `recoveryBackedUpAt` and dispatches `BACKUP_FLAGS_CHANGED_EVENT`).
- Produces: component with props `{ ownerId: string }` — Tasks 3 and 4 mount it with the host surface's current owner id.

- [ ] **Step 1: Write the failing tests**

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import OwnerPhraseReveal from '../OwnerPhraseReveal.svelte';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
import { invoke } from '@tauri-apps/api/core';
const mockInvoke = vi.mocked(invoke);

// Realistic 32-hex owner id — the redaction tests below depend on this
// being long enough to trip /[0-9a-f]{32,}/ if it ever rendered.
const OWNER = 'a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6';
const WORDS = [
  'abandon', 'ability', 'able', 'about', 'above', 'absent',
  'absorb', 'abstract', 'absurd', 'abuse', 'access', 'accident',
  'account', 'accuse', 'achieve', 'acid', 'acoustic', 'acquire',
  'across', 'act', 'action', 'actor', 'actress', 'actual',
];
const backedUpKey = (id: string) =>
  `harmony.onboarding.recoveryArtifactBackedUp:owner-${id}`;

beforeEach(() => {
  mockInvoke.mockReset();
  localStorage.clear();
  sessionStorage.clear();
});

async function revealWords(utils: ReturnType<typeof render>) {
  const { getByTestId } = utils;
  await fireEvent.click(getByTestId('phrase-reveal-open'));
  await fireEvent.click(getByTestId('phrase-reveal-confirm'));
  await Promise.resolve();
  await Promise.resolve();
}

describe('OwnerPhraseReveal (ZEB-650 slice 2, Option A)', () => {
  it('renders collapsed with no words and NO IPC call on mount', () => {
    const { getByTestId, queryByTestId, container } = render(OwnerPhraseReveal, {
      props: { ownerId: OWNER },
    });
    expect(getByTestId('phrase-reveal-open')).toBeTruthy();
    expect(queryByTestId('phrase-grid')).toBeNull();
    expect(mockInvoke).not.toHaveBeenCalled();
    expect(container.textContent).not.toContain('abandon');
  });

  it('opening shows the warning but still fires no IPC', async () => {
    const { getByTestId } = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await fireEvent.click(getByTestId('phrase-reveal-open'));
    expect(getByTestId('phrase-reveal-warning')).toBeTruthy();
    expect(mockInvoke).not.toHaveBeenCalled();
  });

  it('confirm fires the IPC exactly once and renders the blurred 24-word grid', async () => {
    mockInvoke.mockResolvedValue({ words: WORDS, ownerId: OWNER });
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await revealWords(utils);
    expect(mockInvoke).toHaveBeenCalledTimes(1);
    expect(mockInvoke).toHaveBeenCalledWith('export_owner_mnemonic_words');
    const grid = utils.getByTestId('phrase-grid');
    expect(grid.querySelectorAll('li').length).toBe(24);
    expect(grid.classList.contains('blurred')).toBe(true);
  });

  it('unblur reveals the grid; copy + checkbox appear only then', async () => {
    mockInvoke.mockResolvedValue({ words: WORDS, ownerId: OWNER });
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await revealWords(utils);
    expect(utils.queryByTestId('phrase-copy')).toBeNull();
    await fireEvent.click(utils.getByTestId('phrase-reveal-unblur'));
    expect(utils.getByTestId('phrase-grid').classList.contains('blurred')).toBe(false);
    expect(utils.getByTestId('phrase-copy')).toBeTruthy();
    expect(utils.getByTestId('phrase-written-down')).toBeTruthy();
  });

  it('ownerId mismatch discards the words and shows an error — nothing renders', async () => {
    mockInvoke.mockResolvedValue({
      words: WORDS,
      ownerId: 'ffffffffffffffffffffffffffffffff',
    });
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await revealWords(utils);
    expect(utils.queryByTestId('phrase-grid')).toBeNull();
    expect(utils.getByTestId('phrase-reveal-error').textContent).toContain(
      'does not match',
    );
    expect(utils.container.textContent).not.toContain('abandon');
  });

  it('IPC failure shows the backend error inline (wiped-seed case reads naturally)', async () => {
    mockInvoke.mockRejectedValue(
      new Error('Master seed has been wiped from this device — backup is no longer possible.'),
    );
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await revealWords(utils);
    expect(utils.getByTestId('phrase-reveal-error').textContent).toContain('wiped');
    expect(utils.queryByTestId('phrase-grid')).toBeNull();
  });

  it('"I have written these words down" marks the owner-scoped backed-up flag', async () => {
    mockInvoke.mockResolvedValue({ words: WORDS, ownerId: OWNER });
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await revealWords(utils);
    await fireEvent.click(utils.getByTestId('phrase-reveal-unblur'));
    expect(localStorage.getItem(backedUpKey(OWNER))).toBeNull();
    await fireEvent.click(utils.getByTestId('phrase-written-down'));
    expect(localStorage.getItem(backedUpKey(OWNER))).toBe('true');
  });

  it('mere reveal does NOT count as backed up', async () => {
    mockInvoke.mockResolvedValue({ words: WORDS, ownerId: OWNER });
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await revealWords(utils);
    await fireEvent.click(utils.getByTestId('phrase-reveal-unblur'));
    expect(localStorage.getItem(backedUpKey(OWNER))).toBeNull();
  });

  it('hide collapses and clears the words from the DOM', async () => {
    mockInvoke.mockResolvedValue({ words: WORDS, ownerId: OWNER });
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await revealWords(utils);
    await fireEvent.click(utils.getByTestId('phrase-reveal-unblur'));
    await fireEvent.click(utils.getByTestId('phrase-reveal-hide'));
    expect(utils.queryByTestId('phrase-grid')).toBeNull();
    expect(utils.container.textContent).not.toContain('abandon');
    expect(utils.getByTestId('phrase-reveal-open')).toBeTruthy();
  });

  // ── Redaction invariant (spec §3.3): dto.ownerId must never render ──
  it('never renders a 32+ hex run at ANY phase (dto.ownerId stays out of the DOM)', async () => {
    mockInvoke.mockResolvedValue({ words: WORDS, ownerId: OWNER });
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    expect(utils.container.innerHTML).not.toMatch(/[0-9a-f]{32,}/i);
    await revealWords(utils);
    expect(utils.container.innerHTML).not.toMatch(/[0-9a-f]{32,}/i);
    await fireEvent.click(utils.getByTestId('phrase-reveal-unblur'));
    expect(utils.container.innerHTML).not.toMatch(/[0-9a-f]{32,}/i);
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/OwnerPhraseReveal.test.ts`
Expected: FAIL — cannot resolve `../OwnerPhraseReveal.svelte`.

- [ ] **Step 3: Implement the component**

```svelte
<script lang="ts">
  /**
   * Owner recovery-phrase reveal (ZEB-650 slice 2, Option A).
   *
   * Invariant (spec §3.3): owner seed material may exist in the webview
   * only as BIP39 words, only inside this component, only after an explicit
   * user reveal action, and never past the component's visible lifetime.
   * The export IPC fires only on the confirm click — never on mount. Word
   * state is dropped on collapse/unmount (best-effort: JS strings cannot be
   * zeroized; the invariant is about DOM exposure and lifetime).
   *
   * dto.ownerId (a 32-hex-char run) exists only for the cross-check below
   * and must NEVER be rendered — the WelcomeModal redaction invariant
   * forbids any /[0-9a-f]{32,}/ run in innerHTML.
   */
  import { invoke } from '@tauri-apps/api/core';
  import { markRecoveryBackedUp } from '../onboarding-backup-flags';

  interface OwnerMnemonicDto {
    words: string[];
    ownerId: string;
  }

  interface Props {
    /** Hex owner id the host surface is displaying (reveal cross-check). */
    ownerId: string;
  }
  let { ownerId }: Props = $props();

  type Phase =
    | { kind: 'collapsed' }
    | { kind: 'confirm'; inFlight: boolean; error: string | null }
    | { kind: 'revealed'; words: string[]; unblurred: boolean; writtenDown: boolean };

  let phase = $state<Phase>({ kind: 'collapsed' });
  let copied = $state(false);

  function collapse() {
    // Drops the words with the state object — they leave the DOM now.
    phase = { kind: 'collapsed' };
    copied = false;
  }

  async function fetchWords() {
    if (phase.kind !== 'confirm' || phase.inFlight) return;
    phase = { kind: 'confirm', inFlight: true, error: null };
    let dto: OwnerMnemonicDto;
    try {
      dto = await invoke<OwnerMnemonicDto>('export_owner_mnemonic_words');
    } catch (e) {
      phase = {
        kind: 'confirm',
        inFlight: false,
        error: e instanceof Error ? e.message : String(e),
      };
      return;
    }
    if (dto.ownerId !== ownerId) {
      // Words belong to a different identity than the one on screen —
      // discard them and render nothing.
      phase = {
        kind: 'confirm',
        inFlight: false,
        error: 'Recovery phrase does not match the identity on screen — not displaying it.',
      };
      return;
    }
    phase = { kind: 'revealed', words: dto.words, unblurred: false, writtenDown: false };
  }

  function toggleWrittenDown() {
    if (phase.kind !== 'revealed') return;
    const next = !phase.writtenDown;
    phase = { ...phase, writtenDown: next };
    // Marking is one-way: unchecking doesn't unmark (there is no honest
    // "un-back-up" — the words were seen and may be on paper).
    if (next) markRecoveryBackedUp(ownerId);
  }

  async function copyWords() {
    if (phase.kind !== 'revealed' || !phase.unblurred) return;
    try {
      await navigator.clipboard.writeText(phase.words.join(' '));
      copied = true;
    } catch {
      copied = false;
    }
  }
</script>

{#if phase.kind === 'collapsed'}
  <button
    type="button"
    class="linklike"
    data-testid="phrase-reveal-open"
    onclick={() => {
      phase = { kind: 'confirm', inFlight: false, error: null };
    }}
  >
    Or write down your 24-word recovery phrase instead
  </button>
{:else if phase.kind === 'confirm'}
  <div class="phrase-warning" role="note" data-testid="phrase-reveal-warning">
    <p class="warning-copy">
      Anyone who sees these 24 words controls your identity. Make sure no one
      is watching your screen.
    </p>
    {#if phase.error}
      <p class="error" role="alert" data-testid="phrase-reveal-error">{phase.error}</p>
    {/if}
    <div class="phrase-actions">
      <button
        type="button"
        class="secondary"
        data-testid="phrase-reveal-cancel"
        onclick={collapse}
        disabled={phase.inFlight}
      >
        Cancel
      </button>
      <button
        type="button"
        class="primary"
        data-testid="phrase-reveal-confirm"
        onclick={fetchWords}
        disabled={phase.inFlight}
      >
        {phase.inFlight ? 'Loading…' : 'Show recovery phrase'}
      </button>
    </div>
  </div>
{:else}
  <div class="phrase-revealed">
    <!-- Masked placeholders until the explicit Reveal: blur alone is only
         visual — screen readers, find-in-page, and DOM inspection would
         still see the words (round-1 amendment, CodeRabbit PR #437). -->
    <ol
      data-testid="phrase-grid"
      class="mnemonic-grid"
      class:blurred={!phase.unblurred}
    >
      {#each phase.words as w, i (i)}
        <li class="word">{phase.unblurred ? w : '••••••'}</li>
      {/each}
    </ol>
    {#if !phase.unblurred}
      <button
        type="button"
        class="secondary"
        data-testid="phrase-reveal-unblur"
        onclick={() => {
          if (phase.kind === 'revealed') phase = { ...phase, unblurred: true };
        }}
      >
        Reveal
      </button>
    {:else}
      <div class="phrase-actions">
        <button type="button" class="secondary" data-testid="phrase-copy" onclick={copyWords}>
          {copied ? 'Copied' : 'Copy'}
        </button>
        <button type="button" class="secondary" data-testid="phrase-reveal-hide" onclick={collapse}>
          Hide
        </button>
      </div>
      <label class="confirm-label">
        <input
          type="checkbox"
          data-testid="phrase-written-down"
          checked={phase.writtenDown}
          onchange={toggleWrittenDown}
        />
        I've written these words down
      </label>
    {/if}
  </div>
{/if}

<style>
  .mnemonic-grid {
    display: grid;
    grid-template-columns: repeat(4, 1fr);
    gap: 6px;
    background: var(--bg-tertiary);
    border-radius: 6px;
    padding: 12px 12px 12px 36px; /* left padding leaves room for the list marker */
    font-family: var(--font-mono);
    font-size: 0.85em;
    margin: 12px 0;
    list-style: decimal;
  }
  .mnemonic-grid.blurred {
    filter: blur(6px);
    user-select: none;
  }
  .word {
    padding: 2px 0;
  }
  .confirm-label {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 12px 0;
    cursor: pointer;
    color: var(--text-primary);
  }
  .phrase-warning {
    margin: 12px 0;
    padding: 12px;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg-tertiary);
  }
  .warning-copy {
    margin: 0 0 10px;
    color: var(--text-primary);
  }
  .phrase-actions {
    display: flex;
    gap: 8px;
    margin: 8px 0;
  }
  .error {
    color: var(--error, var(--text-danger));
    margin: 8px 0;
  }
  .linklike {
    background: none;
    border: none;
    padding: 0;
    color: var(--accent);
    cursor: pointer;
    text-decoration: underline;
    font-size: 0.9em;
  }
  .primary,
  .secondary {
    padding: 6px 14px;
    border-radius: 6px;
    cursor: pointer;
  }
  .primary {
    background: var(--accent);
    color: var(--bg-primary);
    border: 1px solid var(--accent);
  }
  .secondary {
    background: transparent;
    color: var(--text-primary);
    border: 1px solid var(--border);
  }
  button:disabled {
    opacity: 0.6;
    cursor: default;
  }
</style>
```

NOTE to implementer: before finalizing the styles, check the CSS custom
properties actually defined in `src/styles` (or App-level `:root`) — use the
project's real token names (`--error` vs `--text-danger` vs `--danger`,
`--accent` vs `--accent-primary`). Token names above are the plan's best
guess; the budget-0 rule (no raw hex) is the hard requirement, exact token
choice follows the neighboring components (IdentityPanel / DevicesPanel
styles are the reference).

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/OwnerPhraseReveal.test.ts`
Expected: 10 tests PASS.

- [ ] **Step 5: Type gate + commit**

Run: `npx tsc --noEmit`
Expected: clean.

```bash
git add src/lib/components/OwnerPhraseReveal.svelte src/lib/components/__tests__/OwnerPhraseReveal.test.ts
git commit -m "ZEB-650 slice 2: OwnerPhraseReveal deliberate-reveal component"
```

---

### Task 3: Mount in WelcomeModal backup stage

**Files:**
- Modify: `src/lib/components/WelcomeModal.svelte` (import + mount below the backup actions, ~line 341, before `.wizard-rail`)
- Modify: `src/lib/components/__tests__/WelcomeModal.test.ts` (add `@tauri-apps/api/core` mock + 2 new tests; existing tests untouched)

**Interfaces:**
- Consumes: `OwnerPhraseReveal` with `ownerId={mintResult.state.ownerId}` (Task 2); `mintResult: MintIpcResult | null` already held by the modal (line 45).
- Produces: nothing new for later tasks.

- [ ] **Step 1: Write the failing tests** — append a new describe to `WelcomeModal.test.ts`. Also add at the top of the file (after the existing owner-service/pairing mocks, which stay byte-identical):

```typescript
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
```

and import + reset it:

```typescript
import { invoke } from '@tauri-apps/api/core';
const mockCoreInvoke = vi.mocked(invoke);
// in the existing beforeEach, add: mockCoreInvoke.mockReset();
```

New describe (reuses the file's existing `advanceToBackupStage` helper, whose mint fixture uses ownerId `'a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6'`):

```typescript
describe('WelcomeModal owner phrase reveal (ZEB-650 slice 2)', () => {
  const WORDS = [
    'abandon', 'ability', 'able', 'about', 'above', 'absent',
    'absorb', 'abstract', 'absurd', 'abuse', 'access', 'accident',
    'account', 'accuse', 'achieve', 'acid', 'acoustic', 'acquire',
    'across', 'act', 'action', 'actor', 'actress', 'actual',
  ];

  it('backup stage offers the write-it-down alternative without firing IPC', async () => {
    const { getByTestId } = await advanceToBackupStage();
    expect(getByTestId('phrase-reveal-open')).toBeTruthy();
    expect(mockCoreInvoke).not.toHaveBeenCalled();
  });

  it('full reveal inside the modal keeps the hex-redaction invariant', async () => {
    mockCoreInvoke.mockResolvedValue({
      words: WORDS,
      ownerId: 'a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6',
    });
    const { getByTestId, container } = await advanceToBackupStage();
    await fireEvent.click(getByTestId('phrase-reveal-open'));
    await fireEvent.click(getByTestId('phrase-reveal-confirm'));
    await Promise.resolve();
    await Promise.resolve();
    await fireEvent.click(getByTestId('phrase-reveal-unblur'));
    expect(getByTestId('phrase-grid').querySelectorAll('li').length).toBe(24);
    // dto.ownerId (32 hex chars) must never reach the DOM.
    expect(container.innerHTML).not.toMatch(/[0-9a-f]{32,}/i);
  });
});
```

- [ ] **Step 2: Run to verify failure**

Run: `npx vitest run src/lib/components/__tests__/WelcomeModal.test.ts`
Expected: the 2 new tests FAIL (`phrase-reveal-open` not found); all pre-existing tests still PASS.

- [ ] **Step 3: Implement the mount** — in `WelcomeModal.svelte`:

Add import beside the other component imports:

```typescript
import OwnerPhraseReveal from './OwnerPhraseReveal.svelte';
```

In the `backup` stage markup, insert after the `.actions` div (the one holding `welcome-save-backup` / `welcome-skip-backup`) and before `.wizard-rail`:

```svelte
        {#if mintResult !== null}
          <div class="phrase-alternative">
            <OwnerPhraseReveal ownerId={mintResult.state.ownerId} />
          </div>
        {/if}
```

Add style (token-only):

```css
  .phrase-alternative {
    margin-top: 12px;
    padding-top: 12px;
    border-top: 1px solid var(--border);
  }
```

- [ ] **Step 4: Run the full file to verify everything passes**

Run: `npx vitest run src/lib/components/__tests__/WelcomeModal.test.ts`
Expected: ALL tests PASS — including the two pre-existing redaction tests, byte-identical.

- [ ] **Step 5: Type gate + commit**

Run: `npx tsc --noEmit`

```bash
git add src/lib/components/WelcomeModal.svelte src/lib/components/__tests__/WelcomeModal.test.ts
git commit -m "ZEB-650 slice 2: phrase reveal in WelcomeModal backup stage"
```

---

### Task 4: Mount in DevicesPanel (modal) + last-backed-up freshness

**Files:**
- Modify: `src/lib/components/DevicesPanel.svelte` (button beside "Back up owner identity →" ~line 429; `Modal` host beside the existing backup modal ~line 544; flags-changed listener)
- Modify: `src/lib/components/__tests__/DevicesPanel.test.ts` (new describe)

**Interfaces:**
- Consumes: `OwnerPhraseReveal` (Task 2); `Modal` (already imported, line 15); `BACKUP_FLAGS_CHANGED_EVENT`, `recoveryBackedUpAtMs` from `../onboarding-backup-flags` (slice 1).
- Produces: nothing new for later tasks.

- [ ] **Step 1: Write the failing tests** — append to `DevicesPanel.test.ts` (reuse the file's `metaView` fixture: ownerId `'a4f1c8239b7dd809abcdef0123456789'`, 2 devices, `canBackUp: true`; the global `invoke` is the file's ordered-stub mock; `owner-meta` stays mocked as a unit):

```typescript
describe('DevicesPanel owner phrase reveal (ZEB-650 slice 2)', () => {
  const WORDS = [
    'abandon', 'ability', 'able', 'about', 'above', 'absent',
    'absorb', 'abstract', 'absurd', 'abuse', 'access', 'accident',
    'account', 'accuse', 'achieve', 'acid', 'acoustic', 'acquire',
    'across', 'act', 'action', 'actor', 'actress', 'actual',
  ];

  it('renders the view-phrase button beside back-up, enabled when canBackUp', async () => {
    mockedInvoke.mockResolvedValueOnce(metaView); // get_owner_state
    const { findByTestId } = render(DevicesPanel);
    const btn = await findByTestId('devices-view-phrase');
    expect((btn as HTMLButtonElement).disabled).toBe(false);
  });

  it('disables the view-phrase button when the seed is wiped (canBackUp false)', async () => {
    mockedInvoke.mockResolvedValueOnce({ ...metaView, canBackUp: false });
    const { findByTestId } = render(DevicesPanel);
    const btn = await findByTestId('devices-view-phrase');
    expect((btn as HTMLButtonElement).disabled).toBe(true);
  });

  it('opens the phrase modal, reveals via ordered stub, and checkbox updates last-backed-up', async () => {
    mockedInvoke
      .mockResolvedValueOnce(metaView) // get_owner_state on mount
      .mockResolvedValueOnce({ words: WORDS, ownerId: metaView.ownerId }); // export_owner_mnemonic_words
    const { findByTestId, getByTestId, queryByTestId } = render(DevicesPanel);
    await fireEvent.click(await findByTestId('devices-view-phrase'));
    // Modal open, still collapsed — export not yet consumed.
    expect(getByTestId('phrase-reveal-open')).toBeTruthy();
    await fireEvent.click(getByTestId('phrase-reveal-open'));
    await fireEvent.click(getByTestId('phrase-reveal-confirm'));
    await Promise.resolve();
    await Promise.resolve();
    expect(getByTestId('phrase-grid').querySelectorAll('li').length).toBe(24);
    await fireEvent.click(getByTestId('phrase-reveal-unblur'));
    // Before the checkbox: no last-backed-up line for this owner.
    expect(queryByTestId('devices-last-backed-up')).toBeNull();
    await fireEvent.click(getByTestId('phrase-written-down'));
    await tick();
    // markRecoveryBackedUp dispatched the flags-changed event → the panel's
    // listener refreshed lastBackedUpMs without a remount.
    expect(await findByTestId('devices-last-backed-up')).toBeTruthy();
  });
});
```

Add `import { tick } from 'svelte';` to the test file's imports if not already present.

- [ ] **Step 2: Run to verify failure**

Run: `npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts`
Expected: the 3 new tests FAIL (`devices-view-phrase` not found); all pre-existing tests PASS (the reveal IPC only fires on click, so the ordered stubs of older tests are untouched).

- [ ] **Step 3: Implement** — in `DevicesPanel.svelte`:

Imports (extend the existing lines):

```typescript
import OwnerPhraseReveal from './OwnerPhraseReveal.svelte';
// extend the existing onboarding-backup-flags import with:
//   BACKUP_FLAGS_CHANGED_EVENT
```

State (beside `backupOpen`):

```typescript
let phraseOpen = $state(false);
```

Flags-changed listener (beside the existing owner-keyed meta `$effect`) — keeps
the "Last backed up" line fresh when the phrase checkbox (or any other
surface) marks backed-up while the panel is mounted:

```typescript
$effect(() => {
  const refresh = () => {
    const oid = state?.ownerId ?? null;
    lastBackedUpMs = oid ? recoveryBackedUpAtMs(oid) : null;
  };
  window.addEventListener(BACKUP_FLAGS_CHANGED_EVENT, refresh);
  return () => window.removeEventListener(BACKUP_FLAGS_CHANGED_EVENT, refresh);
});
```

Button — insert immediately after the "Back up owner identity →" button
(~line 436), same disabled gate:

```svelte
          <button
            class="secondary"
            data-testid="devices-view-phrase"
            disabled={!state.canBackUp}
            title={state.canBackUp ? '' : 'Master seed not on this device — the recovery phrase can no longer be shown.'}
            onclick={() => {
              phraseOpen = true;
            }}
          >
            View recovery phrase
          </button>
```

Modal host — insert beside the existing backup modal block (~line 598):

```svelte
  {#if phraseOpen && state !== null}
    <Modal onCancel={() => { phraseOpen = false; }} ariaLabelledby="phrase-modal-heading">
      <h3 class="modal-title" id="phrase-modal-heading">Owner recovery phrase</h3>
      <OwnerPhraseReveal ownerId={state.ownerId} />
    </Modal>
  {/if}
```

(Closing the modal unmounts `OwnerPhraseReveal`, which drops the word state — the spec §3.2 teardown path.)

- [ ] **Step 4: Run the full file to verify everything passes**

Run: `npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts`
Expected: ALL tests PASS (old + 3 new).

- [ ] **Step 5: Type gate + commit**

Run: `npx tsc --noEmit`

```bash
git add src/lib/components/DevicesPanel.svelte src/lib/components/__tests__/DevicesPanel.test.ts
git commit -m "ZEB-650 slice 2: view-recovery-phrase modal in DevicesPanel"
```

---

### Task 5: Full gates + PR

- [ ] **Step 1: Frontend sweep**

Run: `npx tsc --noEmit && npx vitest run`
Expected: clean, all tests pass.

- [ ] **Step 2: Rust sweep** (lib changed → full clippy; full test sweep in background with a supervision net — macOS has no `timeout(1)`, use the Bash tool's timeout parameter / background mode):

Run: `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Then the full workspace sweep (final validation uses the explicit full command, never the selector script — compliance rule 1601744):
`cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
(run in background, supervise, expect ~relink cost since the lib changed).
Expected: clean.

- [ ] **Step 3: Push + PR**

```bash
git push -u origin zeb-650-slice2-owner-recovery
gh pr create --repo zeblithic/harmony-client \
  --title "ZEB-650 slice 2: owner recovery phrase — deliberate-reveal GUI export" \
  --body "<summary: new export_owner_mnemonic_words command (ZEB-428 dto seam), OwnerPhraseReveal component, WelcomeModal + DevicesPanel mounts, redaction invariant tests. Part of ZEB-650.>"
```

Then fire `@coderabbitai review` ONCE, update Linear, arm the converge wakeup.

---

## Self-review (writing-plans checklist)

- **Spec coverage:** §3.1 command+DTO+gates → Task 1; §3.2 state machine steps 1–5 → Task 2 (collapsed/confirm/revealed/checkbox/teardown) with mounts in Tasks 3–4; §3.3 redaction → Global Constraints + explicit tests in Tasks 2/3; §5 slice-2 test list → all present (Rust round-trip + wiped-seed in Task 1; TS no-words-pre-reveal, IPC-only-after-confirm, mismatch-discard, checkbox-marks, teardown-clears, existing redaction unchanged, ownerId-never-renders in Tasks 2–4).
- **Deviations from spec text, deliberate:** (a) §3.2 says "blurred until hover/hold per the IdentityPanel idiom" — the actual IdentityPanel idiom (verified in code) is a class-toggle blur cleared by an explicit Reveal button; the plan mirrors the real idiom, which is also the stronger posture. (b) §3.2's "click-confirm tier" is satisfied by the two-step confirm→unblur sequence.
- **Placeholders:** none — every step carries complete code; the two NOTEs to the implementer are verification instructions (check real token names; copy the exact `from_mnemonic` API from the named recovery_cli test), not deferred design.
- **Type consistency:** `OwnerMnemonicDto { words, ownerId }` identical across Rust serde output, TS interface, and all test fixtures; testids match between component and every consumer test.

## Round-1 amendments (bot converge, PR #437)

Applied after review; the task bodies above are otherwise historical:

1. **Masked grid** (CodeRabbit, Major): words render as `••••••` placeholders until the explicit Reveal — blur alone leaks to screen readers / find-in-page / DOM inspection. (Snippet above updated.)
2. **Error redaction** (Qodo, Bug): backend errors can embed 32-hex owner ids (seed↔owner-state mismatch); `sanitizeError()` strips `/[0-9a-f]{32,}/gi` before any message reaches the DOM.
3. **Post-await teardown guard** (Qodo, Bug): `alive` flag via `onDestroy` + phase re-check after the IPC await, so an Escape-dismissed modal discards a late resolution instead of storing words.
4. **WelcomeModal completion** (CodeRabbit, Major): new optional `onBackedUp` prop; WelcomeModal surfaces a `welcome-phrase-continue` button once the words are confirmed written down, completing via `onMinted` without recording a skip.
5. **DevicesPanel dedup** (CodeRabbit, Trivial): shared `refreshLastBackedUp()` helper for the owner-keyed effect + flags-changed listener.
6. Final-gate command spelled explicitly (Qodo compliance rule 1601744) — see Task 5 Step 2.
