import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import WelcomeModal from '../WelcomeModal.svelte';

// vi.hoisted ensures these are available at mock-factory call time
// (vi.mock is hoisted to the top of the file by Vitest, so module-level
// vi.fn() declarations would be undefined when the factory runs).
const { mintMock, requestExportSavePathMock, exportRecoveryFileMock } = vi.hoisted(() => ({
  mintMock: vi.fn(),
  requestExportSavePathMock: vi.fn(),
  exportRecoveryFileMock: vi.fn(),
}));

// Mock the OwnerService so mint() returns a recoveryToken that looks like
// real hex seed material — the test asserts it NEVER reaches the DOM.
vi.mock('../../owner-service', () => ({
  OwnerService: class {
    mint = mintMock;
    requestExportSavePath = requestExportSavePathMock;
    exportRecoveryFile = exportRecoveryFileMock;
  },
  extractError: (e: unknown) => (e instanceof Error ? e.message : String(e)),
}));

// ZEB-494: WelcomeModal now renders PairingJoiner in the 'joining' stage. Mock
// PairingService so the joiner mounts cleanly in its idle state (no Tauri).
vi.mock('../../pairing-service', () => ({
  PairingService: class {
    state = { kind: 'idle' };
    onChange?: () => void;
    init = vi.fn().mockResolvedValue(undefined);
    dispose = vi.fn();
    startJoiner = vi.fn().mockResolvedValue(undefined);
    selectPeer = vi.fn().mockResolvedValue(undefined);
    confirmSas = vi.fn().mockResolvedValue(undefined);
    cancel = vi.fn().mockResolvedValue(undefined);
  },
  extractError: (e: unknown) => (e instanceof Error ? e.message : String(e)),
}));

beforeEach(() => {
  mintMock.mockReset();
  requestExportSavePathMock.mockReset();
  exportRecoveryFileMock.mockReset();
  localStorage.clear();
  sessionStorage.clear();
});

describe('WelcomeModal recovery-artifact redaction invariant', () => {
  it('pane 2 DOM never contains hex seed/token material', async () => {
    // A recoveryToken that contains a long hex run — if it leaked into the
    // DOM, the regex below would catch it.
    mintMock.mockResolvedValue({
      state: { ownerId: 'x', ownerDisplayName: 'x', devices: [], canBackUp: true },
      recoveryToken: 'deadbeefdeadbeefdeadbeefdeadbeef0123456789abcdef0123456789abcdef',
    });
    const { getByTestId, container } = render(WelcomeModal, {
      props: { open: true, onMinted: vi.fn() },
    });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    // wait a tick for the mint promise + stage transition
    await Promise.resolve();
    await Promise.resolve();
    // Pane 2 ('backup') is now showing. Assert no 32+ hex-char run in the DOM.
    expect(container.innerHTML).not.toMatch(/[0-9a-f]{32,}/);
  });
});

