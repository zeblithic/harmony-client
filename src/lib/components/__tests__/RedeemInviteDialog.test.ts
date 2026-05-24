import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';

// Mock Tauri IPC and event layers before any component module is evaluated.
// The connectivity-adapter calls invoke() and listen() from these packages,
// so mocking here covers the full call chain (adapter → Tauri → mock).
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import { invoke } from '@tauri-apps/api/core';
import RedeemInviteDialog from '../RedeemInviteDialog.svelte';

const mockInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

describe('RedeemInviteDialog', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders URL input and Redeem button', () => {
    const { getByPlaceholderText, getByTestId } = render(RedeemInviteDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    expect(getByPlaceholderText(/harmony:\/\/invite/)).toBeTruthy();
    expect(getByTestId('iroh-redeem-btn')).toBeTruthy();
  });

  it('Redeem button disabled until URL contains harmony://invite/', async () => {
    const { getByTestId, getByPlaceholderText } = render(RedeemInviteDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    const btn = getByTestId('iroh-redeem-btn') as HTMLButtonElement;
    expect(btn.disabled).toBe(true);

    await fireEvent.input(input, { target: { value: 'not a url' } });
    expect(btn.disabled).toBe(true);

    await fireEvent.input(input, { target: { value: 'harmony://invite/v1?ci=...' } });
    expect(btn.disabled).toBe(false);
  });

  it('shows pending spinner when pending=true (LAN path)', () => {
    const { getByRole } = render(RedeemInviteDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn(), pending: true },
    });
    expect(getByRole('status')).toBeTruthy();
  });

  it('renders friendly summary when error provided', () => {
    const { getByText } = render(RedeemInviteDialog, {
      props: {
        onSubmit: vi.fn(),
        onCancel: vi.fn(),
        error: 'BootstrapSignatureInvalid: ed25519 verify failed',
      },
    });
    expect(getByText(/signature is invalid/i)).toBeTruthy();
  });

  it('disclosure exposes variant + tag in DOM', () => {
    const { container } = render(RedeemInviteDialog, {
      props: {
        onSubmit: vi.fn(),
        onCancel: vi.fn(),
        error: 'BootstrapSignatureInvalid: ed25519 verify failed',
      },
    });
    expect(container.textContent).toContain('bootstrap_signature_invalid');
  });

  it('preserves URL on error for retry', () => {
    const { getByPlaceholderText } = render(RedeemInviteDialog, {
      props: {
        onSubmit: vi.fn(),
        onCancel: vi.fn(),
        error: 'timed out after 15s',
        initialUrl: 'harmony://invite/v1?ci=foo',
      },
    });
    const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    expect(input.value).toBe('harmony://invite/v1?ci=foo');
  });

  // ── ZEB-323 Phase 2b: iroh path tests ─────────────────────────────────────

  it('iroh redeem button calls connectivity_redeem_invite_iroh with trimmed URL', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_redeem_invite_iroh') {
        return Promise.resolve({ status: 'pkarr_resolved_no_handshake', communityId: 'c1' });
      }
      return Promise.resolve(null);
    });

    const { getByTestId, getByPlaceholderText } = render(RedeemInviteDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });

    const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    await fireEvent.input(input, { target: { value: '  harmony://invite/v1?ci=abc  ' } });
    await fireEvent.click(getByTestId('iroh-redeem-btn'));

    await waitFor(() => {
      expect(mockInvoke).toHaveBeenCalledWith(
        'connectivity_redeem_invite_iroh',
        { inviteUrl: 'harmony://invite/v1?ci=abc' },
      );
    });
  });

  it('shows fallback button and "found on network" message when pkarr_resolved_no_handshake', async () => {
    // ZEB-323 Phase 2b: pkarr resolution succeeds but the full iroh join
    // handshake is deferred to Phase 2c. The UI should NOT show "Joined ✓";
    // instead it should show the LAN fallback path with an informational message.
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_redeem_invite_iroh') {
        return Promise.resolve({ status: 'pkarr_resolved_no_handshake', communityId: 'c1' });
      }
      return Promise.resolve(null);
    });

    const { getByTestId, getByPlaceholderText } = render(RedeemInviteDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });

    const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    await fireEvent.input(input, { target: { value: 'harmony://invite/v1?ci=x' } });
    await fireEvent.click(getByTestId('iroh-redeem-btn'));

    await waitFor(() => {
      const banner = getByTestId('iroh-error-banner');
      expect(banner.textContent?.toLowerCase()).toContain('found the inviter on the network');
      expect(getByTestId('fallback-lan-btn')).toBeTruthy();
    });
  });

  it('shows inviter_unreachable error message and fallback button', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_redeem_invite_iroh') {
        return Promise.resolve({ status: 'inviter_unreachable' });
      }
      return Promise.resolve(null);
    });

    const { getByTestId, getByPlaceholderText } = render(RedeemInviteDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });

    const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    await fireEvent.input(input, { target: { value: 'harmony://invite/v1?ci=x' } });
    await fireEvent.click(getByTestId('iroh-redeem-btn'));

    await waitFor(() => {
      const banner = getByTestId('iroh-error-banner');
      expect(banner.textContent).toContain("Couldn't reach the inviter");
    });
    expect(getByTestId('fallback-lan-btn')).toBeTruthy();
  });

  it('fallback button calls onSubmit (LAN path) after iroh failure', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_redeem_invite_iroh') {
        return Promise.resolve({ status: 'inviter_unreachable' });
      }
      return Promise.resolve(null);
    });
    const onSubmit = vi.fn();

    const { getByTestId, getByPlaceholderText } = render(RedeemInviteDialog, {
      props: { onSubmit, onCancel: vi.fn() },
    });

    const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    await fireEvent.input(input, { target: { value: 'harmony://invite/v1?ci=x' } });
    await fireEvent.click(getByTestId('iroh-redeem-btn'));

    await waitFor(() => getByTestId('fallback-lan-btn'));
    await fireEvent.click(getByTestId('fallback-lan-btn'));

    expect(onSubmit).toHaveBeenCalledWith('harmony://invite/v1?ci=x');
  });

  it('shows fallback button and error on IPC rejection', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_redeem_invite_iroh') {
        return Promise.reject(new Error('network error'));
      }
      return Promise.resolve(null);
    });

    const { getByTestId, getByPlaceholderText } = render(RedeemInviteDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });

    const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    await fireEvent.input(input, { target: { value: 'harmony://invite/v1?ci=x' } });
    await fireEvent.click(getByTestId('iroh-redeem-btn'));

    await waitFor(() => {
      expect(getByTestId('fallback-lan-btn')).toBeTruthy();
      expect(getByTestId('iroh-error-banner').textContent).toContain('network error');
    });
  });

  it('iroh redeem button is disabled while iroh is pending', async () => {
    let resolveRedeemPromise!: (v: { status: string }) => void;
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_redeem_invite_iroh') {
        return new Promise<{ status: string }>((r) => {
          resolveRedeemPromise = r;
        });
      }
      return Promise.resolve(null);
    });

    const { getByTestId, getByPlaceholderText } = render(RedeemInviteDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });

    const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    await fireEvent.input(input, { target: { value: 'harmony://invite/v1?ci=x' } });
    await fireEvent.click(getByTestId('iroh-redeem-btn'));

    // While pending, the iroh redeem button is replaced by the progress display
    // and cancel is disabled.
    await waitFor(() => {
      const cancel = document.querySelector('button.cancel-btn') as HTMLButtonElement;
      expect(cancel.disabled).toBe(true);
    });

    // Clean up the hanging promise.
    resolveRedeemPromise({ status: 'pkarr_resolved_no_handshake' });
  });

  // ── ZEB-325 Phase 2c: iroh joined path ────────────────────────────────────

  it('shows "Joined ✓" when iroh redeem returns status="joined"', async () => {
    // ZEB-325 Phase 2c: connectivity_redeem_invite_iroh now completes the
    // full handshake (pkarr resolve → iroh connect → PendingJoin → counter-
    // signed Join). On success the IPC returns status='joined' with the
    // community id. The UI must render the "Joined ✓" success label from
    // STAGE_LABELS and dismiss the dialog via onCancel.
    const onCancel = vi.fn();
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_redeem_invite_iroh') {
        return Promise.resolve({ status: 'joined', communityId: 'abc123' });
      }
      return Promise.resolve(null);
    });

    const { getByTestId, getByPlaceholderText } = render(RedeemInviteDialog, {
      // joinedDismissDelayMs=0 fires the post-success dismiss timer on
      // the next macrotask so waitFor doesn't race a 1.2s default.
      props: { onSubmit: vi.fn(), onCancel, joinedDismissDelayMs: 0 },
    });

    const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    await fireEvent.input(input, { target: { value: 'harmony://invite/v1?ci=x' } });
    await fireEvent.click(getByTestId('iroh-redeem-btn'));

    // The success label appears.
    await waitFor(() => {
      const label = getByTestId('iroh-stage-label');
      expect(label.textContent).toContain('Joined');
    });

    // Dialog dismisses itself via onCancel after the success display.
    await waitFor(() => {
      expect(onCancel).toHaveBeenCalled();
    });
  });
});
