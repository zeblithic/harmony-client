import { describe, it, expect } from 'vitest';
import { ReadReceiptService } from '../../read-receipt-service';

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
