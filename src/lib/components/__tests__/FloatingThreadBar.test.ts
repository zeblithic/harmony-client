import { render, screen } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import FloatingThreadBar from '../FloatingThreadBar.svelte';
import type { Peer } from '../../types';
import type { ThreadMetaEntry } from '../../feed-utils';

const alice: Peer = { address: 'a', displayName: 'Alice' };
const bob: Peer = { address: 'b', displayName: 'Bob' };

function makeMeta(entries: [string, number, Peer[]][]): Map<string, ThreadMetaEntry> {
  return new Map(entries.map(([id, count, participants]) => [id, { count, participants }]));
}

const rootMessages = new Map<string, { sender: string; text: string }>([
  ['root-1', { sender: 'Alice', text: 'Check this out' }],
  ['root-2', { sender: 'Bob', text: 'API design discussion' }],
]);

describe('FloatingThreadBar', () => {
  it('renders nothing when no threads qualify', () => {
    const { container } = render(FloatingThreadBar, {
      props: {
        threadMeta: new Map(),
        pinnedThreadIds: new Set(),
        visibleThreadIds: new Set(),
        rootMessages,
      },
    });
    expect(container.querySelector('.floating-thread-bar')).toBeNull();
  });

  it('shows pinned threads regardless of visibility', () => {
    const meta = makeMeta([['root-1', 3, [alice, bob]]]);
    render(FloatingThreadBar, {
      props: {
        threadMeta: meta,
        pinnedThreadIds: new Set(['root-1']),
        visibleThreadIds: new Set(['root-1']),
        rootMessages,
      },
    });
    expect(screen.getByText(/Alice/)).toBeTruthy();
    expect(screen.getByText(/📌/)).toBeTruthy();
  });

  it('shows auto-floated threads when scrolled out of view', () => {
    const meta = makeMeta([['root-1', 2, [alice]]]);
    render(FloatingThreadBar, {
      props: {
        threadMeta: meta,
        pinnedThreadIds: new Set(),
        visibleThreadIds: new Set(),
        rootMessages,
      },
    });
    expect(screen.getByText(/Alice/)).toBeTruthy();
  });

  it('does not show auto-floated threads that are visible', () => {
    const meta = makeMeta([['root-1', 2, [alice]]]);
    const { container } = render(FloatingThreadBar, {
      props: {
        threadMeta: meta,
        pinnedThreadIds: new Set(),
        visibleThreadIds: new Set(['root-1']),
        rootMessages,
      },
    });
    expect(container.querySelector('.floating-thread-bar')).toBeNull();
  });

  it('calls onThreadOpen when a thread entry is clicked', () => {
    const onThreadOpen = vi.fn();
    const meta = makeMeta([['root-1', 2, [alice]]]);
    render(FloatingThreadBar, {
      props: {
        threadMeta: meta,
        pinnedThreadIds: new Set(),
        visibleThreadIds: new Set(),
        rootMessages,
        onThreadOpen,
      },
    });
    screen.getByRole('button').click();
    expect(onThreadOpen).toHaveBeenCalledWith('root-1');
  });
});
