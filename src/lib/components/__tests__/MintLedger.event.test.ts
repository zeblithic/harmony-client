import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, waitFor } from '@testing-library/svelte';
import MintLedger from '../MintLedger.svelte';
import { createMockAdapter } from '../../test-utils';

// Stub @tauri-apps/api/event with a controllable emitter.
const listeners = new Map<string, Array<(e: { payload: unknown }) => void>>();
vi.mock('@tauri-apps/api/event', () => ({
  listen: (event: string, cb: (e: { payload: unknown }) => void) => {
    const arr = listeners.get(event) ?? [];
    arr.push(cb);
    listeners.set(event, arr);
    return Promise.resolve(() => {
      const idx = arr.indexOf(cb);
      if (idx >= 0) arr.splice(idx, 1);
    });
  },
}));

// Stub @tauri-apps/plugin-dialog so the save dialog doesn't open.
vi.mock('@tauri-apps/plugin-dialog', () => ({
  save: vi.fn().mockResolvedValue(null),
}));

function emitMintChanged() {
  for (const cb of listeners.get('mint-changed') ?? []) cb({ payload: undefined });
}

describe('MintLedger reacts to mint-changed event', () => {
  beforeEach(() => listeners.clear());

  it('calls list_transactions again when mint-changed fires', async () => {
    const { adapter } = createMockAdapter();
    const mockInvoke = adapter.invoke as ReturnType<typeof vi.fn>;
    // Wire up the three IPC calls MintLedger makes on load.
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'mint_list_transactions') return Promise.resolve([]);
      if (cmd === 'mint_list_accounts') return Promise.resolve([]);
      if (cmd === 'mint_get_default_currency') return Promise.resolve('USD');
      return Promise.resolve(undefined);
    });

    render(MintLedger, { props: { adapter } });

    // Wait for initial load to complete (list_transactions called at least once).
    await waitFor(() =>
      expect(mockInvoke).toHaveBeenCalledWith('mint_list_transactions', expect.any(Object)),
    );
    const initialCalls = mockInvoke.mock.calls.length;

    // Fire the mint-changed event.
    emitMintChanged();

    // A new load() cycle must issue another mint_list_transactions call.
    await waitFor(() => {
      const newCalls = mockInvoke.mock.calls.length;
      expect(newCalls).toBeGreaterThan(initialCalls);
    });
  });
});
