import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import WelcomeModal from '../WelcomeModal.svelte';
import { identityKeyBackupNote, type IdentityStoreBackend } from '../../identity-backup-copy';

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

// ZEB-650 slice 2: OwnerPhraseReveal (mounted in the backup stage) calls the
// export_owner_mnemonic_words command via direct invoke — mock the core module
// so the reveal flow can be driven without Tauri. The mock stays unused (and
// asserted unused) in every test that doesn't explicitly click the reveal.
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));

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

import { invoke } from '@tauri-apps/api/core';
const mockCoreInvoke = vi.mocked(invoke);

beforeEach(() => {
  mintMock.mockReset();
  requestExportSavePathMock.mockReset();
  exportRecoveryFileMock.mockReset();
  mockCoreInvoke.mockReset();
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

  it('shows a dismissible discoverability privacy note on the welcome stage (ZEB-881)', async () => {
    const { getByTestId, queryByTestId } = render(WelcomeModal, {
      props: { open: true, onMinted: vi.fn() },
    });
    const note = getByTestId('welcome-discoverability-note');
    // Lock the accurate framing: discoverability is identity-address (case-B)
    // discovery, and the note must name the private-mode escape + its location.
    expect(note.textContent).toMatch(/discoverable/i);
    expect(note.textContent).toMatch(/identity address/i);
    expect(note.textContent).toMatch(/private/i);
    expect(note.textContent).toMatch(/Settings\s*→\s*Network/i);
    await fireEvent.click(getByTestId('welcome-discoverability-note-dismiss'));
    expect(queryByTestId('welcome-discoverability-note')).toBeNull();
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

  it('maps a mint failure to friendly copy with the raw backend string tucked into <details>', async () => {
    // The first-run hard gate must not headline a raw backend string. A
    // non-"already exists" failure shows friendly copy, with the raw detail
    // preserved for bug reports inside a disclosure.
    mintMock.mockRejectedValue('keychain write failed: SecKeychainItemCreate -34018');
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted: vi.fn() } });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    await Promise.resolve(); await Promise.resolve();
    const box = getByTestId('welcome-mint-error');
    // Friendly headline, not the raw backend string.
    expect(box.querySelector('.mint-error-summary')?.textContent).toMatch(/couldn.t create your identity/i);
    // Raw detail preserved, but inside the <details> disclosure.
    const details = box.querySelector('details');
    expect(details).toBeTruthy();
    expect(details?.textContent).toContain('SecKeychainItemCreate -34018');
  });

  it('save recovery file calls export with pathToken + passphrase', async () => {
    mintMock.mockResolvedValue({ state: { ownerId: 'ownerX' }, recoveryToken: 'tok' });
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
    // ZEB-587: the backed-up flag is owner-scoped to the freshly-minted identity.
    expect(localStorage.getItem('harmony.onboarding.recoveryArtifactBackedUp:owner-ownerX')).toBe('true');
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
    mintMock.mockResolvedValue({ state: { ownerId: 'ownerX' }, recoveryToken: 'tok' });
    const onMinted = vi.fn();
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted } });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    await Promise.resolve(); await Promise.resolve();
    await fireEvent.click(getByTestId('welcome-skip-backup'));
    await fireEvent.click(getByTestId('welcome-skip-confirm'));
    await Promise.resolve();
    // ZEB-587: the skipped flag is owner-scoped to the freshly-minted identity.
    expect(localStorage.getItem('harmony.onboarding.backupSkipped:owner-ownerX')).toBe('true');
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

