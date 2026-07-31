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
    OutboundFriendRequestDto,
    ReferralView,
    PeerIntroPolicy,
  } from '../friend-service';
  import type { DmInviteService, PendingDmInviteDto } from '../dm-invite-service';
  import {
    getIdentityDiscoverable,
    setIdentityDiscoverable,
    onIdentityDiscoverableChanged,
  } from '../connectivity-adapter';
  import { relativeTime } from '../file-utils';
  import Avatar from './Avatar.svelte';
  import type { ResolvedCard } from '../member-card-service';
  import type { OpenCardPayload } from './MemberRow.svelte';

  let {
    service,
    resolveCard,
    setFriendsBucket,
    onOpenCard,
    dmInviteService,
  }: {
    service: FriendService;
    /** ZEB-840: resolve an owner_id to its live card (name + avatar) via the
     *  app's single MemberCardService; reactive through App's cardVersion (read
     *  transitively when called in the template). Optional so unit tests can omit
     *  it; when absent, names fall back to the frozen hint / short-hex. */
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    /** ZEB-840: set the `friends` subscription bucket (friend + pending owners)
     *  on the shared service. Passing [] on unmount clears only this bucket,
     *  leaving the community / dm / voice buckets intact. Optional for tests. */
    setFriendsBucket?: (ownerIdHexes: string[]) => void;
    /** ZEB-419: open the owner-card drill-down popover (App's openMemberCard). */
    onOpenCard?: (payload: OpenCardPayload, ev: MouseEvent) => void;
    /** ZEB-236 T7: shared DM-invite service (App injects its single instance).
     *  Optional so existing instantiations/tests stay valid; the "DM invites"
     *  section renders only when this is provided AND the pending list is
     *  non-empty. */
    dmInviteService?: DmInviteService;
  } = $props();

  let friends = $state<FriendDto[]>([]);
  let loading = $state(true);
  let error = $state<string | null>(null);

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

  // ZEB-376 Phase 2b Task 14: per-row "request introduction" state, keyed by
  // `${viaOwnerIdHex}:${targetOwnerIdHex}` (a referral target could in
  // principle be browsed via more than one friend, so the composite key
  // avoids collisions between rows). `requestIntroInFlight` guards duplicate
  // clicks; `requestIntroStatus` holds a transient success/error message for
  // that row. Mirrors the referrals* trio above.
  let requestIntroInFlight = $state<Set<string>>(new Set());
  let requestIntroStatus = $state<Map<string, string>>(new Map());

  // ── Phase 1b state ────────────────────────────────────────────────────────

  // Pending friend requests inbox.
  let pendingRequests = $state<PendingFriendRequestDto[]>([]);
  let pendingLoading = $state(true);
  let pendingError = $state<string | null>(null);

  // Per-row in-flight accept/decline guard.
  let requestInFlight = $state<Set<string>>(new Set());

  // ── ZEB-236 T7: pending DM invites (from the injected DmInviteService) ─────
  // Only populated/subscribed when `dmInviteService` is provided. The section
  // renders only when this list is non-empty. Per-row accept/decline guard is
  // keyed by `spaceIdHex` (mirrors requestInFlight for friend requests).
  let dmInvites = $state<PendingDmInviteDto[]>([]);
  let dmInviteError = $state<string | null>(null);
  let dmInviteInFlight = $state<Set<string>>(new Set());
  let unsubscribeDmInviteChanged: (() => void) | null = null;

  // Add-by-key state.
  let addByKeyInput = $state('');
  let addingByKey = $state(false);
  let addByKeyStatus = $state<string | null>(null);

  // ── ZEB-784 / ZEB-783: outbound requests are owned by the NODE ────────────
  // The mutual-key handshake is synchronous with no server-push: after we send a
  // request the link only completes when WE dial again (once the peer accepts).
  //
  // ZEB-415 #2 originally solved that here, with a 10s/30-attempt timer chain
  // inside this component. That chain is gone: it could only run while this
  // panel was mounted, gave up after ~5 minutes, and did not exist at all for a
  // headless node — which is exactly the configuration ZEB-783 was measured on.
  // The node now performs the retry itself, durably and across restarts, so
  // there is exactly ONE retry owner. This panel's job is to SHOW that state
  // (rows below) and offer a manual "Retry now" for the user who is watching
  // and doesn't want to wait for the next node-side pass.
  let outboundRequests = $state<OutboundFriendRequestDto[]>([]);
  let outboundError = $state<string | null>(null);
  // Per-row in-flight guard for Retry now / Cancel, keyed by identityPubHex.
  let outboundInFlight = $state<Set<string>>(new Set());

  // Set once in onDestroy. Any async path that resumes after teardown (the
  // initial add awaiting `addByKey`, a row action awaiting `refresh`) checks
  // this before touching state.
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

  // ZEB-376 Phase 2b Task 14: inbound-introduction policy select. Mirrors the
  // autoAccept* quartet above. Default mirrors the Rust `PeerIntroPolicy`
  // derive default ('fof' — FriendsOfFriends) so the select shows a sane
  // value even before `loadPeerIntroPolicy()` resolves.
  let peerIntroPolicy = $state<PeerIntroPolicy>('fof');
  let peerIntroPolicyLoading = $state(true);
  let peerIntroPolicySaving = $state(false);
  let peerIntroPolicyError = $state<string | null>(null);

  // Unsubscribe handles for our event listeners (set in onMount).
  let unsubscribeChanged: (() => void) | null = null;
  let unsubscribePendingChanged: (() => void) | null = null;

  async function refresh(): Promise<void> {
    try {
      const next = await service.listFriends();
      // Don't assign list state after teardown — it would also re-trigger the
      // friends-bucket $effect (setFriendsBucket) while the panel is closed
      // (ZEB-415 liveness discipline).
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

  // ZEB-236 T7: re-fetch the pending DM-invite list. No-op when the service
  // wasn't injected. Guards post-teardown writes like refresh()/refreshPending().
  async function refreshDmInvites(): Promise<void> {
    if (!dmInviteService) return;
    try {
      const next = await dmInviteService.listPending();
      if (destroyed) return; // see refresh(): no post-teardown state writes
      dmInvites = next;
      dmInviteError = null;
    } catch (e) {
      dmInviteError = e instanceof Error ? e.message : String(e);
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

  // ZEB-376 Phase 2b Task 14: mirrors loadAutoAccept above.
  async function loadPeerIntroPolicy(): Promise<void> {
    try {
      peerIntroPolicy = await service.getPeerIntroPolicy();
      peerIntroPolicyError = null;
    } catch (e) {
      peerIntroPolicyError = e instanceof Error ? e.message : String(e);
    } finally {
      peerIntroPolicyLoading = false;
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
    // ZEB-840: name/avatar repaint is driven by App's shared cardVersion via the
    // resolveCard closure (read transitively in the template) — no local onUpdate
    // wiring needed now that there's a single MemberCardService instance.
    // Re-fetch friends whenever the backend signals a change.
    unsubscribeChanged = service.onFriendsChanged(() => {
      void refresh();
    });
    // Re-fetch pending requests on new inbound request or list mutation.
    unsubscribePendingChanged = service.onPendingRequestsChanged(() => {
      void refreshPending();
      // ZEB-783: the same events that mutate the inbound list also mutate the
      // outbound one — a node-side retry that links emits `friend-list-changed`
      // and clears the record, so the row must disappear without a reload.
      void refreshOutbound();
    });
    // ZEB-236 T7: re-fetch pending DM invites on new-invite / list-mutated
    // (accept/decline, possibly from another device). Only when injected.
    if (dmInviteService) {
      unsubscribeDmInviteChanged = dmInviteService.onPendingChanged(() => {
        void refreshDmInvites();
      });
      void refreshDmInvites();
    }
    void refresh();
    void refreshPending();
    void refreshOutbound();
    void loadAutoAccept();
    void loadPeerIntroPolicy();
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
    unsubscribeDmInviteChanged?.();
    unsubscribeDmInviteChanged = null;
    unsubscribeDiscoverable?.();
    unsubscribeDiscoverable = null;
    // Block any async path that resumes after teardown (initial add / retry).
    destroyed = true;
    // Cancel any in-flight `loadDiscoverable` read so its late resolve can't
    // write `identityDiscoverable` after we've unmounted (CodeRabbit).
    discoverableGen += 1;
    // ZEB-840: clear ONLY the friends bucket on unmount — the shared instance's
    // community / dm / voice buckets must survive (unlike the old dedicated
    // instance's unsubscribeAll).
    setFriendsBucket?.([]);
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

  // ZEB-376 Phase 2b Task 14: ask a friend (`viaOwnerIdHex`, the friend whose
  // catalog we're browsing) to introduce us to one of their referrable friends
  // (`targetOwnerIdHex`, the referral entry). The eventual link is async — it
  // surfaces later via `friend-list-changed` — so this only reports whether the
  // request was successfully SENT. Mirrors handleBrowseReferrals's per-row
  // in-flight guard + transient status pattern.
  async function handleRequestIntro(viaOwnerIdHex: string, targetOwnerIdHex: string): Promise<void> {
    const key = `${viaOwnerIdHex}:${targetOwnerIdHex}`;
    if (requestIntroInFlight.has(key)) return;
    requestIntroInFlight = new Set(requestIntroInFlight).add(key);
    try {
      await service.requestIntroduction(viaOwnerIdHex, targetOwnerIdHex);
      requestIntroStatus = new Map(requestIntroStatus).set(key, 'Introduction requested.');
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      requestIntroStatus = new Map(requestIntroStatus).set(key, `Failed: ${msg}`);
    } finally {
      const next = new Set(requestIntroInFlight);
      next.delete(key);
      requestIntroInFlight = next;
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

  // Map the SpaceKind wire tag ('d'/'g') to a human label; fall back to the
  // raw tag for any unexpected value (owner_state_types.rs SpaceKind).
  function dmInviteKindLabel(kind: string): string {
    if (kind === 'd') return 'DM';
    if (kind === 'g') return 'Group DM';
    return kind;
  }

  // ── ZEB-236 T7: DM-invite accept/decline (mirror handleAccept/handleDecline,
  //    keyed by spaceIdHex; failures surface in the section's dmInviteError) ──
  async function handleDmInviteAccept(spaceIdHex: string): Promise<void> {
    if (!dmInviteService || dmInviteInFlight.has(spaceIdHex)) return;
    dmInviteInFlight = new Set(dmInviteInFlight).add(spaceIdHex);
    try {
      await dmInviteService.accept(spaceIdHex);
      await refreshDmInvites();
    } catch (e) {
      dmInviteError = e instanceof Error ? e.message : String(e);
    } finally {
      const next = new Set(dmInviteInFlight);
      next.delete(spaceIdHex);
      dmInviteInFlight = next;
    }
  }

  async function handleDmInviteDecline(spaceIdHex: string): Promise<void> {
    if (!dmInviteService || dmInviteInFlight.has(spaceIdHex)) return;
    dmInviteInFlight = new Set(dmInviteInFlight).add(spaceIdHex);
    try {
      await dmInviteService.decline(spaceIdHex);
      await refreshDmInvites();
    } catch (e) {
      dmInviteError = e instanceof Error ? e.message : String(e);
    } finally {
      const next = new Set(dmInviteInFlight);
      next.delete(spaceIdHex);
      dmInviteInFlight = next;
    }
  }

  // ZEB-783: re-fetch this user's own unanswered outbound requests. Guards
  // post-teardown writes like refresh()/refreshPending().
  async function refreshOutbound(): Promise<void> {
    try {
      const next = await service.listOutboundRequests();
      if (destroyed) return; // see refresh(): no post-teardown state writes
      outboundRequests = next;
      outboundError = null;
    } catch (e) {
      // Guard the FAILURE path too: `listOutboundRequests` can reject after the
      // panel unmounts, and assigning here would mutate $state on a destroyed
      // component exactly as the success path above is careful not to.
      if (destroyed) return;
      outboundError = e instanceof Error ? e.message : String(e);
    }
  }

  // ZEB-784: re-dial one outbound request NOW rather than waiting for the node's
  // next pass. This is the ONLY dial this component initiates on a pending
  // request — the node owns the recurring retry, so there is no timer here to
  // supersede, cancel, or leak.
  async function handleRetryOutbound(identityPubHex: string): Promise<void> {
    if (outboundInFlight.has(identityPubHex)) return;
    outboundInFlight = new Set(outboundInFlight).add(identityPubHex);
    try {
      const outcome = await service.addByKey(identityPubHex);
      if (destroyed) return;
      if (outcome.kind === 'linked') {
        addByKeyStatus = `Now connected with ${outcome.display ?? shortId(outcome.ownerIdHex)}`;
        await refresh();
      } else if (outcome.kind === 'unreachable') {
        addByKeyStatus = "Still couldn't reach them — we'll keep trying in the background.";
      } else {
        addByKeyStatus = "They haven't accepted yet — we'll keep trying in the background.";
      }
    } catch (e) {
      // Surface it rather than hide it behind the calm "we'll keep trying"
      // copy, but don't drop the row: the node retries regardless of what one
      // manual attempt did.
      addByKeyStatus = `Retry failed: ${e instanceof Error ? e.message : String(e)}`;
    } finally {
      if (!destroyed) {
        const next = new Set(outboundInFlight);
        next.delete(identityPubHex);
        outboundInFlight = next;
        await refreshOutbound();
      }
    }
  }

  // ZEB-783: stop retrying an outbound request and drop the row. Local only —
  // nothing was ever stored on the target, so there is nothing to withdraw.
  async function handleCancelOutbound(identityPubHex: string): Promise<void> {
    if (outboundInFlight.has(identityPubHex)) return;
    outboundInFlight = new Set(outboundInFlight).add(identityPubHex);
    try {
      await service.cancelOutboundRequest(identityPubHex);
      if (destroyed) return;
      // Clear the shared status line. handleAddByKey / handleRetryOutbound both
      // write "we'll keep trying…" into it, and after a cancel that message
      // outlives the row it described — telling the user retries continue for a
      // request they just stopped.
      addByKeyStatus = null;
      await refreshOutbound();
    } catch (e) {
      outboundError = e instanceof Error ? e.message : String(e);
    } finally {
      if (!destroyed) {
        const next = new Set(outboundInFlight);
        next.delete(identityPubHex);
        outboundInFlight = next;
      }
    }
  }

  async function handleAddByKey(): Promise<void> {
    const key = addByKeyInput.trim();
    if (addingByKey || key.length === 0) return;
    addingByKey = true;
    addByKeyStatus = null;
    try {
      const outcome = await service.addByKey(key);
      // The panel may have been torn down while the add was in flight — don't
      // mutate state after unmount (CodeAnt).
      if (destroyed) return;
      if (outcome.kind === 'linked') {
        addByKeyStatus = `Connected with ${outcome.display ?? shortId(outcome.ownerIdHex)}`;
        addByKeyInput = '';
        await refresh();
      } else if (outcome.kind === 'pending') {
        addByKeyStatus =
          "Request sent — they'll need to accept. We'll keep trying until they do.";
        addByKeyInput = '';
        await refreshPending();
        // The node recorded this request; show it in the outbound list so the
        // user has durable evidence the add happened (ZEB-783 — previously it
        // vanished from every surface the moment this message faded).
        if (destroyed) return;
        await refreshOutbound();
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

  // ZEB-376 Phase 2b Task 14: mirrors handleAutoAcceptToggle above, but the new
  // value comes from the <select>'s change event rather than a toggle.
  async function handlePeerIntroPolicyChange(e: Event): Promise<void> {
    if (peerIntroPolicySaving) return;
    const next = (e.target as HTMLSelectElement).value as PeerIntroPolicy;
    peerIntroPolicySaving = true;
    try {
      await service.setPeerIntroPolicy(next);
      peerIntroPolicy = next;
      peerIntroPolicyError = null;
    } catch (err) {
      peerIntroPolicyError = err instanceof Error ? err.message : String(err);
    } finally {
      peerIntroPolicySaving = false;
    }
  }

  function shortId(hex: string): string {
    return hex.length > 12 ? `${hex.slice(0, 12)}…` : hex;
  }

  // ── ZEB-419 / ZEB-840: live owner-card resolution (name + avatar) ─────────
  // resolveCard reads App's shared cardVersion internally, so calling it in the
  // template makes the rows repaint when a poll/event fills a card. Empty strings
  // fall through (`||` not `??`) so a blank card name never masks a usable hint.
  function cardName(ownerIdHex: string): string | undefined {
    return resolveCard?.(ownerIdHex)?.displayName || undefined;
  }
  function cardAvatarUrl(ownerIdHex: string): string | undefined {
    return resolveCard?.(ownerIdHex)?.avatarUrl;
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
    const resolved = resolveCard?.(ownerIdHex);
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

  // ZEB-840: drive the shared service's `friends` bucket from the current friend
  // + pending owner_id set whenever those lists change. setBucket is idempotent
  // and reconciles the union (dropping owners that left the lists), and — unlike
  // the old subscribeVisible — cannot unsubscribe the community/dm/voice buckets.
  $effect(() => {
    // Belt-and-suspenders: never (re)subscribe after teardown. The refresh
    // guards above stop list state changing post-unmount, and Svelte disposes
    // this effect on destroy — this check makes a stray re-run a no-op too.
    if (destroyed) return;
    const ids = [
      ...friends.map((f) => f.ownerIdHex),
      ...pendingRequests.map((r) => r.ownerIdHex),
    ];
    setFriendsBucket?.(ids);
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
                  {@const introKey = `${f.ownerIdHex}:${r.ownerIdHex}`}
                  <li class="referral-item" data-testid="referral-item">
                    <span class="referral-name" title={r.ownerIdHex}>
                      {r.display ?? shortId(r.ownerIdHex)}
                    </span>
                    {#if r.alreadyFriend}
                      <span class="already-friend-badge" data-testid="already-friend-badge">
                        already friends
                      </span>
                    {:else}
                      <button
                        type="button"
                        class="secondary-btn small-btn"
                        disabled={requestIntroInFlight.has(introKey)}
                        onclick={() => handleRequestIntro(f.ownerIdHex, r.ownerIdHex)}
                        data-testid="request-intro-btn"
                      >
                        {requestIntroInFlight.has(introKey) ? 'Requesting…' : 'Request introduction'}
                      </button>
                    {/if}
                    {#if requestIntroStatus.has(introKey)}
                      <span class="muted request-intro-status" data-testid="request-intro-status">
                        {requestIntroStatus.get(introKey)}
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
              {#if req.introducedBy}
                <!-- ZEB-376 Task 11/14: this row is an AskMe-staged introduction
                     offer rather than a plain Path-A link request — badge the
                     voucher (F) so the user knows who's vouching before they
                     Accept/Decline. -->
                <span class="introduced-by-badge" data-testid="introduced-by-badge-{req.ownerIdHex}">
                  introduced by {shortId(req.introducedBy)}
                </span>
              {/if}
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

  <!-- ── ZEB-783: outbound requests — the mirror of the inbox above ─────── -->
  <!-- Rendered only when there is something waiting, so the panel doesn't grow
       an empty section for the common case. Before this, an add that returned
       `pending` left NO trace anywhere: the user saw a UI state indistinguishable
       from having done nothing at all. -->
  {#if outboundRequests.length > 0 || outboundError}
    <div class="subsection" data-testid="outbound-requests-section">
      <h5 class="subsection-title">Sent requests</h5>

      {#if outboundError}
        <p class="error-text" data-testid="outbound-error">{outboundError}</p>
      {/if}

      <ul class="friend-list" data-testid="outbound-list">
        {#each outboundRequests as req (req.identityPubHex)}
          <li class="friend-row">
            <div class="friend-id">
              <!-- No name and no avatar, deliberately: a peer who hasn't
                   accepted has disclosed neither their owner id nor their
                   display name, so the only honest thing to show is the key
                   the user typed. -->
              <span class="friend-name" data-testid="outbound-status-{req.identityPubHex}">
                Waiting for them to accept
              </span>
              <span class="friend-addr" data-testid="outbound-key-{req.identityPubHex}">
                {shortId(req.identityPubHex)} · sent {relativeTime(req.requestedAtMs)}
              </span>
            </div>
            <div class="request-actions">
              <button
                type="button"
                class="accept-btn"
                disabled={outboundInFlight.has(req.identityPubHex)}
                onclick={() => handleRetryOutbound(req.identityPubHex)}
                data-testid="outbound-retry-{req.identityPubHex}"
                title="Try again now — we're already retrying in the background"
              >
                {outboundInFlight.has(req.identityPubHex) ? '…' : 'Retry now'}
              </button>
              <button
                type="button"
                class="unfriend-btn"
                disabled={outboundInFlight.has(req.identityPubHex)}
                onclick={() => handleCancelOutbound(req.identityPubHex)}
                data-testid="outbound-cancel-{req.identityPubHex}"
                title="Stop trying to connect with this key"
              >
                {outboundInFlight.has(req.identityPubHex) ? '…' : 'Cancel'}
              </button>
            </div>
          </li>
        {/each}
      </ul>
    </div>
  {/if}

  <!-- ── ZEB-236 T7: DM invites inbox ───────────────────────────────────── -->
  <!-- Rendered only when the service is injected AND there are pending invites.
       Inviters are non-friends (no nickname / no card), so the row shows the
       short inviter hex + invite kind + relative received time. -->
  {#if dmInviteService && dmInvites.length > 0}
    <div class="subsection" data-testid="dm-invites-section">
      <h5 class="subsection-title">DM invites</h5>

      {#if dmInviteError}
        <p class="error-text" data-testid="dm-invite-error">{dmInviteError}</p>
      {/if}

      <ul class="friend-list" data-testid="dm-invite-list">
        {#each dmInvites as invite (invite.spaceIdHex)}
          <li class="friend-row">
            <div class="friend-id">
              <span
                class="friend-name"
                data-testid="dm-invite-inviter-{invite.spaceIdHex}"
              >{invite.inviterOwnerIdHex.slice(0, 8)}…</span>
              <span class="friend-addr">{dmInviteKindLabel(invite.kind)} · {relativeTime(invite.receivedAtMs)}</span>
            </div>
            <div class="request-actions">
              <button
                type="button"
                class="accept-btn"
                disabled={dmInviteInFlight.has(invite.spaceIdHex)}
                onclick={() => handleDmInviteAccept(invite.spaceIdHex)}
                data-testid="dm-invite-accept-btn"
              >
                {dmInviteInFlight.has(invite.spaceIdHex) ? '…' : 'Accept'}
              </button>
              <button
                type="button"
                class="unfriend-btn"
                disabled={dmInviteInFlight.has(invite.spaceIdHex)}
                onclick={() => handleDmInviteDecline(invite.spaceIdHex)}
                data-testid="dm-invite-decline-btn"
              >
                {dmInviteInFlight.has(invite.spaceIdHex) ? '…' : 'Decline'}
              </button>
            </div>
          </li>
        {/each}
      </ul>
    </div>
  {/if}

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

  <!-- ── ZEB-376 Phase 2b Task 14: inbound-introduction policy ───────────── -->
  <div class="action-block" data-testid="peer-intro-policy-section">
    {#if peerIntroPolicyError}
      <p class="error-text" data-testid="peer-intro-policy-error">{peerIntroPolicyError}</p>
    {/if}
    <label class="add-label" for="peer-intro-policy-select">
      Who can ask a mutual friend to introduce them to me
    </label>
    <select
      id="peer-intro-policy-select"
      class="policy-select"
      value={peerIntroPolicy}
      disabled={peerIntroPolicyLoading || peerIntroPolicySaving}
      onchange={handlePeerIntroPolicyChange}
      data-testid="peer-intro-policy-select"
    >
      <option value="open">Open</option>
      <option value="fof">Friends of friends</option>
      <option value="ask">Ask me</option>
      <option value="closed">Closed</option>
    </select>
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
    font-family: var(--font-ui);
  }

  .nickname-input {
    flex: 0 1 150px;
    min-width: 90px;
    font-size: 12px;
    padding: 4px 6px;
    border-radius: 5px;
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
    flex-wrap: wrap;
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
    padding: 1px 8px;
    border-radius: 20px;
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    border: 1px solid var(--border);
    font-family: var(--font-ui);
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
    font-family: var(--font-mono);
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
    border-radius: 5px;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
    cursor: pointer;
    font-family: var(--font-ui);
  }

  .unfriend-btn:hover:not(:disabled) {
    border-color: var(--danger-muted);
    color: var(--danger-muted);
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
    border-radius: 5px;
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--on-accent);
    cursor: pointer;
    font-family: var(--font-ui);
  }

  .primary-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .secondary-btn {
    font-size: 13px;
    padding: 6px 12px;
    border-radius: 5px;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-primary);
    cursor: pointer;
    font-family: var(--font-ui);
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
    border-radius: 5px;
    border: 1px solid var(--border);
    background: var(--bg-tertiary);
    color: var(--text-primary);
    font-family: var(--font-mono);
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
    color: var(--danger-muted);
    margin: 4px 0 8px;
  }

  /* ZEB-415 #1: discovery-off advisory — amber, distinct from .error-text. */
  .warn-text {
    font-size: 12px;
    color: var(--warning);
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
    border-radius: 5px;
    border: 1px solid var(--accent);
    background: transparent;
    color: var(--accent);
    cursor: pointer;
    font-family: var(--font-ui);
  }

  .accept-btn:hover:not(:disabled) {
    background: var(--accent);
    color: var(--on-accent);
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

  /* ZEB-376 Phase 2b Task 14: inbound-introduction policy select. */
  .policy-select {
    font-size: 12px;
    padding: 6px 8px;
    border-radius: 5px;
    border: 1px solid var(--border);
    background: var(--bg-tertiary);
    color: var(--text-primary);
    font-family: var(--font-ui);
  }

  .policy-select:disabled {
    opacity: 0.5;
    cursor: default;
  }

  /* ZEB-376 Task 11/14: "introduced by F" offer badge — mirrors
     .already-friend-badge's pill shape. */
  .introduced-by-badge {
    align-self: flex-start;
    font-size: 10px;
    padding: 1px 8px;
    border-radius: 20px;
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    border: 1px solid var(--border);
    font-family: var(--font-ui);
  }

  .request-intro-status {
    flex-basis: 100%;
  }
</style>
