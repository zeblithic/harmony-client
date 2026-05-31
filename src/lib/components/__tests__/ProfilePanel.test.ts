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
  const resolve = vi.fn(() => nextResult);
  return {
    resolver: { resolve } as unknown as ProfilePageResolver,
    resolve,
    setResolved(dto: ProfilePageDto | undefined) {
      nextResult = dto;
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
    // No doc yet → the "No page content." placeholder, not About/Links/Details.
    expect(screen.getByText('No page content.')).toBeTruthy();
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
    // Initial render: resolver returns undefined → header-only.
    expect(screen.getByText('No page content.')).toBeTruthy();
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
});
