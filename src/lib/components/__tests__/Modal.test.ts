import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import Modal from '../Modal.svelte';
import ModalHarness from './ModalHarness.svelte';

describe('Modal component', () => {
  it('renders with role=dialog and aria-modal=true', () => {
    const { getByRole } = render(Modal, {
      props: {
        onCancel: () => {},
        ariaLabelledby: 'heading',
      },
    });
    const dialog = getByRole('dialog');
    expect(dialog.getAttribute('aria-modal')).toBe('true');
    expect(dialog.getAttribute('aria-labelledby')).toBe('heading');
  });

  it('calls onCancel when Escape is pressed and canCancel is true', async () => {
    const onCancel = vi.fn();
    const { getByRole } = render(Modal, {
      props: { onCancel, canCancel: true, ariaLabelledby: 'h' },
    });
    const dialog = getByRole('dialog');
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it('does not call onCancel when canCancel is false', async () => {
    const onCancel = vi.fn();
    const { getByRole } = render(Modal, {
      props: { onCancel, canCancel: false, ariaLabelledby: 'h' },
    });
    const dialog = getByRole('dialog');
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(onCancel).not.toHaveBeenCalled();
  });

  it('renders provided children inside the dialog', () => {
    const { getByRole, getByText } = render(ModalHarness, {
      props: { ariaLabelledby: 'h', onCancel: () => {} },
    });
    const dialog = getByRole('dialog');
    expect(dialog.contains(getByText('child content'))).toBe(true);
  });

  it('does not dismiss on backdrop click by default (canDismissOnBackdrop=false)', async () => {
    const onCancel = vi.fn();
    const { container } = render(Modal, {
      props: { onCancel, ariaLabelledby: 'h' },
    });
    const overlay = container.querySelector('.modal-overlay') as HTMLElement;
    await fireEvent.click(overlay);
    expect(onCancel).not.toHaveBeenCalled();
  });

  it('dismisses on backdrop click when canDismissOnBackdrop=true', async () => {
    const onCancel = vi.fn();
    const { container } = render(Modal, {
      props: { onCancel, canDismissOnBackdrop: true, ariaLabelledby: 'h' },
    });
    const overlay = container.querySelector('.modal-overlay') as HTMLElement;
    await fireEvent.click(overlay);
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it('does not dismiss when click bubbles from inside the dialog (target check)', async () => {
    const onCancel = vi.fn();
    const { getByRole } = render(Modal, {
      props: { onCancel, canDismissOnBackdrop: true, ariaLabelledby: 'h' },
    });
    const dialog = getByRole('dialog');
    await fireEvent.click(dialog);
    expect(onCancel).not.toHaveBeenCalled();
  });

  it('does not dismiss on backdrop click when canCancel=false (gating)', async () => {
    const onCancel = vi.fn();
    const { container } = render(Modal, {
      props: { onCancel, canCancel: false, canDismissOnBackdrop: true, ariaLabelledby: 'h' },
    });
    const overlay = container.querySelector('.modal-overlay') as HTMLElement;
    await fireEvent.click(overlay);
    expect(onCancel).not.toHaveBeenCalled();
  });
});
