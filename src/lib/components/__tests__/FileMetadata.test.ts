import { describe, it, expect, afterEach } from 'vitest';
import { render } from '@testing-library/svelte';
import FileMetadata from '../FileMetadata.svelte';
import type { ContentDetail } from '../../types';
// ZEB-946: the "Stored" date honors the owner's date-order preference.
import {
  setTimeFormatSettings,
  _resetTimeFormatServiceForTest,
} from '../../time-format-service';
import { formatDateOnly } from '../../time-format';

const mockItem: ContentDetail = {
  sidecarId: 'mock-sidecar-meta-001',
  cid: '3f9a2c81d4e5f60718293a4b5c6d7e8f3f9a2c81d4e5f60718293a4b5c6d7e8f',
  name: 'test-file.txt',
  category: 'text',
  sensitivity: 'private',
  sizeBytes: 1024,
  storedAt: 1_700_000_000_000, // ms
  replicationTier: 'default',
  replicaCount: 3,
  pinned: false,
  licensed: false,
  parentCid: null,
  isFolder: false,
};

function storedValue(container: HTMLElement): string | null {
  const rows = Array.from(container.querySelectorAll('.metadata-row'));
  const stored = rows.find((r) => r.querySelector('.metadata-label')?.textContent === 'Stored');
  return stored?.querySelector('.metadata-value')?.textContent ?? null;
}

describe('FileMetadata stored-date honors the time-format preference (ZEB-946)', () => {
  afterEach(() => {
    _resetTimeFormatServiceForTest();
  });

  it('renders the stored date in the chosen order', () => {
    setTimeFormatSettings({ clock: 'system', dateOrder: 'ymd' });
    const { container } = render(FileMetadata, { props: { item: mockItem } });
    expect(storedValue(container)).toBe(formatDateOnly(mockItem.storedAt, { dateOrder: 'ymd' }));
  });

  it('follows the locale (unchanged) when the preference is system default', () => {
    const { container } = render(FileMetadata, { props: { item: mockItem } });
    // Default prefs → byte-identical to the prior raw toLocaleDateString().
    expect(storedValue(container)).toBe(new Date(mockItem.storedAt).toLocaleDateString());
  });
});
