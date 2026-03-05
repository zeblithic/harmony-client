import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import UntrustedMediaCard from '../UntrustedMediaCard.svelte';
import type { Message, MediaAttachment } from '../../types';

const mockMessage: Message = {
  id: 'msg-1',
  sender: { address: 'alice-addr', displayName: 'Alice' },
  text: 'Check this out',
  timestamp: 1709654400000,
  media: [],
  priority: 'standard',
};

const mockImageAttachment: MediaAttachment = {
  id: 'att-1',
  type: 'image',
  url: 'https://example.com/photo.jpg',
  title: 'A photo',
};

const mockLinkAttachment: MediaAttachment = {
  id: 'att-2',
  type: 'link',
  url: 'https://example.com/page',
  title: 'A page',
  domain: 'example.com',
};

describe('UntrustedMediaCard', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('renders blocked state with attachment type for images', () => {
    render(UntrustedMediaCard, {
      props: { message: mockMessage, attachment: mockImageAttachment },
    });
    expect(screen.getByText(/blocked media/i)).toBeTruthy();
    expect(screen.getByText('Alice')).toBeTruthy();
  });

  it('renders blocked state with attachment type for links', () => {
    render(UntrustedMediaCard, {
      props: { message: mockMessage, attachment: mockLinkAttachment },
    });
    expect(screen.getByText(/blocked media/i)).toBeTruthy();
  });

  it('does not display URL or domain', () => {
    render(UntrustedMediaCard, {
      props: { message: mockMessage, attachment: mockLinkAttachment },
    });
    expect(screen.queryByText('example.com')).toBeNull();
    expect(screen.queryByText('https://example.com/page')).toBeNull();
  });

  it('shows Show button in initial state', () => {
    render(UntrustedMediaCard, {
      props: { message: mockMessage, attachment: mockImageAttachment },
    });
    expect(screen.getByRole('button', { name: /^show$/i })).toBeTruthy();
  });

  it('transitions to disabled Confirm load after clicking Show', async () => {
    render(UntrustedMediaCard, {
      props: { message: mockMessage, attachment: mockImageAttachment },
    });
    await fireEvent.click(screen.getByRole('button', { name: /^show$/i }));
    const confirmBtn = screen.getByRole('button', { name: /confirm load/i });
    expect(confirmBtn).toBeTruthy();
    expect(confirmBtn.hasAttribute('disabled') || confirmBtn.getAttribute('aria-disabled') === 'true').toBe(true);
  });

  it('enables Confirm load button after 1 second cooldown', async () => {
    render(UntrustedMediaCard, {
      props: { message: mockMessage, attachment: mockImageAttachment },
    });
    await fireEvent.click(screen.getByRole('button', { name: /^show$/i }));
    vi.advanceTimersByTime(1000);
    await vi.advanceTimersByTimeAsync(0);
    const confirmBtn = screen.getByRole('button', { name: /confirm load/i });
    expect(confirmBtn.hasAttribute('disabled')).toBe(false);
    expect(confirmBtn.getAttribute('aria-disabled')).not.toBe('true');
  });

  it('fires onLoad with attachment ID when Confirm load is clicked after cooldown', async () => {
    const onLoad = vi.fn();
    render(UntrustedMediaCard, {
      props: { message: mockMessage, attachment: mockImageAttachment, onLoad },
    });
    await fireEvent.click(screen.getByRole('button', { name: /^show$/i }));
    vi.advanceTimersByTime(1000);
    await vi.advanceTimersByTimeAsync(0);
    await fireEvent.click(screen.getByRole('button', { name: /confirm load/i }));
    expect(onLoad).toHaveBeenCalledWith('att-1');
  });

  it('does not fire onLoad when Confirm load clicked during cooldown', async () => {
    const onLoad = vi.fn();
    render(UntrustedMediaCard, {
      props: { message: mockMessage, attachment: mockImageAttachment, onLoad },
    });
    await fireEvent.click(screen.getByRole('button', { name: /^show$/i }));
    // Don't advance time — still in cooldown
    const confirmBtn = screen.getByRole('button', { name: /confirm load/i });
    await fireEvent.click(confirmBtn);
    expect(onLoad).not.toHaveBeenCalled();
  });

  it('returns to blocked state when Cancel is clicked during cooldown', async () => {
    render(UntrustedMediaCard, {
      props: { message: mockMessage, attachment: mockImageAttachment },
    });
    await fireEvent.click(screen.getByRole('button', { name: /^show$/i }));
    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));
    expect(screen.getByRole('button', { name: /^show$/i })).toBeTruthy();
    expect(screen.queryByRole('button', { name: /confirm load/i })).toBeNull();
  });

  it('has correct aria-label for screen readers', () => {
    render(UntrustedMediaCard, {
      props: { message: mockMessage, attachment: mockImageAttachment },
    });
    const region = screen.getByLabelText(/blocked media.*image.*alice/i);
    expect(region).toBeTruthy();
  });
});
