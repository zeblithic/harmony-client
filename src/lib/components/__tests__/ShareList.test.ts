import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import ShareList from '../ShareList.svelte';
import type { PeerRef } from '../../types';

const sharedWith: PeerRef[] = [
  { address: 'a1b2c3d4e5f6a1b2', displayName: 'Alice' },
  { address: 'b2c3d4e5f6a1b2c3', displayName: 'Bob' },
];

const availablePeers: PeerRef[] = [
  { address: 'a1b2c3d4e5f6a1b2', displayName: 'Alice' },
  { address: 'b2c3d4e5f6a1b2c3', displayName: 'Bob' },
  { address: 'c3d4e5f6a1b2c3d4', displayName: 'Carol' },
  { address: 'd4e5f6a1b2c3d4e5', displayName: 'Dave' },
];

function renderShareList(
  overrides: {
    sharedWith?: PeerRef[];
    availablePeers?: PeerRef[];
    onAdd?: (peer: PeerRef) => void;
    onRemove?: (peer: PeerRef) => void;
  } = {},
) {
  return render(ShareList, {
    props: {
      sharedWith: overrides.sharedWith ?? sharedWith,
      availablePeers: overrides.availablePeers ?? availablePeers,
      onAdd: overrides.onAdd ?? vi.fn(),
      onRemove: overrides.onRemove ?? vi.fn(),
    },
  });
}

describe('ShareList', () => {
  it('renders section header with unlocked lock icon', () => {
    renderShareList();
    const heading = screen.getByText(/Shared with/);
    expect(heading).toBeTruthy();
    // The header contains "(can view)" as a child
    expect(heading.textContent).toMatch(/can view/);
    expect(heading.closest('.share-list-header')).toBeTruthy();
  });

  it('renders as a section with appropriate aria-label', () => {
    const { container } = renderShareList();
    const section = container.querySelector('section[aria-label="Shared with (can view)"]');
    expect(section).toBeTruthy();
  });

  it('renders each shared peer by name', () => {
    renderShareList();
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('Bob')).toBeTruthy();
  });

  it('shows "can view" label for each shared peer', () => {
    renderShareList();
    const labels = screen.getAllByText('can view');
    expect(labels.length).toBe(2);
  });

  it('renders peers in an unordered list', () => {
    const { container } = renderShareList();
    const list = container.querySelector('ul.share-peer-list');
    expect(list).toBeTruthy();
    const items = list!.querySelectorAll('li');
    expect(items.length).toBe(2);
  });

  it('renders Remove button for each peer', () => {
    renderShareList();
    const removeButtons = screen.getAllByRole('button', { name: /Remove/ });
    expect(removeButtons.length).toBe(2);
  });

  it('fires onRemove with the correct peer when Remove is clicked', async () => {
    const onRemove = vi.fn();
    renderShareList({ onRemove });
    const removeButtons = screen.getAllByRole('button', { name: /Remove/ });
    await fireEvent.click(removeButtons[0]);
    expect(onRemove).toHaveBeenCalledOnce();
    expect(onRemove).toHaveBeenCalledWith(sharedWith[0]);
  });

  it('renders "Share with..." contact picker as a select dropdown', () => {
    renderShareList();
    const select = screen.getByLabelText('Share with...');
    expect(select).toBeTruthy();
    expect(select.tagName).toBe('SELECT');
  });

  it('only shows peers not already shared with in the picker', () => {
    renderShareList();
    const select = screen.getByLabelText('Share with...');
    const options = select.querySelectorAll('option:not([disabled])');
    // Alice and Bob are already shared with, so Carol and Dave remain
    const optionTexts = Array.from(options).map((o) => o.textContent);
    expect(optionTexts).toContain('Carol');
    expect(optionTexts).toContain('Dave');
    expect(optionTexts).not.toContain('Alice');
    expect(optionTexts).not.toContain('Bob');
  });

  it('fires onAdd when a peer is selected from the picker', async () => {
    const onAdd = vi.fn();
    renderShareList({ onAdd });
    const select = screen.getByLabelText('Share with...');
    await fireEvent.change(select, { target: { value: 'c3d4e5f6a1b2c3d4' } });
    expect(onAdd).toHaveBeenCalledOnce();
    expect(onAdd).toHaveBeenCalledWith({ address: 'c3d4e5f6a1b2c3d4', displayName: 'Carol' });
  });

  it('shows empty state when no peers are shared with', () => {
    renderShareList({ sharedWith: [] });
    expect(screen.getByText(/Not shared with anyone/)).toBeTruthy();
  });

  it('hides picker when all available peers are already shared with', () => {
    renderShareList({ sharedWith: availablePeers });
    expect(screen.queryByLabelText('Share with...')).toBeNull();
  });

  it('Remove buttons are native <button> elements', () => {
    renderShareList();
    const removeButtons = screen.getAllByRole('button', { name: /Remove/ });
    for (const btn of removeButtons) {
      expect(btn.tagName).toBe('BUTTON');
    }
  });

  it('renders unlock icon in the header', () => {
    const { container } = renderShareList();
    const icon = container.querySelector('.share-list-header .header-icon');
    expect(icon).toBeTruthy();
  });
});
