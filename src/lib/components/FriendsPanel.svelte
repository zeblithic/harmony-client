<script lang="ts">
  /**
   * ZEB-370 Phase 1 + ZEB-371 Phase 1b: Friends settings sub-panel.
   *
   * Phase 1:
   * - Lists active friends (display + short owner_id) via `list_friends`.
   * - "Generate friend link" mints a `harmony://friend/...` URL to copy.
   * - "Add friend" redeems a pasted URL.
   * - Per-row "Unfriend" writes a Revoked tombstone.
   *
   * Phase 1b (new):
   * - "Friend requests" inbox: list pending inbound requests with Accept/Decline.
   * - "Add friend by key" input: paste a 64-byte transport identity pub hex.
   * - "Auto-accept" toggle for peers already known to the local identity.
   *
   * Backed by `FriendService` (passed in so it shares the app's single adapter
   * + event wiring); re-fetches on `friend-list-changed` and
   * `friend-request-received` events.
   * Mirrors `NetworkDiscoverabilitySettings.svelte`'s self-contained,
   * runes-based settings-panel shape.
   */
  import { onMount, onDestroy } from 'svelte';
  import type {
    FriendService,
    FriendDto,
    PendingFriendRequestDto,
    ReferralView,
  } from '../friend-service';
  import {
    getIdentityDiscoverable,
    setIdentityDiscoverable,
    onIdentityDiscoverableChanged,
  } from '../connectivity-adapter';
  import Avatar from './Avatar.svelte';
  import type { MemberCardService } from '../member-card-service';
  import type { OpenCardPayload } from './MemberRow.svelte';

  let {
    service,
    cardService,
    onOpenCard,
  }: {
    service: FriendService;
    /** ZEB-419: dedicated MemberCardService for live friend/request names +
     *  avatars (App injects it). Optional so unit tests can omit it; when
     *  absent, names fall back to the frozen hint / short-hex. */
    cardService?: MemberCardService;
    /** ZEB-419: open the owner-card drill-down popover (App's openMemberCard). */
    onOpenCard?: (payload: OpenCardPayload, ev: MouseEvent) => void;
  } = $props();

  let friends = $state<FriendDto[]>([]);
  let loading = $state(true);
  let error = $state<string | null>(null);

  // ZEB-419: bumped by the dedicated card service's onUpdate (poll/event) so the
  // name/avatar helpers re-read resolve() and the rows repaint.
  let cardVersion = $state(0);

  // Generate-link state.
  let generatedUrl = $state<string | null>(null);
  let generating = $state(false);

  // Add-friend (token redeem) state.
  let pasteUrl = $state('');
  let redeeming = $state(false);
  let addStatus = $state<string | null>(null);

  // Per-row in-flight unfriend guard (by owner_id hex).
  let unfriending = $state<Set<string>>(new Set());

  // Per-row in-flight referrable-toggle guard (by owner_id hex). ZEB-375 Phase 2a.
  let referrableSaving = $state<Set<string>>(new Set());

  // ZEB-419: inline nickname editor. `editingNickname` is the owner_id of the
  // row whose editor is open (or null); `nicknameSaving` guards in-flight saves.
  let editingNickname = $state<string | null>(null);
  let nicknameDraft = $state('');
  let nicknameSaving = $state<Set<string>>(new Set());

  // ZEB-375 Phase 2a Task 7: per-row "browse referrals" state, keyed by the
  // friend's owner_id hex. `loading` is the in-flight guard; `results` holds the
  // last verified ReferralView[] (an empty array renders an "empty" line);
  // `error` is the per-row failure message. A friend with no entry in any map
  // has never been browsed (the panel is collapsed).
  let referralsLoading = $state<Set<string>>(new Set());
  let referralsResults = $state<Map<string, ReferralView[]>>(new Map());
  let referralsError = $state<Map<string, string>>(new Map());

  // ── Phase 1b state ────────────────────────────────────────────────────────

  // Pending friend requests inbox.
  let pendingRequests = $state<PendingFriendRequestDto[]>([]);
  let pendingLoading = $state(true);
  let pendingError = $state<string | null>(null);

  // Per-row in-flight accept/decline guard.
  let requestInFlight = $state<Set<string>>(new Set());

  // Add-by-key state.
  let addByKeyInput = $state('');
  let addingByKey = $state(false);
  let addByKeyStatus = $state<string | null>(null);

  // ── ZEB-415 #2: in-panel auto-retry of a pending outbound add ─────────────
  // The mutual-key handshake is synchronous with no server-push: after we send a
  // request the link only completes when WE dial again (once the peer accepts).
  // Rather than make the user guess when to re-press Connect, we re-attempt on a
  // bounded interval and surface "now connected" the instant it links.
  const ADD_RETRY_INTERVAL_MS = 10_000;
  const ADD_RETRY_MAX_ATTEMPTS = 30; // ~5-minute ceiling
  let addRetryTimer: ReturnType<typeof setTimeout> | null = null;
  // Bumped to invalidate an in-flight retry chain (supersede / cancel / destroy).
  let addRetryGeneration = 0;
  // Set once in onDestroy. Any async path that resumes after teardown (the
  // initial add awaiting `addByKey`, a retry tick awaiting `refresh`) checks
  // this before touching state or scheduling new timers.
  let destroyed = false;

  // ── ZEB-388: my own identity pub hex (share for add-by-key) ───────────────
  let myKeyHex = $state<string | null>(null);
  let myKeyCopied = $state(false);
  // Handle for the "Copied!" reset timer — cleared on re-click and on destroy
  // so we never mutate $state after the component unmounts.
  let myKeyCopiedTimer: ReturnType<typeof setTimeout> | null = null;

  // ── ZEB-415 #1: discovery-off footgun guard ───────────────────────────────
  // Tri-state: null = unknown (still loading — show nothing), false = OFF (warn:
  // peers can't add us by key), true = ON. A read failure stays null so a
  // transient error never nags the user with a false warning.
  let identityDiscoverable = $state<boolean | null>(null);
  let enablingDiscovery = $state(false);
  let unsubscribeDiscoverable: (() => void) | null = null;
  // Bumped by any authoritative update (change event / inline Enable). The
  // mount-time read captures this before awaiting and bails if it changed, so a
  // slow initial read can't clobber a fresher value when it finally resolves.
  let discoverableGen = 0;

  // Auto-accept toggle.
  let autoAccept = $state(false);
  let autoAcceptLoading = $state(true);
  let autoAcceptSaving = $state(false);
  let autoAcceptError = $state<string | null>(null);

  // Unsubscribe handles for our event listeners (set in onMount).
  let unsubscribeChanged: (() => void) | null = null;
  let unsubscribePendingChanged: (() => void) | null = null;

  async function refresh(): Promise<void> {
    try {
      const next = await service.listFriends();
      // Don't assign list state after teardown — it would also re-trigger the
      // card-subscription $effect on the App-owned friendCardService while the
      // panel is closed (ZEB-415 liveness discipline).
      if (destroyed) return;
      friends = next;
      error = null;
      // ZEB-375 Phase 2a: browse is Active-only, so drop any per-row referral
      // state for friends that are no longer Active (now Pending, or absent).
      // Without this, a friend that was browsed while Active and later shows as
      // Pending would still render its prior catalog even though the controls
      // are hidden — `refresh()` is the single point where `friends` changes.
      const activeIds = new Set(
        friends.filter((f) => f.status === 'active').map((f) => f.ownerIdHex),
      );
      referralsResults = new Map(
        [...referralsResults].filter(([ownerIdHex]) => activeIds.has(ownerIdHex)),
      );
      referralsError = new Map(
        [...referralsError].filter(([ownerIdHex]) => activeIds.has(ownerIdHex)),
      );
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }
  }

  async function refreshPending(): Promise<void> {
    try {
      const next = await service.listPendingRequests();
      if (destroyed) return; // see refresh(): no post-teardown state writes
      pendingRequests = next;
      pendingError = null;
    } catch (e) {
      pendingError = e instanceof Error ? e.message : String(e);
    } finally {
      pendingLoading = false;
    }
  }

  async function loadAutoAccept(): Promise<void> {
    try {
      autoAccept = await service.getAutoAccept();
      autoAcceptError = null;
    } catch (e) {
      autoAcceptError = e instanceof Error ? e.message : String(e);
    } finally {
      autoAcceptLoading = false;
    }
  }

  async function loadMyKey(): Promise<void> {
    try {
      myKeyHex = await service.getMyIdentityPubHex();
    } catch (e) {
      // The Ok(None) "node not started" path does NOT throw, so a throw here is
      // a real backend failure (e.g. a poisoned NodeState). Log it rather than
      // swallow it silently; the UI still falls back to the neutral null state.
      console.error(
        'getMyIdentityPubHex failed:',
        e instanceof Error ? e.message : String(e),
      );
      myKeyHex = null;
    }
  }

  // ZEB-415 #1: read the case-B "Allow discovery by identity address" toggle so
  // the "My key" section can warn when sharing the key is futile (peers resolve
  // the identity-keyed pkarr record, which is only published when discovery is on).
  async function loadDiscoverable(): Promise<void> {
    const gen = discoverableGen;
    try {
      const value = await getIdentityDiscoverable();
      // A change event or inline Enable that landed while we were awaiting is
      // strictly fresher than this mount-time snapshot — don't overwrite it.
      if (gen !== discoverableGen) return;
      identityDiscoverable = value;
    } catch (e) {
      // A read failure must NOT nag the user with a (possibly false) warning;
      // leave the tri-state at null. Log so a real backend fault stays visible.
      console.error(
        'getIdentityDiscoverable failed:',
        e instanceof Error ? e.message : String(e),
      );
    }
  }

  async function handleEnableDiscovery(): Promise<void> {
    if (enablingDiscovery) return;
    enablingDiscovery = true;
    try {
      await setIdentityDiscoverable(true);
      if (destroyed) return; // unmounted mid-toggle — don't touch state
      // Optimistic clear — the `connectivity-identity-discoverable-changed`
      // event will also flip this, but updating now hides the warning at once.
      identityDiscoverable = true;
      discoverableGen += 1;
    } catch (e) {
      console.error(
        'setIdentityDiscoverable failed:',
        e instanceof Error ? e.message : String(e),
      );
    } finally {
      enablingDiscovery = false;
    }
  }

  onMount(() => {
    // ZEB-419: repaint names/avatars when the dedicated card service resolves a
    // card (poll or member-card-received event). Guard post-teardown writes.
    if (cardService) {
      cardService.onUpdate = () => {
        if (!destroyed) cardVersion += 1;
      };
    }
    // Re-fetch friends whenever the backend signals a change.
    unsubscribeChanged = service.onFriendsChanged(() => {
      void refresh();
    });
    // Re-fetch pending requests on new inbound request or list mutation.
    unsubscribePendingChanged = service.onPendingRequestsChanged(() => {
      void refreshPending();
    });
    void refresh();
    void refreshPending();
    void loadAutoAccept();
    void loadMyKey();
    void loadDiscoverable();
    // Keep the warning in lockstep with the discovery toggle wherever it's
    // flipped (e.g. the Network settings panel), not just our inline Enable.
    unsubscribeDiscoverable = onIdentityDiscoverableChanged((enabled) => {
      identityDiscoverable = enabled;
      discoverableGen += 1;
    });
  });

  onDestroy(() => {
    unsubscribeChanged?.();
    unsubscribeChanged = null;
    unsubscribePendingChanged?.();
    unsubscribePendingChanged = null;
    unsubscribeDiscoverable?.();
    unsubscribeDiscoverable = null;
    // Block any async path that resumes after teardown (initial add / retry).
    destroyed = true;
    // Cancel any in-flight `loadDiscoverable` read so its late resolve can't
    // write `identityDiscoverable` after we've unmounted (CodeRabbit).
    discoverableGen += 1;
    stopAddRetry();
    // ZEB-419: stop card updates + drop all friend/request card subscriptions.
    if (cardService) {
      cardService.onUpdate = undefined;
      void cardService.unsubscribeAll();
    }
    if (myKeyCopiedTimer) clearTimeout(myKeyCopiedTimer);
    myKeyCopiedTimer = null;
  });

  async function handleGenerate(): Promise<void> {
    if (generating) return;
    generating = true;
    try {
      generatedUrl = await service.generateFriendToken();
      error = null;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      generating = false;
    }
  }

  async function handleCopy(): Promise<void> {
    if (!generatedUrl) return;
    try {
      await navigator.clipboard.writeText(generatedUrl);
    } catch {
      // Clipboard may be unavailable (headless / permission); the URL stays
      // visible in the readonly input for manual copy.
    }
  }

  async function handleCopyMyKey(): Promise<void> {
    if (!myKeyHex) return;
    try {
      await navigator.clipboard.writeText(myKeyHex);
      myKeyCopied = true;
      // Clear any in-flight reset (rapid re-clicks) before scheduling a new one;
      // onDestroy clears it too so we never write $state after unmount.
      if (myKeyCopiedTimer) clearTimeout(myKeyCopiedTimer);
      myKeyCopiedTimer = setTimeout(() => {
        myKeyCopied = false;
        myKeyCopiedTimer = null;
      }, 1500);
    } catch {
      // Clipboard unavailable (headless / permission); the hex stays visible
      // in the readonly input for manual copy. Mirrors handleCopy.
    }
  }

  async function handleAdd(): Promise<void> {
    const url = pasteUrl.trim();
    if (redeeming || url.length === 0) return;
    redeeming = true;
    addStatus = null;
    try {
      const result = await service.redeemFriendToken(url);
      addStatus = `Added ${result.display ?? shortId(result.ownerIdHex)}`;
      pasteUrl = '';
      await refresh();
    } catch (e) {
      addStatus = `Failed: ${e instanceof Error ? e.message : String(e)}`;
    } finally {
      redeeming = false;
    }
  }

  async function handleUnfriend(ownerIdHex: string): Promise<void> {
    if (unfriending.has(ownerIdHex)) return;
    unfriending = new Set(unfriending).add(ownerIdHex);
    try {
      await service.unfriend(ownerIdHex);
      await refresh();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      const next = new Set(unfriending);
      next.delete(ownerIdHex);
      unfriending = next;
    }
  }

  // ── ZEB-419: local nickname editing (active friends) ──────────────────────
  function startEditNickname(f: FriendDto): void {
    editingNickname = f.ownerIdHex;
    nicknameDraft = f.nickname ?? '';
  }
  function cancelEditNickname(): void {
    editingNickname = null;
    nicknameDraft = '';
  }
  // Save (or clear, when blank) the nickname. The backend emits
  // `friend-list-changed` → `refresh()` repaints with the new value, so there's
  // no optimistic local mutation. `destroyed` guards the post-await writes
  // (same liveness discipline as the add-by-key paths).
  async function saveNickname(ownerIdHex: string): Promise<void> {
    if (nicknameSaving.has(ownerIdHex)) return;
    nicknameSaving = new Set(nicknameSaving).add(ownerIdHex);
    try {
      await service.setNickname(ownerIdHex, nicknameDraft.trim() || null);
      if (destroyed) return;
      // Only close the editor if we're still editing THIS row — the user may have
      // opened another friend's editor while this save was in flight.
      if (editingNickname === ownerIdHex) {
        editingNickname = null;
        nicknameDraft = '';
      }
    } catch (e) {
      if (destroyed) return;
      error = `Couldn't save nickname: ${e instanceof Error ? e.message : String(e)}`;
    } finally {
      const next = new Set(nicknameSaving);
      next.delete(ownerIdHex);
      nicknameSaving = next;
    }
  }

  // ZEB-375 Phase 2a: flip a friend's referral-catalog opt-in. `next` is the
  // desired new value (the inverse of the current `referrable`). The backend
  // re-syncs and emits `friend-list-changed`, but we also `refresh()` so the
  // checkbox reflects the new value immediately.
  async function handleToggleReferrable(ownerIdHex: string, next: boolean): Promise<void> {
    if (referrableSaving.has(ownerIdHex)) return;
    referrableSaving = new Set(referrableSaving).add(ownerIdHex);
    try {
      await service.setReferrable(ownerIdHex, next);
      await refresh();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      error = msg;
    } finally {
      const nextSet = new Set(referrableSaving);
      nextSet.delete(ownerIdHex);
      referrableSaving = nextSet;
    }
  }

  // ZEB-375 Phase 2a Task 7: browse a friend's referral catalog. The backend
  // resolves + dials the friend, sends a signed request, VERIFIES the returned
  // catalog, and projects each entry; we only render the verified result (or the
  // per-row error). Read-only — Phase 2a has no "request introduction" action.
  async function handleBrowseReferrals(ownerIdHex: string): Promise<void> {
    if (referralsLoading.has(ownerIdHex)) return;
    referralsLoading = new Set(referralsLoading).add(ownerIdHex);
    // Clear any prior error for this row so the loading state shows cleanly.
    const clearedErr = new Map(referralsError);
    clearedErr.delete(ownerIdHex);
    referralsError = clearedErr;
    try {
      const views = await service.browseReferrals(ownerIdHex);
      referralsResults = new Map(referralsResults).set(ownerIdHex, views);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      referralsError = new Map(referralsError).set(ownerIdHex, msg);
    } finally {
      const next = new Set(referralsLoading);
      next.delete(ownerIdHex);
      referralsLoading = next;
    }
  }

  // ── Phase 1b handlers ─────────────────────────────────────────────────────

  async function handleAccept(ownerIdHex: string): Promise<void> {
    if (requestInFlight.has(ownerIdHex)) return;
    requestInFlight = new Set(requestInFlight).add(ownerIdHex);
    try {
      await service.acceptRequest(ownerIdHex);
      await Promise.all([refresh(), refreshPending()]);
    } catch (e) {
      pendingError = e instanceof Error ? e.message : String(e);
    } finally {
      const next = new Set(requestInFlight);
      next.delete(ownerIdHex);
      requestInFlight = next;
    }
  }

  async function handleDecline(ownerIdHex: string): Promise<void> {
    if (requestInFlight.has(ownerIdHex)) return;
    requestInFlight = new Set(requestInFlight).add(ownerIdHex);
    try {
      await service.declineRequest(ownerIdHex);
      await refreshPending();
    } catch (e) {
      pendingError = e instanceof Error ? e.message : String(e);
    } finally {
      const next = new Set(requestInFlight);
      next.delete(ownerIdHex);
      requestInFlight = next;
    }
  }

  // Cancel any in-flight pending-add retry chain (on supersede / destroy).
  function stopAddRetry(): void {
    addRetryGeneration += 1;
    if (addRetryTimer) {
      clearTimeout(addRetryTimer);
      addRetryTimer = null;
    }
  }

  // Begin retrying `add_friend_by_key` for a key that returned `pending`. Each
  // tick re-dials; a `linked` result reports success and stops, anything else
  // (still pending, a transient `unreachable`, a transient dial error) keeps
  // waiting until the bounded window elapses. The `generation` guard makes a
  // superseded or post-unmount tick a no-op so we never mutate $state after
  // teardown or run two chains at once.
  function startAddRetry(key: string): void {
    stopAddRetry();
    const generation = addRetryGeneration;
    let attempt = 0;
    // Why the latest attempt didn't link, so the terminal (max-attempts) message
    // reflects reality — "they haven't accepted" vs "couldn't reach them" vs a
    // hard error — instead of always blaming a missing accept (Cursor).
    let lastReason = 'they have not accepted yet';
    const waitingCopy =
      "Request sent — they'll need to accept. We'll connect automatically once they do.";
    const tick = async (): Promise<void> => {
      if (generation !== addRetryGeneration) return;
      try {
        const outcome = await service.addByKey(key);
        if (generation !== addRetryGeneration) return; // superseded while awaiting
        if (outcome.kind === 'linked') {
          addByKeyStatus = `Now connected with ${outcome.display ?? shortId(outcome.ownerIdHex)}`;
          addRetryTimer = null;
          // Re-check teardown before the (async) refresh — the panel may have
          // unmounted while we awaited the dial (Cursor).
          if (!destroyed) await refresh();
          return;
        }
        if (outcome.kind === 'unreachable') {
          lastReason = 'we could not reach them on the last try';
        } else {
          // 'pending' → the expected wait; re-assert the calm copy in case a
          // prior transient error had replaced it.
          lastReason = 'they have not accepted yet';
          addByKeyStatus = waitingCopy;
        }
      } catch (e) {
        if (generation !== addRetryGeneration) return;
        const msg = e instanceof Error ? e.message : String(e);
        // Keep retrying (the throw is usually a transient dial blip while the
        // peer is still offline), but surface the failure now rather than hide
        // it behind the rosy "we'll connect automatically" copy.
        console.debug(
          `add-by-key auto-retry attempt ${attempt + 1} failed (still retrying):`,
          msg,
        );
        lastReason = `the last attempt failed: ${msg}`;
        addByKeyStatus = `Still trying to connect — ${lastReason}.`;
      }
      attempt += 1;
      if (attempt >= ADD_RETRY_MAX_ATTEMPTS) {
        // Terminal message names the actual last outcome, not a blanket "still
        // waiting to accept" (Cursor) — and doesn't clobber a surfaced error.
        addByKeyStatus = `Stopped trying to connect — ${lastReason}. Press Connect again to retry.`;
        addRetryTimer = null;
        return;
      }
      addRetryTimer = setTimeout(() => void tick(), ADD_RETRY_INTERVAL_MS);
    };
    addRetryTimer = setTimeout(() => void tick(), ADD_RETRY_INTERVAL_MS);
  }

  async function handleAddByKey(): Promise<void> {
    const key = addByKeyInput.trim();
    if (addingByKey || key.length === 0) return;
    // A fresh add supersedes any prior pending-add retry chain.
    stopAddRetry();
    addingByKey = true;
    addByKeyStatus = null;
    try {
      const outcome = await service.addByKey(key);
      // The panel may have been torn down while the add was in flight — don't
      // mutate state or start a retry chain after unmount (CodeAnt).
      if (destroyed) return;
      if (outcome.kind === 'linked') {
        addByKeyStatus = `Connected with ${outcome.display ?? shortId(outcome.ownerIdHex)}`;
        addByKeyInput = '';
        if (!destroyed) await refresh();
      } else if (outcome.kind === 'pending') {
        addByKeyStatus =
          "Request sent — they'll need to accept. We'll connect automatically once they do.";
        addByKeyInput = '';
        await refreshPending();
        // refreshPending awaited above — re-check teardown before scheduling a
        // retry chain, or an unmount during that refresh leaves timers running
        // on a dead component (Cursor).
        if (destroyed) return;
        startAddRetry(key);
      } else {
        addByKeyStatus =
          "Couldn't reach them — they may need to enable discovery, or try again in a moment.";
      }
    } catch (e) {
      addByKeyStatus = `Failed: ${e instanceof Error ? e.message : String(e)}`;
    } finally {
      addingByKey = false;
    }
  }

  async function handleAutoAcceptToggle(): Promise<void> {
    if (autoAcceptSaving) return;
    const next = !autoAccept;
    autoAcceptSaving = true;
    try {
      await service.setAutoAccept(next);
      autoAccept = next;
      autoAcceptError = null;
    } catch (e) {
      autoAcceptError = e instanceof Error ? e.message : String(e);
    } finally {
      autoAcceptSaving = false;
    }
  }

  function shortId(hex: string): string {
    return hex.length > 12 ? `${hex.slice(0, 12)}…` : hex;
  }

  // ── ZEB-419: live owner-card resolution (name + avatar) ───────────────────
  // Reading `cardVersion` inside these helpers makes the template re-render when
  // a poll/event fills a card. Empty strings fall through (`||` not `??`) so a
  // blank card name never masks a usable hint.
  function cardName(ownerIdHex: string): string | undefined {
    cardVersion;
    return cardService?.resolve(ownerIdHex)?.displayName || undefined;
  }
  function cardAvatarUrl(ownerIdHex: string): string | undefined {
    cardVersion;
    return cardService?.resolve(ownerIdHex)?.avatarUrl;
  }
  // Label ladder: personal nickname ► live card name ► frozen link hint ►
  // short owner_id. The short-hex line under the name stays the verifiable id.
  function friendLabel(f: FriendDto): string {
    return f.nickname || cardName(f.ownerIdHex) || f.display || shortId(f.ownerIdHex);
  }
  // Pending requests have no nickname rung (you nickname a peer after accepting).
  function requestLabel(r: PendingFriendRequestDto): string {
    return cardName(r.ownerIdHex) || r.display || shortId(r.ownerIdHex);
  }

  // ZEB-419: open the owner-card drill-down (App's ProfilePopover owner-card
  // mode). The popover shows the peer's REAL signed card name + full owner_id —
  // never the local nickname — so a misleading nickname can't mask identity.
  function openIdentity(ownerIdHex: string, ev: MouseEvent): void {
    const resolved = cardService?.resolve(ownerIdHex);
    onOpenCard?.(
      {
        ownerIdHex,
        displayName: resolved?.displayName ?? '',
        statusText: resolved?.statusText ?? '',
        avatarUrl: resolved?.avatarUrl,
      },
      ev,
    );
  }

  // Reconcile the dedicated card service's subscriptions to the current friend +
  // pending owner_id set whenever those lists change. subscribeVisible is
  // idempotent and reconciles (unsubscribes owners that left the lists).
  $effect(() => {
    // Belt-and-suspenders: never (re)subscribe after teardown. The refresh
    // guards above stop list state changing post-unmount, and Svelte disposes
    // this effect on destroy — this check makes a stray re-run a no-op too.
    if (destroyed || !cardService) return;
    const ids = [
      ...friends.map((f) => f.ownerIdHex),
      ...pendingRequests.map((r) => r.ownerIdHex),
    ];
    void cardService.subscribeVisible(ids);
  });
