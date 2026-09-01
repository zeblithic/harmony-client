/**
 * ZEB-162 — File Manager drag-drop coordination.
 *
 * Renders the real FileBrowser with a real FileManagerService backed by a
 * mocked Tauri adapter. Simulates dragstart on a leaf row, then drop on:
 *   - a folder row (Case A/B/C dispatch)
 *   - the root-sentinel breadcrumb (Case D)
 *   - a nested breadcrumb (Case A move-out)
 *   - the file list's empty area (no-op)
 *
 * jsdom does not auto-populate `event.dataTransfer` on synthetic
 * DragEvents, so we build a minimal DataTransfer-like shim and feed it
 * via `fireEvent.dragStart(elem, { dataTransfer })`. The shim's
 * setData / getData / types behaviour matches the browser closely
 * enough for the handlers under test.
 */
import { render, fireEvent, cleanup, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, afterEach } from 'vitest';
import FileBrowser from '../FileBrowser.svelte';
import { FileManagerService } from '../../file-manager-service';
import { createMockAdapter } from '../../test-utils';

afterEach(cleanup);

/** Minimal DataTransfer shim: only the surface our handlers touch. */
function makeDataTransfer(): DataTransfer {
  const store = new Map<string, string>();
  const dt = {
    effectAllowed: 'none',
    dropEffect: 'none',
    setData(type: string, value: string) {
      store.set(type, value);
    },
    getData(type: string) {
      return store.get(type) ?? '';
    },
    get types() {
      return Array.from(store.keys());
    },
    clearData() {
      store.clear();
    },
    setDragImage() {
      /* noop */
    },
  } as unknown as DataTransfer;
  return dt;
}

function renderBrowser(
  service: FileManagerService,
  overrides: Record<string, unknown> = {},
) {
  const callbacks = {
    onItemClick: vi.fn(),
    onNavigateFolder: vi.fn(),
    onViewModeChange: vi.fn(),
    onSearchChange: vi.fn(),
    onSectionChange: vi.fn(),
    onUploadClick: vi.fn(),
  };
  const result = render(FileBrowser, {
    props: {
      service,
      currentFolderCid: null as string | null,
      selectedCid: null as string | null,
      viewMode: 'list' as const,
      section: 'private' as const,
      searchQuery: '',
      serviceVersion: 0,
      ...callbacks,
      ...overrides,
    },
  });
  return { ...result, callbacks };
}

