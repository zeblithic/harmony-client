import { render, fireEvent } from '@testing-library/svelte';
import { describe, expect, it, vi } from 'vitest';
import DmInviteToast from '../DmInviteToast.svelte';

const invite = { spaceIdHex: 'a1', inviterOwnerIdHex: 'deadbeefdeadbeefdeadbeefdeadbeef',
  kind: 'dm', memberOwnerIdsHex: [], createdAtMs: 1, receivedAtMs: 2 };

describe('DmInviteToast', () => {
  it('renders inviter short-hex + kind and fires the three callbacks', async () => {
    const onAccept = vi.fn(); const onDecline = vi.fn(); const onLater = vi.fn();
    const { getByText, getByTestId } = render(DmInviteToast, {
      props: { invite, onAccept, onDecline, onLater },
    });
    expect(getByText(/deadbeef/)).toBeTruthy();   // short-hex display
    await fireEvent.click(getByTestId('dm-invite-accept'));
    await fireEvent.click(getByTestId('dm-invite-decline'));
    await fireEvent.click(getByTestId('dm-invite-later'));
    expect(onAccept).toHaveBeenCalledOnce();
    expect(onDecline).toHaveBeenCalledOnce();
    expect(onLater).toHaveBeenCalledOnce();
  });
});
