# Network Visualization Upgrade — Heat, Capabilities, Transport Types

## Goal

Upgrade the network visualization to show node load as a heat gradient, active capabilities as badges, and link transport types as distinct visual styles. All driven by an enhanced mock data service — no backend dependency.

## Architecture

Update existing network visualization components with three visual upgrades, all driven by an enhanced `MockNetworkDataService`:

1. **Node heat** — smooth green→yellow→red gradient based on aggregate load
2. **Capability badges** — small colored dots around each node showing active capabilities (inference, storage, routing, compute). Nodes can have multiple.
3. **Link transport types** — distinct stroke styles per Harmony transport layer

No new components or files. All changes are within existing code.

### Files Modified

| File | Changes |
|------|---------|
| `src/lib/network-types.ts` | Add `NodeCapability`, `TransportType`, new fields on `NetworkNode`/`NetworkLink` |
| `src/lib/network-data-service.ts` | Generate realistic topology with capabilities, transports, heat dynamics |
| `src/lib/graph-utils.ts` | Replace `nodeHealthColor` with `heatToColor()`, add `badgePosition()`, add `linkDashPattern()` |
| `src/lib/components/NetworkGraph.svelte` | Updated canvas rendering for heat, badges, styled links. Update `SimNode`/`SimLink` interfaces and `syncData()`. |
| `src/lib/components/NodeDetail.svelte` | Show capabilities and model name |
| `src/lib/components/LinkDetail.svelte` | Show transport type and encryption status |

## Type Changes

Extensions to existing types in `network-types.ts`:

```typescript
export type NodeCapability = 'inference' | 'storage' | 'routing' | 'compute';
export type TransportType = 'iroh' | 'reticulum' | 'zenoh' | 'rawlink' | 's3';
```

New fields on `NetworkNode`:
- `capabilities: NodeCapability[]` — what this node can do (multiple allowed)
- `heatPercent: number` — 0-100 aggregate load derived from CPU + memory weighted average
- `modelName?: string` — if inference capable, which model (e.g. "qwen3-4b")

New fields on `NetworkLink`:
- `transportType: TransportType` — Harmony protocol layer (iroh tunnel, Reticulum interface, etc.)
- `encrypted: boolean` — whether link has post-quantum encryption

The existing `interfaceType` field describes the physical layer (TCP, UDP, serial). The new `transportType` describes the Harmony protocol layer riding on top. Both are retained.

## Heat Color Mapping

**`heatToColor` replaces `nodeHealthColor` for the node fill color.** The existing `nodeHealthColor(status, isLocal)` is removed. The new function:

`heatToColor(percent: number, status: NodeStatus): string` in `graph-utils.ts`:
- Offline nodes (status `'offline'`): return `#72767d` (matches the existing offline gray from `nodeHealthColor`)
- Otherwise: linear interpolation across three stops: 0%=`#43b581` (green, matches existing online color), 50%=`#faa61a` (amber), 100%=`#ed4245` (red)
- Clamps percent to 0-100

The local node (`isLocal: true`) retains its blurple (`#5865f2`) highlight — `heatToColor` returns blurple for local nodes regardless of heat, keeping the "this is you" indicator.

Full signature: `heatToColor(percent: number, status: NodeStatus, isLocal: boolean): string`

`heatPercent` is computed by the data service on each tick as a weighted average: `0.6 * cpuPercent + 0.4 * (memoryUsedBytes / memoryTotalBytes * 100)`

## Capability Badges

`badgePosition(capabilityIndex: number, nodeRadius: number): { dx: number, dy: number }` in `graph-utils.ts`:

Fixed mapping from capability to index:
- 0 = inference → top-right offset (`{ dx: +nodeRadius * 0.7, dy: -nodeRadius * 0.7 }`)
- 1 = storage → bottom-right (`{ dx: +nodeRadius * 0.7, dy: +nodeRadius * 0.7 }`)
- 2 = routing → bottom-left (`{ dx: -nodeRadius * 0.7, dy: +nodeRadius * 0.7 }`)
- 3 = compute → top-left (`{ dx: -nodeRadius * 0.7, dy: -nodeRadius * 0.7 }`)

Returns `{ dx, dy }` as offset from node center. The canvas renderer adds these to the node's `x, y` to get absolute positions.

Badge colors (constant array in `graph-utils.ts`):
```typescript
export const CAPABILITY_COLORS: Record<NodeCapability, string> = {
  inference: '#5865f2',
  storage: '#57f287',
  routing: '#fee75c',
  compute: '#eb459e',
};
```

Badge radius: 4px (canvas pixels). Only rendered when `transform.k > 0.5` (badges smaller than 2px on screen are hidden).

## Link Rendering

