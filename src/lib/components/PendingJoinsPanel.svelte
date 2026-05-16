<script lang="ts">
    import { onMount, onDestroy } from 'svelte';
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
    let loading = true;

    async function refresh() {
        loading = true;
        try {
            pending = await invoke<PendingJoinDto[]>('list_pending_joins', { communityId });
            recent = await invoke<CounterSignDto[]>('list_recent_counter_signs', {
                communityId,
                limit: 20,
            });
            errorMessage = '';
        } catch (e) {
            errorMessage = e instanceof Error ? e.message : String(e);
        } finally {
            loading = false;
        }
    }

    async function kickJoiner(joinerAddr: string) {
        try {
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

    onMount(async () => {
        if (!canModerate) {
            loading = false;
            return;
        }
        await refresh();
        try {
            convergedUnlisten = await listen('community-state-sync-converged', async (evt) => {
                const payload = evt.payload as { communityId?: string };
                if (payload?.communityId === communityId) {
                    await refresh();
                }
            });
        } catch (e) {
            // Event listener registration may fail in some test environments —
            // that's OK; manual refresh still works.
        }
    });

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
