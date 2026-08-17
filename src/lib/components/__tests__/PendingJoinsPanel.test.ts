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
