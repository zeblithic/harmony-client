import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import NamePromptModal from '../NamePromptModal.svelte';

describe('NamePromptModal — ZEB-336 first-run name', () => {
  it('renders the name input when open', () => {
    render(NamePromptModal, { props: { open: true, onSave: vi.fn(), onSkip: vi.fn() } });
    expect(screen.getByTestId('name-prompt-input')).toBeTruthy();
  });

  // ZEB-610 (Commons G) restyle regression guard: the Commons chrome pass
  // touches only the <style> block, so the primary (Save) and skip controls
  // must keep resolving by their pinned testids afterwards.
  it('primary and skip controls still resolve after the Commons restyle', () => {
    render(NamePromptModal, { props: { open: true, onSave: vi.fn(), onSkip: vi.fn() } });
    expect(screen.getByTestId('name-prompt-save')).toBeTruthy();
    expect(screen.getByTestId('name-prompt-skip')).toBeTruthy();
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

  it('Enter in the input saves the trimmed name', async () => {
    const onSave = vi.fn();
    render(NamePromptModal, { props: { open: true, onSave, onSkip: vi.fn() } });
    const input = screen.getByTestId('name-prompt-input');
    await fireEvent.input(input, { target: { value: '  Jake  ' } });
    await fireEvent.keyDown(input, { key: 'Enter' });
    expect(onSave).toHaveBeenCalledWith('Jake');
  });

  it('Escape skips', async () => {
    const onSkip = vi.fn();
    render(NamePromptModal, { props: { open: true, onSave: vi.fn(), onSkip } });
    await fireEvent.keyDown(screen.getByTestId('name-prompt-modal'), { key: 'Escape' });
    expect(onSkip).toHaveBeenCalled();
  });

  it('Enter outside the input (e.g. on the dialog) does not save', async () => {
    // Regression guard: Enter is scoped to the input, so pressing it while the
    // Skip button is focused must not trigger Save. (CodeRabbit, PR #180.)
    const onSave = vi.fn();
    render(NamePromptModal, { props: { open: true, onSave, onSkip: vi.fn() } });
    await fireEvent.input(screen.getByTestId('name-prompt-input'), { target: { value: 'Jake' } });
    await fireEvent.keyDown(screen.getByTestId('name-prompt-modal'), { key: 'Enter' });
    expect(onSave).not.toHaveBeenCalled();
  });
});

describe('NamePromptModal identicon chip (ZEB-650 slice 1)', () => {
  const OWNER = 'aaaa0000aaaa0000aaaa0000aaaa0000';

  it('renders the chip with identicon + typed name when ownerIdHex is set', async () => {
    render(NamePromptModal, { props: { open: true, ownerIdHex: OWNER, onSave: vi.fn(), onSkip: vi.fn() } });
    const chip = screen.getByTestId('name-prompt-chip');
    expect(chip.querySelector('svg')).toBeTruthy(); // identicon renders inline SVG
    expect(chip.textContent).toContain('Anonymous'); // empty input falls back
    await fireEvent.input(screen.getByTestId('name-prompt-input'), { target: { value: 'Jake' } });
    expect(screen.getByTestId('name-prompt-chip').textContent).toContain('Jake');
    expect(screen.getByTestId('name-prompt-chip').textContent).toContain('self-sovereign');
  });

  it('renders no chip when ownerIdHex is null', () => {
    render(NamePromptModal, { props: { open: true, ownerIdHex: null, onSave: vi.fn(), onSkip: vi.fn() } });
    expect(screen.queryByTestId('name-prompt-chip')).toBeNull();
  });

  it('renders no chip when the prop is omitted (default)', () => {
    render(NamePromptModal, { props: { open: true, onSave: vi.fn(), onSkip: vi.fn() } });
    expect(screen.queryByTestId('name-prompt-chip')).toBeNull();
  });
});
