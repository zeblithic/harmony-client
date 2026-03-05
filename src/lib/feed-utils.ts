import type { Message } from './types';

export type FeedItem =
  | { kind: 'message'; message: Message }
  | { kind: 'quiet-group'; messages: Message[] };

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
