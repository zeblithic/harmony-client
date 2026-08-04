# ZEB-774 — @-mention name fallback + hex-discoverable autocomplete (design)

**Ticket:** ZEB-774 (Medium/Bug). **Scope:** A+C (approved). **Surface:** frontend only.

## Problem (verified against current source, 2026-08-04)

After joining a community a peer renders as truncated owner hex for a long,
unbounded window, and during that window they cannot be @-mentioned by name.
The original ticket's top suggested fixes have since shipped via siblings:

- **#1 roster DTO `displayName`** — ✅ ZEB-777. `MemberInfoDto.display_name` is
  now populated in `list_community_members_impl` from the profile-card cache
  (which unions the durable `PersistentCardStore`, ZEB-839).
- **#3 propagation latency** — ✅ ZEB-568 (eager rebroadcast on peer arrival) +
  ZEB-839 (durable cache; offline/restart shows name).
- **#4 headless mention-wake** — ✅ ZEB-780 (pull-based "was I mentioned?").

Two real client-side gaps remain, both in the GUI @-mention path:

- **Gap A — the mention path drops a fallback rung the member panel has.**
  The shared ladder `resolveMentionLabel()` (`mention-render.ts:43`) resolves
  `nickname ?? card.displayName ?? hex8`. The member panel
  (`MemberRow.svelte:127`) has a 4-rung ladder that inserts the roster-DTO
  `member.displayName` before hex. So a peer the roster *already named* (via
  ZEB-777) still shows `@hex8` in the mention autocomplete, in message-author
  labels, and in inline mention tokens — and is unmentionable by name.
- **Gap B — the matcher can't match owner-id hex.** `filterCandidates()`
  (`mention-compose.ts:107`) matches only `c.label`, never `c.ownerId`. During
  a genuine cold window (row = `@hex8`), typing the visible `@2e9a` through the
  popup dead-ends. This is the ticket's "the hex is the only working handle,
  and it is undiscoverable."

**Out of scope / already handled elsewhere:** the roster-DTO enrichment
(ZEB-777), the durable cache (ZEB-839), rebroadcast cadence (ZEB-568), the
headless compose path (ZEB-780), the "unmatched @name + Enter sends literal
text" behaviour (correct per the ticket's correction 2, not a defect), and the
DM author path (`resolveAuthorLabel`, which already has its own 4-rung ladder).

## Design

### 1. Shared ladder gains a 4th rung (Gaps A + C)

Add an optional `resolveRosterName` resolver to the single shared ladder,
inserted exactly where the member panel places it — between the profile-card
name and the short-hex fallback:

```ts
// mention-render.ts
export function resolveMentionLabel(
  ownerId: string,
  resolveNickname?: (id: string) => string | undefined,
  resolveCard?: (id: string) => { displayName: string } | undefined,
  resolveRosterName?: (id: string) => string | undefined,
): string {
  return (
    nonEmpty(resolveNickname?.(ownerId)) ??
    nonEmpty(resolveCard?.(ownerId)?.displayName) ??
    nonEmpty(resolveRosterName?.(ownerId)) ??
    ownerId.slice(0, 8)
  );
}
```

The param is optional, so existing callers are unaffected; the new rung is
active only where `resolveRosterName` is threaded. This mirrors
`MemberRow.svelte:127` (`nickname ?? card ?? member.displayName ?? hex8`) and
the DM author ladder `resolveAuthorLabel`, so all four surfaces converge on the
same fallback order.

### 2. `resolveRosterName` is built once and threaded (single source)

`CommunityView` owns the `members` roster (each carries `displayName` from
ZEB-777). It builds one resolver:

```ts
// CommunityView.svelte
let rosterNameByOwner = $derived(new Map(members.map((m) => [m.address, m.displayName])));
function resolveRosterName(ownerId: string): string | undefined {
  return rosterNameByOwner.get(ownerId) ?? undefined;
}
```

