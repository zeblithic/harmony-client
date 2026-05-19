/**
 * ZEB-166 — File Manager inline new-folder + folder-load error blocks.
 *
 * Renders the real FileBrowser with a real FileManagerService backed
 * by a mocked Tauri adapter. Exercises the toolbar "+" path that opens
 * the inline placeholder row, the Enter/Escape/blur lifecycle, the
 * empty-after-trim inline error, the backend-rejection-keeps-open
 * error path, the in-flight-blur no-op, the nav-away cleanup, and the
 * inline FolderLoadError retry block.
 *
 * Mirrors file-browser-rename.test.ts for adapter / data setup.
 */
import { render, fireEvent, cleanup, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, afterEach } from 'vitest';
import FileBrowser from '../FileBrowser.svelte';
import { FileManagerService } from '../../file-manager-service';
import { createMockAdapter } from '../../test-utils';

afterEach(cleanup);

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
    onCleanupClick: vi.fn(),
  };
  const result = render(FileBrowser, {
    props: {
      service,
      currentFolderCid: null as string | null,
      selectedCid: null as string | null,
      selectedSidecarId: null as string | null,
      viewMode: 'list' as const,
      section: 'private' as const,
      searchQuery: '',
      showCleanup: false,
      serviceVersion: 0,
      ...callbacks,
      ...overrides,
    },
  });
  return { ...result, callbacks };
}

/** Set up an adapter that serves a stable empty list_content (so the
 *  post-create refetch doesn't disturb the seeded mock state) and a
 *  vi.fn-backed create_folder that defaults to a successful top-level
 *  return shape. Tests can stub specific cases on the returned spy. */
async function setupBrowserWithMockData() {
  const service = new FileManagerService();
  const { adapter } = createMockAdapter();
  adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
    if (cmd === 'list_content') return Promise.resolve([]);
    if (cmd === 'create_folder') {
      return Promise.resolve({ sidecarId: 'new-sidecar-id', cid: 'new-folder-cid' });
    }
    return Promise.resolve(undefined);
  });
  await service.connectAdapter(adapter);
  // Re-seed mock data wiped by the empty list_content during connect.
  (service as unknown as { privateContent: unknown[] }).privateContent = (
    await import('../../mock-file-data')
  ).mockPrivateContent.slice();
  return { service, adapter };
}

