import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import ConnectionBar from '../ConnectionBar.svelte';

describe('ConnectionBar', () => {
  it('renders endpoint input with default value', () => {
    render(ConnectionBar, {
      props: {
        connectionStatus: 'disconnected',
        discoveredCount: 0,
        onConnect: vi.fn(),
        onDisconnect: vi.fn(),
      },
    });
    const input = screen.getByLabelText('Zenoh endpoint') as HTMLInputElement;
    expect(input.value).toBe('tcp/127.0.0.1:7447');
  });

  it('renders connect button when disconnected', () => {
    render(ConnectionBar, {
      props: {
        connectionStatus: 'disconnected',
        discoveredCount: 0,
        onConnect: vi.fn(),
        onDisconnect: vi.fn(),
      },
    });
    expect(screen.getByText('Connect')).toBeTruthy();
  });

  it('renders disconnect button when connected', () => {
    render(ConnectionBar, {
      props: {
        connectionStatus: 'connected',
        discoveredCount: 2,
        onConnect: vi.fn(),
        onDisconnect: vi.fn(),
      },
    });
    expect(screen.getByText('Disconnect')).toBeTruthy();
  });

  it('shows discovered count when connected', () => {
    render(ConnectionBar, {
      props: {
        connectionStatus: 'connected',
        discoveredCount: 3,
        onConnect: vi.fn(),
        onDisconnect: vi.fn(),
      },
    });
    const status = screen.getByRole('status');
    expect(status.getAttribute('aria-label')).toContain('3 nodes discovered');
  });

  it('shows error message', () => {
    render(ConnectionBar, {
      props: {
        connectionStatus: 'error',
        discoveredCount: 0,
        onConnect: vi.fn(),
        onDisconnect: vi.fn(),
        errorMessage: 'connection refused',
      },
    });
    const status = screen.getByRole('status');
    expect(status.getAttribute('aria-label')).toContain('connection refused');
  });

  it('disables input when connected', () => {
    render(ConnectionBar, {
      props: {
        connectionStatus: 'connected',
        discoveredCount: 0,
        onConnect: vi.fn(),
        onDisconnect: vi.fn(),
      },
    });
    const input = screen.getByLabelText('Zenoh endpoint') as HTMLInputElement;
    expect(input.disabled).toBe(true);
  });
});
