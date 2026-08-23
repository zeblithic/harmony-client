/**
 * Integration test for the VineFeed subsystem.
 *
 * Tests end-to-end: render vine cards sorted by date, filter tabs,
 * center-detection autoplay (the feed IS the player since ZEB-612 S2),
 * mark viewed, on-card reshare via the confirm dialog, create button,
 * and empty states.
 *
 * Uses real VineVideo data and real component tree (VineFeed → VineCard
 * → ReshareConfirmDialog), no service stubs.
 */
import { render, screen, fireEvent, cleanup, within } from '@testing-library/svelte';
import { describe, it, expect, vi, afterEach } from 'vitest';
import VineFeed from '../VineFeed.svelte';
import type { VineVideo } from '../../types';
import { resolveOriginalCreator } from '../../vine-utils';

afterEach(cleanup);

// ── Shared fixtures ────────────────────────────────────────────────

const vineBase = Date.now() / 1000 - 3600;

const VINES: VineVideo[] = [
  {
    id: 'v1', creatorAddress: 'a1', creatorName: 'Alice',
    createdAt: vineBase, videoCid: 'cid-v1',
    title: 'Transport demo', viewed: false,
  },
  {
    id: 'v2', creatorAddress: 'b2', creatorName: 'Bob',
    createdAt: vineBase + 120, videoCid: 'cid-v2',
    title: 'Mesh routing', viewed: false,
  },
  {
    id: 'v3', creatorAddress: 'c3', creatorName: 'Carol',
    createdAt: vineBase + 300, videoCid: 'cid-v3',
    viewed: false,
  },
  {
    // Alice's reshare of v2 (Bob's vine). On the real wire, the client
    // doing the first reshare populates originalCreator* — without these
    // fields a downstream test that reshares v4 would see Alice credited
    // as the true origin instead of Bob, a silent false-positive.
    id: 'v4', creatorAddress: 'a1', creatorName: 'Alice',
    createdAt: vineBase + 600, videoCid: 'cid-v4',
    title: 'Cache explained', reshareOf: 'v2',
    originalCreatorAddress: 'b2', originalCreatorName: 'Bob',
    viewed: false,
  },
];

// ── Helper ──────────────────────────────────────────────────────────

function renderFeed(overrides: Record<string, unknown> = {}) {
  const callbacks = {
    onMarkViewed: vi.fn(),
    onPublish: vi.fn(),
    onReshare: vi.fn(),
  };

  const result = render(VineFeed, {
    props: {
      followedVines: VINES,
      discoverVines: [],
      viewedIds: new Set<string>(),
      activeTab: 'following' as const,
      followedAddresses: new Set<string>(),
      ...callbacks,
      ...overrides,
    },
  });

  return { ...result, callbacks };
}

/** The rendered feed row for a vine id (scopes queries to one card). */
function rowFor(container: HTMLElement, vineId: string): HTMLElement {
  const row = container.querySelector<HTMLElement>(`[data-vine-id="${vineId}"]`);
  if (!row) throw new Error(`no feed row for ${vineId}`);
  return row;
}

