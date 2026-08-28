import { describe, test, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import PendingJoinsPanel from '../PendingJoinsPanel.svelte';
// ZEB-946: the "since …" HLC timestamp honors the owner's time-format prefs.
import {
    setTimeFormatSettings,
    _resetTimeFormatServiceForTest,
} from '../../time-format-service';
import { formatFullTimestamp } from '../../time-format';

vi.mock('@tauri-apps/api/core', () => ({
    invoke: vi.fn(),
}));
// ZEB-1016: the panel listens for `community-membership-updated`.
vi.mock('@tauri-apps/api/event', () => ({
    listen: vi.fn().mockResolvedValue(() => {}),
}));

describe('PendingJoinsPanel', () => {
    beforeEach(() => {
        vi.clearAllMocks();
    });

    test('renders 2 pending joins as 2 rows', async () => {
        const { invoke } = await import('@tauri-apps/api/core');
        (invoke as any).mockImplementation((cmd: string) => {
            if (cmd === 'list_pending_joins') {
                return Promise.resolve([
                    {
                        eventId: 'aaa',
                        joinerAddr: '1122334455667788',
                        pendingAtHlc: { wallMs: 1700000000000, logical: 0, deviceId: '00' },
                        inviteeHint: 'alice',
                    },
                    {
                        eventId: 'bbb',
                        joinerAddr: '5566778899AABBCC',
                        pendingAtHlc: { wallMs: 1700000001000, logical: 0, deviceId: '00' },
                    },
                ]);
            }
            if (cmd === 'list_recent_counter_signs') return Promise.resolve([]);
            return Promise.resolve(null);
        });
        const { container } = render(PendingJoinsPanel, {
            props: { communityId: 'abc', canModerate: true },
        });
        await waitFor(() => {
            expect(container.querySelectorAll('li').length).toBeGreaterThanOrEqual(2);
        });
    });

    test('Kick button calls kick_from_community IPC with correct args', async () => {
        const { invoke } = await import('@tauri-apps/api/core');
        (invoke as any).mockImplementation((cmd: string) => {
            if (cmd === 'list_pending_joins') {
                return Promise.resolve([
                    {
                        eventId: 'aaa',
                        joinerAddr: '11223344',
                        pendingAtHlc: { wallMs: 1700000000000, logical: 0, deviceId: '00' },
                    },
                ]);
            }
            if (cmd === 'list_recent_counter_signs') return Promise.resolve([]);
            if (cmd === 'kick_from_community') return Promise.resolve(null);
            return Promise.resolve(null);
        });
        const { getByText } = render(PendingJoinsPanel, {
            props: { communityId: 'abc', canModerate: true },
        });
        await waitFor(() => getByText(/reject/i));
        const btn = getByText(/reject/i);
        await fireEvent.click(btn);
        await waitFor(() => {
            expect(invoke).toHaveBeenCalledWith('kick_from_community', expect.objectContaining({
                communityId: 'abc',
                targetAddr: '11223344',
            }));
        });
    });

    test('Does not render when canModerate=false', () => {
        const { container } = render(PendingJoinsPanel, {
            props: { communityId: 'abc', canModerate: false },
        });
        expect(container.querySelector('.pending-joins-panel')).toBeNull();
    });

    test('Recent counter-signs section renders entries', async () => {
        const { invoke } = await import('@tauri-apps/api/core');
        (invoke as any).mockImplementation((cmd: string) => {
            if (cmd === 'list_pending_joins') return Promise.resolve([]);
            if (cmd === 'list_recent_counter_signs') {
                return Promise.resolve([
                    {
                        joinEventId: '111',
                        joinerAddr: 'aabbccdd',
                        countersignedAtHlc: { wallMs: 1700000000000, logical: 0, deviceId: '00' },
                    },
                ]);
            }
            return Promise.resolve(null);
        });
        const { container } = render(PendingJoinsPanel, {
            props: { communityId: 'abc', canModerate: true },
        });
        await waitFor(() => {
            const recentLis = Array.from(container.querySelectorAll('details:last-of-type li'));
            expect(recentLis.length).toBeGreaterThanOrEqual(1);
        });
    });

    test('renders a neutral CountChip with the pending count in the summary', async () => {
        const { invoke } = await import('@tauri-apps/api/core');
        (invoke as any).mockImplementation((cmd: string) => {
            if (cmd === 'list_pending_joins') {
                return Promise.resolve([
                    {
                        eventId: 'aaa',
                        joinerAddr: '1122334455667788',
                        pendingAtHlc: { wallMs: 1700000000000, logical: 0, deviceId: '00' },
                    },
                    {
                        eventId: 'bbb',
                        joinerAddr: '5566778899AABBCC',
                        pendingAtHlc: { wallMs: 1700000001000, logical: 0, deviceId: '00' },
                    },
                ]);
            }
            if (cmd === 'list_recent_counter_signs') return Promise.resolve([]);
            return Promise.resolve(null);
        });
        const { container, getByText } = render(PendingJoinsPanel, {
            props: { communityId: 'abc', canModerate: true },
        });
        await waitFor(() => {
            // ZEB-653: the parenthetical "(N)" count is now a neutral CountChip
            // (label + mono value) in the section summary.
            expect(getByText('Awaiting counter-sign')).toBeTruthy();
            const chip = container.querySelector('details:first-of-type .count-chip');
            expect(chip?.classList.contains('neutral')).toBe(true);
            expect(chip?.querySelector('.cc-value')?.textContent).toBe('2');
        });
    });

    test('membership_updated_event_triggers_refresh (ZEB-1016)', async () => {
        const { invoke } = vi.mocked(await import('@tauri-apps/api/core'));
        const { listen } = vi.mocked(await import('@tauri-apps/api/event'));

        let capturedHandler:
            | ((event: { payload: { communityId: string } }) => void)
            | null = null;
        listen.mockImplementation(((_event: string, handler: unknown) => {
            capturedHandler = handler as (event: {
                payload: { communityId: string };
            }) => void;
            return Promise.resolve(() => {});
        }) as typeof listen);
        invoke.mockResolvedValue([]);

        render(PendingJoinsPanel, {
            props: { communityId: 'abc', canModerate: true },
        });

        // Initial refresh makes two IPCs (pending + recent) and registers
        // the listener.
        await waitFor(() => {
            expect(invoke).toHaveBeenCalledTimes(2);
            expect(listen).toHaveBeenCalledWith(
                'community-membership-updated',
                expect.any(Function)
            );
            expect(capturedHandler).not.toBeNull();
        });

        // A membership event applied for THIS community → refetch (two more
        // IPCs); a different community's event is ignored.
        capturedHandler!({ payload: { communityId: 'other' } });
        await new Promise((r) => setTimeout(r, 0));
        expect(invoke).toHaveBeenCalledTimes(2);

        capturedHandler!({ payload: { communityId: 'abc' } });
        await waitFor(() => {
            expect(invoke).toHaveBeenCalledTimes(4);
            expect(invoke).toHaveBeenNthCalledWith(
                3,
                'list_pending_joins',
                expect.objectContaining({ communityId: 'abc' })
            );
        });
    });

    test('initial_fetch_waits_for_listener_registration (PR #767)', async () => {
        const { invoke } = vi.mocked(await import('@tauri-apps/api/core'));
        const { listen } = vi.mocked(await import('@tauri-apps/api/event'));

        // Subscription-before-snapshot: an update applied after the list
        // snapshot but before registration would be in neither (Tauri events
        // are not replayed), so the fetch must not start until the
        // subscription is live.
        let resolveListen: ((fn: () => void) => void) | null = null;
        listen.mockImplementation(((_event: string, _handler: unknown) =>
            new Promise<() => void>((res) => (resolveListen = res))) as typeof listen);
        invoke.mockResolvedValue([]);

        render(PendingJoinsPanel, {
            props: { communityId: 'abc', canModerate: true },
        });

        await new Promise((r) => setTimeout(r, 0));
        expect(listen).toHaveBeenCalledTimes(1);
        expect(invoke).not.toHaveBeenCalled();

        resolveListen!(() => {});
        await waitFor(() => {
            expect(invoke).toHaveBeenCalledTimes(2);
        });
    });

    test('failed_listener_registration_still_runs_initial_fetch (PR #767)', async () => {
        const { invoke } = vi.mocked(await import('@tauri-apps/api/core'));
        const { listen } = vi.mocked(await import('@tauri-apps/api/event'));
        const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});

        listen.mockRejectedValue(new Error('no event bridge'));
        invoke.mockResolvedValue([]);

        render(PendingJoinsPanel, {
            props: { communityId: 'abc', canModerate: true },
        });

        // Degraded path (non-Tauri harness): the mount fetch still happens...
        await waitFor(() => {
            expect(invoke).toHaveBeenCalledTimes(2);
        });
        // ...and the rejection is logged as a normalized message, not raw `e`.
        expect(warn).toHaveBeenCalledWith(
            'PendingJoinsPanel: failed to subscribe to membership updates',
            'no event bridge'
        );
        warn.mockRestore();
    });

    describe('honors the time-format preference (ZEB-946)', () => {
        afterEach(() => {
            _resetTimeFormatServiceForTest();
        });

        test('renders the "since" HLC timestamp in the chosen clock + order', async () => {
            setTimeFormatSettings({ clock: '24h', dateOrder: 'ymd' });
            const wallMs = 1_700_000_000_000;
            const { invoke } = await import('@tauri-apps/api/core');
            (invoke as any).mockImplementation((cmd: string) => {
                if (cmd === 'list_pending_joins') {
                    return Promise.resolve([
                        {
                            eventId: 'aaa',
                            joinerAddr: '1122334455667788',
                            pendingAtHlc: { wallMs, logical: 0, deviceId: '00' },
                            inviteeHint: 'alice',
                        },
                    ]);
                }
                if (cmd === 'list_recent_counter_signs') return Promise.resolve([]);
                return Promise.resolve(null);
            });
            const { container } = render(PendingJoinsPanel, {
                props: { communityId: 'abc', canModerate: true },
            });
            await waitFor(() => expect(container.querySelector('.time')).not.toBeNull());
            expect(container.querySelector('.time')?.textContent).toBe(
                `since ${formatFullTimestamp(wallMs, { hour12: false, dateOrder: 'ymd' })}`,
            );
        });
    });
});