describe('WelcomeModal hard gate + flow', () => {
  it('renders explain pane when open and no mint yet', () => {
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted: vi.fn() } });
    expect(getByTestId('welcome-create-identity')).toBeTruthy();
  });

  it('clicks create-my-identity invokes mint with no args', async () => {
    mintMock.mockResolvedValue({ state: {}, recoveryToken: 'tok' });
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted: vi.fn() } });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    expect(mintMock).toHaveBeenCalledWith();
  });

  it('transitions to backup pane on mint success', async () => {
    mintMock.mockResolvedValue({ state: {}, recoveryToken: 'tok' });
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted: vi.fn() } });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    await Promise.resolve(); await Promise.resolve();
    expect(getByTestId('welcome-save-backup')).toBeTruthy();
  });

  it('stays on explain pane with inline error on mint failure', async () => {
    mintMock.mockRejectedValue('mint blew up');
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted: vi.fn() } });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    await Promise.resolve(); await Promise.resolve();
    expect(getByTestId('welcome-mint-error').textContent).toContain('mint blew up');
    expect(getByTestId('welcome-create-identity')).toBeTruthy();
  });

  it('save recovery file calls export with pathToken + passphrase', async () => {
    mintMock.mockResolvedValue({ state: {}, recoveryToken: 'tok' });
    requestExportSavePathMock.mockResolvedValue('path-token-uuid');
    exportRecoveryFileMock.mockResolvedValue({ identityHash: 'h', byteLen: 1, path: '/x' });
    const onMinted = vi.fn();
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted } });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    await Promise.resolve(); await Promise.resolve();
    await fireEvent.input(getByTestId('welcome-backup-passphrase'), { target: { value: 'longenoughpass' } });
    await fireEvent.click(getByTestId('welcome-save-backup'));
    await Promise.resolve(); await Promise.resolve();
    expect(exportRecoveryFileMock).toHaveBeenCalledWith('tok', 'path-token-uuid', 'longenoughpass', null);
    expect(localStorage.getItem('harmony.onboarding.recoveryArtifactBackedUp')).toBe('true');
    expect(onMinted).toHaveBeenCalled();
  });

  it('passphrase under the minimum length disables save button', async () => {
    mintMock.mockResolvedValue({ state: {}, recoveryToken: 'tok' });
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted: vi.fn() } });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    await Promise.resolve(); await Promise.resolve();
    await fireEvent.input(getByTestId('welcome-backup-passphrase'), { target: { value: 'short' } });
    expect((getByTestId('welcome-save-backup') as HTMLButtonElement).disabled).toBe(true);
  });

  it('skip → confirm sets backupSkipped and calls onMinted', async () => {
    mintMock.mockResolvedValue({ state: {}, recoveryToken: 'tok' });
    const onMinted = vi.fn();
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted } });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    await Promise.resolve(); await Promise.resolve();
    await fireEvent.click(getByTestId('welcome-skip-backup'));
    await fireEvent.click(getByTestId('welcome-skip-confirm'));
    await Promise.resolve();
    expect(localStorage.getItem('harmony.onboarding.backupSkipped')).toBe('true');
    expect(onMinted).toHaveBeenCalled();
  });

  it('hard gate ignores Escape keypress', async () => {
    const onMinted = vi.fn();
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted } });
    await fireEvent.keyDown(window, { key: 'Escape' });
    // modal still rendered, onMinted never called
    expect(getByTestId('welcome-modal')).toBeTruthy();
    expect(onMinted).not.toHaveBeenCalled();
  });

  it('hard gate ignores backdrop click', async () => {
    const onMinted = vi.fn();
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted } });
    await fireEvent.click(getByTestId('welcome-modal-backdrop'));
    expect(getByTestId('welcome-modal')).toBeTruthy();
    expect(onMinted).not.toHaveBeenCalled();
  });

  it('moves initial focus into the dialog (focus trap)', async () => {
    // PR #169: aria-modal alone does not trap focus; the $effect must pull
    // focus into the dialog so Tab can be cycled within it.
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted: vi.fn() } });
    await vi.waitFor(() => {
      const active = document.activeElement;
      const modal = getByTestId('welcome-modal');
      expect(modal.contains(active)).toBe(true);
    });
  });

  it('offers a reload escape when mint reports the identity already exists', async () => {
    // PR #169: the hard gate must not deadlock when an identity exists on disk
    // but the node failed to load it. mint() rejects with "already exists";
    // the explain pane must swap the create button for a reload escape.
    mintMock.mockRejectedValue('Owner identity already exists on this device. Wipe via Settings to re-mint.');
    const { getByTestId, queryByTestId } = render(WelcomeModal, {
      props: { open: true, onMinted: vi.fn() },
    });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    await Promise.resolve(); await Promise.resolve();
    // Reload escape is present; the create button is gone (clicking it again
    // would just re-fail).
    expect(getByTestId('welcome-already-exists-reload')).toBeTruthy();
    expect(queryByTestId('welcome-create-identity')).toBeNull();
  });
});

describe('WelcomeModal ZEB-494 — join an existing device', () => {
  it('explain pane offers a "join another of my devices" path alongside mint', () => {
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted: vi.fn() } });
    expect(getByTestId('welcome-create-identity')).toBeTruthy();
    expect(getByTestId('welcome-join-existing')).toBeTruthy();
  });

  it('clicking join mounts the PairingJoiner and replaces the gate content', async () => {
    const { getByTestId, queryByTestId, findByText } = render(WelcomeModal, {
      props: { open: true, onMinted: vi.fn() },
    });
    await fireEvent.click(getByTestId('welcome-join-existing'));
    // PairingJoiner is now the sole modal; the gate's own backdrop/content is
    // suppressed so there is exactly one dialog on screen. findByText waits for
    // the joiner to mount rather than flushing a fixed number of microtasks.
    expect(await findByText('Join existing identity')).toBeTruthy();
    expect(queryByTestId('welcome-modal-backdrop')).toBeNull();
    expect(queryByTestId('welcome-create-identity')).toBeNull();
  });

  it('cancelling the joiner returns to the explain pane (hard gate not dismissed)', async () => {
    const { findByTestId, getByTestId, queryByTestId, findByText } = render(WelcomeModal, {
      props: { open: true, onMinted: vi.fn() },
    });
    await fireEvent.click(getByTestId('welcome-join-existing'));
    // The joiner's idle-state Cancel button → onClose → back to explain.
    await fireEvent.click(await findByText('Cancel'));
    // Wait for the explain pane to re-render rather than flushing microtasks.
    expect(await findByTestId('welcome-create-identity')).toBeTruthy();
    expect(getByTestId('welcome-modal-backdrop')).toBeTruthy();
    expect(queryByTestId('welcome-join-existing')).toBeTruthy();
  });
});
