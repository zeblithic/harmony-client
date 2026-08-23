import { describe, it, expect } from 'vitest';
import { render } from '@testing-library/svelte';
import PeerName from './PeerName.svelte';
import type { ResolvedName } from '../display-label';

// ZEB-977: PeerName is the single place provenance styling happens, so these
// tests pin the anti-impersonation invariant: the petname badge + class exist
// ONLY for `source: 'petname'` — a peer-chosen name (card/roster/wire) can
// never acquire them, no matter what string it carries.

function renderName(name: ResolvedName) {
  const { container } = render(PeerName, { props: { name } });
  const span = container.querySelector('.peer-name')!;
  return {
    span,
    badge: container.querySelector('.petname-badge'),
    text: span.textContent,
  };
}

describe('PeerName', () => {
  it('renders a petname with the badge element and petname class', () => {
    const { span, badge, text } = renderName({ label: 'Koya', source: 'petname' });
    expect(text).toBe('Koya');
    expect(span.classList.contains('petname')).toBe(true);
    expect(badge).not.toBeNull();
    expect(span.getAttribute('data-name-source')).toBe('petname');
  });

  it('renders a card name plain — no badge, no petname class', () => {
    const { span, badge } = renderName({ label: 'Jake', source: 'card' });
    expect(span.classList.contains('petname')).toBe(false);
    expect(badge).toBeNull();
  });

  it('a card name EQUAL to a petname string still gets no petname styling', () => {
    // The attack from ZEB-977: attacker publishes the victim's petname as
    // their card displayName. The style is keyed off source, not the string.
    const { span, badge } = renderName({ label: 'Koya', source: 'card' });
    expect(span.classList.contains('petname')).toBe(false);
    expect(badge).toBeNull();
  });

  it('a card name containing a lookalike glyph gains nothing — the badge is an element, not text', () => {
    const { span, badge, text } = renderName({ label: '🔖 Koya', source: 'card' });
    expect(text).toBe('🔖 Koya'); // the glyph renders as ordinary text…
    expect(badge).toBeNull(); // …but the badge ELEMENT only exists for petnames
    expect(span.classList.contains('petname')).toBe(false);
  });

  it('marks a wire-sourced name as unverified', () => {
    const { span } = renderName({ label: 'FreeText', source: 'wire' });
    expect(span.classList.contains('unverified')).toBe(true);
    expect(span.classList.contains('petname')).toBe(false);
  });

  it('marks a hex fallback with the hex class', () => {
    const { span } = renderName({ label: 'aabbccdd', source: 'hex' });
    expect(span.classList.contains('hex')).toBe(true);
  });

  it('title conveys provenance; an explicit title overrides', () => {
    const { span } = renderName({ label: 'Koya', source: 'petname' });
    expect(span.getAttribute('title')).toBe('Name you assigned');
    const { container } = render(PeerName, {
      props: { name: { label: 'X', source: 'card' } as ResolvedName, title: 'custom' },
    });
    expect(container.querySelector('.peer-name')!.getAttribute('title')).toBe('custom');
  });
});
