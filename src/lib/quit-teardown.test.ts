// PR #494 R1 (CodeRabbit Major): the bounded quit teardown must also drain
// in-flight call-event DM writes — recordCallOutcome's send_dm is detached, so
// without this, hanging up and immediately quitting could kill the process
// before the only durable record of the call reached the outbox.
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { runQuitTeardown } from './quit-teardown';

function deferred<T = void>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

describe('runQuitTeardown', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it('resolves once teardowns and pending writes settle, without warning', async () => {
    const warn = vi.fn();
    const write = deferred();
    const run = runQuitTeardown({
      teardowns: [Promise.resolve()],
      pendingWrites: () => [write.promise],
      timeoutMs: 1500,
      warn,
    });
    write.resolve();
    await run;
    expect(warn).not.toHaveBeenCalled();
  });

  it('snapshots pending writes AFTER teardowns settle (a write registered during end() is drained)', async () => {
    const warn = vi.fn();
    const lateWrite = deferred();
    let lateWriteSettled = false;
    void lateWrite.promise.then(() => { lateWriteSettled = true; });
    const writes: Promise<unknown>[] = [];
    // Simulate callSession.end(): the record write is registered while the
    // teardown promise is settling, not before runQuitTeardown was called.
    const teardown = Promise.resolve().then(() => {
      writes.push(lateWrite.promise);
    });
    let done = false;
    const run = runQuitTeardown({
      teardowns: [teardown],
      pendingWrites: () => [...writes],
      timeoutMs: 1500,
      warn,
    }).then(() => { done = true; });
    await vi.advanceTimersByTimeAsync(0);
    expect(done).toBe(false); // still waiting on the late write
    lateWrite.resolve();
    await vi.advanceTimersByTimeAsync(0);
    await run;
    expect(lateWriteSettled).toBe(true);
    expect(warn).not.toHaveBeenCalled();
  });

  it('a hung write is bounded: returns at timeoutMs with a warning', async () => {
    const warn = vi.fn();
    const hung = deferred();
    let done = false;
    const run = runQuitTeardown({
      teardowns: [Promise.resolve()],
      pendingWrites: () => [hung.promise],
      timeoutMs: 1500,
      warn,
    }).then(() => { done = true; });
    await vi.advanceTimersByTimeAsync(1499);
    expect(done).toBe(false);
    await vi.advanceTimersByTimeAsync(1);
    await run;
    expect(warn).toHaveBeenCalledOnce();
  });

  it('rejecting teardowns and writes never block the quit', async () => {
    const warn = vi.fn();
    await runQuitTeardown({
      teardowns: [Promise.reject(new Error('leave failed'))],
      pendingWrites: () => [Promise.reject(new Error('send failed'))],
      timeoutMs: 1500,
      warn,
    });
    expect(warn).not.toHaveBeenCalled();
  });
});