describe('VineFeed Integration', () => {
  // ── 1. Rendering ──────────────────────────────────────────────────

  it('renders vine cards sorted newest first', () => {
    renderFeed();
    const items = screen.getAllByRole('listitem');
    // v4 (newest) should appear before v1 (oldest)
    expect(items.length).toBe(4);
    expect(items[0].querySelector('[role="button"]')?.getAttribute('aria-label')).toContain('Cache explained');
    expect(items[items.length - 1].querySelector('[role="button"]')?.getAttribute('aria-label')).toContain('Transport demo');
  });

  it('shows creator names on cards', () => {
    renderFeed();
    expect(screen.getAllByText('Alice').length).toBeGreaterThan(0);
    expect(screen.getAllByText('Bob').length).toBeGreaterThan(0);
    expect(screen.getAllByText('Carol').length).toBeGreaterThan(0);
  });

  it('shows vine titles', () => {
    renderFeed();
    expect(screen.getByText('Transport demo')).toBeTruthy();
    expect(screen.getByText('Mesh routing')).toBeTruthy();
    expect(screen.getByText('Cache explained')).toBeTruthy();
  });

  it('shows reshare attribution with the resharer and the view-original verb', () => {
    renderFeed();
    // v4: Alice reshared Bob's vine — "↻ Alice reshared · view original by Bob"
    // (plain text variant: no onViewOriginal wired in this render). ZEB-978:
    // the origin name renders inside <PeerName>, so assert on the row's full
    // textContent rather than a single text node.
    const row = screen.getByText(/Alice reshared ·/);
    expect(row.textContent).toContain('view original by Bob');
  });

  // ── 2. Unviewed count ─────────────────────────────────────────────

  it('shows unviewed count when there are unviewed vines', () => {
    renderFeed();
    expect(screen.getByText('4 new')).toBeTruthy();
  });

  it('hides unviewed count when all are viewed', () => {
    renderFeed({ viewedIds: new Set(['v1', 'v2', 'v3', 'v4']) });
    expect(screen.queryByText(/new$/)).toBeNull();
  });

  // ── 3. Filter tabs ────────────────────────────────────────────────

  it('defaults to All filter', () => {
    renderFeed();
    const allBtn = screen.getByText('All');
    expect(allBtn.classList.contains('active')).toBe(true);
  });

  it('filters to unviewed vines when Unviewed tab is clicked', async () => {
    renderFeed({ viewedIds: new Set(['v1', 'v2']) });

    const unviewedTab = screen.getByText(/^Unviewed/);
    await fireEvent.click(unviewedTab);

    // Only v3 and v4 should remain (unviewed)
    expect(screen.queryByText('Transport demo')).toBeNull();
    expect(screen.queryByText('Mesh routing')).toBeNull();
    expect(screen.getByText('Cache explained')).toBeTruthy();
    // v3 has no title — verify it's still present via its aria-label
    expect(screen.getByLabelText('Untitled vine by Carol')).toBeTruthy();
  });

  it('shows empty state when all vines are viewed in Unviewed filter', async () => {
    // Filter switches clear the playing-card pin, so the strict unviewed
    // view (and its all-caught-up state) stays reachable even though the
    // newest card auto-played (and was auto-marked viewed) on mount.
    renderFeed({ viewedIds: new Set(['v1', 'v2', 'v3', 'v4']) });

    const unviewedTab = screen.getByText(/^Unviewed/);
    await fireEvent.click(unviewedTab);

    expect(screen.getByText(/All caught up/)).toBeTruthy();
  });

  it('shows empty state when no vines exist', () => {
    renderFeed({ followedVines: [] });
    expect(screen.getByText(/Follow creators/)).toBeTruthy();
  });

  // ── 4. Autoplay (the feed is the player, ZEB-612 S2) ──────────────

  it('marks the newest vine viewed on mount (autoplay-on-view)', async () => {
    const { callbacks, container } = renderFeed();
    await vi.waitFor(() => {
      expect(callbacks.onMarkViewed).toHaveBeenCalledWith('v4');
    });
    // Exactly one playing card, and it's the newest row.
    expect(container.querySelectorAll('.vine-card.playing').length).toBe(1);
    expect(rowFor(container, 'v4').querySelector('.vine-card.playing')).toBeTruthy();
  });

  it('clicking a card moves playback to it and marks it viewed', async () => {
    const { callbacks, container } = renderFeed();
    const card = screen.getByLabelText('Transport demo by Alice');
    await fireEvent.click(card);

    expect(callbacks.onMarkViewed).toHaveBeenCalledWith('v1');
    expect(rowFor(container, 'v1').querySelector('.vine-card.playing')).toBeTruthy();
    expect(container.querySelectorAll('.vine-card.playing').length).toBe(1);
  });

  // ── 5. Reshare (on-card verb + confirm dialog) ────────────────────

  it('shows the reshare verb on every card (no own vines in this feed)', () => {
    renderFeed();
    expect(screen.getAllByLabelText('Reshare vine').length).toBe(4);
  });

  it('calls onReshare when reshare is confirmed via dialog', async () => {
    const { callbacks, container } = renderFeed();
    callbacks.onReshare.mockResolvedValue(undefined);

    const reshareBtn = within(rowFor(container, 'v1')).getByLabelText('Reshare vine');
    await fireEvent.click(reshareBtn);

    // onReshare must not fire until the dialog is confirmed.
    expect(callbacks.onReshare).not.toHaveBeenCalled();

    // The dialog's confirm button has the bare label "Reshare"
    // (the cards' buttons use aria-label "Reshare vine"), so
    // /^reshare$/i disambiguates to the dialog's confirm button.
    const confirmBtn = screen.getByRole('button', { name: /^reshare$/i });
    await fireEvent.click(confirmBtn);

    expect(callbacks.onReshare).toHaveBeenCalledWith(
      expect.objectContaining({ id: 'v1' })
    );
  });

  // ── 6. Create button ──────────────────────────────────────────────

  it('shows create button and fires onPublish', async () => {
    const { callbacks } = renderFeed();
    const createBtn = screen.getByRole('button', { name: 'New vine' });
    await fireEvent.click(createBtn);

    expect(callbacks.onPublish).toHaveBeenCalled();
  });

  it('hides create button when onPublish is not provided', () => {
    renderFeed({ onPublish: undefined });
    expect(screen.queryByRole('button', { name: 'New vine' })).toBeNull();
  });

  // ── 7. Accessibility ──────────────────────────────────────────────

  it('renders vine feed as a list with listitem roles', () => {
    renderFeed();
    expect(screen.getByRole('list', { name: 'Vine feed' })).toBeTruthy();
    expect(screen.getAllByRole('listitem').length).toBe(4);
  });

  it('vine cards are keyboard accessible', async () => {
    const { callbacks } = renderFeed();
    const card = screen.getByLabelText('Transport demo by Alice');
    await fireEvent.keyDown(card, { key: 'Enter' });

    expect(callbacks.onMarkViewed).toHaveBeenCalledWith('v1');
  });

  // ── 8. App.svelte wiring smoke (handleVineReshare attribution) ────
  //
  // Verifies the end-to-end shape that App.svelte's `handleVineReshare`
  // produces when a user clicks Reshare → confirms in the dialog. The
  // test wires VineFeed's `onReshare` to a publish-spy that mirrors
  // App.svelte exactly (call `resolveOriginalCreator` and forward to
  // `vineService.publish`). A full App.svelte mount is too heavy
  // (Layout, all services, Tauri adapter, …) for this single assertion;
  // the helper extraction keeps the asserted logic pure and the
  // VineFeed→VineCard→ReshareConfirmDialog wiring real.
  //
  // Two cases:
  // - Resharing a non-reshare → publish payload credits the source
  //   vine's own creator as the origin.
  // - Resharing a reshare → publish payload credits the *true origin*
  //   (the source vine's `originalCreator*` fields), NOT the
  //   intermediate resharer's `creatorAddress`.

  describe('App-level reshare wiring', () => {
    function setupPublishSpy(ownAddress: string | null = null) {
      const publish = vi.fn().mockResolvedValue(undefined);
      // Mirror of App.svelte's handleVineReshare (including the
      // caller-level self-reshare guard — see FIX 1 in PR #120 round 1).
      const onReshare = async (vine: VineVideo) => {
        const isOwn = vine.creatorAddress === 'self'
          || (ownAddress != null && vine.creatorAddress === ownAddress);
        if (isOwn && !vine.reshareOf) return;
        const { originalCreatorAddress, originalCreatorName } =
          resolveOriginalCreator(vine);
        await publish(
          vine.videoCid,
          vine.title,
          vine.id,
          originalCreatorAddress,
          originalCreatorName,
        );
      };
      return { publish, onReshare };
    }

    it('reshares an original vine: publish payload credits the source creator', async () => {
      const { publish, onReshare } = setupPublishSpy();
      const { container } = render(VineFeed, {
        props: {
          followedVines: VINES,
          discoverVines: [],
          viewedIds: new Set<string>(),
          activeTab: 'following' as const,
          followedAddresses: new Set<string>(),
          onMarkViewed: vi.fn(),
          onReshare,
        },
      });

      // Reshare Bob's non-reshare original (v2) straight from its card.
      const row = container.querySelector<HTMLElement>('[data-vine-id="v2"]');
      if (!row) throw new Error('no feed row for v2');
      const reshareBtn = within(row).getByLabelText('Reshare vine');
      await fireEvent.click(reshareBtn);

      const confirmBtn = screen.getByRole('button', { name: /^reshare$/i });
      await fireEvent.click(confirmBtn);

      // The handler awaits publish — `vi.waitFor` polls until the
      // assertion passes, robust against any macrotasks in the chain
      // (FIX 7 in PR #120 round 1). Two `await Promise.resolve()`
      // ticks only flushed microtasks and could race a real publish.
      await vi.waitFor(() => {
        expect(publish).toHaveBeenCalledWith(
          'cid-v2',
          'Mesh routing',
          'v2',
          'b2',   // Bob's address (source creator → origin)
          'Bob',  // Bob's name
        );
      });
    });

    // ── Self-reshare prevention (caller-level guard) ──────────────
    //
    // Pin the guard's two cases directly on the App-mirror helper
    // (see `setupPublishSpy`). The UI hides the Reshare button on own
    // originals, but the guard must still fire to defend against
    // programmatic callers and the late-ownAddress race the UI used
    // to expose (see FIX 2 below).

    it('guard: own ORIGINAL ("self", no reshareOf) → publish NOT called', async () => {
      const { publish, onReshare } = setupPublishSpy();
      const ownOriginal: VineVideo = {
        id: 'v-mine-orig',
        creatorAddress: 'self',
        creatorName: 'You',
        createdAt: vineBase + 1000,
        videoCid: 'cid-mine',
        title: 'My original',
        viewed: true,
      };
      await onReshare(ownOriginal);
      expect(publish).not.toHaveBeenCalled();
    });

    it('guard: own ORIGINAL by hex ownAddress (no reshareOf) → publish NOT called', async () => {
      const ownAddress = 'a1b2c3d4';
      const { publish, onReshare } = setupPublishSpy(ownAddress);
      const ownOriginal: VineVideo = {
        id: 'v-mine-hex',
        creatorAddress: ownAddress,
        creatorName: 'You',
        createdAt: vineBase + 1000,
        videoCid: 'cid-mine-hex',
        title: 'My hex-addressed original',
        viewed: true,
      };
      await onReshare(ownOriginal);
      expect(publish).not.toHaveBeenCalled();
    });

    it('guard (UI layer): own original renders no Reshare verb on its card', () => {
      const ownOriginal: VineVideo = {
        id: 'v-mine-ui',
        creatorAddress: 'self',
        creatorName: 'You',
        createdAt: vineBase + 1000,
        videoCid: 'cid-mine-ui',
        title: 'My original',
        viewed: true,
      };
      render(VineFeed, {
        props: {
          followedVines: [ownOriginal],
          discoverVines: [],
          viewedIds: new Set<string>(['v-mine-ui']),
          activeTab: 'following' as const,
          followedAddresses: new Set<string>(),
          onMarkViewed: vi.fn(),
          onReshare: vi.fn(),
        },
      });
      expect(screen.queryByLabelText('Reshare vine')).toBeNull();
    });

    it("guard: reshare of someone-else's reshare-of-mine → publish IS called with self attribution", async () => {
      // Alice's vine → Bob reshares → I reshare Bob's reshare.
      // The source vine here is Bob's reshare; its `creatorAddress`
      // is Bob (not me), so the guard MUST allow this through.
      // The resolved `originalCreator*` correctly points at me
      // (the true origin), which is spec-allowed.
      const { publish, onReshare } = setupPublishSpy();
      const bobsReshareOfMine: VineVideo = {
        id: 'v-bob-of-me',
        creatorAddress: 'addr-bob',
        creatorName: 'Bob',
        createdAt: vineBase + 2000,
        videoCid: 'cid-of-mine',
        title: 'My original (Bob reshare)',
        reshareOf: 'v-mine-orig',
        originalCreatorAddress: 'self',
        originalCreatorName: 'You',
        viewed: false,
      };
      await onReshare(bobsReshareOfMine);
      expect(publish).toHaveBeenCalledWith(
        'cid-of-mine',
        'My original (Bob reshare)',
        'v-bob-of-me',
        'self',
        'You',
      );
    });

    it('reshares a reshare: publish payload propagates the true origin (transitive)', async () => {
      const { publish, onReshare } = setupPublishSpy();
      // Carol reshares Bob's reshare of Alice's vine. The transitive
      // rule: the originalCreator* fields on Carol's input vine point
      // at Alice (the true origin), not Bob. resolveOriginalCreator
      // must propagate Alice through to publish — re-crediting Bob
      // would lose the origin chain on the third hop.
      const reshareOfReshare: VineVideo = {
        id: 'v-bob-reshare',
        creatorAddress: 'addr-bob',
        creatorName: 'Bob',
        createdAt: vineBase + 900,
        videoCid: 'cid-alice-orig',
        title: 'Alice original',
        reshareOf: 'v-alice-orig',
        originalCreatorAddress: 'addr-alice',
        originalCreatorName: 'Alice',
        viewed: false,
      };

      render(VineFeed, {
        props: {
          followedVines: [reshareOfReshare],
          discoverVines: [],
          viewedIds: new Set<string>(),
          activeTab: 'following' as const,
          followedAddresses: new Set<string>(),
          onMarkViewed: vi.fn(),
          onReshare,
        },
      });

      // The only card in the feed carries the verb — click it directly.
      const reshareBtn = screen.getByLabelText('Reshare vine');
      await fireEvent.click(reshareBtn);

      const confirmBtn = screen.getByRole('button', { name: /^reshare$/i });
      await fireEvent.click(confirmBtn);

      // See FIX 7 note on the sibling case above.
      await vi.waitFor(() => {
        expect(publish).toHaveBeenCalledWith(
          'cid-alice-orig',
          'Alice original',
          'v-bob-reshare',
          'addr-alice',  // ← origin, NOT Bob
          'Alice',       // ← origin, NOT Bob
        );
      });
    });
  });
});
