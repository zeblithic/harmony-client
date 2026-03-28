# Daemon Skeleton Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Zenoh connectivity to the Tauri backend — subscribe to capacity advertisements, push discovered nodes to the Svelte frontend via Tauri events, display connection status in the network viz.

**Architecture:** Zenoh session managed in Tauri state, async commands for connect/disconnect, subscriber task pushes events to frontend. Frontend `ZenohService` uses dependency injection (TauriAdapter interface) for testability. Connection bar in NetworkApp shows status + discovered node count.

**Tech Stack:** Rust (Tauri v2, zenoh v1, tokio), TypeScript (Svelte 5 runes, vitest)

**Spec:** `docs/specs/2026-03-27-daemon-skeleton-design.md`

---

## File Map

| File | Responsibility |
|------|---------------|
| `src-tauri/Cargo.toml` | Add zenoh, tokio, hex deps |
| `src-tauri/src/lib.rs` | ZenohState, connect/disconnect commands, parse_capacity, event emission |
| `src/lib/zenoh-service.ts` | TauriAdapter interface, ZenohService class, DiscoveredNode type |
| `src/lib/zenoh-service.test.ts` | Tests with mock TauriAdapter |
| `src/lib/components/ConnectionBar.svelte` | Endpoint input, connect button, status dot, node count |
| `src/lib/components/__tests__/ConnectionBar.test.ts` | Component tests |
| `src/NetworkApp.svelte` | Wire ConnectionBar + ZenohService |

---

### Task 1: Backend — Zenoh Dependencies + Capacity Parser

Add dependencies and the capacity payload parsing function with tests.

**Files:**
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Add dependencies to Cargo.toml**

Add to `[dependencies]` in `src-tauri/Cargo.toml`:

```toml
zenoh = { version = "1", default-features = false }
tokio = { version = "1", features = ["rt", "sync"] }
hex = "0.4"
```

- [ ] **Step 2: Add CapacityUpdate struct and parse_capacity function**

In `src-tauri/src/lib.rs`, add after the existing `use serde::Serialize;`:

```rust
use serde::Deserialize;

/// Parsed capacity advertisement from a harmony-node.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapacityUpdate {
    pub node_addr: String,
    pub model_cid: String,
    pub ready: bool,
}

/// Zenoh connection status pushed to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZenohStatus {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

const CAPACITY_PREFIX: &str = "harmony/compute/capacity/";

fn parse_capacity(key_expr: &str, payload: &[u8]) -> Option<CapacityUpdate> {
    let node_addr = key_expr.strip_prefix(CAPACITY_PREFIX)?;
    if payload.len() < 33 {
        return None;
    }
    let model_cid = hex::encode(&payload[..32]);
    let ready = payload[32] == 0x01;
    Some(CapacityUpdate {
        node_addr: node_addr.to_string(),
        model_cid,
        ready,
    })
}
```

- [ ] **Step 3: Add unit tests**

Add at the bottom of `src-tauri/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn make_payload(status: u8) -> Vec<u8> {
        let mut p = vec![0xAA; 32]; // model CID
        p.push(status);
        p
    }

    #[test]
    fn parse_capacity_valid_ready() {
        let result = parse_capacity(
            "harmony/compute/capacity/deadbeef01020304",
            &make_payload(0x01),
        );
        let update = result.unwrap();
        assert_eq!(update.node_addr, "deadbeef01020304");
        assert_eq!(update.model_cid, "aa".repeat(32));
        assert!(update.ready);
    }

    #[test]
    fn parse_capacity_valid_busy() {
        let result = parse_capacity(
            "harmony/compute/capacity/node42",
            &make_payload(0x00),
        );
        let update = result.unwrap();
        assert_eq!(update.node_addr, "node42");
        assert!(!update.ready);
    }

    #[test]
    fn parse_capacity_truncated() {
        let result = parse_capacity(
            "harmony/compute/capacity/node1",
            &[0xAA; 10],
        );
        assert!(result.is_none());
    }

    #[test]
    fn parse_capacity_wrong_prefix() {
        let result = parse_capacity(
            "harmony/telemetry/node1/health",
            &make_payload(0x01),
        );
        assert!(result.is_none());
    }

    #[test]
    fn parse_capacity_empty_payload() {
        let result = parse_capacity(
            "harmony/compute/capacity/node1",
            &[],
        );
        assert!(result.is_none());
    }
}
```

- [ ] **Step 4: Verify compilation and tests**

Run from `src-tauri/`:
```bash
cargo test
```

