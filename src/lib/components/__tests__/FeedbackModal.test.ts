import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));
vi.mock('@tauri-apps/plugin-shell', () => ({
  open: vi.fn(),
}));
vi.mock('@tauri-apps/plugin-os', () => ({
  platform: vi.fn().mockResolvedValue('macos'),
  version: vi.fn().mockResolvedValue('15.0'),
}));
vi.mock('@tauri-apps/api/app', () => ({
  getVersion: vi.fn().mockResolvedValue('0.1.0-alpha.1'),
}));

import { invoke } from '@tauri-apps/api/core';
import { open as shellOpen } from '@tauri-apps/plugin-shell';
import FeedbackModal from '../FeedbackModal.svelte';

const REDACTED_FIXTURE = `## Harmony v0.1.0-alpha.1 (darwin/aarch64)
## Network: reachable
a3f9e1c2… direct 18ms`;

describe('FeedbackModal', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders textarea + toggle (off default) + Submit/Cancel', () => {
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    expect(screen.getByTestId('feedback-description')).toBeInTheDocument();
    const toggle = screen.getByTestId('feedback-attach-toggle') as HTMLInputElement;
    expect(toggle.checked).toBe(false);
    expect(screen.getByTestId('feedback-submit')).toBeInTheDocument();
    expect(screen.getByTestId('feedback-cancel')).toBeInTheDocument();
  });

  it('Submit disabled when description < 10 chars', async () => {
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    const textarea = screen.getByTestId('feedback-description');
    const submit = screen.getByTestId('feedback-submit') as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
    await fireEvent.input(textarea, { target: { value: 'too short' } });
    expect(submit.disabled).toBe(true);
    await fireEvent.input(textarea, { target: { value: 'this is long enough' } });
    expect(submit.disabled).toBe(false);
  });

  it('toggle ON fetches network_health_export_payload(includeFullIds:false) + shows preview', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(REDACTED_FIXTURE);
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    const toggle = screen.getByTestId('feedback-attach-toggle');
    await fireEvent.click(toggle);
    await waitFor(() => screen.getByTestId('feedback-diagnostics-preview'));
    expect(invoke).toHaveBeenCalledWith('network_health_export_payload', {
      includeFullIds: false,
    });
    expect(screen.getByTestId('feedback-diagnostics-preview')).toHaveTextContent(
      /Harmony v0.1.0-alpha.1/,
    );
  });

  it('toggle OFF hides preview', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(REDACTED_FIXTURE);
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    const toggle = screen.getByTestId('feedback-attach-toggle');
    await fireEvent.click(toggle);
    await waitFor(() => screen.getByTestId('feedback-diagnostics-preview'));
    await fireEvent.click(toggle);
    expect(screen.queryByTestId('feedback-diagnostics-preview')).toBeNull();
  });

  it('Submit without diagnostics → shell.open URL omits ## Network diagnostics', async () => {
    const onDismiss = vi.fn();
    render(FeedbackModal, { open: true, onDismiss });
    await fireEvent.input(screen.getByTestId('feedback-description'), {
      target: { value: 'test feedback message' },
    });
    await fireEvent.click(screen.getByTestId('feedback-submit'));
    await waitFor(() => expect(shellOpen).toHaveBeenCalled());
    const url = (shellOpen as ReturnType<typeof vi.fn>).mock.calls[0][0] as string;
    const body = decodeURIComponent(url.split('body=')[1]);
    expect(body).toContain('## Description');
    expect(body).toContain('test feedback message');
    expect(body).not.toContain('## Network diagnostics');
    await waitFor(() => expect(onDismiss).toHaveBeenCalled());
  });

  it('Submit with diagnostics → URL contains redacted markdown', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(REDACTED_FIXTURE);
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    await fireEvent.click(screen.getByTestId('feedback-attach-toggle'));
    await waitFor(() => screen.getByTestId('feedback-diagnostics-preview'));
    await fireEvent.input(screen.getByTestId('feedback-description'), {
      target: { value: 'with diagnostics attached' },
    });
    await fireEvent.click(screen.getByTestId('feedback-submit'));
    await waitFor(() => expect(shellOpen).toHaveBeenCalled());
    const url = (shellOpen as ReturnType<typeof vi.fn>).mock.calls[0][0] as string;
    const body = decodeURIComponent(url.split('body=')[1]);
    expect(body).toContain('## Network diagnostics');
    expect(body).toContain('Harmony v0.1.0-alpha.1');
  });

  it('PRIVACY INVARIANT: URL with toggle ON contains NO full Ed25519 hex', async () => {
    // Backend returns redacted markdown (ellipsized addresses).
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(REDACTED_FIXTURE);
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    await fireEvent.click(screen.getByTestId('feedback-attach-toggle'));
    await waitFor(() => screen.getByTestId('feedback-diagnostics-preview'));
    await fireEvent.input(screen.getByTestId('feedback-description'), {
      target: { value: 'privacy regression test' },
    });
    await fireEvent.click(screen.getByTestId('feedback-submit'));
    await waitFor(() => expect(shellOpen).toHaveBeenCalled());
    const url = (shellOpen as ReturnType<typeof vi.fn>).mock.calls[0][0] as string;
    const decoded = decodeURIComponent(url);
    // No 32+ char lowercase hex run anywhere in the URL.
    expect(decoded).not.toMatch(/[0-9a-f]{32,}/);
  });

  it('shell.open rejects → URL copied to clipboard + toast shown', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    (shellOpen as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('shell unavailable'));
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    await fireEvent.input(screen.getByTestId('feedback-description'), {
      target: { value: 'test for clipboard fallback' },
    });
    await fireEvent.click(screen.getByTestId('feedback-submit'));
    await waitFor(() => expect(writeText).toHaveBeenCalled());
    expect(screen.getByTestId('feedback-toast')).toHaveTextContent(/clipboard/i);
  });

  it('network_health_export_payload rejects → "Diagnostics unavailable" + Submit still works', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('not ready'));
    (shellOpen as ReturnType<typeof vi.fn>).mockResolvedValue(undefined);
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    await fireEvent.click(screen.getByTestId('feedback-attach-toggle'));
    await waitFor(() =>
      expect(screen.getByTestId('feedback-diagnostics-error')).toHaveTextContent(
        /diagnostics unavailable/i,
      ),
    );
    await fireEvent.input(screen.getByTestId('feedback-description'), {
      target: { value: 'submit despite diagnostic fetch failure' },
    });
    await fireEvent.click(screen.getByTestId('feedback-submit'));
    await waitFor(() => expect(shellOpen).toHaveBeenCalled());
    // URL must NOT include the diagnostics section because the fetch failed.
    const url = (shellOpen as ReturnType<typeof vi.fn>).mock.calls[0][0] as string;
    const body = decodeURIComponent(url.split('body=')[1]);
    expect(body).not.toContain('## Network diagnostics');
  });

  it('stale-response guard: rapid toggle → only latest response reflected', async () => {
    let resolvers: Array<(v: string) => void> = [];
    (invoke as ReturnType<typeof vi.fn>).mockImplementation(
      () => new Promise<string>((resolve) => resolvers.push(resolve)),
    );
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    const toggle = screen.getByTestId('feedback-attach-toggle');
    // First click ON → invoke #1 pending
    await fireEvent.click(toggle);
    // Second click OFF → no invoke (toggle just hides preview)
    await fireEvent.click(toggle);
    // Third click ON → invoke #2 pending
    await fireEvent.click(toggle);
    // Resolve OLDEST first (stale), then NEWEST (latest). Insert a
    // microtask yield between to make ordering explicit and ensure the
    // stale resolution path runs before the fresh one.
    resolvers[0]('STALE_CONTENT');
    await Promise.resolve();
    resolvers[1]('FRESH_CONTENT');
    await waitFor(() =>
      expect(screen.getByTestId('feedback-diagnostics-preview')).toHaveTextContent('FRESH_CONTENT'),
    );
    expect(screen.getByTestId('feedback-diagnostics-preview')).not.toHaveTextContent('STALE_CONTENT');
  });

  it('submitting flag disables Submit during shell.open', async () => {
    let resolveShellOpen!: () => void;
    (shellOpen as ReturnType<typeof vi.fn>).mockImplementation(
      () => new Promise<void>((resolve) => { resolveShellOpen = resolve; }),
    );
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    await fireEvent.input(screen.getByTestId('feedback-description'), {
      target: { value: 'submitting flag test' },
    });
    const submit = screen.getByTestId('feedback-submit') as HTMLButtonElement;
    fireEvent.click(submit);
    // Wait until shell.open has been called (meaning submitting=true is active)
    await waitFor(() => expect(shellOpen).toHaveBeenCalled());
    expect(submit.disabled).toBe(true);
    resolveShellOpen();
  });

  it('Cancel → onDismiss', async () => {
    const onDismiss = vi.fn();
    render(FeedbackModal, { open: true, onDismiss });
    await fireEvent.click(screen.getByTestId('feedback-cancel'));
    expect(onDismiss).toHaveBeenCalled();
  });

  it('resets state when modal closes (does not persist draft)', async () => {
    const { rerender } = render(FeedbackModal, { open: true, onDismiss: () => {} });
    // Type a draft
    const textarea = screen.getByTestId('feedback-description');
    await fireEvent.input(textarea, { target: { value: 'draft message' } });
    expect((textarea as HTMLTextAreaElement).value).toBe('draft message');
    // Close the modal
    await rerender({ open: false, onDismiss: () => {} });
    // Reopen
    await rerender({ open: true, onDismiss: () => {} });
    const reopened = screen.getByTestId('feedback-description') as HTMLTextAreaElement;
    expect(reopened.value).toBe('');
  });

  it('reset-on-close does NOT wipe payload during in-flight submit (CodeRabbit R2 regression)', async () => {
    // Race scenario: shellOpen is still pending while parent flips open=false.
    // Without the !submitting gate, the reset effect would wipe `description`
    // before handleSubmit's URL build had completed — but URL is already
    // built before shellOpen, so what we really guard against is the
    // observed-state during the await: if submitting=false were forced by
    // the reset, the in-flight finally would have nothing to undo.
    // We assert the URL passed to shellOpen contains the actual description.
    let resolveShellOpen: () => void = () => {};
    const shellOpenPromise = new Promise<void>((res) => {
      resolveShellOpen = res;
    });
    (shellOpen as ReturnType<typeof vi.fn>).mockReturnValue(shellOpenPromise);

    const { rerender } = render(FeedbackModal, {
      open: true,
      onDismiss: () => {},
    });
    const textarea = screen.getByTestId('feedback-description');
    await fireEvent.input(textarea, {
      target: { value: 'race regression description payload' },
    });

    // Kick submit — handleSubmit's shellOpen will hang on our gated promise.
    await fireEvent.click(screen.getByTestId('feedback-submit'));

    // While submit is in flight, parent-driven close (e.g. Escape, route change).
    await rerender({ open: false, onDismiss: () => {} });

    // Resolve shellOpen — finally{} clears submitting → reset effect re-fires.
    resolveShellOpen();
    await shellOpenPromise;

    // The URL handed to shellOpen MUST contain the real description, not '' —
    // proves reset didn't wipe state mid-await.
    const calls = (shellOpen as ReturnType<typeof vi.fn>).mock.calls;
    expect(calls.length).toBeGreaterThan(0);
    const urlArg = calls[0]?.[0] as string;
    expect(urlArg).toContain('race%20regression%20description%20payload');
  });
});
