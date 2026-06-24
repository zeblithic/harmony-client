import { render, screen, fireEvent, within } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import VineFeed from '../VineFeed.svelte';
import type { VineVideo } from '../../types';

const vines: VineVideo[] = [
  {
    id: 'vine-01',
    creatorAddress: 'a1b2c3d4',
    creatorName: 'Alice',
    createdAt: 1700000000,
    videoCid: 'cid-abc',
    title: 'First vine',
    viewed: false,
  },
  {
    id: 'vine-02',
    creatorAddress: 'e5f6g7h8',
    creatorName: 'Bob',
    createdAt: 1700000100,
    videoCid: 'cid-def',
    title: 'Second vine',
    viewed: true,
  },
  {
    id: 'vine-03',
    creatorAddress: 'i9j0k1l2',
    creatorName: 'Carol',
    createdAt: 1700000200,
    videoCid: 'cid-ghi',
    title: 'Third vine',
    viewed: false,
  },
];

/** Build a viewedIds set matching the initial viewed state of the vines. */
function makeViewedIds(vs: VineVideo[] = vines): Set<string> {
  return new Set(vs.filter(v => v.viewed).map(v => v.id));
}

describe('VineFeed', () => {
  it('renders feed title', () => {
    render(VineFeed, { props: { followedVines: vines, viewedIds: makeViewedIds(), activeTab: 'following', followedAddresses: new Set() } });
    expect(screen.getByText('Vines')).toBeTruthy();
  });

  it('renders all vine cards', () => {
    render(VineFeed, { props: { followedVines: vines, viewedIds: makeViewedIds(), activeTab: 'following', followedAddresses: new Set() } });
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('Bob')).toBeTruthy();
    expect(screen.getByText('Carol')).toBeTruthy();
  });

  it('shows unviewed count badge', () => {
    render(VineFeed, { props: { followedVines: vines, viewedIds: makeViewedIds(), activeTab: 'following', followedAddresses: new Set() } });
    expect(screen.getByText('2 new')).toBeTruthy();
  });

  it('hides unviewed badge when all viewed', () => {
    const allViewed = vines.map(v => ({ ...v, viewed: true }));
    render(VineFeed, { props: { followedVines: allViewed, viewedIds: makeViewedIds(allViewed), activeTab: 'following', followedAddresses: new Set() } });
    expect(screen.queryByText(/new/)).toBeNull();
  });

  it('shows empty state when no vines', () => {
    render(VineFeed, { props: { followedVines: [], viewedIds: new Set(), activeTab: 'following', followedAddresses: new Set() } });
    expect(screen.getByText(/Follow creators/)).toBeTruthy();
  });

  it('sorts vines newest first', () => {
    render(VineFeed, { props: { followedVines: vines, viewedIds: makeViewedIds(), activeTab: 'following', followedAddresses: new Set() } });
    const items = screen.getAllByRole('listitem');
    expect(items.length).toBe(3);
    // Carol's vine (createdAt=200) should be first
    expect(items[0].textContent).toContain('Carol');
  });

  it('opens player when a card is clicked', async () => {
    render(VineFeed, { props: { followedVines: vines, viewedIds: makeViewedIds(), activeTab: 'following', followedAddresses: new Set() } });
    // Carol's vine is newest → first card
    await fireEvent.click(screen.getByLabelText('Third vine by Carol'));
    expect(screen.getByRole('dialog')).toBeTruthy();
  });

  it('closes player when close button is clicked', async () => {
    render(VineFeed, { props: { followedVines: vines, viewedIds: makeViewedIds(), activeTab: 'following', followedAddresses: new Set() } });
    await fireEvent.click(screen.getByLabelText('Third vine by Carol'));
    expect(screen.getByRole('dialog')).toBeTruthy();
    const closeBtn = screen.getByLabelText('Close player');
    await fireEvent.click(closeBtn);
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('calls onMarkViewed when a vine is opened', async () => {
    const onMarkViewed = vi.fn();
    render(VineFeed, { props: { followedVines: vines, viewedIds: makeViewedIds(), activeTab: 'following', followedAddresses: new Set(), onMarkViewed } });
    await fireEvent.click(screen.getByLabelText('Third vine by Carol'));
    expect(onMarkViewed).toHaveBeenCalledWith('vine-03');
  });

  it('filters to unviewed when filter tab is clicked', async () => {
    render(VineFeed, { props: { followedVines: vines, viewedIds: makeViewedIds(), activeTab: 'following', followedAddresses: new Set() } });
    const unviewedTab = screen.getByText(/Unviewed/);
    await fireEvent.click(unviewedTab);
    const items = screen.getAllByRole('listitem');
    // Only 2 unviewed vines (Alice, Carol)
    expect(items.length).toBe(2);
    expect(items[0].textContent).toContain('Carol');
    expect(items[1].textContent).toContain('Alice');
  });

  it('shows all-caught-up message when filtering unviewed with none left', async () => {
    const allViewed = vines.map(v => ({ ...v, viewed: true }));
    render(VineFeed, { props: { followedVines: allViewed, viewedIds: makeViewedIds(allViewed), activeTab: 'following', followedAddresses: new Set() } });
    const unviewedTab = screen.getByText('Unviewed');
    await fireEvent.click(unviewedTab);
    expect(screen.getByText(/All caught up/)).toBeTruthy();
  });

  it('renders a labeled create button when onPublish is provided', () => {
    const onPublish = vi.fn();
    render(VineFeed, { props: { followedVines: vines, viewedIds: makeViewedIds(), activeTab: 'following', followedAddresses: new Set(), onPublish } });
    // ZEB-554: the create affordance is now a labeled "New vine" pill, not a
    // bare "+" — its accessible name matches the visible text.
    expect(screen.getByRole('button', { name: 'New vine' })).toBeTruthy();
  });

  it('fires onPublish when the header create button is clicked', async () => {
    const onPublish = vi.fn();
    render(VineFeed, { props: { followedVines: vines, viewedIds: makeViewedIds(), activeTab: 'following', followedAddresses: new Set(), onPublish } });
    await fireEvent.click(screen.getByRole('button', { name: 'New vine' }));
    expect(onPublish).toHaveBeenCalled();
  });

  describe('ZEB-554 empty-state "Share a vine" CTA', () => {
    it('shows the CTA in an empty Following feed and fires onPublish', async () => {
      const onPublish = vi.fn();
      render(VineFeed, { props: {
        followedVines: [], discoverVines: [], viewedIds: new Set(),
        activeTab: 'following', followedAddresses: new Set(), onPublish,
      } });
      const cta = screen.getByRole('button', { name: 'Share a vine' });
      await fireEvent.click(cta);
      expect(onPublish).toHaveBeenCalled();
    });

    it('shows the CTA in an empty Discover feed', () => {
      const onPublish = vi.fn();
      render(VineFeed, { props: {
        followedVines: [], discoverVines: [], viewedIds: new Set(),
        activeTab: 'discover', followedAddresses: new Set(), onPublish,
      } });
      expect(screen.getByRole('button', { name: 'Share a vine' })).toBeTruthy();
    });

    it('omits the CTA in the all-caught-up (Following → Unviewed) state', async () => {
      // The user HAS vines here, just none unviewed — the action is "switch to
      // All", not "post", so the post CTA would be noise.
      const allViewed = vines.map(v => ({ ...v, viewed: true }));
      const onPublish = vi.fn();
      render(VineFeed, { props: {
        followedVines: allViewed, viewedIds: makeViewedIds(allViewed),
        activeTab: 'following', followedAddresses: new Set(), onPublish,
      } });
      await fireEvent.click(screen.getByText('Unviewed'));
      expect(screen.getByText(/All caught up/)).toBeTruthy();
      expect(screen.queryByRole('button', { name: 'Share a vine' })).toBeNull();
    });

    it('omits the CTA when onPublish is not provided', () => {
      render(VineFeed, { props: {
        followedVines: [], discoverVines: [], viewedIds: new Set(),
        activeTab: 'discover', followedAddresses: new Set(),
      } });
      expect(screen.queryByRole('button', { name: 'Share a vine' })).toBeNull();
    });
  });

  it('has accessible feed list', () => {
    render(VineFeed, { props: { followedVines: vines, viewedIds: makeViewedIds(), activeTab: 'following', followedAddresses: new Set() } });
    expect(screen.getByRole('list', { name: 'Vine feed' })).toBeTruthy();
  });

  it('renders Following and Discover tabs', () => {
    render(VineFeed, { props: {
      followedVines: vines, discoverVines: [], viewedIds: makeViewedIds(),
      activeTab: 'following', followedAddresses: new Set(),
    } });
    expect(screen.getByText('Following')).toBeTruthy();
    expect(screen.getByText('Discover')).toBeTruthy();
  });

  it('shows followed vines when Following tab active', () => {
    render(VineFeed, { props: {
      followedVines: vines, discoverVines: [], viewedIds: makeViewedIds(),
      activeTab: 'following', followedAddresses: new Set(),
    } });
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('Carol')).toBeTruthy();
  });

  it('shows discover vines when Discover tab active', () => {
    const discoverVines: VineVideo[] = [{
      id: 'dv-01', creatorAddress: 'xyz', creatorName: 'Dave',
      createdAt: 1700000300, videoCid: 'cid-d', title: 'Discover vine', viewed: false,
    }];
    render(VineFeed, { props: {
      followedVines: [], discoverVines, viewedIds: new Set(),
      activeTab: 'discover', followedAddresses: new Set(),
    } });
    expect(screen.getByText('Dave')).toBeTruthy();
  });

  it('shows empty state with nudge in Following when no followed vines', () => {
    render(VineFeed, { props: {
      followedVines: [], discoverVines: [], viewedIds: new Set(),
      activeTab: 'following', followedAddresses: new Set(),
    } });
    expect(screen.getByText(/Follow creators/)).toBeTruthy();
  });

  it('calls onTabChange when Discover tab clicked', async () => {
    const onTabChange = vi.fn();
    render(VineFeed, { props: {
      followedVines: vines, discoverVines: [], viewedIds: makeViewedIds(),
      activeTab: 'following', followedAddresses: new Set(), onTabChange,
    } });
    await fireEvent.click(screen.getByText('Discover'));
    expect(onTabChange).toHaveBeenCalledWith('discover');
  });

  it('shows follow button on cards in Discover tab', () => {
    const discoverVines: VineVideo[] = [{
      id: 'fb-01', creatorAddress: 'xyz', creatorName: 'Eve',
      createdAt: 1700000300, videoCid: 'cid-e', title: 'Eve vine', viewed: false,
    }];
    render(VineFeed, { props: {
      followedVines: [], discoverVines, viewedIds: new Set(),
      activeTab: 'discover', followedAddresses: new Set(),
    } });
    expect(screen.getByLabelText(/Follow Eve/)).toBeTruthy();
  });

  it('shows Following badge on cards in Following tab for followed creators', () => {
    render(VineFeed, { props: {
      followedVines: vines, discoverVines: [], viewedIds: makeViewedIds(),
      activeTab: 'following', followedAddresses: new Set(['a1b2c3d4', 'e5f6g7h8', 'i9j0k1l2']),
    } });
    // 1 "Following" from the tab button + 3 from VineCard badges = 4
    const matches = screen.getAllByText('Following');
    expect(matches.length).toBe(4);
  });

  it('passes reaction data to vine cards', () => {
    const getReaction = vi.fn().mockReturnValue({ count: 5, likedByMe: true });
    render(VineFeed, { props: {
      followedVines: vines, viewedIds: makeViewedIds(),
      activeTab: 'following', followedAddresses: new Set(), getReaction,
    } });
    expect(getReaction).toHaveBeenCalled();
    const counts = screen.getAllByText('5');
    expect(counts.length).toBeGreaterThan(0);
  });

  it('calls onToggleLike when card like is clicked', async () => {
    const onToggleLike = vi.fn();
    const getReaction = vi.fn().mockReturnValue({ count: 1, likedByMe: false });
    render(VineFeed, { props: {
      followedVines: vines, viewedIds: makeViewedIds(),
      activeTab: 'following', followedAddresses: new Set(),
      getReaction, onToggleLike,
    } });
    const likeBtn = screen.getAllByLabelText(/Like/)[0];
    await fireEvent.click(likeBtn);
    expect(onToggleLike).toHaveBeenCalled();
  });

  it('renders reshare counts derived from the local feed (single-pass index)', () => {
    // FIX 5 (PR #120 round 1): VineFeed no longer takes a
    // `getReshareCount` prop — it computes the count internally from
    // both vine arrays in a single pass (`reshareCountMap` derived).
    // Wire three reshares of `vine-orig` across both feeds and pin
    // that the count surfaces on the original's card. Use the
    // `reshare count` aria-label to disambiguate from unrelated
    // numeric text (see FIX 6).
    const orig: VineVideo = {
      id: 'vine-orig',
      creatorAddress: 'origAddr',
      creatorName: 'OrigCreator',
      createdAt: 1700001000,
      videoCid: 'cid-orig',
      viewed: false,
    };
    const reshare = (suffix: string, reshareOf: string): VineVideo => ({
      id: `r-${suffix}`,
      creatorAddress: `addr-${suffix}`,
      creatorName: `Resharer ${suffix}`,
      createdAt: 1700001100,
      videoCid: `cid-${suffix}`,
      reshareOf,
      viewed: false,
    });
    render(VineFeed, { props: {
      followedVines: [reshare('a', 'vine-orig'), reshare('b', 'vine-orig')],
      discoverVines: [orig, reshare('c', 'vine-orig')],
      viewedIds: new Set(),
      activeTab: 'discover',
      followedAddresses: new Set(),
    } });
    const countEl = screen.getByLabelText(/reshare count/i);
    expect(countEl.textContent).toMatch(/3/);
  });

  it('forwards onViewOriginal to VineCard attribution link', async () => {
    const onViewOriginal = vi.fn();
    const reshared: VineVideo = {
      id: 'vine-r',
      creatorAddress: 'a1b2c3d4',
      creatorName: 'Alice',
      createdAt: 1700002000,
      videoCid: 'cid-r',
      reshareOf: 'orig-1',
      originalCreatorName: 'OrigName',
      viewed: false,
    };
    render(VineFeed, { props: {
      followedVines: [],
      discoverVines: [reshared],
      viewedIds: new Set(),
      activeTab: 'discover',
      followedAddresses: new Set(),
      onViewOriginal,
    } });
    const link = screen.getByRole('button', { name: /originally by OrigName/i });
    await fireEvent.click(link);
    expect(onViewOriginal).toHaveBeenCalledWith('orig-1');
  });

  it('forwards onViewOriginal to VinePlayer attribution link', async () => {
    const onViewOriginal = vi.fn();
    const reshared: VineVideo = {
      id: 'vine-r2',
      creatorAddress: 'a1b2c3d4',
      creatorName: 'Alice',
      createdAt: 1700003000,
      videoCid: 'cid-r2',
      reshareOf: 'orig-2',
      originalCreatorName: 'PlayerOrig',
      viewed: false,
    };
    render(VineFeed, { props: {
      followedVines: [],
      discoverVines: [reshared],
      viewedIds: new Set(),
      activeTab: 'discover',
      followedAddresses: new Set(),
      onViewOriginal,
    } });
    // Open the player by clicking the card
    await fireEvent.click(screen.getByLabelText(/Untitled vine by Alice/));
    // Player exposes its own attribution button — there will be two matches now
    // (one on the card behind, one in the dialog). Scope to the dialog with
    // `within` so the lookup throws on absence rather than returning null.
    const dialog = screen.getByRole('dialog', { name: 'Vine player' });
    const playerLink = within(dialog).getByRole('button', {
      name: /originally by PlayerOrig/i,
    });
    await fireEvent.click(playerLink);
    expect(onViewOriginal).toHaveBeenCalledWith('orig-2');
  });
});
