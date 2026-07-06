import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import StatusPill from '../StatusPill.svelte';
import TallyBar from '../TallyBar.svelte';
import CountChip from '../CountChip.svelte';
import GovConfirmModal from '../GovConfirmModal.svelte';

describe('StatusPill', () => {
  it('renders the default label per variant with the variant class', () => {
    const { container } = render(StatusPill, { props: { variant: 'open' } });
    const pill = container.querySelector('.status-pill.open');
    expect(pill?.textContent).toBe('● Open');
  });
  it('label prop overrides the default and ariaLabel lands on the pill', () => {
    render(StatusPill, {
      props: { variant: 'passing', label: 'Threshold reached', ariaLabel: 'Lifecycle' },
    });
    const pill = screen.getByLabelText('Lifecycle');
    expect(pill.textContent).toBe('Threshold reached');
    expect(pill.classList.contains('passing')).toBe(true);
  });
});

describe('TallyBar', () => {
  it('renders one fill per segment with clamped width and token background', () => {
    const { container } = render(TallyBar, {
      props: {
        segments: [
          { pct: 68, token: '--vote-for' },
          { pct: 140, token: '--vote-against' },
        ],
        label: 'Live tally',
      },
    });
    const fills = container.querySelectorAll('.tally-fill');
    expect(fills.length).toBe(2);
    expect((fills[0] as HTMLElement).style.width).toBe('68%');
    expect((fills[1] as HTMLElement).style.width).toBe('100%'); // clamped
    expect((fills[0] as HTMLElement).style.background).toContain('--vote-for');
    expect(screen.getByLabelText('Live tally')).toBeTruthy();
  });
  it('collapses a NaN pct to an explicit 0% width', () => {
    const { container } = render(TallyBar, {
      props: { segments: [{ pct: Number.NaN, token: '--vote-for' }] },
    });
    expect((container.querySelector('.tally-fill') as HTMLElement).style.width).toBe('0%');
  });
});

describe('CountChip', () => {
  it('renders label + value with the tone class', () => {
    const { container } = render(CountChip, {
      props: { label: 'Threshold', value: '68% reached', tone: 'sage' },
    });
    expect(container.querySelector('.count-chip.sage')).toBeTruthy();
    expect(screen.getByText('Threshold')).toBeTruthy();
    expect(screen.getByText('68% reached')).toBeTruthy();
  });
});

describe('GovConfirmModal', () => {
  it('click severity: confirm fires immediately', async () => {
    const onConfirm = vi.fn();
    const onCancel = vi.fn();
    render(GovConfirmModal, {
      props: { title: 'Confirm thing', confirmLabel: 'Do it', onConfirm, onCancel },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Do it' }));
    expect(onConfirm).toHaveBeenCalledTimes(1);
    await fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(onCancel).toHaveBeenCalledTimes(1);
  });
  it('typed severity: confirm disabled until the match string is typed', async () => {
    const onConfirm = vi.fn();
    render(GovConfirmModal, {
      props: {
        title: 'Confirm revoke',
        confirmLabel: 'Confirm revoke',
        severity: 'typed',
        typedMatch: 'revoke',
        onConfirm,
        onCancel: vi.fn(),
      },
    });
    const confirmBtn = screen.getByRole('button', { name: 'Confirm revoke' });
    expect((confirmBtn as HTMLButtonElement).disabled).toBe(true);
    const input = screen.getByLabelText('Type the word revoke to confirm');
    await fireEvent.input(input, { target: { value: '  ReVoKe ' } });
    expect((confirmBtn as HTMLButtonElement).disabled).toBe(false);
    await fireEvent.click(confirmBtn);
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });
  it('busy disables both buttons', () => {
    render(GovConfirmModal, {
      props: { title: 'T', busy: true, onConfirm: vi.fn(), onCancel: vi.fn() },
    });
    expect((screen.getByRole('button', { name: 'Confirm' }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole('button', { name: 'Cancel' }) as HTMLButtonElement).disabled).toBe(true);
  });
});
