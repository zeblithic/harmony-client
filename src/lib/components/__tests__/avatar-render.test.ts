import { describe, it, expect, vi } from 'vitest';
import { render } from '@testing-library/svelte';
import MemberRow from '../MemberRow.svelte';
import Avatar from '../Avatar.svelte';
import { shortId } from '../../short-addr';

describe('MemberRow avatar', () => {
  it('renders the resolved avatar image when avatarUrl is present', () => {
    const member = { address: 'aa'.repeat(20), displayName: 'Ann', power: 0, status: 'joined' };
    const resolveCard = vi.fn(() => ({ displayName: 'Ann', statusText: 'hi', avatarUrl: 'blob:pic' }));
    const { container } = render(MemberRow, {
      props: {
        member,
        resolveCard,
        viewer: { addr: 'bb'.repeat(20), power: 0, isLastAdmin: false },
      } as any,
    });
    const img = container.querySelector('img');
    expect(img?.getAttribute('src')).toBe('blob:pic');
  });
});

// ZEB-962: Avatar is the shared sink for `alt`/`title`. It must never render a
// blank accessible label — a peer can broadcast a whitespace-only name, and an
// unguarded `alt=""` on the image is an a11y hole. Guard centrally: fall back to
// `shortId(address)` (the identity floor; `address` is a required prop).
describe('Avatar accessible-label guard (ZEB-962)', () => {
  const ADDR = '12'.repeat(16);

  it('uses a non-blank displayName for title and alt', () => {
    const { container } = render(Avatar, {
      props: { address: ADDR, displayName: 'Ann', avatarUrl: 'blob:pic' } as any,
    });
    expect(container.querySelector('.avatar')?.getAttribute('title')).toBe('Ann');
    expect(container.querySelector('img')?.getAttribute('alt')).toBe('Ann');
  });

  it('falls back to shortId(address) when displayName is whitespace-only', () => {
    const { container } = render(Avatar, {
      props: { address: ADDR, displayName: '   ', avatarUrl: 'blob:pic' } as any,
    });
    expect(container.querySelector('.avatar')?.getAttribute('title')).toBe(shortId(ADDR));
    expect(container.querySelector('img')?.getAttribute('alt')).toBe(shortId(ADDR));
  });

  it('falls back to shortId(address) when displayName is omitted', () => {
    const { container } = render(Avatar, {
      props: { address: ADDR, avatarUrl: 'blob:pic' } as any,
    });
    expect(container.querySelector('.avatar')?.getAttribute('title')).toBe(shortId(ADDR));
  });
});
