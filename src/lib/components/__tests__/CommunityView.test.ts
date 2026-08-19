import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import { writable } from 'svelte/store';
import CommunityView from '../CommunityView.svelte';
import { CommunityService } from '../../community-service';
import { ChannelMessageService } from '../../channel-message-service';
import { NavService } from '../../nav-service';
import { VotingAdapter } from '../../voting-adapter';
import type { TauriAdapter } from '../../zenoh-service';
import type { CommunityMember } from '../../types';
import type { VoiceSession } from '../../voice-session';

/** ZEB-351: minimal VoiceSession stub for the voice routing path. A fresh
 *  instance per render so its `state` store isn't shared across tests. The
 *  initial state is the real VoiceSessionState shape: idle / muted / empty. */
function makeVoiceSessionStub(): VoiceSession {
  // Structural stub satisfying the parts CommunityView/VoiceChannelView touch
  // (state store + control methods). Cast through unknown — the real class has
  // private engine fields we deliberately don't reconstruct here.
  return {
    state: writable({
      phase: 'idle' as const,
      community: null,
      channel: null,
      muted: true,
      deafened: false,
      pttMode: false,
      roster: [] as never[],
    }),
    join: vi.fn(async () => {}),
    leave: vi.fn(async () => {}),
    setMuted: vi.fn(async () => {}),
    setDeafened: vi.fn(async () => {}),
    setPttMode: vi.fn(),
    setPttHeld: vi.fn(),
    clearChannelFull: vi.fn(),
  } as unknown as VoiceSession;
}

/** ZEB-608: VotingAdapter stub with the tier3 surface CommunityView's
 *  tabs touch (list + lifecycle subscriptions), unconnected-safe. */
function makeVotingAdapterStub(): VotingAdapter {
  const votingAdapter = new VotingAdapter();
  votingAdapter.listTier3Polls = vi.fn().mockResolvedValue([]);
  const noopUnsub = () => {};
  votingAdapter.subscribeTier3PollCreated = vi.fn().mockReturnValue(noopUnsub);
  votingAdapter.subscribeTier3SortitionComplete = vi.fn().mockReturnValue(noopUnsub);
  votingAdapter.subscribeTier3DraftingOpen = vi.fn().mockReturnValue(noopUnsub);
  votingAdapter.subscribeTier3RatificationOpen = vi.fn().mockReturnValue(noopUnsub);
  votingAdapter.subscribeTier3Finalized = vi.fn().mockReturnValue(noopUnsub);
  return votingAdapter;
}

function makeAdapter(): TauriAdapter & { listeners: Map<string, Function> } {
  const listeners = new Map<string, Function>();
  return {
    listeners,
    invoke: vi.fn(),
    listen: vi.fn(async (event: string, handler: Function) => {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    }),
  } as any;
}

const adminMember: CommunityMember = {
  address: 'aa'.repeat(20),
  displayName: 'Alice',
  power: 100,
  status: 'joined',
};
const general = {
  channelId: '01'.repeat(16),
  name: 'general',
  writePower: 0,
  kind: 'text',
  createdAt: { wallMs: 100, logical: 0, deviceId: 'd' },
};
const announcements = {
  channelId: '02'.repeat(16),
  name: 'announcements',
  writePower: 50,
  kind: 'text',
  createdAt: { wallMs: 200, logical: 0, deviceId: 'd' },
};
const voiceLounge = {
  channelId: '04'.repeat(16),
  name: 'lounge',
  writePower: 0,
  kind: 'voice',
  createdAt: { wallMs: 400, logical: 0, deviceId: 'd' },
};

const townhallFloor = {
  channelId: '05'.repeat(16),
  name: 'assembly',
  writePower: 0,
  kind: 'townhall',
  createdAt: { wallMs: 500, logical: 0, deviceId: 'd' },
};

