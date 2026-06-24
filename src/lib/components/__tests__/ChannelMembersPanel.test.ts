import { describe, it, expect } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import ChannelMembersPanel from '../ChannelMembersPanel.svelte';
import type { CommunityMember } from '../../types';
import type { ResolvedCard } from '../../member-card-service';

// 32-hex-char owner addresses (16-byte owner_id). The first 8 chars are the
// truncated-hex fallback the panel shows when nothing resolves.
const OWN = 'a'.repeat(32);
const PEER = 'deadbeef' + 'cafe1234'.repeat(3); // -> hex prefix "deadbeef"
const PEER2 = 'beadfeed' + 'cafe1234'.repeat(3); // -> hex prefix "beadfeed"

function member(over: Partial<CommunityMember> = {}): CommunityMember {
  return {
    address: PEER,
    displayName: undefined,
    power: 0,
    status: 'joined',
    joinedAt: 1700000000000,
    ...over,
  };
}

const self: CommunityMember = {
  address: OWN,
  displayName: undefined,
  power: 100,
  status: 'joined',
  joinedAt: 1699999999000,
};

function renderedNames(container: HTMLElement): string[] {
  return Array.from(container.querySelectorAll('.member-row .name')).map((el) =>
    (el.textContent ?? '').trim(),
  );
}

const baseProps = (over: Record<string, unknown> = {}) => ({
  members: [self, member()],
  ownAddress: OWN,
  collapsed: false,
  ...over,
});

describe('ChannelMembersPanel — ZEB-432 label ladder', () => {
  it('renders the resolved profile-card name instead of truncated owner hex', () => {
    const card: ResolvedCard = { displayName: 'ZEBbot', statusText: '' };
    const { container } = render(ChannelMembersPanel, {
      props: baseProps({ resolveCard: (id: string) => (id === PEER ? card : undefined) }),
    });
    const names = renderedNames(container);
    expect(names).toContain('ZEBbot');
    expect(names).not.toContain('deadbeef');
  });

  it('prefers a local friend nickname over the profile-card name', () => {
    const card: ResolvedCard = { displayName: 'ZEBbot', statusText: '' };
    const { container } = render(ChannelMembersPanel, {
      props: baseProps({
        resolveCard: (id: string) => (id === PEER ? card : undefined),
        resolveNickname: (id: string) => (id === PEER ? 'Bestie' : undefined),
      }),
    });
    const names = renderedNames(container);
    expect(names).toContain('Bestie');
    expect(names).not.toContain('ZEBbot');
  });

  it('falls back to backend displayName, then truncated hex, when no card resolves', () => {
    const { container } = render(ChannelMembersPanel, {
      props: baseProps({
        members: [
          self,
          member({ address: PEER, displayName: 'BackendName' }),
          member({ address: PEER2, displayName: undefined }),
        ],
      }),
    });
    const names = renderedNames(container);
    expect(names).toContain('BackendName'); // backend displayName when present
    expect(names).toContain('beadfeed'); // hex fallback when nothing resolves
  });

  it('treats a whitespace-only card name as absent and falls through to backend name', () => {
    // The backend card publish caps length but has no non-empty constraint, so a
    // peer can publish display_name = "" / "   "; the ladder must not render it.
    const card: ResolvedCard = { displayName: '   ', statusText: '' };
    const { container } = render(ChannelMembersPanel, {
      props: baseProps({
        members: [self, member({ address: PEER, displayName: 'BackendName' })],
        resolveCard: (id: string) => (id === PEER ? card : undefined),
      }),
    });
    const names = renderedNames(container);
    expect(names).toContain('BackendName');
    expect(names).not.toContain('   '); // never a blank/whitespace label
  });

  it('falls to hex when the card name is empty and there is no backend name', () => {
    const card: ResolvedCard = { displayName: '', statusText: '' };
    const { container } = render(ChannelMembersPanel, {
      props: baseProps({
        members: [self, member({ address: PEER, displayName: undefined })],
        resolveCard: () => card, // empty for everyone
      }),
    });
    const names = renderedNames(container);
    expect(names).toContain('deadbeef'); // PEER hex prefix
    expect(names.every((n) => n.trim().length > 0)).toBe(true); // no blank labels
  });
});

describe('ChannelMembersPanel — ZEB-553 presence dots on the default roster', () => {
  it('renders a presence dot on every row, lit for online members', () => {
    const { container } = render(ChannelMembersPanel, {
      props: baseProps({
        members: [self, member({ address: PEER }), member({ address: PEER2 })],
        isOnline: (id: string) => id === PEER,
      }),
    });
    const rows = container.querySelectorAll('.member-row');
    expect(rows.length).toBe(3);
    // Every row has a dot (the headline presence affordance was previously absent).
    expect(container.querySelectorAll('.presence-dot').length).toBe(3);
    // Self (always online) + PEER are lit; PEER2 is not.
    expect(container.querySelectorAll('.presence-dot.online').length).toBe(2);
  });

  it('always shows self online even when the resolver reports everyone offline', () => {
    const { container } = render(ChannelMembersPanel, {
      props: baseProps({
        members: [self, member({ address: PEER })],
        isOnline: () => false, // zenoh never loops our own beacon back
      }),
    });
    // Rows are ordered self-first; only the self dot is lit.
    const firstRow = container.querySelector('.member-row');
    expect(firstRow?.querySelector('.presence-dot.online')).not.toBeNull();
    expect(container.querySelectorAll('.presence-dot.online').length).toBe(1);
  });

  it('treats everyone but self as offline when no isOnline resolver is provided', () => {
    const { container } = render(ChannelMembersPanel, {
      props: baseProps({ members: [self, member({ address: PEER })] }),
    });
    expect(container.querySelectorAll('.presence-dot.online').length).toBe(1); // self only
  });
});

describe('ChannelMembersPanel — ZEB-553 owner-card open', () => {
  it('opens the owner card on click with the SIGNED card name (never the local nickname)', async () => {
    const calls: Array<{ ownerIdHex: string; displayName: string }> = [];
    const card: ResolvedCard = { displayName: 'ZEBbot', statusText: 'hi' };
    const { container } = render(ChannelMembersPanel, {
      props: baseProps({
        members: [self, member({ address: PEER })],
        resolveCard: (id: string) => (id === PEER ? card : undefined),
        resolveNickname: (id: string) => (id === PEER ? 'Bestie' : undefined),
        onOpenCard: (payload: { ownerIdHex: string; displayName: string }) => calls.push(payload),
      }),
    });
    // The row label shows the nickname-first ladder ("Bestie")…
    const peerBtn = Array.from(container.querySelectorAll<HTMLButtonElement>('.name-btn')).find(
      (b) => (b.textContent ?? '').trim() === 'Bestie',
    );
    expect(peerBtn).toBeTruthy();
    await fireEvent.click(peerBtn!);
    expect(calls).toHaveLength(1);
    expect(calls[0].ownerIdHex).toBe(PEER);
    // …but the owner-card popover must carry the signed card name, not the nickname.
    expect(calls[0].displayName).toBe('ZEBbot');
  });

  it('renders names as plain spans (no card button) when onOpenCard is absent', () => {
    const { container } = render(ChannelMembersPanel, {
      props: baseProps({ members: [self, member({ address: PEER })] }),
    });
    expect(container.querySelectorAll('.name-btn').length).toBe(0);
  });
});
