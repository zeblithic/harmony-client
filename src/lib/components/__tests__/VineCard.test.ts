import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import VineCard from '../VineCard.svelte';
import type { VineVideo } from '../../types';

const vine: VineVideo = {
  id: 'vine-01',
  creatorAddress: 'a1b2c3d4',
  creatorName: 'Alice',
  createdAt: 1700000000,
  videoCid: 'cid-abc123',
  title: 'Demo vine',
  viewed: false,
};

describe('VineCard', () => {
  it('renders creator name and title', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn() } });
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('Demo vine')).toBeTruthy();
  });

  it('shows unviewed indicator for unviewed vines', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn() } });
    expect(screen.getByLabelText('Unviewed')).toBeTruthy();
  });

  it('hides unviewed indicator for viewed vines', () => {
    const viewed = { ...vine, viewed: true };
    render(VineCard, { props: { vine: viewed, onPlay: vi.fn() } });
    expect(screen.queryByLabelText('Unviewed')).toBeNull();
  });

  it('shows attribution row when vine is a reshare', () => {
    const reshared = {
      ...vine,
      reshareOf: 'vine-00',
      originalCreatorName: 'Original Person',
    };
    render(VineCard, { props: { vine: reshared, onPlay: vi.fn() } });
    expect(screen.getByText(/originally by Original Person/i)).toBeTruthy();
  });

  it('does not show attribution row for original vines', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn() } });
    expect(screen.queryByText(/originally by/i)).toBeNull();
  });

  it('attribution row is clickable when onViewOriginal is provided', async () => {
    const onViewOriginal = vi.fn();
    const reshared = {
      ...vine,
      reshareOf: 'vine-00',
      originalCreatorName: 'Original Person',
    };
    render(VineCard, { props: { vine: reshared, onPlay: vi.fn(), onViewOriginal } });
    const link = screen.getByRole('button', { name: /originally by Original Person/i });
    await fireEvent.click(link);
    expect(onViewOriginal).toHaveBeenCalledWith('vine-00');
  });

  it('clicking attribution row does not also trigger onPlay (stops propagation)', async () => {
    const onPlay = vi.fn();
    const onViewOriginal = vi.fn();
    const reshared = {
      ...vine,
      reshareOf: 'vine-00',
      originalCreatorName: 'Original Person',
    };
    render(VineCard, { props: { vine: reshared, onPlay, onViewOriginal } });
    const link = screen.getByRole('button', { name: /originally by Original Person/i });
    await fireEvent.click(link);
    expect(onViewOriginal).toHaveBeenCalled();
    expect(onPlay).not.toHaveBeenCalled();
  });

  it('calls onPlay with vine when clicked', async () => {
    const onPlay = vi.fn();
    render(VineCard, { props: { vine, onPlay } });
    const card = screen.getByRole('button');
    await fireEvent.click(card);
    expect(onPlay).toHaveBeenCalledWith(vine);
  });

  it('calls onPlay on Enter key', async () => {
    const onPlay = vi.fn();
    render(VineCard, { props: { vine, onPlay } });
    const card = screen.getByRole('button');
    await fireEvent.keyDown(card, { key: 'Enter' });
    expect(onPlay).toHaveBeenCalledWith(vine);
  });

  it('calls onPlay on Space key', async () => {
    const onPlay = vi.fn();
    render(VineCard, { props: { vine, onPlay } });
    const card = screen.getByRole('button');
    await fireEvent.keyDown(card, { key: ' ' });
    expect(onPlay).toHaveBeenCalledWith(vine);
  });

  it('has accessible label with title and creator', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn() } });
    const card = screen.getByRole('button');
    expect(card.getAttribute('aria-label')).toBe('Demo vine by Alice');
  });

  it('uses Untitled for vines without title in aria-label', () => {
    const untitled = { ...vine, title: undefined };
    render(VineCard, { props: { vine: untitled, onPlay: vi.fn() } });
    const card = screen.getByRole('button');
    expect(card.getAttribute('aria-label')).toBe('Untitled vine by Alice');
  });

  it('renders follow button when showFollowButton is true and not followed', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), showFollowButton: true, isFollowed: false, onFollow: vi.fn() } });
    expect(screen.getByLabelText(/Follow/)).toBeTruthy();
  });

  it('does not render follow button when showFollowButton is false', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), showFollowButton: false } });
    expect(screen.queryByLabelText(/Follow/)).toBeNull();
  });

  it('renders Following badge when followed', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), showFollowButton: true, isFollowed: true, onUnfollow: vi.fn() } });
    expect(screen.getByText('Following')).toBeTruthy();
  });

  it('calls onFollow with address and name when follow clicked', async () => {
    const onFollow = vi.fn();
    render(VineCard, { props: { vine, onPlay: vi.fn(), showFollowButton: true, isFollowed: false, onFollow } });
    await fireEvent.click(screen.getByLabelText(/Follow/));
    expect(onFollow).toHaveBeenCalledWith('a1b2c3d4', 'Alice');
  });

  it('follow button click does not trigger onPlay', async () => {
    const onPlay = vi.fn();
    const onFollow = vi.fn();
    render(VineCard, { props: { vine, onPlay, showFollowButton: true, isFollowed: false, onFollow } });
    await fireEvent.click(screen.getByLabelText(/Follow/));
    expect(onPlay).not.toHaveBeenCalled();
  });

  it('shows like count when count > 0', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), reactionCount: 3, likedByMe: false } });
    expect(screen.getByText('3')).toBeTruthy();
  });

  it('shows filled heart when liked by me', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), reactionCount: 1, likedByMe: true } });
    expect(screen.getByLabelText('Unlike Demo vine')).toBeTruthy();
  });

  it('shows outline heart when not liked by me', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), reactionCount: 1, likedByMe: false } });
    expect(screen.getByLabelText('Like Demo vine')).toBeTruthy();
  });

  it('hides like row when count is 0 and not liked', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), reactionCount: 0, likedByMe: false } });
    expect(screen.queryByLabelText(/Like/)).toBeNull();
    expect(screen.queryByLabelText(/Unlike/)).toBeNull();
  });

  it('calls onToggleLike when heart clicked', async () => {
    const onToggleLike = vi.fn();
    render(VineCard, { props: { vine, onPlay: vi.fn(), reactionCount: 1, likedByMe: false, onToggleLike } });
    await fireEvent.click(screen.getByLabelText('Like Demo vine'));
    expect(onToggleLike).toHaveBeenCalledWith(vine);
  });

  it('like button click does not trigger onPlay', async () => {
    const onPlay = vi.fn();
    const onToggleLike = vi.fn();
    render(VineCard, { props: { vine, onPlay, reactionCount: 1, likedByMe: false, onToggleLike } });
    await fireEvent.click(screen.getByLabelText('Like Demo vine'));
    expect(onPlay).not.toHaveBeenCalled();
  });

  it('shows reshare count for originals when count > 0', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), reshareCount: 3 } });
    expect(screen.getByLabelText(/reshare count/i)).toBeTruthy();
    expect(screen.getByLabelText(/reshare count/i).textContent).toMatch(/3/);
  });

  it('hides reshare count when zero', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), reshareCount: 0 } });
    expect(screen.queryByLabelText(/reshare count/i)).toBeNull();
  });

  it('does not show reshare count for reshares (counts only meaningful on originals)', () => {
    const reshared = {
      ...vine,
      reshareOf: 'vine-00',
      originalCreatorName: 'Original',
    };
    render(VineCard, { props: { vine: reshared, onPlay: vi.fn(), reshareCount: 5 } });
    expect(screen.queryByLabelText(/reshare count/i)).toBeNull();
  });
});