async function setup(
  channelList: any[] = [general, announcements],
  propOverrides: Record<string, unknown> = {},
  invokeOverrides: Record<string, () => Promise<unknown>> = {},
) {
  const adapter = makeAdapter();
  (adapter.invoke as any).mockImplementation((cmd: string) => {
    if (cmd in invokeOverrides) return invokeOverrides[cmd]();
    if (cmd === 'list_channels') return Promise.resolve(channelList);
    if (cmd === 'list_channel_messages') return Promise.resolve([]);
    if (cmd === 'get_community_governance') return Promise.resolve({ adminQuorum: 1 });
    return Promise.resolve(undefined);
  });
  const communityService = new CommunityService();
  await communityService.connectAdapter(adapter);
  const channelMessageService = new ChannelMessageService();
  await channelMessageService.connectAdapter(adapter);
  const navService = new NavService();
  const props = {
    communityId: 'aa'.repeat(16),
    communityName: 'Test Community',
    communityKind: 'open' as const,
    myPower: 100,
    ownAddress: adminMember.address,
    members: [adminMember],
    isDegraded: false,
    sharedInProfile: false,
    communityService,
    channelMessageService,
    navService,
    onLeave: vi.fn(),
    onKickMember: vi.fn(),
    onSetPowerLevel: vi.fn(),
    onGenerateInvite: vi.fn().mockResolvedValue('harmony://invite/...'),
    onToggleSharedInProfile: vi.fn().mockResolvedValue(undefined),
    // ZEB-351: a ready voice session by default so voice channels route to
    // VoiceChannelView. Tests can override with null to exercise the
    // pre-ready (IPC not yet resolved) guard.
    voiceSession: makeVoiceSessionStub(),
    // ZEB-663: selection is App-owned; the default mirrors what App's
    // resolution effect would pick (first/only channel). Tests override to
    // point the feed at a specific channel.
    selectedChannelId: channelList[0]?.channelId ?? null,
    // ZEB-965: App's reactive nav mirror; empty by default (panel chrome only).
    navNodes: [],
    ...propOverrides,
  };
  const renderResult = render(CommunityView, { props });
  return { adapter, communityService, channelMessageService, navService, props, ...renderResult };
}

