import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import SharedWithMeList from './SharedWithMeList.svelte';
import type { ReceivedFile } from '../types';

// receivedAt fixed far enough in the past that relativeTime's output is
// stable regardless of when the test runs (lands in the "…mo ago" bucket).
const file: ReceivedFile = {
  cid: 'abc', granterAddress: 'dead', granterDisplay: 'Alice',
  fileName: 'quarterly.pdf', fileSize: 4096, mime: 'application/pdf',
  receivedAt: Date.now() - 90 * 86_400_000,
};

describe('SharedWithMeList', () => {
  it('renders a neutral placeholder (NOT the empty message) while unresolved (null)', () => {
    render(SharedWithMeList, { files: null, onDownload: vi.fn() });
    expect(screen.queryByText(/nothing has been shared/i)).toBeNull();
  });

  it('renders the proven-empty message for []', () => {
    render(SharedWithMeList, { files: [], onDownload: vi.fn() });
    expect(screen.getByText(/nothing has been shared/i)).toBeTruthy();
  });

  it('renders a row per file with granter + size + received-time + download, and wires onDownload', async () => {
    const onDownload = vi.fn();
    render(SharedWithMeList, { files: [file], onDownload });
    expect(screen.getByText('quarterly.pdf')).toBeTruthy();
    expect(screen.getByText(/Alice/)).toBeTruthy();
    expect(screen.getByText(/4\.1 KB/)).toBeTruthy();
    // relativeTime buckets anything >= 30 days as "Xmo ago" — stable at ~90d.
    expect(screen.getByText(/mo ago/)).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: /download/i }));
    expect(onDownload).toHaveBeenCalledWith(file);
  });
});
