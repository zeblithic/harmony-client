# Telemetry Subscription Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Subscribe to Harmony telemetry events via Zenoh and push real node health metrics to the Svelte frontend.

**Architecture:** Add `harmony-telemetry` as a git dependency to the Tauri backend. Extend the existing Zenoh subscriber task to also receive `harmony/telemetry/*/health` and `harmony/telemetry/*/capacity_changed` messages. Decode with `harmony_telemetry::decode_event()`, emit `telemetry-event` Tauri IPC events (camelCase). Frontend `ZenohService` listens for these events and maps health payloads to `NodeMetrics` ring buffers.

**Tech Stack:** Rust (Tauri v2, Zenoh, harmony-telemetry), TypeScript (Svelte 5 runes, vitest)

**Spec:** `docs/specs/2026-03-28-telemetry-subscription-design.md`

---

### Task 1: Add harmony-telemetry dependency

**Files:**
- Modify: `src-tauri/Cargo.toml`

- [ ] **Step 1: Add harmony-telemetry git dependency**

Add to `src-tauri/Cargo.toml` under `[dependencies]`:

```toml
harmony-telemetry = { git = "https://github.com/zeblithic/harmony.git", branch = "main" }
```

- [ ] **Step 2: Verify it compiles**

Run: `cd src-tauri && cargo check`
Expected: Success

- [ ] **Step 3: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat: add harmony-telemetry dependency"
```

---

### Task 2: Add TelemetryEventPayload and parse function (TDD)

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Write failing test for parse_telemetry**

Add to the `#[cfg(test)] mod tests` block in `src-tauri/src/lib.rs` (after the existing `parse_capacity_*` tests, around line 383):

```rust
    #[test]
    fn parse_telemetry_valid_health() {
        let event = harmony_telemetry::TelemetryEvent {
            node_addr: "abcd1234".to_string(),
            intent: "health".to_string(),
            sequence: 1,
            timestamp: 1711600000,
            payload: serde_json::json!({"cpu_percent": 42.5, "mem_mb": 512}),
            confidence: None,
            source: None,
        };
        let wire = harmony_telemetry::encode_event(&event).unwrap();
        let result = parse_telemetry(&wire);
        let payload = result.unwrap();
        assert_eq!(payload.node_addr, "abcd1234");
        assert_eq!(payload.intent, "health");
        assert_eq!(payload.sequence, 1);
        assert_eq!(payload.timestamp, 1711600000);
    }

    #[test]
    fn parse_telemetry_valid_capacity_changed() {
        let event = harmony_telemetry::TelemetryEvent {
            node_addr: "node42".to_string(),
            intent: "capacity_changed".to_string(),
            sequence: 5,
            timestamp: 1711600100,
            payload: serde_json::json!({"model_cid": "aa".repeat(32), "ready": true}),
            confidence: None,
            source: Some("qwen3-0.6b".to_string()),
        };
        let wire = harmony_telemetry::encode_event(&event).unwrap();
        let result = parse_telemetry(&wire);
        let payload = result.unwrap();
        assert_eq!(payload.intent, "capacity_changed");
        assert_eq!(payload.source, Some("qwen3-0.6b".to_string()));
    }

    #[test]
    fn parse_telemetry_empty_payload() {
        let result = parse_telemetry(&[]);
        assert!(result.is_none());
    }

    #[test]
    fn parse_telemetry_bad_tag() {
        let result = parse_telemetry(&[0xFF, b'{', b'}']);
        assert!(result.is_none());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo test`
Expected: FAIL — `parse_telemetry` not found

- [ ] **Step 3: Implement TelemetryEventPayload and parse_telemetry**

Add after the existing `parse_capacity` function (around line 67) in `src-tauri/src/lib.rs`:

