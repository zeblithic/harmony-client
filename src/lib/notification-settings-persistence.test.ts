import { describe, it, expect, beforeEach } from 'vitest';
import { NotificationService } from './notification-service';
import {
  notifSettingsKey,
  loadNotificationSettings,
  attachNotificationSettingsPersistence,
} from './notification-settings-persistence';

class MemStorage {
  m = new Map<string, string>();
  getItem(k: string) {
    return this.m.has(k) ? this.m.get(k)! : null;
  }
  setItem(k: string, v: string) {
    this.m.set(k, v);
  }
  removeItem(k: string) {
    this.m.delete(k);
  }
  clear() {
    this.m.clear();
  }
  key(i: number) {
    return [...this.m.keys()][i] ?? null;
  }
  get length() {
    return this.m.size;
  }
}
const OWNER = 'aa'.repeat(16);
const OTHER = 'bb'.repeat(16);
let store: MemStorage;
beforeEach(() => {
  store = new MemStorage();
});

describe('notification-settings-persistence (ZEB-662)', () => {
  it('key is owner-scoped', () => {
    expect(notifSettingsKey(OWNER)).toBe(`harmony:notif-settings:${OWNER}`);
    expect(notifSettingsKey(OWNER)).not.toBe(notifSettingsKey(OTHER));
  });

  it('save-on-change then load restores settings', () => {
    const a = new NotificationService();
    attachNotificationSettingsPersistence(a, OWNER, store as unknown as Storage);
    a.setGlobalPolicy({ quiet: 'silent', standard: 'notify', loud: 'break_dnd' });
    const b = new NotificationService();
    loadNotificationSettings(b, OWNER, store as unknown as Storage);
    expect(b.settings.global).toEqual({ quiet: 'silent', standard: 'notify', loud: 'break_dnd' });
  });

  it('does not leak across owners', () => {
    const a = new NotificationService();
    attachNotificationSettingsPersistence(a, OWNER, store as unknown as Storage);
    a.setPeerPolicy('p', { loud: 'silent' });
    const other = new NotificationService();
    loadNotificationSettings(other, OTHER, store as unknown as Storage);
    expect(other.settings.perPeer.size).toBe(0); // OTHER has no saved settings
  });

  it('load with no stored value is a no-op (keeps defaults)', () => {
    const s = new NotificationService();
    loadNotificationSettings(s, OWNER, store as unknown as Storage);
    expect(s.settings.global).toEqual({ quiet: 'dot_only', standard: 'sound', loud: 'break_dnd' });
  });
});
