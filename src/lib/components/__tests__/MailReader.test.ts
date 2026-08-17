import { describe, it, expect, afterEach } from 'vitest';
import { render } from '@testing-library/svelte';
import MailReader from '../MailReader.svelte';
import type { MailMessageDetail } from '../../types';
// ZEB-946: the mail header time honors the owner's clock preference (12h/24h);
// the word-month date is intentionally left readable/locale-default.
import {
  setTimeFormatSettings,
  _resetTimeFormatServiceForTest,
} from '../../time-format-service';

function makeMessage(timestampSec: number): MailMessageDetail {
  return {
    messageCid: 'cid-1',
    messageId: 'mid-1',
    subject: 'Subject',
    body: 'Body',
    senderAddress: 'aa'.repeat(16),
    recipients: [],
    timestamp: timestampSec,
    attachments: [],
    isReply: false,
    isForward: false,
    bodyState: 'local',
  };
}

function headerDate(container: HTMLElement): string | null {
  return container.querySelector('.date')?.textContent ?? null;
}

const TS = 1_700_000_000; // seconds
const MS = TS * 1000;

// The header keeps the single locale-native combined format (its date/time
// separator is locale-chosen — see the component comment), threading only the
// clock axis. So the expected string is `toLocaleString` with the same options,
// which is locale-robust: it holds under whatever locale the test runtime uses
// (unlike hand-joining the date and time with a literal ", ").
const HEADER_OPTIONS: Intl.DateTimeFormatOptions = {
  year: 'numeric',
  month: 'short',
  day: 'numeric',
  hour: '2-digit',
  minute: '2-digit',
};

describe('MailReader header time honors the clock preference (ZEB-946)', () => {
  afterEach(() => {
    _resetTimeFormatServiceForTest();
  });

  it('renders the header time in the chosen 24h clock', () => {
    setTimeFormatSettings({ clock: '24h', dateOrder: 'system' });
    const { container } = render(MailReader, { props: { message: makeMessage(TS) } });
    expect(headerDate(container)).toBe(
      new Date(MS).toLocaleString([], { ...HEADER_OPTIONS, hour12: false }),
    );
  });

  it('is byte-identical to the prior locale-native header at system default', () => {
    const { container } = render(MailReader, { props: { message: makeMessage(TS) } });
    // No override → the exact prior `toLocaleString()` output, in ANY locale
    // (the combined call preserves the locale-native date/time separator).
    expect(headerDate(container)).toBe(new Date(MS).toLocaleString([], HEADER_OPTIONS));
  });
});
