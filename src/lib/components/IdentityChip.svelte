<script lang="ts">
  /**
   * ZEB-606: nav-footer identity chip — initials avatar with a presence
   * ring, display name, and a mono "● self-sovereign" microline when the
   * owner identity is minted and loaded. Purely presentational; App
   * computes every signal (spec §6). The settings gear stays in the nav
   * header (ZEB-569) — this chip carries no actions.
   */
  let {
    displayName,
    ownerIdHex,
    selfOnline = false,
    selfSovereign = false,
  }: {
    displayName: string;
    ownerIdHex: string | null;
    /** Presence ring — App derives this from visibility + identity state. */
    selfOnline?: boolean;
    /** True when ownerIdentityState === 'present'. */
    selfSovereign?: boolean;
  } = $props();

  let initials = $derived.by(() => {
    const parts = displayName.trim().split(/\s+/).filter(Boolean);
    if (parts.length === 0) return (ownerIdHex ?? '??').slice(0, 2).toUpperCase();
    if (parts.length === 1) return parts[0].slice(0, 2).toUpperCase();
    return (parts[0][0] + parts[parts.length - 1][0]).toUpperCase();
  });

  let shownName = $derived(
    displayName.trim() !== ''
      ? displayName
      : ownerIdHex
        ? `${ownerIdHex.slice(0, 8)}…`
        : 'Anonymous',
  );
</script>

<div class="identity-chip" data-testid="identity-chip">
  <span class="chip-avatar" aria-hidden="true">
    {initials}
    {#if selfOnline}
      <!-- Decorative only: the aria-hidden avatar ancestor removes this from
           the a11y tree, so a role/aria-label here would be dead markup
           (ZEB-606 final review M1). -->
      <span class="presence-ring"></span>
    {/if}
  </span>
  <span class="chip-text">
    <span class="chip-name">{shownName}</span>
    {#if selfSovereign}
      <span class="chip-status">● self-sovereign</span>
    {/if}
  </span>
</div>

<style>
  .identity-chip {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 0;
    min-width: 0;
  }
  .chip-avatar {
    position: relative;
    flex-shrink: 0;
    width: 30px;
    height: 30px;
    border-radius: 8px;
    background: var(--accent);
    color: var(--text-bright);
    font-size: 12px;
    font-weight: 700;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .presence-ring {
    position: absolute;
    right: -2px;
    bottom: -2px;
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: var(--presence-online);
    border: 2px solid var(--bg-secondary);
  }
  .chip-text {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .chip-name {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .chip-status {
    font-family: var(--font-mono);
    font-size: 10px;
    color: var(--presence-online);
  }
</style>
