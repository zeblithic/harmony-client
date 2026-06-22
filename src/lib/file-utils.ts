import type { ContentCategory, ContentItem, ContentSensitivity, ReplicationTier } from './types';

const CATEGORY_ICONS: Record<ContentCategory, string> = {
  music: '\u266A',
  video: '\u25B6',
  text: '\uD83D\uDCC4',
  image: '\uD83D\uDDBC',
  software: '\u2699',
  dataset: '\uD83D\uDCCA',
  bundle: '\uD83D\uDCC1',
};

const TIER_TARGETS: Record<ReplicationTier, number> = {
  expendable: 1,
  light: 2,
  default: 3,
  high: 5,
  ultra: 9,
};

const SENSITIVITY_ICONS: Record<ContentSensitivity, string> = {
  public: '\uD83C\uDF10',       // 🌐
  private: '\uD83D\uDD12',      // 🔒
  intimate: '\uD83D\uDEE1\uFE0F', // 🛡️
  confidential: '\u26A0\uFE0F', // ⚠️
};

export function sensitivityIcon(sensitivity: ContentSensitivity): string {
  return SENSITIVITY_ICONS[sensitivity];
}

export function categoryIcon(category: ContentCategory): string {
  return CATEGORY_ICONS[category] ?? '\uD83D\uDCC4';
}

/** Map a MIME type to a category glyph. `categoryIcon` only accepts the
 *  ContentCategory enum, so this bridges a raw mime (as carried on a
 *  ChannelAttachmentDto) to the same icon set. Unknown mimes get the
 *  generic document glyph. */
export function mimeCategoryIcon(mime: string): string {
  const m = mime.toLowerCase();
  if (m.startsWith('image/')) return CATEGORY_ICONS.image;
  if (m.startsWith('audio/')) return CATEGORY_ICONS.music;
  if (m.startsWith('video/')) return CATEGORY_ICONS.video;
  if (m.startsWith('text/')) return CATEGORY_ICONS.text;
  return CATEGORY_ICONS.text;
}

export function tierTarget(tier: ReplicationTier): number {
  return TIER_TARGETS[tier];
}

export function formatBytes(bytes: number): string {
  if (bytes >= 1_000_000_000) return (bytes / 1_000_000_000).toFixed(1) + ' GB';
  if (bytes >= 1_000_000) return (bytes / 1_000_000).toFixed(1) + ' MB';
  if (bytes >= 1_000) return (bytes / 1_000).toFixed(1) + ' KB';
  return bytes + ' B';
}

// ZEB-299 inline-rename edit-mode match. Top-level rows carry a
// non-empty sidecarId (the sidecar's unique key); manifest-derived
// nested rows carry "". For top-level use sidecarId — two sidecar
// entries CAN share name+cid because ContentIndex::insert only dedupes
// by id. For nested fall back to (name, cid), which manifest
// invariants make unique within a folder. Shared by FileList and
// FileGrid to keep their per-row matching in lockstep.
export function matchesEditing(
  editingItem: ContentItem | null,
  item: ContentItem,
): boolean {
  if (editingItem === null) return false;
  if (editingItem.sidecarId && item.sidecarId) {
    return editingItem.sidecarId === item.sidecarId;
  }
  if (!editingItem.sidecarId && !item.sidecarId) {
    return editingItem.name === item.name && editingItem.cid === item.cid;
  }
  return false;
}

export function relativeTime(timestamp: number): string {
  const diff = Date.now() - timestamp;
  if (diff < 0) return 'just now';
  const mins = Math.floor(diff / 60_000);
  if (mins < 1) return 'just now';
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d ago`;
  const months = Math.floor(days / 30);
  return `${months}mo ago`;
}
