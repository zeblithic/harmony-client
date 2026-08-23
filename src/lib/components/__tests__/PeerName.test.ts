import { render } from '@testing-library/svelte';
import { afterEach, describe, expect, it } from 'vitest';
import PeerName from '../PeerName.svelte';
import { knownPeersState } from '../../known-peers-state.svelte';
import { buildKnownPeersIndex, EMPTY_KNOWN_PEERS } from '../../name-collision';

const KNOWN_HEX = 'aaaa1111aaaa1111aaaa1111aaaa1111';
const STRANGER_HEX = 'dddd4444dddd4444dddd4444dddd4444';

const indexKnowingJake = () =>
  buildKnownPeersIndex([{ label: 'Jake', ownerIdHex: KNOWN_HEX }]);

afterEach(() => {
  knownPeersState.index = EMPTY_KNOWN_PEERS;
});

describe('PeerName collision mark (ZEB-979)', () => {
  it('marks a stranger card name that collides with a known peer', () => {
    knownPeersState.index = indexKnowingJake();
    const { container, getByRole } = render(PeerName, {
      props: { name: { label: 'Jake', source: 'card' as const }, ownerIdHex: STRANGER_HEX },
    });
    const span = container.querySelector('.peer-name');
    expect(span?.getAttribute('data-collision')).toBe('true');
    const mark = getByRole('img');
    expect(mark.getAttribute('aria-label')).toContain('different identity');
    expect(mark.getAttribute('aria-label')).toContain('Jake');
  });

  it('marks a homoglyph collision (Cyrillic а)', () => {
    knownPeersState.index = indexKnowingJake();
    const { container } = render(PeerName, {
      props: { name: { label: 'Jаke', source: 'card' as const }, ownerIdHex: STRANGER_HEX },
    });
    expect(container.querySelector('[data-collision="true"]')).toBeTruthy();
  });

  it('does not mark when no ownerIdHex is provided (site not yet threaded)', () => {
    knownPeersState.index = indexKnowingJake();
    const { container } = render(PeerName, {
      props: { name: { label: 'Jake', source: 'card' as const } },
    });
    expect(container.querySelector('[data-collision]')).toBeNull();
  });

  it('does not mark the known peer rendering their own name', () => {
    knownPeersState.index = indexKnowingJake();
    const { container } = render(PeerName, {
      props: { name: { label: 'Jake', source: 'card' as const }, ownerIdHex: KNOWN_HEX },
    });
    expect(container.querySelector('[data-collision]')).toBeNull();
  });

  it('never marks a petname — and the mark is disjoint from the petname badge', () => {
    knownPeersState.index = indexKnowingJake();
    const { container } = render(PeerName, {
      props: { name: { label: 'Jake', source: 'petname' as const }, ownerIdHex: STRANGER_HEX },
    });
    // Petname provenance: badge yes, collision mark no.
    expect(container.querySelector('.petname-badge')).toBeTruthy();
    expect(container.querySelector('[data-collision]')).toBeNull();
    expect(container.querySelector('.collision-mark')).toBeNull();
  });

  it('a colliding card name never gains the petname badge (ZEB-977 invariant)', () => {
    knownPeersState.index = indexKnowingJake();
    const { container } = render(PeerName, {
      props: { name: { label: 'Jake', source: 'card' as const }, ownerIdHex: STRANGER_HEX },
    });
    expect(container.querySelector('.petname-badge')).toBeNull();
    expect(container.querySelector('.collision-mark')).toBeTruthy();
  });
});