- [ ] **Step 5: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/src/lib.rs
git commit -m "feat(tauri): add zenoh deps and capacity payload parser"
```

---

### Task 2: Backend — Connect/Disconnect Commands

Add the async Tauri commands for Zenoh connection management.

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Add ZenohState and managed state setup**

Add to `src-tauri/src/lib.rs`:

```rust
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};
use tokio::task::JoinHandle;

struct ZenohState {
    session: Option<zenoh::Session>,
    task: Option<JoinHandle<()>>,
}

impl Default for ZenohState {
    fn default() -> Self {
        Self { session: None, task: None }
    }
}
```

- [ ] **Step 2: Add connect_zenoh command**

```rust
#[tauri::command]
async fn connect_zenoh(
    endpoint: String,
    app: AppHandle,
    state: tauri::State<'_, Mutex<ZenohState>>,
) -> Result<(), String> {
    // Disconnect if already connected
    disconnect_inner(&app, &state).await;

    // Build zenoh config — use insert_json5 for the connect endpoint
    // (zenoh v1 config is serde-based, not direct field access)
    let mut config = zenoh::Config::default();
    config
        .insert_json5("connect/endpoints", &format!("[{}]", serde_json::to_string(&endpoint).unwrap()))
        .map_err(|e| format!("config error: {e}"))?;

    // Open session
    let session = zenoh::open(config)
        .await
        .map_err(|e| format!("zenoh open failed: {e}"))?;

    // Subscribe to capacity advertisements
    let subscriber = session
        .declare_subscriber("harmony/compute/capacity/*")
        .await
        .map_err(|e| format!("subscribe failed: {e}"))?;

    // Spawn subscriber task
    let app_handle = app.clone();
    let task = tokio::spawn(async move {
        loop {
            match subscriber.recv_async().await {
                Ok(sample) => {
                    // Note: zenoh v1 API — verify exact methods against your
                    // installed version. KeyExpr implements AsRef<str>.
                    let key = sample.key_expr().as_ref();
                    let payload = sample.payload().to_bytes();
                    if let Some(update) = parse_capacity(key, &payload) {
                        let _ = app_handle.emit("capacity-update", &update);
                    }
                }
                Err(_) => break, // Session closed, exit cleanly
            }
        }
    });

    // Store state
    {
        let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
        guard.session = Some(session);
        guard.task = Some(task);
    }

    let _ = app.emit("zenoh-status", &ZenohStatus {
        status: "connected".to_string(),
        endpoint: Some(endpoint),
        error: None,
    });

    Ok(())
}
```

- [ ] **Step 3: Add disconnect_zenoh command and helper**

```rust
async fn disconnect_inner(
    app: &AppHandle,
    state: &Mutex<ZenohState>,
) {
    let task = {
        let mut guard = match state.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        guard.session.take(); // Drop session → subscriber returns Err → task exits
        guard.task.take()
    };
    if let Some(task) = task {
        let _ = task.await; // Wait for subscriber task to finish
    }
    let _ = app.emit("zenoh-status", &ZenohStatus {
        status: "disconnected".to_string(),
        endpoint: None,
        error: None,
    });
}

#[tauri::command]
async fn disconnect_zenoh(
    app: AppHandle,
    state: tauri::State<'_, Mutex<ZenohState>>,
) -> Result<(), String> {
    disconnect_inner(&app, &state).await;
    Ok(())
}
```

- [ ] **Step 4: Register commands and state in run()**

Update the `run()` function:

```rust
pub fn run() {
    tauri::Builder::default()
        .manage(Mutex::new(ZenohState::default()))
        .invoke_handler(tauri::generate_handler![
            list_vine_videos,
            follow_vine_creator,
            unfollow_vine_creator,
            mark_vine_viewed,
            connect_zenoh,
            disconnect_zenoh,
        ])
        .run(tauri::generate_context!())
        .expect("error while running harmony");
}
```

- [ ] **Step 5: Verify compilation**

```bash
cd src-tauri && cargo check
```

Note: Full `cargo test` still passes (the new commands are async and can't be unit tested without a Tauri runtime, but `parse_capacity` tests run fine).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(tauri): add connect_zenoh/disconnect_zenoh async commands with capacity subscription"
```

---

### Task 3: Frontend — ZenohService with TauriAdapter

Create the service layer with dependency injection for testability.

**Files:**
- Create: `src/lib/zenoh-service.ts`
- Create: `src/lib/zenoh-service.test.ts`

- [ ] **Step 1: Create zenoh-service.ts**

