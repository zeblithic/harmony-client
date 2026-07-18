<script lang="ts">
  /**
   * ZEB-435 (scope #4): the left-communities management surface. Once a
   * community is left it is hidden from the nav (ZEB-427 a′), so this panel is
   * the only way to reach it again locally. It lists left-but-not-deleted
   * communities and offers the deliberately-distinct, irreversible Tier-3
   * "Delete forever" gesture (typed confirmation → `remove_space`): permanent
   * tombstone + local data deletion, and the tombstone blocks any re-invite
   * from resurrecting the community. Leaving stays reversible and is NEVER
   * cascaded into deletion — only this explicit gesture tombstones.
   */
  import { onMount } from 'svelte';
  import type { LeftCommunityNavDto } from '../community-service';
  import TypedConfirmationModal from './TypedConfirmationModal.svelte';

  /** Narrow structural seam (the real `CommunityService` satisfies it) so the
   *  panel is testable without a Tauri adapter. */
  interface LeftCommunitiesService {
    listLeftCommunities(): Promise<LeftCommunityNavDto[]>;
    removeSpace(spaceId: string): Promise<void>;
  }

  let { service }: { service: LeftCommunitiesService } = $props();

  let rows: LeftCommunityNavDto[] = $state([]);
  let loaded = $state(false);
  let loadError: string | null = $state(null);
  let target: LeftCommunityNavDto | null = $state(null);
  let removeError: string | null = $state(null);
  let removing = $state(false);

  async function load(): Promise<void> {
    try {
      rows = await service.listLeftCommunities();
      loadError = null;
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    } finally {
      loaded = true;
    }
  }

  // The panel remounts each time Settings opens (SettingsPanel keeps tabs
  // mounted only while Settings itself is open), so pull-on-mount is fresh
  // per open; each successful delete re-fetches. No push events exist for
  // left spaces — they were already gone from the nav stream.
  onMount(() => {
    void load();
  });

  async function confirmRemove(): Promise<void> {
    if (!target || removing) return;
    removing = true;
    const t = target;
    try {
      await service.removeSpace(t.spaceId);
      removeError = null;
      target = null;
      await load();
    } catch (e) {
      // Surface the backend guard verbatim (e.g. leave-first / still-active
      // refusals) — standard IPC error extraction.
      removeError = e instanceof Error ? e.message : String(e);
      target = null;
    } finally {
      removing = false;
    }
  }

  function fmtLeftAt(ms: number): string {
    return new Date(ms).toLocaleDateString(undefined, {
      year: 'numeric',
      month: 'short',
      day: 'numeric',
    });
  }
</script>

<div class="left-communities">
  <h4>Left communities</h4>
  <p class="hint">
    Communities you have left keep their local data so you can rejoin via a new
    invite. Deleting one forever removes its local data and permanently blocks
    rejoining it.
  </p>

  {#if loadError}
    <p class="error" role="alert">Could not load left communities: {loadError}</p>
  {/if}
  {#if removeError}
    <p class="error" role="alert">Delete failed: {removeError}</p>
  {/if}

  {#if loaded && rows.length === 0 && !loadError}
    <p class="empty">No left communities.</p>
  {:else if rows.length > 0}
    <ul class="rows">
      {#each rows as row (row.spaceId)}
        <li class="row">
          <div class="meta">
            <span class="name">{row.name}</span>
            <span class="left-at">Left {fmtLeftAt(row.leftAtMs)}</span>
          </div>
          <button
            class="delete-btn"
            disabled={removing}
            onclick={() => {
              removeError = null;
              target = row;
            }}
          >
            Delete forever…
          </button>
        </li>
      {/each}
    </ul>
  {/if}
</div>

{#if target}
  <TypedConfirmationModal
    title={`Delete "${target.name}" forever`}
    description={
      'This permanently deletes the community’s local data and blocks it ' +
      'from ever being re-added on this identity — a new invite to the same ' +
      'community will be rejected. This cannot be undone.'
    }
    requiredText={target.name || 'delete'}
    confirmLabel="Delete forever"
    onConfirm={() => void confirmRemove()}
    onCancel={() => {
      target = null;
    }}
  />
{/if}

<style>
  .left-communities {
    padding: 16px;
    border-top: 1px solid var(--border);
  }
  h4 {
    margin: 0 0 8px;
    font-size: 0.875rem;
    font-weight: 600;
    color: var(--text-primary);
    font-family: var(--font-display);
  }
  .hint {
    margin: 0 0 12px;
    font-size: 0.8125rem;
    line-height: 1.5;
    color: var(--text-secondary);
  }
  .error {
    margin: 0 0 12px;
    font-size: 0.8125rem;
    color: var(--danger);
  }
  .empty {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--text-muted);
  }
  .rows {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 8px 12px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 7px;
  }
  .meta {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .name {
    font-size: 0.875rem;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .left-at {
    font-size: 0.75rem;
    color: var(--text-muted);
  }
  .delete-btn {
    flex-shrink: 0;
    background: var(--danger-muted);
    color: var(--text-primary);
    border: none;
    padding: 6px 12px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.8125rem;
  }
  .delete-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .delete-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
</style>
