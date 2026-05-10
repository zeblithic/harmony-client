import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import ChannelSubSidebar from '../ChannelSubSidebar.svelte';
import type { ChannelInfo } from '../../community-service';

const general: ChannelInfo = {
  channelId: '01'.repeat(16),
  name: 'general',
  writePower: 0,
  createdAt: { wallMs: 100, logical: 0, deviceId: 'd1' },
};
const announcements: ChannelInfo = {
  channelId: '02'.repeat(16),
  name: 'announcements',
  writePower: 50,
  createdAt: { wallMs: 200, logical: 0, deviceId: 'd1' },
};
const devTalk: ChannelInfo = {
  channelId: '03'.repeat(16),
  name: 'dev-talk',
  writePower: 0,
  createdAt: { wallMs: 300, logical: 0, deviceId: 'd1' },
};

const baseProps = {
  channels: [general, announcements, devTalk],
  activeChannelId: general.channelId,
  myPower: 100,
  onSelect: vi.fn(),
  onCreateClick: vi.fn(),
  onModifyClick: vi.fn(),
  onDeleteClick: vi.fn(),
};

describe('ChannelSubSidebar', () => {
  it('renders all channels in the order received (parent guarantees oldest-first)', () => {
    const { container } = render(ChannelSubSidebar, { props: baseProps });
    const items = Array.from(container.querySelectorAll('.channel-item .channel-name'))
      .map((el) => el.textContent?.trim());
    expect(items).toEqual(['general', 'announcements', 'dev-talk']);
  });

  it('highlights the active channel', () => {
    const { container } = render(ChannelSubSidebar, { props: baseProps });
    const active = container.querySelector('.channel-item.active');
    expect(active?.querySelector('.channel-name')?.textContent?.trim()).toBe('general');
  });

  it('clicking a channel item dispatches onSelect with channelId', async () => {
    const onSelect = vi.fn();
    const { container } = render(ChannelSubSidebar, { props: { ...baseProps, onSelect } });
    const items = container.querySelectorAll('.channel-item');
    await fireEvent.click(items[1] as HTMLElement);
    expect(onSelect).toHaveBeenCalledWith(announcements.channelId);
  });

  it('+ button visible when myPower >= 50', () => {
    const { container } = render(ChannelSubSidebar, { props: baseProps });
    expect(container.querySelector('button.create-channel-btn')).toBeTruthy();
  });

  it('+ button hidden when myPower < 50', () => {
    const { container } = render(ChannelSubSidebar, {
      props: { ...baseProps, myPower: 25 },
    });
    expect(container.querySelector('button.create-channel-btn')).toBeNull();
  });

  it('+ button click dispatches onCreateClick', async () => {
    const onCreateClick = vi.fn();
    const { container } = render(ChannelSubSidebar, {
      props: { ...baseProps, onCreateClick },
    });
    await fireEvent.click(container.querySelector('button.create-channel-btn') as HTMLElement);
    expect(onCreateClick).toHaveBeenCalled();
  });

  it('right-click on a channel opens context menu when myPower >= 50', async () => {
    const { container } = render(ChannelSubSidebar, { props: baseProps });
    const item = container.querySelectorAll('.channel-item')[1] as HTMLElement;
    await fireEvent.contextMenu(item);
    expect(container.querySelector('.context-menu')).toBeTruthy();
  });

  it('right-click context menu does NOT appear when myPower < 50', async () => {
    const { container } = render(ChannelSubSidebar, {
      props: { ...baseProps, myPower: 25 },
    });
    const item = container.querySelectorAll('.channel-item')[1] as HTMLElement;
    await fireEvent.contextMenu(item);
    expect(container.querySelector('.context-menu')).toBeNull();
  });

  it('Rename context menu item dispatches onModifyClick with the channel', async () => {
    const onModifyClick = vi.fn();
    const { container, getByRole } = render(ChannelSubSidebar, {
      props: { ...baseProps, onModifyClick },
    });
    const item = container.querySelectorAll('.channel-item')[1] as HTMLElement;
    await fireEvent.contextMenu(item);
    await fireEvent.click(getByRole('button', { name: /Rename/i }));
    expect(onModifyClick).toHaveBeenCalledWith(announcements);
  });

  it('Delete context menu item dispatches onDeleteClick with the channel', async () => {
    const onDeleteClick = vi.fn();
    const { container, getByRole } = render(ChannelSubSidebar, {
      props: { ...baseProps, onDeleteClick },
    });
    const item = container.querySelectorAll('.channel-item')[1] as HTMLElement;
    await fireEvent.contextMenu(item);
    await fireEvent.click(getByRole('button', { name: /Delete/i }));
    expect(onDeleteClick).toHaveBeenCalledWith(announcements);
  });

  it('clicking outside dismisses the context menu', async () => {
    const { container } = render(ChannelSubSidebar, { props: baseProps });
    const item = container.querySelectorAll('.channel-item')[1] as HTMLElement;
    await fireEvent.contextMenu(item);
    expect(container.querySelector('.context-menu')).toBeTruthy();

    // Click outside (on the sidebar root)
    await fireEvent.click(container.querySelector('.channel-sub-sidebar') as HTMLElement);
    expect(container.querySelector('.context-menu')).toBeNull();
  });
});
