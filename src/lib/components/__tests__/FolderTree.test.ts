import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import FolderTree from '../FolderTree.svelte';
import type { ContentItem } from '../../types';

const day = 86_400_000;
const now = Date.now();

function makeFolder(overrides: Partial<ContentItem> = {}): ContentItem {
  return {
    sidecarId: 'mock-sidecar-folder',
    cid: 'cid-folder-root',
    name: 'Projects',
    category: 'bundle',
    sensitivity: 'private',
    sizeBytes: 0,
    storedAt: now - 90 * day,
    replicationTier: 'default',
    replicaCount: 3,
    pinned: false,
    licensed: false,
    parentCid: null,
    isFolder: true,
    ...overrides,
  };
}

function makeFile(overrides: Partial<ContentItem> = {}): ContentItem {
  return {
    sidecarId: 'mock-sidecar-file',
    cid: 'cid-file-1',
    name: 'notes.md',
    category: 'text',
    sensitivity: 'private',
    sizeBytes: 1024,
    storedAt: now - 30 * day,
    replicationTier: 'default',
    replicaCount: 3,
    pinned: false,
    licensed: false,
    parentCid: 'cid-folder-root',
    isFolder: false,
    ...overrides,
  };
}

const rootFolder = makeFolder();
const childFile = makeFile();
const nestedFolder = makeFolder({
  cid: 'cid-folder-nested',
  name: 'Subproject',
  parentCid: 'cid-folder-root',
  isFolder: true,
});
const rootFile = makeFile({
  cid: 'cid-root-file',
  name: 'readme.txt',
  parentCid: null,
});

const allItems: ContentItem[] = [rootFolder, childFile, nestedFolder, rootFile];

describe('FolderTree', () => {
  it('renders root folders at the top level', () => {
    render(FolderTree, { props: { items: allItems } });
    expect(screen.getByText('Projects')).toBeTruthy();
  });

  it('does not render non-folder root items', () => {
    render(FolderTree, { props: { items: allItems } });
    // Root-level files are not shown in the folder tree
    expect(screen.queryByText('readme.txt')).toBeNull();
  });

  it('shows folder expand/collapse button with aria-expanded', () => {
    render(FolderTree, { props: { items: allItems } });
    const expandBtn = screen.getByRole('button', { name: /Projects/ });
    expect(expandBtn.getAttribute('aria-expanded')).toBe('false');
  });

  it('expands folder on click and shows children', async () => {
    render(FolderTree, { props: { items: allItems } });
    const expandBtn = screen.getByRole('button', { name: /Projects/ });
    await fireEvent.click(expandBtn);

    expect(expandBtn.getAttribute('aria-expanded')).toBe('true');
    // Children should be visible: nested folder and child file
    expect(screen.getByText('Subproject')).toBeTruthy();
    expect(screen.getByText('notes.md')).toBeTruthy();
  });

  it('collapses folder on second click', async () => {
    render(FolderTree, { props: { items: allItems } });
    const expandBtn = screen.getByRole('button', { name: /Projects/ });
    await fireEvent.click(expandBtn);
    await fireEvent.click(expandBtn);

    expect(expandBtn.getAttribute('aria-expanded')).toBe('false');
    expect(screen.queryByText('Subproject')).toBeNull();
    expect(screen.queryByText('notes.md')).toBeNull();
  });

  it('fires onFolderSelect with cid when a folder is selected', async () => {
    const onFolderSelect = vi.fn();
    render(FolderTree, { props: { items: allItems, onFolderSelect } });
    const expandBtn = screen.getByRole('button', { name: /Projects/ });
    await fireEvent.click(expandBtn);
    expect(onFolderSelect).toHaveBeenCalledWith('cid-folder-root');
  });

  it('fires onFolderSelect with null when root is clicked', async () => {
    const onFolderSelect = vi.fn();
    render(FolderTree, { props: { items: allItems, onFolderSelect } });
    const rootBtn = screen.getByRole('button', { name: /All Files/ });
    await fireEvent.click(rootBtn);
    expect(onFolderSelect).toHaveBeenCalledWith(null);
  });

  it('renders as a navigation landmark', () => {
    render(FolderTree, { props: { items: allItems } });
    expect(screen.getByRole('navigation', { name: /Folder tree/ })).toBeTruthy();
  });

  it('uses tree role for the list', () => {
    render(FolderTree, { props: { items: allItems } });
    expect(screen.getByRole('tree')).toBeTruthy();
  });

  it('renders child files as treeitem role when folder is expanded', async () => {
    render(FolderTree, { props: { items: allItems } });
    const expandBtn = screen.getByRole('button', { name: /Projects/ });
    await fireEvent.click(expandBtn);
    const treeItems = screen.getAllByRole('treeitem');
    expect(treeItems.length).toBeGreaterThanOrEqual(3); // All Files + Projects (expanded children)
  });

  it('highlights selected folder', async () => {
    render(FolderTree, { props: { items: allItems, selectedCid: 'cid-folder-root' } });
    const expandBtn = screen.getByRole('button', { name: /Projects/ });
    expect(expandBtn.getAttribute('aria-current')).toBe('true');
  });

  it('activates folder button on Enter key', async () => {
    const onFolderSelect = vi.fn();
    render(FolderTree, { props: { items: allItems, onFolderSelect } });
    const expandBtn = screen.getByRole('button', { name: /Projects/ });
    await fireEvent.keyDown(expandBtn, { key: 'Enter' });
    expect(onFolderSelect).toHaveBeenCalledWith('cid-folder-root');
  });

  it('activates folder button on Space key', async () => {
    const onFolderSelect = vi.fn();
    render(FolderTree, { props: { items: allItems, onFolderSelect } });
    const expandBtn = screen.getByRole('button', { name: /Projects/ });
    await fireEvent.keyDown(expandBtn, { key: ' ' });
    expect(onFolderSelect).toHaveBeenCalledWith('cid-folder-root');
  });

  it('renders with empty items list', () => {
    render(FolderTree, { props: { items: [] } });
    // Should still show the "All Files" root
    expect(screen.getByRole('button', { name: /All Files/ })).toBeTruthy();
  });
});
