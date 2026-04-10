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

  it('renders codec toggle', () => {
    render(PttButton, { props: { active: false } });
    const toggle = screen.getByRole('radiogroup', { name: /voice codec/i });
    expect(toggle).toBeTruthy();
  });

  it('fires onCodecChange when codec toggle is clicked', async () => {
    const onCodecChange = vi.fn();
    render(PttButton, { props: { active: false, onCodecChange } });
    const codec2 = screen.getByRole('radio', { name: /codec2/i });
    await fireEvent.click(codec2);
    expect(onCodecChange).toHaveBeenCalledWith('codec2');
  });

  it('disables codec toggle when PTT is active', () => {
    render(PttButton, { props: { active: true } });
    const radios = screen.getAllByRole('radio');
    for (const radio of radios) {
      expect(radio.getAttribute('aria-disabled')).toBe('true');
    }
  });
});
