import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, screen, waitFor } from '@testing-library/svelte';
import MintAccountManager from '../MintAccountManager.svelte';
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
    createAccount: vi.fn().mockResolvedValue({}),
    renameAccount: vi.fn().mockResolvedValue({}),
    deleteAccount: vi.fn().mockResolvedValue(undefined),
  } as any;
}

describe('MintAccountManager', () => {
  it('Add button is disabled until a name is typed', async () => {
    render(MintAccountManager, {
      props: {
        service: mockService(),
        accounts: [sampleAccount()],
        onClose: () => {},
        onChanged: () => {},
      },
    });
    const addBtn = screen.getByRole('button', { name: 'Add' });
    expect(addBtn).toBeDisabled();

    const input = screen.getByPlaceholderText('New account name');
    await fireEvent.input(input, { target: { value: 'Savings' } });
    expect(addBtn).not.toBeDisabled();
  });

  it('Add button remains disabled for whitespace-only input', async () => {
    render(MintAccountManager, {
      props: {
        service: mockService(),
        accounts: [sampleAccount()],
        onClose: () => {},
        onChanged: () => {},
      },
    });
    const addBtn = screen.getByRole('button', { name: 'Add' });
    const input = screen.getByPlaceholderText('New account name');

    await fireEvent.input(input, { target: { value: '   ' } });
    expect(addBtn).toBeDisabled();
  });

  it('Create account invokes service with trimmed name', async () => {
    const service = mockService();
    const onChanged = vi.fn();
    render(MintAccountManager, {
      props: {
        service,
        accounts: [sampleAccount()],
        onClose: () => {},
        onChanged,
      },
    });

    const input = screen.getByPlaceholderText('New account name');
    await fireEvent.input(input, { target: { value: '  Chase  ' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));

    await waitFor(() => expect(service.createAccount).toHaveBeenCalledWith('Chase'));
    expect(onChanged).toHaveBeenCalled();
  });

  it('Rename starts in-place edit and Save commits with account id and trimmed name', async () => {
    const service = mockService();
    const onChanged = vi.fn();
    const account = sampleAccount({ id: 'a1', name: 'Chase' });
    render(MintAccountManager, {
      props: {
        service,
        accounts: [account],
        onClose: () => {},
        onChanged,
      },
    });

    // Click Rename — inline input should appear.
    await fireEvent.click(screen.getByRole('button', { name: 'Rename Chase' }));

    const renameInput = screen.getByRole('textbox', { name: /New name for Chase/ });
    expect(renameInput).toBeInTheDocument();

    // Type new name with surrounding whitespace.
    await fireEvent.input(renameInput, { target: { value: '  Wells Fargo  ' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(service.renameAccount).toHaveBeenCalledWith('a1', 'Wells Fargo')
    );
    expect(onChanged).toHaveBeenCalled();
  });

  it('Rename Cancel closes the inline edit without calling service', async () => {
    const service = mockService();
    render(MintAccountManager, {
      props: {
        service,
        accounts: [sampleAccount({ id: 'a1', name: 'Chase' })],
        onClose: () => {},
        onChanged: () => {},
      },
    });

    await fireEvent.click(screen.getByRole('button', { name: 'Rename Chase' }));
    expect(screen.getByRole('textbox', { name: /New name for Chase/ })).toBeInTheDocument();

    await fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(service.renameAccount).not.toHaveBeenCalled();
    // Inline input gone; Rename button back.
    expect(screen.getByRole('button', { name: 'Rename Chase' })).toBeInTheDocument();
  });

  it('Delete with 0 transactions: Confirm Delete enabled immediately', async () => {
    const service = mockService();
    const onChanged = vi.fn();
    const account = sampleAccount({ id: 'a1', name: 'Chase', transactionCount: 0 });
    render(MintAccountManager, {
      props: {
        service,
        accounts: [account],
        onClose: () => {},
        onChanged,
      },
    });

    await fireEvent.click(screen.getByRole('button', { name: 'Delete Chase' }));

    const confirmBtn = screen.getByRole('button', { name: 'Confirm Delete' });
    expect(confirmBtn).not.toBeDisabled();

    await fireEvent.click(confirmBtn);
    await waitFor(() => expect(service.deleteAccount).toHaveBeenCalledWith('a1', null));
    expect(onChanged).toHaveBeenCalled();
  });

  it('Delete with N transactions: Confirm Delete disabled until reassign-to selected', async () => {
    const service = mockService();
    const onChanged = vi.fn();
    const accountA = sampleAccount({ id: 'a1', name: 'Chase', transactionCount: 5 });
    const accountB = sampleAccount({ id: 'a2', name: 'Savings', transactionCount: 0 });
    render(MintAccountManager, {
      props: {
        service,
        accounts: [accountA, accountB],
        onClose: () => {},
        onChanged,
      },
    });

    await fireEvent.click(screen.getByRole('button', { name: 'Delete Chase' }));

    const confirmBtn = screen.getByRole('button', { name: 'Confirm Delete' });
    // Has transactions and no reassign-to selected yet — must be disabled.
    expect(confirmBtn).toBeDisabled();

    // Select the reassign-to target.
    const select = screen.getByRole('combobox');
    await fireEvent.change(select, { target: { value: 'a2' } });

    // Now enabled.
    expect(confirmBtn).not.toBeDisabled();

    await fireEvent.click(confirmBtn);
    await waitFor(() => expect(service.deleteAccount).toHaveBeenCalledWith('a1', 'a2'));
    expect(onChanged).toHaveBeenCalled();
  });

  it('Delete reassign dropdown excludes the account being deleted', async () => {
    const accountA = sampleAccount({ id: 'a1', name: 'Chase', transactionCount: 3 });
    const accountB = sampleAccount({ id: 'a2', name: 'Savings', transactionCount: 0 });
    const accountC = sampleAccount({ id: 'a3', name: 'Brokerage', transactionCount: 0 });
    render(MintAccountManager, {
      props: {
        service: mockService(),
        accounts: [accountA, accountB, accountC],
        onClose: () => {},
        onChanged: () => {},
      },
    });

    await fireEvent.click(screen.getByRole('button', { name: 'Delete Chase' }));

    const select = screen.getByRole('combobox');
    const options = Array.from(select.querySelectorAll('option')).map((o) => (o as HTMLOptionElement).value);
    // Chase (a1) must not appear as a reassign target.
    expect(options).not.toContain('a1');
    expect(options).toContain('a2');
    expect(options).toContain('a3');
  });

  it('supports multiple deletes in sequence after first completes', async () => {
    const service = mockService();
    const accountA = sampleAccount({ id: 'a1', name: 'Chase', transactionCount: 0 });
    const accountB = sampleAccount({ id: 'a2', name: 'Savings', transactionCount: 0 });
    render(MintAccountManager, {
      props: {
        service,
        accounts: [accountA, accountB],
        onClose: () => {},
        onChanged: () => {},
      },
    });

    // Delete first account.
    await fireEvent.click(screen.getByRole('button', { name: 'Delete Chase' }));
    await fireEvent.click(screen.getByRole('button', { name: 'Confirm Delete' }));
    await waitFor(() => expect(service.deleteAccount).toHaveBeenCalledWith('a1', null));

    // Confirm-delete block is gone; can now open delete for second account.
    await fireEvent.click(screen.getByRole('button', { name: 'Delete Savings' }));
    const confirmBtn = screen.getByRole('button', { name: 'Confirm Delete' });
    expect(confirmBtn).not.toBeDisabled();

    await fireEvent.click(confirmBtn);
    await waitFor(() => expect(service.deleteAccount).toHaveBeenCalledWith('a2', null));
    expect(service.deleteAccount).toHaveBeenCalledTimes(2);
  });
});
