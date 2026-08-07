/**
 * ZEB-594 — DOM ↔ segment helpers for the contenteditable-chips compose.
 * These take explicit nodes (not the live Selection) so the serialization
 * contract is fully unit-testable under jsdom; the Svelte component supplies the
 * live caret and applies the DOM mutations.
 */
import type { Segment } from './mention-compose';

const BLOCK_TAGS = new Set(['DIV', 'P']);

/** A valid owner-id is 32 lowercase hex — the same shape the frozen render side
 *  parses (`tokenizeBody`'s /<@([0-9a-f]{32})>/g). A chip whose data-owner-id
 *  fails this is NOT serialized as a mention, so a malformed/empty attribute can
 *  never emit a `<@>` token or a bad mentions[] entry. */
const OWNER_ID_RE = /^[0-9a-f]{32}$/;

/** The generated separator inserted after a picked chip (a normal space; the
 *  editable is white-space: pre-wrap so it survives at end-of-line). */
const SEPARATOR = ' ';

function isChip(el: Element): boolean {
  return el.nodeType === Node.ELEMENT_NODE && el.hasAttribute('data-owner-id');
}

function isValidChip(el: Element): boolean {
  return isChip(el) && OWNER_ID_RE.test(el.getAttribute('data-owner-id') ?? '');
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
        } else if (isValidChip(el)) {
          segments.push({ type: 'mention', ownerId: el.getAttribute('data-owner-id') as string });
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

/** The sibling immediately on `direction`'s side, walking out of collapsed
 *  ancestor edges (bounded by `root`) — so a chip that is a sibling of a
 *  browser-generated block wrapper (`<div>line</div>`), not of the caret's text
 *  node, is still reachable. */
function siblingAcrossAncestors(
  node: Node,
  direction: 'backward' | 'forward',
  root: Node,
): Node | null {
  let cur: Node | null = node;
  while (cur && cur !== root) {
    const sib = direction === 'backward' ? cur.previousSibling : cur.nextSibling;
    if (sib) return sib;
    cur = cur.parentNode;
  }
  return null;
}

/** The DOM node immediately on `direction`'s side of a collapsed caret at
 *  (node, offset), descending into element children or ascending out of
 *  collapsed edges as needed. Returns null when the caret sits in the middle of a
 *  text node (an ordinary character deletion, not a chip boundary). */
function nodeAtCaretEdge(
  node: Node,
  offset: number,
  direction: 'backward' | 'forward',
  root: Node,
): Node | null {
  if (node.nodeType === Node.ELEMENT_NODE) {
    const idx = direction === 'backward' ? offset - 1 : offset;
    const child = node.childNodes[idx];
    return child ?? siblingAcrossAncestors(node, direction, root);
  }
  if (node.nodeType === Node.TEXT_NODE) {
    const len = node.textContent?.length ?? 0;
    if (direction === 'backward' && offset === 0) {
      return node.previousSibling ?? siblingAcrossAncestors(node, 'backward', root);
    }
    if (direction === 'forward' && offset === len) {
      return node.nextSibling ?? siblingAcrossAncestors(node, 'forward', root);
    }
    return null; // caret mid-text → ordinary character deletion
  }
  return null;
}

/** Given a collapsed caret at (node, offset), return the adjacent chip a
 *  Backspace ('backward') or Delete ('forward') should remove atomically, or null.
 *  Looks past a single generated separator space (so the first Backspace after a
 *  pick removes the chip, not the space) and out of block wrappers (bounded by
 *  `root` — the contenteditable element). */
export function chipToDeleteAt(
  node: Node,
  offset: number,
  direction: 'backward' | 'forward',
  root: Node,
): HTMLElement | null {
  const edge = nodeAtCaretEdge(node, offset, direction, root);
  if (!edge) return null;
  if (edge.nodeType === Node.ELEMENT_NODE && isChip(edge as Element)) {
    return edge as HTMLElement;
  }
  // Look past a lone generated separator to the chip beyond it.
  if (edge.nodeType === Node.TEXT_NODE && (edge.textContent ?? '') === SEPARATOR) {
    const beyond = direction === 'backward' ? edge.previousSibling : edge.nextSibling;
    if (beyond && beyond.nodeType === Node.ELEMENT_NODE && isChip(beyond as Element)) {
      return beyond as HTMLElement;
    }
  }
  return null;
}
