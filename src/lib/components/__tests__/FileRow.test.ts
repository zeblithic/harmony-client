import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import FileRow from '../FileRow.svelte';
import type { ContentItem } from '../../types';

// ZEB-612 S3: rows carry only real data — CID chip + observed-replica
// chip; the fabricated staleness/last-accessed cells are gone.
const baseItem: ContentItem = {
  sidecarId: 'mock-sidecar-row-001',
  cid: 'cid-test-001',
  name: 'test-document.md',
  category: 'text',
  sensitivity: 'private',
  sizeBytes: 45_000,
  storedAt: Date.now() - 60 * 86_400_000,
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
    expect(screen.getByText('📄')).toBeTruthy();
  });

  it('renders a mono CID chip with the full cid as tooltip', () => {
    const hexItem: ContentItem = {
      ...baseItem,
      cid: '3f9a2c81d4e5f60718293a4b5c6d7e8f3f9a2c81d4e5f60718293a4b5c6d7e8f',
    };
    render(FileRow, { props: { item: hexItem } });
    const chip = screen.getByTestId('cid-chip');
    expect(chip.textContent).toBe('cid:3f9a2c…7e8f');
    expect(chip.getAttribute('title')).toBe(hexItem.cid);
  });

  it('omits the fabricated staleness and last-accessed cells', () => {
    const { container } = render(FileRow, { props: { item: baseItem } });
    expect(container.querySelector('.staleness-dot')).toBeNull();
    expect(container.querySelector('.file-row-accessed')).toBeNull();
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

  it('shows ×N healthy when copies seen meet the tier target', () => {
    // default tier target = 3, replicaCount = 3
    const { container } = render(FileRow, { props: { item: baseItem } });
    expect(screen.getByText('×3 healthy')).toBeTruthy();
    expect(container.querySelector('.replication-chip.healthy')).toBeTruthy();
  });

  it('shows ×N at risk when copies seen fall below the tier target', () => {
    const atRisk: ContentItem = { ...baseItem, replicaCount: 1 };
    const { container } = render(FileRow, { props: { item: atRisk } });
    expect(screen.getByText('×1 at risk')).toBeTruthy();
    expect(container.querySelector('.replication-chip.at-risk')).toBeTruthy();
  });

  it('shows no replication chip for folders (no tier semantics)', () => {
    const folder: ContentItem = { ...baseItem, isFolder: true };
    const { container } = render(FileRow, { props: { item: folder } });
    expect(container.querySelector('.replication-chip')).toBeNull();
  });

  it('shows lock icon for private sensitivity', () => {
    render(FileRow, { props: { item: baseItem } });
    expect(screen.getByText('🔒')).toBeTruthy();
  });

  it('shows globe icon for public sensitivity', () => {
    const publicItem: ContentItem = { ...baseItem, sensitivity: 'public' };
    render(FileRow, { props: { item: publicItem } });
    expect(screen.getByText('🌐')).toBeTruthy();
  });

  it('shows formatted size', () => {
    render(FileRow, { props: { item: baseItem } });
    expect(screen.getByText('45.0 KB')).toBeTruthy();
  });
});
