import type { ContentCategory, ReplicationTier } from './types';

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

export function categoryIcon(category: ContentCategory): string {
  return CATEGORY_ICONS[category] ?? '\uD83D\uDCC4';
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

export function relativeTime(timestamp: number): string {
  const diff = Date.now() - timestamp;
  const mins = Math.floor(diff / 60_000);
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d ago`;
  const months = Math.floor(days / 30);
  return `${months}mo ago`;
}
