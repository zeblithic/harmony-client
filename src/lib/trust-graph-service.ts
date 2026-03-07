import type { TrustLevel } from './types';
import type { TrustScore, TrustEdge } from './trust-score';
import { getIdentity } from './trust-score';

export interface TrustGraphService {
  resolveMediaTrust(peerAddress: string): TrustLevel | null;
}

function randomInt(min: number, max: number): number {
  return Math.floor(min + Math.random() * (max - min + 1));
}

export class MockTrustGraphService implements TrustGraphService {
  readonly localAddress: string;
  private edges: TrustEdge[] = [];

  constructor(localAddress: string, peerAddresses: string[]) {
    this.localAddress = localAddress;
    this.initMockEdges(peerAddresses);
  }

  private initMockEdges(peers: string[]): void {
    const allAddresses = [this.localAddress, ...peers];

    for (const source of allAddresses) {
      let hasEdge = false;
      for (const target of allAddresses) {
        if (source === target) continue;
        // ~60% chance of having a score for any given peer
        if (Math.random() < 0.4) {
          continue;
        }
        hasEdge = true;
        this.edges.push({
          source,
          target,
          score: randomInt(0, 255) as TrustScore,
          timestamp: Date.now() - randomInt(0, 7 * 24 * 60 * 60 * 1000),
        });
      }
      // Guarantee at least one edge per source to avoid flaky tests
      if (!hasEdge && allAddresses.length > 1) {
        const target = allAddresses.find((a) => a !== source)!;
        this.edges.push({
          source,
          target,
          score: randomInt(0, 255) as TrustScore,
          timestamp: Date.now() - randomInt(0, 7 * 24 * 60 * 60 * 1000),
        });
      }
    }
  }

  setScore(target: string, score: TrustScore): void {
    const existing = this.edges.find(
      (e) => e.source === this.localAddress && e.target === target,
    );
    if (existing) {
      existing.score = score;
      existing.timestamp = Date.now();
    } else {
      this.edges.push({
        source: this.localAddress,
        target,
        score,
        timestamp: Date.now(),
      });
    }
  }

  clearScore(target: string): void {
    this.edges = this.edges.filter(
      (e) => !(e.source === this.localAddress && e.target === target),
    );
  }

  /** Remove all local user's scores (useful for testing) */
  clearAllLocalScores(): void {
    this.edges = this.edges.filter((e) => e.source !== this.localAddress);
  }

  getEdges(): TrustEdge[] {
    return this.edges;
  }

  edgesFrom(address: string): TrustEdge[] {
    return this.edges.filter((e) => e.source === address);
  }

  edgesTo(address: string): TrustEdge[] {
    return this.edges.filter((e) => e.target === address);
  }

  directScore(source: string, target: string): TrustScore | null {
    const edge = this.edges.find((e) => e.source === source && e.target === target);
    return edge ? edge.score : null;
  }

  /**
   * Derive media trust level from the identity dimension of the local
   * user's score for a peer. Returns null if the peer is unscored.
   */
  resolveMediaTrust(peerAddress: string): TrustLevel | null {
    const score = this.directScore(this.localAddress, peerAddress);
    if (score === null) return null;
    const identity = getIdentity(score);
    if (identity <= 1) return 'untrusted';
    if (identity === 2) return 'preview';
    return 'trusted';
  }
}
