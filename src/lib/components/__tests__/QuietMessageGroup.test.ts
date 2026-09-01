import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import QuietMessageGroup from '../QuietMessageGroup.svelte';
import type { Message } from '../../types';

function makeMsg(id: string, sender: string): Message {
  return {
    id,
    sender: { address: sender.toLowerCase(), displayName: sender },
    text: `Quiet message ${id}`,
    timestamp: Date.now(),
    priority: 'quiet',
  };
}

describe('QuietMessageGroup', () => {
  it('renders collapsed with message count', () => {
    render(QuietMessageGroup, {
      props: { messages: [makeMsg('1', 'Alice'), makeMsg('2', 'Bob')] },
    });
    expect(screen.getByText(/2 quiet messages/)).toBeTruthy();
  });

  it('shows unique sender names in collapsed view', () => {
    render(QuietMessageGroup, {
      props: {
        messages: [
          makeMsg('1', 'Alice'),
          makeMsg('2', 'Alice'),
          makeMsg('3', 'Bob'),
        ],
      },
    });
    expect(screen.getByText(/Alice, Bob/)).toBeTruthy();
  });

  it('does not show individual messages when collapsed', () => {
    render(QuietMessageGroup, {
      props: { messages: [makeMsg('1', 'Alice')] },
    });
    expect(screen.queryByText('Quiet message 1')).toBeNull();
  });

  it('expands to show individual messages on click', async () => {
    render(QuietMessageGroup, {
      props: { messages: [makeMsg('1', 'Alice'), makeMsg('2', 'Bob')] },
    });
    const toggle = screen.getByRole('button');
    await fireEvent.click(toggle);
    expect(screen.getByText('Quiet message 1')).toBeTruthy();
    expect(screen.getByText('Quiet message 2')).toBeTruthy();
  });

  it('collapses again on second click', async () => {
    render(QuietMessageGroup, {
      props: { messages: [makeMsg('1', 'Alice')] },
    });
    const toggle = screen.getByRole('button');
    await fireEvent.click(toggle);
    expect(screen.getByText('Quiet message 1')).toBeTruthy();
    await fireEvent.click(toggle);
    expect(screen.queryByText('Quiet message 1')).toBeNull();
  });

  it('renders expanded messages with dimmed class', async () => {
    const { container } = render(QuietMessageGroup, {
      props: { messages: [makeMsg('1', 'Alice')] },
    });
    const toggle = screen.getByRole('button');
    await fireEvent.click(toggle);
    const dimmed = container.querySelector('.quiet-expanded');
    expect(dimmed).toBeTruthy();
  });

  // ZEB-962: the collapsed summary aggregates sender names. A blank DM sender
  // name left a naked ", " in the summary; resolve each through the author ladder.
  it('resolves a blank DM sender through the card in the summary', () => {
    const PEER = 'ef'.repeat(16);
    const dmMsg: Message = {
      id: 'm1',
      sender: { address: PEER, displayName: '' },
      text: 'quiet',
      timestamp: Date.now(),
      priority: 'quiet',
    };
    const { container } = render(QuietMessageGroup, {
      props: {
        messages: [dmMsg],
        resolveCard: (id: string) => (id === PEER ? { displayName: 'Zeb', statusText: '' } : undefined),
      } as any,
    });
    expect(container.querySelector('.quiet-summary')?.textContent).toContain('Zeb');
  });

  it('falls back to short hex for a blank DM sender when no resolver is provided', () => {
    const PEER = 'ef'.repeat(16);
    const dmMsg: Message = {
      id: 'm1',
      sender: { address: PEER, displayName: '' },
      text: 'quiet',
      timestamp: Date.now(),
      priority: 'quiet',
    };
    const { container } = render(QuietMessageGroup, {
      props: { messages: [dmMsg] } as any,
    });
    expect(container.querySelector('.quiet-summary')?.textContent).toContain(PEER.slice(0, 8));
  });
});
