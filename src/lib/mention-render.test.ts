import { describe, it, expect } from 'vitest';
import { tokenizeBody, resolveMentionLabel } from './mention-render';

const ID_A = 'a'.repeat(32);
const ID_B = 'b'.repeat(32);

describe('tokenizeBody', () => {
  it('returns a single text segment when there are no tokens', () => {
    expect(tokenizeBody('hello world')).toEqual([{ type: 'text', text: 'hello world' }]);
  });

  it('returns empty array for empty string', () => {
    expect(tokenizeBody('')).toEqual([]);
  });

  it('splits a token in the middle', () => {
    expect(tokenizeBody(`hey <@${ID_A}> there`)).toEqual([
      { type: 'text', text: 'hey ' },
      { type: 'mention', ownerId: ID_A },
      { type: 'text', text: ' there' },
    ]);
  });

  it('handles a token at the start and end', () => {
    expect(tokenizeBody(`<@${ID_A}>!`)).toEqual([
      { type: 'mention', ownerId: ID_A },
      { type: 'text', text: '!' },
    ]);
    expect(tokenizeBody(`hi <@${ID_A}>`)).toEqual([
      { type: 'text', text: 'hi ' },
      { type: 'mention', ownerId: ID_A },
    ]);
  });

  it('handles adjacent tokens and multiple distinct ids', () => {
    expect(tokenizeBody(`<@${ID_A}><@${ID_B}>`)).toEqual([
      { type: 'mention', ownerId: ID_A },
      { type: 'mention', ownerId: ID_B },
    ]);
  });

  it('does not treat a malformed near-token as a mention', () => {
    const short = 'a'.repeat(31); // 31 hex → not a valid token
    expect(tokenizeBody(`x <@${short}> y`)).toEqual([{ type: 'text', text: `x <@${short}> y` }]);
  });
});

describe('resolveMentionLabel', () => {
  const nick = (id: string) => (id === ID_A ? 'NickA' : undefined);
  const card = (id: string) =>
    id === ID_A ? { displayName: 'CardA' } : id === ID_B ? { displayName: 'CardB' } : undefined;

  it('prefers local nickname', () => {
    expect(resolveMentionLabel(ID_A, nick, card)).toBe('NickA');
  });

  it('falls back to broadcast displayName', () => {
    expect(resolveMentionLabel(ID_B, nick, card)).toBe('CardB');
  });

  it('falls back to short hex when nothing resolves', () => {
    expect(resolveMentionLabel(ID_A, undefined, undefined)).toBe('aaaaaaaa');
  });

  it('treats empty/whitespace nickname or name as absent', () => {
    expect(resolveMentionLabel(ID_A, () => '  ', () => ({ displayName: '' }))).toBe('aaaaaaaa');
    expect(resolveMentionLabel(ID_A, () => '   ', () => ({ displayName: 'Real' }))).toBe('Real');
  });
});
