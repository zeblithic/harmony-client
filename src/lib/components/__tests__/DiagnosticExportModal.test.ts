import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

// Hoisted IPC mocks. vi.mock() runs ahead of the static imports below,
// matching the pattern used in NetworkHealthView.test.ts and the other
// component tests in this folder.
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));
vi.mock('@tauri-apps/plugin-dialog', () => ({
  save: vi.fn(),
}));
vi.mock('@tauri-apps/plugin-fs', () => ({
  writeTextFile: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';
import { save as saveDialog } from '@tauri-apps/plugin-dialog';
import { writeTextFile } from '@tauri-apps/plugin-fs';
import DiagnosticExportModal from '../DiagnosticExportModal.svelte';

const REDACTED_FIXTURE = `## Harmony v0.1.0-alpha.1 (darwin/aarch64)
## Network: reachable
a3f9e1c2… direct 18ms`;
const FULL_FIXTURE = `## Harmony v0.1.0-alpha.1 (darwin/aarch64)
## Network: reachable
a3f9e1c2deadbeef1234567890abcdef direct 18ms`;

describe('DiagnosticExportModal', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders redacted markdown by default (no full Ed25519 hex in DOM)', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(REDACTED_FIXTURE);
    render(DiagnosticExportModal, { onClose: () => {} });
    await waitFor(() => screen.getByTestId('export-preview'));
    const html = document.body.innerHTML;
    // Reject any 32+ char lowercase hex run in the DOM
    expect(html).not.toMatch(/[0-9a-f]{32,}/);
    // And confirm the first IPC call did NOT include full identifiers.
    expect(invoke).toHaveBeenCalledWith('network_health_export_payload', {
      includeFullIds: false,
    });
  });

  it('toggle "Include full identifiers" re-fetches with full IDs', async () => {
    (invoke as ReturnType<typeof vi.fn>)
      .mockResolvedValueOnce(REDACTED_FIXTURE)
      .mockResolvedValueOnce(FULL_FIXTURE);
    render(DiagnosticExportModal, { onClose: () => {} });
    await waitFor(() => screen.getByTestId('export-preview'));
    const toggle = screen.getByTestId('export-full-toggle') as HTMLInputElement;
    await fireEvent.click(toggle);
    await waitFor(() => {
      const html = document.body.innerHTML;
      expect(html).toMatch(/[0-9a-f]{32,}/);
    });
    expect(invoke).toHaveBeenCalledWith('network_health_export_payload', {
      includeFullIds: true,
    });
  });

  it('Copy button calls navigator.clipboard.writeText', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(REDACTED_FIXTURE);
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    render(DiagnosticExportModal, { onClose: () => {} });
    // Wait for the initial export payload to resolve — Copy is
    // disabled while loading (PR #161 R2 P2 fix) so we must wait
    // for the preview to render before clicking.
    await waitFor(() => screen.getByTestId('export-preview'));
    await fireEvent.click(screen.getByTestId('export-copy'));
    await waitFor(() => expect(writeText).toHaveBeenCalledWith(REDACTED_FIXTURE));
  });

  it('Save button opens dialog + writes file', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(REDACTED_FIXTURE);
    (saveDialog as ReturnType<typeof vi.fn>).mockResolvedValue('/tmp/x.txt');
    (writeTextFile as ReturnType<typeof vi.fn>).mockResolvedValue(undefined);
    render(DiagnosticExportModal, { onClose: () => {} });
    // Wait for load complete — Save is disabled while loading.
    await waitFor(() => screen.getByTestId('export-preview'));
    await fireEvent.click(screen.getByTestId('export-save'));
    await waitFor(() =>
      expect(writeTextFile).toHaveBeenCalledWith('/tmp/x.txt', REDACTED_FIXTURE),
    );
    expect(saveDialog).toHaveBeenCalled();
  });

  it('Cancel button calls onClose', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(REDACTED_FIXTURE);
    const onClose = vi.fn();
    render(DiagnosticExportModal, { onClose });
    await waitFor(() => screen.getByTestId('export-cancel'));
    await fireEvent.click(screen.getByTestId('export-cancel'));
    expect(onClose).toHaveBeenCalled();
  });
});
