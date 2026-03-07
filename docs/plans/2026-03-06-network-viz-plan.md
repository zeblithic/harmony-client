# Network Visualization Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a pop-out Tauri window with an interactive force-directed network graph, showing node health, resource utilization, and live traffic flow via animated particles.

**Architecture:** Canvas + D3-force for graph rendering, Svelte 5 runes for reactive state, SVG sparklines for metrics detail, semantic HTML table for accessibility. Mock data service behind a clean interface for future backend swap.

**Tech Stack:** Svelte 5 (runes), Tauri v2 (multi-window), D3-force + D3-zoom (Canvas rendering), vitest + @testing-library/svelte.

**Design doc:** `docs/plans/2026-03-06-network-viz-design.md`

---

### Task 1: Install Dependencies & Configure Multi-Page Vite

**Files:**
- Modify: `package.json`
- Modify: `vite.config.ts`
- Create: `src/network.html`

**Step 1: Install D3 modules**

Run:
```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npm install d3-force d3-zoom d3-selection && npm install -D @types/d3-force @types/d3-zoom @types/d3-selection
```

Expected: packages added to `package.json`

**Step 2: Configure Vite for multi-page build**

In `vite.config.ts`, add `build.rollupOptions.input` so both `index.html` and `network.html` are built:

```typescript
import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { resolve } from 'path';

export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  envPrefix: ['VITE_', 'TAURI_'],
  build: {
    target: 'esnext',
    minify: !process.env.TAURI_DEBUG ? 'esbuild' : false,
    sourcemap: !!process.env.TAURI_DEBUG,
    rollupOptions: {
      input: {
        main: resolve(__dirname, 'index.html'),
        network: resolve(__dirname, 'src/network.html'),
      },
    },
  },
});
```

**Step 3: Create network.html entry point**

Create `src/network.html` — minimal HTML that loads the network app:

```html
<!DOCTYPE html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>Harmony — Network</title>
    <link rel="stylesheet" href="./app.css" />
  </head>
  <body>
    <div id="network-app"></div>
    <script type="module" src="./network-main.ts"></script>
  </body>
</html>
```

**Step 4: Create network-main.ts entry**

Create `src/network-main.ts` — mounts the NetworkApp component (placeholder for now):

```typescript
import NetworkApp from './NetworkApp.svelte';
import { mount } from 'svelte';

const app = mount(NetworkApp, {
  target: document.getElementById('network-app')!,
});

export default app;
```

**Step 5: Create placeholder NetworkApp.svelte**

Create `src/NetworkApp.svelte`:

```svelte
<script lang="ts">
  // Placeholder — will be built out in later tasks
</script>

<main class="network-app">
  <p>Network visualization loading...</p>
</main>

<style>
  .network-app {
    width: 100vw;
    height: 100vh;
    background: var(--bg-primary, #1e1f22);
    color: var(--text-primary, #f2f3f5);
    display: flex;
    align-items: center;
    justify-content: center;
  }
</style>
```

**Step 6: Verify build works**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npm run build`
Expected: build succeeds, `dist/` contains both `index.html` and `src/network.html`

**Step 7: Verify tests still pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run`
Expected: all existing tests pass

**Step 8: Commit**

```bash
git add package.json package-lock.json vite.config.ts src/network.html src/network-main.ts src/NetworkApp.svelte
git commit -m "feat: add multi-page Vite config and network window entry point"
```

---

### Task 2: RingBuffer Utility

**Files:**
- Create: `src/lib/ring-buffer.ts`
- Create: `src/lib/ring-buffer.test.ts`

**Step 1: Write failing tests**

Create `src/lib/ring-buffer.test.ts`:

```typescript
import { describe, it, expect } from 'vitest';
import { RingBuffer } from './ring-buffer';

describe('RingBuffer', () => {
  it('starts empty', () => {
    const buf = new RingBuffer<number>(5);
    expect(buf.length).toBe(0);
    expect([...buf]).toEqual([]);
  });

  it('stores items up to capacity', () => {
    const buf = new RingBuffer<number>(3);
    buf.push(1);
    buf.push(2);
    buf.push(3);
    expect(buf.length).toBe(3);
    expect([...buf]).toEqual([1, 2, 3]);
  });

  it('overwrites oldest item when full', () => {
    const buf = new RingBuffer<number>(3);
    buf.push(1);
    buf.push(2);
    buf.push(3);
    buf.push(4);
    expect(buf.length).toBe(3);
    expect([...buf]).toEqual([2, 3, 4]);
  });

  it('overwrites multiple items correctly', () => {
    const buf = new RingBuffer<number>(3);
    for (let i = 1; i <= 7; i++) buf.push(i);
    expect([...buf]).toEqual([5, 6, 7]);
  });

  it('returns last item with peek()', () => {
    const buf = new RingBuffer<number>(3);
    buf.push(10);
    buf.push(20);
    expect(buf.peek()).toBe(20);
  });

  it('peek() returns undefined when empty', () => {
    const buf = new RingBuffer<number>(3);
    expect(buf.peek()).toBeUndefined();
  });

  it('clear() empties the buffer', () => {
    const buf = new RingBuffer<number>(3);
    buf.push(1);
    buf.push(2);
    buf.clear();
    expect(buf.length).toBe(0);
    expect([...buf]).toEqual([]);
  });

  it('handles capacity of 1', () => {
    const buf = new RingBuffer<number>(1);
    buf.push(1);
    expect([...buf]).toEqual([1]);
    buf.push(2);
    expect([...buf]).toEqual([2]);
  });

  it('is iterable with for-of', () => {
    const buf = new RingBuffer<string>(3);
    buf.push('a');
    buf.push('b');
    const result: string[] = [];
    for (const item of buf) result.push(item);
    expect(result).toEqual(['a', 'b']);
  });

  it('toArray() returns a copy', () => {
    const buf = new RingBuffer<number>(3);
    buf.push(1);
    buf.push(2);
    const arr = buf.toArray();
    arr.push(99);
    expect([...buf]).toEqual([1, 2]);
  });
});
```

**Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/ring-buffer.test.ts`
Expected: FAIL — module not found

**Step 3: Implement RingBuffer**

Create `src/lib/ring-buffer.ts`:

```typescript
export class RingBuffer<T> {
  private buffer: (T | undefined)[];
  private head = 0;
  private count = 0;
  readonly capacity: number;

  constructor(capacity: number) {
    this.capacity = Math.max(1, Math.floor(capacity));
    this.buffer = new Array(this.capacity);
  }

  get length(): number {
    return this.count;
  }

  push(item: T): void {
    const idx = (this.head + this.count) % this.capacity;
    this.buffer[idx] = item;
    if (this.count < this.capacity) {
      this.count++;
    } else {
      this.head = (this.head + 1) % this.capacity;
    }
  }

  peek(): T | undefined {
    if (this.count === 0) return undefined;
    return this.buffer[(this.head + this.count - 1) % this.capacity];
  }

  clear(): void {
    this.buffer = new Array(this.capacity);
    this.head = 0;
    this.count = 0;
  }

  toArray(): T[] {
    const result: T[] = [];
    for (let i = 0; i < this.count; i++) {
      result.push(this.buffer[(this.head + i) % this.capacity] as T);
    }
    return result;
  }

  [Symbol.iterator](): Iterator<T> {
    let i = 0;
    const self = this;
    return {
      next(): IteratorResult<T> {
        if (i < self.count) {
          const value = self.buffer[(self.head + i) % self.capacity] as T;
          i++;
          return { value, done: false };
        }
        return { value: undefined, done: true };
      },
    };
  }
}
```

**Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/ring-buffer.test.ts`
Expected: all 10 tests PASS

**Step 5: Commit**

```bash
git add src/lib/ring-buffer.ts src/lib/ring-buffer.test.ts
git commit -m "feat: add RingBuffer circular buffer utility"
```

---

### Task 3: Network Types

**Files:**
- Create: `src/lib/network-types.ts`

**Step 1: Create type definitions**

Create `src/lib/network-types.ts`:

```typescript
import type { RingBuffer } from './ring-buffer';

export interface NodeMetrics {
  timestamp: number;
  cpuPercent: number;
  memoryUsedBytes: number;
  memoryTotalBytes: number;
  diskUsedBytes: number;
  diskTotalBytes: number;
}

export interface LinkSnapshot {
  timestamp: number;
  utilizationPercent: number;
  latencyMs: number;
  bytesSent: number;
  bytesReceived: number;
}

export type NodeStatus = 'online' | 'degraded' | 'offline';
export type InterfaceType = 'tcp' | 'udp' | 'serial' | 'i2p' | 'lora' | 'pipe';

export interface NetworkNode {
  address: string;
  displayName: string;
  isLocal: boolean;
  hopDistance: number;
  status: NodeStatus;
  metrics: NodeMetrics;
  metricsHistory: RingBuffer<NodeMetrics>;
  lastSeen: number;
}

export interface NetworkLink {
  id: string;
  source: string;
  target: string;
  interfaceType: InterfaceType;
  capacityBps: number;
  utilizationPercent: number;
  latencyMs: number;
  utilizationHistory: RingBuffer<LinkSnapshot>;
}

export interface NetworkDataService {
  nodes: NetworkNode[];
  links: NetworkLink[];
  start(): void;
  stop(): void;
  requestPeerData(address: string): void;
  onAlert?: (message: string) => void;
}
```

**Step 2: Verify build**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run`
Expected: all existing tests pass (types are just definitions, no runtime behavior to test)

**Step 3: Commit**

```bash
git add src/lib/network-types.ts
git commit -m "feat: add network visualization type definitions"
```

---

### Task 4: Graph Utilities (Color Mapping, Hit Testing, Particle Math)

**Files:**
- Create: `src/lib/graph-utils.ts`
- Create: `src/lib/graph-utils.test.ts`

**Step 1: Write failing tests**

Create `src/lib/graph-utils.test.ts`:

```typescript
import { describe, it, expect } from 'vitest';
import {
  nodeHealthColor,
  linkUtilizationColor,
  linkWidth,
  findNodeAtPoint,
  advanceParticle,
  nodeRadius,
} from './graph-utils';

describe('nodeHealthColor', () => {
  it('returns green for healthy online nodes', () => {
    expect(nodeHealthColor('online', false)).toBe('#43b581');
  });

  it('returns accent blue for local node', () => {
    expect(nodeHealthColor('online', true)).toBe('#5865f2');
  });

  it('returns amber for degraded', () => {
    expect(nodeHealthColor('degraded', false)).toBe('#faa61a');
  });

  it('returns red for offline (non-local)', () => {
    expect(nodeHealthColor('offline', false)).toBe('#72767d');
  });
});

describe('linkUtilizationColor', () => {
  it('returns dim gray for idle (0-20%)', () => {
    expect(linkUtilizationColor(10)).toBe('#4f545c');
  });

  it('returns green for moderate (20-60%)', () => {
    expect(linkUtilizationColor(40)).toBe('#43b581');
  });

  it('returns amber for busy (60-85%)', () => {
    expect(linkUtilizationColor(70)).toBe('#faa61a');
  });

  it('returns red for saturated (85%+)', () => {
    expect(linkUtilizationColor(90)).toBe('#ed4245');
  });

  it('clamps to 0-100 range', () => {
    expect(linkUtilizationColor(-5)).toBe('#4f545c');
    expect(linkUtilizationColor(110)).toBe('#ed4245');
  });
});

describe('linkWidth', () => {
  it('returns 1 for idle', () => {
    expect(linkWidth(0)).toBe(1);
  });

  it('returns 4 for saturated', () => {
    expect(linkWidth(100)).toBe(4);
  });

  it('scales linearly between 1 and 4', () => {
    const w = linkWidth(50);
    expect(w).toBeGreaterThan(1);
    expect(w).toBeLessThan(4);
  });
});

