import { render, screen, fireEvent, cleanup, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, afterEach } from 'vitest';
import ProfilePopover from '../ProfilePopover.svelte';
import type { Profile } from '../../types';
import type {
  ProfileBroadcastService,
  ProfileMembershipBroadcastInfo,
} from '../../profile-broadcast-service';
import { knownPeersState } from '../../known-peers-state.svelte';
import { buildKnownPeersIndex, EMPTY_KNOWN_PEERS } from '../../name-collision';

// Stub the Tauri event API: production loads it via dynamic import to
// listen for `profile-broadcast-received`, but in tests there's no
// Tauri runtime so the call rejects with `transformCallback` undefined.
// Without this stub, vitest reports unhandled rejections and exits 1
// even when every test assertion passes.
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(async () => () => {}),
}));

const SELF_ADDR = '00'.repeat(16);
const PEER_ADDR = 'aa'.repeat(16);

const mockProfile: Profile = {
  address: 'a1b2c3d4',
  displayName: 'Alice',
  statusText: 'Working on transport layer',
};

function makeService(opts?: {
  initialCached?: ProfileMembershipBroadcastInfo | null;
}) {
  const subscribe = vi.fn(async () => 1);
  const unsubscribe = vi.fn(async () => {});
  const getCached = vi.fn(async () => opts?.initialCached ?? null);
  const setShared = vi.fn(async () => {});
  return {
    service: { subscribe, unsubscribe, getCached, setShared } as unknown as ProfileBroadcastService,
    subscribe,
    unsubscribe,
    getCached,
  };
}

function baseProps(overrides: Record<string, unknown> = {}) {
  const { service } = makeService();
  return {
    profile: mockProfile,
    x: 100,
    y: 100,
    onClose: vi.fn(),
    ownAddress: SELF_ADDR,
    profileBroadcastService: service,
    resolveCommunityName: (() => null) as (cid: string) => string | null,
    ...overrides,
  };
}

