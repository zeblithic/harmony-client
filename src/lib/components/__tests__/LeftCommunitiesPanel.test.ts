import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import LeftCommunitiesPanel from '../LeftCommunitiesPanel.svelte';
import type { LeftCommunityNavDto } from '../../community-service';

const ROW: LeftCommunityNavDto = {
  spaceId: 'aa'.repeat(16),
  name: 'Old Crew',
  leftAtMs: 1_700_000_000_000,
};

/** Sequenced mock: each listLeftCommunities call returns the next snapshot
 *  (last one repeats), so a post-delete re-fetch can observe the row gone. */
function makeService(rowsSeq: LeftCommunityNavDto[][], removeImpl?: () => Promise<void>) {
  let call = 0;
  return {
    listLeftCommunities: vi.fn(async () => rowsSeq[Math.min(call++, rowsSeq.length - 1)]),
    removeSpace: vi.fn(removeImpl ?? (async () => {})),
  };
}

describe('LeftCommunitiesPanel (ZEB-435)', () => {
  it('shows the empty state when no communities are left', async () => {
    const service = makeService([[]]);
    const { findByText } = render(LeftCommunitiesPanel, { props: { service } });
    await findByText(/No left communities/i);
    expect(service.listLeftCommunities).toHaveBeenCalledTimes(1);
  });

  it('lists left communities with a Delete forever action', async () => {
    const service = makeService([[ROW]]);
    const { findByText, getByText } = render(LeftCommunitiesPanel, { props: { service } });
    await findByText('Old Crew');
    expect(getByText('Delete forever…')).toBeTruthy();
  });

  it('delete forever is typed-confirm: exact name required, then removes and re-fetches', async () => {
    const service = makeService([[ROW], []]);
    const { findByText, getByText, getByPlaceholderText, queryByText } = render(
      LeftCommunitiesPanel,
      { props: { service } },
    );
    await findByText('Old Crew');
    await fireEvent.click(getByText('Delete forever…'));

    // Tier-3 typed confirmation (TypedConfirmationModal): confirm starts
    // disabled, enables only on the exact community name.
    const input = getByPlaceholderText('Type community name exactly...') as HTMLInputElement;
    const confirm = getByText('Delete forever').closest('button') as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);
    await fireEvent.input(input, { target: { value: 'old crew' } });
    expect(confirm.disabled).toBe(true);
    await fireEvent.input(input, { target: { value: 'Old Crew' } });
    expect(confirm.disabled).toBe(false);

    await fireEvent.click(confirm);
    await waitFor(() => {
      expect(service.removeSpace).toHaveBeenCalledWith(ROW.spaceId);
      expect(queryByText('Old Crew')).toBeNull();
    });
    await findByText(/No left communities/i);
    expect(service.listLeftCommunities).toHaveBeenCalledTimes(2);
  });

  it('surfaces a backend refusal and keeps the row', async () => {
    const service = makeService([[ROW], [ROW]], async () => {
      throw new Error('community aa… has not been left — call leave_community before remove_space');
    });
    const { findByText, getByText, getByPlaceholderText } = render(LeftCommunitiesPanel, {
      props: { service },
    });
    await findByText('Old Crew');
    await fireEvent.click(getByText('Delete forever…'));
    const input = getByPlaceholderText('Type community name exactly...') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'Old Crew' } });
    await fireEvent.click(getByText('Delete forever').closest('button') as HTMLButtonElement);

    await findByText(/has not been left/i);
    await findByText('Old Crew');
  });
});
