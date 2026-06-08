import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import FriendsPanel from './FriendsPanel.svelte';
import type { FriendService } from '../friend-service';

const FULL_KEY = 'ab'.repeat(64); // 128 hex chars

function mockService(overrides: Partial<FriendService> = {}): FriendService {
  return {
    listFriends: vi.fn().mockResolvedValue([]),
    listPendingRequests: vi.fn().mockResolvedValue([]),
    getAutoAccept: vi.fn().mockResolvedValue(false),
    onFriendsChanged: vi.fn().mockReturnValue(() => {}),
    onPendingRequestsChanged: vi.fn().mockReturnValue(() => {}),
    getMyIdentityPubHex: vi.fn().mockResolvedValue(null),
    ...overrides,
  } as unknown as FriendService;
}

const writeText = vi.fn().mockResolvedValue(undefined);

beforeEach(() => {
  writeText.mockClear();
  Object.defineProperty(navigator, 'clipboard', {
    value: { writeText },
    configurable: true,
  });
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe('FriendsPanel — My key (ZEB-388)', () => {
  it('renders the key and copies the full hex to the clipboard', async () => {
    const service = mockService({
      getMyIdentityPubHex: vi.fn().mockResolvedValue(FULL_KEY),
    });
    const { findByTestId } = render(FriendsPanel, { props: { service } });

    const input = (await findByTestId('my-key-input')) as HTMLInputElement;
    expect(input.value).toBe(FULL_KEY);

    const btn = await findByTestId('my-key-copy-btn');
    await fireEvent.click(btn);
    expect(writeText).toHaveBeenCalledWith(FULL_KEY);
  });

  it('shows a neutral "start your node" message when no key is available', async () => {
    const service = mockService({
      getMyIdentityPubHex: vi.fn().mockResolvedValue(null),
    });
    const { findByTestId, queryByTestId } = render(FriendsPanel, { props: { service } });

    expect(await findByTestId('my-key-empty')).toBeTruthy();
    expect(queryByTestId('my-key-copy-btn')).toBeNull();
  });
});
