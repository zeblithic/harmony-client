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
    // messageCount=1247 (>0): shows the "~N messages" form.
    expect(screen.getByText(/1247 messages/)).toBeTruthy();
  });

  it('shows generic history message when messageCount is 0 (non-fork community)', () => {
    render(ForkConfirmDialog, { props: { ...baseProps, messageCount: 0 } });
    expect(screen.getByText(/accessible message history \(up to 5000 messages\)/i)).toBeTruthy();
    expect(screen.queryByText(/~0 messages/)).toBeNull();
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

  it('calls onConfirm with {name, silent, alsoLeave, reason} when also_leave is false', async () => {
    const onConfirm = vi.fn();
    render(ForkConfirmDialog, { props: { ...baseProps, onConfirm } });
    // ZEB-649: reason is mandatory — fill it (with padding to pin the trim).
    await fireEvent.input(screen.getByLabelText(/why is this fork happening/i), {
      target: { value: '  Treasury split  ' },
    });
    const btn = screen.getByRole('button', { name: /create fork/i });
    await fireEvent.click(btn);
    expect(onConfirm).toHaveBeenCalledWith({
      name: 'Cool Community (fork)',
      silent: false,
      alsoLeave: false,
      reason: 'Treasury split',
    });
  });

  it('opens typed-confirm second stage when also_leave is checked, requires "leave"', async () => {
    const onConfirm = vi.fn();
    render(ForkConfirmDialog, { props: { ...baseProps, onConfirm } });
    await fireEvent.input(screen.getByLabelText(/why is this fork happening/i), {
      target: { value: 'Treasury split' },
    });
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
      reason: 'Treasury split',
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
    const input = screen.getByLabelText(/^name/i) as HTMLInputElement;
    const btn = screen.getByRole('button', { name: /create fork/i }) as HTMLButtonElement;
    // Satisfy the reason gate so this test isolates the NAME gate.
    await fireEvent.input(screen.getByLabelText(/why is this fork happening/i), {
      target: { value: 'Treasury split' },
    });

    await fireEvent.input(input, { target: { value: '' } });
    expect(btn.disabled).toBe(true);

    await fireEvent.input(input, { target: { value: '   ' } });
    expect(btn.disabled).toBe(true);

    await fireEvent.input(input, { target: { value: 'My fork' } });
    expect(btn.disabled).toBe(false);
  });

  it('ZEB-649: reason is mandatory — button gated until non-whitespace reason; maxlength 280', async () => {
    render(ForkConfirmDialog, { props: { ...baseProps } });
    const reason = screen.getByLabelText(/why is this fork happening/i) as HTMLTextAreaElement;
    const btn = screen.getByRole('button', { name: /create fork/i }) as HTMLButtonElement;

    // Name is prefilled valid; reason starts empty → gated.
    expect(btn.disabled).toBe(true);

    await fireEvent.input(reason, { target: { value: '   ' } });
    expect(btn.disabled).toBe(true);

    await fireEvent.input(reason, { target: { value: 'Treasury split' } });
    expect(btn.disabled).toBe(false);

    // UI cap mirrors the wire cap (280; UTF-16 vs codepoint skew accepted
    // per the moderation-reason precedent).
    expect(reason.getAttribute('maxlength')).toBe('280');
    expect(screen.getByText(/permanent record of the split/i)).toBeTruthy();
  });

  it('shows the always-included snapshot note and the sage consent callout', () => {
    render(ForkConfirmDialog, { props: { ...baseProps } });
    expect(screen.getByText(/a frozen snapshot of every channel is always included/i)).toBeTruthy();
    expect(screen.getByText(/you become its first admin/i)).toBeTruthy();
  });
});
