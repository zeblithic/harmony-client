import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import CommunityView from '../CommunityView.svelte';
import { CommunityService } from '../../community-service';
import { ChannelMessageService } from '../../channel-message-service';
import type { TauriAdapter } from '../../zenoh-service';
import type { CommunityMember } from '../../types';

function makeAdapter(): TauriAdapter & { listeners: Map<string, Function> } {
  const listeners = new Map<string, Function>();
  return {
    listeners,
    invoke: vi.fn(),
    listen: vi.fn(async (event: string, handler: Function) => {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    }),
  } as any;
}

const adminMember: CommunityMember = {
  address: 'aa'.repeat(20),
  displayName: 'Alice',
  power: 100,
  status: 'joined',
};
const general = {
  channelId: '01'.repeat(16),
  name: 'general',
  writePower: 0,
  createdAt: { wallMs: 100, logical: 0, deviceId: 'd' },
};
const announcements = {
  channelId: '02'.repeat(16),
  name: 'announcements',
  writePower: 50,
  createdAt: { wallMs: 200, logical: 0, deviceId: 'd' },
};

async function setup(channelList: any[] = [general, announcements], propOverrides: Record<string, unknown> = {}) {
  const adapter = makeAdapter();
  (adapter.invoke as any).mockImplementation((cmd: string) => {
    if (cmd === 'list_channels') return Promise.resolve(channelList);
    if (cmd === 'list_channel_messages') return Promise.resolve([]);
    return Promise.resolve(undefined);
  });
  const communityService = new CommunityService();
  await communityService.connectAdapter(adapter);
  const channelMessageService = new ChannelMessageService();
  await channelMessageService.connectAdapter(adapter);
  const props = {
    communityId: 'aa'.repeat(16),
    communityName: 'Test Community',
    communityKind: 'open' as const,
    myPower: 100,
    ownAddress: adminMember.address,
    members: [adminMember],
    isDegraded: false,
    communityService,
    channelMessageService,
    onLeave: vi.fn(),
    onKickMember: vi.fn(),
    onSetPowerLevel: vi.fn(),
    onGenerateInvite: vi.fn().mockResolvedValue('harmony://invite/...'),
    ...propOverrides,
  };
  const renderResult = render(CommunityView, { props });
  return { adapter, communityService, channelMessageService, props, ...renderResult };
}

describe('CommunityView', () => {
  it('mounts the three columns', async () => {
    const { container } = await setup();
    await waitFor(() => {
      expect(container.querySelector('.channel-sub-sidebar')).toBeTruthy();
      expect(container.querySelector('.channel-message-feed')).toBeTruthy();
      expect(container.querySelector('.members-panel')).toBeTruthy();
    });
  });

  it('selects #general by default on first visit', async () => {
    const { container } = await setup();
    await waitFor(() => {
      const active = container.querySelector('.channel-item.active');
      expect(active?.querySelector('.channel-name')?.textContent?.trim()).toBe('general');
    });
  });

  it('clicking ⚙️ opens CommunitySettingsPanel modal', async () => {
    const { container, getByLabelText } = await setup();
    await waitFor(() => {
      expect(container.querySelector('.channel-sub-sidebar')).toBeTruthy();
    });
    await fireEvent.click(getByLabelText(/Open community settings/i));
    await waitFor(() => {
      expect(document.querySelector('[role="dialog"]')).toBeTruthy();
    });
  });

  it('clicking + opens CreateChannelDialog', async () => {
    const { container, getByLabelText } = await setup();
    await waitFor(() => {
      expect(container.querySelector('.create-channel-btn')).toBeTruthy();
    });
    await fireEvent.click(container.querySelector('.create-channel-btn') as HTMLElement);
    await waitFor(() => {
      expect(getByLabelText(/Channel name/i)).toBeTruthy();
    });
  });

  it('channel-config-updated Modified silently re-renders header', async () => {
    const { adapter, container } = await setup();
    await waitFor(() => {
      expect(container.querySelector('.channel-message-feed .name')?.textContent?.trim()).toBe('general');
    });

    // Re-list returns a renamed channel.
    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channels') return Promise.resolve([
        { ...general, name: 'general-renamed' },
        announcements,
      ]);
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });

    const handler = adapter.listeners.get('channel-config-updated')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: general.channelId,
        action: 'modified',
        name: 'general-renamed',
        atWallMs: 200,
      },
    });

    await waitFor(() => {
      expect(container.querySelector('.channel-message-feed .name')?.textContent?.trim()).toBe('general-renamed');
    });
  });

  it('channel-config-updated Deleted on active channel cascades to next-newest (or empty if last)', async () => {
    const { adapter, container } = await setup();
    await waitFor(() => {
      expect(container.querySelector('.channel-item.active .channel-name')?.textContent?.trim()).toBe('general');
    });

    // After delete, list_channels returns only announcements.
    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channels') return Promise.resolve([announcements]);
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });

    const handler = adapter.listeners.get('channel-config-updated')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: general.channelId,
        action: 'deleted',
        atWallMs: 300,
      },
    });

    await waitFor(() => {
      // #general gone → cascade picks next channel (announcements).
      expect(container.querySelector('.channel-item.active .channel-name')?.textContent?.trim()).toBe('announcements');
    });
  });

  it('channel-config-updated Deleted on last remaining channel renders empty-state', async () => {
    const { adapter, container } = await setup([general]);
    await waitFor(() => {
      expect(container.querySelector('.channel-item.active')).toBeTruthy();
    });

    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channels') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });

    const handler = adapter.listeners.get('channel-config-updated')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: general.channelId,
        action: 'deleted',
        atWallMs: 300,
      },
    });

    await waitFor(() => {
      expect(container.querySelector('.empty-channels')).toBeTruthy();
    });
  });

  it('+ button hidden when myPower < 50', async () => {
    const { container } = await setup(undefined, { myPower: 25 });
    await waitFor(() => {
      expect(container.querySelector('.channel-sub-sidebar')).toBeTruthy();
    });
    expect(container.querySelector('.create-channel-btn')).toBeNull();
  });

  it('clicking a channel updates active selection and persists via communityService.setSelectedChannel', async () => {
    const { container, communityService } = await setup();
    await waitFor(() => {
      expect(container.querySelectorAll('.channel-item').length).toBeGreaterThan(1);
    });
    const items = container.querySelectorAll('.channel-item');
    await fireEvent.click(items[1] as HTMLElement);
    await waitFor(() => {
      expect(container.querySelector('.channel-item.active .channel-name')?.textContent?.trim()).toBe('announcements');
    });
    expect(communityService.getSelectedChannel('aa'.repeat(16))).toBe(announcements.channelId);
  });
});
