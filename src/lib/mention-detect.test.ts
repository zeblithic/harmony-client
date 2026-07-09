import { describe, it, expect } from 'vitest';
import { messageMentionsOwner } from './mention-detect';

const ME = 'aa'.repeat(16); // 32-hex owner id
const OTHER = 'bb'.repeat(16);

describe('messageMentionsOwner', () => {
  it('true when mentions includes me', () => {
    expect(messageMentionsOwner({ mentions: [OTHER, ME] }, ME)).toBe(true);
  });
  it('false when mentions includes only others', () => {
    expect(messageMentionsOwner({ mentions: [OTHER] }, ME)).toBe(false);
  });
  it('false when mentions is absent', () => {
    expect(messageMentionsOwner({}, ME)).toBe(false);
  });
  it('false when mentions is empty', () => {
    expect(messageMentionsOwner({ mentions: [] }, ME)).toBe(false);
  });
  it('false when selfOwnerId is empty', () => {
    expect(messageMentionsOwner({ mentions: [ME] }, '')).toBe(false);
  });
});
