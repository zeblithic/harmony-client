import { render, cleanup } from '@testing-library/svelte';
import { describe, it, expect, afterEach } from 'vitest';
import MediaCard from '../MediaCard.svelte';

const PEER = 'ef'.repeat(16);
const dmMessage = {
  id: 'm1',
  sender: { address: PEER, displayName: '' }, // DM sender: name resolves live
  text: '',
  timestamp: 1_700_000_000_000,
  media: [],
  priority: 'standard',
};
const attachment = { id: 'a1', type: 'image', url: 'blob:x' };

// ZEB-962: media cards render a DM sender whose baked name is blank; resolve
// through the author ladder rather than showing an empty `.card-sender`.
describe('MediaCard sender name resolution (ZEB-962)', () => {
  afterEach(() => cleanup());

  it('resolves a blank DM sender through the card', () => {
    const { container } = render(MediaCard, {
      props: {
        message: dmMessage,
        attachment,
        resolveCard: (id: string) => (id === PEER ? { displayName: 'Zeb', statusText: '' } : undefined),
      } as any,
    });
    expect(container.querySelector('.card-sender')?.textContent).toBe('Zeb');
  });

  it('falls back to short hex when no resolver is provided', () => {
    const { container } = render(MediaCard, {
      props: { message: dmMessage, attachment } as any,
    });
    expect(container.querySelector('.card-sender')?.textContent).toBe(PEER.slice(0, 8));
  });
});
