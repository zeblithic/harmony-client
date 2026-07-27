<script lang="ts">
  import { untrack } from 'svelte';
  import type { VineVideo } from '../types';
  import VineCard from './VineCard.svelte';
  import ReshareConfirmDialog from './ReshareConfirmDialog.svelte';
  import ConfirmDialog from './ConfirmDialog.svelte';
  import { pickCenterIndex, isOwnOriginalVine } from '../vine-utils';

  type FeedFilter = 'all' | 'unviewed';
  type FeedTab = 'following' | 'discover';

  let {
    followedVines = [],
    discoverVines = [],
    viewedIds,
    activeTab = 'following' as FeedTab,
    followedAddresses = new Set<string>(),
    onTabChange,
    onMarkViewed,
    onPublish,
    onReshare,
    onDelete,
    onFollow,
    onUnfollow,
    resolveVideo,
    getReaction,
    onToggleLike,
    onViewOriginal,
    playTarget = null,
    onPlayTargetConsumed,
    ownAddress,
    getShareFollows,
    onSetShareFollows,
    getShareVinesPublicly,
    onSetShareVinesPublicly,
  }: {
    followedVines?: VineVideo[];
    discoverVines?: VineVideo[];
    viewedIds: Set<string>;
    activeTab?: FeedTab;
    followedAddresses?: Set<string>;
    onTabChange?: (tab: FeedTab) => void;
    /** Fires when a card BECOMES the playing card (autoplay-on-view = viewed). */
    onMarkViewed?: (id: string) => void;
    onPublish?: () => void;
    onReshare?: (vine: VineVideo) => Promise<void> | void;
    /** ZEB-670: delete an own vine (creator tombstone). Confirm is feed-owned. */
    onDelete?: (vine: VineVideo) => Promise<void> | void;
    onFollow?: (address: string, name: string) => void;
    onUnfollow?: (address: string) => void;
    resolveVideo?: (cid: string) => Promise<string>;
    getReaction?: (vineId: string) => { count: number; likedByMe: boolean };
    onToggleLike?: (vine: VineVideo) => void;
    onViewOriginal?: (vineId: string) => void;
    /**
     * Parent-controlled "navigate to this vine" request (attribution links,
     * App.svelte handleViewOriginal). The feed IS the player now: consuming
     * a target scrolls its card to the snap position and plays it, switching
     * tab / clearing the unviewed filter when they would hide it. Consumed →
     * onPlayTargetConsumed nulls the slot for the next click.
     */
    playTarget?: VineVideo | null;
    onPlayTargetConsumed?: () => void;
    /** Local node's hex address — own-original reshare suppression (FIX 2, PR #120). */
    ownAddress?: string;
    /** ZEB-671: read the backend share_follows setting (Tune sheet). */
    getShareFollows?: () => Promise<boolean>;
    /** ZEB-671: flip the backend share_follows setting (Tune sheet). */
    onSetShareFollows?: (on: boolean) => Promise<void>;
    /** ZEB-811: read the backend share_vines_publicly setting (Tune sheet). */
    getShareVinesPublicly?: () => Promise<boolean>;
    /** ZEB-811: flip the backend share_vines_publicly setting (Tune sheet). */
    onSetShareVinesPublicly?: (on: boolean) => Promise<void>;
  } = $props();

  let feedFilter = $state<FeedFilter>('all');

  // ── ZEB-671: Tune prefs (frontend-local; the graph itself is backend) ──
  const TUNE_KEY = 'harmony.vines.tune.v1';
  function loadTunePrefs(): { deg2: boolean; deg3: boolean; muted: string[] } {
    try {
      const raw = localStorage.getItem(TUNE_KEY);
      if (raw) {
        const p = JSON.parse(raw) as { deg2?: boolean; deg3?: boolean; muted?: string[] };
        return { deg2: p.deg2 !== false, deg3: p.deg3 !== false, muted: p.muted ?? [] };
      }
    } catch {
      // Missing/corrupt storage → defaults.
    }
    return { deg2: true, deg3: true, muted: [] };
  }
  const tuneInit = loadTunePrefs();
  /** Show 2nd-degree vines (someone your follow follows). */
  let deg2 = $state(tuneInit.deg2);
  /** Show 3rd-degree vines (a follow-of-a-follow-of-a-follow). */
  let deg3 = $state(tuneInit.deg3);
  /** Follow roots whose Discover influence is muted (via[0] addresses). */
  let muted = $state(new Set<string>(tuneInit.muted));
  let tuneOpen = $state(false);
  /** Backend share_follows state, loaded when the Tune sheet opens. */
  let shareFollows = $state(true);
  /** True once getShareFollows() has succeeded — the toggle stays
   *  disabled until then so a load failure can't present an unknown
   *  privacy state as togglable (CodeRabbit PR #447). */
  let shareFollowsLoaded = $state(false);
  let shareFollowsBusy = $state(false);
  let shareFollowsError = $state('');
  /** Backend share_vines_publicly state, loaded when the Tune sheet opens. */
  let shareVinesPublicly = $state(true);
  /** True once getShareVinesPublicly() has succeeded — same disabled-until-
   *  read gate as shareFollowsLoaded above. */
  let shareVinesPubliclyLoaded = $state(false);
  let shareVinesPubliclyBusy = $state(false);
  let shareVinesPubliclyError = $state('');
  /** Focus target when the Tune sheet opens (dialog a11y). */
  let tuneSheetEl = $state<HTMLDivElement | null>(null);

  function saveTunePrefs() {
    try {
      localStorage.setItem(
        TUNE_KEY,
        JSON.stringify({ deg2, deg3, muted: [...muted] }),
      );
    } catch {
      // Storage unavailable — prefs stay session-local.
    }
  }

  let playingId = $state<string | null>(null);
  /**
   * Card kept visible under the Unviewed filter even after becoming viewed —
   * set when a card starts playing WHILE the filter is active, so the list
   * doesn't yank it away mid-playback. Cleared on filter/tab switches so the
   * strict unviewed view (and its all-caught-up state) stays reachable.
   */
  let unviewedPin = $state<string | null>(null);
  /** cid → blob URL, bounded to the lazy window (playing ± 1). */
  let videoUrls = $state(new Map<string, string>());
  /** cid → seconds; sticky once any mounted video reports metadata. */
  let durations = $state(new Map<string, number>());
  let reshareTarget = $state<VineVideo | null>(null);
  let resharingId = $state<string | null>(null);
  let reshareError = $state('');
  /** ZEB-670: delete-confirmation trio, mirroring the reshare trio above. */
  let deleteTarget = $state<VineVideo | null>(null);
  let deletingId = $state<string | null>(null);
  let deleteError = $state('');
  let feedListEl = $state<HTMLDivElement | null>(null);
  /** Card id to scroll to once it exists in the rendered list. */
  let pendingScrollId = $state<string | null>(null);
  let scrollScheduled = false;
  /** cids with an in-flight resolveVideo call (non-reactive bookkeeping). */
  const pendingCids = new Set<string>();

  // ZEB-671: Discover is graph-only — a vine renders only when its
  // creator has a transitive-follow degree (2° or 3°), honoring the Tune
  // toggles and muted roots. Own offline-fallback publishes
  // (creatorAddress 'self') stay visible: they are not discovery.
  let discoverGraphVines = $derived(
    discoverVines.filter(v =>
      v.creatorAddress === 'self'
      || (((v.degree === 2 && deg2) || (v.degree === 3 && deg3))
        && !(v.via?.[0] != null && muted.has(v.via[0])))
    )
  );

  let activeVines = $derived(
    activeTab === 'following' ? followedVines : discoverGraphVines
  );

  let sortedVines = $derived(
    [...activeVines].sort((a, b) => b.createdAt - a.createdAt)
  );

  let filteredVines = $derived(
    activeTab === 'following' && feedFilter === 'unviewed'
      ? sortedVines.filter(v => !viewedIds.has(v.id) || v.id === unviewedPin)
      : sortedVines
  );

  let unviewedCount = $derived(
    followedVines.filter(v => !viewedIds.has(v.id)).length
  );

  // True "all caught up": the user HAS followed vines but none are unviewed.
  // This is the only empty state where the "Share a vine" CTA is noise (the
  // right action is "switch to All"). A zero-vine feed on the Unviewed filter
  // is NOT all-caught-up — it still needs the create path. (CodeAnt, PR #333.)
  let allCaughtUp = $derived(
    activeTab === 'following' && feedFilter === 'unviewed' && followedVines.length > 0
  );

  // Single-pass reshare-count index over both feeds. Per-card lookup
  // (`reshareCountMap.get(vine.id) ?? 0`) is O(1), so the full feed
  // render is O(N) total instead of the O(N²) it would be if each
  // card called `getReshareCount` and re-filtered both arrays.
  // See FIX 5 in PR #120 round 1.
  let reshareCountMap = $derived.by(() => {
    const map = new Map<string, number>();
    for (const v of followedVines) {
      if (v.reshareOf) map.set(v.reshareOf, (map.get(v.reshareOf) ?? 0) + 1);
    }
    for (const v of discoverVines) {
      if (v.reshareOf) map.set(v.reshareOf, (map.get(v.reshareOf) ?? 0) + 1);
    }
    return map;
  });

  // ZEB-671: addr → display name for provenance lines. Descriptor
  // creator names are the same source every card already renders;
  // unknown addresses fall back to a truncated hex.
  let nameByAddr = $derived.by(() => {
    const m = new Map<string, string>();
    for (const v of [...followedVines, ...discoverVines]) {
      if (!m.has(v.creatorAddress)) m.set(v.creatorAddress, v.creatorName);
    }
    return m;
  });

  function nameOf(addr: string): string {
    return nameByAddr.get(addr) ?? `${addr.slice(0, 8)}…`;
  }

  function degreeLabelOf(v: VineVideo): string | null {
    return v.degree === 2 ? '2nd' : v.degree === 3 ? '3rd' : null;
  }

  /** "Devin follows @ravi" (2°) / "Devin → @ravi → @ada" (3°). */
  function viaLabelOf(v: VineVideo): string | null {
    const via = v.via;
    if (!via || via.length === 0) return null;
    if (v.degree === 2) return `${nameOf(via[0])} follows @${v.creatorName}`;
    if (v.degree === 3 && via.length >= 2) {
      return `${nameOf(via[0])} → @${nameOf(via[1])} → @${v.creatorName}`;
    }
    return null;
  }

  /** Unique provenance roots across the UNFILTERED discover pool — muted
   *  roots must stay listed so they can be unmuted. */
  let muteRoots = $derived.by(() => {
    const set = new Set<string>();
    for (const v of discoverVines) {
      if (v.via?.[0] != null) set.add(v.via[0]);
    }
    return [...set].sort();
  });

  function toggleMute(root: string) {
    const next = new Set(muted);
    if (next.has(root)) next.delete(root);
    else next.add(root);
    muted = next;
    saveTunePrefs();
  }

  async function openTune() {
    tuneOpen = true;
    shareFollowsError = '';
    if (getShareFollows) {
      try {
        shareFollows = await getShareFollows();
        shareFollowsLoaded = true;
      } catch (e: unknown) {
        const msg = e instanceof Error ? e.message : String(e);
        shareFollowsError = msg;
        shareFollowsLoaded = false;
      }
    }
    shareVinesPubliclyError = '';
    if (getShareVinesPublicly) {
      try {
        shareVinesPublicly = await getShareVinesPublicly();
        shareVinesPubliclyLoaded = true;
      } catch (e: unknown) {
        const msg = e instanceof Error ? e.message : String(e);
        shareVinesPubliclyError = msg;
        shareVinesPubliclyLoaded = false;
      }
    }
  }

  async function handleShareFollowsToggle(e: Event) {
    const input = e.currentTarget as HTMLInputElement;
    const want = input.checked;
    if (!onSetShareFollows) return;
    shareFollowsBusy = true;
    try {
      await onSetShareFollows(want);
      shareFollows = want;
      shareFollowsError = '';
    } catch (err: unknown) {
      const msg = err instanceof Error ? err.message : String(err);
      shareFollowsError = msg;
      input.checked = shareFollows;
    } finally {
      shareFollowsBusy = false;
    }
  }

  async function handleShareVinesPubliclyToggle(e: Event) {
    const input = e.currentTarget as HTMLInputElement;
    const want = input.checked;
    if (!onSetShareVinesPublicly) return;
    shareVinesPubliclyBusy = true;
    try {
      await onSetShareVinesPublicly(want);
      shareVinesPublicly = want;
      shareVinesPubliclyError = '';
    } catch (err: unknown) {
      const msg = err instanceof Error ? err.message : String(err);
      shareVinesPubliclyError = msg;
      input.checked = shareVinesPublicly;
    } finally {
      shareVinesPubliclyBusy = false;
    }
  }

  // Dialog a11y (CodeRabbit PR #447): focus moves into the sheet on
  // open; Escape dismisses.
  $effect(() => {
    if (tuneOpen) tuneSheetEl?.focus();
  });

  // ── Center-detection autoplay ───────────────────────────────────────

  function setPlaying(id: string) {
    if (playingId === id) return;
    playingId = id;
    // Playing under the active Unviewed filter pins the card (it's about to
    // become viewed); anywhere else the pin is stale — drop it.
    unviewedPin =
      activeTab === 'following' && feedFilter === 'unviewed' ? id : null;
    onMarkViewed?.(id);
  }

  function recomputePlaying() {
    const list = feedListEl;
    if (!list) return;
    const rows = Array.from(list.querySelectorAll<HTMLElement>('[data-vine-id]'));
    if (rows.length === 0) {
      playingId = null;
      return;
    }
    const listRect = list.getBoundingClientRect();
    const viewportCenter = listRect.top + listRect.height / 2;
    const centers = rows.map(r => {
      const rect = r.getBoundingClientRect();
      return rect.top + rect.height / 2;
    });
    const idx = pickCenterIndex(centers, viewportCenter);
    if (idx >= 0) setPlaying(rows[idx].dataset.vineId!);
  }

  function onFeedScroll() {
    if (scrollScheduled) return;
    scrollScheduled = true;
    requestAnimationFrame(() => {
      scrollScheduled = false;
      recomputePlaying();
    });
  }

  // Re-detect on mount and when the playing card LEAVES the rendered list
  // (filter/tab switch, feed emptied). List mutations that keep the playing
  // card (new arrivals, viewed-set updates) must NOT move playback — only
  // scroll events and explicit activation do. Effects run post-render, so
  // the [data-vine-id] rows are queryable; jsdom rects are all zeros →
  // index 0 wins the tie → the first (newest) card plays on mount.
  $effect(() => {
    const list = filteredVines;
    if (!feedListEl) return;
    const stillListed = playingId != null && list.some(v => v.id === playingId);
    if (!stillListed) untrack(recomputePlaying);
  });

  function activateCard(vine: VineVideo) {
    setPlaying(vine.id);
    pendingScrollId = vine.id;
  }

  // Parent-driven navigation: the feed is the player now.
  $effect(() => {
    if (!playTarget) return;
    const target = playTarget;
    untrack(() => {
      onPlayTargetConsumed?.();
      const inFollowed = followedVines.some(v => v.id === target.id);
      const inDiscover = discoverVines.some(v => v.id === target.id);
      // Not in the local feed (e.g. original never surfaced): the click is
      // silently ignored — same posture as the old player seed path.
      if (!inFollowed && !inDiscover) return;
      const wantTab: FeedTab = inFollowed ? 'following' : 'discover';
      if (activeTab !== wantTab) onTabChange?.(wantTab);
      if (wantTab === 'following' && feedFilter === 'unviewed' && viewedIds.has(target.id)) {
        feedFilter = 'all';
      }
      setPlaying(target.id);
      pendingScrollId = target.id;
    });
  });

  // Scroll the pending card into the snap position once it's rendered
  // (after a tab switch the row appears a tick later; re-run on list change).
  $effect(() => {
    void filteredVines;
    const id = pendingScrollId;
    if (!id || !feedListEl) return;
    const el = feedListEl.querySelector<HTMLElement>(`[data-vine-id="${CSS.escape(id)}"]`);
    if (el) {
      el.scrollIntoView?.({ block: 'start' });
      pendingScrollId = null;
    }
  });

  // ── Lazy blob window (playing ± 1) ──────────────────────────────────

  // Windowed by CARD (vine id), not by CID: reshares carry the original's
  // videoCid, so a cid-keyed window would hand a blob URL to every far-away
  // reshare of the playing clip and mount videos outside the window
  // (Qodo PR #440). The blob map itself stays cid-keyed so windowed cards
  // sharing a CID still share one fetch + one URL.
  let windowIds = $derived.by(() => {
    const idx = filteredVines.findIndex(v => v.id === playingId);
    const around = idx === -1 ? [0, 1] : [idx - 1, idx, idx + 1];
    const set = new Set<string>();
    for (const i of around) {
      const v = filteredVines[i];
      if (v) set.add(v.id);
    }
    return set;
  });

  let windowCids = $derived.by(() => {
    const set = new Set<string>();
    for (const v of filteredVines) {
      if (windowIds.has(v.id)) set.add(v.videoCid);
    }
    return set;
  });

  $effect(() => {
    const want = windowCids;
    // Revoke and drop URLs whose cards left the window.
    let next: Map<string, string> | null = null;
    for (const [cid, url] of videoUrls) {
      if (!want.has(cid)) {
        URL.revokeObjectURL?.(url);
        (next ??= new Map(videoUrls)).delete(cid);
      }
    }
    if (next) videoUrls = next;
    const resolver = resolveVideo;
    if (!resolver) return;
    for (const cid of want) {
      if (videoUrls.has(cid) || pendingCids.has(cid)) continue;
      pendingCids.add(cid);
      resolver(cid)
        .then(url => {
          pendingCids.delete(cid);
          // The component may have unmounted, or the window may have moved,
          // while the fetch was in flight — either way this URL must be
          // revoked here or it leaks (the destroy cleanup already ran /
          // will never see it). Qodo PR #440.
          if (destroyed || !windowCids.has(cid)) {
            URL.revokeObjectURL?.(url);
            return;
          }
          videoUrls = new Map(videoUrls).set(cid, url);
        })
        .catch(() => {
          pendingCids.delete(cid);
        });
    }
  });

  /** Flipped at destroy so late-resolving fetches revoke instead of keep. */
  let destroyed = false;

  // Revoke everything on unmount (body reads nothing reactive → runs once,
  // so the returned cleanup fires only at destroy).
  $effect(() => () => {
    destroyed = true;
    for (const url of videoUrls.values()) URL.revokeObjectURL?.(url);
  });

  function reportDuration(cid: string, seconds: number) {
    if (durations.get(cid) === seconds) return;
    durations = new Map(durations).set(cid, seconds);
  }

  // ── Feed-level reshare (moved from the retired VinePlayer) ──────────

  function requestReshare(vine: VineVideo) {
    if (resharingId) return;
    reshareError = '';
    reshareTarget = vine;
  }

  async function confirmReshare() {
    const vine = reshareTarget;
    reshareTarget = null;
    if (!vine || !onReshare) return;
    resharingId = vine.id;
    try {
      await onReshare(vine);
    } catch (err) {
      // Tauri IPC rejections are strings in production — preserve them
      // (codebase-wide extraction idiom), falling back only when empty.
      const msg = err instanceof Error ? err.message : String(err);
      reshareError = msg || 'Reshare failed';
    } finally {
      resharingId = null;
    }
  }

  // ── ZEB-670: feed-level delete (creator tombstone) ───────────────────

  function requestDelete(vine: VineVideo) {
    if (deletingId) return;
    deleteError = '';
    deleteTarget = vine;
  }

  async function confirmDelete() {
    const vine = deleteTarget;
    deleteTarget = null;
    if (!vine || !onDelete) return;
    deletingId = vine.id;
    try {
      await onDelete(vine);
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      deleteError = msg || 'Delete failed';
    } finally {
      deletingId = null;
    }
  }

  /** Own vines only: local rows are remapped to 'self'; a live row may
   *  still carry our hex address before remap (same duality canReshare's
   *  isOwnOriginalVine handles). */
  function isOwnVine(vine: VineVideo): boolean {
    return vine.creatorAddress === 'self'
      || (ownAddress != null && vine.creatorAddress === ownAddress);
  }
