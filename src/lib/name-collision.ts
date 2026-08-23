/**
 * ZEB-979: card-name collision detection — flag a name published by an
 * identity you DON'T know when it skeleton-matches a name you associate with
 * an identity you DO know (a petname you assigned, a contact's verified card
 * name, or an Active friend's name).
 *
 * The threat: display names are non-unique by design (identity = pubkey), so
 * any identity can publish a card named "Jake". After ZEB-977/978 such a name
 * renders visibly as card/wire provenance — but a viewer who knows a Jake can
 * still be fooled by an identical-looking string. Detection compares NAME
 * SKELETONS, not raw strings, so case tricks, Unicode normalization games,
 * zero-width padding, and common cross-script homoglyphs ("Jаke" with a
 * Cyrillic а) all land on the same key as the honest spelling.
 *
 * Pure functions only — the reactive index handoff lives in
 * `known-peers-state.svelte.ts`, and `PeerName.svelte` / `ProfilePopover`
 * are the render sites.
 */
import { nonEmpty, type ResolvedName } from './display-label';

/** One name you associate with a known identity (input to the index). */
export interface KnownPeerEntry {
  label: string;
  /** 32-char lowercase master owner_id hex (any case accepted, normalized). */
  ownerIdHex: string;
}

/** A detected collision: the known peer the rendered name masquerades as. */
export interface CollisionInfo {
  knownLabel: string;
  knownHex: string;
}

/**
 * skeleton → first known peer claiming it, PLUS the set of all known hexes.
 * `knownHexes` is load-bearing and not derivable from `bySkeleton`: two known
 * peers can legitimately share a skeleton (first-wins keeps one), and the
 * loser must still be exempt from detection — the warning is scoped to
 * identities that are NOT known peers (ticket: "NOT one of your contacts").
 */
export interface KnownPeersIndex {
  bySkeleton: Map<string, { label: string; ownerIdHex: string }>;
  knownHexes: Set<string>;
}

export const EMPTY_KNOWN_PEERS: KnownPeersIndex = {
  bySkeleton: new Map(),
  knownHexes: new Set(),
};

/**
 * Zero-width + layout control characters an attacker can pad a name with at
 * zero visual cost: ZWSP..ZWJ, word joiner, BOM/ZWNBSP, soft hyphen, and the
 * bidi embedding/override/isolate controls (U+200E/F, U+202A–E, U+2066–69).
 */
const INVISIBLES = /[​-‏⁠﻿­‪-‮⁦-⁩]/g;

/**
 * Curated homoglyph map, applied AFTER NFKC + case-fold (so keys are the
 * lowercase forms; a Cyrillic capital Ve in "Вob" reaches the table as в).
 * Deliberately small and hand-auditable — the common Cyrillic/Greek/digit
 * lookalikes that make mixed-script spoofs cheap — not the full UTS #39
 * confusables table (~100 KB, more false positives). Extend as real cases
 * appear; every addition should name the lookalike pair it closes.
 */
const HOMOGLYPHS: Record<string, string> = {
  // Cyrillic → Latin (lowercase forms whose upper- or lowercase glyph
  // shadows a Latin letter).
  а: 'a', // U+0430
  в: 'b', // U+0432 (В ~ B)
  е: 'e', // U+0435
  ё: 'e', // U+0451
  к: 'k', // U+043A (К ~ K)
  м: 'm', // U+043C (М ~ M)
  н: 'h', // U+043D (Н ~ H)
  о: 'o', // U+043E
  р: 'p', // U+0440
  с: 'c', // U+0441
  т: 't', // U+0442 (Т ~ T)
  у: 'y', // U+0443
  х: 'x', // U+0445
  і: 'i', // U+0456
  ї: 'i', // U+0457
  ј: 'j', // U+0458
  ѕ: 's', // U+0455
  ԁ: 'd', // U+0501
  ԛ: 'q', // U+051B
  ԝ: 'w', // U+051D
  // Greek → Latin.
  α: 'a', // U+03B1
  β: 'b', // U+03B2
  ε: 'e', // U+03B5
  η: 'n', // U+03B7
  ι: 'i', // U+03B9
  κ: 'k', // U+03BA
  ν: 'v', // U+03BD
  ο: 'o', // U+03BF
  ρ: 'p', // U+03C1
  τ: 't', // U+03C4
  υ: 'u', // U+03C5
  χ: 'x', // U+03C7
  ω: 'w', // U+03C9
  // Digit → letter lookalikes.
  '0': 'o',
  '1': 'l',
  // Dotless i (ı, U+0131) — Turkish casefold artifact and a classic lookalike.
  ı: 'i',
};

