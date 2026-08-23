<script lang="ts">
  import type { ResolvedName } from '../display-label';

  /**
   * ZEB-977: THE component that renders a resolved peer name, styled by
   * provenance. The anti-impersonation invariant lives here and only here:
   * the petname badge + style are keyed off `name.source`, which only the
   * display-ladder functions produce — so a name the peer chose (card /
   * roster / wire) can never render in the style of a name you assigned.
   * The badge is a CSS-drawn element, not a text glyph, so a published name
   * containing a lookalike character cannot imitate it either.
   */
  let { name, title }: { name: ResolvedName; title?: string } = $props();

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
>{#if name.source === 'petname'}<span class="petname-badge" aria-hidden="true"></span>{/if}{name.label}</span>

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
</style>
