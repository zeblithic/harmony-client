import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import AddLibraryDialog from '../AddLibraryDialog.svelte';

describe('AddLibraryDialog', () => {
  it('renders input and Add button', () => {
    const { getByText, getByPlaceholderText } = render(AddLibraryDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    expect(getByPlaceholderText(/32-character/i)).toBeInTheDocument();
    expect(getByText(/Add library/i)).toBeInTheDocument();
  });

  it('Add button disabled when input is invalid', () => {
    const { getByText } = render(AddLibraryDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    expect(getByText(/Add library/i)).toBeDisabled();
  });

  it('valid 32-hex input enables Add button', async () => {
    const { getByPlaceholderText, getByText } = render(AddLibraryDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    const input = getByPlaceholderText(/32-character/i);
    await fireEvent.input(input, { target: { value: 'aabbccddeeff00112233445566778899' } });
    expect(getByText(/Add library/i)).not.toBeDisabled();
  });

  it('submit invokes onSubmit with normalized lowercase addr', async () => {
    const onSubmit = vi.fn();
    const { getByPlaceholderText, getByText } = render(AddLibraryDialog, {
      props: { onSubmit, onCancel: vi.fn() },
    });
    const input = getByPlaceholderText(/32-character/i);
    await fireEvent.input(input, { target: { value: 'AABBCCDDEEFF00112233445566778899' } });
    await fireEvent.click(getByText(/Add library/i));
    expect(onSubmit).toHaveBeenCalledWith('aabbccddeeff00112233445566778899');
  });

  it('Cancel invokes onCancel', async () => {
    const onCancel = vi.fn();
    const { getByText } = render(AddLibraryDialog, {
      props: { onSubmit: vi.fn(), onCancel },
    });
    await fireEvent.click(getByText(/Cancel/i));
    expect(onCancel).toHaveBeenCalledOnce();
  });

  it('shows validation message for partial input', async () => {
    const { getByPlaceholderText, getByText } = render(AddLibraryDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    const input = getByPlaceholderText(/32-character/i);
    await fireEvent.input(input, { target: { value: 'aabb' } });
    expect(getByText(/exactly 32 hex characters/i)).toBeInTheDocument();
  });

  it('error prop renders banner', () => {
    const { getByText } = render(AddLibraryDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn(), error: 'expected 16 bytes, got 8' },
    });
    expect(getByText(/expected 16 bytes/i)).toBeInTheDocument();
  });
});
