import { describe, it, expect, beforeEach } from 'vitest';
import { get } from 'svelte/store';
import { toastStore, toastsStore } from '../stores/toast';
import {
  showSignalCastToast,
  showDelegationToast,
  showRecallToast,
} from '../voting-toast-wiring';

describe('signed-vote toasts (ZEB-607 D6)', () => {
  beforeEach(() => {
    for (const t of get(toastsStore)) toastStore.dismiss(t.id);
  });
  it('support cast', () => {
    showSignalCastToast(true);
    const toasts = get(toastsStore);
    expect(toasts[toasts.length - 1].message).toBe('✓ Support signaled · signed with your key');
    expect(toasts[toasts.length - 1].durationMs).toBe(2100);
  });
  it('support withdrawn', () => {
    showSignalCastToast(false);
    expect(get(toastsStore).at(-1)?.message).toBe('✓ Support withdrawn · signed with your key');
  });
  it('delegation', () => {
    showDelegationToast('Heating WG');
    expect(get(toastsStore).at(-1)?.message).toBe('↪ Proxied to Heating WG');
  });
  it('recall', () => {
    showRecallToast();
    expect(get(toastsStore).at(-1)?.message).toBe(
      '↩ Delegation recalled — your vote is yours again',
    );
  });
});
