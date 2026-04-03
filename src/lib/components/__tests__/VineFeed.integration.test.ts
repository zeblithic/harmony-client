/**
 * Integration test for the VineFeed subsystem.
 *
 * Tests end-to-end: render vine cards sorted by date, filter tabs,
 * open/close player, player navigation, mark viewed, reshare, create
 * button, and empty states.
 *
 * Uses real VineVideo data and real component tree (VineFeed → VineCard
 * → VinePlayer), no service stubs.
 */
import { render, screen, fireEvent, cleanup } from '@testing-library/svelte';
import { describe, it, expect, vi, afterEach } from 'vitest';
import VineFeed from '../VineFeed.svelte';
import type { VineVideo } from '../../types';

afterEach(cleanup);

// ── Shared fixtures ────────────────────────────────────────────────

const vineBase = Date.now() / 1000 - 3600;

const VINES: VineVideo[] = [
  {
    id: 'v1', creatorAddress: 'a1', creatorName: 'Alice',
    createdAt: vineBase, videoCid: 'cid-v1',
    title: 'Transport demo', viewed: false,
  },
  {
    id: 'v2', creatorAddress: 'b2', creatorName: 'Bob',
    createdAt: vineBase + 120, videoCid: 'cid-v2',
    title: 'Mesh routing', viewed: false,
  },
  {
    id: 'v3', creatorAddress: 'c3', creatorName: 'Carol',
    createdAt: vineBase + 300, videoCid: 'cid-v3',
    viewed: false,
  },
  {
    id: 'v4', creatorAddress: 'a1', creatorName: 'Alice',
    createdAt: vineBase + 600, videoCid: 'cid-v4',
    title: 'Cache explained', reshareOf: 'v2', viewed: false,
  },
];

// ── Helper ──────────────────────────────────────────────────────────

function renderFeed(overrides: Record<string, unknown> = {}) {
  const callbacks = {
    onMarkViewed: vi.fn(),
    onPublish: vi.fn(),
    onReshare: vi.fn(),
  };

  const result = render(VineFeed, {
    props: {
      vines: VINES,
      viewedIds: new Set<string>(),
      ...callbacks,
      ...overrides,
    },
  });

  return { ...result, callbacks };
}