```rust
/// Telemetry event pushed to the frontend via IPC.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryEventPayload {
    pub node_addr: String,
    pub intent: String,
    pub sequence: u64,
    pub timestamp: u64,
    pub payload: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

fn parse_telemetry(wire: &[u8]) -> Option<TelemetryEventPayload> {
    let event = harmony_telemetry::decode_event(wire).ok()?;
    Some(TelemetryEventPayload {
        node_addr: event.node_addr,
        intent: event.intent,
        sequence: event.sequence,
        timestamp: event.timestamp,
        payload: event.payload,
        confidence: event.confidence,
        source: event.source,
    })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo test`
Expected: PASS (all existing + 4 new tests)

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat: add TelemetryEventPayload struct and parse_telemetry function"
```

---

### Task 3: Extend Zenoh subscriber to include telemetry

**Files:**
- Modify: `src-tauri/src/lib.rs`

This is the core backend change. The existing subscriber task (lines 210-235) receives only capacity messages. We extend it to also subscribe to telemetry key expressions and use `tokio::select!` to receive from both.

- [ ] **Step 1: Add telemetry subscriptions alongside capacity**

In `connect_zenoh` (around line 168-185), after the capacity subscriber is declared, declare two telemetry subscribers:

```rust
    // Subscribe to telemetry: health and capacity_changed
    let telem_health = match session
        .declare_subscriber("harmony/telemetry/*/health")
        .await
    {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("telemetry subscribe failed: {e}");
            let _ = app.emit(
                "zenoh-status",
                &ZenohStatus {
                    status: "error".to_string(),
                    endpoint: Some(endpoint.clone()),
                    error: Some(msg.clone()),
                },
            );
            let _ = session.close().await;
            return Err(msg);
        }
    };

    let telem_capacity = match session
        .declare_subscriber("harmony/telemetry/*/capacity_changed")
        .await
    {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("telemetry subscribe failed: {e}");
            let _ = app.emit(
                "zenoh-status",
                &ZenohStatus {
                    status: "error".to_string(),
                    endpoint: Some(endpoint.clone()),
                    error: Some(msg.clone()),
                },
            );
            let _ = session.close().await;
            return Err(msg);
        }
    };
```

Add cancellation checks after each telemetry subscribe await (same pattern as lines 188-192 for the capacity subscriber):

```rust
    // Check after telemetry subscribe awaits
    let was_cancelled = closing.load(Ordering::SeqCst);
    if was_cancelled {
        let _ = session.close().await;
        return Ok(());
    }
