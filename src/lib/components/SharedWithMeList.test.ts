import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import SharedWithMeList from './SharedWithMeList.svelte';
import type { ReceivedFile } from '../types';

const file: ReceivedFile = {
  cid: 'abc', granterAddress: 'dead', granterDisplay: 'Alice',
  fileName: 'quarterly.pdf', fileSize: 4096, mime: 'application/pdf', receivedAt: 1,
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

  it('renders a row per file with granter + download, and wires onDownload', async () => {
    const onDownload = vi.fn();
    render(SharedWithMeList, { files: [file], onDownload });
    expect(screen.getByText('quarterly.pdf')).toBeTruthy();
    expect(screen.getByText(/Alice/)).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: /download/i }));
    expect(onDownload).toHaveBeenCalledWith(file);
  });
});
