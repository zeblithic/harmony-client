import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import ForkConfirmDialog from '../ForkConfirmDialog.svelte';

describe('ForkConfirmDialog', () => {
  const baseProps = {
    originalName: 'Cool Community',
    messageCount: 1247,
    onConfirm: vi.fn(),
    onCancel: vi.fn(),
  };

  it('renders heading, name input, both checkboxes, and snapshot count', () => {
    render(ForkConfirmDialog, { props: { ...baseProps } });
    expect(screen.getByText(/fork this community/i)).toBeTruthy();
    expect(screen.getByLabelText(/name/i)).toBeTruthy();
    expect(screen.getByLabelText(/fork silently/i)).toBeTruthy();
    expect(screen.getByLabelText(/also leave/i)).toBeTruthy();
    expect(screen.getByText(/1247 messages/)).toBeTruthy();
  });

  it('prefills name as "{originalName} (fork)"', () => {
    render(ForkConfirmDialog, { props: { ...baseProps } });
    const input = screen.getByLabelText(/name/i) as HTMLInputElement;
    expect(input.value).toBe('Cool Community (fork)');
  });

  it('toggles "Fork silently" checkbox', async () => {
    render(ForkConfirmDialog, { props: { ...baseProps } });
    const cb = screen.getByLabelText(/fork silently/i) as HTMLInputElement;
    expect(cb.checked).toBe(false);
    await fireEvent.click(cb);
    expect(cb.checked).toBe(true);
  });

  it('toggles "Also leave" checkbox', async () => {
    render(ForkConfirmDialog, { props: { ...baseProps } });
    const cb = screen.getByLabelText(/also leave/i) as HTMLInputElement;
    expect(cb.checked).toBe(false);
    await fireEvent.click(cb);
    expect(cb.checked).toBe(true);
  });

  it('calls onConfirm with {name, silent, alsoLeave} when also_leave is false', async () => {
    const onConfirm = vi.fn();
    render(ForkConfirmDialog, { props: { ...baseProps, onConfirm } });
    const btn = screen.getByRole('button', { name: /create fork/i });
    await fireEvent.click(btn);
    expect(onConfirm).toHaveBeenCalledWith({
      name: 'Cool Community (fork)',
      silent: false,
      alsoLeave: false,
    });
  });

  it('opens typed-confirm second stage when also_leave is checked, requires "leave"', async () => {
    const onConfirm = vi.fn();
    render(ForkConfirmDialog, { props: { ...baseProps, onConfirm } });
    await fireEvent.click(screen.getByLabelText(/also leave/i));
    await fireEvent.click(screen.getByRole('button', { name: /create fork/i }));

    // Now the typed-confirm modal should be visible.
    // The prompt "Type leave to confirm:" has "leave" in a <strong> child, so
    // text is split across elements. Use getAllByText with a function matcher
    // and verify at least one matching element exists.
    const promptMatches = screen.getAllByText((_content, element) => {
      const text = element?.textContent ?? '';
      return /type.*leave/i.test(text);
    });
    expect(promptMatches.length).toBeGreaterThan(0);
    const typedInput = screen.getByLabelText(/type to confirm/i) as HTMLInputElement;

    // Wrong text → confirm button is disabled, onConfirm NOT called.
    await fireEvent.input(typedInput, { target: { value: 'no' } });
    const submitBtn = screen.getByRole('button', { name: /confirm/i }) as HTMLButtonElement;
    expect(submitBtn.disabled).toBe(true);
    expect(onConfirm).not.toHaveBeenCalled();

    // Correct text → onConfirm called with alsoLeave: true.
    await fireEvent.input(typedInput, { target: { value: 'leave' } });
    await fireEvent.click(submitBtn);
    expect(onConfirm).toHaveBeenCalledWith({
      name: 'Cool Community (fork)',
      silent: false,
      alsoLeave: true,
    });
  });

  it('calls onCancel on Cancel button, Escape key, and backdrop click', async () => {
    const onCancel = vi.fn();

    // Cancel button
    render(ForkConfirmDialog, { props: { ...baseProps, onCancel } });
    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));
    expect(onCancel).toHaveBeenCalledTimes(1);

    // Escape key via the dialog element
    const { container: c2 } = render(ForkConfirmDialog, { props: { ...baseProps, onCancel } });
    const dialog = c2.querySelector('[role="dialog"]') as HTMLElement;
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(onCancel).toHaveBeenCalledTimes(2);

    // Backdrop click on the overlay
    const { container: c3 } = render(ForkConfirmDialog, { props: { ...baseProps, onCancel } });
    const overlay = c3.querySelector('.modal-overlay') as HTMLElement;
    await fireEvent.click(overlay);
    expect(onCancel).toHaveBeenCalledTimes(3);
  });

  it('disables Create fork button when name is empty or whitespace-only', async () => {
    render(ForkConfirmDialog, { props: { ...baseProps } });
    const input = screen.getByLabelText(/name/i) as HTMLInputElement;
    const btn = screen.getByRole('button', { name: /create fork/i }) as HTMLButtonElement;

    await fireEvent.input(input, { target: { value: '' } });
    expect(btn.disabled).toBe(true);

    await fireEvent.input(input, { target: { value: '   ' } });
    expect(btn.disabled).toBe(true);

    await fireEvent.input(input, { target: { value: 'My fork' } });
    expect(btn.disabled).toBe(false);
  });
});
