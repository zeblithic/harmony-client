import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { writable } from 'svelte/store';
import TownHallView from '../TownHallView.svelte';
import { ChannelMessageService } from '../../channel-message-service';
import type { TauriAdapter } from '../../zenoh-service';

function fakeSession(state: object) {
  return {
    state: writable({
      phase: 'idle',
      community: null,
      channel: null,
      muted: true,
      deafened: false,
      pttMode: false,
      pttHeld: false,
      roster: [],
      channelFull: false,
      reconnecting: false,
      micBlocked: false,
      selfPower: 0,
      selfModMuted: false,
      selfKicked: false,
      selfInvited: false,
      ...state,
    }),
    join: vi.fn(async () => {}),
    leave: vi.fn(async () => {}),
    setMuted: vi.fn(async () => {}),
    setDeafened: vi.fn(async () => {}),
    setPttMode: vi.fn(async () => {}),
    setPttHeld: vi.fn(),
    setHand: vi.fn(async () => {}),
    inviteToSpeak: vi.fn(async () => {}),
    moderate: vi.fn(async () => {}),
    clearChannelFull: vi.fn(),
  };
}

function member(i: number, over: Record<string, unknown> = {}) {
  return {
    ownerHex: String(i).padStart(2, '0').repeat(16),
    deviceHex: String(i).padStart(2, '0').repeat(32),
    muted: false,
    speaking: false,
    displayName: `User${i}`,
    modMuted: false,
    power: 0,
    handRaisedAt: null,
    // ZEB-853 D5: local first-observed stamp — the speaker queue orders on
    // THIS, not the peer-supplied handRaisedAt. Default null (not raised);
    // tests that raise a hand and assert queue ORDER must set this too.
    handFirstObservedMs: null,
    invited: false,
    ...over,
  };
}

function makeTauriAdapter(): TauriAdapter {
  return {
    invoke: vi.fn(async (cmd: string) => {
      if (cmd === 'list_channel_messages') return [];
      return undefined;
    }),
    listen: vi.fn(async () => () => {}),
  } as never;
}

async function setup(sessionState: object, propOverrides: Record<string, unknown> = {}) {
  const session = fakeSession(sessionState);
  const channelMessageService = new ChannelMessageService();
  await channelMessageService.connectAdapter(makeTauriAdapter());
  const votingAdapter = {
    createTier1Poll: vi.fn(async () => '11'.repeat(32)),
    getPoll: vi.fn(async () => ({ meta: {}, tally: { counts: [], ballot_count: 0 }, options: [] })),
    listActivePolls: vi.fn(async () => []),
    subscribePollCreated: vi.fn(() => () => {}),
    subscribeBallotCast: vi.fn(() => () => {}),
    createTier2Proposal: vi.fn(async () => '22'.repeat(32)),
  };
  const props = {
    session: session as never,
    channelName: 'assembly',
    communityId: 'aa'.repeat(16),
    channelId: 'bb'.repeat(16),
    ownAddress: 'cc'.repeat(16),
    myPower: 50,
    adminQuorum: 3,
    votingAdapter: votingAdapter as never,
    channelMessageService,
    ...propOverrides,
  };
  const result = render(TownHallView, { props });
  return { session, votingAdapter, ...result };
}

describe('TownHallView (ZEB-612 S5): join pane + header', () => {
  it('idle shows the assembly join pane with the join-muted hint', async () => {
    const { session } = await setup({ phase: 'idle' });
    expect(screen.getByText("Join the assembly — you'll join muted.")).toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: /join assembly/i }));
    expect(session.join).toHaveBeenCalledWith('aa'.repeat(16), 'bb'.repeat(16));
  });

  it('header shows LIVE dot and distinct-owner present count', async () => {
    // Two devices of owner 1 + one device of owner 2 → 2 present.
    const twin = member(1, { deviceHex: 'f'.repeat(64) });
    await setup({ phase: 'connected', roster: [member(1), twin, member(2)] });
    expect(screen.getByTestId('th-live-dot')).toBeInTheDocument();
    expect(screen.getByTestId('th-present')).toHaveTextContent('2 present');
  });
});

