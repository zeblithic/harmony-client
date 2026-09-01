import { render, screen } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import FileBrowser from '../FileBrowser.svelte';
import { FileManagerService } from '../../file-manager-service';

function makeProps(overrides: Record<string, unknown> = {}) {
  return {
    service: new FileManagerService(),
    currentFolderCid: null as string | null,
    selectedCid: null as string | null,
    viewMode: 'list' as const,
    section: 'private' as const,
    searchQuery: '',
    onItemClick: vi.fn(),
    onNavigateFolder: vi.fn(),
    onViewModeChange: vi.fn(),
    onSearchChange: vi.fn(),
    onSectionChange: vi.fn(),
    onUploadClick: vi.fn(),
    serviceVersion: 0,
    ...overrides,
  };
}

describe('FileBrowser', () => {
  it('renders file items from service', () => {
    render(FileBrowser, { props: makeProps() });
    // Root-level items from mock data: Projects folder, favorite-track.flac, etc.
    expect(screen.getByText('Projects')).toBeTruthy();
    expect(screen.getByText('favorite-track.flac')).toBeTruthy();
    expect(screen.getByText('distributed-systems-lecture.mp4')).toBeTruthy();
  });

  it('filters items by search query', () => {
    render(FileBrowser, { props: makeProps({ searchQuery: 'favorite' }) });
    expect(screen.getByText('favorite-track.flac')).toBeTruthy();
    // Other root items should be filtered out
    expect(screen.queryByText('Projects')).toBeNull();
    expect(screen.queryByText('distributed-systems-lecture.mp4')).toBeNull();
  });

  it('shows QuotaBar with correct usage', () => {
    const { container } = render(FileBrowser, { props: makeProps() });
    // QuotaBar renders an element with storage info in aria-label
    const quotaEl = container.querySelector('.quota-bar');
    expect(quotaEl?.getAttribute('aria-label')).toContain('Storage:');
  });

  it('shows folders before files', () => {
    const { container } = render(FileBrowser, { props: makeProps() });
    // In the file list, get all file-row elements
    const rows = container.querySelectorAll('.file-row');
    // First row should be the "Projects" folder
    const firstRowName = rows[0]?.querySelector('.file-row-name');
    expect(firstRowName?.textContent).toBe('Projects');
  });

  it('renders breadcrumbs with root at minimum', () => {
    render(FileBrowser, { props: makeProps() });
    expect(screen.getByText('My Content')).toBeTruthy();
  });

  it('renders breadcrumbs with folder name when navigated', () => {
    render(FileBrowser, { props: makeProps({ currentFolderCid: 'cid-folder-projects' }) });
    expect(screen.getByText('My Content')).toBeTruthy();
    // The breadcrumb should show "Projects" as the folder name
    // Note: "Projects" also appears as a folder row in the parent; but at this cid,
    // we're inside Projects showing its children, so "Projects" is in the breadcrumb
    const buttons = screen.getAllByText('Projects');
    // At least one should be a breadcrumb segment button
    expect(buttons.length).toBeGreaterThanOrEqual(1);
  });
});
