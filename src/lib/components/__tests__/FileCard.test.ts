import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import FileCard from '../FileCard.svelte';
import type { ContentItem } from '../../types';

const baseItem: ContentItem = {
  sidecarId: 'mock-sidecar-card-001',
  cid: 'cid-card-001',
  name: 'photo.jpg',
  category: 'image',
  sensitivity: 'public',
  sizeBytes: 8_500_000,
  storedAt: Date.now() - 30 * 86_400_000,
  lastAccessed: Date.now() - 5 * 86_400_000,
  accessCount: 10,
  stalenessScore: 0.15,
  replicationTier: 'default',
  replicaCount: 3,
  pinned: false,
  licensed: false,
  parentCid: null,
  isFolder: false,
};

describe('FileCard', () => {
  it('renders file name', () => {
    render(FileCard, { props: { item: baseItem } });
    expect(screen.getByText('photo.jpg')).toBeTruthy();
  });

  it('shows staleness badge overlay for stale content', () => {
    const staleItem: ContentItem = { ...baseItem, stalenessScore: 0.7 };
    const { container } = render(FileCard, { props: { item: staleItem } });
    expect(container.querySelector('.staleness-dot')).toBeTruthy();
  });

  it('calls onClick with CID when clicked', async () => {
    const onClick = vi.fn();
    render(FileCard, { props: { item: baseItem, onClick } });
    const card = screen.getByRole('button');
    await fireEvent.click(card);
    expect(onClick).toHaveBeenCalledWith('cid-card-001');
  });

  it('shows folder icon for folders', () => {
    const folder: ContentItem = { ...baseItem, isFolder: true, category: 'bundle' };
    render(FileCard, { props: { item: folder } });
    // bundle category icon is 📁
    expect(screen.getByText('\uD83D\uDCC1')).toBeTruthy();
  });

  it('applies selected class when selected', () => {
    const { container } = render(FileCard, { props: { item: baseItem, selected: true } });
    expect(container.querySelector('.file-card.selected')).toBeTruthy();
  });

  it('shows sensitivity icon', () => {
    render(FileCard, { props: { item: baseItem } });
    // public = 🌐
    expect(screen.getByText('\uD83C\uDF10')).toBeTruthy();
  });

  it('shows formatted size', () => {
    render(FileCard, { props: { item: baseItem } });
    expect(screen.getByText('8.5 MB')).toBeTruthy();
  });
});
