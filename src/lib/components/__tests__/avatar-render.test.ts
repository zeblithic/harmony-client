import { describe, it, expect, vi } from 'vitest';
import { render } from '@testing-library/svelte';
import MemberRow from '../MemberRow.svelte';

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