describe('Commons chrome (ZEB-610)', () => {
  // Local render harness mirroring the existing backup-stage tests: mint()
  // resolves with hex-bearing seed material + a 32-hex ownerId so the redaction
  // guard below has something real to catch if the restyle ever surfaces it.
  function renderWelcome() {
    return render(WelcomeModal, { props: { open: true, onMinted: vi.fn() } });
  }

  async function advanceToBackupStage() {
    mintMock.mockResolvedValue({
      state: {
        ownerId: 'a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6',
        ownerDisplayName: 'x',
        devices: [],
        canBackUp: true,
      },
      recoveryToken: 'deadbeefdeadbeefdeadbeefdeadbeef0123456789abcdef0123456789abcdef',
    });
    const utils = renderWelcome();
    await fireEvent.click(utils.getByTestId('welcome-create-identity'));
    // wait a tick for the mint promise + stage transition
    await Promise.resolve();
    await Promise.resolve();
    return utils;
  }

  // The wizard progress rail is present on the welcome stage.
  it('renders the wizard pip rail on the welcome stage', () => {
    const { getByTestId } = renderWelcome();
    expect(getByTestId('wizard-progress')).toBeTruthy();
  });

  // The rail also shows on the middle (minting) stage with the step-2 counter —
  // the design requires it on all three stages (explain/minting/backup); this
  // closes the coverage gap between the welcome and backup cases (CodeRabbit #412).
  it('shows the wizard rail with the step-2 counter while minting', async () => {
    let resolveMint!: (value: unknown) => void;
    mintMock.mockReturnValue(
      new Promise((resolve) => {
        resolveMint = resolve;
      }),
    );
    const { getByTestId, queryByText } = renderWelcome();
    await fireEvent.click(getByTestId('welcome-create-identity'));
    await Promise.resolve();
    // Mint promise still pending → stage is 'minting': rail present, step 2 of 3.
    expect(getByTestId('wizard-progress')).toBeTruthy();
    expect(queryByText('Step 2 of 3')).toBeTruthy();
    // Resolve so the pending mint leaves no dangling promise.
    resolveMint({
      state: {
        ownerId: 'a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6',
        ownerDisplayName: 'x',
        devices: [],
        canBackUp: true,
      },
      recoveryToken: 'deadbeefdeadbeefdeadbeefdeadbeef0123456789abcdef0123456789abcdef',
    });
    await Promise.resolve();
  });

  // The backup (Step 3) stage shows the real encrypted-file passphrase field,
  // NOT a recovery-phrase word grid (honesty ledger §0.1).
  it('backup stage offers the encrypted-file passphrase, not a phrase grid', async () => {
    const { getByTestId, queryByText } = await advanceToBackupStage();
    expect(getByTestId('welcome-backup-passphrase')).toBeTruthy();
    // No 12/24-word mnemonic grid is rendered.
    expect(queryByText(/recovery phrase · 12 words/i)).toBeNull();
  });

  // Redaction invariant still holds after restyle.
  it('never leaks a 32+ hex run in the DOM after restyle', async () => {
    const { container } = await advanceToBackupStage();
    expect(container.innerHTML).not.toMatch(/[0-9a-f]{32,}/i);
  });
});

