import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import QuickFilters from '../QuickFilters.svelte';

describe('QuickFilters', () => {
  it('renders category filter section', () => {
    render(QuickFilters, { props: {} });
    expect(screen.getByText('Category')).toBeTruthy();
  });

  it('renders status filter section', () => {
    render(QuickFilters, { props: {} });
    expect(screen.getByText('Status')).toBeTruthy();
  });

  it('renders replication tier filter section', () => {
    render(QuickFilters, { props: {} });
    expect(screen.getByText('Replication Tier')).toBeTruthy();
  });

  it('renders category checkboxes for each content category', () => {
    render(QuickFilters, { props: {} });
    expect(screen.getByLabelText('Music')).toBeTruthy();
    expect(screen.getByLabelText('Videos')).toBeTruthy();
    expect(screen.getByLabelText('Documents')).toBeTruthy();
    expect(screen.getByLabelText('Images')).toBeTruthy();
    expect(screen.getByLabelText('Software')).toBeTruthy();
    expect(screen.getByLabelText('Dataset')).toBeTruthy();
    expect(screen.getByLabelText('Bundle')).toBeTruthy();
  });

  it('renders status filter buttons', () => {
    render(QuickFilters, { props: {} });
    expect(screen.getByRole('button', { name: 'Pinned by me' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Licensed' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Under-replicated' })).toBeTruthy();
  });

  it('renders tier filter buttons', () => {
    render(QuickFilters, { props: {} });
    expect(screen.getByRole('button', { name: 'Expendable' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Light' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Default' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'High' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Ultra' })).toBeTruthy();
  });

  it('fires onFilterChange when a category checkbox is toggled', async () => {
    const onFilterChange = vi.fn();
    render(QuickFilters, { props: { onFilterChange } });
    const musicCheckbox = screen.getByLabelText('Music');
    await fireEvent.click(musicCheckbox);
    expect(onFilterChange).toHaveBeenCalledOnce();
    const filters = onFilterChange.mock.calls[0][0];
    expect(filters.categories).toContain('music');
  });

  it('fires onFilterChange when a category is unchecked', async () => {
    const onFilterChange = vi.fn();
    render(QuickFilters, { props: { onFilterChange } });
    const musicCheckbox = screen.getByLabelText('Music');
    await fireEvent.click(musicCheckbox); // check
    await fireEvent.click(musicCheckbox); // uncheck
    expect(onFilterChange).toHaveBeenCalledTimes(2);
    const filters = onFilterChange.mock.calls[1][0];
    expect(filters.categories).not.toContain('music');
  });

  it('omits the fabrication-backed Stale toggle (ZEB-612 S3)', () => {
    render(QuickFilters, { props: { onFilterChange: vi.fn() } });
    expect(screen.queryByRole('button', { name: 'Stale' })).toBeNull();
  });

  it('fires onFilterChange when a status button is toggled', async () => {
    const onFilterChange = vi.fn();
    render(QuickFilters, { props: { onFilterChange } });
    const pinnedBtn = screen.getByRole('button', { name: 'Pinned by me' });
    await fireEvent.click(pinnedBtn);
    expect(onFilterChange).toHaveBeenCalled();
    const filters = onFilterChange.mock.calls[0][0];
    expect(filters.pinned).toBe(true);
  });

  it('toggles status button off on second click', async () => {
    const onFilterChange = vi.fn();
    render(QuickFilters, { props: { onFilterChange } });
    const pinnedBtn = screen.getByRole('button', { name: 'Pinned by me' });
    await fireEvent.click(pinnedBtn);
    await fireEvent.click(pinnedBtn);
    const filters = onFilterChange.mock.calls[1][0];
    expect(filters.pinned).toBe(false);
  });

  it('fires onFilterChange when a tier button is toggled', async () => {
    const onFilterChange = vi.fn();
    render(QuickFilters, { props: { onFilterChange } });
    const highBtn = screen.getByRole('button', { name: 'High' });
    await fireEvent.click(highBtn);
    expect(onFilterChange).toHaveBeenCalled();
    const filters = onFilterChange.mock.calls[0][0];
    expect(filters.tiers).toContain('high');
  });

  it('sections are collapsible with aria-expanded', async () => {
    render(QuickFilters, { props: {} });
    const categoryHeader = screen.getByRole('button', { name: /Category/ });
    expect(categoryHeader.getAttribute('aria-expanded')).toBe('true');
    await fireEvent.click(categoryHeader);
    expect(categoryHeader.getAttribute('aria-expanded')).toBe('false');
  });

  it('hides section content when collapsed', async () => {
    render(QuickFilters, { props: {} });
    const categoryHeader = screen.getByRole('button', { name: /Category/ });
    await fireEvent.click(categoryHeader);
    // Category checkboxes should be hidden
    expect(screen.queryByLabelText('Music')).toBeNull();
  });

  it('status buttons have aria-pressed attribute', () => {
    render(QuickFilters, { props: {} });
    const pinnedBtn = screen.getByRole('button', { name: 'Pinned by me' });
    expect(pinnedBtn.getAttribute('aria-pressed')).toBe('false');
  });

  it('sets aria-pressed to true when status button is active', async () => {
    render(QuickFilters, { props: {} });
    const pinnedBtn = screen.getByRole('button', { name: 'Pinned by me' });
    await fireEvent.click(pinnedBtn);
    expect(pinnedBtn.getAttribute('aria-pressed')).toBe('true');
  });

  it('renders as a section with appropriate aria-label', () => {
    const { container } = render(QuickFilters, { props: {} });
    const section = container.querySelector('section[aria-label="Quick filters"]');
    expect(section).toBeTruthy();
  });
});
