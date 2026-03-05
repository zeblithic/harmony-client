import { describe, it, expect } from 'vitest';
import { NotificationService } from './notification-service';
import type { NotificationPolicy } from './types';

const DEFAULT_POLICY: NotificationPolicy = {
  quiet: 'dot_only',
  standard: 'sound',
  loud: 'break_dnd',
};

describe('NotificationService', () => {
  it('returns global defaults when no overrides exist', () => {
    const svc = new NotificationService();
    expect(svc.resolve('quiet', 'peer-1')).toBe('dot_only');
    expect(svc.resolve('standard', 'peer-1')).toBe('sound');
    expect(svc.resolve('loud', 'peer-1')).toBe('break_dnd');
  });

  it('respects per-peer override over global', () => {
    const svc = new NotificationService();
    svc.setPeerPolicy('peer-1', { quiet: 'silent' });
    expect(svc.resolve('quiet', 'peer-1')).toBe('silent');
    expect(svc.resolve('standard', 'peer-1')).toBe('sound');
  });

  it('respects per-community override over global', () => {
    const svc = new NotificationService();
    svc.setCommunityPolicy('comm-1', { standard: 'notify' });
    expect(svc.resolve('standard', 'peer-1', 'comm-1')).toBe('notify');
    expect(svc.resolve('standard', 'peer-1')).toBe('sound');
  });

  it('per-peer beats per-community', () => {
    const svc = new NotificationService();
    svc.setCommunityPolicy('comm-1', { loud: 'sound' });
    svc.setPeerPolicy('peer-1', { loud: 'silent' });
    expect(svc.resolve('loud', 'peer-1', 'comm-1')).toBe('silent');
  });

  it('falls through partial peer override to community', () => {
    const svc = new NotificationService();
    svc.setCommunityPolicy('comm-1', { standard: 'notify' });
    svc.setPeerPolicy('peer-1', { quiet: 'silent' });
    expect(svc.resolve('standard', 'peer-1', 'comm-1')).toBe('notify');
  });

  it('setGlobalPolicy replaces all global defaults', () => {
    const svc = new NotificationService();
    svc.setGlobalPolicy({ quiet: 'silent', standard: 'notify', loud: 'sound' });
    expect(svc.resolve('quiet', 'peer-1')).toBe('silent');
    expect(svc.resolve('standard', 'peer-1')).toBe('notify');
    expect(svc.resolve('loud', 'peer-1')).toBe('sound');
  });

  it('clearPeerPolicy removes the override', () => {
    const svc = new NotificationService();
    svc.setPeerPolicy('peer-1', { quiet: 'silent' });
    svc.clearPeerPolicy('peer-1');
    expect(svc.resolve('quiet', 'peer-1')).toBe('dot_only');
  });

  it('clearCommunityPolicy removes the override', () => {
    const svc = new NotificationService();
    svc.setCommunityPolicy('comm-1', { standard: 'notify' });
    svc.clearCommunityPolicy('comm-1');
    expect(svc.resolve('standard', 'peer-1', 'comm-1')).toBe('sound');
  });

  it('shouldPlaySound returns true for sound and break_dnd', () => {
    const svc = new NotificationService();
    expect(svc.shouldPlaySound('sound')).toBe(true);
    expect(svc.shouldPlaySound('break_dnd')).toBe(true);
    expect(svc.shouldPlaySound('notify')).toBe(false);
    expect(svc.shouldPlaySound('dot_only')).toBe(false);
    expect(svc.shouldPlaySound('silent')).toBe(false);
  });
});
