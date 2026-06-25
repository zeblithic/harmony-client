import { describe, it, expect, vi } from 'vitest';
import { render } from '@testing-library/svelte';
import ProfileEditor from '../ProfileEditor.svelte';
import type { Profile } from '../../types';

// Tag each identicon with the seed it was generated from so we can assert WHICH
// id the self-avatar was seeded with (the identicon itself is opaque otherwise).
vi.mock('../../identicon', () => ({
  generateIdenticon: (address: string, size = 64) =>
    `<svg data-seed="${address}" data-size="${size}"></svg>`,
}));

// ProfileEditor dynamically imports Tauri core + the avatar normalizer; stub
// both so a bare render() doesn't touch the IPC boundary.
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn(async () => undefined) }));
vi.mock('../../avatar-normalize', () => ({
  normalizeAvatar: vi.fn(async () => new Uint8Array([1, 2, 3])),
  validateAvatarInput: vi.fn(),
  AVATAR_MAX_INPUT_BYTES: 10485760,
  AVATAR_EDGE: 256,
}));

// `address` is the random local placeholder; OWNER_ID is the canonical owner_id
// hex that members-list / chat seed from.
const profile: Profile = {
  address: 'deadbeef01020304',
  displayName: 'Alice',
  statusText: '',
};
const OWNER_ID = '00112233445566778899aabbccddeeff';

function seedOf(container: HTMLElement): string | null | undefined {
  return container.querySelector('.avatar-preview svg')?.getAttribute('data-seed');
}

describe('ProfileEditor self-avatar seed (ZEB-567)', () => {
  it('seeds the identicon from ownerIdHex (canonical owner_id) when provided', () => {
    const { container } = render(ProfileEditor, {
      props: { profile, onSave: vi.fn(), ownerIdHex: OWNER_ID },
    });
    // Must match members/chat (owner_id), NOT the random placeholder address.
    expect(seedOf(container)).toBe(OWNER_ID);
    expect(seedOf(container)).not.toBe(profile.address);
  });

  it('falls back to profile.address when ownerIdHex is absent', () => {
    const { container } = render(ProfileEditor, {
      props: { profile, onSave: vi.fn() },
    });
    expect(seedOf(container)).toBe(profile.address);
  });
});
