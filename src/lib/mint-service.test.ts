import { describe, it, expect, vi } from 'vitest';
import { MintService } from './mint-service';
import { createMockAdapter } from './test-utils';

describe('MintService', () => {
  describe('listTransactions', () => {
    it('passes filter as camelCase nulls when omitted', async () => {
      const { adapter: a } = createMockAdapter();
      (a.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce([]);
      const svc = new MintService(a);
      await svc.listTransactions();
      expect(a.invoke).toHaveBeenCalledWith('mint_list_transactions', {
        dateFrom: null,
        dateTo: null,
        accountId: null,
      });
    });

    it('passes provided filter values verbatim', async () => {
      const { adapter: a } = createMockAdapter();
      (a.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce([]);
      const svc = new MintService(a);
      await svc.listTransactions({ dateFrom: '2026-01-01', accountId: 'acc-7' });
      expect(a.invoke).toHaveBeenCalledWith('mint_list_transactions', {
        dateFrom: '2026-01-01',
        dateTo: null,
        accountId: 'acc-7',
      });
    });
  });

  describe('getTransaction', () => {
    it('passes id and returns the resolved transaction (or null)', async () => {
      const { adapter: a } = createMockAdapter();
      (a.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce({
        id: 'tx-1',
        transactionDate: '2026-05-19',
        amount: '1.00',
        currency: 'USD',
        accountId: 'a',
        accountName: 'Chase',
        description: 'Coffee',
        metadata: null,
        createdAt: '2026-05-19T00:00:00Z',
        updatedAt: '2026-05-19T00:00:00Z',
      });
      const svc = new MintService(a);
      const r = await svc.getTransaction('tx-1');
      expect(r?.id).toBe('tx-1');
      expect(a.invoke).toHaveBeenCalledWith('mint_get_transaction', { id: 'tx-1' });
    });

    it('returns null when the backend returns null', async () => {
      const { adapter: a } = createMockAdapter();
      (a.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce(null);
      const svc = new MintService(a);
      const r = await svc.getTransaction('missing');
      expect(r).toBeNull();
    });
  });

  describe('createTransaction', () => {
    it('wraps payload in { payload }', async () => {
      const { adapter: a } = createMockAdapter();
      (a.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce({});
      const svc = new MintService(a);
      const payload = {
        transactionDate: '2026-05-19',
        amount: '-42.50',
        currency: 'USD',
        accountId: 'abc',
        description: 'Coffee',
      };
      await svc.createTransaction(payload);
      expect(a.invoke).toHaveBeenCalledWith('mint_create_transaction', { payload });
    });
  });

  describe('updateTransaction', () => {
    it('passes id alongside payload', async () => {
      const { adapter: a } = createMockAdapter();
      (a.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce({});
      const svc = new MintService(a);
      await svc.updateTransaction('tx-id', { amount: '99.99' });
      expect(a.invoke).toHaveBeenCalledWith('mint_update_transaction', {
        id: 'tx-id',
        payload: { amount: '99.99' },
      });
    });

    it('metadata: null clears the field', async () => {
      const { adapter: a } = createMockAdapter();
      (a.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce({});
      const svc = new MintService(a);
      await svc.updateTransaction('tx-id', { metadata: null });
      expect(a.invoke).toHaveBeenCalledWith('mint_update_transaction', {
        id: 'tx-id',
        payload: { metadata: null },
      });
    });

    it('absent metadata leaves the field alone (does NOT include it in payload)', async () => {
      const { adapter: a } = createMockAdapter();
      (a.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce({});
      const svc = new MintService(a);
      await svc.updateTransaction('tx-id', { amount: '1.00' });
      const [, args] = (a.invoke as ReturnType<typeof vi.fn>).mock.calls[0];
      expect('metadata' in args.payload).toBe(false);
    });
  });

  describe('deleteTransaction', () => {
    it('passes id only', async () => {
      const { adapter: a } = createMockAdapter();
      (a.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce(undefined);
      const svc = new MintService(a);
      await svc.deleteTransaction('tx-id');
      expect(a.invoke).toHaveBeenCalledWith('mint_delete_transaction', { id: 'tx-id' });
    });
  });

  describe('listAccounts', () => {
    it('passes empty object', async () => {
      const { adapter: a } = createMockAdapter();
      (a.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce([]);
      const svc = new MintService(a);
      await svc.listAccounts();
      expect(a.invoke).toHaveBeenCalledWith('mint_list_accounts', {});
    });
  });

  describe('deleteAccount', () => {
    it('defaults reassignTo to null when omitted', async () => {
      const { adapter: a } = createMockAdapter();
      (a.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce(undefined);
      const svc = new MintService(a);
      await svc.deleteAccount('acc-id');
      expect(a.invoke).toHaveBeenCalledWith('mint_delete_account', {
        id: 'acc-id',
        reassignTo: null,
      });
    });

    it('passes reassignTo when provided', async () => {
      const { adapter: a } = createMockAdapter();
      (a.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce(undefined);
      const svc = new MintService(a);
      await svc.deleteAccount('acc-id', 'target-id');
      expect(a.invoke).toHaveBeenCalledWith('mint_delete_account', {
        id: 'acc-id',
        reassignTo: 'target-id',
      });
    });
  });

  describe('exportCsv', () => {
    it('passes outputPath verbatim with null date filters', async () => {
      const { adapter: a } = createMockAdapter();
      (a.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce({ rowsWritten: 0, outputPath: '/tmp/o.csv', byteSize: 50 });
      const svc = new MintService(a);
      const r = await svc.exportCsv('/tmp/o.csv');
      expect(r.outputPath).toBe('/tmp/o.csv');
      expect(a.invoke).toHaveBeenCalledWith('mint_export_csv', {
        outputPath: '/tmp/o.csv',
        dateFrom: null,
        dateTo: null,
      });
    });

    it('passes date filter when provided', async () => {
      const { adapter: a } = createMockAdapter();
      (a.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce({ rowsWritten: 5, outputPath: '/tmp/o.csv', byteSize: 200 });
      const svc = new MintService(a);
      await svc.exportCsv('/tmp/o.csv', { dateFrom: '2026-05-01', dateTo: '2026-05-31' });
      expect(a.invoke).toHaveBeenCalledWith('mint_export_csv', {
        outputPath: '/tmp/o.csv',
        dateFrom: '2026-05-01',
        dateTo: '2026-05-31',
      });
    });
  });

  describe('settings', () => {
    it('getDefaultCurrency passes empty object', async () => {
      const { adapter: a } = createMockAdapter();
      (a.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce('USD');
      const svc = new MintService(a);
      const r = await svc.getDefaultCurrency();
      expect(r).toBe('USD');
      expect(a.invoke).toHaveBeenCalledWith('mint_get_default_currency', {});
    });

    it('setDefaultCurrency passes the currency string', async () => {
      const { adapter: a } = createMockAdapter();
      (a.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce(undefined);
      const svc = new MintService(a);
      await svc.setDefaultCurrency('JPY');
      expect(a.invoke).toHaveBeenCalledWith('mint_set_default_currency', { currency: 'JPY' });
    });
  });
});
