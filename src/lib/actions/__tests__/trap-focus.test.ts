import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { trapFocus } from '../trap-focus';

describe('trap-focus action', () => {
  let cleanup: { destroy(): void } | undefined;

  beforeEach(() => {
    document.body.innerHTML = '';
  });

  afterEach(() => {
    cleanup?.destroy();
    cleanup = undefined;
    document.body.innerHTML = '';
  });

  it('focuses the first focusable element on mount', () => {
    document.body.innerHTML = `
      <button id="trigger">Open</button>
      <div id="modal">
        <button id="b1">B1</button>
        <button id="b2">B2</button>
      </div>
    `;
    const trigger = document.querySelector<HTMLButtonElement>('#trigger')!;
    trigger.focus();
    const modal = document.querySelector<HTMLElement>('#modal')!;
    cleanup = trapFocus(modal, {});
    expect(document.activeElement?.id).toBe('b1');
  });

  it('focuses the modal container itself when no focusables are present', () => {
    document.body.innerHTML = `
      <button id="trigger">Open</button>
      <div id="modal"><p>No focusables here.</p></div>
    `;
    const trigger = document.querySelector<HTMLButtonElement>('#trigger')!;
    trigger.focus();
    const modal = document.querySelector<HTMLElement>('#modal')!;
    cleanup = trapFocus(modal, {});
    expect(document.activeElement).toBe(modal);
    expect(modal.getAttribute('tabindex')).toBe('-1');
  });

  it('skips disabled buttons when picking the first focusable', () => {
    document.body.innerHTML = `
      <button id="trigger">Open</button>
      <div id="modal">
        <button id="b1" disabled>Disabled</button>
        <button id="b2">B2</button>
      </div>
    `;
    document.querySelector<HTMLButtonElement>('#trigger')!.focus();
    const modal = document.querySelector<HTMLElement>('#modal')!;
    cleanup = trapFocus(modal, {});
    expect(document.activeElement?.id).toBe('b2');
  });

  function pressKey(target: HTMLElement, key: string, opts: { shift?: boolean } = {}) {
    const event = new KeyboardEvent('keydown', {
      key,
      shiftKey: opts.shift ?? false,
      bubbles: true,
      cancelable: true,
    });
    target.dispatchEvent(event);
    return event;
  }

  it('cycles Tab from last focusable to first', () => {
    document.body.innerHTML = `
      <div id="modal">
        <button id="b1">B1</button>
        <button id="b2">B2</button>
      </div>
    `;
    const modal = document.querySelector<HTMLElement>('#modal')!;
    cleanup = trapFocus(modal, {});
    const last = document.querySelector<HTMLButtonElement>('#b2')!;
    last.focus();
    const event = pressKey(last, 'Tab');
    expect(event.defaultPrevented).toBe(true);
    expect(document.activeElement?.id).toBe('b1');
  });

  it('cycles Shift+Tab from first focusable to last', () => {
    document.body.innerHTML = `
      <div id="modal">
        <button id="b1">B1</button>
        <button id="b2">B2</button>
      </div>
    `;
    const modal = document.querySelector<HTMLElement>('#modal')!;
    cleanup = trapFocus(modal, {});
    const first = document.querySelector<HTMLButtonElement>('#b1')!;
    first.focus();
    const event = pressKey(first, 'Tab', { shift: true });
    expect(event.defaultPrevented).toBe(true);
    expect(document.activeElement?.id).toBe('b2');
  });

  it('does not preventDefault on Tab from middle of focusable list', () => {
    document.body.innerHTML = `
      <div id="modal">
        <button id="b1">B1</button>
        <button id="b2">B2</button>
        <button id="b3">B3</button>
      </div>
    `;
    const modal = document.querySelector<HTMLElement>('#modal')!;
    cleanup = trapFocus(modal, {});
    const middle = document.querySelector<HTMLButtonElement>('#b2')!;
    middle.focus();
    const event = pressKey(middle, 'Tab');
    expect(event.defaultPrevented).toBe(false);
  });

  it('re-queries focusables on each Tab so dynamically-disabled buttons are skipped', () => {
    document.body.innerHTML = `
      <div id="modal">
        <button id="b1">B1</button>
        <button id="b2">B2</button>
        <button id="b3">B3</button>
      </div>
    `;
    const modal = document.querySelector<HTMLElement>('#modal')!;
    cleanup = trapFocus(modal, {});
    document.querySelector<HTMLButtonElement>('#b3')!.disabled = true;
    const last = document.querySelector<HTMLButtonElement>('#b2')!;
    last.focus();
    pressKey(last, 'Tab');
    expect(document.activeElement?.id).toBe('b1');
  });

  it('calls onCancel on Escape when canCancel is true', () => {
    document.body.innerHTML = `
      <div id="modal"><button id="b1">B1</button></div>
    `;
    const modal = document.querySelector<HTMLElement>('#modal')!;
    const onCancel = vi.fn();
    cleanup = trapFocus(modal, { onCancel, canCancel: true });
    pressKey(modal, 'Escape');
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it('treats omitted canCancel as true', () => {
    document.body.innerHTML = `
      <div id="modal"><button id="b1">B1</button></div>
    `;
    const modal = document.querySelector<HTMLElement>('#modal')!;
    const onCancel = vi.fn();
    cleanup = trapFocus(modal, { onCancel });
    pressKey(modal, 'Escape');
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it('does not call onCancel on Escape when canCancel is false', () => {
    document.body.innerHTML = `
      <div id="modal"><button id="b1">B1</button></div>
    `;
    const modal = document.querySelector<HTMLElement>('#modal')!;
    const onCancel = vi.fn();
    cleanup = trapFocus(modal, { onCancel, canCancel: false });
    pressKey(modal, 'Escape');
    expect(onCancel).not.toHaveBeenCalled();
  });

  it('restores focus to previouslyFocused on destroy', () => {
    document.body.innerHTML = `
      <button id="trigger">Open</button>
      <div id="modal"><button id="b1">B1</button></div>
    `;
    const trigger = document.querySelector<HTMLButtonElement>('#trigger')!;
    trigger.focus();
    expect(document.activeElement?.id).toBe('trigger');
    const modal = document.querySelector<HTMLElement>('#modal')!;
    const handle = trapFocus(modal, {});
    expect(document.activeElement?.id).toBe('b1');
    handle.destroy();
    expect(document.activeElement?.id).toBe('trigger');
  });

  it('does not throw when previouslyFocused was removed before destroy', () => {
    document.body.innerHTML = `
      <button id="trigger">Open</button>
      <div id="modal"><button id="b1">B1</button></div>
    `;
    const trigger = document.querySelector<HTMLButtonElement>('#trigger')!;
    trigger.focus();
    const modal = document.querySelector<HTMLElement>('#modal')!;
    const handle = trapFocus(modal, {});
    trigger.remove();
    expect(() => handle.destroy()).not.toThrow();
  });

  it('honors canCancel changes via update()', () => {
    document.body.innerHTML = `
      <div id="modal"><button id="b1">B1</button></div>
    `;
    const modal = document.querySelector<HTMLElement>('#modal')!;
    const onCancel = vi.fn();
    const handle = trapFocus(modal, { onCancel, canCancel: true });
    cleanup = handle;
    pressKey(modal, 'Escape');
    expect(onCancel).toHaveBeenCalledTimes(1);
    handle.update({ onCancel, canCancel: false });
    pressKey(modal, 'Escape');
    expect(onCancel).toHaveBeenCalledTimes(1); // still 1 — second press blocked
  });
});
