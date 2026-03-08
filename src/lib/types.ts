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
}

export type ThreadDisplayMode = 'panel' | 'inline' | 'muted';

export type NavNodeType = 'folder' | 'channel' | 'dm' | 'group-chat';
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
  unreadLevel: UnreadLevel;
  lastActivity?: number;
  peer?: Peer;
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
  /** Whether the current user has viewed this vine. */
  viewed: boolean;
}

export type AppMode = 'messages' | 'vines';
