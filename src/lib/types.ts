export interface Peer {
  address: string;
  displayName: string;
  avatarUrl?: string;
}

export interface SoundOverrides {
  quiet?: string;
  standard?: string;
  loud?: string;
}

export interface Profile extends Peer {
  statusText?: string;
  /** CID for full-size avatar — resolved via content transport (future) */
  avatarCid?: string;
  /** CID for thumbnail avatar — resolved via content transport (future) */
  avatarMiniCid?: string;
  /** ZEB-345: CID (hex) of the long-form profile-page doc, if one was ingested.
   *  Staged by the editor on save and resolved lazily via the
   *  ProfilePageResolver when the owner's panel opens. */
  profilePageRoot?: string;
  notificationSounds?: SoundOverrides;
}

export interface MediaAttachment {
  id: string;
  type: 'image' | 'link' | 'code';
  /** URL for images and links */
  url?: string;
  /** OG title or filename */
  title?: string;
  /** Extracted domain for link indicators (e.g. "github.com") */
  domain?: string;
  /** Source code for code blocks */
  content?: string;
}

export type MessagePriority = 'quiet' | 'standard' | 'loud';

export type NotificationAction = 'silent' | 'dot_only' | 'notify' | 'sound' | 'break_dnd';

export interface NotificationPolicy {
  quiet: NotificationAction;
  standard: NotificationAction;
  loud: NotificationAction;
}

export interface NotificationSettings {
  global: NotificationPolicy;
  perCommunity: Map<string, Partial<NotificationPolicy>>;
  perPeer: Map<string, Partial<NotificationPolicy>>;
  perPeerSounds: Map<string, SoundOverrides>;
  perCommunitySounds: Map<string, SoundOverrides>;
}

export interface Message {
  id: string;
  sender: Peer;
  text: string;
  /** Unix timestamp in milliseconds */
  timestamp: number;
  /** Empty array for text-only messages */
  media: MediaAttachment[];
  /** Message priority level, defaults to 'standard' */
  priority: MessagePriority;
  /** ID of the thread root message this is a reply to */
  replyTo?: string;
  /** Channel this message belongs to (e.g. "general"). */
  channel?: string;
  /** Community/hub this message belongs to (e.g. "harmony-dev"). */
  hub?: string;
  /**
   * Phase 4 (ZEB-228) — DM lifecycle delivery state. Undefined for
   * non-self / received messages. Self-sent messages start in 'sending',
   * transition to 'delivered' on dm-delivered IPC, 'expired' on
   * dm-expired, 'failed' on send_dm error.
   */
  deliveryState?: 'sending' | 'delivered' | 'expired' | 'failed';
  /**
   * Phase 4 (ZEB-228) — hex OutboxEntryId, used to correlate
   * dm-delivered / dm-expired / dm-deleted IPC events to the right
   * Message in the per-channel buffer. Set on self-Messages after
   * send_dm returns; absent on received messages.
   */
  messageId?: string;
  /**
   * ZEB-357 — parsed call-event payload for DM messages carrying the
   * call-event mime type (caller-authored call outcome records). When set,
   * the feed renders a system line instead of a text bubble; `text` holds
   * the direction-aware label as a fallback for previews/search.
   */
  callEvent?: import('./call-log').CallEventPayload;
}

export type ThreadDisplayMode = 'panel' | 'inline' | 'muted';

export type NavNodeType = 'folder' | 'channel' | 'dm' | 'group-chat' | 'community';
export type DisplayMode = 'text' | 'icon' | 'both';
export type SortOrder = 'activity' | 'pinned' | 'alphabetical';
export type UnreadLevel = 'none' | 'quiet' | 'standard' | 'loud';

export type TrustLevel = 'untrusted' | 'preview' | 'trusted';

export interface TrustSettings {
  global: TrustLevel;
  perPeer: Map<string, TrustLevel>;
  perCommunity: Map<string, TrustLevel>;
}

