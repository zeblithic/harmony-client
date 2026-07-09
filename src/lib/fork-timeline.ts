/**
 * ZEB-285 Phase 1 Task 11: pure helpers for merging pre-fork snapshot
 * messages with live channel messages into a unified HLC-ordered timeline.
 *
 * Extracted from ChannelMessageFeed.svelte so this logic can be unit-tested
 * without a DOM environment.
 */

import type { ChannelMessageDto } from './channel-message-service';
import { compareHlc } from './hlc';

/** A message in the unified timeline — either from the snapshot (pre-fork)
 *  or from the live channel log (post-fork). */
export interface TimelineMessage {
  msg: ChannelMessageDto;
  isPreFork: boolean;
}

/** A separator row in the unified timeline rendered at the fork boundary. */
export interface ForkDivider {
  kind: 'fork-divider';
  originalCommunityName: string;
  forkedAtMs: number;
  /** ZEB-649: the forker's stated why, quoted on the divider band.
   *  Null for forks created before the reason field existed. */
  forkReason: string | null;
}

/** A single row in the rendered timeline — either a message or a divider. */
export type TimelineRow = TimelineMessage | ForkDivider;

/**
 * Merge HLC-ordered pre-fork snapshot messages with live post-fork messages.
 * Returns a unified timeline with a `ForkDivider` row inserted at the
 * boundary between the last pre-fork message and the first live message.
 * The divider is only inserted when both lists are non-empty.
 *
 * HLC ordering: wallMs primary → logical secondary → deviceId tertiary
 * (the shared `compareHlc` in hlc.ts).
 */
export function buildUnifiedTimeline(
  snapshotMessages: ChannelMessageDto[],
  liveMessages: ChannelMessageDto[],
  originalCommunityName: string,
  forkedAtMs: number,
  forkReason: string | null = null,
): TimelineRow[] {
  const pre: TimelineMessage[] = snapshotMessages.map((msg) => ({ msg, isPreFork: true }));
  const live: TimelineMessage[] = liveMessages.map((msg) => ({ msg, isPreFork: false }));

  // Merge the two sorted lists (both are HLC-ascending).
  const merged = mergeSortedByHlc(pre, live);

  if (pre.length === 0 || live.length === 0) {
    // No boundary to mark when one side is empty.
    return merged;
  }

  // Find the divider position: the first live row that comes AFTER the last
  // pre-fork row in the merged (HLC-sorted) stream.
  //
  // Naively matching "first live row after index 0" is incorrect when some
  // live messages sort earlier than some pre-fork messages by HLC — the
  // divider would land inside a live-only prefix rather than at the true
  // pre→live boundary.
  //
  // Correct approach:
  //   1. Find the index of the LAST pre-fork row.
  //   2. The divider goes immediately before the next live row after it.
  //   3. If no live row follows the last pre-fork row (all live messages sort
  //      earlier than all pre-fork messages), there is no clean post-fork
  //      section to separate — skip the divider in that case.
  let lastPreForkIdx = -1;
  for (let i = merged.length - 1; i >= 0; i--) {
    if (merged[i].isPreFork) {
      lastPreForkIdx = i;
      break;
    }
  }
  if (lastPreForkIdx === -1) {
    // All rows are live (pre.length > 0 check above guarantees pre is non-empty,
    // so this branch is unreachable in practice, but guard for safety).
    return merged;
  }
  // Find the first live row strictly after the last pre-fork row.
  let dividerIndex = -1;
  for (let i = lastPreForkIdx + 1; i < merged.length; i++) {
    if (!merged[i].isPreFork) {
      dividerIndex = i;
      break;
    }
  }
  if (dividerIndex === -1) {
    // No live row follows the last pre-fork row — all live messages sort
    // earlier than all pre-fork messages. No divider to insert.
    return merged;
  }

  const divider: ForkDivider = {
    kind: 'fork-divider',
    originalCommunityName,
    forkedAtMs,
    forkReason,
  };

  return [
    ...merged.slice(0, dividerIndex),
    divider,
    ...merged.slice(dividerIndex),
  ];
}

/** Standard 2-pointer merge of two HLC-ascending arrays of TimelineMessage. */
function mergeSortedByHlc(
  a: TimelineMessage[],
  b: TimelineMessage[],
): TimelineMessage[] {
  const result: TimelineMessage[] = [];
  let ai = 0;
  let bi = 0;
  while (ai < a.length && bi < b.length) {
    if (compareHlc(a[ai].msg.at, b[bi].msg.at) <= 0) {
      result.push(a[ai++]);
    } else {
      result.push(b[bi++]);
    }
  }
  while (ai < a.length) result.push(a[ai++]);
  while (bi < b.length) result.push(b[bi++]);
  return result;
}
