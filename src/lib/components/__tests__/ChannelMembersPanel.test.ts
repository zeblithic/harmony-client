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
        presence: (id: string) => ({ state: (id === PEER ? 'online' : 'offline') as 'online' | 'offline' }),
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
        presence: () => ({ state: 'offline' as const }), // zenoh never loops our own beacon back
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

  it('marks each presence dot with role="img" + an online/offline label (finding 15)', () => {
    const { container } = render(ChannelMembersPanel, {
      props: baseProps({ members: [self, member({ address: PEER })], presence: () => ({ state: 'offline' as const }) }),
    });
    const dots = container.querySelectorAll('.presence-dot');
    expect(dots.length).toBe(2);
    for (const dot of dots) {
      expect(dot.getAttribute('role')).toBe('img');
      expect(dot.getAttribute('aria-label')).toMatch(/online|offline/i);
    }
  });

  it('exposes the roster as a list with listitem rows (finding 15)', () => {
    const { container } = render(ChannelMembersPanel, {
      props: baseProps({ members: [self, member({ address: PEER })] }),
    });
    expect(container.querySelector('.member-list')?.getAttribute('role')).toBe('list');
    const rows = container.querySelectorAll('.member-row');
    expect(rows.length).toBe(2);
    for (const row of rows) expect(row.getAttribute('role')).toBe('listitem');
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

describe('ChannelMembersPanel — ZEB-553 item 11 roster loading state', () => {
  it('shows a loading affordance instead of a bare "0" when loading with an empty roster', () => {
    const { container, getByText } = render(ChannelMembersPanel, {
      props: baseProps({ members: [], loading: true }),
    });
    getByText(/Loading members/i);
    // The true count is unknown mid-fetch, so the badge shows an ellipsis — a
    // "0" would read as "this community has no members".
    expect(container.querySelector('.count')?.textContent).toBe('…');
    // The list itself isn't rendered while the loading status stands in for it.
    expect(container.querySelector('.member-list')).toBeNull();
  });

  it('renders the roster (not the loading row) once members are present even while loading stays true', () => {
    // Background refresh case: the roster is already on screen, so a refresh
    // must not flash a loading state over it.
    const { container, queryByText } = render(ChannelMembersPanel, {
      props: baseProps({ members: [self, member({ address: PEER })], loading: true }),
    });
    expect(queryByText(/Loading members/i)).toBeNull();
    expect(container.querySelectorAll('.member-row').length).toBe(2);
  });

  it('shows the empty list (not the loading row) when not loading and the roster is genuinely empty', () => {
    const { container, queryByText } = render(ChannelMembersPanel, {
      props: baseProps({ members: [], loading: false }),
    });
    expect(queryByText(/Loading members/i)).toBeNull();
    expect(container.querySelector('.member-list')).not.toBeNull();
    expect(container.querySelectorAll('.member-row').length).toBe(0);
    expect(container.querySelector('.count')?.textContent).toBe('0');
  });

  it('marks the loading affordance as a status live-region and keeps it out of the list (a11y)', () => {
    const { container } = render(ChannelMembersPanel, {
      props: baseProps({ members: [], loading: true }),
    });
    const status = container.querySelector('.member-loading');
    expect(status?.getAttribute('role')).toBe('status');
    // It must be a sibling of the (absent) list, never an <li>, so the list
    // stays a pure list of member listitems for screen readers.
    expect(status?.tagName).toBe('P');
  });

  it('defaults to no loading affordance when the loading prop is omitted', () => {
    const { container, queryByText } = render(ChannelMembersPanel, {
      props: baseProps({ members: [self] }),
    });
    expect(queryByText(/Loading members/i)).toBeNull();
    expect(container.querySelector('.member-list')).not.toBeNull();
  });
});

// ZEB-972 — stale presence honesty on the always-visible roster. A roster row
// whose beacon is overdue for backend eviction must stop reading as online:
// hollow stale dot + honest last-seen title.
describe('ChannelMembersPanel — ZEB-972 stale presence honesty', () => {
  it('renders a stale member hollow (not online) with a last-seen + warning title', () => {
    const seen = Date.now() - 2 * 60_000;
    const { container } = render(ChannelMembersPanel, {
      props: baseProps({
        members: [self, member({ address: PEER })],
        presence: (id: string) =>
          id === PEER
            ? { state: 'stale' as const, lastSeenMs: seen }
            : { state: 'offline' as const },
      }),
    });
    expect(container.querySelectorAll('.presence-dot.online').length).toBe(1); // self only
    const staleDot = container.querySelector('.presence-dot.stale');
    expect(staleDot).not.toBeNull();
    expect(staleDot?.getAttribute('title')).toBe('Last seen ~2m ago — connection may be stale');
  });

  it('renders an offline member with a session-known stamp as "Offline · last seen …"', () => {
    const seen = Date.now() - 5 * 60_000;
    const { container } = render(ChannelMembersPanel, {
      props: baseProps({
        members: [self, member({ address: PEER })],
        presence: () => ({ state: 'offline' as const, lastSeenMs: seen }),
      }),
    });
    const dots = container.querySelectorAll('.presence-dot');
    expect(dots[1]?.getAttribute('title')).toBe('Offline · last seen ~5m ago');
  });
});

// ZEB-972 (CodeAnt PR #722): the channel roster must honor "Appear offline"
// like MemberRow/CommunityMembersPanel — one hollow view and one solid-green
// view of the same self dot contradicted the setting.
describe('ChannelMembersPanel — self-invisible dot', () => {
  it('renders the self dot hollow/invisible (not online) when selfInvisible is on', () => {
    const { container } = render(ChannelMembersPanel, {
      props: baseProps({ members: [self, member({ address: PEER })], selfInvisible: true }),
    });
    const firstRow = container.querySelector('.member-row'); // self sorts first
    const dot = firstRow?.querySelector('.presence-dot');
    expect(dot?.classList.contains('online')).toBe(false);
    expect(dot?.classList.contains('self-invisible')).toBe(true);
    expect(dot?.getAttribute('title')).toBe('Appearing offline');
  });

  it('keeps the self dot solid online when selfInvisible is off', () => {
    const { container } = render(ChannelMembersPanel, {
      props: baseProps({ members: [self, member({ address: PEER })] }),
    });
    const dot = container.querySelector('.member-row')?.querySelector('.presence-dot');
    expect(dot?.classList.contains('online')).toBe(true);
    expect(dot?.classList.contains('self-invisible')).toBe(false);
  });
});
