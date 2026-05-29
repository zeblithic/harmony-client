import { describe, it, expect, afterEach } from 'vitest';
import { trapFocus } from '../focus-trap';

// Build: <body> <div.background><button#bg> </div> <div.backdrop><div.dialog>
// <button#a><button#b></div></div> </body>, matching the modal DOM shape the
// trap expects (dialog.parentElement === backdrop; backdrop's siblings are the
// background to inert).
function buildDom() {
  document.body.innerHTML = '';
  const background = document.createElement('div');
  background.className = 'background';
  const bgBtn = document.createElement('button');
  bgBtn.id = 'bg';
  background.appendChild(bgBtn);

  const backdrop = document.createElement('div');
  backdrop.className = 'backdrop';
  const dialog = document.createElement('div');
  dialog.tabIndex = -1;
  const a = document.createElement('button');
  a.id = 'a';
  const b = document.createElement('button');
  b.id = 'b';
  dialog.append(a, b);
  backdrop.appendChild(dialog);

  document.body.append(background, backdrop);
  return { background, backdrop, dialog, bgBtn, a, b };
}

afterEach(() => {
  document.body.innerHTML = '';
});

describe('trapFocus', () => {
  it('moves initial focus into the dialog and inerts background siblings', () => {
    const { background, dialog, a } = buildDom();
    const cleanup = trapFocus(dialog);
    expect(dialog.contains(document.activeElement)).toBe(true);
    expect(document.activeElement).toBe(a);
    expect(background.hasAttribute('inert')).toBe(true);
    cleanup();
  });

  it('cleanup clears inert and restores the previously focused element', () => {
    const { background, dialog, bgBtn } = buildDom();
    bgBtn.focus();
    expect(document.activeElement).toBe(bgBtn);
    const cleanup = trapFocus(dialog);
    expect(background.hasAttribute('inert')).toBe(true);
    cleanup();
    expect(background.hasAttribute('inert')).toBe(false);
    expect(document.activeElement).toBe(bgBtn);
  });

  it('wraps Tab from the last focusable back to the first', () => {
    const { dialog, a, b } = buildDom();
    const cleanup = trapFocus(dialog);
    b.focus();
    const ev = new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true });
    dialog.dispatchEvent(ev);
    expect(ev.defaultPrevented).toBe(true);
    expect(document.activeElement).toBe(a);
    cleanup();
  });

  it('wraps Shift+Tab from the first focusable back to the last', () => {
    const { dialog, a, b } = buildDom();
    const cleanup = trapFocus(dialog);
    a.focus();
    const ev = new KeyboardEvent('keydown', { key: 'Tab', shiftKey: true, bubbles: true, cancelable: true });
    dialog.dispatchEvent(ev);
    expect(ev.defaultPrevented).toBe(true);
    expect(document.activeElement).toBe(b);
    cleanup();
  });
});
