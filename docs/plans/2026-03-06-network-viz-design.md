# Network Visualization Design

**Goal:** A dedicated pop-out window that visualizes the Harmony network as an interactive force-directed graph, showing node health, resource utilization (CPU/memory/disk), and live traffic flow across links — enabling operators to watch workloads distribute in real time.

**Scope:** MVP covers mock data, ego-centric graph navigation, animated link particles, sparkline metrics, and full accessibility (data table + ARIA live announcements). Real backend telemetry integration is out of scope — the data service interface is designed for easy swap-in later.

---

## Window Architecture

The network visualization runs in a **separate Tauri WebviewWindow**, launched from a button in the main chat window's NavPanel. This gives full screen real estate (second monitor friendly) while keeping chat accessible.

```
Main Window (existing)              Network Window (new)
┌─────────────────────┐            ┌──────────────────────────────┐
│ Nav │ Text │ Media   │  button   │  Toolbar                     │
│     │ Feed │ Feed    │ ───────>  │  Canvas Graph │ Detail Panel │
│     │      │         │           │               │              │
└─────────────────────┘            │  Status Bar                  │
                                   └──────────────────────────────┘
```

**Window properties:**
- Spawned via `new WebviewWindow('network-viz', { ... })` from `@tauri-apps/api/webviewWindow`
- Default size: 1200x800, minimum 800x600, resizable
- Title: "Harmony — Network"
- If already open, clicking the button focuses the existing window (no duplicates)
- Independent lifecycle — closing either window doesn't affect the other
- Shares the same Rust process and Tauri event bus as the main window

**Entry point:** A second HTML entry point (`network.html`) with its own Svelte root (`NetworkApp.svelte`). Shares `src/lib/` utilities, types, design tokens, and components (e.g., `Avatar.svelte` for identicons).

**Cross-window communication:** Both windows subscribe to the same Tauri event channels (`network:node-update`, `network:link-update`). No direct window-to-window messaging needed for MVP.

---

## Graph Topology

**Ego-centric with progressive navigation:**

- The local node is placed at the center of the graph
- Direct peers are arranged around it (1 hop)
- Peers-of-peers form an outer ring (2+ hops)
- Clicking any node re-centers the graph on that node, smoothly animating the transition
- `requestPeerData(address)` expands the graph by requesting a peer's neighbors

**Telemetry sharing model:**

- Default (test network): all telemetry visible, always-on — crucial for system development and debugging
- Production: opt-in per metric. Nodes explicitly choose what to share via protocol-level consent. This is a built-in feature of the network design, not an afterthought.

---

## Component Architecture

```
NetworkApp.svelte              — window root, owns state + data service
├── NetworkToolbar.svelte      — re-center, zoom-fit, table/graph toggle, settings
├── NetworkGraph.svelte        — Canvas element + D3-force simulation
│   └── (renders to <canvas>, no child components)
├── DetailPanel.svelte         — right sidebar, contextual content
│   ├── NodeDetail.svelte      — identity, address, sparklines for CPU/mem/disk
│   │   └── Sparkline.svelte   — tiny SVG line chart (reusable)
│   └── LinkDetail.svelte      — endpoints, throughput sparkline, latency
├── DataTable.svelte           — accessible table view (toggled via toolbar)
├── AriaAnnouncer.svelte       — hidden live region for state-change announcements
└── NetworkStatusBar.svelte    — bottom bar with summary stats
```

**Key separation:** NetworkGraph owns Canvas + D3 simulation and emits events (`onNodeClick`, `onNodeHover`, `onLinkClick`). The parent manages selection state and routes it to the DetailPanel.

---

## Rendering: Canvas + D3-Force

**Why Canvas + D3-force:**
- D3-force is the battle-tested standard for force-directed graph layout
- Canvas scales to 100s-1000s of nodes (SVG struggles past ~300)
- We import only `d3-force` (~7KB) and `d3-zoom` (~5KB + `d3-selection` ~4KB) — ~16KB total, not the full D3 bundle
- The detail panel (sparklines, metrics) is regular Svelte/HTML/SVG — best of both worlds

**Force simulation setup:**
- `d3.forceSimulation(nodes)` with:
  - `forceLink(links)` — keeps connected nodes together
  - `forceManyBody()` — repulsion between all nodes
  - `forceCenter()` — gravity toward canvas center
  - `forceCollide()` — prevents node overlap

**Render loop (requestAnimationFrame):**

1. Clear canvas
2. Apply zoom transform (from d3-zoom)
3. Draw links (lines, colored by utilization)
4. Advance + draw particles (animated dots along links)
5. Draw nodes (circles + labels)
6. Draw selection highlight (glow ring)
7. Target 60fps, skip frames when tab is hidden

---

## Visual Design

**Color language** (extends existing `app.css` design tokens):