(`m.displayName` is `string | null`; `?? undefined` normalizes, and the ladder's
`nonEmpty()` rejects blanks — `nonEmpty` already accepts `null | undefined`.)

Threading:

- **Gap A** — the `joinedMentionCandidates` label build (`CommunityView:167`)
  passes `resolveRosterName` as the 4th arg to `resolveMentionLabel`.
- **Gap C** — `resolveRosterName` is passed as a prop to `ChannelMessageFeed`
  (direct channel) and `TownHallView` (which forwards it to its nested
  `ChannelMessageFeed` at `:466`). `ChannelMessageFeed` uses it at both
  `resolveMentionLabel` call sites: `authorLabel` (`:547`) and the inline
  mention-token render (`:972`). No `resolveMentionLabel` call site in the
  channel/townhall surface is left on the 3-rung ladder.

### 3. Matcher becomes hex-discoverable (Gap B)

`filterCandidates` becomes a single-pass, mutually-exclusive 3-way partition;
name matches keep their exact current order, hex-prefix-only matches append:

```ts
// mention-compose.ts
export function filterCandidates(candidates, query, limit = 8) {
  const q = query.trim().toLowerCase();
  if (q === '') return candidates.slice(0, limit);
  const labelPrefix = [], labelSubstr = [], hexPrefix = [];
  for (const c of candidates) {
    const label = c.label.toLowerCase();
    if (label.startsWith(q)) labelPrefix.push(c);
    else if (label.includes(q)) labelSubstr.push(c);
    else if (c.ownerId.toLowerCase().startsWith(q)) hexPrefix.push(c);
  }
  return [...labelPrefix, ...labelSubstr, ...hexPrefix].slice(0, limit);
}
```

- **Hex match = prefix** (`ownerId.startsWith(q)`): users type the truncated
  prefix they can see. Prefix keeps noise low.
- **Ranking** = label-prefix ► label-substring ► hex-prefix. A candidate matched
  by label is never re-counted as a hex match (exclusive `else if`), preserving
  today's name-first UX; hex-only matches are purely additive.
- No length gate: the `limit` cap already bounds output, and name matches always
  rank ahead of hex matches.

### 4. Autocomplete row teaches the handle (row hint)

`MentionAutocomplete.svelte` renders a subtle muted owner-hex prefix beside the
label, but **only when the label is a resolved name** (i.e. the label is not
already the hex) — so named rows show `Name  2e9a2151` and unresolved rows
(label already = hex) are not doubled:

```svelte
<button ...>
  <span class="label">{c.label}</span>
  {#if c.label !== c.ownerId.slice(0, 8)}
    <span class="hex-hint">{c.ownerId.slice(0, 8)}</span>
  {/if}
</button>
```

`.hex-hint` is muted/smaller (`var(--text-secondary)`, `0.8em`), floated to the
row's trailing edge. The candidate type is unchanged — the component derives the
hint from the `ownerId` it already carries.

## Error handling / edge cases

- `nonEmpty` already treats `null`/`''`/whitespace as absent, so a roster row
  with a blank/absent `displayName` falls through to the hex rung unchanged.
- The `self` sentinel and wire-supplied `senderName` rungs live only in
  `resolveAuthorLabel` (DM path) and are untouched.
- Hex-prefix matching is additive and case-insensitive; owner ids are lowercase
  hex, so the `toLowerCase()` on both sides is defensive but correct.

## Testing (frontend only — `tsc --noEmit` + vitest)

- `mention-render.test.ts`: roster rung used when nickname+card absent; nickname
  and card still win over roster name; hex only when all three absent.
- `mention-compose.test.ts`: `@2e9a` matches the candidate whose `ownerId`
  starts with `2e9a`; name matches rank before hex-only matches; a query that
  matches a label does not also surface the same candidate as a hex match;
  empty-query and `limit` behavior preserved.
- `MentionAutocomplete.test.ts`: hint shown for a named row; hidden when the
  label equals the 8-char hex.

No Rust changes; the `frontend` CI job (`tsc` + vitest) is the gate.