export interface NavNode {
  id: string;
  parentId: string | null;
  type: NavNodeType;
  name: string;
  icon?: string;
  expanded: boolean;
  displayMode?: DisplayMode;
  sortOrder?: SortOrder;
  unreadCount: number;
  /** ZEB-662: session-ephemeral count of unseen @-mentions in this node.
   *  On a community node it is the sum of its descendant channels' counts.
   *  Reset on restart; cleared when the channel is opened. */
  mentionCount: number;
  /** ZEB-357: unseen missed-call count on DM / group-chat rows (the
   *  missed-class subset of unreadCount, from DmUnreadService). Restart-
   *  durable via the shared unread cursor; cleared when the thread opens.
   *  Absent/0 on all other node types. */
  missedCallCount?: number;
  unreadLevel: UnreadLevel;
  /** ZEB-663: channel kind, set only on `type: 'channel'` nodes. Drives the
   *  nav row glyph (# vs 🔊 vs ⚖). Absent on all other node types. */
  channelKind?: 'text' | 'voice' | 'townhall';
  lastActivity?: number;
  peer?: Peer;
  /** ZEB-285: hex SpaceId of the original community this node was forked from.
   *  Present only for community nodes created via fork_community. */
  forkedFrom?: string;
  /**
   * ZEB-254: true when the join countersign has not yet arrived from the admin
   * (invite-only community, admin was offline at redeem time). The community
   * appears in nav greyed/italic until the backend emits nav-updated
   * { pending: false } once the JoinCountersign lands.
   */
  pending?: boolean;
}

/** Mirrors harmony-content VineDescriptor on the TypeScript side. */
export interface VineVideo {
  /** Unique ID for this vine (hex-encoded bundle CID). */
  id: string;
  /** Hex-encoded 128-bit creator address. */
  creatorAddress: string;
  /** Creator display name (resolved from profile store). */
  creatorName: string;
  /** Unix timestamp in seconds when the vine was created. */
  createdAt: number;
  /** Hex-encoded CID of the raw video content blob. */
  videoCid: string;
  /** Optional human-readable title (max 140 bytes). */
  title?: string;
  /** If this vine is a reshare, the hex-encoded CID of the original. */
  reshareOf?: string;
  /**
   * If this vine is a reshare, the address of the original creator
   * (always the true origin — traces through reshare-of-reshare chains).
   * Undefined for non-reshare originals.
   */
  originalCreatorAddress?: string;
  /** Display name of the original creator (snapshot at reshare time). */
  originalCreatorName?: string;
  /** Whether the current user has viewed this vine. */
  viewed: boolean;
  /**
   * ZEB-670: true when this vine is a reshare whose original was deleted
   * by its creator (tombstone). Rendered as a "Removed by creator" stub —
   * no playback, no like/reshare.
   */
  originalRemoved?: boolean;
  /**
   * ZEB-671: transitive-follow graph distance (2 or 3) when the creator
   * is reachable through published follow lists. Undefined for followed
   * creators and unreachable ones — Discover renders only vines with a
   * degree ("no algorithm, just your social graph").
   */
  degree?: number;
  /**
   * ZEB-671: the follow chain that reached the creator — `via[0]` is one
   * of the viewer's own follows; the creator is not included.
   */
  via?: string[];
}

export type AppMode = 'messages' | 'vines' | 'files' | 'spellbook' | 'mail' | 'mint' | 'network';

// ── Mail Types ────────────────────────────────────────────────────────

export type MailFolderKind = 'inbox' | 'sent' | 'drafts' | 'trash';

export interface MailEntry {
  messageCid: string;
  messageId: string;
  senderAddress: string;
  timestamp: number;
  subjectSnippet: string;
  read: boolean;
  /**
   * 'local' = body cached on disk and ready to render.
   * 'pending' = walker registered the header from the gateway but the body
   *   has not been fetched yet; first open will trigger a CAS fetch.
   * Defaults to 'local' on legacy index.json files (serde default in Rust).
   */
  bodyState: 'local' | 'pending';
}

export interface MailMessageDetail {
  messageCid: string;
  messageId: string;
  subject: string;
  body: string;
  senderAddress: string;
  recipients: Array<{ address: string; recipientType: 'to' | 'cc' | 'bcc' }>;
  timestamp: number;
  attachments: Array<{ cid: string; filename: string; mimeType: string; size: number }>;
  isReply: boolean;
  isForward: boolean;
  inReplyTo?: string;
  bodyState: 'local' | 'pending';
}

export interface MailFolderCounts {
  total: number;
  unread: number;
}

export type MailCounts = Record<MailFolderKind, MailFolderCounts>;

// ── File Manager Types ──────────────────────────────────────────────

