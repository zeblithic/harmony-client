import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import RedeemInviteDialog from '../RedeemInviteDialog.svelte';

describe('RedeemInviteDialog', () => {
  it('renders URL input and Redeem button', () => {
    const { getByPlaceholderText, getByText } = render(RedeemInviteDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    expect(getByPlaceholderText(/harmony:\/\/invite/)).toBeTruthy();
    expect(getByText('Redeem')).toBeTruthy();
  });

  it('Redeem button disabled until URL contains harmony://invite/', async () => {
    const { getByText, getByPlaceholderText } = render(RedeemInviteDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    let btn = getByText('Redeem').closest('button') as HTMLButtonElement;
    expect(btn.disabled).toBe(true);

    await fireEvent.input(input, { target: { value: 'not a url' } });
    expect(btn.disabled).toBe(true);

    await fireEvent.input(input, { target: { value: 'harmony://invite/v1?ci=...' } });
    expect(btn.disabled).toBe(false);
  });

  it('Submit calls onSubmit with the URL', async () => {
    const onSubmit = vi.fn();
    const { getByText, getByPlaceholderText } = render(RedeemInviteDialog, {
      props: { onSubmit, onCancel: vi.fn() },
    });
    const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    await fireEvent.input(input, { target: { value: 'harmony://invite/v1?ci=foo' } });
    await fireEvent.click(getByText('Redeem'));
    expect(onSubmit).toHaveBeenCalledWith('harmony://invite/v1?ci=foo');
  });

  it('shows pending spinner when pending=true', () => {
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
});
