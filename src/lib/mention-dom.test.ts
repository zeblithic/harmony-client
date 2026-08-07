import { describe, it, expect } from 'vitest';
import { domToSegments, createChip, chipToDeleteAt } from './mention-dom';

const ID_A = 'a'.repeat(32);
const ID_B = 'b'.repeat(32);

/** Build an editable root from an HTML string for walk tests. */
function root(html: string): HTMLDivElement {
  const div = document.createElement('div');
  div.innerHTML = html;
  return div;
}

describe('createChip', () => {
  it('builds a non-editable chip carrying the ownerId and @label text', () => {
    const chip = createChip(document, ID_A, 'Jake (Koya)');
    expect(chip.tagName).toBe('SPAN');
    expect(chip.getAttribute('contenteditable')).toBe('false');
    expect(chip.getAttribute('data-owner-id')).toBe(ID_A);
    expect(chip.classList.contains('mention-chip')).toBe(true);
    expect(chip.textContent).toBe('@Jake (Koya)');
  });
});

describe('domToSegments', () => {
  it('plain text → one text segment', () => {
    expect(domToSegments(root('hello world'))).toEqual([{ type: 'text', text: 'hello world' }]);
  });
  it('empty root → empty segments', () => {
    expect(domToSegments(root(''))).toEqual([]);
  });
  it('text + chip + text → interleaved segments', () => {
    const div = root('hey ');
    div.appendChild(createChip(document, ID_A, 'Jake'));
    div.appendChild(document.createTextNode(' there'));
    expect(domToSegments(div)).toEqual([
      { type: 'text', text: 'hey ' },
      { type: 'mention', ownerId: ID_A },
      { type: 'text', text: ' there' },
    ]);
  });
  it('a chip alone → one mention segment', () => {
    const div = root('');
    div.appendChild(createChip(document, ID_A, 'Jake'));
    expect(domToSegments(div)).toEqual([{ type: 'mention', ownerId: ID_A }]);
  });
  it('two adjacent chips → two mention segments in order', () => {
    const div = root('');
    div.appendChild(createChip(document, ID_A, 'Jake'));
    div.appendChild(createChip(document, ID_B, 'Bob'));
    expect(domToSegments(div)).toEqual([
      { type: 'mention', ownerId: ID_A },
      { type: 'mention', ownerId: ID_B },
    ]);
  });
  it('<br> becomes a newline in the text stream', () => {
    expect(domToSegments(root('a<br>b'))).toEqual([{ type: 'text', text: 'a\nb' }]);
  });
  it('block-wrapped lines (browser Shift+Enter) become newlines', () => {
    expect(domToSegments(root('line1<div>line2</div>'))).toEqual([
      { type: 'text', text: 'line1\nline2' },
    ]);
  });
  it('coalesces adjacent text nodes into one segment', () => {
    const div = root('');
    div.appendChild(document.createTextNode('a'));
    div.appendChild(document.createTextNode('b'));
    expect(domToSegments(div)).toEqual([{ type: 'text', text: 'ab' }]);
  });
});

describe('chipToDeleteAt', () => {
  it('Backspace with the caret right after a chip returns that chip', () => {
    const div = root('');
    const chip = createChip(document, ID_A, 'Jake');
    div.appendChild(chip);
    // caret at (div, 1) = immediately after child 0 (the chip)
    expect(chipToDeleteAt(div, 1, 'backward')).toBe(chip);
  });
  it('Delete with the caret right before a chip returns that chip', () => {
    const div = root('');
    const chip = createChip(document, ID_A, 'Jake');
    div.appendChild(chip);
    expect(chipToDeleteAt(div, 0, 'forward')).toBe(chip);
  });
  it('Backspace at offset 0 inside a text node preceded by a chip returns the chip', () => {
    const div = root('');
    const chip = createChip(document, ID_A, 'Jake');
    div.appendChild(chip);
    const text = document.createTextNode('x');
    div.appendChild(text);
    expect(chipToDeleteAt(text, 0, 'backward')).toBe(chip);
  });
  it('returns null when there is no adjacent chip', () => {
    const div = root('hello');
    expect(chipToDeleteAt(div.firstChild!, 3, 'backward')).toBeNull();
  });
});
