import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import MismatchDisplay from '../MismatchDisplay.svelte';

describe('MismatchDisplay', () => {
  function getLines(): string[] {
    const pre = screen.getByLabelText(/mismatch feedback/i);
    return pre.textContent?.split('\n') ?? [];
  }

  it('renders the flashcard-design.md spec example with caret under the final syllable', () => {
    // expected KEVO 'O'I JU'E, heard KEVO 'O'I JU'O — diff at nibble index 5
    // (low nibble of the 3rd byte). Caret should land under 'O/'E (the 2
    // chars rendered at the byte's 2nd syllable position).
    render(MismatchDisplay, {
      props: {
        expectedBytes: [0xac, 0x03, 0x52],
        heardNibbles: [0xa, 0xc, 0x0, 0x3, 0x5, 0x0],
        firstDiffNibbleIdx: 5,
      },
    });
    const [expected, heard, caret] = getLines();
    expect(expected).toBe("Expected: KEVO 'O'I JU'E");
    expect(heard).toBe("Heard:    KEVO 'O'I JU'O");
    // 10 (label) + 2*5 (first 2 bytes + separators) + 1*2 (skip high syllable of byte 3) = 22
    expect(caret).toBe(' '.repeat(22) + '^^');
  });

  it('places caret at the first column of the data region for a high-nibble diff on byte 0', () => {
    render(MismatchDisplay, {
      props: {
        expectedBytes: [0x92],
        heardNibbles: [0xa, 0x2],
        firstDiffNibbleIdx: 0,
      },
    });
    const [, , caret] = getLines();
    expect(caret).toBe(' '.repeat(10) + '^^');
  });

  it('offsets caret by 2 for a low-nibble diff within the first byte', () => {
    render(MismatchDisplay, {
      props: {
        expectedBytes: [0x92],
        heardNibbles: [0x9, 0x3],
        firstDiffNibbleIdx: 1,
      },
    });
    const [, , caret] = getLines();
    expect(caret).toBe(' '.repeat(12) + '^^');
  });

  it('renders ?? for unpaired trailing heard nibble', () => {
    render(MismatchDisplay, {
      props: {
        expectedBytes: [0x92, 0xa8],
        heardNibbles: [0x9, 0x2, 0xa],
        firstDiffNibbleIdx: 3,
      },
    });
    const [, heard] = getLines();
    expect(heard).toBe("Heard:    KU'E KE??");
  });

  it('omits the caret line when firstDiffNibbleIdx is -1 (no divergence)', () => {
    // Regression for Qodo/CodeRabbit/CodeAnt PR #49 finding: the template
    // always rendered `{caretPrefix}^^`, which meant a caret would appear
    // at column 0 when caretPrefix is empty. In the component's intended
    // usage FlashcardView gates rendering on a non-null mismatch, but the
    // component shouldn't lie about a mismatch that doesn't exist.
    render(MismatchDisplay, {
      props: {
        expectedBytes: [0x92],
        heardNibbles: [0x9, 0x2],
        firstDiffNibbleIdx: -1,
      },
    });
    const lines = getLines();
    expect(lines).toHaveLength(2);
    expect(lines[0]).toBe("Expected: KU'E");
    expect(lines[1]).toBe("Heard:    KU'E");
  });

  it('exposes role=status with aria-live=polite for screen readers', () => {
    render(MismatchDisplay, {
      props: {
        expectedBytes: [0x92],
        heardNibbles: [0xa, 0x2],
        firstDiffNibbleIdx: 0,
      },
    });
    const status = screen.getByRole('status');
    expect(status.getAttribute('aria-live')).toBe('polite');
  });
});
