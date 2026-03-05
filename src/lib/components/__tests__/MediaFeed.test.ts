import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import MediaFeed from '../MediaFeed.svelte';
import type { Message } from '../../types';

const messagesWithMedia: Message[] = [
  {
    id: 'msg-1',
    sender: { address: 'abc', displayName: 'Alice' },
    text: 'Image here',
    timestamp: Date.now(),
    media: [{ id: 'img-1', type: 'image', url: 'https://example.com/img.png', title: 'Screenshot' }],
    priority: 'standard',
  },
  {
    id: 'msg-2',
    sender: { address: 'def', displayName: 'Bob' },
    text: 'No media',
    timestamp: Date.now(),
    media: [],
    priority: 'standard',
  },
  {
    id: 'msg-3',
    sender: { address: 'ghi', displayName: 'Carol' },
    text: 'Link share',
    timestamp: Date.now(),
    media: [{ id: 'link-1', type: 'link', url: 'https://example.com', title: 'Example', domain: 'example.com' }],
    priority: 'standard',
  },
];

describe('MediaFeed', () => {
  it('renders only messages that have media', () => {
    render(MediaFeed, { props: { messages: messagesWithMedia } });
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('Carol')).toBeTruthy();
    expect(screen.queryByText('Bob')).toBeNull();
  });

  it('shows empty state when no media exists', () => {
    const noMedia: Message[] = [
      { id: 'm1', sender: { address: 'x', displayName: 'X' }, text: 'hi', timestamp: Date.now(), media: [], priority: 'standard' },
    ];
    render(MediaFeed, { props: { messages: noMedia } });
    expect(screen.getByText('No media yet')).toBeTruthy();
  });
});
