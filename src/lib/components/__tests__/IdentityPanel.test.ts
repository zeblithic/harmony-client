import { render, screen, fireEvent } from '@testing-library/svelte';
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

  it('clicking Backup… shows backup placeholder and hides default buttons', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xa1b2c3d4/);

    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));

    expect(screen.getByText(/backup wizard placeholder/i)).toBeTruthy();
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

  it('Back button in backup placeholder returns to idle state', async () => {
    render(IdentityPanel);
    await screen.findByText(/0xa1b2c3d4/);

    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));
    expect(screen.getByText(/backup wizard placeholder/i)).toBeTruthy();

    await fireEvent.click(screen.getByRole('button', { name: /← back/i }));

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