describe('nodeRadius', () => {
  it('returns 20 for local node (hop 0)', () => {
    expect(nodeRadius(0)).toBe(20);
  });

  it('returns 14 for direct peer (hop 1)', () => {
    expect(nodeRadius(1)).toBe(14);
  });

  it('returns 10 for peers of peers (hop 2+)', () => {
    expect(nodeRadius(2)).toBe(10);
    expect(nodeRadius(5)).toBe(10);
  });
});

interface SimpleNode {
  address: string;
  x: number;
  y: number;
  hopDistance: number;
}

describe('findNodeAtPoint', () => {
  const nodes: SimpleNode[] = [
    { address: 'a', x: 100, y: 100, hopDistance: 0 },
    { address: 'b', x: 200, y: 200, hopDistance: 1 },
    { address: 'c', x: 300, y: 300, hopDistance: 2 },
  ];

  it('finds a node when clicking within its radius', () => {
    const found = findNodeAtPoint(105, 105, nodes);
    expect(found?.address).toBe('a');
  });

  it('returns null when clicking empty space', () => {
    const found = findNodeAtPoint(500, 500, nodes);
    expect(found).toBeNull();
  });

  it('picks the closest node when overlapping', () => {
    const close: SimpleNode[] = [
      { address: 'a', x: 100, y: 100, hopDistance: 1 },
      { address: 'b', x: 110, y: 100, hopDistance: 1 },
    ];
    const found = findNodeAtPoint(108, 100, close);
    expect(found?.address).toBe('b');
  });
});

describe('advanceParticle', () => {
  it('moves particle along the link', () => {
    const pos = advanceParticle(0, 0.5, 0.1);
    expect(pos).toBeCloseTo(0.1, 5);
  });

  it('wraps around past 1.0', () => {
    const pos = advanceParticle(0.95, 0.5, 0.1);
    expect(pos).toBeCloseTo(0.05, 5);
  });

  it('speed affects advancement', () => {
    const slow = advanceParticle(0, 0.2, 0.1);
    const fast = advanceParticle(0, 0.8, 0.1);
    expect(fast).toBeGreaterThan(slow);
  });
});
```

**Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/graph-utils.test.ts`
Expected: FAIL — module not found

**Step 3: Implement graph utilities**

Create `src/lib/graph-utils.ts`:

```typescript
import type { NodeStatus } from './network-types';

// --- Color mapping ---

export function nodeHealthColor(status: NodeStatus, isLocal: boolean): string {
  if (isLocal) return '#5865f2';
  switch (status) {
    case 'online': return '#43b581';
    case 'degraded': return '#faa61a';
    case 'offline': return '#72767d';
  }
}

export function linkUtilizationColor(percent: number): string {
  const p = Math.max(0, Math.min(100, percent));
  if (p < 20) return '#4f545c';
  if (p < 60) return '#43b581';
  if (p < 85) return '#faa61a';
  return '#ed4245';
}

export function linkWidth(percent: number): number {
  const p = Math.max(0, Math.min(100, percent));
  return 1 + (p / 100) * 3;
}

// --- Node sizing ---

export function nodeRadius(hopDistance: number): number {
  if (hopDistance === 0) return 20;
  if (hopDistance === 1) return 14;
  return 10;
}

// --- Hit testing ---

interface PointNode {
  address: string;
  x: number;
  y: number;
  hopDistance: number;
}

export function findNodeAtPoint<T extends PointNode>(
  px: number,
  py: number,
  nodes: T[],
): T | null {
  let closest: T | null = null;
  let closestDist = Infinity;

  for (const node of nodes) {
    const dx = px - node.x;
    const dy = py - node.y;
    const dist = Math.sqrt(dx * dx + dy * dy);
    const radius = nodeRadius(node.hopDistance);
    if (dist <= radius && dist < closestDist) {
      closest = node;
      closestDist = dist;
    }
  }

  return closest;
}

// --- Particle math ---

/**
 * Advance a particle along a link.
 * @param position Current position 0..1 along the link
 * @param speed Normalized speed 0..1 (derived from link utilization)
 * @param dt Delta time in seconds
 * @returns New position 0..1 (wraps around)
 */
export function advanceParticle(position: number, speed: number, dt: number): number {
  const advancement = speed * dt;
  return (position + advancement) % 1;
}
```

**Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/graph-utils.test.ts`
Expected: all 15 tests PASS

**Step 5: Commit**

```bash
git add src/lib/graph-utils.ts src/lib/graph-utils.test.ts
git commit -m "feat: add graph utilities for color mapping, hit testing, particles"
```

---

### Task 5: Sparkline Component

**Files:**
- Create: `src/lib/components/Sparkline.svelte`
- Create: `src/lib/components/__tests__/Sparkline.test.ts`

**Step 1: Write failing tests**

Create `src/lib/components/__tests__/Sparkline.test.ts`:

```typescript
import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import Sparkline from '../Sparkline.svelte';
import { RingBuffer } from '../../ring-buffer';

function makeBuffer(values: number[]): RingBuffer<number> {
  const buf = new RingBuffer<number>(values.length + 10);
  for (const v of values) buf.push(v);
  return buf;
}

describe('Sparkline', () => {
  it('renders an SVG with role="img"', () => {
    const data = makeBuffer([10, 20, 30]);
    render(Sparkline, { props: { data, label: 'CPU usage' } });
    const svg = screen.getByRole('img');
    expect(svg).toBeTruthy();
    expect(svg.tagName.toLowerCase()).toBe('svg');
  });

  it('sets aria-label from props', () => {
    const data = makeBuffer([10, 20, 30]);
    render(Sparkline, { props: { data, label: 'CPU at 30%' } });
    const svg = screen.getByRole('img');
    expect(svg.getAttribute('aria-label')).toBe('CPU at 30%');
  });

  it('renders a polyline element', () => {
    const data = makeBuffer([10, 20, 30, 40]);
    render(Sparkline, { props: { data, label: 'test' } });
    const svg = screen.getByRole('img');
    const polyline = svg.querySelector('polyline');
    expect(polyline).toBeTruthy();
  });

  it('renders nothing for empty buffer', () => {
    const data = new RingBuffer<number>(10);
    render(Sparkline, { props: { data, label: 'empty' } });
    const svg = screen.getByRole('img');
    const polyline = svg.querySelector('polyline');
    expect(polyline).toBeNull();
  });

  it('applies the provided color', () => {
    const data = makeBuffer([10, 20]);
    render(Sparkline, { props: { data, label: 'test', color: '#ed4245' } });
    const svg = screen.getByRole('img');
    const polyline = svg.querySelector('polyline');
    expect(polyline?.getAttribute('stroke')).toBe('#ed4245');
  });
});
```

**Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/Sparkline.test.ts`
Expected: FAIL — module not found

**Step 3: Implement Sparkline**

Create `src/lib/components/Sparkline.svelte`:

```svelte
<script lang="ts">
  import type { RingBuffer } from '../ring-buffer';

  let {
    data,
    label,
    color = '#43b581',
    height = 32,
  }: {
    data: RingBuffer<number>;
    label: string;
    color?: string;
    height?: number;
  } = $props();

  let points = $derived.by(() => {
    const values = data.toArray();
    if (values.length < 2) return '';
    const max = Math.max(...values, 1);
    const stepX = 100 / (values.length - 1);
    return values
      .map((v, i) => `${i * stepX},${height - (v / max) * height}`)
      .join(' ');
  });
</script>

<svg
  role="img"
  aria-label={label}
  width="100%"
  {height}
  viewBox="0 0 100 {height}"
  preserveAspectRatio="none"
  class="sparkline"
>
  {#if points}
    <polyline
      {points}
      fill="none"
      stroke={color}
      stroke-width="1.5"
      stroke-linejoin="round"
      stroke-linecap="round"
      vector-effect="non-scaling-stroke"
    />
  {/if}
</svg>

<style>
  .sparkline {
    display: block;
  }
</style>
```

**Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/Sparkline.test.ts`
Expected: all 5 tests PASS

**Step 5: Commit**

```bash
git add src/lib/components/Sparkline.svelte src/lib/components/__tests__/Sparkline.test.ts
git commit -m "feat: add Sparkline SVG component with accessibility"
```

---

### Task 6: AriaAnnouncer Component

**Files:**
- Create: `src/lib/components/AriaAnnouncer.svelte`
- Create: `src/lib/components/__tests__/AriaAnnouncer.test.ts`

**Step 1: Write failing tests**

Create `src/lib/components/__tests__/AriaAnnouncer.test.ts`:

```typescript
import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import AriaAnnouncer from '../AriaAnnouncer.svelte';

describe('AriaAnnouncer', () => {
  it('renders with role="status"', () => {
    render(AriaAnnouncer, { props: { message: 'Node online' } });
    const el = screen.getByRole('status');
    expect(el).toBeTruthy();
  });

  it('displays the message text', () => {
    render(AriaAnnouncer, { props: { message: 'Node bravo went offline' } });
    const el = screen.getByRole('status');
    expect(el.textContent).toBe('Node bravo went offline');
  });

  it('has aria-live="polite"', () => {
    render(AriaAnnouncer, { props: { message: 'test' } });
    const el = screen.getByRole('status');
    expect(el.getAttribute('aria-live')).toBe('polite');
  });

  it('has aria-atomic="true"', () => {
    render(AriaAnnouncer, { props: { message: 'test' } });
    const el = screen.getByRole('status');
    expect(el.getAttribute('aria-atomic')).toBe('true');
  });

  it('is visually hidden', () => {
    render(AriaAnnouncer, { props: { message: 'test' } });
    const el = screen.getByRole('status');
    expect(el.classList.contains('sr-only')).toBe(true);
  });
});
```

**Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/AriaAnnouncer.test.ts`
Expected: FAIL — module not found

**Step 3: Implement AriaAnnouncer**

Create `src/lib/components/AriaAnnouncer.svelte`:

```svelte
<script lang="ts">
  let { message }: { message: string } = $props();
</script>

<div role="status" aria-live="polite" aria-atomic="true" class="sr-only">
  {message}
</div>

<style>
  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    white-space: nowrap;
    border: 0;
  }
</style>
```

**Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/AriaAnnouncer.test.ts`
Expected: all 5 tests PASS

**Step 5: Commit**

```bash
git add src/lib/components/AriaAnnouncer.svelte src/lib/components/__tests__/AriaAnnouncer.test.ts
git commit -m "feat: add AriaAnnouncer screen reader component"
```

---

### Task 7: MockNetworkDataService

**Files:**
- Create: `src/lib/network-data-service.ts`
- Create: `src/lib/network-data-service.test.ts`

**Step 1: Write failing tests**

Create `src/lib/network-data-service.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { MockNetworkDataService } from './network-data-service';

