/**
 * ZEB-298 PR 2 Task 10 — extracted toast wiring for the
 * `voting-delegate-signaled-on-your-behalf` Tauri event.
 *
 * Lives outside `App.svelte` so the wiring is testable without
 * spinning up the full root component lifecycle. App.svelte calls
 * `setupDelegateOnBehalfToast(votingAdapter)` once the
 * `TauriAdapter` is established and `votingAdapter.connectAdapter`
 * has resolved — at that point the underlying Tauri listener is
 * already wired and any future event will reach the handler we
 * register here.
 */

import type { VotingAdapter } from './voting-adapter';
import { toastStore } from './stores/toast';
import type { VotingDelegateSignaledOnYourBehalfPayload } from './types/voting';

/** Shorten a 32-hex `OwnerAddr` (16 bytes) to its first 8 chars
 *  (= first 4 bytes). Matches the abbreviation convention used in
 *  message senders elsewhere (see `message-service.ts` display rows). */
export function shortAddr(hex: string): string {
  return hex.slice(0, 8);
}

/** Render the toast message for a delegate-on-behalf event. The
 *  message format is locked by ZEB-298 Task 10:
 *
 *      Delegate {short(delegate)} signaled {support|against}
 *      proposal {short(proposalId)}
 *
 *  Future enrichment (resolving `delegate` to a profile display
 *  name) is a follow-up — keeps this task's surface minimal. */
export function formatDelegateOnBehalfMessage(
  payload: VotingDelegateSignaledOnYourBehalfPayload,
): string {
  const verb = payload.support ? 'signaled support for' : 'signaled against';
  return `Delegate ${shortAddr(payload.delegate)} ${verb} proposal ${shortAddr(payload.proposalId)}`;
}

/** Subscribe the toast UI to delegate-on-behalf events.
 *
 *  Returns the unsubscribe closure from the underlying
 *  `VotingAdapter`. The default 5000ms toast duration matches the
 *  ZEB-298 plan; manual dismissal is also possible via the Toast
 *  component's close button. */
export function setupDelegateOnBehalfToast(adapter: VotingAdapter): () => void {
  return adapter.subscribeDelegateSignaledOnYourBehalf((payload) => {
    toastStore.show(formatDelegateOnBehalfMessage(payload), 5000);
  });
}

/** ZEB-607 D6 — signed-vote feedback toasts. ~2.1s dwell matches the
 *  design prototype; deliberately shorter than the 5s delegate-on-
 *  behalf toast above (whose copy is locked by ZEB-298 Task 10). */
const SIGNED_TOAST_MS = 2100;

export function showSignalCastToast(support: boolean): void {
  toastStore.show(
    support
      ? '✓ Support signaled · signed with your key'
      : '✓ Support withdrawn · signed with your key',
    SIGNED_TOAST_MS,
  );
}

export function showDelegationToast(delegateName: string): void {
  toastStore.show(`↪ Proxied to ${delegateName}`, SIGNED_TOAST_MS);
}

export function showRecallToast(): void {
  toastStore.show('↩ Delegation recalled — your vote is yours again', SIGNED_TOAST_MS);
}
