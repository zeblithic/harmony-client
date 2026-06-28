import { describe, it, expect } from 'vitest';
import {
  detectMentionTrigger,
  applyMentionPick,
  filterCandidates,
  reconcileCompose,
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

describe('applyMentionPick', () => {
  it('replaces the @query range with "@label " and tracks the id', () => {
    const r = applyMentionPick('hey @ja', 4, 7, { ownerId: ID_A, label: 'Jake (Koya)' });
    expect(r.text).toBe('hey @Jake (Koya) ');
    expect(r.caret).toBe('hey @Jake (Koya) '.length);
    expect(r.tracked).toEqual({ ownerId: ID_A, label: 'Jake (Koya)' });
  });
  it('keeps trailing text after the caret', () => {
    const r = applyMentionPick('@j end', 0, 2, { ownerId: ID_A, label: 'Jay' });
    expect(r.text).toBe('@Jay  end');
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
});

describe('reconcileCompose', () => {
  it('no tracked mentions → body unchanged, empty mentions', () => {
    expect(reconcileCompose('plain text', [])).toEqual({ body: 'plain text', mentions: [] });
  });
  it('rewrites a tracked mention to a token + array', () => {
    expect(
      reconcileCompose('hey @Jake (Koya) !', [{ ownerId: ID_A, label: 'Jake (Koya)' }]),
    ).toEqual({ body: `hey <@${ID_A}> !`, mentions: [ID_A] });
  });
  it('drops a pick whose label was edited away (degrades to text)', () => {
    expect(reconcileCompose('hey @Jak !', [{ ownerId: ID_A, label: 'Jake' }])).toEqual({
      body: 'hey @Jak !',
      mentions: [],
    });
  });
  it('does NOT tokenize a pick extended at the right edge (@JakeX)', () => {
    // Right-boundary guard: appending chars to a picked label must not corrupt
    // the body into "<@id>X" (Qodo bug / CodeRabbit).
    expect(reconcileCompose('@JakeX', [{ ownerId: ID_A, label: 'Jake' }])).toEqual({
      body: '@JakeX',
      mentions: [],
    });
    expect(reconcileCompose('@Jake2 hi', [{ ownerId: ID_A, label: 'Jake' }])).toEqual({
      body: '@Jake2 hi',
      mentions: [],
    });
  });
  it('does NOT tokenize a label merged into a word/email (left boundary)', () => {
    expect(reconcileCompose('mail a@Jake', [{ ownerId: ID_A, label: 'Jake' }])).toEqual({
      body: 'mail a@Jake',
      mentions: [],
    });
  });
  it('tokenizes a mention followed immediately by whitespace or end', () => {
    expect(reconcileCompose('@Jake', [{ ownerId: ID_A, label: 'Jake' }])).toEqual({
      body: `<@${ID_A}>`,
      mentions: [ID_A],
    });
    expect(reconcileCompose('@Jake\n', [{ ownerId: ID_A, label: 'Jake' }])).toEqual({
      body: `<@${ID_A}>\n`,
      mentions: [ID_A],
    });
  });
  it('longest label wins over a prefix label', () => {
    const tracked = [
      { ownerId: ID_A, label: 'Jake' },
      { ownerId: ID_B, label: 'Jake (Koya)' },
    ];
    expect(reconcileCompose('@Jake (Koya)', tracked)).toEqual({
      body: `<@${ID_B}>`,
      mentions: [ID_B],
    });
  });
  it('two same-label distinct ids map left-to-right', () => {
    const tracked = [
      { ownerId: ID_A, label: 'Jake' },
      { ownerId: ID_B, label: 'Jake' },
    ];
    expect(reconcileCompose('@Jake and @Jake', tracked)).toEqual({
      body: `<@${ID_A}> and <@${ID_B}>`,
      mentions: [ID_A, ID_B],
    });
  });
  it('dedupes a repeated same id in the mentions array', () => {
    const tracked = [
      { ownerId: ID_A, label: 'Jake' },
      { ownerId: ID_A, label: 'Jake' },
    ];
    expect(reconcileCompose('@Jake @Jake', tracked)).toEqual({
      body: `<@${ID_A}> <@${ID_A}>`,
      mentions: [ID_A],
    });
  });
});
