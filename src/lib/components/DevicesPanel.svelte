<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import {
    OwnerService,
    extractError,
    type DeviceView,
    type OwnerStateView,
    type RevokeReason,
  } from '../owner-service';
  import { loadProfile } from '../profile-service';
  import {
    loadDeviceLabel,
    saveDeviceLabel,
    clearDeviceLabel,
    resolveDefaultDeviceLabel,
  } from '../device-label-service';
  import { setDevicePetname, MAX_DEVICE_PETNAME_CHARS } from '../device-petname-service';
  import { bumpFleetEpoch, requestQuorumEpochBump } from '../fleet-epoch-service';
  import { setButlerPin, extractButlerPinError } from '../butler-pin-service';
  import { fetchCommunitiesCount } from '../owner-meta';
  // ZEB-946: standalone date/time labels honor the owner's time-format prefs.
  import { formatDateOnly, formatFullTimestamp, type TimeFormatPrefs } from '../time-format';
  import { timeFormatPrefs } from '../time-format-service';
  import {
    BACKUP_FLAGS_CHANGED_EVENT,
    markRecoveryBackedUp,
    recoveryBackedUpAtMs,
  } from '../onboarding-backup-flags';
  import {
    MAX_RECOVERY_COMMENT_BYTES,
    MIN_RECOVERY_PASSPHRASE_LEN,
  } from '../recovery-policy';
  import PairingInviter from './PairingInviter.svelte';
  import PairingJoiner from './PairingJoiner.svelte';
  import Modal from './Modal.svelte';
  import OwnerRestoreWizard from './OwnerRestoreWizard.svelte';
  import OwnerPhraseReveal from './OwnerPhraseReveal.svelte';
  import RemoveDeviceDialog from './RemoveDeviceDialog.svelte';

  let svc = new OwnerService();
  let state = $state<OwnerStateView | null>(null);
  let loading = $state(true);
  let loadError = $state<string | null>(null);
  let modalOpen = $state(false);
  let mintInFlight = $state(false);
  let mintError = $state<string | null>(null);
  let recoveryToken = $state<string | null>(null);
  // ZEB-196: generation guard for in-flight token issuance. Bumped on every new
  // issue and on closeBackup, so a token whose async issuance resolves *after*
  // the modal was cancelled (or after a newer issue started) is revoked rather
  // than stored — otherwise it would linger in the bounded backend TOKEN_CACHE
  // and still LRU-evict a legitimate token. Plain (non-$state) counter: read/
  // written imperatively, never drives the UI.
  let backupTokenGen = 0;

  // ZEB-650 meta facts. communitiesCount/lastBackedUpMs are plain $state (not
  // $derived) because localStorage and the IPC aren't reactive — they're keyed
  // to the owner identity by the $effect below and updated at the exact
  // mutation points (commitBackup).
  let communitiesCount = $state<number | null>(null);
  let lastBackedUpMs = $state<number | null>(null);
  const firstEnrolledMs = $derived(
    state !== null && state.devices.length > 0
      ? Math.min(...state.devices.map((d) => d.enrolledAt)) * 1000
      : null,
  );

  // Keyed to state.ownerId — NOT run-once on mount — so an in-panel mint
  // (empty → populated) or any later owner swap re-derives both facts for the
  // NEW owner instead of showing the previous identity's (Qodo, PR #436).
  // Single source for "recompute Last-backed-up from the current owner" —
  // shared by the owner-keyed effect and the flags-changed listener below
  // (CodeRabbit PR #437).
  function refreshLastBackedUp() {
    const oid = state?.ownerId ?? null;
    lastBackedUpMs = oid ? recoveryBackedUpAtMs(oid) : null;
  }

  let metaOwnerId: string | null = null;
  $effect(() => {
    const ownerId = state?.ownerId ?? null;
    if (ownerId === metaOwnerId) return;
    metaOwnerId = ownerId;
    refreshLastBackedUp();
    communitiesCount = null;
    if (ownerId) {
      // async/await (not .then) so a unit-mocked seam returning undefined
      // can't throw; await tolerates non-promise values.
      void (async () => {
        const n = await fetchCommunitiesCount();
        // Late-async guard: publish only if this owner is still current.
        if (metaOwnerId === ownerId) {
          communitiesCount = typeof n === 'number' ? n : null;
        }
      })();
    }
  });

  // Keep "Last backed up" fresh when ANY surface marks backed-up while this
  // panel is mounted — e.g. the phrase-reveal checkbox in the modal below, or
  // the reminder banner's inline backup (ZEB-650 slice 2).
  $effect(() => {
    window.addEventListener(BACKUP_FLAGS_CHANGED_EVENT, refreshLastBackedUp);
    return () => window.removeEventListener(BACKUP_FLAGS_CHANGED_EVENT, refreshLastBackedUp);
  });

  // Per-device label (owner-private). Seeded from the store; defaulted to the
  // OS hostname on first run in onMount.
  let deviceLabel = $state<string | null>(loadDeviceLabel());

  /**
   * ZEB-336: the owner header and the this-device row are DISTINCT names.
   *
   * - Owner header ← `profile.displayName` (the owner's canonical name, also
   *   broadcast owner-keyed by profile_card_broadcast).
   * - This-device row ← the per-device LABEL store (`device-label-service`),
   *   which is owner-private and never overlaid from the owner name.
   *
   * The backend has no access to localStorage, so it returns placeholders for
   * both; we overlay each from its own local store. Defensive: a missing store
   * value leaves the backend value in place rather than blanking the field.
   */
  function applyLocalOverlay(view: OwnerStateView | null): OwnerStateView | null {
    if (!view) return null;
    const ownerName = loadProfile(view.ownerId)?.displayName;
    return {
      ...view,
      ...(ownerName ? { ownerDisplayName: ownerName } : {}),
      devices: view.devices.map((d) => {
        // ZEB-668 S4 ladder: petName ?? local label (self only, pre-S4
        // fallback) ?? backend displayName ("Device xxxxxxxx").
        //
        // `""` = explicitly CLEARED — the fleet has an opinion, so the
        // private label must NOT fill in, or a clear issued from a sibling
        // leaves this panel showing the stale name forever (Greptile P1,
        // PR #454 round 2). Only null (never named) / undefined (pre-S4
        // shape) fall back to the local label.
        const pet = d.petName?.trim();
        if (pet) return { ...d, displayName: pet };
        const fleetHasNoOpinion = d.petName === null || d.petName === undefined;
        if (fleetHasNoOpinion && d.isThisDevice && deviceLabel) {
          return { ...d, displayName: deviceLabel };
        }
        return d;
      }),
    };
  }

  svc.onChange = () => { state = applyLocalOverlay(svc.state); };

  onMount(async () => {
    try {
      await svc.refresh();
    } catch (e) {
      loadError = extractError(e);
    } finally {
      loading = false;
    }
    // ZEB-668 S4: one-shot migration — a user-set pre-S4 local label seeds
    // the fleet petname map so siblings stop showing "Device xxxxxxxx".
    // MUST run before the hostname-default block below: at this point
    // `deviceLabel` is still loadDeviceLabel()'s value, i.e. user-set only
    // (hostname defaults are never persisted and must never migrate).
    // Best-effort (node may be down); idempotent (guard is false once set).
    // `=== null` (not falsy): migrate only when the backend AFFIRMS the
    // device was never named. An absent field (undefined) means a pre-S4
    // view shape, and `""` means explicitly CLEARED — neither may trigger
    // migration (a cleared name must never be resurrected from a stale
    // local label; PR #454 round 1).
    const selfDev = svc.state?.devices.find((d) => d.isThisDevice);
    if (selfDev && selfDev.petName === null && deviceLabel) {
      try {
        await setDevicePetname(selfDev.deviceVkHex, deviceLabel);
        await svc.refresh();
      } catch {
        // Non-fatal: retried on next panel open.
      }
    }
    // ZEB-336: when this device has no user-set label, default the DISPLAY to
    // the OS hostname, resolved FRESH each launch. NOT persisted — only a user
    // rename (saveRename) writes the store — so a one-off hostname-resolution
    // hiccup can never lock in the "This device" fallback. (CodeRabbit, PR #180.)
    // Isolated from the refresh try/catch above so a hostname hiccup can't blank
    // the already-loaded devices list via the loadError banner.
    if (!deviceLabel) {
      try {
        deviceLabel = await resolveDefaultDeviceLabel();
        state = applyLocalOverlay(svc.state);
      } catch {
        // Non-fatal: keep the backend device label until the user sets one.
      }
    }
  });

  let inviterOpen = $state(false);
  let joinerOpen = $state(false);

  let backupOpen = $state(false);
  // ZEB-650 slice 2: owner recovery-phrase modal open/closed. Closing
  // unmounts OwnerPhraseReveal, which drops the word state (spec §3.2).
  let phraseOpen = $state(false);
  // ZEB-454: owner-mnemonic restore wizard open/closed.
  let restoreOpen = $state(false);
  let backupPassphrase = $state('');
  let backupPassphraseConfirm = $state('');
  let backupComment = $state('');
  // Two flags so the button label can distinguish phases (the export IPC
  // is the only "Encrypting…" phase; the OS save dialog is "Choosing
  // location…"). Both flags gate the disabled predicate; only the
  // export-phase flag gates the "Encrypting…" label.
  // (Cursor Bugbot, PR #66 review.)
  let backupDialogInFlight = $state(false);
  let backupInFlight = $state(false);
  let backupError = $state<string | null>(null);
  let backupSavedPath = $state<string | null>(null);

  let renamingDeviceId = $state<string | null>(null);
  let renameDraft = $state('');

  function startRename(device: { deviceId: string; displayName: string; isThisDevice: boolean }) {
    renameError = null;
    renamingDeviceId = device.deviceId;
    renameDraft = device.displayName;
  }

  let renameError = $state<string | null>(null);
  let renameInFlight = $state(false);

  // ZEB-668 S5: fleet key rotation (seed-holder action; the staleness
  // signal itself is backend-proven — any device removal newer than the
  // last rotation).
  let rotateInFlight = $state(false);
  let rotateError = $state<string | null>(null);

  async function rotateFleetKeys() {
    if (rotateInFlight) return;
    rotateError = null;
    rotateInFlight = true;
    try {
      await bumpFleetEpoch();
      await svc.refresh();
    } catch (e) {
      rotateError = extractError(e);
    } finally {
      rotateInFlight = false;
    }
  }

  // ZEB-677 S5: master-less rotation. A device without the master seed opens a
  // K=2 quorum request; a sibling co-signs and the initiator's sweep installs
  // the new epoch. Distinct in-flight/error state from the seed-holder path.
  let quorumRotateInFlight = $state(false);
  let quorumRotateError = $state<string | null>(null);

  async function requestQuorumRotate() {
    if (quorumRotateInFlight) return;
    quorumRotateError = null;
    quorumRotateInFlight = true;
    try {
      await requestQuorumEpochBump();
      await svc.refresh();
    } catch (e) {
      quorumRotateError = extractError(e);
    } finally {
      quorumRotateInFlight = false;
    }
  }

  // ZEB-668 S4: rename any row via the fleet-synced petname IPC. Empty
  // input is an explicit CLEAR (backend LWW tombstone `name: ""` — PR #454
  // round 1). The self row ALSO keeps the ZEB-336 localStorage label in
  // step — saved on rename, removed on clear — so the local fallback can
  // never resurrect a cleared name. (Pre-S4 this only wrote localStorage,
  // so siblings never saw the name.)
  async function saveRename(device: DeviceView) {
    if (renameInFlight) return;
    const trimmed = renameDraft.trim();
    // Code points, not UTF-16 units — matches the backend's chars().count()
    // so non-ASCII names hit the same cap on both sides (PR #454 round 1).
    if ([...trimmed].length > MAX_DEVICE_PETNAME_CHARS) {
      renameError = `Name is too long (max ${MAX_DEVICE_PETNAME_CHARS} characters).`;
      return;
    }
    renameError = null;
    renameInFlight = true;
    try {
      await setDevicePetname(device.deviceVkHex, trimmed);
      if (device.isThisDevice) {
        // ZEB-336: never the owner profile — only the per-device label.
        if (trimmed) {
          saveDeviceLabel(trimmed);
          deviceLabel = trimmed;
        } else {
          clearDeviceLabel();
          deviceLabel = null;
        }
      }
      renamingDeviceId = null;
      await svc.refresh();
    } catch (e) {
      renameError = extractError(e);
    } finally {
      renameInFlight = false;
    }
  }

  function cancelRename() {
    renameError = null;
    renamingDeviceId = null;
  }

  // ZEB-418 P2 D17: butler pin toggle. Single-select: toggling ON pins the
  // device; toggling the already-pinned device OFF clears the pin. Low-risk
  // action (advisory ordering, always reversible) — no confirmation tier.
  let butlerPinError = $state<string | null>(null);
  let butlerPinInFlight = $state(false);

  async function handleButlerPinToggle(device: { deviceVkHex: string; butlerPinned: boolean }) {
    if (butlerPinInFlight) return;
    butlerPinError = null;
    butlerPinInFlight = true;
    try {
      // If already pinned, toggle OFF (clear); otherwise pin this device.
      // Round-2 Greptile P1: the backend validates against the enrolled set
      // of 64-hex VERIFY-KEY ids (SP1 form) — `deviceVkHex`, never
      // `deviceId` (the identity-hash form, which is always rejected).
      const newPin = device.butlerPinned ? null : device.deviceVkHex;
      await setButlerPin(newPin);
      // Re-fetch the device list so butler_pinned reflects the new state.
      await svc.refresh();
    } catch (e) {
      butlerPinError = extractButlerPinError(e);
    } finally {
      butlerPinInFlight = false;
    }
  }

  // ZEB-668 S2: device revoke. Active rows render normally; revoked rows
  // move to the collapsed "Removed devices" section (real RevocationSet
  // data — honesty rule).
  const activeDevices = $derived((state?.devices ?? []).filter((d) => !d.revoked));
  const removedDevices = $derived(
    // .filter() already yields a fresh array, so in-place .sort() is safe —
    // and unlike ES2023 .toSorted(), it works on older system WebViews.
    (state?.devices ?? [])
      .filter((d) => d.revoked)
      .sort((a, b) => (b.revokedAt ?? 0) - (a.revokedAt ?? 0)),
  );
  let removedOpen = $state(false);
  let removeTarget = $state<DeviceView | null>(null);
  let removeInFlight = $state(false);
  let removeError = $state<string | null>(null);

  async function handleRemoveConfirm(reason: RevokeReason) {
    if (!removeTarget || removeInFlight) return;
    removeError = null;
    removeInFlight = true;
    try {
      // ZEB-677 S3: master-less path — write a co-sign request instead of
      // revoking directly. The pending banner takes over as the surface;
      // the revocation lands when a sibling approves.
      if (!state?.canBackUp && removeTarget.quorumRemovable) {
        await svc.requestQuorumRevocation(removeTarget.deviceVkHex, reason);
      } else {
        await svc.revoke(removeTarget.deviceVkHex, reason);
      }
      // A self-revoke also fires device-revoked-self → App-level terminal
      // state; the refresh below just keeps this panel honest meanwhile.
      await svc.refresh();
      removeTarget = null;
    } catch (e) {
      removeError = extractError(e);
    } finally {
      removeInFlight = false;
    }
  }

  // ZEB-677 S3: quorum co-sign banner state. The in-flight id gates ALL
  // banners (one ceremony action at a time — deliberate) and labels the
  // busy button.
  let quorumActionInFlight = $state<string | null>(null);
  let quorumError = $state<string | null>(null);

  /** Petname/display-name join for banner copy; 8-hex prefix fallback. */
  function quorumDeviceName(deviceId: string): string {
    const row = state?.devices.find((d) => d.deviceId === deviceId);
    return row?.petName?.trim() || row?.displayName || deviceId.slice(0, 8);
  }

  async function handleQuorumAction(requestId: string, action: 'approve' | 'decline') {
    if (quorumActionInFlight) return;
    quorumError = null;
    quorumActionInFlight = requestId;
    try {
      if (action === 'approve') {
        await svc.cosignQuorumRequest(requestId);
      } else {
        await svc.declineQuorumRequest(requestId);
      }
      await svc.refresh();
    } catch (e) {
      quorumError = extractError(e);
    } finally {
      quorumActionInFlight = null;
    }
  }

  // ZEB-677 S4: quorum-enrollment arm surface. Shares the S3 in-flight/error
  // channels. The sentinel id occupies quorumActionInFlight the same way a
  // requestId does — one ceremony action at a time. It can never collide with
  // a real requestId (which is hex).
  const ARM_ENROLLMENT_ACTION = 'arm-enrollment';

  // Live-ticking clock for the arm countdown. The DEADLINE is the backend
  // `quorumArmedUntilMs` cell (the authority); `now` only drives the display's
  // per-second recompute. Interval is created/cleared by the $effect below
  // (gated on the backend deadline, never on the now-derived flag — reading
  // `now` inside that effect would loop with the interval's write).
  let now = $state(Date.now());
  let armCountdownTimer: ReturnType<typeof setInterval> | null = null;

  function clearArmCountdown() {
    if (armCountdownTimer !== null) {
      clearInterval(armCountdownTimer);
      armCountdownTimer = null;
    }
  }

  const enrollmentArmed = $derived(
    typeof state?.quorumArmedUntilMs === 'number' && state.quorumArmedUntilMs > now,
  );
  const armCountdown = $derived(
    formatCountdown((state?.quorumArmedUntilMs ?? 0) - now),
  );

  $effect(() => {
    // Tick only while the backend reports an arm deadline. On disarm/expiry
    // the cell clears (via the owner-quorum-updated refresh) → interval stops.
    if (typeof state?.quorumArmedUntilMs === 'number') {
      now = Date.now();
      if (armCountdownTimer === null) {
        armCountdownTimer = setInterval(() => { now = Date.now(); }, 1000);
      }
    } else {
      clearArmCountdown();
    }
  });
  onDestroy(clearArmCountdown);

  function formatCountdown(ms: number): string {
    const total = Math.max(0, Math.ceil(ms / 1000));
    const m = Math.floor(total / 60);
    const s = total % 60;
    return `${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
  }

  async function handleArmEnrollment() {
    if (quorumActionInFlight) return;
    quorumError = null;
    quorumActionInFlight = ARM_ENROLLMENT_ACTION;
    try {
      await svc.armEnrollment();
      // owner-quorum-updated also refreshes; this keeps the panel honest even
      // if the event is dropped in a headless/test context.
      await svc.refresh();
    } catch (e) {
      quorumError = extractError(e);
    } finally {
      quorumActionInFlight = null;
    }
  }

  async function handleDisarmEnrollment() {
    if (quorumActionInFlight) return;
    quorumError = null;
    quorumActionInFlight = ARM_ENROLLMENT_ACTION;
    try {
      await svc.disarmEnrollment();
      await svc.refresh();
    } catch (e) {
      quorumError = extractError(e);
    } finally {
      quorumActionInFlight = null;
    }
  }

  // ZEB-668 S6: replace = typed-confirm revoke (reason locked to
  // "decommissioned") + immediately launch inviter pairing; the old row's
  // fleet petname carries to the successor at pairing completion.
  let replaceTarget = $state<DeviceView | null>(null);
  let replaceInFlight = $state(false);
  let replaceError = $state<string | null>(null);
  // Captured at confirm; null = nothing to carry. Cleared by the carry
  // attempt or by closing the pairing modal un-completed (the replace
  // honestly degrades to a plain remove — the revoke already landed).
  let pendingCarryPetname = $state<string | null>(null);
  let carryError = $state<string | null>(null);

  async function handleReplaceConfirm() {
    if (!replaceTarget || replaceInFlight) return;
    replaceError = null;
    replaceInFlight = true;
    try {
      const carry = replaceTarget.petName?.trim() || null;
      // Revoke BEFORE pairing: the master path bumps the fleet epoch, so
      // the inviter seals the successor into the post-revocation window,
      // never the compromised one.
      await svc.revoke(replaceTarget.deviceVkHex, 'decommissioned');
      // The revoke is the only hard gate. Once it lands, pairing MUST
      // open — gating on the refresh strands the user mid-flow after an
      // irreversible removal, and a retry would re-revoke an
      // already-revoked device (Qodo + CodeRabbit, PR #456 round 1).
      pendingCarryPetname = carry;
      carryError = null;
      replaceTarget = null;
      inviterOpen = true;
      try {
        await svc.refresh();
      } catch {
        // Best-effort: owner-devices-updated or the next panel refresh
        // catches this row up.
      }
    } catch (e) {
      replaceError = extractError(e);
    } finally {
      replaceInFlight = false;
    }
  }

  async function handlePairingComplete(deviceIdHex: string) {
    const carry = pendingCarryPetname;
    pendingCarryPetname = null;
    // A stale alert from an earlier failed carry must not survive an
    // unrelated pairing's completion (CodeRabbit, PR #456 round 1).
    carryError = null;
    // The successor row must be visible to map its 32-hex deviceId to the
    // 64-hex vk the petname IPC keys on (PairingInviter folded the
    // enrollment into the resident doc before firing onComplete). Two
    // bounded attempts: a transiently-failed first refresh must not lose
    // the carry (Qodo, PR #456 round 1) — but no background retry
    // machinery; on final miss we degrade to the honest alert, which
    // names the value so a manual rename recovers it.
    if (!carry) {
      try {
        await svc.refresh();
      } catch {
        // Best-effort; owner-devices-updated re-refreshes.
      }
      return;
    }
    let successor: DeviceView | undefined;
    for (let attempt = 0; attempt < 2 && !successor; attempt += 1) {
      try {
        await svc.refresh();
      } catch {
        // Best-effort; the second attempt (or owner-devices-updated)
        // covers a transient failure.
      }
      successor = state?.devices.find((d) => d.deviceId === deviceIdHex);
    }
    if (!successor) {
      carryError = `Couldn't carry the name "${carry}" to the new device — rename it manually.`;
      return;
    }
    try {
      await setDevicePetname(successor.deviceVkHex, carry);
      await svc.refresh();
    } catch (e) {
      carryError = `Couldn't carry the name "${carry}" to the new device — rename it manually. (${extractError(e)})`;
    }
  }

  // Live refresh when trust state changes under us (S1 replication: a
  // sibling's revoke/enrollment arrives via the trust engine's detector).
  // ZEB-677 S3: owner-quorum-updated joins it — a co-sign request arriving
  // or completing must surface without a panel reopen.
  let unlistenDevicesUpdated: (() => void) | null = null;
  let unlistenQuorumUpdated: (() => void) | null = null;
  // Teardown of a Tauri unlisten handle must never throw mid-sequence: if the
  // first handle threw, the second would leak (stale refresh callbacks firing
  // after unmount). Swallow — the listener is being discarded regardless.
  function safeUnlisten(fn: (() => void) | null | undefined) {
    try {
      fn?.();
    } catch {
      /* ignore — teardown errors are non-fatal */
    }
  }
  onMount(() => {
    let cancelled = false;
    const refresh = () => {
      svc.refresh().catch(() => {
        // Live refresh is best-effort; a stale roster self-heals on the
        // next event or panel open.
      });
    };
    listen('owner-devices-updated', refresh)
      .then((unlisten) => {
        if (cancelled) {
          safeUnlisten(unlisten);
          return;
        }
        unlistenDevicesUpdated = unlisten;
      })
      .catch(() => {
        // Registration can fail in tests / headless — non-fatal.
      });
    listen('owner-quorum-updated', refresh)
      .then((unlisten) => {
        if (cancelled) {
          safeUnlisten(unlisten);
          return;
        }
        unlistenQuorumUpdated = unlisten;
      })
      .catch(() => {
        // Registration can fail in tests / headless — non-fatal.
      });
    return () => {
      cancelled = true;
    };
  });
  onDestroy(() => {
    safeUnlisten(unlistenDevicesUpdated);
    unlistenDevicesUpdated = null;
    safeUnlisten(unlistenQuorumUpdated);
    unlistenQuorumUpdated = null;
  });

  async function openBackup() {
    backupOpen = true;
    backupPassphrase = '';
    backupPassphraseConfirm = '';
    backupComment = '';
    backupError = null;
    backupSavedPath = null;
    // Token-reuse policy: if a token is already in hand (e.g., handleConfirmMint
    // just minted and stashed one), use it. Otherwise issue a fresh one. This
    // preserves the happy mint→backup path (one token, one consume) while still
    // healing post-failure paths because commitBackup nulls the token in finally
    // (whether export succeeded or failed) — so a second openBackup always
    // re-issues. Tokens ARE TTL-bounded (5min) and LRU-evictable, so a stale
    // post-mint token will surface as "expired or invalid" on commit, which is
    // recoverable via the inline Retry button.
    if (recoveryToken !== null) return;
    await issueTokenGuarded();
  }

  async function retryIssueToken() {
    backupError = null;
    await issueTokenGuarded();
  }

  // ZEB-196: issue a recovery token, guarding against the modal being cancelled
  // (or a newer issue starting) while the async issuance is in flight. If the
  // generation has moved on by the time the token arrives — closeBackup bumps
  // it, as does a subsequent issue — the modal is no longer waiting for this
  // token: store nothing and revoke the orphan (fire-and-forget) so it can't
  // linger in the backend cache and LRU-evict a legitimate token.
  async function issueTokenGuarded() {
    const myGen = ++backupTokenGen;
    try {
      const token = await svc.issueRecoveryToken();
      if (myGen !== backupTokenGen || !backupOpen) {
        void svc.revokeRecoveryToken(token).catch(() => {});
        return;
      }
      recoveryToken = token;
    } catch (e) {
      // Only surface the error if this issuance is still the current one; a
      // failure from an abandoned (cancelled) issue must not stamp the UI.
      if (myGen === backupTokenGen) backupError = extractError(e);
    }
  }

  async function commitBackup() {
    // Function-level reentrancy guard: even with the Save backup button
    // gated on the in-flight flags, two click handlers can be queued in
    // the same event-loop turn before Svelte's reactivity propagates
    // `disabled` to the DOM. Symmetric with IdentityPanel's
    // advanceFromFileEntry guard. (CodeRabbit, PR #66 review.)
    if (backupDialogInFlight || backupInFlight) return;
    // Capture before the awaits — `state` could change underneath us, and the
    // backed-up flag must name the identity actually exported (ZEB-587 pattern).
    const backupOwnerId = state?.ownerId ?? null;
    // Clear any error from a prior commit attempt in this same modal session
    // BEFORE re-validating. Without this, a stale error string from the
    // previous click can render alongside (or instead of) the current
    // validation outcome — e.g., user fixes a passphrase mismatch but the
    // "Passphrases do not match" string lingers because no early-return path
    // resets backupError. The token/dialog/export path below has its own
    // backupError = null (line above the export try block); this guards the
    // pre-validation early-return paths.
    backupError = null;
    if (recoveryToken === null) {
      backupError = 'No recovery token available.';
      return;
    }
    if (backupPassphrase !== backupPassphraseConfirm) {
      backupError = 'Passphrases do not match.';
      return;
    }
    // Count Unicode codepoints, not UTF-16 code units, so the check matches
    // the Rust backend's `passphrase.chars().count()` for multibyte input
    // (emoji, CJK). Spreading a string yields one element per codepoint.
    if ([...backupPassphrase].length < MIN_RECOVERY_PASSPHRASE_LEN) {
      backupError = `Recovery passphrase must be at least ${MIN_RECOVERY_PASSPHRASE_LEN} characters.`;
      return;
    }
    // Comment cap is BYTES (matches harmony-owner's hard 256-byte limit on
    // the underlying field). The maxlength={256} attribute on the input is
    // a UI hint counting characters, which over-permits for multibyte input;
    // enforce the byte cap explicitly here so the backend never rejects.
    //
    // Validate the SAME string we send to the backend — backupComment.trim().
    // Validating the raw (untrimmed) string falsely rejects comments where
    // leading/trailing whitespace pushes the byte count past 256 even though
    // the trimmed form fits.
    const trimmedComment = backupComment.trim();
    const commentBytes = new TextEncoder().encode(trimmedComment).length;
    if (commentBytes > MAX_RECOVERY_COMMENT_BYTES) {
      backupError = `Comment must be at most ${MAX_RECOVERY_COMMENT_BYTES} bytes (currently ${commentBytes}).`;
      return;
    }
    // Mark dialog-in-flight BEFORE the save dialog opens so a fast double-
    // click on Save backup cannot queue a second dialog and consume the
    // single-use recovery token twice (CodeRabbit, PR #66 review).
    // The two flags are deliberately separate: backupDialogInFlight gates
    // disable; only backupInFlight (the export-phase flag) drives the
    // "Encrypting…" label, which would otherwise lie during the dialog
    // phase (Cursor Bugbot, PR #66 review).
    backupDialogInFlight = true;
    let pathToken: string | null;
    try {
      pathToken = await svc.requestExportSavePath({
        defaultFilename: 'owner-recovery.bin',
        filterName: 'Recovery file',
        filterExtensions: ['bin'],
      });
    } catch (e) {
      backupError = extractError(e);
      return;
    } finally {
      backupDialogInFlight = false;
    }
    if (pathToken === null) return;  // user cancelled
    backupInFlight = true;
    try {
      const info = await svc.exportRecoveryFile(
        recoveryToken,
        pathToken,
        backupPassphrase,
        trimmedComment ? trimmedComment : null,
      );
      // Wipe passphrase fields immediately on success — shortens secret
      // retention in the renderer between this point and closeBackup
      // (CodeRabbit, PR #66 review). closeBackup also wipes them; this
      // is belt-and-braces.
      backupPassphrase = '';
      backupPassphraseConfirm = '';
      backupSavedPath = info.path;
      // ZEB-650 gap fix: a Devices-panel backup now clears the reminder banner,
      // exactly like BackupReminderBanner's own save path.
      if (backupOwnerId) {
        markRecoveryBackedUp(backupOwnerId);
        lastBackedUpMs = recoveryBackedUpAtMs(backupOwnerId);
      }
    } catch (e) {
      backupError = extractError(e);
    } finally {
      // Token is single-use server-side: take_token consumes it on any path
      // past validation, including disk-write failures. Always null it so
      // the next openBackup() call issues a fresh token instead of replaying
      // a stale one (which would error "expired or invalid").
      recoveryToken = null;
      backupInFlight = false;
    }
  }

  function closeBackup() {
    backupOpen = false;
    // ZEB-196: invalidate any in-flight token issuance so a token that resolves
    // after this close is revoked (not stored) by issueTokenGuarded.
    backupTokenGen++;
    // ZEB-196: proactively revoke the server-side token on cancel so an
    // issued-but-unconsumed token can't linger for its 5-minute TTL and
    // LRU-evict a legitimate live token (e.g. a freshly minted one). Capture
    // the reference BEFORE nulling; fire-and-forget (don't block the modal
    // close on the round-trip) and swallow errors — the token TTL-expires
    // anyway, so there is nothing the user could act on. On the happy export
    // path commitBackup has already nulled the token, so this is skipped and no
    // wasted revoke is issued.
    const tokenToRevoke = recoveryToken;
    if (tokenToRevoke !== null) {
      void svc.revokeRecoveryToken(tokenToRevoke).catch(() => {});
    }
    // Tokens are single-use server-side; don't carry across opens.
    recoveryToken = null;
    // Wipe sensitive passphrase material from component state instead of
    // letting it linger between modal sessions. JS strings are immutable so
    // we can't actually zero the underlying buffer (the original allocations
    // remain in V8's heap until GC), but dropping our references at least
    // makes them eligible for collection rather than holding them indefinitely
    // across the panel's lifetime.
    backupPassphrase = '';
    backupPassphraseConfirm = '';
    backupComment = '';
    backupError = null;
  }

  function formatOwnerFingerprint(hex: string): string {
    // 32 hex chars (16 bytes) → eight groups of 4 hex chars separated by ·.
    // Renders the FULL 16-byte owner identity hash so users can disambiguate
    // their own owner identity from another's (truncating to 8 of 16 bytes
    // would only cover half the entropy and weaken visual disambiguation).
    if (hex.length < 32) return hex;
    return hex.match(/.{4}/g)!.slice(0, 8).join('·');
  }

  function deviceInitial(name: string): string {
    return name.trim().charAt(0).toUpperCase() || '?';
  }

  function formatEnrolledAt(ts: number, prefs: TimeFormatPrefs): string {
    const ms = ts * 1000;
    const now = Date.now();
    const ageDays = Math.floor((now - ms) / (1000 * 60 * 60 * 24));
    if (ageDays < 1) return 'today';
    if (ageDays < 2) return 'yesterday';
    if (ageDays < 30) return `${ageDays}d ago`;
    return formatDateOnly(ms, prefs);
  }

  // ZEB-668 S4: heartbeat-tolerant relative time. The stamp cadence is
  // ~7.5 min (fleet-net re-stamp tick) — hence "just now" out to 10 min and
  // the "~" prefix on minute/hour buckets (honesty ledger: last seen = last
  // fleet heartbeat, not live presence).
  function formatLastSeen(ms: number, prefs: TimeFormatPrefs): string {
    const min = Math.floor((Date.now() - ms) / 60000);
    if (min < 10) return 'just now';
    if (min < 60) return `~${min}m ago`;
    const h = Math.floor(min / 60);
    if (h < 24) return `~${h}h ago`;
    const d = Math.floor(h / 24);
    if (d < 30) return `${d}d ago`;
    return formatDateOnly(ms, prefs);
  }

  /** ZEB-721: coarse "~Nm / ~Nh / ~Nd" rendering of a clock-regression skew (seconds). */
  function formatApproxDuration(secs: number): string {
    const m = Math.floor(secs / 60);
    if (m < 60) return `~${Math.max(1, m)}m`;
    const h = Math.floor(m / 60);
    if (h < 24) return `~${h}h`;
    const d = Math.floor(h / 24);
    return `~${d}d`;
  }

  async function handleConfirmMint() {
    mintInFlight = true;
    mintError = null;
    try {
      const result = await svc.mint();
      recoveryToken = result.recoveryToken;
      modalOpen = false;
    } catch (e) {
      mintError = extractError(e);
    } finally {
      mintInFlight = false;
    }
  }
