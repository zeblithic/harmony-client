# Network Viz Upgrade Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Upgrade the network visualization with node heat gradients, capability badges, and transport-typed links, all driven by an enhanced mock data service.

**Architecture:** Extend existing types with `NodeCapability`, `TransportType`, `heatPercent`, and new link fields. Replace `nodeHealthColor` with `heatToColor` for node fill. Add `badgePosition` and `linkDashPattern` utilities. Update canvas renderer and detail panels. Enhance `MockNetworkDataService` to generate realistic Harmony topology.

**Tech Stack:** Svelte 5 (runes), TypeScript, D3-force (canvas), vitest

**Spec:** `docs/specs/2026-03-27-network-viz-upgrade-design.md`

---

## Important Implementation Notes

- **Component test fixtures must be updated.** `__tests__/NodeDetail.test.ts` `makeTestNode()` and `__tests__/LinkDetail.test.ts` `makeTestLink()`/`makeNodes()` must include the new required fields (`capabilities`, `heatPercent`, `transportType`, `encrypted`) or TypeScript will reject them after Task 1.
- **`addDiscoveredNodes`** in `network-data-service.ts` calls `createNode` — must be updated when `createNode`'s signature changes in Task 4.
- **Timer tests:** `network-data-service.test.ts` uses `vi.useFakeTimers()`. Heat tests must use `vi.advanceTimersByTime(1000)`, not real `Promise`-based waiting.

## File Map

| File | Responsibility |
|------|---------------|
| `src/lib/network-types.ts` | Add `NodeCapability`, `TransportType`, new fields |
| `src/lib/graph-utils.ts` | Replace `nodeHealthColor` → `heatToColor`, add `badgePosition`, `linkDashPattern`, `CAPABILITY_COLORS` |
| `src/lib/graph-utils.test.ts` | Tests for new utils, update existing `nodeHealthColor` tests |
| `src/lib/network-data-service.ts` | Generate realistic topology with capabilities, transports, heat |
| `src/lib/network-data-service.test.ts` | Tests for new mock data fields |
| `src/lib/components/NetworkGraph.svelte` | Heat colors, badges, styled links in canvas renderer |
| `src/lib/components/NodeDetail.svelte` | Show capabilities + model name |
| `src/lib/components/LinkDetail.svelte` | Show transport type + encrypted status |
| `src/lib/components/__tests__/NodeDetail.test.ts` | Update `makeTestNode()` with new fields |
| `src/lib/components/__tests__/LinkDetail.test.ts` | Update `makeTestLink()`/`makeNodes()` with new fields |

---

### Task 1: Type Extensions

Add new types and fields to `network-types.ts`.

**Files:**
- Modify: `src/lib/network-types.ts`

- [ ] **Step 1: Add new type aliases and extend interfaces**

In `src/lib/network-types.ts`, add after the `InterfaceType` declaration (line 21):

```typescript
export type NodeCapability = 'inference' | 'storage' | 'routing' | 'compute';
export type TransportType = 'iroh' | 'reticulum' | 'zenoh' | 'rawlink' | 's3';
```

Add to `NetworkNode` interface (after `lastSeen`, line 31):

```typescript
  capabilities: NodeCapability[];
  heatPercent: number;
  modelName?: string;
```

Add to `NetworkLink` interface (after `utilizationHistory`, line 42):

```typescript
  transportType: TransportType;
  encrypted: boolean;
```

- [ ] **Step 2: Verify compilation**

Run: `npx vitest run`
Expected: Some test failures where mock data doesn't include new required fields. That's expected — we fix those in later tasks.

Actually, since `MockNetworkDataService` constructs these objects without the new fields, TypeScript will error. We need to add the fields to the mock too. But we do that in Task 4. For now, add temporary defaults in `createNode` and `createLink` to keep things compiling:

In `src/lib/network-data-service.ts`, in `createNode` (around line 67), add to the returned object:

