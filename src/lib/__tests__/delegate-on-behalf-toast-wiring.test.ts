import { describe, it, expect, beforeEach } from 'vitest';
import { get } from 'svelte/store';
import { toastStore, toastsStore } from '../stores/toast';
import {
  setupDelegateOnBehalfToast,
  formatDelegateOnBehalfMessage,
  shortAddr,
} from '../voting-toast-wiring';
import { VotingAdapter } from '../voting-adapter';

describe('delegate-on-behalf toast wiring', () => {
  let adapter: VotingAdapter;

  beforeEach(() => {
    adapter = new VotingAdapter();
    // Clear any existing toasts so each test starts from an empty list.
    const toasts = get(toastsStore);
    toasts.forEach((t) => toastStore.dismiss(t.id));
  });

  it('shortAddr returns the first 8 hex chars', () => {
    expect(shortAddr('0123456789abcdef0123456789abcdef')).toBe('01234567');
  });

  it('formats the message with support verb', () => {
    const msg = formatDelegateOnBehalfMessage({
      communityId: 'aa'.repeat(16),
      proposalId: 'bb'.repeat(16),
      delegate: 'cc'.repeat(16),
      support: true,
    });
    expect(msg).toMatch(/signaled support for/);
    expect(msg).toMatch(/cccccccc/); // shortAddr of delegate
    expect(msg).toMatch(/bbbbbbbb/); // shortAddr of proposal
  });

  it('formats the message with against verb', () => {
    const msg = formatDelegateOnBehalfMessage({
      communityId: 'aa'.repeat(16),
      proposalId: 'bb'.repeat(16),
      delegate: 'cc'.repeat(16),
      support: false,
    });
    expect(msg).toMatch(/signaled against/);
    expect(msg).not.toMatch(/signaled support for/);
  });

  it('subscribed handler shows a toast on event', () => {
    setupDelegateOnBehalfToast(adapter);
    // The internal subscribers list on the adapter holds our handler.
    // Reach in to trigger it directly — exercising the full Tauri
    // dispatch is voting-adapter's responsibility, covered by its own
    // tests. Here we verify the wiring contract (subscribe → toast).
    const subs = (
      adapter as unknown as {
        delegateSignaledOnYourBehalfSubs: Array<
          (p: {
            communityId: string;
            proposalId: string;
            delegate: string;
            support: boolean;
          }) => void
        >;
      }
    ).delegateSignaledOnYourBehalfSubs;
    expect(subs.length).toBeGreaterThan(0);
    subs[0]({
      communityId: 'a'.repeat(32),
      proposalId: 'b'.repeat(32),
      delegate: 'c'.repeat(32),
      support: true,
    });
    const toasts = get(toastsStore);
    expect(toasts.length).toBe(1);
    expect(toasts[0].message).toMatch(/signaled support for/);
    expect(toasts[0].message).toMatch(/cccccccc/);
    expect(toasts[0].durationMs).toBe(5000);
  });

  it('unsubscribe removes the handler', () => {
    const unsub = setupDelegateOnBehalfToast(adapter);
    const subs = (
      adapter as unknown as {
        delegateSignaledOnYourBehalfSubs: Array<unknown>;
      }
    ).delegateSignaledOnYourBehalfSubs;
    expect(subs.length).toBe(1);
    unsub();
    expect(subs.length).toBe(0);
  });
});
