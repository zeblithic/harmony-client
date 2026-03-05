import { render, screen } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import ThreadView from '../ThreadView.svelte';
import { TrustService } from '../../trust-service';
import type { Message } from '../../types';

function trustedService(): TrustService {
  const ts = new TrustService();
  ts.setGlobalTrust('trusted');
  return ts;
}

const rootMsg: Message = {
  id: 'root-1',
  sender: { address: 'alice', displayName: 'Alice' },
  text: 'Check this out',
  timestamp: Date.now(),
  media: [],
  priority: 'standard',
};

const replies: Message[] = [
  {
    id: 'reply-1',
    sender: { address: 'bob', displayName: 'Bob' },
    text: 'Looks great!',
    timestamp: Date.now() + 1000,
    media: [],
    priority: 'standard',
    replyTo: 'root-1',
  },
  {
    id: 'reply-2',
    sender: { address: 'carol', displayName: 'Carol' },
    text: 'Nice work',
    timestamp: Date.now() + 2000,
    media: [],
    priority: 'standard',
    replyTo: 'root-1',
  },
];

describe('ThreadView', () => {
  it('renders thread root message', () => {
    render(ThreadView, {
      props: { rootMessage: rootMsg, replies: [], trustService: trustedService() },
    });
    expect(screen.getByText('Check this out')).toBeTruthy();
    expect(screen.getByText('Alice')).toBeTruthy();
  });

  it('renders reply messages', () => {
    render(ThreadView, {
      props: { rootMessage: rootMsg, replies, trustService: trustedService() },
    });
    expect(screen.getByText('Looks great!')).toBeTruthy();
    expect(screen.getByText('Nice work')).toBeTruthy();
  });

  it('calls onClose when close button is clicked', () => {
    const onClose = vi.fn();
    render(ThreadView, {
      props: { rootMessage: rootMsg, replies: [], onClose, trustService: trustedService() },
    });
    const closeBtn = screen.getByLabelText('Close thread');
    closeBtn.click();
    expect(onClose).toHaveBeenCalled();
  });

  it('has correct aria role and label', () => {
    const { container } = render(ThreadView, {
      props: { rootMessage: rootMsg, replies: [], trustService: trustedService() },
    });
    const section = container.querySelector('[role="complementary"]');
    expect(section).toBeTruthy();
    expect(section?.getAttribute('aria-label')).toContain('Thread');
  });

  it('renders compose bar with reply placeholder', () => {
    render(ThreadView, {
      props: { rootMessage: rootMsg, replies: [], trustService: trustedService() },
    });
    const textarea = screen.getByPlaceholderText(/Message #thread/);
    expect(textarea).toBeTruthy();
  });
});
