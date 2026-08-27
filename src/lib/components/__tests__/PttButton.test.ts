import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import PttButton from '../PttButton.svelte';

describe('PttButton', () => {
  it('renders with mic label', () => {
    render(PttButton, { props: { active: false } });
    expect(screen.getByRole('button', { name: /push to talk/i })).toBeTruthy();
  });

  it('fires onPttStart on mousedown', async () => {
    const onPttStart = vi.fn();
    render(PttButton, { props: { active: false, onPttStart } });
    const btn = screen.getByRole('button', { name: /push to talk/i });
    await fireEvent.mouseDown(btn);
    expect(onPttStart).toHaveBeenCalledOnce();
  });

  it('fires onPttStop on mouseup after mousedown', async () => {
    const onPttStop = vi.fn();
    render(PttButton, { props: { active: false, onPttStop } });
    const btn = screen.getByRole('button', { name: /push to talk/i });
    await fireEvent.mouseDown(btn);
    await fireEvent.mouseUp(btn);
    expect(onPttStop).toHaveBeenCalledOnce();
  });

  it('shows active styling when active', () => {
    render(PttButton, { props: { active: true } });
    const btn = screen.getByRole('button', { name: /push to talk/i });
    expect(btn.classList.contains('active')).toBe(true);
  });

  it('shows processing styling when processing', () => {
    render(PttButton, { props: { active: false, processing: true } });
    const btn = screen.getByRole('button', { name: /push to talk/i });
    expect(btn.classList.contains('processing')).toBe(true);
  });

  it('fires onPttStart on spacebar keydown', async () => {
    const onPttStart = vi.fn();
    render(PttButton, { props: { active: false, onPttStart } });
    await fireEvent.keyDown(window, { code: 'Space' });
    expect(onPttStart).toHaveBeenCalledOnce();
  });

  it('fires onPttStop on spacebar keyup after keydown', async () => {
    const onPttStop = vi.fn();
    render(PttButton, { props: { active: false, onPttStop } });
    await fireEvent.keyDown(window, { code: 'Space' });
    await fireEvent.keyUp(window, { code: 'Space' });
    expect(onPttStop).toHaveBeenCalledOnce();
  });

  it('does not double-fire onPttStart on key repeat', async () => {
    const onPttStart = vi.fn();
    render(PttButton, { props: { active: true, onPttStart } });
    await fireEvent.keyDown(window, { code: 'Space', repeat: true });
    expect(onPttStart).not.toHaveBeenCalled();
  });

  it('is disabled when disabled prop is true', () => {
    render(PttButton, { props: { active: false, disabled: true } });
    const btn = screen.getByRole('button', { name: /push to talk/i });
    expect(btn.hasAttribute('disabled')).toBe(true);
  });

  it('does not fire onPttStart when spacebar pressed on form controls', async () => {
    const onPttStart = vi.fn();
    render(PttButton, { props: { active: false, onPttStart } });

    // Simulate spacebar originating from an <input> element
    const input = document.createElement('input');
    document.body.appendChild(input);
    await fireEvent.keyDown(input, { code: 'Space' });
    expect(onPttStart).not.toHaveBeenCalled();
    document.body.removeChild(input);
  });

  it('fires onPttStart on spacebar when PTT button is focused', async () => {
    const onPttStart = vi.fn();
    render(PttButton, { props: { active: false, onPttStart } });
    const btn = screen.getByRole('button', { name: /push to talk/i });
    await fireEvent.keyDown(btn, { code: 'Space' });
    expect(onPttStart).toHaveBeenCalledOnce();
  });

  it('does not fire onPttStart when spacebar pressed on other buttons', async () => {
    const onPttStart = vi.fn();
    render(PttButton, { props: { active: false, onPttStart } });

    const other = document.createElement('button');
    document.body.appendChild(other);
    await fireEvent.keyDown(other, { code: 'Space' });
    expect(onPttStart).not.toHaveBeenCalled();
    document.body.removeChild(other);
  });

  it('does not fire onPttStop when mouse released while spacebar still held', async () => {
    const onPttStart = vi.fn();
    const onPttStop = vi.fn();
    render(PttButton, { props: { active: false, onPttStart, onPttStop } });
    const btn = screen.getByRole('button', { name: /push to talk/i });

    // Spacebar activates PTT
    await fireEvent.keyDown(window, { code: 'Space' });
    expect(onPttStart).toHaveBeenCalledOnce();

    // Mouse also pressed (no duplicate start)
    await fireEvent.mouseDown(btn);
    expect(onPttStart).toHaveBeenCalledOnce();

    // Mouse released — spacebar still held, so no stop
    await fireEvent.mouseUp(btn);
    expect(onPttStop).not.toHaveBeenCalled();

    // Spacebar released — now all sources cleared, fire stop
    await fireEvent.keyUp(window, { code: 'Space' });
    expect(onPttStop).toHaveBeenCalledOnce();
  });

  it('does not fire onPttStart when spacebar pressed on select', async () => {
    const onPttStart = vi.fn();
    render(PttButton, { props: { active: false, onPttStart } });

    const select = document.createElement('select');
    document.body.appendChild(select);
    await fireEvent.keyDown(select, { code: 'Space' });
    expect(onPttStart).not.toHaveBeenCalled();
    document.body.removeChild(select);
  });

  it('still fires onPttStop on mouseup when disabled flips true mid-hold', async () => {
    // Regression guard: if a parent toggles `disabled` (e.g., calibration
    // lost, permission revoked) during a press, release must still
    // unwind state or the parent's pttActive sticks true forever.
    const onPttStart = vi.fn();
    const onPttStop = vi.fn();
    const { rerender } = render(PttButton, {
      props: { active: false, disabled: false, onPttStart, onPttStop },
    });
    const btn = screen.getByRole('button', { name: /push to talk/i });
    await fireEvent.mouseDown(btn);
    expect(onPttStart).toHaveBeenCalledOnce();

    await rerender({ active: true, disabled: true, onPttStart, onPttStop });

    await fireEvent.mouseUp(btn);
    expect(onPttStop).toHaveBeenCalledOnce();
  });

  it('still fires onPttStop on spacebar keyup when disabled flips true mid-hold', async () => {
    const onPttStart = vi.fn();
    const onPttStop = vi.fn();
    const { rerender } = render(PttButton, {
      props: { active: false, disabled: false, onPttStart, onPttStop },
    });
    await fireEvent.keyDown(window, { code: 'Space' });
    expect(onPttStart).toHaveBeenCalledOnce();

    await rerender({ active: true, disabled: true, onPttStart, onPttStop });

    await fireEvent.keyUp(window, { code: 'Space' });
    expect(onPttStop).toHaveBeenCalledOnce();
  });

  it('still fires onPttStop on touchend when disabled flips true mid-hold', async () => {
    // Mobile regression guard: touch events aren't subject to the same
    // disabled-button event filtering that blocks mouseup in modern
    // browsers, so the button's onTouchend is reachable even when the
    // element is disabled — but we still need to cover that the
    // deactivate() path unwinds correctly for touch inputs too.
    const onPttStart = vi.fn();
    const onPttStop = vi.fn();
    const { rerender } = render(PttButton, {
      props: { active: false, disabled: false, onPttStart, onPttStop },
    });
    const btn = screen.getByRole('button', { name: /push to talk/i });
    await fireEvent.touchStart(btn);
    expect(onPttStart).toHaveBeenCalledOnce();

    await rerender({ active: true, disabled: true, onPttStart, onPttStop });

    await fireEvent.touchEnd(btn);
    expect(onPttStop).toHaveBeenCalledOnce();
  });

  it('still fires onPttStop on touchcancel when disabled flips true mid-hold', async () => {
    const onPttStart = vi.fn();
    const onPttStop = vi.fn();
    const { rerender } = render(PttButton, {
      props: { active: false, disabled: false, onPttStart, onPttStop },
    });
    const btn = screen.getByRole('button', { name: /push to talk/i });
    await fireEvent.touchStart(btn);
    await rerender({ active: true, disabled: true, onPttStart, onPttStop });
    await fireEvent.touchCancel(btn);
    expect(onPttStop).toHaveBeenCalledOnce();
  });

  it('does not fire onPttStop when cursor drifts off button mid-hold', async () => {
    // Release is the commit point: dragging the cursor off the button must
    // NOT end PTT. The window-level mouseup handler catches the actual
    // release anywhere on the page, so onmouseleave would only serve to
    // cut the hold short on pointer drift — which is a UX bug, not a safety
    // net. Guard against a regression that reintroduces onmouseleave.
    const onPttStart = vi.fn();
    const onPttStop = vi.fn();
    render(PttButton, { props: { active: false, onPttStart, onPttStop } });
    const btn = screen.getByRole('button', { name: /push to talk/i });

    await fireEvent.mouseDown(btn);
    expect(onPttStart).toHaveBeenCalledOnce();

    await fireEvent.mouseLeave(btn);
    expect(onPttStop).not.toHaveBeenCalled();

    await fireEvent.mouseUp(window);
    expect(onPttStop).toHaveBeenCalledOnce();
  });

  it('fires onPttStop via window mouseup when disabled blocks button mouseup', async () => {
    // Real-browser simulation: Chrome M120+, Safari 17+, Firefox 124+ all
    // filter mouseup on disabled form controls per the HTML spec, so the
    // button's local onmouseup never fires when disabled. jsdom doesn't
    // replicate that filtering, so we simulate it by dispatching mouseup
    // only at window level — which is where the production fix lives.
    const onPttStart = vi.fn();
    const onPttStop = vi.fn();
    const { rerender } = render(PttButton, {
      props: { active: false, disabled: false, onPttStart, onPttStop },
    });
    const btn = screen.getByRole('button', { name: /push to talk/i });
    await fireEvent.mouseDown(btn);
    expect(onPttStart).toHaveBeenCalledOnce();

    await rerender({ active: true, disabled: true, onPttStart, onPttStop });

    // Note: dispatching directly on window (not the button) to simulate
    // the browser-level disabled filter. The window handler is the only
    // path that can unwind the hold in that scenario.
    await fireEvent.mouseUp(window);
    expect(onPttStop).toHaveBeenCalledOnce();
  });
});