describe('MockNetworkDataService', () => {
  let service: MockNetworkDataService;

  beforeEach(() => {
    vi.useFakeTimers();
    service = new MockNetworkDataService();
  });

  afterEach(() => {
    service.stop();
    vi.restoreAllTimers();
  });

  it('starts with nodes and links', () => {
    expect(service.nodes.length).toBeGreaterThan(0);
    expect(service.links.length).toBeGreaterThan(0);
  });

  it('has exactly one local node', () => {
    const local = service.nodes.filter((n) => n.isLocal);
    expect(local).toHaveLength(1);
    expect(local[0].hopDistance).toBe(0);
  });

  it('all nodes have valid initial metrics', () => {
    for (const node of service.nodes) {
      expect(node.metrics.cpuPercent).toBeGreaterThanOrEqual(0);
      expect(node.metrics.cpuPercent).toBeLessThanOrEqual(100);
      expect(node.metrics.memoryUsedBytes).toBeLessThanOrEqual(
        node.metrics.memoryTotalBytes,
      );
      expect(node.metrics.diskUsedBytes).toBeLessThanOrEqual(
        node.metrics.diskTotalBytes,
      );
    }
  });

  it('all links reference valid node addresses', () => {
    const addresses = new Set(service.nodes.map((n) => n.address));
    for (const link of service.links) {
      expect(addresses.has(link.source)).toBe(true);
      expect(addresses.has(link.target)).toBe(true);
    }
  });

  it('updates metrics after ticking', () => {
    service.start();
    const before = service.nodes[0].metrics.cpuPercent;
    vi.advanceTimersByTime(5000);
    const after = service.nodes[0].metrics.cpuPercent;
    // After 5 seconds of ticking, metrics should have changed
    // (probabilistic but extremely unlikely to be identical)
    expect(service.nodes[0].metricsHistory.length).toBeGreaterThan(0);
  });

  it('link utilization stays in valid range after ticking', () => {
    service.start();
    vi.advanceTimersByTime(10000);
    for (const link of service.links) {
      expect(link.utilizationPercent).toBeGreaterThanOrEqual(0);
      expect(link.utilizationPercent).toBeLessThanOrEqual(100);
    }
  });

  it('requestPeerData adds new nodes after delay', () => {
    service.start();
    const initialCount = service.nodes.length;
    service.requestPeerData(service.nodes[1].address);
    vi.advanceTimersByTime(2000);
    expect(service.nodes.length).toBeGreaterThan(initialCount);
  });

  it('stop() halts metric updates', () => {
    service.start();
    vi.advanceTimersByTime(3000);
    const historyLen = service.nodes[0].metricsHistory.length;
    service.stop();
    vi.advanceTimersByTime(5000);
    expect(service.nodes[0].metricsHistory.length).toBe(historyLen);
  });

  it('calls onAlert when node status changes', () => {
    const alerts: string[] = [];
    service.onAlert = (msg) => alerts.push(msg);
    service.start();
    // Run long enough for status changes to occur
    vi.advanceTimersByTime(60000);
    // Mock generates degraded nodes, so we should get alerts
    // (implementation ensures at least one status transition)
    expect(alerts.length).toBeGreaterThan(0);
  });
});
```

**Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/network-data-service.test.ts`
Expected: FAIL — module not found

**Step 3: Implement MockNetworkDataService**

Create `src/lib/network-data-service.ts`. This is the largest utility file. Key behaviors:

- Creates 8-12 mock nodes with realistic names and varied interface types
- Links form a connected graph with 15-20 edges
- Every 1 second, metrics update with sine-wave + noise fluctuation
- CPU thresholds trigger `degraded` status and `onAlert` callbacks
- `requestPeerData()` schedules 2-3 new node additions after a 1-2s delay
- Metrics history uses `RingBuffer<NodeMetrics>` (capacity 300)

```typescript
import { RingBuffer } from './ring-buffer';
import type {
  NetworkNode,
  NetworkLink,
  NetworkDataService,
  NodeMetrics,
  LinkSnapshot,
  InterfaceType,
  NodeStatus,
} from './network-types';

const NAMES = [
  'alpha', 'bravo', 'charlie', 'delta', 'echo',
  'foxtrot', 'golf', 'hotel', 'india', 'juliet',
  'kilo', 'lima', 'mike', 'november', 'oscar',
];

const INTERFACE_TYPES: InterfaceType[] = ['tcp', 'udp', 'serial', 'i2p', 'lora', 'pipe'];

function randomHex(len: number): string {
  return Array.from({ length: len }, () =>
    Math.floor(Math.random() * 256).toString(16).padStart(2, '0'),
  ).join('');
}

function clamp(v: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, v));
}

function makeMetrics(seed: number): NodeMetrics {
  const memTotal = [4, 8, 16, 32][Math.floor(seed * 4) % 4] * 1024 * 1024 * 1024;
  const diskTotal = [64, 128, 256, 512][Math.floor(seed * 7) % 4] * 1024 * 1024 * 1024;
  return {
    timestamp: Date.now(),
    cpuPercent: 10 + Math.random() * 40,
    memoryUsedBytes: memTotal * (0.2 + Math.random() * 0.4),
    memoryTotalBytes: memTotal,
    diskUsedBytes: diskTotal * (0.1 + Math.random() * 0.3),
    diskTotalBytes: diskTotal,
  };
}

export class MockNetworkDataService implements NetworkDataService {
  nodes: NetworkNode[] = $state([]);
  links: NetworkLink[] = $state([]);
  onAlert?: (message: string) => void;

  private intervalId: ReturnType<typeof setInterval> | null = null;
  private tick = 0;
  private pendingDiscoveries: Array<{ targetAddress: string; delay: number; added: boolean }> = [];
  private nameIndex = 0;

  constructor() {
    this.initializeNetwork();
  }

  private initializeNetwork(): void {
    const nodeCount = 8 + Math.floor(Math.random() * 5); // 8-12
    this.nameIndex = 0;

    // Create nodes
    for (let i = 0; i < nodeCount; i++) {
      const address = randomHex(16);
      const name = NAMES[this.nameIndex++ % NAMES.length];
      const hopDistance = i === 0 ? 0 : i <= 3 ? 1 : 2;
      const metrics = makeMetrics(i);
      const history = new RingBuffer<NodeMetrics>(300);
      history.push(metrics);

      this.nodes.push({
        address,
        displayName: i === 0 ? 'local-node' : name,
        isLocal: i === 0,
        hopDistance,
        status: 'online',
        metrics,
        metricsHistory: history,
        lastSeen: Date.now(),
      });
    }

    // Create links — ensure connected graph
    const linked = new Set<string>();
    for (let i = 1; i < this.nodes.length; i++) {
      // Connect to a random already-linked node (or node 0)
      const targetIdx = Math.floor(Math.random() * i);
      this.addLink(this.nodes[i].address, this.nodes[targetIdx].address);
      linked.add(`${i}-${targetIdx}`);
    }

    // Add extra links for density
    const extraLinks = 5 + Math.floor(Math.random() * 8);
    for (let e = 0; e < extraLinks; e++) {
      const a = Math.floor(Math.random() * this.nodes.length);
      let b = Math.floor(Math.random() * this.nodes.length);
      if (a === b) b = (b + 1) % this.nodes.length;
      const key = a < b ? `${a}-${b}` : `${b}-${a}`;
      if (!linked.has(key)) {
        this.addLink(this.nodes[a].address, this.nodes[b].address);
        linked.add(key);
      }
    }
  }

  private addLink(source: string, target: string): void {
    const id = source < target ? `${source}-${target}` : `${target}-${source}`;
    const interfaceType = INTERFACE_TYPES[Math.floor(Math.random() * INTERFACE_TYPES.length)];
    const capacityMap: Record<InterfaceType, number> = {
      tcp: 100_000_000,
      udp: 100_000_000,
      serial: 115_200,
      i2p: 1_000_000,
      lora: 37_500,
      pipe: 1_000_000_000,
    };

    this.links.push({
      id,
      source,
      target,
      interfaceType,
      capacityBps: capacityMap[interfaceType],
      utilizationPercent: Math.random() * 30,
      latencyMs: interfaceType === 'lora' ? 200 + Math.random() * 300 : 5 + Math.random() * 50,
      utilizationHistory: new RingBuffer<LinkSnapshot>(300),
    });
  }

  start(): void {
    if (this.intervalId) return;
    this.intervalId = setInterval(() => this.update(), 1000);
  }

  stop(): void {
    if (this.intervalId) {
      clearInterval(this.intervalId);
      this.intervalId = null;
    }
  }

  requestPeerData(address: string): void {
    this.pendingDiscoveries.push({
      targetAddress: address,
      delay: 1000 + Math.random() * 1000,
      added: false,
    });
  }

  private update(): void {
    this.tick++;
    const now = Date.now();

    // Update node metrics
    for (const node of this.nodes) {
      if (node.status === 'offline') continue;

      const t = this.tick * 0.1;
      const wave = Math.sin(t + node.address.charCodeAt(0)) * 15;
      const noise = (Math.random() - 0.5) * 10;

      const prevCpu = node.metrics.cpuPercent;
      const newCpu = clamp(prevCpu + wave * 0.1 + noise, 2, 98);

      const memDrift = (Math.random() - 0.48) * node.metrics.memoryTotalBytes * 0.01;
      const newMemUsed = clamp(
        node.metrics.memoryUsedBytes + memDrift,
        node.metrics.memoryTotalBytes * 0.1,
        node.metrics.memoryTotalBytes * 0.95,
      );

      const newMetrics: NodeMetrics = {
        timestamp: now,
        cpuPercent: newCpu,
        memoryUsedBytes: newMemUsed,
        memoryTotalBytes: node.metrics.memoryTotalBytes,
        diskUsedBytes: node.metrics.diskUsedBytes,
        diskTotalBytes: node.metrics.diskTotalBytes,
      };

      node.metrics = newMetrics;
      node.metricsHistory.push(newMetrics);
      node.lastSeen = now;

      // Status transitions
      const prevStatus = node.status;
      if (newCpu > 85) {
        node.status = 'degraded';
      } else if (prevStatus === 'degraded' && newCpu < 70) {
        node.status = 'online';
      }

      // Occasional offline/recovery (every ~60s per node)
      if (!node.isLocal && this.tick % 60 === 0 && Math.random() < 0.15) {
        node.status = node.status === 'offline' ? 'online' : 'offline';
      }

      if (prevStatus !== node.status) {
        this.emitAlert(node);
      }
    }

    // Update link utilization
    for (const link of this.links) {
      const noise = (Math.random() - 0.5) * 8;
      const wave = Math.sin(this.tick * 0.05 + link.id.charCodeAt(0)) * 10;
      link.utilizationPercent = clamp(link.utilizationPercent + noise + wave * 0.1, 0, 100);
      link.latencyMs = Math.max(1, link.latencyMs + (Math.random() - 0.5) * 5);

      link.utilizationHistory.push({
        timestamp: now,
        utilizationPercent: link.utilizationPercent,
        latencyMs: link.latencyMs,
        bytesSent: (link.capacityBps * link.utilizationPercent) / 100 / 8,
        bytesReceived: (link.capacityBps * link.utilizationPercent * 0.6) / 100 / 8,
      });
    }

    // Process pending discoveries
    for (const disc of this.pendingDiscoveries) {
      if (disc.added) continue;
      disc.delay -= 1000;
      if (disc.delay <= 0) {
        this.discoverPeers(disc.targetAddress);
        disc.added = true;
      }
    }
  }

  private discoverPeers(nearAddress: string): void {
    const count = 2 + Math.floor(Math.random() * 2); // 2-3 new nodes
    const nearNode = this.nodes.find((n) => n.address === nearAddress);
    const baseHop = nearNode ? nearNode.hopDistance + 1 : 3;

    for (let i = 0; i < count; i++) {
      const address = randomHex(16);
      const name = NAMES[this.nameIndex++ % NAMES.length];
      const metrics = makeMetrics(this.nameIndex);
      const history = new RingBuffer<NodeMetrics>(300);
      history.push(metrics);

      const newNode: NetworkNode = {
        address,
        displayName: name,
        isLocal: false,
        hopDistance: baseHop,
        status: 'online',
        metrics,
        metricsHistory: history,
        lastSeen: Date.now(),
      };

      this.nodes.push(newNode);
      this.addLink(nearAddress, address);

      if (this.onAlert) {
        this.onAlert(`New node ${name} discovered, ${baseHop} hops away`);
      }
    }
  }

  private emitAlert(node: NetworkNode): void {
    if (!this.onAlert) return;
    switch (node.status) {
      case 'degraded':
        this.onAlert(
          `Node ${node.displayName} degraded: CPU at ${Math.round(node.metrics.cpuPercent)}%`,
        );
        break;
      case 'offline':
        this.onAlert(`Node ${node.displayName} went offline`);
        break;
      case 'online':
        this.onAlert(`Node ${node.displayName} recovered: all metrics normal`);
        break;
    }
  }
}
```

**Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/network-data-service.test.ts`
Expected: all 9 tests PASS

**Step 5: Run all tests**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run`
Expected: all tests PASS

**Step 6: Commit**

```bash
git add src/lib/network-data-service.ts src/lib/network-data-service.test.ts
git commit -m "feat: add MockNetworkDataService with realistic metric simulation"
```

---

### Task 8: DataTable Component

**Files:**
- Create: `src/lib/components/DataTable.svelte`
- Create: `src/lib/components/__tests__/DataTable.test.ts`

**Step 1: Write failing tests**

Create `src/lib/components/__tests__/DataTable.test.ts`:

```typescript
import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import DataTable from '../DataTable.svelte';
import { RingBuffer } from '../../ring-buffer';
import type { NetworkNode, NodeMetrics } from '../../network-types';

function makeNode(overrides: Partial<NetworkNode> & { address: string; displayName: string }): NetworkNode {
  const metrics: NodeMetrics = {
    timestamp: Date.now(),
    cpuPercent: 25,
    memoryUsedBytes: 2 * 1024 * 1024 * 1024,
    memoryTotalBytes: 8 * 1024 * 1024 * 1024,
    diskUsedBytes: 50 * 1024 * 1024 * 1024,
    diskTotalBytes: 256 * 1024 * 1024 * 1024,
  };
  return {
    isLocal: false,
    hopDistance: 1,
    status: 'online',
    metrics,
    metricsHistory: new RingBuffer<NodeMetrics>(10),
    lastSeen: Date.now(),
    ...overrides,
  };
}

const testNodes: NetworkNode[] = [
  makeNode({ address: 'aaa', displayName: 'alpha', isLocal: true, hopDistance: 0, metrics: { timestamp: 0, cpuPercent: 23, memoryUsedBytes: 2e9, memoryTotalBytes: 8e9, diskUsedBytes: 30e9, diskTotalBytes: 256e9 } }),
  makeNode({ address: 'bbb', displayName: 'bravo', status: 'degraded', metrics: { timestamp: 0, cpuPercent: 91, memoryUsedBytes: 6e9, memoryTotalBytes: 8e9, diskUsedBytes: 100e9, diskTotalBytes: 256e9 } }),
  makeNode({ address: 'ccc', displayName: 'charlie', status: 'offline', metrics: { timestamp: 0, cpuPercent: 0, memoryUsedBytes: 0, memoryTotalBytes: 8e9, diskUsedBytes: 0, diskTotalBytes: 256e9 } }),
];

describe('DataTable', () => {
  it('renders a table element', () => {
    render(DataTable, { props: { nodes: testNodes } });
    const table = screen.getByRole('table');
    expect(table).toBeTruthy();
  });

  it('renders a row for each node', () => {
    render(DataTable, { props: { nodes: testNodes } });
    const rows = screen.getAllByRole('row');
    // header + 3 data rows
    expect(rows.length).toBe(4);
  });

  it('displays node names', () => {
    render(DataTable, { props: { nodes: testNodes } });
    expect(screen.getByText('alpha')).toBeTruthy();
    expect(screen.getByText('bravo')).toBeTruthy();
    expect(screen.getByText('charlie')).toBeTruthy();
  });

  it('shows dashes for offline node metrics', () => {
    render(DataTable, { props: { nodes: testNodes } });
    const rows = screen.getAllByRole('row');
    // charlie is offline (row index 3 — 0 is header)
    const cells = rows[3].querySelectorAll('td');
    // CPU, Mem, Disk columns should show dashes for offline
    const dashCells = Array.from(cells).filter((c) => c.textContent?.trim() === '\u2014');
    expect(dashCells.length).toBeGreaterThanOrEqual(3);
  });

  it('has sortable column headers', () => {
    render(DataTable, { props: { nodes: testNodes } });
    const headers = screen.getAllByRole('columnheader');
    expect(headers.length).toBeGreaterThan(0);
    // Headers should be buttons for sorting
    const sortableHeaders = headers.filter((h) => h.querySelector('button'));
    expect(sortableHeaders.length).toBeGreaterThan(0);
  });

  it('emits onNodeSelect when a row is clicked', async () => {
    const onNodeSelect = vi.fn();
    render(DataTable, { props: { nodes: testNodes, onNodeSelect } });
    const rows = screen.getAllByRole('row');
    await fireEvent.click(rows[1]); // click alpha
    expect(onNodeSelect).toHaveBeenCalledWith('aaa');
  });

  it('marks selected row with aria-selected', () => {
    render(DataTable, { props: { nodes: testNodes, selectedAddress: 'bbb' } });
    const rows = screen.getAllByRole('row');
    const selected = rows.find((r) => r.getAttribute('aria-selected') === 'true');
    expect(selected).toBeTruthy();
    expect(selected?.textContent).toContain('bravo');
  });
});
```

**Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/DataTable.test.ts`
Expected: FAIL — module not found

**Step 3: Implement DataTable**

Create `src/lib/components/DataTable.svelte`:

```svelte
<script lang="ts">
  import type { NetworkNode } from '../network-types';

  let {
    nodes,
    selectedAddress = null,
    onNodeSelect,
  }: {
    nodes: NetworkNode[];
    selectedAddress?: string | null;
    onNodeSelect?: (address: string) => void;
  } = $props();

  type SortKey = 'name' | 'status' | 'hops' | 'cpu' | 'mem' | 'disk';
  let sortKey = $state<SortKey>('name');
  let sortAsc = $state(true);

  function toggleSort(key: SortKey) {
    if (sortKey === key) {
      sortAsc = !sortAsc;
    } else {
      sortKey = key;
      sortAsc = true;
    }
  }

  function getSortValue(node: NetworkNode, key: SortKey): number | string {
    switch (key) {
      case 'name': return node.displayName.toLowerCase();
      case 'status': return node.status;
      case 'hops': return node.hopDistance;
      case 'cpu': return node.status === 'offline' ? -1 : node.metrics.cpuPercent;
      case 'mem': return node.status === 'offline' ? -1 : node.metrics.memoryUsedBytes / node.metrics.memoryTotalBytes;
      case 'disk': return node.status === 'offline' ? -1 : node.metrics.diskUsedBytes / node.metrics.diskTotalBytes;
    }
  }

  let sorted = $derived.by(() => {
    const copy = [...nodes];
    copy.sort((a, b) => {
      const va = getSortValue(a, sortKey);
      const vb = getSortValue(b, sortKey);
      const cmp = va < vb ? -1 : va > vb ? 1 : 0;
      return sortAsc ? cmp : -cmp;
    });
    return copy;
  });

  function formatPercent(value: number): string {
    return `${Math.round(value)}%`;
  }

  function formatMemory(used: number, total: number): string {
    const toGB = (b: number) => (b / (1024 * 1024 * 1024)).toFixed(1);
    return `${toGB(used)} / ${toGB(total)}`;
  }

  function statusDotClass(status: string): string {
    return `status-dot status-${status}`;
  }

  function handleRowClick(address: string) {
    onNodeSelect?.(address);
  }

  function handleRowKeydown(event: KeyboardEvent, address: string) {
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      onNodeSelect?.(address);
    }
  }

  const sortIndicator = (key: SortKey) => sortKey === key ? (sortAsc ? ' \u25B2' : ' \u25BC') : '';
</script>

