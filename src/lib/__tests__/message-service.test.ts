import { describe, it, expect, vi, beforeEach } from 'vitest';
import { MessageService } from '../message-service';
import type { TauriAdapter } from '../zenoh-service';

function makeAdapter(): TauriAdapter & { listeners: Map<string, Function> } {
  const listeners = new Map<string, Function>();
  return {
    listeners,
    invoke: vi.fn(),
    listen: vi.fn(async (event: string, handler: Function) => {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    }),
  } as any;
}

const SENDER = 'cd'.repeat(16); // 32-char sender hex

// ZEB-962: `wireToMessage` bakes `Message.sender.displayName`, read RAW by the
// thread/collapse render surfaces (ThreadIndicator, QuietMessageGroup). The
// original `senderName || slice(0,8)`
// only floors `""`; a whitespace-only broadcast name is truthy and gets baked,
// then rendered blank by every raw consumer. `nonEmpty` floors it at the write.
describe('MessageService wireToMessage sender name (ZEB-962)', () => {
  let service: MessageService;
  let adapter: ReturnType<typeof makeAdapter>;

  beforeEach(() => {
    service = new MessageService();
    adapter = makeAdapter();
  });

  function receive(senderName: string) {
    const handler = adapter.listeners.get('message-received')!;
    handler({
      payload: {
        id: 'm1',
        senderAddress: SENDER,
        senderName,
        text: 'hi',
        timestamp: 1,
        channel: 'chan',
      },
    });
    return service.messages.find((m) => m.id === 'm1');
  }

  it('keeps a non-blank broadcast sender name', async () => {
    await service.connectAdapter(adapter);
    expect(receive('Alice')?.sender.displayName).toBe('Alice');
  });

  it('floors a whitespace-only sender name to short hex, not blank', async () => {
    await service.connectAdapter(adapter);
    expect(receive('   ')?.sender.displayName).toBe(SENDER.slice(0, 8));
  });

  it('floors an empty sender name to short hex', async () => {
    await service.connectAdapter(adapter);
    expect(receive('')?.sender.displayName).toBe(SENDER.slice(0, 8));
  });
});