```typescript
// src/lib/zenoh-service.ts

/** Abstraction over Tauri IPC for testability. */
export interface TauriAdapter {
  invoke(cmd: string, args?: Record<string, unknown>): Promise<unknown>;
  listen(event: string, handler: (event: { payload: unknown }) => void): Promise<() => void>;
}

export interface DiscoveredNode {
  nodeAddr: string;
  modelCid: string;
  ready: boolean;
  lastSeen: number;
}

export interface CapacityUpdate {
  nodeAddr: string;
  modelCid: string;
  ready: boolean;
}

export interface ZenohStatusEvent {
  status: 'connected' | 'disconnected' | 'error';
  endpoint?: string;
  error?: string;
}

export type ConnectionStatus = 'disconnected' | 'connecting' | 'connected' | 'error';

export class ZenohService {
  connectionStatus: ConnectionStatus = 'disconnected';
  discoveredNodes: Map<string, DiscoveredNode> = new Map();
  errorMessage?: string;

  private adapter: TauriAdapter;
  private unlisteners: Array<() => void> = [];

  constructor(adapter: TauriAdapter) {
    this.adapter = adapter;
  }

  async init(): Promise<void> {
    const unlistenCapacity = await this.adapter.listen(
      'capacity-update',
      (event) => {
        const update = event.payload as CapacityUpdate;
        this.discoveredNodes.set(update.nodeAddr, {
          ...update,
          lastSeen: Date.now(),
        });
      },
    );
    this.unlisteners.push(unlistenCapacity);

    const unlistenStatus = await this.adapter.listen(
      'zenoh-status',
      (event) => {
        const status = event.payload as ZenohStatusEvent;
        if (status.status === 'connected') {
          this.connectionStatus = 'connected';
          this.errorMessage = undefined;
        } else if (status.status === 'disconnected') {
          this.connectionStatus = 'disconnected';
          this.errorMessage = undefined;
        } else if (status.status === 'error') {
          this.connectionStatus = 'error';
          this.errorMessage = status.error;
        }
      },
    );
    this.unlisteners.push(unlistenStatus);
  }

  async connect(endpoint: string): Promise<void> {
    this.connectionStatus = 'connecting';
    this.errorMessage = undefined;
    try {
      await this.adapter.invoke('connect_zenoh', { endpoint });
    } catch (e) {
      this.connectionStatus = 'error';
      this.errorMessage = String(e);
    }
  }

  async disconnect(): Promise<void> {
    try {
      await this.adapter.invoke('disconnect_zenoh');
    } catch {
      // Ignore disconnect errors
    }
    this.discoveredNodes.clear();
  }

  destroy(): void {
    for (const unlisten of this.unlisteners) {
      unlisten();
    }
    this.unlisteners = [];
  }
}
```

- [ ] **Step 2: Create zenoh-service.test.ts**

