import type { TauriAdapter } from './zenoh-service';

/** How long to wait before retrying a failed CID fetch (30s). */
const RETRY_COOLDOWN_MS = 30_000;

/** Validated long-form profile document returned by `fetch_profile_doc`. */
export interface ProfilePageDto {
  bio: string;
  links: { label: string; url: string }[];
  fields: { key: string; value: string }[];
}

/**
 * Resolves profile-page CIDs to validated profile documents via content
 * transport.
 *
 * Twin of {@link AvatarResolver}, but DTO-valued and lazy: a fetch is only
 * kicked off when {@link resolve} is called for a CID (e.g. when a profile
 * panel opens), never eagerly per member. Fetches the document from the Zenoh
 * mesh using the `fetch_profile_doc` Tauri command. Results are cached so each
 * CID is fetched at most once.
 */
export class ProfilePageResolver {
  /** Called when a new CID has been resolved so the UI can re-render. */
  onChange?: () => void;

  private adapter: TauriAdapter | null = null;
  private cache = new Map<string, ProfilePageDto>();
  private pending = new Set<string>();
  /** CID → timestamp of last failure. Retryable after RETRY_COOLDOWN_MS. */
  private failedAt = new Map<string, number>();
  private destroyed = false;

  connectAdapter(adapter: TauriAdapter): void {
    this.adapter = adapter;
  }

  /** Return the resolved profile document for a CID, or undefined if not yet
   *  resolved. Automatically kicks off a fetch if the CID hasn't been seen
   *  before (or its failure cooldown has elapsed). */
  resolve(cid: string): ProfilePageDto | undefined {
    if (this.destroyed) return undefined;
    const cached = this.cache.get(cid);
    if (cached) return cached;
    const failTime = this.failedAt.get(cid);
    const cooledOff = failTime !== undefined && Date.now() - failTime >= RETRY_COOLDOWN_MS;
    if (!this.pending.has(cid) && (failTime === undefined || cooledOff)) {
      this.fetch(cid);
    }
    return undefined;
  }

  /**
   * Coarse resolution state for a CID, so the UI can distinguish "still
   * loading" from "fetch failed" — `resolve()` returns `undefined` for BOTH a
   * fetch in flight AND a failed fetch (during the retry cooldown), so a panel
   * keying only off `resolve()` would show "Loading…" forever after a failure.
   *
   *   - 'resolved' → the doc is cached and `resolve()` returns it.
   *   - 'loading'  → a fetch is in flight (or hasn't been attempted yet, in
   *                  which case `resolve()` will kick one off).
   *   - 'error'    → the last fetch failed and we're inside the retry cooldown.
   */
  status(cid: string): 'resolved' | 'loading' | 'error' {
    if (this.cache.has(cid)) return 'resolved';
    if (this.pending.has(cid)) return 'loading';
    if (this.failedAt.has(cid)) return 'error';
    return 'loading'; // not yet attempted — resolve() will kick a fetch
  }

  private async fetch(cid: string): Promise<void> {
    if (!this.adapter) return;
    this.pending.add(cid);
    try {
      const dto = (await this.adapter.invoke('fetch_profile_doc', { cid })) as ProfilePageDto;
      if (this.destroyed) return;
      this.cache.set(cid, dto);
      this.onChange?.();
    } catch (err) {
      if (!this.destroyed) {
        const msg = err instanceof Error ? err.message : String(err);
        console.warn(`Profile doc fetch failed for CID ${cid}: ${msg}`);
        this.failedAt.set(cid, Date.now());
      }
    } finally {
      this.pending.delete(cid);
    }
  }

  destroy(): void {
    this.destroyed = true;
    this.cache.clear();
    this.pending.clear();
    this.failedAt.clear();
  }
}
