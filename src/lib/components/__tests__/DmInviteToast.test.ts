import { render, fireEvent } from '@testing-library/svelte';
import { describe, expect, it, vi } from 'vitest';
import DmInviteToast from '../DmInviteToast.svelte';

const invite = { spaceIdHex: 'a1', inviterOwnerIdHex: 'deadbeefdeadbeefdeadbeefdeadbeef',
  kind: 'd' as const, memberOwnerIdsHex: [], createdAtMs: 1, receivedAtMs: 2 };

describe('DmInviteToast', () => {
  it('renders inviter short-hex + mapped kind label and fires the three callbacks', async () => {
    const onAccept = vi.fn(); const onDecline = vi.fn(); const onLater = vi.fn();
    const { getByText, getByTestId } = render(DmInviteToast, {
      props: { invite, onAccept, onDecline, onLater },
    });
    expect(getByText(/deadbeef/)).toBeTruthy();   // short-hex display
    // The 'd' wire tag renders as the human label "DM", not the raw tag.
    expect(getByText(/\(DM\)/)).toBeTruthy();
    await fireEvent.click(getByTestId('dm-invite-accept'));
    await fireEvent.click(getByTestId('dm-invite-decline'));
    await fireEvent.click(getByTestId('dm-invite-later'));
    expect(onAccept).toHaveBeenCalledOnce();
    expect(onDecline).toHaveBeenCalledOnce();
    expect(onLater).toHaveBeenCalledOnce();
  });

  // ZEB-961: resolve the inviter's broadcast card name when a resolver is
  // provided (the inviter may have published a profile card even as a non-friend).
  it('resolves the inviter card name over hex when resolveCard is provided', () => {
    const { getByText, queryByText } = render(DmInviteToast, {
      props: {
        invite,
        onAccept: vi.fn(),
        onDecline: vi.fn(),
        onLater: vi.fn(),
        resolveCard: (id: string) =>
          id === invite.inviterOwnerIdHex ? { displayName: 'Zeb', statusText: '' } : undefined,
      },
    });
    // ZEB-977: the name renders inside <PeerName>, so "From" and the label
    // are separate elements — assert on the resolved name node itself.
    expect(getByText('Zeb')).toBeTruthy();
    // The hex must not show once the card name resolves.
    expect(queryByText(/deadbeef/)).toBeNull();
  });

  // ZEB-977: the petname rung outranks the card name, provenance-styled.
  it('prefers the local petname over the card name and badges it', () => {
    const { getByText, container } = render(DmInviteToast, {
      props: {
        invite,
        onAccept: vi.fn(),
        onDecline: vi.fn(),
        onLater: vi.fn(),
        resolveNickname: (id: string) =>
          id === invite.inviterOwnerIdHex ? 'Zebby' : undefined,
        resolveCard: (id: string) =>
          id === invite.inviterOwnerIdHex ? { displayName: 'Zeb', statusText: '' } : undefined,
      },
    });
    const name = getByText('Zebby');
    expect(name.closest('.peer-name')?.getAttribute('data-name-source')).toBe('petname');
    expect(container.querySelector('.petname-badge')).not.toBeNull();
  });
});
