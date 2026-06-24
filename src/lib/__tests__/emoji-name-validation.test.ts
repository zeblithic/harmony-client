import { describe, it, expect } from 'vitest';
import { validEmojiName, emojiNameError, MAX_EMOJI_NAME_LEN } from '../emoji-name-validation';

describe('emoji-name-validation (mirrors emoji_names.rs::valid_emoji_name)', () => {
  it('accepts 1..=32 chars of [A-Za-z0-9_-]', () => {
    expect(validEmojiName('catjam')).toBe(true);
    expect(validEmojiName('cat_jam-2')).toBe(true);
    expect(validEmojiName('x')).toBe(true);
    expect(validEmojiName('x'.repeat(MAX_EMOJI_NAME_LEN))).toBe(true);
  });

  it('rejects empty, over-length, spaces, punctuation and non-ASCII', () => {
    expect(validEmojiName('')).toBe(false);
    expect(validEmojiName('x'.repeat(MAX_EMOJI_NAME_LEN + 1))).toBe(false);
    expect(validEmojiName('cat jam')).toBe(false); // the headline finding-10 case
    expect(validEmojiName('cat!')).toBe(false);
    expect(validEmojiName('café')).toBe(false);
  });

  it('returns a friendly, specific message for each failure mode and null when valid', () => {
    expect(emojiNameError('')).toMatch(/enter a name/i);
    expect(emojiNameError('x'.repeat(MAX_EMOJI_NAME_LEN + 1))).toMatch(/at most 32/i);
    expect(emojiNameError('cat jam')).toMatch(/letters, numbers/i);
    expect(emojiNameError('valid_name')).toBeNull();
  });

  it('treats the 32-char boundary inclusively, 33 exclusively', () => {
    expect(emojiNameError('a'.repeat(MAX_EMOJI_NAME_LEN))).toBeNull();
    expect(emojiNameError('a'.repeat(MAX_EMOJI_NAME_LEN + 1))).not.toBeNull();
  });
});