</script>

<section class="devices-panel" aria-labelledby="devices-heading">
  <h2 id="devices-heading">Devices</h2>

  {#if loading}
    <p class="loading">Loading…</p>
  {:else if loadError}
    <p class="error" role="alert">Failed to load: {loadError}</p>
  {:else if state === null}
    <div class="empty">
      <p class="explainer">
        You haven't created an owner identity yet. Either start a new one for this
        device, or join an existing one already running on another of your devices.
      </p>
      <div class="empty-actions">
        <button class="primary" onclick={() => { modalOpen = true; }}>
          Bind this device to a new owner identity →
        </button>
        <button class="secondary" onclick={() => { joinerOpen = true; }}>
          Join existing identity →
        </button>
      </div>
    </div>
  {:else}
    <div class="populated">
      <!-- ① Owner identity header -->
      <div class="owner-header">
        <div class="label">OWNER IDENTITY</div>
        <div class="owner-row">
          <div class="owner-avatar" aria-hidden="true">{deviceInitial(state.ownerDisplayName)}</div>
          <div class="owner-identity">
            <div class="owner-name-row">
              <span class="owner-name">{state.ownerDisplayName}</span>
              <span class="ss-badge">● self-sovereign</span>
            </div>
            <div class="owner-fingerprint">{formatOwnerFingerprint(state.ownerId)}</div>
            <div class="owner-meta" data-testid="devices-meta">
              <span class="meta-key" data-testid="devices-meta-keytype">ed25519</span>
              {#if firstEnrolledMs !== null}
                <span data-testid="devices-meta-enrolled">First device enrolled {formatDateOnly(firstEnrolledMs, $timeFormatPrefs)}</span>
              {/if}
              {#if communitiesCount !== null}
                <span data-testid="devices-meta-communities">{communitiesCount} {communitiesCount === 1 ? 'community' : 'communities'}</span>
              {/if}
            </div>
            {#if lastBackedUpMs !== null}
              <div class="owner-meta" data-testid="devices-last-backed-up">Last backed up {formatDateOnly(lastBackedUpMs, $timeFormatPrefs)}</div>
            {/if}
          </div>
          <button
            class="primary"
            disabled={!state.canBackUp}
            title={state.canBackUp ? '' : 'Master seed not on this device — backup is no longer possible.'}
            onclick={openBackup}
          >
            Back up owner identity →
          </button>
          <button
            class="secondary"
            data-testid="devices-view-phrase"
            disabled={!state.canBackUp}
            title={state.canBackUp ? '' : 'Master seed not on this device — the recovery phrase can no longer be shown.'}
            onclick={() => { phraseOpen = true; }}
          >
            View recovery phrase
          </button>
        </div>
        <!-- ZEB-454: re-adopt this owner identity from its 24-word recovery
             phrase (the GUI analog of `restore owner-mnemonic`). -->
        <button
          class="restore-link"
          data-testid="devices-restore-mnemonic"
          onclick={() => { restoreOpen = true; }}
        >
          Restore from recovery phrase…
        </button>
      </div>
      {#if restoreOpen}
        <!-- On success, reload so a fresh start_node loads the re-minted
             owner_state (mirrors the pairing-join completion path). -->
        <OwnerRestoreWizard
          currentOwnerId={state.ownerId}
          onRestored={() => location.reload()}
          onCancel={() => { restoreOpen = false; }}
        />
      {/if}

      <!-- ZEB-721: regressed host clock. This device's own liveness cert is
           stamped in the future, so renewal is paused and peers will eventually
           see it age out. Informational only — the remedy is fixing the clock. -->
      {#if typeof state.selfClockRegressedSkewSecs === 'number' && state.selfClockRegressedSkewSecs > 0}
        <div class="epoch-banner" data-testid="clock-regressed-banner" role="alert">
          <p class="epoch-text">
            This device's clock appears to have moved backwards ({formatApproxDuration(
              state.selfClockRegressedSkewSecs
            )} behind its last check-in). Liveness renewal is paused until the clock is
            corrected — re-sync system time (NTP) to restore trust freshness on your other
            devices.
          </p>
        </div>
      {/if}

      <!-- ZEB-668 S5: fleet-key staleness. Rendered only on the backend's
           proof (a removal newer than the last rotation); the button only
           where the bump IPC can succeed (seed present) — honesty rule. -->
      {#if state.fleetEpochStale}
        <div class="epoch-banner" data-testid="fleet-epoch-banner">
          {#if state.canBackUp}
            <p class="epoch-text">
              Fleet keys predate the last device removal. Removed devices can
              still read fleet data until you rotate.
            </p>
            <button
              class="secondary"
              data-testid="rotate-fleet-keys"
              disabled={rotateInFlight}
              onclick={rotateFleetKeys}
            >
              {rotateInFlight ? 'Rotating…' : 'Rotate fleet keys'}
            </button>
            {#if rotateError}
              <p class="error" role="alert">{rotateError}</p>
            {/if}
          {:else}
            <p class="epoch-text">
              Fleet keys predate the last device removal. Rotate them from the
              device that holds your master key.
            </p>
            {#if state.canArmEnrollment === true}
              <!-- ZEB-677 S5: no master seed available? A K=2 quorum across
                   your other Master-certed devices can rotate instead. -->
              <p class="epoch-text">
                Lost your master device? Your other devices can co-sign the
                rotation — until it lands, a removed device may still read
                fleet-synced data.
              </p>
              <button
                class="secondary"
                data-testid="rotate-fleet-keys-quorum"
                disabled={quorumRotateInFlight}
                onclick={requestQuorumRotate}
              >
                {quorumRotateInFlight ? 'Requesting…' : 'Rotate fleet keys (needs a co-sign)'}
              </button>
              {#if quorumRotateError}
                <p class="error" role="alert">{quorumRotateError}</p>
              {/if}
            {/if}
          {/if}
        </div>
      {/if}

      <!-- ZEB-677 S3: quorum co-sign surfaces. Approve is click-confirm —
           the destructive typed-confirm already happened on the initiating
           device (spec §4.2). -->
      {#each (state.quorumRequests ?? []).filter((r) => r.canCosign) as req (req.requestId)}
        <div class="epoch-banner quorum-banner" data-testid="quorum-cosign-banner">
          <p class="epoch-text">
            Co-sign request from <strong>{quorumDeviceName(req.initiatorDeviceId)}</strong>: remove
            <strong>{quorumDeviceName(req.targetDeviceId)}</strong> ({req.reason})
          </p>
          <div class="quorum-actions">
            <button
              class="primary"
              data-testid="quorum-approve"
              disabled={quorumActionInFlight !== null}
              onclick={() => handleQuorumAction(req.requestId, 'approve')}
            >
              {quorumActionInFlight === req.requestId ? 'Signing…' : 'Approve'}
            </button>
            <button
              class="secondary"
              data-testid="quorum-decline"
              disabled={quorumActionInFlight !== null}
              onclick={() => handleQuorumAction(req.requestId, 'decline')}
            >
              Decline
            </button>
          </div>
        </div>
      {/each}
      {#each (state.quorumRequests ?? []).filter((r) => r.initiatedByMe) as req (req.requestId)}
        <div class="epoch-banner quorum-banner" data-testid="quorum-pending-note">
          <p class="epoch-text">
            {#if req.declined}
              Removal of <strong>{quorumDeviceName(req.targetDeviceId)}</strong> was declined by
              another device.
            {:else if req.cosignerSigned}
              Removal of <strong>{quorumDeviceName(req.targetDeviceId)}</strong> was co-signed —
              finishing up.
            {:else}
              Waiting for another device to co-sign the removal of
              <strong>{quorumDeviceName(req.targetDeviceId)}</strong> — expires
              {formatFullTimestamp(req.expiresAtMs, $timeFormatPrefs)}.
            {/if}
          </p>
        </div>
      {/each}

      <!-- ZEB-677 S4: quorum-enrollment arm surface. Rendered ONLY where the
           backend affirms a ≥2-active-Master fleet (canArmEnrollment) — a
           fleet that can't co-sign an enrollment must see no false promise
           (spec §8 honesty rule). Click-confirm tier: arming is reversible. -->
      {#if state.canArmEnrollment === true}
        {#if enrollmentArmed}
          <div class="epoch-banner quorum-banner" data-testid="quorum-arm-active">
            <p class="epoch-text">
              This device is approving one new device enrollment —
              <strong data-testid="quorum-arm-countdown">{armCountdown}</strong> remaining.
            </p>
            <div class="quorum-actions">
              <button
                class="secondary"
                data-testid="quorum-arm-cancel"
                disabled={quorumActionInFlight !== null}
                onclick={handleDisarmEnrollment}
              >
                {quorumActionInFlight === ARM_ENROLLMENT_ACTION ? 'Cancelling…' : 'Cancel'}
              </button>
            </div>
          </div>
        {:else}
          <div class="epoch-banner quorum-banner" data-testid="quorum-arm-surface">
            <p class="epoch-text">
              For the next 15 minutes this device will approve one new device
              enrollment started from your other devices.
            </p>
            <div class="quorum-actions">
              <button
                class="primary"
                data-testid="quorum-arm-button"
                disabled={quorumActionInFlight !== null}
                onclick={handleArmEnrollment}
              >
                {quorumActionInFlight === ARM_ENROLLMENT_ACTION ? 'Approving…' : 'Approve adding a device'}
              </button>
            </div>
          </div>
        {/if}
      {/if}
      {#if quorumError}
        <p class="error" role="alert">{quorumError}</p>
      {/if}

      <!-- ② Devices list -->
      <div class="devices-list">
        <div class="label">MY DEVICES ({activeDevices.length})</div>
        {#each activeDevices as device (device.deviceId)}
          <div class="device-row">
            <div class="device-icon">{deviceInitial(device.displayName)}</div>
            <div class="device-meta">
              <div class="device-name-row">
                {#if renamingDeviceId === device.deviceId}
                  <input
                    type="text"
                    bind:value={renameDraft}
                    aria-label="Device name"
                    onkeydown={(e) => {
                      if (e.key === 'Enter') saveRename(device);
                      if (e.key === 'Escape') cancelRename();
                    }}
                  />
                  <button class="secondary" disabled={renameInFlight} onclick={() => saveRename(device)}>Save</button>
                  <button class="secondary" onclick={cancelRename}>Cancel</button>
                {:else}
                  <span class="device-name">{device.displayName}</span>
                  {#if device.isThisDevice}
                    <span class="this-device-marker">this device</span>
                    <button class="rename-btn" onclick={() => startRename(device)}>Rename</button>
                    <button
                      class="remove-btn"
                      onclick={() => {
                        removeError = null;
                        removeTarget = device;
                      }}
                    >
                      Remove this device
                    </button>
                  {:else}
                    <!-- ZEB-668 S4: petnames are fleet-synced, so siblings
                         are renamable from any device. -->
                    <button class="rename-btn" onclick={() => startRename(device)}>Rename</button>
                    {#if state.canBackUp}
                      <!-- Honesty rule: the sibling affordances render only
                           where the IPC can succeed (master seed present). -->
                      <!-- ZEB-668 S6: replace = remove + re-pair + petname
                           carry-over; same severity as remove (the removal
                           is immediate), so the same button treatment.
                           Master-only: re-pairing needs the seed (the S4
                           arm flow is the master-less enrollment story). -->
                      <button
                        class="remove-btn"
                        onclick={() => {
                          replaceError = null;
                          replaceTarget = device;
                        }}
                      >
                        Replace…
                      </button>
                    {/if}
                    {#if state.canBackUp || device.quorumRemovable}
                      <!-- ZEB-677 S3: also visible on master-less devices
                           when a sibling can co-sign (quorumRemovable is
                           computed server-side, spec §4.1). -->
                      <button
                        class="remove-btn"
                        onclick={() => {
                          removeError = null;
                          removeTarget = device;
                        }}
                      >
                        Remove…
                      </button>
                    {/if}
                  {/if}
                {/if}
              </div>
              <div class="device-secondary">
                {#if device.trustDecision.kind === 'full'}
                  <span class="trust-badge full">● trusted</span>
                {:else if device.trustDecision.kind === 'provisional'}
                  <span class="trust-badge provisional">● provisional</span>
                {:else}
                  <span class="trust-badge refused">● refused</span>
                {/if}
                <span class="separator">·</span>
                <span>added {formatEnrolledAt(device.enrolledAt, $timeFormatPrefs)}</span>
                <span class="separator">·</span>
                <span class="fingerprint">{device.fingerprint}</span>
                <!-- ZEB-668 S4: presence line, non-self rows only (self is
                     trivially present; peer liveness doesn't track self).
                     lastSeenMs null → render nothing (honesty rule). -->
                {#if !device.isThisDevice}
                  {#if device.connectedNow}
                    <span class="separator">·</span>
                    <span class="online-badge">● online</span>
                  {:else if typeof device.lastSeenMs === 'number'}
                    <!-- typeof guard (PR #454 round 1): an omitted field
                         (pre-S4 shape) must read as absent, same as null. -->
                    <span class="separator">·</span>
                    <span>last seen {formatLastSeen(device.lastSeenMs, $timeFormatPrefs)}</span>
                  {/if}
                {/if}
              </div>
              <!-- ZEB-418 P2 D17: always-on butler toggle -->
              <div class="butler-row">
                <label class="butler-label">
                  <input
                    type="checkbox"
                    class="butler-checkbox"
                    checked={device.butlerPinned}
                    disabled={butlerPinInFlight}
                    onchange={() => handleButlerPinToggle(device)}
                    aria-label={device.butlerPinned
                      ? `Remove always-on butler from ${device.displayName}`
                      : `Set ${device.displayName} as always-on butler`}
                  />
                  <span class="butler-label-text">Always-on butler</span>
                </label>
              </div>
            </div>
          </div>
        {/each}
        {#if butlerPinError}
          <p class="error" role="alert">{butlerPinError}</p>
        {/if}
        {#if renameError}
          <p class="error" role="alert">{renameError}</p>
        {/if}
        {#if carryError}
          <p class="error" role="alert">{carryError}</p>
        {/if}
      </div>

      <!-- ②b Removed devices (ZEB-668 S2) — collapsed; real RevocationSet data -->
      {#if removedDevices.length > 0}
        <div class="removed-section">
          <button
            class="removed-toggle"
            aria-expanded={removedOpen}
            onclick={() => (removedOpen = !removedOpen)}
          >
            <span class="removed-chevron" aria-hidden="true">{removedOpen ? '▾' : '▸'}</span>
            Removed devices ({removedDevices.length})
          </button>
          {#if removedOpen}
            {#each removedDevices as device (device.deviceId)}
              <div class="removed-row">
                <span class="removed-name">{device.displayName}</span>
                {#if device.revokedReason}
                  <span class="removed-reason">{device.revokedReason}</span>
                {/if}
                {#if device.revokedAt !== null}
                  <span class="removed-date">removed {formatEnrolledAt(device.revokedAt, $timeFormatPrefs)}</span>
                {/if}
              </div>
            {/each}
          {/if}
        </div>
      {/if}

      <!-- ③ Add-another-device footer -->
      <div class="add-another-footer">
        <div class="label">ADD ANOTHER DEVICE</div>
        {#if state.canBackUp}
          <button class="primary" onclick={() => { inviterOpen = true; }}>
            Add another device →
          </button>
          <p class="explainer">
            Both devices need to be on the same Wi-Fi network and in pairing mode.
            The new device will join under your existing owner identity.
          </p>
        {:else}
          <p class="explainer">
            This device cannot enroll others — its master seed has been wiped.
            Use a device that holds the master seed to add new devices.
          </p>
        {/if}
      </div>
    </div>
  {/if}

  {#if removeTarget}
    <RemoveDeviceDialog
      deviceName={removeTarget.displayName}
      isSelf={removeTarget.isThisDevice}
      isSeedHolder={removeTarget.isThisDevice && state?.canBackUp === true}
      quorum={state?.canBackUp !== true && removeTarget.quorumRemovable === true}
      busy={removeInFlight}
      error={removeError}
      onConfirm={handleRemoveConfirm}
      onCancel={() => {
        removeTarget = null;
        removeError = null;
      }}
    />
  {/if}

  {#if replaceTarget}
    <!-- ZEB-668 S6: sibling-only (self rows never offer Replace), so
         isSelf/isSeedHolder are statically false. -->
    <RemoveDeviceDialog
      mode="replace"
      deviceName={replaceTarget.displayName}
      isSelf={false}
      isSeedHolder={false}
      busy={replaceInFlight}
      error={replaceError}
      onConfirm={handleReplaceConfirm}
      onCancel={() => {
        replaceTarget = null;
        replaceError = null;
      }}
    />
  {/if}

  {#if backupOpen}
    <Modal
      onCancel={closeBackup}
      canCancel={!backupDialogInFlight && !backupInFlight}
      ariaLabelledby="backup-modal-heading"
    >
      <h3 class="modal-title" id="backup-modal-heading">Back up owner identity</h3>
      {#if backupSavedPath}
        <p>Recovery file written to <code>{backupSavedPath}</code>. Keep it somewhere safe.</p>
        <button class="primary" onclick={closeBackup}>Done</button>
      {:else}
        <p>
          Choose a strong passphrase. The encrypted file alone cannot be opened
          without it.
        </p>
        <label>
          Passphrase
          <input type="password" bind:value={backupPassphrase} aria-label="Passphrase" />
        </label>
        <label>
          Confirm passphrase
          <input type="password" bind:value={backupPassphraseConfirm} aria-label="Confirm passphrase" />
        </label>
        <label>
          Comment (optional)
          <!--
            No `maxlength` attribute — that counts UTF-16 code units, but
            the harmony-owner backend cap is 256 bytes. commitBackup
            validates byte length explicitly via TextEncoder before submit.
          --><input type="text" bind:value={backupComment} aria-label="Comment" />
        </label>
        {#if backupError}
          <p class="error" role="alert">{backupError}</p>
        {/if}
        <div class="modal-actions">
          <button class="secondary" onclick={closeBackup} disabled={backupDialogInFlight || backupInFlight}>Cancel</button>
          {#if recoveryToken === null && backupError}
            <!--
              Token-issuance failed (e.g., locked keychain). Inline retry
              avoids forcing the user to close + reopen the modal.
            -->
            <button class="secondary" onclick={retryIssueToken} disabled={backupDialogInFlight || backupInFlight}>Retry</button>
          {/if}
          <!--
            Disable Save backup when no token is available (e.g., issue_owner_recovery_token
            failed during openBackup). Otherwise the user clicks Save and gets a confusing
            "No recovery token available" inline error instead of the disabled-state hint.
          -->
          <button class="primary" onclick={commitBackup} disabled={backupDialogInFlight || backupInFlight || recoveryToken === null}>
            {#if backupInFlight}Encrypting…{:else if backupDialogInFlight}Choose location…{:else}Save backup{/if}
          </button>
        </div>
      {/if}
    </Modal>
  {/if}

  {#if phraseOpen && state !== null}
    <Modal onCancel={() => { phraseOpen = false; }} ariaLabelledby="phrase-modal-heading">
      <h3 class="modal-title" id="phrase-modal-heading">Owner recovery phrase</h3>
      <OwnerPhraseReveal ownerId={state.ownerId} />
    </Modal>
  {/if}

  {#if joinerOpen}
    <PairingJoiner onClose={async () => {
      joinerOpen = false;
      await svc.refresh();
    }} />
  {/if}
  {#if inviterOpen}
    <PairingInviter
      hostname={state?.ownerDisplayName ?? 'this device'}
      onComplete={handlePairingComplete}
      onClose={async () => {
        inviterOpen = false;
        // Abandoned replace-pairing: the revoke already landed; drop the
        // pending carry so a LATER unrelated pairing can't inherit it.
        pendingCarryPetname = null;
        await svc.refresh();
      }}
    />
  {/if}

  {#if modalOpen}
    <Modal
      onCancel={() => { modalOpen = false; }}
      canCancel={!mintInFlight}
      ariaLabelledby="modal-heading"
    >
      <h3 class="modal-title" id="modal-heading">Create your owner identity</h3>
      <p>
        This will create your owner identity. This device will be bound as the first device.
        You'll receive a recovery file to back up — you can do this immediately or later.
      </p>
      {#if mintError}
        <p class="error" role="alert">{mintError}</p>
      {/if}
      <div class="modal-actions">
        <button class="secondary" onclick={() => { modalOpen = false; }} disabled={mintInFlight}>
          Cancel
        </button>
        <button class="primary" onclick={handleConfirmMint} disabled={mintInFlight}>
          {mintInFlight ? 'Creating…' : 'Create owner identity'}
        </button>
      </div>
    </Modal>
  {/if}
</section>

<style>
  .devices-panel {
    padding: 16px;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 10px;
    margin-bottom: 16px;
  }
  .devices-panel h2 {
    margin: 0 0 12px;
    font-family: var(--font-display);
    font-size: 18px;
    font-weight: 600;
    letter-spacing: -0.01em;
    color: var(--text-primary);
  }
  .empty .explainer {
    color: var(--text-secondary);
    font-size: 13px;
    margin-bottom: 12px;
  }
  .empty-actions { display: flex; flex-direction: column; gap: 8px; }
  .primary, .secondary {
    padding: 6px 12px;
    border-radius: 8px;
    border: 1px solid var(--border);
    cursor: pointer;
    font-family: var(--font-ui);
    font-size: 13px;
  }
  .primary {
    background: var(--accent);
    color: var(--on-accent);
    border-color: var(--accent);
  }
  .primary:hover:not(:disabled) {
    background: var(--accent-hover);
    border-color: var(--accent-hover);
  }
  .secondary {
    background: var(--surface-raised);
    color: var(--text-primary);
  }
  .secondary:hover:not(:disabled) {
    background: var(--bg-tertiary);
  }
  .primary:disabled, .secondary:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .primary:focus-visible, .secondary:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .modal-title {
    margin: 0 0 12px;
    font-family: var(--font-display);
    font-size: 1.3rem;
    font-weight: 600;
    letter-spacing: -0.01em;
    color: var(--text-primary);
  }
  .modal-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 16px;
  }
  .error {
    color: var(--danger);
    font-size: 13px;
    margin: 8px 0;
  }
  .loading {
    color: var(--text-muted);
    font-size: 13px;
  }
  .label {
    font-size: 11px;
    font-weight: 600;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin-bottom: 6px;
  }
  /* ① Owner identity → Commons card */
  .owner-header {
    padding: 14px;
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 10px;
    box-shadow: var(--shadow-e1);
    margin-bottom: 14px;
  }
  .owner-row {
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .owner-avatar {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 40px;
    height: 40px;
    border-radius: 10px;
    background: var(--accent);
    color: var(--on-accent);
    font-family: var(--font-display);
    font-size: 18px;
    font-weight: 600;
  }
  .owner-identity {
    flex: 1;
    min-width: 0;
  }
  .owner-name-row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .owner-name {
    font-family: var(--font-display);
    font-size: 1.05rem;
    font-weight: 600;
    color: var(--text-primary);
  }
  /* "● self-sovereign" — RoleBadge grammar (mono 600 uppercase pill) */
  .ss-badge {
    display: inline-block;
    font-family: var(--font-mono);
    font-weight: 600;
    font-size: 10px;
    line-height: 1.4;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    padding: 2px 8px;
    border-radius: 20px;
    white-space: nowrap;
    color: var(--primary-deep);
    background: var(--primary-soft);
  }
  .owner-fingerprint {
    font-size: 12px;
    color: var(--text-muted);
    font-family: var(--font-mono);
    margin-top: 2px;
  }
  /* ZEB-650: derivable meta facts under the fingerprint. */
  .owner-meta {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 12px;
    color: var(--text-muted);
    margin-top: 4px;
    flex-wrap: wrap;
  }
  .owner-meta .meta-key {
    display: inline-block;
    font-family: var(--font-mono);
    font-weight: 600;
    font-size: 10px;
    line-height: 1.4;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    padding: 2px 8px;
    border-radius: 20px;
    white-space: nowrap;
    color: var(--text-muted);
    border: 1px solid var(--border);
  }
  .restore-link {
    margin-top: 12px;
    padding: 0;
    background: none;
    border: none;
    color: var(--accent);
    font-size: 0.8rem;
    cursor: pointer;
    text-decoration: underline;
    text-align: left;
  }
  .restore-link:hover {
    color: var(--accent-hover);
  }
  /* ② Device rows → white cards */
  .devices-list {
    margin-bottom: 14px;
  }
  .device-row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 10px;
    background: var(--surface-raised);
    border: 1px solid var(--line-soft);
    border-radius: 8px;
  }
  .device-row + .device-row {
    margin-top: 8px;
  }
  .device-icon {
    width: 32px;
    height: 32px;
    border-radius: 8px;
    /* Trust-blue rejection: neutral tertiary chip, never a cool blue. */
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 14px;
    font-weight: 600;
    flex-shrink: 0;
  }
  .device-meta {
    flex: 1;
    min-width: 0;
  }
  .device-name-row {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: 8px;
  }
  .device-name {
    font-weight: 600;
    color: var(--text-primary);
  }
  /* "this device" → sage pill */
  .this-device-marker {
    font-size: 10px;
    font-weight: 600;
    letter-spacing: 0.02em;
    padding: 2px 8px;
    border-radius: 20px;
    color: var(--primary-deep);
    background: var(--primary-soft);
  }
  .device-secondary {
    font-size: 12px;
    color: var(--text-secondary);
    margin-top: 2px;
  }
  .trust-badge.full { color: var(--success); }
  .trust-badge.provisional { color: var(--warning-bright); }
  .trust-badge.refused { color: var(--danger); }
  /* ZEB-668 S4: live-connection chip on sibling rows. */
  .online-badge { color: var(--success); }
  .separator { margin: 0 6px; color: var(--text-muted); }
  .fingerprint { font-family: var(--font-mono); }
  .add-another-footer .explainer {
    font-size: 12px;
    color: var(--text-secondary);
    line-height: 1.5;
    margin: 0;
  }
  .add-another-footer .primary {
    margin-bottom: 8px;
  }
  .rename-btn {
    font-size: 11px;
    padding: 4px 8px;
    border: 1px solid var(--border);
    background: var(--surface-raised);
    color: var(--text-primary);
    border-radius: 6px;
    cursor: pointer;
  }
  .rename-btn:hover {
    background: var(--bg-tertiary);
  }
  /* ZEB-668 S2: destructive row affordance + Removed-devices section */
  .remove-btn {
    font-size: 11px;
    padding: 4px 8px;
    border: 1px solid var(--danger-border-muted, var(--border));
    background: var(--surface-raised);
    color: var(--danger);
    border-radius: 6px;
    cursor: pointer;
  }
  .remove-btn:hover {
    background: var(--bg-tertiary);
  }
  .removed-section {
    margin-top: 4px;
  }
  .removed-toggle {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    background: none;
    border: none;
    padding: 4px 0;
    cursor: pointer;
    color: var(--text-secondary);
    font-size: 12px;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }
  .removed-chevron {
    font-size: 10px;
  }
  .removed-row {
    display: flex;
    align-items: baseline;
    gap: 8px;
    padding: 6px 0 0 16px;
    opacity: 0.75;
    font-size: 0.9rem;
  }
  .removed-name {
    color: var(--text-primary);
  }
  .removed-reason {
    color: var(--danger);
    font-size: 0.8rem;
  }
  .removed-date {
    color: var(--text-secondary);
    font-size: 0.8rem;
  }
  /* ZEB-418 P2 D17: butler pin toggle */
  .butler-row {
    margin-top: 6px;
  }
  .butler-label {
    display: flex;
    align-items: center;
    gap: 6px;
    cursor: pointer;
    user-select: none;
  }
  .butler-checkbox {
    cursor: pointer;
    accent-color: var(--accent);
  }
  .butler-checkbox:disabled {
    cursor: not-allowed;
    opacity: 0.5;
  }
  .butler-label-text {
    font-size: 12px;
    color: var(--text-secondary);
  }
  /* ZEB-668 S5: fleet-key staleness banner. */
  .epoch-banner {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    align-items: flex-start;
    padding: 0.75rem 1rem;
    border: 1px solid color-mix(in srgb, var(--warning) 40%, transparent);
    background: color-mix(in srgb, var(--warning) 10%, transparent);
    border-radius: 6px;
  }

  /* ZEB-677 S3: co-sign banner shares the epoch-banner shape; the action
     row sits under the request line. */
  .quorum-actions {
    display: flex;
    gap: 0.5rem;
  }

  .epoch-text {
    margin: 0;
    color: var(--warning);
    font-size: 0.9rem;
  }
</style>
