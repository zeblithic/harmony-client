<script lang="ts">
    import { invoke } from '@tauri-apps/api/core';
    // ZEB-1016: live refresh on membership sync activity via the
    // `community-membership-updated` Tauri event (emitted by the community
    // sync engines once per applied local insert / incoming publish batch).
    import { listen, type UnlistenFn } from '@tauri-apps/api/event';
    import CountChip from './governance/CountChip.svelte';
    import PeerName from './PeerName.svelte';
    // ZEB-946: the "since …" HLC timestamp honors the owner's time-format prefs.
    import { formatFullTimestamp, type TimeFormatPrefs } from '../time-format';
    import { timeFormatPrefs } from '../time-format-service';
    // ZEB-961: resolve the joiner owner_id through the shared display-name ladder
    // (nickname → card → inviteeHint → short hex). The parent
    // (CommunitySettingsPanel) threads its own resolveCard/resolveNickname down.
    import { resolveMentionLabel } from '../mention-render';
    import type { ResolvedCard } from '../member-card-service';

    let { communityId, canModerate, resolveCard, resolveNickname }: {
        communityId: string;
        canModerate: boolean;
        resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
        resolveNickname?: (ownerIdHex: string) => string | undefined;
    } = $props();

    interface PendingJoinDto {
        eventId: string;
        joinerAddr: string;
        pendingAtHlc: { wallMs: number; logical: number; deviceId: string };
        inviteeHint?: string;
    }

    interface CounterSignDto {
        joinEventId: string;
        joinerAddr: string;
        countersignedAtHlc: { wallMs: number; logical: number; deviceId: string };
    }

    let pending: PendingJoinDto[] = $state([]);
    let recent: CounterSignDto[] = $state([]);
    let errorMessage = $state('');
    // R3 (M4): start non-loading so the !canModerate branch doesn't flash
    // a transient "Loading…" state. refresh() flips this to true at the
    // start of each fetch (M4 fix).
    let loading = $state(false);

    // Non-reactive stale-guard token (plain `let`, NOT `$state`): a monotonic
    // counter used only inside async guards, never rendered.
    // R4-7: latestCallId is captured at the start of each refresh(); out-of-order
    // async responses must not overwrite the results of the latest refresh.
    let latestCallId = 0;

    async function refresh() {
        // R3 (M4): reset loading state at the start of every refresh so
        // subsequent fetches (kickJoiner-then-refresh) also surface a
        // transient loading indicator. Without this, after the first load
        // completes loading stays false forever and the UI shows stale data
        // with no visual cue that a refresh is in flight.
        const myCallId = ++latestCallId;
        loading = true;
        try {
            const nextPending = await invoke<PendingJoinDto[]>('list_pending_joins', {
                communityId,
            });
            const nextRecent = await invoke<CounterSignDto[]>('list_recent_counter_signs', {
                communityId,
                limit: 20,
            });
            // R4-7: discard the result if a newer refresh has started
            // while we were awaiting. The newer call will publish its
            // own pending/recent — overwriting with our stale values
            // would surface incorrect data (e.g., entries for a
            // previously-selected community after a switch).
            if (myCallId !== latestCallId) {
                return;
            }
            pending = nextPending;
            recent = nextRecent;
            errorMessage = '';
        } catch (e) {
            // R4-7: also guard the error path. A late rejection from a
            // stale call shouldn't blank out errors / valid results
            // from the latest call.
            if (myCallId !== latestCallId) {
                return;
            }
            errorMessage = e instanceof Error ? e.message : String(e);
        } finally {
            // Only clear `loading` if THIS call is still the latest. A
            // newer in-flight call sets loading=true at its start; we
            // must not clobber that here.
            if (myCallId === latestCallId) {
                loading = false;
            }
        }
    }

    async function kickJoiner(joinerAddr: string) {
        try {
            // ZEB-250: result is AdminActionResult{Completed|Pending}. A pending
            // joiner's power is always 0 (not admin-tier), so this will always
            // be Completed, but we type it correctly for forwards-compatibility.
            await invoke('kick_from_community', {
                communityId,
                targetAddr: joinerAddr,
                reason: 'Manually rejected pending join',
            });
            await refresh();
        } catch (e) {
            errorMessage = e instanceof Error ? e.message : String(e);
        }
    }

    function formatHlc(hlc: { wallMs: number }, prefs: TimeFormatPrefs): string {
        return formatFullTimestamp(hlc.wallMs, prefs);
    }

    // R3 (M3): re-fetch when `canModerate` or `communityId` change. Svelte-5
    // $effect re-runs only when its tracked deps change and cleans up the prior
    // run first, so it natively replaces the Svelte-4 reactive-statement +
    // manual `lastWatched*` dedup. The stale-guard token above preserves the
    // original R4-7 race protection.
    //
    // ZEB-1016: the effect also registers a `community-membership-updated`
    // listener so a moderator sitting on this panel sees newly synced
    // PendingJoin / JoinCountersign events without re-opening it. (ZEB-976
    // removed a listener for `community-state-sync-converged` here because
    // nothing ever emitted it; this one is backed by real emit sites in
    // community_state_sync.rs.) The effect cleanup handles both the
    // community-switch re-run and unmount; the `cancelled` flag covers the
    // window where the async `listen` registration resolves after cleanup.
    $effect(() => {
        // Read deps synchronously so the effect re-runs when they change.
        const canMod = canModerate;
        const cid = communityId;

        if (!canMod) {
            // Non-moderator path: clear any prior data and skip the fetch.
            // R4-7: bump latestCallId so any in-flight refresh (kicked off before
            // canMod flipped to false) discards its result when it resolves rather
            // than re-populating pending/recent for a community the caller is no
            // longer authorized to moderate.
            ++latestCallId;
            pending = [];
            recent = [];
            errorMessage = '';
            loading = false;
            return;
        }

        void refresh();

        let cancelled = false;
        let unlisten: UnlistenFn | undefined;
        void listen<{ communityId: string }>('community-membership-updated', (event) => {
            // Events for other communities are not ours to refetch.
            if (event.payload.communityId === cid) {
                void refresh();
            }
        })
            .then((fn) => {
                if (cancelled) {
                    fn();
                } else {
                    unlisten = fn;
                }
            })
            .catch((e) => {
                // Non-Tauri harnesses (vitest/jsdom) have no event bridge;
                // a failed subscription degrades to mount/prop/action-only
                // refresh (the IrohRelaySettings precedent).
                console.warn(
                    'PendingJoinsPanel: failed to subscribe to membership updates',
                    e
                );
            });
        return () => {
            cancelled = true;
            unlisten?.();
        };
    });
