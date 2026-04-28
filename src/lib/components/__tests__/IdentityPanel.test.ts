import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import IdentityPanel from '../IdentityPanel.svelte';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));

const mockInvoke = vi.mocked(invoke);

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

    // Task 6 placeholder should be rendered (fileEntry phase exists, Task 4 placeholder covers it)
    await waitFor(() => {
      expect(screen.queryByRole('button', { name: /continue/i })).not.toBeInTheDocument();
    });
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
