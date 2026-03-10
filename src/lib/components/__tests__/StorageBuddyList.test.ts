import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import StorageBuddyList from '../StorageBuddyList.svelte';
import type { StorageBuddy, PeerRef } from '../../types';

const buddies: StorageBuddy[] = [
  {
    address: 'a1b2c3d4e5f6a1b2',
    displayName: 'Alice',
    storageUsedBytes: 1_200_000_000,
    online: true,
  },
  {
    address: 'b2c3d4e5f6a1b2c3',
    displayName: 'Bob',
    storageUsedBytes: 800_000_000,
    online: false,
  },
];

const availablePeers: PeerRef[] = [
  { address: 'a1b2c3d4e5f6a1b2', displayName: 'Alice' },
  { address: 'b2c3d4e5f6a1b2c3', displayName: 'Bob' },
  { address: 'c3d4e5f6a1b2c3d4', displayName: 'Carol' },
  { address: 'd4e5f6a1b2c3d4e5', displayName: 'Dave' },
];

function renderBuddyList(
  overrides: {
    buddies?: StorageBuddy[];
    availablePeers?: PeerRef[];
    onAdd?: (peer: PeerRef) => void;
    onRemove?: (buddy: StorageBuddy) => void;
  } = {},
) {
  return render(StorageBuddyList, {
    props: {
      buddies: overrides.buddies ?? buddies,
      availablePeers: overrides.availablePeers ?? availablePeers,
      onAdd: overrides.onAdd ?? vi.fn(),
      onRemove: overrides.onRemove ?? vi.fn(),
    },
  });
}

describe('StorageBuddyList', () => {
  it('renders section header with sealed envelope icon', () => {
    renderBuddyList();
    expect(screen.getByText(/Stored by/)).toBeTruthy();
    expect(screen.getByText(/encrypted/)).toBeTruthy();
  });

  it('renders as a section with appropriate aria-label', () => {
    const { container } = renderBuddyList();
    const section = container.querySelector('section[aria-label="Stored by (encrypted)"]');
    expect(section).toBeTruthy();
  });

  it('renders each buddy by name', () => {
    renderBuddyList();
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('Bob')).toBeTruthy();
  });

  it('shows formatted storage usage for each buddy', () => {
    renderBuddyList();
    expect(screen.getByText(/storing 1\.2 GB/)).toBeTruthy();
    expect(screen.getByText(/storing 800\.0 MB/)).toBeTruthy();
  });

  it('renders buddies in an unordered list', () => {
    const { container } = renderBuddyList();
    const list = container.querySelector('ul.buddy-peer-list');
    expect(list).toBeTruthy();
    const items = list!.querySelectorAll('li');
    expect(items.length).toBe(2);
  });

  it('shows online indicator with aria-label for online buddy', () => {
    renderBuddyList();
    const onlineIndicator = screen.getByLabelText('Alice is online');
    expect(onlineIndicator).toBeTruthy();
  });

  it('shows offline indicator with aria-label for offline buddy', () => {
    renderBuddyList();
    const offlineIndicator = screen.getByLabelText('Bob is offline');
    expect(offlineIndicator).toBeTruthy();
  });

  it('renders Remove button for each buddy', () => {
    renderBuddyList();
    const removeButtons = screen.getAllByRole('button', { name: /Remove/ });
    expect(removeButtons.length).toBe(2);
  });

  it('fires onRemove with the correct buddy when Remove is clicked', async () => {
    const onRemove = vi.fn();
    renderBuddyList({ onRemove });
    const removeButtons = screen.getAllByRole('button', { name: /Remove/ });
    await fireEvent.click(removeButtons[0]);
    expect(onRemove).toHaveBeenCalledOnce();
    expect(onRemove).toHaveBeenCalledWith(buddies[0]);
  });

  it('renders "Add buddy" contact picker as a select dropdown', () => {
    renderBuddyList();
    const select = screen.getByLabelText('Add buddy');
    expect(select).toBeTruthy();
    expect(select.tagName).toBe('SELECT');
  });

  it('only shows peers not already buddies in the picker', () => {
    renderBuddyList();
    const select = screen.getByLabelText('Add buddy');
    const options = select.querySelectorAll('option:not([disabled])');
    const optionTexts = Array.from(options).map((o) => o.textContent);
    expect(optionTexts).toContain('Carol');
    expect(optionTexts).toContain('Dave');
    expect(optionTexts).not.toContain('Alice');
    expect(optionTexts).not.toContain('Bob');
  });

  it('fires onAdd when a peer is selected from the picker', async () => {
    const onAdd = vi.fn();
    renderBuddyList({ onAdd });
    const select = screen.getByLabelText('Add buddy');
    await fireEvent.change(select, { target: { value: 'c3d4e5f6a1b2c3d4' } });
    expect(onAdd).toHaveBeenCalledOnce();
    expect(onAdd).toHaveBeenCalledWith({ address: 'c3d4e5f6a1b2c3d4', displayName: 'Carol' });
  });

  it('shows empty state when no buddies exist', () => {
    renderBuddyList({ buddies: [] });
    expect(screen.getByText(/No storage buddies/)).toBeTruthy();
  });

  it('hides picker when all available peers are already buddies', () => {
    const allBuddies: StorageBuddy[] = availablePeers.map((p) => ({
      ...p,
      storageUsedBytes: 100_000,
      online: true,
    }));
    renderBuddyList({ buddies: allBuddies });
    expect(screen.queryByLabelText('Add buddy')).toBeNull();
  });

  it('Remove buttons are native <button> elements', () => {
    renderBuddyList();
    const removeButtons = screen.getAllByRole('button', { name: /Remove/ });
    for (const btn of removeButtons) {
      expect(btn.tagName).toBe('BUTTON');
    }
  });

  it('shows replica reassignment warning on remove button title', () => {
    renderBuddyList();
    const removeButtons = screen.getAllByRole('button', { name: /Remove/ });
    for (const btn of removeButtons) {
      expect(btn.getAttribute('title')).toMatch(/replica/i);
    }
  });

  it('renders envelope icon in the header', () => {
    const { container } = renderBuddyList();
    const icon = container.querySelector('.buddy-list-header .header-icon');
    expect(icon).toBeTruthy();
  });
});
