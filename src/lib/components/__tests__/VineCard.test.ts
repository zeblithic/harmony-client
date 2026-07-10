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

  it('shows the ❚❚ paused glyph only when not playing', async () => {
    const { rerender } = render(VineCard, props({ isPlaying: false }));
    expect(screen.getByText('❚❚')).toBeTruthy();
    await rerender(props({ isPlaying: true }));
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
