import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import * as dialog from '@tauri-apps/plugin-dialog';
import IdentityPanel from '../IdentityPanel.svelte';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/plugin-dialog', () => ({ save: vi.fn() }));

const mockInvoke = vi.mocked(invoke);
const mockSave = vi.mocked(dialog.save);

describe('IdentityPanel — default state', () => {
  beforeEach(() => {
    mockInvoke.mockReset();
  });

  it('renders the truncated identity hash and two action buttons', async () => {
    // 32-char hex (actual [u8; 16] identity hash encoded as hex)
    const fullHash = 'a1b2c3d4'.repeat(4);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return fullHash;
      throw new Error(`unexpected invoke: ${cmd}`);
    });

    render(IdentityPanel);

    // Wait for the async load — 8-char prefix displayed as 0xXXXXXXXX…
    await screen.findByText(/0xa1b2c3d4/);

    expect(screen.getByRole('button', { name: /backup/i })).toBeTruthy();
    expect(screen.getByRole('button', { name: /restore/i })).toBeTruthy();
  });

  it('copies the full 32-char identity hash to clipboard on click', async () => {
    const fullHash = 'a1b2c3d4'.repeat(4);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return fullHash;
      throw new Error(`unexpected invoke: ${cmd}`);
    });

    const writeText = vi.fn();
    Object.defineProperty(navigator, 'clipboard', {
      value: { writeText },
      writable: true,
    });

    render(IdentityPanel);
    const hashElement = await screen.findByText(/0xa1b2c3d4/);
    await fireEvent.click(hashElement);

    expect(writeText).toHaveBeenCalledWith(fullHash);
  });

  it('does not throw when clipboard is unavailable', async () => {
    const fullHash = 'a1b2c3d4'.repeat(4);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return fullHash;
      throw new Error(`unexpected invoke: ${cmd}`);
    });

    // Remove clipboard to simulate unavailable API
    Object.defineProperty(navigator, 'clipboard', {
      value: undefined,
      writable: true,
    });

    render(IdentityPanel);
    const hashElement = await screen.findByText(/0xa1b2c3d4/);

    // Should not throw
    await expect(fireEvent.click(hashElement)).resolves.not.toThrow();
  });
});

describe('IdentityPanel — wizard mode toggles', () => {
  beforeEach(() => {
    mockInvoke.mockReset();
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a1b2c3d4'.repeat(4);
      throw new Error(`unexpected invoke: ${cmd}`);
    });
  });

  it('clicking Backup… shows backup type picker and hides default buttons', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xa1b2c3d4/);

    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));

    expect(screen.getByText(/choose backup type/i)).toBeTruthy();
    expect(screen.queryByRole('button', { name: /backup/i })).toBeNull();
    expect(screen.queryByRole('button', { name: /restore/i })).toBeNull();
  });

  it('clicking Restore… shows restore source picker and hides default buttons', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xa1b2c3d4/);

    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));

    expect(screen.getByText(/restore identity/i)).toBeTruthy();
    expect(screen.queryByRole('button', { name: /backup/i })).toBeNull();
    // The Restore button in idle is gone; Continue appears instead
    expect(screen.getByRole('button', { name: /continue/i })).toBeTruthy();
  });

  it('Cancel button in backup type picker returns to idle state', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xa1b2c3d4/);

    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));
    expect(screen.getByText(/choose backup type/i)).toBeTruthy();

    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));

    // Should be back to idle: hash and action buttons visible
    await screen.findByText(/0xa1b2c3d4/);
    expect(screen.getByRole('button', { name: /backup/i })).toBeTruthy();
    expect(screen.getByRole('button', { name: /restore/i })).toBeTruthy();
  });

  it('Cancel button in restore source picker returns to idle state', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xa1b2c3d4/);

    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));
    expect(screen.getByText(/restore identity/i)).toBeTruthy();

    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));

    await screen.findByText(/0xa1b2c3d4/);
    expect(screen.getByRole('button', { name: /backup/i })).toBeTruthy();
    expect(screen.getByRole('button', { name: /restore/i })).toBeTruthy();
  });
});