describe('CommunityView', () => {
  it('mounts the two columns (feed + right panel); channels is the default right view', async () => {
    const { container } = await setup();
    await waitFor(() => {
      expect(container.querySelector('.channel-message-feed')).toBeTruthy();
      // ZEB-965: the right column defaults to the channel list — it is the
      // primary channel navigation now that the left nav is communities-only.
      expect(container.querySelector('.channels-panel')).toBeTruthy();
    });
    expect(container.querySelector('.members-panel')).toBeNull();
    // ZEB-663: the per-community ChannelSubSidebar is gone.
    expect(container.querySelector('.channel-sub-sidebar')).toBeNull();
  });

  describe('right-panel channels/members toggle (ZEB-965)', () => {
    it('👥 switches the right panel to members; # switches back to channels', async () => {
      const { container, getByLabelText } = await setup();
      await waitFor(() => {
        expect(container.querySelector('.channels-panel')).toBeTruthy();
      });
      await fireEvent.click(getByLabelText(/Show members panel/i));
      expect(container.querySelector('.members-panel')).toBeTruthy();
      expect(container.querySelector('.channels-panel')).toBeNull();
      await fireEvent.click(getByLabelText(/Show channels panel/i));
      expect(container.querySelector('.channels-panel')).toBeTruthy();
      expect(container.querySelector('.members-panel')).toBeNull();
    });

    it('clicking the active view toggle hides the right panel entirely', async () => {
      const { container, getByLabelText } = await setup();
      await waitFor(() => {
        expect(container.querySelector('.channels-panel')).toBeTruthy();
      });
      await fireEvent.click(getByLabelText(/Hide channels panel/i));
      expect(container.querySelector('.channels-panel')).toBeNull();
      expect(container.querySelector('.members-panel')).toBeNull();
      // And back on.
      await fireEvent.click(getByLabelText(/Show channels panel/i));
      expect(container.querySelector('.channels-panel')).toBeTruthy();
    });

    it('reflects the active view in aria-pressed on both header toggles', async () => {
      const { container, getByLabelText } = await setup();
      await waitFor(() => {
        expect(container.querySelector('.channels-panel')).toBeTruthy();
      });
      expect(getByLabelText(/Hide channels panel/i).getAttribute('aria-pressed')).toBe('true');
      expect(getByLabelText(/Show members panel/i).getAttribute('aria-pressed')).toBe('false');
      await fireEvent.click(getByLabelText(/Show members panel/i));
      expect(getByLabelText(/Hide members panel/i).getAttribute('aria-pressed')).toBe('true');
      expect(getByLabelText(/Show channels panel/i).getAttribute('aria-pressed')).toBe('false');
    });

    it('renders channel rows from the navNodes prop and tracks its updates', async () => {
      // ZEB-965: NavService.nodes is a plain (non-reactive) property — App
      // mirrors it into $state and passes it down. The panel must render from
      // that PROP, not from navService.nodes, or it goes permanently stale
      // (e.g. a just-joined community whose channels sync in moments later).
      const communityId = 'aa'.repeat(16);
      const chanBase = { unreadCount: 0, mentionCount: 0, unreadLevel: 'none' as const, expanded: false };
      const communityNode = {
        id: communityId, parentId: null, type: 'community' as const, name: 'Test Community',
        expanded: true, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' as const,
      };
      const navNodes = [
        communityNode,
        { id: 'nav-ch-1', parentId: communityId, type: 'channel' as const, channelKind: 'text' as const, name: 'harbor', ...chanBase },
      ];
      const { container, rerender, props } = await setup([general], { navNodes });
      await waitFor(() => {
        expect(container.querySelector('[data-testid="nav-row-nav-ch-1"]')).toBeTruthy();
      });
      // A later nav update (channel sync) must show up without a remount.
      await rerender({
        ...props,
        navNodes: [
          ...navNodes,
          { id: 'nav-ch-2', parentId: communityId, type: 'channel' as const, channelKind: 'text' as const, name: 'lighthouse', ...chanBase },
        ],
      });
      await waitFor(() => {
        expect(container.querySelector('[data-testid="nav-row-nav-ch-2"]')).toBeTruthy();
      });
    });

    it('channel-manage affordances honor a community-customized kick threshold (ZEB-733 parity)', async () => {
      // CodeRabbit #716: the gate must read the per-community governance kick
      // threshold (what verify_event enforces since ZEB-733), not the global
      // const — a community that RAISES kick must hide the affordances from a
      // power-60 member, and one that LOWERS it must show them to a power-40.
      const communityId = 'aa'.repeat(16);
      const chanBase = { unreadCount: 0, mentionCount: 0, unreadLevel: 'none' as const, expanded: false };
      const navNodes = [
        {
          id: communityId, parentId: null, type: 'community' as const, name: 'Test Community',
          expanded: true, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' as const,
        },
        { id: 'nav-ch-1', parentId: communityId, type: 'channel' as const, channelKind: 'text' as const, name: 'harbor', ...chanBase },
      ];

      // Raised threshold (kick 75), viewer power 60 → no manage affordances.
      const raised = await setup([general], { navNodes, myPower: 60 }, {
        get_community_governance: () => Promise.resolve({ adminQuorum: 1, kick: 75 }),
      });
      await waitFor(() => {
        expect(raised.container.querySelector('[data-testid="nav-row-nav-ch-1"]')).toBeTruthy();
      });
      await waitFor(() => {
        expect(raised.container.querySelector(`[data-testid="add-channel-row-${communityId}"]`)).toBeNull();
        expect(raised.container.querySelector('[data-testid="channel-menu-trigger-nav-ch-1"]')).toBeNull();
      });
      raised.unmount();

      // Lowered threshold (kick 30), viewer power 40 → affordances appear once
      // the governance snapshot resolves.
      const lowered = await setup([general], { navNodes, myPower: 40 }, {
        get_community_governance: () => Promise.resolve({ adminQuorum: 1, kick: 30 }),
      });
      await waitFor(() => {
        expect(lowered.container.querySelector(`[data-testid="add-channel-row-${communityId}"]`)).toBeTruthy();
        expect(lowered.container.querySelector('[data-testid="channel-menu-trigger-nav-ch-1"]')).toBeTruthy();
      });
    });

    it('the channels panel proposals row opens the Proposals view (bindable activeView)', async () => {
      const { container } = await setup([general], { votingAdapter: makeVotingAdapterStub() });
      await waitFor(() => {
        expect(container.querySelector('.channels-panel')).toBeTruthy();
      });
      const row = container.querySelector('[data-testid^="proposals-row-"]')!;
      expect(row).toBeTruthy();
      await fireEvent.click(row);
      await waitFor(() => {
        expect(row.classList.contains('active')).toBe(true);
      });
    });
  });

  // ZEB-663: default #general selection + delete-fallback cascade moved to App
  // (its resolution effect) and are unit-covered by resolveChannelSelection in
  // nav-utils.test.ts. CommunityView is now purely prop-driven off
  // `selectedChannelId`, so those cases live with the helper, not here.

  it('renders the feed for the prop-selected channel', async () => {
    const { container } = await setup([general, announcements], {
      selectedChannelId: announcements.channelId,
    });
    await waitFor(() => {
      expect(container.querySelector('.channel-message-feed .name')?.textContent?.trim()).toBe('announcements');
    });
  });

  it('routes a voice channel to VoiceChannelView (not the message feed)', async () => {
    // ZEB-351: V3 contract — a voice channel mounts the V3 VoiceChannelView
    // (root .voice-view) wired to the injected voiceSession, NOT the message
    // feed. The idle session shows the "Join Voice" pane.
    const { container, getByText } = await setup([voiceLounge]);
    await waitFor(() => {
      expect(container.querySelector('.voice-view')).toBeTruthy();
    });
    expect(container.querySelector('.channel-message-feed')).toBeNull();
    // The V3 join control (idle phase) is present.
    expect(getByText('Join Voice')).toBeTruthy();
  });

  it('does not mount the voice view until a voiceSession is provided (ZEB-351)', async () => {
    // Pre-ready window: get_self_voice_identity hasn't resolved, so
    // voiceSession is null. The voice routing is guarded — neither the voice
    // view nor the message feed renders for a voice channel.
    const { container } = await setup([voiceLounge], { voiceSession: null });
    await waitFor(() => {
      expect(container.querySelector('.community-view')).toBeTruthy();
    });
    expect(container.querySelector('.voice-view')).toBeNull();
    expect(container.querySelector('.channel-message-feed')).toBeNull();
  });

  it('routes a townhall channel to TownHallView (ZEB-612 S5)', async () => {
    const { container, getByText } = await setup([townhallFloor], {
      votingAdapter: makeVotingAdapterStub(),
    });
    await waitFor(() => {
      expect(container.querySelector('.townhall-view')).toBeTruthy();
    });
    expect(container.querySelector('.voice-view')).toBeNull();
    expect(getByText("Join the assembly — you'll join muted.")).toBeTruthy();
  });

  it('keeps a text channel on ChannelMessageFeed', async () => {
    const { container } = await setup([general]);
    await waitFor(() => {
      expect(container.querySelector('.channel-message-feed')).toBeTruthy();
    });
    expect(container.querySelector('.voice-view')).toBeNull();
  });

  it('clicking ⚙️ opens CommunitySettingsPanel modal', async () => {
    const { container, getByLabelText } = await setup();
    await waitFor(() => {
      expect(container.querySelector('.community-view')).toBeTruthy();
    });
    await fireEvent.click(getByLabelText(/Open community settings/i));
    await waitFor(() => {
      expect(document.querySelector('[role="dialog"]')).toBeTruthy();
    });
  });

  // ZEB-663: channel create/rename/delete triggers moved to the nav
  // (AddChannelNavRow + NavNodeRow context menu) and the dialogs are hoisted to
  // App. CommunityView no longer owns those affordances, so their tests live at
  // the App/nav level, not here.

  it('channel-config-updated Modified silently re-renders header', async () => {
    const { adapter, container } = await setup();
    await waitFor(() => {
      expect(container.querySelector('.channel-message-feed .name')?.textContent?.trim()).toBe('general');
    });

    // Re-list returns a renamed channel.
    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channels') return Promise.resolve([
        { ...general, name: 'general-renamed' },
        announcements,
      ]);
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });

    const handler = adapter.listeners.get('channel-config-updated')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: general.channelId,
        action: 'modified',
        name: 'general-renamed',
        atWallMs: 200,
      },
    });

    await waitFor(() => {
      expect(container.querySelector('.channel-message-feed .name')?.textContent?.trim()).toBe('general-renamed');
    });
  });

  // ZEB-663: delete-active cascade to next-newest is App's resolution effect
  // now — see resolveChannelSelection in nav-utils.test.ts
  // ('cascades off a just-deleted active channel'). The empty-state path below
  // is CommunityView's own render responsibility and stays here.

  it('channel-config-updated Deleted on last remaining channel renders empty-state', async () => {
    const { adapter, container } = await setup([general]);
    await waitFor(() => {
      expect(container.querySelector('.channel-message-feed')).toBeTruthy();
    });

    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channels') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });

    const handler = adapter.listeners.get('channel-config-updated')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: general.channelId,
        action: 'deleted',
        atWallMs: 300,
      },
    });

    await waitFor(() => {
      expect(container.querySelector('.empty-channels')).toBeTruthy();
    });
  });

  // ZEB-663: channel selection is nav-driven (App.openCommunityChannel) and the
  // create-power gate lives on the nav's AddChannelNavRow, both exercised at the
  // App/nav level — no ChannelSubSidebar here to click.

  it('Constitutional tab mounts Tier3ProposalPanel when votingAdapter is provided', async () => {
    const votingAdapter = new VotingAdapter();
    votingAdapter.listTier3Polls = vi.fn().mockResolvedValue([]);
    // Stub out all subscribe methods so onMount doesn't error on an unconnected adapter.
    const noopUnsub = () => {};
    votingAdapter.subscribeTier3PollCreated = vi.fn().mockReturnValue(noopUnsub);
    votingAdapter.subscribeTier3SortitionComplete = vi.fn().mockReturnValue(noopUnsub);
    votingAdapter.subscribeTier3DraftingOpen = vi.fn().mockReturnValue(noopUnsub);
    votingAdapter.subscribeTier3RatificationOpen = vi.fn().mockReturnValue(noopUnsub);
    votingAdapter.subscribeTier3Finalized = vi.fn().mockReturnValue(noopUnsub);

    const { container, getByText } = await setup(undefined, { votingAdapter });

    // Wait for the view tabs to appear.
    await waitFor(() => {
      expect(getByText('Constitutional')).toBeTruthy();
    });

    // Click the Constitutional tab.
    await fireEvent.click(getByText('Constitutional'));

    // Tier3ProposalPanel renders a <section class="tier3-panel"> as its root.
    await waitFor(() => {
      expect(container.querySelector('.tier3-panel')).toBeTruthy();
    });
  });

  it('drives card subscriptions for the JOINED member set in the channel view (no overlay) and tears down on destroy (ZEB-341)', async () => {
    const subscribeVisibleCards = vi.fn();
    const unsubscribeCards = vi.fn();
    const bob: CommunityMember = {
      address: 'bb'.repeat(20),
      displayName: 'Bob',
      power: 0,
      status: 'joined',
    };
    const eve: CommunityMember = {
      address: 'ee'.repeat(20),
      displayName: 'Eve',
      power: 0,
      status: 'banned',
    };
    const { unmount } = await setup(undefined, {
      members: [adminMember, bob, eve],
      subscribeVisibleCards,
      unsubscribeCards,
    });

    // The channel view itself drives subscription — the members overlay
    // (.community-members-panel) is NOT mounted here, proving message-author
    // name resolution no longer depends on the user opening that overlay
    // (regression guard for Cursor Bugbot's "subscriber pool never active
    // unless overlay panel open" finding on PR #171).
    await waitFor(() => expect(subscribeVisibleCards).toHaveBeenCalled());
    expect(document.querySelector('.community-members-panel')).toBeNull();
    const lastCallArgs = subscribeVisibleCards.mock.calls.at(-1)![0] as string[];
    // Joined members are subscribed; the banned member (Eve) is NOT — the
    // active subscription count is bounded by live membership, not lifetime
    // ban accumulation.
    expect(lastCallArgs).toEqual(
      expect.arrayContaining([adminMember.address, bob.address]),
    );
    expect(lastCallArgs).not.toContain(eve.address);

    // Leaving the community view (component destroy = "view change" in the
    // spec) tears down all subscriptions + the poll loop.
    unmount();
    expect(unsubscribeCards).toHaveBeenCalled();
  });

  it('activeView is externally drivable to proposals (ZEB-606 deep-link)', async () => {
    const votingHost = makeAdapter();
    (votingHost.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'voting_list_tier2_proposals') return Promise.resolve([]);
      if (cmd === 'voting_get_my_delegate') return Promise.resolve(null);
      return Promise.resolve(undefined);
    });
    const votingAdapter = new VotingAdapter();
    await votingAdapter.connectAdapter(votingHost);
    const { container } = await setup([general, announcements], {
      votingAdapter,
      activeView: 'proposals',
    });
    await waitFor(() => {
      expect(container.querySelector('.community-proposals')).toBeTruthy();
    });
  });

  it('Charter tab mounts CharterView when votingAdapter is provided (ZEB-608)', async () => {
    const { container, getByText } = await setup(undefined, {
      votingAdapter: makeVotingAdapterStub(),
    });
    await waitFor(() => {
      expect(getByText('Charter')).toBeTruthy();
    });
    await fireEvent.click(getByText('Charter'));
    await waitFor(() => {
      expect(container.querySelector('.charter-view')).toBeTruthy();
    });
  });

  it('activeView is externally drivable to charter (deep-link)', async () => {
    const { container } = await setup(undefined, {
      votingAdapter: makeVotingAdapterStub(),
      activeView: 'charter',
    });
    await waitFor(() => {
      expect(container.querySelector('.charter-view')).toBeTruthy();
    });
  });

  it('a governance activeView without a votingAdapter shows an unavailable state, not the channel feed (Greptile #410)', async () => {
    // Deep-linked to a governance view before the voting adapter is ready:
    // the guarded governance branches fall through, and we must NOT silently
    // render channel content under a governance tab state.
    const { container } = await setup([general, announcements], {
      votingAdapter: undefined,
      activeView: 'charter',
    });
    await waitFor(() => {
      expect(container.querySelector('.community-view')).toBeTruthy();
    });
    expect(container.querySelector('[role="status"]')?.textContent).toContain(
      'live connection to community governance',
    );
    expect(container.querySelector('.channel-message-feed')).toBeNull();
  });

  it('Propose amendment switches the view to the Constitutional tab', async () => {
    const { container, getByText, getByRole } = await setup(undefined, {
      votingAdapter: makeVotingAdapterStub(),
      activeView: 'charter',
    });
    await waitFor(() => {
      expect(container.querySelector('.charter-view')).toBeTruthy();
    });
    await fireEvent.click(getByRole('button', { name: 'Propose amendment' }));
    await waitFor(() => {
      expect(container.querySelector('.tier3-panel')).toBeTruthy();
    });
    expect(getByText('Constitutional').getAttribute('aria-pressed')).toBe('true');
  });

  it('threads the fetched admin quorum into the settings panel (fixes always-shows-1, ZEB-608 §0.2)', async () => {
    const { container, getByLabelText, getByText } = await setup(
      undefined,
      {},
      { get_community_governance: () => Promise.resolve({ adminQuorum: 2 }) },
    );
    await waitFor(() => {
      expect(container.querySelector('.community-view')).toBeTruthy();
    });
    await fireEvent.click(getByLabelText(/Open community settings/i));
    await waitFor(() => {
      // adminMember (myPower 100) sees the admin-governance section with the
      // REAL fetched quorum, not the component default of 1.
      expect(getByText(/Current admin quorum: 2 of/)).toBeTruthy();
    });
  });
});
