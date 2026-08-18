<script lang="ts">
  import type { FileGrant } from '../types';
  import Avatar from './Avatar.svelte';
  import { nonEmpty } from '../display-label';
  import { shortAddr } from '../short-addr';

  let {
    grants,
    availableFriends,
    isEncrypted,
    onGrant,
    onRevoke,
  }: {
    /** The owner's grant list for the current file. `null` until
     *  `listGrants` resolves — the list self-hides rather than rendering a
     *  fabricated "Not shared with anyone" as a pre-query placeholder;
     *  `[]` is the real, proven-empty state (ZEB-674 C5 honesty; mirrors
     *  the `ContributionMeter`/`summary` null-until-resolved idiom). */
    grants: FileGrant[] | null;
    /** Friends eligible to be granted access — the picker excludes anyone
     *  already in `grants`. */
    availableFriends: { address: string; displayName: string | null }[];
    /** Public files carry no viewer ACL (an unencrypted CID has no key to
     *  gate on) — the whole surface self-hides when false, even if a
     *  caller mistakenly mounts it (defense-in-depth honesty gate). */
    isEncrypted: boolean;
    onGrant: (address: string) => Promise<void>;
    onRevoke: (address: string) => Promise<void>;
  } = $props();

  // A grantee's card displayName has no non-blank publish constraint, so it can
  // be "" / "   ". Guard every name with nonEmpty() and fall back to the shared
  // shortAddr (ZEB-607) — never a blank label, and never the full untruncated
  // address.
  let grantedAddresses = $derived(new Set((grants ?? []).map((g) => g.granteeAddress)));
  let pickableFriends = $derived(
    availableFriends.filter((f) => !grantedAddresses.has(f.address)),
  );

  /**
   * ZEB-782. With no friends, the picker was omitted and the section
   * collapsed to a bare "Not shared with anyone" — no control, no
   * explanation, no way to learn that sharing is friend-gated. A user who
   * followed the v0.2.0 release notes (which said "community members")
   * landed on a feature that appeared present and did nothing.
   *
   * Three states that used to render as one, now told apart. The
   * distinction that matters is *why* there is nothing to pick: having no
   * friends is a dead end with an action attached, whereas having already
   * granted everyone is a finished job. Same empty picker, opposite
   * meanings.
   */
  let pickerHint = $derived(
    availableFriends.length === 0
      ? 'You can share encrypted files with your friends. Add a friend to get started — sharing runs over the friend connection, so community membership on its own is not enough.'
      : pickableFriends.length === 0
        ? 'Everyone you can share with already has access to this file.'
        : null,
  );

  // Backend rejections carry a stable `ineligible:` machine prefix — strip it
  // for display exactly like the backup toggle does (FileDetailPanel.svelte).
  function displayError(err: unknown): string {
    const msg = err instanceof Error ? err.message : String(err);
    return msg.startsWith('ineligible:')
      ? `Not eligible: ${msg.slice('ineligible:'.length).trim()}`
      : msg;
  }

  let grantPending = $state(false);
  let grantError = $state<string | null>(null);
  /** Address currently being revoked, or null. A per-address flag (not a
   *  single boolean) so revoking one row doesn't disable the others. */
  let revokePending = $state<string | null>(null);
  let revokeError = $state<string | null>(null);

  async function handlePickerChange(e: Event) {
    const select = e.currentTarget as HTMLSelectElement;
    const address = select.value;
    select.value = '';
    if (!address || grantPending) return;
    grantPending = true;
    grantError = null;
    try {
      await onGrant(address);
    } catch (err) {
      grantError = displayError(err);
    } finally {
      grantPending = false;
    }
  }

  async function handleRevoke(address: string) {
    if (revokePending) return;
    revokePending = address;
    revokeError = null;
    try {
      await onRevoke(address);
    } catch (err) {
      revokeError = displayError(err);
    } finally {
      revokePending = null;
    }
  }
</script>

