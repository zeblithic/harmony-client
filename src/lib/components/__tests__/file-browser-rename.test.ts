/**
 * ZEB-299 — File Manager inline rename.
 *
 * Renders the real FileBrowser with a real FileManagerService backed
 * by a mocked Tauri adapter. Exercises the F2 keyboard path, the
 * Enter/Escape/blur lifecycle on the input, the empty-after-trim
 * inline error, and the same-name short-circuit (no IPC fires).
 *
 * Mirrors file-browser-drag-drop.test.ts for adapter / data setup.
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
      serviceVersion: 0,
      ...callbacks,
      ...overrides,
    },
  });
  return { ...result, callbacks };
}

/** Set up an adapter that serves a stable empty list_content (so the
 *  post-rename refetch doesn't disturb the seeded mock state) and a
 *  vi.fn-backed rename_content that defaults to a successful top-level
 *  return shape. Tests can stub specific cases on the returned spy. */
async function setupBrowserWithMockData() {
  const service = new FileManagerService();
  const { adapter } = createMockAdapter();
  adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
    if (cmd === 'list_content') return Promise.resolve([]);
    if (cmd === 'rename_content') return Promise.resolve({ srcNewCid: null });
    return Promise.resolve(undefined);
  });
  await service.connectAdapter(adapter);
  // Re-seed mock data wiped by the empty list_content during connect.
  (service as unknown as { privateContent: unknown[] }).privateContent = (
    await import('../../mock-file-data')
  ).mockPrivateContent.slice();
  return { service, adapter };
}

