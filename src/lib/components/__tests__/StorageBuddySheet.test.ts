import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import StorageBuddySheet from '../StorageBuddySheet.svelte';
import type { StorageBuddyDto, ContributionSummaryDto } from '../../storage-buddy-service';
import type { Profile } from '../../types';

const GB = 1_000_000_000;
const ADDR_A = 'aa'.repeat(16);
const ADDR_B = 'bb'.repeat(16);
const ADDR_C = 'cc'.repeat(16);
const ADDR_F = 'ff'.repeat(16);

function buddy(over: Partial<StorageBuddyDto> = {}): StorageBuddyDto {
  return {
    ownerAddress: ADDR_A,
    petName: 'Ana',
    status: 'active',
    myPledgeBytes: 2 * GB,
    theirPledgeBytes: 5 * GB,
    hostedForThemBytes: GB,
    theyReportHoldingBytes: 1.5 * GB,
    reportAgeMs: 120_000,
    ...over,
  };
}

function summary(over: Partial<ContributionSummaryDto> = {}): ContributionSummaryDto {
  return {
    hostedBytes: GB,
    budgetBytes: 10 * GB,
    buddyCount: 1,
    health: 'healthy',
    ...over,
  };
}

function props(over: Record<string, unknown> = {}) {
  return {
    buddies: [buddy()],
    summary: summary(),
    friendContacts: new Map<string, Profile>([
      [ADDR_F, { address: ADDR_F, displayName: 'Farid' } as Profile],
      [ADDR_A, { address: ADDR_A, displayName: 'Ana' } as Profile],
    ]),
    onClose: vi.fn(),
    onSetPledge: vi.fn().mockResolvedValue(undefined),
    onRemove: vi.fn().mockResolvedValue(undefined),
    onSetBudget: vi.fn().mockResolvedValue(undefined),
    ...over,
  };
}