describe('IdentityPanel — error state', () => {
  beforeEach(() => {
    mockInvoke.mockReset();
  });

  it('shows error message when identity hash cannot be loaded', async () => {
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') throw new Error('identity store locked');
      throw new Error(`unexpected invoke: ${cmd}`);
    });

    render(IdentityPanel);

    await screen.findByText(/could not read identity store/i);
    // Buttons should not be present in error state
    expect(screen.queryByRole('button', { name: /backup/i })).toBeNull();
    expect(screen.queryByRole('button', { name: /restore/i })).toBeNull();
  });
});

describe('Backup wizard — step 1 (type picker)', () => {
  beforeEach(() => {
    mockInvoke.mockReset();
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      throw new Error(`unexpected: ${cmd}`);
    });
  });

  it('shows two options when Backup… is clicked', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);

    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));

    expect(screen.getByLabelText(/24-word recovery phrase/i)).toBeInTheDocument();
    expect(screen.getByLabelText(/encrypted recovery file/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /continue/i })).toBeInTheDocument();
  });

  it('Continue button is disabled until a type is selected', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));

    const continueBtn = screen.getByRole('button', { name: /continue/i });
    expect(continueBtn).toBeDisabled();

    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    expect(continueBtn).not.toBeDisabled();
  });

  it('Cancel returns to idle state', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));

    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));

    await screen.findByText(/0xaaaaaaaa/);
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();
  });

  it('selecting file and clicking Continue transitions to fileEntry phase', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));

    await fireEvent.click(screen.getByLabelText(/encrypted recovery file/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    // Should now be on the file entry screen (Task 6 implementation)
    await screen.findByText(/recovery file passphrase/i);
    expect(screen.queryByText(/choose backup type/i)).not.toBeInTheDocument();
  });
});

