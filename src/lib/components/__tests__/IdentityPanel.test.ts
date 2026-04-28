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

  it('clicking Restore… shows restore placeholder and hides default buttons', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xa1b2c3d4/);

    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));

    expect(screen.getByText(/restore wizard placeholder/i)).toBeTruthy();
    expect(screen.queryByRole('button', { name: /backup/i })).toBeNull();
    expect(screen.queryByRole('button', { name: /restore/i })).toBeNull();
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

  it('Back button in restore placeholder returns to idle state', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xa1b2c3d4/);

    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));
    expect(screen.getByText(/restore wizard placeholder/i)).toBeTruthy();

    await fireEvent.click(screen.getByRole('button', { name: /← back/i }));

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