export type ReplicationTier = 'expendable' | 'light' | 'default' | 'high' | 'ultra';
export type ContentSensitivity = 'public' | 'private' | 'intimate' | 'confidential';
export type FileViewMode = 'list' | 'grid';
export type ContentSection = 'private' | 'published';
export type PublishMode = 'durable' | 'ephemeral';
export type CleanupReason = 'stale' | 'duplicate-of-public' | 'over-replicated' | 'expired';

/** Mirrors harmony-roxy ContentCategory. */
export type ContentCategory = 'music' | 'video' | 'text' | 'image' | 'software' | 'dataset' | 'bundle';

/** Mirrors harmony-roxy UsageRights bitflags as a simpler TS set. */
export type UsageRight = 'stream' | 'download' | 'remix' | 'reshare';

export interface ContentItem {
  /**
   * ZEB-164: opaque per-entry stable identity. Empty string for
   * manifest-derived rows (children of a folder bundle that have no
   * sidecar entry of their own). Backend mutations (pin, archive, burn,
   * setReplicationTier) take sidecarId, not cid.
   */
  sidecarId: string;
  cid: string;
  name: string;
  category: ContentCategory;
  sensitivity: ContentSensitivity;
  sizeBytes: number;
  storedAt: number;
  replicationTier: ReplicationTier;
  /** ZEB-612 S3: observed replica count — 1 (self) + distinct peer
   *  sessions seen announcing this CID. A lower bound ("copies seen").
   *  The fabricated lastAccessed/accessCount/stalenessScore fields were
   *  removed with their renderers; real signals return with real
   *  backends. */
  replicaCount: number;
  pinned: boolean;
  licensed: boolean;
  /** Hidden from the active File Manager list when true. Optional so mock
   *  fixtures can omit it; absent === not archived. */
  archived?: boolean;
  parentCid: string | null;
  isFolder: boolean;
  /** ZEB-669 S3: "back up with buddies" flag. Root sidecar entries only —
   *  false for manifest-derived rows. Optional so mock fixtures can omit
   *  it; absent === not flagged. */
  backup?: boolean;
  /** ZEB-669 S4: provenance recorded at entry creation; `null`/absent for
   *  legacy, manifest-derived, and demo rows (the detail panel renders no
   *  "From" row — never fabricated). */
  origin?: ContentOriginInfo | null;
}

/** ZEB-669 S4: mirrors Rust `OriginInfo` (camelCase serde). The reserved
 *  kinds have no emitting seam yet — channel downloads and buddy pins
 *  don't create index entries today. */
export interface ContentOriginInfo {
  kind: 'selfIngest' | 'channelDownload' | 'buddyPin';
  introducer: string | null;
}

/** ZEB-612 S3: the detail view carries only real fields — the mock
 *  sharedWith/storageBuddies surfaces returned with real hosting
 *  accounting in ZEB-669 (meter/manage sheet); `origin` is now a real
 *  recorded-at-creation field on ContentItem. */
export type ContentDetail = ContentItem;

/** ZEB-612 S3: honest quota — real local usage plus the real (enforced)
 *  pinned-content budget from `get_storage_budget`. There is NO overall
 *  storage quota in the system, so there is no `totalBytes`: the UI
 *  shows used bytes without an invented denominator. */
export interface QuotaStatus {
  usedBytes: number;
  byCategory: Partial<Record<ContentCategory, number>>;
  /** Bytes of pinned content (deduped by CID) — the budget's consumer. */
  pinnedUsedBytes: number;
  /** `maxPinnedBytes` from the backend; null when unknown (demo mode or
   *  IPC failure) — render used-only in that case. */
  pinnedBudgetBytes: number | null;
}

export interface CleanupRecommendation {
  /**
   * ZEB-164: per-entry stable identity matching the ContentItem.sidecarId
   * that backs this recommendation. Required so action handlers can route
   * sidecar mutations (burn/archive/pin) without a CID re-lookup, which
   * would be non-deterministic when two entries share a CID.
   */
  sidecarId: string;
  cid: string;
  name: string;
  category: ContentCategory;
  sensitivity: ContentSensitivity;
  sizeBytes: number;
  reason: CleanupReason;
  stalenessScore: number;
  spaceRecoverable: number;
  confidence: number;
}

export interface PublishedItem {
  cid: string;
  name: string;
  category: ContentCategory;
  sizeBytes: number;
  publishedAt: number;
  publishMode: PublishMode;
}

export interface UploadCandidate {
  file: File;
  sensitivity: ContentSensitivity;
  replicationTier: ReplicationTier;
}

