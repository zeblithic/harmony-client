import { describe, it, expect } from 'vitest';
import { TrustService } from './trust-service';
import { MockTrustGraphService } from './trust-graph-service';
import { buildScore } from './trust-score';

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

describe('TrustService with trust graph fallback', () => {
  it('falls back to trust graph when no override exists', () => {
    const graph = new MockTrustGraphService('local', ['peer-1']);
    graph.setScore('peer-1', buildScore(3, 0, 0, 0)); // identity=3 -> trusted
    const svc = new TrustService(graph);
    expect(svc.resolve('peer-1')).toBe('trusted');
  });

  it('per-peer override beats trust graph', () => {
    const graph = new MockTrustGraphService('local', ['peer-1']);
    graph.setScore('peer-1', buildScore(3, 0, 0, 0)); // identity=3 -> trusted
    const svc = new TrustService(graph);
    svc.setPeerTrust('peer-1', 'untrusted');
    expect(svc.resolve('peer-1')).toBe('untrusted');
  });

  it('per-community override beats trust graph', () => {
    const graph = new MockTrustGraphService('local', ['peer-1']);
    graph.setScore('peer-1', buildScore(3, 0, 0, 0));
    const svc = new TrustService(graph);
    svc.setCommunityTrust('comm-1', 'untrusted');
    expect(svc.resolve('peer-1', 'comm-1')).toBe('untrusted');
  });

  it('falls through to global when trust graph returns null', () => {
    const graph = new MockTrustGraphService('local', ['peer-1']);
    graph.clearAllLocalScores();
    const svc = new TrustService(graph);
    expect(svc.resolve('peer-1')).toBe('untrusted');
  });

  it('works without a trust graph (backwards compatible)', () => {
    const svc = new TrustService();
    expect(svc.resolve('peer-1')).toBe('untrusted');
  });
});
