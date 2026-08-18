import { describe, it, expect, afterEach } from 'vitest';
import { render } from '@testing-library/svelte';
import MailInbox from '../MailInbox.svelte';
import type { MailEntry } from '../../types';
// ZEB-946: the "today" mail time honors the owner's clock preference (12h/24h).
import {
  setTimeFormatSettings,
  _resetTimeFormatServiceForTest,
} from '../../time-format-service';
import { formatClockTime } from '../../time-format';
// ZEB-961: the sender label resolves through the shared display-name ladder.
import { shortId } from '../../short-addr';

function makeEntry(timestampSec: number): MailEntry {
  return {
    messageCid: 'cid-1',
    messageId: 'mid-1',
    senderAddress: 'aa'.repeat(16),
    timestamp: timestampSec,
    subjectSnippet: 'hello',
    read: false,
    bodyState: 'local',
  };
}

function mailTime(container: HTMLElement): string | null {
  return container.querySelector('.mail-time')?.textContent ?? null;
}

describe('MailInbox time honors the clock preference (ZEB-946)', () => {
  afterEach(() => {
    _resetTimeFormatServiceForTest();
  });

  it("renders today's mail time in the chosen 24h clock", () => {
    setTimeFormatSettings({ clock: '24h', dateOrder: 'system' });
    const nowSec = Math.floor(Date.now() / 1000); // same calendar day → time-of-day branch
    const { container } = render(MailInbox, {
      props: { entries: [makeEntry(nowSec)], activeFolder: 'inbox', selectedCid: null },
    });
    expect(mailTime(container)).toBe(formatClockTime(nowSec * 1000, { hour12: false }));
  });

  it('follows the locale (unchanged) when the clock preference is system default', () => {
    const nowSec = Math.floor(Date.now() / 1000);
    const { container } = render(MailInbox, {
      props: { entries: [makeEntry(nowSec)], activeFolder: 'inbox', selectedCid: null },
    });
    // Default prefs → byte-identical to the prior raw toLocaleTimeString().
    expect(mailTime(container)).toBe(
      new Date(nowSec * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }),
    );
  });

  // ZEB-952: an old message routes through the calendar-day seam to the
  // month/day bucket (deterministic — any real `now` is >7 days after 2020).
  // Locks that MailInbox exercises the non-today path, not just "today".
  it('renders an old message as month/day (calendar-day bucketing wired through the seam)', () => {
    const oldMs = new Date(2020, 0, 15, 9, 0).getTime(); // 2020-01-15 09:00 local
    const { container } = render(MailInbox, {
      props: {
        entries: [makeEntry(Math.floor(oldMs / 1000))],
        activeFolder: 'inbox',
        selectedCid: null,
      },
    });
    expect(mailTime(container)).toBe(
      new Date(oldMs).toLocaleDateString([], { month: 'short', day: 'numeric' }),
    );
  });
});

describe('MailInbox sender name resolution (ZEB-961)', () => {
  const SENDER = 'aa'.repeat(16); // 32-char owner_id hex

  function entry(): MailEntry {
    return {
      messageCid: 'cid-1',
      messageId: 'mid-1',
      senderAddress: SENDER,
      timestamp: Math.floor(Date.now() / 1000),
      subjectSnippet: 'hi',
      read: false,
      bodyState: 'local',
    };
  }

  function senderLabel(container: HTMLElement): string | null {
    return container.querySelector('.mail-sender')?.textContent ?? null;
  }

  it('resolves the sender card name when a resolver is provided', () => {
    const { container } = render(MailInbox, {
      props: {
        entries: [entry()],
        activeFolder: 'inbox',
        selectedCid: null,
        resolveCard: (id: string) =>
          id === SENDER ? { displayName: 'Postmaster', statusText: '' } : undefined,
      },
    });
    expect(senderLabel(container)).toBe('Postmaster');
  });

  it('falls back to the shared shortId when no card resolves', () => {
    const { container } = render(MailInbox, {
      props: {
        entries: [entry()],
        activeFolder: 'inbox',
        selectedCid: null,
        resolveCard: () => undefined,
      },
    });
    expect(senderLabel(container)).toBe(shortId(SENDER));
  });
});
