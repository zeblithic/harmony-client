import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import LibraryDirectoryBrowser from '../LibraryDirectoryBrowser.svelte';
import type { LibraryDirectoryService, LibraryInfo, DirectoryEntry } from '../../library-directory-service';
import type { TauriAdapter } from '../../zenoh-service';

function mockService(
  list: LibraryInfo[],
  browse: DirectoryEntry[] = [],
): LibraryDirectoryService {
  return {
    list: vi.fn().mockResolvedValue(list),
    browse: vi.fn().mockResolvedValue(browse),
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

  it('Join calls onJoin with invite_url', async () => {
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
      expect(onJoin).toHaveBeenCalledWith('harmony://invite/?p=AAAA');
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
    await new Promise((r) => setTimeout(r, 250));
    // Initial + ONE debounced refetch = 2 calls.
    expect(svc.list).toHaveBeenCalledTimes(2);
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
});
