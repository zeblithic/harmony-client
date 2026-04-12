# Vine Reshare Improvements Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Improve reshare UX with original creator attribution, reshare counts derived from the local feed, a confirmation dialog before resharing, and navigation to the original vine.

**Architecture:** Two optional fields (`originalCreatorAddress`, `originalCreatorName`) are added to the vine descriptor wire format so reshares always carry attribution. Reshare counts are derived on demand by filtering the local feed — no new Zenoh keyspace. A new `ReshareConfirmDialog` component gates the reshare action. VineCard shows attribution and reshare counts; VinePlayer shows attribution and routes reshare through the dialog.

**Tech Stack:** Rust/serde (Tauri backend), TypeScript/Svelte 5 (frontend), Vitest (tests)

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `src-tauri/src/lib.rs` | Modify | Add `original_creator_address`/`original_creator_name` to wire types |
| `src/lib/types.ts` | Modify | Add `originalCreatorAddress?`/`originalCreatorName?` to VineVideo |
| `src/lib/vine-service.ts` | Modify | Add `getReshareCount()`, `findVine()`, extend `publish()` signature |
| `src/lib/vine-service.test.ts` | Modify | Tests for new service methods |
| `src/lib/mock-data.ts` | Modify | Add original creator fields to mock reshare |
| `src/lib/components/ReshareConfirmDialog.svelte` | Create | Confirmation dialog component |
| `src/lib/components/__tests__/ReshareConfirmDialog.test.ts` | Create | Dialog tests |
| `src/lib/components/VineCard.svelte` | Modify | Attribution row, reshare count, onViewOriginal |
| `src/lib/components/__tests__/VineCard.test.ts` | Modify | Updated tests for new card features |
| `src/lib/components/VinePlayer.svelte` | Modify | Attribution row, confirmation dialog integration |
| `src/lib/components/__tests__/VinePlayer.test.ts` | Modify | Updated tests for player changes |
| `src/lib/components/VineFeed.svelte` | Modify | Thread new props to cards and player |
| `src/lib/components/__tests__/VineFeed.integration.test.ts` | Modify | Updated integration tests |
| `src/App.svelte` | Modify | Wire reshare count, view original, updated reshare handler |

---

### Task 1: Rust Wire Format — Add Original Creator Fields

**Files:**
- Modify: `src-tauri/src/lib.rs:576-617` (type definitions)
- Modify: `src-tauri/src/lib.rs:683-694` (publish_vine wire construction)
- Modify: `src-tauri/src/lib.rs:1376-1439` (serde tests)

- [ ] **Step 1: Write failing tests for new fields**

Add these tests after the existing `publish_vine_payload_creator_name_defaults` test (around line 1439):

```rust
#[test]
fn vine_descriptor_original_creator_roundtrip() {
    let vine = VineDescriptorPayload {
        id: "vine-reshare-1".to_string(),
        creator_address: "aabb".to_string(),
        creator_name: "Alice".to_string(),
        created_at: 1711600000,
        video_cid: "cc".repeat(32),
        title: Some("Reshared vine".to_string()),
        reshare_of: Some("vine-orig-1".to_string()),
        original_creator_address: Some("ddee".to_string()),
        original_creator_name: Some("Bob".to_string()),
    };
    let json = serde_json::to_vec(&vine).unwrap();
    let parsed: VineDescriptorPayload = serde_json::from_slice(&json).unwrap();
    assert_eq!(parsed.original_creator_address.as_deref(), Some("ddee"));
    assert_eq!(parsed.original_creator_name.as_deref(), Some("Bob"));
}

#[test]
fn vine_descriptor_original_creator_omitted_when_none() {
    let vine = VineDescriptorPayload {
        id: "vine-orig-1".to_string(),
        creator_address: "aabb".to_string(),
        creator_name: "Alice".to_string(),
        created_at: 1711600000,
        video_cid: "cc".repeat(32),
        title: None,
        reshare_of: None,
        original_creator_address: None,
        original_creator_name: None,
    };
    let json = String::from_utf8(serde_json::to_vec(&vine).unwrap()).unwrap();
    assert!(!json.contains("originalCreatorAddress"), "None fields should be skipped: {json}");
    assert!(!json.contains("originalCreatorName"), "None fields should be skipped: {json}");
}

#[test]
fn vine_descriptor_camel_case_original_creator() {
    let vine = VineDescriptorPayload {
        id: "vine-1".to_string(),
        creator_address: "aa".to_string(),
        creator_name: "Alice".to_string(),
        created_at: 0,
        video_cid: "bb".to_string(),
        title: None,
        reshare_of: Some("vine-0".to_string()),
        original_creator_address: Some("cc".to_string()),
        original_creator_name: Some("Bob".to_string()),
    };
    let json = String::from_utf8(serde_json::to_vec(&vine).unwrap()).unwrap();
    assert!(json.contains("\"originalCreatorAddress\""), "expected camelCase: {json}");
    assert!(json.contains("\"originalCreatorName\""), "expected camelCase: {json}");
    assert!(!json.contains("\"original_creator_address\""), "unexpected snake_case: {json}");
}

#[test]
fn publish_vine_payload_original_creator_defaults() {
    let json = r#"{
        "videoCid": "aabb"
    }"#;
    let parsed: PublishVinePayload = serde_json::from_str(json).unwrap();
    assert!(parsed.original_creator_address.is_none());
    assert!(parsed.original_creator_name.is_none());
}

#[test]
fn publish_vine_payload_original_creator_present() {
    let json = r#"{
        "videoCid": "aabb",
        "reshareOf": "vine-0",
        "originalCreatorAddress": "ddee",
        "originalCreatorName": "Bob"
    }"#;
    let parsed: PublishVinePayload = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.original_creator_address.as_deref(), Some("ddee"));
    assert_eq!(parsed.original_creator_name.as_deref(), Some("Bob"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo test`
Expected: Compilation errors — `original_creator_address` and `original_creator_name` fields don't exist yet.

- [ ] **Step 3: Add fields to VineDescriptorPayload**

In `src-tauri/src/lib.rs`, add two fields after `reshare_of` in `VineDescriptorPayload` (around line 588):

```rust
/// Vine descriptor published/received over Zenoh.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VineDescriptorPayload {
    pub id: String,
    pub creator_address: String,
    pub creator_name: String,
    pub created_at: u64,
    pub video_cid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reshare_of: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_creator_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_creator_name: Option<String>,
}
```

- [ ] **Step 4: Add fields to PublishVinePayload**

