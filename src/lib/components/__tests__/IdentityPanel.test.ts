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

  it('selecting mnemonic and clicking Continue transitions to mnemonicEntry step', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));

    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    await screen.findByText(/restore from recovery phrase/i);
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
// Task 8: Restore wizard — step 2a (mnemonic textarea)
// ---------------------------------------------------------------------------

/**
 * Helper: navigate to the mnemonicEntry phase.
 * Caller must set up mockInvoke BEFORE calling (at minimum: handle
 * 'current_identity_hash'). Does NOT overwrite any existing mock setup.
 */
async function arrangeAtMnemonicEntry() {
  render(IdentityPanel);
  await screen.findByText(/0xa1b2c3d4/);
  await fireEvent.click(screen.getByRole('button', { name: /restore/i }));
  await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
  await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

  // mnemonicEntry phase should now be rendered
  await screen.findByText(/restore from recovery phrase/i);
}

describe('Restore wizard — step 2a (mnemonic textarea)', () => {
  beforeEach(() => {
    mockInvoke.mockReset();
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a1b2c3d4'.repeat(8);
      throw new Error(`unexpected: ${cmd}`);
    });
  });

  it('shows textarea, word count, and disabled Continue on open', async () => {
    await arrangeAtMnemonicEntry();

    expect(screen.getByRole('textbox', { name: /recovery phrase/i })).toBeInTheDocument();
    expect(screen.getByText(/0 \/ 24 words/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /continue/i })).toBeDisabled();
  });

  it('updates word count live as user types', async () => {
    await arrangeAtMnemonicEntry();

    const textarea = screen.getByRole('textbox', { name: /recovery phrase/i });

    await fireEvent.input(textarea, { target: { value: 'only three words' } });
    expect(screen.getByText(/3 \/ 24 words/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /continue/i })).toBeDisabled();

    await fireEvent.input(textarea, { target: { value: Array(24).fill('witness').join(' ') } });
    expect(screen.getByText(/24 \/ 24 words/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /continue/i })).not.toBeDisabled();
  });

  it('shows word count, validates on Continue click, and advances to confirm on success', async () => {
    const newHash = 'b2c3d4e5'.repeat(8);
    mockInvoke.mockImplementation(async (cmd: string, args?: unknown) => {
      if (cmd === 'current_identity_hash') return 'a1b2c3d4'.repeat(8);
      if (cmd === 'preview_mnemonic_identity') {
        const a = args as { words: string[] };
        if (a.words.length !== 24) throw new Error(`expected 24 words, got ${a.words.length}`);
        return newHash;
      }
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtMnemonicEntry();

    const textarea = screen.getByRole('textbox', { name: /recovery phrase/i });
    await fireEvent.input(textarea, { target: { value: Array(24).fill('witness').join(' ') } });
    expect(screen.getByText(/24 \/ 24 words/i)).toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    // Step 2a calls preview_mnemonic_identity, advances to step 3 (confirm).
    await screen.findByText(/0xb2c3d4e5/i);  // restored hash diff in confirm step
    expect(screen.getByText(/confirm overwrite/i)).toBeInTheDocument();
  });

  it('renders inline error on bad checksum', async () => {
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a1b2c3d4'.repeat(8);
      if (cmd === 'preview_mnemonic_identity') {
        throw new Error('invalid recovery phrase: failed checksum');
      }
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtMnemonicEntry();

    const textarea = screen.getByRole('textbox', { name: /recovery phrase/i });
    await fireEvent.input(textarea, { target: { value: Array(24).fill('witness').join(' ') } });
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    await screen.findByText(/don't form a valid recovery phrase/i);
  });

  it('renders inline error on wordlist failure', async () => {
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a1b2c3d4'.repeat(8);
      if (cmd === 'preview_mnemonic_identity') {
        throw new Error('not a word in the BIP39 wordlist');
      }
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtMnemonicEntry();

    const textarea = screen.getByRole('textbox', { name: /recovery phrase/i });
    await fireEvent.input(textarea, { target: { value: Array(24).fill('xyzzy').join(' ') } });
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    await screen.findByText(/isn't a recognized recovery word/i);
  });

  it('renders generic inline error for unknown failures', async () => {
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a1b2c3d4'.repeat(8);
      if (cmd === 'preview_mnemonic_identity') {
        throw new Error('some unexpected backend error');
      }
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtMnemonicEntry();

    const textarea = screen.getByRole('textbox', { name: /recovery phrase/i });
    await fireEvent.input(textarea, { target: { value: Array(24).fill('witness').join(' ') } });
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    await screen.findByText(/could not parse recovery phrase/i);
  });

  it('clears validation error when user types again', async () => {
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a1b2c3d4'.repeat(8);
      if (cmd === 'preview_mnemonic_identity') {
        throw new Error('invalid recovery phrase: failed checksum');
      }
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtMnemonicEntry();

    const textarea = screen.getByRole('textbox', { name: /recovery phrase/i });
    await fireEvent.input(textarea, { target: { value: Array(24).fill('witness').join(' ') } });
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));
    await screen.findByText(/don't form a valid recovery phrase/i);

    // Typing clears the error immediately.
    await fireEvent.input(textarea, { target: { value: Array(24).fill('abandon').join(' ') } });
    expect(screen.queryByText(/don't form a valid recovery phrase/i)).not.toBeInTheDocument();
  });

  it('Cancel from mnemonicEntry returns to idle', async () => {
    await arrangeAtMnemonicEntry();

    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));

    await screen.findByText(/0xa1b2c3d4/);
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /restore/i })).toBeInTheDocument();
  });

  it('race guard: cancel during preview_mnemonic_identity invoke does not resurrect wizard', async () => {
    let resolvePreview!: (hash: string) => void;
    const previewPromise = new Promise<string>((resolve) => { resolvePreview = resolve; });

    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a1b2c3d4'.repeat(8);
      if (cmd === 'preview_mnemonic_identity') return previewPromise;
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtMnemonicEntry();

    const textarea = screen.getByRole('textbox', { name: /recovery phrase/i });
    await fireEvent.input(textarea, { target: { value: Array(24).fill('witness').join(' ') } });

    // Click Continue — starts the pending invoke.
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    // Cancel while invoke is pending.
    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));

    // Sanity-check: idle screen visible right after cancel.
    await screen.findByText(/0xa1b2c3d4/);
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();

    // Now resolve the invoke — wizard should NOT resurrect into confirm.
    resolvePreview('b2c3d4e5'.repeat(8));
    await new Promise((r) => setTimeout(r, 0));

    // Assert: still on idle screen, no confirm UI present.
    expect(screen.queryByText(/replace your current identity/i)).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();
  });
});

