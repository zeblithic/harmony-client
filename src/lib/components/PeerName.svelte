<script lang="ts">
  import type { ResolvedName } from '../display-label';
  import { detectCollision } from '../name-collision';
  import { knownPeersState } from '../known-peers-state.svelte';

  /**
   * ZEB-977: THE component that renders a resolved peer name, styled by
   * provenance. The anti-impersonation invariant lives here and only here:
   * the petname badge + style are keyed off `name.source`, which only the
   * display-ladder functions produce — so a name the peer chose (card /
   * roster / wire) can never render in the style of a name you assigned.
   * The badge is a CSS-drawn element, not a text glyph, so a published name
   * containing a lookalike character cannot imitate it either.
   *
   * ZEB-979: pass `ownerIdHex` to arm collision detection — a third-party
   * name (card/roster/wire) that skeleton-matches a name you know (petname
   * or a known peer's card name) under a DIFFERENT identity gets a warning
   * mark. The mark is the opposite polarity of the petname badge (warning
   * vs trust) and visually disjoint from it, so the ZEB-977 invariant holds.
   */
  let {
    name,
    title,
    ownerIdHex,
  }: { name: ResolvedName; title?: string; ownerIdHex?: string } = $props();

  const collision = $derived(
    ownerIdHex !== undefined
      ? detectCollision(name, ownerIdHex, knownPeersState.index)
      : undefined,
  );

  const sourceTitle: Record<ResolvedName['source'], string> = {
    petname: 'Name you assigned',
    card: 'Name they published',
    roster: 'Name they published',
    wire: 'Unverified name from the message',
    hex: 'Identity prefix',
    self: 'You',
  };
</script>

<span
  class="peer-name"
  class:petname={name.source === 'petname'}
  class:unverified={name.source === 'wire'}
  class:hex={name.source === 'hex'}
  title={title ?? sourceTitle[name.source]}
  data-name-source={name.source}
  data-collision={collision ? 'true' : undefined}
>{#if name.source === 'petname'}<span class="petname-badge" aria-hidden="true"></span>{/if}{name.label}{#if collision}<span
    class="collision-mark"
    role="img"
    aria-label={`Warning: different identity from the ${collision.knownLabel} you know`}
    title={`Different identity from the ${collision.knownLabel} you know`}
  ></span>{/if}</span>

<style>
  .peer-name {
    /* Inherit the host surface's typography; provenance styling is additive. */
    display: inline;
  }

  .peer-name.petname {
    color: var(--accent);
  }

  /* The badge: a small CSS-drawn tag shape. An element, never a character —
     a card name containing "🔖"-alikes gets no styling from it. */
  .petname-badge {
    display: inline-block;
    width: 0.55em;
    height: 0.55em;
    margin-right: 0.3em;
    background: var(--accent);
    clip-path: polygon(0 0, 100% 0, 100% 55%, 50% 100%, 0 55%);
    vertical-align: baseline;
  }

  .peer-name.unverified {
    text-decoration: underline dotted;
    text-decoration-thickness: 1px;
    text-underline-offset: 2px;
    opacity: 0.9;
  }

  .peer-name.hex {
    font-family: var(--font-mono, monospace);
    color: var(--text-muted);
  }

  /* ZEB-979: impersonation-risk mark — a CSS-drawn amber triangle, an
     element (never a character) for the same reason as the petname badge:
     a published name containing "⚠"-alikes gets no styling from it. The
     warning polarity keeps it visually disjoint from the (trust-polarity)
     petname badge, which additionally can never co-occur with it: the badge
     requires source 'petname' and detection excludes that source. */
  .collision-mark {
    display: inline-block;
    width: 0.7em;
    height: 0.62em;
    margin-left: 0.3em;
    background: var(--warning);
    clip-path: polygon(50% 0, 100% 100%, 0 100%);
    vertical-align: baseline;
  }
</style>