describe('Backup wizard — step 2a (mnemonic reveal)', () => {
  beforeEach(() => {
    mockInvoke.mockReset();
  });

  it('fetches words and shows them blurred initially', async () => {
    const words = Array.from({ length: 24 }, (_, i) => `word${i + 1}`);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_mnemonic_words') return words;
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));
    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    await screen.findByText('word1');
    const grid = screen.getByTestId('mnemonic-grid');
    expect(grid).toHaveClass('blurred');

    expect(screen.getByRole('button', { name: /reveal/i })).toBeInTheDocument();
  });

  it('Done is disabled until checkbox ticked AND grid revealed', async () => {
    const words = Array.from({ length: 24 }, (_, i) => `word${i + 1}`);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_mnemonic_words') return words;
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));
    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));
    await screen.findByText('word1');

    const doneBtn = screen.getByRole('button', { name: /done/i });
    expect(doneBtn).toBeDisabled();

    // Reveal first.
    await fireEvent.click(screen.getByRole('button', { name: /reveal/i }));
    expect(doneBtn).toBeDisabled();  // still disabled, checkbox not ticked

    // Tick checkbox.
    await fireEvent.click(screen.getByLabelText(/i've stored this safely/i));
    expect(doneBtn).not.toBeDisabled();
  });

  it('Done returns to idle state', async () => {
    const words = Array.from({ length: 24 }, (_, i) => `word${i + 1}`);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_mnemonic_words') return words;
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));
    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));
    await screen.findByText('word1');

    await fireEvent.click(screen.getByRole('button', { name: /reveal/i }));
    await fireEvent.click(screen.getByLabelText(/i've stored this safely/i));
    await fireEvent.click(screen.getByRole('button', { name: /done/i }));

    await screen.findByText(/0xaaaaaaaa/);
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();
  });

  it('Cancel from mnemonic reveal returns to idle state', async () => {
    const words = Array.from({ length: 24 }, (_, i) => `word${i + 1}`);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_mnemonic_words') return words;
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));
    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));
    await screen.findByText('word1');

    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));

    await screen.findByText(/0xaaaaaaaa/);
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();
  });

  it('shows identity hash anchor on reveal screen', async () => {
    const words = Array.from({ length: 24 }, (_, i) => `word${i + 1}`);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_mnemonic_words') return words;
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));
    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));
    await screen.findByText('word1');

    expect(screen.getByText(/backing up identity/i)).toBeInTheDocument();
    expect(screen.getByText(/0xaaaaaaaa/)).toBeInTheDocument();
  });

  it('shows error message when export_mnemonic_words fails', async () => {
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_mnemonic_words') throw new Error('key not found');
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));
    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    await screen.findByText(/could not load recovery phrase/i);
    expect(screen.getByRole('button', { name: /back to settings/i })).toBeInTheDocument();
  });

  it('grid loses blurred class after Reveal is clicked', async () => {
    const words = Array.from({ length: 24 }, (_, i) => `word${i + 1}`);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_mnemonic_words') return words;
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));
    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));
    await screen.findByText('word1');

    const grid = screen.getByTestId('mnemonic-grid');
    expect(grid).toHaveClass('blurred');

    await fireEvent.click(screen.getByRole('button', { name: /reveal/i }));
    expect(grid).not.toHaveClass('blurred');
  });

  it('cancel during pending export does not resurrect the wizard', async () => {
    // Make the invoke hang via a pending promise we control.
    let resolveExport!: (words: string[]) => void;
    const exportPromise = new Promise<string[]>((resolve) => { resolveExport = resolve; });

    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_mnemonic_words') return exportPromise;
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);

    // Open backup wizard, pick mnemonic, click Continue (starts the pending invoke).
    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));
    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    // Cancel the wizard while invoke is pending.
    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));

    // Sanity-check: idle screen visible right after cancel.
    await screen.findByText(/0xaaaaaaaa/);
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();

    // Now resolve the invoke — wizard should NOT resurrect.
    resolveExport(Array.from({ length: 24 }, (_, i) => `word${i + 1}`));
    // Give Svelte a tick to potentially re-render.
    await new Promise((r) => setTimeout(r, 0));

    // Assert: still on idle screen, no mnemonic-reveal UI present.
    expect(screen.queryByText(/anyone with them can recover/i)).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();
  });
});

// ---------------------------------------------------------------------------
// Task 6: Backup — step 2b (file entry screen)
// ---------------------------------------------------------------------------

/**
 * Helper: navigate to the fileEntry phase.
 * Callers are responsible for setting up mockInvoke BEFORE calling this
 * (at minimum: handle 'current_identity_hash'). This function does NOT
 * overwrite any existing mock setup.
 */
async function arrangeAtFileEntry() {
  render(IdentityPanel);
  await screen.findByText(/0xaaaaaaaa/);
  await fireEvent.click(screen.getByRole('button', { name: /backup/i }));
  await fireEvent.click(screen.getByLabelText(/encrypted recovery file/i));
  await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

  // fileEntry phase should now be rendered
  await screen.findByText(/recovery file passphrase/i);
}

