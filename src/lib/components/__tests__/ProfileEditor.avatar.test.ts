import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent, screen, waitFor } from '@testing-library/svelte';
import ProfileEditor from '../ProfileEditor.svelte';
import type { Profile } from '../../types';

vi.mock('../../avatar-normalize', () => ({
  normalizeAvatar: vi.fn(async () => new Uint8Array([1, 2, 3])),
  validateAvatarInput: vi.fn(),
  AVATAR_MAX_INPUT_BYTES: 10485760,
  AVATAR_EDGE: 256,
}));

// Mock the Tauri core invoke (ProfileEditor dynamically imports it).
const invokeMock = vi.fn(async (cmd: string) =>
  cmd === 'ingest_avatar_bytes' ? 'cidhex123' : undefined,
);
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import { normalizeAvatar } from '../../avatar-normalize';

const testProfile: Profile = {
  address: 'deadbeef01020304',
  displayName: 'Alice',
  statusText: 'Building the mesh',
};

// jsdom does not implement URL.createObjectURL; stub it for the local preview.
beforeEach(() => {
  invokeMock.mockClear();
  (normalizeAvatar as ReturnType<typeof vi.fn>).mockClear();
  URL.createObjectURL = vi.fn(() => 'blob:fake');
});

function pickFile() {
  const input = screen.getByLabelText('Avatar image') as HTMLInputElement;
  const file = new File([new Uint8Array([9, 9, 9])], 'pic.png', {
    type: 'image/png',
  });
  return fireEvent.change(input, { target: { files: [file] } });
}

describe('ProfileEditor avatar upload', () => {
  it('normalizes + ingests a picked image and stores the returned CID', async () => {
    const onSave = vi.fn();
    render(ProfileEditor, { props: { profile: testProfile, onSave } });

    await pickFile();

    // (1) picking a file normalizes it.
    await waitFor(() =>
      expect(normalizeAvatar).toHaveBeenCalledWith(expect.any(File)),
    );

    // (2) then invokes ingest_avatar_bytes with the normalized bytes as an array.
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('ingest_avatar_bytes', {
        bytes: [1, 2, 3],
      }),
    );

    // (3) the returned CID is carried into the saved profile as avatarCid.
    await fireEvent.click(screen.getByText('Save'));
    expect(onSave).toHaveBeenCalledWith(
      expect.objectContaining({ avatarCid: 'cidhex123' }),
    );
  });

  it('preserves existing profile fields when saving after an avatar pick', async () => {
    const onSave = vi.fn();
    render(ProfileEditor, { props: { profile: testProfile, onSave } });

    await pickFile();
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('ingest_avatar_bytes', {
        bytes: [1, 2, 3],
      }),
    );

    await fireEvent.click(screen.getByText('Save'));
    const saved = onSave.mock.calls[0][0] as Profile;
    expect(saved.displayName).toBe('Alice');
    expect(saved.statusText).toBe('Building the mesh');
    expect(saved.avatarCid).toBe('cidhex123');
    // A local blob preview URL is self-seeded for instant feedback.
    expect(saved.avatarUrl).toBe('blob:fake');
  });

  it('shows an animated-format notice when the picked file is a GIF (finding 16)', async () => {
    render(ProfileEditor, { props: { profile: testProfile, onSave: vi.fn() } });
    const input = screen.getByLabelText('Avatar image') as HTMLInputElement;
    const gif = new File([new Uint8Array([9, 9, 9])], 'anim.gif', { type: 'image/gif' });
    await fireEvent.change(input, { target: { files: [gif] } });
    await waitFor(() => expect(screen.getByText(/static frame/i)).toBeInTheDocument());
  });

  it('shows an animated-format notice when the picked file is a WebP (finding 16)', async () => {
    render(ProfileEditor, { props: { profile: testProfile, onSave: vi.fn() } });
    const input = screen.getByLabelText('Avatar image') as HTMLInputElement;
    const webp = new File([new Uint8Array([9, 9, 9])], 'anim.webp', { type: 'image/webp' });
    await fireEvent.change(input, { target: { files: [webp] } });
    await waitFor(() => expect(screen.getByText(/static frame/i)).toBeInTheDocument());
  });

  it('does NOT show the animated-format notice for a static PNG (finding 16)', async () => {
    render(ProfileEditor, { props: { profile: testProfile, onSave: vi.fn() } });
    await pickFile(); // PNG
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('ingest_avatar_bytes', { bytes: [1, 2, 3] }),
    );
    expect(screen.queryByText(/static frame/i)).toBeNull();
  });

  it('does NOT drop an existing avatarCid when saving without picking a new file', async () => {
    const profileWithAvatar: Profile = {
      ...testProfile,
      avatarCid: 'existing-cid',
    };
    const onSave = vi.fn();
    render(ProfileEditor, { props: { profile: profileWithAvatar, onSave } });

    // Save without picking any avatar file — stagedAvatarCid stays undefined,
    // so handleSave must fall back to the profile's existing avatarCid.
    await fireEvent.click(screen.getByText('Save'));
    expect(onSave).toHaveBeenCalledWith(
      expect.objectContaining({ avatarCid: 'existing-cid' }),
    );
    // No ingest should have been called.
    expect(invokeMock).not.toHaveBeenCalled();
  });
});
