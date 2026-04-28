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

  async exportRecoveryFile(
    recoveryToken: string,
    path: string,
    passphrase: string,
    comment: string | null,
  ): Promise<ExportInfo> {
    return invoke<ExportInfo>('export_owner_recovery_file_to_path', {
      recoveryToken,
      path,
      passphrase,
      comment,
    });
  }
}

/** Memory-rule-compliant error extraction for invoke rejections. */
export function extractError(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
