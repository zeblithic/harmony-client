import { render, screen } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import ThreadIndicator from '../ThreadIndicator.svelte';
import type { Peer } from '../../types';

// IntersectionObserver mock
let observerCallback: IntersectionObserverCallback;
let observedElements: Element[] = [];
const mockDisconnect = vi.fn();

class MockIntersectionObserver {
  constructor(callback: IntersectionObserverCallback) {
    observerCallback = callback;
  }
  observe(el: Element) { observedElements.push(el); }
  unobserve(el: Element) { observedElements = observedElements.filter(e => e !== el); }
  disconnect() { mockDisconnect(); observedElements = []; }
}

function simulateIntersection(el: Element, isIntersecting: boolean) {
  observerCallback(
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
    observedElements = [];
    mockDisconnect.mockClear();
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

  it('calls onVisibilityChange when intersection changes', async () => {
    const onVisibilityChange = vi.fn();
    render(ThreadIndicator, {
      props: { count: 2, participants, rootId: 'root-1', onVisibilityChange },
    });
    await vi.waitFor(() => expect(observedElements.length).toBe(1));
    const el = observedElements[0];

    simulateIntersection(el, true);
    expect(onVisibilityChange).toHaveBeenCalledWith('root-1', true);

    simulateIntersection(el, false);
    expect(onVisibilityChange).toHaveBeenCalledWith('root-1', false);
  });

  it('disconnects observer on unmount', async () => {
    const onVisibilityChange = vi.fn();
    const { unmount } = render(ThreadIndicator, {
      props: { count: 1, participants: [participants[0]], rootId: 'root-1', onVisibilityChange },
    });
    await vi.waitFor(() => expect(observedElements.length).toBe(1));
    unmount();
    expect(mockDisconnect).toHaveBeenCalled();
  });
});