describe('Backup wizard — step 2b (file entry screen)', () => {
  beforeEach(() => {
    mockInvoke.mockReset();
    mockSave.mockReset();
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      throw new Error(`unexpected: ${cmd}`);
    });
  });

  it('renders passphrase fields, confirm field, comment field, and Continue button', async () => {
    await arrangeAtFileEntry();

    expect(screen.getByRole('textbox', { name: /comment/i })).toBeInTheDocument();
    // password fields are not role=textbox; find them by aria-label
    expect(screen.getByLabelText(/^passphrase$/i)).toBeInTheDocument();
    expect(screen.getByLabelText(/confirm passphrase/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /continue/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /cancel/i })).toBeInTheDocument();
  });

  it('Continue is disabled when passphrase is empty', async () => {
    await arrangeAtFileEntry();

    const continueBtn = screen.getByRole('button', { name: /continue/i });
    expect(continueBtn).toBeDisabled();
  });

  it('Continue is disabled when passphrase and confirm do not match', async () => {
    await arrangeAtFileEntry();

    await fireEvent.input(screen.getByLabelText(/^passphrase$/i), { target: { value: 'abc123' } });
    await fireEvent.input(screen.getByLabelText(/confirm passphrase/i), { target: { value: 'xyz789' } });

    expect(screen.getByRole('button', { name: /continue/i })).toBeDisabled();
  });

  it('Continue is enabled when passphrase is non-empty and matches confirm', async () => {
    await arrangeAtFileEntry();

    await fireEvent.input(screen.getByLabelText(/^passphrase$/i), { target: { value: 'hunter2' } });
    await fireEvent.input(screen.getByLabelText(/confirm passphrase/i), { target: { value: 'hunter2' } });

    expect(screen.getByRole('button', { name: /continue/i })).not.toBeDisabled();
  });

  it('two empty passphrases (both equal) still disables Continue', async () => {
    // "" === "" is true but passphrase.length === 0 must keep it disabled
    await arrangeAtFileEntry();
    // Don't type anything
    expect(screen.getByRole('button', { name: /continue/i })).toBeDisabled();
  });

  it('show/hide toggle flips both passphrase inputs between password and text type', async () => {
    await arrangeAtFileEntry();

    const passInput = screen.getByLabelText(/^passphrase$/i);
    const confirmInput = screen.getByLabelText(/confirm passphrase/i);

    // Both start as password
    expect(passInput).toHaveAttribute('type', 'password');
    expect(confirmInput).toHaveAttribute('type', 'password');

    // Click Show toggle
    await fireEvent.click(screen.getByRole('button', { name: /show passphrase/i }));

    expect(passInput).toHaveAttribute('type', 'text');
    expect(confirmInput).toHaveAttribute('type', 'text');

    // Click Hide toggle
    await fireEvent.click(screen.getByRole('button', { name: /hide passphrase/i }));

    expect(passInput).toHaveAttribute('type', 'password');
    expect(confirmInput).toHaveAttribute('type', 'password');
  });

  it('Cancel from file entry returns to idle state', async () => {
    await arrangeAtFileEntry();

    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));

    await screen.findByText(/0xaaaaaaaa/);
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();
  });

  it('displays the identity hash anchor on the file entry screen', async () => {
    await arrangeAtFileEntry();

    expect(screen.getByText(/backing up identity/i)).toBeInTheDocument();
    expect(screen.getByText(/0xaaaaaaaa/)).toBeInTheDocument();
  });
});

// ---------------------------------------------------------------------------
// Task 6: Backup — step 3b save flow
// ---------------------------------------------------------------------------

