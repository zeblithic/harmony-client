/**
 * ZEB-163 — Folder-as-root OS upload UI tests.
 *
 * Renders the real FileBrowser with a real FileManagerService backed by
 * a mocked Tauri adapter (whose listen captures handler refs so tests
 * can fire fake os-folder-dropped + folder-ingest-progress events) and
 * a mocked @tauri-apps/plugin-dialog so picker tests don't open a real
 * native dialog. Exercises the picker entry, drag-drop entry, in-flight
 * serialisation, progress-event plumbing, cancel button, summary modal
 * branches (success/cancelled/skipped/failed/overflow), mid-ingest
 * navigation preservation, and the post-ingest serviceVersion refetch.
 *
 * Mirrors file-browser-create-folder.test.ts for adapter / data setup
 * (TestHarness dedup deferred to ZEB-183).
 */
import { render, fireEvent, cleanup, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, afterEach } from 'vitest';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import FileBrowser from '../FileBrowser.svelte';
import { FileManagerService, type IngestFolderTreeResult } from '../../file-manager-service';
import { createMockAdapter } from '../../test-utils';

vi.mock('@tauri-apps/plugin-dialog', () => ({ open: vi.fn() }));

const mockOpenDialog = vi.mocked(openDialog);

afterEach(() => {
  cleanup();
  mockOpenDialog.mockReset();
});

function renderBrowser(
  service: FileManagerService,
  adapter: ReturnType<typeof createMockAdapter>['adapter'],
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
      adapter,
      currentFolderCid: null as string | null,
      selectedCid: null as string | null,
      selectedSidecarId: null as string | null,
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

/** Spin up a real FileManagerService against a mock adapter wired with a
 *  vi.fn invoke that defaults to safe behaviors for the IPCs the browser
 *  fires during mount/refetch. Tests can re-stub invoke for specific
 *  behaviors. */
async function setupBrowserWithMockData() {
  const service = new FileManagerService();
  const harness = createMockAdapter();
  const { adapter } = harness;
  adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
    if (cmd === 'list_content') return Promise.resolve([]);
    if (cmd === 'cancel_folder_ingest') return Promise.resolve(undefined);
    return Promise.resolve(undefined);
  });
  await service.connectAdapter(adapter);
  (service as unknown as { privateContent: unknown[] }).privateContent = (
    await import('../../mock-file-data')
  ).mockPrivateContent.slice();
  return { service, adapter, emit: harness.emit };
}