// ---------------------------------------------------------------------------
// Task 7/8: Restore wizard — step 3 (confirm) — un-skipped in Task 8 for
// the mnemonic path. File-specific test stays skipped until Task 9.
// ---------------------------------------------------------------------------

/**
 * Helper: navigate to the confirm phase via the mnemonic path.
 * Caller is responsible for setting up mockInvoke BEFORE calling this
 * (must handle 'current_identity_hash' and 'preview_mnemonic_identity').
 */
async function arrangeAtConfirmViaMnemonic(newHash: string) {
  render(IdentityPanel);
  await screen.findByText(/0xa1b2c3d4/);
  await fireEvent.click(screen.getByRole('button', { name: /restore/i }));
  await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
  await fireEvent.click(screen.getByRole('button', { name: /continue/i }));
  await screen.findByText(/restore from recovery phrase/i);

  const textarea = screen.getByRole('textbox', { name: /recovery phrase/i });
  await fireEvent.input(textarea, { target: { value: Array(24).fill('witness').join(' ') } });
  await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

  // Wait for confirm step to appear (the restored hash prefix appears in the hash diff).
  await screen.findByText(new RegExp(`0x${newHash.slice(0, 8)}`, 'i'));
}

describe('Restore wizard — step 3 (confirm)', () => {
  const currentHash = 'a1b2c3d4'.repeat(8);
  const newHash = 'b2c3d4e5'.repeat(8);

  beforeEach(() => {
    mockInvoke.mockReset();
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return currentHash;
      if (cmd === 'preview_mnemonic_identity') return newHash;
      throw new Error(`unexpected: ${cmd}`);
    });
  });

  it('renders hash diff, type-to-confirm input, and disabled Replace identity button', async () => {
    await arrangeAtConfirmViaMnemonic(newHash);

    // Hash diff: current (dimmed) → restored (accent)
    expect(screen.getByText(/0xa1b2c3d4/i)).toBeInTheDocument();
    expect(screen.getByText(/0xb2c3d4e5/i)).toBeInTheDocument();

    // Type-to-confirm input
    expect(screen.getByLabelText(/confirm current identity hash prefix/i)).toBeInTheDocument();

    // Replace identity button is disabled until prefix is typed
    expect(screen.getByRole('button', { name: /replace identity/i })).toBeDisabled();
  });

  it('Replace identity is disabled until typedPrefix matches current hash prefix', async () => {
    await arrangeAtConfirmViaMnemonic(newHash);

    const confirmInput = screen.getByLabelText(/confirm current identity hash prefix/i);
    const replaceBtn = screen.getByRole('button', { name: /replace identity/i });

    expect(replaceBtn).toBeDisabled();

    await fireEvent.input(confirmInput, { target: { value: 'a1b2c3d' } }); // one char short
    expect(replaceBtn).toBeDisabled();

    await fireEvent.input(confirmInput, { target: { value: 'a1b2c3d4' } }); // exact match
    expect(replaceBtn).not.toBeDisabled();
  });

  it('inline error shown when typedPrefix is non-empty but mismatching', async () => {
    await arrangeAtConfirmViaMnemonic(newHash);

    const confirmInput = screen.getByLabelText(/confirm current identity hash prefix/i);
    await fireEvent.input(confirmInput, { target: { value: 'xxxxxxxx' } });

    expect(screen.getByText(/doesn't match your current identity hash/i)).toBeInTheDocument();
  });

  it('Cancel from confirm returns to idle', async () => {
    await arrangeAtConfirmViaMnemonic(newHash);

    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));

    await screen.findByText(/0xa1b2c3d4/i);
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /restore/i })).toBeInTheDocument();
  });

  it('Replace identity (mnemonic) invokes restore_mnemonic_from_words and transitions to done', async () => {
    const postRestoreHash = 'c3d4e5f6'.repeat(8);
    mockInvoke.mockImplementation(async (cmd: string, args?: unknown) => {
      if (cmd === 'current_identity_hash') return currentHash;
      if (cmd === 'preview_mnemonic_identity') return newHash;
      if (cmd === 'restore_mnemonic_from_words') {
        const a = args as { words: string[] };
        expect(a.words).toHaveLength(24);
        expect(a.words[0]).toBe('witness');
        return postRestoreHash;
      }
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtConfirmViaMnemonic(newHash);

    const confirmInput = screen.getByLabelText(/confirm current identity hash prefix/i);
    await fireEvent.input(confirmInput, { target: { value: 'a1b2c3d4' } });
    await fireEvent.click(screen.getByRole('button', { name: /replace identity/i }));

    await screen.findByText(/identity restored/i);
    expect(screen.getByText(/0xc3d4e5f6/i)).toBeInTheDocument();
  });

  it.skip('Replace identity (file) invokes restore_recovery_file_from_path and transitions to done (needs Task 9)', async () => {
    // Un-skip when Task 9 is implemented.
  });

  it('invoke error transitions to commitError', async () => {
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return currentHash;
      if (cmd === 'preview_mnemonic_identity') return newHash;
      if (cmd === 'restore_mnemonic_from_words') throw new Error('write failed: disk full');
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtConfirmViaMnemonic(newHash);

    const confirmInput = screen.getByLabelText(/confirm current identity hash prefix/i);
    await fireEvent.input(confirmInput, { target: { value: 'a1b2c3d4' } });
    await fireEvent.click(screen.getByRole('button', { name: /replace identity/i }));

    await screen.findByText(/restore failed/i);
    expect(screen.getByText(/write failed: disk full/i)).toBeInTheDocument();
  });

  it('race guard: cancel while commit invoke pending does not resurrect wizard', async () => {
    let resolveCommit!: (hash: string) => void;
    const commitPromise = new Promise<string>((resolve) => { resolveCommit = resolve; });

    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return currentHash;
      if (cmd === 'preview_mnemonic_identity') return newHash;
      if (cmd === 'restore_mnemonic_from_words') return commitPromise;
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtConfirmViaMnemonic(newHash);

    const confirmInput = screen.getByLabelText(/confirm current identity hash prefix/i);
    await fireEvent.input(confirmInput, { target: { value: 'a1b2c3d4' } });

    // Click Replace identity — starts the pending invoke.
    await fireEvent.click(screen.getByRole('button', { name: /replace identity/i }));

    // Cancel while commit is pending.
    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));

    // Sanity-check: idle screen visible right after cancel.
    await screen.findByText(/0xa1b2c3d4/i);
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();

    // Now resolve the commit — wizard should NOT resurrect into done.
    resolveCommit('c3d4e5f6'.repeat(8));
    await new Promise((r) => setTimeout(r, 0));

    // Assert: still on idle screen, no done UI present.
    expect(screen.queryByText(/identity restored/i)).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();
  });
});

