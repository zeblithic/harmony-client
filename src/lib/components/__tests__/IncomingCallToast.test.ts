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
        incomingCall: { callId: 'call-1', callerName: { label: 'Alice', source: 'card' } },
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
        incomingCall: { callId: 'call-42', callerName: { label: 'Bob', source: 'card' } },
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
        incomingCall: { callId: 'call-99', callerName: { label: 'Carol', source: 'card' } },
        onAccept: vi.fn(),
        onDecline,
      },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Decline call' }));
    expect(onDecline).toHaveBeenCalledWith('call-99');
  });

  // ZEB-980 — the toast renders the caller through PeerName, so a caller you've
  // petnamed keeps the "name you assigned" provenance badge on the incoming-call
  // surface (the one the ticket flags for impersonation pressure).
  it('shows the petname badge when the caller name is petname-sourced', () => {
    const { container } = render(IncomingCallToast, {
      props: {
        incomingCall: { callId: 'c', callerName: { label: 'Ziggy', source: 'petname' } },
        onAccept: vi.fn(),
        onDecline: vi.fn(),
      },
    });
    expect(screen.getByText('Ziggy')).toBeInTheDocument();
    expect(container.querySelector('.petname-badge')).not.toBeNull();
  });

  it('shows NO petname badge when the caller name is card-sourced', () => {
    const { container } = render(IncomingCallToast, {
      props: {
        incomingCall: { callId: 'c', callerName: { label: 'Ziggy', source: 'card' } },
        onAccept: vi.fn(),
        onDecline: vi.fn(),
      },
    });
    expect(screen.getByText('Ziggy')).toBeInTheDocument();
    expect(container.querySelector('.petname-badge')).toBeNull();
  });

  // ZEB-980 — the group-call toast passes the caller name (through PeerName) and
  // the group name as SEPARATE fields, replacing the old "caller · group" string
  // concat, so the caller's provenance badge survives.
  it('renders the group name suffix alongside a petname-badged caller (group call)', () => {
    const { container } = render(IncomingCallToast, {
      props: {
        incomingCall: {
          callId: 'g',
          callerName: { label: 'Ziggy', source: 'petname' },
          groupName: 'Crew',
        },
        onAccept: vi.fn(),
        onDecline: vi.fn(),
      },
    });
    expect(screen.getByText('Ziggy')).toBeInTheDocument();
    expect(screen.getByText(/· Crew/)).toBeInTheDocument();
    expect(container.querySelector('.petname-badge')).not.toBeNull();
  });
});