</script>

{#if canModerate}
    <section class="pending-joins-panel">
        {#if errorMessage}
            <p class="error">{errorMessage}</p>
        {/if}

        <details class="join-section" open={pending.length > 0}>
            <summary class="section-summary">
                <CountChip label="Awaiting counter-sign" value={String(pending.length)} tone="neutral" />
            </summary>
            {#if loading}
                <p class="muted">Loading…</p>
            {:else if pending.length === 0}
                <p class="muted">No pending join requests.</p>
            {:else}
                <ul>
                    {#each pending as p (p.eventId)}
                        <li class="join-row">
                            <span class="joiner"><PeerName name={resolveMentionLabel(p.joinerAddr, resolveNickname, resolveCard, () => p.inviteeHint)} ownerIdHex={p.joinerAddr} /></span>
                            <span class="time">since {formatHlc(p.pendingAtHlc, $timeFormatPrefs)}</span>
                            <button class="reject-btn" type="button" onclick={() => kickJoiner(p.joinerAddr)}>
                                Reject (kick)
                            </button>
                        </li>
                    {/each}
                </ul>
            {/if}
        </details>

        <details class="join-section">
            <summary class="section-summary">
                <CountChip label="Recent joins" value={String(recent.length)} tone="neutral" />
            </summary>
            {#if loading}
                <p class="muted">Loading…</p>
            {:else if recent.length === 0}
                <p class="muted">No recent counter-signs.</p>
            {:else}
                <ul>
                    {#each recent as r (r.joinEventId)}
                        <li class="join-row">
                            <span class="joiner"><PeerName name={resolveMentionLabel(r.joinerAddr, resolveNickname, resolveCard)} ownerIdHex={r.joinerAddr} /></span>
                            <span class="time">at {formatHlc(r.countersignedAtHlc, $timeFormatPrefs)}</span>
                        </li>
                    {/each}
                </ul>
            {/if}
        </details>
    </section>
{/if}

<style>
    /* ZEB-653: flat panel (the parent CommunitySettingsPanel frames it in a
       .section with a "Join requests" label; the reference PendingAdminProposalsPanel
       keeps its panel flat too) — cards live on the rows, not nested on the panel. */
    .pending-joins-panel {
        margin: 16px 0;
        display: flex;
        flex-direction: column;
        gap: 12px;
    }
    .section-summary {
        display: inline-flex;
        align-items: center;
        gap: 8px;
        cursor: pointer;
        padding: 4px 0;
    }
    .pending-joins-panel ul {
        list-style: none;
        padding: 0;
        margin: 8px 0 0;
        display: flex;
        flex-direction: column;
        gap: 8px;
    }
    /* Commons card anatomy per join row (surface-raised + border + 8px + e1 shadow). */
    .join-row {
        display: flex;
        gap: 12px;
        align-items: center;
        padding: 12px;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface-raised);
        box-shadow: var(--shadow-e1);
    }
    /* Truncated ID + HLC timestamp read as machine data → mono. */
    .joiner {
        font-weight: 600;
        font-family: var(--font-mono);
    }
    .time {
        color: var(--text-dim);
        font-size: 0.8rem;
        font-family: var(--font-mono);
    }
    .muted {
        color: var(--text-dim);
        padding: 4px 0;
    }
    .error {
        color: var(--danger-text-muted);
    }
    /* Destructive inline action — matches the LastAdminWarningDialog convention
       (--danger-muted fill + --text-bright), rubric 5px radius. */
    .reject-btn {
        margin-left: auto;
        padding: 4px 10px;
        border: 1px solid var(--danger-muted);
        border-radius: 5px;
        background: var(--danger-muted);
        color: var(--text-bright);
        font: inherit;
        font-weight: 600;
        cursor: pointer;
    }
    .reject-btn:hover {
        filter: brightness(1.05);
    }
    .reject-btn:focus-visible {
        outline: 2px solid var(--accent);
        outline-offset: 1px;
    }
</style>
