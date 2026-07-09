/**
 * ZEB-662 — owner-scoped localStorage persistence for NotificationService.
 * Mirrors the ZEB-586 owner-scoping pattern: settings are keyed by the owner's
 * hex id so switching identity never leaks another owner's policy. Storage is
 * injectable for tests; defaults to window.localStorage when available.
 */
import type { NotificationService } from './notification-service';

export function notifSettingsKey(ownerIdHex: string): string {
  return `harmony:notif-settings:${ownerIdHex}`;
}

function defaultStorage(): Storage | null {
  try {
    return typeof localStorage !== 'undefined' ? localStorage : null;
  } catch {
    return null;
  }
}

/** Hydrate `service` from persisted settings for `ownerIdHex` (no-op if none). */
export function loadNotificationSettings(
  service: NotificationService,
  ownerIdHex: string,
  storage: Storage | null = defaultStorage(),
): void {
  if (!storage || !ownerIdHex) return;
  const raw = storage.getItem(notifSettingsKey(ownerIdHex));
  if (raw) service.load(raw);
}

/** Wire `service.onChange` to persist on every mutation for `ownerIdHex`. */
export function attachNotificationSettingsPersistence(
  service: NotificationService,
  ownerIdHex: string,
  storage: Storage | null = defaultStorage(),
): void {
  if (!storage || !ownerIdHex) return;
  service.onChange = () => {
    try {
      storage.setItem(notifSettingsKey(ownerIdHex), service.serialize());
    } catch {
      /* quota/denied — best-effort */
    }
  };
}
