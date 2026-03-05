import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import ProfilePopover from '../ProfilePopover.svelte';
import type { Profile } from '../../types';

const mockProfile: Profile = {
  address: 'a1b2c3d4',
  displayName: 'Alice',
  statusText: 'Working on transport layer',
};

describe('ProfilePopover', () => {
  it('renders display name and status text', () => {
    render(ProfilePopover, {
      props: {
        profile: mockProfile,
        x: 100,
        y: 100,
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('Working on transport layer')).toBeTruthy();
  });

  it('renders truncated peer address', () => {
    render(ProfilePopover, {
      props: {
        profile: mockProfile,
        x: 100,
        y: 100,
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('a1b2c3d4')).toBeTruthy();
  });

  it('renders sound slot labels', () => {
    render(ProfilePopover, {
      props: {
        profile: mockProfile,
        x: 100,
        y: 100,
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('Quiet')).toBeTruthy();
    expect(screen.getByText('Standard')).toBeTruthy();
    expect(screen.getByText('Loud')).toBeTruthy();
  });

  it('shows "System default" when no custom sounds set', () => {
    render(ProfilePopover, {
      props: {
        profile: mockProfile,
        x: 100,
        y: 100,
        onClose: vi.fn(),
      },
    });
    const defaults = screen.getAllByText('System default');
    expect(defaults.length).toBe(3);
  });

  it('calls onClose when Escape is pressed', async () => {
    const onClose = vi.fn();
    render(ProfilePopover, {
      props: {
        profile: mockProfile,
        x: 100,
        y: 100,
        onClose,
      },
    });
    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(onClose).toHaveBeenCalled();
  });

  it('handles profile without status text', () => {
    const noStatus: Profile = { address: 'xyz789', displayName: 'Bob' };
    render(ProfilePopover, {
      props: {
        profile: noStatus,
        x: 100,
        y: 100,
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('Bob')).toBeTruthy();
    expect(screen.queryByText('Working on transport layer')).toBeNull();
  });
});