describe('Backup wizard — step 3b (save recovery file)', () => {
  beforeEach(() => {
    mockInvoke.mockReset();
    mockSave.mockReset();
  });

  it('writes the file and shows the saved path on success', async () => {
    const savePath = '/tmp/identity.recovery';
    mockSave.mockResolvedValue(savePath);
    mockInvoke.mockImplementation(async (cmd: string, args?: unknown) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_recovery_file_to_path') {
        const a = args as { outPath: string; passphrase: string; comment: string | null };
        expect(a.outPath).toBe(savePath);
        expect(a.passphrase).toBe('hunter2');
        expect(a.comment).toBe('laptop-2026-04-15');
        return undefined;
      }
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtFileEntry();

    await fireEvent.input(screen.getByLabelText(/^passphrase$/i), { target: { value: 'hunter2' } });
    await fireEvent.input(screen.getByLabelText(/confirm passphrase/i), { target: { value: 'hunter2' } });
    await fireEvent.input(screen.getByRole('textbox', { name: /comment/i }), { target: { value: 'laptop-2026-04-15' } });
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    // The success text is split across a <p> and a <code>; use a flexible matcher.
    await screen.findByText(/recovery file saved/i);
    expect(screen.getByText('/tmp/identity.recovery')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /done/i })).not.toBeDisabled();
  });

  it('null comment (empty string) passes comment: null to invoke', async () => {
    const savePath = '/tmp/identity.recovery';
    mockSave.mockResolvedValue(savePath);
    let capturedComment: string | null | undefined;
    mockInvoke.mockImplementation(async (cmd: string, args?: unknown) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_recovery_file_to_path') {
        capturedComment = (args as { comment: string | null }).comment;
        return undefined;
      }
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtFileEntry();

    await fireEvent.input(screen.getByLabelText(/^passphrase$/i), { target: { value: 'abc' } });
    await fireEvent.input(screen.getByLabelText(/confirm passphrase/i), { target: { value: 'abc' } });
    // Leave comment empty
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    await screen.findByText(/recovery file saved/i);
    expect(capturedComment).toBeNull();
  });

  it('Done from fileSaved returns to idle', async () => {
    const savePath = '/tmp/identity.recovery';
    mockSave.mockResolvedValue(savePath);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_recovery_file_to_path') return undefined;
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtFileEntry();

    await fireEvent.input(screen.getByLabelText(/^passphrase$/i), { target: { value: 'pass' } });
    await fireEvent.input(screen.getByLabelText(/confirm passphrase/i), { target: { value: 'pass' } });
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));
    await screen.findByText(/recovery file saved/i);

    await fireEvent.click(screen.getByRole('button', { name: /done/i }));

    await screen.findByText(/0xaaaaaaaa/);
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();
  });

  it('dialog cancel (null path) silently returns to file entry step', async () => {
    mockSave.mockResolvedValue(null); // user cancelled the dialog
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_recovery_file_to_path') throw new Error('should not be called');
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtFileEntry();

    await fireEvent.input(screen.getByLabelText(/^passphrase$/i), { target: { value: 'pass' } });
    await fireEvent.input(screen.getByLabelText(/confirm passphrase/i), { target: { value: 'pass' } });
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    // After dialog cancel: should stay on fileEntry (no error, no success)
    await waitFor(() => {
      expect(screen.getByText(/recovery file passphrase/i)).toBeInTheDocument();
    });
    expect(screen.queryByText(/wrote/i)).not.toBeInTheDocument();
  });

  it('invoke failure shows error screen with Back and Cancel buttons', async () => {
    const savePath = '/tmp/identity.recovery';
    mockSave.mockResolvedValue(savePath);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_recovery_file_to_path') throw new Error('disk full');
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtFileEntry();

    await fireEvent.input(screen.getByLabelText(/^passphrase$/i), { target: { value: 'pass' } });
    await fireEvent.input(screen.getByLabelText(/confirm passphrase/i), { target: { value: 'pass' } });
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    await screen.findByText(/could not save to/i);
    expect(screen.getByRole('button', { name: /back/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /cancel/i })).toBeInTheDocument();
  });

  it('Back from fileSaveError returns to fileEntry step', async () => {
    const savePath = '/tmp/identity.recovery';
    mockSave.mockResolvedValue(savePath);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_recovery_file_to_path') throw new Error('disk full');
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtFileEntry();

    await fireEvent.input(screen.getByLabelText(/^passphrase$/i), { target: { value: 'pass' } });
    await fireEvent.input(screen.getByLabelText(/confirm passphrase/i), { target: { value: 'pass' } });
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));
    await screen.findByText(/could not save to/i);

    await fireEvent.click(screen.getByRole('button', { name: /back/i }));

    await screen.findByText(/recovery file passphrase/i);
    expect(screen.getByRole('button', { name: /continue/i })).toBeInTheDocument();
  });

  it('Cancel from fileSaveError returns to idle', async () => {
    const savePath = '/tmp/identity.recovery';
    mockSave.mockResolvedValue(savePath);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_recovery_file_to_path') throw new Error('disk full');
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtFileEntry();

    await fireEvent.input(screen.getByLabelText(/^passphrase$/i), { target: { value: 'pass' } });
    await fireEvent.input(screen.getByLabelText(/confirm passphrase/i), { target: { value: 'pass' } });
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));
    await screen.findByText(/could not save to/i);

    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));

    await screen.findByText(/0xaaaaaaaa/);
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();
  });

  // Race regression: cancel during pending file write must not resurrect the wizard.
  // Mirrors the pattern from Task 5's "cancel during pending export" test.
  it('cancel during pending file write does not resurrect the wizard', async () => {
    const savePath = '/tmp/identity.recovery';
    // save() dialog resolves immediately with a path.
    mockSave.mockResolvedValue(savePath);

    // Make the file write hang via a promise we control.
    let resolveWrite!: () => void;
    const writePromise = new Promise<void>((resolve) => { resolveWrite = resolve; });

    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_recovery_file_to_path') return writePromise;
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtFileEntry();

    await fireEvent.input(screen.getByLabelText(/^passphrase$/i), { target: { value: 'pass' } });
    await fireEvent.input(screen.getByLabelText(/confirm passphrase/i), { target: { value: 'pass' } });

    // Click Continue — save dialog resolves immediately, then file write starts (pending).
    // The component is still in fileEntry state while the write is pending.
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    // Cancel while write is pending. The Cancel button is still visible since
    // fileEntry is still rendered (no transition until write completes).
    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));

    // Sanity-check: idle screen visible right after cancel.
    await screen.findByText(/0xaaaaaaaa/);
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();

    // Now resolve the write — wizard should NOT transition to fileSaved.
    resolveWrite();
    await new Promise((r) => setTimeout(r, 0));

    // Assert: still on idle, no success screen.
    expect(screen.queryByText(/recovery file saved/i)).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();
  });
});

