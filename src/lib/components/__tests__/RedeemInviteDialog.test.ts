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

  // ZEB-610 (Commons G) honesty ledger §0.4: the redeem surface must never
  // render fabricated member/channel counts — an un-joined community's roster
  // and channel list are unknowable to the redeemer. This dialog carries no
  // community/inviter props (it is a URL-paste redeem form), so this is a
  // standing regression guard against a future "142 members · 6 channels"
  // preview being bolted on with invented numbers.
  it('does not render invented member or channel counts', () => {
    const { queryByText } = render(RedeemInviteDialog, {
      props: {
        onSubmit: vi.fn(),
        onCancel: vi.fn(),
        initialUrl: 'harmony://invite/v1?ci=abc',
      },
    });
    expect(queryByText(/\d+\s+members/i)).toBeNull();
    expect(queryByText(/\d+\s+channels/i)).toBeNull();
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

  it('clears the iroh error banner when handing off to the LAN fallback (no stacked banners)', async () => {
    // finding 12: after iroh fails and the user clicks "Try via local network",
    // the iroh banner must clear so a subsequent LAN failure (which the parent
    // surfaces via the `error` prop → the mapped banner) doesn't stack a second
    // error banner on top of the iroh one.
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_redeem_invite_iroh') {
        return Promise.resolve({ status: 'inviter_unreachable' });
      }
      return Promise.resolve(null);
    });

    const { getByTestId, queryByTestId, getByPlaceholderText } = render(RedeemInviteDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });

    const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    await fireEvent.input(input, { target: { value: 'harmony://invite/v1?ci=x' } });
    await fireEvent.click(getByTestId('iroh-redeem-btn'));

    await waitFor(() => getByTestId('iroh-error-banner'));
    await fireEvent.click(getByTestId('fallback-lan-btn'));

    // The iroh banner is gone; the LAN path now owns error display.
    expect(queryByTestId('iroh-error-banner')).toBeNull();
  });

  it('shows reach-but-local-failure message and NO fallback button when join_failed', async () => {
    // ZEB-325 PR #159 R1: status='join_failed' means the inviter was reached
    // and a valid JoinCountersign was delivered, but the local insert/commit
    // failed (engine insert, fence violation, commit rollback). The
    // Reticulum LAN fallback would just re-run the same local path against
    // the same local engine state — so the fallback button MUST be suppressed
    // and the error banner MUST distinguish this from "couldn't reach the
    // inviter". The community_id (truncated) is surfaced as a correlation
    // hint for bug reports.
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_redeem_invite_iroh') {
        return Promise.resolve({
          status: 'join_failed',
          communityId: 'abcdef0123456789',
        });
      }
      return Promise.resolve(null);
    });

    const { getByTestId, getByPlaceholderText, queryByTestId } = render(RedeemInviteDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });

    const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    await fireEvent.input(input, { target: { value: 'harmony://invite/v1?ci=x' } });
    await fireEvent.click(getByTestId('iroh-redeem-btn'));

    await waitFor(() => {
      const banner = getByTestId('iroh-error-banner');
      expect(banner.textContent).toContain('Reached the inviter');
      expect(banner.textContent).toContain("couldn’t complete the join locally");
      // community_id hint is surfaced (truncated to first 12 chars).
      expect(banner.textContent).toContain('abcdef012345');
    });
    // The LAN fallback button is suppressed for join_failed — Reticulum
    // can't help a local insert failure.
    expect(queryByTestId('fallback-lan-btn')).toBeNull();
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
    //
    // ZEB-325 PR #159 F11: drive the dismiss timer through fake timers
    // rather than `joinedDismissDelayMs: 0`. The 0ms variant raced the
    // dismiss macrotask under some scheduler interleavings — the
    // dialog's `onCancel` fires after a setTimeout(0), which can land
    // after `waitFor`'s default 1s budget on slow CI runners. With
    // `vi.useFakeTimers({ shouldAdvanceTime: true })` the test
    // deterministically advances time before asserting and is robust
    // to any host-process scheduling pressure.
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      const onCancel = vi.fn();
      mockInvoke.mockImplementation((cmd: string) => {
        if (cmd === 'connectivity_redeem_invite_iroh') {
          return Promise.resolve({ status: 'joined', communityId: 'abc123' });
        }
        return Promise.resolve(null);
      });

      const { getByTestId, getByPlaceholderText } = render(RedeemInviteDialog, {
        // 50ms delay + explicit advance keeps the test deterministic
        // without depending on `setTimeout(0)` macrotask ordering.
        props: { onSubmit: vi.fn(), onCancel, joinedDismissDelayMs: 50 },
      });

      const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
      await fireEvent.input(input, { target: { value: 'harmony://invite/v1?ci=x' } });
      await fireEvent.click(getByTestId('iroh-redeem-btn'));

      // The success label appears.
      await waitFor(() => {
        const label = getByTestId('iroh-stage-label');
        expect(label.textContent).toContain('Joined');
      });

      // Advance past the 50ms dismiss timer; onCancel must fire.
      await vi.advanceTimersByTimeAsync(50);
      expect(onCancel).toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });
});
