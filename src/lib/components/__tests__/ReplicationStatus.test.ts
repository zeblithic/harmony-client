import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import ReplicationStatus from '../ReplicationStatus.svelte';

describe('ReplicationStatus', () => {
  it('shows replica count and target', () => {
    render(ReplicationStatus, {
      props: {
        tier: 'default',
        replicaCount: 3,
        onTierChange: vi.fn(),
      },
    });
    // default tier target is 3
    expect(screen.getByText(/3 of 3/)).toBeTruthy();
  });

  it('shows warning when under-replicated', () => {
    render(ReplicationStatus, {
      props: {
        tier: 'high',
        replicaCount: 2,
        onTierChange: vi.fn(),
      },
    });
    // high tier target is 5, only 2 replicas
    expect(screen.getByText(/2 of 5/)).toBeTruthy();
    expect(screen.getByText(/under-replicated/i)).toBeTruthy();
  });

  it('does not show warning when fully replicated', () => {
    const { container } = render(ReplicationStatus, {
      props: {
        tier: 'default',
        replicaCount: 3,
        onTierChange: vi.fn(),
      },
    });
    expect(container.textContent).not.toMatch(/under-replicated/i);
  });

  it('calls onTierChange when tier is changed via select', async () => {
    const onTierChange = vi.fn();
    render(ReplicationStatus, {
      props: {
        tier: 'default',
        replicaCount: 3,
        onTierChange,
      },
    });
    const select = screen.getByLabelText('Replication tier');
    await fireEvent.change(select, { target: { value: 'high' } });
    expect(onTierChange).toHaveBeenCalledWith('high');
  });
});
