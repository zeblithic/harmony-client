import { render, screen } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import ThreadIndicator from '../ThreadIndicator.svelte';
import type { Peer } from '../../types';

const participants: Peer[] = [
  { address: 'a', displayName: 'Alice' },
  { address: 'b', displayName: 'Bob' },
];

describe('ThreadIndicator', () => {
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
});