```typescript
    capabilities: ['routing'] as NodeCapability[],
    heatPercent: 0,
```

In `createLink` (around line 90), add:

```typescript
    transportType: 'zenoh' as TransportType,
    encrypted: false,
```

Add the imports at the top of `network-data-service.ts`:

```typescript
import type {
  NetworkNode,
  NetworkLink,
  NodeMetrics,
  LinkSnapshot,
  InterfaceType,
  NodeStatus,
  NetworkDataService,
  NodeCapability,
  TransportType,
} from './network-types';
```

- [ ] **Step 3: Verify all tests pass**

Run: `npx vitest run`

- [ ] **Step 4: Commit**

```bash
git add src/lib/network-types.ts src/lib/network-data-service.ts
git commit -m "feat: add NodeCapability, TransportType, and heat fields to network types"
```

---

### Task 2: Graph Utilities — heatToColor, badgePosition, linkDashPattern

Replace `nodeHealthColor` and add new utility functions.

**Files:**
- Modify: `src/lib/graph-utils.ts`
- Modify: `src/lib/graph-utils.test.ts`

- [ ] **Step 1: Replace nodeHealthColor with heatToColor and add new utilities**

In `src/lib/graph-utils.ts`, replace the `nodeHealthColor` function (lines 3-12) and add the new functions. The full updated file should have:

```typescript
import type { NodeStatus, NodeCapability, TransportType } from './network-types';

export const CAPABILITY_COLORS: Record<NodeCapability, string> = {
  inference: '#5865f2',
  storage: '#57f287',
  routing: '#fee75c',
  compute: '#eb459e',
};

const CAPABILITY_INDEX: Record<NodeCapability, number> = {
  inference: 0,
  storage: 1,
  routing: 2,
  compute: 3,
};

function lerpColor(a: string, b: string, t: number): string {
  const ar = parseInt(a.slice(1, 3), 16);
  const ag = parseInt(a.slice(3, 5), 16);
  const ab = parseInt(a.slice(5, 7), 16);
  const br = parseInt(b.slice(1, 3), 16);
  const bg = parseInt(b.slice(3, 5), 16);
  const bb = parseInt(b.slice(5, 7), 16);
  const r = Math.round(ar + (br - ar) * t);
  const g = Math.round(ag + (bg - ag) * t);
  const bl = Math.round(ab + (bb - ab) * t);
  return `#${r.toString(16).padStart(2, '0')}${g.toString(16).padStart(2, '0')}${bl.toString(16).padStart(2, '0')}`;
}

export function heatToColor(percent: number, status: NodeStatus, isLocal: boolean): string {
  if (status === 'offline') return '#72767d';
  if (isLocal) return '#5865f2';
  const p = Math.max(0, Math.min(100, percent));
  if (p <= 50) {
    return lerpColor('#43b581', '#faa61a', p / 50);
  }
  return lerpColor('#faa61a', '#ed4245', (p - 50) / 50);
}

export function badgePosition(
  capability: NodeCapability,
  nodeRadius: number,
): { dx: number; dy: number } {
  const offset = nodeRadius * 0.7;
  const index = CAPABILITY_INDEX[capability];
  switch (index) {
    case 0: return { dx: offset, dy: -offset };   // top-right: inference
    case 1: return { dx: offset, dy: offset };     // bottom-right: storage
    case 2: return { dx: -offset, dy: offset };    // bottom-left: routing
    case 3: return { dx: -offset, dy: -offset };   // top-left: compute
    default: return { dx: offset, dy: -offset };
  }
}

