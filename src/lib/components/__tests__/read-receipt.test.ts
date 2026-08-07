import { describe, it, expect } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import { ReadReceiptService } from '../../read-receipt-service';
import TextFeed from '../TextFeed.svelte';

function fakeAdapter() {
  let cb: ((e: { payload: unknown }) => void) | undefined;
  return {
    listen: async (_name: string, fn: (e: { payload: unknown }) => void) => {
      cb = fn;
      return () => {};
    },
    emit: (payload: unknown) => cb?.({ payload }),
  };
}

describe('ReadReceiptService', () => {
  it('tracks the latest per-space watermark and seen-time, monotonically', async () => {
    const a = fakeAdapter();
    const svc = new ReadReceiptService();
    await svc.init(a as never);
    a.emit({ spaceId: 'aa', from: 'bb', readUpTo: 100, at: 150 });
    expect(svc.getWatermark('aa')).toBe(100);
    expect(svc.getSeenAt('aa')).toBe(150);
    // A newer watermark advances.
    a.emit({ spaceId: 'aa', from: 'bb', readUpTo: 200, at: 250 });
    expect(svc.getWatermark('aa')).toBe(200);
    // A stale (older) watermark is ignored.
    a.emit({ spaceId: 'aa', from: 'bb', readUpTo: 50, at: 999 });
    expect(svc.getWatermark('aa')).toBe(200);
    expect(svc.getSeenAt('aa')).toBe(250);
    // Unknown space → undefined.
    expect(svc.getWatermark('zz')).toBeUndefined();
  });
});

describe('TextFeed read-receipt toggle', () => {
  it('renders a toggle in a 1:1 DM header and reports changes', async () => {
    const calls: boolean[] = [];
    const { getByTestId } = render(TextFeed, {
      props: {
        messages: [],
        channelType: 'dm',
        channelName: 'Alice',
        channelId: 'aa'.repeat(16),
        ownAddress: 'me',
        readReceiptOn: false,
        onToggleReadReceipt: (on: boolean) => calls.push(on),
      },
    });
    const toggle = getByTestId('read-receipt-toggle');
    await fireEvent.click(toggle);
    expect(calls).toEqual([true]); // off → on
  });

  it('shows one "Seen HH:MM" under the newest own message within the watermark', () => {
    const mk = (id: string, ts: number, self: boolean) => ({
      id,
      text: id,
      timestamp: ts,
      media: [],
      priority: 'standard' as const,
      sender: { address: self ? 'self' : 'peer', displayName: self ? 'Me' : 'Them' },
    });
    const { queryAllByText } = render(TextFeed, {
      props: {
        channelType: 'dm',
        channelId: 'aa'.repeat(16),
        ownAddress: 'me',
        // own@100 (seen), peer@150, own@200 (after watermark → NOT seen)
        messages: [mk('a', 100, true), mk('b', 150, false), mk('c', 200, true)],
        peerReadUpToMs: 150,
        peerSeenAtMs: 160,
      },
    });
    // Exactly one "Seen …" line, attached to the own message at ts=100.
    expect(queryAllByText(/^Seen /).length).toBe(1);
  });
});