</script>

<div class="vine-feed">
  <header class="feed-header">
    <h2 class="feed-title">Vines</h2>
    {#if unviewedCount > 0}
      <span class="unviewed-count" aria-label="{unviewedCount} unviewed">{unviewedCount} new</span>
    {/if}
    <div class="header-spacer"></div>
    {#if onPublish}
      <button type="button" class="create-btn" onclick={onPublish}>
        <span class="create-btn-icon" aria-hidden="true">+</span>
        New vine
      </button>
    {/if}
  </header>

  <div class="tab-bar">
    <button type="button" class="tab" class:active={activeTab === 'following'} onclick={() => { unviewedPin = null; onTabChange?.('following'); }}>Following</button>
    <button type="button" class="tab" class:active={activeTab === 'discover'} onclick={() => { unviewedPin = null; onTabChange?.('discover'); }}>Discover</button>
  </div>

  {#if activeTab === 'following'}
    <div class="filter-bar">
      <button type="button" class="filter-tab" class:active={feedFilter === 'all'} onclick={() => { feedFilter = 'all'; unviewedPin = null; }}>All</button>
      <button type="button" class="filter-tab" class:active={feedFilter === 'unviewed'} onclick={() => { feedFilter = 'unviewed'; unviewedPin = null; }}>
        Unviewed{#if unviewedCount > 0}&nbsp;({unviewedCount}){/if}
      </button>
    </div>
  {:else}
    <div class="filter-bar">
      <span class="discover-hint">Your follow graph, 2°–3° — no algorithm.</span>
      <button type="button" class="tune-btn" data-testid="tune-btn" onclick={openTune}>⚙ Tune</button>
    </div>
  {/if}

  {#if reshareError}
    <p class="reshare-error" role="alert">{reshareError}</p>
  {/if}

  {#if deleteError}
    <p class="reshare-error" role="alert">{deleteError}</p>
  {/if}

  {#if filteredVines.length === 0}
    <div class="empty-state">
      <p class="empty-state-text">
        {#if activeTab === 'following'}
          {#if feedFilter === 'unviewed'}
            All caught up — no unviewed vines.
          {:else}
            Follow creators to build your feed. Check out the Discover tab to find people to follow.
          {/if}
        {:else}
          Nothing in your extended follow graph yet. Discover surfaces
          vines from people your follows follow (2nd and 3rd degree) —
          no algorithm, just your social graph.
        {/if}
      </p>
      {#if onPublish && !allCaughtUp}
        <button type="button" class="empty-state-cta" onclick={onPublish}>
          <span class="create-btn-icon" aria-hidden="true">+</span>
          Share a vine
        </button>
      {/if}
    </div>
  {:else}
    <div
      class="feed-list"
      role="list"
      aria-label="Vine feed"
      bind:this={feedListEl}
      onscroll={onFeedScroll}
    >
      {#each filteredVines as vine (vine.id)}
        {@const reaction = getReaction?.(vine.id)}
        <div class="feed-item" role="listitem" data-vine-id={vine.id}>
          <VineCard
            {vine}
            isPlaying={playingId === vine.id}
            isViewed={viewedIds.has(vine.id)}
            videoUrl={windowIds.has(vine.id) ? (videoUrls.get(vine.videoCid) ?? null) : null}
            duration={durations.get(vine.videoCid) ?? null}
            onActivate={activateCard}
            onDuration={reportDuration}
            showFollowButton={vine.creatorAddress !== 'self'}
            isFollowed={followedAddresses.has(vine.creatorAddress)}
            {onFollow}
            {onUnfollow}
            reactionCount={reaction?.count ?? 0}
            likedByMe={reaction?.likedByMe ?? false}
            {onToggleLike}
            reshareCount={reshareCountMap.get(vine.id) ?? 0}
            canReshare={!!onReshare && !isOwnOriginalVine(vine, ownAddress)}
            resharing={resharingId === vine.id}
            onReshare={requestReshare}
            canDelete={!!onDelete && isOwnVine(vine)}
            deleting={deletingId === vine.id}
            onDelete={requestDelete}
            {onViewOriginal}
            degreeLabel={activeTab === 'discover' ? degreeLabelOf(vine) : null}
            viaLabel={activeTab === 'discover' ? viaLabelOf(vine) : null}
          />
        </div>
      {/each}
    </div>
  {/if}
</div>

<svelte:window onkeydown={(e) => { if (tuneOpen && e.key === 'Escape') tuneOpen = false; }} />

{#if tuneOpen}
  <div
    class="tune-overlay"
    role="presentation"
    onclick={(e) => { if (e.target === e.currentTarget) tuneOpen = false; }}
  >
    <div
      class="tune-sheet"
      role="dialog"
      aria-modal="true"
      aria-label="Tune your Discover"
      tabindex="-1"
      bind:this={tuneSheetEl}
    >
      <h3 class="tune-title">Tune your Discover</h3>
      <p class="tune-copy">Degree follows — no algorithm, just your social graph.</p>
      <label class="tune-row">
        <input type="checkbox" bind:checked={deg2} onchange={saveTunePrefs} />
        <span>2nd degree — someone your follow follows</span>
      </label>
      <label class="tune-row">
        <input type="checkbox" bind:checked={deg3} onchange={saveTunePrefs} />
        <span>3rd degree — a follow-of-a-follow-of-a-follow</span>
      </label>
      {#if muteRoots.length > 0}
        <p class="tune-subhead">Mute a follow's influence</p>
        {#each muteRoots as root (root)}
          <label class="tune-row">
            <input type="checkbox" checked={!muted.has(root)} onchange={() => toggleMute(root)} />
            <span>via {nameOf(root)}</span>
          </label>
        {/each}
      {/if}
      {#if getShareFollows && onSetShareFollows}
        <div class="tune-divider" role="separator"></div>
        <label class="tune-row">
          <input
            type="checkbox"
            checked={shareFollows}
            disabled={shareFollowsBusy || !shareFollowsLoaded}
            onchange={handleShareFollowsToggle}
            data-testid="share-follows-toggle"
          />
          <span>Share my follows — powers your friends' Discover the same way</span>
        </label>
        {#if shareFollowsError}
          <p class="tune-error" role="alert">{shareFollowsError}</p>
        {/if}
      {/if}
      {#if getShareVinesPublicly && onSetShareVinesPublicly}
        <label class="tune-row">
          <input
            type="checkbox"
            checked={shareVinesPublicly}
            disabled={shareVinesPubliclyBusy || !shareVinesPubliclyLoaded}
            onchange={handleShareVinesPubliclyToggle}
            data-testid="share-vines-publicly-toggle"
          />
          <span>Share my vines publicly</span>
        </label>
        <p class="tune-help">Publishes a relay record so followers outside your communities can fetch your vines.</p>
        {#if shareVinesPubliclyError}
          <p class="tune-error" role="alert">{shareVinesPubliclyError}</p>
        {/if}
      {/if}
      <button type="button" class="tune-done" onclick={() => { tuneOpen = false; }}>
        Done · {discoverGraphVines.length} {discoverGraphVines.length === 1 ? 'vine' : 'vines'} in Discover
      </button>
    </div>
  </div>
{/if}

{#if reshareTarget}
  <ReshareConfirmDialog
    vine={reshareTarget}
    onConfirm={confirmReshare}
    onCancel={() => { reshareTarget = null; }}
  />
{/if}

{#if deleteTarget}
  <ConfirmDialog
    title="Delete vine?"
    message={`This deletes "${deleteTarget.title ?? 'this vine'}" from your feed and asks peers to drop their copies. Peers that are offline may keep it until they reconnect.`}
    confirmLabel="Delete"
    destructive={true}
    onConfirm={confirmDelete}
    onCancel={() => { deleteTarget = null; }}
  />
{/if}

<style>
  .vine-feed {
    display: flex;
    flex-direction: column;
    height: 100%;
  }

  .feed-header {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 16px 16px 8px;
  }

  .feed-title {
    color: var(--text-primary);
    font-size: 1rem;
    font-weight: 600;
    margin: 0;
  }

  .unviewed-count {
    color: var(--gov-clay-deep);
    font-size: 0.75rem;
    font-weight: 600;
    font-family: var(--font-mono);
    background: var(--gov-clay-soft);
    padding: 2px 10px;
    border-radius: 999px;
  }

  .header-spacer {
    flex: 1;
  }

  .create-btn {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    background: var(--accent);
    color: var(--on-accent);
    border: none;
    padding: 6px 12px;
    border-radius: 5px;
    font-size: 0.8rem;
    font-weight: 600;
    cursor: pointer;
    transition: opacity 0.15s;
  }

  .create-btn:hover {
    opacity: 0.85;
  }

  .create-btn-icon {
    font-size: 1rem;
    line-height: 1;
  }

  .tab-bar {
    display: flex;
    gap: 0;
    padding: 0 16px 4px;
    border-bottom: 1px solid var(--bg-tertiary);
  }

  .tab {
    background: none;
    border: none;
    border-bottom: 2px solid transparent;
    color: var(--text-muted);
    font-size: 0.85rem;
    font-weight: 500;
    padding: 8px 16px;
    cursor: pointer;
    transition: color 0.15s, border-color 0.15s;
  }

  .tab:hover {
    color: var(--text-primary);
  }

  .tab.active {
    color: var(--text-primary);
    font-weight: 600;
    border-bottom-color: var(--accent);
  }

  .filter-bar {
    display: flex;
    gap: 4px;
    padding: 0 16px 8px;
  }

  .filter-tab {
    background: none;
    border: none;
    color: var(--text-muted);
    font-size: 0.75rem;
    font-weight: 500;
    padding: 4px 10px;
    border-radius: 999px;
    cursor: pointer;
    transition: background 0.15s, color 0.15s;
  }

  .filter-tab:hover {
    background: var(--bg-tertiary);
  }

  .filter-tab.active {
    background: var(--bg-tertiary);
    color: var(--text-primary);
    font-weight: 600;
  }

  .reshare-error {
    color: var(--danger);
    font-size: 0.78rem;
    margin: 0;
    padding: 4px 16px;
  }

  .empty-state {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 14px;
    padding: 32px 16px;
  }

  .empty-state-text {
    color: var(--text-muted);
    font-size: 0.85rem;
    text-align: center;
    margin: 0;
    max-width: 36ch;
  }

  .empty-state-cta {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    background: var(--accent);
    color: var(--on-accent);
    border: none;
    padding: 8px 16px;
    border-radius: 5px;
    font-size: 0.85rem;
    font-weight: 600;
    cursor: pointer;
    transition: opacity 0.15s;
  }

  .empty-state-cta:hover {
    opacity: 0.85;
  }

  .feed-list {
    flex: 1;
    overflow-y: auto;
    scroll-snap-type: y mandatory;
    scroll-behavior: smooth;
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 0 16px 16px;
  }

  @media (prefers-reduced-motion: reduce) {
    .feed-list {
      scroll-behavior: auto;
    }
  }

  .feed-item {
    flex: 0 0 auto;
    height: 100%;
    scroll-snap-align: start;
    scroll-snap-stop: always;
  }

  /* ── ZEB-671: Discover Tune ─────────────────────────────────────── */

  .discover-hint {
    color: var(--text-secondary);
    font-size: 0.72rem;
    align-self: center;
  }

  .tune-btn {
    margin-left: auto;
    display: inline-flex;
    align-items: center;
    gap: 5px;
    background: transparent;
    color: var(--text-primary);
    border: 1px solid var(--border);
    padding: 4px 12px;
    border-radius: 999px;
    font-size: 0.75rem;
    font-weight: 600;
    cursor: pointer;
  }

  .tune-btn:hover {
    border-color: var(--accent);
  }

  .tune-overlay {
    position: fixed;
    inset: 0;
    background: var(--overlay);
    z-index: 30;
    display: flex;
    align-items: flex-end;
    justify-content: center;
  }

  .tune-sheet {
    background: var(--bg-primary);
    border-radius: 12px 12px 0 0;
    padding: 20px 20px 16px;
    width: min(420px, 100%);
    max-height: 70vh;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .tune-title {
    margin: 0;
    font-size: 0.95rem;
    color: var(--text-primary);
  }

  .tune-copy {
    margin: 0;
    font-size: 0.75rem;
    color: var(--text-secondary);
  }

  .tune-subhead {
    margin: 6px 0 0;
    font-size: 0.72rem;
    font-weight: 600;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--text-secondary);
  }

  .tune-row {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 0.8rem;
    color: var(--text-primary);
    cursor: pointer;
  }

  .tune-divider {
    border-top: 1px solid var(--border);
    margin: 6px 0 0;
  }

  .tune-help {
    margin: 0;
    font-size: 0.7rem;
    color: var(--text-secondary);
  }

  .tune-error {
    margin: 0;
    font-size: 0.75rem;
    color: var(--gov-clay-deep);
  }

  .tune-done {
    margin-top: 8px;
    padding: 11px;
    border: none;
    border-radius: 8px;
    background: var(--accent);
    color: var(--on-accent);
    font-size: 0.82rem;
    font-weight: 600;
    cursor: pointer;
  }

  .tune-done:hover {
    opacity: 0.85;
  }
</style>
