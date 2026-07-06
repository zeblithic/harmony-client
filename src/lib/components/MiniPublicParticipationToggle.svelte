<script lang="ts">
  /**
   * ZEB-311 — Mini-public decline affordance.
   *
   * Shown only when myRole === 'mini_public' AND stage is
   * Deliberation or Drafting (the parent panel gates rendering).
   * One-shot decline; once declined, the button is hidden and a
   * confirmation message takes its place.
   *
   * Per ZEB-287 R4: every $props field destructured below.
   * Per Tauri error-extraction memory.
   */
  import type { Tier3PollExport } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';

  let {
    detail,
    adapter,
    myAddr,
    onDecline,
  }: {
    detail: Tier3PollExport;
    adapter: VotingAdapter;
    myAddr: string;
    onDecline: () => void;
  } = $props();

  let pending = $state(false);
  let error = $state<string | null>(null);

  let alreadyDeclined = $derived(
    detail.declined.some(([owner]) => owner === myAddr),
  );

  async function clickDecline() {
    if (pending) return;
    pending = true;
    error = null;
    try {
      await adapter.declineSortition(detail.pollId, undefined);
      onDecline();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      pending = false;
    }
  }
</script>

{#if alreadyDeclined}
  <p class="already-declined">You declined this role; a backup member is filling the slot.</p>
{:else}
  <div class="decline-affordance">
    <p>You're a member of the mini-public. Active participation in deliberation + drafting is expected.</p>
    <button type="button" onclick={clickDecline} disabled={pending}>
      {pending ? 'Declining…' : 'Decline mini-public role'}
    </button>
    {#if error}<p class="error">{error}</p>{/if}
  </div>
{/if}

<style>
  .decline-affordance { margin: 0.75rem 0; }
  .decline-affordance button {
    background: transparent;
    color: var(--vote-against);
    border: 1px solid var(--danger-border-muted);
    padding: 0.35rem 0.8rem;
    border-radius: 4px;
    cursor: pointer;
  }
  .already-declined { color: var(--text-faint); font-style: italic; }
  .error { color: var(--danger); }
</style>