**Stroke style** — new `linkDashPattern(transportType: TransportType): number[]` in `graph-utils.ts`:
- `iroh`: `[]` (solid)
- `reticulum`: `[8, 4]` (dashed)
- `zenoh`: `[2, 4]` (dotted)
- `rawlink`: `[4, 8]` (long-dash)
- `s3`: `[1, 3]` (fine dotted)

**Line width** — `linkWidth` retains its current behavior: driven by `utilizationPercent` (busier links are thicker). This is unchanged from the existing implementation. The `capacityBps` is NOT used for width — the spec originally proposed this but it requires normalization across orders of magnitude (115kbps to 1Gbps) which adds complexity for minimal visual benefit. Utilization-based width already conveys "how busy is this link."

**Color** — `linkUtilizationColor` continues to drive color based on utilization. No change.

**Encrypted glow** — encrypted links (`encrypted: true`) get a wider (2x width) transparent white stroke drawn behind the main stroke. Applied in the canvas `drawLinks()` pass.

## NetworkGraph.svelte Internal Updates

The `SimNode` and `SimLink` local interfaces must be extended with the new fields:

```typescript
// Add to SimNode:
capabilities: NodeCapability[];
heatPercent: number;
modelName?: string;

// Add to SimLink:
transportType: TransportType;
encrypted: boolean;
```

`createSimNodes()` and `createSimLinks()` must copy these fields from source data.

`syncData()` must sync `heatPercent`, `capabilities`, and `status` on nodes each tick (in addition to the existing `status` sync). Links sync `transportType` and `encrypted` (these don't change per-tick but should be present after node add/remove).

## Mock Data Service Updates

The `MockNetworkDataService` generates a realistic Harmony mesh:

**Node generation:**
- Each node gets 1-3 random capabilities
- ~30% have inference (assigned a model name from: "qwen3-4b", "qwen3-0.8b", "qwen3-9b")
- ~60% have storage
- ~80% have routing
- ~40% have compute
- `heatPercent` computed from CPU + memory each tick

**Link generation:**
- `transportType` assigned based on connected node capabilities:
  - Both nodes have inference → `iroh` (high bandwidth tunnel for model coordination)
  - Neither node has inference → `reticulum` (mesh relay)
  - One has inference, other doesn't → `zenoh` (pub/sub session)
  - `rawlink` and `s3` are not generated by the mock — they are reserved for live telemetry when real transport data is available
- `encrypted`: `iroh` = true, `zenoh` = true, `reticulum` = false, `rawlink` = false

**Heat dynamics:**
- CPU/memory vary over time (sine wave + random noise)
- Inference nodes run hotter on average (baseline CPU 40-60% vs 10-30% for routing-only)
- Nodes occasionally spike to simulate inference bursts

## Detail Panel Updates

**NodeDetail:** Add a "Capabilities" row listing active capability names (e.g. "inference, storage, routing"). Show `modelName` if the node has inference capability (e.g. "Model: qwen3-4b").

**LinkDetail:** Add "Transport" row showing `transportType` (e.g. "iroh"). Add "Encrypted" indicator (checkmark or cross).

## Testing Strategy

### Unit tests

- `heatToColor(0, 'online', false)` → `#43b581`, `heatToColor(50, 'online', false)` → `#faa61a`, `heatToColor(100, 'online', false)` → `#ed4245`
- `heatToColor(50, 'offline', false)` → `#72767d` (offline ignores heat)
- `heatToColor(50, 'online', true)` → `#5865f2` (local node always blurple)
- `heatToColor` clamps values outside 0-100
- `badgePosition(0, 10)` → `{ dx: 7, dy: -7 }` (top-right, inference)
- `badgePosition(2, 10)` → `{ dx: -7, dy: 7 }` (bottom-left, routing)
- `linkDashPattern('iroh')` → `[]`, `linkDashPattern('reticulum')` → `[8, 4]`
- `CAPABILITY_COLORS.inference` → `#5865f2`
- Mock data service generates nodes with valid capabilities and links with valid transport types

### Existing tests

- All 744 existing tests continue to pass
- Component tests for `NodeDetail`, `LinkDetail`, `NetworkGraph` updated to include new fields in test data
- Tests referencing `nodeHealthColor` updated to use `heatToColor`

## Scope Exclusions

- **No live telemetry** — mock data only. Live TelemetryEvent subscription is a follow-on bead.
- **No DSD flow animation** — capability badges show inference capability, no animated draft→verify flow
- **No node grouping/clustering** — force layout remains flat
- **No on-screen legend** — badge colors are documented, not shown in UI (future enhancement)
- **No node size scaling** — all nodes remain sized by `nodeRadius(hopDistance)` as today
- **No `rawlink`/`s3` mock generation** — these transport types exist in the type but are reserved for live data
