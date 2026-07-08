<script lang="ts">
  import type { ConnectionStatus } from '../zenoh-service';
  import { tokenColor } from '../theme-colors';
  import { appliedTheme } from '../theme-service';

  let {
    connectionStatus,
    discoveredCount,
    defaultEndpoint = 'tcp/127.0.0.1:7447',
    onConnect,
    onDisconnect,
    errorMessage,
  }: {
    connectionStatus: ConnectionStatus;
    discoveredCount: number;
    defaultEndpoint?: string;
    onConnect: (endpoint: string) => void;
    onDisconnect: () => void;
    errorMessage?: string;
  } = $props();

  function loadEndpoint(): string {
    try {
      const saved = localStorage.getItem('zenoh-endpoint');
      if (saved) return saved;
    } catch {
      // localStorage unavailable (SSR, test environment)
    }
    return defaultEndpoint;
  }

  let endpoint = $state(loadEndpoint());

  function handleConnect() {
    try {
      localStorage.setItem('zenoh-endpoint', endpoint);
    } catch {
      // localStorage unavailable
    }
    onConnect(endpoint);
  }

  function handleDisconnect() {
    onDisconnect();
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter') {
      e.preventDefault();
      if (connectionStatus === 'disconnected' || connectionStatus === 'error') {
        handleConnect();
      }
    }
  }

  const STATUS_TOKENS: Record<ConnectionStatus, string> = {
    disconnected: '--text-muted',
    connecting: '--warning',
    connected: '--presence-online',
    error: '--danger',
    reconnecting: '--warning',
  };

  // A derived (not a call-time function) so a theme switch repaints the dot:
  // void $appliedTheme re-runs it on a flip — tokenColor() isn't reactive on
  // its own (ZEB-645).
  let statusColorValue = $derived.by(() => {
    void $appliedTheme;
    return tokenColor(STATUS_TOKENS[connectionStatus]);
  });

  let statusLabel = $derived.by(() => {
    switch (connectionStatus) {
      case 'disconnected': return 'Disconnected';
      case 'connecting': return 'Connecting...';
      case 'connected': return `Connected, ${discoveredCount} node${discoveredCount !== 1 ? 's' : ''} discovered`;
      case 'error': return `Error: ${errorMessage ?? 'unknown'}`;
      case 'reconnecting': return errorMessage ?? 'Reconnecting...';
    }
  });
</script>

<div class="connection-bar" role="toolbar" aria-label="Zenoh connection">
  <input
    class="endpoint-input"
    type="text"
    bind:value={endpoint}
    placeholder="tcp/host:port"
    disabled={connectionStatus === 'connected' || connectionStatus === 'connecting' || connectionStatus === 'reconnecting'}
    onkeydown={handleKeydown}
    aria-label="Zenoh endpoint"
  />

  {#if connectionStatus === 'connected' || connectionStatus === 'connecting' || connectionStatus === 'reconnecting'}
    <button
      class="connect-btn disconnect"
      onclick={handleDisconnect}
    >
      {connectionStatus === 'connecting' || connectionStatus === 'reconnecting' ? 'Cancel' : 'Disconnect'}
    </button>
  {:else}
    <button class="connect-btn" onclick={handleConnect}>
      Connect
    </button>
  {/if}

  <div class="status-indicator" role="status" aria-label={statusLabel}>
    <span class="status-dot" style="background: {statusColorValue}"></span>
    <span class="status-text">{statusLabel}</span>
  </div>
</div>

<style>
  .connection-bar {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 12px;
    background: var(--bg-secondary);
    border-bottom: 1px solid var(--bg-tertiary);
    font-size: 12px;
  }

  .endpoint-input {
    width: 200px;
    padding: 4px 8px;
    border: 1px solid var(--bg-tertiary);
    border-radius: 4px;
    background: var(--bg-primary);
    color: var(--text-primary);
    font-family: var(--font-mono);
    font-size: 11px;
  }

  .endpoint-input:disabled {
    opacity: 0.5;
  }

  .endpoint-input:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -1px;
  }

  .connect-btn {
    padding: 4px 12px;
    border: none;
    border-radius: 4px;
    background: var(--accent);
    color: var(--on-accent);
    font-size: 11px;
    font-weight: 600;
    cursor: pointer;
    white-space: nowrap;
  }

  .connect-btn:hover { opacity: 0.9; }

  .connect-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .connect-btn.disconnect {
    background: var(--bg-tertiary);
  }

  .connect-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .status-indicator {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-left: auto;
    color: var(--text-secondary);
  }

  .status-dot {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
  }

  .status-text {
    font-size: 11px;
    white-space: nowrap;
  }
</style>
