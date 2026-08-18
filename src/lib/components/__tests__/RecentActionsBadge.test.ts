import { render, fireEvent } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import RecentActionsBadge from '../RecentActionsBadge.svelte';
import type { ModerationEvent } from '../../types';

// 32-char owner_id hex fixtures (actor + target).
const ACTOR = 'ab'.repeat(16);
const TARGET = 'cd'.repeat(16);

function kickEvent(): ModerationEvent {
  return {
    eventId: 'e1',
    kind: 'kick',
    actorAddr: ACTOR,
    targetAddr: TARGET,
    reason: null,
    newPower: null,
    hlc: { wallMs: 1_700_000_000_000, logical: 0, deviceId: '00' },
  };
}

// The badge starts collapsed; events render only once expanded.
async function expand(getByRole: (role: string, opts: { name: RegExp }) => HTMLElement) {
  await fireEvent.click(getByRole('button', { name: /recent moderation actions/i }));
}

describe('RecentActionsBadge display-name resolution (ZEB-961)', () => {
  it('resolves actor + target through the ladder: nickname over card', async () => {
    const { getByRole, getByText } = render(RecentActionsBadge, {
      props: {
        events: [kickEvent()],
        resolveCard: (id: string) =>
          id === ACTOR
            ? { displayName: 'ActorCard', statusText: '' }
            : id === TARGET
              ? { displayName: 'TargetCard', statusText: '' }
              : undefined,
        // Actor has a local nickname (wins over the card); target has none.
        resolveNickname: (id: string) => (id === ACTOR ? 'ActorNick' : undefined),
      },
    });
    await expand(getByRole);
    expect(getByText(/ActorNick kicked TargetCard/)).toBeTruthy();
  });

  it('falls back to short hex when neither nickname nor card resolves', async () => {
    const { getByRole, getByText } = render(RecentActionsBadge, {
      props: {
        events: [kickEvent()],
        resolveCard: () => undefined,
        resolveNickname: () => undefined,
      },
    });
    await expand(getByRole);
    expect(
      getByText(new RegExp(`${ACTOR.slice(0, 8)} kicked ${TARGET.slice(0, 8)}`)),
    ).toBeTruthy();
  });
});
