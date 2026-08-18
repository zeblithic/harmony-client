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

  // ── ZEB-782: why the picker is absent ────────────────────────────────
  //
  // Two states used to be indistinguishable: "you have no friends, so this
  // feature cannot work yet" and "you have already shared with everyone".
  // Both rendered as a bare "Not shared with anyone" with no picker, which
  // reads as a broken feature in the first case and a finished job in the
  // second. Each assertion below pins the *distinction*, not merely the
  // presence of some text — checking only that a hint exists would pass
  // with one hint reused for both states, which is the bug.
  describe('empty-picker explanation — ZEB-782', () => {
    it('tells a user with no friends that sharing is friend-gated', () => {
      renderShareList({ grants: [], availableFriends: [] });
      const hint = screen.getByTestId('share-picker-hint').textContent ?? '';
      expect(hint).toMatch(/add a friend/i);
      // The precondition must be stated, since the release notes previously
      // implied community membership was enough.
      expect(hint).toMatch(/community membership/i);
      expect(screen.queryByLabelText('Share with...')).toBeNull();
    });

    it('tells a user who granted everyone that the job is done, not broken', () => {
      renderShareList({
        grants: [
          { granteeAddress: 'addr-alice', displayName: 'Alice', grantedAt: 1 },
          { granteeAddress: 'addr-bob', displayName: 'Bob', grantedAt: 2 },
          { granteeAddress: 'addr-carol', displayName: 'Carol', grantedAt: 3 },
        ],
      });
      const hint = screen.getByTestId('share-picker-hint').textContent ?? '';
      expect(hint).toMatch(/already has access/i);
      // Must NOT tell someone who has friends to go add one.
      expect(hint).not.toMatch(/add a friend/i);
    });

    it('stays quiet when the picker is usable — a hint would be noise', () => {
      renderShareList({ grants: [] });
      expect(screen.queryByTestId('share-picker-hint')).toBeNull();
      expect(screen.getByLabelText('Share with...')).toBeTruthy();
    });

    it('says nothing before grants resolve, so the hint cannot pre-empt the query', () => {
      // `grants: null` means listGrants has not returned. Claiming "everyone
      // already has access" here would be a guess, and claiming "add a
      // friend" would be wrong for a user who has some.
      renderShareList({ grants: null, availableFriends: [] });
      expect(screen.queryByTestId('share-picker-hint')).toBeNull();
    });
  });

  // ── Gated off entirely on public (unencrypted) files ─────────────────
  it('renders nothing when isEncrypted is false, even with resolved grants', () => {
    renderShareList({ grants: [GRANT_A], isEncrypted: false });
    expect(screen.queryByLabelText('Shared with (can view)')).toBeNull();
    expect(screen.queryByText('Not shared with anyone')).toBeNull();
    expect(screen.queryByText('Alice')).toBeNull();
  });
});

// ── ZEB-960: never render a blank label or leak the full grantee address ──
//
// A grantee's card displayName has no non-blank constraint at publish, so a
// peer can carry displayName = "" / "   ". The old `displayName ?? address`
// rendered a whitespace name as the label (blank) and — for a null name — the
// FULL untruncated address. The ladder now guards with nonEmpty() and falls
// back to a truncated short id.
describe('ShareList — ZEB-960 name ladder', () => {
  // Long enough (>16 chars) that the shared shortAddr (ZEB-607: first 8 + '…' +
  // last 4) truncates, so the full-address leak is visibly distinct.
  const LONG = 'deadbeefcafe0011223344';
  const SHORT = 'deadbeef…3344';

  it('renders the short id (not a blank label) for a whitespace-only grant name', () => {
    renderShareList({ grants: [{ granteeAddress: LONG, displayName: '   ', grantedAt: 1 }] });
    expect(screen.getByText(SHORT)).toBeTruthy();
    // The revoke control names the peer by the same short id, never a blank.
    expect(screen.getByRole('button', { name: `Revoke ${SHORT}` })).toBeTruthy();
  });

  it('truncates the fallback for a null grant name instead of leaking the full address', () => {
    renderShareList({ grants: [{ granteeAddress: LONG, displayName: null, grantedAt: 1 }] });
    expect(screen.getByText(SHORT)).toBeTruthy();
    // The full untruncated address must NOT appear anywhere.
    expect(screen.queryByText(LONG)).toBeNull();
  });

  it('shows the short id (not a blank option) for a whitespace-only picker friend', () => {
    renderShareList({ grants: [], availableFriends: [{ address: LONG, displayName: '   ' }] });
    const select = screen.getByLabelText('Share with...') as HTMLSelectElement;
    const texts = Array.from(select.options).map((o) => o.text);
    expect(texts).toContain(SHORT);
    expect(texts).not.toContain(LONG);
  });
});
