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
});