describe('ProfilePopover', () => {
  afterEach(() => cleanup());

  it('renders display name and status text', () => {
    render(ProfilePopover, {
      props: baseProps(),
    });
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('Working on transport layer')).toBeTruthy();
  });

  it('renders truncated peer address', () => {
    render(ProfilePopover, {
      props: baseProps(),
    });
    expect(screen.getByText('a1b2c3d4')).toBeTruthy();
  });

  it('renders sound slot labels', () => {
    render(ProfilePopover, {
      props: baseProps(),
    });
    expect(screen.getByText('Quiet')).toBeTruthy();
    expect(screen.getByText('Standard')).toBeTruthy();
    expect(screen.getByText('Loud')).toBeTruthy();
  });

  it('shows "System default" when no custom sounds set', () => {
    render(ProfilePopover, {
      props: baseProps(),
    });
    const defaults = screen.getAllByText('System default');
    expect(defaults.length).toBe(3);
  });

  it('calls onClose when Escape is pressed', async () => {
    const onClose = vi.fn();
    render(ProfilePopover, {
      props: baseProps({ onClose }),
    });
    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(onClose).toHaveBeenCalled();
  });

  it('handles profile without status text', () => {
    const noStatus: Profile = { address: 'xyz789', displayName: 'Bob' };
    render(ProfilePopover, {
      props: baseProps({ profile: noStatus }),
    });
    expect(screen.getByText('Bob')).toBeTruthy();
    expect(screen.queryByText('Working on transport layer')).toBeNull();
  });

  it('popover_subscribes_on_mount', async () => {
    const { service, subscribe } = makeService();
    const peerProfile: Profile = { address: PEER_ADDR, displayName: 'Peer' };
    render(ProfilePopover, {
      props: baseProps({
        profile: peerProfile,
        profileBroadcastService: service,
      }),
    });
    await waitFor(() => expect(subscribe).toHaveBeenCalledWith(PEER_ADDR));
  });

  it('popover_unsubscribes_on_close', async () => {
    const { service, subscribe, unsubscribe } = makeService();
    const peerProfile: Profile = { address: PEER_ADDR, displayName: 'Peer' };
    const { unmount } = render(ProfilePopover, {
      props: baseProps({
        profile: peerProfile,
        profileBroadcastService: service,
      }),
    });
    // Let the async subscribe() resolve before unmounting; without
    // this, the cleanup fires before subscriptionId is captured.
    await waitFor(() => expect(subscribe).toHaveBeenCalled());
    unmount();
    await waitFor(() => expect(unsubscribe).toHaveBeenCalled());
  });

  it('popover_shows_loading_then_loaded', async () => {
    const { service } = makeService({
      initialCached: {
        ownerAddr: PEER_ADDR,
        communityIds: ['bb'.repeat(16)],
        sharedAt: '5000',
      },
    });
    const peerProfile: Profile = { address: PEER_ADDR, displayName: 'Peer' };
    const { getByText } = render(ProfilePopover, {
      props: baseProps({
        profile: peerProfile,
        profileBroadcastService: service,
        resolveCommunityName: () => 'Test Community',
      }),
    });
    // First render shows the loading state.
    expect(getByText('Looking up public memberships…')).toBeTruthy();
    // After cache hydration, the community name appears.
    await waitFor(() => expect(getByText('Test Community')).toBeTruthy());
  });

  // ── ZEB-341: owner-card mode ───────────────────────────────────────
  const OWNER_HEX = 'ab'.repeat(16);

  it('owner-card mode renders name, status, owner_id and Admin role', () => {
    render(ProfilePopover, {
      props: {
        mode: 'owner-card',
        card: { ownerIdHex: OWNER_HEX, displayName: 'Alice', statusText: 'hi', power: 100 },
        x: 10,
        y: 10,
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('hi')).toBeTruthy();
    expect(screen.getByText(OWNER_HEX)).toBeTruthy();
    expect(screen.getByText('Admin')).toBeTruthy();
    // Reticulum-only sections are absent in owner-card mode.
    expect(screen.queryByText('Public memberships')).toBeNull();
    expect(screen.queryByText('Notification sounds')).toBeNull();
  });

  it('owner-card mode derives Moderator and Member roles from power', () => {
    const { unmount } = render(ProfilePopover, {
      props: {
        mode: 'owner-card',
        card: { ownerIdHex: OWNER_HEX, displayName: 'Mod', statusText: '', power: 50 },
        x: 0,
        y: 0,
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('Moderator')).toBeTruthy();
    unmount();
    render(ProfilePopover, {
      props: {
        mode: 'owner-card',
        card: { ownerIdHex: OWNER_HEX, displayName: 'Regular', statusText: '', power: 0 },
        x: 0,
        y: 0,
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('Member')).toBeTruthy();
  });

  it('owner-card mode with undefined power omits the role line', () => {
    render(ProfilePopover, {
      props: {
        mode: 'owner-card',
        card: { ownerIdHex: OWNER_HEX, displayName: 'Author', statusText: '' },
        x: 0,
        y: 0,
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('Author')).toBeTruthy();
    expect(screen.getByText(OWNER_HEX)).toBeTruthy();
    expect(screen.queryByText('Admin')).toBeNull();
    expect(screen.queryByText('Moderator')).toBeNull();
    expect(screen.queryByText('Member')).toBeNull();
  });

  it('owner-card mode renders a "Banned" flag when status is banned', () => {
    render(ProfilePopover, {
      props: {
        mode: 'owner-card',
        card: { ownerIdHex: OWNER_HEX, displayName: 'Bob', statusText: '', power: 0, membershipStatus: 'banned' },
        x: 0,
        y: 0,
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('Banned')).toBeTruthy();
  });

  it('owner-card mode does NOT render "Banned" for the normal joined status', () => {
    render(ProfilePopover, {
      props: {
        mode: 'owner-card',
        card: { ownerIdHex: OWNER_HEX, displayName: 'Bob', statusText: '', power: 0, membershipStatus: 'joined' },
        x: 0,
        y: 0,
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('Bob')).toBeTruthy();
    expect(screen.queryByText('Banned')).toBeNull();
  });

  it('owner-card mode shows "Name unavailable" when displayName is empty', () => {
    render(ProfilePopover, {
      props: {
        mode: 'owner-card',
        card: { ownerIdHex: OWNER_HEX, displayName: '', statusText: '' },
        x: 0,
        y: 0,
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('Name unavailable')).toBeTruthy();
    expect(screen.getByText(OWNER_HEX)).toBeTruthy();
  });

  it('owner-card mode copies owner_id on click', async () => {
    const writeText = vi.fn(async () => {});
    // Save + restore navigator.clipboard so this test doesn't leak a mutated
    // global into later tests (some environments expose a real clipboard).
    const prevClipboard = navigator.clipboard;
    try {
      Object.defineProperty(navigator, 'clipboard', {
        configurable: true,
        value: { writeText },
      });
      render(ProfilePopover, {
        props: {
          mode: 'owner-card',
          card: { ownerIdHex: OWNER_HEX, displayName: 'Alice', statusText: '', power: 100 },
          x: 0,
          y: 0,
          onClose: vi.fn(),
        },
      });
      await fireEvent.click(screen.getByLabelText('Copy owner ID'));
      expect(writeText).toHaveBeenCalledWith(OWNER_HEX);
    } finally {
      Object.defineProperty(navigator, 'clipboard', {
        configurable: true,
        value: prevClipboard,
      });
    }
  });

  it('owner-card mode shows "View full profile" and fires onViewProfile with the owner id', async () => {
    const onViewProfile = vi.fn();
    render(ProfilePopover, {
      props: {
        mode: 'owner-card',
        card: { ownerIdHex: OWNER_HEX, displayName: 'Alice', statusText: '', power: 100 },
        x: 0,
        y: 0,
        onClose: vi.fn(),
        onViewProfile,
      },
    });
    await fireEvent.click(screen.getByText('View full profile'));
    expect(onViewProfile).toHaveBeenCalledWith(OWNER_HEX);
  });

  it('owner-card mode hides "View full profile" when no onViewProfile prop is given', () => {
    render(ProfilePopover, {
      props: {
        mode: 'owner-card',
        card: { ownerIdHex: OWNER_HEX, displayName: 'Alice', statusText: '', power: 100 },
        x: 0,
        y: 0,
        onClose: vi.fn(),
      },
    });
    expect(screen.queryByText('View full profile')).toBeNull();
  });

  it('owner-card mode closes on Escape', async () => {
    const onClose = vi.fn();
    render(ProfilePopover, {
      props: {
        mode: 'owner-card',
        card: { ownerIdHex: OWNER_HEX, displayName: 'Alice', statusText: '', power: 100 },
        x: 0,
        y: 0,
        onClose,
      },
    });
    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(onClose).toHaveBeenCalled();
  });

  it('owner-card mode: clicking a member-name/author button does NOT close (allows switching)', async () => {
    const onClose = vi.fn();
    render(ProfilePopover, {
      props: {
        mode: 'owner-card',
        card: { ownerIdHex: OWNER_HEX, displayName: 'Alice', statusText: '', power: 100 },
        x: 0,
        y: 0,
        onClose,
      },
    });
    // The click-outside listener attaches on a setTimeout(0); let it register.
    await new Promise((r) => setTimeout(r, 5));
    for (const cls of ['name name-btn', 'author author-btn']) {
      const btn = document.createElement('button');
      btn.className = cls;
      document.body.appendChild(btn);
      await fireEvent.click(btn);
      btn.remove();
    }
    // Clicking a popover-opening trigger must not close — openMemberCard handles
    // the switch, so the popover stays open for the newly clicked member.
    expect(onClose).not.toHaveBeenCalled();
  });

  it('owner-card mode: clicking truly outside closes', async () => {
    const onClose = vi.fn();
    render(ProfilePopover, {
      props: {
        mode: 'owner-card',
        card: { ownerIdHex: OWNER_HEX, displayName: 'Alice', statusText: '', power: 100 },
        x: 0,
        y: 0,
        onClose,
      },
    });
    await new Promise((r) => setTimeout(r, 5));
    const div = document.createElement('div');
    document.body.appendChild(div);
    await fireEvent.click(div);
    div.remove();
    expect(onClose).toHaveBeenCalled();
  });

  it('popover_shows_no_memberships_after_timeout', async () => {
    vi.useFakeTimers();
    try {
      const { service } = makeService(); // returns null
      const peerProfile: Profile = { address: PEER_ADDR, displayName: 'Peer' };
      const { getByText } = render(ProfilePopover, {
        props: baseProps({
          profile: peerProfile,
          profileBroadcastService: service,
        }),
      });
      expect(getByText('Looking up public memberships…')).toBeTruthy();
      // Advance 3s; loading flips off and empty state renders.
      await vi.advanceTimersByTimeAsync(3100);
      expect(getByText('No public memberships shared.')).toBeTruthy();
    } finally {
      // Always restore real timers — if any assertion above throws, the
      // bare `vi.useRealTimers()` call wouldn't run and subsequent tests
      // would inherit fake-timer state.
      vi.useRealTimers();
    }
  });
});

// ZEB-962: the name line reads a verbatim-cache display name. `|| 'Name
// unavailable'` (owner-card) floors `""` but lets a whitespace-only name
// through; the reticulum-mode line had no fallback at all. `nonEmpty` guards
// both so a blank/whitespace broadcast name never renders as an empty line.
describe('ProfilePopover blank-name guard (ZEB-962)', () => {
  afterEach(() => cleanup());

  it('reticulum mode: a whitespace-only profile name shows the fallback, not a blank line', () => {
    render(ProfilePopover, {
      props: baseProps({
        profile: { address: 'a1b2c3d4', displayName: '   ', statusText: '' },
      }),
    });
    expect(screen.getByText('Name unavailable')).toBeTruthy();
  });

  it('owner-card mode: a whitespace-only card name shows the fallback', () => {
    render(ProfilePopover, {
      props: baseProps({
        mode: 'owner-card',
        card: { ownerIdHex: 'ab'.repeat(16), displayName: '  ', statusText: '', power: 0 },
      }),
    });
    expect(screen.getByText('Name unavailable')).toBeTruthy();
  });
});


// ── ZEB-977: petname + private-notes editor (owner-card mode) ────────────
describe('ProfilePopover contact editor (ZEB-977)', () => {
  afterEach(cleanup);

  const CARD_OWNER = 'ab'.repeat(16);
  const ownerCard = {
    ownerIdHex: CARD_OWNER,
    displayName: 'CardName',
    statusText: '',
  };

  function makeContactsService(entry?: { petname?: string; notes?: string }) {
    return {
      list: vi.fn(async () =>
        entry
          ? [{ ownerIdHex: CARD_OWNER, ...entry, firstSeenMs: 1, updatedMs: 2 }]
          : [],
      ),
      setPetname: vi.fn(async () => null),
      setNotes: vi.fn(async () => null),
    } as any;
  }

  it('renders the editor for another identity and saves petname + notes', async () => {
    const contactsService = makeContactsService();
    render(ProfilePopover, {
      props: {
        mode: 'owner-card',
        card: ownerCard,
        x: 0,
        y: 0,
        onClose: vi.fn(),
        contactsService,
        selfOwnerIdHex: '00'.repeat(16),
      } as any,
    });
    const editor = await screen.findByTestId('contact-editor');
    expect(editor).toBeTruthy();
    const petInput = screen.getByLabelText('Your petname for this person');
    const notesInput = screen.getByLabelText('Your private notes about this person');
    await waitFor(() => expect(petInput).not.toBeDisabled());
    await fireEvent.input(petInput, { target: { value: 'Koya' } });
    await fireEvent.input(notesInput, { target: { value: 'garden club' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => {
      expect(contactsService.setPetname).toHaveBeenCalledWith(CARD_OWNER, 'Koya');
      expect(contactsService.setNotes).toHaveBeenCalledWith(CARD_OWNER, 'garden club');
    });
  });

  it('preloads existing drafts and clears with null on blank save', async () => {
    const contactsService = makeContactsService({ petname: 'Old', notes: 'old notes' });
    render(ProfilePopover, {
      props: {
        mode: 'owner-card',
        card: ownerCard,
        x: 0,
        y: 0,
        onClose: vi.fn(),
        contactsService,
        selfOwnerIdHex: '00'.repeat(16),
      } as any,
    });
    const petInput = (await screen.findByLabelText(
      'Your petname for this person',
    )) as HTMLInputElement;
    await waitFor(() => expect(petInput.value).toBe('Old'));
    await fireEvent.input(petInput, { target: { value: '   ' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => {
      expect(contactsService.setPetname).toHaveBeenCalledWith(CARD_OWNER, null);
    });
  });

  it('hides the editor on the SELF card and when no service is wired', async () => {
    const { unmount } = render(ProfilePopover, {
      props: {
        mode: 'owner-card',
        card: ownerCard,
        x: 0,
        y: 0,
        onClose: vi.fn(),
        contactsService: makeContactsService(),
        selfOwnerIdHex: CARD_OWNER, // the card IS us
      } as any,
    });
    expect(screen.queryByTestId('contact-editor')).toBeNull();
    unmount();
    render(ProfilePopover, {
      props: {
        mode: 'owner-card',
        card: ownerCard,
        x: 0,
        y: 0,
        onClose: vi.fn(),
      } as any,
    });
    expect(screen.queryByTestId('contact-editor')).toBeNull();
  });

  it('keeps the signed identity section untouched: card name + full hex still shown', async () => {
    render(ProfilePopover, {
      props: {
        mode: 'owner-card',
        card: ownerCard,
        x: 0,
        y: 0,
        onClose: vi.fn(),
        contactsService: makeContactsService({ petname: 'Koya' }),
        selfOwnerIdHex: '00'.repeat(16),
      } as any,
    });
    // The drill-down convention (ZEB-419/PR #240): identity section shows the
    // SIGNED card name and full hex — never the petname.
    expect(screen.getByText('CardName')).toBeTruthy();
    expect(screen.getByText(CARD_OWNER)).toBeTruthy();
  });
});

// ZEB-979: impersonation-risk drill-down — the popover is where the user
// lands to investigate a marked name, so it must spell the situation out
// with BOTH hexes visible.
describe('ProfilePopover collision drill-down (ZEB-979)', () => {
  const KNOWN_HEX = 'aaaa1111aaaa1111aaaa1111aaaa1111';
  const STRANGER_HEX = 'dddd4444dddd4444dddd4444dddd4444';

  afterEach(() => {
    knownPeersState.index = EMPTY_KNOWN_PEERS;
    cleanup();
  });

  it('warns on a stranger card whose name collides with a known peer, with both hexes', () => {
    knownPeersState.index = buildKnownPeersIndex([
      { label: 'Jake', ownerIdHex: KNOWN_HEX },
    ]);
    render(ProfilePopover, {
      props: {
        mode: 'owner-card' as const,
        card: { ownerIdHex: STRANGER_HEX, displayName: 'Jake' },
        x: 0,
        y: 0,
        onClose: vi.fn(),
      },
    });
    const warning = screen.getByTestId('collision-warning');
    expect(warning.textContent).toContain('different identity');
    expect(warning.textContent).toContain('Jake');
    expect(warning.textContent).toContain(KNOWN_HEX);
    expect(warning.textContent).toContain(STRANGER_HEX);
  });

  it('shows no warning on the known peer\'s own card', () => {
    knownPeersState.index = buildKnownPeersIndex([
      { label: 'Jake', ownerIdHex: KNOWN_HEX },
    ]);
    render(ProfilePopover, {
      props: {
        mode: 'owner-card' as const,
        card: { ownerIdHex: KNOWN_HEX, displayName: 'Jake' },
        x: 0,
        y: 0,
        onClose: vi.fn(),
      },
    });
    expect(screen.queryByTestId('collision-warning')).toBeNull();
  });

  it('shows no warning when the index is empty', () => {
    render(ProfilePopover, {
      props: {
        mode: 'owner-card' as const,
        card: { ownerIdHex: STRANGER_HEX, displayName: 'Jake' },
        x: 0,
        y: 0,
        onClose: vi.fn(),
      },
    });
    expect(screen.queryByTestId('collision-warning')).toBeNull();
  });
});
