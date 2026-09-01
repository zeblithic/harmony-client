import type { TauriAdapter } from './zenoh-service';
import type {
  ContentItem,
  ContentDetail,
  ContentOriginInfo,
  QuotaStatus,
  FileManagerSettings,
  ReplicationTier,
  ContentCategory,
  IngestOptions,
  FileGrant,
  ReceivedFile,
  ReceivedGrantWire,
} from './types';
import { mockPrivateContent } from './mock-file-data';
import { nonEmpty } from './display-label';
import { shortId } from './short-addr';

/** Wire format for content availability announcements from the Rust backend. */
export interface ContentAnnouncementEvent {
  cid: string;
  sizeBytes: number;
}

/** Count received files strictly newer than `lastSeenMs` (the unread badge).
 *  `null` (unresolved) → 0, so an in-flight/failed load never shows a badge. */
export function unreadReceivedCount(
  files: ReceivedFile[] | null,
  lastSeenMs: number,
): number {
  if (!files) return 0;
  return files.filter((f) => f.receivedAt > lastSeenMs).length;
}

/** Next value for the "last seen" badge watermark, or null = do not advance it.
 *  A failed/unresolved load (files === null) must NOT advance the watermark —
 *  else pre-existing grants get silently marked seen and never re-badge. The
 *  proven-empty ([]) case DOES advance (there is genuinely nothing unseen). */
export function nextSharedLastSeen(files: ReceivedFile[] | null, nowMs: number): number | null {
  if (files === null) return null;
  const newest = files.reduce((m, f) => Math.max(m, f.receivedAt), 0);
  return Math.max(newest, nowMs);
}

/** Wire format returned by the ingest_content Tauri command. */
interface IngestResult {
  sidecarId: string;
  cid: string;
  fileName: string;
  sizeBytes: number;
}

/** Wire format returned by the create_folder Tauri command. */
export interface CreateFolderResult {
  sidecarId: string;
  cid: string;
}

/** Wire format returned by the move_content Tauri command (ZEB-162). */
export interface MoveContentResult {
  /** New CID of the source top-level after the move. Null for Case C
   *  (source top-level was deleted). */
  srcNewCid: string | null;
  /** SidecarId of the (possibly newly minted) destination top-level entry.
   *  For Cases A/B/C this equals the dst_sidecar_id arg; for Case D this
   *  is a freshly minted sidecar_id. */
  dstSidecarId: string;
  /** New CID of the destination top-level after the move. For Case D
   *  this equals srcChildCid. */
  dstNewCid: string;
}

/** Wire format returned by the rename_content Tauri command (ZEB-299). */
export interface RenameContentResult {
  /** New top-level CID after the ancestor walk + rekey. Null for the
   *  top-level case — renaming a sidecar row's file_name doesn't change
   *  the top-level CID (the name lives in the sidecar, not the manifest). */
  srcNewCid: string | null;
}

/** Skip buckets surfaced in the folder-ingest summary modal. Mirrors the
 *  Rust `SkipCounts` struct (serde rename_all = "camelCase"). */
export interface SkipCounts {
  hidden: number;
  symlink: number;
  /** FIFOs, sockets, block/char devices — non-addressable filesystem nodes
   *  the walker can't ingest. Bucketed separately from the named cases so
   *  the summary modal can render them without conflating "we don't follow
   *  symlinks" with "we can't ingest a device node". */
  other: number;
}

/** One entry in the bounded `failed` list of an ingest result. */
export interface FailedEntry {
  path: string;
  message: string;
}

/** Wire format returned by the ingest_folder_tree Tauri command (ZEB-163).
 *  Mirrors the Rust `IngestFolderTreeResult` struct (serde rename_all =
 *  "camelCase"). `rootSidecarId` and `rootCid` are non-null iff the walker
 *  reached the root manifest build before any cancel/abort. */
