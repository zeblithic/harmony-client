import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import NavNodeRow from '../NavNodeRow.svelte';
import type { NavNode } from '../../types';

function makeNode(overrides: Partial<NavNode> = {}): NavNode {
  return {
    id: 'test-node',
    parentId: null,
    type: 'channel',
    name: 'general',
    expanded: false,
    unreadCount: 0,
    mentionCount: 0,
    unreadLevel: 'none',
    ...overrides,
  };
}

describe('NavNodeRow', () => {
  it('renders channel name with # prefix', () => {
    render(NavNodeRow, {
      props: {
        node: makeNode({ type: 'channel', name: 'general' }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    expect(screen.getByText('#')).toBeTruthy();
    expect(screen.getByText('general')).toBeTruthy();
  });

  it('shows a clay mention badge when mentionCount > 0 (ZEB-662)', () => {
    render(NavNodeRow, {
      props: {
        node: makeNode({ type: 'channel', name: 'general', mentionCount: 2 }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    const badge = screen.getByTestId('mention-badge');
    expect(badge.textContent).toBe('@2');
    expect(badge.getAttribute('aria-label')).toBe('2 unread mentions');
  });

  it('renders no mention badge when mentionCount is 0 (ZEB-662)', () => {
    render(NavNodeRow, {
      props: {
        node: makeNode({ type: 'channel', name: 'general', mentionCount: 0 }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    expect(screen.queryByTestId('mention-badge')).toBeNull();
  });

  it('shows a missed-call badge on a DM row when missedCallCount > 0 (ZEB-357)', () => {
    render(NavNodeRow, {
      props: {
        node: makeNode({ type: 'dm', name: 'Alice', missedCallCount: 2 }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    const badge = screen.getByTestId('missed-call-badge');
    expect(badge.textContent).toContain('2');
    expect(badge.getAttribute('aria-label')).toBe('2 missed calls');
  });

  it('renders no missed-call badge when missedCallCount is 0 or absent (ZEB-357)', () => {
    render(NavNodeRow, {
      props: {
        node: makeNode({ type: 'dm', name: 'Alice' }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    expect(screen.queryByTestId('missed-call-badge')).toBeNull();
  });

  it('renders color bands matching ancestry depth', () => {
    const { container } = render(NavNodeRow, {
      props: {
        node: makeNode(),
        colorAncestry: [0, 1, 2],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    const bands = container.querySelectorAll('.color-band');
    expect(bands.length).toBe(3);
  });

  it('renders folder with name', () => {
    render(NavNodeRow, {
      props: {
        node: makeNode({ type: 'folder', name: 'Work', expanded: true }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    expect(screen.getByText('Work')).toBeTruthy();
  });

  it('shows standard unread badge with count', () => {
    render(NavNodeRow, {
      props: {
        node: makeNode({ unreadCount: 5, unreadLevel: 'standard' }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    const badge = screen.getByText('5');
    expect(badge.classList.contains('unread-badge')).toBe(true);
    expect(badge.classList.contains('loud')).toBe(false);
  });

  it('shows loud unread badge with pulsing class', () => {
    render(NavNodeRow, {
      props: {
        node: makeNode({ unreadCount: 2, unreadLevel: 'loud' }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    const badge = screen.getByText('2');
    expect(badge.classList.contains('unread-badge')).toBe(true);
    expect(badge.classList.contains('loud')).toBe(true);
  });

  it('shows quiet unread dot', () => {
    const { container } = render(NavNodeRow, {
      props: {
        node: makeNode({ unreadLevel: 'quiet', unreadCount: 1 }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    const dot = container.querySelector('.unread-dot');
    expect(dot).toBeTruthy();
  });

  it('renders close bracket when isLastChild and has ancestry', () => {
    const { container } = render(NavNodeRow, {
      props: {
        node: makeNode(),
        colorAncestry: [0],
        displayMode: 'text',
        isLastChild: true,
      },
    });
    const closeBracket = container.querySelector('.bracket-close');
    expect(closeBracket).toBeTruthy();
  });

  // ZEB-600: presence dot — driven by the injected presenceOnline resolver.
  it('shows the presence dot when presenceOnline(node) is true', () => {
    const { container } = render(NavNodeRow, {
      props: {
        node: makeNode({ type: 'community', name: 'IPFS Crew' }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
        presenceOnline: () => true,
      },
    });
    expect(container.querySelector('.nav-presence-dot')).toBeTruthy();
  });

  it('hides the presence dot when presenceOnline(node) is false', () => {
    const { container } = render(NavNodeRow, {
      props: {
        node: makeNode({ type: 'community', name: 'IPFS Crew' }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
        presenceOnline: () => false,
      },
    });
    expect(container.querySelector('.nav-presence-dot')).toBeNull();
  });

  it('hides the presence dot when no resolver is provided (default off)', () => {
    const { container } = render(NavNodeRow, {
      props: {
        node: makeNode({ type: 'community', name: 'IPFS Crew' }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    expect(container.querySelector('.nav-presence-dot')).toBeNull();
  });
});

describe('NavNodeRow — unread badge display cap (ZEB-665)', () => {
  it('renders "99+" when unreadCount exceeds 99', () => {
    render(NavNodeRow, {
      props: {
        node: makeNode({ unreadCount: 100, unreadLevel: 'standard' }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    expect(screen.getByText('99+')).toBeTruthy();
  });

  it('renders the exact count at or below 99', () => {
    render(NavNodeRow, {
      props: {
        node: makeNode({ unreadCount: 99, unreadLevel: 'standard' }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    expect(screen.getByText('99')).toBeTruthy();
  });

  it('community quiet level renders the dot, not a number', () => {
    const { container } = render(NavNodeRow, {
      props: {
        node: makeNode({ type: 'community', name: 'Crew', unreadCount: 12, unreadLevel: 'quiet' }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    expect(container.querySelector('.unread-dot')).toBeTruthy();
    expect(container.querySelector('.unread-badge')).toBeNull();
  });
});

describe('NavNodeRow — channel kind glyph (ZEB-663)', () => {
  it('renders 🔊 for a voice channel and NOT #', () => {
    const { container } = render(NavNodeRow, {
      props: {
        node: makeNode({ type: 'channel', name: 'lounge', channelKind: 'voice' }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    const icon = container.querySelector('.type-icon');
    expect(icon?.textContent).toContain('🔊');
    expect(icon?.textContent).not.toContain('#');
  });

  it('renders # for a text channel', () => {
    render(NavNodeRow, {
      props: {
        node: makeNode({ type: 'channel', name: 'general', channelKind: 'text' }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    expect(screen.getByText('#')).toBeTruthy();
  });
});

describe('NavNodeRow — channel context menu (ZEB-663)', () => {
  const channel = () =>
    makeNode({ id: 'ch1', parentId: 'c1', type: 'channel', name: 'general' });

  function renderRow(over: Record<string, unknown> = {}) {
    return render(NavNodeRow, {
      props: {
        node: channel(),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
        canManageChannel: () => true,
        onRenameChannel: vi.fn(),
        onDeleteChannel: vi.fn(),
        ...over,
      },
    });
  }

  it('right-click opens the rename/delete menu when the viewer can manage', async () => {
    const { container } = renderRow();
    await fireEvent.contextMenu(container.querySelector('.nav-row') as HTMLElement);
    expect(container.querySelector('.channel-context-menu')).toBeTruthy();
  });

  it('right-click does NOT open the menu when the viewer cannot manage', async () => {
    const { container } = renderRow({ canManageChannel: () => false });
    await fireEvent.contextMenu(container.querySelector('.nav-row') as HTMLElement);
    expect(container.querySelector('.channel-context-menu')).toBeNull();
  });

  it('Rename dispatches onRenameChannel with (communityId, channelId)', async () => {
    const onRenameChannel = vi.fn();
    const { container, getByRole } = renderRow({ onRenameChannel });
    await fireEvent.contextMenu(container.querySelector('.nav-row') as HTMLElement);
    await fireEvent.click(getByRole('menuitem', { name: /Rename/i }));
    expect(onRenameChannel).toHaveBeenCalledWith('c1', 'ch1');
  });

  it('Delete dispatches onDeleteChannel with (communityId, channelId)', async () => {
    const onDeleteChannel = vi.fn();
    const { container, getByRole } = renderRow({ onDeleteChannel });
    await fireEvent.contextMenu(container.querySelector('.nav-row') as HTMLElement);
    await fireEvent.click(getByRole('menuitem', { name: /Delete/i }));
    expect(onDeleteChannel).toHaveBeenCalledWith('c1', 'ch1');
  });

  it('clicking outside dismisses the menu', async () => {
    const { container } = renderRow();
    await fireEvent.contextMenu(container.querySelector('.nav-row') as HTMLElement);
    expect(container.querySelector('.channel-context-menu')).toBeTruthy();
    await fireEvent.click(document.body);
    expect(container.querySelector('.channel-context-menu')).toBeNull();
  });

  it('demotion (canManageChannel → false) closes an open menu (§6.8)', async () => {
    const { container, rerender } = renderRow();
    await fireEvent.contextMenu(container.querySelector('.nav-row') as HTMLElement);
    expect(container.querySelector('.channel-context-menu')).toBeTruthy();
    await rerender({
      node: channel(),
      colorAncestry: [],
      displayMode: 'text',
      isLastChild: false,
      canManageChannel: () => false,
      onRenameChannel: vi.fn(),
      onDeleteChannel: vi.fn(),
    });
    expect(container.querySelector('.channel-context-menu')).toBeNull();
  });
});

describe('NavNodeRow — keyboard-accessible ⋯ trigger (ZEB-664)', () => {
  const channel = () =>
    makeNode({ id: 'ch1', parentId: 'c1', type: 'channel', name: 'general' });

  function renderRow(over: Record<string, unknown> = {}) {
    return render(NavNodeRow, {
      props: {
        node: channel(),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
        canManageChannel: () => true,
        onRenameChannel: vi.fn(),
        onDeleteChannel: vi.fn(),
        ...over,
      },
    });
  }

  it('renders the trigger only when the viewer can manage the channel', () => {
    const { getByTestId } = renderRow();
    expect(getByTestId('channel-menu-trigger-ch1')).toBeTruthy();
  });

  it('hides the trigger when the viewer cannot manage', () => {
    const { queryByTestId } = renderRow({ canManageChannel: () => false });
    expect(queryByTestId('channel-menu-trigger-ch1')).toBeNull();
  });

  it('hides the trigger when no canManageChannel resolver is provided', () => {
    const { queryByTestId } = render(NavNodeRow, {
      props: {
        node: channel(),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    expect(queryByTestId('channel-menu-trigger-ch1')).toBeNull();
  });

  it('announces popup state: aria-haspopup="menu" + aria-expanded tracks the menu', async () => {
    const { container, getByTestId } = renderRow();
    const trigger = getByTestId('channel-menu-trigger-ch1');
    expect(trigger.getAttribute('aria-haspopup')).toBe('menu');
    expect(trigger.getAttribute('aria-expanded')).toBe('false');
    await fireEvent.click(trigger);
    expect(container.querySelector('.channel-context-menu')).toBeTruthy();
    expect(trigger.getAttribute('aria-expanded')).toBe('true');
  });

  it('clicking the trigger toggles the menu (second click closes, not reopens)', async () => {
    const { container, getByTestId } = renderRow();
    const trigger = getByTestId('channel-menu-trigger-ch1');
    await fireEvent.click(trigger);
    expect(container.querySelector('.channel-context-menu')).toBeTruthy();
    // The document-level dismiss listener runs capture-phase; the trigger is
    // whitelisted there so this click reaches the toggle handler and closes.
    await fireEvent.click(trigger);
    expect(container.querySelector('.channel-context-menu')).toBeNull();
  });

  it('clicking the trigger does not select the channel row', async () => {
    const onClick = vi.fn();
    const { getByTestId } = renderRow({ onClick });
    await fireEvent.click(getByTestId('channel-menu-trigger-ch1'));
    expect(onClick).not.toHaveBeenCalled();
  });

  it('Enter/Space keydown on the trigger does not bubble into row selection', async () => {
    const onClick = vi.fn();
    const { getByTestId } = renderRow({ onClick });
    const trigger = getByTestId('channel-menu-trigger-ch1');
    await fireEvent.keyDown(trigger, { key: 'Enter' });
    await fireEvent.keyDown(trigger, { key: ' ' });
    expect(onClick).not.toHaveBeenCalled();
  });

  it('moves focus to the first menu item when the menu opens', async () => {
    const { getByTestId, getByRole } = renderRow();
    await fireEvent.click(getByTestId('channel-menu-trigger-ch1'));
    expect(document.activeElement).toBe(getByRole('menuitem', { name: /Rename/i }));
  });

  it('Escape closes the menu and returns focus to the trigger', async () => {
    const { container, getByTestId } = renderRow();
    const trigger = getByTestId('channel-menu-trigger-ch1');
    await fireEvent.click(trigger);
    const menu = container.querySelector('.channel-context-menu') as HTMLElement;
    await fireEvent.keyDown(menu, { key: 'Escape' });
    expect(container.querySelector('.channel-context-menu')).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it('ArrowDown/ArrowUp cycle focus through the menu items', async () => {
    const { container, getByTestId, getByRole } = renderRow();
    await fireEvent.click(getByTestId('channel-menu-trigger-ch1'));
    const menu = container.querySelector('.channel-context-menu') as HTMLElement;
    const rename = getByRole('menuitem', { name: /Rename/i });
    const del = getByRole('menuitem', { name: /Delete/i });
    expect(document.activeElement).toBe(rename);
    await fireEvent.keyDown(menu, { key: 'ArrowDown' });
    expect(document.activeElement).toBe(del);
    await fireEvent.keyDown(menu, { key: 'ArrowDown' }); // wraps
    expect(document.activeElement).toBe(rename);
    await fireEvent.keyDown(menu, { key: 'ArrowUp' }); // wraps back
    expect(document.activeElement).toBe(del);
  });

  it('Rename via the trigger path dispatches onRenameChannel with (communityId, channelId)', async () => {
    const onRenameChannel = vi.fn();
    const { getByTestId, getByRole } = renderRow({ onRenameChannel });
    await fireEvent.click(getByTestId('channel-menu-trigger-ch1'));
    await fireEvent.click(getByRole('menuitem', { name: /Rename/i }));
    expect(onRenameChannel).toHaveBeenCalledWith('c1', 'ch1');
  });

  it('demotion removes the trigger and closes an open menu (§6.8)', async () => {
    const { container, getByTestId, queryByTestId, rerender } = renderRow();
    await fireEvent.click(getByTestId('channel-menu-trigger-ch1'));
    expect(container.querySelector('.channel-context-menu')).toBeTruthy();
    await rerender({
      node: channel(),
      colorAncestry: [],
      displayMode: 'text',
      isLastChild: false,
      canManageChannel: () => false,
      onRenameChannel: vi.fn(),
      onDeleteChannel: vi.fn(),
    });
    expect(container.querySelector('.channel-context-menu')).toBeNull();
    expect(queryByTestId('channel-menu-trigger-ch1')).toBeNull();
  });

  it('right-click open + Escape still returns focus to the trigger (shared menu)', async () => {
    const { container, getByTestId } = renderRow();
    await fireEvent.contextMenu(container.querySelector('.nav-row') as HTMLElement);
    const menu = container.querySelector('.channel-context-menu') as HTMLElement;
    expect(menu).toBeTruthy();
    await fireEvent.keyDown(menu, { key: 'Escape' });
    expect(document.activeElement).toBe(getByTestId('channel-menu-trigger-ch1'));
  });

  it('Tab closes the menu (focus moves on, popup must not linger)', async () => {
    const { container, getByTestId } = renderRow();
    await fireEvent.click(getByTestId('channel-menu-trigger-ch1'));
    const menu = container.querySelector('.channel-context-menu') as HTMLElement;
    await fireEvent.keyDown(menu, { key: 'Tab' });
    expect(container.querySelector('.channel-context-menu')).toBeNull();
  });

  it('demotion while the menu is focused parks focus on the row, not body', async () => {
    const { container, getByTestId, rerender } = renderRow();
    await fireEvent.click(getByTestId('channel-menu-trigger-ch1'));
    // Menu is open and its first item holds focus (auto-focused on open).
    expect((document.activeElement as HTMLElement).getAttribute('role')).toBe('menuitem');
    await rerender({
      node: channel(),
      colorAncestry: [],
      displayMode: 'text',
      isLastChild: false,
      canManageChannel: () => false,
      onRenameChannel: vi.fn(),
      onDeleteChannel: vi.fn(),
    });
    expect(container.querySelector('.channel-context-menu')).toBeNull();
    expect(document.activeElement).toBe(getByTestId('nav-row-ch1'));
  });

  it('demotion while the trigger is focused (menu closed) parks focus on the row', async () => {
    const { getByTestId, rerender } = renderRow();
    const trigger = getByTestId('channel-menu-trigger-ch1');
    trigger.focus();
    expect(document.activeElement).toBe(trigger);
    await rerender({
      node: channel(),
      colorAncestry: [],
      displayMode: 'text',
      isLastChild: false,
      canManageChannel: () => false,
      onRenameChannel: vi.fn(),
      onDeleteChannel: vi.fn(),
    });
    expect(document.activeElement).toBe(getByTestId('nav-row-ch1'));
  });
});

describe('NavNodeRow — community quiet dot visible at rest (ZEB-967)', () => {
  // Post-ZEB-965 the community row is the ONLY left-nav surface for channel
  // activity, so its quiet dot must not be hover-gated: hover-only means no
  // passive signal at all. Channel/DM rows keep the hover-reveal design —
  // their panels carry always-visible badges for the louder tiers.
  it('community row renders the quiet dot with the always-visible class', () => {
    const { container } = render(NavNodeRow, {
      props: {
        node: makeNode({ type: 'community', name: 'Crew', unreadCount: 3, unreadLevel: 'quiet' }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    const dot = container.querySelector('.unread-dot');
    expect(dot).toBeTruthy();
    expect(dot?.classList.contains('always-visible')).toBe(true);
  });

  it('channel row keeps the hover-gated dot (no always-visible class)', () => {
    const { container } = render(NavNodeRow, {
      props: {
        node: makeNode({ type: 'channel', name: 'general', unreadCount: 1, unreadLevel: 'quiet' }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    const dot = container.querySelector('.unread-dot');
    expect(dot).toBeTruthy();
    expect(dot?.classList.contains('always-visible')).toBe(false);
  });

  it('DM row keeps the hover-gated dot (no always-visible class)', () => {
    const { container } = render(NavNodeRow, {
      props: {
        node: makeNode({ type: 'dm', name: 'ada', unreadCount: 1, unreadLevel: 'quiet' }),
        colorAncestry: [],
        displayMode: 'text',
        isLastChild: false,
      },
    });
    const dot = container.querySelector('.unread-dot');
    expect(dot).toBeTruthy();
    expect(dot?.classList.contains('always-visible')).toBe(false);
  });
});
