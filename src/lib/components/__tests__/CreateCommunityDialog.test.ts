import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import CreateCommunityDialog from '../CreateCommunityDialog.svelte';

describe('CreateCommunityDialog', () => {
  it('renders name input + kind toggle + Create button', () => {
    const { getByPlaceholderText, getByText } = render(CreateCommunityDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    expect(getByPlaceholderText('Community name')).toBeTruthy();
    expect(getByText('Open')).toBeTruthy();
    expect(getByText('Invite-only')).toBeTruthy();
    expect(getByText('Create')).toBeTruthy();
  });

  it('default kind is invite-only', () => {
    const { getByLabelText } = render(CreateCommunityDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    const inviteOnly = getByLabelText('Invite-only') as HTMLInputElement;
    expect(inviteOnly.checked).toBe(true);
  });

  it('Create button disabled when name is empty', () => {
    const { getByText } = render(CreateCommunityDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    const btn = getByText('Create').closest('button') as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
  });

  it('Create button enables when name is non-empty', async () => {
    const { getByText, getByPlaceholderText } = render(CreateCommunityDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    const input = getByPlaceholderText('Community name') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'Test Crew' } });
    const btn = getByText('Create').closest('button') as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
  });

  it('submit calls onSubmit with name + kind', async () => {
    const onSubmit = vi.fn();
    const { getByText, getByPlaceholderText } = render(CreateCommunityDialog, {
      props: { onSubmit, onCancel: vi.fn() },
    });
    await fireEvent.input(getByPlaceholderText('Community name'), { target: { value: 'Test Crew' } });
    await fireEvent.click(getByText('Create'));
    expect(onSubmit).toHaveBeenCalledWith('Test Crew', 'invite-only');
  });

  it('switching to Open and submitting passes "open" kind', async () => {
    const onSubmit = vi.fn();
    const { getByText, getByLabelText, getByPlaceholderText } = render(CreateCommunityDialog, {
      props: { onSubmit, onCancel: vi.fn() },
    });
    await fireEvent.input(getByPlaceholderText('Community name'), { target: { value: 'Open Crew' } });
    await fireEvent.click(getByLabelText('Open'));
    await fireEvent.click(getByText('Create'));
    expect(onSubmit).toHaveBeenCalledWith('Open Crew', 'open');
  });

  it('cancel calls onCancel', async () => {
    const onCancel = vi.fn();
    const { getByText } = render(CreateCommunityDialog, {
      props: { onSubmit: vi.fn(), onCancel },
    });
    await fireEvent.click(getByText('Cancel'));
    expect(onCancel).toHaveBeenCalled();
  });
});
