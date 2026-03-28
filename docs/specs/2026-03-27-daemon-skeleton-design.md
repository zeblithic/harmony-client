# Daemon Skeleton — Zenoh Connection + Capacity Discovery

## Goal

Add a Zenoh client to the Tauri backend that connects to a configurable endpoint, subscribes to capacity advertisements, and pushes discovered node data to the Svelte frontend via Tauri events. First slice of the full daemon integration (harmony-client-1s7).

## Architecture

The Tauri backend gains a Zenoh session that subscribes to `harmony/compute/capacity/*`. When capacity advertisements arrive, the backend parses them and emits Tauri events to the frontend. The frontend displays connection status and discovered node count.

### Data Flow

```
harmony-node → publishes capacity on Zenoh
    ↓
Tauri backend (Zenoh subscriber) → parses 33-byte payload
    ↓
Tauri event "capacity-update" → JSON { nodeAddr, modelCid, ready }
    ↓
Svelte frontend (zenoh-service.ts) → updates discoveredNodes list
    ↓
NetworkApp connection bar → shows "3 nodes discovered"
```

### Dependencies Added to `src-tauri/Cargo.toml`

```toml
zenoh = { version = "1", default-features = false }
tokio = { version = "1", features = ["sync"] }
hex = "0.4"
```

Zenoh v1.x is the current stable release. The `tokio` dep adds `sync` for `watch` channels (Tauri v2 already provides the tokio runtime).

No harmony core crates are added in this slice — the capacity payload is parsed inline (32 bytes CID + 1 byte status). Core crate dependencies come in follow-on slices when we need identity, Reticulum, etc.

**Payload format note:** The 33-byte binary format matches `harmony-node/src/inference.rs` (`build_capacity_payload`). This format is not yet stabilized in a shared crate. Implementors should verify against the current `harmony-node` source. A follow-on bead should move the format into a shared crate once core deps are added.

## Backend Implementation

All changes in `src-tauri/src/lib.rs`. No new crates or workspace restructuring.

### Managed State

```rust
use std::sync::Mutex;
use tokio::task::JoinHandle;

struct ZenohState {
    session: Option<zenoh::Session>,
    task: Option<JoinHandle<()>>,
}
```

