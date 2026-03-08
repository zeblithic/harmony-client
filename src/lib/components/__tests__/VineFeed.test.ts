import { render, screen, fireEvent } from '@testing-library/svelte';
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
    render(VineFeed, { props: { vines, viewedIds: makeViewedIds() } });
    expect(screen.getByText('Vines')).toBeTruthy();
  });

  it('renders all vine cards', () => {
    render(VineFeed, { props: { vines, viewedIds: makeViewedIds() } });
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('Bob')).toBeTruthy();
    expect(screen.getByText('Carol')).toBeTruthy();
  });

  it('shows unviewed count badge', () => {
    render(VineFeed, { props: { vines, viewedIds: makeViewedIds() } });
    expect(screen.getByText('2 new')).toBeTruthy();
  });

  it('hides unviewed badge when all viewed', () => {
    const allViewed = vines.map(v => ({ ...v, viewed: true }));
    render(VineFeed, { props: { vines: allViewed, viewedIds: makeViewedIds(allViewed) } });
    expect(screen.queryByText(/new/)).toBeNull();
  });

  it('shows empty state when no vines', () => {
    render(VineFeed, { props: { vines: [], viewedIds: new Set() } });
    expect(screen.getByText(/No vines yet/)).toBeTruthy();
  });

  it('sorts vines newest first', () => {
    render(VineFeed, { props: { vines, viewedIds: makeViewedIds() } });
    const items = screen.getAllByRole('listitem');
    expect(items.length).toBe(3);
    // Carol's vine (createdAt=200) should be first
    expect(items[0].textContent).toContain('Carol');
  });

  it('opens player when a card is clicked', async () => {
    render(VineFeed, { props: { vines, viewedIds: makeViewedIds() } });
    const cards = screen.getAllByRole('button');
    await fireEvent.click(cards[0]); // Click first card (Carol, newest)
    // Player dialog should appear
    expect(screen.getByRole('dialog')).toBeTruthy();
  });

  it('closes player when close button is clicked', async () => {
    render(VineFeed, { props: { vines, viewedIds: makeViewedIds() } });
    const cards = screen.getAllByRole('button');
    await fireEvent.click(cards[0]);
    expect(screen.getByRole('dialog')).toBeTruthy();
    const closeBtn = screen.getByLabelText('Close player');
    await fireEvent.click(closeBtn);
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('calls onMarkViewed when a vine is opened', async () => {
    const onMarkViewed = vi.fn();
    render(VineFeed, { props: { vines, viewedIds: makeViewedIds(), onMarkViewed } });
    const cards = screen.getAllByRole('button');
    await fireEvent.click(cards[0]); // Carol (vine-03)
    expect(onMarkViewed).toHaveBeenCalledWith('vine-03');
  });

  it('has accessible feed list', () => {
    render(VineFeed, { props: { vines, viewedIds: makeViewedIds() } });
    expect(screen.getByRole('list', { name: 'Vine feed' })).toBeTruthy();
  });
});
