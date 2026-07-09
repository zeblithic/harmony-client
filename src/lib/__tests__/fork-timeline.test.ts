import { describe, it, expect } from 'vitest';
import { buildUnifiedTimeline } from '../fork-timeline';
import type { ChannelMessageDto } from '../channel-message-service';

function makeMsg(wallMs: number, logical = 0, deviceId = 'dev0'): ChannelMessageDto {
  return {
    messageId: `msg-${wallMs}-${logical}-${deviceId}`,
    communityId: 'aa'.repeat(16),
    channelId: 'bb'.repeat(16),
    author: 'cc'.repeat(20),
    at: { wallMs, logical, deviceId },
    body: [],
  };
}

const PARENT_NAME = 'Original Community';
const FORKED_AT_MS = 1000;

describe('buildUnifiedTimeline', () => {
  it('returns pre-fork messages HLC-ascending with no divider when live is empty', () => {
    const snap = [makeMsg(100), makeMsg(200)];
    const rows = buildUnifiedTimeline(snap, [], PARENT_NAME, FORKED_AT_MS);
    expect(rows).toHaveLength(2);
    expect(rows.every((r) => 'isPreFork' in r && r.isPreFork)).toBe(true);
  });

  it('returns live messages with no divider when snapshot is empty', () => {
    const live = [makeMsg(300), makeMsg(400)];
    const rows = buildUnifiedTimeline([], live, PARENT_NAME, FORKED_AT_MS);
    expect(rows).toHaveLength(2);
    expect(rows.every((r) => 'isPreFork' in r && !r.isPreFork)).toBe(true);
  });

  it('inserts a fork-divider between pre-fork and live messages', () => {
    const snap = [makeMsg(100), makeMsg(200)];
    const live = [makeMsg(300), makeMsg(400)];
    const rows = buildUnifiedTimeline(snap, live, PARENT_NAME, FORKED_AT_MS);
    expect(rows).toHaveLength(5); // 2 pre + divider + 2 live
    expect(rows[0]).toMatchObject({ isPreFork: true, msg: { at: { wallMs: 100 } } });
    expect(rows[1]).toMatchObject({ isPreFork: true, msg: { at: { wallMs: 200 } } });
    expect(rows[2]).toMatchObject({ kind: 'fork-divider', originalCommunityName: PARENT_NAME, forkedAtMs: FORKED_AT_MS });
    expect(rows[3]).toMatchObject({ isPreFork: false, msg: { at: { wallMs: 300 } } });
    expect(rows[4]).toMatchObject({ isPreFork: false, msg: { at: { wallMs: 400 } } });
  });

  it('interleaves messages by HLC ascending and places divider after the last pre-fork row', () => {
    // A pre-fork snapshot message (350) sorts later than a live message (200).
    // Merged order: [pre100, live200, pre350, live400].
    // Correct divider position: after the LAST pre-fork row (pre350), i.e. before live400.
    // The divider must NOT appear before live200 — that would split the live-only prefix.
    const snap = [makeMsg(100), makeMsg(350)];
    const live = [makeMsg(200), makeMsg(400)];
    const rows = buildUnifiedTimeline(snap, live, PARENT_NAME, FORKED_AT_MS);
    const msgs = rows.filter((r) => 'msg' in r) as Array<{ msg: ChannelMessageDto; isPreFork: boolean }>;
    const times = msgs.map((r) => r.msg.at.wallMs);
    expect(times).toEqual([100, 200, 350, 400]);
    // Divider appears after the last pre-fork row (pre350) and before live400.
    expect(rows).toHaveLength(5); // 2 pre + 1 interleaved live + divider + 1 live
    const divIdx = rows.findIndex((r) => 'kind' in r);
    expect(divIdx).toBe(3); // [pre100, live200, pre350, divider, live400]
    expect(rows[divIdx + 1]).toMatchObject({ isPreFork: false, msg: { at: { wallMs: 400 } } });
  });

  it('places divider at last-pre→first-post-live boundary, not at first live row, when live messages sort earlier', () => {
    // All live messages sort earlier than all pre-fork messages by HLC.
    // After merging: [live100, live200, pre300, pre400].
    // There is no live row AFTER a pre-fork row, so no divider should be inserted.
    const snap = [makeMsg(300), makeMsg(400)];
    const live = [makeMsg(100), makeMsg(200)];
    const rows = buildUnifiedTimeline(snap, live, PARENT_NAME, FORKED_AT_MS);
    // No divider: all live messages precede all pre-fork messages.
    const divIdx = rows.findIndex((r) => 'kind' in r);
    expect(divIdx).toBe(-1);
    // All 4 messages still present in HLC order.
    expect(rows).toHaveLength(4);
    const msgs = rows as Array<{ msg: ChannelMessageDto; isPreFork: boolean }>;
    expect(msgs.map((r) => r.msg.at.wallMs)).toEqual([100, 200, 300, 400]);
  });

  it('places divider correctly when live and pre-fork messages interleave by HLC', () => {
    // Interleaved: pre100, live150, pre300, live400.
    // Last pre-fork index = 2 (pre300). First live after that = index 3 (live400).
    // Divider should appear before live400, not before live150.
    const snap = [makeMsg(100), makeMsg(300)];
    const live = [makeMsg(150), makeMsg(400)];
    const rows = buildUnifiedTimeline(snap, live, PARENT_NAME, FORKED_AT_MS);
    // Merged: [pre100, live150, pre300, divider, live400]
    expect(rows).toHaveLength(5);
    expect(rows[0]).toMatchObject({ isPreFork: true, msg: { at: { wallMs: 100 } } });
    expect(rows[1]).toMatchObject({ isPreFork: false, msg: { at: { wallMs: 150 } } });
    expect(rows[2]).toMatchObject({ isPreFork: true, msg: { at: { wallMs: 300 } } });
    expect(rows[3]).toMatchObject({ kind: 'fork-divider' });
    expect(rows[4]).toMatchObject({ isPreFork: false, msg: { at: { wallMs: 400 } } });
  });

  it('respects HLC tie-breaking: logical then deviceId', () => {
    const snap = [makeMsg(100, 0, 'aaa'), makeMsg(100, 1, 'aaa')];
    const live = [makeMsg(100, 0, 'bbb'), makeMsg(200, 0, 'aaa')];
    const rows = buildUnifiedTimeline(snap, live, PARENT_NAME, FORKED_AT_MS);
    const msgs = rows.filter((r) => 'msg' in r) as Array<{ msg: ChannelMessageDto }>;
    expect(msgs[0].msg.at).toMatchObject({ wallMs: 100, logical: 0, deviceId: 'aaa' });
    expect(msgs[1].msg.at).toMatchObject({ wallMs: 100, logical: 0, deviceId: 'bbb' });
    expect(msgs[2].msg.at).toMatchObject({ wallMs: 100, logical: 1, deviceId: 'aaa' });
    expect(msgs[3].msg.at).toMatchObject({ wallMs: 200, logical: 0, deviceId: 'aaa' });
  });

  it('ZEB-649: divider carries forkReason when provided, null by default', () => {
    const pre = [makeMsg(100)];
    const live = [makeMsg(200)];
    const withReason = buildUnifiedTimeline(pre, live, 'Origin', 150, 'Treasury split');
    const divider = withReason.find((r) => 'kind' in r);
    expect(divider && 'forkReason' in divider ? divider.forkReason : undefined).toBe(
      'Treasury split',
    );
    const without = buildUnifiedTimeline(pre, live, 'Origin', 150);
    const divider2 = without.find((r) => 'kind' in r);
    expect(divider2 && 'forkReason' in divider2 ? divider2.forkReason : undefined).toBeNull();
  });
});
