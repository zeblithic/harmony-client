import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, waitFor, fireEvent } from '@testing-library/svelte';
import RecoveryBanner from '../RecoveryBanner.svelte';
import type { RecoveryProposalDto, RecoveryStateDto } from '../../recovery-types';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

const { invoke } = vi.mocked(await import('@tauri-apps/api/core'));

const MY_ADDR = 'ff'.repeat(16);
const LOST = '01'.repeat(16);
const PROPOSER = '11'.repeat(16);
const NEW_ADMIN = '21'.repeat(16);

const NAMES: Record<string, string> = {
  [LOST]: 'ada',
  [PROPOSER]: 'bob',
  [NEW_ADMIN]: 'cyn',
  [MY_ADDR]: 'me',
};

function resolveName(addr: string): string {
  return NAMES[addr] ?? addr.slice(0, 8);
}

function makeProposal(overrides: Partial<RecoveryProposalDto> = {}): RecoveryProposalDto {
  return {
    proposalEventId: 'b0'.repeat(16),
    proposerAddr: PROPOSER,
    lostAdminAddr: LOST,
    newAdminAddr: NEW_ADMIN,
    signerAddrs: [PROPOSER],
    signersSoFar: 1,
    threshold: 2,
    proposedAtWallMs: 100_000,
    deadlineMs: null,
    phase: 'collecting',
    vetoedByAddr: null,
    rotationEligibleAtMs: null,
    selfHasCosigned: false,
    ...overrides,
  };
}

function makeState(overrides: Partial<RecoveryStateDto> = {}): RecoveryStateDto {
  return {
    config: { designateAddrs: [PROPOSER], threshold: 2, vetoWindowMs: 604_800_000 },
    proposals: [makeProposal()],
    selfIsDesignate: false,
    selfPower: 0,
    ...overrides,
  };
}

function mockRecoveryState(state: RecoveryStateDto) {
  (invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
    if (cmd === 'get_recovery_state') return Promise.resolve(state);
    return Promise.resolve(undefined);
  });
}

function renderBanner(communityId = 'c0'.repeat(16)) {
  return render(RecoveryBanner, {
    props: {
      communityId,
      myAddress: MY_ADDR,
      resolveName,
      onOpenRecoverySettings: vi.fn(),
    },
  });
}