Wrapped in `Mutex` and registered as Tauri managed state. `zenoh::Session` is `Send + Sync` (it's `Arc`-wrapped internally).

No cancel token needed — dropping the Zenoh session causes `subscriber.recv_async()` to return an error, which the subscriber task treats as a clean shutdown signal.

### Commands

Both commands are `async fn` — Tauri v2 supports async commands natively via its internal tokio runtime.

**`connect_zenoh(endpoint: String) -> Result<(), String>`**
1. If already connected, disconnect first (tear down existing session)
2. Build Zenoh config with the endpoint as a connect target
3. Open Zenoh session: `zenoh::open(config).await`
4. Declare subscriber on `harmony/compute/capacity/*`
5. Spawn a tokio task that loops on `subscriber.recv_async().await`:
   - On `Ok(sample)`: extract `node_addr` from key expression (strip `harmony/compute/capacity/` prefix), parse 33-byte payload, emit Tauri event `"capacity-update"`
   - On `Err(_)`: session was closed, exit task cleanly
6. Store session + task handle in managed state
7. Emit `"zenoh-status"` event: `{ status: "connected", endpoint: "..." }`
8. On failure at any step: emit `"zenoh-status"` with `{ status: "error", error: "<message>" }` and return `Err`

**`disconnect_zenoh() -> Result<(), String>`**
1. Take session from managed state (this drops it, closing subscribers)
2. Await task handle to ensure subscriber task exits cleanly
3. Emit `"zenoh-status"` event: `{ status: "disconnected" }`

### Capacity Payload Parsing

Inline function (no harmony core dep):

```rust
struct CapacityUpdate {
    node_addr: String,
    model_cid: String,
    ready: bool,
}

fn parse_capacity(key_expr: &str, payload: &[u8]) -> Option<CapacityUpdate> {
    let node_addr = key_expr.strip_prefix("harmony/compute/capacity/")?;
    if payload.len() < 33 { return None; }
    let model_cid = hex::encode(&payload[..32]);
    let ready = payload[32] == 0x01;
    Some(CapacityUpdate {
        node_addr: node_addr.to_string(),
        model_cid,
        ready,
    })
}
```

Payload format (from `harmony-node/src/inference.rs`):
- `[model_gguf_cid: 32 bytes][status: u8]`
- `status == 0x01` → ready, `status == 0x00` → busy

### Event Payloads

**`"capacity-update"`** — emitted per capacity advertisement:
```typescript
interface CapacityUpdate {
  nodeAddr: string;
  modelCid: string;
  ready: boolean;
}
```

**`"zenoh-status"`** — emitted on connect, disconnect, and error:
```typescript
interface ZenohStatus {
  status: 'connected' | 'disconnected' | 'error';
  endpoint?: string;
  error?: string;
}
```

Backend always emits the full `ZenohStatus` object (not a bare string). The `endpoint` field is populated on `connected`, `error` field on `error`.

## Frontend Implementation

### New File: `src/lib/zenoh-service.ts`

A service that manages Zenoh connection state and discovered nodes. Uses **dependency injection** for the Tauri API so it can be tested without Tauri:

```typescript
interface TauriAdapter {
  invoke(cmd: string, args?: Record<string, unknown>): Promise<unknown>;
  listen(event: string, handler: (event: { payload: unknown }) => void): Promise<() => void>;
}

interface DiscoveredNode {
  nodeAddr: string;
  modelCid: string;
  ready: boolean;
  lastSeen: number;
}

class ZenohService {
  connectionStatus: 'disconnected' | 'connecting' | 'connected' | 'error';
  discoveredNodes: Map<string, DiscoveredNode>;
  errorMessage?: string;

  constructor(private adapter: TauriAdapter) { ... }
  async connect(endpoint: string): Promise<void>;
  async disconnect(): Promise<void>;
  destroy(): void;
}
```

- `constructor(adapter)` — stores adapter, registers event listeners
- `connect(endpoint)` — sets status to `'connecting'`, calls `adapter.invoke("connect_zenoh", { endpoint })`. On success, status transitions via `"zenoh-status"` event. On invoke error, sets status to `'error'`.
- `"capacity-update"` listener upserts into `discoveredNodes` map, updates `lastSeen`
- `"zenoh-status"` listener updates `connectionStatus` and `errorMessage`
- `disconnect()` calls `adapter.invoke("disconnect_zenoh")`, clears `discoveredNodes`
- `destroy()` calls unlisten functions from all registered listeners

In production, `TauriAdapter` is implemented with real `@tauri-apps/api/core` `invoke` and `@tauri-apps/api/event` `listen`. In tests, a mock adapter is injected.

### NetworkApp.svelte Update

Add a connection bar above the graph/table area:

- Text input for Zenoh endpoint (default: `tcp/127.0.0.1:7447`, persisted to localStorage)
- Connect / Disconnect button (toggles based on connection status)
- Status indicator with `role="status"` and `aria-label` describing current state (e.g., "Connected to tcp/127.0.0.1:7447, 3 nodes discovered")
- Status dot color: gray (disconnected), yellow (connecting), green (connected), red (error)
- Discovered node count badge: "N nodes discovered" (or "No live connection" when disconnected)

The connection bar is a thin horizontal strip — it doesn't take significant space from the graph.

Discovered nodes are NOT merged into the D3 force graph in this slice. They appear only as the count badge. Graph integration is a follow-on bead (harmony-client-mzn).

## Testing Strategy

### Backend (Rust) — unit tests in `src-tauri/src/lib.rs`

- `parse_capacity` with valid 33-byte payload → correct CID hex + status bool
- `parse_capacity` with truncated payload (< 33 bytes) → `None`
- `parse_capacity` with status `0x01` → `ready = true`
- `parse_capacity` with status `0x00` → `ready = false`
- `parse_capacity` extracts node address from key expression correctly
- `parse_capacity` with wrong key prefix → `None`

No integration test for Zenoh connection — requires a running Zenoh router.

### Frontend (TypeScript) — vitest

**`zenoh-service.test.ts`** — uses a mock `TauriAdapter`:
- `connect()` calls `adapter.invoke("connect_zenoh", { endpoint })`
- `"capacity-update"` event upserts into `discoveredNodes`
- Duplicate `"capacity-update"` for same `nodeAddr` updates `lastSeen`
- `"zenoh-status"` with `status: "error"` sets `connectionStatus` and `errorMessage`
- `disconnect()` clears `discoveredNodes` and sets status to `"disconnected"`
- `destroy()` calls all unlisten functions

**Connection bar component test:**
- Renders endpoint input with default value
- Renders connect button when disconnected
- Renders disconnect button when connected
- Shows node count when nodes discovered
- Status dot has appropriate `aria-label`

### Manual end-to-end

1. Start `harmony-node` with `--inference-gguf-cid` + `--inference-tokenizer-cid` (publishes capacity)
2. Start `harmony-client` with `npm run tauri dev`
3. Enter Zenoh endpoint in connection bar, click Connect
4. Verify: status green, "1 node discovered"
5. Click Disconnect → verify status gray, count clears

## Scope Exclusions

- **No D3 graph integration** — discovered nodes shown as count badge only (harmony-client-mzn)
- **No identity/auth** — anonymous Zenoh session
- **No transport manager** — Zenoh handles its own TCP/UDP
- **No harmony-daemon crate** — inline in src-tauri, refactor later
- **No content queries** — subscription only
- **No auto-reconnect** — manual connect/disconnect (harmony-client-zby)
- **No telemetry subscription** — capacity only (harmony-client-26l)
- **No node aging/expiry** — discovered nodes persist until disconnect (harmony-client-zby)
- **No re-connect while connected** — `connect_zenoh` disconnects first if already connected