// ---------------------------------------------------------------------------
// Task 7: Restore wizard — step 1 (pickSource)
// ---------------------------------------------------------------------------

describe('Restore wizard — step 1 (pickSource)', () => {
  beforeEach(() => {
    mockInvoke.mockReset();
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      throw new Error(`unexpected: ${cmd}`);
    });
  });

  it('shows two source options and a disabled Continue button on open', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);

    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));

    expect(screen.getByLabelText(/24-word recovery phrase/i)).toBeInTheDocument();
    expect(screen.getByLabelText(/recovery file/i)).toBeInTheDocument();
    const continueBtn = screen.getByRole('button', { name: /continue/i });
    expect(continueBtn).toBeDisabled();
  });

  it('Continue becomes enabled after selecting "24-word recovery phrase"', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));

    const continueBtn = screen.getByRole('button', { name: /continue/i });
    expect(continueBtn).toBeDisabled();

    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    expect(continueBtn).not.toBeDisabled();
  });

  it('Continue becomes enabled after selecting "Recovery file"', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));

    const continueBtn = screen.getByRole('button', { name: /continue/i });
    expect(continueBtn).toBeDisabled();

    await fireEvent.click(screen.getByLabelText(/recovery file/i));
    expect(continueBtn).not.toBeDisabled();
  });

  it('Cancel from pickSource returns to idle', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));

    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));

    await screen.findByText(/0xaaaaaaaa/);
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /restore/i })).toBeInTheDocument();
  });

  it('Cancel from pickSource clears prior selection on re-open', async () => {
    // Regression: resetToIdle() must clear selectedRestoreSource, otherwise
    // the radio stays checked and Continue is enabled when the user re-opens
    // the wizard without explicitly picking a source.
    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);

    // First entry: pick file, then cancel.
    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));
    await fireEvent.click(screen.getByLabelText(/recovery file/i));
    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));
    await screen.findByText(/0xaaaaaaaa/);

    // Re-open: no radio should be checked, Continue should be disabled.
    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));
    const fileRadio = screen.getByLabelText(/recovery file/i) as HTMLInputElement;
    const mnemonicRadio = screen.getByLabelText(/24-word recovery phrase/i) as HTMLInputElement;
    expect(fileRadio.checked).toBe(false);
    expect(mnemonicRadio.checked).toBe(false);
    expect(screen.getByRole('button', { name: /continue/i })).toBeDisabled();
  });

  it('selecting mnemonic and clicking Continue transitions to mnemonicEntry placeholder', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));

    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    await screen.findByText(/enter recovery phrase/i);
    expect(screen.queryByText(/restore identity/i)).not.toBeInTheDocument();
  });

  it('selecting file and clicking Continue transitions to fileEntry placeholder', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));

    await fireEvent.click(screen.getByLabelText(/recovery file/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    await screen.findByText(/select recovery file/i);
    expect(screen.queryByText(/restore identity/i)).not.toBeInTheDocument();
  });
});

