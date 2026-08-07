import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import MentionInput from '../MentionInput.svelte';
import { createChip } from '../../mention-dom';

/** Place a collapsed caret at (node, offset) so the component reads it via getSelection. */
function setCaret(node: Node, offset: number) {
  const range = document.createRange();
  range.setStart(node, offset);
  range.collapse(true);
  const sel = window.getSelection()!;
  sel.removeAllRanges();
  sel.addRange(range);
}

const ID_A = 'a'.repeat(32);

function mount(overrides: Record<string, unknown> = {}) {
  const onSend = vi.fn();
  const { container } = render(MentionInput, {
    props: {
      candidates: [{ ownerId: ID_A, label: 'Jake' }],
      placeholder: 'Message #general',
      ariaLabel: 'Channel message',
      disabled: false,
      onSend,
      ...overrides,
    },
  });
  const editable = container.querySelector('[role="textbox"]') as HTMLElement;
  return { editable, onSend, container };
}

function keydown(el: HTMLElement, init: KeyboardEventInit & { isComposing?: boolean }) {
  const ev = new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init });
  if (init.isComposing) Object.defineProperty(ev, 'isComposing', { get: () => true });
  el.dispatchEvent(ev);
  return ev;
}

describe('MentionInput', () => {
  it('renders a labelled multiline textbox with a placeholder', () => {
    const { editable } = mount();
    expect(editable.getAttribute('role')).toBe('textbox');
    expect(editable.getAttribute('aria-multiline')).toBe('true');
    expect(editable.getAttribute('aria-label')).toBe('Channel message');
    expect(editable.getAttribute('data-placeholder')).toBe('Message #general');
    expect(editable.getAttribute('contenteditable')).toBe('true');
  });

  it('Enter serializes the DOM and calls onSend with body + mentions', () => {
    const { editable, onSend } = mount();
    editable.appendChild(document.createTextNode('hey '));
    editable.appendChild(createChip(document, ID_A, 'Jake'));
    keydown(editable, { key: 'Enter' });
    expect(onSend).toHaveBeenCalledWith({ body: `hey <@${ID_A}>`, mentions: [ID_A] });
  });

  it('Enter on a plain message sends no mentions', () => {
    const { editable, onSend } = mount();
    editable.appendChild(document.createTextNode('just text'));
    keydown(editable, { key: 'Enter' });
    expect(onSend).toHaveBeenCalledWith({ body: 'just text', mentions: [] });
  });

  it('Enter on an empty input still fires onSend (parent guards emptiness)', () => {
    const { editable, onSend } = mount();
    keydown(editable, { key: 'Enter' });
    expect(onSend).toHaveBeenCalledWith({ body: '', mentions: [] });
  });

  it('Shift+Enter does NOT send (newline falls through to the browser)', () => {
    const { editable, onSend } = mount();
    editable.appendChild(document.createTextNode('line'));
    const ev = keydown(editable, { key: 'Enter', shiftKey: true });
    expect(onSend).not.toHaveBeenCalled();
    expect(ev.defaultPrevented).toBe(false);
  });

  it('Enter during IME composition does NOT send', () => {
    const { editable, onSend } = mount();
    editable.appendChild(document.createTextNode('かな'));
    keydown(editable, { key: 'Enter', isComposing: true });
    expect(onSend).not.toHaveBeenCalled();
  });

  it('disabled makes the surface non-editable and marks it aria-disabled', () => {
    const { editable } = mount({ disabled: true });
    expect(editable.getAttribute('contenteditable')).toBe('false');
    expect(editable.getAttribute('aria-disabled')).toBe('true');
  });

  it('IME Enter with the autocomplete open neither picks a candidate nor sends', async () => {
    const { editable, onSend, container } = mount();
    editable.appendChild(document.createTextNode('@Ja'));
    setCaret(editable.firstChild!, 3);
    await fireEvent.input(editable);
    await waitFor(() =>
      expect(container.querySelector('[data-testid="mention-autocomplete"]')).toBeTruthy(),
    );
    // The IME guard runs before the autocomplete branch, so a composing Enter is
    // ignored — no pick, no send, trigger text intact.
    keydown(editable, { key: 'Enter', isComposing: true });
    expect(onSend).not.toHaveBeenCalled();
    expect(editable.querySelector('.mention-chip')).toBeNull();
    expect(editable.textContent).toBe('@Ja');
  });

  it('one Backspace with the caret after a [chip][separator] deletes both atomically', () => {
    // The state pick() leaves behind: a chip followed by its generated separator
    // space, caret after the space. (The full pick→chip flow is covered in the
    // ChannelMessageFeed integration test; here we isolate the delete behavior
    // from jsdom's deleteContents empty-text-node artifacts.)
    const { editable } = mount();
    const chip = createChip(document, 'a'.repeat(32), 'Jake');
    const sep = document.createTextNode(' ');
    editable.appendChild(chip);
    editable.appendChild(sep);
    setCaret(editable, 2); // after child 1 (the separator)
    keydown(editable, { key: 'Backspace' });
    expect(editable.querySelector('.mention-chip')).toBeNull();
    expect(editable.contains(sep)).toBe(false);
  });
});
