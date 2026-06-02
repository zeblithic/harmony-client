import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import NamePromptModal from '../NamePromptModal.svelte';

describe('NamePromptModal — ZEB-336 first-run name', () => {
  it('renders the name input when open', () => {
    render(NamePromptModal, { props: { open: true, onSave: vi.fn(), onSkip: vi.fn() } });
    expect(screen.getByTestId('name-prompt-input')).toBeTruthy();
  });

  it('does not render when closed', () => {
    render(NamePromptModal, { props: { open: false, onSave: vi.fn(), onSkip: vi.fn() } });
    expect(screen.queryByTestId('name-prompt-input')).toBeNull();
  });

  it('Save calls onSave with the trimmed name', async () => {
    const onSave = vi.fn();
    render(NamePromptModal, { props: { open: true, onSave, onSkip: vi.fn() } });
    await fireEvent.input(screen.getByTestId('name-prompt-input'), { target: { value: '  Jake  ' } });
    await fireEvent.click(screen.getByTestId('name-prompt-save'));
    expect(onSave).toHaveBeenCalledWith('Jake');
  });

  it('disables Save when the name is empty / whitespace', async () => {
    render(NamePromptModal, { props: { open: true, onSave: vi.fn(), onSkip: vi.fn() } });
    await fireEvent.input(screen.getByTestId('name-prompt-input'), { target: { value: '   ' } });
    expect((screen.getByTestId('name-prompt-save') as HTMLButtonElement).disabled).toBe(true);
  });

  it('Skip calls onSkip and not onSave', async () => {
    const onSave = vi.fn();
    const onSkip = vi.fn();
    render(NamePromptModal, { props: { open: true, onSave, onSkip } });
    await fireEvent.click(screen.getByTestId('name-prompt-skip'));
    expect(onSkip).toHaveBeenCalled();
    expect(onSave).not.toHaveBeenCalled();
  });
});
