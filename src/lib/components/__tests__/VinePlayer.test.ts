import { render, screen, fireEvent } from '@testing-library/svelte';
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

  it('shows reshare label when vine is a reshare', () => {
    const resharedVine = { ...vine, reshareOf: 'vine-00' };
    render(VinePlayer, { props: { vine: resharedVine, onClose: vi.fn() } });
    expect(screen.getByText('Reshared')).toBeTruthy();
  });

  it('does not show reshare label for original vines', () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn() } });
    expect(screen.queryByText('Reshared')).toBeNull();
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

  it('shows video CID in placeholder', () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn() } });
    expect(screen.getByText('cid-abc123')).toBeTruthy();
  });

  it('has dialog role for accessibility', () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn() } });
    expect(screen.getByRole('dialog')).toBeTruthy();
  });
});