describe('TownHallView (ZEB-612 S5): spotlight', () => {
  it('empty state when no one is or was speaking', async () => {
    await setup({ phase: 'connected', roster: [member(1)] });
    expect(screen.getByTestId('th-spotlight-empty')).toHaveTextContent('No one has the floor.');
  });

  it('shows the speaking member with 🎙 speaking, a timer, and a MOD pill at power ≥ 50', async () => {
    await setup({
      phase: 'connected',
      roster: [member(1, { speaking: true, power: 60 }), member(2)],
    });
    const spot = screen.getByTestId('th-spotlight');
    expect(spot).toHaveTextContent('User1');
    expect(spot).toHaveTextContent(/🎙 speaking/);
    expect(screen.getByTestId('th-mod-pill')).toHaveTextContent('MOD');
    expect(screen.getByTestId('th-timer')).toBeInTheDocument();
    expect(screen.getByTestId('th-waveform')).toBeInTheDocument();
  });
});

describe('TownHallView (ZEB-612 S5): in-room grid', () => {
  it('renders hand and mute badges from real roster data', async () => {
    await setup({
      phase: 'connected',
      roster: [member(1, { handRaisedAt: 1000 }), member(2, { muted: true })],
    });
    expect(screen.getByTestId('th-hand-badge')).toBeInTheDocument();
    expect(screen.getByLabelText('muted')).toBeInTheDocument();
  });

  it('caps the grid at 18 tiles with a mono +N more overflow tile', async () => {
    const roster = Array.from({ length: 21 }, (_, i) => member(i));
    await setup({ phase: 'connected', roster });
    expect(screen.getAllByTestId('th-tile')).toHaveLength(18);
    expect(screen.getByTestId('th-overflow')).toHaveTextContent('+3 more');
  });
});

describe('TownHallView (ZEB-612 S5): control bar + raise hand', () => {
  it('raise-hand toggle calls setHand(true) then reads Lower hand', async () => {
    const { session } = await setup({ phase: 'connected', roster: [member(1)] });
    const btn = screen.getByTestId('th-raise-hand');
    expect(btn).toHaveTextContent('✋ Raise hand');
    await fireEvent.click(btn);
    expect(session.setHand).toHaveBeenCalledWith(true);
    await waitFor(() => expect(btn).toHaveTextContent('✋ Lower hand'));
  });

  it('keeps the voice controls (mute toggle, deafen, leave)', async () => {
    const { session } = await setup({ phase: 'connected', muted: true, roster: [member(1)] });
    await fireEvent.click(screen.getByRole('button', { name: /unmute|muted/i }));
    expect(session.setMuted).toHaveBeenCalledWith(false);
    await fireEvent.click(screen.getByRole('button', { name: /leave/i }));
    expect(session.leave).toHaveBeenCalled();
  });
});

