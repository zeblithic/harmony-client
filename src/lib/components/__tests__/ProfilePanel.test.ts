import { render, screen, cleanup, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, afterEach } from 'vitest';
import ProfilePanel from '../ProfilePanel.svelte';
import type { ProfilePageResolver, ProfilePageDto } from '../../profile-page-resolver';

const OWNER_HEX = 'ab'.repeat(16);
const PAGE_CID = 'cd'.repeat(32);

const SAMPLE_DOC: ProfilePageDto = {
  bio: 'Builder of meshes',
  links: [{ label: 'site', url: 'https://example.com' }],
  fields: [{ key: 'pronouns', value: 'they/them' }],
};

/**
 * Minimal resolver double. `resolve` returns whatever `nextResult` currently
 * holds, so a test can flip it from undefined → DTO to simulate a lazy
 * fetch_profile_doc landing between renders.
 */
function makeResolver(initial: ProfilePageDto | undefined) {
  let nextResult = initial;
  let nextStatus: 'resolved' | 'loading' | 'error' = initial ? 'resolved' : 'loading';
  const resolve = vi.fn(() => nextResult);
  const status = vi.fn(() => nextStatus);
  return {
    resolver: { resolve, status } as unknown as ProfilePageResolver,
    resolve,
    status,
    setResolved(dto: ProfilePageDto | undefined) {
      nextResult = dto;
      nextStatus = dto ? 'resolved' : 'loading';
    },
    setStatus(s: 'resolved' | 'loading' | 'error') {
      nextStatus = s;
    },
  };
}

describe('ProfilePanel', () => {
  afterEach(() => cleanup());

  it('renders header-only (default docVersion) when the doc is unresolved', () => {
    const { resolver } = makeResolver(undefined);
    render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Alice', statusText: 'hi', profilePageRoot: PAGE_CID },
        resolver,
        onClose: vi.fn(),
        // docVersion intentionally omitted → defaults to 0 (existing-test posture).
      },
    });
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('hi')).toBeTruthy();
    expect(screen.getByText(OWNER_HEX)).toBeTruthy();
    // Root is set but the doc hasn't resolved yet → Loading placeholder, not
    // the "No page content." empty state and not About/Links/Details.
    expect(screen.getByText('Loading profile…')).toBeTruthy();
    expect(screen.queryByText('No page content.')).toBeNull();
    expect(screen.queryByText('About')).toBeNull();
  });

  it('shows an error state when the fetch failed (status=error), not endless Loading', () => {
    const { resolver, setStatus } = makeResolver(undefined);
    setStatus('error');
    render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Alice', profilePageRoot: PAGE_CID },
        resolver,
        onClose: vi.fn(),
      },
    });
    const alert = screen.getByText("Couldn't load this profile page.");
    expect(alert.getAttribute('role')).toBe('alert');
    expect(screen.queryByText('Loading profile…')).toBeNull();
    expect(screen.queryByText('No page content.')).toBeNull();
    expect(screen.queryByText('About')).toBeNull();
  });

  it('re-resolves the doc when docVersion is bumped (lazy fetch landed)', async () => {
    const { resolver, setResolved } = makeResolver(undefined);
    const { rerender } = render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Alice', profilePageRoot: PAGE_CID },
        resolver,
        docVersion: 0,
        onClose: vi.fn(),
      },
    });
    // Initial render: resolver returns undefined but a root is set → Loading.
    expect(screen.getByText('Loading profile…')).toBeTruthy();
    expect(screen.queryByText('No page content.')).toBeNull();
    expect(screen.queryByText('Builder of meshes')).toBeNull();

    // Simulate the fetch resolving, then App bumping docVersion (resolver.onChange).
    setResolved(SAMPLE_DOC);
    await rerender({
      ownerIdHex: OWNER_HEX,
      card: { displayName: 'Alice', profilePageRoot: PAGE_CID },
      resolver,
      docVersion: 1,
      onClose: vi.fn(),
    });

    await waitFor(() => expect(screen.getByText('Builder of meshes')).toBeTruthy());
    expect(screen.getByText('About')).toBeTruthy();
    expect(screen.getByText('site')).toBeTruthy();
    expect(screen.getByText('they/them')).toBeTruthy();
    expect(screen.queryByText('No page content.')).toBeNull();
  });

  it('does not call resolve when the card has no profilePageRoot', () => {
    const { resolver, resolve } = makeResolver(undefined);
    render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Bob' },
        resolver,
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('Bob')).toBeTruthy();
    expect(resolve).not.toHaveBeenCalled();
    expect(screen.getByText('No page content.')).toBeTruthy();
  });

  // ZEB-962: `card?.displayName || 'Name unavailable'` floored `""` but let a
  // whitespace-only card name through as a blank header. `nonEmpty` closes it.
  it('shows the fallback for a whitespace-only card display name', () => {
    const { resolver } = makeResolver(undefined);
    render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: '   ', profilePageRoot: PAGE_CID },
        resolver,
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('Name unavailable')).toBeTruthy();
  });
});
