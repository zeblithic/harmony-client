import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import LibraryDirectoryBrowser from '../LibraryDirectoryBrowser.svelte';
import type {
  LibraryDirectoryService,
  LibraryInfo,
  DirectoryEntry,
  DiscoveredLibraryInfo,
} from '../../library-directory-service';
import type { TauriAdapter } from '../../zenoh-service';

function mockService(
  list: LibraryInfo[],
  browse: DirectoryEntry[] = [],
  discovered: DiscoveredLibraryInfo[] = [],
): LibraryDirectoryService {
  return {
    list: vi.fn().mockResolvedValue(list),
    browse: vi.fn().mockResolvedValue(browse),
    listDiscovered: vi.fn().mockResolvedValue(discovered),
    add: vi.fn().mockResolvedValue(undefined),
    remove: vi.fn().mockResolvedValue(undefined),
  } as unknown as LibraryDirectoryService;
}

function mockAdapter(): TauriAdapter {
  return {
    invoke: vi.fn(),
    listen: vi.fn(async () => () => {}),
  } as unknown as TauriAdapter;
}

const fixtureEntry: DirectoryEntry = {
  community_id: '11111111111111111111111111111111',
  community_addr: '22222222222222222222222222222222',
  name: 'Test Community',
  description: 'A fixture',
  topics: ['test'],
  invite_url: 'harmony://invite/?p=AAAA',
  listed_by_count: 1,
  unattested: false,
  listed_at: { w: 0, l: 0, d: 'd' },
};