describe('TownHallView (ZEB-612 S5): speaker queue', () => {
  it('orders raised hands by first-observed time and numbers the rows', async () => {
    await setup({
      phase: 'connected',
      roster: [
        // ZEB-853 D5: the queue orders on the LOCAL handFirstObservedMs, not
        // the peer-supplied handRaisedAt — stamp both here so the fixture
        // exercises the real (production) ordering key.
        member(1, { handRaisedAt: 2000, handFirstObservedMs: 2000 }),
        member(2, { handRaisedAt: 1000, handFirstObservedMs: 1000 }),
        member(3),
      ],
    });
    const rows = screen.getAllByTestId('th-queue-row');
    expect(rows).toHaveLength(2);
    expect(rows[0]).toHaveTextContent('User2');
    expect(rows[0]).toHaveTextContent('wants to speak ✋');
    expect(rows[1]).toHaveTextContent('User1');
  });

  it('Invite is visible to all but disabled below power 50; click delegates to inviteToSpeak', async () => {
    const { session } = await setup({
      phase: 'connected',
      selfPower: 0,
      roster: [member(1, { handRaisedAt: 1000 })],
    });
    expect(screen.getByTestId('th-invite')).toBeDisabled();
    session.state.update((s) => ({ ...s, selfPower: 60 }));
    await waitFor(() => expect(screen.getByTestId('th-invite')).toBeEnabled());
    await fireEvent.click(screen.getByTestId('th-invite'));
    expect(session.inviteToSpeak).toHaveBeenCalledWith(member(1).ownerHex);
  });

  it('invited entries dim with an "invited" label instead of the button', async () => {
    await setup({
      phase: 'connected',
      roster: [member(1, { handRaisedAt: 1000, invited: true })],
    });
    expect(screen.queryByTestId('th-invite')).not.toBeInTheDocument();
    expect(screen.getByTestId('th-queue-row')).toHaveTextContent('invited');
  });

  it('shows an empty-queue note when no hands are raised', async () => {
    await setup({ phase: 'connected', roster: [member(1)] });
    expect(screen.getByText('No raised hands yet.')).toBeInTheDocument();
  });
});

describe('TownHallView (ZEB-612 S5): invited banner', () => {
  it('selfInvited shows the banner; Unmute accepts (unmute + lower hand)', async () => {
    const { session } = await setup({ phase: 'connected', selfInvited: true, roster: [member(1)] });
    const banner = screen.getByTestId('th-invited-banner');
    expect(banner).toHaveTextContent("You've been invited to speak — Unmute?");
    await fireEvent.click(screen.getByTestId('th-invite-accept'));
    expect(session.setMuted).toHaveBeenCalledWith(false);
    expect(session.setHand).toHaveBeenCalledWith(false);
    await waitFor(() => {
      expect(screen.queryByTestId('th-invited-banner')).not.toBeInTheDocument();
    });
  });

  it('Dismiss lowers the hand without unmuting', async () => {
    const { session } = await setup({ phase: 'connected', selfInvited: true, roster: [member(1)] });
    await fireEvent.click(screen.getByTestId('th-invite-dismiss'));
    expect(session.setHand).toHaveBeenCalledWith(false);
    expect(session.setMuted).not.toHaveBeenCalled();
    await waitFor(() => {
      expect(screen.queryByTestId('th-invited-banner')).not.toBeInTheDocument();
    });
  });

  it('Unmute is disabled while mod-silenced (accept would be a silent no-op)', async () => {
    await setup({
      phase: 'connected',
      selfInvited: true,
      selfModMuted: true,
      roster: [member(1)],
    });
    expect(screen.getByTestId('th-invite-accept')).toBeDisabled();
    expect(screen.getByTestId('th-invite-dismiss')).toBeEnabled();
  });

  it('a failed lower-hand IPC keeps the banner up and surfaces the error', async () => {
    const { session } = await setup({ phase: 'connected', selfInvited: true, roster: [member(1)] });
    (session.setHand as ReturnType<typeof vi.fn>).mockRejectedValueOnce(new Error('engine gone'));
    await fireEvent.click(screen.getByTestId('th-invite-dismiss'));
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent(/engine gone/);
    expect(screen.getByTestId('th-invited-banner')).toBeInTheDocument();
  });
});

describe('TownHallView (ZEB-612 S5): rail — motion card + backchannel', () => {
  it('renders the motion card and the backchannel with the room composer placeholder', async () => {
    await setup({ phase: 'connected', roster: [member(1)] });
    expect(screen.getByText('⚖ Call this to a motion')).toBeInTheDocument();
    await waitFor(() => {
      expect(document.querySelector('[placeholder="Message the room…"]')).toBeTruthy();
    });
  });
});
