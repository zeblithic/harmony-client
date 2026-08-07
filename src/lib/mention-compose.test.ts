import { describe, it, expect } from 'vitest';
import {
  detectMentionTrigger,
  filterCandidates,
  serializeSegments,
  type MentionCandidate,
} from './mention-compose';

const ID_A = 'a'.repeat(32);
const ID_B = 'b'.repeat(32);

describe('detectMentionTrigger', () => {
  it('detects a trigger at the start', () => {
    expect(detectMentionTrigger('@ja', 3)).toEqual({ query: 'ja', atIndex: 0 });
  });
  it('detects a trigger after whitespace', () => {
    expect(detectMentionTrigger('hey @ja', 7)).toEqual({ query: 'ja', atIndex: 4 });
  });
  it('returns null for an email-like @ (not at a word boundary)', () => {
    expect(detectMentionTrigger('a@b', 3)).toBeNull();
  });
  it('returns null when whitespace sits between @ and caret', () => {
    expect(detectMentionTrigger('@jo bar', 7)).toBeNull();
  });
  it('returns null when there is no @', () => {
    expect(detectMentionTrigger('hello', 5)).toBeNull();
  });
  it('uses the nearest @ and respects its boundary', () => {
    expect(detectMentionTrigger('@a@b', 4)).toBeNull();
    expect(detectMentionTrigger('@a @b', 5)).toEqual({ query: 'b', atIndex: 3 });
  });
  it('an empty query (just typed @) is a trigger', () => {
    expect(detectMentionTrigger('hi @', 4)).toEqual({ query: '', atIndex: 3 });
  });
});

describe('filterCandidates', () => {
  const cands: MentionCandidate[] = [
    { ownerId: ID_A, label: 'Jake (Koya)' },
    { ownerId: ID_B, label: 'Jasmine' },
    { ownerId: 'c'.repeat(32), label: 'Mike Jakeson' },
  ];
  it('returns all (capped) for an empty query', () => {
    expect(filterCandidates(cands, '', 2)).toHaveLength(2);
  });
  it('case-insensitive substring match', () => {
    expect(filterCandidates(cands, 'jas').map((c) => c.label)).toEqual(['Jasmine']);
  });
  it('prefix matches sort ahead of mid-string matches', () => {
    expect(filterCandidates(cands, 'jak').map((c) => c.label)).toEqual([
      'Jake (Koya)',
      'Mike Jakeson',
    ]);
  });
  it('respects the limit', () => {
    expect(filterCandidates(cands, 'ja', 1)).toHaveLength(1);
  });

  // ZEB-774: owner-id hex prefix matching, so a peer still shown as raw hex is
  // findable by the hex the user can see.
  it('matches an owner-id hex prefix when no label matches', () => {
    const hexCands: MentionCandidate[] = [
      { ownerId: '2e9a2151303c23ed8630301147057e18', label: 'UI Probe' },
      { ownerId: ID_A, label: 'Jasmine' },
    ];
    expect(filterCandidates(hexCands, '2e9a').map((c) => c.label)).toEqual(['UI Probe']);
  });

  it('ranks name matches ahead of hex-only matches', () => {
    const rankCands: MentionCandidate[] = [
      { ownerId: 'deadbeef'.repeat(4), label: 'Alice' }, // hex starts 'dea', label does not match
      { ownerId: 'f'.repeat(32), label: 'Deandra' }, // label starts 'dea'
    ];
    expect(filterCandidates(rankCands, 'dea').map((c) => c.label)).toEqual(['Deandra', 'Alice']);
  });

  it('does not double-count a candidate matching both its label and its hex', () => {
    const c: MentionCandidate[] = [{ ownerId: 'abcd'.repeat(8), label: 'abcdef' }];
    expect(filterCandidates(c, 'abc')).toHaveLength(1);
  });
});

describe('serializeSegments', () => {
  it('text-only segments pass through verbatim', () => {
    expect(serializeSegments([{ type: 'text', text: 'plain text' }])).toEqual({
      body: 'plain text',
      mentions: [],
    });
  });
  it('a mention segment becomes a <@id> token + array entry', () => {
    expect(
      serializeSegments([
        { type: 'text', text: 'hey ' },
        { type: 'mention', ownerId: ID_A },
        { type: 'text', text: ' !' },
      ]),
    ).toEqual({ body: `hey <@${ID_A}> !`, mentions: [ID_A] });
  });
  it('a mention with no surrounding text serializes alone', () => {
    expect(serializeSegments([{ type: 'mention', ownerId: ID_A }])).toEqual({
      body: `<@${ID_A}>`,
      mentions: [ID_A],
    });
  });
  it('adjacent distinct mentions preserve order in the array', () => {
    expect(
      serializeSegments([
        { type: 'mention', ownerId: ID_A },
        { type: 'text', text: ' and ' },
        { type: 'mention', ownerId: ID_B },
      ]),
    ).toEqual({ body: `<@${ID_A}> and <@${ID_B}>`, mentions: [ID_A, ID_B] });
  });
  it('dedupes a repeated id in the mentions array, first-seen order', () => {
    expect(
      serializeSegments([
        { type: 'mention', ownerId: ID_A },
        { type: 'text', text: ' ' },
        { type: 'mention', ownerId: ID_A },
      ]),
    ).toEqual({ body: `<@${ID_A}> <@${ID_A}>`, mentions: [ID_A] });
  });
  it('plain typed "@Name" text is NOT tokenized (only chips are mentions)', () => {
    expect(serializeSegments([{ type: 'text', text: '@Jake hi' }])).toEqual({
      body: '@Jake hi',
      mentions: [],
    });
  });
  it('preserves newlines inside a text segment', () => {
    expect(serializeSegments([{ type: 'text', text: 'line1\nline2' }])).toEqual({
      body: 'line1\nline2',
      mentions: [],
    });
  });
  it('empty segment list → empty body', () => {
    expect(serializeSegments([])).toEqual({ body: '', mentions: [] });
  });
});
