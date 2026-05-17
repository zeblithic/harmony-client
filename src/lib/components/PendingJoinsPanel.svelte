<script lang="ts">
    import { invoke } from '@tauri-apps/api/core';
    import { listen, type UnlistenFn } from '@tauri-apps/api/event';
    export let communityId: string;
    export let canModerate: boolean;

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

    let pending: PendingJoinDto[] = [];
    let recent: CounterSignDto[] = [];
    let convergedUnlisten: UnlistenFn | null = null;
    let errorMessage = '';
    // R3 (M4): start non-loading so the !canModerate branch doesn't flash
    // a transient "Loading…" state. refresh() flips this to true at the
    // start of each fetch (M4 fix).
    let loading = false;
    // R4-7: monotonically-increasing token captured at the start of each
    // refresh(). Out-of-order async responses (e.g., a stale invoke()
    // still in-flight when communityId / canModerate changes — or when
    // a converged event triggers a refresh while a manual kickJoiner-
    // triggered refresh is mid-flight) must NOT overwrite the results
    // of the latest refresh. Each refresh checks `myCallId === latestCallId`
    // before committing pending/recent/errorMessage/loading.
    let latestCallId = 0;

    async function refresh() {
        // R3 (M4): reset loading state at the start of every refresh so
        // subsequent fetches (triggered by community-state-sync-converged
        // events or kickJoiner-then-refresh) also surface a transient
        // loading indicator. Without this, after the first load completes
        // loading stays false forever and the UI shows stale data with no
        // visual cue that a refresh is in flight.
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

    function formatHlc(hlc: { wallMs: number }): string {
        return new Date(hlc.wallMs).toLocaleString();
    }

    // R3 (M3): switch from onMount to a reactive statement so the panel
    // re-fetches when `canModerate` or `communityId` change. onMount only
    // fires once at component instantiation — if the parent toggles
    // canModerate (e.g., power-level change observed via state-root sync)
    // or swaps the displayed community without unmounting, the old
    // onMount-fetched data would stay stuck. Svelte 4 reactive statement
    // shape; the project hasn't moved to runes yet for this component.
    let lastWatchedCanModerate: boolean | undefined = undefined;
    let lastWatchedCommunityId: string | undefined = undefined;
    // R5-3: monotonic token for watchDeps() invocations, separate from
    // latestCallId. Each watchDeps() call captures `myWatchId =
    // ++latestWatchId` at entry and re-checks it after every `await`
    // boundary. If a newer watchDeps() invocation has started while we
    // were awaiting (because canModerate or communityId changed mid-
    // flight), the stale invocation bails out without registering a
    // listener for the OLD community / overwriting convergedUnlisten.
    // Without this, an `await refresh()` can resume after a newer
    // deps-change has occurred, and the listener registered next would
    // refresh the wrong panel on later converge events.
    let latestWatchId = 0;
    $: void watchDeps(canModerate, communityId);

    async function watchDeps(canMod: boolean, cid: string) {
        if (canMod === lastWatchedCanModerate && cid === lastWatchedCommunityId) {
            return;
        }
        const myWatchId = ++latestWatchId;
        lastWatchedCanModerate = canMod;
        lastWatchedCommunityId = cid;
        // Tear down any prior listener before re-registering.
        if (convergedUnlisten) {
            try {
                convergedUnlisten();
            } catch {
                /* ignore */
            }
            convergedUnlisten = null;
        }
        if (!canMod) {
            // Non-moderator path: clear any prior data and stop.
            // R4-7: bump latestCallId so any in-flight refresh (kicked
            // off before canMod flipped to false) discards its result
            // when it resolves rather than re-populating pending/recent
            // for a community the caller is no longer authorized to
            // moderate.
            ++latestCallId;
            pending = [];
            recent = [];
            errorMessage = '';
            loading = false;
            return;
        }
        await refresh();
        // R5-3: post-refresh stale-check. If a newer watchDeps()
        // invocation has started while refresh() was awaiting (e.g.,
        // user switched community or canModerate flipped), abandon
        // listener registration for the OLD community. The newer
        // invocation owns convergedUnlisten now.
        if (
            myWatchId !== latestWatchId ||
            cid !== communityId ||
            canMod !== canModerate ||
            !canModerate
        ) {
            return;
        }
        try {
            const unlisten = await listen('community-state-sync-converged', async (evt) => {
                const payload = evt.payload as { communityId?: string };
                if (payload?.communityId === cid) {
                    await refresh();
                }
            });
            // R5-3: post-listen stale-check. Even after a successful
            // refresh, the `await listen(...)` itself can race a deps
            // change. If we are no longer the latest watch, tear down
            // the listener we just registered (without overwriting
            // convergedUnlisten — that belongs to the newer invocation).
            if (
                myWatchId !== latestWatchId ||
                cid !== communityId ||
                canMod !== canModerate ||
                !canModerate
            ) {
                try {
                    unlisten();
                } catch {
                    /* ignore */
                }
                return;
            }
            convergedUnlisten = unlisten;
        } catch (e) {
            // Event listener registration may fail in some test environments —
            // that's OK; manual refresh still works.
        }
    }

    import { onDestroy } from 'svelte';
    onDestroy(() => {
        convergedUnlisten?.();
    });
</script>

{#if canModerate}
    <section class="pending-joins-panel">
        {#if errorMessage}
            <p class="error">{errorMessage}</p>
        {/if}

        <details open={pending.length > 0}>
            <summary>Awaiting counter-sign ({pending.length})</summary>
            {#if loading}
                <p class="muted">Loading…</p>
            {:else if pending.length === 0}
                <p class="muted">No pending join requests.</p>
            {:else}
                <ul>
                    {#each pending as p (p.eventId)}
                        <li>
                            <span class="joiner">{p.inviteeHint ?? p.joinerAddr.slice(0, 12)}</span>
                            <span class="time">since {formatHlc(p.pendingAtHlc)}</span>
                            <button on:click={() => kickJoiner(p.joinerAddr)}>Reject (kick)</button>
                        </li>
                    {/each}
                </ul>
            {/if}
        </details>

        <details>
            <summary>Recent joins ({recent.length})</summary>
            {#if loading}
                <p class="muted">Loading…</p>
            {:else if recent.length === 0}
                <p class="muted">No recent counter-signs.</p>
            {:else}
                <ul>
                    {#each recent as r (r.joinEventId)}
                        <li>
                            <span class="joiner">{r.joinerAddr.slice(0, 12)}</span>
                            <span class="time">at {formatHlc(r.countersignedAtHlc)}</span>
                        </li>
                    {/each}
                </ul>
            {/if}
        </details>
    </section>
{/if}

<style>
    .pending-joins-panel {
        margin: 1em 0;
    }
    .pending-joins-panel ul {
        list-style: none;
        padding: 0;
    }
    .pending-joins-panel li {
        padding: 0.4em 0;
        display: flex;
        gap: 0.6em;
        align-items: center;
    }
    .joiner {
        font-weight: 600;
    }
    .time {
        color: #999;
        font-size: 0.9em;
    }
    .muted {
        color: #999;
    }
    .error {
        color: #c33;
    }
    summary {
        cursor: pointer;
        padding: 0.4em 0;
    }
</style>
