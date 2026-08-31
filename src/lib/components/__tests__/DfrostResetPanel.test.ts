import { describe, it, expect, vi, beforeEach } from 'vitest';
import { tick as svelteTick } from 'svelte';
import { render, waitFor, fireEvent } from '@testing-library/svelte';
import DfrostResetPanel from '../DfrostResetPanel.svelte';
import type { DfrostCommitteeSummaryDto, ResetProposalDto } from '../../dfrost-reset-types';
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
    effectiveQuorum: null,
    ...overrides,
  };
}

function makeSummary(overrides: Partial<DfrostCommitteeSummaryDto> = {}): DfrostCommitteeSummaryDto {
  return {
    active: true,
    currentEpoch: 3,
    jointVk: 'aa'.repeat(32),
    memberAddrs: [MEMBER_A, MEMBER_B],
    threshold: 2,
    maxSigners: 2,
    pendingReset: false,
    ...overrides,
  };
}

/** Pre-DKG shape — no committee yet. */
const PRE_DKG_SUMMARY = makeSummary({
  active: false,
  currentEpoch: 0,
  jointVk: null,
  memberAddrs: [],
  threshold: 0,
  maxSigners: 0,
});

/** ZEB-1042 round 1: a KNOWN summary state now gates the propose form
 *  (pre-DKG / pendingReset disable it; an active committee locks the
 *  target fields), so tests that hand-type the target vk/epoch must run
 *  on the one path where that's still possible — the summary read
 *  failing ('error'). The default stays the failure path for exactly
 *  those legacy manual-entry tests. */
