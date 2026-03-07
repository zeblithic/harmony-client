import { describe, it, expect } from 'vitest';
import { MockTrustGraphService } from './trust-graph-service';
import { getIdentity, buildScore } from './trust-score';

describe('MockTrustGraphService', () => {
  it('initializes with edges between mock peers', () => {
    const svc = new MockTrustGraphService('local-addr', ['peer-a', 'peer-b', 'peer-c']);
    expect(svc.getEdges().length).toBeGreaterThan(0);
  });

  it('stores localAddress', () => {
    const svc = new MockTrustGraphService('local-addr', ['peer-a']);
    expect(svc.localAddress).toBe('local-addr');
  });
});

describe('setScore / directScore', () => {
  it('sets and retrieves a direct score', () => {
    const svc = new MockTrustGraphService('local-addr', ['peer-a']);
    svc.setScore('peer-a', 0xFF);
    expect(svc.directScore('local-addr', 'peer-a')).toBe(0xFF);
  });

  it('updates existing score', () => {
    const svc = new MockTrustGraphService('local-addr', ['peer-a']);
    svc.setScore('peer-a', 0xFF);
    svc.setScore('peer-a', 0x00);
    expect(svc.directScore('local-addr', 'peer-a')).toBe(0x00);
  });

  it('returns null for unscored pair', () => {
    const svc = new MockTrustGraphService('local-addr', ['peer-a']);
    svc.clearAllLocalScores();
    expect(svc.directScore('local-addr', 'peer-a')).toBeNull();
  });
});

describe('clearScore', () => {
  it('removes a score', () => {
    const svc = new MockTrustGraphService('local-addr', ['peer-a']);
    svc.setScore('peer-a', 0xFF);
    svc.clearScore('peer-a');
    expect(svc.directScore('local-addr', 'peer-a')).toBeNull();
  });
});

describe('edgesFrom / edgesTo', () => {
  it('edgesFrom returns all edges from a given source', () => {
    const svc = new MockTrustGraphService('local-addr', ['peer-a', 'peer-b']);
    svc.setScore('peer-a', 0xFF);
    svc.setScore('peer-b', 0x80);
    const edges = svc.edgesFrom('local-addr');
    const targets = edges.map((e) => e.target);
    expect(targets).toContain('peer-a');
    expect(targets).toContain('peer-b');
  });

  it('edgesTo returns all edges pointing to a given target', () => {
    const svc = new MockTrustGraphService('local-addr', ['peer-a', 'peer-b']);
    svc.setScore('peer-a', 0xFF);
    // peer-a may also have mock edges pointing to others
    const edges = svc.edgesTo('peer-a');
    expect(edges.length).toBeGreaterThan(0);
    expect(edges.every((e) => e.target === 'peer-a')).toBe(true);
  });
});

describe('resolveMediaTrust', () => {
  it('returns untrusted for identity 00', () => {
    const svc = new MockTrustGraphService('local-addr', ['peer-a']);
    svc.setScore('peer-a', buildScore(0, 3, 3, 3));
    expect(svc.resolveMediaTrust('peer-a')).toBe('untrusted');
  });

  it('returns untrusted for identity 01', () => {
    const svc = new MockTrustGraphService('local-addr', ['peer-a']);
    svc.setScore('peer-a', buildScore(1, 3, 3, 3));
    expect(svc.resolveMediaTrust('peer-a')).toBe('untrusted');
  });

  it('returns preview for identity 10', () => {
    const svc = new MockTrustGraphService('local-addr', ['peer-a']);
    svc.setScore('peer-a', buildScore(2, 0, 0, 0));
    expect(svc.resolveMediaTrust('peer-a')).toBe('preview');
  });

  it('returns trusted for identity 11', () => {
    const svc = new MockTrustGraphService('local-addr', ['peer-a']);
    svc.setScore('peer-a', buildScore(3, 0, 0, 0));
    expect(svc.resolveMediaTrust('peer-a')).toBe('trusted');
  });

  it('returns null for unscored peer', () => {
    const svc = new MockTrustGraphService('local-addr', ['peer-a']);
    svc.clearAllLocalScores();
    expect(svc.resolveMediaTrust('peer-a')).toBeNull();
  });
});