describe('FileBrowser inline new-folder + load error (ZEB-166)', () => {
  // ── 1. + opens the inline placeholder row ─────────────────────────

  it('clicking + opens the inline placeholder row', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const { container } = renderBrowser(service);

    // No placeholder before the click.
    expect(container.querySelector('.file-row-name-input')).toBeNull();

    const newFolderBtn = container.querySelector(
      'button[aria-label="New Folder"]',
    ) as HTMLButtonElement;
    expect(newFolderBtn).toBeTruthy();
    await fireEvent.click(newFolderBtn);

    // Placeholder input renders, empty, and (via autoFocus) focused.
    const input = await waitFor(() => {
      const el = container.querySelector(
        '.file-row-name-input',
      ) as HTMLInputElement | null;
      if (!el) throw new Error('placeholder input not rendered');
      return el;
    });
    expect(input.value).toBe('');
    // Autofocus action focuses the placeholder input on mount.
    expect(document.activeElement).toBe(input);

    // No create_folder IPC fired just from opening the placeholder.
    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'create_folder',
      expect.any(Object),
    );
  });

  // ── 2. Enter on a valid name fires create_folder IPC ─────────────

  it('Enter on a valid name fires create_folder IPC and closes the placeholder', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const { container } = renderBrowser(service);

    const newFolderBtn = container.querySelector(
      'button[aria-label="New Folder"]',
    ) as HTMLButtonElement;
    await fireEvent.click(newFolderBtn);

    const input = await waitFor(() => {
      const el = container.querySelector(
        '.file-row-name-input',
      ) as HTMLInputElement | null;
      if (!el) throw new Error('placeholder input not rendered');
      return el;
    });

    await fireEvent.input(input, { target: { value: 'My New Folder' } });
    await fireEvent.keyDown(input, { key: 'Enter' });

    // Root creation: parentSidecarId = null, parentPath = [].
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith(
        'create_folder',
        expect.objectContaining({
          name: 'My New Folder',
          parentSidecarId: null,
          parentPath: [],
        }),
      );
    });

    // Placeholder gone after success.
    await waitFor(() => {
      const stillThere = container.querySelector('.file-row-name-input');
      if (stillThere) throw new Error('placeholder should be gone after commit');
    });
  });

  // ── 3. Whitespace-only Enter shows inline empty-name error ───────

  it('whitespace-only Enter shows inline empty-name error, no IPC', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const { container, findByRole } = renderBrowser(service);

    const newFolderBtn = container.querySelector(
      'button[aria-label="New Folder"]',
    ) as HTMLButtonElement;
    await fireEvent.click(newFolderBtn);

    const input = await waitFor(() => {
      const el = container.querySelector(
        '.file-row-name-input',
      ) as HTMLInputElement | null;
      if (!el) throw new Error('placeholder input not rendered');
      return el;
    });

    await fireEvent.input(input, { target: { value: '   ' } });
    await fireEvent.keyDown(input, { key: 'Enter' });

    const banner = await findByRole('alert');
    expect(banner.textContent).toContain('Name cannot be empty');

    // Input still rendered for retry.
    expect(container.querySelector('.file-row-name-input')).not.toBeNull();

    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'create_folder',
      expect.any(Object),
    );
  });

  // ── 4. Escape cancels — placeholder gone, no IPC ─────────────────

  it('Escape cancels — placeholder gone, no IPC', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const { container } = renderBrowser(service);

    const newFolderBtn = container.querySelector(
      'button[aria-label="New Folder"]',
    ) as HTMLButtonElement;
    await fireEvent.click(newFolderBtn);

    const input = await waitFor(() => {
      const el = container.querySelector(
        '.file-row-name-input',
      ) as HTMLInputElement | null;
      if (!el) throw new Error('placeholder input not rendered');
      return el;
    });

    await fireEvent.input(input, { target: { value: 'garbage' } });
    await fireEvent.keyDown(input, { key: 'Escape' });

    await waitFor(() => {
      const stillThere = container.querySelector('.file-row-name-input');
      if (stillThere) throw new Error('placeholder should be gone after Escape');
    });
    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'create_folder',
      expect.any(Object),
    );
  });

  // ── 5. Blur cancels — placeholder gone (Finder parity) ───────────

  it('blur cancels — placeholder gone (Finder parity)', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const { container } = renderBrowser(service);

    const newFolderBtn = container.querySelector(
      'button[aria-label="New Folder"]',
    ) as HTMLButtonElement;
    await fireEvent.click(newFolderBtn);

    const input = await waitFor(() => {
      const el = container.querySelector(
        '.file-row-name-input',
      ) as HTMLInputElement | null;
      if (!el) throw new Error('placeholder input not rendered');
      return el;
    });

    await fireEvent.blur(input);

    await waitFor(() => {
      const stillThere = container.querySelector('.file-row-name-input');
      if (stillThere) throw new Error('placeholder should be gone after blur');
    });
    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'create_folder',
      expect.any(Object),
    );
  });

  // ── 6. Blur during in-flight IPC is a no-op ──────────────────────

  it('blur during in-flight IPC is a no-op — input survives subsequent rejection', async () => {
    // Wire a manually-controlled deferred so the blur fires during the
    // await window. Without the in-flight guard, blur would cancel
    // edit mode and the catch's newFolderError would orphan a banner
    // with no input to retry in.
    const service = new FileManagerService();
    const { adapter } = createMockAdapter();
    let rejectCreate!: (err: unknown) => void;
    const createPending = new Promise((_resolve, reject) => {
      rejectCreate = reject;
    });
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'list_content') return Promise.resolve([]);
      if (cmd === 'create_folder') return createPending;
      return Promise.resolve(undefined);
    });
    await service.connectAdapter(adapter);
    (service as unknown as { privateContent: unknown[] }).privateContent = (
      await import('../../mock-file-data')
    ).mockPrivateContent.slice();

    const { container, findByRole } = renderBrowser(service);

    const newFolderBtn = container.querySelector(
      'button[aria-label="New Folder"]',
    ) as HTMLButtonElement;
    await fireEvent.click(newFolderBtn);

    const input = await waitFor(() => {
      const el = container.querySelector(
        '.file-row-name-input',
      ) as HTMLInputElement | null;
      if (!el) throw new Error('placeholder input not rendered');
      return el;
    });

    await fireEvent.input(input, { target: { value: 'in-flight-folder' } });
    await fireEvent.keyDown(input, { key: 'Enter' });

    // Blur fires while the IPC is still awaiting — must be a no-op.
    await fireEvent.blur(input);

    // Now resolve the IPC with a rejection so the catch block runs.
    rejectCreate(new Error('name already in use'));

    const banner = await findByRole('alert');
    expect(banner.textContent).toContain('name already in use');

    // Input must still be in the DOM — user has to be able to fix and retry.
    const stillThere = container.querySelector('.file-row-name-input');
    expect(stillThere).not.toBeNull();
  });

  // ── 7. Backend rejection keeps placeholder open with inline error ─

  it('backend rejection keeps placeholder open with the error inline', async () => {
    const service = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'list_content') return Promise.resolve([]);
      if (cmd === 'create_folder') {
        return Promise.reject('A folder named X already exists');
      }
      return Promise.resolve(undefined);
    });
    await service.connectAdapter(adapter);
    (service as unknown as { privateContent: unknown[] }).privateContent = (
      await import('../../mock-file-data')
    ).mockPrivateContent.slice();

    const { container, findByRole } = renderBrowser(service);

    const newFolderBtn = container.querySelector(
      'button[aria-label="New Folder"]',
    ) as HTMLButtonElement;
    await fireEvent.click(newFolderBtn);

    const input = await waitFor(() => {
      const el = container.querySelector(
        '.file-row-name-input',
      ) as HTMLInputElement | null;
      if (!el) throw new Error('placeholder input not rendered');
      return el;
    });

    await fireEvent.input(input, { target: { value: 'X' } });
    await fireEvent.keyDown(input, { key: 'Enter' });

    const banner = await findByRole('alert');
    expect(banner.textContent).toContain('A folder named X already exists');

    // Placeholder still rendered for retry.
    expect(container.querySelector('.file-row-name-input')).not.toBeNull();
  });

  // ── 8. Nav-away while creating clears placeholder state ──────────

  it('navigating away while creating clears placeholder state', async () => {
    const { service } = await setupBrowserWithMockData();
    const { container, rerender } = renderBrowser(service);

    const newFolderBtn = container.querySelector(
      'button[aria-label="New Folder"]',
    ) as HTMLButtonElement;
    await fireEvent.click(newFolderBtn);

    const input = await waitFor(() => {
      const el = container.querySelector(
        '.file-row-name-input',
      ) as HTMLInputElement | null;
      if (!el) throw new Error('placeholder input not rendered');
      return el;
    });

    await fireEvent.input(input, { target: { value: 'half-typed' } });

    // Navigate to a nested folder — the $effect on currentFolderCid
    // calls cancelCreateFolder() which resets creatingFolder /
    // newFolderName / newFolderError.
    await rerender({
      service,
      currentFolderCid: 'cid-folder-projects',
      selectedCid: null,
      selectedSidecarId: null,
      viewMode: 'list',
      section: 'private',
      searchQuery: '',
      showCleanup: false,
      serviceVersion: 0,
      onItemClick: vi.fn(),
      onNavigateFolder: vi.fn(),
      onViewModeChange: vi.fn(),
      onSearchChange: vi.fn(),
      onSectionChange: vi.fn(),
      onUploadClick: vi.fn(),
      onCleanupClick: vi.fn(),
    });

    await waitFor(() => {
      const stillThere = container.querySelector('.file-row-name-input');
      if (stillThere) throw new Error('placeholder should be gone after nav');
    });

    // Re-open after nav: input should be empty (newFolderName reset)
    // and no error banner (newFolderError reset).
    const newFolderBtn2 = container.querySelector(
      'button[aria-label="New Folder"]',
    ) as HTMLButtonElement;
    await fireEvent.click(newFolderBtn2);
    const input2 = await waitFor(() => {
      const el = container.querySelector(
        '.file-row-name-input',
      ) as HTMLInputElement | null;
      if (!el) throw new Error('placeholder input not rendered after re-open');
      return el;
    });
    expect(input2.value).toBe('');
    expect(container.querySelector('.file-row-error')).toBeNull();
  });

  // ── 9. Folder-load error renders the inline retry block ──────────

  it('folder-load error renders the inline retry block', async () => {
    const service = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi
      .fn()
      .mockImplementation((cmd: string, args?: unknown) => {
        if (cmd === 'list_content') {
          const a = args as { folderCid: string | null };
          if (a?.folderCid === 'cid-folder-projects') {
            return Promise.reject(new Error('malformed manifest'));
          }
          return Promise.resolve([]);
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

    // Wait for the FolderLoadError block to mount after the rejection.
    await waitFor(() => {
      const block = container.querySelector('.folder-load-error');
      if (!block) throw new Error('FolderLoadError block not yet rendered');
    });

    const block = container.querySelector('.folder-load-error') as HTMLElement;
    expect(block).toBeTruthy();
    expect(block.textContent).toContain('Could not load folder contents:');
    expect(block.textContent).toContain('malformed manifest');

    // FileList / FileGrid NOT rendered while the error block is up.
    expect(container.querySelector('.file-list')).toBeNull();
    expect(container.querySelector('.file-grid')).toBeNull();
  });

  // ── 10. Retry button re-fires listFolderContents ─────────────────

  it('Retry button re-fires listFolderContents', async () => {
    const service = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi
      .fn()
      .mockImplementation((cmd: string, args?: unknown) => {
        if (cmd === 'list_content') {
          const a = args as { folderCid: string | null };
          if (a?.folderCid === 'cid-folder-projects') {
            return Promise.reject(new Error('transient backend hiccup'));
          }
          return Promise.resolve([]);
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

    // Wait for the error block.
    await waitFor(() => {
      const block = container.querySelector('.folder-load-error');
      if (!block) throw new Error('FolderLoadError block not yet rendered');
    });

    // Count list_content invocations for the failing folder before
    // the retry click.
    const callsBefore = (adapter.invoke as ReturnType<typeof vi.fn>).mock.calls.filter(
      (c) =>
        c[0] === 'list_content' &&
        (c[1] as { folderCid: string | null })?.folderCid === 'cid-folder-projects',
    ).length;
    expect(callsBefore).toBeGreaterThan(0);

    const retryBtn = container.querySelector(
      '.folder-load-error-retry',
    ) as HTMLButtonElement;
    expect(retryBtn).toBeTruthy();
    await fireEvent.click(retryBtn);

    // After the retry token bumps and the effect re-runs, list_content
    // is invoked again for the same folder.
    await waitFor(() => {
      const callsAfter = (
        adapter.invoke as ReturnType<typeof vi.fn>
      ).mock.calls.filter(
        (c) =>
          c[0] === 'list_content' &&
          (c[1] as { folderCid: string | null })?.folderCid === 'cid-folder-projects',
      ).length;
      if (callsAfter <= callsBefore) {
        throw new Error('list_content was not re-invoked after Retry');
      }
    });
  });

  // ── 11. Nav-away while error showing clears the banner ───────────

  it('nav-away while error showing clears the banner and renders the new folder', async () => {
    const service = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi
      .fn()
      .mockImplementation((cmd: string, args?: unknown) => {
        if (cmd === 'list_content') {
          const a = args as { folderCid: string | null };
          if (a?.folderCid === 'cid-folder-projects') {
            return Promise.reject(new Error('malformed manifest'));
          }
          return Promise.resolve([]);
        }
        return Promise.resolve(undefined);
      });
    await service.connectAdapter(adapter);
    (service as unknown as { privateContent: unknown[] }).privateContent = (
      await import('../../mock-file-data')
    ).mockPrivateContent.slice();

    const { container, rerender } = renderBrowser(service, {
      currentFolderCid: 'cid-folder-projects',
    });

    // Wait for the error block.
    await waitFor(() => {
      const block = container.querySelector('.folder-load-error');
      if (!block) throw new Error('FolderLoadError block not yet rendered');
    });

    // Navigate back to root.
    await rerender({
      service,
      currentFolderCid: null,
      selectedCid: null,
      selectedSidecarId: null,
      viewMode: 'list',
      section: 'private',
      searchQuery: '',
      showCleanup: false,
      serviceVersion: 0,
      onItemClick: vi.fn(),
      onNavigateFolder: vi.fn(),
      onViewModeChange: vi.fn(),
      onSearchChange: vi.fn(),
      onSectionChange: vi.fn(),
      onUploadClick: vi.fn(),
      onCleanupClick: vi.fn(),
    });

    // FolderLoadError block must be gone.
    await waitFor(() => {
      const stillThere = container.querySelector('.folder-load-error');
      if (stillThere) throw new Error('error block should be gone after nav-away');
    });

    // The FileList for root renders instead.
    expect(container.querySelector('.file-list')).not.toBeNull();
  });
});
