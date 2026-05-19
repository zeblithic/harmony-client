// Mirrors src-tauri/src/mint.rs types — see spec § API surface > Types.
// Tauri auto-converts the seam from Rust snake_case <-> JS camelCase.

export interface Transaction {
  id: string;
  transactionDate: string;  // ISO 8601 'YYYY-MM-DD'
  amount: string;           // decimal string, e.g. '-42.50'
  currency: string;         // 1-5 all-caps ASCII
  accountId: string;
  accountName: string;
  description: string;
  metadata: string | null;
  createdAt: string;
  updatedAt: string;
}

export interface NewTransaction {
  transactionDate: string;
  amount: string;
  currency: string;
  accountId: string;
  description: string;
  metadata?: string;
}

export interface UpdateTransactionPayload {
  transactionDate?: string;
  amount?: string;
  currency?: string;
  accountId?: string;
  description?: string;
  /** null = clear the field; absent = leave alone; string = set new value */
  metadata?: string | null;
}

export interface Account {
  id: string;
  name: string;
  createdAt: string;
  transactionCount: number;
}

export interface ListFilter {
  dateFrom?: string;
  dateTo?: string;
  accountId?: string;
}

export interface ExportSummary {
  rowsWritten: number;
  outputPath: string;
  byteSize: number;
}
