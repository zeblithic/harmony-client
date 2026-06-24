// Client-side mirror of `src-tauri/src/emoji_names.rs::valid_emoji_name`:
// an emoji nickname is 1..=32 chars of [A-Za-z0-9_-]. Validating before the IPC
// lets the naming inputs give immediate, friendly feedback instead of bouncing
// the user off the backend's raw error string — and, critically, without
// closing the editor and losing the text they typed.
//
// Keep these rules in lockstep with `emoji_names.rs`; a drift either way means
// the client rejects something the backend would accept (or vice-versa).

export const MAX_EMOJI_NAME_LEN = 32;

const NAME_CHARSET = /^[A-Za-z0-9_-]+$/;

/** True iff `name` satisfies the backend's `valid_emoji_name` rule. */
export function validEmojiName(name: string): boolean {
  const n = [...name].length; // codepoint count — mirrors Rust `chars().count()`
  return n >= 1 && n <= MAX_EMOJI_NAME_LEN && NAME_CHARSET.test(name);
}

/**
 * Friendly validation message for an emoji name, or `null` when it is valid.
 * Mirrors the backend rules so the client can reject before the IPC round-trip.
 */
export function emojiNameError(name: string): string | null {
  const n = [...name].length;
  if (n === 0) return 'Enter a name.';
  if (n > MAX_EMOJI_NAME_LEN) return `Names can be at most ${MAX_EMOJI_NAME_LEN} characters.`;
  if (!NAME_CHARSET.test(name)) return 'Use only letters, numbers, _ or - (no spaces).';
  return null;
}