describe('FileBrowser drag-drop (ZEB-162)', () => {
  // ── Folder-row drop ──────────────────────────────────────────────

  it('drags a root-level leaf onto a root-level folder row → Case C move', async () => {
    const service = new FileManagerService();
    const { adapter } = createMockAdapter();
    // list_content (the post-move refresh) returns [] so we don't
    // disturb the mock state mid-test.
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'list_content') return Promise.resolve([]);
      if (cmd === 'move_content') {
        return Promise.resolve({
          srcNewCid: null,
          dstSidecarId: 'mock-sidecar-1',
          dstNewCid: 'cid-folder-projects-new',
        });
      }
      return Promise.resolve(undefined);
    });
    await service.connectAdapter(adapter);
    // connectAdapter wiped privateContent (mock adapter list_content
    // returned []), so re-seed with mock data so the FileBrowser has
    // rows to drag and drop on. Using a fresh service that hasn't been
    // connected would skip the adapter-required moveContent path.
    // Easier: re-install the mock data directly.
    (service as unknown as { privateContent: unknown[] }).privateContent = (
      await import('../../mock-file-data')
    ).mockPrivateContent.slice();

    const { container } = renderBrowser(service);

    const sourceRow = container.querySelector(
      'button[aria-label="favorite-track.flac"]',
    ) as HTMLElement;
    const folderRow = container.querySelector(
      'button[aria-label="Projects"]',
    ) as HTMLElement;
    expect(sourceRow).toBeTruthy();
    expect(folderRow).toBeTruthy();

    const dt = makeDataTransfer();
    await fireEvent.dragStart(sourceRow, { dataTransfer: dt });
    // After dragStart, the payload should be in dataTransfer.
    const raw = dt.getData('application/x-harmony-content');
    expect(raw).not.toBe('');
    const payload = JSON.parse(raw);
    expect(payload).toMatchObject({
      srcSidecarId: 'mock-sidecar-4',
      srcChildCid: 'cid-song-favorite',
      srcChildKind: 'leaf',
    });
    // Root-level drag → srcPath is [child.cid] (Case C source shape).
    expect(payload.srcPath).toEqual(['cid-song-favorite']);

    await fireEvent.dragOver(folderRow, { dataTransfer: dt });
    await fireEvent.drop(folderRow, { dataTransfer: dt });

    // The service issued the expected move_content invocation.
    expect(adapter.invoke).toHaveBeenCalledWith(
      'move_content',
      expect.objectContaining({
        srcSidecarId: 'mock-sidecar-4',
        srcChildCid: 'cid-song-favorite',
        srcPath: ['cid-song-favorite'],
        dstSidecarId: 'mock-sidecar-1',
        dstPath: ['cid-folder-projects'],
        newName: null,
      }),
    );
  });

  // ── Breadcrumb root-sentinel drop (Case D) ────────────────────────

  it('drags a nested leaf onto the root breadcrumb → Case D move-to-root', async () => {
    const service = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'list_content') return Promise.resolve([]);
      if (cmd === 'move_content') {
        return Promise.resolve({
          srcNewCid: 'cid-folder-projects-new',
          dstSidecarId: 'mock-sidecar-newly-minted',
          dstNewCid: 'cid-design-doc',
        });
      }
      return Promise.resolve(undefined);
    });
    await service.connectAdapter(adapter);
    (service as unknown as { privateContent: unknown[] }).privateContent = (
      await import('../../mock-file-data')
    ).mockPrivateContent.slice();

    const { container } = renderBrowser(service, {
      currentFolderCid: 'cid-folder-projects',
    });

    // Now we're inside Projects. Drag mesh-design.md onto the "My
    // Content" breadcrumb segment (segIdx = 0, root sentinel).
    const sourceRow = container.querySelector(
      'button[aria-label="mesh-design.md"]',
    ) as HTMLElement;
    expect(sourceRow).toBeTruthy();
    // Breadcrumb segIdx 0 is the root sentinel "My Content".
    const rootSegment = Array.from(
      container.querySelectorAll('button.breadcrumb-segment'),
    ).find((b) => b.textContent === 'My Content') as HTMLElement;
    expect(rootSegment).toBeTruthy();

    const dt = makeDataTransfer();
    await fireEvent.dragStart(sourceRow, { dataTransfer: dt });
    const payload = JSON.parse(dt.getData('application/x-harmony-content'));
    // Inside Projects, srcPath = navStack chain = [cid-folder-projects].
    expect(payload).toMatchObject({
      srcSidecarId: 'mock-sidecar-1',
      srcChildCid: 'cid-design-doc',
      srcPath: ['cid-folder-projects'],
    });

    await fireEvent.dragOver(rootSegment, { dataTransfer: dt });
    await fireEvent.drop(rootSegment, { dataTransfer: dt });

    expect(adapter.invoke).toHaveBeenCalledWith(
      'move_content',
      expect.objectContaining({
        srcSidecarId: 'mock-sidecar-1',
        srcChildCid: 'cid-design-doc',
        srcPath: ['cid-folder-projects'],
        dstSidecarId: null,
        dstPath: [],
        newName: null,
      }),
    );
  });

  // ── Nested breadcrumb drop ────────────────────────────────────────

  it('drags a leaf onto a nested breadcrumb → same-top-level move within ancestor chain', async () => {
    // Build a synthetic two-level nesting: root → Projects
    // (mock-sidecar-1, cid-folder-projects) → SubFolder (cid-faux-sub) →
    // sub-leaf.txt. The test drags sub-leaf.txt onto the "Projects"
    // breadcrumb segment from inside SubFolder.
    //
    // The mock adapter's list_content must return the children of the
    // current folder; the FileBrowser's listFolderContents-driven
    // folderItems takes precedence over service.getContents(parentCid)
    // once a fetch has resolved.
    const subFolderRow = {
      sidecarId: '',
      cid: 'cid-faux-sub',
      name: 'SubFolder',
      sizeBytes: 0,
      storedAt: Date.now(),
      sensitivity: 'private' as const,
      replicationTier: 'default' as const,
      pinned: false,
      licensed: false,
      archived: false,
      kind: 'folder' as const,
    };
    const subLeafRow = {
      sidecarId: '',
      cid: 'cid-sub-leaf',
      name: 'sub-leaf.txt',
      sizeBytes: 100,
      storedAt: Date.now(),
      sensitivity: 'private' as const,
      replicationTier: 'default' as const,
      pinned: false,
      licensed: false,
      archived: false,
      kind: 'leaf' as const,
    };

    const service = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi.fn().mockImplementation((cmd: string, args?: unknown) => {
      if (cmd === 'list_content') {
        const a = args as { folderCid: string | null };
        if (a?.folderCid === 'cid-folder-projects') {
          return Promise.resolve([subFolderRow]);
        }
        if (a?.folderCid === 'cid-faux-sub') {
          return Promise.resolve([subLeafRow]);
        }
        return Promise.resolve([]);
      }
      if (cmd === 'move_content') {
        return Promise.resolve({
          srcNewCid: 'rekey-1',
          dstSidecarId: 'mock-sidecar-1',
          dstNewCid: 'rekey-1',
        });
      }
      return Promise.resolve(undefined);
    });
    await service.connectAdapter(adapter);
    // Seed root with the real mock data so Projects appears at root
    // (its sidecarId is mock-sidecar-1, the top-level for the cascade).
    (service as unknown as { privateContent: unknown[] }).privateContent = (
      await import('../../mock-file-data')
    ).mockPrivateContent.slice();

    // Navigate via two render passes: first into Projects (so navStack[0]
    // gets stamped with sidecarId from the root listing), then into the
    // faux subfolder (manifest-derived, no sidecar of its own).
    const { rerender, container } = renderBrowser(service);

    // Click Projects in the root view to forward-navigate.
    const projectsRow = container.querySelector(
      'button[aria-label="Projects"]',
    ) as HTMLElement;
    await fireEvent.click(projectsRow);
    // The onNavigateFolder mock just spies; we drive currentFolderCid
    // ourselves via rerender so the $effect picks up the click stash.
    await rerender({
      service,
      currentFolderCid: 'cid-folder-projects',
      selectedCid: null,
      viewMode: 'list',
      section: 'private',
      searchQuery: '',
      serviceVersion: 0,
      onItemClick: vi.fn(),
      onNavigateFolder: vi.fn(),
      onViewModeChange: vi.fn(),
      onSearchChange: vi.fn(),
      onSectionChange: vi.fn(),
      onUploadClick: vi.fn(),
    });

    // Now inside Projects. Wait for the listFolderContents fetch to
    // resolve and the SubFolder row to render, then click it.
    await waitFor(() => {
      const r = container.querySelector('button[aria-label="SubFolder"]');
      if (!r) throw new Error('SubFolder row not yet rendered');
    });
    const subRow = container.querySelector(
      'button[aria-label="SubFolder"]',
    ) as HTMLElement;
    await fireEvent.click(subRow);
    await rerender({
      service,
      currentFolderCid: 'cid-faux-sub',
      selectedCid: null,
      viewMode: 'list',
      section: 'private',
      searchQuery: '',
      serviceVersion: 0,
      onItemClick: vi.fn(),
      onNavigateFolder: vi.fn(),
      onViewModeChange: vi.fn(),
      onSearchChange: vi.fn(),
      onSectionChange: vi.fn(),
      onUploadClick: vi.fn(),
    });

    // Now we're inside SubFolder. Wait for the leaf row to render.
    await waitFor(() => {
      const r = container.querySelector('button[aria-label="sub-leaf.txt"]');
      if (!r) throw new Error('sub-leaf.txt row not yet rendered');
    });
    const leafRow = container.querySelector(
      'button[aria-label="sub-leaf.txt"]',
    ) as HTMLElement;
    const projectsSegment = Array.from(
      container.querySelectorAll('button.breadcrumb-segment'),
    ).find((b) => b.textContent === 'Projects') as HTMLElement;
    expect(projectsSegment).toBeTruthy();

    const dt = makeDataTransfer();
    await fireEvent.dragStart(leafRow, { dataTransfer: dt });
    const payload = JSON.parse(dt.getData('application/x-harmony-content'));
    expect(payload.srcSidecarId).toBe('mock-sidecar-1');
    // navStack inside SubFolder: [Projects, SubFolder].
    expect(payload.srcPath).toEqual(['cid-folder-projects', 'cid-faux-sub']);
    expect(payload.srcChildCid).toBe('cid-sub-leaf');

    await fireEvent.dragOver(projectsSegment, { dataTransfer: dt });
    await fireEvent.drop(projectsSegment, { dataTransfer: dt });

    expect(adapter.invoke).toHaveBeenCalledWith(
      'move_content',
      expect.objectContaining({
        srcSidecarId: 'mock-sidecar-1',
        srcPath: ['cid-folder-projects', 'cid-faux-sub'],
        srcChildCid: 'cid-sub-leaf',
        dstSidecarId: 'mock-sidecar-1',
        // Projects breadcrumb (segIdx 1, navIdx 0) → dstPath = [navStack[0].cid].
        dstPath: ['cid-folder-projects'],
        newName: null,
      }),
    );
  });

  // ── Empty-area no-op ─────────────────────────────────────────────

  it('drop on the empty file-list area is a no-op (no move_content invocation)', async () => {
    const service = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'list_content') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    await service.connectAdapter(adapter);
    (service as unknown as { privateContent: unknown[] }).privateContent = (
      await import('../../mock-file-data')
    ).mockPrivateContent.slice();

    const { container } = renderBrowser(service);

    const sourceRow = container.querySelector(
      'button[aria-label="favorite-track.flac"]',
    ) as HTMLElement;
    const fileList = container.querySelector('.file-list') as HTMLElement;
    expect(sourceRow).toBeTruthy();
    expect(fileList).toBeTruthy();

    const dt = makeDataTransfer();
    await fireEvent.dragStart(sourceRow, { dataTransfer: dt });
    // Dispatch dragover and drop on the file-list container itself
    // (not on a row). No ondragover/ondrop handlers attached → drop is
    // ignored by the browser as well as by us. Even if event fires, the
    // service should not see move_content.
    await fireEvent.dragOver(fileList, { dataTransfer: dt });
    await fireEvent.drop(fileList, { dataTransfer: dt });

    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'move_content',
      expect.any(Object),
    );
  });

  // ── Drop on self is silently ignored ──────────────────────────────

  it('drop on self (same folder row) is a silent no-op', async () => {
    const service = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'list_content') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    await service.connectAdapter(adapter);
    (service as unknown as { privateContent: unknown[] }).privateContent = (
      await import('../../mock-file-data')
    ).mockPrivateContent.slice();

    const { container } = renderBrowser(service);

    const folderRow = container.querySelector(
      'button[aria-label="Projects"]',
    ) as HTMLElement;
    expect(folderRow).toBeTruthy();

    const dt = makeDataTransfer();
    await fireEvent.dragStart(folderRow, { dataTransfer: dt });
    await fireEvent.dragOver(folderRow, { dataTransfer: dt });
    await fireEvent.drop(folderRow, { dataTransfer: dt });

    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'move_content',
      expect.any(Object),
    );
  });

  // ── Backend errors surface inline ─────────────────────────────────

  it('surfaces a backend error message inline (no window.alert)', async () => {
    const service = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'list_content') return Promise.resolve([]);
      if (cmd === 'move_content') {
        return Promise.reject(
          new Error("Error: destination already has an entry named 'foo.txt'"),
        );
      }
      return Promise.resolve(undefined);
    });
    await service.connectAdapter(adapter);
    (service as unknown as { privateContent: unknown[] }).privateContent = (
      await import('../../mock-file-data')
    ).mockPrivateContent.slice();

    const { container, findByRole } = renderBrowser(service);

    const sourceRow = container.querySelector(
      'button[aria-label="favorite-track.flac"]',
    ) as HTMLElement;
    const folderRow = container.querySelector(
      'button[aria-label="Projects"]',
    ) as HTMLElement;

    const dt = makeDataTransfer();
    await fireEvent.dragStart(sourceRow, { dataTransfer: dt });
    await fireEvent.dragOver(folderRow, { dataTransfer: dt });
    await fireEvent.drop(folderRow, { dataTransfer: dt });

    // The error banner should appear with the cleaned-up message
    // (strip the "Error: " prefix per CLAUDE.md's IPC error pattern).
    const banner = await findByRole('alert');
    expect(banner.textContent).toContain(
      "destination already has an entry named 'foo.txt'",
    );
    expect(banner.textContent).not.toContain('Error: ');
  });
});
