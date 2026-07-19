import { describe, it, expect, vi, beforeEach } from 'vitest';
import { RecoveryAlertService, type RecoveryAlertDeps } from './recovery-alert';
import type { RecoveryProposalDto, RecoveryStateDto } from './recovery-types';

function makeProposal(overrides: Partial<RecoveryProposalDto> = {}): RecoveryProposalDto {
  return {
    proposalEventId: 'b0'.repeat(16),
    proposerAddr: '11'.repeat(16),
    lostAdminAddr: '01'.repeat(16),
    newAdminAddr: '21'.repeat(16),
    signerAddrs: ['11'.repeat(16)],
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
    config: { designateAddrs: ['11'.repeat(16), '12'.repeat(16)], threshold: 2, vetoWindowMs: 604_800_000 },
    proposals: [makeProposal()],
    selfIsDesignate: false,
    selfPower: 100,
    ...overrides,
  };
}

function makeDeps(focused: boolean): RecoveryAlertDeps & {
  toasts: string[];
  osNotifications: { title: string; body: string }[];
} {
  const toasts: string[] = [];
  const osNotifications: { title: string; body: string }[] = [];
  return {
    toasts,
    osNotifications,
    isFocused: () => focused,
    sendOsNotification: (o) => osNotifications.push(o),
    showToast: (m) => toasts.push(m),
  };
}

describe('RecoveryAlertService', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('does nothing below power 100', async () => {
    const deps = makeDeps(true);
    const svc = new RecoveryAlertService(deps);
    await svc.onRecoveryState('com-1', 'Zeblithic', makeState({ selfPower: 40 }));
    expect(deps.toasts).toHaveLength(0);
    expect(deps.osNotifications).toHaveLength(0);
  });

  it('toasts a collecting proposal with the remaining-signature count when focused', async () => {
    const deps = makeDeps(true);
    const svc = new RecoveryAlertService(deps);
    await svc.onRecoveryState('com-1', 'Zeblithic', makeState());
    expect(deps.toasts).toHaveLength(1);
    expect(deps.toasts[0]).toContain('Zeblithic');
    expect(deps.toasts[0]).toContain('1 more signature');
    expect(deps.toasts[0]).toContain('veto');
    expect(deps.osNotifications).toHaveLength(0);
  });

  it('sends an OS notification when unfocused, with time-locked copy naming the date', async () => {
    const deps = makeDeps(false);
    const svc = new RecoveryAlertService(deps);
    const deadline = Date.UTC(2026, 7, 1);
    await svc.onRecoveryState(
      'com-1',
      'Zeblithic',
      makeState({
        proposals: [makeProposal({ phase: 'timeLocked', deadlineMs: deadline, signersSoFar: 2 })],
      }),
    );
    expect(deps.osNotifications).toHaveLength(1);
    expect(deps.osNotifications[0].title).toContain('admin recovery');
    expect(deps.osNotifications[0].body).toContain('executes on');
    expect(deps.toasts).toHaveLength(0);
  });

  it('dedupes per (community, proposal, phase) but re-notifies on a phase change', async () => {
    const deps = makeDeps(true);
    const svc = new RecoveryAlertService(deps);
    const collecting = makeState();
    await svc.onRecoveryState('com-1', 'Zeblithic', collecting);
    await svc.onRecoveryState('com-1', 'Zeblithic', collecting);
    expect(deps.toasts).toHaveLength(1);

    // Same proposal enters time-locked → one more notification.
    await svc.onRecoveryState(
      'com-1',
      'Zeblithic',
      makeState({
        proposals: [makeProposal({ phase: 'timeLocked', deadlineMs: 700_000, signersSoFar: 2 })],
      }),
    );
    expect(deps.toasts).toHaveLength(2);
  });

  it('ignores terminal phases', async () => {
    const deps = makeDeps(true);
    const svc = new RecoveryAlertService(deps);
    await svc.onRecoveryState(
      'com-1',
      'Zeblithic',
      makeState({
        proposals: [
          makeProposal({ phase: 'executed' }),
          makeProposal({ proposalEventId: 'b1'.repeat(16), phase: 'vetoed', vetoedByAddr: '01'.repeat(16) }),
          makeProposal({ proposalEventId: 'b2'.repeat(16), phase: 'expired' }),
          makeProposal({ proposalEventId: 'b3'.repeat(16), phase: 'stalled' }),
        ],
      }),
    );
    expect(deps.toasts).toHaveLength(0);
    expect(deps.osNotifications).toHaveLength(0);
  });

  it('falls back to toast when the focus query throws', async () => {
    const deps = makeDeps(true);
    deps.isFocused = () => {
      throw new Error('window gone');
    };
    const svc = new RecoveryAlertService(deps);
    await svc.onRecoveryState('com-1', 'Zeblithic', makeState());
    expect(deps.toasts).toHaveLength(1);
    expect(deps.osNotifications).toHaveLength(0);
  });
});
