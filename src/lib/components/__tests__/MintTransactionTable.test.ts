import { describe, it, expect } from 'vitest';
import { render, fireEvent, screen } from '@testing-library/svelte';
import MintTransactionTable from '../MintTransactionTable.svelte';
import type { Transaction } from '../../mint-types';

function sample(over: Partial<Transaction> = {}): Transaction {
  return {
    id: 't1',
    transactionDate: '2026-05-19',
    amount: '-42.50',
    currency: 'USD',
    accountId: 'a1',
    accountName: 'Chase',
    description: 'Coffee',
    metadata: null,
    createdAt: '2026-05-19T10:00:00Z',
    updatedAt: '2026-05-19T10:00:00Z',
    ...over,
  };
}

describe('MintTransactionTable', () => {
  it('renders one row per transaction', () => {
    render(MintTransactionTable, {
      props: {
        transactions: [sample(), sample({ id: 't2' })],
        onEdit: () => {},
        onDelete: () => {},
      },
    });
    expect(screen.getAllByRole('row')).toHaveLength(3); // header + 2
  });

  it('shows empty state when no transactions', () => {
    render(MintTransactionTable, {
      props: {
        transactions: [],
        onEdit: () => {},
        onDelete: () => {},
      },
    });
    expect(screen.getByText(/No transactions yet/)).toBeInTheDocument();
  });

  it('truncates metadata over 40 characters with ellipsis', () => {
    const longMeta = '{"description":"' + 'x'.repeat(100) + '"}';
    render(MintTransactionTable, {
      props: {
        transactions: [sample({ metadata: longMeta })],
        onEdit: () => {},
        onDelete: () => {},
      },
    });
    const code = screen.getByTitle(longMeta);
    expect(code.textContent).toContain('…');
    // 40 chars of metadata + 1 ellipsis character = 41 total.
    expect(code.textContent!.length).toBeLessThanOrEqual(41);
  });

  it('calls onEdit with the transaction id when Edit button clicked', async () => {
    let edited: string | null = null;
    render(MintTransactionTable, {
      props: {
        transactions: [sample({ id: 'tx-7' })],
        onEdit: (id: string) => { edited = id; },
        onDelete: () => {},
      },
    });
    await fireEvent.click(screen.getByLabelText('Edit transaction'));
    expect(edited).toBe('tx-7');
  });

  it('calls onDelete with the transaction id when Delete button clicked', async () => {
    let deleted: string | null = null;
    render(MintTransactionTable, {
      props: {
        transactions: [sample({ id: 'tx-99' })],
        onEdit: () => {},
        onDelete: (id: string) => { deleted = id; },
      },
    });
    await fireEvent.click(screen.getByLabelText('Delete transaction'));
    expect(deleted).toBe('tx-99');
  });
});