describe('WelcomeModal owner phrase reveal (ZEB-650 slice 2)', () => {
  const OWNER = 'a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6';
  const WORDS = [
    'abandon', 'ability', 'able', 'about', 'above', 'absent',
    'absorb', 'abstract', 'absurd', 'abuse', 'access', 'accident',
    'account', 'accuse', 'achieve', 'acid', 'acoustic', 'acquire',
    'across', 'act', 'action', 'actor', 'actress', 'actual',
  ];

  async function advanceToBackupStage() {
    mintMock.mockResolvedValue({
      state: {
        ownerId: OWNER,
        ownerDisplayName: 'x',
        devices: [],
        canBackUp: true,
      },
      recoveryToken: 'deadbeefdeadbeefdeadbeefdeadbeef0123456789abcdef0123456789abcdef',
    });
    const utils = render(WelcomeModal, { props: { open: true, onMinted: vi.fn() } });
    await fireEvent.click(utils.getByTestId('welcome-create-identity'));
    await Promise.resolve();
    await Promise.resolve();
    return utils;
  }

  it('backup stage offers the write-it-down alternative without fetching the mnemonic', async () => {
    const { getByTestId } = await advanceToBackupStage();
    expect(getByTestId('phrase-reveal-open')).toBeTruthy();
    // ZEB-768: the modal now queries identity_store_backend on mount, so IPC
    // is no longer silent at this stage — but the mnemonic reveal must still
    // not fire until the user explicitly opens it (hex-redaction invariant).
    expect(mockCoreInvoke).not.toHaveBeenCalledWith('export_owner_mnemonic_words');
  });

  it('full reveal inside the modal keeps the hex-redaction invariant', async () => {
    mockCoreInvoke.mockResolvedValue({ words: WORDS, ownerId: OWNER });
    const { getByTestId, container } = await advanceToBackupStage();
    await fireEvent.click(getByTestId('phrase-reveal-open'));
    await fireEvent.click(getByTestId('phrase-reveal-confirm'));
    await Promise.resolve();
    await Promise.resolve();
    await fireEvent.click(getByTestId('phrase-reveal-unblur'));
    expect(getByTestId('phrase-grid').querySelectorAll('li').length).toBe(24);
    // dto.ownerId (32 hex chars) must never reach the DOM.
    expect(container.innerHTML).not.toMatch(/[0-9a-f]{32,}/i);
  });

  // A phrase backup needs its own exit: Continue must complete the modal via
  // onMinted WITHOUT recording a skip (CodeRabbit PR #437).
  it('written-down checkbox surfaces Continue, which completes without skipping', async () => {
    mockCoreInvoke.mockResolvedValue({ words: WORDS, ownerId: OWNER });
    mintMock.mockResolvedValue({
      state: { ownerId: OWNER, ownerDisplayName: 'x', devices: [], canBackUp: true },
      recoveryToken: 'deadbeefdeadbeefdeadbeefdeadbeef0123456789abcdef0123456789abcdef',
    });
    const onMinted = vi.fn();
    const { getByTestId, queryByTestId } = render(WelcomeModal, {
      props: { open: true, onMinted },
    });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    await Promise.resolve();
    await Promise.resolve();
    await fireEvent.click(getByTestId('phrase-reveal-open'));
    await fireEvent.click(getByTestId('phrase-reveal-confirm'));
    await Promise.resolve();
    await Promise.resolve();
    await fireEvent.click(getByTestId('phrase-reveal-unblur'));
    // No exit for this path until the words are confirmed written down.
    expect(queryByTestId('welcome-phrase-continue')).toBeNull();
    await fireEvent.click(getByTestId('phrase-written-down'));
    await fireEvent.click(getByTestId('welcome-phrase-continue'));
    expect(onMinted).toHaveBeenCalledTimes(1);
    // Completed as a BACKUP, not a skip.
    expect(localStorage.getItem(`harmony.onboarding.backupSkipped:owner-${OWNER}`)).toBeNull();
    expect(
      localStorage.getItem(`harmony.onboarding.recoveryArtifactBackedUp:owner-${OWNER}`),
    ).toBe('true');
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

// ZEB-768 (CodeRabbit, PR #570): the backup-stage note must reflect the
// backend identity_store_backend reports — a keychain claim ONLY when the IPC
// says 'keychain', and backend-neutral wording for an unrecognized value or a
// failed call. These pin the wiring end-to-end (IPC string → normalize →
// derived note → rendered DOM text); the copy strings themselves are pinned in
// identity-backup-copy.test.ts.
describe('WelcomeModal identity-store backend copy (ZEB-768)', () => {
  const OWNER = 'a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6';

  // Drive the modal to the backup stage with identity_store_backend routed to
  // `backendReply` — a string to resolve, or an Error to reject with — and
  // wait until the derived note settles to the expected backend's copy. Every
  // other invoke (e.g. the mnemonic reveal) resolves undefined and is never
  // exercised here. Returns the actual rendered note text.
  async function backupNoteFor(
    backendReply: string | Error,
    expectedBackend: IdentityStoreBackend,
  ): Promise<string> {
    mintMock.mockResolvedValue({
      state: { ownerId: OWNER, ownerDisplayName: 'x', devices: [], canBackUp: true },
      recoveryToken: 'deadbeefdeadbeefdeadbeefdeadbeef0123456789abcdef0123456789abcdef',
    });
    mockCoreInvoke.mockImplementation((cmd: string) =>
      cmd === 'identity_store_backend'
        ? backendReply instanceof Error
          ? Promise.reject(backendReply)
          : Promise.resolve(backendReply)
        : Promise.resolve(undefined),
    );
    const { container, getByTestId } = render(WelcomeModal, {
      props: { open: true, onMinted: vi.fn() },
    });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    const expected = identityKeyBackupNote(expectedBackend);
    let actual = '';
    // waitFor settles both the mint transition (backup stage) AND the async
    // onMount backend query flowing into the derived note.
    await waitFor(() => {
      actual = container.querySelector('.keychain-note')?.textContent?.trim() ?? '';
      expect(actual).toBe(expected);
    });
    return actual;
  }

  it('shows the keychain note only when the backend reports keychain', async () => {
    const note = await backupNoteFor('keychain', 'keychain');
    expect(note.toLowerCase()).toContain('keychain');
  });

  it('shows the encrypted-file note — no keychain claim — when the backend reports encrypted-file', async () => {
    const note = await backupNoteFor('encrypted-file', 'encrypted-file');
    expect(note.toLowerCase()).toContain('encrypted file');
    expect(note.toLowerCase()).not.toContain('keychain');
  });

  it('falls back to the backend-neutral note for an unrecognized backend string', async () => {
    const note = await backupNoteFor('secret-service', 'unknown');
    expect(note.toLowerCase()).not.toContain('keychain');
  });

  it('falls back to the backend-neutral note when the backend query rejects', async () => {
    const note = await backupNoteFor(new Error('ipc down'), 'unknown');
    expect(note.toLowerCase()).not.toContain('keychain');
  });
});

// ZEB-830: onMount queries identity_store_backend BEFORE mint, when the seed's
// location isn't yet decided; mint can fall through to the encrypted file even
// with a keychain handle available. The modal must RE-QUERY after mint so the
// backup note reflects where the seed actually landed, not the stale pre-mint
// availability guess.
describe('WelcomeModal post-mint backend re-query (ZEB-830)', () => {
  const OWNER = 'a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6';

  it('re-queries after mint so the note reflects the post-mint backend, not the onMount value', async () => {
    mintMock.mockResolvedValue({
      state: { ownerId: OWNER, ownerDisplayName: 'x', devices: [], canBackUp: true },
      recoveryToken: 'deadbeefdeadbeefdeadbeefdeadbeef0123456789abcdef0123456789abcdef',
    });
    // onMount (pre-mint, 1st call) reports 'keychain'; the mint falls through to
    // the file, so the post-mint re-query (2nd call) reports 'encrypted-file'.
    const replies = ['keychain', 'encrypted-file'];
    let call = 0;
    mockCoreInvoke.mockImplementation((cmd: string) =>
      cmd === 'identity_store_backend'
        ? Promise.resolve(replies[Math.min(call++, replies.length - 1)])
        : Promise.resolve(undefined),
    );
    const { container, getByTestId } = render(WelcomeModal, {
      props: { open: true, onMinted: vi.fn() },
    });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    // The note must settle on the POST-mint (encrypted-file) copy — proving the
    // re-query overrode the stale onMount 'keychain' value.
    const expected = identityKeyBackupNote('encrypted-file');
    await waitFor(() => {
      const actual = container.querySelector('.keychain-note')?.textContent?.trim() ?? '';
      expect(actual).toBe(expected);
    });
    expect(container.querySelector('.keychain-note')?.textContent?.toLowerCase()).not.toContain(
      'keychain',
    );
    // Queried at least twice: once on mount, once after mint.
    const backendCalls = mockCoreInvoke.mock.calls.filter(([cmd]) => cmd === 'identity_store_backend');
    expect(backendCalls.length).toBeGreaterThanOrEqual(2);
  });

  it('a slow pre-mint query resolving AFTER the post-mint query cannot clobber the result', async () => {
    // The generation guard: onMount (1st call) resolves LATE with a stale
    // 'keychain'; the post-mint (2nd call) resolves immediately with the real
    // 'encrypted-file'. Without the guard, the late onMount resolution would
    // overwrite the note back to keychain — the exact race CodeRabbit/Qodo flagged.
    mintMock.mockResolvedValue({
      state: { ownerId: OWNER, ownerDisplayName: 'x', devices: [], canBackUp: true },
      recoveryToken: 'deadbeefdeadbeefdeadbeefdeadbeef0123456789abcdef0123456789abcdef',
    });
    let releaseFirst!: () => void;
    let call = 0;
    mockCoreInvoke.mockImplementation((cmd: string) => {
      if (cmd !== 'identity_store_backend') return Promise.resolve(undefined);
      call++;
      if (call === 1) {
        return new Promise((res) => {
          releaseFirst = () => res('keychain');
        });
      }
      return Promise.resolve('encrypted-file');
    });
    const { container, getByTestId } = render(WelcomeModal, {
      props: { open: true, onMinted: vi.fn() },
    });
    // Ensure the pre-mint (onMount) query is in-flight (call 1, pending) BEFORE
    // minting, so the post-mint query is deterministically call 2 — onMount
    // awaits getVersion first, so without this gate the post-mint query could
    // become the pending call and block handleCreateIdentity.
    await waitFor(() => expect(call).toBeGreaterThanOrEqual(1));
    await fireEvent.click(getByTestId('welcome-create-identity'));
    const expected = identityKeyBackupNote('encrypted-file');
    await waitFor(() => {
      expect(container.querySelector('.keychain-note')?.textContent?.trim()).toBe(expected);
    });
    // Release the stale onMount query LAST — the guard must reject it.
    releaseFirst();
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    expect(container.querySelector('.keychain-note')?.textContent?.trim()).toBe(expected);
    expect(container.querySelector('.keychain-note')?.textContent?.toLowerCase()).not.toContain(
      'keychain',
    );
  });
});
