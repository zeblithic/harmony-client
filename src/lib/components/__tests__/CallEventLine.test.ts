// ZEB-357 — call-event system line rendered in DM feeds in place of a bubble.
import { describe, it, expect, afterEach } from 'vitest';
import { render, screen, cleanup } from '@testing-library/svelte';
import CallEventLine from '../CallEventLine.svelte';
import { formatMessageTimestamp, formatFullTimestamp } from '../../time-format';
import type { Message } from '../../types';

afterEach(cleanup);

function callMessage(
  outcome: 'answered' | 'no_answer' | 'declined' | 'busy' | 'canceled',
  opts: { isSelf?: boolean; durationMs?: number } = {},
): { message: Message; isSelf: boolean } {
  const message: Message = {
    id: `cid-${outcome}`,
    sender: opts.isSelf
      ? { address: 'self', displayName: 'You' }
      : { address: 'peer-hex', displayName: 'Peer' },
    text: 'fallback',
    timestamp: 1_700_000_000_000,
    media: [],
    priority: 'standard',
    channel: 'space-1',
    hub: '',
    callEvent: {
      v: 1,
      callId: 'ab'.repeat(16),
      outcome,
      ...(opts.durationMs !== undefined ? { durationMs: opts.durationMs } : {}),
    },
  };
  return { message, isSelf: opts.isSelf ?? false };
}

describe('CallEventLine', () => {
  it('renders the recipient label for a missed call with the missed accent', () => {
    const { message, isSelf } = callMessage('no_answer');
    render(CallEventLine, { props: { message, isSelf } });
    const line = screen.getByTestId('call-event-line');
    expect(line.textContent).toContain('Missed call');
    expect(line.classList.contains('missed')).toBe(true);
  });

  it('renders the author label with duration for an answered call, without the accent', () => {
    const { message, isSelf } = callMessage('answered', { isSelf: true, durationMs: 263_000 });
    render(CallEventLine, { props: { message, isSelf } });
    const line = screen.getByTestId('call-event-line');
    expect(line.textContent).toContain('Voice call · 4m 23s');
    expect(line.classList.contains('missed')).toBe(false);
  });

  it('a canceled call is missed for the recipient but not for the author', () => {
    const recipient = callMessage('canceled');
    render(CallEventLine, { props: { message: recipient.message, isSelf: recipient.isSelf } });
    expect(screen.getByTestId('call-event-line').textContent).toContain('Missed call');
    cleanup();
    const author = callMessage('canceled', { isSelf: true });
    render(CallEventLine, { props: { message: author.message, isSelf: author.isSelf } });
    const line = screen.getByTestId('call-event-line');
    expect(line.textContent).toContain('Call canceled');
    expect(line.classList.contains('missed')).toBe(false);
  });

  it('marks a failed record so the caller knows the callee will never see it', () => {
    const { message, isSelf } = callMessage('no_answer', { isSelf: true });
    message.deliveryState = 'failed';
    render(CallEventLine, { props: { message, isSelf } });
    expect(screen.getByTestId('call-event-line').textContent).toContain('not delivered');
  });

  it('does not show the failure marker for delivered or received lines', () => {
    const { message, isSelf } = callMessage('no_answer', { isSelf: true });
    message.deliveryState = 'delivered';
    render(CallEventLine, { props: { message, isSelf } });
    expect(screen.getByTestId('call-event-line').textContent).not.toContain('not delivered');
  });

  it('shows the ZEB-943 date-aware timestamp with a full-datetime tooltip', () => {
    const { message, isSelf } = callMessage('declined');
    render(CallEventLine, { props: { message, isSelf } });
    const time = screen.getByTestId('call-event-line').querySelector('.time') as HTMLElement;
    // Wiring contract: the visible label is the shared date-aware formatter's
    // output and the tooltip carries the full datetime (format correctness is
    // pinned in time-format.test.ts). The fixture timestamp (2023) is always a
    // past year, so a date component is always rendered.
    const ts = 1_700_000_000_000;
    expect(time.textContent).toBe(formatMessageTimestamp(ts, Date.now()));
    expect(time.getAttribute('title')).toBe(formatFullTimestamp(ts));
    // PR #692 review: semantic <time datetime> for parity/a11y with the other
    // message surfaces.
    expect(time.tagName.toLowerCase()).toBe('time');
    expect(time.getAttribute('datetime')).toBe(new Date(ts).toISOString());
  });
});
