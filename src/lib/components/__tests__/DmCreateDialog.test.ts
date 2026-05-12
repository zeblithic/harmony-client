import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import DmCreateDialog from '../DmCreateDialog.svelte';
import type { Profile } from '../../types';

function buildProfiles(entries: Array<[string, string]>): Map<string, Profile> {
  return new Map(
    entries.map(([address, displayName]) => [address, { address, displayName }]),
  );
}

const testProfiles = buildProfiles([
  ['bob-hex', 'Bob'],
  ['carol-hex', 'Carol'],
  ['dave-hex', 'Dave'],
]);

function buildSixteenProfiles(): Map<string, Profile> {
  const entries: Array<[string, string]> = [];
  for (let i = 1; i <= 16; i++) {
    entries.push([`addr-${i}`, `Person${i}`]);
  }
  return buildProfiles(entries);
}

describe('DmCreateDialog', () => {
  it('renders search box and shows 0/15 counter', () => {
    const { getByPlaceholderText, getByText } = render(DmCreateDialog, {
      props: { profiles: testProfiles, onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    expect(getByPlaceholderText(/search/i)).toBeInTheDocument();
    expect(getByText(/0 of 15/i)).toBeInTheDocument();
  });

  it('calls onSubmit with kind=dm + 1 recipient when one selected', async () => {
    const onSubmit = vi.fn();
    const { getByText } = render(DmCreateDialog, {
      props: { profiles: testProfiles, onSubmit, onCancel: vi.fn() },
    });
    await fireEvent.click(getByText('Bob'));
    await fireEvent.click(getByText(/start dm/i));
    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({
        kind: 'dm',
        members: ['bob-hex'],
      }),
    );
    const arg = onSubmit.mock.calls[0][0];
    expect(typeof arg.name).toBe('string');
    expect(arg.name.length).toBeGreaterThan(0);
  });

  it('calls onSubmit with kind=group-dm when 2+ recipients selected', async () => {
    const onSubmit = vi.fn();
    const { getByText } = render(DmCreateDialog, {
      props: { profiles: testProfiles, onSubmit, onCancel: vi.fn() },
    });
    await fireEvent.click(getByText('Bob'));
    await fireEvent.click(getByText('Carol'));
    await fireEvent.click(getByText(/start dm/i));
    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({
        kind: 'group-dm',
        members: ['bob-hex', 'carol-hex'],
      }),
    );
  });

  it('shows cap hint and blocks selection at 15 recipients', async () => {
    const onSubmit = vi.fn();
    const profiles = buildSixteenProfiles();
    const { getByText, queryByText } = render(DmCreateDialog, {
      props: { profiles, onSubmit, onCancel: vi.fn() },
    });

    // Select the first 15.
    for (let i = 1; i <= 15; i++) {
      await fireEvent.click(getByText(`Person${i}`));
    }
    expect(getByText(/15 of 15/i)).toBeInTheDocument();
    expect(getByText(/larger groups work better as communities/i)).toBeInTheDocument();

    // 16th attempt: Person16 button should still be visible (not yet selected)
    // but clicking it must be a silent no-op — selected length stays at 15.
    const sixteenth = getByText('Person16');
    await fireEvent.click(sixteenth);
    expect(getByText(/15 of 15/i)).toBeInTheDocument();
    // counter must NOT have advanced past the cap
    expect(queryByText(/16 of 15/i)).toBeNull();
  });

  it('at-cap: convert-to-community button is visible and click invokes onConvertToCommunity', async () => {
    const onConvertToCommunity = vi.fn();
    const onSubmit = vi.fn();
    const profiles = buildSixteenProfiles();
    const { getByRole } = render(DmCreateDialog, {
      props: {
        profiles,
        onSubmit,
        onCancel: vi.fn(),
        onConvertToCommunity,
      },
    });

    // Select the first 15 to reach the cap.
    for (let i = 1; i <= 15; i++) {
      await fireEvent.click(getByRole('button', { name: `Person${i}` }));
    }

    const convertBtn = getByRole('button', { name: /convert to community/i });
    await fireEvent.click(convertBtn);
    expect(onConvertToCommunity).toHaveBeenCalledOnce();
    // DmCreateDialog itself does NOT submit the DM on conversion — App
    // closes this dialog and opens CreateCommunityDialog instead.
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it('below-cap: convert-to-community button is NOT visible', () => {
    const { queryByRole } = render(DmCreateDialog, {
      props: {
        profiles: testProfiles,
        onSubmit: vi.fn(),
        onCancel: vi.fn(),
        onConvertToCommunity: vi.fn(),
      },
    });
    expect(queryByRole('button', { name: /convert to community/i })).toBeNull();
  });

  it('at-cap without onConvertToCommunity prop: no button, no error', async () => {
    const profiles = buildSixteenProfiles();
    const { getByText, queryByRole } = render(DmCreateDialog, {
      props: { profiles, onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    for (let i = 1; i <= 15; i++) {
      await fireEvent.click(getByText(`Person${i}`));
    }
    expect(getByText(/15 of 15/i)).toBeInTheDocument();
    // Without the callback prop the button must not appear (no orphan UI).
    expect(queryByRole('button', { name: /convert to community/i })).toBeNull();
  });

  it('calls onCancel without IPC when Cancel clicked', async () => {
    const onSubmit = vi.fn();
    const onCancel = vi.fn();
    const { getByText } = render(DmCreateDialog, {
      props: { profiles: testProfiles, onSubmit, onCancel },
    });
    await fireEvent.click(getByText('Cancel'));
    expect(onCancel).toHaveBeenCalledOnce();
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it('removing a chip deselects and re-enables that contact', async () => {
    const { getByText, queryByText, getByLabelText } = render(DmCreateDialog, {
      props: { profiles: testProfiles, onSubmit: vi.fn(), onCancel: vi.fn() },
    });

    // Select Bob — a chip should appear, and Bob should disappear from the list.
    await fireEvent.click(getByText('Bob'));
    const removeChip = getByLabelText(/remove bob/i);
    expect(removeChip).toBeInTheDocument();
    expect(getByText(/1 of 15/i)).toBeInTheDocument();

    // Click the chip to deselect.
    await fireEvent.click(removeChip);
    expect(queryByText(/remove bob/i)).toBeNull();
    expect(getByText(/0 of 15/i)).toBeInTheDocument();
    // Bob should be re-listed in the contact list.
    expect(getByText('Bob')).toBeInTheDocument();
  });

  it('disables Start DM button when no recipients selected', () => {
    const { getByText } = render(DmCreateDialog, {
      props: { profiles: testProfiles, onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    const submitBtn = getByText(/start dm/i);
    expect(submitBtn).toBeDisabled();
  });
});