Add after `reshare_of` in `PublishVinePayload` (around line 599):

```rust
/// Vine descriptor sent from the frontend to publish.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishVinePayload {
    pub video_cid: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub reshare_of: Option<String>,
    #[serde(default)]
    pub original_creator_address: Option<String>,
    #[serde(default)]
    pub original_creator_name: Option<String>,
    /// Creator's display name (included so receivers can display it).
    #[serde(default)]
    pub creator_name: String,
}
```

- [ ] **Step 5: Add fields to VineVideoDto**

Add after `reshare_of` in `VineVideoDto` (around line 615):

```rust
/// Vine video descriptor returned to the frontend (includes local viewed state).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VineVideoDto {
    pub id: String,
    pub creator_address: String,
    pub creator_name: String,
    pub created_at: u64,
    pub video_cid: String,
    pub title: Option<String>,
    pub reshare_of: Option<String>,
    pub original_creator_address: Option<String>,
    pub original_creator_name: Option<String>,
    pub viewed: bool,
}
```

- [ ] **Step 6: Update publish_vine wire construction**

In `publish_vine` function (around line 683), add the new fields to the `VineDescriptorPayload` construction:

```rust
    let wire = VineDescriptorPayload {
        id: format!(
            "vine-{}-{now_secs}-{:08x}",
            &node_addr[..8.min(node_addr.len())],
            rand::random::<u32>()
        ),
        creator_address: node_addr.clone(),
        creator_name: vine.creator_name,
        created_at: now_secs,
        video_cid: vine.video_cid,
        title: vine.title,
        reshare_of: vine.reshare_of,
        original_creator_address: vine.original_creator_address,
        original_creator_name: vine.original_creator_name,
    };
```

- [ ] **Step 7: Fix existing tests — add missing fields**

Update the existing test structs that construct `VineDescriptorPayload` to include the new fields. Each existing test that constructs a `VineDescriptorPayload` needs `original_creator_address: None, original_creator_name: None` added.

In `vine_descriptor_roundtrip` (line 1378):
```rust
        let vine = VineDescriptorPayload {
            id: "vine-abc-1234".to_string(),
            creator_address: "deadbeef01020304".to_string(),
            creator_name: "Alice".to_string(),
            created_at: 1711600000,
            video_cid: "aa".repeat(32),
            title: Some("Demo vine".to_string()),
            reshare_of: None,
            original_creator_address: None,
            original_creator_name: None,
        };
```

In `vine_descriptor_camel_case` (line 1399):
```rust
        let vine = VineDescriptorPayload {
            id: "vine-1".to_string(),
            creator_address: "aa".to_string(),
            creator_name: "Bob".to_string(),
            created_at: 0,
            video_cid: "bb".to_string(),
            title: None,
            reshare_of: Some("vine-0".to_string()),
            original_creator_address: None,
            original_creator_name: None,
        };
```

- [ ] **Step 8: Run tests to verify they pass**