</script>

<div class="friends-section" data-testid="friends-panel">
  <div class="section-header">
    <h4 class="section-title">Friends</h4>
  </div>

  {#if error}
    <p class="error-text" data-testid="friends-error">{error}</p>
  {/if}

  <!-- Active friends list. -->
  {#if loading}
    <p class="muted">Loading…</p>
  {:else if friends.length === 0}
    <p class="muted" data-testid="friends-empty">No friends yet. Share a friend link to connect.</p>
  {:else}
    <ul class="friend-list" data-testid="friend-list">
      {#each friends as f (f.ownerIdHex)}
        <li class="friend-row">
          <Avatar
            address={f.ownerIdHex}
            displayName={friendLabel(f)}
            avatarUrl={cardAvatarUrl(f.ownerIdHex)}
            size={28}
          />
          <div class="friend-id">
            <span class="friend-name" data-testid="friend-name-{f.ownerIdHex}">{friendLabel(f)}</span>
            <button
              type="button"
              class="friend-addr identity-btn"
              title="Verify identity — show full key"
              data-testid="friend-identity-{f.ownerIdHex}"
              onclick={(e) => openIdentity(f.ownerIdHex, e)}
            >{shortId(f.ownerIdHex)}</button>
          </div>
          <!-- ZEB-375 Phase 2a: the referrable opt-in + browse action only work
               for Active friends — the backend returns a typed error for
               non-Active links. `list_friends` also returns Pending rows, so
               gate both controls on status to avoid surfacing functional-looking
               controls that throw. -->
          {#if f.status === 'active'}
            <!-- ZEB-419: local nickname editor (this device only). -->
            {#if editingNickname === f.ownerIdHex}
              <input
                type="text"
                class="nickname-input"
                placeholder="Nickname (only you see this)"
                bind:value={nicknameDraft}
                data-testid="nickname-input-{f.ownerIdHex}"
                onkeydown={(e) => {
                  if (e.key === 'Enter') saveNickname(f.ownerIdHex);
                  else if (e.key === 'Escape') cancelEditNickname();
                }}
              />
              <button
                type="button"
                class="secondary-btn small-btn"
                disabled={nicknameSaving.has(f.ownerIdHex)}
                onclick={() => saveNickname(f.ownerIdHex)}
                data-testid="nickname-save-{f.ownerIdHex}"
              >
                {nicknameSaving.has(f.ownerIdHex) ? '…' : 'Save'}
              </button>
              <button
                type="button"
                class="secondary-btn small-btn"
                onclick={cancelEditNickname}
                data-testid="nickname-cancel-{f.ownerIdHex}"
              >
                Cancel
              </button>
            {:else}
              <button
                type="button"
                class="secondary-btn small-btn"
                onclick={() => startEditNickname(f)}
                data-testid="set-nickname-btn-{f.ownerIdHex}"
              >
                {f.nickname ? 'Edit nickname' : 'Set nickname'}
              </button>
            {/if}
            <label class="referrable-toggle" data-testid="referrable-toggle-label">
              <input
                type="checkbox"
                class="referrable-checkbox"
                checked={f.referrable}
                disabled={referrableSaving.has(f.ownerIdHex)}
                onchange={() => handleToggleReferrable(f.ownerIdHex, !f.referrable)}
                data-testid="referrable-checkbox"
              />
              <span class="referrable-label-text">Referrable</span>
            </label>
            <button
              type="button"
              class="secondary-btn small-btn"
              disabled={referralsLoading.has(f.ownerIdHex)}
              onclick={() => handleBrowseReferrals(f.ownerIdHex)}
              data-testid="browse-referrals-btn"
            >
              {referralsLoading.has(f.ownerIdHex) ? 'Loading…' : 'Browse referrals'}
            </button>
          {/if}
          <button
            type="button"
            class="unfriend-btn"
            disabled={unfriending.has(f.ownerIdHex)}
            onclick={() => handleUnfriend(f.ownerIdHex)}
            data-testid="unfriend-btn"
          >
            {unfriending.has(f.ownerIdHex) ? '…' : 'Unfriend'}
          </button>
        </li>
        <!-- ZEB-375 Phase 2a: read-only referral catalog for this friend. Shown
             only once browsed (an error, or a results entry — possibly empty).
             Active-only and consistently gated: BOTH branches check
             `f.status === 'active'`, and `refresh()` prunes these maps to Active
             friends, so a stale catalog can't render after a status change. -->
        {#if f.status === 'active' && referralsError.has(f.ownerIdHex)}
          <li class="referrals-row" data-testid="referrals-error-row">
            <p class="error-text" data-testid="referrals-error">
              {referralsError.get(f.ownerIdHex)}
            </p>
          </li>
        {:else if f.status === 'active' && referralsResults.has(f.ownerIdHex)}
          <li class="referrals-row" data-testid="referrals-row">
            {#if referralsResults.get(f.ownerIdHex)!.length === 0}
              <p class="muted" data-testid="referrals-empty">
                No referrals shared.
              </p>
            {:else}
              <ul class="referrals-list" data-testid="referrals-list">
                {#each referralsResults.get(f.ownerIdHex)! as r (r.ownerIdHex)}
                  <li class="referral-item" data-testid="referral-item">
                    <span class="referral-name" title={r.ownerIdHex}>
                      {r.display ?? shortId(r.ownerIdHex)}
                    </span>
                    {#if r.alreadyFriend}
                      <span class="already-friend-badge" data-testid="already-friend-badge">
                        already friends
                      </span>
                    {/if}
                  </li>
                {/each}
              </ul>
            {/if}
          </li>
        {/if}
      {/each}
    </ul>
  {/if}

  <!-- Generate a friend link to share. -->
  <div class="action-block">
    <button
      type="button"
      class="primary-btn"
      disabled={generating}
      onclick={handleGenerate}
      data-testid="generate-friend-link"
    >
      {generating ? 'Generating…' : 'Generate friend link'}
    </button>
    {#if generatedUrl}
      <div class="generated-row">
        <input
          type="text"
          readonly
          class="url-input"
          value={generatedUrl}
          data-testid="generated-url"
          onfocus={(e) => (e.currentTarget as HTMLInputElement).select()}
        />
        <button type="button" class="secondary-btn" onclick={handleCopy} data-testid="copy-url">
          Copy
        </button>
      </div>
    {/if}
  </div>

  <!-- Add a friend by pasting their link. -->
  <div class="action-block">
    <label class="add-label" for="friend-url-input">Add friend</label>
    <div class="add-row">
      <input
        id="friend-url-input"
        type="text"
        class="url-input"
        placeholder="harmony://friend/…"
        bind:value={pasteUrl}
        data-testid="add-friend-input"
      />
      <button
        type="button"
        class="primary-btn"
        disabled={redeeming || pasteUrl.trim().length === 0}
        onclick={handleAdd}
        data-testid="add-friend-btn"
      >
        {redeeming ? 'Adding…' : 'Add'}
      </button>
    </div>
    {#if addStatus}
      <p class="muted" data-testid="add-status">{addStatus}</p>
    {/if}
  </div>

  <!-- ── Phase 1b: Friend requests inbox ────────────────────────────────── -->
  <div class="subsection" data-testid="friend-requests-section">
    <h5 class="subsection-title">Friend requests</h5>

    {#if pendingError}
      <p class="error-text" data-testid="pending-error">{pendingError}</p>
    {/if}

    {#if pendingLoading}
      <p class="muted">Loading…</p>
    {:else if pendingRequests.length === 0}
      <p class="muted" data-testid="pending-empty">No pending requests.</p>
    {:else}
      <ul class="friend-list" data-testid="pending-list">
        {#each pendingRequests as req (req.ownerIdHex)}
          <li class="friend-row">
            <Avatar
              address={req.ownerIdHex}
              displayName={requestLabel(req)}
              avatarUrl={cardAvatarUrl(req.ownerIdHex)}
              size={28}
            />
            <div class="friend-id">
              <span class="friend-name" data-testid="friend-name-{req.ownerIdHex}">{requestLabel(req)}</span>
              <button
                type="button"
                class="friend-addr identity-btn"
                title="Verify identity — show full key"
                data-testid="friend-identity-{req.ownerIdHex}"
                onclick={(e) => openIdentity(req.ownerIdHex, e)}
              >{shortId(req.ownerIdHex)}</button>
            </div>
            <div class="request-actions">
              <button
                type="button"
                class="accept-btn"
                disabled={requestInFlight.has(req.ownerIdHex)}
                onclick={() => handleAccept(req.ownerIdHex)}
                data-testid="accept-btn"
              >
                {requestInFlight.has(req.ownerIdHex) ? '…' : 'Accept'}
              </button>
              <button
                type="button"
                class="unfriend-btn"
                disabled={requestInFlight.has(req.ownerIdHex)}
                onclick={() => handleDecline(req.ownerIdHex)}
                data-testid="decline-btn"
              >
                {requestInFlight.has(req.ownerIdHex) ? '…' : 'Decline'}
              </button>
            </div>
          </li>
        {/each}
      </ul>
    {/if}
  </div>

  <!-- ── ZEB-388: My key (share so a peer can add you by key) ───────────── -->
  <div class="action-block" data-testid="my-key-section">
    <label class="add-label" for="my-key-input">My key</label>
    {#if myKeyHex}
      <div class="add-row">
        <input
          id="my-key-input"
          type="text"
          class="url-input"
          readonly
          value={myKeyHex}
          data-testid="my-key-input"
        />
        <button
          type="button"
          class="secondary-btn"
          onclick={handleCopyMyKey}
          data-testid="my-key-copy-btn"
        >
          {myKeyCopied ? 'Copied!' : 'Copy'}
        </button>
      </div>
      <p class="muted">Share this so a friend can add you with "Add friend by key".</p>
      {#if identityDiscoverable === false}
        <p class="warn-text" data-testid="my-key-discovery-warning">
          Friends can't add you with this key until you turn on "Allow discovery
          by identity address".
          <button
            type="button"
            class="link-btn"
            onclick={handleEnableDiscovery}
            disabled={enablingDiscovery}
            data-testid="my-key-enable-discovery-btn"
          >
            {enablingDiscovery ? 'Enabling…' : 'Enable'}
          </button>
        </p>
      {/if}
    {:else}
      <p class="muted" data-testid="my-key-empty">Start your node to view your key.</p>
    {/if}
  </div>

  <!-- ── Phase 1b: Add friend by public key ─────────────────────────────── -->
  <div class="action-block" data-testid="add-by-key-section">
    <label class="add-label" for="add-by-key-input">Add friend by key</label>
    <div class="add-row">
      <input
        id="add-by-key-input"
        type="text"
        class="url-input"
        placeholder="64-byte identity public key (hex)…"
        bind:value={addByKeyInput}
        data-testid="add-by-key-input"
      />
      <button
        type="button"
        class="primary-btn"
        disabled={addingByKey || addByKeyInput.trim().length === 0}
        onclick={handleAddByKey}
        data-testid="add-by-key-btn"
      >
        {addingByKey ? 'Connecting…' : 'Connect'}
      </button>
    </div>
    {#if addByKeyStatus}
      <p class="muted" data-testid="add-by-key-status">{addByKeyStatus}</p>
    {/if}
  </div>

  <!-- ── Phase 1b: Auto-accept toggle ───────────────────────────────────── -->
  <div class="action-block" data-testid="auto-accept-section">
    {#if autoAcceptError}
      <p class="error-text" data-testid="auto-accept-error">{autoAcceptError}</p>
    {/if}
    <label class="toggle-row" data-testid="auto-accept-toggle-label">
      <input
        type="checkbox"
        class="toggle-checkbox"
        checked={autoAccept}
        disabled={autoAcceptLoading || autoAcceptSaving}
        onchange={handleAutoAcceptToggle}
        data-testid="auto-accept-checkbox"
      />
      <span class="toggle-label-text">Auto-accept friends I already know</span>
    </label>
  </div>
</div>

<style>
  .friends-section {
    padding: 12px 0;
  }

  .section-header {
    margin-bottom: 8px;
  }

  .section-title {
    margin: 0;
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .friend-list {
    list-style: none;
    margin: 0 0 12px;
    padding: 0;
  }

  .friend-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: 12px;
    padding: 6px 0;
    border-bottom: 1px solid var(--border);
  }

  .friend-id {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
    flex: 1 1 auto;
  }

  .referrable-toggle {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-shrink: 0;
    cursor: pointer;
    user-select: none;
  }

  .referrable-checkbox {
    width: 14px;
    height: 14px;
    cursor: pointer;
    flex-shrink: 0;
  }

  .referrable-checkbox:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .referrable-label-text {
    font-size: 12px;
    color: var(--text-secondary);
  }

  /* ZEB-375 Phase 2a: per-friend read-only referral list. */

  .small-btn {
    flex-shrink: 0;
    font-size: 12px;
    padding: 4px 10px;
  }

  .nickname-input {
    flex: 0 1 150px;
    min-width: 90px;
    font-size: 12px;
    padding: 4px 6px;
    border-radius: 4px;
    border: 1px solid var(--border);
    background: var(--bg-secondary);
    color: var(--text-primary);
  }

  .referrals-row {
    list-style: none;
    padding: 4px 0 8px 12px;
    border-bottom: 1px solid var(--border);
  }

  .referrals-list {
    list-style: none;
    margin: 0;
    padding: 0;
  }

  .referral-item {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 3px 0;
  }

  .referral-name {
    font-size: 12px;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .already-friend-badge {
    flex-shrink: 0;
    font-size: 10px;
    padding: 1px 6px;
    border-radius: 8px;
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    border: 1px solid var(--border);
  }

  .friend-name {
    font-size: 13px;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .friend-addr {
    font-size: 11px;
    color: var(--text-secondary);
    font-family: var(--font-mono, monospace);
  }

  /* ZEB-419: the short-hex line doubles as the identity drill-down trigger.
     Reset button chrome but keep the .friend-addr font/colour. */
  .identity-btn {
    background: transparent;
    border: none;
    padding: 0;
    cursor: pointer;
    text-align: left;
  }
  .identity-btn:hover {
    text-decoration: underline;
    color: var(--text-primary);
  }
  .identity-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
    border-radius: 2px;
  }

  .unfriend-btn {
    flex-shrink: 0;
    font-size: 12px;
    padding: 4px 10px;
    border-radius: 4px;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
    cursor: pointer;
  }

  .unfriend-btn:hover:not(:disabled) {
    border-color: #d83c3e;
    color: #d83c3e;
  }

  .unfriend-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .action-block {
    margin-top: 12px;
  }

  .primary-btn {
    font-size: 13px;
    padding: 6px 12px;
    border-radius: 4px;
    border: 1px solid var(--accent);
    background: var(--accent);
    color: #fff;
    cursor: pointer;
  }

  .primary-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .secondary-btn {
    font-size: 13px;
    padding: 6px 12px;
    border-radius: 4px;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-primary);
    cursor: pointer;
  }

  .generated-row,
  .add-row {
    display: flex;
    gap: 6px;
    margin-top: 8px;
  }

  .url-input {
    flex: 1;
    min-width: 0;
    font-size: 12px;
    padding: 6px 8px;
    border-radius: 4px;
    border: 1px solid var(--border);
    background: var(--bg-tertiary);
    color: var(--text-primary);
    font-family: var(--font-mono, monospace);
  }

  .add-label {
    display: block;
    font-size: 12px;
    color: var(--text-secondary);
    margin-bottom: 2px;
  }

  .muted {
    font-size: 12px;
    color: var(--text-secondary);
    margin: 4px 0;
  }

  .error-text {
    font-size: 12px;
    color: #d83c3e;
    margin: 4px 0 8px;
  }

  /* ZEB-415 #1: discovery-off advisory — amber, distinct from .error-text. */
  .warn-text {
    font-size: 12px;
    color: #e0a23c;
    margin: 4px 0 8px;
  }

  /* Inline text-button (the "Enable" action inside the discovery warning). */
  .link-btn {
    font: inherit;
    padding: 0;
    border: none;
    background: none;
    color: var(--accent);
    text-decoration: underline;
    cursor: pointer;
  }

  .link-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  /* Phase 1b additions */

  .subsection {
    margin-top: 16px;
    padding-top: 12px;
    border-top: 1px solid var(--border);
  }

  .subsection-title {
    margin: 0 0 8px;
    font-size: 12px;
    font-weight: 600;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.4px;
  }

  .request-actions {
    display: flex;
    gap: 6px;
    flex-shrink: 0;
  }

  .accept-btn {
    flex-shrink: 0;
    font-size: 12px;
    padding: 4px 10px;
    border-radius: 4px;
    border: 1px solid var(--accent);
    background: transparent;
    color: var(--accent);
    cursor: pointer;
  }

  .accept-btn:hover:not(:disabled) {
    background: var(--accent);
    color: #fff;
  }

  .accept-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .toggle-row {
    display: flex;
    align-items: center;
    gap: 8px;
    cursor: pointer;
    user-select: none;
  }

  .toggle-checkbox {
    width: 14px;
    height: 14px;
    cursor: pointer;
    flex-shrink: 0;
  }

  .toggle-checkbox:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .toggle-label-text {
    font-size: 12px;
    color: var(--text-primary);
  }
</style>
