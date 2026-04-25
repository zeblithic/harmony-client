import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import PublishedView from '../PublishedView.svelte';
import type { PublishedItem } from '../../types';

const day = 86_400_000;
const now = Date.now();

const items: PublishedItem[] = [
  {
    cid: 'cid-pub-blog',
    name: 'decentralized-identity-post.md',
    category: 'text',
    sizeBytes: 12_000,
    publishedAt: now - 14 * day,
    publishMode: 'durable',
  },
  {
    cid: 'cid-pub-album',
    name: 'ambient-loops-vol2.zip',
    category: 'music',
    sizeBytes: 120_000_000,
    publishedAt: now - 7 * day,
    publishMode: 'durable',
  },
  {
    cid: 'cid-pub-status',
    name: 'weekly-status-update.txt',
    category: 'text',
    sizeBytes: 3_500,
    publishedAt: now - 1 * day,
    publishMode: 'ephemeral',
  },
];

function baseProps() {
  return { items };
}

describe('PublishedView', () => {
  // ── Basic rendering ──────────────────────────────────────────────

  it('renders a heading for the published catalog', () => {
    render(PublishedView, { props: baseProps() });
    expect(screen.getByRole('heading', { name: /published content/i })).toBeTruthy();
  });

  it('renders all published items by name', () => {
    render(PublishedView, { props: baseProps() });
    expect(screen.getByText('decentralized-identity-post.md')).toBeTruthy();
    expect(screen.getByText('ambient-loops-vol2.zip')).toBeTruthy();
    expect(screen.getByText('weekly-status-update.txt')).toBeTruthy();
  });

  it('renders items in a list role', () => {
    render(PublishedView, { props: baseProps() });
    expect(screen.getByRole('list')).toBeTruthy();
    const listItems = screen.getAllByRole('listitem');
    expect(listItems).toHaveLength(3);
  });

  // ── Publish mode indicators ──────────────────────────────────────

  it('shows globe icon for durable publish mode', () => {
    render(PublishedView, { props: baseProps() });
    // Two durable items -> two globe icons
    const globes = screen.getAllByText('🌐');
    expect(globes).toHaveLength(2);
  });

  it('shows wind icon for ephemeral publish mode', () => {
    render(PublishedView, { props: baseProps() });
    const winds = screen.getAllByText('🌬');
    expect(winds).toHaveLength(1);
  });

  it('labels durable mode icon for screen readers', () => {
    render(PublishedView, { props: baseProps() });
    const durableLabels = screen.getAllByLabelText(/durable/i);
    expect(durableLabels.length).toBeGreaterThanOrEqual(2);
  });

  it('labels ephemeral mode icon for screen readers', () => {
    render(PublishedView, { props: baseProps() });
    const ephemeralLabels = screen.getAllByLabelText(/ephemeral/i);
    expect(ephemeralLabels.length).toBeGreaterThanOrEqual(1);
  });

  // ── Item metadata ────────────────────────────────────────────────

  it('shows formatted file size for each item', () => {
    render(PublishedView, { props: baseProps() });
    expect(screen.getByText('12.0 KB')).toBeTruthy();
    expect(screen.getByText('120.0 MB')).toBeTruthy();
    expect(screen.getByText('3.5 KB')).toBeTruthy();
  });

  it('shows category icon for each item', () => {
    render(PublishedView, { props: baseProps() });
    // categoryIcon('text') = '📄', categoryIcon('music') = '♪'
    const textIcons = screen.getAllByText('📄');
    expect(textIcons).toHaveLength(2); // two text items
    expect(screen.getByText('♪')).toBeTruthy(); // one music item
  });

  it('shows relative time for publish dates', () => {
    render(PublishedView, { props: baseProps() });
    // relativeTime uses "d ago", "h ago" format
    // 14 days ago -> "14d ago", 7 days -> "7d ago", 1 day -> "24h ago" or "1d ago"
    expect(screen.getByText('14d ago')).toBeTruthy();
    expect(screen.getByText('7d ago')).toBeTruthy();
  });

  // ── Empty state ──────────────────────────────────────────────────

  it('shows empty message when no published items', () => {
    render(PublishedView, { props: { items: [] } });
    expect(screen.getByText(/no published content/i)).toBeTruthy();
  });

  it('does not show the list when empty', () => {
    render(PublishedView, { props: { items: [] } });
    expect(screen.queryByRole('list')).toBeNull();
  });

  // ── Search ───────────────────────────────────────────────────────

  it('renders a search input', () => {
    render(PublishedView, { props: baseProps() });
    expect(screen.getByRole('searchbox')).toBeTruthy();
  });

  it('filters items by search query', async () => {
    render(PublishedView, { props: baseProps() });
    const searchInput = screen.getByRole('searchbox');
    await fireEvent.input(searchInput, { target: { value: 'ambient' } });
    expect(screen.getByText('ambient-loops-vol2.zip')).toBeTruthy();
    expect(screen.queryByText('decentralized-identity-post.md')).toBeNull();
    expect(screen.queryByText('weekly-status-update.txt')).toBeNull();
  });

  it('search is case-insensitive', async () => {
    render(PublishedView, { props: baseProps() });
    const searchInput = screen.getByRole('searchbox');
    await fireEvent.input(searchInput, { target: { value: 'AMBIENT' } });
    expect(screen.getByText('ambient-loops-vol2.zip')).toBeTruthy();
  });

  it('shows empty state when search matches nothing', async () => {
    render(PublishedView, { props: baseProps() });
    const searchInput = screen.getByRole('searchbox');
    await fireEvent.input(searchInput, { target: { value: 'nonexistent-file' } });
    expect(screen.getByText(/no published content/i)).toBeTruthy();
  });

  // ── Category filter ──────────────────────────────────────────────

  it('renders category filter buttons', () => {
    render(PublishedView, { props: baseProps() });
    // Should have category filter buttons for categories present in items
    expect(screen.getByRole('button', { name: /all/i })).toBeTruthy();
    expect(screen.getByRole('button', { name: /text/i })).toBeTruthy();
    expect(screen.getByRole('button', { name: /music/i })).toBeTruthy();
  });

  it('filters by category when a filter button is clicked', async () => {
    render(PublishedView, { props: baseProps() });
    const musicBtn = screen.getByRole('button', { name: /^music$/i });
    await fireEvent.click(musicBtn);
    expect(screen.getByText('ambient-loops-vol2.zip')).toBeTruthy();
    expect(screen.queryByText('decentralized-identity-post.md')).toBeNull();
    expect(screen.queryByText('weekly-status-update.txt')).toBeNull();
  });

  it('shows all items when "All" filter is selected', async () => {
    render(PublishedView, { props: baseProps() });
    // First filter to music
    const musicBtn = screen.getByRole('button', { name: /^music$/i });
    await fireEvent.click(musicBtn);
    // Then click All
    const allBtn = screen.getByRole('button', { name: /all/i });
    await fireEvent.click(allBtn);
    expect(screen.getByText('decentralized-identity-post.md')).toBeTruthy();
    expect(screen.getByText('ambient-loops-vol2.zip')).toBeTruthy();
    expect(screen.getByText('weekly-status-update.txt')).toBeTruthy();
  });

  it('marks active category filter with aria-pressed', async () => {
    render(PublishedView, { props: baseProps() });
    const allBtn = screen.getByRole('button', { name: /all/i });
    expect(allBtn.getAttribute('aria-pressed')).toBe('true');

    const musicBtn = screen.getByRole('button', { name: /^music$/i });
    await fireEvent.click(musicBtn);
    expect(musicBtn.getAttribute('aria-pressed')).toBe('true');
    expect(allBtn.getAttribute('aria-pressed')).toBe('false');
  });

  // ── Search + category filter combined ────────────────────────────

  it('combines search and category filters', async () => {
    render(PublishedView, { props: baseProps() });
    // Filter to text category
    const textBtn = screen.getByRole('button', { name: /^text$/i });
    await fireEvent.click(textBtn);
    // Both text items should show
    expect(screen.getByText('decentralized-identity-post.md')).toBeTruthy();
    expect(screen.getByText('weekly-status-update.txt')).toBeTruthy();
    // Now search for "weekly"
    const searchInput = screen.getByRole('searchbox');
    await fireEvent.input(searchInput, { target: { value: 'weekly' } });
    expect(screen.queryByText('decentralized-identity-post.md')).toBeNull();
    expect(screen.getByText('weekly-status-update.txt')).toBeTruthy();
  });

  // ── No action buttons (read-only) ───────────────────────────────

  it('does not render any delete or edit buttons', () => {
    render(PublishedView, { props: baseProps() });
    expect(screen.queryByRole('button', { name: /delete/i })).toBeNull();
    expect(screen.queryByRole('button', { name: /edit/i })).toBeNull();
    expect(screen.queryByRole('button', { name: /burn/i })).toBeNull();
    expect(screen.queryByRole('button', { name: /remove/i })).toBeNull();
  });

  it('does not render checkboxes (no bulk actions)', () => {
    const { container } = render(PublishedView, { props: baseProps() });
    const checkboxes = container.querySelectorAll('input[type="checkbox"]');
    expect(checkboxes).toHaveLength(0);
  });

  // ── No quota bar ─────────────────────────────────────────────────

  it('does not render a quota bar', () => {
    render(PublishedView, { props: baseProps() });
    expect(screen.queryByText(/quota/i)).toBeNull();
    expect(screen.queryByRole('progressbar')).toBeNull();
  });

  // ── Accessibility ────────────────────────────────────────────────

  it('uses a landmark region for the view', () => {
    const { container } = render(PublishedView, { props: baseProps() });
    const region = container.querySelector('[aria-label="Published content catalog"]');
    expect(region).toBeTruthy();
  });

  it('search input has accessible label', () => {
    render(PublishedView, { props: baseProps() });
    expect(screen.getByLabelText(/search published/i)).toBeTruthy();
  });

  it('category filters are in a toolbar role', () => {
    render(PublishedView, { props: baseProps() });
    expect(screen.getByRole('toolbar', { name: /category filter/i })).toBeTruthy();
  });

  // ── Item count ───────────────────────────────────────────────────

  it('shows item count', () => {
    render(PublishedView, { props: baseProps() });
    expect(screen.getByText(/3 items/i)).toBeTruthy();
  });

  it('updates item count when filtered', async () => {
    render(PublishedView, { props: baseProps() });
    const musicBtn = screen.getByRole('button', { name: /^music$/i });
    await fireEvent.click(musicBtn);
    expect(screen.getByText(/1 item\b/i)).toBeTruthy();
  });

  it('uses singular "item" for count of 1', async () => {
    render(PublishedView, { props: baseProps() });
    const musicBtn = screen.getByRole('button', { name: /^music$/i });
    await fireEvent.click(musicBtn);
    // Should say "1 item" not "1 items"
    expect(screen.getByText(/1 item\b/i)).toBeTruthy();
    expect(screen.queryByText(/1 items/i)).toBeNull();
  });
});