export function linkDashPattern(transportType: TransportType): number[] {
  switch (transportType) {
    case 'iroh': return [];
    case 'reticulum': return [8, 4];
    case 'zenoh': return [2, 4];
    case 'rawlink': return [4, 8];
    case 's3': return [1, 3];
  }
}
```

Keep all existing functions (`linkUtilizationColor`, `linkWidth`, `nodeRadius`, `findNodeAtPoint`, `findLinkAtPoint`, `advanceParticle`) unchanged.

- [ ] **Step 2: Update tests**

In `src/lib/graph-utils.test.ts`, replace the `nodeHealthColor` describe block with `heatToColor` tests and add new test blocks:

Replace the `describe('nodeHealthColor', ...)` block (lines 12-35) with:

```typescript
describe('heatToColor', () => {
  it('returns green for 0% load', () => {
    expect(heatToColor(0, 'online', false)).toBe('#43b581');
  });

  it('returns amber for 50% load', () => {
    expect(heatToColor(50, 'online', false)).toBe('#faa61a');
  });

  it('returns red for 100% load', () => {
    expect(heatToColor(100, 'online', false)).toBe('#ed4245');
  });

  it('returns gray for offline regardless of heat', () => {
    expect(heatToColor(50, 'offline', false)).toBe('#72767d');
  });

  it('returns blurple for local node regardless of heat', () => {
    expect(heatToColor(80, 'online', true)).toBe('#5865f2');
  });

  it('clamps below 0', () => {
    expect(heatToColor(-10, 'online', false)).toBe('#43b581');
  });

  it('clamps above 100', () => {
    expect(heatToColor(150, 'online', false)).toBe('#ed4245');
  });

  it('interpolates between green and amber at 25%', () => {
    const color = heatToColor(25, 'online', false);
    // Should be somewhere between #43b581 and #faa61a
    expect(color).not.toBe('#43b581');
    expect(color).not.toBe('#faa61a');
    expect(color.startsWith('#')).toBe(true);
    expect(color.length).toBe(7);
  });
});
```

Also add:

```typescript
describe('badgePosition', () => {
  it('places inference at top-right', () => {
    const pos = badgePosition('inference', 10);
    expect(pos.dx).toBe(7);
    expect(pos.dy).toBe(-7);
  });

  it('places storage at bottom-right', () => {
    const pos = badgePosition('storage', 10);
    expect(pos.dx).toBe(7);
    expect(pos.dy).toBe(7);
  });

  it('places routing at bottom-left', () => {
    const pos = badgePosition('routing', 10);
    expect(pos.dx).toBe(-7);
    expect(pos.dy).toBe(7);
  });

  it('places compute at top-left', () => {
    const pos = badgePosition('compute', 10);
    expect(pos.dx).toBe(-7);
    expect(pos.dy).toBe(-7);
  });
});

describe('linkDashPattern', () => {
  it('returns empty array for iroh (solid)', () => {
    expect(linkDashPattern('iroh')).toEqual([]);
  });

  it('returns dashed pattern for reticulum', () => {
    expect(linkDashPattern('reticulum')).toEqual([8, 4]);
  });

  it('returns dotted pattern for zenoh', () => {
    expect(linkDashPattern('zenoh')).toEqual([2, 4]);
  });

  it('returns long-dash for rawlink', () => {
    expect(linkDashPattern('rawlink')).toEqual([4, 8]);
  });

  it('returns fine dotted for s3', () => {
    expect(linkDashPattern('s3')).toEqual([1, 3]);
  });
});

