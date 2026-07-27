import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
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
    const { container } = render(VineFeed, { props: { followedVines: vines, viewedIds: makeViewedIds(), activeTab: 'following', followedAddresses: new Set() } });
    const items = screen.getAllByRole('listitem');
    expect(items.length).toBe(3);
    // Carol's vine (createdAt=200) should be first
    expect(items[0].textContent).toContain('Carol');
    const order = Array.from(container.querySelectorAll('[data-vine-id]')).map(
      el => (el as HTMLElement).dataset.vineId,
    );
    expect(order).toEqual(['vine-03', 'vine-02', 'vine-01']);
  });

  it('filters to unviewed when filter tab is clicked', async () => {
    render(VineFeed, { props: { followedVines: vines, viewedIds: makeViewedIds(), activeTab: 'following', followedAddresses: new Set(), onMarkViewed: vi.fn() } });
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
    render(VineFeed, { props: { followedVines: allViewed, viewedIds: makeViewedIds(allViewed), activeTab: 'following', followedAddresses: new Set(), onMarkViewed: vi.fn() } });
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

    it('shows the CTA in an empty Following → Unviewed feed with no followed vines', async () => {
      // Regression (CodeAnt, PR #333): suppression must be gated on actually
      // HAVING vines, not just the filter state. A zero-vine user who flips to
      // Unviewed is not "all caught up" — they still need the create path.
      const onPublish = vi.fn();
      render(VineFeed, { props: {
        followedVines: [], discoverVines: [], viewedIds: new Set(),
        activeTab: 'following', followedAddresses: new Set(), onPublish,
      } });
      await fireEvent.click(screen.getByText('Unviewed'));
      expect(screen.getByRole('button', { name: 'Share a vine' })).toBeTruthy();
    });

    it('omits the CTA in the all-caught-up (Following → Unviewed) state', async () => {
      // The user HAS vines here, just none unviewed — the action is "switch to
      // All", not "post", so the post CTA would be noise.
      const allViewed = vines.map(v => ({ ...v, viewed: true }));
      const onPublish = vi.fn();
      render(VineFeed, { props: {
        followedVines: allViewed, viewedIds: makeViewedIds(allViewed),
        activeTab: 'following', followedAddresses: new Set(), onPublish, onMarkViewed: vi.fn(),
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
      // ZEB-671: Discover is graph-only — a vine renders only with a degree.
      degree: 2, via: ['root-1'],
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
      degree: 2, via: ['root-1'],
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
      degree: 2, via: ['root-1'],
    };
    const reshare = (suffix: string, reshareOf: string): VineVideo => ({
      id: `r-${suffix}`,
      creatorAddress: `addr-${suffix}`,
      creatorName: `Resharer ${suffix}`,
      createdAt: 1700001100,
      videoCid: `cid-${suffix}`,
      reshareOf,
      viewed: false,
      // ZEB-671: Discover renders graph-reachable creators only.
      degree: 2, via: ['root-1'],
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
      // ZEB-671: Discover renders graph-reachable creators only.
      degree: 2, via: ['root-1'],
    };
    render(VineFeed, { props: {
      followedVines: [],
      discoverVines: [reshared],
      viewedIds: new Set(),
      activeTab: 'discover',
      followedAddresses: new Set(),
      onViewOriginal,
    } });
    const link = screen.getByRole('button', { name: /view original by OrigName/i });
    await fireEvent.click(link);
    expect(onViewOriginal).toHaveBeenCalledWith('orig-1');
  });

  describe('center-detection autoplay (ZEB-612 S2)', () => {
    it('the first (newest) card plays on mount and is marked viewed', async () => {
      const onMarkViewed = vi.fn();
      const { container } = render(VineFeed, { props: {
        followedVines: vines, viewedIds: makeViewedIds(), onMarkViewed,
      } });
      await waitFor(() => expect(onMarkViewed).toHaveBeenCalledWith('vine-03'));
      const playing = container.querySelectorAll('.vine-card.playing');
      expect(playing.length).toBe(1);
    });

    it('clicking a card moves the single playing slot to it', async () => {
      const onMarkViewed = vi.fn();
      const { container } = render(VineFeed, { props: {
        followedVines: vines, viewedIds: makeViewedIds(), onMarkViewed,
      } });
      await fireEvent.click(screen.getByRole('button', { name: /First vine by Alice/ }));
      expect(onMarkViewed).toHaveBeenCalledWith('vine-01');
      const row = container.querySelector('[data-vine-id="vine-01"]');
      expect(row?.querySelector('.vine-card.playing')).toBeTruthy();
      expect(container.querySelectorAll('.vine-card.playing').length).toBe(1);
    });
  });

  describe('lazy video window (ZEB-612 S2)', () => {
    // lib.dom types URL.revokeObjectURL, but jsdom doesn't implement it —
    // install a spy and restore whatever was (or wasn't) there afterwards.
    const revoke = vi.fn();
    const urlGlobal = URL as unknown as { revokeObjectURL?: (url: string) => void };
    const hadRevoke = 'revokeObjectURL' in URL;
    const origRevoke = urlGlobal.revokeObjectURL;

    beforeEach(() => {
      revoke.mockClear();
      urlGlobal.revokeObjectURL = revoke;
    });

    afterEach(() => {
      if (hadRevoke) {
        urlGlobal.revokeObjectURL = origRevoke;
      } else {
        delete urlGlobal.revokeObjectURL;
      }
    });

    const five: VineVideo[] = [1, 2, 3, 4, 5].map(n => ({
      id: `vine-0${n}`, creatorAddress: `addr-${n}`, creatorName: `C${n}`,
      createdAt: 1700000000 + n * 100, videoCid: `cid-${n}`, viewed: false,
    }));

    it('mounts <video> only for the playing card and its neighbors', async () => {
      const resolveVideo = vi.fn(async (cid: string) => `blob:fake-${cid}`);
      const { container } = render(VineFeed, { props: {
        followedVines: five, viewedIds: new Set<string>(), resolveVideo, onMarkViewed: vi.fn(),
      } });
      // Newest-first order: vine-05 plays (index 0); window = vine-05 + vine-04.
      // ZEB-811: resolveVideo also receives the card's creatorAddress (the
      // relay-fallback dial target).
      await waitFor(() => expect(container.querySelectorAll('[data-testid="stage-video"]').length).toBe(2));
      expect(resolveVideo).toHaveBeenCalledWith('cid-5', 'addr-5');
      expect(resolveVideo).toHaveBeenCalledWith('cid-4', 'addr-4');
      expect(resolveVideo).not.toHaveBeenCalledWith('cid-1', 'addr-1');
    });

    it('revokes blob URLs when cards leave the window', async () => {
      const resolveVideo = vi.fn(async (cid: string) => `blob:fake-${cid}`);
      const { container } = render(VineFeed, { props: {
        followedVines: five, viewedIds: new Set<string>(), resolveVideo, onMarkViewed: vi.fn(),
      } });
      await waitFor(() => expect(container.querySelectorAll('[data-testid="stage-video"]').length).toBe(2));
      // Jump to the oldest card: window becomes vine-01 + vine-02 → cid-5/cid-4 evicted.
      await fireEvent.click(screen.getByRole('button', { name: /Untitled vine by C1/ }));
      await waitFor(() => expect(revoke).toHaveBeenCalledWith('blob:fake-cid-5'));
      expect(revoke).toHaveBeenCalledWith('blob:fake-cid-4');
    });

    it('windows by card, not by CID — a far reshare sharing the playing CID stays video-less (Qodo #440)', async () => {
      const resolveVideo = vi.fn(async (cid: string) => `blob:fake-${cid}`);
      const shared: VineVideo[] = [
        { id: 'vine-05', creatorAddress: 'a5', creatorName: 'C5', createdAt: 1700000500, videoCid: 'cid-shared', viewed: false },
        { id: 'vine-04', creatorAddress: 'a4', creatorName: 'C4', createdAt: 1700000400, videoCid: 'cid-4', viewed: false },
        { id: 'vine-03', creatorAddress: 'a3', creatorName: 'C3', createdAt: 1700000300, videoCid: 'cid-3', viewed: false },
        { id: 'vine-02', creatorAddress: 'a2', creatorName: 'C2', createdAt: 1700000200, videoCid: 'cid-2', viewed: false },
        // Reshare of vine-05's clip: same videoCid, far outside the window.
        { id: 'vine-01', creatorAddress: 'a1', creatorName: 'C1', createdAt: 1700000100, videoCid: 'cid-shared', reshareOf: 'vine-05', originalCreatorAddress: 'a5', originalCreatorName: 'C5', viewed: false },
      ];
      const { container } = render(VineFeed, { props: {
        followedVines: shared, viewedIds: new Set<string>(), resolveVideo, onMarkViewed: vi.fn(),
      } });
      // Window = playing (vine-05) + neighbor (vine-04): exactly two videos,
      // even though vine-01 shares the playing card's CID.
      await waitFor(() => expect(container.querySelectorAll('[data-testid="stage-video"]').length).toBe(2));
      expect(container.querySelector('[data-vine-id="vine-05"] [data-testid="stage-video"]')).toBeTruthy();
      expect(container.querySelector('[data-vine-id="vine-01"] [data-testid="stage-video"]')).toBeNull();
    });

    it('revokes a blob that resolves after unmount (no leak — Qodo #440)', async () => {
      let resolveFetch!: (url: string) => void;
      const resolveVideo = vi.fn(() => new Promise<string>(res => { resolveFetch = res; }));
      const { unmount } = render(VineFeed, { props: {
        followedVines: [vines[0]], viewedIds: new Set<string>(), resolveVideo, onMarkViewed: vi.fn(),
      } });
      await waitFor(() => expect(resolveVideo).toHaveBeenCalled());
      unmount();
      resolveFetch('blob:late');
      await waitFor(() => expect(revoke).toHaveBeenCalledWith('blob:late'));
    });
  });

  describe('unviewed filter pin (ZEB-612 S2)', () => {
    it('keeps the card that became viewed BY playing under the active Unviewed filter', async () => {
      const onMarkViewed = vi.fn();
      const baseProps = {
        followedVines: vines, activeTab: 'following' as const,
        followedAddresses: new Set<string>(), onMarkViewed,
      };
      const { rerender } = render(VineFeed, { props: { ...baseProps, viewedIds: makeViewedIds() } });
      await fireEvent.click(screen.getByText(/Unviewed/));
      // vine-01 is unviewed → still listed; activate it under the filter.
      await fireEvent.click(screen.getByRole('button', { name: /First vine by Alice/ }));
      expect(onMarkViewed).toHaveBeenCalledWith('vine-01');
      // Parent marks it viewed and pushes the updated set down (as App.svelte
      // would) — the pinned card must NOT vanish from under the user.
      await rerender({ ...baseProps, viewedIds: new Set([...makeViewedIds(), 'vine-01']) });
      expect(screen.getByRole('button', { name: /First vine by Alice/ })).toBeTruthy();
    });

    it('still reaches the all-caught-up state when every vine is viewed', async () => {
      const allViewed = vines.map(v => ({ ...v, viewed: true }));
      render(VineFeed, { props: {
        followedVines: allViewed,
        viewedIds: new Set(allViewed.map(v => v.id)),
        onMarkViewed: vi.fn(),
        onPublish: vi.fn(),
      } });
      await fireEvent.click(screen.getByText(/Unviewed/));
      expect(screen.getByText('All caught up — no unviewed vines.')).toBeTruthy();
    });
  });

  describe('playTarget navigation (ZEB-612 S2 — feed is the player)', () => {
    it('consumes the target, plays it, and marks it viewed', async () => {
      const onMarkViewed = vi.fn();
      const onPlayTargetConsumed = vi.fn();
      const baseProps = {
        followedVines: vines, viewedIds: makeViewedIds(),
        onMarkViewed, onPlayTargetConsumed,
      };
      const { container, rerender } = render(VineFeed, { props: { ...baseProps, playTarget: null } });
      await rerender({ ...baseProps, playTarget: vines[0] });
      await waitFor(() => expect(onPlayTargetConsumed).toHaveBeenCalled());
      expect(onMarkViewed).toHaveBeenCalledWith('vine-01');
      const row = container.querySelector('[data-vine-id="vine-01"]');
      expect(row?.querySelector('.vine-card.playing')).toBeTruthy();
    });

    it('switches to the tab that owns the target', async () => {
      const onTabChange = vi.fn();
      const discoverOnly = [{ ...vines[0], id: 'disc-1' }];
      const baseProps = {
        followedVines: vines, discoverVines: discoverOnly, viewedIds: makeViewedIds(),
        activeTab: 'following' as const, onTabChange,
        onMarkViewed: vi.fn(), onPlayTargetConsumed: vi.fn(),
      };
      const { rerender } = render(VineFeed, { props: { ...baseProps, playTarget: null } });
      await rerender({ ...baseProps, playTarget: discoverOnly[0] });
      await waitFor(() => expect(onTabChange).toHaveBeenCalledWith('discover'));
    });
  });

  describe('feed-level reshare (ZEB-612 S2 — replaces the player flow)', () => {
    it('card verb → confirm dialog → onReshare', async () => {
      const onReshare = vi.fn().mockResolvedValue(undefined);
      render(VineFeed, { props: {
        followedVines: [vines[0]], viewedIds: new Set<string>(),
        onReshare, onMarkViewed: vi.fn(),
      } });
      await fireEvent.click(screen.getByRole('button', { name: 'Reshare vine' }));
      // ReshareConfirmDialog is up — confirm it.
      await fireEvent.click(screen.getByRole('button', { name: /^Reshare$/ }));
      await waitFor(() => expect(onReshare).toHaveBeenCalledWith(expect.objectContaining({ id: 'vine-01' })));
    });

    it('cancel closes the dialog without resharing', async () => {
      const onReshare = vi.fn();
      render(VineFeed, { props: {
        followedVines: [vines[0]], viewedIds: new Set<string>(),
        onReshare, onMarkViewed: vi.fn(),
      } });
      await fireEvent.click(screen.getByRole('button', { name: 'Reshare vine' }));
      await fireEvent.click(screen.getByRole('button', { name: /Cancel/ }));
      expect(onReshare).not.toHaveBeenCalled();
    });

    it('surfaces a reshare failure as a feed-level alert', async () => {
      const onReshare = vi.fn().mockRejectedValue(new Error('publish failed: not connected'));
      render(VineFeed, { props: {
        followedVines: [vines[0]], viewedIds: new Set<string>(),
        onReshare, onMarkViewed: vi.fn(),
      } });
      await fireEvent.click(screen.getByRole('button', { name: 'Reshare vine' }));
      await fireEvent.click(screen.getByRole('button', { name: /^Reshare$/ }));
      await waitFor(() => expect(screen.getByRole('alert')).toHaveTextContent(/publish failed/));
    });

    it('suppresses the verb on own originals (isOwnOriginalVine guard)', () => {
      const own: VineVideo = { ...vines[0], id: 'own-1', creatorAddress: 'self' };
      render(VineFeed, { props: {
        followedVines: [own], viewedIds: new Set<string>(),
        onReshare: vi.fn(), onMarkViewed: vi.fn(),
      } });
      expect(screen.queryByRole('button', { name: 'Reshare vine' })).toBeNull();
    });
  });
});

describe('feed-level delete (ZEB-670 creator tombstone)', () => {
  const own: VineVideo = { ...vines[0], id: 'own-1', creatorAddress: 'self', title: 'Mine' };

  it('offers the verb only on own vines', () => {
    render(VineFeed, { props: {
      followedVines: [own, vines[1]], viewedIds: new Set<string>(),
      onDelete: vi.fn(), onMarkViewed: vi.fn(),
    } });
    expect(screen.getAllByRole('button', { name: 'Delete vine' })).toHaveLength(1);
  });

  it('card verb → ConfirmDialog → onDelete; dialog copy is honest about reach', async () => {
    const onDelete = vi.fn().mockResolvedValue(undefined);
    render(VineFeed, { props: {
      followedVines: [own], viewedIds: new Set<string>(),
      onDelete, onMarkViewed: vi.fn(),
    } });
    await fireEvent.click(screen.getByRole('button', { name: 'Delete vine' }));
    expect(screen.getByText('Delete vine?')).toBeTruthy();
    expect(screen.getByText(/offline may keep it/)).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: /^Delete$/ }));
    await waitFor(() => expect(onDelete).toHaveBeenCalledWith(expect.objectContaining({ id: 'own-1' })));
  });

  it('cancel closes the dialog without deleting', async () => {
    const onDelete = vi.fn();
    render(VineFeed, { props: {
      followedVines: [own], viewedIds: new Set<string>(),
      onDelete, onMarkViewed: vi.fn(),
    } });
    await fireEvent.click(screen.getByRole('button', { name: 'Delete vine' }));
    await fireEvent.click(screen.getByRole('button', { name: /Cancel/ }));
    expect(onDelete).not.toHaveBeenCalled();
  });

  it('surfaces a delete failure as a feed-level alert', async () => {
    const onDelete = vi.fn().mockRejectedValue(new Error('not your vine: only the creator can delete a vine'));
    render(VineFeed, { props: {
      followedVines: [own], viewedIds: new Set<string>(),
      onDelete, onMarkViewed: vi.fn(),
    } });
    await fireEvent.click(screen.getByRole('button', { name: 'Delete vine' }));
    await fireEvent.click(screen.getByRole('button', { name: /^Delete$/ }));
    await waitFor(() => expect(screen.getByRole('alert')).toHaveTextContent(/not your vine/));
  });

  // ── ZEB-671: graph-only Discover + Tune ──────────────────────────

  describe('Discover graph (ZEB-671)', () => {
    const graphVines: VineVideo[] = [
      {
        id: 'g2-01', creatorAddress: 'ravi-addr', creatorName: 'Ravi',
        createdAt: 1700000400, videoCid: 'cid-g2', title: 'Second degree', viewed: false,
        degree: 2, via: ['devin-addr'],
      },
      {
        id: 'g3-01', creatorAddress: 'ada-addr', creatorName: 'Ada',
        createdAt: 1700000500, videoCid: 'cid-g3', title: 'Third degree', viewed: false,
        degree: 3, via: ['devin-addr', 'ravi-addr'],
      },
      {
        id: 'nog-01', creatorAddress: 'stranger-addr', creatorName: 'Stranger',
        createdAt: 1700000600, videoCid: 'cid-nog', title: 'Unconnected', viewed: false,
      },
    ];

    function renderDiscover(extra: Record<string, unknown> = {}) {
      return render(VineFeed, { props: {
        followedVines: [], discoverVines: graphVines, viewedIds: new Set(),
        activeTab: 'discover', followedAddresses: new Set(),
        ...extra,
      } });
    }

    beforeEach(() => {
      localStorage.removeItem('harmony.vines.tune.v1');
    });

    it('renders only graph-reachable vines (no degree → hidden)', () => {
      renderDiscover();
      expect(screen.getByText('Ravi')).toBeTruthy();
      expect(screen.getByText('Ada')).toBeTruthy();
      expect(screen.queryByText('Stranger')).toBeNull();
    });

    it('badges cards with 2nd/3rd degree chips and provenance lines', () => {
      renderDiscover();
      const chips = screen.getAllByTestId('degree-chip');
      expect(chips.map(c => c.textContent)).toEqual(expect.arrayContaining(['2nd', '3rd']));
      // 2°: "{root} follows @{creator}" — root name unknown → truncated addr.
      expect(screen.getByText(/devin-ad… follows @Ravi/)).toBeTruthy();
      // 3°: chain copy; middle hop resolves to Ravi's known creatorName.
      expect(screen.getByText(/devin-ad… → @Ravi → @Ada/)).toBeTruthy();
    });

    it('keeps own offline-fallback publishes visible without a degree', () => {
      const own: VineVideo = {
        id: 'own-01', creatorAddress: 'self', creatorName: 'You',
        createdAt: 1700000700, videoCid: 'cid-own', title: 'Mine', viewed: true,
      };
      renderDiscover({ discoverVines: [...graphVines, own] });
      expect(screen.getByText('You')).toBeTruthy();
    });

    it('Tune 2° toggle hides second-degree vines and updates the count', async () => {
      renderDiscover();
      await fireEvent.click(screen.getByTestId('tune-btn'));
      expect(screen.getByRole('dialog', { name: 'Tune your Discover' })).toBeTruthy();
      expect(screen.getByText(/Done · 2 vines in Discover/)).toBeTruthy();

      const deg2Toggle = screen.getByText(/2nd degree — someone/).closest('label')!
        .querySelector('input')!;
      await fireEvent.click(deg2Toggle);
      expect(screen.getByText(/Done · 1 vine in Discover/)).toBeTruthy();
      expect(screen.queryByText('Ravi')).toBeNull();
      expect(screen.getByText('Ada')).toBeTruthy();
    });

    it('muting a follow root hides its vines', async () => {
      renderDiscover();
      await fireEvent.click(screen.getByTestId('tune-btn'));
      // Sole root is devin-addr — both vines trace to it.
      const muteToggle = screen.getByText(/via devin-ad…/).closest('label')!
        .querySelector('input')!;
      await fireEvent.click(muteToggle);
      expect(screen.getByText(/Done · 0 vines in Discover/)).toBeTruthy();
      expect(screen.queryByText('Ravi')).toBeNull();
      expect(screen.queryByText('Ada')).toBeNull();
    });

    it('persists Tune prefs to localStorage', async () => {
      renderDiscover();
      await fireEvent.click(screen.getByTestId('tune-btn'));
      const deg3Toggle = screen.getByText(/3rd degree — a follow/).closest('label')!
        .querySelector('input')!;
      await fireEvent.click(deg3Toggle);
      const stored = JSON.parse(localStorage.getItem('harmony.vines.tune.v1')!);
      expect(stored.deg3).toBe(false);
    });

    it('Share-my-follows toggle reads and writes through the props', async () => {
      const getShareFollows = vi.fn().mockResolvedValue(true);
      const onSetShareFollows = vi.fn().mockResolvedValue(undefined);
      renderDiscover({ getShareFollows, onSetShareFollows });
      await fireEvent.click(screen.getByTestId('tune-btn'));
      await waitFor(() => expect(getShareFollows).toHaveBeenCalled());

      await fireEvent.click(screen.getByTestId('share-follows-toggle'));
      await waitFor(() => expect(onSetShareFollows).toHaveBeenCalledWith(false));
    });

    it('keeps the share-follows toggle disabled when the read fails', async () => {
      const getShareFollows = vi.fn().mockRejectedValue(new Error('ipc down'));
      const onSetShareFollows = vi.fn();
      renderDiscover({ getShareFollows, onSetShareFollows });
      await fireEvent.click(screen.getByTestId('tune-btn'));
      await waitFor(() => expect(screen.getByRole('alert').textContent).toContain('ipc down'));
      expect((screen.getByTestId('share-follows-toggle') as HTMLInputElement).disabled).toBe(true);
      expect(onSetShareFollows).not.toHaveBeenCalled();
    });

    it('surfaces a share-follows write failure and reverts the box', async () => {
      const getShareFollows = vi.fn().mockResolvedValue(true);
      const onSetShareFollows = vi.fn().mockRejectedValue(new Error('not connected'));
      renderDiscover({ getShareFollows, onSetShareFollows });
      await fireEvent.click(screen.getByTestId('tune-btn'));
      await waitFor(() => expect(getShareFollows).toHaveBeenCalled());

      await fireEvent.click(screen.getByTestId('share-follows-toggle'));
      await waitFor(() => expect(screen.getByRole('alert').textContent).toContain('not connected'));
      expect((screen.getByTestId('share-follows-toggle') as HTMLInputElement).checked).toBe(true);
    });

    it('Share-my-vines-publicly toggle reads and writes through the props', async () => {
      const getShareVinesPublicly = vi.fn().mockResolvedValue(true);
      const onSetShareVinesPublicly = vi.fn().mockResolvedValue(undefined);
      renderDiscover({ getShareVinesPublicly, onSetShareVinesPublicly });
      await fireEvent.click(screen.getByTestId('tune-btn'));
      await waitFor(() => expect(getShareVinesPublicly).toHaveBeenCalled());

      await fireEvent.click(screen.getByTestId('share-vines-publicly-toggle'));
      await waitFor(() => expect(onSetShareVinesPublicly).toHaveBeenCalledWith(false));
    });

    it('keeps the share-vines-publicly toggle disabled when the read fails', async () => {
      const getShareVinesPublicly = vi.fn().mockRejectedValue(new Error('ipc down'));
      const onSetShareVinesPublicly = vi.fn();
      renderDiscover({ getShareVinesPublicly, onSetShareVinesPublicly });
      await fireEvent.click(screen.getByTestId('tune-btn'));
      await waitFor(() => expect(screen.getByRole('alert').textContent).toContain('ipc down'));
      expect((screen.getByTestId('share-vines-publicly-toggle') as HTMLInputElement).disabled).toBe(true);
      expect(onSetShareVinesPublicly).not.toHaveBeenCalled();
    });

    it('surfaces a share-vines-publicly write failure and reverts the box', async () => {
      const getShareVinesPublicly = vi.fn().mockResolvedValue(true);
      const onSetShareVinesPublicly = vi.fn().mockRejectedValue(new Error('not connected'));
      renderDiscover({ getShareVinesPublicly, onSetShareVinesPublicly });
      await fireEvent.click(screen.getByTestId('tune-btn'));
      await waitFor(() => expect(getShareVinesPublicly).toHaveBeenCalled());

      await fireEvent.click(screen.getByTestId('share-vines-publicly-toggle'));
      await waitFor(() => expect(screen.getByRole('alert').textContent).toContain('not connected'));
      expect((screen.getByTestId('share-vines-publicly-toggle') as HTMLInputElement).checked).toBe(true);
    });

    it('shows the graph-only empty state copy', () => {
      renderDiscover({ discoverVines: [] });
      expect(screen.getByText(/no algorithm, just your social graph/)).toBeTruthy();
    });
  });
});
