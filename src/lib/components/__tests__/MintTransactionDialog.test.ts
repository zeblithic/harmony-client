import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, screen, waitFor } from '@testing-library/svelte';
import MintTransactionDialog from '../MintTransactionDialog.svelte';
import type { Account } from '../../mint-types';

function sampleAccount(over: Partial<Account> = {}): Account {
  return {
    id: 'a1',
    name: 'Chase',
    createdAt: '',
    transactionCount: 0,
    ...over,
  };
}

function mockService() {
  return {
    getTransaction: vi.fn(),
    createTransaction: vi.fn().mockResolvedValue({}),
    updateTransaction: vi.fn().mockResolvedValue({}),
  } as any;
}

const defaultProps = {
  accounts: [sampleAccount()],
  defaultCurrency: 'USD',
  editingId: null as string | null,
  onClose: () => {},
  onSaved: () => {},
};

describe('MintTransactionDialog', () => {
  it('disables Save when amount is invalid', async () => {
    render(MintTransactionDialog, {
      props: {
        ...defaultProps,
        service: mockService(),
      },
    });
    const save = screen.getByRole('button', { name: 'Save' });
    // No amount or description filled — Save must be disabled.
    expect(save).toBeDisabled();

    const amountInput = screen.getByLabelText(/Amount/);
    const descInput = screen.getByLabelText(/Description/);

    // Fill description so only amount is the failing field.
    await fireEvent.input(descInput, { target: { value: 'Coffee' } });
    await fireEvent.input(amountInput, { target: { value: 'not a number' } });
    expect(save).toBeDisabled();

    // Fix amount — Save should now be enabled.
    await fireEvent.input(amountInput, { target: { value: '-42.50' } });
    expect(save).not.toBeDisabled();
  });

  it('disables Save when description is empty', async () => {
    render(MintTransactionDialog, {
      props: {
        ...defaultProps,
        service: mockService(),
      },
    });
    const save = screen.getByRole('button', { name: 'Save' });
    const amountInput = screen.getByLabelText(/Amount/);
    const descInput = screen.getByLabelText(/Description/);

    // Valid amount, no description — still disabled.
    await fireEvent.input(amountInput, { target: { value: '100.00' } });
    expect(save).toBeDisabled();

    // Add description — Save enabled.
    await fireEvent.input(descInput, { target: { value: 'Grocery run' } });
    expect(save).not.toBeDisabled();
  });

  it('disables Save when metadata is invalid JSON', async () => {
    render(MintTransactionDialog, {
      props: {
        ...defaultProps,
        service: mockService(),
      },
    });
    const save = screen.getByRole('button', { name: 'Save' });
    const amountInput = screen.getByLabelText(/Amount/);
    const descInput = screen.getByLabelText(/Description/);
    const metaInput = screen.getByLabelText(/Metadata/);

    // Fill required fields.
    await fireEvent.input(amountInput, { target: { value: '50.00' } });
    await fireEvent.input(descInput, { target: { value: 'Dinner' } });
    expect(save).not.toBeDisabled();

    // Set metadata to an unterminated JSON object — must disable Save.
    await fireEvent.input(metaInput, { target: { value: '{' } });
    expect(save).toBeDisabled();

    // Fix metadata — Save re-enabled.
    await fireEvent.input(metaInput, { target: { value: '{"tag":"food"}' } });
    expect(save).not.toBeDisabled();
  });

  it('calls createTransaction with form data when not editing', async () => {
    const service = mockService();
    const onSaved = vi.fn();
    render(MintTransactionDialog, {
      props: {
        ...defaultProps,
        service,
        onSaved,
      },
    });

    await fireEvent.input(screen.getByLabelText(/Amount/), { target: { value: '12.00' } });
    await fireEvent.input(screen.getByLabelText(/Description/), { target: { value: 'Lunch' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(service.createTransaction).toHaveBeenCalled());
    const payload = service.createTransaction.mock.calls[0][0];
    expect(payload.amount).toBe('12.00');
    expect(payload.description).toBe('Lunch');
    // Empty metadata in add mode must be omitted (undefined), not sent as null.
    expect(payload.metadata).toBeUndefined();
    expect(onSaved).toHaveBeenCalled();
  });

  it('calls updateTransaction with metadata: null when metadata field cleared in edit mode', async () => {
    const service = mockService();
    service.getTransaction.mockResolvedValueOnce({
      id: 'tx-1',
      transactionDate: '2026-05-19',
      amount: '10.00',
      currency: 'USD',
      accountId: 'a1',
      accountName: 'Chase',
      description: 'Old',
      metadata: '{"tag":"x"}',
      createdAt: '',
      updatedAt: '',
    });

    render(MintTransactionDialog, {
      props: {
        ...defaultProps,
        service,
        editingId: 'tx-1',
      },
    });

    // Wait for the $effect to fire and populate the metadata field.
    await waitFor(() =>
      expect((screen.getByLabelText(/Metadata/) as HTMLTextAreaElement).value).toBe('{"tag":"x"}')
    );

    // Clear the metadata field.
    await fireEvent.input(screen.getByLabelText(/Metadata/), { target: { value: '' } });

    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(service.updateTransaction).toHaveBeenCalled());
    const [id, payload] = service.updateTransaction.mock.calls[0];
    expect(id).toBe('tx-1');
    // Empty metadata in edit mode must send explicit null to clear server-side.
    expect(payload.metadata).toBeNull();
  });
});
