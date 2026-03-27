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

### Dependencies Added

| Dependency | Purpose |
|-----------|---------|
| `zenoh` | Zenoh session + subscriber |
| `tokio` | Async runtime (explicit features if needed) |
| `hex` | Encode CID bytes to hex string for frontend |

No harmony core crates are added in this slice — the capacity payload is parsed inline (32 bytes CID + 1 byte status). Core crate dependencies come in follow-on slices when we need identity, Reticulum, etc.

## Backend Implementation

All changes in `src-tauri/src/lib.rs`. No new crates or workspace restructuring.

### Managed State

```rust
use std::sync::Mutex;

struct ZenohState {
    session: Option<zenoh::Session>,
    cancel_token: Option<tokio::sync::watch::Sender<bool>>,
}
```

Wrapped in `Mutex` and registered as Tauri managed state.

### Commands

**`connect_zenoh(endpoint: String)`**
1. Build Zenoh config with the endpoint as a connect target
2. Open Zenoh session: `zenoh::open(config).await`
3. Subscribe to `harmony/compute/capacity/*`
4. Spawn a tokio task that loops on `subscriber.recv_async().await`:
   - Extract `node_addr` from key expression (strip `harmony/compute/capacity/` prefix)
   - Parse 33-byte payload: `[model_cid: 32 bytes][status: u8]`
   - Emit Tauri event `"capacity-update"` with JSON:
     ```json
     { "nodeAddr": "deadbeef...", "modelCid": "aabbcc...", "ready": true }
     ```
5. Create a cancel token (`tokio::sync::watch`) to stop the task on disconnect
6. Store session + cancel token in managed state
7. Emit `"zenoh-status"` event with `"connected"`

**`disconnect_zenoh()`**
1. Send cancel signal via watch channel
2. Close Zenoh session
3. Clear managed state
4. Emit `"zenoh-status"` event with `"disconnected"`

### Capacity Payload Parsing

Reuses the same binary format published by `harmony-node/src/inference.rs`:

```
[model_gguf_cid: 32 bytes LE] [status: u8]
```

- `status == 0x01` → ready
- `status == 0x00` → busy

The node address comes from the Zenoh key expression, not the payload:
`harmony/compute/capacity/{node_addr}` → strip prefix → `node_addr`

### Event Payloads

**`"capacity-update"`:**
```typescript
interface CapacityUpdate {
  nodeAddr: string;  // hex-encoded node address
  modelCid: string;  // hex-encoded 32-byte CID
  ready: boolean;
}
```

**`"zenoh-status"`:**
```typescript
interface ZenohStatus {
  status: 'connected' | 'disconnected' | 'error';
  endpoint?: string;
  error?: string;
}
```

## Frontend Implementation

### New File: `src/lib/zenoh-service.ts`

A reactive service that manages Zenoh connection state and discovered nodes.

```typescript
interface DiscoveredNode {
  nodeAddr: string;
  modelCid: string;
  ready: boolean;
  lastSeen: number;
}

interface ZenohService {
  connectionStatus: 'disconnected' | 'connecting' | 'connected' | 'error';
  discoveredNodes: Map<string, DiscoveredNode>;  // keyed by nodeAddr
  errorMessage?: string;
  connect(endpoint: string): Promise<void>;
  disconnect(): Promise<void>;
  destroy(): void;  // unsubscribe from Tauri events
}
```

- `connect()` sets status to `'connecting'`, invokes `connect_zenoh` command, listens for `"zenoh-status"` event
- `"capacity-update"` listener upserts into `discoveredNodes` map (keyed by `nodeAddr`), updates `lastSeen`
- `disconnect()` invokes `disconnect_zenoh`, clears `discoveredNodes`
- `destroy()` removes all Tauri event listeners (called on component unmount)

### NetworkApp.svelte Update

Add a connection bar above the graph/table area:

- Text input for Zenoh endpoint (default: `tcp/127.0.0.1:7447`, persisted to localStorage)
- Connect / Disconnect button (toggles based on connection status)
- Status dot: gray (disconnected), yellow (connecting), green (connected), red (error)
- Discovered node count badge: "N nodes discovered" (or "No live connection" when disconnected)

The connection bar is a thin horizontal strip — it doesn't take significant space from the graph.

Discovered nodes are NOT merged into the D3 force graph in this slice. They appear only as the count badge. Graph integration is a follow-on bead.

## Testing Strategy

### Backend (Rust) — unit tests

- Parse valid 33-byte capacity payload → correct CID hex + status bool
- Parse truncated payload (< 33 bytes) → error
- Parse payload with status 0x01 → ready=true
- Parse payload with status 0x00 → ready=false
- Extract node address from key expression: `"harmony/compute/capacity/deadbeef"` → `"deadbeef"`
- Extract from unexpected key format → error/empty

### Frontend (TypeScript) — vitest

**`zenoh-service.test.ts`:**
- `connect()` invokes `connect_zenoh` with endpoint
- `capacity-update` event upserts into `discoveredNodes`
- Duplicate `capacity-update` for same nodeAddr updates lastSeen
- `disconnect()` clears discoveredNodes and sets status to disconnected
- `destroy()` removes event listeners

**Connection bar component test:**
- Renders endpoint input with default value
- Renders connect button when disconnected
- Renders disconnect button when connected
- Shows node count when nodes are discovered

### Manual end-to-end

1. Start `harmony-node` with `--inference-gguf-cid` + `--inference-tokenizer-cid` (publishes capacity)
2. Start `harmony-client` with `npm run tauri dev`
3. Enter Zenoh endpoint, click Connect
4. Verify: status green, "1 node discovered"
5. Stop harmony-node → verify node disappears (or ages out)

## Scope Exclusions

- **No D3 graph integration** — discovered nodes shown as count badge only
- **No identity/auth** — anonymous Zenoh session
- **No transport manager** — Zenoh handles its own TCP/UDP
- **No harmony-daemon crate** — inline in src-tauri, refactor later
- **No content queries** — subscription only
- **No auto-reconnect** — manual connect/disconnect
- **No telemetry subscription** — capacity only (telemetry depends on NodeRuntime publishing health)
- **No node aging/expiry** — discovered nodes persist until disconnect. Expiry is follow-on.
