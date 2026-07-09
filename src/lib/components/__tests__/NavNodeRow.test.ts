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
