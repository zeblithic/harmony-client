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
  for (const m of messages) {
    if (!m.replyTo) continue;
    let entry = meta.get(m.replyTo);
    if (!entry) {
      entry = { count: 0, participants: [] };
      meta.set(m.replyTo, entry);
    }
    entry.count++;
    if (!entry.participants.some(p => p.address === m.sender.address)) {
      entry.participants.push(m.sender);
    }
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
