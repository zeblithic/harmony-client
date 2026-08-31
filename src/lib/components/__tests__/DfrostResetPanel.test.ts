import { describe, it, expect, vi, beforeEach } from 'vitest';
import { tick as svelteTick } from 'svelte';
import { render, waitFor, fireEvent } from '@testing-library/svelte';
import DfrostResetPanel from '../DfrostResetPanel.svelte';
import type { ResetProposalDto } from '../../dfrost-reset-types';
import type { CommunityMember } from '../../types';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));

const { invoke } = vi.mocked(await import('@tauri-apps/api/core'));

const COMMUNITY_ID = 'c0'.repeat(16);
const PROPOSER = '11'.repeat(16);
const MEMBER_A = '21'.repeat(16);
const MEMBER_B = '22'.repeat(16);
const MEMBER_C = '23'.repeat(16);

const JOINED_MEMBERS: CommunityMember[] = [
  { address: PROPOSER, displayName: 'ada', power: 100, status: 'joined' },
  { address: MEMBER_A, displayName: 'bob', power: 0, status: 'joined' },
  { address: MEMBER_B, displayName: 'cyn', power: 0, status: 'joined' },
  { address: MEMBER_C, displayName: 'dee', power: 0, status: 'joined' },
];

function makeProposal(overrides: Partial<ResetProposalDto> = {}): ResetProposalDto {
  return {
    proposalEventId: 'b0'.repeat(16),
    proposerAddr: PROPOSER,
    targetVk: 'aa'.repeat(32),
    targetEpoch: 3,
    newMemberAddrs: [MEMBER_A, MEMBER_B],
    newThreshold: 2,
    vetoWindowMs: 259_200_000, // 72h
    signerAddrs: [PROPOSER],
    proposedAtWallMs: 100_000,
    deadlineMs: null,
    authorizedAtMs: null,
    endorsed: false,
    phase: 'collecting',
    consumedNewVk: null,
    consumptionSuperseded: false,
    selfHasCosigned: false,
    ...overrides,
  };
}

function mockResetState(proposals: ResetProposalDto[]) {
  (invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
    if (cmd === 'get_dfrost_reset_state') return Promise.resolve(proposals);
    return Promise.resolve(undefined);
  });
}

function renderPanel(props: Record<string, unknown> = {}) {
  return render(DfrostResetPanel, {
    props: {
      communityId: COMMUNITY_ID,
      joinedMembers: JOINED_MEMBERS,
      canAdmin: true,
      adminQuorum: 2,
      ...props,
    } as any,
  });
}

