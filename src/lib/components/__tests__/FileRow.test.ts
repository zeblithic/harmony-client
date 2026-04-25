import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import FileRow from '../FileRow.svelte';
import type { ContentItem } from '../../types';

const baseItem: ContentItem = {
  sidecarId: 'mock-sidecar-row-001',
  cid: 'cid-test-001',
  name: 'test-document.md',
  category: 'text',
  sensitivity: 'private',
  sizeBytes: 45_000,
  storedAt: Date.now() - 60 * 86_400_000,
  lastAccessed: Date.now() - 2 * 86_400_000,
  accessCount: 18,
  stalenessScore: 0.15,
  replicationTier: 'default',
  replicaCount: 3,
  pinned: false,
  licensed: false,
  parentCid: null,
  isFolder: false,
};

describe('FileRow', () => {
  it('renders file name and category icon', () => {
    render(FileRow, { props: { item: baseItem } });
    expect(screen.getByText('test-document.md')).toBeTruthy();
    // Text category icon is 📄
    expect(screen.getByText('\uD83D\uDCC4')).toBeTruthy();
  });

  it('shows staleness badge for stale content', () => {
    const staleItem: ContentItem = { ...baseItem, stalenessScore: 0.7 };
    const { container } = render(FileRow, { props: { item: staleItem } });
    expect(container.querySelector('.staleness-dot')).toBeTruthy();
  });

  it('calls onClick with ContentItem when clicked', async () => {
    const onClick = vi.fn();
    render(FileRow, { props: { item: baseItem, onClick } });
    const row = screen.getByRole('row');
    await fireEvent.click(row);
    expect(onClick).toHaveBeenCalledWith(baseItem);
  });

  it('applies selected class when selected', () => {
    const { container } = render(FileRow, { props: { item: baseItem, selected: true } });
    expect(container.querySelector('.file-row.selected')).toBeTruthy();
  });

  it('shows replication info', () => {
    render(FileRow, { props: { item: baseItem } });
    // default tier target = 3, replicaCount = 3
    expect(screen.getByText('3/3')).toBeTruthy();
  });

  it('shows lock icon for private sensitivity', () => {
    render(FileRow, { props: { item: baseItem } });
    expect(screen.getByText('\uD83D\uDD12')).toBeTruthy();
  });

  it('shows globe icon for public sensitivity', () => {
    const publicItem: ContentItem = { ...baseItem, sensitivity: 'public' };
    render(FileRow, { props: { item: publicItem } });
    expect(screen.getByText('\uD83C\uDF10')).toBeTruthy();
  });

  it('shows formatted size', () => {
    render(FileRow, { props: { item: baseItem } });
    expect(screen.getByText('45.0 KB')).toBeTruthy();
  });
});