function mockResetState(
  proposals: ResetProposalDto[],
  summary: DfrostCommitteeSummaryDto | 'error' = 'error',
) {
  (invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
    if (cmd === 'get_dfrost_reset_state') return Promise.resolve(proposals);
    if (cmd === 'get_dfrost_committee_summary')
      return summary === 'error'
        ? Promise.reject(new Error('summary unavailable'))
        : Promise.resolve(summary);
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
    // Deliberately WITHOUT `shouldAdvanceTime: true` (ZEB-1031 Task 9
    // review round 1 C1): that option ties the fake clock's progression to
    // REAL elapsed wall-clock time via a background real interval — which
    // reliably passed in the full parallel suite (enough other work
    // running alongside masked the drift) but failed deterministically
    // under `npx vitest run` on this file in isolation (less contention →
    // the drift consistently landed on a different hour than expected:
    // "2d 1h remaining" instead of "2d 2h remaining"). Every clock move
    // below is now driven explicitly via `vi.advanceTimersByTimeAsync` —
    // no real time ever leaks into the fake clock, so the test is
    // deterministic under both isolation and full-suite runs.
    vi.useFakeTimers();
    try {
      const now = Date.UTC(2026, 7, 1, 12, 0, 0);
      vi.setSystemTime(now);
      const deadline = now + 2 * 86_400_000 + 3 * 3_600_000; // 2d 3h out
      mockResetState([makeProposal({ phase: 'window', deadlineMs: deadline })]);
      const { getByText } = renderPanel();
      // Initial render depends on the async invoke() resolving —
      // testing-library's `waitFor` resolves via a MutationObserver
      // watching the DOM (a microtask, not a timer), so it needs no fake-
      // timer advance at all here; no fake-clock value is being asserted,
      // just that the row has appeared.
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

  it('renders the effective (tipping-time) quorum, falling back to the live adminQuorum while collecting', async () => {
    // CR review round 1: the denominator must come from the proposal's
    // own effectiveQuorum once tipped, not the live adminQuorum, so a
    // later ChangeQuorum can't relabel an already-authorized proposal.
    mockResetState([
      makeProposal({ phase: 'collecting', effectiveQuorum: null, signerAddrs: [PROPOSER] }),
    ]);
    const { getByText } = renderPanel({ adminQuorum: 3 });
    await waitFor(() => {
      expect(getByText('1 of 3 required')).toBeTruthy();
    });
  });

  it('keeps the tipping-time effectiveQuorum denominator even after the live adminQuorum changes', async () => {
    mockResetState([
      makeProposal({
        phase: 'window',
        deadlineMs: Date.now() + 60_000,
        effectiveQuorum: 2,
        signerAddrs: [PROPOSER, MEMBER_A],
      }),
    ]);
    // Live adminQuorum has since been raised to 3 — the panel must still
    // report the pinned effectiveQuorum (2), not the live value.
    const { getByText } = renderPanel({ adminQuorum: 3 });
    await waitFor(() => {
      expect(getByText('2 of 2 required')).toBeTruthy();
    });
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

  it('keeps the submit button disabled with only one successor member (backend requires >=2)', async () => {
    // CodeAnt majors 1+2 (review round 1): the backend rejects a 1-member
    // or threshold-1 successor config — the form must not advertise those
    // as submittable.
    mockResetState([]);
    const { getByText, container } = renderPanel();
    await fireEvent.click(getByText('Propose a committee reset…'));

    const vkInput = container.querySelector(
      'input[placeholder*="64-char hex"]',
    ) as HTMLInputElement;
    await fireEvent.input(vkInput, { target: { value: 'ab'.repeat(32) } });
    const epochInput = container.querySelector('input[type="number"][min="0"]') as HTMLInputElement;
    await fireEvent.input(epochInput, { target: { value: '4' } });

    const bobRow = getByText('@bob').closest('button')!;
    await fireEvent.click(bobRow);

    const submitButton = getByText('Review proposal…') as HTMLButtonElement;
    expect(submitButton.disabled).toBe(true);

    const thresholdInput = container.querySelector('.threshold-input') as HTMLInputElement;
    expect(thresholdInput.min).toBe('2');

    // Selecting a second member alone isn't enough while threshold
    // still starts below 2 — but the panel now defaults newThreshold
    // to 2, so a second selection should make the form submittable.
    const cynRow = getByText('@cyn').closest('button')!;
    await fireEvent.click(cynRow);
    expect(submitButton.disabled).toBe(false);
  });

  it('hides the propose form entirely when canAdmin is false', async () => {
    mockResetState([]);
    const { queryByText } = renderPanel({ canAdmin: false });
    await waitFor(() => {
      expect(queryByText('Propose a committee reset…')).toBeNull();
    });
  });

  // ── ZEB-1042: committee summary + propose-form prefill ────────────────

  it('renders the current committee line from get_dfrost_committee_summary', async () => {
    mockResetState([], makeSummary());
    const { getByTestId } = renderPanel();
    await waitFor(() => {
      const line = getByTestId('dfrost-committee-summary');
      expect(line.textContent).toContain('epoch 3');
      expect(line.textContent).toContain('2-of-2');
      // shortHex of 'aa'.repeat(32): first 8 + … + last 4.
      expect(line.textContent).toContain('aaaaaaaa…aaaa');
      expect(line.textContent).not.toContain('reset in progress');
    });
  });

  it('flags an in-flight reset and disables proposing while pendingReset', async () => {
    // The REACHABLE pendingReset shape: apply_reset_marker deactivates
    // the committee (active=false, vk=None) when it pins the successor —
    // {active: true, pendingReset: true} is unrepresentable.
    mockResetState(
      [],
      makeSummary({ active: false, jointVk: null, pendingReset: true }),
    );
    const { getByTestId, getByText } = renderPanel();
    await waitFor(() => {
      expect(getByTestId('dfrost-committee-summary').textContent).toContain(
        'Committee reset in progress',
      );
    });
    const toggle = getByText('Propose a committee reset…') as HTMLButtonElement;
    expect(toggle.disabled).toBe(true);
    expect(toggle.title).toBe('A reset is already in progress.');
  });

  it('renders the no-committee line pre-DKG and disables proposing (nothing to reset)', async () => {
    // CodeAnt major 2 (round 1): RS-P1–P5 never validate the target
    // against dfrost state, so the backend would accept a proposal with
    // no resettable committee behind it — the form must not offer one.
    mockResetState([], PRE_DKG_SUMMARY);
    const { getByTestId, getByText } = renderPanel();
    await waitFor(() => {
      expect(getByTestId('dfrost-committee-summary').textContent).toContain(
        'No active D-FROST committee yet',
      );
    });
    const toggle = getByText('Propose a committee reset…') as HTMLButtonElement;
    expect(toggle.disabled).toBe(true);
    expect(toggle.title).toBe('There is no active committee to reset.');
  });

  it('prefills and locks the target vk/epoch from an active committee, and submits the prefilled values', async () => {
    mockResetState([], makeSummary({ currentEpoch: 7 }));
    const { getByText, container } = renderPanel();
    await waitFor(() => {
      expect(getByText('Propose a committee reset…')).toBeTruthy();
    });
    await fireEvent.click(getByText('Propose a committee reset…'));

    let vkInput: HTMLInputElement;
    let epochInput: HTMLInputElement;
    await waitFor(() => {
      vkInput = container.querySelector('input[placeholder*="64-char hex"]') as HTMLInputElement;
      epochInput = container.querySelector('input[type="number"][min="0"]') as HTMLInputElement;
      expect(vkInput.value).toBe('aa'.repeat(32));
      expect(epochInput.value).toBe('7');
      expect(vkInput.readOnly).toBe(true);
      expect(epochInput.readOnly).toBe(true);
    });
    expect(getByText('Target key and epoch are filled from the active committee.')).toBeTruthy();

    // Successor config is still the admin's to choose — and the propose
    // payload must carry the prefilled vk/epoch verbatim.
    await fireEvent.click(getByText('@bob').closest('button')!);
    await fireEvent.click(getByText('@cyn').closest('button')!);
    await fireEvent.click(getByText('Review proposal…'));
    await fireEvent.click(getByText('Propose reset'));
    await waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('propose_dfrost_reset', {
        communityId: COMMUNITY_ID,
        targetVkHex: 'aa'.repeat(32),
        targetEpoch: 7,
        newMembers: [MEMBER_A, MEMBER_B],
        newThreshold: 2,
        vetoWindowMs: 72 * 3_600_000,
      });
    });
  });

  it('freezes the prefilled target values while the confirm modal is open', async () => {
    // CodeAnt major 1 (round 1): a 60s poll landing while the confirm
    // modal is up must not rewrite targetVkHex/targetEpoch — the admin
    // submits exactly the values they reviewed.
    vi.useFakeTimers();
    try {
      let epoch = 7;
      (invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
        if (cmd === 'get_dfrost_reset_state') return Promise.resolve([]);
        if (cmd === 'get_dfrost_committee_summary')
          return Promise.resolve(makeSummary({ currentEpoch: epoch }));
        return Promise.resolve(undefined);
      });
      const { getByText, container } = renderPanel();
      // Flush the mount fetches (microtasks) and the render.
      await vi.advanceTimersByTimeAsync(0);
      await svelteTick();

      await fireEvent.click(getByText('Propose a committee reset…'));
      await vi.advanceTimersByTimeAsync(0); // form-open refreshCommittee
      await svelteTick();
      const epochInput = container.querySelector(
        'input[type="number"][min="0"]',
      ) as HTMLInputElement;
      expect(epochInput.value).toBe('7');

      await fireEvent.click(getByText('@bob').closest('button')!);
      await fireEvent.click(getByText('@cyn').closest('button')!);
      await fireEvent.click(getByText('Review proposal…'));

      // Committee refresh completes elsewhere; the next poll observes it
      // while the modal is up.
      epoch = 8;
      await vi.advanceTimersByTimeAsync(60_000);
      await svelteTick();

      await fireEvent.click(getByText('Propose reset'));
      await vi.advanceTimersByTimeAsync(0);
      expect(invoke).toHaveBeenCalledWith(
        'propose_dfrost_reset',
        expect.objectContaining({ targetEpoch: 7 }),
      );
    } finally {
      vi.useRealTimers();
    }
  });

  it('blocks Review until the open-time committee refresh settles', async () => {
    // CodeRabbit (round 2): opening the form kicks an async summary
    // refresh; until it settles, the fields still hold last-poll values
    // — and the round-1 confirm-freeze would pin them if the admin
    // raced into the modal. Review must stay disabled while the refresh
    // is in flight, then enable with the FRESH values.
    let resolveHeld: ((s: DfrostCommitteeSummaryDto) => void) | undefined;
    let summaryCalls = 0;
    (invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'get_dfrost_reset_state') return Promise.resolve([]);
      if (cmd === 'get_dfrost_committee_summary') {
        summaryCalls += 1;
        if (summaryCalls === 1) return Promise.resolve(makeSummary({ currentEpoch: 7 }));
        // The form-open refresh: held until the test releases it.
        return new Promise<DfrostCommitteeSummaryDto>((res) => {
          resolveHeld = res;
        });
      }
      return Promise.resolve(undefined);
    });
    const { getByText, container } = renderPanel();
    await waitFor(() => {
      expect(getByText('Propose a committee reset…')).toBeTruthy();
    });
    await fireEvent.click(getByText('Propose a committee reset…'));

    // Fill everything else in while the open-time refresh hangs.
    await fireEvent.click(getByText('@bob').closest('button')!);
    await fireEvent.click(getByText('@cyn').closest('button')!);
    const submitButton = getByText('Review proposal…') as HTMLButtonElement;
    expect(submitButton.disabled).toBe(true); // refresh in flight — gated

    resolveHeld!(makeSummary({ currentEpoch: 8 }));
    const epochInput = container.querySelector('input[type="number"][min="0"]') as HTMLInputElement;
    await waitFor(() => {
      expect(epochInput.value).toBe('8'); // fresh values landed
      expect(submitButton.disabled).toBe(false);
    });
  });

  it('falls back to manual entry when the committee summary read fails', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'get_dfrost_reset_state') return Promise.resolve([]);
      if (cmd === 'get_dfrost_committee_summary')
        return Promise.reject(new Error('caller is not a Joined member'));
      return Promise.resolve(undefined);
    });
    const { getByText, container, queryByTestId } = renderPanel();
    await waitFor(() => {
      expect(getByText('Propose a committee reset…')).toBeTruthy();
    });
    // No summary → no committee line at all.
    expect(queryByTestId('dfrost-committee-summary')).toBeNull();

    await fireEvent.click(getByText('Propose a committee reset…'));
    await waitFor(() => {
      expect(
        getByText(/Couldn't load the active committee \(caller is not a Joined member\)/),
      ).toBeTruthy();
    });
    const vkInput = container.querySelector(
      'input[placeholder*="64-char hex"]',
    ) as HTMLInputElement;
    expect(vkInput.readOnly).toBe(false);
    await fireEvent.input(vkInput, { target: { value: 'ab'.repeat(32) } });
    expect(vkInput.value).toBe('ab'.repeat(32));
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