describe('StorageBuddySheet', () => {
  it('classifies buddies into active / incoming / outgoing sections', () => {
    render(StorageBuddySheet, {
      props: props({
        buddies: [
          buddy(),
          buddy({ ownerAddress: ADDR_B, petName: 'Bo', status: 'pendingIncoming', myPledgeBytes: 0 }),
          buddy({ ownerAddress: ADDR_C, petName: 'Cy', status: 'pendingOutgoing', theirPledgeBytes: null }),
        ],
      }),
    });
    expect(screen.getByLabelText('Active buddies')).toBeTruthy();
    expect(screen.getByLabelText('Invites for you')).toBeTruthy();
    expect(screen.getByLabelText('Sent invites')).toBeTruthy();
    expect(screen.getByTestId('buddy-accept')).toBeTruthy();
    expect(screen.getByTestId('invite-cancel')).toBeTruthy();
  });

  it('shows the signed report line, or honest "No report yet"', () => {
    render(StorageBuddySheet, {
      props: props({
        buddies: [
          buddy(),
          buddy({ ownerAddress: ADDR_B, petName: 'Bo', theyReportHoldingBytes: null, reportAgeMs: null }),
        ],
      }),
    });
    expect(screen.getByText(/They hold 1\.5 GB for you\s*· 2m ago/)).toBeTruthy();
    expect(screen.getByText('No report yet')).toBeTruthy();
  });

  it('Accept pledges 0 bytes back (0-byte accept is valid)', async () => {
    const p = props({
      buddies: [buddy({ status: 'pendingIncoming', myPledgeBytes: 0 })],
    });
    render(StorageBuddySheet, { props: p });
    await fireEvent.click(screen.getByTestId('buddy-accept'));
    expect(p.onSetPledge).toHaveBeenCalledWith(ADDR_A, 0);
  });

  it('Decline is single-click (reversible dismissal)', async () => {
    const p = props({
      buddies: [buddy({ status: 'pendingIncoming', myPledgeBytes: 0 })],
    });
    render(StorageBuddySheet, { props: p });
    await fireEvent.click(screen.getByTestId('buddy-decline'));
    expect(p.onRemove).toHaveBeenCalledWith(ADDR_A);
  });

  it('Remove requires the tier-2 arm → confirm sequence', async () => {
    const p = props();
    render(StorageBuddySheet, { props: p });
    await fireEvent.click(screen.getByTestId('buddy-remove'));
    expect(p.onRemove).not.toHaveBeenCalled();
    await fireEvent.click(screen.getByTestId('buddy-remove-confirm'));
    expect(p.onRemove).toHaveBeenCalledWith(ADDR_A);
  });

  it('outgoing Cancel also requires arm → confirm', async () => {
    const p = props({
      buddies: [buddy({ status: 'pendingOutgoing', theirPledgeBytes: null })],
    });
    render(StorageBuddySheet, { props: p });
    await fireEvent.click(screen.getByTestId('invite-cancel'));
    expect(p.onRemove).not.toHaveBeenCalled();
    await fireEvent.click(screen.getByTestId('invite-cancel-confirm'));
    expect(p.onRemove).toHaveBeenCalledWith(ADDR_A);
  });

  it('pledge slider and number input stay in sync (shared value)', async () => {
    render(StorageBuddySheet, { props: props() });
    const slider = screen.getByTestId('pledge-slider') as HTMLInputElement;
    const number = screen.getByTestId('pledge-number') as HTMLInputElement;
    expect(slider.value).toBe('2');
    expect(number.value).toBe('2');
    await fireEvent.input(slider, { target: { value: '4' } });
    expect(number.value).toBe('4');
  });

  it('committing a pledge converts GB to bytes', async () => {
    const p = props();
    render(StorageBuddySheet, { props: p });
    const slider = screen.getByTestId('pledge-slider') as HTMLInputElement;
    await fireEvent.input(slider, { target: { value: '4' } });
    await fireEvent.change(slider, { target: { value: '4' } });
    expect(p.onSetPledge).toHaveBeenCalledWith(ADDR_A, 4 * GB);
  });

  it('budget slider+number share state and commit converts GB to bytes', async () => {
    const p = props();
    render(StorageBuddySheet, { props: p });
    const slider = screen.getByTestId('budget-slider') as HTMLInputElement;
    const number = screen.getByTestId('budget-number') as HTMLInputElement;
    expect(slider.value).toBe('10');
    expect(number.value).toBe('10');
    await fireEvent.input(slider, { target: { value: '25' } });
    expect(number.value).toBe('25');
    await fireEvent.change(slider, { target: { value: '25' } });
    expect(p.onSetBudget).toHaveBeenCalledWith(25 * GB);
  });

  it('invite picker excludes existing buddies and filters by search', async () => {
    render(StorageBuddySheet, { props: props() });
    // ADDR_A is an active buddy — excluded even though it's a friend.
    expect(screen.queryByTestId(`invite-candidate-${ADDR_A}`)).toBeNull();
    expect(screen.getByTestId(`invite-candidate-${ADDR_F}`)).toBeTruthy();
    const search = screen.getByTestId('invite-search') as HTMLInputElement;
    await fireEvent.input(search, { target: { value: 'zzz' } });
    expect(screen.queryByTestId(`invite-candidate-${ADDR_F}`)).toBeNull();
    expect(screen.getByText('No eligible friends match.')).toBeTruthy();
  });

  it('sending an invite pledges the chosen bytes', async () => {
    const p = props();
    render(StorageBuddySheet, { props: p });
    await fireEvent.click(screen.getByTestId(`invite-candidate-${ADDR_F}`));
    const number = screen.getByTestId('invite-pledge-number') as HTMLInputElement;
    await fireEvent.input(number, { target: { value: '1.5' } });
    await fireEvent.click(screen.getByTestId('invite-send'));
    expect(p.onSetPledge).toHaveBeenCalledWith(ADDR_F, 1.5 * GB);
  });

  it('surfaces action rejections inline via role=alert', async () => {
    const p = props({
      onRemove: vi.fn().mockRejectedValue(new Error('engine shutting down')),
    });
    render(StorageBuddySheet, { props: p });
    await fireEvent.click(screen.getByTestId('buddy-remove'));
    await fireEvent.click(screen.getByTestId('buddy-remove-confirm'));
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('engine shutting down');
  });

  it('Done fires onClose', async () => {
    const p = props();
    render(StorageBuddySheet, { props: p });
    await fireEvent.click(screen.getByTestId('sheet-done'));
    expect(p.onClose).toHaveBeenCalledOnce();
  });

  it('a late-arriving summary re-seeds the budget pair (Qodo: stale untrack seed)', async () => {
    const p = props({ summary: null });
    const { rerender } = render(StorageBuddySheet, { props: p });
    const number = screen.getByTestId('budget-number') as HTMLInputElement;
    expect(number.value).toBe('0');
    expect(number.disabled).toBe(true);
    await rerender({ ...p, summary: summary({ budgetBytes: 25 * GB }) });
    expect(number.value).toBe('25');
    expect(number.disabled).toBe(false);
  });

  it('an over-budget pledge keeps the slider max at the pledge (no silent clamp)', () => {
    render(StorageBuddySheet, {
      props: props({
        buddies: [buddy({ myPledgeBytes: 40 * GB })],
        summary: summary({ budgetBytes: 10 * GB }),
      }),
    });
    const slider = screen.getByTestId('pledge-slider') as HTMLInputElement;
    expect(slider.max).toBe('40');
    expect(slider.value).toBe('40');
  });

  // ── ZEB-960: petName and card displayName have no non-blank publish
  // constraint; a whitespace value must fall through to the short address, not
  // render a blank buddy name / invite candidate. ──
  describe('ZEB-960 name ladder', () => {
    it('falls through a whitespace petName AND whitespace card name to the short address', () => {
      render(StorageBuddySheet, {
        props: props({
          buddies: [buddy({ ownerAddress: ADDR_B, petName: '   ', status: 'active' })],
          friendContacts: new Map<string, Profile>([
            [ADDR_B, { address: ADDR_B, displayName: '   ' } as Profile],
          ]),
        }),
      });
      // ADDR_B = 'bb'.repeat(16) → shortAddr = 'bbbbbb…bbbb'
      expect(screen.getByTestId(`buddy-row-${ADDR_B}`).textContent).toContain('bbbbbb…bbbb');
    });

    it('lists a whitespace-displayName invite candidate by its short address', () => {
      render(StorageBuddySheet, {
        props: props({
          friendContacts: new Map<string, Profile>([
            [ADDR_F, { address: ADDR_F, displayName: '   ' } as Profile],
          ]),
        }),
      });
      // ADDR_F = 'ff'.repeat(16) → shortAddr = 'ffffff…ffff'
      expect(screen.getByTestId(`invite-candidate-${ADDR_F}`).textContent).toContain('ffffff…ffff');
    });
  });
});
