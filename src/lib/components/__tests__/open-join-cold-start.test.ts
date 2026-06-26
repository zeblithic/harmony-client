import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import LibraryDirectoryBrowser from '../LibraryDirectoryBrowser.svelte';
import type {
  LibraryDirectoryService,
  LibraryInfo,
  DirectoryEntry,
  DiscoveredLibraryInfo,
} from '../../library-directory-service';
import type { TauriAdapter } from '../../zenoh-service';
import type { RedeemInviteResultDto } from '../../community-service';

/**
 * Open-community cross-WAN first-contact: the `connectivity_open_join_iroh`
 * IPC returns a `RedeemInviteResultDto` with an optional `status` field that
 * drives the join UI. This suite asserts the status → UI mapping in
 * `LibraryDirectoryBrowser` (the open-community join flow):
 *   - "searching" → non-blocking retry banner (NOT an error; spinner shown).
 *   - "joined"    → normal joined success (delegates to onJoin + closes modal).
 *   - "rejected"  → distinct blocked banner (NOT the searching spinner).
 *
 * `onOpenJoinIroh` (wired to the adapter's `openJoinIroh`) is mocked to return
 * each status.
 */

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

const COMMUNITY_ID = '11111111111111111111111111111111';
const INVITE_URL = 'harmony://invite/?p=OPEN';

const fixtureEntry: DirectoryEntry = {
  community_id: COMMUNITY_ID,
  community_addr: '22222222222222222222222222222222',
  name: 'Open Community',
  description: 'A fixture',
  topics: ['test'],
  invite_url: INVITE_URL,
  listed_by_count: 1,
  unattested: false,
  listed_at: { w: 0, l: 0, d: 'd' },
};

function dto(status: string | undefined): RedeemInviteResultDto {
  return {
    communityId: COMMUNITY_ID,
    communityName: 'Open Community',
    isInviteOnly: false,
    pending: false,
    ...(status !== undefined ? { status } : {}),
  };
}

function renderBrowser(
  onOpenJoinIroh: (inviteUrl: string) => Promise<RedeemInviteResultDto>,
  onJoin = vi.fn().mockResolvedValue(undefined),
  onClose = vi.fn(),
) {
  const svc = mockService(
    [{ address: 'aabbccddeeff00112233445566778899', added_at: { w: 0, l: 0, d: 'd' }, entry_count: 1 }],
    [fixtureEntry],
  );
  const utils = render(LibraryDirectoryBrowser, {
    props: { service: svc, adapter: mockAdapter(), onJoin, onOpenJoinIroh, onClose },
  });
  return { ...utils, onJoin, onClose, svc };
}

describe('open-community cold-start join (connectivity_open_join_iroh status mapping)', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('"searching" renders the non-blocking retry banner — NOT an error, modal stays open', async () => {
    const onOpenJoinIroh = vi.fn().mockResolvedValue(dto('searching'));
    const { findByText, getByTestId, queryByTestId, onJoin, onClose } =
      renderBrowser(onOpenJoinIroh);

    await fireEvent.click(await findByText('Join'));

    // The cross-WAN path was invoked with the entry's invite URL.
    await waitFor(() => {
      expect(onOpenJoinIroh).toHaveBeenCalledWith(INVITE_URL);
    });

    // Retry banner is rendered with reassuring "keep trying" copy and a
    // status role (in-progress, not an alert).
    const banner = await waitFor(() => getByTestId('cold-start-searching'));
    expect(banner).toBeInTheDocument();
    expect(banner.getAttribute('role')).toBe('status');
    expect(banner.textContent).toMatch(/keep trying/i);

    // It is NOT the rejected/blocked banner.
    expect(queryByTestId('cold-start-rejected')).toBeNull();

    // Non-blocking: the LAN onJoin side-effects did NOT fire and the modal
    // did NOT close (the user keeps waiting in place).
    expect(onJoin).not.toHaveBeenCalled();
    expect(onClose).not.toHaveBeenCalled();
  });

  it('"joined" takes the normal joined success path (onJoin + close), no cold-start banner', async () => {
    const onOpenJoinIroh = vi.fn().mockResolvedValue(dto('joined'));
    const { findByText, onJoin, onClose, queryByTestId } = renderBrowser(onOpenJoinIroh);

    await fireEvent.click(await findByText('Join'));

    await waitFor(() => {
      expect(onJoin).toHaveBeenCalledWith(COMMUNITY_ID);
      expect(onClose).toHaveBeenCalled();
    });

    // Neither cold-start banner is shown on the joined path.
    expect(queryByTestId('cold-start-searching')).toBeNull();
    expect(queryByTestId('cold-start-rejected')).toBeNull();
  });

  it('"rejected" renders the distinct blocked banner — NOT the searching spinner', async () => {
    const onOpenJoinIroh = vi.fn().mockResolvedValue(dto('rejected'));
    const { findByText, getByTestId, queryByTestId, onJoin, onClose } =
      renderBrowser(onOpenJoinIroh);

    await fireEvent.click(await findByText('Join'));

    const banner = await waitFor(() => getByTestId('cold-start-rejected'));
    expect(banner).toBeInTheDocument();
    expect(banner.getAttribute('role')).toBe('alert');
    expect(banner.textContent).toMatch(/declined/i);

    // It is NOT the searching spinner.
    expect(queryByTestId('cold-start-searching')).toBeNull();

    // Blocked: did not join, did not close.
    expect(onJoin).not.toHaveBeenCalled();
    expect(onClose).not.toHaveBeenCalled();
  });

  it('status-absent (legacy/local) result falls through to the normal joined path', async () => {
    const onOpenJoinIroh = vi.fn().mockResolvedValue(dto(undefined));
    const { findByText, onJoin, onClose, queryByTestId } = renderBrowser(onOpenJoinIroh);

    await fireEvent.click(await findByText('Join'));

    await waitFor(() => {
      expect(onJoin).toHaveBeenCalledWith(COMMUNITY_ID);
      expect(onClose).toHaveBeenCalled();
    });
    expect(queryByTestId('cold-start-searching')).toBeNull();
    expect(queryByTestId('cold-start-rejected')).toBeNull();
  });
});