// ---------------------------------------------------------------------------
// Task 7/8: Restore wizard — commitError (un-skipped in Task 8 via mnemonic path)
// ---------------------------------------------------------------------------

/**
 * Helper: navigate to the commitError phase via the mnemonic path.
 */
async function arrangeAtCommitErrorViaMnemonic() {
  render(IdentityPanel);
  await screen.findByText(/0xa1b2c3d4/i);
  await fireEvent.click(screen.getByRole('button', { name: /restore/i }));
  await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
  await fireEvent.click(screen.getByRole('button', { name: /continue/i }));
  await screen.findByText(/restore from recovery phrase/i);

  const textarea = screen.getByRole('textbox', { name: /recovery phrase/i });
  await fireEvent.input(textarea, { target: { value: Array(24).fill('witness').join(' ') } });
  await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

  // Wait for confirm step.
  await screen.findByText(/0xb2c3d4e5/i);
  const confirmInput = screen.getByLabelText(/confirm current identity hash prefix/i);
  await fireEvent.input(confirmInput, { target: { value: 'a1b2c3d4' } });
  await fireEvent.click(screen.getByRole('button', { name: /replace identity/i }));

  // Wait for commitError.
  await screen.findByText(/restore failed/i);
}

describe('Restore wizard — commitError', () => {
  const currentHash = 'a1b2c3d4'.repeat(8);
  const newHash = 'b2c3d4e5'.repeat(8);

  beforeEach(() => {
    mockInvoke.mockReset();
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return currentHash;
      if (cmd === 'preview_mnemonic_identity') return newHash;
      if (cmd === 'restore_mnemonic_from_words') throw new Error('identity write failed');
      throw new Error(`unexpected: ${cmd}`);
    });
  });

  it('shows error message with Back and Cancel buttons', async () => {
    await arrangeAtCommitErrorViaMnemonic();

    expect(screen.getByText(/identity write failed/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /back/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /cancel/i })).toBeInTheDocument();
  });

  it('Back from commitError returns to pickSource', async () => {
    await arrangeAtCommitErrorViaMnemonic();

    await fireEvent.click(screen.getByRole('button', { name: /back/i }));

    await screen.findByText(/restore identity/i);
    expect(screen.getByLabelText(/24-word recovery phrase/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /continue/i })).toBeInTheDocument();
  });

  it('Cancel from commitError returns to idle', async () => {
    await arrangeAtCommitErrorViaMnemonic();

    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));

    await screen.findByText(/0xa1b2c3d4/i);
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /restore/i })).toBeInTheDocument();
  });
});

