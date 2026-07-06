import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import IdentityChip from '../IdentityChip.svelte';

describe('IdentityChip (ZEB-606)', () => {
  it('renders two-word initials, name, and the self-sovereign microline', () => {
    const { container } = render(IdentityChip, {
      props: { displayName: 'Jake Englund', ownerIdHex: 'ab'.repeat(16), selfOnline: true, selfSovereign: true },
    });
    expect(container.querySelector('.chip-avatar')?.textContent).toContain('JE');
    expect(screen.getByText('Jake Englund')).toBeTruthy();
    expect(screen.getByText('● self-sovereign')).toBeTruthy();
    expect(container.querySelector('.presence-ring')).toBeTruthy();
  });

  it('single-word names use their first two characters', () => {
    const { container } = render(IdentityChip, {
      props: { displayName: 'zeblith', ownerIdHex: null },
    });
    expect(container.querySelector('.chip-avatar')?.textContent).toContain('ZE');
  });

  it('empty name falls back to the owner id prefix for initials and name', () => {
    const { container } = render(IdentityChip, {
      props: { displayName: '', ownerIdHex: 'deadbeef' + 'ab'.repeat(12) },
    });
    expect(container.querySelector('.chip-avatar')?.textContent).toContain('DE');
    expect(screen.getByText('deadbeef…')).toBeTruthy();
  });

  it('hides the ring and microline when offline / not self-sovereign', () => {
    const { container } = render(IdentityChip, {
      props: { displayName: 'Jake', ownerIdHex: null, selfOnline: false, selfSovereign: false },
    });
    expect(container.querySelector('.presence-ring')).toBeNull();
    expect(screen.queryByText('● self-sovereign')).toBeNull();
  });
});
