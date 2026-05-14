import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import ReshareConfirmDialog from '../ReshareConfirmDialog.svelte';
import type { VineVideo } from '../../types';

describe('ReshareConfirmDialog', () => {
  it('renders the heading and vine title', () => {
    const vine: VineVideo = {
      id: 'vine-1',
      creatorAddress: 'addr-1',
      creatorName: 'Alice',
      createdAt: 1700000000,
      videoCid: 'cid-abc',
      title: 'Cool vine',
      viewed: false,
    };
    render(ReshareConfirmDialog, {
      props: { vine, onConfirm: vi.fn(), onCancel: vi.fn() },
    });
    expect(screen.getByText(/reshare this vine/i)).toBeTruthy();
    expect(screen.getByText('Cool vine')).toBeTruthy();
  });

  it('shows the original creator name when the vine is a reshare', () => {
    const resharedVine: VineVideo = {
      id: 'vine-2',
      creatorAddress: 'addr-2',
      creatorName: 'Bob',
      createdAt: 1700000000,
      videoCid: 'cid-abc',
      title: 'Cool vine',
      viewed: false,
      reshareOf: 'vine-1',
      originalCreatorName: 'Alice',
    };
    render(ReshareConfirmDialog, {
      props: { vine: resharedVine, onConfirm: vi.fn(), onCancel: vi.fn() },
    });
    // The dialog should surface "Alice" (the original) so the user knows
    // attribution goes to Alice, not Bob (the resharer).
    expect(screen.getByText(/Alice/)).toBeTruthy();
  });

  it('falls back to creator name when not a reshare', () => {
    const vine: VineVideo = {
      id: 'vine-1',
      creatorAddress: 'addr-1',
      creatorName: 'Alice',
      createdAt: 1700000000,
      videoCid: 'cid-abc',
      title: 'Cool vine',
      viewed: false,
    };
    render(ReshareConfirmDialog, {
      props: { vine, onConfirm: vi.fn(), onCancel: vi.fn() },
    });
    expect(screen.getByText(/Alice/)).toBeTruthy();
  });

  it('falls back to immediate creator name when reshareOf is set but originalCreatorName is missing (legacy)', () => {
    // Legacy reshare: reshareOf populated, but no originalCreatorName snapshot.
    // The dialog should attribute to the immediate creator rather than render
    // "Originally by undefined" / empty.
    const legacyReshare: VineVideo = {
      id: 'vine-3',
      creatorAddress: 'addr-3',
      creatorName: 'Carol',
      createdAt: 1700000000,
      videoCid: 'cid-abc',
      title: 'Legacy reshare',
      viewed: false,
      reshareOf: 'vine-1',
    };
    render(ReshareConfirmDialog, {
      props: { vine: legacyReshare, onConfirm: vi.fn(), onCancel: vi.fn() },
    });
    expect(screen.getByText(/Carol/)).toBeTruthy();
  });

  it('calls onConfirm when the Reshare button is clicked', async () => {
    const vine: VineVideo = {
      id: 'vine-1',
      creatorAddress: 'addr-1',
      creatorName: 'Alice',
      createdAt: 1700000000,
      videoCid: 'cid-abc',
      title: 'Cool vine',
      viewed: false,
    };
    const onConfirm = vi.fn();
    render(ReshareConfirmDialog, {
      props: { vine, onConfirm, onCancel: vi.fn() },
    });
    // Tighten the name regex to ^reshare$ so we don't match the heading
    // "Reshare this vine?" or other text containing "reshare".
    const btn = screen.getByRole('button', { name: /^reshare$/i });
    await fireEvent.click(btn);
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it('calls onCancel when the Cancel button is clicked', async () => {
    const vine: VineVideo = {
      id: 'vine-1',
      creatorAddress: 'addr-1',
      creatorName: 'Alice',
      createdAt: 1700000000,
      videoCid: 'cid-abc',
      title: 'Cool vine',
      viewed: false,
    };
    const onCancel = vi.fn();
    render(ReshareConfirmDialog, {
      props: { vine, onConfirm: vi.fn(), onCancel },
    });
    const btn = screen.getByRole('button', { name: /cancel/i });
    await fireEvent.click(btn);
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it('calls onCancel on Escape key', async () => {
    const vine: VineVideo = {
      id: 'vine-1',
      creatorAddress: 'addr-1',
      creatorName: 'Alice',
      createdAt: 1700000000,
      videoCid: 'cid-abc',
      title: 'Cool vine',
      viewed: false,
    };
    const onCancel = vi.fn();
    // trap-focus binds keydown on the dialog node (not window), so we
    // dispatch Escape on the [role="dialog"] element — same pattern as
    // ConfirmationModal.test.ts.
    const { container } = render(ReshareConfirmDialog, {
      props: { vine, onConfirm: vi.fn(), onCancel },
    });
    const dialog = container.querySelector('[role="dialog"]') as HTMLElement;
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(onCancel).toHaveBeenCalled();
  });

  it('calls onCancel on backdrop click', async () => {
    // Spec §UI Components → ReshareConfirmDialog: "Dismissible with Escape
    // key or clicking the backdrop." We pass canDismissOnBackdrop={true}
    // to Modal so a click on .modal-overlay (outside the dialog body)
    // fires onCancel. Confirmed not to fire on inner clicks via Modal's
    // own test for target-check gating.
    const vine: VineVideo = {
      id: 'vine-1',
      creatorAddress: 'addr-1',
      creatorName: 'Alice',
      createdAt: 1700000000,
      videoCid: 'cid-abc',
      title: 'Cool vine',
      viewed: false,
    };
    const onCancel = vi.fn();
    const { container } = render(ReshareConfirmDialog, {
      props: { vine, onConfirm: vi.fn(), onCancel },
    });
    const overlay = container.querySelector('.modal-overlay') as HTMLElement;
    await fireEvent.click(overlay);
    expect(onCancel).toHaveBeenCalledTimes(1);
  });
});