describe('DfrostResetPanel', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('shows the empty state when there are no proposals', async () => {
    mockResetState([]);
    const { getByText } = renderPanel();
    await waitFor(() => {
      expect(getByText('No committee reset proposals in this community.')).toBeTruthy();
    });
  });

  it('renders a phase chip for each lifecycle phase', async () => {
    const phases: ResetProposalDto['phase'][] = [
      'collecting',
      'window',
      'authorized',
      'consumed',
      'vetoed',
      'expired',
      'lapsed',
    ];
    mockResetState(
      phases.map((phase, i) =>
        makeProposal({
          proposalEventId: `b${i}`.repeat(16).slice(0, 32),
          phase,
          deadlineMs: phase === 'window' ? Date.now() + 10_000 : null,
        }),
      ),
    );
    const { getByText } = renderPanel();
    await waitFor(() => {
      expect(getByText('Collecting signatures')).toBeTruthy();
      expect(getByText('Veto window open')).toBeTruthy();
      expect(getByText('Authorized')).toBeTruthy();
      expect(getByText('Consumed')).toBeTruthy();
      expect(getByText('Vetoed')).toBeTruthy();
      expect(getByText('Expired')).toBeTruthy();
      expect(getByText('Lapsed')).toBeTruthy();
    });
  });

  it('renders a live countdown for the window phase from deadlineMs', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      const now = Date.UTC(2026, 7, 1, 12, 0, 0);
      vi.setSystemTime(now);
      const deadline = now + 2 * 86_400_000 + 3 * 3_600_000; // 2d 3h out
      mockResetState([makeProposal({ phase: 'window', deadlineMs: deadline })]);
      const { getByText } = renderPanel();
      // Initial render depends on the async invoke() resolving — this one
      // wait genuinely needs waitFor (no fake-clock value is being
      // asserted, just that the row has appeared).
      await waitFor(() => {
        expect(getByText('2d 3h remaining')).toBeTruthy();
      });

      // Advance the clock (deterministically, via the fake-timer API rather
      // than a second vi.setSystemTime call) past the 1-hour mark; the
      // countdown ticks down independently of the 60s network poll. Flush
      // with Svelte's own tick() rather than testing-library's waitFor —
      // waitFor's real-time polling loop interacts with
      // `shouldAdvanceTime` and drifts the fake clock further than intended.
      await vi.advanceTimersByTimeAsync(3_600_000);
      await svelteTick();
      expect(getByText('2d 2h remaining')).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });

  it('disables Co-sign once selfHasCosigned is true, and invokes it otherwise', async () => {
    mockResetState([makeProposal({ selfHasCosigned: false })]);
    const { container } = renderPanel();
    let btn: HTMLButtonElement | null = null;
    await waitFor(() => {
      btn = container.querySelector('button.act:not(.endorse):not(.veto)');
      expect(btn?.textContent?.trim()).toBe('Co-sign');
      expect(btn?.disabled).toBe(false);
    });
    await fireEvent.click(btn!);
    await waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('cosign_dfrost_reset', {
        communityId: COMMUNITY_ID,
        targetEventId: 'b0'.repeat(16),
      });
    });
  });

  it('shows Co-sign disabled with "Co-signed" once already signed', async () => {
    mockResetState([makeProposal({ selfHasCosigned: true })]);
    const { getByText } = renderPanel();
    await waitFor(() => {
      const btn = getByText('Co-signed') as HTMLButtonElement;
      expect(btn.disabled).toBe(true);
    });
  });

  it('hides Co-sign entirely when canAdmin is false', async () => {
    mockResetState([makeProposal()]);
    const { container, getByText } = renderPanel({ canAdmin: false });
    await waitFor(() => expect(getByText('Collecting signatures')).toBeTruthy());
    expect(container.querySelector('button.act:not(.endorse):not(.veto)')).toBeNull();
  });

  it('sends "endorse" and "veto" verdicts from the committee-response buttons', async () => {
    mockResetState([makeProposal({ phase: 'window', deadlineMs: Date.now() + 60_000 })]);
    const { getByText } = renderPanel();
    let endorseBtn: HTMLElement;
    await waitFor(() => {
      endorseBtn = getByText('Endorse');
      expect(endorseBtn).toBeTruthy();
    });
    await fireEvent.click(getByText('Endorse'));
    await waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('respond_dfrost_reset', {
        communityId: COMMUNITY_ID,
        targetEventId: 'b0'.repeat(16),
        verdict: 'endorse',
      });
    });

    await fireEvent.click(getByText('Veto'));
    await waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('respond_dfrost_reset', {
        communityId: COMMUNITY_ID,
        targetEventId: 'b0'.repeat(16),
        verdict: 'veto',
      });
    });
  });

  it('hides the committee-response buttons outside the active phases', async () => {
    mockResetState([makeProposal({ phase: 'consumed' })]);
    const { queryByText, getByText } = renderPanel();
    await waitFor(() => expect(getByText('Consumed')).toBeTruthy());
    expect(queryByText('Endorse')).toBeNull();
    expect(queryByText('Veto')).toBeNull();
  });

  it('clamps the veto-window number input to the 24h-720h range', async () => {
    mockResetState([]);
    const { getByText, container } = renderPanel();
    await fireEvent.click(getByText('Propose a committee reset…'));

    const numberInput = container.querySelector(
      '.paired-input input[type="number"]',
    ) as HTMLInputElement;
    expect(numberInput.value).toBe('72');

    await fireEvent.input(numberInput, { target: { value: '1' } });
    expect(numberInput.value).toBe('24');

    await fireEvent.input(numberInput, { target: { value: '5000' } });
    expect(numberInput.value).toBe('720');
  });

  it('submits propose_dfrost_reset with the exact camelCase payload after click-confirm', async () => {
    mockResetState([]);
    const { getByText, container } = renderPanel();
    await fireEvent.click(getByText('Propose a committee reset…'));

    const vkInput = container.querySelector(
      'input[placeholder*="64-char hex"]',
    ) as HTMLInputElement;
    await fireEvent.input(vkInput, { target: { value: 'ab'.repeat(32) } });

    const epochInput = container.querySelector('input[type="number"][min="0"]') as HTMLInputElement;
    await fireEvent.input(epochInput, { target: { value: '4' } });

    // Select two successor members.
    const bobRow = getByText('@bob').closest('button')!;
    const cynRow = getByText('@cyn').closest('button')!;
    await fireEvent.click(bobRow);
    await fireEvent.click(cynRow);

    const thresholdInput = container.querySelector('.threshold-input') as HTMLInputElement;
    await fireEvent.input(thresholdInput, { target: { value: '2' } });

    const windowNumberInput = container.querySelector(
      '.paired-input input[type="number"]',
    ) as HTMLInputElement;
    await fireEvent.input(windowNumberInput, { target: { value: '48' } });

    await fireEvent.click(getByText('Review proposal…'));

    // Click-confirm modal — the IPC must not fire until confirmed.
    expect(invoke).not.toHaveBeenCalledWith('propose_dfrost_reset', expect.anything());
    await fireEvent.click(getByText('Propose reset'));

    await waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('propose_dfrost_reset', {
        communityId: COMMUNITY_ID,
        targetVkHex: 'ab'.repeat(32),
        targetEpoch: 4,
        newMembers: [MEMBER_A, MEMBER_B],
        newThreshold: 2,
        vetoWindowMs: 48 * 3_600_000,
      });
    });
  });

  it('hides the propose form entirely when canAdmin is false', async () => {
    mockResetState([]);
    const { queryByText } = renderPanel({ canAdmin: false });
    await waitFor(() => {
      expect(queryByText('Propose a committee reset…')).toBeNull();
    });
  });

  it('surfaces respond_dfrost_reset errors via the Tauri error-extraction convention', async () => {
    mockResetState([makeProposal({ phase: 'window', deadlineMs: Date.now() + 60_000 })]);
    (invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'get_dfrost_reset_state')
        return Promise.resolve([
          makeProposal({ phase: 'window', deadlineMs: Date.now() + 60_000 }),
        ]);
      if (cmd === 'respond_dfrost_reset') return Promise.reject(new Error('not a committee member'));
      return Promise.resolve(undefined);
    });
    const { getByText } = renderPanel();
    await waitFor(() => expect(getByText('Endorse')).toBeTruthy());
    await fireEvent.click(getByText('Endorse'));
    await waitFor(() => {
      expect(getByText('not a committee member')).toBeTruthy();
    });
  });
});