// ---------------------------------------------------------------------------
// Task 7: Restore wizard — step 3 (confirm) — needs Tasks 8/9 to drive into;
// using it.skip per plan Option B. Un-skip at end of Task 8.
// ---------------------------------------------------------------------------

describe('Restore wizard — step 3 (confirm)', () => {
  beforeEach(() => {
    mockInvoke.mockReset();
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a1b2c3d4'.repeat(8);
      throw new Error(`unexpected: ${cmd}`);
    });
  });

  it.skip('renders hash diff, type-to-confirm input, and disabled Replace identity button (needs Task 8/9 to navigate here)', async () => {
    // Navigate via Task 8's mnemonic entry → confirm transition.
    // Un-skip when Task 8 is implemented.
  });

  it.skip('Replace identity is disabled until typedPrefix matches current hash prefix (needs Task 8/9)', async () => {
    // Un-skip when Task 8 is implemented.
  });

  it.skip('inline error shown when typedPrefix is non-empty but mismatching (needs Task 8/9)', async () => {
    // Un-skip when Task 8 is implemented.
  });

  it.skip('Cancel from confirm returns to idle (needs Task 8/9)', async () => {
    // Un-skip when Task 8 is implemented.
  });

  it.skip('Replace identity (mnemonic) invokes restore_mnemonic_from_words and transitions to done (needs Task 8)', async () => {
    // Un-skip when Task 8 is implemented.
  });

  it.skip('Replace identity (file) invokes restore_recovery_file_from_path and transitions to done (needs Task 9)', async () => {
    // Un-skip when Task 9 is implemented.
  });

  it.skip('invoke error transitions to commitError (needs Task 8/9)', async () => {
    // Un-skip when Task 8 is implemented.
  });

  it.skip('race guard: cancel while commit invoke pending does not resurrect wizard (needs Task 8/9)', async () => {
    // Un-skip when Task 8 is implemented.
  });
});

// ---------------------------------------------------------------------------
// Task 7: Restore wizard — commitError
// ---------------------------------------------------------------------------

describe('Restore wizard — commitError', () => {
  beforeEach(() => {
    mockInvoke.mockReset();
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      throw new Error(`unexpected: ${cmd}`);
    });
  });

  it.skip('shows error message with Back and Cancel buttons (needs Task 8/9 to navigate here)', async () => {
    // The commitError variant is reachable only after the confirm step, which
    // requires Tasks 8/9 to build the entry path. Un-skip after Task 8.
  });

  it.skip('Back from commitError returns to pickSource (needs Task 8/9)', async () => {
    // Un-skip after Task 8.
  });

  it.skip('Cancel from commitError returns to idle (needs Task 8/9)', async () => {
    // Un-skip after Task 8.
  });
});

// ---------------------------------------------------------------------------
// Task 7: Restore wizard — step 4 (done)
// ---------------------------------------------------------------------------

describe('Restore wizard — step 4 (done)', () => {
  beforeEach(() => {
    mockInvoke.mockReset();
  });

  it.skip('shows new identity hash prefix with click-to-copy and Done button (needs Task 8/9 to navigate here)', async () => {
    // The done step is reachable only after confirm, which requires Tasks 8/9.
    // Un-skip when Task 8 is implemented.
  });

  it.skip('Done button refreshes fullHash via current_identity_hash and returns to idle (needs Task 8/9)', async () => {
    // Un-skip when Task 8 is implemented.
  });

  it.skip('race guard: cancel between done render and Done click does not double-transition (needs Task 8/9)', async () => {
    // Un-skip when Task 8 is implemented.
  });
});
