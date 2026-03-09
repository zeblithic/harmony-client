import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import StorageBuddySummary from '../StorageBuddySummary.svelte';
import type { StorageBuddy } from '../../types';

const twoBuddies: StorageBuddy[] = [
  {
    address: 'addr-alice',
    displayName: 'Alice',
    storageUsedBytes: 1_200_000_000,
    online: true,
  },
  {
    address: 'addr-bob',
    displayName: 'Bob',
    storageUsedBytes: 800_000_000,
    online: false,
  },
];

describe('StorageBuddySummary', () => {
  it('renders buddy count', () => {
    render(StorageBuddySummary, { props: { buddies: twoBuddies } });
    expect(screen.getByText(/2 buddies/)).toBeTruthy();
  });

  it('renders online count', () => {
    render(StorageBuddySummary, { props: { buddies: twoBuddies } });
    expect(screen.getByText(/1 online/)).toBeTruthy();
  });

  it('renders Manage button', () => {
    render(StorageBuddySummary, { props: { buddies: twoBuddies } });
    expect(screen.getByRole('button', { name: /Manage/ })).toBeTruthy();
  });

  it('fires onManageBuddies when Manage is clicked', async () => {
    const onManageBuddies = vi.fn();
    render(StorageBuddySummary, { props: { buddies: twoBuddies, onManageBuddies } });
    await fireEvent.click(screen.getByRole('button', { name: /Manage/ }));
    expect(onManageBuddies).toHaveBeenCalledOnce();
  });

  it('shows 0 buddies when list is empty', () => {
    render(StorageBuddySummary, { props: { buddies: [] } });
    expect(screen.getByText(/0 buddies/)).toBeTruthy();
    expect(screen.getByText(/0 online/)).toBeTruthy();
  });

  it('shows all online when all buddies are online', () => {
    const allOnline: StorageBuddy[] = [
      { ...twoBuddies[0], online: true },
      { ...twoBuddies[1], online: true },
    ];
    render(StorageBuddySummary, { props: { buddies: allOnline } });
    expect(screen.getByText(/2 online/)).toBeTruthy();
  });

  it('renders as a section with appropriate aria-label', () => {
    const { container } = render(StorageBuddySummary, { props: { buddies: twoBuddies } });
    const section = container.querySelector('section[aria-label="Storage buddies"]');
    expect(section).toBeTruthy();
  });

  it('Manage control is a native button (keyboard accessible by default)', () => {
    render(StorageBuddySummary, { props: { buddies: twoBuddies } });
    const manageBtn = screen.getByRole('button', { name: /Manage/ });
    // Native <button> elements handle Enter and Space activation natively
    expect(manageBtn.tagName).toBe('BUTTON');
  });

  it('shows plural correctly for 1 buddy', () => {
    const oneBuddy: StorageBuddy[] = [twoBuddies[0]];
    render(StorageBuddySummary, { props: { buddies: oneBuddy } });
    expect(screen.getByText(/1 buddy(?!s)/)).toBeTruthy();
  });
});
