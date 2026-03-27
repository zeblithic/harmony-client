<script lang="ts">
  import type { ConnectionStatus } from '../zenoh-service';

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

  const statusColors: Record<ConnectionStatus, string> = {
    disconnected: '#72767d',
    connecting: '#faa61a',
    connected: '#43b581',
    error: '#ed4245',
  };

  let statusLabel = $derived.by(() => {
    switch (connectionStatus) {
      case 'disconnected': return 'Disconnected';
      case 'connecting': return 'Connecting...';
      case 'connected': return `Connected, ${discoveredCount} node${discoveredCount !== 1 ? 's' : ''} discovered`;
      case 'error': return `Error: ${errorMessage ?? 'unknown'}`;
    }
  });
</script>

<div class="connection-bar" role="toolbar" aria-label="Zenoh connection">
  <input
    class="endpoint-input"
    type="text"
    bind:value={endpoint}
    placeholder="tcp/host:port"
    disabled={connectionStatus === 'connected' || connectionStatus === 'connecting'}
    onkeydown={handleKeydown}
    aria-label="Zenoh endpoint"
  />

  {#if connectionStatus === 'connected' || connectionStatus === 'connecting'}
    <button
      class="connect-btn disconnect"
      onclick={handleDisconnect}
      disabled={connectionStatus === 'connecting'}
    >
      Disconnect
    </button>
  {:else}
    <button class="connect-btn" onclick={handleConnect}>
      Connect
    </button>
  {/if}

  <div class="status-indicator" role="status" aria-label={statusLabel}>
    <span class="status-dot" style="background: {statusColors[connectionStatus]}"></span>
    <span class="status-text">{statusLabel}</span>
  </div>
</div>

<style>
  .connection-bar {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 12px;
    background: var(--bg-secondary, #2f3136);
    border-bottom: 1px solid var(--bg-tertiary, #40444b);
    font-size: 12px;
  }

  .endpoint-input {
    width: 200px;
    padding: 4px 8px;
    border: 1px solid var(--bg-tertiary, #40444b);
    border-radius: 4px;
    background: var(--bg-primary, #1e1f22);
    color: var(--text-primary, #dcddde);
    font-family: monospace;
    font-size: 11px;
  }

  .endpoint-input:disabled {
    opacity: 0.5;
  }

  .endpoint-input:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: -1px;
  }

  .connect-btn {
    padding: 4px 12px;
    border: none;
    border-radius: 4px;
    background: var(--accent, #5865f2);
    color: white;
    font-size: 11px;
    font-weight: 600;
    cursor: pointer;
    white-space: nowrap;
  }

  .connect-btn:hover { opacity: 0.9; }

  .connect-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 2px;
  }

  .connect-btn.disconnect {
    background: var(--bg-tertiary, #40444b);
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
    color: var(--text-secondary, #b9bbbe);
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
