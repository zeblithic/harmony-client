import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import VinePlayer from '../VinePlayer.svelte';
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

describe('VinePlayer', () => {
  it('renders creator name and title', () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn() } });
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('Demo vine')).toBeTruthy();
  });

  it('does not show legacy "Reshared" label anymore (replaced by attribution row)', () => {
    const resharedVine: VineVideo = {
      ...vine,
      reshareOf: 'vine-00',
      originalCreatorName: 'Original',
    };
    render(VinePlayer, { props: { vine: resharedVine, onClose: vi.fn() } });
    // "Reshared" by itself should no longer appear; attribution row replaces it.
    expect(screen.queryByText(/^Reshared$/)).toBeNull();
  });

  it('shows attribution row when vine is a reshare', () => {
    const resharedVine: VineVideo = {
      ...vine,
      reshareOf: 'vine-00',
      originalCreatorName: 'Original Person',
    };
    render(VinePlayer, {
      props: { vine: resharedVine, onClose: vi.fn() },
    });
    expect(screen.getByText(/originally by Original Person/i)).toBeTruthy();
  });

  it('attribution row is clickable when onViewOriginal is provided', async () => {
    const onViewOriginal = vi.fn();
    const resharedVine: VineVideo = {
      ...vine,
      reshareOf: 'vine-00',
      originalCreatorName: 'Original Person',
    };
    render(VinePlayer, {
      props: {
        vine: resharedVine,
        onClose: vi.fn(),
        onViewOriginal,
      },
    });
    const link = screen.getByRole('button', { name: /originally by Original Person/i });
    await fireEvent.click(link);
    expect(onViewOriginal).toHaveBeenCalledWith('vine-00');
  });

  it('does not render attribution row for non-reshare original vines', () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn() } });
    expect(screen.queryByText(/originally by/i)).toBeNull();
  });

  // ── Reshare confirmation flow ─────────────────────────────────────

  it('opens confirmation dialog when Reshare button is clicked', async () => {
    render(VinePlayer, {
      props: { vine, onClose: vi.fn(), onReshare: vi.fn() },
    });
    await fireEvent.click(screen.getByLabelText('Reshare vine'));
    // Dialog should now be visible.
    expect(screen.getByText(/reshare this vine\?/i)).toBeTruthy();
  });

  it('calls onReshare only after dialog confirm', async () => {
    const onReshare = vi.fn().mockResolvedValue(undefined);
    render(VinePlayer, {
      props: { vine, onClose: vi.fn(), onReshare },
    });
    await fireEvent.click(screen.getByLabelText('Reshare vine'));
    expect(onReshare).not.toHaveBeenCalled();
    // The dialog's confirm button has the bare label "Reshare" (the player's
    // button is labelled "↗ Reshare" or "↗ Resharing…"), so /^reshare$/i
    // disambiguates to the dialog's confirm button.
    await fireEvent.click(screen.getByRole('button', { name: /^reshare$/i }));
    expect(onReshare).toHaveBeenCalledWith(vine);
  });

  it('does not call onReshare on dialog cancel', async () => {
    const onReshare = vi.fn();
    render(VinePlayer, {
      props: { vine, onClose: vi.fn(), onReshare },
    });
    await fireEvent.click(screen.getByLabelText('Reshare vine'));
    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));
    expect(onReshare).not.toHaveBeenCalled();
  });

  it('hides Reshare button on own original vines', () => {
    const ownOriginal: VineVideo = { ...vine, creatorAddress: 'self', reshareOf: undefined };
    render(VinePlayer, {
      props: { vine: ownOriginal, onClose: vi.fn(), onReshare: vi.fn() },
    });
    expect(screen.queryByLabelText('Reshare vine')).toBeNull();
  });

  it("shows Reshare button on own reshare of someone else's vine", () => {
    // Edge: I reshared someone else's vine. I should still be able to
    // re-reshare (or unshare → reshare). The hide rule is "own ORIGINAL",
    // not "anything I published".
    const ownReshare: VineVideo = { ...vine, creatorAddress: 'self', reshareOf: 'vine-orig' };
    render(VinePlayer, {
      props: { vine: ownReshare, onClose: vi.fn(), onReshare: vi.fn() },
    });
    expect(screen.getByLabelText('Reshare vine')).toBeTruthy();
  });

  it('hides Reshare button when creatorAddress matches ownAddress (hex form, no "self" remap yet)', () => {
    // FIX 2 (PR #120 round 1): vines that arrive on the wire before
    // `vineService.ownAddress` is set don't get remapped to the magic
    // `'self'` value by `wireToVine`. Without the ownAddress prop, the
    // 'self'-only check missed those vines, and the Reshare button
    // showed — letting the user accidentally self-reshare via the
    // bot-discovered-then-removed publish() guard.
    const myAddress = '0xabcdef0123';
    const ownHexOriginal: VineVideo = {
      ...vine,
      creatorAddress: myAddress,
      reshareOf: undefined,
    };
    render(VinePlayer, {
      props: {
        vine: ownHexOriginal,
        onClose: vi.fn(),
        onReshare: vi.fn(),
        ownAddress: myAddress,
      },
    });
    expect(screen.queryByLabelText('Reshare vine')).toBeNull();
  });

  it('calls onClose when close button is clicked', async () => {
    const onClose = vi.fn();
    render(VinePlayer, { props: { vine, onClose } });
    const closeBtn = screen.getByLabelText('Close player');
    await fireEvent.click(closeBtn);
    expect(onClose).toHaveBeenCalledOnce();
  });

  it('renders next/previous buttons when callbacks provided', () => {
    render(VinePlayer, {
      props: { vine, onClose: vi.fn(), onNext: vi.fn(), onPrevious: vi.fn() },
    });
    expect(screen.getByLabelText('Next vine')).toBeTruthy();
    expect(screen.getByLabelText('Previous vine')).toBeTruthy();
  });

  it('does not render nav buttons when callbacks are absent', () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn() } });
    expect(screen.queryByLabelText('Next vine')).toBeNull();
    expect(screen.queryByLabelText('Previous vine')).toBeNull();
  });

  it('calls onNext when next button is clicked', async () => {
    const onNext = vi.fn();
    render(VinePlayer, {
      props: { vine, onClose: vi.fn(), onNext },
    });
    await fireEvent.click(screen.getByLabelText('Next vine'));
    expect(onNext).toHaveBeenCalledOnce();
  });

  it('shows placeholder when no resolveVideo provided', () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn() } });
    expect(screen.getByText('Video playback coming soon')).toBeTruthy();
    expect(screen.getByText('cid-abc123')).toBeTruthy();
  });

  it('has dialog role for accessibility', () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn() } });
    expect(screen.getByRole('dialog')).toBeTruthy();
  });

  // ── Video playback ──────────────────────────────────────────────

  it('shows loading state while resolving video', () => {
    // Never-resolving promise simulates an in-flight fetch.
    const resolveVideo = vi.fn().mockReturnValue(new Promise(() => {}));
    render(VinePlayer, { props: { vine, onClose: vi.fn(), resolveVideo } });
    expect(screen.getByText('Loading video…')).toBeTruthy();
    expect(resolveVideo).toHaveBeenCalledWith('cid-abc123');
  });

  it('shows video element after resolution', async () => {
    const blobUrl = 'blob:http://localhost/fake-video';
    const resolveVideo = vi.fn().mockResolvedValue(blobUrl);
    render(VinePlayer, { props: { vine, onClose: vi.fn(), resolveVideo } });
    await waitFor(() => {
      const video = screen.getByLabelText('Vine video') as HTMLVideoElement;
      expect(video.tagName).toBe('VIDEO');
      expect(video.src).toBe(blobUrl);
    });
  });

  it('shows error state when resolution fails', async () => {
    const resolveVideo = vi.fn().mockRejectedValue(new Error('not found'));
    render(VinePlayer, { props: { vine, onClose: vi.fn(), resolveVideo } });
    await waitFor(() => {
      expect(screen.getByText('not found')).toBeTruthy();
    });
  });

  // ── Like button ──────────────────────────────────────────────────

  it('renders like button when onToggleLike provided', () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn(), onToggleLike: vi.fn(), reactionCount: 0, likedByMe: false } });
    expect(screen.getByLabelText('Like Demo vine')).toBeTruthy();
  });

  it('does not render like button when onToggleLike absent', () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn() } });
    expect(screen.queryByLabelText(/Like/)).toBeNull();
    expect(screen.queryByLabelText(/Unlike/)).toBeNull();
  });

  it('shows filled heart when liked', () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn(), onToggleLike: vi.fn(), reactionCount: 3, likedByMe: true } });
    expect(screen.getByLabelText('Unlike Demo vine')).toBeTruthy();
  });

  it('shows reaction count', () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn(), onToggleLike: vi.fn(), reactionCount: 5, likedByMe: false } });
    expect(screen.getByText('5')).toBeTruthy();
  });

  it('calls onToggleLike when like button clicked', async () => {
    const onToggleLike = vi.fn();
    render(VinePlayer, { props: { vine, onClose: vi.fn(), onToggleLike, reactionCount: 0, likedByMe: false } });
    await fireEvent.click(screen.getByLabelText('Like Demo vine'));
    expect(onToggleLike).toHaveBeenCalledWith(vine);
  });
});
