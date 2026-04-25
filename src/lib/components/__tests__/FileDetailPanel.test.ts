import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import FileDetailPanel from '../FileDetailPanel.svelte';
import type { ContentDetail } from '../../types';

const mockDetail: ContentDetail = {
  sidecarId: 'mock-sidecar-detail-001',
  cid: 'test-cid',
  name: 'test-file.txt',
  category: 'text',
  sensitivity: 'private',
  sizeBytes: 1024,
  storedAt: Date.now() - 86400000,
  lastAccessed: Date.now() - 3600000,
  accessCount: 5,
  stalenessScore: 0.4,
  replicationTier: 'default',
  replicaCount: 3,
  pinned: false,
  licensed: false,
  parentCid: null,
  isFolder: false,
  sharedWith: [{ address: 'peer1', displayName: 'Alice' }],
  storageBuddies: [{ address: 'peer2', displayName: 'Bob' }],
  origin: 'self-created',
};

function renderPanel(detailOverrides: Partial<ContentDetail> = {}) {
  return render(FileDetailPanel, {
    props: {
      detail: { ...mockDetail, ...detailOverrides },
      onTierChange: vi.fn(),
      onPublish: vi.fn(),
      onRelease: vi.fn(),
      onBurn: vi.fn(),
      onArchive: vi.fn(),
      onPin: vi.fn(),
      onUnpin: vi.fn(),
      onExport: vi.fn(),
    },
  });
}

describe('FileDetailPanel', () => {
  it('renders file name from detail', () => {
    renderPanel();
    expect(screen.getByText('test-file.txt')).toBeTruthy();
  });

  it('shows replication status with correct counts', () => {
    renderPanel();
    // default tier target is 3, replicaCount is 3
    expect(screen.getByText(/3 of 3/)).toBeTruthy();
  });

  it('renders action buttons', () => {
    renderPanel();
    expect(screen.getByLabelText('Publish (permanent)')).toBeTruthy();
    expect(screen.getByLabelText('Release (ephemeral)')).toBeTruthy();
    expect(screen.getByLabelText('Burn')).toBeTruthy();
    expect(screen.getByLabelText('Archive')).toBeTruthy();
    expect(screen.getByLabelText('Export')).toBeTruthy();
  });

  it('renders as an aside with file details aria label', () => {
    const { container } = renderPanel();
    const aside = container.querySelector('aside.file-detail-panel');
    expect(aside).toBeTruthy();
    expect(aside!.getAttribute('aria-label')).toBe('File details');
  });

  it('shows sensitivity badge', () => {
    renderPanel();
    expect(screen.getByText(/Private/)).toBeTruthy();
  });

  it('shows staleness score', () => {
    renderPanel();
    expect(screen.getByText(/0\.40/)).toBeTruthy();
  });

  it('shows Pin button when item is not pinned', () => {
    renderPanel({ pinned: false });
    expect(screen.getByLabelText('Pin')).toBeTruthy();
  });

  it('shows Unpin button when item is pinned', () => {
    renderPanel({ pinned: true });
    expect(screen.getByLabelText('Unpin')).toBeTruthy();
  });

  it('fires burn callback after confirmation dialog', async () => {
    const onBurn = vi.fn();
    render(FileDetailPanel, {
      props: {
        detail: mockDetail,
        onTierChange: vi.fn(),
        onPublish: vi.fn(),
        onRelease: vi.fn(),
        onBurn,
        onArchive: vi.fn(),
        onPin: vi.fn(),
        onUnpin: vi.fn(),
        onExport: vi.fn(),
      },
    });
    await fireEvent.click(screen.getByLabelText('Burn'));
    // Burn now requires confirmation
    expect(onBurn).not.toHaveBeenCalled();
    // Click the confirm button inside the dialog
    const dialog = screen.getByRole('dialog');
    const confirmBtn = dialog.querySelector('.confirm-btn') as HTMLElement;
    await fireEvent.click(confirmBtn);
    expect(onBurn).toHaveBeenCalledOnce();
  });
});