Run: `cd src-tauri && cargo test`
Expected: All tests pass, including the 5 new ones.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat: add original creator fields to vine descriptor wire types"
```

---

### Task 2: TypeScript Types + VineService Methods

**Files:**
- Modify: `src/lib/types.ts:29-39` (VineVideo interface)
- Modify: `src/lib/vine-service.ts` (VineDescriptorEvent, publish, wireToVine, new methods)
- Modify: `src/lib/vine-service.test.ts` (new tests)
- Modify: `src/lib/mock-data.ts:441-450` (mock reshare)

- [ ] **Step 1: Write failing tests for new VineService methods**

Add these tests to `src/lib/vine-service.test.ts`, inside the existing `describe('VineService')` block, after the reaction tests:

```typescript
  // ── Reshare helpers ─────────────────────────────────────────────────

  it('getReshareCount returns 0 when no reshares exist', () => {
    expect(svc.getReshareCount('vine-01')).toBe(0);
  });

  it('getReshareCount counts vines with matching reshareOf', () => {
    // vine-04 in mock data has reshareOf: 'vine-02'
    expect(svc.getReshareCount('vine-02')).toBe(1);
  });

  it('getReshareCount counts across both followed and discover feeds', async () => {
    const { adapter, emit } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue(true);
    await svc.connectAdapter(adapter);
    // Move a reshare to followed feed
    emit('vine-received', {
      id: 'reshare-f1', creatorAddress: 'aabb', creatorName: 'Alice',
      createdAt: 1, videoCid: 'cid-r1', reshareOf: 'vine-02', source: 'followed',
    });
    // vine-04 is in discover (mock data), reshare-f1 is in followed
    expect(svc.getReshareCount('vine-02')).toBe(2);
  });

  it('findVine returns vine from discoverVines', () => {
    const found = svc.findVine('vine-01');
    expect(found).toBeDefined();
    expect(found!.id).toBe('vine-01');
  });

  it('findVine returns vine from followedVines', async () => {
    const { adapter, emit } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue(true);
    await svc.connectAdapter(adapter);
    emit('vine-received', {
      id: 'fv-find', creatorAddress: 'aabb', creatorName: 'Alice',
      createdAt: 1, videoCid: 'cid-ff', source: 'followed',
    });
    expect(svc.findVine('fv-find')).toBeDefined();
  });

  it('findVine returns undefined for unknown vine', () => {
    expect(svc.findVine('nonexistent')).toBeUndefined();
  });

  it('publish passes originalCreatorAddress and originalCreatorName to adapter', async () => {
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    await svc.publish('cid-pub', 'Title', 'reshare-of-1', 'orig-addr', 'OrigAuthor');
    expect(adapter.invoke).toHaveBeenCalledWith('publish_vine', {
      vine: {
        videoCid: 'cid-pub',
        title: 'Title',
        reshareOf: 'reshare-of-1',
        creatorName: 'You',
        originalCreatorAddress: 'orig-addr',
        originalCreatorName: 'OrigAuthor',
      },
    });
  });

  it('publish omits original creator fields when not provided', async () => {
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    await svc.publish('cid-pub', 'Title');
    expect(adapter.invoke).toHaveBeenCalledWith('publish_vine', {
      vine: {
        videoCid: 'cid-pub',
        title: 'Title',
        reshareOf: undefined,
        creatorName: 'You',
        originalCreatorAddress: undefined,
        originalCreatorName: undefined,
      },
    });
  });

  it('wireToVine passes through originalCreator fields', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('vine-received', {
      id: 'oc-v1', creatorAddress: 'x', creatorName: 'X',
      createdAt: 1, videoCid: 'cid-oc',
      reshareOf: 'orig-1',
      originalCreatorAddress: 'orig-addr',
      originalCreatorName: 'OrigAuthor',
    });
    const vine = svc.vines.find(v => v.id === 'oc-v1')!;
    expect(vine.originalCreatorAddress).toBe('orig-addr');
    expect(vine.originalCreatorName).toBe('OrigAuthor');
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/vine-service.test.ts`
Expected: FAIL — `getReshareCount`, `findVine` don't exist; `publish` doesn't accept original creator args; `originalCreatorAddress` not on VineVideo type.

- [ ] **Step 3: Add fields to VineVideo type**

In `src/lib/types.ts`, add to the `VineVideo` interface (after `reshareOf?`):

```typescript
export interface VineVideo {
  id: string;
  creatorAddress: string;
  creatorName: string;
  createdAt: number;
  videoCid: string;
  title?: string;
  reshareOf?: string;
  originalCreatorAddress?: string;
  originalCreatorName?: string;
  viewed?: boolean;
}
```

- [ ] **Step 4: Add fields to VineDescriptorEvent**

In `src/lib/vine-service.ts`, update the `VineDescriptorEvent` interface:

```typescript
export interface VineDescriptorEvent {
  id: string;
  creatorAddress: string;
  creatorName: string;
  createdAt: number;
  videoCid: string;
  title?: string;
  reshareOf?: string;
  originalCreatorAddress?: string;
  originalCreatorName?: string;
  source?: 'followed' | 'discover';
}
```

- [ ] **Step 5: Update wireToVine to pass through new fields**

In `src/lib/vine-service.ts`, update the `wireToVine` method to include the new fields in the returned object:

```typescript
  private wireToVine(wire: VineDescriptorEvent): VineVideo {
    const isSelf = this.ownAddress != null && wire.creatorAddress === this.ownAddress;

    return {
      id: wire.id,
      creatorAddress: isSelf ? 'self' : wire.creatorAddress,
      creatorName: isSelf
        ? this.ownDisplayName
        : wire.creatorName || wire.creatorAddress.slice(0, 8),
      createdAt: wire.createdAt,
      videoCid: wire.videoCid,
      title: wire.title,
      reshareOf: wire.reshareOf,
      originalCreatorAddress: wire.originalCreatorAddress,
      originalCreatorName: wire.originalCreatorName,
      viewed: isSelf,
    };
  }
```

- [ ] **Step 6: Update publish() signature and adapter call**

In `src/lib/vine-service.ts`, extend the `publish` method:

```typescript
  async publish(
    videoCid: string,
    title?: string,
    reshareOf?: string,
    originalCreatorAddress?: string,
    originalCreatorName?: string,
  ): Promise<void> {
    if (this.adapter) {
      try {
        await this.adapter.invoke('publish_vine', {
          vine: {
            videoCid,
            title,
            reshareOf,
            creatorName: this.ownDisplayName,
            originalCreatorAddress,
            originalCreatorName,
          },
        });
        return;
      } catch (err: unknown) {
        const msg = err instanceof Error ? err.message : String(err);
        if (!msg.includes('not connected') && !msg.includes('event loop')) {
          throw err;
        }
      }
    }

    // Offline fallback
    const id = `vine-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
    this.seenIds.add(id);
    this.viewedIds = new Set([...this.viewedIds, id]);
    const vine: VineVideo = {
      id,
      creatorAddress: 'self',
      creatorName: this.ownDisplayName,
      createdAt: Math.floor(Date.now() / 1000),
      videoCid,
      title,
      reshareOf,
      originalCreatorAddress,
      originalCreatorName,
      viewed: true,
    };
    this.discoverVines = [...this.discoverVines, vine];
    this.onChange?.();
  }
```

- [ ] **Step 7: Add getReshareCount() method**

Add after `getReaction()` in `VineService`:

```typescript
  /** Count reshares of a vine visible in the local feed. */
  getReshareCount(vineId: string): number {
    let count = 0;
    for (const v of this.followedVines) {
      if (v.reshareOf === vineId) count++;
    }
    for (const v of this.discoverVines) {
      if (v.reshareOf === vineId) count++;
    }
    return count;
  }
```

- [ ] **Step 8: Add findVine() method**

Add after `getReshareCount()`:

```typescript
  /** Find a vine by ID across both feeds. */
  findVine(vineId: string): VineVideo | undefined {
    return this.followedVines.find(v => v.id === vineId)
      ?? this.discoverVines.find(v => v.id === vineId);
  }
```

- [ ] **Step 9: Update mock data**

In `src/lib/mock-data.ts`, update the reshare vine (vine-04) to include original creator fields:

```typescript
  {
    id: 'vine-04',
    creatorAddress: 'a1b2c3d4',
    creatorName: 'Alice',
    createdAt: vineBase + 600,
    videoCid: 'cid-video-alice-02',
    title: 'Cache hit rates explained',
    reshareOf: 'vine-02',
    originalCreatorAddress: 'e5f6g7h8',
    originalCreatorName: 'Bob',
    viewed: false,
  },
```

- [ ] **Step 10: Run tests to verify they pass**

Run: `npx vitest run src/lib/vine-service.test.ts`
Expected: All tests pass, including the 9 new ones.

- [ ] **Step 11: Commit**

```bash
git add src/lib/types.ts src/lib/vine-service.ts src/lib/vine-service.test.ts src/lib/mock-data.ts
git commit -m "feat: add getReshareCount, findVine, and original creator fields to VineService"
```

---

### Task 3: ReshareConfirmDialog Component

**Files:**
- Create: `src/lib/components/ReshareConfirmDialog.svelte`
- Create: `src/lib/components/__tests__/ReshareConfirmDialog.test.ts`

- [ ] **Step 1: Write failing tests**

Create `src/lib/components/__tests__/ReshareConfirmDialog.test.ts`:

```typescript
import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import ReshareConfirmDialog from '../ReshareConfirmDialog.svelte';
import type { VineVideo } from '../../types';

const vine: VineVideo = {
  id: 'vine-01',
  creatorAddress: 'a1b2c3d4',
  creatorName: 'Alice',
  createdAt: 1700000000,
  videoCid: 'cid-abc123',
  title: 'Demo vine',
  viewed: false,
};

const resharedVine: VineVideo = {
  ...vine,
  reshareOf: 'vine-00',
  originalCreatorAddress: 'orig-addr',
  originalCreatorName: 'Bob',
};

describe('ReshareConfirmDialog', () => {
  it('renders dialog with vine title', () => {
    render(ReshareConfirmDialog, { props: { vine, onConfirm: vi.fn(), onCancel: vi.fn() } });
    expect(screen.getByText('Reshare this vine?')).toBeTruthy();
    expect(screen.getByText('Demo vine')).toBeTruthy();
  });

  it('shows original creator name for reshares', () => {
    render(ReshareConfirmDialog, { props: { vine: resharedVine, onConfirm: vi.fn(), onCancel: vi.fn() } });
    expect(screen.getByText(/Bob/)).toBeTruthy();
  });

  it('shows resharer name when not a reshare', () => {
    render(ReshareConfirmDialog, { props: { vine, onConfirm: vi.fn(), onCancel: vi.fn() } });
    expect(screen.getByText(/Alice/)).toBeTruthy();
  });

  it('shows Untitled for vines without title', () => {
    const untitled = { ...vine, title: undefined };
    render(ReshareConfirmDialog, { props: { vine: untitled, onConfirm: vi.fn(), onCancel: vi.fn() } });
    expect(screen.getByText('Untitled vine')).toBeTruthy();
  });

  it('calls onConfirm when Reshare button is clicked', async () => {
    const onConfirm = vi.fn();
    render(ReshareConfirmDialog, { props: { vine, onConfirm, onCancel: vi.fn() } });
    await fireEvent.click(screen.getByText('Reshare'));
    expect(onConfirm).toHaveBeenCalledOnce();
  });

  it('calls onCancel when Cancel button is clicked', async () => {
    const onCancel = vi.fn();
    render(ReshareConfirmDialog, { props: { vine, onConfirm: vi.fn(), onCancel } });
    await fireEvent.click(screen.getByText('Cancel'));
    expect(onCancel).toHaveBeenCalledOnce();
  });

  it('calls onCancel when Escape is pressed', async () => {
    const onCancel = vi.fn();
    render(ReshareConfirmDialog, { props: { vine, onConfirm: vi.fn(), onCancel } });
    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(onCancel).toHaveBeenCalledOnce();
  });

  it('calls onCancel when backdrop is clicked', async () => {
    const onCancel = vi.fn();
    render(ReshareConfirmDialog, { props: { vine, onConfirm: vi.fn(), onCancel } });
    const backdrop = screen.getByRole('dialog').parentElement!;
    await fireEvent.click(backdrop);
    expect(onCancel).toHaveBeenCalledOnce();
  });

  it('does not call onCancel when dialog body is clicked', async () => {
    const onCancel = vi.fn();
    render(ReshareConfirmDialog, { props: { vine, onConfirm: vi.fn(), onCancel } });
    await fireEvent.click(screen.getByRole('dialog'));
    expect(onCancel).not.toHaveBeenCalled();
  });

  it('has dialog role for accessibility', () => {
    render(ReshareConfirmDialog, { props: { vine, onConfirm: vi.fn(), onCancel: vi.fn() } });
    expect(screen.getByRole('dialog')).toBeTruthy();
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/ReshareConfirmDialog.test.ts`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement ReshareConfirmDialog**

Create `src/lib/components/ReshareConfirmDialog.svelte`:

```svelte
<script lang="ts">
  import type { VineVideo } from '../types';

  let { vine, onConfirm, onCancel }: {
    vine: VineVideo;
    onConfirm: () => void;
    onCancel: () => void;
  } = $props();

  let displayCreator = $derived(vine.originalCreatorName ?? vine.creatorName);

  function handleBackdropClick(e: MouseEvent) {
    if (e.target === e.currentTarget) onCancel();
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Escape') onCancel();
  }
</script>

<svelte:window onkeydown={handleKeyDown} />

<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="reshare-backdrop" onclick={handleBackdropClick}>
  <div class="reshare-dialog" role="dialog" aria-label="Reshare confirmation" aria-modal="true">
    <p class="dialog-title">Reshare this vine?</p>
    <p class="dialog-detail">{vine.title ?? 'Untitled vine'}</p>
    <p class="dialog-creator">by {displayCreator}</p>
    <div class="dialog-buttons">
      <button type="button" class="dialog-btn cancel" onclick={onCancel}>Cancel</button>
      <button type="button" class="dialog-btn confirm" onclick={onConfirm}>Reshare</button>
    </div>
  </div>
</div>

<style>
  .reshare-backdrop {
    position: fixed;
    inset: 0;
    z-index: 200;
    background: rgba(0, 0, 0, 0.6);
    display: flex;
    align-items: center;
    justify-content: center;
  }

  .reshare-dialog {
    background: var(--bg-secondary);
    border-radius: 12px;
    padding: 20px;
    width: 280px;
    max-width: 90vw;
    text-align: center;
  }

  .dialog-title {
    font-size: 0.95rem;
    font-weight: 600;
    color: var(--text-primary);
    margin: 0 0 8px;
  }

  .dialog-detail {
    font-size: 0.8rem;
    color: var(--text-secondary);
    margin: 0 0 4px;
  }

  .dialog-creator {
    font-size: 0.75rem;
    color: var(--text-muted);
    margin: 0 0 16px;
  }

  .dialog-buttons {
    display: flex;
    gap: 8px;
    justify-content: center;
  }

  .dialog-btn {
    padding: 8px 20px;
    border-radius: 6px;
    font-size: 0.85rem;
    font-weight: 500;
    cursor: pointer;
    border: none;
    transition: opacity 0.15s;
  }

  .dialog-btn:hover {
    opacity: 0.85;
  }

  .dialog-btn.cancel {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
  }

  .dialog-btn.confirm {
    background: var(--accent);
    color: white;
  }
</style>
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/ReshareConfirmDialog.test.ts`
Expected: All 10 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/ReshareConfirmDialog.svelte src/lib/components/__tests__/ReshareConfirmDialog.test.ts
git commit -m "feat: add ReshareConfirmDialog component"
```

---

### Task 4: VineCard — Attribution Row + Reshare Count

**Files:**
- Modify: `src/lib/components/VineCard.svelte`
- Modify: `src/lib/components/__tests__/VineCard.test.ts`

- [ ] **Step 1: Write failing tests**

Add these tests to `src/lib/components/__tests__/VineCard.test.ts`, replacing the existing reshare badge tests:

```typescript
  // ── Reshare attribution ──────────────────────────────────────────

  it('shows attribution row for reshares with original creator name', () => {
    const reshared = { ...vine, reshareOf: 'vine-00', originalCreatorName: 'Bob' };
    render(VineCard, { props: { vine: reshared, onPlay: vi.fn() } });
    expect(screen.getByText(/originally by/)).toBeTruthy();
    expect(screen.getByText('Bob')).toBeTruthy();
  });

  it('does not show attribution for original vines', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn() } });
    expect(screen.queryByText(/originally by/)).toBeNull();
  });

  it('calls onViewOriginal when original creator name is clicked', async () => {
    const onViewOriginal = vi.fn();
    const reshared = { ...vine, reshareOf: 'vine-00', originalCreatorName: 'Bob' };
    render(VineCard, { props: { vine: reshared, onPlay: vi.fn(), onViewOriginal } });
    await fireEvent.click(screen.getByText('Bob'));
    expect(onViewOriginal).toHaveBeenCalledWith('vine-00');
  });

  it('attribution click does not trigger onPlay', async () => {
    const onPlay = vi.fn();
    const onViewOriginal = vi.fn();
    const reshared = { ...vine, reshareOf: 'vine-00', originalCreatorName: 'Bob' };
    render(VineCard, { props: { vine: reshared, onPlay, onViewOriginal } });
    await fireEvent.click(screen.getByText('Bob'));
    expect(onPlay).not.toHaveBeenCalled();
  });

  // ── Reshare count ────────────────────────────────────────────────

  it('shows reshare count when greater than 0 for original vines', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), reshareCount: 3 } });
    expect(screen.getByText('3')).toBeTruthy();
  });

  it('hides reshare count when 0', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), reshareCount: 0 } });
    // Only check for the reshare count element, not other "0" text
    expect(screen.queryByLabelText(/reshare/i)).toBeNull();
  });

  it('does not show reshare count on reshares', () => {
    const reshared = { ...vine, reshareOf: 'vine-00', originalCreatorName: 'Bob' };
    render(VineCard, { props: { vine: reshared, onPlay: vi.fn(), reshareCount: 2 } });
    // reshareCount prop is ignored for reshares — only shown on originals
    expect(screen.queryByLabelText(/reshared \d+ time/)).toBeNull();
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/VineCard.test.ts`
Expected: FAIL — old reshare badge tests may conflict; new props not recognized.

- [ ] **Step 3: Remove old reshare badge tests**

In `src/lib/components/__tests__/VineCard.test.ts`, delete the two existing reshare badge tests:

```typescript
  // DELETE these two tests:
  it('shows reshare badge when vine is a reshare', () => { ... });
  it('does not show reshare badge for original vines', () => { ... });
```

- [ ] **Step 4: Update VineCard component**

In `src/lib/components/VineCard.svelte`, update the props to add `reshareCount` and `onViewOriginal`:

```typescript
  let { vine, onPlay, isViewed, showFollowButton = false, isFollowed = false, onFollow, onUnfollow, reactionCount = 0, likedByMe = false, onToggleLike, reshareCount = 0, onViewOriginal }: {
    vine: VineVideo;
    onPlay: (vine: VineVideo) => void;
    isViewed?: boolean;
    showFollowButton?: boolean;
    isFollowed?: boolean;
    onFollow?: (address: string, name: string) => void;
    onUnfollow?: (address: string) => void;
    reactionCount?: number;
    likedByMe?: boolean;
    onToggleLike?: (vine: VineVideo) => void;
    reshareCount?: number;
    onViewOriginal?: (vineId: string) => void;
  } = $props();
```

Add a handler for the attribution click:

```typescript
  function handleAttributionClick(e: MouseEvent) {
    e.stopPropagation();
    if (vine.reshareOf) onViewOriginal?.(vine.reshareOf);
  }
```

Replace the reshare badge block in the template. Find and replace:

```svelte
    {#if vine.reshareOf}
      <span class="reshare-badge">reshare</span>
    {/if}
```

With:

```svelte
    {#if vine.reshareOf && vine.originalCreatorName}
      <div class="reshare-attribution">
        <span class="reshare-icon" aria-hidden="true">↗</span>
        <span class="reshare-attr-text">
          originally by
          <button type="button" class="original-name" onclick={handleAttributionClick}>
            {vine.originalCreatorName}
          </button>
        </span>
      </div>
    {/if}
```

Update the social stats row to include reshare count. Replace:

```svelte
    {#if reactionCount > 0 || likedByMe}
      <div class="card-like-row">
```

With:

```svelte
    {#if reactionCount > 0 || likedByMe || (reshareCount > 0 && !vine.reshareOf)}
      <div class="card-like-row">
```

Add the reshare count display inside the `card-like-row` div, after the `card-like-count` span:

```svelte
    {#if reactionCount > 0 || likedByMe || (reshareCount > 0 && !vine.reshareOf)}
      <div class="card-like-row">
        {#if reactionCount > 0 || likedByMe}
          <button
            type="button"
            class="card-heart"
            onclick={handleLikeClick}
            aria-label={likedByMe ? `Unlike ${vine.title ?? 'vine'}` : `Like ${vine.title ?? 'vine'}`}
          >
            {likedByMe ? '❤️' : '🤍'}
          </button>
          <span class="card-like-count">{reactionCount}</span>
        {/if}
        {#if reshareCount > 0 && !vine.reshareOf}
          <span class="reshare-count" aria-label="Reshared {reshareCount} {reshareCount === 1 ? 'time' : 'times'}">
            <span class="reshare-count-icon" aria-hidden="true">↗</span>
            {reshareCount}
          </span>
        {/if}
      </div>
    {/if}
```

- [ ] **Step 5: Update VineCard styles**

Replace the `.reshare-badge` style with the new attribution and reshare count styles:

```css
  /* Delete .reshare-badge style block */

  .reshare-attribution {
    display: flex;
    align-items: center;
    gap: 4px;
    margin-top: 1px;
  }

  .reshare-icon {
    font-size: 0.65rem;
    color: var(--text-muted);
  }

  .reshare-attr-text {
    font-size: 0.7rem;
    color: var(--text-muted);
    font-style: italic;
  }

  .original-name {
    background: none;
    border: none;
    padding: 0;
    color: var(--text-secondary);
    font-style: normal;
    font-weight: 500;
    font-size: 0.7rem;
    cursor: pointer;
    text-decoration: none;
  }

  .original-name:hover {
    color: var(--text-primary);
    text-decoration: underline;
  }

  .reshare-count {
    display: flex;
    align-items: center;
    gap: 3px;
    font-size: 0.7rem;
    color: var(--text-muted);
    margin-left: 6px;
  }

  .reshare-count-icon {
    font-size: 0.65rem;
  }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/VineCard.test.ts`
Expected: All tests pass (old reshare badge tests removed, new attribution + count tests pass).

- [ ] **Step 7: Commit**

```bash
git add src/lib/components/VineCard.svelte src/lib/components/__tests__/VineCard.test.ts
git commit -m "feat: replace reshare badge with attribution row and reshare count on VineCard"
```

---

### Task 5: VinePlayer — Attribution + Confirmation Dialog

**Files:**
- Modify: `src/lib/components/VinePlayer.svelte`
- Modify: `src/lib/components/__tests__/VinePlayer.test.ts`

- [ ] **Step 1: Write failing tests**

Add/modify these tests in `src/lib/components/__tests__/VinePlayer.test.ts`. First, replace the existing reshare label tests:

```typescript
  // DELETE these two tests:
  // 'shows reshare label when vine is a reshare'
  // 'does not show reshare label for original vines'

  // ── Reshare attribution ──────────────────────────────────────────

  it('shows attribution row for reshares with original creator name', () => {
    const resharedVine = { ...vine, reshareOf: 'vine-00', originalCreatorName: 'Bob' };
    render(VinePlayer, { props: { vine: resharedVine, onClose: vi.fn(), onReshare: vi.fn() } });
    expect(screen.getByText(/originally by/)).toBeTruthy();
    expect(screen.getByText('Bob')).toBeTruthy();
  });

  it('does not show attribution for original vines', () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn() } });
    expect(screen.queryByText(/originally by/)).toBeNull();
  });

  it('calls onViewOriginal when attribution name is clicked', async () => {
    const onViewOriginal = vi.fn();
    const resharedVine = { ...vine, reshareOf: 'vine-00', originalCreatorName: 'Bob' };
    render(VinePlayer, { props: { vine: resharedVine, onClose: vi.fn(), onViewOriginal, onReshare: vi.fn() } });
    await fireEvent.click(screen.getByText('Bob'));
    expect(onViewOriginal).toHaveBeenCalledWith('vine-00');
  });

  // ── Reshare confirmation dialog ──────────────────────────────────

  it('shows confirmation dialog when reshare button is clicked', async () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn(), onReshare: vi.fn() } });
    await fireEvent.click(screen.getByLabelText('Reshare vine'));
    // Dialog should appear
    expect(screen.getByText('Reshare this vine?')).toBeTruthy();
  });

  it('calls onReshare after dialog confirmation', async () => {
    const onReshare = vi.fn().mockResolvedValue(undefined);
    render(VinePlayer, { props: { vine, onClose: vi.fn(), onReshare } });
    await fireEvent.click(screen.getByLabelText('Reshare vine'));
    await fireEvent.click(screen.getByText('Reshare'));
    expect(onReshare).toHaveBeenCalledWith(expect.objectContaining({ id: 'vine-01' }));
  });

  it('closes dialog on Cancel without resharing', async () => {
    const onReshare = vi.fn();
    render(VinePlayer, { props: { vine, onClose: vi.fn(), onReshare } });
    await fireEvent.click(screen.getByLabelText('Reshare vine'));
    await fireEvent.click(screen.getByText('Cancel'));
    expect(onReshare).not.toHaveBeenCalled();
    expect(screen.queryByText('Reshare this vine?')).toBeNull();
  });

  // ── Self-reshare hiding ──────────────────────────────────────────

  it('hides reshare button on own original vines', () => {
    const ownVine = { ...vine, creatorAddress: 'self' };
    render(VinePlayer, { props: { vine: ownVine, onClose: vi.fn(), onReshare: vi.fn() } });
    expect(screen.queryByLabelText('Reshare vine')).toBeNull();
  });

  it('shows reshare button on own reshared vines', () => {
    const ownReshare = { ...vine, creatorAddress: 'self', reshareOf: 'vine-00' };
    render(VinePlayer, { props: { vine: ownReshare, onClose: vi.fn(), onReshare: vi.fn() } });
    expect(screen.getByLabelText('Reshare vine')).toBeTruthy();
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/VinePlayer.test.ts`
Expected: FAIL — old reshare label tests broken; new tests fail.

- [ ] **Step 3: Remove old reshare label tests**

Delete the two existing reshare label tests from the test file:

```typescript
  // DELETE:
  it('shows reshare label when vine is a reshare', () => { ... });
  it('does not show reshare label for original vines', () => { ... });
```

- [ ] **Step 4: Update VinePlayer props and state**

In `src/lib/components/VinePlayer.svelte`, add the new prop and state:

```typescript
  let { vine, onClose, onNext, onPrevious, onReshare, resolveVideo, onToggleLike, reactionCount = 0, likedByMe = false, onViewOriginal }: {
    vine: VineVideo;
    onClose: () => void;
    onNext?: () => void;
    onPrevious?: () => void;
    onReshare?: (vine: VineVideo) => Promise<void> | void;
    resolveVideo?: (cid: string) => Promise<string>;
    onToggleLike?: (vine: VineVideo) => void;
    reactionCount?: number;
    likedByMe?: boolean;
    onViewOriginal?: (vineId: string) => void;
  } = $props();

  let showReshareConfirm = $state(false);
```

Add the import for the dialog at the top of the script:

```typescript
  import ReshareConfirmDialog from './ReshareConfirmDialog.svelte';
```

Add a derived for hiding reshare on own originals:

```typescript
  let canReshare = $derived(
    onReshare != null && !(vine.creatorAddress === 'self' && !vine.reshareOf)
  );
```

Reset confirm dialog when vine changes (add to the existing `$effect` that resets reshare state):

```typescript
  $effect(() => { void vine; reshareGeneration++; resharing = false; reshareError = ''; showReshareConfirm = false; });
```

- [ ] **Step 5: Update VinePlayer template**

Replace the reshare label:

```svelte
    {#if vine.reshareOf}
      <p class="reshare-label">Reshared</p>
    {/if}
```

With the attribution row:

```svelte
    {#if vine.reshareOf && vine.originalCreatorName}
      <div class="player-reshare-attr">
        <span class="reshare-icon" aria-hidden="true">↗</span>
        <span class="reshare-attr-text">
          originally by
          <button type="button" class="original-name" onclick={() => onViewOriginal?.(vine.reshareOf!)}>
            {vine.originalCreatorName}
          </button>
        </span>
      </div>
    {/if}
```

Replace the reshare button to open the confirmation dialog instead of resharing directly. Change:

```svelte
      {#if onReshare}
        <button type="button" class="action-btn" onclick={handleReshare} disabled={resharing} aria-label="Reshare vine">
          <span aria-hidden="true">↗</span> {resharing ? 'Resharing\u2026' : 'Reshare'}
        </button>
      {/if}
```

To:

```svelte
      {#if canReshare}
        <button type="button" class="action-btn" onclick={() => showReshareConfirm = true} disabled={resharing} aria-label="Reshare vine">
          <span aria-hidden="true">↗</span> {resharing ? 'Resharing\u2026' : 'Reshare'}
        </button>
      {/if}
```

Add the confirmation dialog at the end of the component template (before the closing style tag):

```svelte
{#if showReshareConfirm}
  <ReshareConfirmDialog
    {vine}
    onConfirm={() => { showReshareConfirm = false; handleReshare(); }}
    onCancel={() => showReshareConfirm = false}
  />
{/if}
```

- [ ] **Step 6: Update VinePlayer styles**

Replace the `.reshare-label` style with the new attribution styles:

```css
  /* Delete .reshare-label style block */

  .player-reshare-attr {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 4px;
    margin-bottom: 8px;
  }

  .reshare-icon {
    font-size: 0.75rem;
    color: var(--text-muted);
  }

  .reshare-attr-text {
    font-size: 0.8rem;
    color: var(--text-muted);
    font-style: italic;
  }

  .original-name {
    background: none;
    border: none;
    padding: 0;
    color: var(--text-secondary);
    font-style: normal;
    font-weight: 500;
    font-size: 0.8rem;
    cursor: pointer;
    text-decoration: none;
  }

  .original-name:hover {
    color: var(--text-primary);
    text-decoration: underline;
  }
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/VinePlayer.test.ts`
Expected: All tests pass.

- [ ] **Step 8: Commit**

```bash
git add src/lib/components/VinePlayer.svelte src/lib/components/__tests__/VinePlayer.test.ts
git commit -m "feat: add reshare attribution and confirmation dialog to VinePlayer"
```

---

### Task 6: VineFeed Prop Threading

**Files:**
- Modify: `src/lib/components/VineFeed.svelte`
- Modify: `src/lib/components/__tests__/VineFeed.integration.test.ts`

- [ ] **Step 1: Write failing tests**

Update `src/lib/components/__tests__/VineFeed.integration.test.ts`. First, update the test fixtures to include original creator fields on the reshare vine:

```typescript
const VINES: VineVideo[] = [
  {
    id: 'v1', creatorAddress: 'a1', creatorName: 'Alice',
    createdAt: vineBase, videoCid: 'cid-v1',
    title: 'Transport demo', viewed: false,
  },
  {
    id: 'v2', creatorAddress: 'b2', creatorName: 'Bob',
    createdAt: vineBase + 120, videoCid: 'cid-v2',
    title: 'Mesh routing', viewed: false,
  },
  {
    id: 'v3', creatorAddress: 'c3', creatorName: 'Carol',
    createdAt: vineBase + 300, videoCid: 'cid-v3',
    viewed: false,
  },
  {
    id: 'v4', creatorAddress: 'a1', creatorName: 'Alice',
    createdAt: vineBase + 600, videoCid: 'cid-v4',
    title: 'Cache explained', reshareOf: 'v2',
    originalCreatorAddress: 'b2', originalCreatorName: 'Bob',
    viewed: false,
  },
];
```

Replace the existing "shows reshare badge" test and add new integration tests:

```typescript
  // DELETE: it('shows reshare badge for reshared vines', () => { ... });

  it('shows reshare attribution for reshared vines', () => {
    renderFeed();
    expect(screen.getByText(/originally by/)).toBeTruthy();
    expect(screen.getByText('Bob')).toBeTruthy();
  });

  it('shows reshare count on original vine that was reshared', () => {
    renderFeed({ getReshareCount: (id: string) => id === 'v2' ? 1 : 0 });
    // v2 (Mesh routing by Bob) has 1 reshare
    // The count "1" should appear in the feed alongside the reshare icon
    const items = screen.getAllByRole('listitem');
    // v2 is third in sorted order (v4, v3, v2, v1)
    expect(items[2].textContent).toContain('1');
  });

  it('opens original vine in player when attribution is clicked', async () => {
    const { callbacks } = renderFeed();
    // Click Bob's name on the attribution row (v4 is a reshare of v2)
    await fireEvent.click(screen.getByText('Bob'));
    // Player should open with v2 (the original)
    expect(screen.getByRole('dialog', { name: 'Vine player' })).toBeTruthy();
    // v2's CID should be visible
    expect(screen.getByText('cid-v2')).toBeTruthy();
    // onMarkViewed should be called for v2
    expect(callbacks.onMarkViewed).toHaveBeenCalledWith('v2');
  });
```

Update the `renderFeed` helper to include the new props:

```typescript
function renderFeed(overrides: Record<string, unknown> = {}) {
  const callbacks = {
    onMarkViewed: vi.fn(),
    onPublish: vi.fn(),
    onReshare: vi.fn(),
    getReshareCount: ((_id: string) => 0) as (id: string) => number,
  };

  const result = render(VineFeed, {
    props: {
      followedVines: VINES,
      discoverVines: [],
      viewedIds: new Set<string>(),
      activeTab: 'following' as const,
      followedAddresses: new Set<string>(),
      ...callbacks,
      ...overrides,
    },
  });

  return { ...result, callbacks };
}
```

Update the existing integration test "calls onReshare when reshare button is clicked" to account for the confirmation dialog:

```typescript
  it('calls onReshare after reshare confirmation', async () => {
    const { callbacks } = renderFeed();
    callbacks.onReshare.mockResolvedValue(undefined);

    // Open the player
    const card = screen.getByLabelText('Transport demo by Alice');
    await fireEvent.click(card);

    // Click reshare button
    const reshareBtn = screen.getByLabelText('Reshare vine');
    await fireEvent.click(reshareBtn);

    // Confirm in dialog
    await fireEvent.click(screen.getByText('Reshare'));

    expect(callbacks.onReshare).toHaveBeenCalledWith(
      expect.objectContaining({ id: 'v1' })
    );
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/VineFeed.integration.test.ts`
Expected: FAIL — VineFeed doesn't accept `getReshareCount` or `onViewOriginal` props yet.

- [ ] **Step 3: Update VineFeed props and add internal handleViewOriginal**

In `src/lib/components/VineFeed.svelte`, add `getReshareCount` as an external prop. `onViewOriginal` is NOT an external prop — VineFeed handles it internally by looking up the vine and opening the player.

```typescript
  let {
    followedVines = [],
    discoverVines = [],
    viewedIds,
    activeTab = 'following' as FeedTab,
    followedAddresses = new Set<string>(),
    onTabChange,
    onMarkViewed,
    onPublish,
    onReshare,
    onFollow,
    onUnfollow,
    resolveVideo,
    getReaction,
    onToggleLike,
    getReshareCount,
  }: {
    followedVines?: VineVideo[];
    discoverVines?: VineVideo[];
    viewedIds: Set<string>;
    activeTab?: FeedTab;
    followedAddresses?: Set<string>;
    onTabChange?: (tab: FeedTab) => void;
    onMarkViewed?: (id: string) => void;
    onPublish?: () => void;
    onReshare?: (vine: VineVideo) => Promise<void> | void;
    onFollow?: (address: string, name: string) => void;
    onUnfollow?: (address: string) => void;
    resolveVideo?: (cid: string) => Promise<string>;
    getReaction?: (vineId: string) => { count: number; likedByMe: boolean };
    onToggleLike?: (vine: VineVideo) => void;
    getReshareCount?: (vineId: string) => number;
  } = $props();
```

Add a `handleViewOriginal` function inside the `<script>` block (after `previousVine`):

```typescript
  function handleViewOriginal(vineId: string) {
    const allVines = [...followedVines, ...discoverVines];
    const original = allVines.find(v => v.id === vineId);
    if (original) openPlayer(original);
  }
```

- [ ] **Step 4: Pass new props to VineCard**

In VineFeed's template, update the VineCard rendering inside the `{#each}` block to pass the new props. `onViewOriginal` uses the internal `handleViewOriginal`:

```svelte
      {#each filteredVines as vine (vine.id)}
        {@const reaction = getReaction?.(vine.id)}
        <div role="listitem">
          <VineCard
            {vine}
            onPlay={openPlayer}
            isViewed={viewedIds.has(vine.id)}
            showFollowButton={vine.creatorAddress !== 'self'}
            isFollowed={followedAddresses.has(vine.creatorAddress)}
            {onFollow}
            {onUnfollow}
            reactionCount={reaction?.count ?? 0}
            likedByMe={reaction?.likedByMe ?? false}
            {onToggleLike}
            reshareCount={getReshareCount?.(vine.id) ?? 0}
            onViewOriginal={handleViewOriginal}
          />
        </div>
      {/each}
```

- [ ] **Step 5: Pass new props to VinePlayer**

Update the VinePlayer rendering to pass `onViewOriginal` using the internal handler:

```svelte
{#if activeVine}
  <VinePlayer
    vine={activeVine}
    onClose={closePlayer}
    onNext={activeIndex >= 0 && activeIndex < playerList.length - 1 ? nextVine : undefined}
    onPrevious={activeIndex > 0 ? previousVine : undefined}
    {onReshare}
    {resolveVideo}
    reactionCount={getReaction?.(activeVine.id)?.count ?? 0}
    likedByMe={getReaction?.(activeVine.id)?.likedByMe ?? false}
    {onToggleLike}
    onViewOriginal={handleViewOriginal}
  />
{/if}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/VineFeed.integration.test.ts`
Expected: All tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/lib/components/VineFeed.svelte src/lib/components/__tests__/VineFeed.integration.test.ts
git commit -m "feat: thread reshare count and view-original props through VineFeed"
```

---

### Task 7: App.svelte Wiring

**Files:**
- Modify: `src/App.svelte:61-104` (vine service state and handlers)

- [ ] **Step 1: Add vineGetReshareCount reactive state**

In `src/App.svelte`, after the `vineGetReaction` declaration (around line 69), add:

```typescript
  let vineGetReshareCount = $state<(vineId: string) => number>(
    (vineId: string) => vineService.getReshareCount(vineId)
  );
```

Update the `vineService.onChange` callback to reassign it:

```typescript
  vineService.onChange = () => {
    followedVines = [...vineService.followedVines];
    discoverVines = [...vineService.discoverVines];
    vineViewedIds = new Set(vineService.viewedIds);
    followedAddresses = new Set(vineService.followedAddresses);
    vineGetReaction = (vineId: string) => vineService.getReaction(vineId);
    vineGetReshareCount = (vineId: string) => vineService.getReshareCount(vineId);
  };
```

- [ ] **Step 2: Update handleVineReshare to pass original creator info**

Replace the existing `handleVineReshare` function:

```typescript
  async function handleVineReshare(vine: import('./lib/types').VineVideo) {
    // Self-reshare guard: don't reshare your own original content
    if (vine.creatorAddress === 'self' && !vine.reshareOf) return;

    // Carry attribution through — if this is already a reshare, use its
    // original creator; otherwise the vine's own creator is the original.
    const originalAddr = vine.originalCreatorAddress ?? vine.creatorAddress;
    const originalName = vine.originalCreatorName ?? vine.creatorName;

    try {
      await vineService.publish(vine.videoCid, vine.title, vine.id, originalAddr, originalName);
    } catch (err) {
      console.error('Vine reshare failed', err);
      throw err;
    }
  }
```

- [ ] **Step 3: Pass getReshareCount to VineFeed**

Note: `onViewOriginal` is handled internally by VineFeed (added in Task 6). App.svelte does NOT pass an `onViewOriginal` prop.

In `src/App.svelte`, update the VineFeed rendering:

```svelte
    <VineFeed
      {followedVines}
      {discoverVines}
      viewedIds={vineViewedIds}
      activeTab={vineTab}
      {followedAddresses}
      onTabChange={(tab) => { vineTab = tab; }}
      onMarkViewed={handleMarkVineViewed}
      onPublish={() => showVinePublish = true}
      onReshare={handleVineReshare}
      onFollow={handleVineFollow}
      onUnfollow={handleVineUnfollow}
      getReaction={vineGetReaction}
      onToggleLike={handleVineToggleLike}
      resolveVideo={resolveVideoFn}
      getReshareCount={vineGetReshareCount}
    />
```

Note: `onViewOriginal` is NOT passed from App.svelte — VineFeed handles it internally (see Task 6).

- [ ] **Step 4: Run all tests**

Run: `npx vitest run`
Expected: All tests pass across all test files.

- [ ] **Step 5: Commit**

```bash
git add src/App.svelte
git commit -m "feat: wire reshare count and attribution through App.svelte"
```
