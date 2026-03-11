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

  it('fires onPttStop on mouseup', async () => {
    const onPttStop = vi.fn();
    render(PttButton, { props: { active: true, onPttStop } });
    const btn = screen.getByRole('button', { name: /push to talk/i });
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

  it('fires onPttStop on spacebar keyup', async () => {
    const onPttStop = vi.fn();
    render(PttButton, { props: { active: true, onPttStop } });
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

  it('does not fire onPttStart when spacebar pressed on select', async () => {
    const onPttStart = vi.fn();
    render(PttButton, { props: { active: false, onPttStart } });

    const select = document.createElement('select');
    document.body.appendChild(select);
    await fireEvent.keyDown(select, { code: 'Space' });
    expect(onPttStart).not.toHaveBeenCalled();
    document.body.removeChild(select);
  });
});