export interface FileManagerSettings {
  defaultReplicationTier: ReplicationTier;
  defaultViewMode: FileViewMode;
  confirmationOverrides: Partial<Record<ContentSensitivity, number>>;
}

// ── Community types (ZEB-263) ─────────────────────────────────────

/** HLC timestamp as serialized by Tauri (serde camelCase). */
export interface Hlc {
  wallMs: number;
  logical: number;
  deviceId: string;
}

export type ModerationEventKind = 'kick' | 'unban' | 'set_power';

/** Mirrors `ModerationEventDto` in src-tauri (ZEB-284). */
export interface ModerationEvent {
  eventId: string;       // 64-char hex
  kind: ModerationEventKind;
  actorAddr: string;     // 32-char hex
  targetAddr: string;    // 32-char hex
  reason: string | null;
  newPower: number | null;
  hlc: Hlc;
}

export interface CommunityMember {
  address: string;
  displayName?: string;
  power: number;       // 0-100
  status: 'joined' | 'left' | 'invited' | 'banned';
  joinedAt?: number;
}

// ZEB-287 Phase 2: types matching backend DTOs for fork lineage IPCs.
// Mirrors the camelCase-serialized shapes returned by get_community_lineage
// and list_community_forks. SpaceId / OwnerAddr are hex strings at the IPC
// boundary; nullable fields use `| null` (matching serde's None → null).

export interface ParentLineageDto {
  /** Hex-encoded SpaceId of this ancestor. */
  spaceId: string;
  /** Frozen display name of this ancestor at the time it was added to the chain. */
  name: string;
  /** wall_ms of this ancestor's fork-from-parent event; null for the root. */
  forkedAtWallMs: number | null;
  /** ZEB-649: why this ancestor forked from its predecessor; null for the
   *  root, pre-ZEB-649 hops, and Phase-1 synthesized entries. */
  reason: string | null;
}

export interface CommunityLineageDto {
  /** Hex SpaceId of immediate parent, or null for top-level. */
  forkedFrom: string | null;
  /** wall_ms of this community's fork-from-parent event; null for top-level. */
  forkedAtWallMs: number | null;
  /** Ancestors above immediate parent (root → above immediate parent). */
  parentLineage: ParentLineageDto[];
  /** This community's own SpaceId (hex). */
  selfSpaceId: string;
  /** This community's own display name. */
  selfName: string;
  /** ZEB-649: THIS community's own fork reason; null for top-level and
   *  pre-ZEB-649 forks. */
  forkReason: string | null;
}

export interface ForkDescendantDto {
  /** Hex SpaceId of the descendant fork community. */
  forkSpaceId: string;
  /** Hex OwnerAddr of the forker. */
  forkerAddr: string;
  /** Resolved display name of forker, or null (Phase 2: always null pending ZEB-281). */
  forkerDisplayName: string | null;
  /** wall_ms of the Fork event. */
  forkedAtWallMs: number;
  /** Whether the descendant community is in local NavService/OwnerState. */
  locallyKnown: boolean;
  /** ZEB-649: the forker's stated why; null for pre-ZEB-649 Fork events. */
  reason: string | null;
}

// Mirrors backend POWER_THRESHOLDS in src-tauri/src/community_membership.rs:1108.
export const POWER_THRESHOLDS = {
  invite: 0,
  kick: 50,
  setPower: 100,
  max: 100,
} as const;

export type PowerRole = 'member' | 'mod' | 'admin';

export function powerToRole(power: number): PowerRole {
  if (power >= POWER_THRESHOLDS.setPower) return 'admin';
  if (power >= POWER_THRESHOLDS.kick) return 'mod';
  return 'member';
}

/**
 * ZEB-250: discriminated return type for admin moderation IPCs
 * (`set_power_level`, `kick_from_community`).
 *
 * - `Completed` → action was applied directly (admin_quorum == 1 OR
 *   action is not admin-affecting). Existing behavior.
 * - `Pending` → action was admin-affecting AND admin_quorum > 1; an
 *   AdminProposal was minted. `signers_so_far` is 1 (the proposer);
 *   awaits `quorum_required - 1` more AdminCountersign events.
 *
 * Mirrors `AdminActionResult` in src-tauri/src/lib.rs.
 */
export type AdminActionResult =
  | { kind: 'Completed' }
  | {
      kind: 'Pending';
      proposal_event_id: string;
      signers_so_far: number;
      quorum_required: number;
    };
