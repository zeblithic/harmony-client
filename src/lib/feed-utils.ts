import type { Message, Peer } from './types';

export type FeedItem =
  | { kind: 'message'; message: Message }
  | { kind: 'quiet-group'; messages: Message[] };

export function getThreadReplies(messages: Message[], rootId: string): Message[] {
  return messages.filter(m => m.replyTo === rootId);
}

export function getThreadRoots(messages: Message[]): Set<string> {
  const roots = new Set<string>();
  for (const m of messages) {
    if (m.replyTo) roots.add(m.replyTo);
  }
  return roots;
}

export interface ThreadMetaEntry {
  count: number;
  participants: Peer[];
}

export function getThreadMeta(messages: Message[]): Map<string, ThreadMetaEntry> {
  const meta = new Map<string, ThreadMetaEntry>();
  const roots = getThreadRoots(messages);
  for (const rootId of roots) {
    const replies = getThreadReplies(messages, rootId);
    const seen = new Set<string>();
    const participants: Peer[] = [];
    for (const r of replies) {
      if (!seen.has(r.sender.address)) {
        seen.add(r.sender.address);
        participants.push(r.sender);
      }
    }
    meta.set(rootId, { count: replies.length, participants });
  }
  return meta;
}

export function groupMessages(messages: Message[]): FeedItem[] {
  const items: FeedItem[] = [];
  let quietBuffer: Message[] = [];

  function flushQuiet() {
    if (quietBuffer.length > 0) {
      items.push({ kind: 'quiet-group', messages: quietBuffer });
      quietBuffer = [];
    }
  }

  for (const msg of messages) {
    if (msg.priority === 'quiet') {
      quietBuffer.push(msg);
    } else {
      flushQuiet();
      items.push({ kind: 'message', message: msg });
    }
  }

  flushQuiet();
  return items;
}