describe('FileBrowser inline rename (ZEB-299)', () => {
  // ── F2 with no selection: no-op ───────────────────────────────────

  it('F2 with no selection does nothing (no input renders)', async () => {
    const { service } = await setupBrowserWithMockData();
    const { container } = renderBrowser(service, { selectedSidecarId: null });

    // Dispatch F2 on the file-browser root. With no selection, the
    // handler short-circuits and no input should appear.
    const browser = container.querySelector('.file-browser') as HTMLElement;
    expect(browser).toBeTruthy();
    await fireEvent.keyDown(browser, { key: 'F2' });

    // Wait a tick to let any reactive effects settle.
    await Promise.resolve();
    const input = container.querySelector('.file-row-name-input');
    expect(input).toBeNull();
  });

  // ── F2 with selection: enters edit mode ───────────────────────────

  it('F2 with a selected row enters edit mode (input replaces name span)', async () => {
    const { service } = await setupBrowserWithMockData();
    const { container } = renderBrowser(service, {
      selectedSidecarId: 'mock-sidecar-4', // favorite-track.flac
    });

    const browser = container.querySelector('.file-browser') as HTMLElement;
    await fireEvent.keyDown(browser, { key: 'F2' });

    await waitFor(() => {
      const input = container.querySelector(
        '.file-row-name-input',
      ) as HTMLInputElement | null;
      if (!input) throw new Error('rename input not rendered');
      expect(input.value).toBe('favorite-track.flac');
    });
  });

  // ── Enter commits via service.renameContent ───────────────────────

  it('Enter inside the rename input commits via service.renameContent', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const { container } = renderBrowser(service, {
      selectedSidecarId: 'mock-sidecar-4',
    });

    const browser = container.querySelector('.file-browser') as HTMLElement;
    await fireEvent.keyDown(browser, { key: 'F2' });

    const input = await waitFor(() => {
      const el = container.querySelector(
        '.file-row-name-input',
      ) as HTMLInputElement | null;
      if (!el) throw new Error('rename input not rendered');
      return el;
    });

    // Replace the input value, then press Enter.
    await fireEvent.input(input, { target: { value: 'renamed-track.flac' } });
    await fireEvent.keyDown(input, { key: 'Enter' });

    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith(
        'rename_content',
        expect.objectContaining({
          srcSidecarId: 'mock-sidecar-4',
          srcChildCid: 'cid-song-favorite',
          srcChildName: 'favorite-track.flac',
          srcPath: ['cid-song-favorite'],
          newName: 'renamed-track.flac',
        }),
      );
    });
  });

  // ── Escape cancels (input gone, name span back) ───────────────────

  it('Escape cancels edit mode (input gone, name span back, no IPC)', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const { container } = renderBrowser(service, {
      selectedSidecarId: 'mock-sidecar-4',
    });

    const browser = container.querySelector('.file-browser') as HTMLElement;
    await fireEvent.keyDown(browser, { key: 'F2' });

    const input = await waitFor(() => {
      const el = container.querySelector(
        '.file-row-name-input',
      ) as HTMLInputElement | null;
      if (!el) throw new Error('rename input not rendered');
      return el;
    });

    await fireEvent.input(input, { target: { value: 'garbage' } });
    await fireEvent.keyDown(input, { key: 'Escape' });

    await waitFor(() => {
      const stillThere = container.querySelector('.file-row-name-input');
      if (stillThere) throw new Error('input should be gone after Escape');
    });
    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'rename_content',
      expect.any(Object),
    );
  });

  // ── Blur cancels (Finder parity) ──────────────────────────────────

  it('blurring the input cancels edit mode (matches Finder)', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const { container } = renderBrowser(service, {
      selectedSidecarId: 'mock-sidecar-4',
    });

    const browser = container.querySelector('.file-browser') as HTMLElement;
    await fireEvent.keyDown(browser, { key: 'F2' });

    const input = await waitFor(() => {
      const el = container.querySelector(
        '.file-row-name-input',
      ) as HTMLInputElement | null;
      if (!el) throw new Error('rename input not rendered');
      return el;
    });

    await fireEvent.blur(input);

    await waitFor(() => {
      const stillThere = container.querySelector('.file-row-name-input');
      if (stillThere) throw new Error('input should be gone after blur');
    });
    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'rename_content',
      expect.any(Object),
    );
  });

  // ── Empty-after-trim shows inline error, doesn't fire IPC ─────────

  it('empty-after-trim shows inline error and skips the IPC', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const { container, findByRole } = renderBrowser(service, {
      selectedSidecarId: 'mock-sidecar-4',
    });

    const browser = container.querySelector('.file-browser') as HTMLElement;
    await fireEvent.keyDown(browser, { key: 'F2' });

    const input = await waitFor(() => {
      const el = container.querySelector(
        '.file-row-name-input',
      ) as HTMLInputElement | null;
      if (!el) throw new Error('rename input not rendered');
      return el;
    });

    // Whitespace-only — empty after trim.
    await fireEvent.input(input, { target: { value: '   ' } });
    await fireEvent.keyDown(input, { key: 'Enter' });

    const banner = await findByRole('alert');
    expect(banner.textContent).toContain('Name cannot be empty');
    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'rename_content',
      expect.any(Object),
    );
  });

  // ── Same-name short-circuit ──────────────────────────────────────

  it('typing the same name + Enter cancels without firing the IPC', async () => {
    const { service, adapter } = await setupBrowserWithMockData();
    const { container } = renderBrowser(service, {
      selectedSidecarId: 'mock-sidecar-4',
    });

    const browser = container.querySelector('.file-browser') as HTMLElement;
    await fireEvent.keyDown(browser, { key: 'F2' });

    const input = await waitFor(() => {
      const el = container.querySelector(
        '.file-row-name-input',
      ) as HTMLInputElement | null;
      if (!el) throw new Error('rename input not rendered');
      return el;
    });

    // Submit unchanged value — exact same name as the item.
    expect(input.value).toBe('favorite-track.flac');
    await fireEvent.keyDown(input, { key: 'Enter' });

    await waitFor(() => {
      const stillThere = container.querySelector('.file-row-name-input');
      if (stillThere) throw new Error('input should be gone after same-name skip');
    });
    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'rename_content',
      expect.any(Object),
    );
  });

  // ── Round 5: blur during in-flight commit must NOT cancel ─────────

  it('blurring while the rename IPC is in flight keeps edit mode (error still retryable)', async () => {
    // Wire a manually-controlled deferred so the blur fires during the
    // await window. Without the in-flight guard, blur would cancel
    // edit mode and the catch's renameError would orphan a banner
    // with no input to retry in.
    const service = new FileManagerService();
    const { adapter } = createMockAdapter();
    let rejectRename!: (err: unknown) => void;
    const renamePending = new Promise((_resolve, reject) => {
      rejectRename = reject;
    });
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'list_content') return Promise.resolve([]);
      if (cmd === 'rename_content') return renamePending;
      return Promise.resolve(undefined);
    });
    await service.connectAdapter(adapter);
    (service as unknown as { privateContent: unknown[] }).privateContent = (
      await import('../../mock-file-data')
    ).mockPrivateContent.slice();

    const { container, findByRole } = renderBrowser(service, {
      selectedSidecarId: 'mock-sidecar-4',
    });

    const browser = container.querySelector('.file-browser') as HTMLElement;
    await fireEvent.keyDown(browser, { key: 'F2' });

    const input = await waitFor(() => {
      const el = container.querySelector(
        '.file-row-name-input',
      ) as HTMLInputElement | null;
      if (!el) throw new Error('rename input not rendered');
      return el;
    });

    await fireEvent.input(input, { target: { value: 'in-flight.flac' } });
    await fireEvent.keyDown(input, { key: 'Enter' });

    // Blur fires while the IPC is still awaiting — must be a no-op.
    await fireEvent.blur(input);

    // Now resolve the IPC with a rejection so the catch block runs.
    rejectRename(new Error('name already in use'));

    const banner = await findByRole('alert');
    expect(banner.textContent).toContain('name already in use');

    // Input must still be in the DOM — user has to be able to fix and retry.
    const stillThere = container.querySelector('.file-row-name-input');
    expect(stillThere).not.toBeNull();
  });
});
