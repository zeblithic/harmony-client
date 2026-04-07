# Vine Like/Reaction System Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a single like/heart toggle to vines with optimistic UI, Zenoh pub/sub distribution, and in-memory reaction state.

**Architecture:** Rust backend handles Zenoh publish/subscribe for reactions via a new `publish_vine_reaction` Tauri command and a `harmony/vines/*/reactions/**` wildcard subscription. TypeScript VineService tracks reaction counts in-memory with optimistic updates. VineCard and VinePlayer both render a heart toggle + count.

**Tech Stack:** Rust / Tauri v2 / Svelte 5 (runes) / Zenoh pub/sub / Vitest / @testing-library/svelte

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `src-tauri/src/lib.rs` | Modify | Add `VineReactionPayload`, `PublishReactionPayload` structs + `publish_vine_reaction` Tauri command. Add reaction key routing in `emit_frontend_event`. Register command in `.invoke_handler()`. |
| `src-tauri/src/event_loop.rs` | Modify | Add `harmony/vines/*/reactions/**` subscription at startup. |
| `src/lib/vine-service.ts` | Modify | Add `reactionMap`, `toggleLike()`, `getReaction()`, reaction event listener, self-echo dedup. |
| `src/lib/vine-service.test.ts` | Modify | Add tests for toggleLike, getReaction, incoming reactions, self-echo dedup, unlike, offline. |
| `src/lib/components/VineCard.svelte` | Modify | Add like row (heart button + count) with stopPropagation. |
| `src/lib/components/__tests__/VineCard.test.ts` | Modify | Add tests for like display states and click behavior. |
| `src/lib/components/VinePlayer.svelte` | Modify | Add like button in footer-actions alongside Reshare. |
| `src/lib/components/__tests__/VinePlayer.test.ts` | Modify | Add tests for like button in player. |
| `src/lib/components/VineFeed.svelte` | Modify | Thread reaction props (getReaction, onToggleLike) to VineCard and VinePlayer. |
| `src/lib/components/__tests__/VineFeed.test.ts` | Modify | Add tests for reaction prop threading. |
| `src/App.svelte` | Modify | Wire VineService reaction state into VineFeed. |

---

### Task 1: Rust — Reaction Payload Types & Serialization Tests

**Files:**
- Modify: `src-tauri/src/lib.rs:574-618` (vine types section)

- [ ] **Step 1: Write failing tests for VineReactionPayload serialization**

Add these tests to the existing `mod tests` block at the bottom of `src-tauri/src/lib.rs` (after the `publish_vine_payload_creator_name_defaults` test around line 1358):

```rust
#[test]
fn vine_reaction_payload_roundtrip() {
    let reaction = VineReactionPayload {
        vine_id: "vine-abc-1234".to_string(),
        reactor_address: "deadbeef01020304".to_string(),
        reactor_name: "Alice".to_string(),
        liked: true,
        timestamp: 1711600000,
    };
    let json = serde_json::to_vec(&reaction).unwrap();
    let parsed: VineReactionPayload = serde_json::from_slice(&json).unwrap();
    assert_eq!(parsed.vine_id, "vine-abc-1234");
    assert_eq!(parsed.reactor_address, "deadbeef01020304");
    assert_eq!(parsed.reactor_name, "Alice");
    assert!(parsed.liked);
    assert_eq!(parsed.timestamp, 1711600000);
}

#[test]
fn vine_reaction_payload_camel_case() {
    let reaction = VineReactionPayload {
        vine_id: "vine-1".to_string(),
        reactor_address: "aa".to_string(),
        reactor_name: "Bob".to_string(),
        liked: false,
        timestamp: 0,
    };
    let json = String::from_utf8(serde_json::to_vec(&reaction).unwrap()).unwrap();
    assert!(json.contains("\"vineId\""), "expected camelCase: {json}");
    assert!(json.contains("\"reactorAddress\""), "expected camelCase: {json}");
    assert!(json.contains("\"reactorName\""), "expected camelCase: {json}");
    assert!(!json.contains("\"vine_id\""), "unexpected snake_case: {json}");
    assert!(!json.contains("\"reactor_address\""), "unexpected snake_case: {json}");
}

#[test]
fn publish_reaction_payload_deserialize() {
    let json = r#"{
        "vineId": "vine-abc",
        "vineCreatorAddress": "deadbeef",
        "liked": true
    }"#;
    let parsed: PublishReactionPayload = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.vine_id, "vine-abc");
    assert_eq!(parsed.vine_creator_address, "deadbeef");
    assert!(parsed.liked);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo test vine_reaction 2>&1`