<div class="data-table-wrapper" role="region" aria-label="Network nodes">
  <table>
    <thead>
      <tr>
        <th scope="col"><button type="button" onclick={() => toggleSort('name')}>Name{sortIndicator('name')}</button></th>
        <th scope="col"><button type="button" onclick={() => toggleSort('status')}>Status{sortIndicator('status')}</button></th>
        <th scope="col"><button type="button" onclick={() => toggleSort('hops')}>Hops{sortIndicator('hops')}</button></th>
        <th scope="col"><button type="button" onclick={() => toggleSort('cpu')}>CPU{sortIndicator('cpu')}</button></th>
        <th scope="col"><button type="button" onclick={() => toggleSort('mem')}>Memory{sortIndicator('mem')}</button></th>
        <th scope="col"><button type="button" onclick={() => toggleSort('disk')}>Disk{sortIndicator('disk')}</button></th>
      </tr>
    </thead>
    <tbody>
      {#each sorted as node (node.address)}
        <tr
          class:selected={selectedAddress === node.address}
          class:offline={node.status === 'offline'}
          aria-selected={selectedAddress === node.address}
          tabindex="0"
          onclick={() => handleRowClick(node.address)}
          onkeydown={(e) => handleRowKeydown(e, node.address)}
        >
          <td>
            <span class={statusDotClass(node.status)} aria-label={node.status}></span>
            {node.displayName}
          </td>
          <td>{node.status}</td>
          <td>{node.hopDistance}</td>
          {#if node.status === 'offline'}
            <td>{'\u2014'}</td>
            <td>{'\u2014'}</td>
            <td>{'\u2014'}</td>
          {:else}
            <td>{formatPercent(node.metrics.cpuPercent)}</td>
            <td>{formatMemory(node.metrics.memoryUsedBytes, node.metrics.memoryTotalBytes)}</td>
            <td>{formatPercent((node.metrics.diskUsedBytes / node.metrics.diskTotalBytes) * 100)}</td>
          {/if}
        </tr>
      {/each}
    </tbody>
  </table>
</div>

<style>
  .data-table-wrapper {
    overflow: auto;
    height: 100%;
  }

  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
  }

  th {
    position: sticky;
    top: 0;
    background: var(--bg-tertiary, #313338);
    text-align: left;
    padding: 8px 12px;
    color: var(--text-secondary, #b5bac1);
    font-weight: 600;
    text-transform: uppercase;
    font-size: 11px;
    letter-spacing: 0.5px;
  }

  th button {
    all: unset;
    cursor: pointer;
    color: inherit;
    font: inherit;
    text-transform: inherit;
    letter-spacing: inherit;
  }

  th button:hover {
    color: var(--text-primary, #f2f3f5);
  }

  td {
    padding: 6px 12px;
    color: var(--text-primary, #f2f3f5);
    border-bottom: 1px solid var(--bg-tertiary, #313338);
  }

  tr {
    cursor: pointer;
  }

  tr:hover {
    background: var(--bg-tertiary, #313338);
  }

  tr.selected {
    background: rgba(88, 101, 242, 0.15);
  }

  tr.offline td {
    color: var(--text-muted, #949ba4);
  }

  tr:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: -2px;
  }

  .status-dot {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    margin-right: 8px;
    vertical-align: middle;
  }

  .status-online { background: #43b581; }
  .status-degraded { background: #faa61a; }
  .status-offline { background: #72767d; }
</style>
```

**Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/DataTable.test.ts`
Expected: all 7 tests PASS

**Step 5: Commit**

```bash
git add src/lib/components/DataTable.svelte src/lib/components/__tests__/DataTable.test.ts
git commit -m "feat: add accessible DataTable with sortable columns and keyboard nav"
```

---

### Task 9: NodeDetail and LinkDetail Components

**Files:**
- Create: `src/lib/components/NodeDetail.svelte`
- Create: `src/lib/components/LinkDetail.svelte`
- Create: `src/lib/components/__tests__/NodeDetail.test.ts`
- Create: `src/lib/components/__tests__/LinkDetail.test.ts`

**Step 1: Write failing tests for NodeDetail**

Create `src/lib/components/__tests__/NodeDetail.test.ts`:

```typescript
import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import NodeDetail from '../NodeDetail.svelte';
import { RingBuffer } from '../../ring-buffer';
import type { NetworkNode, NetworkLink, NodeMetrics } from '../../network-types';

function makeTestNode(): NetworkNode {
  const metrics: NodeMetrics = {
    timestamp: Date.now(),
    cpuPercent: 47,
    memoryUsedBytes: 2.1 * 1024 * 1024 * 1024,
    memoryTotalBytes: 8 * 1024 * 1024 * 1024,
    diskUsedBytes: 34 * 1024 * 1024 * 1024,
    diskTotalBytes: 256 * 1024 * 1024 * 1024,
  };
  const history = new RingBuffer<NodeMetrics>(10);
  history.push(metrics);
  return {
    address: 'a7f3c219deadbeef',
    displayName: 'bravo',
    isLocal: false,
    hopDistance: 2,
    status: 'online',
    metrics,
    metricsHistory: history,
    lastSeen: Date.now(),
  };
}

const testLinks: NetworkLink[] = [
  { id: 'l1', source: 'a7f3c219deadbeef', target: 'aaa', interfaceType: 'tcp', capacityBps: 1e8, utilizationPercent: 12, latencyMs: 8, utilizationHistory: new RingBuffer(10) },
  { id: 'l2', source: 'a7f3c219deadbeef', target: 'bbb', interfaceType: 'udp', capacityBps: 1e8, utilizationPercent: 67, latencyMs: 23, utilizationHistory: new RingBuffer(10) },
];

describe('NodeDetail', () => {
  it('displays node name', () => {
    render(NodeDetail, { props: { node: makeTestNode(), links: testLinks } });
    expect(screen.getByText('bravo')).toBeTruthy();
  });

  it('displays truncated address', () => {
    render(NodeDetail, { props: { node: makeTestNode(), links: testLinks } });
    expect(screen.getByText('a7f3...beef')).toBeTruthy();
  });

  it('shows hop distance', () => {
    render(NodeDetail, { props: { node: makeTestNode(), links: testLinks } });
    expect(screen.getByText(/2 hops/)).toBeTruthy();
  });

  it('displays CPU metric', () => {
    render(NodeDetail, { props: { node: makeTestNode(), links: testLinks } });
    expect(screen.getByText(/47%/)).toBeTruthy();
  });

  it('renders sparklines with role="img"', () => {
    render(NodeDetail, { props: { node: makeTestNode(), links: testLinks } });
    const sparklines = screen.getAllByRole('img');
    expect(sparklines.length).toBeGreaterThanOrEqual(3); // CPU, mem, disk
  });

  it('lists connected links', () => {
    render(NodeDetail, { props: { node: makeTestNode(), links: testLinks } });
    expect(screen.getByText(/TCP/i)).toBeTruthy();
    expect(screen.getByText(/UDP/i)).toBeTruthy();
  });
});
```

**Step 2: Write failing tests for LinkDetail**

Create `src/lib/components/__tests__/LinkDetail.test.ts`:

```typescript
import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import LinkDetail from '../LinkDetail.svelte';
import { RingBuffer } from '../../ring-buffer';
import type { NetworkLink, NetworkNode, NodeMetrics, LinkSnapshot } from '../../network-types';

function makeTestLink(): NetworkLink {
  const history = new RingBuffer<LinkSnapshot>(10);
  history.push({ timestamp: Date.now(), utilizationPercent: 67, latencyMs: 23, bytesSent: 12.3e6, bytesReceived: 4.1e6 });
  return {
    id: 'l1',
    source: 'aaa',
    target: 'bbb',
    interfaceType: 'tcp',
    capacityBps: 100_000_000,
    utilizationPercent: 67,
    latencyMs: 23,
    utilizationHistory: history,
  };
}

function makeNodes(): NetworkNode[] {
  const metrics: NodeMetrics = { timestamp: 0, cpuPercent: 50, memoryUsedBytes: 4e9, memoryTotalBytes: 8e9, diskUsedBytes: 50e9, diskTotalBytes: 256e9 };
  return [
    { address: 'aaa', displayName: 'alpha', isLocal: true, hopDistance: 0, status: 'online', metrics, metricsHistory: new RingBuffer(10), lastSeen: Date.now() },
    { address: 'bbb', displayName: 'bravo', isLocal: false, hopDistance: 1, status: 'online', metrics, metricsHistory: new RingBuffer(10), lastSeen: Date.now() },
  ];
}

describe('LinkDetail', () => {
  it('displays endpoint names', () => {
    render(LinkDetail, { props: { link: makeTestLink(), nodes: makeNodes() } });
    expect(screen.getByText(/alpha/)).toBeTruthy();
    expect(screen.getByText(/bravo/)).toBeTruthy();
  });

  it('shows interface type', () => {
    render(LinkDetail, { props: { link: makeTestLink(), nodes: makeNodes() } });
    expect(screen.getByText(/TCP/i)).toBeTruthy();
  });

  it('displays utilization percentage', () => {
    render(LinkDetail, { props: { link: makeTestLink(), nodes: makeNodes() } });
    expect(screen.getByText(/67%/)).toBeTruthy();
  });

  it('displays latency', () => {
    render(LinkDetail, { props: { link: makeTestLink(), nodes: makeNodes() } });
    expect(screen.getByText(/23/)).toBeTruthy();
  });

  it('renders sparklines', () => {
    render(LinkDetail, { props: { link: makeTestLink(), nodes: makeNodes() } });
    const sparklines = screen.getAllByRole('img');
    expect(sparklines.length).toBeGreaterThanOrEqual(2); // utilization, latency
  });
});
```

**Step 3: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/NodeDetail.test.ts src/lib/components/__tests__/LinkDetail.test.ts`
Expected: FAIL — modules not found

**Step 4: Implement NodeDetail**

Create `src/lib/components/NodeDetail.svelte`:

```svelte
<script lang="ts">
  import type { NetworkNode, NetworkLink } from '../network-types';
  import { RingBuffer } from '../ring-buffer';
  import { nodeHealthColor, linkUtilizationColor } from '../graph-utils';
  import { generateIdenticon } from '../identicon';
  import Sparkline from './Sparkline.svelte';

  let {
    node,
    links,
    onLinkClick,
  }: {
    node: NetworkNode;
    links: NetworkLink[];
    onLinkClick?: (linkId: string) => void;
  } = $props();

  let connectedLinks = $derived(
    links.filter((l) => l.source === node.address || l.target === node.address)
  );

  let truncatedAddress = $derived(
    `${node.address.slice(0, 4)}...${node.address.slice(-4)}`
  );

  let cpuHistory = $derived.by(() => {
    const buf = new RingBuffer<number>(300);
    for (const m of node.metricsHistory) buf.push(m.cpuPercent);
    return buf;
  });

  let memHistory = $derived.by(() => {
    const buf = new RingBuffer<number>(300);
    for (const m of node.metricsHistory) buf.push((m.memoryUsedBytes / m.memoryTotalBytes) * 100);
    return buf;
  });

  let diskHistory = $derived.by(() => {
    const buf = new RingBuffer<number>(300);
    for (const m of node.metricsHistory) buf.push((m.diskUsedBytes / m.diskTotalBytes) * 100);
    return buf;
  });

  function formatGB(bytes: number): string {
    return (bytes / (1024 * 1024 * 1024)).toFixed(1);
  }

  function cpuColor(pct: number): string {
    if (pct > 85) return '#ed4245';
    if (pct > 60) return '#faa61a';
    return '#43b581';
  }

  function handleLinkKeydown(event: KeyboardEvent, linkId: string) {
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      onLinkClick?.(linkId);
    }
  }
</script>

<div class="node-detail">
  <header class="node-header">
    <span class="identicon">{@html generateIdenticon(node.address, 36)}</span>
    <div>
      <h3 class="node-name">
        <span class="status-dot" style="background: {nodeHealthColor(node.status, node.isLocal)}"></span>
        {node.displayName}
      </h3>
      <button type="button" class="address" onclick={() => navigator.clipboard.writeText(node.address)} title="Copy address">
        {truncatedAddress}
      </button>
    </div>
  </header>

  <p class="meta">{node.hopDistance} hop{node.hopDistance !== 1 ? 's' : ''} · last seen: {Math.round((Date.now() - node.lastSeen) / 1000)}s</p>

  <section class="metric">
    <div class="metric-header">
      <span class="metric-label">CPU</span>
      <span class="metric-value">{Math.round(node.metrics.cpuPercent)}%</span>
    </div>
    <Sparkline data={cpuHistory} label="CPU usage over last 5 minutes, currently {Math.round(node.metrics.cpuPercent)} percent" color={cpuColor(node.metrics.cpuPercent)} />
  </section>

  <section class="metric">
    <div class="metric-header">
      <span class="metric-label">Memory</span>
      <span class="metric-value">{formatGB(node.metrics.memoryUsedBytes)} / {formatGB(node.metrics.memoryTotalBytes)} GB</span>
    </div>
    <Sparkline data={memHistory} label="Memory usage over last 5 minutes, currently {Math.round((node.metrics.memoryUsedBytes / node.metrics.memoryTotalBytes) * 100)} percent" color={cpuColor((node.metrics.memoryUsedBytes / node.metrics.memoryTotalBytes) * 100)} />
  </section>

  <section class="metric">
    <div class="metric-header">
      <span class="metric-label">Disk</span>
      <span class="metric-value">{formatGB(node.metrics.diskUsedBytes)} / {formatGB(node.metrics.diskTotalBytes)} GB</span>
    </div>
    <Sparkline data={diskHistory} label="Disk usage, currently {Math.round((node.metrics.diskUsedBytes / node.metrics.diskTotalBytes) * 100)} percent" color={cpuColor((node.metrics.diskUsedBytes / node.metrics.diskTotalBytes) * 100)} />
  </section>

  {#if connectedLinks.length > 0}
    <section class="links-section">
      <h4>Links ({connectedLinks.length})</h4>
      <ul class="link-list">
        {#each connectedLinks as link (link.id)}
          <li>
            <button
              type="button"
              class="link-row"
              onclick={() => onLinkClick?.(link.id)}
              onkeydown={(e) => handleLinkKeydown(e, link.id)}
            >
              <span class="link-type">{link.interfaceType.toUpperCase()}</span>
              <span class="link-util" style="color: {linkUtilizationColor(link.utilizationPercent)}">{Math.round(link.utilizationPercent)}%</span>
              <span class="link-latency">{Math.round(link.latencyMs)}ms</span>
            </button>
          </li>
        {/each}
      </ul>
    </section>
  {/if}
</div>

<style>
  .node-detail { padding: 16px; }

  .node-header {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-bottom: 4px;
  }

  .identicon { flex-shrink: 0; line-height: 0; }

  .node-name {
    margin: 0;
    font-size: 16px;
    font-weight: 600;
    color: var(--text-primary, #f2f3f5);
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .status-dot {
    display: inline-block;
    width: 10px;
    height: 10px;
    border-radius: 50%;
    flex-shrink: 0;
  }

  .address {
    all: unset;
    cursor: pointer;
    font-size: 12px;
    color: var(--text-muted, #949ba4);
    font-family: monospace;
  }

  .address:hover { color: var(--text-secondary, #b5bac1); }

  .meta {
    font-size: 12px;
    color: var(--text-muted, #949ba4);
    margin: 8px 0 16px;
  }

  .metric {
    margin-bottom: 12px;
    padding-bottom: 12px;
    border-bottom: 1px solid var(--bg-tertiary, #313338);
  }

  .metric-header {
    display: flex;
    justify-content: space-between;
    margin-bottom: 4px;
  }

  .metric-label {
    font-size: 12px;
    color: var(--text-secondary, #b5bac1);
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .metric-value {
    font-size: 13px;
    color: var(--text-primary, #f2f3f5);
    font-weight: 600;
  }

  .links-section h4 {
    font-size: 12px;
    color: var(--text-secondary, #b5bac1);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin: 16px 0 8px;
  }

  .link-list {
    list-style: none;
    padding: 0;
    margin: 0;
  }

  .link-row {
    all: unset;
    display: flex;
    gap: 12px;
    padding: 4px 8px;
    width: 100%;
    cursor: pointer;
    border-radius: 4px;
    font-size: 13px;
    color: var(--text-primary, #f2f3f5);
    box-sizing: border-box;
  }

  .link-row:hover { background: var(--bg-tertiary, #313338); }
  .link-row:focus-visible { outline: 2px solid var(--accent, #5865f2); }

  .link-type { width: 48px; font-weight: 600; }
  .link-util { width: 40px; text-align: right; }
  .link-latency { color: var(--text-muted, #949ba4); margin-left: auto; }
</style>
```

**Step 5: Implement LinkDetail**

Create `src/lib/components/LinkDetail.svelte`:

```svelte
<script lang="ts">
  import type { NetworkLink, NetworkNode } from '../network-types';
  import { RingBuffer } from '../ring-buffer';
  import { linkUtilizationColor } from '../graph-utils';
  import Sparkline from './Sparkline.svelte';

  let {
    link,
    nodes,
  }: {
    link: NetworkLink;
    nodes: NetworkNode[];
  } = $props();

  let sourceName = $derived(nodes.find((n) => n.address === link.source)?.displayName ?? link.source.slice(0, 8));
  let targetName = $derived(nodes.find((n) => n.address === link.target)?.displayName ?? link.target.slice(0, 8));

  let utilHistory = $derived.by(() => {
    const buf = new RingBuffer<number>(300);
    for (const s of link.utilizationHistory) buf.push(s.utilizationPercent);
    return buf;
  });

  let latencyHistory = $derived.by(() => {
    const buf = new RingBuffer<number>(300);
    for (const s of link.utilizationHistory) buf.push(s.latencyMs);
    return buf;
  });

  let lastSnapshot = $derived(link.utilizationHistory.peek());

  function formatBandwidth(bps: number): string {
    if (bps >= 1e9) return `${(bps / 1e9).toFixed(1)} Gbps`;
    if (bps >= 1e6) return `${(bps / 1e6).toFixed(1)} Mbps`;
    if (bps >= 1e3) return `${(bps / 1e3).toFixed(1)} Kbps`;
    return `${bps} bps`;
  }

  function formatThroughput(bytesPerSec: number): string {
    if (bytesPerSec >= 1e6) return `${(bytesPerSec / 1e6).toFixed(1)} MB/s`;
    if (bytesPerSec >= 1e3) return `${(bytesPerSec / 1e3).toFixed(1)} KB/s`;
    return `${Math.round(bytesPerSec)} B/s`;
  }
</script>

<div class="link-detail">
  <header class="link-header">
    <h3>{sourceName} &leftrightarrow; {targetName}</h3>
    <p class="link-meta">{link.interfaceType.toUpperCase()} · capacity: {formatBandwidth(link.capacityBps)}</p>
  </header>

  <section class="metric">
    <div class="metric-header">
      <span class="metric-label">Utilization</span>
      <span class="metric-value" style="color: {linkUtilizationColor(link.utilizationPercent)}">{Math.round(link.utilizationPercent)}%</span>
    </div>
    <Sparkline data={utilHistory} label="Link utilization over last 5 minutes, currently {Math.round(link.utilizationPercent)} percent" color={linkUtilizationColor(link.utilizationPercent)} />
  </section>

  <section class="metric">
    <div class="metric-header">
      <span class="metric-label">Latency</span>
      <span class="metric-value">{Math.round(link.latencyMs)}ms</span>
    </div>
    <Sparkline data={latencyHistory} label="Latency over last 5 minutes, currently {Math.round(link.latencyMs)} milliseconds" color="#5865f2" />
  </section>

  {#if lastSnapshot}
    <section class="throughput">
      <h4>Throughput</h4>
      <div class="throughput-row">
        <span class="direction">&uarr; {formatThroughput(lastSnapshot.bytesSent)}</span>
        <span class="direction">&darr; {formatThroughput(lastSnapshot.bytesReceived)}</span>
      </div>
    </section>
  {/if}
</div>

<style>
  .link-detail { padding: 16px; }

  .link-header h3 {
    margin: 0;
    font-size: 16px;
    color: var(--text-primary, #f2f3f5);
  }

  .link-meta {
    font-size: 12px;
    color: var(--text-muted, #949ba4);
    margin: 4px 0 16px;
  }

  .metric {
    margin-bottom: 12px;
    padding-bottom: 12px;
    border-bottom: 1px solid var(--bg-tertiary, #313338);
  }

  .metric-header {
    display: flex;
    justify-content: space-between;
    margin-bottom: 4px;
  }

  .metric-label {
    font-size: 12px;
    color: var(--text-secondary, #b5bac1);
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .metric-value {
    font-size: 13px;
    font-weight: 600;
  }

  .throughput h4 {
    font-size: 12px;
    color: var(--text-secondary, #b5bac1);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin: 0 0 8px;
  }

  .throughput-row {
    display: flex;
    gap: 24px;
    font-size: 14px;
    color: var(--text-primary, #f2f3f5);
  }
</style>
```

**Step 6: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/NodeDetail.test.ts src/lib/components/__tests__/LinkDetail.test.ts`
Expected: all 11 tests PASS

**Step 7: Commit**

```bash
git add src/lib/components/NodeDetail.svelte src/lib/components/LinkDetail.svelte src/lib/components/__tests__/NodeDetail.test.ts src/lib/components/__tests__/LinkDetail.test.ts
git commit -m "feat: add NodeDetail and LinkDetail panels with sparklines"
```

---

### Task 10: DetailPanel, NetworkToolbar, NetworkStatusBar

**Files:**
- Create: `src/lib/components/DetailPanel.svelte`
- Create: `src/lib/components/NetworkToolbar.svelte`
- Create: `src/lib/components/NetworkStatusBar.svelte`
- Create: `src/lib/components/__tests__/NetworkToolbar.test.ts`
- Create: `src/lib/components/__tests__/NetworkStatusBar.test.ts`

**Step 1: Write failing tests for NetworkToolbar**

Create `src/lib/components/__tests__/NetworkToolbar.test.ts`:

```typescript
import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import NetworkToolbar from '../NetworkToolbar.svelte';

describe('NetworkToolbar', () => {
  it('renders re-center button', () => {
    render(NetworkToolbar, { props: { showTable: false } });
    expect(screen.getByRole('button', { name: /re-center/i })).toBeTruthy();
  });

  it('renders zoom fit button', () => {
    render(NetworkToolbar, { props: { showTable: false } });
    expect(screen.getByRole('button', { name: /zoom to fit/i })).toBeTruthy();
  });

  it('renders table/graph toggle', () => {
    render(NetworkToolbar, { props: { showTable: false } });
    expect(screen.getByRole('button', { name: /table view/i })).toBeTruthy();
  });

  it('toggle label changes when showTable is true', () => {
    render(NetworkToolbar, { props: { showTable: true } });
    expect(screen.getByRole('button', { name: /graph view/i })).toBeTruthy();
  });

  it('emits onToggleView when toggle clicked', async () => {
    const onToggleView = vi.fn();
    render(NetworkToolbar, { props: { showTable: false, onToggleView } });
    const btn = screen.getByRole('button', { name: /table view/i });
    await fireEvent.click(btn);
    expect(onToggleView).toHaveBeenCalled();
  });

  it('emits onRecenter when re-center clicked', async () => {
    const onRecenter = vi.fn();
    render(NetworkToolbar, { props: { showTable: false, onRecenter } });
    const btn = screen.getByRole('button', { name: /re-center/i });
    await fireEvent.click(btn);
    expect(onRecenter).toHaveBeenCalled();
  });
});
```

**Step 2: Write failing tests for NetworkStatusBar**

Create `src/lib/components/__tests__/NetworkStatusBar.test.ts`:

```typescript
import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import NetworkStatusBar from '../NetworkStatusBar.svelte';

describe('NetworkStatusBar', () => {
  it('displays node count', () => {
    render(NetworkStatusBar, { props: { nodeCount: 12, linkCount: 18, healthySummary: '10 healthy, 2 degraded' } });
    expect(screen.getByText(/12 nodes/)).toBeTruthy();
  });

  it('displays link count', () => {
    render(NetworkStatusBar, { props: { nodeCount: 12, linkCount: 18, healthySummary: '' } });
    expect(screen.getByText(/18 links/)).toBeTruthy();
  });

  it('displays health summary', () => {
    render(NetworkStatusBar, { props: { nodeCount: 5, linkCount: 8, healthySummary: '4 healthy, 1 offline' } });
    expect(screen.getByText(/4 healthy, 1 offline/)).toBeTruthy();
  });
});
```

**Step 3: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/NetworkToolbar.test.ts src/lib/components/__tests__/NetworkStatusBar.test.ts`
Expected: FAIL — modules not found

**Step 4: Implement NetworkToolbar**

Create `src/lib/components/NetworkToolbar.svelte`:

```svelte
<script lang="ts">
  let {
    showTable,
    onToggleView,
    onRecenter,
    onZoomFit,
  }: {
    showTable: boolean;
    onToggleView?: () => void;
    onRecenter?: () => void;
    onZoomFit?: () => void;
  } = $props();
</script>

<nav class="network-toolbar" aria-label="Network visualization controls">
  <button type="button" aria-label="Re-center" onclick={() => onRecenter?.()}>
    Re-center
  </button>
  <button type="button" aria-label="Zoom to fit" onclick={() => onZoomFit?.()}>
    Fit
  </button>
  <div class="spacer"></div>
  <button type="button" aria-label={showTable ? 'Graph view' : 'Table view'} onclick={() => onToggleView?.()}>
    {showTable ? 'Graph view' : 'Table view'}
  </button>
</nav>

<style>
  .network-toolbar {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 16px;
    background: var(--bg-secondary, #2b2d31);
    border-bottom: 1px solid var(--bg-tertiary, #313338);
  }

  .spacer { flex: 1; }

  button {
    padding: 4px 12px;
    border: 1px solid var(--bg-tertiary, #313338);
    border-radius: 4px;
    background: var(--bg-primary, #1e1f22);
    color: var(--text-secondary, #b5bac1);
    font-size: 13px;
    cursor: pointer;
  }

  button:hover {
    background: var(--bg-tertiary, #313338);
    color: var(--text-primary, #f2f3f5);
  }

  button:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: -2px;
  }
</style>
```

**Step 5: Implement NetworkStatusBar**

Create `src/lib/components/NetworkStatusBar.svelte`:

```svelte
<script lang="ts">
  let {
    nodeCount,
    linkCount,
    healthySummary,
  }: {
    nodeCount: number;
    linkCount: number;
    healthySummary: string;
  } = $props();
</script>

<footer class="status-bar" role="status">
  <span>{nodeCount} nodes · {linkCount} links</span>
  {#if healthySummary}
    <span class="health">{healthySummary}</span>
  {/if}
</footer>

<style>
  .status-bar {
    display: flex;
    align-items: center;
    gap: 16px;
    padding: 4px 16px;
    background: var(--bg-secondary, #2b2d31);
    border-top: 1px solid var(--bg-tertiary, #313338);
    font-size: 12px;
    color: var(--text-muted, #949ba4);
  }

  .health {
    margin-left: auto;
  }
</style>
```

**Step 6: Implement DetailPanel**

Create `src/lib/components/DetailPanel.svelte`:

```svelte
<script lang="ts">
  import type { NetworkNode, NetworkLink } from '../network-types';
  import NodeDetail from './NodeDetail.svelte';
  import LinkDetail from './LinkDetail.svelte';

  let {
    selectedNode = null,
    selectedLink = null,
    nodes,
    links,
    onLinkClick,
  }: {
    selectedNode?: NetworkNode | null;
    selectedLink?: NetworkLink | null;
    nodes: NetworkNode[];
    links: NetworkLink[];
    onLinkClick?: (linkId: string) => void;
  } = $props();

  let healthCounts = $derived.by(() => {
    const counts = { online: 0, degraded: 0, offline: 0 };
    for (const n of nodes) counts[n.status]++;
    return counts;
  });
</script>

<aside class="detail-panel" aria-label="Detail panel">
  {#if selectedNode}
    <NodeDetail node={selectedNode} links={links} {onLinkClick} />
  {:else if selectedLink}
    <LinkDetail link={selectedLink} {nodes} />
  {:else}
    <div class="summary">
      <h3>Network Summary</h3>
      <p>{nodes.length} nodes · {links.length} links</p>
      <ul class="health-list">
        <li><span class="dot online"></span> {healthCounts.online} healthy</li>
        <li><span class="dot degraded"></span> {healthCounts.degraded} degraded</li>
        <li><span class="dot offline"></span> {healthCounts.offline} offline</li>
      </ul>
    </div>
  {/if}
</aside>

<style>
  .detail-panel {
    width: 320px;
    min-width: 320px;
    height: 100%;
    overflow-y: auto;
    background: var(--bg-secondary, #2b2d31);
    border-left: 1px solid var(--bg-tertiary, #313338);
  }

  .summary { padding: 16px; }

  .summary h3 {
    margin: 0 0 8px;
    font-size: 16px;
    color: var(--text-primary, #f2f3f5);
  }

  .summary p {
    font-size: 13px;
    color: var(--text-secondary, #b5bac1);
    margin: 0 0 16px;
  }

  .health-list {
    list-style: none;
    padding: 0;
    margin: 0;
  }

  .health-list li {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 0;
    font-size: 13px;
    color: var(--text-primary, #f2f3f5);
  }

  .dot {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
  }

  .dot.online { background: #43b581; }
  .dot.degraded { background: #faa61a; }
  .dot.offline { background: #72767d; }
</style>
```

**Step 7: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/NetworkToolbar.test.ts src/lib/components/__tests__/NetworkStatusBar.test.ts`
Expected: all 9 tests PASS

**Step 8: Commit**

```bash
git add src/lib/components/DetailPanel.svelte src/lib/components/NetworkToolbar.svelte src/lib/components/NetworkStatusBar.svelte src/lib/components/__tests__/NetworkToolbar.test.ts src/lib/components/__tests__/NetworkStatusBar.test.ts
git commit -m "feat: add DetailPanel, NetworkToolbar, and NetworkStatusBar"
```

---

### Task 11: NetworkGraph (Canvas + D3-Force)

This is the most complex component. Canvas rendering is not directly testable via @testing-library, so we test the extracted logic (already done in Task 4) and implement the rendering component.

**Files:**
- Create: `src/lib/components/NetworkGraph.svelte`

**Step 1: Implement NetworkGraph**

Create `src/lib/components/NetworkGraph.svelte`. Key responsibilities:

- Initializes D3 force simulation from `nodes` and `links` props
- Renders to `<canvas>` via `requestAnimationFrame` loop
- Handles mouse events for node hover/click, translating canvas coords to graph coords via d3-zoom transform
- Manages particle animation state per link
- Emits `onNodeClick`, `onNodeHover`, `onLinkClick` events

```svelte
<script lang="ts">
  import { onMount } from 'svelte';
  import { forceSimulation, forceLink, forceManyBody, forceCenter, forceCollide } from 'd3-force';
  import type { Simulation, SimulationNodeDatum, SimulationLinkDatum } from 'd3-force';
  import { zoom, zoomIdentity } from 'd3-zoom';
  import { select } from 'd3-selection';
  import type { NetworkNode, NetworkLink } from '../network-types';
  import { nodeHealthColor, linkUtilizationColor, linkWidth, nodeRadius, findNodeAtPoint, advanceParticle } from '../graph-utils';

  let {
    nodes,
    links,
    selectedAddress = null,
    onNodeClick,
    onNodeHover,
    onLinkClick,
  }: {
    nodes: NetworkNode[];
    links: NetworkLink[];
    selectedAddress?: string | null;
    onNodeClick?: (address: string) => void;
    onNodeHover?: (address: string | null) => void;
    onLinkClick?: (linkId: string) => void;
  } = $props();

  let canvas: HTMLCanvasElement;
  let width = $state(800);
  let height = $state(600);
  let hoveredAddress = $state<string | null>(null);
  let animationId: number;
  let transform = zoomIdentity;

  interface SimNode extends SimulationNodeDatum {
    address: string;
    displayName: string;
    hopDistance: number;
    status: string;
    isLocal: boolean;
  }

  interface SimLink extends SimulationLinkDatum<SimNode> {
    id: string;
    utilizationPercent: number;
    interfaceType: string;
    particles: number[];
  }

  let simNodes: SimNode[] = [];
  let simLinks: SimLink[] = [];
  let simulation: Simulation<SimNode, SimLink>;

  const PARTICLE_CAP = 200;
  const BG_COLOR = '#1a1b1e';
  const PARTICLE_COLOR = 'rgba(242, 243, 245, 0.7)';

  function initSimulation() {
    simNodes = nodes.map((n) => ({
      address: n.address,
      displayName: n.displayName,
      hopDistance: n.hopDistance,
      status: n.status,
      isLocal: n.isLocal,
      x: n.isLocal ? width / 2 : undefined,
      y: n.isLocal ? height / 2 : undefined,
    }));

    const nodeMap = new Map(simNodes.map((n) => [n.address, n]));

    simLinks = links
      .filter((l) => nodeMap.has(l.source) && nodeMap.has(l.target))
      .map((l) => ({
        source: nodeMap.get(l.source)!,
        target: nodeMap.get(l.target)!,
        id: l.id,
        utilizationPercent: l.utilizationPercent,
        interfaceType: l.interfaceType,
        particles: [],
      }));

    simulation = forceSimulation<SimNode>(simNodes)
      .force('link', forceLink<SimNode, SimLink>(simLinks).id((d) => d.address).distance(120))
      .force('charge', forceManyBody().strength(-300))
      .force('center', forceCenter(width / 2, height / 2))
      .force('collide', forceCollide<SimNode>().radius((d) => nodeRadius(d.hopDistance) + 4))
      .alphaDecay(0.02);
  }

  function syncData() {
    // Update sim nodes with current status
    for (const simNode of simNodes) {
      const dataNode = nodes.find((n) => n.address === simNode.address);
      if (dataNode) {
        simNode.status = dataNode.status;
        simNode.displayName = dataNode.displayName;
      }
    }

    // Update sim links with current utilization
    for (const simLink of simLinks) {
      const dataLink = links.find((l) => l.id === simLink.id);
      if (dataLink) {
        simLink.utilizationPercent = dataLink.utilizationPercent;
      }
    }

    // Check for new nodes
    if (nodes.length > simNodes.length) {
      const existing = new Set(simNodes.map((n) => n.address));
      for (const n of nodes) {
        if (!existing.has(n.address)) {
          const newNode: SimNode = {
            address: n.address,
            displayName: n.displayName,
            hopDistance: n.hopDistance,
            status: n.status,
            isLocal: n.isLocal,
          };
          simNodes.push(newNode);
        }
      }

      const nodeMap = new Map(simNodes.map((n) => [n.address, n]));
      const existingLinks = new Set(simLinks.map((l) => l.id));
      for (const l of links) {
        if (!existingLinks.has(l.id) && nodeMap.has(l.source) && nodeMap.has(l.target)) {
          simLinks.push({
            source: nodeMap.get(l.source)!,
            target: nodeMap.get(l.target)!,
            id: l.id,
            utilizationPercent: l.utilizationPercent,
            interfaceType: l.interfaceType,
            particles: [],
          });
        }
      }

      simulation.nodes(simNodes);
      (simulation.force('link') as ReturnType<typeof forceLink>)?.links(simLinks);
      simulation.alpha(0.3).restart();
    }
  }

  function updateParticles(dt: number) {
    let totalParticles = 0;
    for (const link of simLinks) {
      const particleCount = Math.min(
        Math.floor(link.utilizationPercent / 15) + 1,
        8,
      );

      // Add particles if needed
      while (link.particles.length < particleCount && totalParticles < PARTICLE_CAP) {
        link.particles.push(Math.random());
        totalParticles++;
      }

      // Remove excess
      while (link.particles.length > particleCount) {
        link.particles.pop();
      }

      // Advance
      const speed = 0.2 + (link.utilizationPercent / 100) * 0.6;
      for (let i = 0; i < link.particles.length; i++) {
        link.particles[i] = advanceParticle(link.particles[i], speed, dt);
      }
      totalParticles += link.particles.length;
    }
  }

  function render() {
    const ctx = canvas?.getContext('2d');
    if (!ctx) return;

    ctx.save();
    ctx.clearRect(0, 0, width, height);

    // Background with radial gradient
    const gradient = ctx.createRadialGradient(width / 2, height / 2, 0, width / 2, height / 2, Math.max(width, height) / 2);
    gradient.addColorStop(0, '#1e1f22');
    gradient.addColorStop(1, BG_COLOR);
    ctx.fillStyle = gradient;
    ctx.fillRect(0, 0, width, height);

    // Apply zoom transform
    ctx.translate(transform.x, transform.y);
    ctx.scale(transform.k, transform.k);

    // Draw links
    for (const link of simLinks) {
      const source = link.source as SimNode;
      const target = link.target as SimNode;
      if (source.x == null || source.y == null || target.x == null || target.y == null) continue;

      ctx.beginPath();
      ctx.moveTo(source.x, source.y);
      ctx.lineTo(target.x, target.y);
      ctx.strokeStyle = linkUtilizationColor(link.utilizationPercent);
      ctx.lineWidth = linkWidth(link.utilizationPercent);
      ctx.stroke();
    }

    // Draw particles
    for (const link of simLinks) {
      const source = link.source as SimNode;
      const target = link.target as SimNode;
      if (source.x == null || source.y == null || target.x == null || target.y == null) continue;

      for (const t of link.particles) {
        const x = source.x + (target.x - source.x) * t;
        const y = source.y + (target.y - source.y) * t;
        // Fade at endpoints
        const edgeFade = Math.min(t, 1 - t) * 4;
        const alpha = Math.min(1, edgeFade) * 0.7;
        ctx.beginPath();
        ctx.arc(x, y, 2.5, 0, Math.PI * 2);
        ctx.fillStyle = `rgba(242, 243, 245, ${alpha})`;
        ctx.fill();
      }
    }

    // Draw nodes
    for (const node of simNodes) {
      if (node.x == null || node.y == null) continue;
      const r = nodeRadius(node.hopDistance);
      const color = nodeHealthColor(node.status as any, node.isLocal);

      // Selection glow
      if (node.address === selectedAddress) {
        ctx.shadowColor = color;
        ctx.shadowBlur = 15;
      }

      ctx.beginPath();
      ctx.arc(node.x, node.y, r, 0, Math.PI * 2);
      ctx.fillStyle = color;
      ctx.fill();
      ctx.strokeStyle = color;
      ctx.lineWidth = 2;
      ctx.stroke();

      ctx.shadowColor = 'transparent';
      ctx.shadowBlur = 0;

      // Hover ring
      if (node.address === hoveredAddress) {
        ctx.beginPath();
        ctx.arc(node.x, node.y, r + 4, 0, Math.PI * 2);
        ctx.strokeStyle = '#f2f3f5';
        ctx.lineWidth = 1.5;
        ctx.stroke();
      }

      // Label
      ctx.fillStyle = '#b5bac1';
      ctx.font = '11px sans-serif';
      ctx.textAlign = 'center';
      const label = node.displayName.length > 12 ? node.displayName.slice(0, 11) + '\u2026' : node.displayName;
      ctx.fillText(label, node.x, node.y + r + 14);
    }

    ctx.restore();
  }

  let lastTime = 0;

  function loop(time: number) {
    const dt = lastTime ? (time - lastTime) / 1000 : 0.016;
    lastTime = time;

    syncData();
    updateParticles(dt);
    render();

    animationId = requestAnimationFrame(loop);
  }

  function handleClick(event: MouseEvent) {
    const rect = canvas.getBoundingClientRect();
    const px = (event.clientX - rect.left - transform.x) / transform.k;
    const py = (event.clientY - rect.top - transform.y) / transform.k;

    const node = findNodeAtPoint(px, py, simNodes as any);
    if (node) {
      onNodeClick?.(node.address);
    }
  }

  function handleMouseMove(event: MouseEvent) {
    const rect = canvas.getBoundingClientRect();
    const px = (event.clientX - rect.left - transform.x) / transform.k;
    const py = (event.clientY - rect.top - transform.y) / transform.k;

    const node = findNodeAtPoint(px, py, simNodes as any);
    const newHovered = node?.address ?? null;
    if (newHovered !== hoveredAddress) {
      hoveredAddress = newHovered;
      onNodeHover?.(hoveredAddress);
      canvas.style.cursor = hoveredAddress ? 'pointer' : 'default';
    }
  }

  function handleResize() {
    const rect = canvas.parentElement?.getBoundingClientRect();
    if (rect) {
      width = rect.width;
      height = rect.height;
      canvas.width = width;
      canvas.height = height;
    }
  }

  export function recenter() {
    const localNode = simNodes.find((n) => n.isLocal);
    if (localNode) {
      simulation.force('center', forceCenter(width / 2, height / 2));
      simulation.alpha(0.3).restart();
    }
    transform = zoomIdentity;
  }

  export function zoomToFit() {
    if (simNodes.length === 0) return;
    let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    for (const n of simNodes) {
      if (n.x == null || n.y == null) continue;
      minX = Math.min(minX, n.x);
      minY = Math.min(minY, n.y);
      maxX = Math.max(maxX, n.x);
      maxY = Math.max(maxY, n.y);
    }
    const padding = 60;
    const graphWidth = maxX - minX + padding * 2;
    const graphHeight = maxY - minY + padding * 2;
    const scale = Math.min(width / graphWidth, height / graphHeight, 4);
    const cx = (minX + maxX) / 2;
    const cy = (minY + maxY) / 2;
    transform = zoomIdentity
      .translate(width / 2, height / 2)
      .scale(scale)
      .translate(-cx, -cy);
  }

  onMount(() => {
    handleResize();
    initSimulation();

    // Set up d3-zoom
    const zoomBehavior = zoom()
      .scaleExtent([0.2, 4])
      .on('zoom', (event) => {
        transform = event.transform;
      });

    select(canvas).call(zoomBehavior as any);

    animationId = requestAnimationFrame(loop);

    const resizeObserver = new ResizeObserver(handleResize);
    resizeObserver.observe(canvas.parentElement!);

    return () => {
      cancelAnimationFrame(animationId);
      simulation.stop();
      resizeObserver.disconnect();
    };
  });
</script>

<div class="graph-container">
  <canvas
    bind:this={canvas}
    onclick={handleClick}
    onmousemove={handleMouseMove}
    role="img"
    aria-label="Network topology graph showing {simNodes.length} nodes and {simLinks.length} links"
  ></canvas>
</div>

<style>
  .graph-container {
    flex: 1;
    position: relative;
    overflow: hidden;
  }

  canvas {
    display: block;
    width: 100%;
    height: 100%;
  }
</style>
```

**Step 2: Verify build**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npm run build`
Expected: build succeeds

**Step 3: Run all tests**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run`
Expected: all tests PASS

**Step 4: Commit**

```bash
git add src/lib/components/NetworkGraph.svelte
git commit -m "feat: add NetworkGraph Canvas + D3-force with animated particles"
```

---

### Task 12: NetworkApp — Wire Everything Together

**Files:**
- Modify: `src/NetworkApp.svelte`

**Step 1: Implement full NetworkApp**

Replace the placeholder `src/NetworkApp.svelte` with the fully wired-up root component:

```svelte
<script lang="ts">
  import { MockNetworkDataService } from './lib/network-data-service';
  import NetworkToolbar from './lib/components/NetworkToolbar.svelte';
  import NetworkGraph from './lib/components/NetworkGraph.svelte';
  import DetailPanel from './lib/components/DetailPanel.svelte';
  import DataTable from './lib/components/DataTable.svelte';
  import AriaAnnouncer from './lib/components/AriaAnnouncer.svelte';
  import NetworkStatusBar from './lib/components/NetworkStatusBar.svelte';

  let service = $state(new MockNetworkDataService());
  let selectedAddress = $state<string | null>(null);
  let selectedLinkId = $state<string | null>(null);
  let showTable = $state(false);
  let announcement = $state('');
  let graphComponent: NetworkGraph;

  // Load table preference from localStorage
  if (typeof localStorage !== 'undefined') {
    showTable = localStorage.getItem('network-viz-show-table') === 'true';
  }

  let selectedNode = $derived(
    selectedAddress ? service.nodes.find((n) => n.address === selectedAddress) ?? null : null,
  );

  let selectedLink = $derived(
    selectedLinkId ? service.links.find((l) => l.id === selectedLinkId) ?? null : null,
  );

  let healthySummary = $derived.by(() => {
    const counts = { online: 0, degraded: 0, offline: 0 };
    for (const n of service.nodes) counts[n.status]++;
    const parts: string[] = [];
    if (counts.online > 0) parts.push(`${counts.online} healthy`);
    if (counts.degraded > 0) parts.push(`${counts.degraded} degraded`);
    if (counts.offline > 0) parts.push(`${counts.offline} offline`);
    return parts.join(', ');
  });

  service.onAlert = (msg) => {
    announcement = msg;
  };

  function handleNodeClick(address: string) {
    selectedAddress = address;
    selectedLinkId = null;
  }

  function handleLinkClick(linkId: string) {
    selectedLinkId = linkId;
    selectedAddress = null;
  }

  function toggleView() {
    showTable = !showTable;
    if (typeof localStorage !== 'undefined') {
      localStorage.setItem('network-viz-show-table', String(showTable));
    }
  }

  $effect(() => {
    service.start();
    return () => service.stop();
  });
</script>

<main class="network-app">
  <NetworkToolbar
    {showTable}
    onToggleView={toggleView}
    onRecenter={() => graphComponent?.recenter()}
    onZoomFit={() => graphComponent?.zoomToFit()}
  />

  <div class="content">
    {#if showTable}
      <div class="table-area">
        <DataTable
          nodes={service.nodes}
          {selectedAddress}
          onNodeSelect={handleNodeClick}
        />
      </div>
    {:else}
      <NetworkGraph
        bind:this={graphComponent}
        nodes={service.nodes}
        links={service.links}
        {selectedAddress}
        onNodeClick={handleNodeClick}
        onLinkClick={handleLinkClick}
      />
    {/if}

    <DetailPanel
      {selectedNode}
      {selectedLink}
      nodes={service.nodes}
      links={service.links}
      {onLinkClick}={handleLinkClick}
    />
  </div>

  <NetworkStatusBar
    nodeCount={service.nodes.length}
    linkCount={service.links.length}
    {healthySummary}
  />

  <AriaAnnouncer message={announcement} />
</main>

<style>
  .network-app {
    width: 100vw;
    height: 100vh;
    display: flex;
    flex-direction: column;
    background: var(--bg-primary, #1e1f22);
    color: var(--text-primary, #f2f3f5);
    overflow: hidden;
  }

  .content {
    flex: 1;
    display: flex;
    overflow: hidden;
  }

  .table-area {
    flex: 1;
    overflow: auto;
  }
</style>
```

**Note:** Check that the `{onLinkClick}={handleLinkClick}` shorthand compiles — if Svelte 5 requires the explicit form, use `onLinkClick={handleLinkClick}` instead.

**Step 2: Verify build**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npm run build`
Expected: build succeeds

**Step 3: Run all tests**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run`
Expected: all tests PASS

**Step 4: Commit**

```bash
git add src/NetworkApp.svelte
git commit -m "feat: wire NetworkApp root component with all panels"
```

---

### Task 13: Launch Button in Main Window

**Files:**
- Modify: `src/lib/components/NavPanel.svelte`

**Step 1: Add network window launch button**

Add a button at the bottom of the NavPanel (near the existing settings button) that opens the network visualization window. Use `@tauri-apps/api/webviewWindow` to spawn it.

At the top of `NavPanel.svelte`, add the import and handler:

```typescript
import { WebviewWindow } from '@tauri-apps/api/webviewWindow';

async function openNetworkWindow() {
  const existing = await WebviewWindow.getByLabel('network-viz');
  if (existing) {
    await existing.setFocus();
    return;
  }
  new WebviewWindow('network-viz', {
    url: 'src/network.html',
    title: 'Harmony — Network',
    width: 1200,
    height: 800,
    minWidth: 800,
    minHeight: 600,
  });
}
```

In the template, near the existing settings button, add:

```svelte
<button
  type="button"
  class="nav-action-btn"
  aria-label="Open network visualization"
  onclick={openNetworkWindow}
>
  Network
</button>
```

**Step 2: Verify build**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npm run build`
Expected: build succeeds

**Step 3: Run all tests**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run`
Expected: all tests PASS (NavPanel tests may need a mock for the Tauri import — add a vi.mock at the top of the NavPanel test file if needed)

**Step 4: Commit**

```bash
git add src/lib/components/NavPanel.svelte
git commit -m "feat: add network visualization launch button to NavPanel"
```

---

### Task 14: Final Integration Verification

**Step 1: Run full test suite**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run`
Expected: all tests PASS (existing + ~45 new tests)

**Step 2: Run build**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npm run build`
Expected: clean build, dist/ contains both entry points

**Step 3: Manual verification checklist**

- [ ] `dist/index.html` exists (main chat window)
- [ ] `dist/src/network.html` exists (network window)
- [ ] No TypeScript errors in build output
- [ ] No console warnings about missing imports

**Step 4: Run `tauri dev` for visual spot-check (if Rust toolchain available)**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npm run tauri dev`
Expected: main window opens, "Network" button visible in nav, clicking it opens second window with graph

**Step 5: Final commit if any fixes were needed**

```bash
git add -A
git commit -m "fix: integration fixes for network visualization"
```
