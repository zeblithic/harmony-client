import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import ProfileEditor from '../ProfileEditor.svelte';
import type { Profile } from '../../types';

const testProfile: Profile = {
  address: 'deadbeef01020304',
  displayName: 'Alice',
  statusText: 'Building the mesh',
};

describe('ProfileEditor', () => {
  it('renders display name input with current value', () => {
    render(ProfileEditor, {
      props: { profile: testProfile, onSave: vi.fn() },
    });
    const input = screen.getByLabelText('Display name') as HTMLInputElement;
    expect(input.value).toBe('Alice');
  });

  it('renders status input with current value', () => {
    render(ProfileEditor, {
      props: { profile: testProfile, onSave: vi.fn() },
    });
    const input = screen.getByLabelText('Status text') as HTMLInputElement;
    expect(input.value).toBe('Building the mesh');
  });

  it('renders avatar section', () => {
    render(ProfileEditor, {
      props: { profile: testProfile, onSave: vi.fn() },
    });
    expect(screen.getByLabelText('Edit your profile')).toBeTruthy();
  });

  it('renders save button', () => {
    render(ProfileEditor, {
      props: { profile: testProfile, onSave: vi.fn() },
    });
    expect(screen.getByText('Save')).toBeTruthy();
  });

  it('shows address', () => {
    render(ProfileEditor, {
      props: { profile: testProfile, onSave: vi.fn() },
    });
    expect(screen.getByText('deadbeef01020304')).toBeTruthy();
  });

  it('renders empty status input when statusText is undefined', () => {
    const noStatus: Profile = { address: 'aa', displayName: 'Bob' };
    render(ProfileEditor, {
      props: { profile: noStatus, onSave: vi.fn() },
    });
    const input = screen.getByLabelText('Status text') as HTMLInputElement;
    expect(input.value).toBe('');
  });

  it('calls onSave with trimmed values on button click', async () => {
    const onSave = vi.fn();
    render(ProfileEditor, {
      props: { profile: testProfile, onSave },
    });
    const nameInput = screen.getByLabelText('Display name') as HTMLInputElement;
    await fireEvent.input(nameInput, { target: { value: '  Bob  ' } });
    await fireEvent.click(screen.getByText('Save'));
    expect(onSave).toHaveBeenCalledWith(
      expect.objectContaining({ displayName: 'Bob' }),
    );
  });

  it('falls back to Anonymous when display name is empty', async () => {
    const onSave = vi.fn();
    render(ProfileEditor, {
      props: { profile: testProfile, onSave },
    });
    const nameInput = screen.getByLabelText('Display name') as HTMLInputElement;
    await fireEvent.input(nameInput, { target: { value: '' } });
    await fireEvent.click(screen.getByText('Save'));
    expect(onSave).toHaveBeenCalledWith(
      expect.objectContaining({ displayName: 'Anonymous' }),
    );
  });

  it('sets statusText to undefined when empty', async () => {
    const onSave = vi.fn();
    render(ProfileEditor, {
      props: { profile: testProfile, onSave },
    });
    const statusInput = screen.getByLabelText('Status text') as HTMLInputElement;
    await fireEvent.input(statusInput, { target: { value: '' } });
    await fireEvent.click(screen.getByText('Save'));
    const saved = onSave.mock.calls[0][0] as Profile;
    expect(saved.statusText).toBeUndefined();
  });
});