```typescript
// src/lib/zenoh-service.test.ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { ZenohService, type TauriAdapter, type CapacityUpdate, type ZenohStatusEvent } from './zenoh-service';

function createMockAdapter() {
  const listeners: Record<string, Array<(event: { payload: unknown }) => void>> = {};
  const adapter: TauriAdapter = {
    invoke: vi.fn().mockResolvedValue(undefined),
    listen: vi.fn().mockImplementation(async (event: string, handler: (event: { payload: unknown }) => void) => {
      if (!listeners[event]) listeners[event] = [];
      listeners[event].push(handler);
      return () => {
        listeners[event] = listeners[event].filter((h) => h !== handler);
      };
    }),
  };
  function emit(event: string, payload: unknown) {
    for (const handler of listeners[event] ?? []) {
      handler({ payload });
    }
  }
  return { adapter, emit, listeners };
}

describe('ZenohService', () => {
  let service: ZenohService;
  let mock: ReturnType<typeof createMockAdapter>;

  beforeEach(async () => {
    mock = createMockAdapter();
    service = new ZenohService(mock.adapter);
    await service.init();
  });

  it('starts disconnected', () => {
    expect(service.connectionStatus).toBe('disconnected');
    expect(service.discoveredNodes.size).toBe(0);
  });

  it('connect invokes connect_zenoh with endpoint', async () => {
    await service.connect('tcp/127.0.0.1:7447');
    expect(mock.adapter.invoke).toHaveBeenCalledWith('connect_zenoh', {
      endpoint: 'tcp/127.0.0.1:7447',
    });
  });

  it('sets connecting status during connect', async () => {
    const promise = service.connect('tcp/127.0.0.1:7447');
    expect(service.connectionStatus).toBe('connecting');
    await promise;
  });

  it('updates status on zenoh-status connected event', async () => {
    mock.emit('zenoh-status', {
      status: 'connected',
      endpoint: 'tcp/127.0.0.1:7447',
    } satisfies ZenohStatusEvent);
    expect(service.connectionStatus).toBe('connected');
  });

  it('updates status on zenoh-status error event', () => {
    mock.emit('zenoh-status', {
      status: 'error',
      error: 'connection refused',
    } satisfies ZenohStatusEvent);
    expect(service.connectionStatus).toBe('error');
    expect(service.errorMessage).toBe('connection refused');
  });

  it('upserts discovered node on capacity-update', () => {
    mock.emit('capacity-update', {
      nodeAddr: 'deadbeef',
      modelCid: 'aabb',
      ready: true,
    } satisfies CapacityUpdate);
    expect(service.discoveredNodes.size).toBe(1);
    const node = service.discoveredNodes.get('deadbeef')!;
    expect(node.modelCid).toBe('aabb');
    expect(node.ready).toBe(true);
    expect(node.lastSeen).toBeGreaterThan(0);
  });

  it('updates lastSeen on duplicate capacity-update', () => {
    mock.emit('capacity-update', {
      nodeAddr: 'node1',
      modelCid: 'cc',
      ready: true,
    } satisfies CapacityUpdate);
    const first = service.discoveredNodes.get('node1')!.lastSeen;

    mock.emit('capacity-update', {
      nodeAddr: 'node1',
      modelCid: 'cc',
      ready: false,
    } satisfies CapacityUpdate);
    const second = service.discoveredNodes.get('node1')!;
    expect(second.lastSeen).toBeGreaterThanOrEqual(first);
    expect(second.ready).toBe(false);
  });

  it('disconnect clears discovered nodes', async () => {
    mock.emit('capacity-update', {
      nodeAddr: 'node1',
      modelCid: 'cc',
      ready: true,
    } satisfies CapacityUpdate);
    expect(service.discoveredNodes.size).toBe(1);

    await service.disconnect();
    expect(service.discoveredNodes.size).toBe(0);
    expect(mock.adapter.invoke).toHaveBeenCalledWith('disconnect_zenoh');
  });

  it('sets error on invoke failure', async () => {
    (mock.adapter.invoke as ReturnType<typeof vi.fn>).mockRejectedValueOnce('timeout');
    await service.connect('tcp/bad:9999');
    expect(service.connectionStatus).toBe('error');
    expect(service.errorMessage).toBe('timeout');
  });

  it('destroy removes listeners', () => {
    service.destroy();
    // Emit after destroy — should not update state
    mock.emit('capacity-update', {
      nodeAddr: 'after-destroy',
      modelCid: 'xx',
      ready: true,
    } satisfies CapacityUpdate);
    expect(service.discoveredNodes.size).toBe(0);
  });
});
```

- [ ] **Step 3: Verify tests pass**

Run: `npx vitest run src/lib/zenoh-service.test.ts`

- [ ] **Step 4: Commit**

```bash
git add src/lib/zenoh-service.ts src/lib/zenoh-service.test.ts
git commit -m "feat: add ZenohService with TauriAdapter injection and tests"
```

---

### Task 4: Frontend — ConnectionBar Component

Create the connection bar UI component with tests.

**Files:**
- Create: `src/lib/components/ConnectionBar.svelte`
- Create: `src/lib/components/__tests__/ConnectionBar.test.ts`
- Modify: `src/NetworkApp.svelte`

- [ ] **Step 1: Create ConnectionBar.svelte**

```svelte
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

  let endpoint = $state(defaultEndpoint);

  // Load from localStorage
  if (typeof localStorage !== 'undefined') {
    const saved = localStorage.getItem('zenoh-endpoint');
    if (saved) endpoint = saved;
  }

  function handleConnect() {
    if (typeof localStorage !== 'undefined') {
      localStorage.setItem('zenoh-endpoint', endpoint);
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

  <div
    class="status-indicator"
    role="status"
    aria-label={statusLabel}
  >
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

  .connect-btn:hover {
    opacity: 0.9;
  }

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
```

- [ ] **Step 2: Create ConnectionBar.test.ts**

