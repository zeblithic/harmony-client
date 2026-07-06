<script lang="ts">
  /**
   * ZEB-606: slim mono connection-status strip for the nav footer —
   * "● connected · N peers". The design placed this in the window chrome,
   * but the app uses native decorations (no titlebar exists), so it lives
   * at the bottom of the nav column (spec §0.2/§5).
   *
   * Self-contained: owns its network-health subscription (snapshot +
   * network-health-changed), mirroring NetworkHealthView's race-safe
   * destroyed-flag teardown. Renders nothing until a snapshot with a
   * resolved network arrives (no "offline" flash during boot).
   */
  import { onDestroy, onMount } from 'svelte';
  import { onNetworkHealthChanged, snapshot } from '../network-health-adapter';
  import type { NetworkHealthSnapshot } from '../types/network-health';
  import type { UnlistenFn } from '@tauri-apps/api/event';

  let snap = $state<NetworkHealthSnapshot | null>(null);
  let destroyed = false;
  let unlisten: UnlistenFn | null = null;

  async function refresh() {
    try {
      const s = await snapshot();
      if (!destroyed) snap = s;
    } catch {
      // IPC unavailable (boot window) — keep whatever we had; the next
      // network-health-changed event retries.
    }
  }

  onMount(async () => {
    await refresh();
    try {
      const resolved = await onNetworkHealthChanged(() => {
        void refresh();
      });
      if (destroyed) {
        resolved();
      } else {
        unlisten = resolved;
      }
    } catch (e) {
      console.warn(
        '[zeb-606] status chip subscribe failed:',
        e instanceof Error ? e.message : String(e),
      );
    }
  });

  onDestroy(() => {
    destroyed = true;
    if (unlisten) unlisten();
  });

  let chip = $derived.by((): { kind: 'ok' | 'warn' | 'danger'; text: string; title?: string } | null => {
    if (!snap) return null;
    if (snap.transportDisabledReason) {
      return { kind: 'danger', text: '● offline', title: snap.transportDisabledReason };
    }
    if (!snap.myNetwork) return null; // still initializing — no flash
    const n = snap.peers.filter((p) => p.connectionMode !== 'noConnection').length;
    const peers = `${n} ${n === 1 ? 'peer' : 'peers'}`;
    if (snap.myNetwork.reachability === 'unreachable') {
      return { kind: 'danger', text: '● offline' };
    }
    if (snap.myNetwork.reachability === 'degraded') {
      return { kind: 'warn', text: `● degraded · ${peers}` };
    }
    return { kind: 'ok', text: `● connected · ${peers}` };
  });
</script>

{#if chip}
  <div class="status-chip status-{chip.kind}" title={chip.title}>{chip.text}</div>
{/if}

<style>
  .status-chip {
    font-family: var(--font-mono);
    font-size: 10px;
    line-height: 1;
    padding: 4px 6px;
    border-radius: 4px;
    user-select: none;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .status-ok {
    color: var(--net-ok-fg);
    background: var(--net-ok-bg);
  }
  .status-warn {
    color: var(--net-warn-fg);
    background: var(--net-warn-bg);
  }
  .status-danger {
    color: var(--net-danger-fg);
    background: var(--net-danger-bg);
  }
</style>
