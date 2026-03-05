import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import ComposeBar from '../ComposeBar.svelte';

describe('ComposeBar', () => {
  it('renders with standard priority selected by default', () => {
    const { container } = render(ComposeBar);
    const standardBtn = container.querySelector('[data-priority="standard"]');
    expect(standardBtn?.classList.contains('active')).toBe(true);
  });

  it('changes priority when toggle buttons are clicked', async () => {
    const { container } = render(ComposeBar);
    const quietBtn = container.querySelector('[data-priority="quiet"]')!;
    await fireEvent.click(quietBtn);
    expect(quietBtn.classList.contains('active')).toBe(true);
    const standardBtn = container.querySelector('[data-priority="standard"]');
    expect(standardBtn?.classList.contains('active')).toBe(false);
  });

  it('calls onSend with text and current priority on Enter', async () => {
    const onSend = vi.fn();
    render(ComposeBar, { props: { onSend } });
    const textarea = screen.getByRole('textbox');
    await fireEvent.input(textarea, { target: { value: 'Hello' } });
    await fireEvent.keyDown(textarea, { key: 'Enter' });
    expect(onSend).toHaveBeenCalledWith('Hello', 'standard');
  });

  it('sends as quiet on Ctrl+Enter regardless of toggle state', async () => {
    const onSend = vi.fn();
    render(ComposeBar, { props: { onSend } });
    const textarea = screen.getByRole('textbox');
    await fireEvent.input(textarea, { target: { value: 'Whisper' } });
    await fireEvent.keyDown(textarea, { key: 'Enter', ctrlKey: true });
    expect(onSend).toHaveBeenCalledWith('Whisper', 'quiet');
  });

  it('resets priority to standard after sending', async () => {
    const onSend = vi.fn();
    const { container } = render(ComposeBar, { props: { onSend } });
    const loudBtn = container.querySelector('[data-priority="loud"]')!;
    await fireEvent.click(loudBtn);
    const textarea = screen.getByRole('textbox');
    await fireEvent.input(textarea, { target: { value: 'Alert!' } });
    await fireEvent.keyDown(textarea, { key: 'Enter' });
    expect(onSend).toHaveBeenCalledWith('Alert!', 'loud');
    const standardBtn = container.querySelector('[data-priority="standard"]');
    expect(standardBtn?.classList.contains('active')).toBe(true);
  });

  it('does not send empty messages', async () => {
    const onSend = vi.fn();
    render(ComposeBar, { props: { onSend } });
    const textarea = screen.getByRole('textbox');
    await fireEvent.keyDown(textarea, { key: 'Enter' });
    expect(onSend).not.toHaveBeenCalled();
  });

  it('clears textarea after sending', async () => {
    const onSend = vi.fn();
    render(ComposeBar, { props: { onSend } });
    const textarea = screen.getByRole('textbox') as HTMLTextAreaElement;
    await fireEvent.input(textarea, { target: { value: 'Hello' } });
    await fireEvent.keyDown(textarea, { key: 'Enter' });
    expect(textarea.value).toBe('');
  });

  it('shows priority label when not standard', async () => {
    const { container } = render(ComposeBar);
    expect(container.querySelector('.priority-label')).toBeNull();
    const quietBtn = container.querySelector('[data-priority="quiet"]')!;
    await fireEvent.click(quietBtn);
    expect(screen.getByText('sending quietly')).toBeTruthy();
  });

  it('allows newline with Shift+Enter', async () => {
    const onSend = vi.fn();
    render(ComposeBar, { props: { onSend } });
    const textarea = screen.getByRole('textbox');
    await fireEvent.keyDown(textarea, { key: 'Enter', shiftKey: true });
    expect(onSend).not.toHaveBeenCalled();
  });
});
