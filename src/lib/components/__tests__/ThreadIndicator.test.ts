import { render, screen } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import ThreadIndicator from '../ThreadIndicator.svelte';
import type { Peer } from '../../types';

// Per-instance IntersectionObserver mock
// `disconnect` is typed as a callable mock (vi.fn's default Mock type is
// Procedure | Constructable, which TS can't resolve for plain calls).
let observers: { callback: IntersectionObserverCallback; elements: Element[]; disconnect: ReturnType<typeof vi.fn<() => void>> }[] = [];

function latestObserver() {
  return observers[observers.length - 1];
}

class MockIntersectionObserver {
  private _entry: typeof observers[number];
  constructor(callback: IntersectionObserverCallback) {
    this._entry = { callback, elements: [], disconnect: vi.fn<() => void>() };
    observers.push(this._entry);
  }
  observe(el: Element) { this._entry.elements.push(el); }
  unobserve(el: Element) { this._entry.elements = this._entry.elements.filter(e => e !== el); }
  disconnect() { this._entry.disconnect(); this._entry.elements = []; }
}

function simulateIntersection(obs: typeof observers[number], el: Element, isIntersecting: boolean) {
  obs.callback(
    [{ target: el, isIntersecting } as IntersectionObserverEntry],
    {} as IntersectionObserver,
  );
}

const participants: Peer[] = [
  { address: 'a', displayName: 'Alice' },
  { address: 'b', displayName: 'Bob' },
];

describe('ThreadIndicator', () => {
  beforeEach(() => {
    observers = [];
    vi.stubGlobal('IntersectionObserver', MockIntersectionObserver);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('shows reply count and participant names', () => {
    render(ThreadIndicator, {
      props: { count: 3, participants, rootId: 'root-1' },
    });
    expect(screen.getByText(/3 replies/)).toBeTruthy();
    expect(screen.getByText(/Alice, Bob/)).toBeTruthy();
  });

  it('calls onOpen when clicked', async () => {
    const onOpen = vi.fn();
    render(ThreadIndicator, {
      props: { count: 2, participants, rootId: 'root-1', onOpen },
    });
    screen.getByRole('button').click();
    expect(onOpen).toHaveBeenCalledWith('root-1');
  });

  it('shows singular reply for count of 1', () => {
    render(ThreadIndicator, {
      props: { count: 1, participants: [participants[0]], rootId: 'root-1' },
    });
    expect(screen.getByText(/1 reply/)).toBeTruthy();
  });

  it('truncates participant names beyond 3', () => {
    const many: Peer[] = [
      { address: 'a', displayName: 'Alice' },
      { address: 'b', displayName: 'Bob' },
      { address: 'c', displayName: 'Carol' },
      { address: 'd', displayName: 'Dave' },
    ];
    render(ThreadIndicator, {
      props: { count: 5, participants: many, rootId: 'root-1' },
    });
    expect(screen.getByText(/\+1 more/)).toBeTruthy();
  });

  it('has correct aria-label', () => {
    render(ThreadIndicator, {
      props: { count: 3, participants, rootId: 'root-1' },
    });
    const btn = screen.getByRole('button');
    expect(btn.getAttribute('aria-label')).toContain('3 replies');
    expect(btn.getAttribute('aria-label')).toContain('Alice');
  });

  it('optimistically reports visible on mount, then tracks intersection', async () => {
    const onVisibilityChange = vi.fn();
    render(ThreadIndicator, {
      props: { count: 2, participants, rootId: 'root-1', onVisibilityChange },
    });
    await vi.waitFor(() => expect(latestObserver().elements.length).toBe(1));

    // Optimistic true fires via microtask
    await vi.waitFor(() => expect(onVisibilityChange).toHaveBeenCalledWith('root-1', true));
    onVisibilityChange.mockClear();

    // Observer corrects if off-screen
    const obs = latestObserver();
    simulateIntersection(obs, obs.elements[0], false);
    expect(onVisibilityChange).toHaveBeenCalledWith('root-1', false);
  });

  it('calls onVisibilityChange(false) and disconnects on unmount', async () => {
    const onVisibilityChange = vi.fn();
    const { unmount } = render(ThreadIndicator, {
      props: { count: 1, participants: [participants[0]], rootId: 'root-1', onVisibilityChange },
    });
    await vi.waitFor(() => expect(latestObserver().elements.length).toBe(1));
    const obs = latestObserver();

    // Simulate visible first
    simulateIntersection(obs, obs.elements[0], true);
    onVisibilityChange.mockClear();

    unmount();
    expect(obs.disconnect).toHaveBeenCalled();
    expect(onVisibilityChange).toHaveBeenCalledWith('root-1', false);
  });

  // ZEB-962: DM thread participants carry a blank baked name (resolved live, not
  // frozen at ingest), so the reply preview + aria-label must resolve through the
  // author ladder rather than render the raw blank.
  it('resolves a blank DM participant name through the card', () => {
    const PEER = 'ef'.repeat(16);
    const { container } = render(ThreadIndicator, {
      props: {
        count: 2,
        participants: [{ address: PEER, displayName: '' }],
        rootId: 'root-1',
        resolveCard: (id: string) => (id === PEER ? { displayName: 'Zeb', statusText: '' } : undefined),
      } as any,
    });
    expect(container.querySelector('.thread-info')?.textContent).toContain('Zeb');
    expect(container.querySelector('.thread-indicator')?.getAttribute('aria-label')).toContain('Zeb');
  });

  it('falls back to short hex when no card resolves a blank DM name', () => {
    const PEER = 'ef'.repeat(16);
    const { container } = render(ThreadIndicator, {
      props: { count: 1, participants: [{ address: PEER, displayName: '' }], rootId: 'root-1' } as any,
    });
    expect(container.querySelector('.thread-info')?.textContent).toContain(PEER.slice(0, 8));
    expect(container.querySelector('.thread-indicator')?.getAttribute('aria-label')).toContain(
      PEER.slice(0, 8),
    );
  });
});
