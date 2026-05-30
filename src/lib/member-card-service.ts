export interface ResolvedCard {
  displayName: string;
  statusText: string;
}

/**
 * Resolves a community member's owner_id -> {displayName, statusText}.
 * Task 1 (ZEB-341): self-seed only (local profile, synchronous, no network).
 * A later task extends this to subscribe to peers' owner_id card broadcasts.
 * owner_id keys are lowercase 32-char hex (matches MemberInfoDto.addr / msg.author).
 */
export class MemberCardService {
  private cards: Map<string, ResolvedCard> = new Map();
  /** Seed (or overwrite) the self owner_id from the local profile. Synchronous. */
  seedSelf(ownerIdHex: string, profile: ResolvedCard): void {
    this.cards.set(ownerIdHex.toLowerCase(), { ...profile });
  }
  /** Resolve an owner_id to its card, or undefined if unresolved. */
  resolve(ownerIdHex: string): ResolvedCard | undefined {
    return this.cards.get(ownerIdHex.toLowerCase());
  }
}
