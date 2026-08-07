/**
 * ZEB-594 — DOM ↔ segment helpers for the contenteditable-chips compose.
 * These take explicit nodes (not the live Selection) so the serialization
 * contract is fully unit-testable under jsdom; the Svelte component supplies the
 * live caret and applies the DOM mutations.
 */
import type { Segment } from './mention-compose';

const BLOCK_TAGS = new Set(['DIV', 'P']);

function isChip(el: Element): boolean {
  return el.nodeType === Node.ELEMENT_NODE && el.hasAttribute('data-owner-id');
}

/** Build a chip element: an atomic, non-editable inline span carrying the ownerId
 *  and showing the human `@label`. */
export function createChip(doc: Document, ownerId: string, label: string): HTMLSpanElement {
  const chip = doc.createElement('span');
  chip.className = 'mention-chip';
  chip.setAttribute('contenteditable', 'false');
  chip.setAttribute('data-owner-id', ownerId);
  chip.textContent = `@${label}`;
  return chip;
}

/** Walk a contenteditable root into compose segments. Chips → mention segments;
 *  text nodes → text; <br> and block-element boundaries → '\n'; adjacent text is
 *  coalesced so serializeSegments sees clean runs. */
export function domToSegments(root: Node): Segment[] {
  const segments: Segment[] = [];
  const pushText = (text: string) => {
    if (text === '') return;
    const last = segments[segments.length - 1];
    if (last && last.type === 'text') last.text += text;
    else segments.push({ type: 'text', text });
  };
  const walk = (node: Node) => {
    for (const child of Array.from(node.childNodes)) {
      if (child.nodeType === Node.TEXT_NODE) {
        pushText(child.textContent ?? '');
      } else if (child.nodeType === Node.ELEMENT_NODE) {
        const el = child as Element;
        if (el.tagName === 'BR') {
          pushText('\n');
        } else if (isChip(el)) {
          segments.push({ type: 'mention', ownerId: el.getAttribute('data-owner-id') ?? '' });
        } else {
          // A block element the browser wraps a soft line in starts a new line
          // before its content (except the very first content in the root).
          if (BLOCK_TAGS.has(el.tagName) && segments.length > 0) pushText('\n');
          walk(el);
        }
      }
    }
  };
  walk(root);
  return segments;
}

/** Given a collapsed caret at (node, offset), return the adjacent chip a
 *  Backspace ('backward') or Delete ('forward') should remove atomically, or null.
 *  Handles both a caret directly among the root's children and a caret at the very
 *  edge of a text node sitting next to a chip. */
export function chipToDeleteAt(
  node: Node,
  offset: number,
  direction: 'backward' | 'forward',
): HTMLElement | null {
  // Case 1: caret is positioned among an element's child nodes.
  if (node.nodeType === Node.ELEMENT_NODE) {
    const kids = node.childNodes;
    const idx = direction === 'backward' ? offset - 1 : offset;
    const cand = kids[idx];
    if (cand && cand.nodeType === Node.ELEMENT_NODE && isChip(cand as Element)) {
      return cand as HTMLElement;
    }
    return null;
  }
  // Case 2: caret at the edge of a text node adjacent to a chip.
  if (node.nodeType === Node.TEXT_NODE) {
    const text = node as Text;
    if (direction === 'backward' && offset === 0) {
      const prev = text.previousSibling;
      if (prev && prev.nodeType === Node.ELEMENT_NODE && isChip(prev as Element)) {
        return prev as HTMLElement;
      }
    }
    if (direction === 'forward' && offset === (text.textContent?.length ?? 0)) {
      const next = text.nextSibling;
      if (next && next.nodeType === Node.ELEMENT_NODE && isChip(next as Element)) {
        return next as HTMLElement;
      }
    }
    return null;
  }
  return null;
}
