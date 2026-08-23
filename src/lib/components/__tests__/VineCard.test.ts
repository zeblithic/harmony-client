import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import VineCard from '../VineCard.svelte';
import type { VineVideo } from '../../types';

const vine: VineVideo = {
  id: 'vine-01',
  creatorAddress: 'a1b2c3d4',
  creatorName: 'Alice',
  createdAt: 1700000000,
  videoCid: 'cid-abc',
  title: 'First vine',
  viewed: false,
};

function props(over: Record<string, unknown> = {}) {
  return { vine, onActivate: vi.fn(), ...over };
}

describe('VineCard (ZEB-612 S2 full-bleed)', () => {
  it('renders creator, title, and timestamp', () => {
    render(VineCard, props());
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('First vine')).toBeTruthy();
  });

  it('activates on click and on Enter (feed centers + plays it)', async () => {
    const onActivate = vi.fn();
    render(VineCard, props({ onActivate }));
    const card = screen.getByRole('button', { name: /First vine by Alice/ });
    await fireEvent.click(card);
    await fireEvent.keyDown(card, { key: 'Enter' });
    expect(onActivate).toHaveBeenCalledTimes(2);
  });

  it('mounts a muted looping video when a blob URL is supplied', () => {
    render(VineCard, props({ videoUrl: 'blob:fake-1' }));
    const video = screen.getByTestId('stage-video') as HTMLVideoElement;
    expect(video.getAttribute('src')).toBe('blob:fake-1');
    expect(video.hasAttribute('loop')).toBe(true);
    expect(video.muted).toBe(true);
  });

  it('renders the ▶ placeholder without a blob URL (outside the lazy window)', () => {
    render(VineCard, props());
    expect(screen.queryByTestId('stage-video')).toBeNull();
    expect(screen.getByText('▶')).toBeTruthy();
  });

  it('shows the ❚❚ paused glyph only over a loaded video when not playing', async () => {
    const { rerender } = render(VineCard, props({ videoUrl: 'blob:fake-1', isPlaying: false }));
    expect(screen.getByText('❚❚')).toBeTruthy();
    await rerender(props({ videoUrl: 'blob:fake-1', isPlaying: true }));
    expect(screen.queryByText('❚❚')).toBeNull();
  });

  it('omits the paused glyph on cards outside the lazy window (no stacked glyphs)', () => {
    render(VineCard, props({ videoUrl: null, isPlaying: false }));
    expect(screen.getByText('▶')).toBeTruthy();
    expect(screen.queryByText('❚❚')).toBeNull();
  });

  it('reports duration from loadedmetadata (honest badge source)', async () => {
    const onDuration = vi.fn();
    render(VineCard, props({ videoUrl: 'blob:fake-1', onDuration }));
    const video = screen.getByTestId('stage-video') as HTMLVideoElement;
    Object.defineProperty(video, 'duration', { value: 6.0, configurable: true });
    await fireEvent(video, new Event('loadedmetadata'));
    expect(onDuration).toHaveBeenCalledWith('cid-abc', 6.0);
  });

  it('renders the mono duration pill when duration is known', () => {
    render(VineCard, props({ duration: 6 }));
    expect(screen.getByTestId('duration-pill')).toHaveTextContent('↻ 0:06');
  });

  it('omits the duration pill when unknown (no fabricated duration)', () => {
    render(VineCard, props());
    expect(screen.queryByTestId('duration-pill')).toBeNull();
  });

  it('shows the clay unviewed dot only when unviewed', async () => {
    const { rerender } = render(VineCard, props({ isViewed: false }));
    expect(screen.getByLabelText('Unviewed')).toBeTruthy();
    await rerender(props({ isViewed: true }));
    expect(screen.queryByLabelText('Unviewed')).toBeNull();
  });

  it('renders reshare attribution with the view-original verb', async () => {
    const onViewOriginal = vi.fn();
    const reshare: VineVideo = {
      ...vine, id: 'vine-rs', reshareOf: 'vine-orig',
      creatorName: 'Bob', creatorAddress: 'bbbb',
      originalCreatorAddress: 'a1b2c3d4', originalCreatorName: 'Alice',
    };
    render(VineCard, props({ vine: reshare, onViewOriginal }));
    // The ↻ glyph is an aria-hidden sibling span — match the row's own text.
    expect(screen.getByText(/Bob reshared ·/)).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: 'view original by Alice' }));
    expect(onViewOriginal).toHaveBeenCalledWith('vine-orig');
  });

  it('offers the Reshare verb when canReshare, with in-flight state', async () => {
    const onReshare = vi.fn();
    const { rerender } = render(VineCard, props({ canReshare: true, onReshare }));
    await fireEvent.click(screen.getByRole('button', { name: 'Reshare vine' }));
    expect(onReshare).toHaveBeenCalledWith(vine);
    await rerender(props({ canReshare: true, onReshare, resharing: true }));
    expect(screen.getByRole('button', { name: 'Reshare vine' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Reshare vine' })).toHaveTextContent('Resharing…');
  });

  it('shows a static reshare-count chip when the verb is unavailable (own original)', () => {
    render(VineCard, props({ canReshare: false, reshareCount: 3 }));
    expect(screen.queryByRole('button', { name: 'Reshare vine' })).toBeNull();
    expect(screen.getByLabelText('reshare count 3')).toBeTruthy();
  });

  it('hides the reshare count on reshares themselves (counts credit originals)', () => {
    const reshare: VineVideo = { ...vine, id: 'r1', reshareOf: 'orig' };
    render(VineCard, props({ vine: reshare, canReshare: true, onReshare: vi.fn(), reshareCount: 5 }));
    expect(screen.queryByLabelText('reshare count 5')).toBeNull();
  });

  it('like button toggles and stops propagation to onActivate', async () => {
    const onToggleLike = vi.fn();
    const onActivate = vi.fn();
    render(VineCard, props({ onToggleLike, onActivate, reactionCount: 2, likedByMe: false }));
    await fireEvent.click(screen.getByRole('button', { name: 'Like First vine' }));
    expect(onToggleLike).toHaveBeenCalledWith(vine);
    expect(onActivate).not.toHaveBeenCalled();
  });

  it('follow button follows/unfollows without activating the card', async () => {
    const onFollow = vi.fn();
    const onActivate = vi.fn();
    render(VineCard, props({ showFollowButton: true, isFollowed: false, onFollow, onActivate }));
    await fireEvent.click(screen.getByRole('button', { name: 'Follow Alice' }));
    expect(onFollow).toHaveBeenCalledWith('a1b2c3d4', 'Alice');
    expect(onActivate).not.toHaveBeenCalled();
  });
});

describe('VineCard delete verb + removed stub (ZEB-670)', () => {
  it('offers Delete only when canDelete && onDelete, with in-flight state', async () => {
    const onDelete = vi.fn();
    const onActivate = vi.fn();
    const { rerender } = render(VineCard, props({ canDelete: true, onDelete, onActivate }));
    const btn = screen.getByRole('button', { name: 'Delete vine' });
    await fireEvent.click(btn);
    expect(onDelete).toHaveBeenCalledWith(expect.objectContaining({ id: 'vine-01' }));
    expect(onActivate).not.toHaveBeenCalled(); // stopPropagation

    await rerender(props({ canDelete: true, onDelete, deleting: true }));
    const busy = screen.getByRole('button', { name: 'Delete vine' }) as HTMLButtonElement;
    expect(busy.disabled).toBe(true);
    expect(busy.textContent).toContain('Deleting…');
  });

  it('hides Delete without canDelete (not our vine)', () => {
    render(VineCard, props({ canDelete: false, onDelete: vi.fn() }));
    expect(screen.queryByRole('button', { name: 'Delete vine' })).toBeNull();
  });

  it('renders originalRemoved reshares as a stub — no video, no like/reshare', () => {
    const stub: VineVideo = {
      ...vine,
      id: 'vine-stub',
      reshareOf: 'vine-gone',
      originalCreatorAddress: 'gone-addr',
      originalRemoved: true,
    };
    render(VineCard, {
      vine: stub,
      videoUrl: 'blob:fake-1', // resolved before the tombstone landed — must be ignored
      onToggleLike: vi.fn(),
      canReshare: true,
      onReshare: vi.fn(),
    });
    expect(screen.getByTestId('removed-stub').textContent).toContain('Removed by creator');
    expect(screen.queryByTestId('stage-video')).toBeNull();
    expect(screen.queryByRole('button', { name: /Like|Unlike/ })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Reshare vine' })).toBeNull();
    // Attribution context stays (whose reshare this was).
    expect(screen.getByText(/reshared/)).toBeTruthy();
  });
});

// ── ZEB-978: author label resolves through the shared ladder ────────────

describe('VineCard author ladder (ZEB-978)', () => {
  const ADDR = 'a1b2c3d4e5f60718293a4b5c6d7e8f90';
  const spoof: VineVideo = { ...vine, creatorAddress: ADDR, creatorName: 'Trusted Friend' };

  it('renders a petname over the wire creatorName, badged as petname', () => {
    render(VineCard, props({
      vine: spoof,
      resolveNickname: (id: string) => (id === ADDR ? 'Actual Rando' : undefined),
    }));
    expect(screen.queryByText('Trusted Friend')).toBeNull();
    const name = screen.getByText('Actual Rando');
    expect(name.closest('[data-name-source]')?.getAttribute('data-name-source')).toBe('petname');
  });

  it('renders the verified card name when no petname is assigned', () => {
    render(VineCard, props({
      vine: spoof,
      resolveCard: (id: string) => (id === ADDR ? { displayName: 'Zebulon' } : undefined),
    }));
    expect(screen.queryByText('Trusted Friend')).toBeNull();
    const name = screen.getByText('Zebulon');
    expect(name.closest('[data-name-source]')?.getAttribute('data-name-source')).toBe('card');
  });

  it('marks a wire-only creatorName as unverified', () => {
    render(VineCard, props({ vine: spoof }));
    const name = screen.getByText('Trusted Friend');
    expect(name.closest('[data-name-source]')?.getAttribute('data-name-source')).toBe('wire');
  });

  it('resolves the reshare attribution through the ORIGINAL creator address', () => {
    const reshare: VineVideo = {
      ...vine, id: 'vine-rs2', reshareOf: 'vine-00',
      creatorAddress: ADDR, creatorName: 'Resharer',
      originalCreatorAddress: 'feedfacecafebeef0123456789abcdef',
      originalCreatorName: 'Wire Snapshot',
    };
    render(VineCard, props({
      vine: reshare,
      onViewOriginal: vi.fn(),
      resolveNickname: (id: string) =>
        (id === 'feedfacecafebeef0123456789abcdef' ? 'Origin Pet' : undefined),
    }));
    expect(screen.getByRole('button', { name: 'view original by Origin Pet' })).toBeTruthy();
    expect(screen.queryByText(/Wire Snapshot/)).toBeNull();
  });

  it('uses the resolved label in the card aria-label', () => {
    render(VineCard, props({
      vine: spoof,
      resolveNickname: (id: string) => (id === ADDR ? 'Actual Rando' : undefined),
    }));
    expect(screen.getByRole('button', { name: /First vine by Actual Rando/ })).toBeTruthy();
  });

  it('passes the resolved label to onFollow (stored name = the name you know them by)', async () => {
    const onFollow = vi.fn();
    render(VineCard, props({
      vine: spoof,
      showFollowButton: true,
      onFollow,
      resolveNickname: (id: string) => (id === ADDR ? 'Actual Rando' : undefined),
    }));
    await fireEvent.click(screen.getByRole('button', { name: 'Follow Actual Rando' }));
    expect(onFollow).toHaveBeenCalledWith(ADDR, 'Actual Rando');
  });
});