| Element | State | Color |
|---------|-------|-------|
| Node | healthy | `#43b581` (green) |
| Node | warning | `#faa61a` (amber) |
| Node | critical | `#ed4245` (red) |
| Node | offline | `#72767d` (muted gray) |
| Node | local (self) | `#5865f2` (accent blue) |
| Link | idle (0-20%) | `#4f545c` (dim gray) |
| Link | moderate (20-60%) | `#43b581` (green) |
| Link | busy (60-85%) | `#faa61a` (amber) |
| Link | saturated (85%+) | `#ed4245` (red) |
| Particle | — | `#f2f3f5` at 70% opacity |

**Node rendering:**
- Circle, radius scaled by hop distance (local: 20px, peers: 14px, peers-of-peers: 10px)
- Filled with health-state color, stroked with brighter variant
- Label below circle: display name in `--text-secondary`, truncated at 12 chars
- Selected node: pulsing glow ring via `shadowBlur`
- Hovered node: brighter stroke + pointer cursor

**Link rendering:**
- Line width: 1px (idle) to 4px (saturated), proportional to utilization
- Color follows utilization gradient

**Animated particles:**
- Small circles (radius 2-3px) traveling along links from source to target
- Speed proportional to throughput; bidirectional traffic shows particles in both directions
- 3-8 particles per link depending on traffic volume
- Zero-traffic links show only the static line (no particles)
- Opacity fades at endpoints for smooth appearance
- Global particle cap (~200) to prevent GPU strain

**Canvas background:** `#1a1b1e` with subtle radial gradient from center — slightly darker than `--bg-primary` for depth.

**Pan/zoom (d3-zoom):** scroll wheel zooms, drag pans. Range: 0.2x-4x. Double-click to fit. Trackpad pinch supported natively.

---

## Detail Panel

**Width:** 320px fixed, collapsible via toggle handle.

**States:**
- **Nothing selected:** network summary — total nodes, links, health distribution, aggregate bandwidth
- **Node selected:** NodeDetail with metrics + sparklines
- **Link selected:** LinkDetail with throughput + latency

**NodeDetail layout:**

```
┌──────────────────────────┐
│ [identicon] bravo        │  name + status dot
│ a7f3...c219              │  truncated address, click to copy
│ 2 hops · last seen: 3s   │
├──────────────────────────┤
│ CPU          47%         │  bar + sparkline below
│ [5-min sparkline]        │
├──────────────────────────┤
│ Memory    2.1 / 8.0 GB   │
│ [5-min sparkline]        │
├──────────────────────────┤
│ Disk      34 / 256 GB    │
│ [5-min sparkline]        │
├──────────────────────────┤
│ Links (3)                │
│  → alpha   TCP  12%  8ms │  clickable rows
│  → charlie UDP  67% 23ms │
│  → delta   LoRa  4% 340ms│
└──────────────────────────┘
```

**LinkDetail layout:**

```
┌──────────────────────────┐
│ alpha <-> bravo          │  endpoint names
│ TCP · capacity: 100 Mbps │
├──────────────────────────┤
│ Utilization      67%     │
│ [sparkline]              │
├──────────────────────────┤
│ Latency          23ms    │
│ [sparkline]              │
├──────────────────────────┤
│ Throughput                │
│  up 12.3 MB/s  down 4.1 MB/s │
└──────────────────────────┘
```

**Sparkline component:**
- SVG, not Canvas — lives in normal Svelte DOM
- Takes a `RingBuffer<number>`, renders a polyline
- Width fills container, height 32px
- Line color matches current health state (green/amber/red)
- `role="img"` with `aria-label` describing value + trend (e.g., "CPU usage over last 5 minutes, currently 47 percent, trend stable")
- Reactively updates via `$derived` as the ring buffer receives new entries

---

## Data Model

**Core types** (new file: `network-types.ts`):

```typescript
interface NetworkNode {
  address: string;           // harmony address (16 bytes hex)
  displayName: string;
  isLocal: boolean;
  hopDistance: number;        // 0 = self
  status: 'online' | 'degraded' | 'offline';
  metrics: NodeMetrics;
  metricsHistory: RingBuffer<NodeMetrics>;  // ~300 entries (5 min @ 1s)
  lastSeen: number;
}

interface NodeMetrics {
  timestamp: number;
  cpuPercent: number;        // 0-100
  memoryUsedBytes: number;
  memoryTotalBytes: number;
  diskUsedBytes: number;
  diskTotalBytes: number;
}

interface NetworkLink {
  id: string;
  source: string;            // node address
  target: string;            // node address
  interfaceType: 'tcp' | 'udp' | 'serial' | 'i2p' | 'lora' | 'pipe';
  capacityBps: number;
  utilizationPercent: number;
  latencyMs: number;
  utilizationHistory: RingBuffer<LinkSnapshot>;
}

interface LinkSnapshot {
  timestamp: number;
  utilizationPercent: number;
  latencyMs: number;
  bytesSent: number;
  bytesReceived: number;
}
```

**RingBuffer<T>:** fixed-size circular buffer (~50 lines). Push overwrites oldest. Iterable. Trivially testable.

**Service interface:**

```typescript
interface NetworkDataService {
  nodes: NetworkNode[];
  links: NetworkLink[];
  start(): void;
  stop(): void;
  requestPeerData(address: string): void;
  onAlert?: (message: string) => void;
}
```

