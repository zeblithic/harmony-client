import type { TauriAdapter } from './zenoh-service';
import { assertDecodedDimsOk } from './avatar-normalize';

/** How long to wait before retrying a failed CID fetch (30s). */
const RETRY_COOLDOWN_MS = 30_000;

/**
 * Resolves avatar CIDs to displayable blob URLs via content transport.
 *
 * Fetches raw bytes from the Zenoh mesh using the `fetch_avatar` Tauri
 * command and creates object URLs for rendering in <img> tags. Results
 * are cached so each CID is fetched at most once.
 */
export class AvatarResolver {
  /** Called when a new CID has been resolved so the UI can re-render. */
  onChange?: () => void;

  private adapter: TauriAdapter | null = null;
  private cache = new Map<string, string>();
  private pending = new Set<string>();
  /** CID → timestamp of last failure. Retryable after RETRY_COOLDOWN_MS. */
  private failedAt = new Map<string, number>();
  private destroyed = false;

  connectAdapter(adapter: TauriAdapter): void {
    this.adapter = adapter;
  }

  /** Return the resolved blob URL for a CID, or undefined if not yet resolved.
   *  Automatically kicks off a fetch if the CID hasn't been seen before. */
  resolve(cid: string): string | undefined {
    const cached = this.cache.get(cid);
    if (cached) return cached;
    const failTime = this.failedAt.get(cid);
    const cooledOff = failTime !== undefined && Date.now() - failTime >= RETRY_COOLDOWN_MS;
    if (!this.pending.has(cid) && (failTime === undefined || cooledOff)) {
      this.fetchCid(cid);
    }
    return undefined;
  }

  private async fetchCid(cid: string): Promise<void> {
    if (!this.adapter) return;
    this.pending.add(cid);
    try {
      const bytes = (await this.adapter.invoke('fetch_avatar', { cid })) as number[];
      if (this.destroyed) return;
      const mime = detectImageMime(bytes);
      const blob = new Blob([new Uint8Array(bytes)], { type: mime });
      // ZEB-344: decoded-dimension guard on the RECEIVE path (parity with
      // normalizeAvatar's ingest guard) — reject a decode bomb before its blob
      // URL ever reaches an <img>. A throw here lands in the catch → identicon.
      const bmp = await createImageBitmap(blob);
      try {
        assertDecodedDimsOk(bmp.width, bmp.height);
      } finally {
        bmp.close();
      }
      const url = URL.createObjectURL(blob);
      this.cache.set(cid, url);
      this.onChange?.();
    } catch (err) {
      if (!this.destroyed) {
        console.warn(`Avatar fetch failed for CID ${cid}:`, err);
        this.failedAt.set(cid, Date.now());
      }
    } finally {
      this.pending.delete(cid);
    }
  }

  destroy(): void {
    this.destroyed = true;
    for (const url of this.cache.values()) {
      URL.revokeObjectURL(url);
    }
    this.cache.clear();
    this.pending.clear();
    this.failedAt.clear();
  }
}

/** Detect image MIME type from magic bytes, defaulting to image/png. */
function detectImageMime(bytes: number[]): string {
  if (bytes[0] === 0xFF && bytes[1] === 0xD8) return 'image/jpeg';
  if (bytes[0] === 0x89 && bytes[1] === 0x50) return 'image/png';
  if (bytes[0] === 0x47 && bytes[1] === 0x49) return 'image/gif';
  if (bytes[0] === 0x52 && bytes[1] === 0x49) return 'image/webp';
  return 'image/png';
}
