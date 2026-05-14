/**
 * ZEB-285 Phase 1 Task 11: pure helpers for merging pre-fork snapshot
 * messages with live channel messages into a unified HLC-ordered timeline.
 *
 * Extracted from ChannelMessageFeed.svelte so this logic can be unit-tested
 * without a DOM environment.
 */

import type { ChannelMessageDto, HlcDto } from './channel-message-service';

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
}

/** A single row in the rendered timeline — either a message or a divider. */
export type TimelineRow = TimelineMessage | ForkDivider;

/**
 * Merge HLC-ordered pre-fork snapshot messages with live post-fork messages.
 * Returns a unified timeline with a `ForkDivider` row inserted at the
 * boundary between the last pre-fork message and the first live message.
 * The divider is only inserted when both lists are non-empty.
 *
 * HLC ordering: wallMs primary → logical secondary → deviceId tertiary.
 * This mirrors the `compareHlc` convention in channel-message-service.ts.
 */
export function buildUnifiedTimeline(
  snapshotMessages: ChannelMessageDto[],
  liveMessages: ChannelMessageDto[],
  originalCommunityName: string,
  forkedAtMs: number,
): TimelineRow[] {
  const pre: TimelineMessage[] = snapshotMessages.map((msg) => ({ msg, isPreFork: true }));
  const live: TimelineMessage[] = liveMessages.map((msg) => ({ msg, isPreFork: false }));

  // Merge the two sorted lists (both are HLC-ascending).
  const merged = mergeSortedByHlc(pre, live);

  if (pre.length === 0 || live.length === 0) {
    // No boundary to mark when one side is empty.
    return merged;
  }

  // Find the transition index: the first live row after at least one pre-fork row.
  const dividerIndex = merged.findIndex((row, i) => i > 0 && !row.isPreFork);
  if (dividerIndex === -1) {
    return merged;
  }

  const divider: ForkDivider = {
    kind: 'fork-divider',
    originalCommunityName,
    forkedAtMs,
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

/** HLC comparison: wallMs → logical → deviceId. Returns negative/0/positive. */
function compareHlc(a: HlcDto, b: HlcDto): number {
  if (a.wallMs !== b.wallMs) return a.wallMs - b.wallMs;
  if (a.logical !== b.logical) return a.logical - b.logical;
  return a.deviceId < b.deviceId ? -1 : a.deviceId > b.deviceId ? 1 : 0;
}