{#if isEncrypted && grants !== null}
  <section class="share-list" aria-label="Shared with (can view)">
    <div class="share-list-header">
      <span class="header-icon" aria-hidden="true">&#x1F513;</span>
      <h3>Shared with <span class="header-detail">(can view)</span></h3>
    </div>

    {#if grants.length === 0}
      <p class="empty-state">Not shared with anyone</p>
    {:else}
      <ul class="share-peer-list">
        {#each grants as grant (grant.granteeAddress)}
          <li class="peer-row">
            <Avatar
              address={grant.granteeAddress}
              displayName={nonEmpty(grant.displayName) ?? shortAddr(grant.granteeAddress)}
              size={28}
            />
            <span class="peer-name">{nonEmpty(grant.displayName) ?? shortAddr(grant.granteeAddress)}</span>
            <span class="peer-role">can view</span>
            <span class="peer-icon" aria-hidden="true">&#x1F513;</span>
            <button
              type="button"
              class="remove-btn"
              aria-label="Revoke {nonEmpty(grant.displayName) ?? shortAddr(grant.granteeAddress)}"
              disabled={revokePending === grant.granteeAddress}
              onclick={() => handleRevoke(grant.granteeAddress)}
            >
              {revokePending === grant.granteeAddress ? 'Revoking…' : 'Revoke'}
            </button>
          </li>
        {/each}
      </ul>
    {/if}

    {#if revokeError}
      <p class="share-error" role="alert">{revokeError}</p>
    {/if}

    {#if pickerHint}
      <p class="picker-hint" data-testid="share-picker-hint">{pickerHint}</p>
    {/if}

    {#if pickableFriends.length > 0}
      <div class="picker-row">
        <label for="share-picker" class="visually-hidden">Share with...</label>
        <select
          id="share-picker"
          aria-label="Share with..."
          class="peer-picker"
          disabled={grantPending}
          onchange={handlePickerChange}
        >
          <option value="" disabled selected>Share with...</option>
          {#each pickableFriends as friend (friend.address)}
            <option value={friend.address}>{nonEmpty(friend.displayName) ?? shortAddr(friend.address)}</option>
          {/each}
        </select>
      </div>
    {/if}

    {#if grantError}
      <p class="share-error" role="alert">{grantError}</p>
    {/if}
  </section>
{/if}

<style>
  .share-list {
    padding: 4px 0;
  }

  .share-list-header {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-bottom: 8px;
  }

  .share-list-header h3 {
    margin: 0;
    font-size: 0.8rem;
    font-weight: 600;
    color: var(--text-primary);
  }

  .header-icon {
    font-size: 0.9rem;
  }

  .header-detail {
    color: var(--text-secondary);
    font-weight: 400;
  }

  .empty-state {
    font-size: 0.8rem;
    color: var(--text-muted);
    margin: 4px 0;
    font-style: italic;
  }

  /* ZEB-782: explains why the picker is absent. Not italic — this is
     guidance to act on, not a placeholder for missing content, and the
     italic empty-state right above it already reads as "nothing here". */
  .picker-hint {
    font-size: 0.78rem;
    line-height: 1.4;
    color: var(--text-muted);
    margin: 6px 0 0;
  }

  .share-peer-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .peer-row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 8px;
    border-radius: 4px;
    background: var(--share-bg);
  }

  .peer-row:hover {
    background: var(--share-bg-hover);
  }

  .peer-name {
    font-size: 0.85rem;
    color: var(--text-primary);
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .peer-role {
    font-size: 0.75rem;
    color: var(--text-secondary);
    flex-shrink: 0;
  }

  .peer-icon {
    font-size: 0.75rem;
    flex-shrink: 0;
  }

  .remove-btn {
    padding: 2px 8px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: none;
    color: var(--text-secondary);
    font-size: 0.7rem;
    cursor: pointer;
    flex-shrink: 0;
  }

  .remove-btn:hover {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }

  .remove-btn:disabled {
    opacity: 0.6;
    cursor: default;
  }

  .remove-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .picker-row {
    margin-top: 8px;
  }

  .peer-picker {
    width: 100%;
    padding: 4px 8px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-tertiary);
    color: var(--text-primary);
    font-size: 0.8rem;
    cursor: pointer;
  }

  .peer-picker:disabled {
    opacity: 0.6;
    cursor: default;
  }

  .peer-picker:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .share-error {
    margin: 4px 0 0;
    font-size: 0.75rem;
    color: var(--danger);
  }

  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    margin: -1px;
    padding: 0;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    white-space: nowrap;
    border: 0;
  }
</style>
