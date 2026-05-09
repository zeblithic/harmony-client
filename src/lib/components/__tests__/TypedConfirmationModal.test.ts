import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import TypedConfirmationModal from '../TypedConfirmationModal.svelte';

describe('TypedConfirmationModal', () => {
  const baseProps = {
    title: 'Leave IPFS Crew (you are the only admin)',
    description: 'If you leave, no one can promote new admins.',
    requiredText: 'IPFS Crew',
    confirmLabel: 'Leave anyway',
    onConfirm: vi.fn(),
    onCancel: vi.fn(),
  };

  it('confirm button starts disabled', () => {
    const { getByText } = render(TypedConfirmationModal, { props: baseProps });
    const btn = getByText('Leave anyway').closest('button') as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
  });

  it('partial typed string keeps button disabled', async () => {
    const { getByText, getByPlaceholderText } = render(TypedConfirmationModal, { props: baseProps });
    const input = getByPlaceholderText('Type community name exactly...') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'IPFS Cre' } });
    const btn = getByText('Leave anyway').closest('button') as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
  });

  it('case-mismatched string keeps button disabled', async () => {
    const { getByText, getByPlaceholderText } = render(TypedConfirmationModal, { props: baseProps });
    const input = getByPlaceholderText('Type community name exactly...') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'ipfs crew' } });
    const btn = getByText('Leave anyway').closest('button') as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
  });

  it('exact match enables the confirm button', async () => {
    const { getByText, getByPlaceholderText } = render(TypedConfirmationModal, { props: baseProps });
    const input = getByPlaceholderText('Type community name exactly...') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'IPFS Crew' } });
    const btn = getByText('Leave anyway').closest('button') as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
  });

  it('confirm fires onConfirm when typed string matches and button clicked', async () => {
    const onConfirm = vi.fn();
    const { getByText, getByPlaceholderText } = render(TypedConfirmationModal, {
      props: { ...baseProps, onConfirm },
    });
    const input = getByPlaceholderText('Type community name exactly...') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'IPFS Crew' } });
    await fireEvent.click(getByText('Leave anyway'));
    expect(onConfirm).toHaveBeenCalled();
  });
});
