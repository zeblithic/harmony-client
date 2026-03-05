import { describe, it, expect } from 'vitest';
import { TrustService } from './trust-service';

describe('TrustService', () => {
  it('returns global default (untrusted) when no overrides exist', () => {
    const svc = new TrustService();
    expect(svc.resolve('peer-1')).toBe('untrusted');
  });

  it('respects custom global default', () => {
    const svc = new TrustService();
    svc.setGlobalTrust('trusted');
    expect(svc.resolve('peer-1')).toBe('trusted');
  });

  it('respects per-peer override over global', () => {
    const svc = new TrustService();
    svc.setPeerTrust('peer-1', 'trusted');
    expect(svc.resolve('peer-1')).toBe('trusted');
    expect(svc.resolve('peer-2')).toBe('untrusted');
  });

  it('respects per-community override over global', () => {
    const svc = new TrustService();
    svc.setCommunityTrust('comm-1', 'trusted');
    expect(svc.resolve('peer-1', 'comm-1')).toBe('trusted');
    expect(svc.resolve('peer-1')).toBe('untrusted');
  });

  it('per-peer beats per-community', () => {
    const svc = new TrustService();
    svc.setCommunityTrust('comm-1', 'trusted');
    svc.setPeerTrust('peer-1', 'untrusted');
    expect(svc.resolve('peer-1', 'comm-1')).toBe('untrusted');
  });

  it('clearPeerTrust removes the override', () => {
    const svc = new TrustService();
    svc.setPeerTrust('peer-1', 'trusted');
    svc.clearPeerTrust('peer-1');
    expect(svc.resolve('peer-1')).toBe('untrusted');
  });

  it('clearCommunityTrust removes the override', () => {
    const svc = new TrustService();
    svc.setCommunityTrust('comm-1', 'trusted');
    svc.clearCommunityTrust('comm-1');
    expect(svc.resolve('peer-1', 'comm-1')).toBe('untrusted');
  });
});

describe('TrustService.loadedAttachments', () => {
  it('tracks loaded attachment IDs', () => {
    const svc = new TrustService();
    expect(svc.isLoaded('att-1')).toBe(false);
    svc.markLoaded('att-1');
    expect(svc.isLoaded('att-1')).toBe(true);
    expect(svc.isLoaded('att-2')).toBe(false);
  });

  it('clearLoaded resets all loaded state', () => {
    const svc = new TrustService();
    svc.markLoaded('att-1');
    svc.markLoaded('att-2');
    svc.clearLoaded();
    expect(svc.isLoaded('att-1')).toBe(false);
    expect(svc.isLoaded('att-2')).toBe(false);
  });
});

describe('TrustService.isGated', () => {
  it('gates image attachments', () => {
    expect(TrustService.isGated({ id: '1', type: 'image', url: 'https://example.com/img.png' })).toBe(true);
  });

  it('gates link attachments', () => {
    expect(TrustService.isGated({ id: '2', type: 'link', url: 'https://example.com' })).toBe(true);
  });

  it('does not gate code attachments', () => {
    expect(TrustService.isGated({ id: '3', type: 'code', content: 'console.log("hi")' })).toBe(false);
  });
});
