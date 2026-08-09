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

  it('renders friendly summary keyed on the structured error code', () => {
    const { getByText } = render(RedeemInviteDialog, {
      props: {
        onSubmit: vi.fn(),
        onCancel: vi.fn(),
        // ZEB-885: the backend now returns a structured { code, message }; the
        // dialog switches on the code, not the raw prose.
        error: {
          code: 'bootstrap_signature_invalid',
          message: 'redeem_invite: admin_bootstrap signature verify failed',
        },
      },
    });
    expect(getByText(/signature is invalid/i)).toBeTruthy();
  });

  it('disclosure exposes the code + raw message in DOM', () => {
    const { container } = render(RedeemInviteDialog, {
      props: {
        onSubmit: vi.fn(),
        onCancel: vi.fn(),
        error: {
          code: 'bootstrap_signature_invalid',
          message: 'redeem_invite: admin_bootstrap signature verify failed',
        },
      },
    });
    expect(container.textContent).toContain('bootstrap_signature_invalid');
    expect(container.textContent).toContain('signature verify failed');
  });

  it('falls back to generic copy for an unknown code (forward-compat)', () => {
    const { getByText } = render(RedeemInviteDialog, {
      props: {
        onSubmit: vi.fn(),
        onCancel: vi.fn(),
        error: { code: 'some_future_backend_code', message: 'brand new failure' } as never,
      },
    });
    expect(getByText(/Couldn't complete the invite redemption/i)).toBeTruthy();
  });

  it('preserves URL on error for retry', () => {
    const { getByPlaceholderText } = render(RedeemInviteDialog, {
      props: {
        onSubmit: vi.fn(),
        onCancel: vi.fn(),
        error: { code: 'inviter_unreachable', message: 'timed out after 15s' },
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

  it('suppresses the LAN fallback for a non-recoverable structured rejection, showing summary + hint', async () => {
    // ZEB-885: `internal` is a local/internal failure — the Reticulum LAN
    // fallback reruns the same machinery and can't recover it, so the button is
    // suppressed. The banner carries the actionable hint, not just the summary.
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_redeem_invite_iroh') {
        return Promise.reject({ code: 'internal', message: 'boom' });
      }
      return Promise.resolve(null);
    });

    const { getByTestId, queryByTestId, getByPlaceholderText } = render(RedeemInviteDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });

    const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    await fireEvent.input(input, { target: { value: 'harmony://invite/v1?ci=x' } });
    await fireEvent.click(getByTestId('iroh-redeem-btn'));

    await waitFor(() => {
      const banner = getByTestId('iroh-error-banner');
      expect(banner.textContent).toContain('Something went wrong redeeming the invite');
      expect(banner.textContent).toContain('bug on our side'); // the hint reaches the user
    });
    expect(queryByTestId('fallback-lan-btn')).toBeNull();
  });

  it('offers the LAN fallback for a network-recoverable structured rejection', async () => {
    // A reachability failure the LAN path could plausibly resolve → keep the
    // fallback button.
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_redeem_invite_iroh') {
        return Promise.reject({ code: 'inviter_unreachable', message: 'pkarr timeout' });
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
      expect(getByTestId('iroh-error-banner').textContent).toContain('Inviter is offline');
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

// ─────────────────────────────────────────────────────────────────────────────
// ZEB-650 slice 3: debounced pure-local invite preview card.
//
// The `preview_invite` IPC is pure local computation (decode + token-sig
// verify + expiry) — no join, no network — so firing it on keystroke settle
// is safe. The card must stay honest per ZEB-610 §0.4: community name and
// inviter authorization only, never member/channel counts (the standing
// guard above also covers this).
// ─────────────────────────────────────────────────────────────────────────────
describe('RedeemInviteDialog invite preview (ZEB-650 slice 3)', () => {
  const VALID_URL = 'harmony://invite/v1?ci=abc';
  const PREVIEW = {
    communityName: 'Cascadia Commons',
    isInviteOnly: true,
    inviterVerified: true,
    inviterFingerprint: 'ab12·cd34',
    inviterDisplayName: null as string | null,
    expired: false,
  };

  beforeEach(() => {
    vi.clearAllMocks();
  });

  function mockPreview(dto: Partial<typeof PREVIEW> | Error) {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'preview_invite') {
        return dto instanceof Error
          ? Promise.reject(dto)
          : Promise.resolve({ ...PREVIEW, ...dto });
      }
      return Promise.resolve(null);
    });
  }

  function renderDialog() {
    return render(RedeemInviteDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
  }

  async function typeUrl(
    utils: { getByPlaceholderText: (m: RegExp) => HTMLElement },
    value: string,
  ) {
    const input = utils.getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    await fireEvent.input(input, { target: { value } });
  }

  it('debounces: no preview IPC before settle, exactly one call after', async () => {
    mockPreview({});
    const utils = renderDialog();
    await typeUrl(utils, 'harmony://invite/v1?ci=a');
    await typeUrl(utils, 'harmony://invite/v1?ci=ab');
    await typeUrl(utils, VALID_URL);
    expect(mockInvoke).not.toHaveBeenCalledWith('preview_invite', expect.anything());
    await waitFor(() => {
      expect(mockInvoke).toHaveBeenCalledWith('preview_invite', { url: VALID_URL });
    });
    const previewCalls = mockInvoke.mock.calls.filter(
      (call: unknown[]) => call[0] === 'preview_invite',
    );
    expect(previewCalls.length).toBe(1);
  });

  it('renders the card: name, invite-only chip, verified line with fingerprint fallback', async () => {
    mockPreview({});
    const utils = renderDialog();
    await typeUrl(utils, VALID_URL);
    const card = await waitFor(() => utils.getByTestId('redeem-preview-card'));
    expect(card.textContent).toContain('Cascadia Commons');
    expect(utils.getByTestId('preview-invite-only-chip')).toBeTruthy();
    const verified = utils.getByTestId('preview-verified');
    expect(verified.textContent).toMatch(/invite signature verified/i);
    expect(verified.textContent).toContain('ab12·cd34');
  });

  it('prefers the display name over the fingerprint when present', async () => {
    mockPreview({ inviterDisplayName: 'Mara Okafor' });
    const utils = renderDialog();
    await typeUrl(utils, VALID_URL);
    const line = await waitFor(() => utils.getByTestId('preview-verified'));
    expect(line.textContent).toContain('Mara Okafor');
    expect(line.textContent).not.toContain('ab12·cd34');
  });

  it('unverified invite shows the neutral line, not an error', async () => {
    mockPreview({ inviterVerified: false, isInviteOnly: false });
    const utils = renderDialog();
    await typeUrl(utils, VALID_URL);
    const line = await waitFor(() => utils.getByTestId('preview-unverified'));
    expect(line.textContent).toMatch(/signature not verifiable/i);
    expect(utils.queryByTestId('preview-verified')).toBeNull();
    expect(utils.queryByTestId('preview-invite-only-chip')).toBeNull();
  });

  it('expired invite shows the notice and disables Redeem', async () => {
    mockPreview({ expired: true });
    const utils = renderDialog();
    await typeUrl(utils, VALID_URL);
    await waitFor(() => utils.getByTestId('preview-expired'));
    const redeem = utils.getByTestId('iroh-redeem-btn') as HTMLButtonElement;
    expect(redeem.disabled).toBe(true);
  });

  it('preview failure shows only the invalid-link line and keeps the dialog usable', async () => {
    mockPreview(new Error('decode: bad'));
    const utils = renderDialog();
    await typeUrl(utils, VALID_URL);
    const invalid = await waitFor(() => utils.getByTestId('redeem-preview-invalid'));
    expect(invalid.textContent).toMatch(/looks invalid/i);
    expect(utils.queryByTestId('redeem-preview-card')).toBeNull();
    const redeem = utils.getByTestId('iroh-redeem-btn') as HTMLButtonElement;
    expect(redeem.disabled).toBe(false);
    const cancel = utils.getByText('Cancel') as HTMLButtonElement;
    expect(cancel.disabled).toBe(false);
  });

  // Editing from one valid invite URL to another must not leave the previous
  // URL's card (or its expired/verified verdicts) visible during the new
  // URL's debounce + IPC window (Qodo/CodeRabbit PR #438).
  it('editing to a different valid URL clears the stale card immediately', async () => {
    const URL_A = 'harmony://invite/v1?ci=aaa';
    const URL_B = 'harmony://invite/v1?ci=bbb';
    mockInvoke.mockImplementation((cmd: string, args?: { url?: string }) => {
      if (cmd === 'preview_invite') {
        return Promise.resolve({
          ...PREVIEW,
          communityName: args?.url === URL_A ? 'First Commons' : 'Second Commons',
          expired: args?.url === URL_A,
        });
      }
      return Promise.resolve(null);
    });
    const utils = renderDialog();
    await typeUrl(utils, URL_A);
    const card = await waitFor(() => utils.getByTestId('redeem-preview-card'));
    expect(card.textContent).toContain('First Commons');
    const redeem = utils.getByTestId('iroh-redeem-btn') as HTMLButtonElement;
    expect(redeem.disabled).toBe(true); // URL A is expired

    await typeUrl(utils, URL_B);
    // Stale card and stale expired-gate must be gone synchronously, before
    // the new debounce resolves.
    expect(utils.queryByTestId('redeem-preview-card')).toBeNull();
    expect(redeem.disabled).toBe(false);

    const newCard = await waitFor(() => utils.getByTestId('redeem-preview-card'));
    expect(newCard.textContent).toContain('Second Commons');
    expect(newCard.textContent).not.toContain('First Commons');
  });

  it('clearing to an invalid format removes the card', async () => {
    mockPreview({});
    const utils = renderDialog();
    await typeUrl(utils, VALID_URL);
    await waitFor(() => utils.getByTestId('redeem-preview-card'));
    await typeUrl(utils, 'nonsense');
    expect(utils.queryByTestId('redeem-preview-card')).toBeNull();
  });

  it('renders no member or channel counts on the preview card', async () => {
    mockPreview({});
    const utils = renderDialog();
    await typeUrl(utils, VALID_URL);
    await waitFor(() => utils.getByTestId('redeem-preview-card'));
    expect(utils.queryByText(/\d+\s+members/i)).toBeNull();
    expect(utils.queryByText(/\d+\s+channels/i)).toBeNull();
  });
});
