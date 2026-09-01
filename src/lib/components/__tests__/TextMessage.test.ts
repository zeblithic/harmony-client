import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import TextMessage from '../TextMessage.svelte';
import type { Message } from '../../types';

const mockMessage: Message = {
  id: 'test-1',
  sender: { address: 'abc123', displayName: 'Alice' },
  text: 'Hello world',
  timestamp: new Date('2026-03-04T12:00:00Z').getTime(),
  priority: 'standard',
};

describe('TextMessage', () => {
  it('renders sender name and message text', () => {
    render(TextMessage, { props: { message: mockMessage } });
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('Hello world')).toBeTruthy();
  });

  it('renders loud message with accent border class', () => {
    const loudMsg: Message = {
      ...mockMessage,
      id: 'loud-1',
      priority: 'loud',
    };
    const { container } = render(TextMessage, { props: { message: loudMsg } });
    const el = container.querySelector('.text-message');
    expect(el?.classList.contains('loud')).toBe(true);
  });

  it('does not add loud class to standard messages', () => {
    const { container } = render(TextMessage, { props: { message: mockMessage } });
    const el = container.querySelector('.text-message');
    expect(el?.classList.contains('loud')).toBe(false);
  });

  it('shows reply-to header when replyTo is set', () => {
    const replyMsg: Message = {
      ...mockMessage,
      id: 'reply-1',
      replyTo: 'parent-1',
    };
    const parentMsg: Message = {
      ...mockMessage,
      id: 'parent-1',
      text: 'This is the parent message with some long text that should be truncated',
    };
    render(TextMessage, {
      props: {
        message: replyMsg,
        allMessages: [parentMsg, replyMsg],
      },
    });
    expect(screen.getByText(/↩/)).toBeTruthy();
    expect(screen.getAllByText(/Alice/).length).toBeGreaterThanOrEqual(2);
  });

  it('does not show reply-to header when replyTo is not set', () => {
    render(TextMessage, {
      props: { message: mockMessage },
    });
    expect(screen.queryByText(/↩/)).toBeNull();
  });
});

// ZEB-228 Phase 4 Task 14 — inline manual delete on stuck/expired DM messages.
// Self-Messages whose lifecycle has stalled (expired/failed/sending>60s) get an
// inline ⓧ button. The button is only meaningful for self-sent DM messages that
// already have a `messageId` (the OutboxEntryId used to correlate the
// `delete_outbox_entry` IPC).
describe('TextMessage delete button (ZEB-228 Phase 4)', () => {
  const baseSelfMessage: Message = {
    id: 'self-1',
    sender: { address: 'self', displayName: 'You' },
    text: 'hello',
    timestamp: Date.now(),
    priority: 'standard',
    messageId: 'mid1',
  };

  it('shows delete button for self-Message in expired state', () => {
    render(TextMessage, {
      props: {
        message: { ...baseSelfMessage, deliveryState: 'expired' },
        isSelf: true,
        onDelete: vi.fn(),
      },
    });
    expect(screen.queryByLabelText(/delete this message/i)).toBeTruthy();
  });

  it('shows delete button for self-Message in failed state', () => {
    render(TextMessage, {
      props: {
        message: { ...baseSelfMessage, deliveryState: 'failed' },
        isSelf: true,
        onDelete: vi.fn(),
      },
    });
    expect(screen.queryByLabelText(/delete this message/i)).toBeTruthy();
  });

  it('shows delete button for self-Message stuck in sending > 60s', () => {
    render(TextMessage, {
      props: {
        message: {
          ...baseSelfMessage,
          deliveryState: 'sending',
          timestamp: Date.now() - 70_000,
        },
        isSelf: true,
        onDelete: vi.fn(),
      },
    });
    expect(screen.queryByLabelText(/delete this message/i)).toBeTruthy();
  });

  it('hides delete button for self-Message in sending state under 60s', () => {
    render(TextMessage, {
      props: {
        message: {
          ...baseSelfMessage,
          deliveryState: 'sending',
          timestamp: Date.now() - 5_000,
        },
        isSelf: true,
        onDelete: vi.fn(),
      },
    });
    expect(screen.queryByLabelText(/delete this message/i)).toBeNull();
  });

  it('hides delete button for received messages', () => {
    render(TextMessage, {
      props: {
        message: { ...baseSelfMessage, deliveryState: 'expired' },
        isSelf: false,
        onDelete: vi.fn(),
      },
    });
    expect(screen.queryByLabelText(/delete this message/i)).toBeNull();
  });

  it('hides delete button for delivered self-Message', () => {
    render(TextMessage, {
      props: {
        message: { ...baseSelfMessage, deliveryState: 'delivered' },
        isSelf: true,
        onDelete: vi.fn(),
      },
    });
    expect(screen.queryByLabelText(/delete this message/i)).toBeNull();
  });

  it('hides delete button when messageId is undefined', () => {
    const noIdMsg: Message = {
      ...baseSelfMessage,
      deliveryState: 'expired',
    };
    delete noIdMsg.messageId;
    render(TextMessage, {
      props: {
        message: noIdMsg,
        isSelf: true,
        onDelete: vi.fn(),
      },
    });
    expect(screen.queryByLabelText(/delete this message/i)).toBeNull();
  });

  it('calls onDelete with messageId when clicked', async () => {
    const onDelete = vi.fn();
    render(TextMessage, {
      props: {
        message: { ...baseSelfMessage, deliveryState: 'expired' },
        isSelf: true,
        onDelete,
      },
    });
    const btn = screen.getByLabelText(/delete this message/i);
    await fireEvent.click(btn);
    expect(onDelete).toHaveBeenCalledWith('mid1');
  });
});