```typescript
// src/lib/components/__tests__/ConnectionBar.test.ts
import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import ConnectionBar from '../ConnectionBar.svelte';

describe('ConnectionBar', () => {
  it('renders endpoint input with default value', () => {
    render(ConnectionBar, {
      props: {
        connectionStatus: 'disconnected',
        discoveredCount: 0,
        onConnect: vi.fn(),
        onDisconnect: vi.fn(),
      },
    });
    const input = screen.getByLabelText('Zenoh endpoint') as HTMLInputElement;
    expect(input.value).toBe('tcp/127.0.0.1:7447');
  });

  it('renders connect button when disconnected', () => {
    render(ConnectionBar, {
      props: {
        connectionStatus: 'disconnected',
        discoveredCount: 0,
        onConnect: vi.fn(),
        onDisconnect: vi.fn(),
      },
    });
    expect(screen.getByText('Connect')).toBeTruthy();
  });

  it('renders disconnect button when connected', () => {
    render(ConnectionBar, {
      props: {
        connectionStatus: 'connected',
        discoveredCount: 2,
        onConnect: vi.fn(),
        onDisconnect: vi.fn(),
      },
    });
    expect(screen.getByText('Disconnect')).toBeTruthy();
  });

  it('shows discovered count when connected', () => {
    render(ConnectionBar, {
      props: {
        connectionStatus: 'connected',
        discoveredCount: 3,
        onConnect: vi.fn(),
        onDisconnect: vi.fn(),
      },
    });
    const status = screen.getByRole('status');
    expect(status.getAttribute('aria-label')).toContain('3 nodes discovered');
  });

  it('shows error message', () => {
    render(ConnectionBar, {
      props: {
        connectionStatus: 'error',
        discoveredCount: 0,
        onConnect: vi.fn(),
        onDisconnect: vi.fn(),
        errorMessage: 'connection refused',
      },
    });
    const status = screen.getByRole('status');
    expect(status.getAttribute('aria-label')).toContain('connection refused');
  });

  it('disables input when connected', () => {
    render(ConnectionBar, {
      props: {
        connectionStatus: 'connected',
        discoveredCount: 0,
        onConnect: vi.fn(),
        onDisconnect: vi.fn(),
      },
    });
    const input = screen.getByLabelText('Zenoh endpoint') as HTMLInputElement;
    expect(input.disabled).toBe(true);
  });
});
```

- [ ] **Step 3: Verify tests pass**

Run: `npx vitest run src/lib/components/__tests__/ConnectionBar.test.ts`

- [ ] **Step 4: Commit**

```bash
git add src/lib/components/ConnectionBar.svelte src/lib/components/__tests__/ConnectionBar.test.ts
git commit -m "feat: add ConnectionBar component with endpoint input and status indicator"
```

---

### Task 5: Wire Into NetworkApp

Connect the ZenohService and ConnectionBar to NetworkApp.svelte.

**Files:**
- Modify: `src/NetworkApp.svelte`

- [ ] **Step 1: Add ConnectionBar to NetworkApp**

In `src/NetworkApp.svelte`, add imports:

```typescript
import ConnectionBar from './lib/components/ConnectionBar.svelte';
```

Add state for connection (after existing state declarations):

```typescript
// Zenoh connection state (non-reactive stubs for now — real TauriAdapter
// will be wired when running in Tauri; in dev/browser mode, these are no-ops)
let zenohStatus = $state<'disconnected' | 'connecting' | 'connected' | 'error'>('disconnected');
let discoveredCount = $state(0);
let zenohError = $state<string | undefined>(undefined);

function handleConnect(endpoint: string) {
  zenohStatus = 'connecting';
  // In Tauri mode, this would call ZenohService.connect()
  // For now, log the attempt (no Tauri in dev browser)
  console.log('Zenoh connect requested:', endpoint);
  setTimeout(() => {
    zenohStatus = 'disconnected'; // No Tauri in dev mode
  }, 1000);
}

function handleDisconnect() {
  zenohStatus = 'disconnected';
  discoveredCount = 0;
}
```

Add `<ConnectionBar>` in the template, right after the `<NetworkToolbar>`:

```svelte
<ConnectionBar
  connectionStatus={zenohStatus}
  {discoveredCount}
  errorMessage={zenohError}
  onConnect={handleConnect}
  onDisconnect={handleDisconnect}
/>
```

- [ ] **Step 2: Verify all tests pass**

Run: `npx vitest run`

All 770+ tests should pass (existing 764 + ~6 new from ConnectionBar + ~9 from ZenohService).

- [ ] **Step 3: Verify build**

Run: `npm run build`

- [ ] **Step 4: Commit**

```bash
git add src/NetworkApp.svelte
git commit -m "feat: wire ConnectionBar into NetworkApp with Zenoh connection stubs"
```
