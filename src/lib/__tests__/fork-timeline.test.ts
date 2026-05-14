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

  it('interleaves messages by HLC ascending across the boundary', () => {
    // Snapshot has a message that is more recent than the first live message
    // (shouldn't happen in practice, but the merge must still be correct).
    const snap = [makeMsg(100), makeMsg(350)];
    const live = [makeMsg(200), makeMsg(400)];
    const rows = buildUnifiedTimeline(snap, live, PARENT_NAME, FORKED_AT_MS);
    const msgs = rows.filter((r) => 'msg' in r) as Array<{ msg: ChannelMessageDto; isPreFork: boolean }>;
    const times = msgs.map((r) => r.msg.at.wallMs);
    expect(times).toEqual([100, 200, 350, 400]);
    // Divider appears before the first live message in the merged stream.
    const divIdx = rows.findIndex((r) => 'kind' in r);
    expect(divIdx).toBeGreaterThan(0);
    const firstLiveIdx = rows.findIndex((r) => 'isPreFork' in r && !r.isPreFork);
    expect(firstLiveIdx).toBe(divIdx + 1);
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
});
