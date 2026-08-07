/**
 * ZEB-214 — receives `dm-read-receipt` events (a peer told us they've read our
 * DM up to a watermark) and exposes a per-space watermark + seen-time. The
 * watermark (`readUpTo`, ms) gates which of our sent messages show "Seen";
 * `at` (ms) is the receipt send-time shown as "Seen HH:MM". Both advance
 * monotonically — a stale/duplicate receipt never regresses the display.
 */
interface DmReadReceiptPayload {
  spaceId: string;
  from: string;
  readUpTo: number;
  at: number;
}

interface ListenAdapter {
  listen(name: string, cb: (e: { payload: unknown }) => void): Promise<() => void>;
}

export class ReadReceiptService {
  private watermarks = new Map<string, number>();
  private seenAt = new Map<string, number>();
  private unlisten?: () => void;
  onChange?: () => void;

  async init(adapter: ListenAdapter): Promise<void> {
    this.unlisten = await adapter.listen('dm-read-receipt', (event) => {
      const p = event.payload as DmReadReceiptPayload;
      if (!p || typeof p.spaceId !== 'string' || typeof p.readUpTo !== 'number') return;
      const prev = this.watermarks.get(p.spaceId) ?? -1;
      if (p.readUpTo <= prev) return; // monotonic: ignore stale/duplicate
      this.watermarks.set(p.spaceId, p.readUpTo);
      if (typeof p.at === 'number') this.seenAt.set(p.spaceId, p.at);
      this.onChange?.();
    });
  }

  /** The peer's read-watermark (ms) for a space — gates which own messages show "Seen". */
  getWatermark(spaceId: string): number | undefined {
    return this.watermarks.get(spaceId);
  }

  /** The receipt's send-time (ms) for a space — the "Seen HH:MM" clock. */
  getSeenAt(spaceId: string): number | undefined {
    return this.seenAt.get(spaceId);
  }

  destroy(): void {
    this.unlisten?.();
    this.unlisten = undefined;
  }
}
