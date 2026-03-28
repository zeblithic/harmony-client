# Telemetry Subscription in Daemon — Design Spec

## Goal

Subscribe to Harmony network telemetry events (`health` and
`capacity_changed` intents) via Zenoh in the Tauri backend and push
real node metrics to the frontend, replacing mock sentinel values
with live data.

## Background

The harmony-client Tauri backend connects to Zenoh and subscribes to
`harmony/compute/capacity/*` for node discovery. The frontend has
`NodeMetrics` interfaces, ring buffers for metric history, and heat
map visualizations — but all metrics are zeros or mock data for
discovered nodes.

The harmony-telemetry crate (in harmony core) defines
`TelemetryEvent` with a flexible `intent` field and JSON payload,
published on `harmony/telemetry/{node_addr}/{intent}`. The types,
wire format (JSON with 0x00 tag byte), and Zenoh namespace helpers
are complete.

This spec wires the receive side: backend subscribes, decodes, emits
to frontend via Tauri IPC.

## Design Decisions

- **Extend existing Zenoh pattern, don't embed NodeRuntime**: The
  backend already has a working Zenoh subscriber for capacity. Adding
  telemetry subscriptions follows the same pattern. NodeRuntime
  embedding is a separate bead (`harmony-client-9lk`).
- **Single `telemetry-event` IPC event**: All intents flow through
  one Tauri event type, discriminated by `intent` field. Adding new
  intents requires zero backend changes.
- **Health + capacity_changed initially**: Only two intents
  subscribed. The existing `capacity-update` event stays for
  backwards compatibility.
- **No new UI components**: Existing `NodeMetrics` consumers,
  ring buffers, and heat maps already work — they just need real
  data instead of zeros.

## Backend Changes

### New Dependency

Add `harmony-telemetry` to `src-tauri/Cargo.toml`. This is a git
dependency from the harmony core workspace (same source as
harmony-zenoh). It provides `TelemetryEvent`, `decode_event()`, and
`TelemetryError`.

### Telemetry Subscriber

When `connect_zenoh` establishes a session, subscribe to two
additional key expressions:

- `harmony/telemetry/*/health`
- `harmony/telemetry/*/capacity_changed`

These use Zenoh's wildcard matching — `*` matches any single node
address segment.

The subscriber task (spawned alongside the existing capacity
subscriber) receives messages, decodes them with
`harmony_telemetry::decode_event()`, and emits a `telemetry-event`
Tauri IPC event.

### IPC Event Payload

```rust
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TelemetryEventPayload {
    node_addr: String,
    intent: String,
    sequence: u64,
    timestamp: u64,
    payload: serde_json::Value,
    confidence: Option<f32>,
    source: Option<String>,
}
```

Uses `camelCase` serialization to match the existing `CapacityUpdate`
and `ZenohStatus` IPC structs. The `node_addr` field is taken from
the decoded `TelemetryEvent` (authoritative source, already
validated by the publisher). The JSON `payload` field is passed
through opaquely — the frontend interprets it based on `intent`.

### Integration with Existing Subscriber

Two options for the subscriber task:

1. **Extend the existing task** — add telemetry subscriptions to the
   same spawned task that handles capacity. Shares the same
   generation counter and closing flag.
2. **Separate task** — spawn a second subscriber task for telemetry.
   Independent lifecycle.

Option 1 is preferred — single task, single generation check, single
cleanup path. The task uses `tokio::select!` to receive from both
the capacity subscriber and the telemetry subscriber.

### Disconnect Cleanup

On `disconnect_zenoh`, the closing flag signals the subscriber task
to exit. Both capacity and telemetry subscriptions are dropped when
the session closes. Same lifecycle as today.

### Error Handling

- `decode_event()` failures (malformed payload, unknown tag): log
  with `eprintln!` (or `tracing::warn!` if tracing is added), skip
  the message. Don't crash or disconnect.