describe('PendingJoinsPanel display-name resolution (ZEB-961)', () => {
    const JOINER = 'ab'.repeat(16); // 32-char owner_id hex

    beforeEach(() => {
        vi.clearAllMocks();
    });

    function mockPending(hint?: string) {
        return (cmd: string) => {
            if (cmd === 'list_pending_joins') {
                return Promise.resolve([
                    {
                        eventId: 'aaa',
                        joinerAddr: JOINER,
                        pendingAtHlc: { wallMs: 1_700_000_000_000, logical: 0, deviceId: '00' },
                        ...(hint ? { inviteeHint: hint } : {}),
                    },
                ]);
            }
            if (cmd === 'list_recent_counter_signs') return Promise.resolve([]);
            return Promise.resolve(null);
        };
    }

    test('resolves the local nickname OVER card, hint and hex', async () => {
        const { invoke } = await import('@tauri-apps/api/core');
        (invoke as any).mockImplementation(mockPending('alice'));
        const { container } = render(PendingJoinsPanel, {
            props: {
                communityId: 'abc',
                canModerate: true,
                resolveCard: (id: string) =>
                    id === JOINER ? { displayName: 'JoinerCard', statusText: '' } : undefined,
                resolveNickname: (id: string) => (id === JOINER ? 'JoinerNick' : undefined),
            },
        });
        await waitFor(() =>
            expect(container.querySelector('.joiner')?.textContent).toBe('JoinerNick'),
        );
    });

    test('honors the inviteeHint (roster rung) when no card/nickname resolves', async () => {
        const { invoke } = await import('@tauri-apps/api/core');
        (invoke as any).mockImplementation(mockPending('alice'));
        const { container } = render(PendingJoinsPanel, {
            props: {
                communityId: 'abc',
                canModerate: true,
                resolveCard: () => undefined,
                resolveNickname: () => undefined,
            },
        });
        await waitFor(() => expect(container.querySelector('.joiner')?.textContent).toBe('alice'));
    });

    test('falls back to short hex when neither resolver nor hint is present', async () => {
        const { invoke } = await import('@tauri-apps/api/core');
        (invoke as any).mockImplementation(mockPending());
        const { container } = render(PendingJoinsPanel, {
            props: { communityId: 'abc', canModerate: true },
        });
        await waitFor(() =>
            expect(container.querySelector('.joiner')?.textContent).toBe(JOINER.slice(0, 8)),
        );
    });
});
