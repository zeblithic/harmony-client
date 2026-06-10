import { invoke } from '@tauri-apps/api/core';

export interface OwnerStateView {
  ownerId: string;
  ownerDisplayName: string;
  devices: DeviceView[];
  canBackUp: boolean;
}

export interface DeviceView {
  deviceId: string;
  displayName: string;
  isThisDevice: boolean;
  trustDecision: TrustDecisionView;
  enrolledAt: number;
  fingerprint: string;
  /** ZEB-418 P2 D17: true iff this device is the owner's pinned butler. */
  butlerPinned: boolean;
  /**
   * 64-hex ed25519 verify key — the SP1 device-id form `setButlerPin`
   * expects. `deviceId` (identity-hash form) is NOT accepted there.
   */
  deviceVkHex: string;
}

export interface TrustDecisionView {
  kind: 'full' | 'provisional' | 'refused';
  reason: string | null;
}

export interface MintIpcResult {
  state: OwnerStateView;
  recoveryToken: string;
}

export interface ExportInfo {
  identityHash: string;
  byteLen: number;
  path: string;
}

export interface ExportSavePathRequest {
  title?: string;
  defaultFilename: string;
  filterName: string;
  filterExtensions: string[];
}

/**
 * Service-class wrapper around the owner-binding Tauri commands.
 *
 * Mirrors `notification-service.ts` pattern: methods + onChange callback
 * for reactive state updates. Error extraction follows the project's
 * memory rule (production rejections are strings; tests emit Errors).
 */
export class OwnerService {
  state: OwnerStateView | null = null;
  onChange?: () => void;

  async refresh(): Promise<void> {
    const view = await invoke<OwnerStateView | null>('get_owner_state');
    this.state = view;
    this.onChange?.();
  }

  async mint(): Promise<MintIpcResult> {
    const result = await invoke<MintIpcResult>('mint_owner_identity');
    this.state = result.state;
    this.onChange?.();
    return result;
  }

  async issueRecoveryToken(): Promise<string> {
    const result = await invoke<{ recoveryToken: string }>('issue_owner_recovery_token');
    return result.recoveryToken;
  }

  async exportRecoveryFile(
    recoveryToken: string,
    pathToken: string,
    passphrase: string,
    comment: string | null,
  ): Promise<ExportInfo> {
    return invoke<ExportInfo>('export_owner_recovery_file_to_path', {
      recoveryToken,
      pathToken,
      passphrase,
      comment,
    });
  }

  async requestExportSavePath(req: ExportSavePathRequest): Promise<string | null> {
    return invoke<string | null>('request_export_save_path', { request: req });
  }
}

/** Memory-rule-compliant error extraction for invoke rejections. */
export function extractError(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
