import { describe, it, expect, vi, beforeEach } from 'vitest';
import { StorageBuddyService } from './storage-buddy-service';
import type { TauriAdapter } from './zenoh-service';

// Fake-adapter idiom from friend-service.test.ts: capture listeners in a Map
// so tests can fire backend events by hand.
function makeAdapter(): TauriAdapter & { listeners: Map<string, (event: unknown) => void> } {
  const listeners = new Map<string, (event: unknown) => void>();
  return {
    listeners,
    invoke: vi.fn(),
    listen: vi.fn(async (event: string, handler: (event: unknown) => void) => {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    }),
  } as unknown as TauriAdapter & { listeners: Map<string, (event: unknown) => void> };
}

describe('StorageBuddyService', () => {
  let adapter: ReturnType<typeof makeAdapter>;
  let service: StorageBuddyService;

  beforeEach(async () => {
    adapter = makeAdapter();
    service = new StorageBuddyService();
    await service.connectAdapter(adapter);
  });

  it('listBuddies invokes get_storage_buddies', async () => {
    const rows = [
      {
        ownerAddress: 'ab'.repeat(16),
        petName: null,
        status: 'active',
        myPledgeBytes: 1_000_000_000,
        theirPledgeBytes: 2_000_000_000,
        hostedForThemBytes: 500,
        theyReportHoldingBytes: null,
        reportAgeMs: null,
      },
    ];
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue(rows);
    const got = await service.listBuddies();
    expect(adapter.invoke).toHaveBeenCalledWith('get_storage_buddies', {});
    expect(got).toEqual(rows);
  });

  it('setPledge invokes set_buddy_pledge with camelCase args (0-byte accept valid)', async () => {
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue(undefined);
    await service.setPledge('cd'.repeat(16), 0);
    expect(adapter.invoke).toHaveBeenCalledWith('set_buddy_pledge', {
      ownerAddress: 'cd'.repeat(16),
      bytes: 0,
    });
  });

  it('removeBuddy invokes remove_storage_buddy', async () => {
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue(undefined);
    await service.removeBuddy('ef'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledWith('remove_storage_buddy', {
      ownerAddress: 'ef'.repeat(16),
    });
  });

  it('setSharedBudget invokes set_shared_budget', async () => {
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue(undefined);
    await service.setSharedBudget(10_000_000_000);
    expect(adapter.invoke).toHaveBeenCalledWith('set_shared_budget', {
      bytes: 10_000_000_000,
    });
  });

  it('getContributionSummary invokes get_contribution_summary', async () => {
    const summary = {
      hostedBytes: 42,
      budgetBytes: 10_000_000_000,
      buddyCount: 2,
      health: 'healthy',
    };
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue(summary);
    expect(await service.getContributionSummary()).toEqual(summary);
    expect(adapter.invoke).toHaveBeenCalledWith('get_contribution_summary', {});
  });

  it('subscribes both backend events and onChange fires for either', async () => {
    expect([...adapter.listeners.keys()].sort()).toEqual([
      'contribution-updated',
      'storage-buddies-updated',
    ]);

    const cb = vi.fn();
    service.onChange(cb);
    adapter.listeners.get('storage-buddies-updated')!(null);
    expect(cb).toHaveBeenCalledTimes(1);
    adapter.listeners.get('contribution-updated')!(null);
    expect(cb).toHaveBeenCalledTimes(2);
  });

  it('onChange unsubscribe removes only that listener', async () => {
    const a = vi.fn();
    const b = vi.fn();
    const offA = service.onChange(a);
    service.onChange(b);
    offA();
    adapter.listeners.get('storage-buddies-updated')!(null);
    expect(a).not.toHaveBeenCalled();
    expect(b).toHaveBeenCalledTimes(1);
  });

  it('destroy unsubscribes backend listeners and clears callbacks', async () => {
    const cb = vi.fn();
    service.onChange(cb);
    service.destroy();
    expect(adapter.listeners.size).toBe(0);
  });

  it('normalizes string rejections into Error (prod rejections are strings)', async () => {
    (adapter.invoke as ReturnType<typeof vi.fn>).mockRejectedValue(
      'pledge cap reached (64 buddies)'
    );
    await expect(service.setPledge('ab'.repeat(16), 1)).rejects.toThrow(
      'pledge cap reached (64 buddies)'
    );
  });

  it('throws before connectAdapter', async () => {
    const fresh = new StorageBuddyService();
    await expect(fresh.listBuddies()).rejects.toThrow('adapter not connected');
  });
});
