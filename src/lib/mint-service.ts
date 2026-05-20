import type { TauriAdapter } from './zenoh-service';
import type {
  Transaction,
  NewTransaction,
  UpdateTransactionPayload,
  Account,
  ListFilter,
  ExportSummary,
} from './mint-types';

/**
 * Frontend wrapper for the mint_* Tauri commands. Methods invoke
 * exactly one Tauri command with camelCase parameter keys; the
 * Tauri IPC layer converts them to snake_case for the Rust side.
 *
 * Optional fields are sent as explicit `null` in the invoke payload
 * (not `undefined`) so they serialize cleanly across the IPC boundary
 * — except `UpdateTransactionPayload.metadata`, which preserves the
 * absent-vs-null distinction (absent = leave alone, null = clear).
 */
export class MintService {
  constructor(private readonly adapter: TauriAdapter) {}

  async listTransactions(filter: ListFilter = {}): Promise<Transaction[]> {
    return (await this.adapter.invoke('mint_list_transactions', {
      dateFrom: filter.dateFrom ?? null,
      dateTo: filter.dateTo ?? null,
      accountId: filter.accountId ?? null,
    })) as Transaction[];
  }

  async getTransaction(id: string): Promise<Transaction | null> {
    return (await this.adapter.invoke('mint_get_transaction', { id })) as Transaction | null;
  }

  async createTransaction(payload: NewTransaction): Promise<Transaction> {
    return (await this.adapter.invoke('mint_create_transaction', { payload })) as Transaction;
  }

  async updateTransaction(id: string, payload: UpdateTransactionPayload): Promise<Transaction> {
    return (await this.adapter.invoke('mint_update_transaction', { id, payload })) as Transaction;
  }

  async deleteTransaction(id: string): Promise<void> {
    return (await this.adapter.invoke('mint_delete_transaction', { id })) as void;
  }

  async listAccounts(): Promise<Account[]> {
    return (await this.adapter.invoke('mint_list_accounts', {})) as Account[];
  }

  async createAccount(name: string): Promise<Account> {
    return (await this.adapter.invoke('mint_create_account', { name })) as Account;
  }

  async renameAccount(id: string, name: string): Promise<Account> {
    return (await this.adapter.invoke('mint_rename_account', { id, name })) as Account;
  }

  async deleteAccount(id: string, reassignTo: string | null = null): Promise<void> {
    return (await this.adapter.invoke('mint_delete_account', { id, reassignTo })) as void;
  }

  async getDefaultCurrency(): Promise<string | null> {
    return (await this.adapter.invoke('mint_get_default_currency', {})) as string | null;
  }

  async setDefaultCurrency(currency: string): Promise<void> {
    return (await this.adapter.invoke('mint_set_default_currency', { currency })) as void;
  }

  async exportCsv(
    outputPath: string,
    filter: { dateFrom?: string; dateTo?: string } = {}
  ): Promise<ExportSummary> {
    return (await this.adapter.invoke('mint_export_csv', {
      outputPath,
      dateFrom: filter.dateFrom ?? null,
      dateTo: filter.dateTo ?? null,
    })) as ExportSummary;
  }
}
