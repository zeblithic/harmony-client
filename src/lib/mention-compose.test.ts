import { describe, it, expect } from 'vitest';
import {
  detectMentionTrigger,
  applyMentionPick,
  filterCandidates,
  reconcileCompose,
  shiftTrackedSpans,
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
    expect(r.tracked).toEqual({ ownerId: ID_A, label: 'Jake (Koya)', start: 4 });
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
      reconcileCompose('hey @Jake (Koya) !', [{ ownerId: ID_A, label: 'Jake (Koya)', start: 4 }]),
    ).toEqual({ body: `hey <@${ID_A}> !`, mentions: [ID_A] });
  });
  it('drops a pick whose label was edited away (degrades to text)', () => {
    expect(
      reconcileCompose('hey @Jak !', [{ ownerId: ID_A, label: 'Jake', start: 4 }]),
    ).toEqual({ body: 'hey @Jak !', mentions: [] });
  });
  it('does NOT tokenize a pick extended at the right edge (@JakeX)', () => {
    // Right-boundary guard: appending chars to a picked label must not corrupt
    // the body into "<@id>X" (Qodo bug / CodeRabbit).
    expect(
      reconcileCompose('@JakeX', [{ ownerId: ID_A, label: 'Jake', start: 0 }]),
    ).toEqual({ body: '@JakeX', mentions: [] });
    expect(
      reconcileCompose('@Jake2 hi', [{ ownerId: ID_A, label: 'Jake', start: 0 }]),
    ).toEqual({ body: '@Jake2 hi', mentions: [] });
  });
  it('does NOT tokenize a label merged into a word/email (left boundary)', () => {
    expect(
      reconcileCompose('mail a@Jake', [{ ownerId: ID_A, label: 'Jake', start: 6 }]),
    ).toEqual({ body: 'mail a@Jake', mentions: [] });
  });
  it('tokenizes a mention followed immediately by whitespace or end', () => {
    expect(
      reconcileCompose('@Jake', [{ ownerId: ID_A, label: 'Jake', start: 0 }]),
    ).toEqual({ body: `<@${ID_A}>`, mentions: [ID_A] });
    expect(
      reconcileCompose('@Jake\n', [{ ownerId: ID_A, label: 'Jake', start: 0 }]),
    ).toEqual({ body: `<@${ID_A}>\n`, mentions: [ID_A] });
  });
  it('longest label wins over a prefix label', () => {
    const tracked = [
      { ownerId: ID_A, label: 'Jake', start: 0 },
      { ownerId: ID_B, label: 'Jake (Koya)', start: 0 },
    ];
    expect(reconcileCompose('@Jake (Koya)', tracked)).toEqual({
      body: `<@${ID_B}>`,
      mentions: [ID_B],
    });
  });
  it('two same-label distinct ids map left-to-right', () => {
    const tracked = [
      { ownerId: ID_A, label: 'Jake', start: 0 },
      { ownerId: ID_B, label: 'Jake', start: 10 },
    ];
    expect(reconcileCompose('@Jake and @Jake', tracked)).toEqual({
      body: `<@${ID_A}> and <@${ID_B}>`,
      mentions: [ID_A, ID_B],
    });
  });
  it('dedupes a repeated same id in the mentions array', () => {
    const tracked = [
      { ownerId: ID_A, label: 'Jake', start: 0 },
      { ownerId: ID_A, label: 'Jake', start: 6 },
    ];
    expect(reconcileCompose('@Jake @Jake', tracked)).toEqual({
      body: `<@${ID_A}> <@${ID_A}>`,
      mentions: [ID_A],
    });
  });
});

describe('shiftTrackedSpans', () => {
  const span = (start: number, label: string, ownerId = ID_A) => ({ ownerId, label, start });

  it('shifts a span when text is inserted before it', () => {
    // 'hi @Jake' → 'yo hi @Jake' : '@Jake' moves from offset 3 to 6.
    expect(shiftTrackedSpans('hi @Jake', 'yo hi @Jake', [span(3, 'Jake')])).toEqual([
      span(6, 'Jake'),
    ]);
  });
  it('keeps a span unchanged when text is appended after it', () => {
    // 'Jake' span ends exactly at the edit point (p == end) → kept.
    expect(shiftTrackedSpans('@Jake', '@Jake!!', [span(0, 'Jake')])).toEqual([span(0, 'Jake')]);
  });
  it('drops a span whose whole text was deleted', () => {
    expect(shiftTrackedSpans('hi @Jake !', 'hi  !', [span(3, 'Jake')])).toEqual([]);
  });
  it('drops a span edited in the middle', () => {
    expect(shiftTrackedSpans('@Jake', '@JXke', [span(0, 'Jake')])).toEqual([]);
  });
  it('delete-then-retype regression: deletion drops the pick, retype is plain text', () => {
    // Headline bug: the span is invalidated on delete; a later identical retype
    // has no tracked entry, so reconcile emits no id.
    const afterDelete = shiftTrackedSpans('@Jake ', '', [span(0, 'Jake')]);
    expect(afterDelete).toEqual([]);
    expect(reconcileCompose('@Jake ', afterDelete)).toEqual({ body: '@Jake ', mentions: [] });
  });
  it('with two mentions, an edit between them keeps the earlier and shifts the later', () => {
    // '@Al @Bob' → '@AlXX @Bob' : 'Al' stays at 0, 'Bob' shifts 4 → 6.
    const tracked = [span(0, 'Al'), span(4, 'Bob', ID_B)];
    expect(shiftTrackedSpans('@Al @Bob', '@AlXX @Bob', tracked)).toEqual([
      span(0, 'Al'),
      span(6, 'Bob', ID_B),
    ]);
  });
  it('drops a span when a paste-over-selection covers it, keeping the survivor', () => {
    // Select '@Bob' (offsets 4..7) and paste 'ZZZ' → 'Bob' invalidated, 'Al' kept.
    const tracked = [span(0, 'Al'), span(4, 'Bob', ID_B)];
    expect(shiftTrackedSpans('@Al @Bob end', '@Al ZZZ end', tracked)).toEqual([span(0, 'Al')]);
  });
  it('returns the spans unchanged on a no-op edit', () => {
    expect(shiftTrackedSpans('@Jake', '@Jake', [span(0, 'Jake')])).toEqual([span(0, 'Jake')]);
  });
});