/**
 * Canonical comparison key for a display name. Two names with the same
 * skeleton are treated as "the same name" for collision purposes.
 * Order matters: NFKC first (folds fullwidth/ligature forms into ASCII so
 * case-fold and the homoglyph table see plain letters), then case-fold, then
 * invisible-strip, then homoglyph mapping, then whitespace collapse.
 */
export function nameSkeleton(name: string): string {
  const folded = name.normalize('NFKC').toLowerCase().replace(INVISIBLES, '');
  let out = '';
  for (const ch of folded) out += HOMOGLYPHS[ch] ?? ch;
  return out.replace(/\s+/g, ' ').trim();
}

/**
 * Build the known-peers index from every name pool the caller trusts.
 * Pass pools strongest-claim-first (petnames, then contacts' card names,
 * then friends): a skeleton tie keeps the FIRST entry, so the warning names
 * the strongest claimant. `extraKnownHexes` exempts identities with no name
 * entry — the caller passes the local user's own hex so a self render can
 * never collide with a petname you assigned to someone else.
 */
export function buildKnownPeersIndex(
  entries: readonly KnownPeerEntry[],
  extraKnownHexes: readonly string[] = [],
): KnownPeersIndex {
  const bySkeleton = new Map<string, { label: string; ownerIdHex: string }>();
  const knownHexes = new Set<string>();
  for (const entry of entries) {
    const hex = entry.ownerIdHex.toLowerCase();
    knownHexes.add(hex);
    const label = nonEmpty(entry.label);
    if (label === undefined) continue;
    const skeleton = nameSkeleton(label);
    if (skeleton === '') continue;
    if (!bySkeleton.has(skeleton)) {
      bySkeleton.set(skeleton, { label: label.trim(), ownerIdHex: hex });
    }
  }
  for (const hex of extraKnownHexes) knownHexes.add(hex.toLowerCase());
  return { bySkeleton, knownHexes };
}

/**
 * Does `name`, rendered for `ownerIdHex`, masquerade as a known peer?
 *
 * Fires only when ALL of:
 *  - the label is third-party text (`card` / `roster` / `wire`) — a
 *    `petname` is yours, `hex` is derived, `self` is you;
 *  - `ownerIdHex` is NOT itself a known peer (known peers rendering their
 *    own — possibly shared — name are never flagged);
 *  - the label's skeleton matches a known peer's name.
 *
 * The known peer's hex is guaranteed ≠ `ownerIdHex` by the second condition,
 * so a hit always means "same name, different identity".
 */
export function detectCollision(
  name: Pick<ResolvedName, 'label' | 'source'>,
  ownerIdHex: string,
  index: KnownPeersIndex,
): CollisionInfo | undefined {
  if (name.source !== 'card' && name.source !== 'roster' && name.source !== 'wire') {
    return undefined;
  }
  const hex = ownerIdHex.toLowerCase();
  if (index.knownHexes.has(hex)) return undefined;
  const skeleton = nameSkeleton(name.label);
  if (skeleton === '') return undefined;
  const known = index.bySkeleton.get(skeleton);
  if (known === undefined) return undefined;
  return { knownLabel: known.label, knownHex: known.ownerIdHex };
}
