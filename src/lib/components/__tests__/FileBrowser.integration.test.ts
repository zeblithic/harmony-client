/**
 * Integration test for the file manager subsystem.
 *
 * Tests the happy path end-to-end: browse files, select a file, view detail,
 * switch view modes, navigate into a folder, publish a file through
 * confirmation gates, and verify the service state changed.
 *
 * Uses a real FileManagerService with mock data (no vi.fn() stubs for the
 * service itself), mirroring how App.svelte wires things together.
 */
import { render, screen, fireEvent, cleanup } from '@testing-library/svelte';
import { describe, it, expect, vi, afterEach } from 'vitest';
import FileBrowser from '../FileBrowser.svelte';
import FileDetailPanel from '../FileDetailPanel.svelte';
import { FileManagerService } from '../../file-manager-service';
import type { ContentDetail, FileViewMode, ContentSection } from '../../types';

afterEach(cleanup);

describe('File Manager Integration', () => {
  // ── Helper: render FileBrowser with a real service ────────────────
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
        viewMode: 'list' as FileViewMode,
        section: 'private' as ContentSection,
        searchQuery: '',
        showCleanup: false,
        serviceVersion: 0,
        ...callbacks,
        ...overrides,
      },
    });

    return { ...result, callbacks };
  }

  // ── Helper: render FileDetailPanel with a content detail ──────────
  function renderDetail(detail: ContentDetail) {
    const callbacks = {
      onTierChange: vi.fn(),
      onPublish: vi.fn(),
      onRelease: vi.fn(),
      onBurn: vi.fn(),
      onArchive: vi.fn(),
      onPin: vi.fn(),
      onUnpin: vi.fn(),
      onExport: vi.fn(),
    };

    const result = render(FileDetailPanel, {
      props: {
        detail,
        ...callbacks,
      },
    });

    return { ...result, callbacks };
  }

  // ── 1. Browse files ──────────────────────────────────────────────

  it('lists root-level files from service on initial render', () => {
    const service = new FileManagerService();
    renderBrowser(service);

    // Root-level items from mock data
    expect(screen.getByText('Projects')).toBeTruthy();
    expect(screen.getByText('favorite-track.flac')).toBeTruthy();
    expect(screen.getByText('distributed-systems-lecture.mp4')).toBeTruthy();
    expect(screen.getByText('key-backup.enc')).toBeTruthy();
    expect(screen.getByText('sensor-readings.parquet')).toBeTruthy();
    expect(screen.getByText('harmony-client-v0.3.tar.gz')).toBeTruthy();
    expect(screen.getByText('family-reunion-2025.jpg')).toBeTruthy();

    // Items inside the Projects folder should NOT appear at root
    expect(screen.queryByText('mesh-design.md')).toBeNull();
    expect(screen.queryByText('architecture.svg')).toBeNull();
  });

  // ── 2. Click a file to select it ─────────────────────────────────

  it('fires onItemClick when a file row is clicked', async () => {
    const service = new FileManagerService();
    const { callbacks } = renderBrowser(service);

    const fileRow = screen.getByLabelText('favorite-track.flac');
    await fireEvent.click(fileRow);

    // ZEB-164: onItemClick now receives the full ContentItem, not just the cid.
    expect(callbacks.onItemClick).toHaveBeenCalledOnce();
    const arg = callbacks.onItemClick.mock.calls[0][0];
    expect(arg.cid).toBe('cid-song-favorite');
    expect(arg.sidecarId).toBe('mock-sidecar-4');
  });

  // ── 3. Click a folder to navigate ────────────────────────────────

  it('fires onNavigateFolder when a folder row is clicked', async () => {
    const service = new FileManagerService();
    const { callbacks } = renderBrowser(service);

    const folderRow = screen.getByLabelText('Projects');
    await fireEvent.click(folderRow);

    // Folders navigate, not select — onItemClick should NOT be called
    expect(callbacks.onItemClick).not.toHaveBeenCalled();
    expect(callbacks.onNavigateFolder).toHaveBeenCalledWith('cid-folder-projects');
  });

  // ── 4. Navigate into folder and see children ─────────────────────

  it('shows folder children when navigated into a folder', () => {
    const service = new FileManagerService();
    renderBrowser(service, { currentFolderCid: 'cid-folder-projects' });

    // Children of Projects folder
    expect(screen.getByText('mesh-design.md')).toBeTruthy();
    expect(screen.getByText('architecture.svg')).toBeTruthy();

    // Root-level items should NOT appear
    expect(screen.queryByText('favorite-track.flac')).toBeNull();
  });

  // ── 5. Breadcrumbs show folder path ──────────────────────────────

  it('shows breadcrumb trail when inside a folder', () => {
    const service = new FileManagerService();
    renderBrowser(service, { currentFolderCid: 'cid-folder-projects' });

    expect(screen.getByText('My Content')).toBeTruthy();
    // "Projects" appears in breadcrumbs
    const projectsElements = screen.getAllByText('Projects');
    expect(projectsElements.length).toBeGreaterThanOrEqual(1);
  });

  // ── 6. Switch view mode (list/grid) ──────────────────────────────

  it('fires onViewModeChange when grid view button is clicked', async () => {
    const service = new FileManagerService();
    const { callbacks } = renderBrowser(service);

    const gridBtn = screen.getByLabelText('Grid view');
    await fireEvent.click(gridBtn);

    expect(callbacks.onViewModeChange).toHaveBeenCalledWith('grid');
  });

  it('renders file cards in grid mode', () => {
    const service = new FileManagerService();
    const { container } = renderBrowser(service, { viewMode: 'grid' });

    // Grid mode renders FileCard components instead of FileRow
    const cards = container.querySelectorAll('.file-card');
    expect(cards.length).toBeGreaterThan(0);

    // Should NOT have file-row elements
    const rows = container.querySelectorAll('.file-row');
    expect(rows.length).toBe(0);
  });

  // ── 7. Search files ──────────────────────────────────────────────

  it('filters files by search query', () => {
    const service = new FileManagerService();
    renderBrowser(service, { searchQuery: 'lecture' });

    expect(screen.getByText('distributed-systems-lecture.mp4')).toBeTruthy();
    expect(screen.queryByText('favorite-track.flac')).toBeNull();
    expect(screen.queryByText('Projects')).toBeNull();
  });

  it('search matches a pasted CID (ZEB-612 S3)', () => {
    const service = new FileManagerService();
    renderBrowser(service, { searchQuery: 'cid-song-favorite' });

    expect(screen.getByText('favorite-track.flac')).toBeTruthy();
    expect(screen.queryByText('distributed-systems-lecture.mp4')).toBeNull();
  });

  it("search matches the UI's own chip text `cid:{shortCid}` (Greptile round 2)", () => {
    const service = new FileManagerService();
    // The row chip for cid-song-favorite renders `cid:cid-so…rite` —
    // pasting exactly that must find the file.
    renderBrowser(service, { searchQuery: 'cid:cid-so…rite' });

    expect(screen.getByText('favorite-track.flac')).toBeTruthy();
    expect(screen.queryByText('distributed-systems-lecture.mp4')).toBeNull();
  });

  // ── 8. Section switch (private/published) ────────────────────────

  it('fires onSectionChange when Published tab is clicked', async () => {
    const service = new FileManagerService();
    const { callbacks } = renderBrowser(service);

    const publishedBtn = screen.getByText('Published');
    await fireEvent.click(publishedBtn);

    expect(callbacks.onSectionChange).toHaveBeenCalledWith('published');
  });

  it('shows PublishedView when section is published', () => {
    const service = new FileManagerService();
    const { container } = renderBrowser(service, { section: 'published' });

    expect(container.querySelector('.published-view')).toBeTruthy();
  });

  // ── 9. Selected file detail panel ────────────────────────────────

  it('renders file detail panel with correct metadata', () => {
    const service = new FileManagerService();
    const detail = service.getContentDetail('cid-song-favorite')!;
    expect(detail).toBeDefined();

    renderDetail(detail);

    // File name
    expect(screen.getByText('favorite-track.flac')).toBeTruthy();

    // Sensitivity badge
    expect(screen.getByText('Public')).toBeTruthy();

    // Replication info (5 seen vs high-tier target 5; ZEB-612 S3 copy)
    expect(screen.getByText('×5 · copies seen (this device + peers)')).toBeTruthy();

    // Action buttons
    expect(screen.getByLabelText('Publish (permanent)')).toBeTruthy();
    expect(screen.getByLabelText('Release (ephemeral)')).toBeTruthy();
    expect(screen.getByLabelText('Burn')).toBeTruthy();
    expect(screen.getByLabelText('Export')).toBeTruthy();
  });

  // ── 10. Pin/Unpin toggle ─────────────────────────────────────────

  it('shows Unpin for pinned files, Pin for unpinned files', () => {
    const service = new FileManagerService();

    // favorite-track.flac is pinned
    const pinnedDetail = service.getContentDetail('cid-song-favorite')!;
    const { unmount: u1 } = renderDetail(pinnedDetail);
    expect(screen.getByLabelText('Unpin')).toBeTruthy();
    u1();

    // distributed-systems-lecture.mp4 is NOT pinned
    const unpinnedDetail = service.getContentDetail('cid-video-lecture')!;
    renderDetail(unpinnedDetail);
    expect(screen.getByLabelText('Pin')).toBeTruthy();
  });

  // ── 11. Full publish flow through confirmation gates ─────────────

  describe('publish flow through confirmation gates', () => {
    it('publishes a public file with double confirm and moves it to published catalog', async () => {
      const service = new FileManagerService();

      // Start with the software bundle (public sensitivity = double confirm)
      const detail = service.getContentDetail('cid-app-build')!;
      expect(detail).toBeDefined();
      expect(detail.sensitivity).toBe('public');

      // Record initial published count
      const publishedBefore = service.getPublishedContent().length;

      // Render the detail panel with a real publish handler
      const onPublish = vi.fn((cid: string) => {
        service.publish([cid]);
      });

      render(FileDetailPanel, {
        props: {
          detail,
          onTierChange: vi.fn(),
          onPublish,
          onRelease: vi.fn(),
          onBurn: vi.fn(),
          onArchive: vi.fn(),
          onPin: vi.fn(),
          onUnpin: vi.fn(),
          onExport: vi.fn(),
        },
      });

      // Step 1: Click Publish button to start flow
      await fireEvent.click(screen.getByLabelText('Publish (permanent)'));

      // DoubleConfirmDialog should appear
      expect(screen.getByRole('dialog')).toBeTruthy();
      expect(
        screen.getByText(/Publishing makes this content permanently public/),
      ).toBeTruthy();

      // Step 2: Gate 1 — Continue
      await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));

      // Gate 2 — second message should show filename
      expect(
        screen.getByText(/You are about to publish harmony-client-v0\.3\.tar\.gz/),
      ).toBeTruthy();

      // Step 3: Gate 2 — Confirm Publish
      await fireEvent.click(
        screen.getByRole('button', { name: /Confirm Publish/i }),
      );

      // onPublish callback should have fired
      expect(onPublish).toHaveBeenCalledWith('cid-app-build');

      // Verify service state: item moved from private to published
      const publishedAfter = service.getPublishedContent();
      expect(publishedAfter.length).toBe(publishedBefore + 1);

      const publishedItem = publishedAfter.find((p) => p.cid === 'cid-app-build');
      expect(publishedItem).toBeDefined();
      expect(publishedItem!.name).toBe('harmony-client-v0.3.tar.gz');
      expect(publishedItem!.publishMode).toBe('durable');

      // Item should no longer be in private content
      const privateAfter = service.getContents();
      const stillPrivate = privateAfter.find((i) => i.cid === 'cid-app-build');
      expect(stillPrivate).toBeUndefined();
    });

    it('releases a file with ephemeral mode via double confirm', async () => {
      const service = new FileManagerService();
      const detail = service.getContentDetail('cid-video-lecture')!;

      const publishedBefore = service.getPublishedContent().length;

      const onRelease = vi.fn((cid: string) => {
        service.release([cid]);
      });

      render(FileDetailPanel, {
        props: {
          detail,
          onTierChange: vi.fn(),
          onPublish: vi.fn(),
          onRelease,
          onBurn: vi.fn(),
          onArchive: vi.fn(),
          onPin: vi.fn(),
          onUnpin: vi.fn(),
          onExport: vi.fn(),
        },
      });

      // Click Release button
      await fireEvent.click(screen.getByLabelText('Release (ephemeral)'));

      // DoubleConfirmDialog with release messaging
      expect(screen.getByRole('dialog')).toBeTruthy();
      expect(
        screen.getByText(/publicly available.*persist on the network or fade/i),
      ).toBeTruthy();

      // Gate 1: Continue
      await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));

      // Gate 2: Confirm Release
      await fireEvent.click(
        screen.getByRole('button', { name: /Confirm Release/i }),
      );

      expect(onRelease).toHaveBeenCalledWith('cid-video-lecture');

      // Verify service state
      const publishedAfter = service.getPublishedContent();
      expect(publishedAfter.length).toBe(publishedBefore + 1);

      const released = publishedAfter.find((p) => p.cid === 'cid-video-lecture');
      expect(released).toBeDefined();
      expect(released!.publishMode).toBe('ephemeral');
    });

    it('publishes an intimate file with triple confirm (double + type-to-confirm)', async () => {
      const service = new FileManagerService();

      // family-reunion-2025.jpg is intimate sensitivity
      const detail = service.getContentDetail('cid-family-photo')!;
      expect(detail.sensitivity).toBe('intimate');

      const publishedBefore = service.getPublishedContent().length;

      const onPublish = vi.fn((cid: string) => {
        service.publish([cid]);
      });

      render(FileDetailPanel, {
        props: {
          detail,
          onTierChange: vi.fn(),
          onPublish,
          onRelease: vi.fn(),
          onBurn: vi.fn(),
          onArchive: vi.fn(),
          onPin: vi.fn(),
          onUnpin: vi.fn(),
          onExport: vi.fn(),
        },
      });

      // Start publish flow
      await fireEvent.click(screen.getByLabelText('Publish (permanent)'));

      // Double confirm gates
      await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
      await fireEvent.click(
        screen.getByRole('button', { name: /Confirm Publish/i }),
      );

      // TypeToConfirmDialog should appear
      expect(screen.getByRole('dialog')).toBeTruthy();
      expect(screen.getByLabelText('Type to confirm')).toBeTruthy();

      // Type wrong text first — button should be disabled
      const input = screen.getByLabelText('Type to confirm');
      await fireEvent.input(input, { target: { value: 'wrong' } });
      const confirmBtn = screen.getByRole('button', { name: /Confirm Publish/i });
      expect(confirmBtn.hasAttribute('disabled')).toBe(true);

      // Type correct filename
      await fireEvent.input(input, {
        target: { value: 'family-reunion-2025.jpg' },
      });

      // Now confirm should work
      await fireEvent.click(
        screen.getByRole('button', { name: /Confirm Publish/i }),
      );

      expect(onPublish).toHaveBeenCalledWith('cid-family-photo');

      // Verify state changed
      const publishedAfter = service.getPublishedContent();
      expect(publishedAfter.length).toBe(publishedBefore + 1);
      expect(publishedAfter.find((p) => p.cid === 'cid-family-photo')).toBeDefined();
    });

    it('publishes a confidential file through all 4 gates (double + type + OOB)', async () => {
      const service = new FileManagerService();

      // key-backup.enc is confidential sensitivity
      const detail = service.getContentDetail('cid-private-keys-backup')!;
      expect(detail.sensitivity).toBe('confidential');

      const publishedBefore = service.getPublishedContent().length;

      const onPublish = vi.fn((cid: string) => {
        service.publish([cid]);
      });

      render(FileDetailPanel, {
        props: {
          detail,
          onTierChange: vi.fn(),
          onPublish,
          onRelease: vi.fn(),
          onBurn: vi.fn(),
          onArchive: vi.fn(),
          onPin: vi.fn(),
          onUnpin: vi.fn(),
          onExport: vi.fn(),
        },
      });

      // Gate 1: Start publish
      await fireEvent.click(screen.getByLabelText('Publish (permanent)'));

      // Gate 2: Double confirm - Continue
      await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));

      // Gate 3: Double confirm - Confirm Publish
      await fireEvent.click(
        screen.getByRole('button', { name: /Confirm Publish/i }),
      );

      // Gate 4: Type to confirm
      const input = screen.getByLabelText('Type to confirm');
      await fireEvent.input(input, { target: { value: 'key-backup.enc' } });
      await fireEvent.click(
        screen.getByRole('button', { name: /Confirm Publish/i }),
      );

      // Gate 5: OOB stub
      expect(screen.getByRole('dialog')).toBeTruthy();
      expect(screen.getByText(/future version/i)).toBeTruthy();
      expect(onPublish).not.toHaveBeenCalled();

      await fireEvent.click(screen.getByRole('button', { name: /Confirm/i }));

      expect(onPublish).toHaveBeenCalledWith('cid-private-keys-backup');

      // Verify state
      const publishedAfter = service.getPublishedContent();
      expect(publishedAfter.length).toBe(publishedBefore + 1);
      expect(
        publishedAfter.find((p) => p.cid === 'cid-private-keys-backup'),
      ).toBeDefined();
    });
  });

  // ── 12. Burn removes from private content ────────────────────────

  it('burn removes a file from service', () => {
    const service = new FileManagerService();
    const privateBefore = service.getContents().length;

    service.burn(['mock-sidecar-7']);

    const privateAfter = service.getContents();
    expect(privateAfter.length).toBe(privateBefore - 1);
    expect(privateAfter.find((i) => i.cid === 'cid-training-data')).toBeUndefined();
  });

  // ── 13. Quota updates after operations ───────────────────────────

  it('quota decreases after burning a file', () => {
    const service = new FileManagerService();
    const quotaBefore = service.getQuotaStatus().usedBytes;

    service.burn(['mock-sidecar-5']);

    const quotaAfter = service.getQuotaStatus().usedBytes;
    expect(quotaAfter).toBeLessThan(quotaBefore);
    expect(quotaBefore - quotaAfter).toBe(1_500_000_000);
  });

  it('quota decreases after publishing (moved out of private)', () => {
    const service = new FileManagerService();
    const quotaBefore = service.getQuotaStatus().usedBytes;

    service.publish(['cid-song-favorite']);

    const quotaAfter = service.getQuotaStatus().usedBytes;
    expect(quotaAfter).toBeLessThan(quotaBefore);
    expect(quotaBefore - quotaAfter).toBe(35_000_000);
  });

  // ── 14. Accessibility checks ─────────────────────────────────────

  describe('accessibility', () => {
    it('file list has table role and column headers', () => {
      const service = new FileManagerService();
      const { container } = renderBrowser(service);

      const table = container.querySelector('[role="table"]');
      expect(table).toBeTruthy();
      expect(table!.getAttribute('aria-label')).toBe('File list');

      // Column headers
      const headers = container.querySelectorAll('[role="columnheader"]');
      expect(headers.length).toBeGreaterThanOrEqual(4); // Name, Size, Last Accessed, Replicas
    });

    it('file rows have row role and aria-label', () => {
      const service = new FileManagerService();
      const { container } = renderBrowser(service);

      const rows = container.querySelectorAll('[role="row"]');
      // At least header row + file rows
      expect(rows.length).toBeGreaterThan(1);

      // Each file row button has aria-label
      const fileRows = container.querySelectorAll('button.file-row');
      fileRows.forEach((row) => {
        expect(row.getAttribute('aria-label')).toBeTruthy();
      });
    });

    it('breadcrumbs have navigation landmark', () => {
      const service = new FileManagerService();
      const { container } = renderBrowser(service);

      const nav = container.querySelector('nav.breadcrumbs');
      expect(nav).toBeTruthy();
      expect(nav!.getAttribute('aria-label')).toBe('File navigation');
    });

    it('quota bar button has descriptive aria-label', () => {
      const service = new FileManagerService();
      renderBrowser(service);

      const buttons = screen.getAllByRole('button');
      const quotaBtn = buttons.find(b => b.getAttribute('aria-label')?.includes('Storage:'));
      expect(quotaBtn).toBeTruthy();
      // ZEB-612 S3: no invented total — the label reports real usage only.
      expect(quotaBtn!.getAttribute('aria-label')).toMatch(/\d+.*stored locally/);
    });

    it('section toggle buttons have aria-pressed', () => {
      const service = new FileManagerService();
      renderBrowser(service);

      const privateBtn = screen.getByText('Private');
      const publishedBtn = screen.getByText('Published');

      expect(privateBtn.getAttribute('aria-pressed')).toBe('true');
      expect(publishedBtn.getAttribute('aria-pressed')).toBe('false');
    });

    it('view mode buttons have aria-pressed', () => {
      const service = new FileManagerService();
      renderBrowser(service);

      const listBtn = screen.getByLabelText('List view');
      const gridBtn = screen.getByLabelText('Grid view');

      expect(listBtn.getAttribute('aria-pressed')).toBe('true');
      expect(gridBtn.getAttribute('aria-pressed')).toBe('false');
    });

    it('search input has aria-label', () => {
      const service = new FileManagerService();
      renderBrowser(service);

      const searchInput = screen.getByLabelText('Search files');
      expect(searchInput).toBeTruthy();
      expect(searchInput.tagName).toBe('INPUT');
    });

    it('file detail panel is an aside with aria-label', () => {
      const service = new FileManagerService();
      const detail = service.getContentDetail('cid-song-favorite')!;
      const { container } = renderDetail(detail);

      const aside = container.querySelector('aside');
      expect(aside).toBeTruthy();
      expect(aside!.getAttribute('aria-label')).toBe('File details');
    });

    it('confirmation dialogs have role="dialog" and aria-modal', async () => {
      const service = new FileManagerService();
      const detail = service.getContentDetail('cid-app-build')!;

      render(FileDetailPanel, {
        props: {
          detail,
          onTierChange: vi.fn(),
          onPublish: vi.fn(),
          onRelease: vi.fn(),
          onBurn: vi.fn(),
          onArchive: vi.fn(),
          onPin: vi.fn(),
          onUnpin: vi.fn(),
          onExport: vi.fn(),
        },
      });

      await fireEvent.click(screen.getByLabelText('Publish (permanent)'));

      const dialog = screen.getByRole('dialog');
      expect(dialog).toBeTruthy();
      expect(dialog.getAttribute('aria-modal')).toBe('true');
      expect(dialog.getAttribute('aria-labelledby')).toBeTruthy();
    });

    it('replication tier select has aria-label', () => {
      const service = new FileManagerService();
      const detail = service.getContentDetail('cid-song-favorite')!;
      renderDetail(detail);

      const select = screen.getByLabelText('Replication tier');
      expect(select).toBeTruthy();
      expect(select.tagName).toBe('SELECT');
    });

    it('all action buttons use native button elements', () => {
      const service = new FileManagerService();
      const detail = service.getContentDetail('cid-song-favorite')!;
      renderDetail(detail);

      const buttons = screen.getAllByRole('button');
      buttons.forEach((btn) => {
        expect(btn.tagName).toBe('BUTTON');
      });
    });

    it('file cards in grid mode have aria-label', () => {
      const service = new FileManagerService();
      const { container } = renderBrowser(service, { viewMode: 'grid' });

      const cards = container.querySelectorAll('button.file-card');
      expect(cards.length).toBeGreaterThan(0);
      cards.forEach((card) => {
        expect(card.getAttribute('aria-label')).toBeTruthy();
      });
    });

    it('mock share and buddy lists are gone from the detail panel (ZEB-612 S3 → ZEB-669)', () => {
      const service = new FileManagerService();
      const detail = service.getContentDetail('cid-song-favorite')!;
      const { container } = renderDetail(detail);

      expect(container.querySelector('[aria-label="Shared with (can view)"]')).toBeNull();
      expect(container.querySelector('[aria-label="Stored by (encrypted)"]')).toBeNull();
    });
  });
});
