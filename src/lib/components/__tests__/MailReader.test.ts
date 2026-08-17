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
import { formatClockTime } from '../../time-format';

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
const WORD_DATE = new Date(MS).toLocaleDateString([], {
  year: 'numeric',
  month: 'short',
  day: 'numeric',
});

describe('MailReader header time honors the clock preference (ZEB-946)', () => {
  afterEach(() => {
    _resetTimeFormatServiceForTest();
  });

  it('renders the header time in the chosen 24h clock, keeping the word-month date', () => {
    setTimeFormatSettings({ clock: '24h', dateOrder: 'system' });
    const { container } = render(MailReader, { props: { message: makeMessage(TS) } });
    expect(headerDate(container)).toBe(`${WORD_DATE}, ${formatClockTime(MS, { hour12: false })}`);
  });

  it('is byte-identical to the prior locale header at system default', () => {
    const { container } = render(MailReader, { props: { message: makeMessage(TS) } });
    const prior = new Date(MS).toLocaleString([], {
      year: 'numeric',
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
    });
    expect(headerDate(container)).toBe(prior);
  });
});
