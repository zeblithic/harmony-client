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
 * Frontend wrapper for the mint_* Tauri commands. Each method invokes
 * exactly one Tauri command with camelCase parameters; the Tauri IPC
 * layer translates to the snake_case Rust signatures.
 *
 * Optional fields are sent as explicit `null` in the invoke payload
 * (not `undefined`) so they serialize cleanly across the IPC boundary.
 */
export class MintService {
  constructor(private readonly adapter: TauriAdapter) {}

  async listTransactions(filter: ListFilter = {}): Promise<Transaction[]> {
    return this.adapter.invoke('mint_list_transactions', {
      dateFrom: filter.dateFrom ?? null,
      dateTo: filter.dateTo ?? null,
      accountId: filter.accountId ?? null,
    }) as Promise<Transaction[]>;
  }

  async getTransaction(id: string): Promise<Transaction | null> {
    return this.adapter.invoke('mint_get_transaction', { id }) as Promise<Transaction | null>;
  }

  async createTransaction(payload: NewTransaction): Promise<Transaction> {
    return this.adapter.invoke('mint_create_transaction', { payload }) as Promise<Transaction>;
  }

  async updateTransaction(id: string, payload: UpdateTransactionPayload): Promise<Transaction> {
    return this.adapter.invoke('mint_update_transaction', { id, payload }) as Promise<Transaction>;
  }

  async deleteTransaction(id: string): Promise<void> {
    return this.adapter.invoke('mint_delete_transaction', { id }) as Promise<void>;
  }

  async listAccounts(): Promise<Account[]> {
    return this.adapter.invoke('mint_list_accounts', {}) as Promise<Account[]>;
  }

  async createAccount(name: string): Promise<Account> {
    return this.adapter.invoke('mint_create_account', { name }) as Promise<Account>;
  }

  async renameAccount(id: string, name: string): Promise<Account> {
    return this.adapter.invoke('mint_rename_account', { id, name }) as Promise<Account>;
  }

  async deleteAccount(id: string, reassignTo: string | null = null): Promise<void> {
    return this.adapter.invoke('mint_delete_account', { id, reassignTo }) as Promise<void>;
  }

  async getDefaultCurrency(): Promise<string | null> {
    return this.adapter.invoke('mint_get_default_currency', {}) as Promise<string | null>;
  }

  async setDefaultCurrency(currency: string): Promise<void> {
    return this.adapter.invoke('mint_set_default_currency', { currency }) as Promise<void>;
  }

  async exportCsv(
    outputPath: string,
    filter: { dateFrom?: string; dateTo?: string } = {}
  ): Promise<ExportSummary> {
    return this.adapter.invoke('mint_export_csv', {
      outputPath,
      dateFrom: filter.dateFrom ?? null,
      dateTo: filter.dateTo ?? null,
    }) as Promise<ExportSummary>;
  }
}
