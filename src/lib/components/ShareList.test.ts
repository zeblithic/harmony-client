import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import ShareList from './ShareList.svelte';
import type { FileGrant } from '../types';

const GRANT_A: FileGrant = {
  granteeAddress: 'addr-alice',
  displayName: 'Alice',
  grantedAt: 1000,
};
const GRANT_B: FileGrant = {
  granteeAddress: 'addr-bob',
  displayName: null,
  grantedAt: 2000,
};

const FRIENDS = [
  { address: 'addr-alice', displayName: 'Alice' },
  { address: 'addr-bob', displayName: 'Bob' },
  { address: 'addr-carol', displayName: 'Carol' },
];

function renderShareList(overrides: Record<string, unknown> = {}) {
  return render(ShareList, {
    props: {
      grants: null,
      availableFriends: FRIENDS,
      isEncrypted: true,
      onGrant: vi.fn().mockResolvedValue(undefined),
      onRevoke: vi.fn().mockResolvedValue(undefined),
      ...overrides,
    },
  });
}

describe('ShareList', () => {
  // ── Honesty: unresolved vs proven-empty ─────────────────────────────
  it('renders nothing (not the empty state) while grants is unresolved (null)', () => {
    renderShareList({ grants: null });
    expect(screen.queryByText('Not shared with anyone')).toBeNull();
    expect(screen.queryByLabelText('Shared with (can view)')).toBeNull();
  });

  it('renders "Not shared with anyone" only once grants has resolved to []', () => {
    renderShareList({ grants: [] });
    expect(screen.getByText('Not shared with anyone')).toBeTruthy();
    expect(screen.getByLabelText('Shared with (can view)')).toBeTruthy();
  });

  // ── One row per grant + Revoke ───────────────────────────────────────
  it('renders one row per grant, showing displayName ?? granteeAddress', () => {
    renderShareList({ grants: [GRANT_A, GRANT_B] });
    expect(screen.getByText('Alice')).toBeTruthy();
    // GRANT_B has no displayName — falls back to the raw address.
    expect(screen.getByText('addr-bob')).toBeTruthy();
    expect(screen.queryByText('Not shared with anyone')).toBeNull();
  });

  it('the Revoke control calls onRevoke with the grantee address', async () => {
    const onRevoke = vi.fn().mockResolvedValue(undefined);
    renderShareList({ grants: [GRANT_A], onRevoke });
    const btn = screen.getByRole('button', { name: 'Revoke Alice' });
    await fireEvent.click(btn);
    expect(onRevoke).toHaveBeenCalledWith('addr-alice');
  });

  it('strips the ineligible: prefix from a revoke rejection', async () => {
    const onRevoke = vi.fn().mockRejectedValue(new Error('ineligible: not a friend'));
    renderShareList({ grants: [GRANT_A], onRevoke });
    await fireEvent.click(screen.getByRole('button', { name: 'Revoke Alice' }));
    await waitFor(() => {
      expect(screen.getByRole('alert').textContent).toBe('Not eligible: not a friend');
    });
  });

  // ── Picker excludes already-granted friends + calls onGrant ─────────
  it('the picker excludes already-granted friends', () => {
    renderShareList({ grants: [GRANT_A] });
    const select = screen.getByLabelText('Share with...') as HTMLSelectElement;
    const optionValues = Array.from(select.options).map((o) => o.value);
    expect(optionValues).not.toContain('addr-alice');
    expect(optionValues).toContain('addr-bob');
    expect(optionValues).toContain('addr-carol');
  });

  it('picking a friend calls onGrant with their address', async () => {
    const onGrant = vi.fn().mockResolvedValue(undefined);
    renderShareList({ grants: [GRANT_A], onGrant });
    const select = screen.getByLabelText('Share with...') as HTMLSelectElement;
    await fireEvent.change(select, { target: { value: 'addr-bob' } });
    expect(onGrant).toHaveBeenCalledWith('addr-bob');
  });

  it('strips the ineligible: prefix from a grant rejection', async () => {
    const onGrant = vi.fn().mockRejectedValue(new Error('ineligible: unencrypted content'));
    renderShareList({ grants: [], onGrant });
    const select = screen.getByLabelText('Share with...') as HTMLSelectElement;
    await fireEvent.change(select, { target: { value: 'addr-alice' } });
    await waitFor(() => {
      expect(screen.getByRole('alert').textContent).toBe('Not eligible: unencrypted content');
    });
  });

  it('hides the picker once every available friend is already granted', () => {
    renderShareList({
      grants: [
        { granteeAddress: 'addr-alice', displayName: 'Alice', grantedAt: 1 },
        { granteeAddress: 'addr-bob', displayName: 'Bob', grantedAt: 2 },
        { granteeAddress: 'addr-carol', displayName: 'Carol', grantedAt: 3 },
      ],
    });
    expect(screen.queryByLabelText('Share with...')).toBeNull();
  });

  // ── Gated off entirely on public (unencrypted) files ─────────────────
  it('renders nothing when isEncrypted is false, even with resolved grants', () => {
    renderShareList({ grants: [GRANT_A], isEncrypted: false });
    expect(screen.queryByLabelText('Shared with (can view)')).toBeNull();
    expect(screen.queryByText('Not shared with anyone')).toBeNull();
    expect(screen.queryByText('Alice')).toBeNull();
  });
});