```

Place this check after the `telem_capacity` declaration (one check covers both telemetry subscribers since they're sequential).

- [ ] **Step 2: Rewrite the subscriber task to use tokio::select!**

Replace the existing subscriber loop (lines 210-234) with a `tokio::select!` loop that receives from all three subscribers:

```rust
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = subscriber.recv_async() => {
                        match result {
                            Ok(sample) => {
                                let key = sample.key_expr().as_str();
                                let payload = sample.payload().to_bytes();
                                if let Some(update) = parse_capacity(key, &payload) {
                                    let _ = app_handle.emit("capacity-update", &update);
                                }
                            }
                            Err(e) => {
                                if !task_closing.load(Ordering::SeqCst) {
                                    let _ = app_handle.emit(
                                        "zenoh-status",
                                        &ZenohStatus {
                                            status: "error".to_string(),
                                            endpoint: None,
                                            error: Some(format!("session lost: {e}")),
                                        },
                                    );
                                }
                                break;
                            }
                        }
                    }
                    result = telem_health.recv_async() => {
                        match result {
                            Ok(sample) => {
                                let payload = sample.payload().to_bytes();
                                if let Some(event) = parse_telemetry(&payload) {
                                    let _ = app_handle.emit("telemetry-event", &event);
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    result = telem_capacity.recv_async() => {
                        match result {
                            Ok(sample) => {
                                let payload = sample.payload().to_bytes();
                                if let Some(event) = parse_telemetry(&payload) {
                                    let _ = app_handle.emit("telemetry-event", &event);
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });
```

- [ ] **Step 3: Verify compilation**

Run: `cd src-tauri && cargo check`
Expected: Success

- [ ] **Step 4: Run tests**

Run: `cd src-tauri && cargo test`
Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat: subscribe to telemetry health and capacity_changed via Zenoh"
```

---

### Task 4: Add frontend telemetry types

**Files:**
- Create: `src/lib/telemetry-types.ts`

- [ ] **Step 1: Create telemetry-types.ts**

```typescript
/** Matches the camelCase-serialized TelemetryEventPayload from the Tauri backend. */
export interface TelemetryEvent {
  nodeAddr: string;
  intent: string;
  sequence: number;
  timestamp: number;
  /** Opaque JSON payload — shape depends on intent. */
  payload: unknown;
  confidence?: number;
  source?: string;
}

/** Shape of payload when intent === "health". */
export interface HealthPayload {
  cpu_percent?: number;
  mem_mb?: number;
}

/** Shape of payload when intent === "capacity_changed". */
export interface CapacityChangedPayload {
  model_cid?: string;
  ready?: boolean;
}
```

- [ ] **Step 2: Commit**

```bash
git add src/lib/telemetry-types.ts
git commit -m "feat: add TelemetryEvent TypeScript types"
```

---

### Task 5: Extend ZenohService to handle telemetry events (TDD)

**Files:**
- Modify: `src/lib/zenoh-service.ts`
- Create: `src/lib/zenoh-service-telemetry.test.ts`

- [ ] **Step 1: Write failing tests for telemetry handling**

Create `src/lib/zenoh-service-telemetry.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { ZenohService, type TauriAdapter, type DiscoveredNode } from '../zenoh-service';

function mockAdapter(): TauriAdapter & { handlers: Record<string, (e: { payload: unknown }) => void> } {
  const handlers: Record<string, (e: { payload: unknown }) => void> = {};
  return {
    handlers,
    invoke: vi.fn().mockResolvedValue(undefined),
    listen: vi.fn().mockImplementation((event: string, handler: (e: { payload: unknown }) => void) => {
      handlers[event] = handler;
      return Promise.resolve(() => {});
    }),
  };
}

describe('ZenohService telemetry', () => {
  let service: ZenohService;
  let adapter: ReturnType<typeof mockAdapter>;

  beforeEach(async () => {
    adapter = mockAdapter();
    service = new ZenohService(adapter);
    await service.init();
    // Simulate connected state with a known node
    service.connectionStatus = 'connected';
    service.discoveredNodes.set('abcd1234', {
      nodeAddr: 'abcd1234',
      modelCid: 'aa'.repeat(32),
      ready: true,
      lastSeen: Date.now(),
    });
  });

  it('registers telemetry-event listener on init', () => {
    expect(adapter.listen).toHaveBeenCalledWith('telemetry-event', expect.any(Function));
  });

  it('updates node metrics on health telemetry', () => {
    const onChange = vi.fn();
    service.onChange = onChange;
    adapter.handlers['telemetry-event']({
      payload: {
        nodeAddr: 'abcd1234',
        intent: 'health',
        sequence: 1,
        timestamp: 1711600000,
        payload: { cpu_percent: 42.5, mem_mb: 512 },
      },
    });
    const node = service.discoveredNodes.get('abcd1234')!;
    expect(node.cpuPercent).toBe(42.5);
    expect(node.memMb).toBe(512);
    expect(onChange).toHaveBeenCalled();
  });

  it('updates node ready status on capacity_changed telemetry', () => {
    adapter.handlers['telemetry-event']({
      payload: {
        nodeAddr: 'abcd1234',
        intent: 'capacity_changed',
        sequence: 2,
        timestamp: 1711600100,
        payload: { ready: false },
      },
    });
    const node = service.discoveredNodes.get('abcd1234')!;
    expect(node.ready).toBe(false);
  });

  it('ignores telemetry for unknown nodes', () => {
    const onChange = vi.fn();
    service.onChange = onChange;
    adapter.handlers['telemetry-event']({
      payload: {
        nodeAddr: 'unknown_node',
        intent: 'health',
        sequence: 1,
        timestamp: 1711600000,
        payload: { cpu_percent: 10 },
      },
    });
    expect(onChange).not.toHaveBeenCalled();
  });

  it('ignores unknown intents without error', () => {
    adapter.handlers['telemetry-event']({
      payload: {
        nodeAddr: 'abcd1234',
        intent: 'object_detected',
        sequence: 3,
        timestamp: 1711600200,
        payload: { class: 'person' },
      },
    });
    // No error, node unchanged
    const node = service.discoveredNodes.get('abcd1234')!;
    expect(node.cpuPercent).toBeUndefined();
  });

  it('ignores telemetry when not connected', () => {
    service.connectionStatus = 'disconnected';
    const onChange = vi.fn();
    service.onChange = onChange;
    adapter.handlers['telemetry-event']({
      payload: {
        nodeAddr: 'abcd1234',
        intent: 'health',
        sequence: 1,
        timestamp: 1711600000,
        payload: { cpu_percent: 50 },
      },
    });
    expect(onChange).not.toHaveBeenCalled();
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/zenoh-service-telemetry.test.ts`
Expected: FAIL — telemetry-event listener not registered, `cpuPercent` property doesn't exist

- [ ] **Step 3: Extend DiscoveredNode with health fields**

In `src/lib/zenoh-service.ts`, update the `DiscoveredNode` interface (line 7-12):

```typescript
export interface DiscoveredNode {
  nodeAddr: string;
  modelCid: string;
  ready: boolean;
  lastSeen: number;
  /** Latest CPU usage from health telemetry (0-100). */
  cpuPercent?: number;
  /** Latest memory usage in MB from health telemetry. */
  memMb?: number;
}
```

- [ ] **Step 4: Add telemetry event listener in init()**

In `src/lib/zenoh-service.ts`, add after the `unlistenStatus` block (around line 105) and before the closing `}` of `init()`:

```typescript
    const unlistenTelemetry = await this.adapter.listen(
      'telemetry-event',
      (event) => {
        if (this.connectionStatus !== 'connected') return;
        const telem = event.payload as import('./telemetry-types').TelemetryEvent;
        const node = this.discoveredNodes.get(telem.nodeAddr);
        if (!node) return;

        if (telem.intent === 'health') {
          const p = telem.payload as import('./telemetry-types').HealthPayload;
          if (p.cpu_percent !== undefined) node.cpuPercent = p.cpu_percent;
          if (p.mem_mb !== undefined) node.memMb = p.mem_mb;
          node.lastSeen = Date.now();
          this.onChange?.();
        } else if (telem.intent === 'capacity_changed') {
          const p = telem.payload as import('./telemetry-types').CapacityChangedPayload;
          if (p.ready !== undefined) node.ready = p.ready;
          if (p.model_cid !== undefined) node.modelCid = p.model_cid;
          node.lastSeen = Date.now();
          this.onChange?.();
        }
        // Unknown intents: silently ignore (forward-compatible)
      },
    );
    this.unlisteners.push(unlistenTelemetry);
```

Note: The spec mentions pushing `NodeMetrics` entries to the ring
buffer on each health event. This is deferred — the ring buffers
are created in `discoveredToNetworkNode()` (zenoh-utils) but are
not accessible from ZenohService. Populating ring buffers with live
telemetry data requires a follow-up that either passes the buffer
reference into the service or adds a metrics-push hook in the graph
conversion layer. The current implementation updates the
`DiscoveredNode` fields, which flow into the next
`discoveredToNetworkNode()` call.

- [ ] **Step 5: Run tests to verify they pass**

Run: `npx vitest run src/lib/zenoh-service-telemetry.test.ts`
Expected: PASS (all 6 tests)

- [ ] **Step 6: Run full test suite**

Run: `npx vitest run`
Expected: All tests pass (existing + new)

- [ ] **Step 7: Commit**

```bash
git add src/lib/zenoh-service.ts src/lib/zenoh-service-telemetry.test.ts
git commit -m "feat: handle telemetry events in ZenohService with health and capacity_changed"
```

---

### Task 6: Wire health metrics into graph conversion

**Files:**
- Modify: `src/lib/zenoh-utils.ts`

- [ ] **Step 1: Write failing test for metrics in discoveredToNetworkNode**

Add to the existing test file for zenoh-utils (or create `src/lib/zenoh-utils-telemetry.test.ts`):

```typescript
import { describe, it, expect } from 'vitest';
import { discoveredToNetworkNode } from '../zenoh-utils';

describe('discoveredToNetworkNode with telemetry', () => {
  it('uses real CPU metrics when available', () => {
    const node = discoveredToNetworkNode({
      nodeAddr: 'abcd1234',
      modelCid: 'aa'.repeat(32),
      ready: true,
      lastSeen: Date.now(),
      cpuPercent: 42.5,
      memMb: 512,
    });
    expect(node.metrics.cpuPercent).toBe(42.5);
    expect(node.metrics.memoryUsedBytes).toBe(512 * 1024 * 1024);
  });

  it('uses zero sentinels when no telemetry', () => {
    const node = discoveredToNetworkNode({
      nodeAddr: 'abcd1234',
      modelCid: 'aa'.repeat(32),
      ready: true,
      lastSeen: Date.now(),
    });
    expect(node.metrics.cpuPercent).toBe(0);
    expect(node.metrics.memoryUsedBytes).toBe(0);
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/zenoh-utils-telemetry.test.ts`
Expected: FAIL — `cpuPercent` property not accepted by `discoveredToNetworkNode`

- [ ] **Step 3: Update discoveredToNetworkNode to use health metrics**

In `src/lib/zenoh-utils.ts`, update the metrics object in `discoveredToNetworkNode()` (lines 65-72):

```typescript
    metrics: {
      timestamp: node.lastSeen,
      cpuPercent: node.cpuPercent ?? 0,
      memoryUsedBytes: (node.memMb ?? 0) * 1024 * 1024,
      memoryTotalBytes: 1, // sentinel — total memory not available in current health payload
      diskUsedBytes: 0,
      diskTotalBytes: 1,   // sentinel — avoids NaN in usage% until real metrics arrive
    },
```

Also update the `heatPercent` calculation:

```typescript
    heatPercent: node.cpuPercent ?? 0,
```

The `DiscoveredNode` import in zenoh-utils.ts already comes from zenoh-service.ts, which now includes the optional `cpuPercent` and `memMb` fields.

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/zenoh-utils-telemetry.test.ts`
Expected: PASS

- [ ] **Step 5: Run full test suite**

Run: `npx vitest run`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add src/lib/zenoh-utils.ts src/lib/zenoh-utils-telemetry.test.ts
git commit -m "feat: wire telemetry health metrics into graph node conversion"
```

---

### Task 7: Final verification

- [ ] **Step 1: Run backend tests**

Run: `cd src-tauri && cargo test`
Expected: All tests pass

- [ ] **Step 2: Run frontend tests**

Run: `npx vitest run`
Expected: All tests pass

- [ ] **Step 3: Build the app**

Run: `npm run build`
Expected: Success (Vite build completes)

- [ ] **Step 4: Fix any issues**

Address compilation, test, or build errors iteratively.

- [ ] **Step 5: Commit if any fixes needed**

```bash
git add -A
git commit -m "chore: fix build/test issues from telemetry integration"
```

- [ ] **Step 6: Verify git status is clean**

Run: `git status`
Expected: Clean working tree