describe('VineFeed Integration', () => {
  // ── 1. Rendering ──────────────────────────────────────────────────

  it('renders vine cards sorted newest first', () => {
    renderFeed();
    const cards = screen.getAllByRole('button').filter(
      btn => btn.getAttribute('aria-label')?.includes('by ')
    );
    // v4 (newest) should appear before v1 (oldest)
    expect(cards.length).toBe(4);
    expect(cards[0].getAttribute('aria-label')).toContain('Cache explained');
    expect(cards[cards.length - 1].getAttribute('aria-label')).toContain('Transport demo');
  });

  it('shows creator names on cards', () => {
    renderFeed();
    expect(screen.getAllByText('Alice').length).toBeGreaterThan(0);
    expect(screen.getAllByText('Bob').length).toBeGreaterThan(0);
    expect(screen.getAllByText('Carol').length).toBeGreaterThan(0);
  });

  it('shows vine titles', () => {
    renderFeed();
    expect(screen.getByText('Transport demo')).toBeTruthy();
    expect(screen.getByText('Mesh routing')).toBeTruthy();
    expect(screen.getByText('Cache explained')).toBeTruthy();
  });

  it('shows reshare badge for reshared vines', () => {
    renderFeed();
    expect(screen.getByText('reshare')).toBeTruthy();
  });

  // ── 2. Unviewed count ─────────────────────────────────────────────

  it('shows unviewed count when there are unviewed vines', () => {
    renderFeed();
    expect(screen.getByText('4 new')).toBeTruthy();
  });

  it('hides unviewed count when all are viewed', () => {
    renderFeed({ viewedIds: new Set(['v1', 'v2', 'v3', 'v4']) });
    expect(screen.queryByText(/new$/)).toBeNull();
  });

  // ── 3. Filter tabs ────────────────────────────────────────────────

  it('defaults to All filter', () => {
    renderFeed();
    const allBtn = screen.getByText('All');
    expect(allBtn.classList.contains('active')).toBe(true);
  });

  it('filters to unviewed vines when Unviewed tab is clicked', async () => {
    renderFeed({ viewedIds: new Set(['v1', 'v2']) });

    const unviewedTab = screen.getByText(/^Unviewed/);
    await fireEvent.click(unviewedTab);

    // Only v3 and v4 should remain (unviewed)
    expect(screen.queryByText('Transport demo')).toBeNull();
    expect(screen.queryByText('Mesh routing')).toBeNull();
    expect(screen.getByText('Cache explained')).toBeTruthy();
  });

  it('shows empty state when all vines are viewed in Unviewed filter', async () => {
    renderFeed({ viewedIds: new Set(['v1', 'v2', 'v3', 'v4']) });

    const unviewedTab = screen.getByText(/^Unviewed/);
    await fireEvent.click(unviewedTab);

    expect(screen.getByText(/All caught up/)).toBeTruthy();
  });

  it('shows empty state when no vines exist', () => {
    renderFeed({ vines: [] });
    expect(screen.getByText(/No vines yet/)).toBeTruthy();
  });

  // ── 4. Player ─────────────────────────────────────────────────────

  it('opens player when a vine card is clicked', async () => {
    renderFeed();
    const card = screen.getByLabelText('Transport demo by Alice');
    await fireEvent.click(card);

    // Player overlay appears
    expect(screen.getByRole('dialog', { name: 'Vine player' })).toBeTruthy();
    // Shows vine CID in player
    expect(screen.getByText('cid-v1')).toBeTruthy();
  });

  it('marks vine as viewed when player opens', async () => {
    const { callbacks } = renderFeed();
    const card = screen.getByLabelText('Transport demo by Alice');
    await fireEvent.click(card);

    expect(callbacks.onMarkViewed).toHaveBeenCalledWith('v1');
  });

  it('closes player when close button is clicked', async () => {
    renderFeed();
    const card = screen.getByLabelText('Transport demo by Alice');
    await fireEvent.click(card);

    const closeBtn = screen.getByLabelText('Close player');
    await fireEvent.click(closeBtn);

    expect(screen.queryByRole('dialog', { name: 'Vine player' })).toBeNull();
  });

  it('closes player on Escape key', async () => {
    renderFeed();
    const card = screen.getByLabelText('Transport demo by Alice');
    await fireEvent.click(card);

    await fireEvent.keyDown(window, { key: 'Escape' });

    expect(screen.queryByRole('dialog', { name: 'Vine player' })).toBeNull();
  });

  it('navigates to next vine with ArrowRight', async () => {
    const { callbacks } = renderFeed();
    // Open the first vine in sorted order (v4 — newest)
    const card = screen.getByLabelText('Cache explained by Alice');
    await fireEvent.click(card);

    // Navigate next
    await fireEvent.keyDown(window, { key: 'ArrowRight' });

    // Should mark v3 (untitled) as viewed
    expect(callbacks.onMarkViewed).toHaveBeenCalledWith('v3');
  });

  it('navigates to previous vine with ArrowLeft', async () => {
    const { callbacks } = renderFeed();
    // Open the second vine in sorted order (v3 — Carol's untitled)
    const card = screen.getByLabelText('Untitled vine by Carol');
    await fireEvent.click(card);
    callbacks.onMarkViewed.mockClear();

    // Navigate previous → should go to v4 (newest in sorted order)
    await fireEvent.keyDown(window, { key: 'ArrowLeft' });

    expect(callbacks.onMarkViewed).toHaveBeenCalledWith('v4');
  });

  // ── 5. Reshare ────────────────────────────────────────────────────

  it('shows reshare button in player', async () => {
    renderFeed();
    const card = screen.getByLabelText('Transport demo by Alice');
    await fireEvent.click(card);

    expect(screen.getByLabelText('Reshare vine')).toBeTruthy();
  });

  it('calls onReshare when reshare button is clicked', async () => {
    const { callbacks } = renderFeed();
    callbacks.onReshare.mockResolvedValue(undefined);

    const card = screen.getByLabelText('Transport demo by Alice');
    await fireEvent.click(card);

    const reshareBtn = screen.getByLabelText('Reshare vine');
    await fireEvent.click(reshareBtn);

    expect(callbacks.onReshare).toHaveBeenCalledWith(
      expect.objectContaining({ id: 'v1' })
    );
  });

  // ── 6. Create button ──────────────────────────────────────────────

  it('shows create button and fires onPublish', async () => {
    const { callbacks } = renderFeed();
    const createBtn = screen.getByLabelText('Create vine');
    await fireEvent.click(createBtn);

    expect(callbacks.onPublish).toHaveBeenCalled();
  });

  it('hides create button when onPublish is not provided', () => {
    renderFeed({ onPublish: undefined });
    expect(screen.queryByLabelText('Create vine')).toBeNull();
  });

  // ── 7. Accessibility ──────────────────────────────────────────────

  it('renders vine feed as a list with listitem roles', () => {
    renderFeed();
    expect(screen.getByRole('list', { name: 'Vine feed' })).toBeTruthy();
    expect(screen.getAllByRole('listitem').length).toBe(4);
  });

  it('vine cards are keyboard accessible', async () => {
    const { callbacks } = renderFeed();
    const card = screen.getByLabelText('Transport demo by Alice');
    await fireEvent.keyDown(card, { key: 'Enter' });

    expect(callbacks.onMarkViewed).toHaveBeenCalledWith('v1');
  });
});
