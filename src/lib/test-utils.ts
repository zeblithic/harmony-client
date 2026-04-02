import { vi } from 'vitest';
import type { TauriAdapter } from './zenoh-service';

/** Creates a mock TauriAdapter with an emit helper for triggering events in tests. */
export function createMockAdapter() {
  const listeners = new Map<string, (event: { payload: unknown }) => void>();
  const unlisten = vi.fn();
  const adapter: TauriAdapter = {
    invoke: vi.fn().mockResolvedValue(undefined),
    listen: vi.fn().mockImplementation((event: string, handler: (event: { payload: unknown }) => void) => {
      listeners.set(event, handler);
      return Promise.resolve(unlisten);
    }),
  };
  function emit(event: string, payload: unknown) {
    listeners.get(event)?.({ payload });
  }
  return { adapter, emit, unlisten };
}
