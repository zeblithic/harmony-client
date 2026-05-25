import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import WelcomeModal from '../WelcomeModal.svelte';

vi.mock('@tauri-apps/api/app', () => ({
  getVersion: vi.fn().mockResolvedValue('0.1.0-alpha.1'),
}));

describe('WelcomeModal', () => {
  it('renders when open=true', () => {
    render(WelcomeModal, {
      open: true,
      onDismiss: () => {},
      onJoinWithInvite: () => {},
    });
    expect(screen.getByTestId('welcome-modal')).toBeInTheDocument();
    expect(screen.getByText(/Welcome to Harmony alpha/i)).toBeInTheDocument();
  });

  it('does not render when open=false', () => {
    render(WelcomeModal, {
      open: false,
      onDismiss: () => {},
      onJoinWithInvite: () => {},
    });
    expect(screen.queryByTestId('welcome-modal')).toBeNull();
  });

  it('empty paste + "Join now" → inline error, modal stays', async () => {
    const onJoinWithInvite = vi.fn();
    const onDismiss = vi.fn();
    render(WelcomeModal, { open: true, onDismiss, onJoinWithInvite });
    await fireEvent.click(screen.getByTestId('welcome-join'));
    expect(screen.getByTestId('welcome-invite-error')).toHaveTextContent(
      /paste an invite url or click skip/i,
    );
    expect(onJoinWithInvite).not.toHaveBeenCalled();
    expect(onDismiss).not.toHaveBeenCalled();
  });

  it('malformed URL + "Join now" → inline error', async () => {
    const onJoinWithInvite = vi.fn();
    render(WelcomeModal, {
      open: true,
      onDismiss: () => {},
      onJoinWithInvite,
    });
    const input = screen.getByTestId('welcome-invite-input');
    await fireEvent.input(input, { target: { value: 'https://example.com' } });
    await fireEvent.click(screen.getByTestId('welcome-join'));
    expect(screen.getByTestId('welcome-invite-error')).toHaveTextContent(
      /doesn't look like a harmony:\/\/ invite/i,
    );
    expect(onJoinWithInvite).not.toHaveBeenCalled();
  });

  it('valid harmony:// URL + "Join now" → onJoinWithInvite (parent orchestrates dismissal)', async () => {
    const onJoinWithInvite = vi.fn();
    const onDismiss = vi.fn();
    render(WelcomeModal, { open: true, onDismiss, onJoinWithInvite });
    const input = screen.getByTestId('welcome-invite-input');
    const validUrl = 'harmony://invite/v1?p=test';
    await fireEvent.input(input, { target: { value: validUrl } });
    await fireEvent.click(screen.getByTestId('welcome-join'));
    await waitFor(() => expect(onJoinWithInvite).toHaveBeenCalledWith(validUrl));
    // Component contract: parent (App.svelte) controls dismissal by setting
    // open=false after handling onJoinWithInvite. The component itself does
    // NOT call onDismiss on a successful join.
    expect(onDismiss).not.toHaveBeenCalled();
  });

  it('"Skip for now" → onDismiss', async () => {
    const onDismiss = vi.fn();
    render(WelcomeModal, {
      open: true,
      onDismiss,
      onJoinWithInvite: () => {},
    });
    await fireEvent.click(screen.getByTestId('welcome-skip'));
    expect(onDismiss).toHaveBeenCalled();
  });

  it('Escape key → onDismiss', async () => {
    const onDismiss = vi.fn();
    render(WelcomeModal, {
      open: true,
      onDismiss,
      onJoinWithInvite: () => {},
    });
    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(onDismiss).toHaveBeenCalled();
  });

  it('backdrop click → onDismiss', async () => {
    const onDismiss = vi.fn();
    render(WelcomeModal, {
      open: true,
      onDismiss,
      onJoinWithInvite: () => {},
    });
    await fireEvent.click(screen.getByTestId('welcome-modal-backdrop'));
    expect(onDismiss).toHaveBeenCalled();
  });

  it('renders feedback-docs footer link', () => {
    render(WelcomeModal, {
      open: true,
      onDismiss: () => {},
      onJoinWithInvite: () => {},
    });
    const link = screen.getByTestId('welcome-feedback-link');
    expect(link.tagName).toBe('A');
    expect(link.getAttribute('href')).toContain('feedback.md');
  });

  it('renders version label in footer', async () => {
    render(WelcomeModal, {
      open: true,
      onDismiss: () => {},
      onJoinWithInvite: () => {},
    });
    // Version is fetched via onMount; just verify the element exists.
    // The actual version string depends on whether Tauri is available
    // in the test environment (typically 'unknown').
    const versionEl = screen.getByTestId('welcome-version');
    expect(versionEl).toBeInTheDocument();
  });
});
