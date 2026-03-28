# Merge Discovered Nodes into D3 Force Graph — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show real nodes discovered via Zenoh capacity advertisements as live nodes in the D3 force graph, alongside mock nodes.

**Architecture:** Convert `DiscoveredNode` from `ZenohService` into `NetworkNode` objects and merge them into the nodes array that feeds the graph. Real nodes are visually distinguished by a "live" tag and their `modelName` from the capacity payload. A new `discoveredToNetworkNode()` utility handles the conversion. The merge happens in `syncZenohState()` which already runs on every service change.

**Tech Stack:** TypeScript, Svelte 5

**Spec:** Inline — scope is small enough that spec + plan are combined.

---

## Design

### Conversion: DiscoveredNode → NetworkNode

A `DiscoveredNode` from Zenoh has: `nodeAddr`, `modelCid`, `ready`, `lastSeen`.

A `NetworkNode` needs: `address`, `displayName`, `isLocal`, `hopDistance`, `status`, `metrics`, `metricsHistory`, `lastSeen`, `capabilities`, `heatPercent`, `modelName`.

The conversion function fills in defaults for fields we don't have from capacity data:
- `displayName`: truncated address (first 8 hex chars) + " (live)"
- `isLocal`: false (discovered nodes are always remote)
- `hopDistance`: 2 (default — we don't know actual hops)
- `status`: `ready ? 'online' : 'degraded'`
- `metrics`: zeroed (no telemetry data yet)
- `metricsHistory`: empty RingBuffer
- `capabilities`: `['inference']` (they advertised a model CID, so they have inference). Add `'routing'` too since all harmony nodes route.
- `heatPercent`: 0 (no CPU/mem data — shows as green, which is fine for "unknown load")
- `modelName`: derived from `modelCid` (first 8 hex chars for display)

### Merge Strategy

In `syncZenohState()`:
1. Get current `discoveredNodes` from ZenohService
2. Convert each to a `NetworkNode`
3. Merge with mock nodes: mock nodes come from `service.nodes`, real nodes from Zenoh. Deduplicate by address (real overrides mock if same address, though unlikely).
4. Set `nodes` and update `discoveredCount`

### Visual Distinction

Real nodes are distinguished by:
- `displayName` includes "(live)" suffix
- They'll have `capabilities: ['inference', 'routing']` which shows the inference badge
- They have `modelName` which shows in the detail panel
- `heatPercent: 0` means they render green (cool) — accurate since we have no load data

No new rendering code needed — the existing heat/badge/detail infrastructure handles everything.

---

## File Map

| File | Responsibility |
|------|---------------|
| `src/lib/zenoh-utils.ts` | `discoveredToNetworkNode()` conversion function |
| `src/lib/zenoh-utils.test.ts` | Tests for conversion |
| `src/NetworkApp.svelte` | Merge real nodes into `nodes` array in `syncZenohState()` |

---

### Task 1: Conversion Utility + Tests

**Files:**
- Create: `src/lib/zenoh-utils.ts`
- Create: `src/lib/zenoh-utils.test.ts`

- [ ] **Step 1: Create zenoh-utils.ts**

```typescript
import type { DiscoveredNode } from './zenoh-service';
import type { NetworkNode } from './network-types';
import { RingBuffer } from './ring-buffer';

const METRICS_HISTORY_CAPACITY = 300;

/** Convert a DiscoveredNode from Zenoh capacity into a NetworkNode for the graph. */
export function discoveredToNetworkNode(node: DiscoveredNode): NetworkNode {
  const shortAddr = node.nodeAddr.length > 8
    ? node.nodeAddr.slice(0, 8)
    : node.nodeAddr;
  const shortCid = node.modelCid.length > 8
    ? node.modelCid.slice(0, 8)
    : node.modelCid;

  return {
    address: node.nodeAddr,
    displayName: `${shortAddr} (live)`,
    isLocal: false,
    hopDistance: 2,
    status: node.ready ? 'online' : 'degraded',
    metrics: {
      timestamp: node.lastSeen,
      cpuPercent: 0,
      memoryUsedBytes: 0,
      memoryTotalBytes: 1,
      diskUsedBytes: 0,
      diskTotalBytes: 1,
    },
    metricsHistory: new RingBuffer(METRICS_HISTORY_CAPACITY),
    lastSeen: node.lastSeen,
    capabilities: ['inference', 'routing'],
    heatPercent: 0,
    modelName: shortCid,
  };
}
```

- [ ] **Step 2: Create zenoh-utils.test.ts**

```typescript
import { describe, it, expect } from 'vitest';
import { discoveredToNetworkNode } from './zenoh-utils';
import type { DiscoveredNode } from './zenoh-service';

describe('discoveredToNetworkNode', () => {
  const sample: DiscoveredNode = {
    nodeAddr: 'deadbeef01020304aabbccdd',
    modelCid: 'aabbccdd11223344eeff0011',
    ready: true,
    lastSeen: 1711500000,
  };

  it('sets address from nodeAddr', () => {
    const node = discoveredToNetworkNode(sample);
    expect(node.address).toBe('deadbeef01020304aabbccdd');
  });

  it('truncates displayName and adds (live) suffix', () => {
    const node = discoveredToNetworkNode(sample);
    expect(node.displayName).toBe('deadbeef (live)');
  });

  it('marks online when ready', () => {
    const node = discoveredToNetworkNode(sample);
    expect(node.status).toBe('online');
  });

  it('marks degraded when not ready', () => {
    const node = discoveredToNetworkNode({ ...sample, ready: false });
    expect(node.status).toBe('degraded');
  });

  it('has inference and routing capabilities', () => {
    const node = discoveredToNetworkNode(sample);
    expect(node.capabilities).toContain('inference');
    expect(node.capabilities).toContain('routing');
  });

  it('sets modelName from truncated CID', () => {
    const node = discoveredToNetworkNode(sample);
    expect(node.modelName).toBe('aabbccdd');
  });

  it('sets heatPercent to 0', () => {
    const node = discoveredToNetworkNode(sample);
    expect(node.heatPercent).toBe(0);
  });

  it('is not local', () => {
    const node = discoveredToNetworkNode(sample);
    expect(node.isLocal).toBe(false);
  });

  it('handles short address without truncation', () => {
    const node = discoveredToNetworkNode({ ...sample, nodeAddr: 'abcd' });
    expect(node.displayName).toBe('abcd (live)');
  });
});
```

- [ ] **Step 3: Verify**

Run: `npx vitest run src/lib/zenoh-utils.test.ts`

- [ ] **Step 4: Commit**

```bash
git add src/lib/zenoh-utils.ts src/lib/zenoh-utils.test.ts
git commit -m "feat: add discoveredToNetworkNode conversion utility"
```

---

### Task 2: Merge into NetworkApp

**Files:**
- Modify: `src/NetworkApp.svelte`

- [ ] **Step 1: Import and wire conversion**

In `src/NetworkApp.svelte`, add import:
```typescript
import { discoveredToNetworkNode } from './lib/zenoh-utils';
```

Update `syncZenohState()` to merge discovered nodes into the nodes array:

```typescript
  function syncZenohState() {
    if (zenohService) {
      zenohStatus = zenohService.connectionStatus;
      discoveredCount = zenohService.discoveredNodes.size;
      zenohError = zenohService.errorMessage;

      // Merge discovered nodes into the graph
      if (zenohService.connectionStatus === 'connected' && zenohService.discoveredNodes.size > 0) {
        const realAddresses = new Set<string>();
        const realNodes: NetworkNode[] = [];
        for (const discovered of zenohService.discoveredNodes.values()) {
          realNodes.push(discoveredToNetworkNode(discovered));
          realAddresses.add(discovered.nodeAddr);
        }
        // Combine: mock nodes (excluding any with same address as real) + real nodes
        const mockNodes = service.nodes
          .filter(n => !realAddresses.has(n.address))
          .map(n => ({ ...n }));
        nodes = [...mockNodes, ...realNodes];
      }
    }
  }
```

Also update the `onTick` handler to include real nodes when connected:

```typescript
  service.onTick = () => {
    // Start with mock nodes
    const mockNodes = service.nodes.map((n) => ({ ...n }));
    links = service.links.map((l) => ({ ...l }));

    // Merge real nodes if connected
    if (zenohService?.connectionStatus === 'connected' && zenohService.discoveredNodes.size > 0) {
      const realAddresses = new Set<string>();
      const realNodes: NetworkNode[] = [];
      for (const discovered of zenohService.discoveredNodes.values()) {
        realNodes.push(discoveredToNetworkNode(discovered));
        realAddresses.add(discovered.nodeAddr);
      }
      nodes = [...mockNodes.filter(n => !realAddresses.has(n.address)), ...realNodes];
    } else {
      nodes = mockNodes;
    }

    syncZenohState();
  };
```

- [ ] **Step 2: Verify all tests pass**

Run: `npx vitest run`

- [ ] **Step 3: Verify build**

Run: `npm run build`

- [ ] **Step 4: Commit**

```bash
git add src/NetworkApp.svelte
git commit -m "feat: merge discovered Zenoh nodes into D3 force graph"
```