describe('RecoveryBanner', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    localStorage.clear();
  });

  it('renders nothing when there are no proposals', async () => {
    mockRecoveryState(makeState({ proposals: [] }));
    const { container } = renderBanner();
    await waitFor(() => expect(invoke).toHaveBeenCalledWith('get_recovery_state', expect.anything()));
    expect(container.querySelector('[data-testid="recovery-banner"]')).toBeNull();
  });

  it('shows the collecting line with the remaining-signature count', async () => {
    mockRecoveryState(makeState());
    const { getByText } = renderBanner();
    await waitFor(() => {
      expect(getByText(/Recovery of @ada proposed by @bob — 1 more signature needed/)).toBeTruthy();
    });
  });

  it('shows the time-locked line naming the new admin and deadline', async () => {
    const deadline = Date.UTC(2026, 7, 1);
    mockRecoveryState(
      makeState({
        proposals: [makeProposal({ phase: 'timeLocked', deadlineMs: deadline, signersSoFar: 2 })],
      }),
    );
    const { getByText } = renderBanner();
    await waitFor(() => {
      expect(getByText(/@cyn becomes admin of this community on .* unless a current admin vetoes/)).toBeTruthy();
    });
  });

  it('offers Co-sign only to a designate who has not signed, and invokes cosign', async () => {
    mockRecoveryState(makeState({ selfIsDesignate: true }));
    const { container } = renderBanner();
    let btn: Element | null = null;
    await waitFor(() => {
      btn = container.querySelector('button[aria-label^="Co-sign"]');
      expect(btn).toBeTruthy();
    });
    await fireEvent.click(btn!);
    await waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('cosign_admin_recovery', {
        communityId: 'c0'.repeat(16),
        proposalEventId: 'b0'.repeat(16),
      });
    });
  });

  it('hides Co-sign for a designate who already signed', async () => {
    mockRecoveryState(
      makeState({ selfIsDesignate: true, proposals: [makeProposal({ selfHasCosigned: true })] }),
    );
    const { container, getByText } = renderBanner();
    await waitFor(() => expect(getByText(/Recovery of @ada/)).toBeTruthy());
    expect(container.querySelector('button[aria-label^="Co-sign"]')).toBeNull();
  });

  it('offers Veto only at power 100 and routes it through click-confirm', async () => {
    mockRecoveryState(makeState({ selfPower: 100 }));
    const { container, getByText } = renderBanner();
    let vetoBtn: Element | null = null;
    await waitFor(() => {
      vetoBtn = container.querySelector('button[aria-label^="Veto"]');
      expect(vetoBtn).toBeTruthy();
    });
    await fireEvent.click(vetoBtn!);
    // Click-confirm modal appears; the IPC fires only on confirm.
    expect(invoke).not.toHaveBeenCalledWith('veto_admin_recovery', expect.anything());
    const confirm = getByText('Veto recovery');
    await fireEvent.click(confirm);
    await waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('veto_admin_recovery', {
        communityId: 'c0'.repeat(16),
        proposalEventId: 'b0'.repeat(16),
      });
    });
  });

  it('hides Veto below power 100', async () => {
    mockRecoveryState(makeState({ selfPower: 40 }));
    const { container, getByText } = renderBanner();
    await waitFor(() => expect(getByText(/Recovery of @ada/)).toBeTruthy());
    expect(container.querySelector('button[aria-label^="Veto"]')).toBeNull();
  });

  it('resolves a vetoed proposal to "vetoed by NAME" and dismisses durably', async () => {
    mockRecoveryState(
      makeState({
        proposals: [makeProposal({ phase: 'vetoed', vetoedByAddr: LOST })],
      }),
    );
    const { container, getByText } = renderBanner();
    await waitFor(() => {
      expect(getByText(/Recovery of @ada was vetoed by @ada/)).toBeTruthy();
    });
    const dismiss = container.querySelector('button[aria-label="Dismiss resolved recovery notice"]');
    expect(dismiss).toBeTruthy();
    await fireEvent.click(dismiss!);
    await waitFor(() => {
      expect(container.querySelector('[data-testid="recovery-banner"]')).toBeNull();
    });
  });

  it('shows rotation-pending-finality on an executed proposal until the finality wall', async () => {
    mockRecoveryState(
      makeState({
        proposals: [
          makeProposal({
            phase: 'executed',
            deadlineMs: Date.now() - 1_000,
            rotationEligibleAtMs: Date.now() + 86_400_000,
            signersSoFar: 2,
          }),
        ],
      }),
    );
    const { getByText } = renderBanner();
    await waitFor(() => {
      expect(getByText(/Admin recovery completed — @cyn replaced @ada as admin/)).toBeTruthy();
      expect(getByText(/membership key rotation pending finality/)).toBeTruthy();
    });
  });

  it('nudges the new admin to review recovery settings on execution', async () => {
    const onOpen = vi.fn();
    mockRecoveryState(
      makeState({
        proposals: [
          makeProposal({ phase: 'executed', newAdminAddr: MY_ADDR, signersSoFar: 2 }),
        ],
      }),
    );
    const { getByText } = render(RecoveryBanner, {
      props: {
        communityId: 'c0'.repeat(16),
        myAddress: MY_ADDR,
        resolveName,
        onOpenRecoverySettings: onOpen,
      },
    });
    let nudge: HTMLElement | null = null;
    await waitFor(() => {
      nudge = getByText('Review recovery settings');
      expect(nudge).toBeTruthy();
    });
    await fireEvent.click(nudge!);
    expect(onOpen).toHaveBeenCalled();
    // Dismissed durably — the nudge button does not come back.
    await waitFor(() => {
      expect(() => getByText('Review recovery settings')).toThrow();
    });
  });
});