describe('CAPABILITY_COLORS', () => {
  it('has blurple for inference', () => {
    expect(CAPABILITY_COLORS.inference).toBe('#5865f2');
  });

  it('has green for storage', () => {
    expect(CAPABILITY_COLORS.storage).toBe('#57f287');
  });

  it('has yellow for routing', () => {
    expect(CAPABILITY_COLORS.routing).toBe('#fee75c');
  });

  it('has pink for compute', () => {
    expect(CAPABILITY_COLORS.compute).toBe('#eb459e');
  });
});
```

Update the import at top of test file:

```typescript
import {
  heatToColor,
  linkUtilizationColor,
  linkWidth,
  findNodeAtPoint,
  findLinkAtPoint,
  advanceParticle,
  nodeRadius,
  badgePosition,
  linkDashPattern,
  CAPABILITY_COLORS,
} from './graph-utils';
```

- [ ] **Step 3: Fix any remaining references to nodeHealthColor**

Search for `nodeHealthColor` in the codebase and update references. Key files:
- `src/lib/components/NodeDetail.svelte` (line 5, line 34) — update import and usage to `heatToColor`
- Any component tests referencing `nodeHealthColor`

In `NodeDetail.svelte`, change:
```typescript
import { nodeHealthColor, linkUtilizationColor } from '../graph-utils';
```
to:
```typescript
import { heatToColor, linkUtilizationColor } from '../graph-utils';
```

And change:
```typescript
let statusColor = $derived(nodeHealthColor(node.status, node.isLocal));
```
to:
```typescript
let statusColor = $derived(heatToColor(node.heatPercent, node.status, node.isLocal));
```

- [ ] **Step 4: Verify all tests pass**

Run: `npx vitest run`

- [ ] **Step 5: Commit**

```bash
git add src/lib/graph-utils.ts src/lib/graph-utils.test.ts src/lib/components/NodeDetail.svelte
git commit -m "feat: replace nodeHealthColor with heatToColor, add badge and link utilities"
```

---

### Task 3: Detail Panel Updates

Add capabilities and transport info to the detail panels.

**Files:**
- Modify: `src/lib/components/NodeDetail.svelte`
- Modify: `src/lib/components/LinkDetail.svelte`

- [ ] **Step 1: Add capabilities section to NodeDetail**

In `src/lib/components/NodeDetail.svelte`, add after the `meta-line` paragraph (after line 151), before the `{#if node.status === 'offline'}` block:

```svelte
  {#if node.capabilities.length > 0}
    <p class="meta-line">
      {node.capabilities.join(', ')}
      {#if node.modelName}
        · Model: {node.modelName}
      {/if}
    </p>
  {/if}
```

- [ ] **Step 2: Add transport info to LinkDetail**

In `src/lib/components/LinkDetail.svelte`, update the meta-line (line 64-67) to include transport type:

Replace:
```svelte
  <p class="meta-line">
    <span class="interface-type">{link.interfaceType.toUpperCase()}</span>
    · capacity: {formatBandwidth(link.capacityBps)}
  </p>
```

With:
```svelte
  <p class="meta-line">
    <span class="interface-type">{link.transportType}</span>
    ({link.interfaceType.toUpperCase()})
    · capacity: {formatBandwidth(link.capacityBps)}
    {#if link.encrypted}
      · <span class="encrypted-badge" title="Post-quantum encrypted">🔒</span>
    {/if}
  </p>
```

Add CSS for the encrypted badge in the `<style>` block:

```css
  .encrypted-badge {
    font-size: 12px;
  }
```

- [ ] **Step 3: Verify tests pass**

Run: `npx vitest run`

- [ ] **Step 4: Commit**

```bash
git add src/lib/components/NodeDetail.svelte src/lib/components/LinkDetail.svelte
git commit -m "feat: show capabilities and transport type in detail panels"
```

---

### Task 4: Mock Data Service — Realistic Topology

Update `MockNetworkDataService` to generate nodes with capabilities, heat dynamics, and transport-typed links.

**Files:**
- Modify: `src/lib/network-data-service.ts`
- Modify: `src/lib/network-data-service.test.ts`

- [ ] **Step 1: Update node and link creation**

Replace the temporary defaults from Task 1 with realistic generation logic in `network-data-service.ts`:

Update `createNode` to accept and assign capabilities:

```typescript
const MODEL_NAMES = ['qwen3-4b', 'qwen3-0.8b', 'qwen3-9b'];

function createNode(
  name: string,
  isLocal: boolean,
  hopDistance: number,
  capabilities: NodeCapability[],
  modelName?: string,
): NetworkNode {
  return {
    address: randomHexAddress(),
    displayName: name,
    isLocal,
    hopDistance,
    status: 'online',
    metrics: createInitialMetrics(),
    metricsHistory: new RingBuffer<NodeMetrics>(METRICS_HISTORY_CAPACITY),
    lastSeen: Date.now(),
    capabilities,
    heatPercent: 0,
    modelName,
  };
}
```

Update `createLink` to assign transport type based on source/target capabilities:

```typescript
function createLink(
  source: string,
  target: string,
  sourceNode: NetworkNode,
  targetNode: NetworkNode,
): NetworkLink {
  const interfaceType = pickRandom(INTERFACE_TYPES);
  const capacityMap: Record<InterfaceType, number> = {
    tcp: 100_000_000,
    udp: 100_000_000,
    serial: 115_200,
    i2p: 1_000_000,
    lora: 37_500,
    pipe: 1_000_000_000,
  };

  const bothInference = sourceNode.capabilities.includes('inference')
    && targetNode.capabilities.includes('inference');
  const neitherInference = !sourceNode.capabilities.includes('inference')
    && !targetNode.capabilities.includes('inference');

  let transportType: TransportType;
  let encrypted: boolean;
  if (bothInference) {
    transportType = 'iroh';
    encrypted = true;
  } else if (neitherInference) {
    transportType = 'reticulum';
    encrypted = false;
  } else {
    transportType = 'zenoh';
    encrypted = true;
  }

  return {
    id: `${source.slice(0, 8)}-${target.slice(0, 8)}`,
    source,
    target,
    interfaceType,
    capacityBps: capacityMap[interfaceType],
    utilizationPercent: randomBetween(5, 40),
    latencyMs: randomBetween(1, 200),
    utilizationHistory: new RingBuffer<LinkSnapshot>(METRICS_HISTORY_CAPACITY),
    transportType,
    encrypted,
  };
}
```

Update the constructor to generate capabilities per node:

```typescript
function randomCapabilities(): { capabilities: NodeCapability[]; modelName?: string } {
  const caps: NodeCapability[] = [];
  if (Math.random() < 0.8) caps.push('routing');
  if (Math.random() < 0.6) caps.push('storage');
  if (Math.random() < 0.4) caps.push('compute');
  let modelName: string | undefined;
  if (Math.random() < 0.3) {
    caps.push('inference');
    modelName = pickRandom(MODEL_NAMES);
  }
  if (caps.length === 0) caps.push('routing'); // at least one capability
  return { capabilities: caps, modelName };
}
```

Update constructor calls to use `randomCapabilities()` and pass nodes to `createLink`.

Update the `tick()` method to compute `heatPercent` each tick:

In the metrics update block (after `node.metrics = newMetrics;`), add:

```typescript
        const memPercent = (newMetrics.memoryUsedBytes / newMetrics.memoryTotalBytes) * 100;
        node.heatPercent = clamp(0.6 * newMetrics.cpuPercent + 0.4 * memPercent, 0, 100);
```

Make inference nodes run hotter by adjusting `createInitialMetrics` to accept a baseline:

```typescript
function createInitialMetrics(hotBaseline: boolean): NodeMetrics {
  const cpuBase = hotBaseline ? randomBetween(40, 60) : randomBetween(10, 30);
  // ... rest unchanged, just use cpuBase instead of randomBetween(10, 50)
}
```

- [ ] **Step 2: Update tests**

In `src/lib/network-data-service.test.ts`, add tests for the new fields:

```typescript
it('generates nodes with capabilities', () => {
  const service = new MockNetworkDataService();
  for (const node of service.nodes) {
    expect(node.capabilities.length).toBeGreaterThan(0);
    for (const cap of node.capabilities) {
      expect(['inference', 'storage', 'routing', 'compute']).toContain(cap);
    }
  }
});

it('assigns modelName only to inference nodes', () => {
  const service = new MockNetworkDataService();
  for (const node of service.nodes) {
    if (node.capabilities.includes('inference')) {
      expect(node.modelName).toBeDefined();
    } else {
      expect(node.modelName).toBeUndefined();
    }
  }
});

it('generates links with valid transport types', () => {
  const service = new MockNetworkDataService();
  for (const link of service.links) {
    expect(['iroh', 'reticulum', 'zenoh', 'rawlink', 's3']).toContain(link.transportType);
    expect(typeof link.encrypted).toBe('boolean');
  }
});

it('computes heatPercent after tick', () => {
  const service = new MockNetworkDataService();
  service.start();
  // Wait for one tick
  return new Promise<void>((resolve) => {
    service.onTick = () => {
      service.stop();
      const onlineNode = service.nodes.find(n => n.status !== 'offline');
      if (onlineNode) {
        expect(onlineNode.heatPercent).toBeGreaterThanOrEqual(0);
        expect(onlineNode.heatPercent).toBeLessThanOrEqual(100);
      }
      resolve();
    };
  });
});
```

- [ ] **Step 3: Verify all tests pass**

Run: `npx vitest run`

- [ ] **Step 4: Commit**

```bash
git add src/lib/network-data-service.ts src/lib/network-data-service.test.ts
git commit -m "feat: generate realistic Harmony topology with capabilities, transports, and heat"
```

---

### Task 5: NetworkGraph Canvas Rendering

Update the canvas renderer to use heat colors, draw capability badges, and style links by transport type.

**Files:**
- Modify: `src/lib/components/NetworkGraph.svelte`

- [ ] **Step 1: Update SimNode and SimLink interfaces**

Read the file first to find the exact `SimNode`/`SimLink` definitions and `syncData` method. Add the new fields to both interfaces and update `createSimNodes`/`createSimLinks` to copy them. Update `syncData` to sync `heatPercent`, `capabilities`, and `status` per tick.

- [ ] **Step 2: Update drawNodes to use heatToColor and draw badges**

In the `drawNodes` function, replace the `nodeHealthColor` call with `heatToColor(node.heatPercent, node.status, node.isLocal)`.

After drawing the node circle, add badge rendering:

```typescript
// Draw capability badges (only when zoomed in enough)
if (transform.k > 0.5) {
  const r = nodeRadius(node.hopDistance);
  for (const cap of node.capabilities) {
    const { dx, dy } = badgePosition(cap, r);
    ctx.beginPath();
    ctx.arc(node.x + dx, node.y + dy, 4, 0, Math.PI * 2);
    ctx.fillStyle = CAPABILITY_COLORS[cap];
    ctx.fill();
    ctx.strokeStyle = '#1e1f22';
    ctx.lineWidth = 1;
    ctx.stroke();
  }
}
```

- [ ] **Step 3: Update drawLinks to use dash patterns and encrypted glow**

In the `drawLinks` function, set `ctx.setLineDash(linkDashPattern(link.transportType))` before stroking each link. Add the encrypted glow:

```typescript
if (link.encrypted) {
  ctx.save();
  ctx.strokeStyle = 'rgba(255, 255, 255, 0.1)';
  ctx.lineWidth = linkWidth(link.utilizationPercent) * 2;
  ctx.setLineDash(linkDashPattern(link.transportType));
  ctx.stroke();
  ctx.restore();
}
```

Reset dash after drawing: `ctx.setLineDash([])`.

- [ ] **Step 4: Update imports**

Add imports for `heatToColor`, `badgePosition`, `linkDashPattern`, `CAPABILITY_COLORS` from `../graph-utils` and `NodeCapability`, `TransportType` from `../network-types`.

- [ ] **Step 5: Verify all tests pass**

Run: `npx vitest run`

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/NetworkGraph.svelte
git commit -m "feat: render heat gradient, capability badges, and transport-styled links on canvas"
```