- Subscriber receive errors: same handling as existing capacity
  subscriber (break loop, emit disconnected status).

Note: `tracing` is not currently a dependency. Use `eprintln!` for
now — adding tracing is a separate concern.

## Frontend Changes

### New Types

Create `src/lib/telemetry-types.ts`:

```typescript
/** Matches the camelCase-serialized TelemetryEventPayload from the backend. */
export interface TelemetryEvent {
    nodeAddr: string;
    intent: string;
    sequence: number;
    timestamp: number;
    /** Opaque JSON payload — shape depends on intent. May be an object,
     *  array, string, number, or null. */
    payload: unknown;
    confidence?: number;
    source?: string;
}

/** Shape of payload when intent === "health". Fields are snake_case
 *  because they originate from the publishing node's JSON, not from
 *  serde rename. */
export interface HealthPayload {
    cpu_percent?: number;
    mem_mb?: number;
}

export interface CapacityChangedPayload {
    model_cid?: string;
    ready?: boolean;
}
```

### ZenohService Extension

In `zenoh-service.ts`, register a listener for `telemetry-event`
in `init()`:

```typescript
this.unlisteners.push(await this.adapter.listen(
    'telemetry-event',
    (event) => this.handleTelemetryEvent(event.payload)
));
```

The unlisten function is pushed to the existing `this.unlisteners`
array, ensuring cleanup in `destroy()` follows the same pattern as
other listeners.

`handleTelemetryEvent` (private method) switches on `intent`:

- **`health`**: Extract `cpu_percent` and `mem_mb` from
  `payload`. If the node exists in `discoveredNodes`, update its
  metrics. Push a `NodeMetrics` entry to the node's ring buffer.
  Trigger `onChange()`.
- **`capacity_changed`**: Extract `model_cid` and `ready` from
  `payload`. Update the node's discovered state. Trigger
  `onChange()`.
- **Unknown intent**: Ignore silently (forward-compatible).

### DiscoveredNode Extension

Add optional health fields to `DiscoveredNode`:

```typescript
interface DiscoveredNode {
    nodeAddr: string;
    modelCid: string;
    ready: boolean;
    lastSeen: number;
    // New: latest health metrics
    cpuPercent?: number;
    memMb?: number;
}
```

### zenoh-utils Update

In `discoveredToNetworkNode()`, use real health metrics when
available instead of zero sentinels:

```typescript
metrics: {
    timestamp: Date.now(),
    cpuPercent: node.cpuPercent ?? 0,
    // mem_mb from health payload is used-memory only. Total memory
    // is not available until a richer health payload is defined.
    // Use used-memory for the "used" field; keep sentinel for "total"
    // so percentage calculations don't show 100%.
    memoryUsedBytes: (node.memMb ?? 0) * 1024 * 1024,
    memoryTotalBytes: 1,
    diskUsedBytes: 0,
    diskTotalBytes: 1,
}
```

## Testing

### Backend Tests (Rust)

- Decode a valid health telemetry payload and verify
  `TelemetryEventPayload` fields
- Decode a valid capacity_changed payload
- Malformed payload (bad tag byte) is skipped without panic
- Empty payload is skipped without panic

### Frontend Tests (vitest)

- `handleTelemetryEvent` with health intent updates discovered
  node metrics
- `handleTelemetryEvent` with capacity_changed intent updates
  ready status
- `handleTelemetryEvent` with unknown intent is silently ignored
- Health metrics flow through to `discoveredToNetworkNode()`
  conversion (non-zero CPU/memory)
- Stale node filtering still works with telemetry-updated nodes

## Out of Scope

- Embedding harmony-runtime NodeRuntime (`harmony-client-9lk`)
- Publishing telemetry from the client
- New UI components or visualizations
- Replacing `capacity-update` event (backwards compat preserved)
- Wildcard subscription to all intents
- Telemetry persistence or history beyond ring buffers
