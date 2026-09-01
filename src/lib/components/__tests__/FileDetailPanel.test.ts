import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi, afterEach } from 'vitest';
import { tick } from 'svelte';
import FileDetailPanel from '../FileDetailPanel.svelte';
import type { ContentDetail } from '../../types';

// ZEB-612 S3: the panel renders only real data — full CID with a copy
// affordance, the "copies seen" replication box, and Used-by-vines.
// Mock-backed ShareList / StorageBuddyList / origin are gone (ZEB-669).
const mockDetail: ContentDetail = {
  sidecarId: 'mock-sidecar-detail-001',
  cid: '3f9a2c81d4e5f60718293a4b5c6d7e8f3f9a2c81d4e5f60718293a4b5c6d7e8f',
  name: 'test-file.txt',
  category: 'text',
  sensitivity: 'private',
  sizeBytes: 1024,
  storedAt: Date.now() - 86400000,
  replicationTier: 'default',
  replicaCount: 3,
  pinned: false,
  licensed: false,
  parentCid: null,
  isFolder: false,
};

function renderPanel(
  detailOverrides: Partial<ContentDetail> = {},
  extraProps: Record<string, unknown> = {},
) {
  return render(FileDetailPanel, {
    props: {
      detail: { ...mockDetail, ...detailOverrides },
      onTierChange: vi.fn(),
      onBurn: vi.fn(),
      onArchive: vi.fn(),
      onPin: vi.fn(),
      onUnpin: vi.fn(),
      onExport: vi.fn(),
      ...extraProps,
    },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('FileDetailPanel', () => {
  it('renders file name from detail', () => {
    renderPanel();
    expect(screen.getByText('test-file.txt')).toBeTruthy();
  });

  it('renders the full CID in mono with a Copy CID button', () => {
    renderPanel();
    const cidEl = screen.getByTestId('cid-full');
    expect(cidEl.textContent).toBe(mockDetail.cid);
    expect(screen.getByRole('button', { name: 'Copy CID' })).toBeTruthy();
  });

  it('Copy CID writes the cid to the clipboard and flips to ✓ Copied', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal('navigator', { ...navigator, clipboard: { writeText } });
    renderPanel();
    await fireEvent.click(screen.getByRole('button', { name: 'Copy CID' }));
    await tick();
    expect(writeText).toHaveBeenCalledWith(mockDetail.cid);
    expect(screen.getByText('✓ Copied')).toBeTruthy();
  });

  it('replication box: healthy copy above target', () => {
    renderPanel({ replicaCount: 5 });
    expect(screen.getByText('×5 · copies seen (this device + peers)')).toBeTruthy();
    expect(screen.getByText('Above the ×3 target for default.')).toBeTruthy();
  });

  it('replication box: at-risk copy below target', () => {
    renderPanel({ replicaCount: 1, replicationTier: 'high' });
    expect(screen.getByText('×1 · copies seen (this device + peers)')).toBeTruthy();
    expect(screen.getByText('Below the ×5 target for high.')).toBeTruthy();
  });

  it('replication box: exactly-at-target counts as meeting it', () => {
    renderPanel({ replicaCount: 3 });
    expect(screen.getByText('Above the ×3 target for default.')).toBeTruthy();
  });

  it('tier select still drives onTierChange (real IPC)', async () => {
    const onTierChange = vi.fn();
    renderPanel({}, { onTierChange });
    const select = screen.getByLabelText('Replication tier') as HTMLSelectElement;
    await fireEvent.change(select, { target: { value: 'ultra' } });
    expect(onTierChange).toHaveBeenCalledWith('ultra');
  });

  it('shows Used by N vines only when N > 0', async () => {
    const { unmount } = renderPanel({}, { usedByVines: 2 });
    expect(screen.getByText('Used by 2 vines')).toBeTruthy();
    unmount();
    renderPanel({}, { usedByVines: 0 });
    expect(screen.queryByText(/Used by/)).toBeNull();
  });

  it('uses singular copy for a single referencing vine', () => {
    renderPanel({}, { usedByVines: 1 });
    expect(screen.getByText('Used by 1 vine')).toBeTruthy();
  });

  it('mock surfaces are gone: no ShareList, no StorageBuddyList, no origin row, no staleness bar', () => {
    const { container } = renderPanel();
    expect(screen.queryByText(/Shared with/i)).toBeNull();
    expect(screen.queryByText(/Storage buddies/i)).toBeNull();
    expect(screen.queryByText(/Origin/)).toBeNull();
    expect(container.querySelector('.staleness-bar-track')).toBeNull();
  });

  it('renders action buttons', () => {
    renderPanel();
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
    renderPanel({}, { onBurn });
    await fireEvent.click(screen.getByLabelText('Burn'));
    // Burn now requires confirmation
    expect(onBurn).not.toHaveBeenCalled();
    // Click the confirm button inside the dialog
    const dialog = screen.getByRole('dialog');
    const confirmBtn = dialog.querySelector('.confirm-btn') as HTMLElement;
    await fireEvent.click(confirmBtn);
    expect(onBurn).toHaveBeenCalledOnce();
  });

  // ── ZEB-669 S3: "Back up with buddies" toggle ─────────────────────────

  it('backup section renders only when onSetBackup is provided AND the row has a sidecar', () => {
    const { unmount } = renderPanel({}, { onSetBackup: vi.fn() });
    expect(screen.getByTestId('backup-section')).toBeTruthy();
    unmount();
    // Manifest-derived rows (empty sidecarId) have no sidecar to flag.
    const { unmount: u2 } = renderPanel({ sidecarId: '' }, { onSetBackup: vi.fn() });
    expect(screen.queryByTestId('backup-section')).toBeNull();
    u2();
    // No handler (e.g. bare construction) → section hidden.
    renderPanel();
    expect(screen.queryByTestId('backup-section')).toBeNull();
  });

  it('disables the toggle with reason copy for non-public files', () => {
    renderPanel({ sensitivity: 'private' }, { onSetBackup: vi.fn() });
    const box = screen.getByTestId('backup-checkbox') as HTMLInputElement;
    expect(box.disabled).toBe(true);
    expect(screen.getByText('Only public files can be backed up by buddies.')).toBeTruthy();
  });

  it('enables the toggle for public files and calls onSetBackup', async () => {
    const onSetBackup = vi.fn().mockResolvedValue(undefined);
    renderPanel({ sensitivity: 'public' }, { onSetBackup });
    const box = screen.getByTestId('backup-checkbox') as HTMLInputElement;
    expect(box.disabled).toBe(false);
    await fireEvent.click(box);
    expect(onSetBackup).toHaveBeenCalledWith(mockDetail.sidecarId, true);
  });

  it('clearing stays allowed when an already-flagged file is no longer eligible', async () => {
    const onSetBackup = vi.fn().mockResolvedValue(undefined);
    renderPanel({ sensitivity: 'private', backup: true }, { onSetBackup });
    const box = screen.getByTestId('backup-checkbox') as HTMLInputElement;
    expect(box.checked).toBe(true);
    expect(box.disabled).toBe(false);
    expect(screen.queryByText('Only public files can be backed up by buddies.')).toBeNull();
    // The clearing flow itself must reach the backend (CodeRabbit PR #450).
    await fireEvent.click(box);
    expect(onSetBackup).toHaveBeenCalledWith(mockDetail.sidecarId, false);
  });

  it('renders the ineligible rejection inline and reverts the checkbox', async () => {
    const onSetBackup = vi
      .fn()
      .mockRejectedValue(new Error('ineligible: content class is not public durable'));
    renderPanel({ sensitivity: 'public' }, { onSetBackup });
    const box = screen.getByTestId('backup-checkbox') as HTMLInputElement;
    await fireEvent.click(box);
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('Not eligible: content class is not public durable');
    expect(box.checked).toBe(false);
  });

  // ── ZEB-669 S4: "From" (origin) row ───────────────────────────────────

  it('renders no From row when origin is absent (legacy/manifest rows)', () => {
    renderPanel();
    expect(screen.queryByTestId('from-row')).toBeNull();
  });

  it('renders "Added by you" for self-ingested entries', () => {
    renderPanel({ origin: { kind: 'selfIngest', introducer: null } });
    expect(screen.getByTestId('from-row').textContent).toContain('Added by you');
  });
});