export interface IngestFolderTreeResult {
  jobId: string;
  rootSidecarId: string | null;
  rootCid: string | null;
  rootName: string;
  totalFilesSeen: number;
  /** Pre-walk leaf count taken before the walker started. The cancelled
   *  headline uses this as the denominator ("Cancelled — added 4 of 100
   *  files") so a mid-walk cancel doesn't claim a truncated total. `-1`
   *  when the pre-walk failed; the modal falls back to `totalFilesSeen`. */
  preWalkTotal: number;
  succeeded: number;
  skipped: SkipCounts;
  failed: FailedEntry[];
  failedOverflow: number;
  cancelled: boolean;
}

/** Wire format for entries returned by the list_content Tauri command. */
interface ContentItemWire {
  sidecarId: string;
  cid: string;
  name: string;
  sizeBytes: number;
  storedAt: number;
  sensitivity: 'private' | 'confidential' | 'public';
  replicationTier: ReplicationTier;
  pinned: boolean;
  licensed: boolean;
  archived: boolean;
  /** Source-of-truth node type from the backend. */
  kind: 'leaf' | 'folder';
  /** ZEB-612 S3: observed replica count — 1 (self) + distinct peer
   *  sessions seen announcing this CID. A lower bound ("copies seen"). */
  replicaCount: number;
  /** ZEB-669 S3: "back up with buddies" flag (root sidecar rows only).
   *  Optional: pre-ZEB-669 backends omit it (mapped to false). */
  backup?: boolean;
  /** ZEB-669 S4: provenance recorded at creation; null for legacy and
   *  manifest-derived rows. Optional: pre-ZEB-669 backends omit it. */
  origin?: ContentOriginInfo | null;
  /** ZEB-674 T8: whether this CID's content class is encrypted — derived
   *  backend-side from the CID header flag bit. Always present from a
   *  ZEB-674+ backend; treated as `false` when omitted (pre-ZEB-674). */
  encrypted?: boolean;
}

/** Wire shape of the `get_storage_budget` query (ZEB-612 S3). */
interface StorageBudgetWire {
  cacheCapacity: number;
  /** The PINNED content budget the runtime enforces — not an overall quota. */
  maxPinnedBytes: number;
}

const MUSIC_EXTS = ['mp3', 'flac', 'wav', 'ogg', 'aac', 'm4a', 'opus', 'wma'];
const VIDEO_EXTS = ['mp4', 'mkv', 'avi', 'mov', 'webm', 'flv', 'wmv'];
const IMAGE_EXTS = ['jpg', 'jpeg', 'png', 'gif', 'svg', 'webp', 'bmp', 'ico', 'tiff'];
const SOFTWARE_EXTS = ['exe', 'app', 'dmg', 'deb', 'rpm', 'msi', 'wasm', 'tar', 'gz', 'zip', 'rar', '7z'];
const DATASET_EXTS = ['csv', 'parquet', 'arrow', 'jsonl', 'ndjson', 'tsv', 'sqlite', 'db'];

/** Infer a content category from a file name's extension. */
export function inferCategory(fileName: string): ContentCategory {
  const ext = fileName.split('.').pop()?.toLowerCase() ?? '';
  if (MUSIC_EXTS.includes(ext)) return 'music';
  if (VIDEO_EXTS.includes(ext)) return 'video';
  if (IMAGE_EXTS.includes(ext)) return 'image';
  if (SOFTWARE_EXTS.includes(ext)) return 'software';
  if (DATASET_EXTS.includes(ext)) return 'dataset';
  return 'text';
}

function wireToContentItem(wire: ContentItemWire): ContentItem {
  // `kind` is the source of truth on the wire; `isFolder` is a derived
  // client-side convenience kept for back-compat with existing
  // filter/sort predicates in FileBrowser and FileList.
  return {
    sidecarId: wire.sidecarId,
    cid: wire.cid,
    name: wire.name,
    category: inferCategory(wire.name),
    sensitivity: wire.sensitivity,
    sizeBytes: wire.sizeBytes,
    storedAt: wire.storedAt,
    replicationTier: wire.replicationTier,
    replicaCount: wire.replicaCount,
    pinned: wire.pinned,
    licensed: wire.licensed,
    archived: wire.archived,
    parentCid: null,
    isFolder: wire.kind === 'folder',
    backup: wire.backup ?? false,
    origin: wire.origin ?? null,
    encrypted: wire.encrypted ?? false,
  };
}

