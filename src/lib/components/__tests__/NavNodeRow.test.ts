import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
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
});
