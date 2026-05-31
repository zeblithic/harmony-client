import { render, screen, fireEvent, cleanup, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, afterEach, beforeEach } from 'vitest';

// ProfileEditor dynamically `import('@tauri-apps/api/core')` inside its handlers,
// so a module mock is sufficient — the dynamic import resolves to the same mock.
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';
import ProfileEditor from '../components/ProfileEditor.svelte';
import type { Profile } from '../types';

const mockInvoke = invoke as ReturnType<typeof vi.fn>;

function baseProfile(overrides: Partial<Profile> = {}): Profile {
  return {
    address: 'addr-self',
    displayName: 'Alice',
    statusText: 'building',
    ...overrides,
  };
}

describe('ProfileEditor — About section', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });
  afterEach(() => cleanup());

  it('ingests the doc and stages the returned profilePageRoot on save', async () => {
    mockInvoke.mockResolvedValue('cid-page-root');
    const onSave = vi.fn();
    render(ProfileEditor, { props: { profile: baseProfile(), onSave } });

    // Bio.
    await fireEvent.input(screen.getByLabelText('Bio'), {
      target: { value: 'Hello mesh' },
    });

    // Add a link row, fill it.
    await fireEvent.click(screen.getByText('Add link'));
    await fireEvent.input(screen.getByLabelText('Link label 1'), {
      target: { value: 'Site' },
    });
    await fireEvent.input(screen.getByLabelText('Link URL 1'), {
      target: { value: 'https://example.com' },
    });

    // Add a field row, fill it.
    await fireEvent.click(screen.getByText('Add field'));
    await fireEvent.input(screen.getByLabelText('Field key 1'), {
      target: { value: 'pronoun' },
    });
    await fireEvent.input(screen.getByLabelText('Field value 1'), {
      target: { value: 'they/them' },
    });

    await fireEvent.click(screen.getByText('Save'));

    await waitFor(() => expect(onSave).toHaveBeenCalled());

    expect(mockInvoke).toHaveBeenCalledWith('ingest_profile_doc', {
      bio: 'Hello mesh',
      links: [{ label: 'Site', url: 'https://example.com' }],
      fields: [{ key: 'pronoun', value: 'they/them' }],
    });

    const emitted = onSave.mock.calls[0][0] as Profile;
    expect(emitted.profilePageRoot).toBe('cid-page-root');
  });

  it('does not ingest and leaves profilePageRoot undefined for an empty About', async () => {
    const onSave = vi.fn();
    render(ProfileEditor, { props: { profile: baseProfile(), onSave } });

    await fireEvent.click(screen.getByText('Save'));

    await waitFor(() => expect(onSave).toHaveBeenCalled());

    expect(mockInvoke).not.toHaveBeenCalledWith(
      'ingest_profile_doc',
      expect.anything(),
    );
    const emitted = onSave.mock.calls[0][0] as Profile;
    expect(emitted.profilePageRoot).toBeUndefined();
  });

  it('filters blank link/field rows before ingest', async () => {
    mockInvoke.mockResolvedValue('cid-filtered');
    const onSave = vi.fn();
    render(ProfileEditor, { props: { profile: baseProfile(), onSave } });

    await fireEvent.input(screen.getByLabelText('Bio'), {
      target: { value: 'Bio only' },
    });

    // Two link rows: one filled, one left entirely blank.
    await fireEvent.click(screen.getByText('Add link'));
    await fireEvent.click(screen.getByText('Add link'));
    await fireEvent.input(screen.getByLabelText('Link label 1'), {
      target: { value: 'Site' },
    });
    await fireEvent.input(screen.getByLabelText('Link URL 1'), {
      target: { value: 'https://example.com' },
    });
    // Link row 2 stays blank → must be filtered.

    // One field row left blank.
    await fireEvent.click(screen.getByText('Add field'));

    await fireEvent.click(screen.getByText('Save'));
    await waitFor(() => expect(onSave).toHaveBeenCalled());

    expect(mockInvoke).toHaveBeenCalledWith('ingest_profile_doc', {
      bio: 'Bio only',
      links: [{ label: 'Site', url: 'https://example.com' }],
      fields: [],
    });
  });

  it('surfaces an ingest error without emitting the profile', async () => {
    mockInvoke.mockRejectedValue(new Error('ingest boom'));
    const onSave = vi.fn();
    render(ProfileEditor, { props: { profile: baseProfile(), onSave } });

    await fireEvent.input(screen.getByLabelText('Bio'), {
      target: { value: 'Hello' },
    });
    await fireEvent.click(screen.getByText('Save'));

    await waitFor(() => expect(screen.getByText('ingest boom')).toBeTruthy());
    expect(onSave).not.toHaveBeenCalled();
  });

  it('prefills bio/links/fields from fetch_profile_doc when profile has a root', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'fetch_profile_doc') {
        return Promise.resolve({
          bio: 'Existing bio',
          links: [{ label: 'Home', url: 'https://home.example' }],
          fields: [{ key: 'city', value: 'mesh' }],
        });
      }
      return Promise.resolve('cid-new');
    });
    const onSave = vi.fn();
    render(ProfileEditor, {
      props: {
        profile: baseProfile({ profilePageRoot: 'cid-existing' }),
        onSave,
      },
    });

    await waitFor(() =>
      expect(mockInvoke).toHaveBeenCalledWith('fetch_profile_doc', {
        cid: 'cid-existing',
      }),
    );
    // Prefilled bio appears in the textarea.
    await waitFor(() =>
      expect((screen.getByLabelText('Bio') as HTMLTextAreaElement).value).toBe(
        'Existing bio',
      ),
    );
    expect((screen.getByLabelText('Link label 1') as HTMLInputElement).value).toBe(
      'Home',
    );
    expect((screen.getByLabelText('Field key 1') as HTMLInputElement).value).toBe(
      'city',
    );
  });

  it('preserves the existing root when saving before the prefill resolves', async () => {
    // fetch_profile_doc never resolves within the test → prefill stays pending.
    // Saving with an empty (unedited) About must NOT drop the existing page:
    // it must re-emit profile.profilePageRoot and never call ingest_profile_doc.
    let resolvePrefill: ((v: unknown) => void) | undefined;
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'fetch_profile_doc') {
        return new Promise((res) => {
          resolvePrefill = res;
        });
      }
      return Promise.resolve('cid-should-not-be-used');
    });
    const onSave = vi.fn();
    render(ProfileEditor, {
      props: {
        profile: baseProfile({ profilePageRoot: 'cid-existing' }),
        onSave,
      },
    });

    // Prefill fetch was kicked off but is still pending.
    await waitFor(() =>
      expect(mockInvoke).toHaveBeenCalledWith('fetch_profile_doc', {
        cid: 'cid-existing',
      }),
    );

    await fireEvent.click(screen.getByText('Save'));
    await waitFor(() => expect(onSave).toHaveBeenCalled());

    // No re-ingest of an empty doc...
    expect(mockInvoke).not.toHaveBeenCalledWith(
      'ingest_profile_doc',
      expect.anything(),
    );
    // ...and the existing root is preserved rather than dropped.
    const emitted = onSave.mock.calls[0][0] as Profile;
    expect(emitted.profilePageRoot).toBe('cid-existing');

    // Drain the pending prefill so the unmount handler is clean.
    resolvePrefill?.({ bio: '', links: [], fields: [] });
  });

  it('does not overwrite user edits when a late prefill resolves', async () => {
    // Prefill resolves only after the user has already typed into the bio.
    let resolvePrefill: ((v: unknown) => void) | undefined;
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'fetch_profile_doc') {
        return new Promise((res) => {
          resolvePrefill = res;
        });
      }
      return Promise.resolve('cid-new');
    });
    const onSave = vi.fn();
    render(ProfileEditor, {
      props: {
        profile: baseProfile({ profilePageRoot: 'cid-existing' }),
        onSave,
      },
    });

    await waitFor(() =>
      expect(mockInvoke).toHaveBeenCalledWith('fetch_profile_doc', {
        cid: 'cid-existing',
      }),
    );

    // User types BEFORE the prefill lands.
    await fireEvent.input(screen.getByLabelText('Bio'), {
      target: { value: 'My own words' },
    });

    // Late prefill resolves — it must NOT clobber the user's edit.
    resolvePrefill?.({
      bio: 'Stale prefilled bio',
      links: [],
      fields: [],
    });

    await waitFor(() =>
      expect((screen.getByLabelText('Bio') as HTMLTextAreaElement).value).toBe(
        'My own words',
      ),
    );
  });
});