describe('LibraryDirectoryBrowser', () => {
  it('empty state shows CTA when no libraries', async () => {
    const { findByText } = render(LibraryDirectoryBrowser, {
      props: {
        service: mockService([]),
        adapter: mockAdapter(),
        onJoin: vi.fn(),
        onClose: vi.fn(),
      },
    });
    expect(await findByText(/Add a library to start browsing/i)).toBeInTheDocument();
  });

  it('paste-and-add flow calls service.add', async () => {
    const svc = mockService([]);
    const { findByText, getByPlaceholderText } = render(LibraryDirectoryBrowser, {
      props: { service: svc, adapter: mockAdapter(), onJoin: vi.fn(), onClose: vi.fn() },
    });
    await fireEvent.click(await findByText(/\+ Add a library/i));
    const input = getByPlaceholderText(/32-character/i);
    await fireEvent.input(input, { target: { value: 'aabbccddeeff00112233445566778899' } });
    await fireEvent.click(await findByText(/Add library/));
    await waitFor(() => {
      expect(svc.add).toHaveBeenCalledWith('aabbccddeeff00112233445566778899');
    });
  });

  it('with libraries: browse list renders entries', async () => {
    const svc = mockService(
      [{ address: 'aabbccddeeff00112233445566778899', added_at: { w: 0, l: 0, d: 'd' }, entry_count: 1 }],
      [fixtureEntry],
    );
    const { findByText } = render(LibraryDirectoryBrowser, {
      props: { service: svc, adapter: mockAdapter(), onJoin: vi.fn(), onClose: vi.fn() },
    });
    expect(await findByText(/Test Community/)).toBeInTheDocument();
    expect(await findByText(/Listed by 1 library/)).toBeInTheDocument();
  });

  it('Join calls onJoin with community_id (ZEB-252 Phase 6)', async () => {
    const onJoin = vi.fn().mockResolvedValue(undefined);
    const onClose = vi.fn();
    const svc = mockService(
      [{ address: 'aabbccddeeff00112233445566778899', added_at: { w: 0, l: 0, d: 'd' }, entry_count: 1 }],
      [fixtureEntry],
    );
    const { findByText } = render(LibraryDirectoryBrowser, {
      props: { service: svc, adapter: mockAdapter(), onJoin, onClose },
    });
    await fireEvent.click(await findByText(/Join/));
    await waitFor(() => {
      // ZEB-252 Sub-D Phase 6: onJoin now receives community_id, not invite_url.
      // App wires this to communityService.joinOpenCommunity(communityId).
      expect(onJoin).toHaveBeenCalledWith('11111111111111111111111111111111');
      expect(onJoin).not.toHaveBeenCalledWith(expect.stringMatching(/^harmony:/));
      expect(onClose).toHaveBeenCalled();
    });
  });

  it('remove library chip calls service.remove', async () => {
    const svc = mockService(
      [{ address: 'aabbccddeeff00112233445566778899', added_at: { w: 0, l: 0, d: 'd' }, entry_count: 0 }],
      [],
    );
    const { findByLabelText } = render(LibraryDirectoryBrowser, {
      props: { service: svc, adapter: mockAdapter(), onJoin: vi.fn(), onClose: vi.fn() },
    });
    await fireEvent.click(await findByLabelText(/Remove library aabbccdd/));
    await waitFor(() => {
      expect(svc.remove).toHaveBeenCalledWith('aabbccddeeff00112233445566778899');
    });
  });

  it('library-directory-updated event triggers debounced refetch', async () => {
    const svc = mockService(
      [{ address: 'aabbccddeeff00112233445566778899', added_at: { w: 0, l: 0, d: 'd' }, entry_count: 0 }],
      [],
    );
    const listenerBox: { fn: (() => void) | null } = { fn: null };
    const adapter = {
      invoke: vi.fn(),
      listen: vi.fn(async (event: string, cb: () => void) => {
        if (event === 'library-directory-updated') listenerBox.fn = cb;
        return () => {};
      }),
    } as unknown as TauriAdapter;
    render(LibraryDirectoryBrowser, {
      props: { service: svc, adapter, onJoin: vi.fn(), onClose: vi.fn() },
    });
    await waitFor(() => expect(svc.list).toHaveBeenCalledTimes(1));
    // Wait for the async `listen` promise to resolve and the listener
    // to be wired up. (The browser stashes the unsubscribe handle in a
    // `.then` callback that races with the synchronous test path.)
    await waitFor(() => expect(listenerBox.fn).not.toBeNull());
    listenerBox.fn?.();
    listenerBox.fn?.(); // multiple events within debounce window — still one refetch
    // R2 F3 (CodeRabbit): use `waitFor` for a condition-based wait rather
    // than a fixed 250ms sleep so the test doesn't break if the debounce
    // window changes. `waitFor` polls until the assertion passes or times
    // out, so it tolerates any debounce ≤ the default timeout (1000ms).
    await waitFor(() => {
      // Initial + ONE debounced refetch = 2 calls.
      expect(svc.list).toHaveBeenCalledTimes(2);
    });
  });

  // R2 F4: previously the remove-library handler only console.warn'd on
  // service.remove failure — the user clicked ✕ and saw nothing happen.
  // Now the error surfaces inline next to the libraries bar.
  it('remove library failure surfaces inline error', async () => {
    const svc = {
      list: vi.fn().mockResolvedValue([
        { address: 'aabbccddeeff00112233445566778899', added_at: { w: 0, l: 0, d: 'd' }, entry_count: 0 },
      ]),
      browse: vi.fn().mockResolvedValue([]),
      listDiscovered: vi.fn().mockResolvedValue([]),
      add: vi.fn(),
      remove: vi.fn().mockRejectedValue(new Error('test error')),
    } as unknown as LibraryDirectoryService;
    const { findByLabelText, findByText } = render(LibraryDirectoryBrowser, {
      props: { service: svc, adapter: mockAdapter(), onJoin: vi.fn(), onClose: vi.fn() },
    });
    await fireEvent.click(await findByLabelText(/Remove library aabbccdd/));
    expect(await findByText(/Could not remove library: test error/i)).toBeInTheDocument();
  });

  it('add-library error is surfaced inline', async () => {
    const svc = {
      list: vi.fn().mockResolvedValue([]),
      browse: vi.fn().mockResolvedValue([]),
      listDiscovered: vi.fn().mockResolvedValue([]),
      add: vi.fn().mockRejectedValue(new Error('expected 16 bytes, got 8')),
      remove: vi.fn(),
    } as unknown as LibraryDirectoryService;
    const { findByText, getByPlaceholderText } = render(LibraryDirectoryBrowser, {
      props: { service: svc, adapter: mockAdapter(), onJoin: vi.fn(), onClose: vi.fn() },
    });
    await fireEvent.click(await findByText(/\+ Add a library/i));
    const input = getByPlaceholderText(/32-character/i);
    await fireEvent.input(input, { target: { value: 'aabbccddeeff00112233445566778899' } });
    await fireEvent.click(await findByText(/Add library/));
    expect(await findByText(/expected 16 bytes/i)).toBeInTheDocument();
  });

  // ZEB-279 Sub-D Phase 2: discovered libraries panel.
  describe('Discovered libraries panel', () => {
    it('discovered_panel_renders_with_n_entries', async () => {
      const svc = mockService([], [], [
        { libraryAddr: '11'.padEnd(32, '0'), name: 'LibA', description: 'desc A', listedAt: '0' },
        { libraryAddr: '22'.padEnd(32, '0'), name: 'LibB', description: 'desc B', listedAt: '0' },
        { libraryAddr: '33'.padEnd(32, '0'), name: 'LibC', description: 'desc C', listedAt: '0' },
      ]);
      const { findByText } = render(LibraryDirectoryBrowser, {
        props: { service: svc, adapter: mockAdapter(), onJoin: vi.fn(), onClose: vi.fn() },
      });
      // Header renders with the correct count and auto-expanded (N>0)
      // so the row names are visible.
      expect(await findByText(/Discovered libraries \(3\)/)).toBeInTheDocument();
      expect(await findByText('LibA')).toBeInTheDocument();
      expect(await findByText('LibB')).toBeInTheDocument();
      expect(await findByText('LibC')).toBeInTheDocument();
    });

    it('discovered_section_collapsed_when_empty', async () => {
      const svc = mockService([], [], []);
      const { findByText, queryByText } = render(LibraryDirectoryBrowser, {
        props: { service: svc, adapter: mockAdapter(), onJoin: vi.fn(), onClose: vi.fn() },
      });
      // Header renders "(0)" but rows are not visible (section collapsed).
      expect(await findByText(/Discovered libraries \(0\)/)).toBeInTheDocument();
      // Sanity: no discovered row content rendered. We can't query for
      // a specific row name here (there are none) — instead assert that
      // the toggle indicator shows the collapsed glyph (▶) rather than
      // the expanded glyph (▼).
      expect(queryByText(/▼ Discovered libraries/)).toBeNull();
      expect(await findByText(/▶ Discovered libraries/)).toBeInTheDocument();
    });

    it('click_add_invokes_service_add_with_correct_addr', async () => {
      const libAddr = 'abcd'.padEnd(32, '0');
      // R3 F4 (CodeRabbit Trivial): also assert the user-visible effect
      // — the row is removed from "Discovered libraries" via the IPC
      // filter (backend excludes already-added addrs from
      // list_discovered_libraries). Mock listDiscovered with successive
      // values so the post-add refresh observes the filtered result.
      const listDiscovered = vi
        .fn()
        .mockResolvedValueOnce([
          { libraryAddr: libAddr, name: 'LibX', description: 'd', listedAt: '0' },
        ])
        .mockResolvedValue([]); // post-add refresh: filtered out
      const svc = {
        list: vi.fn().mockResolvedValue([]),
        browse: vi.fn().mockResolvedValue([]),
        listDiscovered,
        add: vi.fn().mockResolvedValue(undefined),
        remove: vi.fn(),
      } as unknown as LibraryDirectoryService;
      const { findByRole, queryByText, findByText } = render(LibraryDirectoryBrowser, {
        props: { service: svc, adapter: mockAdapter(), onJoin: vi.fn(), onClose: vi.fn() },
      });
      // F5: each discovered Add button now carries a unique aria-label
      // including the library name, so screen readers (and tests) can
      // distinguish rows.
      const discoveredAddBtn = await findByRole('button', { name: /Add LibX/i });
      await fireEvent.click(discoveredAddBtn);
      await waitFor(() => {
        expect(svc.add).toHaveBeenCalledWith(libAddr);
      });
      // After add succeeds, refresh() refetches listDiscovered which
      // now returns []. The LibX row should be gone from the DOM.
      await waitFor(() => {
        expect(listDiscovered).toHaveBeenCalledTimes(2);
      });
      // Section header now shows (0) and the row name is no longer
      // rendered anywhere.
      expect(await findByText(/Discovered libraries \(0\)/)).toBeInTheDocument();
      expect(queryByText('LibX')).toBeNull();
    });

    // F5 (CodeAnt + CodeRabbit Major Logic): the discovered panel
    // auto-expands once on first non-empty observation, then never
    // re-opens after the user manually toggles it. Previously, every
    // refresh re-applied `if (n>0 && !open) open = true`, so manual
    // collapse was non-sticky.
    it('discovered_panel_stays_collapsed_after_user_collapses', async () => {
      const listenerBox: { fn: (() => void) | null } = { fn: null };
      const adapter = {
        invoke: vi.fn(),
        listen: vi.fn(async (event: string, cb: () => void) => {
          if (event === 'library-directory-updated') listenerBox.fn = cb;
          return () => {};
        }),
      } as unknown as TauriAdapter;

      // Drive the discovered list via successive resolves: start empty,
      // then non-empty (auto-opens), then non-empty again after user
      // collapsed (must stay collapsed).
      const discoveredA = [
        { libraryAddr: 'aa'.padEnd(32, '0'), name: 'LibA', description: '', listedAt: '0' },
      ];
      const listDiscovered = vi
        .fn()
        .mockResolvedValueOnce([]) // initial refresh: empty → stays collapsed
        .mockResolvedValueOnce(discoveredA) // 2nd refresh: non-empty → auto-opens
        .mockResolvedValueOnce(discoveredA); // 3rd refresh: still non-empty → must stay closed after manual collapse
      const svc = {
        list: vi.fn().mockResolvedValue([]),
        browse: vi.fn().mockResolvedValue([]),
        listDiscovered,
        add: vi.fn(),
        remove: vi.fn(),
      } as unknown as LibraryDirectoryService;

      const { findByText, queryByText } = render(LibraryDirectoryBrowser, {
        props: { service: svc, adapter, onJoin: vi.fn(), onClose: vi.fn() },
      });

      // First render: initial refresh returns []. Section is collapsed.
      expect(await findByText(/▶ Discovered libraries \(0\)/)).toBeInTheDocument();
      await waitFor(() => expect(listenerBox.fn).not.toBeNull());

      // Trigger the second refresh via the listener — list now has 1
      // entry, auto-expand fires (▼ glyph).
      listenerBox.fn?.();
      // R2 F3 (CodeRabbit): replace the fixed 250ms sleep with
      // `waitFor` so the test doesn't break if the debounce window
      // changes. `findByText` already polls, so the subsequent assert
      // is naturally condition-based — but we explicitly wait for the
      // second listDiscovered call to land first to make the intent
      // (debounce flushed → second refresh observed) explicit.
      await waitFor(() => {
        expect(listDiscovered).toHaveBeenCalledTimes(2);
      });
      expect(await findByText(/▼ Discovered libraries \(1\)/)).toBeInTheDocument();

      // User manually collapses the panel by clicking the toggle.
      await fireEvent.click(await findByText(/▼ Discovered libraries \(1\)/));
      expect(await findByText(/▶ Discovered libraries \(1\)/)).toBeInTheDocument();

      // Trigger another refresh — discovered is STILL non-empty, but
      // because the user manually toggled, auto-expand must NOT fire.
      listenerBox.fn?.();
      await waitFor(() => {
        expect(listDiscovered).toHaveBeenCalledTimes(3);
      });
      // Still collapsed: ▶ glyph, not ▼.
      expect(await findByText(/▶ Discovered libraries \(1\)/)).toBeInTheDocument();
      expect(queryByText(/▼ Discovered libraries/)).toBeNull();
    });

    it('add_failure_surfaces_inline', async () => {
      const libAddr = 'fail'.padEnd(32, '0');
      const svc = {
        list: vi.fn().mockResolvedValue([]),
        browse: vi.fn().mockResolvedValue([]),
        listDiscovered: vi.fn().mockResolvedValue([
          { libraryAddr: libAddr, name: 'BadLib', description: '', listedAt: '0' },
        ]),
        add: vi.fn().mockRejectedValue(new Error('library add failed')),
        remove: vi.fn(),
      } as unknown as LibraryDirectoryService;
      const { findAllByText, findByText } = render(LibraryDirectoryBrowser, {
        props: { service: svc, adapter: mockAdapter(), onJoin: vi.fn(), onClose: vi.fn() },
      });
      const addButtons = await findAllByText(/^Add$/);
      const discoveredAddBtn = addButtons.find(
        (b) => b.closest('.discovered-row') !== null,
      );
      expect(discoveredAddBtn).toBeDefined();
      await fireEvent.click(discoveredAddBtn!);
      // Inline error shows next to the row.
      expect(await findByText(/library add failed/)).toBeInTheDocument();
    });
  });

  // ZEB-280 Sub-D Phase 3: inline ⚠ Unattested badge. The badge renders
  // when entry.unattested === true (Rust: !unattested_by.is_empty()).
  // Amber (NOT red) styling — admin sig is still the content trust
  // anchor; only the wrapping (transport-integrity) sig is compromised.
  it('unattested_badge_renders_when_dto_unattested_true', async () => {
    const mockEntry: DirectoryEntry = {
      community_id: 'cc'.repeat(16),
      community_addr: 'dd'.repeat(16),
      name: 'Test Community',
      description: 'A community for testing.',
      topics: ['test'],
      invite_url: 'harmony://invite/?p=AAAA',
      listed_by_count: 1,
      unattested: true, // ← drives badge
      listed_at: { w: 0, l: 0, d: 'd' },
    };
    const svc = mockService(
      [{ address: 'aa'.repeat(16), added_at: { w: 0, l: 0, d: 'd' }, entry_count: 1 }],
      [mockEntry],
    );
    const { container } = render(LibraryDirectoryBrowser, {
      props: { service: svc, adapter: mockAdapter(), onJoin: vi.fn(), onClose: vi.fn() },
    });

    // Wait for the entries to load (refresh runs in $effect).
    await waitFor(() => {
      expect(container.querySelector('.unattested-badge')).toBeTruthy();
    });

    const badge = container.querySelector('.unattested-badge')!;
    expect(badge.textContent).toContain('Unattested');
    expect(badge.getAttribute('aria-label')).toContain('wrapping signature');
    expect(badge.getAttribute('title')).toContain('admin');
  });

  it('unattested_badge_absent_when_dto_unattested_false', async () => {
    const mockEntry: DirectoryEntry = {
      community_id: 'cc'.repeat(16),
      community_addr: 'dd'.repeat(16),
      name: 'Test Community',
      description: 'A community for testing.',
      topics: ['test'],
      invite_url: 'harmony://invite/?p=AAAA',
      listed_by_count: 1,
      unattested: false,
      listed_at: { w: 0, l: 0, d: 'd' },
    };
    const svc = mockService(
      [{ address: 'aa'.repeat(16), added_at: { w: 0, l: 0, d: 'd' }, entry_count: 1 }],
      [mockEntry],
    );
    const { container } = render(LibraryDirectoryBrowser, {
      props: { service: svc, adapter: mockAdapter(), onJoin: vi.fn(), onClose: vi.fn() },
    });

    // Wait for entries to load THEN assert badge is NOT present.
    await waitFor(() => {
      expect(container.textContent).toContain('Test Community');
    });

    expect(container.querySelector('.unattested-badge')).toBeNull();
  });
});
