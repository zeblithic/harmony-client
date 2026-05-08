import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import ConfirmationModal from '../ConfirmationModal.svelte';

describe('ConfirmationModal', () => {
  const baseProps = {
    title: 'Kick Bob from IPFS Crew?',
    description: 'Bob will be banned from rejoining.',
    confirmLabel: 'Kick Bob',
    danger: true,
    onConfirm: vi.fn(),
    onCancel: vi.fn(),
  };

  it('renders title, description, and both buttons', () => {
    const { getByText } = render(ConfirmationModal, { props: baseProps });
    expect(getByText('Kick Bob from IPFS Crew?')).toBeTruthy();
    expect(getByText('Bob will be banned from rejoining.')).toBeTruthy();
    expect(getByText('Kick Bob')).toBeTruthy();
    expect(getByText('Cancel')).toBeTruthy();
  });

  it('confirm button is on the LEFT (offset from row-end-right triggers)', () => {
    const { getByText } = render(ConfirmationModal, { props: baseProps });
    const confirmBtn = getByText('Kick Bob').closest('button')!;
    const cancelBtn = getByText('Cancel').closest('button')!;
    // The confirm button must come BEFORE the cancel button in DOM order
    // so it lays out to the left visually (with default LTR flexbox).
    expect(confirmBtn.compareDocumentPosition(cancelBtn) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it('clicking confirm calls onConfirm', async () => {
    const onConfirm = vi.fn();
    const { getByText } = render(ConfirmationModal, { props: { ...baseProps, onConfirm } });
    await fireEvent.click(getByText('Kick Bob'));
    expect(onConfirm).toHaveBeenCalled();
  });

  it('clicking cancel calls onCancel', async () => {
    const onCancel = vi.fn();
    const { getByText } = render(ConfirmationModal, { props: { ...baseProps, onCancel } });
    await fireEvent.click(getByText('Cancel'));
    expect(onCancel).toHaveBeenCalled();
  });

  it('Escape key cancels', async () => {
    const onCancel = vi.fn();
    const { container } = render(ConfirmationModal, { props: { ...baseProps, onCancel } });
    const dialog = container.querySelector('[role="dialog"]') as HTMLElement;
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(onCancel).toHaveBeenCalled();
  });
});
