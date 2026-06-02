import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent, cleanup } from '@testing-library/svelte';
import { afterEach } from 'vitest';
import IncomingCallToast from '../IncomingCallToast.svelte';

afterEach(() => { cleanup(); });

describe('IncomingCallToast', () => {
  it('renders nothing when incomingCall is null', () => {
    render(IncomingCallToast, {
      props: {
        incomingCall: null,
        onAccept: vi.fn(),
        onDecline: vi.fn(),
      },
    });
    expect(screen.queryByTestId('incoming-call')).toBeNull();
  });

  it('renders caller name and "Incoming call" when incomingCall is set', () => {
    render(IncomingCallToast, {
      props: {
        incomingCall: { callId: 'call-1', callerName: 'Alice' },
        onAccept: vi.fn(),
        onDecline: vi.fn(),
      },
    });
    expect(screen.getByTestId('incoming-call')).toBeInTheDocument();
    expect(screen.getByText('Alice')).toBeInTheDocument();
    expect(screen.getByText('Incoming call')).toBeInTheDocument();
  });

  it('calls onAccept with the callId when Accept button is clicked', async () => {
    const onAccept = vi.fn();
    render(IncomingCallToast, {
      props: {
        incomingCall: { callId: 'call-42', callerName: 'Bob' },
        onAccept,
        onDecline: vi.fn(),
      },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Accept call' }));
    expect(onAccept).toHaveBeenCalledWith('call-42');
  });

  it('calls onDecline with the callId when Decline button is clicked', async () => {
    const onDecline = vi.fn();
    render(IncomingCallToast, {
      props: {
        incomingCall: { callId: 'call-99', callerName: 'Carol' },
        onAccept: vi.fn(),
        onDecline,
      },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Decline call' }));
    expect(onDecline).toHaveBeenCalledWith('call-99');
  });
});