**MockNetworkDataService:**
- Simulates 8-12 nodes with ~15-20 links
- Metrics fluctuate realistically (sine waves + noise, occasional spikes)
- Simulates workload migration: CPU rises on one node while falling on another, link utilization spikes during transfer
- Nodes occasionally go degraded/offline and recover
- `requestPeerData()` "discovers" 2-3 new nodes after a short delay

**Svelte integration:** The service's `nodes` and `links` arrays are `$state` — mutations trigger fine-grained reactivity automatically. No manual reassignment needed.

---

## Accessibility

Accessibility is a design requirement, not a nice-to-have.

**Data table (DataTable.svelte):**
- Full alternative view, toggled via toolbar button (`Ctrl/Cmd+T`), replaces Canvas area
- Semantic `<table>` with `<thead>`, `<tbody>`, `<th scope="col">` — native screen reader table navigation
- Sortable columns: name, status, hops, CPU%, mem%, disk%, links
- Click header to sort asc/desc with visual indicator
- Row click opens detail panel (same as clicking a graph node)
- Arrow key navigation between rows, Enter to select, Tab between columns
- Offline nodes show dashes instead of stale metrics
- Status dots use same color language as graph nodes
- Selection state shared between table and graph views

**ARIA live announcements (AriaAnnouncer.svelte):**
- Hidden `<div role="status" aria-live="polite" aria-atomic="true">` with `sr-only` class
- Debounced to max 1 announcement per 3 seconds
- Announces: node online/offline/degraded/recovered, new node discovered, link saturated

**Sparkline accessibility:**
- `role="img"` with reactive `aria-label`: "CPU usage over last 5 minutes, currently 47 percent, trend rising"

**Graph/table toggle:**
- State persisted in `localStorage`
- Both views share selection state — seamless switching

---

## Testing Strategy

**Unit tests (~40-50 new tests):**

| Component/Module | Coverage |
|-----------------|----------|
| RingBuffer | push, overflow, iteration, size, clear, edge cases |
| MockNetworkDataService | metrics fluctuation bounds, node status transitions, requestPeerData adds nodes |
| Sparkline | SVG path from data, aria-label updates, empty buffer |
| DataTable | renders all nodes, sorts by each column, keyboard nav, row click, offline dashes |
| AriaAnnouncer | message text updates, debounce |
| NodeDetail | displays metrics, lists links, identicon, click-to-copy |
| LinkDetail | endpoints, throughput direction, sparklines |
| NetworkToolbar | toggle emits event, accessible labels |
| NetworkStatusBar | correct counts |
| graph-utils | hit-testing (findNodeAtPoint), particle advancement, color mapping |

**Canvas rendering** is not unit-tested directly. Instead, we extract and test the pure logic:
- D3-force simulation setup (correct forces)
- Hit-testing: given (x, y) click, which node?
- Particle math: given link endpoints + speed, correct position advancement

**Integration tests:**
- NetworkApp: service starts → nodes in table → click node → detail panel → toggle view → state persists
- Window launch: verify WebviewWindow constructor called with correct options (mock Tauri API)

**Out of scope:** visual regression tests, E2E Tauri window tests, performance benchmarks.

---

## Dependencies

| Package | Size (gzipped) | Purpose |
|---------|----------------|---------|
| `d3-force` | ~7 KB | Force simulation engine |
| `d3-zoom` | ~5 KB | Pan/zoom Canvas transforms |
| `d3-selection` | ~4 KB | Peer dependency of d3-zoom |

Total: **~16 KB**. Everything else is hand-built.

---

## New Files

```
src/
├── network.html                        second window entry point
├── NetworkApp.svelte                   network window root
├── lib/
│   ├── network-types.ts                NetworkNode, NetworkLink, NodeMetrics, etc.
│   ├── ring-buffer.ts                  RingBuffer<T> utility
│   ├── network-data-service.ts         interface + MockNetworkDataService
│   ├── graph-utils.ts                  hit-testing, particle math, color mapping
│   └── components/
│       ├── NetworkGraph.svelte         Canvas + D3-force
│       ├── NetworkToolbar.svelte       toolbar buttons
│       ├── DetailPanel.svelte          container (node vs link vs summary)
│       ├── NodeDetail.svelte           node metrics + sparklines
│       ├── LinkDetail.svelte           link metrics + sparklines
│       ├── Sparkline.svelte            reusable SVG sparkline
│       ├── DataTable.svelte            accessible table view
│       ├── AriaAnnouncer.svelte        screen reader announcements
│       └── NetworkStatusBar.svelte     bottom summary bar
```

~14 source files, ~8-10 test files.

---

## Out of Scope

- Real backend telemetry (data service interface ready for swap-in)
- Persistent metric history (SQLite)
- Cross-window navigation (click node → jump to chat)
- Custom graph layouts (hierarchical, radial)
- Network alerts/notification system
- Graph export/screenshot
- Mobile/responsive design (desktop second-monitor window)