Expected: FAIL — `VineReactionPayload` and `PublishReactionPayload` not found

- [ ] **Step 3: Add the payload structs**

Add these structs in `src-tauri/src/lib.rs` after the `FollowEntryResponse` struct (around line 626), before the `publish_vine` function:

```rust
/// Vine reaction published/received over Zenoh.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VineReactionPayload {
    pub vine_id: String,
    pub reactor_address: String,
    pub reactor_name: String,
    pub liked: bool,
    pub timestamp: u64,
}

/// Vine reaction sent from the frontend to publish.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishReactionPayload {
    pub vine_id: String,
    pub vine_creator_address: String,
    pub liked: bool,
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo test vine_reaction 2>&1`
Expected: 3 tests PASS

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat: add VineReactionPayload and PublishReactionPayload types"
```

---

### Task 2: Rust — `publish_vine_reaction` Tauri Command

**Files:**
- Modify: `src-tauri/src/lib.rs:628-692` (vine commands section) and `~1050` (invoke_handler)

- [ ] **Step 1: Add the `publish_vine_reaction` command**

Add this function after the `publish_vine` function (after line 692), before `list_vine_videos`:

```rust
/// Publish a vine reaction (like/unlike) to the mesh network via Zenoh pub/sub.
///
/// Publishes JSON to `harmony/vines/{vine_creator_address}/reactions/{vine_id}/{own_addr}`.
#[tauri::command]
async fn publish_vine_reaction(
    reaction: PublishReactionPayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    if reaction.vine_id.trim().is_empty() {
        return Err("vine_id is required".to_string());
    }
    if reaction.vine_creator_address.trim().is_empty() {
        return Err("vine_creator_address is required".to_string());
    }

    let (publish_tx, node_addr, display_name) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        let tx = guard
            .publish_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?;
        // The display name is set by the frontend via publish_profile; fall back to truncated address.
        let name = if guard.node_addr.is_empty() {
            "Unknown".to_string()
        } else {
            guard.node_addr[..8.min(guard.node_addr.len())].to_string()
        };
        (tx, guard.node_addr.clone(), name)
    };

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let wire = VineReactionPayload {
        vine_id: reaction.vine_id.clone(),
        reactor_address: node_addr.clone(),
        reactor_name: display_name,
        liked: reaction.liked,
        timestamp: now_secs,
    };

    let key_expr = format!(
        "harmony/vines/{}/reactions/{}/{}",
        reaction.vine_creator_address, reaction.vine_id, node_addr
    );
    let payload = serde_json::to_vec(&wire).map_err(|e| format!("serialize: {e}"))?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    publish_tx
        .send(event_loop::PublishRequest {
            key_expr,
            payload,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;

    reply_rx
        .await
        .map_err(|_| "event loop dropped publish request".to_string())?
}
```

- [ ] **Step 2: Register the command in the invoke handler**

In the `tauri::Builder` chain (around line 1050), add `publish_vine_reaction` to the `.invoke_handler(tauri::generate_handler![...])` list, after `publish_vine`:

```rust
            publish_vine,
            publish_vine_reaction,
```

- [ ] **Step 3: Verify compilation**

Run: `cd src-tauri && cargo check 2>&1`
Expected: Compiles without errors

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat: add publish_vine_reaction Tauri command"
```

---

### Task 3: Rust — Reaction Subscription & Event Routing

**Files:**
- Modify: `src-tauri/src/event_loop.rs:195-230` (subscription setup) and `~668-694` (emit_frontend_event)

- [ ] **Step 1: Add the reaction wildcard subscription**

In `src-tauri/src/event_loop.rs`, after the vine descriptor subscription block (after line 208, before the comment about per-creator subscriptions around line 210), add:

```rust
    // Subscribe to vine reactions (likes/unlikes).
    dispatch_action(
        RuntimeAction::Subscribe {
            key_expr: "harmony/vines/*/reactions/**".to_string(),
        },
        &session,
        &zenoh_tx,
        &udp,
        &broadcast_addr,
        &app,
        &closing,
        &own_zid,
    )
    .await;
```

- [ ] **Step 2: Add reaction event routing in `emit_frontend_event`**

In `src-tauri/src/event_loop.rs`, inside `emit_frontend_event`, the `harmony/vines/` branch (around line 668) currently handles vine descriptors. The reaction keys are longer (`harmony/vines/{addr}/reactions/{cid}/{reactor}`), so they will also match the `harmony/vines/` prefix check. We need to distinguish them.

Replace the existing vine branch:

```rust
    } else if key_expr.starts_with("harmony/vines/") {
        // Deserialize as typed payload first to reject malformed data,
        // then re-serialize with the source tag injected.
        if let Ok(vine) = serde_json::from_slice::<crate::VineDescriptorPayload>(payload) {
            // ... existing vine descriptor routing ...
        }
    }
```

With a version that checks for the `/reactions/` sub-path first:

```rust
    } else if key_expr.starts_with("harmony/vines/") {
        if key_expr.contains("/reactions/") {
            // Vine reaction event — emit directly to frontend.
            if let Ok(reaction) = serde_json::from_slice::<crate::VineReactionPayload>(payload) {
                let _ = app.emit("vine-reaction-received", &reaction);
            }
        } else {
            // Vine descriptor — deserialize as typed payload first to reject malformed data,
            // then re-serialize with the source tag injected.
            if let Ok(vine) = serde_json::from_slice::<crate::VineDescriptorPayload>(payload) {
                let is_followed = {
                    let set = followed_set.lock().unwrap();
                    set.contains(vine.creator_address.as_str())
                };
                let source = if is_followed { "followed" } else { "discover" };
                // Re-serialize to Value so we can inject the source field
                if let Ok(mut val) = serde_json::to_value(&vine) {
                    if let Some(obj) = val.as_object_mut() {
                        obj.insert("source".to_string(), serde_json::Value::String(source.to_string()));
                    }
                    let _ = app.emit("vine-received", &val);
                }
            }
        }
    }
```

- [ ] **Step 3: Verify compilation**

Run: `cd src-tauri && cargo check 2>&1`
Expected: Compiles without errors

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/event_loop.rs
git commit -m "feat: add vine reaction subscription and event routing"
```

---

### Task 4: VineService — Reaction State & toggleLike

**Files:**
- Modify: `src/lib/vine-service.ts`
- Test: `src/lib/vine-service.test.ts`

- [ ] **Step 1: Write failing tests for reaction state and toggleLike**

Add these tests to `src/lib/vine-service.test.ts` at the end of the `describe('VineService', ...)` block:

```typescript
  // ── Reactions ──────────────────────────────────────────────────────

  it('getReaction returns zero state for unknown vine', () => {
    const r = svc.getReaction('nonexistent');
    expect(r.count).toBe(0);
    expect(r.likedByMe).toBe(false);
  });

  it('toggleLike optimistically sets likedByMe and increments count', async () => {
    const vine = mockVines[0];
    svc.onChange = vi.fn();
    await svc.toggleLike(vine);
    const r = svc.getReaction(vine.id);
    expect(r.likedByMe).toBe(true);
    expect(r.count).toBe(1);
    expect(svc.onChange).toHaveBeenCalled();
  });

  it('toggleLike again unlikes and decrements count', async () => {
    const vine = mockVines[0];
    await svc.toggleLike(vine);
    await svc.toggleLike(vine);
    const r = svc.getReaction(vine.id);
    expect(r.likedByMe).toBe(false);
    expect(r.count).toBe(0);
  });

  it('toggleLike calls publish_vine_reaction on adapter', async () => {
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    const vine = mockVines[0];
    await svc.toggleLike(vine);
    expect(adapter.invoke).toHaveBeenCalledWith('publish_vine_reaction', {
      reaction: {
        vineId: vine.id,
        vineCreatorAddress: vine.creatorAddress,
        liked: true,
      },
    });
  });

  it('toggleLike works offline without adapter', async () => {
    const vine = mockVines[0];
    await svc.toggleLike(vine);
    expect(svc.getReaction(vine.id).likedByMe).toBe(true);
  });

  it('toggleLike rolls back on adapter error', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('network error'));
    await svc.connectAdapter(adapter);
    const vine = mockVines[0];
    await svc.toggleLike(vine);
    const r = svc.getReaction(vine.id);
    expect(r.likedByMe).toBe(false);
    expect(r.count).toBe(0);
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/vine-service.test.ts 2>&1`
Expected: FAIL — `toggleLike` and `getReaction` not found on VineService

- [ ] **Step 3: Implement reaction state and toggleLike**

In `src/lib/vine-service.ts`, add these fields after the `followedAddresses` field (around line 40):

```typescript
  /** In-memory reaction state per vine. */
  private reactionMap = new Map<string, { count: number; likedByMe: boolean; reactors: Set<string> }>();
```

Add these methods after the `isFollowed` method (around line 186):

```typescript
  /** Get reaction state for a vine. Returns zero state if no reactions tracked. */
  getReaction(vineId: string): { count: number; likedByMe: boolean } {
    const entry = this.reactionMap.get(vineId);
    return entry
      ? { count: entry.count, likedByMe: entry.likedByMe }
      : { count: 0, likedByMe: false };
  }

  /** Toggle like on a vine with optimistic update. */
  async toggleLike(vine: VineVideo): Promise<void> {
    const entry = this.reactionMap.get(vine.id) ?? { count: 0, likedByMe: false, reactors: new Set<string>() };
    const wasLiked = entry.likedByMe;
    const newLiked = !wasLiked;

    // Optimistic update
    entry.likedByMe = newLiked;
    entry.count = Math.max(0, entry.count + (newLiked ? 1 : -1));
    if (newLiked) {
      entry.reactors.add('self');
    } else {
      entry.reactors.delete('self');
    }
    this.reactionMap.set(vine.id, entry);
    this.onChange?.();

    if (this.adapter) {
      try {
        await this.adapter.invoke('publish_vine_reaction', {
          reaction: {
            vineId: vine.id,
            vineCreatorAddress: vine.creatorAddress,
            liked: newLiked,
          },
        });
      } catch {
        // Rollback on failure
        entry.likedByMe = wasLiked;
        entry.count = Math.max(0, entry.count + (wasLiked ? 1 : -1));
        if (wasLiked) {
          entry.reactors.add('self');
        } else {
          entry.reactors.delete('self');
        }
        this.reactionMap.set(vine.id, entry);
        this.onChange?.();
      }
    }
  }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/vine-service.test.ts 2>&1`
Expected: All tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/lib/vine-service.ts src/lib/vine-service.test.ts
git commit -m "feat: add reaction state and toggleLike to VineService"
```

---

### Task 5: VineService — Incoming Reaction Listener

**Files:**
- Modify: `src/lib/vine-service.ts`
- Test: `src/lib/vine-service.test.ts`

- [ ] **Step 1: Write failing tests for incoming reaction handling**

Add these tests to `src/lib/vine-service.test.ts` in the reactions section:

```typescript
  it('incoming reaction increments count', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('vine-reaction-received', {
      vineId: mockVines[0].id,
      reactorAddress: 'peer-abc',
      reactorName: 'Peer',
      liked: true,
      timestamp: 1700000500,
    });
    const r = svc.getReaction(mockVines[0].id);
    expect(r.count).toBe(1);
    expect(r.likedByMe).toBe(false);
  });

  it('incoming reaction deduplicates by reactor address', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    const event = {
      vineId: mockVines[0].id,
      reactorAddress: 'peer-abc',
      reactorName: 'Peer',
      liked: true,
      timestamp: 1700000500,
    };
    emit('vine-reaction-received', event);
    emit('vine-reaction-received', event);
    expect(svc.getReaction(mockVines[0].id).count).toBe(1);
  });

  it('incoming unlike decrements count', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('vine-reaction-received', {
      vineId: mockVines[0].id,
      reactorAddress: 'peer-abc',
      reactorName: 'Peer',
      liked: true,
      timestamp: 1700000500,
    });
    emit('vine-reaction-received', {
      vineId: mockVines[0].id,
      reactorAddress: 'peer-abc',
      reactorName: 'Peer',
      liked: false,
      timestamp: 1700000600,
    });
    expect(svc.getReaction(mockVines[0].id).count).toBe(0);
  });

  it('skips self-echo reactions', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.ownAddress = 'myaddr';
    await svc.connectAdapter(adapter);
    // Manually set a like so we can verify no double-count
    const vine = mockVines[0];
    await svc.toggleLike(vine);
    expect(svc.getReaction(vine.id).count).toBe(1);
    // Self-echo arrives from network
    emit('vine-reaction-received', {
      vineId: vine.id,
      reactorAddress: 'myaddr',
      reactorName: 'You',
      liked: true,
      timestamp: 1700000500,
    });
    // Count should still be 1, not 2
    expect(svc.getReaction(vine.id).count).toBe(1);
  });

  it('ignores reactions for unknown vine IDs', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.onChange = vi.fn();
    await svc.connectAdapter(adapter);
    (svc.onChange as ReturnType<typeof vi.fn>).mockClear();
    emit('vine-reaction-received', {
      vineId: 'nonexistent-vine',
      reactorAddress: 'peer-abc',
      reactorName: 'Peer',
      liked: true,
      timestamp: 1700000500,
    });
    // onChange should not have been called for unknown vine
    expect(svc.onChange).not.toHaveBeenCalled();
  });

  it('calls onChange when reaction arrives', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.onChange = vi.fn();
    await svc.connectAdapter(adapter);
    (svc.onChange as ReturnType<typeof vi.fn>).mockClear();
    emit('vine-reaction-received', {
      vineId: mockVines[0].id,
      reactorAddress: 'peer-xyz',
      reactorName: 'Peer',
      liked: true,
      timestamp: 1700000500,
    });
    expect(svc.onChange).toHaveBeenCalledOnce();
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/vine-service.test.ts 2>&1`
Expected: FAIL — incoming reaction events are not handled yet

- [ ] **Step 3: Add reaction event listener in connectAdapter**

In `src/lib/vine-service.ts`, inside the `connectAdapter` method (around line 56-76), after the existing `adapter.listen('vine-received', ...)` block, add a second listener:

```typescript
    const unlistenReaction = await adapter.listen(
      'vine-reaction-received',
      (event) => {
        const wire = event.payload as {
          vineId: string;
          reactorAddress: string;
          reactorName: string;
          liked: boolean;
          timestamp: number;
        };

        // Skip self-echo — already applied optimistically
        if (wire.reactorAddress === 'self' || (this.ownAddress && wire.reactorAddress === this.ownAddress)) {
          return;
        }

        // Ignore reactions for vines not in our feed
        const known = this.followedVines.some(v => v.id === wire.vineId)
          || this.discoverVines.some(v => v.id === wire.vineId);
        if (!known) return;

        const entry = this.reactionMap.get(wire.vineId)
          ?? { count: 0, likedByMe: false, reactors: new Set<string>() };

        const alreadyTracked = entry.reactors.has(wire.reactorAddress);

        if (wire.liked) {
          if (alreadyTracked) return; // Already counted
          entry.reactors.add(wire.reactorAddress);
          entry.count += 1;
        } else {
          if (!alreadyTracked) return; // Nothing to remove
          entry.reactors.delete(wire.reactorAddress);
          entry.count = Math.max(0, entry.count - 1);
        }

        this.reactionMap.set(wire.vineId, entry);
        this.onChange?.();
      },
    );
    this.unlisteners.push(unlistenReaction);
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/vine-service.test.ts 2>&1`
Expected: All tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/lib/vine-service.ts src/lib/vine-service.test.ts
git commit -m "feat: handle incoming vine reactions with dedup and self-echo suppression"
```

---

### Task 6: VineCard — Like Button

**Files:**
- Modify: `src/lib/components/VineCard.svelte`
- Test: `src/lib/components/__tests__/VineCard.test.ts`

- [ ] **Step 1: Write failing tests for VineCard like button**

Add these tests to `src/lib/components/__tests__/VineCard.test.ts`:

```typescript
  it('shows like count when count > 0', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), reactionCount: 3, likedByMe: false } });
    expect(screen.getByText('3')).toBeTruthy();
  });

  it('shows filled heart when liked by me', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), reactionCount: 1, likedByMe: true } });
    expect(screen.getByLabelText('Unlike Demo vine')).toBeTruthy();
  });

  it('shows outline heart when not liked by me', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), reactionCount: 1, likedByMe: false } });
    expect(screen.getByLabelText('Like Demo vine')).toBeTruthy();
  });

  it('hides like row when count is 0 and not liked', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), reactionCount: 0, likedByMe: false } });
    expect(screen.queryByLabelText(/Like/)).toBeNull();
    expect(screen.queryByLabelText(/Unlike/)).toBeNull();
  });

  it('calls onToggleLike when heart clicked', async () => {
    const onToggleLike = vi.fn();
    render(VineCard, { props: { vine, onPlay: vi.fn(), reactionCount: 1, likedByMe: false, onToggleLike } });
    await fireEvent.click(screen.getByLabelText('Like Demo vine'));
    expect(onToggleLike).toHaveBeenCalledWith(vine);
  });

  it('like button click does not trigger onPlay', async () => {
    const onPlay = vi.fn();
    const onToggleLike = vi.fn();
    render(VineCard, { props: { vine, onPlay, reactionCount: 1, likedByMe: false, onToggleLike } });
    await fireEvent.click(screen.getByLabelText('Like Demo vine'));
    expect(onPlay).not.toHaveBeenCalled();
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/VineCard.test.ts 2>&1`
Expected: FAIL — new props and elements don't exist yet

- [ ] **Step 3: Add like props and UI to VineCard**

In `src/lib/components/VineCard.svelte`, update the props destructuring (around line 6-14) to add the new props:

```typescript
  let { vine, onPlay, isViewed, showFollowButton = false, isFollowed = false, onFollow, onUnfollow, reactionCount = 0, likedByMe = false, onToggleLike }: {
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
  } = $props();
```

Add a like click handler after `handleFollowClick` (around line 40):

```typescript
  function handleLikeClick(e: MouseEvent) {
    e.stopPropagation();
    onToggleLike?.(vine);
  }
```

Add the like row in the template, after the follow button block (after the `{#if showFollowButton}...{/if}` block, before the closing `</div>` of `.card-info`):

```svelte
    {#if reactionCount > 0 || likedByMe}
      <div class="card-like-row">
        <button
          type="button"
          class="card-heart"
          onclick={handleLikeClick}
          aria-label={likedByMe ? `Unlike ${vine.title ?? 'vine'}` : `Like ${vine.title ?? 'vine'}`}
        >
          {likedByMe ? '❤️' : '🤍'}
        </button>
        <span class="card-like-count">{reactionCount}</span>
      </div>
    {/if}
```

Add the styles at the end of the `<style>` block:

```css
  .card-like-row {
    display: flex;
    align-items: center;
    gap: 4px;
    margin-top: 2px;
  }

  .card-heart {
    background: none;
    border: none;
    cursor: pointer;
    font-size: 0.8rem;
    padding: 0;
    line-height: 1;
    transition: transform 0.15s;
  }

  .card-heart:hover {
    transform: scale(1.2);
  }

  .card-like-count {
    font-size: 0.7rem;
    color: var(--text-muted);
    font-weight: 500;
  }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/VineCard.test.ts 2>&1`
Expected: All tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/VineCard.svelte src/lib/components/__tests__/VineCard.test.ts
git commit -m "feat: add like heart button and count to VineCard"
```

---

### Task 7: VinePlayer — Like Button

**Files:**
- Modify: `src/lib/components/VinePlayer.svelte`
- Test: `src/lib/components/__tests__/VinePlayer.test.ts`

- [ ] **Step 1: Write failing tests for VinePlayer like button**

Add these tests to `src/lib/components/__tests__/VinePlayer.test.ts`:

```typescript
  // ── Like button ──────────────────────────────────────────────────

  it('renders like button when onToggleLike provided', () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn(), onToggleLike: vi.fn(), reactionCount: 0, likedByMe: false } });
    expect(screen.getByLabelText('Like Demo vine')).toBeTruthy();
  });

  it('does not render like button when onToggleLike absent', () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn() } });
    expect(screen.queryByLabelText(/Like/)).toBeNull();
    expect(screen.queryByLabelText(/Unlike/)).toBeNull();
  });

  it('shows filled heart when liked', () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn(), onToggleLike: vi.fn(), reactionCount: 3, likedByMe: true } });
    expect(screen.getByLabelText('Unlike Demo vine')).toBeTruthy();
  });

  it('shows reaction count', () => {
    render(VinePlayer, { props: { vine, onClose: vi.fn(), onToggleLike: vi.fn(), reactionCount: 5, likedByMe: false } });
    expect(screen.getByText('5')).toBeTruthy();
  });

  it('calls onToggleLike when like button clicked', async () => {
    const onToggleLike = vi.fn();
    render(VinePlayer, { props: { vine, onClose: vi.fn(), onToggleLike, reactionCount: 0, likedByMe: false } });
    await fireEvent.click(screen.getByLabelText('Like Demo vine'));
    expect(onToggleLike).toHaveBeenCalledWith(vine);
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/VinePlayer.test.ts 2>&1`
Expected: FAIL — new props and elements don't exist yet

- [ ] **Step 3: Add like props and UI to VinePlayer**

In `src/lib/components/VinePlayer.svelte`, update the props destructuring (around line 7-14) to add like props:

```typescript
  let { vine, onClose, onNext, onPrevious, onReshare, resolveVideo, onToggleLike, reactionCount = 0, likedByMe = false }: {
    vine: VineVideo;
    onClose: () => void;
    onNext?: () => void;
    onPrevious?: () => void;
    onReshare?: (vine: VineVideo) => Promise<void> | void;
    resolveVideo?: (cid: string) => Promise<string>;
    onToggleLike?: (vine: VineVideo) => void;
    reactionCount?: number;
    likedByMe?: boolean;
  } = $props();
```

In the template, add the like button in `footer-actions` (around line 149), before the reshare button:

```svelte
      {#if onToggleLike}
        <button
          type="button"
          class="action-btn like-btn"
          class:liked={likedByMe}
          onclick={() => onToggleLike?.(vine)}
          aria-label={likedByMe ? `Unlike ${vine.title ?? 'vine'}` : `Like ${vine.title ?? 'vine'}`}
        >
          <span class="heart">{likedByMe ? '❤️' : '🤍'}</span>
          {#if reactionCount > 0}
            <span class="like-count">{reactionCount}</span>
          {/if}
        </button>
      {/if}
```

Add the styles at the end of the `<style>` block:

```css
  .like-btn.liked {
    color: #ed4245;
    border-color: rgba(237, 66, 69, 0.3);
  }

  .heart {
    font-size: 1rem;
  }

  .like-count {
    font-size: 0.8rem;
  }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/VinePlayer.test.ts 2>&1`
Expected: All tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/VinePlayer.svelte src/lib/components/__tests__/VinePlayer.test.ts
git commit -m "feat: add like button to VinePlayer footer"
```

---

### Task 8: VineFeed — Thread Reaction Props

**Files:**
- Modify: `src/lib/components/VineFeed.svelte`
- Test: `src/lib/components/__tests__/VineFeed.test.ts`

- [ ] **Step 1: Write failing tests for reaction prop threading**

Add these tests to `src/lib/components/__tests__/VineFeed.test.ts`:

```typescript
  it('passes reaction data to vine cards', () => {
    const getReaction = vi.fn().mockReturnValue({ count: 5, likedByMe: true });
    render(VineFeed, { props: {
      followedVines: vines, viewedIds: makeViewedIds(),
      activeTab: 'following', followedAddresses: new Set(), getReaction,
    } });
    // getReaction should be called for each vine in the feed
    expect(getReaction).toHaveBeenCalled();
    // Should show the count from getReaction
    const counts = screen.getAllByText('5');
    expect(counts.length).toBeGreaterThan(0);
  });

  it('calls onToggleLike when card like is clicked', async () => {
    const onToggleLike = vi.fn();
    const getReaction = vi.fn().mockReturnValue({ count: 1, likedByMe: false });
    render(VineFeed, { props: {
      followedVines: vines, viewedIds: makeViewedIds(),
      activeTab: 'following', followedAddresses: new Set(),
      getReaction, onToggleLike,
    } });
    // Carol's vine is newest → first card — click its like button
    const likeBtn = screen.getAllByLabelText(/Like/)[0];
    await fireEvent.click(likeBtn);
    expect(onToggleLike).toHaveBeenCalled();
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/VineFeed.test.ts 2>&1`
Expected: FAIL — `getReaction` and `onToggleLike` props not recognized

- [ ] **Step 3: Add reaction props to VineFeed**

In `src/lib/components/VineFeed.svelte`, update the props destructuring (around line 9-35) to add the new props:

Add `getReaction` and `onToggleLike` to both the destructuring and the type annotation:

```typescript
    getReaction,
    onToggleLike,
```

In the type annotation block:

```typescript
    getReaction?: (vineId: string) => { count: number; likedByMe: boolean };
    onToggleLike?: (vine: VineVideo) => void;
```

In the template, update the `<VineCard>` inside the `{#each}` block (around line 136-144) to pass reaction data:

```svelte
          <VineCard
            {vine}
            onPlay={openPlayer}
            isViewed={viewedIds.has(vine.id)}
            showFollowButton={vine.creatorAddress !== 'self'}
            isFollowed={followedAddresses.has(vine.creatorAddress)}
            {onFollow}
            {onUnfollow}
            reactionCount={getReaction?.(vine.id)?.count ?? 0}
            likedByMe={getReaction?.(vine.id)?.likedByMe ?? false}
            {onToggleLike}
          />
```

Update the `<VinePlayer>` block (around line 152-160) to pass reaction data:

```svelte
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
  />
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/VineFeed.test.ts 2>&1`
Expected: All tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/VineFeed.svelte src/lib/components/__tests__/VineFeed.test.ts
git commit -m "feat: thread reaction props through VineFeed to VineCard and VinePlayer"
```

---

### Task 9: App.svelte — Wire Reaction State

**Files:**
- Modify: `src/App.svelte`

- [ ] **Step 1: Add reaction state to the onChange handler**

In `src/App.svelte`, add a reactive state variable for the getReaction function after the `followedAddresses` state (around line 68):

```typescript
  let vineGetReaction = $state<(vineId: string) => { count: number; likedByMe: boolean }>(
    (vineId: string) => vineService.getReaction(vineId)
  );
```

In the `vineService.onChange` callback (around line 70-75), add a line to trigger reactivity by reassigning `vineGetReaction`:

```typescript
    vineGetReaction = (vineId: string) => vineService.getReaction(vineId);
```

- [ ] **Step 2: Add the toggleLike handler**

After the `handleVineUnfollow` function (around line 116), add:

```typescript
  function handleVineToggleLike(vine: import('./lib/types').VineVideo) {
    vineService.toggleLike(vine).catch((err) => {
      console.error('Toggle like failed', err);
    });
  }
```

- [ ] **Step 3: Pass reaction props to VineFeed**

In the `vineFeed` snippet (around line 629-643), add the new props to the `<VineFeed>` component:

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
    />
```

- [ ] **Step 4: Verify all tests pass**

Run: `npx vitest run 2>&1`
Expected: All test suites PASS

- [ ] **Step 5: Commit**

```bash
git add src/App.svelte
git commit -m "feat: wire vine reaction state from VineService into VineFeed"
```