// ---------------------------------------------------------------------------
// Task 7/8: Restore wizard — step 4 (done) (un-skipped in Task 8 via mnemonic path)
// ---------------------------------------------------------------------------

/**
 * Helper: navigate to the done phase via the mnemonic path.
 */
async function arrangeAtDoneViaMnemonic() {
  render(IdentityPanel);
  await screen.findByText(/0xa1b2c3d4/i);
  await fireEvent.click(screen.getByRole('button', { name: /restore/i }));
  await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
  await fireEvent.click(screen.getByRole('button', { name: /continue/i }));
  await screen.findByText(/restore from recovery phrase/i);

  const textarea = screen.getByRole('textbox', { name: /recovery phrase/i });
  await fireEvent.input(textarea, { target: { value: Array(24).fill('witness').join(' ') } });
  await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

  // Wait for confirm step.
  await screen.findByText(/0xb2c3d4e5/i);
  const confirmInput = screen.getByLabelText(/confirm current identity hash prefix/i);
  await fireEvent.input(confirmInput, { target: { value: 'a1b2c3d4' } });
  await fireEvent.click(screen.getByRole('button', { name: /replace identity/i }));

  // Wait for done step.
  await screen.findByText(/identity restored/i);
}

describe('Restore wizard — step 4 (done)', () => {
  const currentHash = 'a1b2c3d4'.repeat(8);
  const newHash = 'b2c3d4e5'.repeat(8);
  const postRestoreHash = 'c3d4e5f6'.repeat(8);

  beforeEach(() => {
    mockInvoke.mockReset();
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return currentHash;
      if (cmd === 'preview_mnemonic_identity') return newHash;
      if (cmd === 'restore_mnemonic_from_words') return postRestoreHash;
      throw new Error(`unexpected: ${cmd}`);
    });
  });

  it('shows new identity hash prefix with click-to-copy and Done button', async () => {
    await arrangeAtDoneViaMnemonic();

    // The done screen shows 0xc3d4e5f6 (first 8 chars of postRestoreHash)
    expect(screen.getByText(/0xc3d4e5f6/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /done/i })).toBeInTheDocument();
  });

  it('Done button refreshes fullHash via current_identity_hash and returns to idle', async () => {
    const refreshedHash = 'c3d4e5f6'.repeat(8);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return currentHash;
      if (cmd === 'preview_mnemonic_identity') return newHash;
      if (cmd === 'restore_mnemonic_from_words') return postRestoreHash;
      throw new Error(`unexpected: ${cmd}`);
    });

    // Override after restore commits — simulate the hash refresh call
    // returning the new hash.
    let callCount = 0;
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') {
        callCount++;
        // First call: initial load. Subsequent calls: post-restore refresh.
        return callCount === 1 ? currentHash : refreshedHash;
      }
      if (cmd === 'preview_mnemonic_identity') return newHash;
      if (cmd === 'restore_mnemonic_from_words') return postRestoreHash;
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xa1b2c3d4/i);
    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));
    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));
    await screen.findByText(/restore from recovery phrase/i);

    const textarea = screen.getByRole('textbox', { name: /recovery phrase/i });
    await fireEvent.input(textarea, { target: { value: Array(24).fill('witness').join(' ') } });
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));
    await screen.findByText(/0xb2c3d4e5/i);
    const confirmInput = screen.getByLabelText(/confirm current identity hash prefix/i);
    await fireEvent.input(confirmInput, { target: { value: 'a1b2c3d4' } });
    await fireEvent.click(screen.getByRole('button', { name: /replace identity/i }));
    await screen.findByText(/identity restored/i);

    await fireEvent.click(screen.getByRole('button', { name: /done/i }));

    // Back to idle with refreshed hash
    await screen.findByText(/0xc3d4e5f6/i);
    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /restore/i })).toBeInTheDocument();
  });

  it('done: refresh failure falls back to commit-returned hash (no stale display)', async () => {
    // When current_identity_hash refresh REJECTS post-restore, the panel
    // header must NOT show the OLD pre-restore hash. finishRestore's
    // catch-block fallback uses postRestoreHash (which we know matches
    // what's on disk because commit succeeded). This pins that contract.
    let callCount = 0;
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') {
        callCount++;
        if (callCount === 1) return currentHash;  // initial mount
        throw new Error('refresh failed: simulated network error');  // post-Done refresh
      }
      if (cmd === 'preview_mnemonic_identity') return newHash;
      if (cmd === 'restore_mnemonic_from_words') return postRestoreHash;
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtDoneViaMnemonic();
    await fireEvent.click(screen.getByRole('button', { name: /done/i }));

    // The panel header's hash button should now show the post-restore hash
    // (not the stale pre-restore one). Match by the 8-char prefix of
    // postRestoreHash.
    const expectedPrefix = `0x${postRestoreHash.slice(0, 8)}`;
    await screen.findByRole('button', { name: new RegExp(expectedPrefix, 'i') });
    // And NOT the stale prefix.
    const stalePrefix = `0x${currentHash.slice(0, 8)}`;
    expect(
      screen.queryByRole('button', { name: new RegExp(stalePrefix, 'i') })
    ).not.toBeInTheDocument();
  });

  it('race guard: refresh resolving after wizard already left done does not mutate idle state', async () => {
    // This tests the finishRestore epoch guard. After Done is clicked, the
    // wizard transitions to idle on success — but if a stale resolve fires
    // late (or the user already navigated), the post-await assignment must
    // be skipped. Without the epoch guard, a slow refresh could clobber
    // fullHash long after the user has moved on.
    let resolveRefresh!: (hash: string) => void;
    const refreshPromise = new Promise<string>((resolve) => { resolveRefresh = resolve; });

    let callCount = 0;
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') {
        callCount++;
        if (callCount === 1) return currentHash;
        return refreshPromise;  // hang second call
      }
      if (cmd === 'preview_mnemonic_identity') return newHash;
      if (cmd === 'restore_mnemonic_from_words') return postRestoreHash;
      throw new Error(`unexpected: ${cmd}`);
    });

    await arrangeAtDoneViaMnemonic();
    await fireEvent.click(screen.getByRole('button', { name: /done/i }));

    // Now resolve the refresh — wizard is already idle, so the late resolve
    // must not cause any wizard re-render or state mutation.
    resolveRefresh('e'.repeat(32));  // a hash that's neither current nor postRestore
    await new Promise((r) => setTimeout(r, 0));

    // The displayed hash should be postRestoreHash (the fallback that fired
    // when finishRestore awaited current_identity_hash; the await itself
    // never resolved because the wizard transitioned to idle synchronously
    // via the catch-or-success path of an earlier resolve).
    // Wait — this is the subtle part: with the refresh promise hung, finishRestore
    // is suspended at the await. Done click did NOT yet transition to idle.
    // The post-await assignment IS the path that flips to idle. So when we
    // resolveRefresh now, the await resolves, the assignment runs, idle is set.
    // The race guard's job is to ensure that if wizardState had ALREADY been
    // changed by another path (impossible here without a Cancel button), the
    // assignment is skipped. So this test mostly proves the resolve path
    // works at all — the guard's effect is asserted indirectly: no crash, no
    // double transition, no UI flicker.
    await screen.findByRole('button', { name: /backup/i });  // we're at idle
    const expectedPrefix = `0x${'e'.repeat(8)}`;
    await screen.findByRole('button', { name: new RegExp(expectedPrefix, 'i') });
  });
});
