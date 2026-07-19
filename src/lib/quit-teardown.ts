/**
 * ZEB-356 quit teardown, extracted from App.svelte for testability in
 * PR #494 R1 (CodeRabbit): the bounded drain must also cover in-flight
 * call-event DM writes. `recordCallOutcome`'s `send_dm` is detached from
 * `callSession.end()`, so without draining it here, hanging up and
 * immediately quitting could terminate the process before the only durable
 * record of the call reached the outbox.
 *
 * Order matters: the pending-write set is snapshotted AFTER the teardowns
 * settle, because `callSession.end()` is what registers the terminal
 * call-event write (via onCallOutcome) — a snapshot taken up front would
 * miss it. Everything races the single `timeoutMs` bound: quit must never
 * hang on a wedged send.
 */
export interface QuitTeardownDeps {
  /** Voice/call leave promises (already error-swallowed or raced via
   *  allSettled — a rejection must not block quit). */
  teardowns: Array<Promise<unknown>>;
  /** Snapshot of in-flight durable writes; called after `teardowns` settle. */
  pendingWrites: () => Array<Promise<unknown>>;
  timeoutMs?: number;
  warn: (msg: string) => void;
}

export async function runQuitTeardown(deps: QuitTeardownDeps): Promise<void> {
  const timeoutMs = deps.timeoutMs ?? 1500;
  const drain = Promise.allSettled(deps.teardowns).then(() =>
    Promise.allSettled(deps.pendingWrites()),
  );
  const timedOut = Symbol('timeout');
  let timer: ReturnType<typeof setTimeout> | undefined;
  const raced = await Promise.race([
    drain,
    new Promise((r) => { timer = setTimeout(() => r(timedOut), timeoutMs); }),
  ]);
  clearTimeout(timer);
  if (raced === timedOut) {
    deps.warn(`quit teardown exceeded ${timeoutMs}ms; quitting anyway`);
  }
}