function deferred<T>(): {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (err: unknown) => void;
} {
  let resolve!: (value: T) => void;
  let reject!: (err: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

/** Extract the jobId the frontend minted for the most recent
 *  `ingest_folder_tree` invoke. ZEB-163 round-1 fix: the frontend
 *  generates the UUID synchronously before the IPC so the Cancel button
 *  works during the pre-walk window; tests that emit progress events
 *  need to match against this real id. */
function getMintedJobId(adapter: { invoke: unknown }): string {
  const invoke = adapter.invoke as { mock: { calls: unknown[][] } };
  const calls = invoke.mock.calls.filter((c) => c[0] === 'ingest_folder_tree');
  const call = calls[calls.length - 1];
  if (!call) throw new Error('ingest_folder_tree not invoked yet');
  return (call[1] as { jobId: string }).jobId;
}

function makeResult(overrides: Partial<IngestFolderTreeResult> = {}): IngestFolderTreeResult {
  return {
    jobId: 'job-1',
    rootSidecarId: 'new-root-sidecar',
    rootCid: 'cid-new-root',
    rootName: 'Dropped',
    totalFilesSeen: 3,
    preWalkTotal: 3,
    succeeded: 3,
    skipped: { hidden: 0, symlink: 0, other: 0 },
    failed: [],
    failedOverflow: 0,
    cancelled: false,
    ...overrides,
  };
}

describe('FileBrowser folder ingest UI (ZEB-163)', () => {
  // ── 1. Add-folder click opens native directory picker ────────────────
  it('Add folder button opens the native directory picker', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    mockOpenDialog.mockResolvedValueOnce(null); // user cancels — no ingest

    const { container } = renderBrowser(service, adapter);

    const btn = container.querySelector(
      'button[aria-label="Add folder from disk"]',
    ) as HTMLButtonElement;
    expect(btn).toBeTruthy();
    await fireEvent.click(btn);

    await waitFor(() => {
      expect(mockOpenDialog).toHaveBeenCalledWith({
        directory: true,
        multiple: false,
      });
    });
    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'ingest_folder_tree',
      expect.any(Object),
    );
  });

  // ── 2. Picker resolution fires ingest_folder_tree IPC ────────────────
  it('picker resolution dispatches ingest_folder_tree with root-level args', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const ingest = deferred<IngestFolderTreeResult>();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'ingest_folder_tree') return ingest.promise;
      if (cmd === 'list_content') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    mockOpenDialog.mockResolvedValueOnce('/tmp/picked');

    const { container } = renderBrowser(service, adapter);
    await fireEvent.click(
      container.querySelector('button[aria-label="Add folder from disk"]') as HTMLButtonElement,
    );

    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith(
        'ingest_folder_tree',
        expect.objectContaining({
          rootPath: '/tmp/picked',
          parentSidecarId: null,
          parentPath: [],
        }),
      );
    });

    ingest.resolve(makeResult());
  });

  // ── 3. Picker dispatched from inside a nested folder carries
  //      the top-level sidecar id and parentPath chain ────────────────────
  it('picker resolution from a nested folder dispatches the parent breadcrumb chain', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const ingest = deferred<IngestFolderTreeResult>();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'ingest_folder_tree') return ingest.promise;
      if (cmd === 'list_content') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    mockOpenDialog.mockResolvedValueOnce('/tmp/nested-pick');

    const { container } = renderBrowser(service, adapter, {
      currentFolderCid: 'cid-folder-projects',
    });

    await fireEvent.click(
      container.querySelector('button[aria-label="Add folder from disk"]') as HTMLButtonElement,
    );

    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith(
        'ingest_folder_tree',
        expect.objectContaining({
          rootPath: '/tmp/nested-pick',
          parentSidecarId: 'mock-sidecar-1',
          parentPath: ['cid-folder-projects'],
        }),
      );
    });

    ingest.resolve(makeResult());
  });

  // ── 4. OS-folder-drop event triggers ingest ──────────────────────────
  it('os-folder-dropped event triggers ingest_folder_tree with the dropped path', async () => {
    const { service, adapter, emit } = await setupBrowserWithMockData();
    const ingest = deferred<IngestFolderTreeResult>();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'ingest_folder_tree') return ingest.promise;
      if (cmd === 'list_content') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });

    renderBrowser(service, adapter);
    // Give the $effect that subscribes to os-folder-dropped a microtask
    // to register before we emit.
    await waitFor(() => {
      expect(adapter.listen).toHaveBeenCalledWith(
        'os-folder-dropped',
        expect.any(Function),
      );
    });

    emit('os-folder-dropped', { path: '/tmp/dropped', x: 100, y: 200 });

    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith(
        'ingest_folder_tree',
        expect.objectContaining({ rootPath: '/tmp/dropped' }),
      );
    });

    ingest.resolve(makeResult());
  });

  // ── 5. Multiple drops while in-flight are ignored ────────────────────
  it('rapid os-folder-drops while a walk is in-flight only dispatch one ingest', async () => {
    const { service, adapter, emit } = await setupBrowserWithMockData();
    const ingest = deferred<IngestFolderTreeResult>();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'ingest_folder_tree') return ingest.promise;
      if (cmd === 'list_content') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });

    renderBrowser(service, adapter);
    await waitFor(() => {
      expect(adapter.listen).toHaveBeenCalledWith(
        'os-folder-dropped',
        expect.any(Function),
      );
    });

    emit('os-folder-dropped', { path: '/tmp/first', x: 0, y: 0 });
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith(
        'ingest_folder_tree',
        expect.objectContaining({ rootPath: '/tmp/first' }),
      );
    });
    emit('os-folder-dropped', { path: '/tmp/second', x: 0, y: 0 });
    emit('os-folder-dropped', { path: '/tmp/third', x: 0, y: 0 });

    // Microtask flush so any (incorrect) second invoke would land.
    await new Promise((r) => setTimeout(r, 0));

    const ingestCalls = (adapter.invoke as ReturnType<typeof vi.fn>).mock.calls.filter(
      (c) => c[0] === 'ingest_folder_tree',
    );
    expect(ingestCalls).toHaveLength(1);

    ingest.resolve(makeResult());
  });

  // ── 6. Progress event updates the modal ──────────────────────────────
  it('folder-ingest-progress event updates the modal counter and current path', async () => {
    const { service, adapter, emit } = await setupBrowserWithMockData();
    const ingest = deferred<IngestFolderTreeResult>();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'ingest_folder_tree') return ingest.promise;
      if (cmd === 'list_content') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    mockOpenDialog.mockResolvedValueOnce('/tmp/work');

    const { container } = renderBrowser(service, adapter);
    await fireEvent.click(
      container.querySelector('button[aria-label="Add folder from disk"]') as HTMLButtonElement,
    );

    // Progress modal opens immediately (indeterminate) once the ingest
    // promise is awaited.
    await waitFor(() => {
      expect(document.querySelector('[role="dialog"]')).not.toBeNull();
    });

    emit('folder-ingest-progress', {
      jobId: getMintedJobId(adapter),
      completed: 7,
      total: 20,
      currentPath: '/tmp/work/file7.txt',
    });

    await waitFor(() => {
      const counter = document.querySelector('.counter');
      if (!counter || !/7 of 20/.test(counter.textContent ?? '')) {
        throw new Error(`counter not yet updated: "${counter?.textContent}"`);
      }
    });

    // Bot finding 11: the currentPath also has to render. The
    // FolderIngestProgressModal middle-truncates long paths, so we match
    // a substring distinctive enough to identify the file (the leaf
    // name) rather than the whole path.
    await waitFor(() => {
      const path = document.querySelector('.current-path');
      if (!path || !/file7\.txt/.test(path.textContent ?? '')) {
        throw new Error(`current-path not rendered: "${path?.textContent}"`);
      }
    });

    ingest.resolve(makeResult());
  });

  // ── 7. Cancel button triggers cancel_folder_ingest IPC ────────────────
  it('Cancel button dispatches cancel_folder_ingest and flips label to "Cancelling…"', async () => {
    const { service, adapter, emit } = await setupBrowserWithMockData();
    const ingest = deferred<IngestFolderTreeResult>();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'ingest_folder_tree') return ingest.promise;
      if (cmd === 'cancel_folder_ingest') return Promise.resolve(undefined);
      if (cmd === 'list_content') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    mockOpenDialog.mockResolvedValueOnce('/tmp/cancel-me');

    const { container } = renderBrowser(service, adapter);
    await fireEvent.click(
      container.querySelector('button[aria-label="Add folder from disk"]') as HTMLButtonElement,
    );

    // Wait for the progress modal to mount. jobId is minted synchronously
    // by startFolderIngest BEFORE the IPC call (round-1 fix #3), so Cancel
    // is targetable from the moment the modal renders — no need to wait
    // for the first progress event.
    await waitFor(() => {
      expect(document.querySelector('.cancel-btn')).not.toBeNull();
    });
    const mintedJobId = getMintedJobId(adapter);
    emit('folder-ingest-progress', {
      jobId: mintedJobId,
      completed: 1,
      total: 50,
      currentPath: '/tmp/cancel-me/a',
    });
    await waitFor(() => {
      const c = document.querySelector('.counter');
      if (!c || !/1 of 50/.test(c.textContent ?? '')) {
        throw new Error('progress not captured yet');
      }
    });

    const cancelBtn = document.querySelector('.cancel-btn') as HTMLButtonElement;
    await fireEvent.click(cancelBtn);

    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('cancel_folder_ingest', {
        jobId: mintedJobId,
      });
    });

    await waitFor(() => {
      const label = document.querySelector('.cancel-btn')?.textContent ?? '';
      if (!/Cancelling/.test(label)) {
        throw new Error(`cancel button label still: "${label}"`);
      }
    });

    ingest.resolve(makeResult({ jobId: mintedJobId, cancelled: true }));
  });

  // ── 8. Promise resolution closes progress, opens summary ─────────────
  it('ingest_folder_tree resolution closes the progress modal and opens the summary modal', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const ingest = deferred<IngestFolderTreeResult>();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'ingest_folder_tree') return ingest.promise;
      if (cmd === 'list_content') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    mockOpenDialog.mockResolvedValueOnce('/tmp/done');

    const { container } = renderBrowser(service, adapter);
    await fireEvent.click(
      container.querySelector('button[aria-label="Add folder from disk"]') as HTMLButtonElement,
    );

    await waitFor(() => {
      expect(document.querySelector('.counter')).not.toBeNull();
    });

    ingest.resolve(
      makeResult({
        rootName: 'Photos',
        succeeded: 12,
        totalFilesSeen: 12,
      }),
    );

    // Progress modal goes away; summary modal renders.
    await waitFor(() => {
      const headline = document.querySelector('.title');
      if (!headline || !/Added folder Photos with 12 files/.test(headline.textContent ?? '')) {
        throw new Error(`headline not yet: "${headline?.textContent}"`);
      }
    });

    // The counter element from the progress modal must no longer be present.
    expect(document.querySelector('.counter')).toBeNull();
  });

  // ── 9. Summary cancelled state ───────────────────────────────────────
  it('summary modal headline reflects a cancelled walk', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const ingest = deferred<IngestFolderTreeResult>();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'ingest_folder_tree') return ingest.promise;
      if (cmd === 'list_content') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    mockOpenDialog.mockResolvedValueOnce('/tmp/abort');

    const { container } = renderBrowser(service, adapter);
    await fireEvent.click(
      container.querySelector('button[aria-label="Add folder from disk"]') as HTMLButtonElement,
    );

    await waitFor(() => {
      expect(document.querySelector('.counter')).not.toBeNull();
    });

    ingest.resolve(
      makeResult({
        cancelled: true,
        succeeded: 4,
        totalFilesSeen: 10,
        // Round-1 fix: cancelled headline now uses preWalkTotal as the
        // denominator (the pre-walk count taken before the walker
        // started), not totalFilesSeen (counters of leaves the walker
        // actually touched before the cancel). preWalkTotal > 0 wins.
        preWalkTotal: 10,
      }),
    );

    await waitFor(() => {
      const headline = document.querySelector('.title');
      if (!headline || !/Cancelled — added 4 of 10 files/.test(headline.textContent ?? '')) {
        throw new Error(`cancelled headline not yet: "${headline?.textContent}"`);
      }
    });
  });

  // ── 9b. Cancelled headline omits the denominator when the pre-walk failed
  //       (Round-4 bot fix: totalFilesSeen counts only leaves the walker
  //       actually touched, so substituting it for preWalkTotal silently
  //       changed the meaning of the headline).
  it('summary modal cancelled headline omits denominator when preWalkTotal is -1', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const ingest = deferred<IngestFolderTreeResult>();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'ingest_folder_tree') return ingest.promise;
      if (cmd === 'list_content') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    mockOpenDialog.mockResolvedValueOnce('/tmp/abort-pre-walk-fail');

    const { container } = renderBrowser(service, adapter);
    await fireEvent.click(
      container.querySelector('button[aria-label="Add folder from disk"]') as HTMLButtonElement,
    );

    await waitFor(() => {
      expect(document.querySelector('.counter')).not.toBeNull();
    });

    ingest.resolve(
      makeResult({
        cancelled: true,
        succeeded: 7,
        totalFilesSeen: 9,
        preWalkTotal: -1,
      }),
    );

    await waitFor(() => {
      const headline = document.querySelector('.title');
      const text = headline?.textContent ?? '';
      if (!/Cancelled — added 7 files \(total unknown\)/.test(text)) {
        throw new Error(`pre-walk-fail cancelled headline not yet: "${text}"`);
      }
      // Guard against the old fallback to totalFilesSeen sneaking back.
      if (/of 9 files/.test(text)) {
        throw new Error(`headline still uses totalFilesSeen denominator: "${text}"`);
      }
    });
  });

  // ── 10. Summary skipped section renders all rows when > 0 ──────
  it('summary modal renders hidden / symlink / other rows when each count is > 0', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const ingest = deferred<IngestFolderTreeResult>();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'ingest_folder_tree') return ingest.promise;
      if (cmd === 'list_content') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    mockOpenDialog.mockResolvedValueOnce('/tmp/skips');

    const { container } = renderBrowser(service, adapter);
    await fireEvent.click(
      container.querySelector('button[aria-label="Add folder from disk"]') as HTMLButtonElement,
    );

    await waitFor(() => {
      expect(document.querySelector('.counter')).not.toBeNull();
    });

    ingest.resolve(
      makeResult({
        skipped: { hidden: 3, symlink: 1, other: 4 },
      }),
    );

    await waitFor(() => {
      if (!document.querySelector('details')) {
        throw new Error('details element not yet rendered');
      }
    });

    const summaryText =
      document.querySelector('details summary')?.textContent ?? '';
    expect(summaryText).toMatch(/Skipped: 8 items/);
    const rows = Array.from(document.querySelectorAll('details li')).map(
      (li) => li.textContent ?? '',
    );
    expect(rows.some((t) => /3 hidden files/.test(t))).toBe(true);
    expect(rows.some((t) => /1 symlinks/.test(t))).toBe(true);
    expect(rows.some((t) => /4 other special files/.test(t))).toBe(true);
  });

  // ── 11. Summary failed section + overflow row ────────────────────────
  it('summary modal renders the capped failed list with an overflow row when failedOverflow > 0', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const ingest = deferred<IngestFolderTreeResult>();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'ingest_folder_tree') return ingest.promise;
      if (cmd === 'list_content') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    mockOpenDialog.mockResolvedValueOnce('/tmp/fails');

    const { container } = renderBrowser(service, adapter);
    await fireEvent.click(
      container.querySelector('button[aria-label="Add folder from disk"]') as HTMLButtonElement,
    );

    await waitFor(() => {
      expect(document.querySelector('.counter')).not.toBeNull();
    });

    ingest.resolve(
      makeResult({
        failed: [
          { path: '/tmp/fails/a.txt', message: 'permission denied' },
          { path: '/tmp/fails/b.txt', message: 'I/O error' },
        ],
        failedOverflow: 7,
      }),
    );

    await waitFor(() => {
      if (!document.querySelector('.overflow')) {
        throw new Error('overflow row not yet rendered');
      }
    });

    const overflow = document.querySelector('.overflow');
    expect(overflow?.textContent ?? '').toMatch(/and 7 more/);

    // The two enumerated failures should each appear in a list row.
    const rowTexts = Array.from(document.querySelectorAll('details li')).map(
      (li) => li.textContent ?? '',
    );
    expect(rowTexts.some((t) => /\/tmp\/fails\/a\.txt/.test(t))).toBe(true);
    expect(rowTexts.some((t) => /\/tmp\/fails\/b\.txt/.test(t))).toBe(true);

    // Summary count = failed.length + failedOverflow = 2 + 7 = 9.
    const summaryEl = Array.from(
      document.querySelectorAll('details summary'),
    ).find((el) => /Failed/.test(el.textContent ?? ''));
    expect(summaryEl?.textContent ?? '').toMatch(/Failed: 9 files/);
  });

  // ── 12. Navigation mid-ingest does not dismiss the progress modal ────
  it('navigating to a different folder while a walk is in-flight does not dismiss the progress modal', async () => {
    const { service, adapter, emit } = await setupBrowserWithMockData();
    const ingest = deferred<IngestFolderTreeResult>();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'ingest_folder_tree') return ingest.promise;
      if (cmd === 'list_content') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    mockOpenDialog.mockResolvedValueOnce('/tmp/long-walk');

    const { container, rerender } = renderBrowser(service, adapter);
    await fireEvent.click(
      container.querySelector('button[aria-label="Add folder from disk"]') as HTMLButtonElement,
    );

    await waitFor(() => {
      expect(document.querySelector('.counter')).not.toBeNull();
    });
    emit('folder-ingest-progress', {
      jobId: getMintedJobId(adapter),
      completed: 2,
      total: 100,
      currentPath: '/tmp/long-walk/x',
    });
    await waitFor(() => {
      const c = document.querySelector('.counter');
      if (!c || !/2 of 100/.test(c.textContent ?? '')) {
        throw new Error('progress not captured');
      }
    });

    await rerender({
      service,
      adapter,
      currentFolderCid: 'cid-folder-projects',
      selectedCid: null,
      selectedSidecarId: null,
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

    // Progress modal stays open across the navigation.
    const counter = document.querySelector('.counter');
    expect(counter).not.toBeNull();
    expect(counter?.textContent ?? '').toMatch(/2 of 100/);

    ingest.resolve(makeResult({ jobId: 'job-walk' }));
  });

  // ── 13. serviceVersion bump on success refetches list_content ────────
  it('successful ingest bumps serviceVersion so the root listing re-fetches', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const ingest = deferred<IngestFolderTreeResult>();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'ingest_folder_tree') return ingest.promise;
      if (cmd === 'list_content') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    mockOpenDialog.mockResolvedValueOnce('/tmp/refetch');

    const { container } = renderBrowser(service, adapter, {
      currentFolderCid: 'cid-folder-projects',
    });

    // Baseline: count list_content invocations before completion. Mount
    // and the initial nav both drive list_content for the current cid.
    await waitFor(() => {
      const calls = (adapter.invoke as ReturnType<typeof vi.fn>).mock.calls.filter(
        (c) =>
          c[0] === 'list_content' &&
          (c[1] as { folderCid: string | null })?.folderCid === 'cid-folder-projects',
      );
      if (calls.length === 0) throw new Error('initial list_content not yet fired');
    });
    const callsBefore = (
      adapter.invoke as ReturnType<typeof vi.fn>
    ).mock.calls.filter(
      (c) =>
        c[0] === 'list_content' &&
        (c[1] as { folderCid: string | null })?.folderCid === 'cid-folder-projects',
    ).length;

    await fireEvent.click(
      container.querySelector('button[aria-label="Add folder from disk"]') as HTMLButtonElement,
    );
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith(
        'ingest_folder_tree',
        expect.any(Object),
      );
    });

    ingest.resolve(makeResult());

    // serviceVersion++ on success triggers the fetch effect to re-run for
    // the current folder. The post-resolution list_content invocation is
    // the assertion.
    await waitFor(() => {
      const callsAfter = (
        adapter.invoke as ReturnType<typeof vi.fn>
      ).mock.calls.filter(
        (c) =>
          c[0] === 'list_content' &&
          (c[1] as { folderCid: string | null })?.folderCid === 'cid-folder-projects',
      ).length;
      if (callsAfter <= callsBefore) {
        throw new Error(
          `list_content not re-fetched after completion: before=${callsBefore}, after=${callsAfter}`,
        );
      }
    });
  });

  // ── 14. Picker click is a no-op while a walk is in-flight ────────────
  it('clicking "Add folder…" while a walk is in-flight does not re-open the picker', async () => {
    const { service, adapter, emit } = await setupBrowserWithMockData();
    const ingest = deferred<IngestFolderTreeResult>();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'ingest_folder_tree') return ingest.promise;
      if (cmd === 'list_content') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    mockOpenDialog.mockResolvedValueOnce('/tmp/first-walk');

    const { container } = renderBrowser(service, adapter);
    const btn = container.querySelector(
      'button[aria-label="Add folder from disk"]',
    ) as HTMLButtonElement;
    await fireEvent.click(btn);

    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith(
        'ingest_folder_tree',
        expect.any(Object),
      );
    });
    // Emit a progress event so activeIngestProgress is firmly set —
    // it's already set synchronously by startFolderIngest, but the
    // event also captures the jobId.
    emit('folder-ingest-progress', {
      jobId: getMintedJobId(adapter),
      completed: 1,
      total: 10,
      currentPath: '/tmp/first-walk/a',
    });

    // Second click while in-flight: the activeIngestProgress guard
    // returns early, so openDialog must NOT be invoked a second time.
    await fireEvent.click(btn);
    await fireEvent.click(btn);

    await new Promise((r) => setTimeout(r, 0));
    expect(mockOpenDialog).toHaveBeenCalledTimes(1);

    ingest.resolve(makeResult());
  });
});