export class FileManagerService {
  readonly settings: FileManagerSettings;
  /** Called whenever content state changes so the UI can re-render. */
  onChange?: () => void;
  /** CIDs announced on the mesh (real network data). */
  announcedCids = new Map<string, { sizeBytes: number; firstSeen: number }>();
  /** ZEB-612 S3: real pinned budget from get_storage_budget; null until
   *  connected (demo mode) or when the query fails. */
  private pinnedBudgetBytes: number | null = null;

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];
  private privateContent: ContentItem[];

  constructor(overrides?: Partial<FileManagerSettings>) {
    this.settings = {
      defaultReplicationTier: 'default',
      defaultViewMode: 'list',
      ...overrides,
    };

    // Each instance gets its own deep copy so mutations are isolated
    this.privateContent = structuredClone(mockPrivateContent);
  }

  /** Connect a Tauri adapter and start listening for content announcements. */
  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return; // already wired; prevent duplicate listeners

    // Do the full bootstrap (list + listener registration) before committing
    // `this.adapter`. If either step fails the service stays disconnected so
    // a later retry isn't blocked by the guard above.
    let raw: ContentItemWire[] | null | undefined;
    let unlisten: () => void;
    try {
      raw = (await adapter.invoke('list_content', { folderCid: null })) as ContentItemWire[] | null | undefined;
      unlisten = await adapter.listen(
        'content-announced',
        (event) => {
          const wire = event.payload as ContentAnnouncementEvent;
          if (this.announcedCids.has(wire.cid)) return;
          this.announcedCids = new Map([
            ...this.announcedCids,
            [wire.cid, { sizeBytes: wire.sizeBytes, firstSeen: Date.now() }],
          ]);
          this.onChange?.();
        },
      );
    } catch (err) {
      // Leave this.adapter null so the caller (or a retry path) can try again.
      throw err;
    }

    this.adapter = adapter;
    // Per ZEB-146, the file-manager UI must never mix mocks with real state
    // once the backend adapter is connected. Archived items stay in the
    // sidecar but are hidden from the active list (spec: archive = UI-hide).
    this.privateContent = Array.isArray(raw)
      ? raw.filter((w) => !w.archived).map(wireToContentItem)
      : [];
    this.unlisteners.push(unlisten);
    // ZEB-612 S3: fetch the real pinned budget. Non-fatal — a failure
    // (e.g. runtime not booted) degrades to a used-only quota display.
    try {
      const budget = (await adapter.invoke('get_storage_budget')) as StorageBudgetWire;
      this.pinnedBudgetBytes = budget.maxPinnedBytes;
    } catch {
      this.pinnedBudgetBytes = null;
    }
    this.onChange?.();
  }

  /** Returns private content. With no args returns a copy of all; with parentCid filters by parent. */
  getContents(parentCid?: string | null): ContentItem[] {
    if (parentCid === undefined) {
      return [...this.privateContent];
    }
    return this.privateContent.filter((item) => item.parentCid === parentCid);
  }

  /** Returns detail for a single content item, or undefined if not found.
   *  ZEB-612 S3: only real fields — the mock sharedWith/storageBuddies/
   *  origin surfaces return with real hosting accounting (ZEB-669).
   *
   *  ZEB-164 allows multiple sidecar entries to share a CID, so a CID-only
   *  lookup can land on the wrong sibling (stale backup/pin state after a
   *  toggle — Greptile PR #450). Pass `sidecarId` when the caller knows the
   *  exact row; the CID lookup remains the fallback for manifest-derived
   *  rows (empty sidecarId) and legacy callers. */
  getContentDetail(cid: string, sidecarId?: string): ContentDetail | undefined {
    const item =
      (sidecarId ? this.privateContent.find((i) => i.sidecarId === sidecarId) : undefined) ??
      this.privateContent.find((i) => i.cid === cid);
    if (!item) return undefined;
    return { ...item };
  }

  /**
   * ZEB-669 S3: re-fetch the root listing. Storage-buddy events can carry
   * backup-flag changes made outside this window (headless RPC, another
   * device surface) — without a refetch the file list and detail panel go
   * stale until an unrelated reload (Greptile PR #450). No-op in demo mode.
   */
  async refreshContents(): Promise<void> {
    await this.refetchRoot();
  }

  /** Computes quota status from current private content. */
  getQuotaStatus(): QuotaStatus {
    const byCategory: Partial<Record<ContentCategory, number>> = {};
    let usedBytes = 0;
    // ZEB-164: multiple sidecar entries can share a CID. Storage is
    // content-addressed, so each unique CID contributes its bytes once.
    // We pick the first-seen entry's category for byCategory tie-breaking
    // (HashMap iteration is non-deterministic on the wire, but
    // privateContent is locally stable for the duration of a session).
    const seenCids = new Set<string>();
    // A CID is pinned if ANY of its sidecar entries pins it — mirror of the
    // backend's is_cid_pinned_by_any OR-join (ZEB-164 symlink semantics).
    const pinnedCids = new Set(
      this.privateContent.filter((i) => i.pinned).map((i) => i.cid),
    );
    let pinnedUsedBytes = 0;
    for (const item of this.privateContent) {
      if (seenCids.has(item.cid)) continue;
      seenCids.add(item.cid);
      usedBytes += item.sizeBytes;
      byCategory[item.category] = (byCategory[item.category] ?? 0) + item.sizeBytes;
      if (pinnedCids.has(item.cid)) pinnedUsedBytes += item.sizeBytes;
    }

    return {
      usedBytes,
      byCategory,
      pinnedUsedBytes,
      pinnedBudgetBytes: this.pinnedBudgetBytes,
    };
  }

  /**
   * Permanently removes content items. With ZEB-164's symlink-style sidecar,
   * burn is "remove this entry from my list" — quota only frees on the
   * last-reference burn (when no sibling sidecar entry references the CID).
   */
  async burn(sidecarIds: string[]): Promise<void> {
    if (!this.adapter) {
      // Offline-only path: still mutate local state so tests/Storybook work.
      const idSet = new Set(sidecarIds);
      this.privateContent = this.privateContent.filter((i) => !idSet.has(i.sidecarId));
      this.onChange?.();
      return;
    }
    const results = await Promise.allSettled(
      sidecarIds.map((sidecarId) => this.adapter!.invoke('burn_content', { sidecarId })),
    );
    const succeeded = new Set(
      sidecarIds.filter((_, i) => {
        const r = results[i];
        return r.status === 'fulfilled' && r.value === true;
      }),
    );
    this.privateContent = this.privateContent.filter((i) => !succeeded.has(i.sidecarId));
    this.onChange?.();
  }

  /** Move content to cold storage (archive tier). Items are removed from
   *  the active file list and the backend is notified to migrate the data. */
  async archive(sidecarIds: string[]): Promise<void> {
    if (!this.adapter) {
      const idSet = new Set(sidecarIds);
      this.privateContent = this.privateContent.filter((i) => !idSet.has(i.sidecarId));
      this.onChange?.();
      return;
    }
    const results = await Promise.allSettled(
      sidecarIds.map((sidecarId) => this.adapter!.invoke('archive_content', { sidecarId })),
    );
    const succeeded = new Set(
      sidecarIds.filter((_, i) => {
        const r = results[i];
        return r.status === 'fulfilled' && r.value === true;
      }),
    );
    this.privateContent = this.privateContent.filter((i) => !succeeded.has(i.sidecarId));
    this.onChange?.();
  }

  /** Sets the pinned flag on a content item. */
  async pin(sidecarId: string): Promise<void> {
    if (!this.adapter) {
      // Offline-only path: mutate local state for Storybook/test contexts.
      const item = this.privateContent.find((i) => i.sidecarId === sidecarId);
      if (item) item.pinned = true;
      this.onChange?.();
      return;
    }
    const ok = (await this.adapter.invoke('pin_content', { sidecarId })) as boolean;
    if (ok === false) {
      throw new Error('pin quota exhausted');
    }
    const item = this.privateContent.find((i) => i.sidecarId === sidecarId);
    if (item) item.pinned = true;
    this.onChange?.();
  }

  /**
   * ZEB-669 S3: sets the "back up with buddies" flag. Backend is the
   * eligibility authority — non-public-durable CIDs reject with a stable
   * `ineligible:` prefix (clearing is always allowed). Rejections propagate
   * to the caller so the detail panel can render the reason inline.
   */
  async setBackupFlag(sidecarId: string, backup: boolean): Promise<void> {
    if (!this.adapter) {
      // Offline-only path: mutate local state for Storybook/test contexts.
      const item = this.privateContent.find((i) => i.sidecarId === sidecarId);
      if (item) item.backup = backup;
      this.onChange?.();
      return;
    }
    try {
      await this.adapter.invoke('set_backup_flag', { sidecarId, backup });
    } catch (e) {
      // Normalize both production (string) + test (Error) rejection shapes
      // (CLAUDE.md "Tauri IPC error extraction").
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(msg);
    }
    const item = this.privateContent.find((i) => i.sidecarId === sidecarId);
    if (item) item.backup = backup;
    this.onChange?.();
  }

  /**
   * ZEB-674: lists grantees a CID has been shared with. Backend-only —
   * returns [] without a connected adapter (Storybook/test contexts have
   * no encrypted-share concept).
   */
  async listGrants(cid: string): Promise<FileGrant[]> {
    if (!this.adapter) return [];
    return (await this.adapter.invoke('list_grants', { cid })) as FileGrant[];
  }

  /**
   * ZEB-674: grants a peer read access to an encrypted CID. Backend is the
   * eligibility authority — ineligible shares (unencrypted content,
   * non-friend grantee, unreachable devices) reject with a stable
   * `ineligible:` prefix. Rejections propagate to the caller so the share
   * dialog can render the reason inline.
   */
  async grantRead(cid: string, granteeAddress: string): Promise<void> {
    if (!this.adapter) throw new Error('adapter not connected');
    try {
      await this.adapter.invoke('grant_read', { cid, granteeAddress });
    } catch (e) {
      // Normalize both production (string) + test (Error) rejection shapes
      // (CLAUDE.md "Tauri IPC error extraction").
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(msg);
    }
  }

  /** ZEB-674: revokes a previously granted peer's read access to a CID. */
  async revokeRead(cid: string, granteeAddress: string): Promise<void> {
    if (!this.adapter) throw new Error('adapter not connected');
    try {
      await this.adapter.invoke('revoke_read', { cid, granteeAddress });
    } catch (e) {
      // Normalize both production (string) + test (Error) rejection shapes
      // (CLAUDE.md "Tauri IPC error extraction").
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(msg);
    }
  }

  /** ZEB-723: lists files others have shared with this user. Backend-only —
   *  returns [] without a connected adapter (demo/test). A real IPC rejection
   *  PROPAGATES (never swallowed to []) so the caller keeps the honest
   *  unresolved (null) state on failure. */
  async listReceivedGrants(): Promise<ReceivedFile[]> {
    if (!this.adapter) return [];
    const rows = (await this.adapter.invoke('list_received_grants')) as ReceivedGrantWire[];
    return rows.map((r) => ({
      cid: r.cid,
      granterAddress: r.granterAddress,
      // ZEB-785: a present, non-blank name wins; otherwise fall back to a
      // truncated owner hex (the FriendsPanel/DelegationWidget convention for
      // people), never the full 32-char address mid-sentence.
      granterDisplay: nonEmpty(r.displayName) ?? shortId(r.granterAddress),
      fileName: r.fileName,
      fileSize: r.fileSize,
      mime: r.mime,
      receivedAt: r.receivedAt,
    }));
  }

  /** ZEB-723: download a shared file to disk. `export_content` fetches the
   *  ciphertext from the network and decrypts via `received_file_grants`
   *  (ZEB-674 T12) — the grantee read path is already complete. */
  async exportReceived(cid: string, fileName: string): Promise<void> {
    if (!this.adapter) return;
    await this.adapter.invoke('export_content', { cid, fileName });
  }

  /** Clears the pinned flag on a content item. */
  async unpin(sidecarId: string): Promise<void> {
    if (!this.adapter) {
      const item = this.privateContent.find((i) => i.sidecarId === sidecarId);
      if (item) item.pinned = false;
      this.onChange?.();
      return;
    }
    await this.adapter.invoke('unpin_content', { sidecarId });
    const item = this.privateContent.find((i) => i.sidecarId === sidecarId);
    if (item) item.pinned = false;
    this.onChange?.();
  }

  /** Updates the replication tier for specified items. */
  async setReplicationTier(sidecarIds: string[], tier: ReplicationTier): Promise<void> {
    if (this.adapter) {
      await this.adapter.invoke('set_replication_tier', { sidecarIds, tier });
    }
    const idSet = new Set(sidecarIds);
    for (const item of this.privateContent) {
      if (idSet.has(item.sidecarId)) {
        item.replicationTier = tier;
      }
    }
    this.onChange?.();
  }

  /** Export content to the local filesystem via a native save dialog.
   *  Each CID triggers a separate save dialog on the Rust backend. */
  async exportToDevice(cids: string[]): Promise<void> {
    if (!this.adapter) return;
    for (const cid of cids) {
      const item = this.privateContent.find((i) => i.cid === cid);
      const fileName = item?.name ?? cid;
      await this.adapter.invoke('export_content', { cid, fileName });
    }
  }

  /** Open a file picker, ingest the selected file into the content store,
   *  and add it to the private content list.
   *
   *  ZEB-674: pass `{ encrypted: true }` to route to `ingest_content_encrypted`,
   *  which produces a per-file-DEK-encrypted CID (later shareable via
   *  grantRead). The default (unset/false) keeps the existing unencrypted
   *  `ingest_content` path unchanged.
   *
   *  Returns the new ContentItem, or undefined if the user cancels or no adapter. */
  async ingest(parentCid?: string | null, options?: IngestOptions): Promise<ContentItem | undefined> {
    if (!this.adapter) return undefined;
    const command = options?.encrypted ? 'ingest_content_encrypted' : 'ingest_content';
    const result = (await this.adapter.invoke(command)) as IngestResult;
    // ZEB-164: CID-based dedupe removed — the backend mints a fresh sidecar_id
    // on every ingest, even for duplicate-content uploads. Multiple sidecar
    // entries per CID are expected and intentional (symlink-style semantics).
    // Defense-in-depth against the practically-impossible UUID v4 collision:
    if (this.privateContent.some((i) => i.sidecarId === result.sidecarId)) return undefined;
    const item: ContentItem = {
      sidecarId: result.sidecarId,
      cid: result.cid,
      name: result.fileName,
      category: inferCategory(result.fileName),
      sensitivity: 'private',
      sizeBytes: result.sizeBytes,
      storedAt: Date.now(),
      replicationTier: this.settings.defaultReplicationTier,
      // Fresh ingest: this node is the only holder until peers announce.
      replicaCount: 1,
      pinned: false,
      licensed: false,
      archived: false,
      parentCid: parentCid ?? null,
      isFolder: false,
      // ZEB-674 T8: the optimistic local item must reflect which ingest
      // command actually ran — a refetch confirms this from the CID's real
      // header flag, but the pre-refetch row shouldn't lie in the meantime.
      encrypted: options?.encrypted === true,
    };
    this.privateContent.push(item);
    return item;
  }

  /**
   * Load the contents of a specific folder via the backend's
   * list_content(folder_cid) path. Returns the live contents WITHOUT
   * caching — callers are responsible for binding to state.
   */
  async listFolderContents(folderCid: string): Promise<ContentItem[]> {
    if (!this.adapter) return [];
    // Let errors propagate. The backend distinguishes transient states
    // (bundle evicted from cache) from permanent corruption (manifest/bundle
    // mismatch, malformed manifest). Swallowing both as `[]` hides the
    // latter — a corrupted folder looks indistinguishable from an empty
    // one with only a console log. Callers decide how to surface the
    // error to users.
    const raw = (await this.adapter.invoke('list_content', { folderCid })) as
      | ContentItemWire[]
      | null
      | undefined;
    return Array.isArray(raw)
      ? raw.filter((w) => !w.archived).map(wireToContentItem)
      : [];
  }

  /**
   * Create a new folder via the backend.
   *
   * @param name              folder display name
   * @param parentSidecarId   the top-level sidecar entry's id (root entry
   *                          owning the cascade), or null for root creation
   * @param parentPath        CID chain from top-level root (inclusive) down
   *                          to the immediate parent; empty for root creation
   *
   * Returns `{ sidecarId, cid }`. For nested creation, `sidecarId` is the
   * unchanged top-level entry's id; `cid` is the new top-level root CID
   * after the ancestor cascade.
   *
   * Refetches the root listing and emits onChange. Callers navigating
   * inside a folder at the time of creation should also refetch the
   * folder contents.
   */
  async createFolder(
    name: string,
    parentSidecarId: string | null,
    parentPath: string[],
  ): Promise<CreateFolderResult> {
    if (!this.adapter) throw new Error('adapter not connected');
    const result = (await this.adapter.invoke('create_folder', {
      name,
      parentSidecarId,
      parentPath,
    })) as CreateFolderResult;
    try {
      await this.refetchRoot();
    } catch (err) {
      console.warn(
        'createFolder: refetchRoot failed (folder was created); UI may show stale list:',
        err,
      );
    }
    return result;
  }

  /**
   * Move a file or sub-folder between File Manager locations (ZEB-162).
   *
   * The four cases are inferred by the backend from the arg shape:
   * - Case A (same top-level): `dstSidecarId === srcSidecarId` and
   *   `dstPath[0] === srcPath[0]`.
   * - Case B (across top-levels): `dstSidecarId` is set and ≠ `srcSidecarId`.
   * - Case C (root → nested): `srcPath.length === 1 && srcPath[0] === srcChildCid`,
   *   `dstSidecarId` set.
   * - Case D (nested → root): `dstSidecarId === null && dstPath === []`.
   *
   * @param args - move operands. `newName` must remain `null` in this
   *   slice (rename is ZEB-299).
   *
   * Returns the rekeyed CIDs and the destination sidecar id. On any
   * backend error throws with the error message — callers surface it
   * inline (no `alert`).
   */
  async moveContent(args: {
    srcSidecarId: string;
    srcPath: string[];
    srcChildCid: string;
    /**
     * Manifest entry name of the dragged row (or sidecar `file_name`
     * when the source IS a top-level entry). Required because sibling
     * entries can share a CID — name disambiguates which sibling to
     * remove. Mismatch with the on-disk manifest aborts the move.
     */
    srcChildName: string;
    dstSidecarId: string | null;
    dstPath: string[];
    newName?: null;
  }): Promise<MoveContentResult> {
    if (!this.adapter) throw new Error('adapter not connected');
    const result = (await this.adapter.invoke('move_content', {
      srcSidecarId: args.srcSidecarId,
      srcPath: args.srcPath,
      srcChildCid: args.srcChildCid,
      srcChildName: args.srcChildName,
      dstSidecarId: args.dstSidecarId,
      dstPath: args.dstPath,
      newName: null,
    })) as MoveContentResult;
    // The move may have removed (Case C) or rekeyed (Cases A/B/D) one or
    // two top-level sidecar entries, plus mutated nested manifests inside
    // the affected chains. Same refresh shape as createFolder: re-list the
    // root so the top-level rows reflect the new CIDs and any
    // addition/removal of root entries. Nested-folder views are kept in
    // sync by FileBrowser's serviceVersion-tracking effect when refetchRoot
    // fires onChange.
    try {
      await this.refetchRoot();
    } catch (err) {
      console.warn(
        'moveContent: refetchRoot failed (move succeeded); UI may show stale list:',
        err,
      );
    }
    return result;
  }

  /**
   * Rename a file or folder in place (ZEB-299). Two cases dispatch on
   * the backend by input shape:
   * - Top-level (srcPath.length === 1 && srcPath[0] === srcChildCid):
   *   single sidecar `file_name` write, returns `{ srcNewCid: null }`.
   * - Nested: walks ancestor chain, CAS-rekeys the top-level sidecar,
   *   returns `{ srcNewCid: <new top-level cid> }`.
   *
   * Same refresh shape as moveContent — re-list the root so top-level
   * CIDs reflect the rekey, and onChange propagates so any in-folder
   * view re-fetches via the serviceVersion-tracking effect.
   */
  async renameContent(args: {
    srcSidecarId: string;
    srcPath: string[];
    srcChildCid: string;
    /** Current name on the manifest entry / sidecar — disambiguator for
     *  shared-CID siblings, same role as in moveContent. Mismatch with
     *  the on-disk manifest aborts the rename. */
    srcChildName: string;
    newName: string;
  }): Promise<RenameContentResult> {
    if (!this.adapter) throw new Error('adapter not connected');
    const result = (await this.adapter.invoke('rename_content', {
      srcSidecarId: args.srcSidecarId,
      srcPath: args.srcPath,
      srcChildCid: args.srcChildCid,
      srcChildName: args.srcChildName,
      newName: args.newName,
    })) as RenameContentResult;
    try {
      await this.refetchRoot();
    } catch (err) {
      console.warn(
        'renameContent: refetchRoot failed (rename succeeded); UI may show stale list:',
        err,
      );
    }
    return result;
  }

  /**
   * Ingest a folder tree from the local filesystem (ZEB-163). Resolves
   * when the walker settles (success, partial, or cancel). The IPC also
   * emits `folder-ingest-progress` events for the modal to consume.
   *
   * @param rootPath          absolute filesystem path of the dropped /
   *                          picked directory
   * @param parentSidecarId   top-level sidecar entry id when ingesting
   *                          into a nested folder; null at root
   * @param parentPath        CID chain from top-level root (inclusive)
   *                          down to the immediate parent; empty for
   *                          root ingest. Matches `createFolder`'s shape.
   */
  async ingestFolderTree(
    jobId: string,
    rootPath: string,
    parentSidecarId: string | null,
    parentPath: string[],
  ): Promise<IngestFolderTreeResult> {
    if (!this.adapter) throw new Error('adapter not connected');
    const result = (await this.adapter.invoke('ingest_folder_tree', {
      jobId,
      rootPath,
      parentSidecarId,
      parentPath,
    })) as IngestFolderTreeResult;
    // Mirror createFolder/moveContent/renameContent: re-list the root so
    // other consumers of `privateContent` (not just FileBrowser via its
    // serviceVersion++ on resolve) see the new sidecar entry. Refetch is
    // best-effort — the ingest itself succeeded if we got here, so we
    // only log on refresh failure rather than turning it into an error.
    try {
      await this.refetchRoot();
    } catch (err) {
      console.warn(
        'ingestFolderTree: refetchRoot failed (ingest succeeded); UI may show stale list:',
        err,
      );
    }
    return result;
  }

  /** Flip the cancel flag on an in-flight `ingest_folder_tree` job
   *  (ZEB-163). Best-effort — backend treats unknown job ids as a
   *  no-op rather than an error. */
  async cancelFolderIngest(jobId: string): Promise<void> {
    if (!this.adapter) return;
    await this.adapter.invoke('cancel_folder_ingest', { jobId });
  }

  /** Register an external unlisten handle so it gets cleaned up alongside the service. */
  addUnlisten(fn: () => void): void {
    this.unlisteners.push(fn);
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
  }

  // ── Private helpers ─────────────────────────────────────────────────

  private async refetchRoot(): Promise<void> {
    if (!this.adapter) return;
    const raw = (await this.adapter.invoke('list_content', { folderCid: null })) as
      | ContentItemWire[]
      | null
      | undefined;
    this.privateContent = Array.isArray(raw)
      ? raw.filter((w) => !w.archived).map(wireToContentItem)
      : [];
    this.onChange?.();
  }
}
