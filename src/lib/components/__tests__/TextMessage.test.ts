import { render, screen } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import TextMessage from '../TextMessage.svelte';
import type { Message } from '../../types';

const mockMessage: Message = {
  id: 'test-1',
  sender: { address: 'abc123', displayName: 'Alice' },
  text: 'Hello world',
  timestamp: new Date('2026-03-04T12:00:00Z').getTime(),
  media: [],
  priority: 'standard',
};

const mockMessageWithMedia: Message = {
  id: 'test-2',
  sender: { address: 'def456', displayName: 'Bob' },
  text: 'Check this out',
  timestamp: new Date('2026-03-04T12:05:00Z').getTime(),
  media: [
    {
      id: 'media-1',
      type: 'link',
      url: 'https://example.com',
      title: 'Example Site',
      domain: 'example.com',
    },
  ],
  priority: 'standard',
};

describe('TextMessage', () => {
  it('renders sender name and message text', () => {
    render(TextMessage, { props: { message: mockMessage } });
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('Hello world')).toBeTruthy();
  });

  it('shows media indicator pill when not collapsed', () => {
    render(TextMessage, {
      props: { message: mockMessageWithMedia, collapsed: false },
    });
    expect(screen.getByText('example.com')).toBeTruthy();
  });

  it('shows inline embed when collapsed', () => {
    render(TextMessage, {
      props: { message: mockMessageWithMedia, collapsed: true },
    });
    expect(screen.getByText('Example Site')).toBeTruthy();
  });

  it('calls onMediaClick when pill is clicked', async () => {
    const onClick = vi.fn();
    render(TextMessage, {
      props: {
        message: mockMessageWithMedia,
        collapsed: false,
        onMediaClick: onClick,
      },
    });
    const pill = screen.getByText('example.com').closest('button');
    pill?.click();
    expect(onClick).toHaveBeenCalledWith('media-1');
  });
});
