# Vine Follow/Feed System Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a follow/unfollow system for vine creators with a two-feed UI (Following + Discover), per-creator Zenoh subscriptions, JSON persistence, and source-tagged vine routing.

**Architecture:** Rust backend owns follow state (FollowManager) and Zenoh subscriptions. A new `follow_tx` channel sends follow/unfollow messages to the event loop. The frontend VineService splits into two vine arrays routed by a `source` tag on the `vine-received` IPC event. VineFeed gets top-level Following/Discover tabs with contextual follow buttons on VineCard.

**Tech Stack:** Rust (Tauri v2, Zenoh, serde_json, tokio mpsc), TypeScript (Svelte 5, Vitest)

**Spec:** `docs/specs/2026-04-07-vine-follow-feed-design.md`

---

## File Structure

### New files
- `src-tauri/src/follows.rs` — FollowManager struct: load/save JSON, follow/unfollow, name updates
- (No new frontend files — all changes extend existing ones)

### Modified files
- `src-tauri/src/lib.rs` — Add FollowRequest channel to NodeState, new Tauri commands, wire FollowManager into app state
- `src-tauri/src/event_loop.rs` — Accept follow_rx channel, create per-creator Zenoh subs, tag vine source, suppress wildcard dupes
- `src/lib/vine-service.ts` — Split vines into followedVines/discoverVines, add follow/unfollow methods, route by source
- `src/lib/types.ts` — Add `source` field to VineVideo (or a new VineReceivedEvent type)
- `src/lib/components/VineFeed.svelte` — Two-tab layout (Following/Discover), pass follow props
- `src/lib/components/VineCard.svelte` — Follow/unfollow button, creator name display
- `src/App.svelte` — Wire follow state, tab state, follow/unfollow handlers

### Test files
- `src-tauri/src/follows.rs` — Inline `#[cfg(test)]` module
- `src/lib/vine-service.test.ts` — New tests for follow/unfollow, routing, reconciliation
- `src/lib/components/__tests__/VineCard.test.ts` — New tests for follow button
- `src/lib/components/__tests__/VineFeed.test.ts` — New tests for tab switching

---

## Task 1: FollowManager — Rust persistence layer

**Files:**
- Create: `src-tauri/src/follows.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod follows;`)

- [ ] **Step 1: Write the failing tests**

In `src-tauri/src/follows.rs`, write the module with tests first:

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FollowEntry {
    pub address: String,
    #[serde(default)]
    pub name: Option<String>,
    pub followed_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct FollowFile {
    version: u32,
    follows: Vec<FollowEntry>,
}

pub struct FollowManager {
    entries: HashMap<String, FollowEntry>,
    data_dir: PathBuf,
}

impl FollowManager {
    /// Load follows from disk; returns empty manager if file missing.
    pub fn load(data_dir: &Path) -> Self {
        todo!()
    }

    /// Persist current state to follows.json (atomic write).
    fn save(&self) {
        todo!()
    }

    /// Add a creator to the follow list. Returns true if newly added.
    pub fn follow(&mut self, address: String, name: Option<String>) -> bool {
        todo!()
    }

    /// Remove a creator from the follow list. Returns true if was present.
    pub fn unfollow(&mut self, address: &str) -> bool {
        todo!()
    }

    /// Check if a creator is followed.
    pub fn is_followed(&self, address: &str) -> bool {
        todo!()
    }

    /// Return all followed creators.
    pub fn list(&self) -> Vec<FollowEntry> {
        todo!()
    }

    /// Update the display name for a followed creator.
    pub fn update_name(&mut self, address: &str, name: String) {
        todo!()
    }

    /// Return all followed addresses (for event loop startup).
    pub fn addresses(&self) -> Vec<String> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("harmony-test-{}", rand::random::<u32>()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_empty_dir_returns_empty_manager() {
        let dir = temp_dir();
        let mgr = FollowManager::load(&dir);
        assert!(mgr.list().is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn follow_and_list() {
        let dir = temp_dir();
        let mut mgr = FollowManager::load(&dir);
        assert!(mgr.follow("aabb".to_string(), Some("Alice".to_string())));
        let list = mgr.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].address, "aabb");
        assert_eq!(list[0].name.as_deref(), Some("Alice"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn follow_is_idempotent() {
        let dir = temp_dir();
        let mut mgr = FollowManager::load(&dir);
        assert!(mgr.follow("aabb".to_string(), None));
        assert!(!mgr.follow("aabb".to_string(), None));
        assert_eq!(mgr.list().len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unfollow_returns_true_when_present() {
        let dir = temp_dir();
        let mut mgr = FollowManager::load(&dir);
        mgr.follow("aabb".to_string(), None);
        assert!(mgr.unfollow("aabb"));
        assert!(!mgr.is_followed("aabb"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unfollow_returns_false_when_absent() {
        let dir = temp_dir();
        let mgr = FollowManager::load(&dir);
        assert!(!mgr.unfollow("nonexistent"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn persistence_round_trip() {
        let dir = temp_dir();
        {
            let mut mgr = FollowManager::load(&dir);
            mgr.follow("aabb".to_string(), Some("Alice".to_string()));
            mgr.follow("ccdd".to_string(), None);
        }
        let mgr2 = FollowManager::load(&dir);
        assert_eq!(mgr2.list().len(), 2);
        assert!(mgr2.is_followed("aabb"));
        assert!(mgr2.is_followed("ccdd"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_name() {
        let dir = temp_dir();
        let mut mgr = FollowManager::load(&dir);
        mgr.follow("aabb".to_string(), Some("Alice".to_string()));
        mgr.update_name("aabb", "Alicia".to_string());
        let entry = mgr.list().into_iter().find(|e| e.address == "aabb").unwrap();
        assert_eq!(entry.name.as_deref(), Some("Alicia"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn addresses_returns_all_followed() {
        let dir = temp_dir();
        let mut mgr = FollowManager::load(&dir);
        mgr.follow("aabb".to_string(), None);
        mgr.follow("ccdd".to_string(), None);
        let mut addrs = mgr.addresses();
        addrs.sort();
        assert_eq!(addrs, vec!["aabb", "ccdd"]);
        fs::remove_dir_all(&dir).ok();
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo test --lib follows -- --nocapture`
Expected: FAIL with `not yet implemented`

- [ ] **Step 3: Implement FollowManager**

Replace the `todo!()` bodies:

```rust
impl FollowManager {
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("follows.json");
        let entries = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    match serde_json::from_str::<FollowFile>(&content) {
                        Ok(file) => file
                            .follows
                            .into_iter()
                            .map(|e| (e.address.clone(), e))
                            .collect(),
                        Err(_) => HashMap::new(),
                    }
                }
                Err(_) => HashMap::new(),
            }
        } else {
            HashMap::new()
        };
        Self {
            entries,
            data_dir: data_dir.to_path_buf(),
        }
    }

    fn save(&self) {
        let file = FollowFile {
            version: 1,
            follows: self.entries.values().cloned().collect(),
        };
        let json = match serde_json::to_string_pretty(&file) {
            Ok(j) => j,
            Err(_) => return,
        };
        let tmp = self.data_dir.join("follows.json.tmp");
        let target = self.data_dir.join("follows.json");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, &target);
        }
    }

    pub fn follow(&mut self, address: String, name: Option<String>) -> bool {
        if self.entries.contains_key(&address) {
            return false;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.entries.insert(
            address.clone(),
            FollowEntry {
                address,
                name,
                followed_at: now,
            },
        );
        self.save();
        true
    }

    pub fn unfollow(&mut self, address: &str) -> bool {
        let removed = self.entries.remove(address).is_some();
        if removed {
            self.save();
        }
        removed
    }

    pub fn is_followed(&self, address: &str) -> bool {
        self.entries.contains_key(address)
    }

    pub fn list(&self) -> Vec<FollowEntry> {
        let mut list: Vec<_> = self.entries.values().cloned().collect();
        list.sort_by_key(|e| e.followed_at);
        list
    }

    pub fn update_name(&mut self, address: &str, name: String) {
        if let Some(entry) = self.entries.get_mut(address) {
            entry.name = Some(name);
            self.save();
        }
    }

    pub fn addresses(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }
}
```

- [ ] **Step 4: Add mod declaration**

In `src-tauri/src/lib.rs`, add after the existing `mod` declarations (after `mod event_loop;` and `mod identity;`):

```rust
mod follows;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd src-tauri && cargo test --lib follows -- --nocapture`
Expected: All 8 tests PASS

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/follows.rs src-tauri/src/lib.rs
git commit -m "feat: add FollowManager with JSON persistence and tests"
```

---

## Task 2: Follow channel and event loop integration

**Files:**
- Modify: `src-tauri/src/event_loop.rs`
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Add FollowRequest to event_loop.rs**

After the `IngestRequest` struct (line 43), add:

```rust
/// A follow/unfollow request sent from the Tauri command thread into the event loop.
pub enum FollowRequest {
    Follow { address: String },
    Unfollow { address: String },
}
```

- [ ] **Step 2: Add follow_rx parameter to event_loop::run()**

Modify the `run()` function signature (line 62) to accept the new channel and a list of initial follows, plus a shared reference to the FollowManager for checking follow state:

```rust
pub async fn run(
    mut runtime: NodeRuntime<MemoryBookStore>,
    startup_actions: Vec<RuntimeAction>,
    app: AppHandle,
    endpoint: Option<String>,
    ready_tx: oneshot::Sender<Result<(), String>>,
    mut shutdown: watch::Receiver<bool>,
    mut publish_rx: mpsc::Receiver<PublishRequest>,
    mut fetch_rx: mpsc::Receiver<FetchRequest>,
    mut ingest_rx: mpsc::Receiver<IngestRequest>,
    mut follow_rx: mpsc::Receiver<FollowRequest>,
    initial_follows: Vec<String>,
    followed_set: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
) {
```

- [ ] **Step 3: Create per-creator subscriptions on startup**

After the wildcard vine subscription setup (after line 200), add:

```rust
// Subscribe to each followed creator's vine announcements.
for address in &initial_follows {
    dispatch_action(
        RuntimeAction::Subscribe {
            key_expr: format!("harmony/vines/{address}/announce/**"),
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
}
```

- [ ] **Step 4: Handle follow/unfollow channel messages in the select loop**

After the ingest_rx handler (around line 380), add:

```rust
// ── Follow/unfollow requests from Tauri commands ─────
Some(req) = follow_rx.recv() => {
    match req {
        FollowRequest::Follow { address } => {
            {
                let mut set = followed_set.lock().unwrap();
                set.insert(address.clone());
            }
            dispatch_action(
                RuntimeAction::Subscribe {
                    key_expr: format!("harmony/vines/{address}/announce/**"),
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
        }
        FollowRequest::Unfollow { address } => {
            {
                let mut set = followed_set.lock().unwrap();
                set.remove(&address);
            }
            dispatch_action(
                RuntimeAction::Unsubscribe {
                    key_expr: format!("harmony/vines/{address}/announce/**"),
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
        }
    }
}
```

- [ ] **Step 5: Tag vine source and suppress wildcard duplicates in emit_frontend_event**

Modify `emit_frontend_event` (line 626) to accept the followed set and tag the source. Change the function signature:

```rust
fn emit_frontend_event(
    app: &AppHandle,
    key_expr: &str,
    payload: &[u8],
    hop_distance: Option<u8>,
    followed_set: &std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
) {
```

Replace the vine handler block (lines 640-643):

```rust
    } else if key_expr.starts_with("harmony/vines/") {
        if let Ok(mut vine) = serde_json::from_slice::<serde_json::Value>(payload) {
            let creator = vine.get("creatorAddress")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let is_followed = {
                let set = followed_set.lock().unwrap();
                set.contains(creator)
            };
            // Determine source: if it came from a per-creator sub
            // (key_expr contains /announce/), it's "followed".
            // If it came from the wildcard and creator IS followed,
            // suppress the duplicate — the per-creator sub will deliver it.
            let is_per_creator_sub = key_expr.contains("/announce/");
            if !is_per_creator_sub && is_followed {
                // Suppress wildcard duplicate for followed creators
                return;
            }
            let source = if is_per_creator_sub { "followed" } else { "discover" };
            if let Some(obj) = vine.as_object_mut() {
                obj.insert("source".to_string(), serde_json::Value::String(source.to_string()));
            }
            let _ = app.emit("vine-received", &vine);
        }
```

- [ ] **Step 6: Update all call sites of emit_frontend_event to pass followed_set**

Search for all calls to `emit_frontend_event` in event_loop.rs and add the `&followed_set` argument. There should be one call site in the ZenohEvent::Subscription handler. Update it to pass `&followed_set`.

- [ ] **Step 7: Wire the follow channel in lib.rs start_node**

In `lib.rs`, add `follow_tx` to NodeState (after `ingest_tx` on line 20):

```rust
follow_tx: Option<tokio::sync::mpsc::Sender<event_loop::FollowRequest>>,
```

In `start_node` (after line 217), create the channel and the followed set:

```rust
let (follow_tx, follow_rx) = tokio::sync::mpsc::channel(64);
```

Load the FollowManager and extract initial follows:

```rust
let app_data_dir = app.path().app_data_dir().map_err(|e| format!("app_data_dir: {e}"))?;
std::fs::create_dir_all(&app_data_dir).map_err(|e| format!("create app_data_dir: {e}"))?;
let follow_mgr = follows::FollowManager::load(&app_data_dir);
let initial_follows = follow_mgr.addresses();
let followed_set = std::sync::Arc::new(std::sync::Mutex::new(
    initial_follows.iter().cloned().collect::<std::collections::HashSet<String>>(),
));
```

Add `follow_mgr` and `followed_set` to NodeState (you'll need to add these fields):

```rust
follow_mgr: Option<follows::FollowManager>,
followed_set: Option<std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>>,
```

Pass the new args to `event_loop::run()` (after `ingest_rx` on line 314):

```rust
event_loop::run(
    runtime,
    startup_actions,
    app_clone,
    ep_clone,
    ready_tx,
    shutdown_rx,
    publish_rx,
    fetch_rx,
    ingest_rx,
    follow_rx,
    initial_follows,
    followed_set_clone,
)
.await;
```

Store `follow_tx`, `follow_mgr`, and `followed_set` in the guard (after line 325):

```rust
guard.follow_tx = Some(follow_tx);
guard.follow_mgr = Some(follow_mgr);
guard.followed_set = Some(followed_set);
```

Also clean up `follow_tx` in the stop path (after `old_ingest` around line 227):

```rust
let old_follow = guard.follow_tx.take();
// ...
drop(old_follow);
```

- [ ] **Step 8: Build and verify compilation**

Run: `cd src-tauri && cargo check`
Expected: Compiles with no errors (warnings OK)

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/event_loop.rs src-tauri/src/lib.rs
git commit -m "feat: wire follow channel into event loop with source tagging"
```

---

## Task 3: Tauri follow/unfollow commands

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Replace the follow/unfollow stubs**

Replace the stubs at lines 652-664 with real implementations:

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowEntryResponse {
    pub address: String,
    pub name: Option<String>,
    pub followed_at: u64,
}

#[tauri::command]
async fn follow_vine_creator(
    address: String,
    name: Option<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let mut guard = state.lock().map_err(|e| format!("lock: {e}"))?;

    // Reject self-follow
    if address == guard.node_addr {
        return Err("cannot follow yourself".to_string());
    }

    let mgr = guard.follow_mgr.as_mut().ok_or("not connected")?;
    if !mgr.follow(address.clone(), name) {
        return Ok(false); // already followed
    }

    // Update the shared followed_set
    if let Some(ref set) = guard.followed_set {
        let mut s = set.lock().unwrap();
        s.insert(address.clone());
    }

    // Send to event loop to create Zenoh subscription
    if let Some(ref tx) = guard.follow_tx {
        let _ = tx.try_send(event_loop::FollowRequest::Follow {
            address,
        });
    }

    Ok(true)
}

#[tauri::command]
async fn unfollow_vine_creator(
    address: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let mut guard = state.lock().map_err(|e| format!("lock: {e}"))?;

    let mgr = guard.follow_mgr.as_mut().ok_or("not connected")?;
    if !mgr.unfollow(&address) {
        return Ok(false); // wasn't followed
    }

    // Update the shared followed_set
    if let Some(ref set) = guard.followed_set {
        let mut s = set.lock().unwrap();
        s.remove(&address);
    }

    // Send to event loop to destroy Zenoh subscription
    if let Some(ref tx) = guard.follow_tx {
        let _ = tx.try_send(event_loop::FollowRequest::Unfollow {
            address,
        });
    }

    Ok(true)
}

#[tauri::command]
fn list_followed(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<FollowEntryResponse>, String> {
    let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
    let mgr = guard.follow_mgr.as_ref().ok_or("not connected")?;
    Ok(mgr
        .list()
        .into_iter()
        .map(|e| FollowEntryResponse {
            address: e.address,
            name: e.name,
            followed_at: e.followed_at,
        })
        .collect())
}

#[tauri::command]
fn is_followed(
    address: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
    let mgr = guard.follow_mgr.as_ref().ok_or("not connected")?;
    Ok(mgr.is_followed(&address))
}
```

- [ ] **Step 2: Register the new commands in run()**

In the `invoke_handler` call (around line 940), add `list_followed` and `is_followed`. The existing `follow_vine_creator` and `unfollow_vine_creator` are already registered:

```rust
.invoke_handler(tauri::generate_handler![
    // ...existing commands...
    list_followed,
    is_followed,
    // ...rest...
])
```

- [ ] **Step 3: Build and verify**

Run: `cd src-tauri && cargo check`
Expected: Compiles with no errors

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat: implement follow/unfollow Tauri commands with FollowManager"
```

---

## Task 4: VineService — split feeds and follow methods

**Files:**
- Modify: `src/lib/vine-service.ts`
- Modify: `src/lib/vine-service.test.ts`

- [ ] **Step 1: Write failing tests for the new follow/routing behavior**

Add to `src/lib/vine-service.test.ts`:

```typescript
  // ── Follow / feed routing ──────────────────────────────────────────

  it('routes "followed" vines to followedVines', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('vine-received', {
      id: 'fv-1', creatorAddress: 'aabb', creatorName: 'Alice',
      createdAt: 1, videoCid: 'cid-f1', source: 'followed',
    });
    expect(svc.followedVines.length).toBe(1);
    expect(svc.discoverVines.length).toBe(0);
  });

  it('routes "discover" vines to discoverVines', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('vine-received', {
      id: 'dv-1', creatorAddress: 'ccdd', creatorName: 'Bob',
      createdAt: 1, videoCid: 'cid-d1', source: 'discover',
    });
    expect(svc.discoverVines.length).toBe(1);
    expect(svc.followedVines.length).toBe(0);
  });

  it('treats vines without source as discover', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('vine-received', {
      id: 'ns-1', creatorAddress: 'eeff', creatorName: 'Carol',
      createdAt: 1, videoCid: 'cid-ns',
    });
    expect(svc.discoverVines.length).toBe(1);
  });

  it('follow moves existing vines from discover to followed', async () => {
    const { adapter, emit } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue(true);
    await svc.connectAdapter(adapter);
    emit('vine-received', {
      id: 'mv-1', creatorAddress: 'aabb', creatorName: 'Alice',
      createdAt: 1, videoCid: 'cid-mv1', source: 'discover',
    });
    expect(svc.discoverVines.length).toBe(1);
    await svc.follow('aabb', 'Alice');
    expect(svc.discoverVines.length).toBe(0);
    expect(svc.followedVines.length).toBe(1);
  });

  it('follow calls follow_creator Tauri command', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue(true);
    await svc.connectAdapter(adapter);
    await svc.follow('aabb', 'Alice');
    expect(adapter.invoke).toHaveBeenCalledWith('follow_vine_creator', {
      address: 'aabb', name: 'Alice',
    });
  });

  it('unfollow removes vines from followedVines', async () => {
    const { adapter, emit } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue(true);
    await svc.connectAdapter(adapter);
    svc.followedAddresses.add('aabb');
    emit('vine-received', {
      id: 'uf-1', creatorAddress: 'aabb', creatorName: 'Alice',
      createdAt: 1, videoCid: 'cid-uf1', source: 'followed',
    });
    await svc.unfollow('aabb');
    expect(svc.followedVines.length).toBe(0);
    expect(svc.followedAddresses.has('aabb')).toBe(false);
  });

  it('unfollow calls unfollow_vine_creator Tauri command', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue(true);
    await svc.connectAdapter(adapter);
    svc.followedAddresses.add('aabb');
    await svc.unfollow('aabb');
    expect(adapter.invoke).toHaveBeenCalledWith('unfollow_vine_creator', {
      address: 'aabb',
    });
  });

  it('isFollowed checks local cache', () => {
    svc.followedAddresses.add('aabb');
    expect(svc.isFollowed('aabb')).toBe(true);
    expect(svc.isFollowed('ccdd')).toBe(false);
  });

  it('loadFollowed populates followedAddresses', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'list_followed') {
        return Promise.resolve([
          { address: 'aabb', name: 'Alice', followedAt: 1 },
          { address: 'ccdd', name: null, followedAt: 2 },
        ]);
      }
      return Promise.resolve(undefined);
    });
    await svc.connectAdapter(adapter);
    await svc.loadFollowed();
    expect(svc.followedAddresses.has('aabb')).toBe(true);
    expect(svc.followedAddresses.has('ccdd')).toBe(true);
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/vine-service.test.ts`
Expected: New tests FAIL (followedVines/discoverVines/follow/unfollow don't exist yet)

- [ ] **Step 3: Update VineDescriptorEvent and implement the service changes**

In `src/lib/vine-service.ts`, update the wire type to include `source`:

```typescript
export interface VineDescriptorEvent {
  id: string;
  creatorAddress: string;
  creatorName: string;
  createdAt: number;
  videoCid: string;
  title?: string;
  reshareOf?: string;
  source?: 'followed' | 'discover';
}
```

Replace the `VineService` class properties. Change `vines` to `followedVines` and `discoverVines`, add `followedAddresses`:

```typescript
export class VineService {
  followedVines: VineVideo[] = [];
  discoverVines: VineVideo[] = [];
  /** @deprecated Use followedVines/discoverVines instead. Kept for backwards compat during migration. */
  get vines(): VineVideo[] {
    return [...this.followedVines, ...this.discoverVines];
  }
  onChange?: () => void;
  ownAddress: string | null = null;
  ownDisplayName = 'You';
  viewedIds = new Set<string>();
  followedAddresses = new Set<string>();

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];
  private seenIds = new Set<string>();

  constructor() {
    // Seed with mock data into discover feed.
    this.discoverVines = [...mockVines];
    for (const v of this.discoverVines) {
      this.seenIds.add(v.id);
      if (v.viewed) this.viewedIds.add(v.id);
    }
  }
```

Update `connectAdapter` to route by `source`:

```typescript
  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    this.adapter = adapter;
    const unlisten = await adapter.listen(
      'vine-received',
      (event) => {
        const wire = event.payload as VineDescriptorEvent;
        if (this.seenIds.has(wire.id)) return;
        this.seenIds.add(wire.id);
        const vine = this.wireToVine(wire);
        if (vine.viewed) this.viewedIds = new Set([...this.viewedIds, vine.id]);
        if (wire.source === 'followed') {
          this.followedVines = [...this.followedVines, vine];
        } else {
          this.discoverVines = [...this.discoverVines, vine];
        }
        this.onChange?.();
      },
    );
    this.unlisteners.push(unlisten);
  }
```

Add the new methods:

```typescript
  async follow(address: string, name?: string): Promise<void> {
    if (this.adapter) {
      await this.adapter.invoke('follow_vine_creator', { address, name: name ?? null });
    }
    this.followedAddresses.add(address);
    // Move existing vines for this creator from discover to followed
    const toMove = this.discoverVines.filter(v => v.creatorAddress === address);
    if (toMove.length > 0) {
      this.discoverVines = this.discoverVines.filter(v => v.creatorAddress !== address);
      this.followedVines = [...this.followedVines, ...toMove];
    }
    this.onChange?.();
  }

  async unfollow(address: string): Promise<void> {
    if (this.adapter) {
      await this.adapter.invoke('unfollow_vine_creator', { address });
    }
    this.followedAddresses.delete(address);
    this.followedVines = this.followedVines.filter(v => v.creatorAddress !== address);
    this.onChange?.();
  }

  async loadFollowed(): Promise<void> {
    if (!this.adapter) return;
    try {
      const entries = await this.adapter.invoke('list_followed', {}) as Array<{
        address: string;
        name: string | null;
        followedAt: number;
      }>;
      for (const entry of entries) {
        this.followedAddresses.add(entry.address);
      }
    } catch {
      // Not connected yet — will retry when Zenoh connects
    }
  }

  isFollowed(address: string): boolean {
    return this.followedAddresses.has(address);
  }
```

- [ ] **Step 4: Update existing tests that reference `svc.vines`**

Existing tests that reference `svc.vines` still work via the backwards-compat getter, but tests checking `svc.vines.length` for appended vines should continue to work since the getter combines both arrays. Verify no test breakage.

- [ ] **Step 5: Run all vine service tests**

Run: `npx vitest run src/lib/vine-service.test.ts`
Expected: All tests PASS (old + new)

- [ ] **Step 6: Commit**

```bash
git add src/lib/vine-service.ts src/lib/vine-service.test.ts
git commit -m "feat: split VineService into followed/discover feeds with follow methods"
```

---

## Task 5: VineCard — follow button and creator name

**Files:**
- Modify: `src/lib/components/VineCard.svelte`
- Modify: `src/lib/components/__tests__/VineCard.test.ts`

- [ ] **Step 1: Write failing tests for follow button**

Add to `src/lib/components/__tests__/VineCard.test.ts`:

```typescript
  it('renders follow button when showFollowButton is true and not followed', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), showFollowButton: true, isFollowed: false, onFollow: vi.fn() } });
    expect(screen.getByLabelText(/Follow/)).toBeTruthy();
  });

  it('does not render follow button when showFollowButton is false', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), showFollowButton: false } });
    expect(screen.queryByLabelText(/Follow/)).toBeNull();
  });

  it('renders Following badge when followed', () => {
    render(VineCard, { props: { vine, onPlay: vi.fn(), showFollowButton: true, isFollowed: true, onUnfollow: vi.fn() } });
    expect(screen.getByText('Following')).toBeTruthy();
  });

  it('calls onFollow with address and name when follow clicked', async () => {
    const onFollow = vi.fn();
    render(VineCard, { props: { vine, onPlay: vi.fn(), showFollowButton: true, isFollowed: false, onFollow } });
    await fireEvent.click(screen.getByLabelText(/Follow/));
    expect(onFollow).toHaveBeenCalledWith('a1b2c3d4', 'Alice');
  });

  it('follow button click does not trigger onPlay', async () => {
    const onPlay = vi.fn();
    const onFollow = vi.fn();
    render(VineCard, { props: { vine, onPlay, showFollowButton: true, isFollowed: false, onFollow } });
    await fireEvent.click(screen.getByLabelText(/Follow/));
    expect(onPlay).not.toHaveBeenCalled();
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/VineCard.test.ts`
Expected: New tests FAIL

- [ ] **Step 3: Update VineCard component**

Replace the props destructuring (lines 6-10) with:

```typescript
  let { vine, onPlay, isViewed, showFollowButton = false, isFollowed = false, onFollow, onUnfollow }: {
    vine: VineVideo;
    onPlay: (vine: VineVideo) => void;
    isViewed?: boolean;
    showFollowButton?: boolean;
    isFollowed?: boolean;
    onFollow?: (address: string, name: string) => void;
    onUnfollow?: (address: string) => void;
  } = $props();
```

Add a follow button handler after `handleKeyDown` (after line 25):

```typescript
  function handleFollowClick(e: MouseEvent) {
    e.stopPropagation();
    if (isFollowed) {
      onUnfollow?.(vine.creatorAddress);
    } else {
      onFollow?.(vine.creatorAddress, vine.creatorName);
    }
  }
```

In the template, add the follow button inside `.card-info` after the reshare badge (after line 54):

```svelte
    {#if showFollowButton}
      <button
        type="button"
        class="follow-btn"
        class:following={isFollowed}
        aria-label={isFollowed ? `Unfollow ${vine.creatorName}` : `Follow ${vine.creatorName}`}
        onclick={handleFollowClick}
      >
        {isFollowed ? 'Following' : 'Follow'}
      </button>
    {/if}
```

Add styles for the follow button in the `<style>` block:

```css
  .follow-btn {
    display: inline-block;
    font-size: 0.7rem;
    font-weight: 600;
    padding: 2px 10px;
    border-radius: 12px;
    cursor: pointer;
    width: fit-content;
    transition: background 0.15s, color 0.15s;
    background: var(--accent);
    color: white;
    border: 1px solid var(--accent);
  }

  .follow-btn:hover {
    opacity: 0.85;
  }

  .follow-btn.following {
    background: transparent;
    color: var(--text-muted);
    border-color: var(--text-muted);
  }

  .follow-btn.following:hover {
    border-color: #e74c3c;
    color: #e74c3c;
  }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/VineCard.test.ts`
Expected: All tests PASS (old + new)

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/VineCard.svelte src/lib/components/__tests__/VineCard.test.ts
git commit -m "feat: add follow/unfollow button to VineCard"
```

---

## Task 6: VineFeed — Following/Discover tabs

**Files:**
- Modify: `src/lib/components/VineFeed.svelte`
- Modify: `src/lib/components/__tests__/VineFeed.test.ts`

- [ ] **Step 1: Write failing tests for tab layout**

Add to `src/lib/components/__tests__/VineFeed.test.ts`:

```typescript
  it('renders Following and Discover tabs', () => {
    render(VineFeed, { props: {
      followedVines: vines, discoverVines: [], viewedIds: makeViewedIds(),
      activeTab: 'following', followedAddresses: new Set(),
    } });
    expect(screen.getByText('Following')).toBeTruthy();
    expect(screen.getByText('Discover')).toBeTruthy();
  });

  it('shows followed vines when Following tab active', () => {
    render(VineFeed, { props: {
      followedVines: vines, discoverVines: [], viewedIds: makeViewedIds(),
      activeTab: 'following', followedAddresses: new Set(),
    } });
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('Carol')).toBeTruthy();
  });

  it('shows discover vines when Discover tab active', () => {
    const discoverVines: VineVideo[] = [{
      id: 'dv-01', creatorAddress: 'xyz', creatorName: 'Dave',
      createdAt: 1700000300, videoCid: 'cid-d', title: 'Discover vine', viewed: false,
    }];
    render(VineFeed, { props: {
      followedVines: [], discoverVines: discoverVines, viewedIds: new Set(),
      activeTab: 'discover', followedAddresses: new Set(),
    } });
    expect(screen.getByText('Dave')).toBeTruthy();
  });

  it('shows empty state with nudge in Following when no followed vines', () => {
    render(VineFeed, { props: {
      followedVines: [], discoverVines: [], viewedIds: new Set(),
      activeTab: 'following', followedAddresses: new Set(),
    } });
    expect(screen.getByText(/Follow creators/)).toBeTruthy();
  });

  it('calls onTabChange when Discover tab clicked', async () => {
    const onTabChange = vi.fn();
    render(VineFeed, { props: {
      followedVines: vines, discoverVines: [], viewedIds: makeViewedIds(),
      activeTab: 'following', followedAddresses: new Set(), onTabChange,
    } });
    await fireEvent.click(screen.getByText('Discover'));
    expect(onTabChange).toHaveBeenCalledWith('discover');
  });

  it('passes showFollowButton to cards in Discover tab', () => {
    const discoverVines: VineVideo[] = [{
      id: 'fb-01', creatorAddress: 'xyz', creatorName: 'Eve',
      createdAt: 1700000300, videoCid: 'cid-e', title: 'Eve vine', viewed: false,
    }];
    render(VineFeed, { props: {
      followedVines: [], discoverVines, viewedIds: new Set(),
      activeTab: 'discover', followedAddresses: new Set(),
    } });
    // The follow button should be present on discover cards
    expect(screen.getByLabelText(/Follow Eve/)).toBeTruthy();
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/VineFeed.test.ts`
Expected: New tests FAIL (props changed)

- [ ] **Step 3: Rewrite VineFeed with tab layout**

Replace the entire `<script>` section of `VineFeed.svelte`:

```typescript
<script lang="ts">
  import type { VineVideo } from '../types';
  import VineCard from './VineCard.svelte';
  import VinePlayer from './VinePlayer.svelte';

  type FeedFilter = 'all' | 'unviewed';
  type FeedTab = 'following' | 'discover';

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
  } = $props();

  let activeVine = $state<VineVideo | null>(null);
  let feedFilter = $state<FeedFilter>('all');
  let playerList = $state<VineVideo[]>([]);
  let activeIndex = $state(-1);

  // Select the active vine list based on tab
  let activeVines = $derived(
    activeTab === 'following' ? followedVines : discoverVines
  );

  let sortedVines = $derived(
    [...activeVines].sort((a, b) => b.createdAt - a.createdAt)
  );

  let filteredVines = $derived(
    activeTab === 'following' && feedFilter === 'unviewed'
      ? sortedVines.filter(v => !viewedIds.has(v.id))
      : sortedVines
  );

  let unviewedCount = $derived(
    followedVines.filter(v => !viewedIds.has(v.id)).length
  );

  function openPlayer(vine: VineVideo) {
    if (!activeVine) {
      playerList = [...filteredVines];
    }
    activeIndex = playerList.findIndex(v => v.id === vine.id);
    activeVine = vine;
    onMarkViewed?.(vine.id);
  }

  function closePlayer() {
    activeVine = null;
    playerList = [];
    activeIndex = -1;
  }

  function nextVine() {
    if (activeIndex >= 0 && activeIndex < playerList.length - 1) {
      const next = playerList[activeIndex + 1];
      activeIndex = activeIndex + 1;
      activeVine = next;
      onMarkViewed?.(next.id);
    }
  }

  function previousVine() {
    if (activeIndex > 0) {
      const prev = playerList[activeIndex - 1];
      activeIndex = activeIndex - 1;
      activeVine = prev;
      onMarkViewed?.(prev.id);
    }
  }
</script>
```

Replace the template:

```svelte
<div class="vine-feed">
  <header class="feed-header">
    <h2 class="feed-title">Vines</h2>
    {#if unviewedCount > 0}
      <span class="unviewed-count" aria-label="{unviewedCount} unviewed">{unviewedCount} new</span>
    {/if}
    <div class="header-spacer"></div>
    {#if onPublish}
      <button type="button" class="create-btn" onclick={onPublish} aria-label="Create vine">+</button>
    {/if}
  </header>

  <div class="tab-bar">
    <button type="button" class="tab" class:active={activeTab === 'following'} onclick={() => onTabChange?.('following')}>Following</button>
    <button type="button" class="tab" class:active={activeTab === 'discover'} onclick={() => onTabChange?.('discover')}>Discover</button>
  </div>

  {#if activeTab === 'following'}
    <div class="filter-bar">
      <button type="button" class="filter-tab" class:active={feedFilter === 'all'} onclick={() => feedFilter = 'all'}>All</button>
      <button type="button" class="filter-tab" class:active={feedFilter === 'unviewed'} onclick={() => feedFilter = 'unviewed'}>
        Unviewed{#if unviewedCount > 0}&nbsp;({unviewedCount}){/if}
      </button>
    </div>
  {/if}

  {#if filteredVines.length === 0}
    <p class="empty-state">
      {#if activeTab === 'following'}
        {#if feedFilter === 'unviewed'}
          All caught up — no unviewed vines.
        {:else}
          Follow creators to build your feed. Check out the Discover tab to find people to follow.
        {/if}
      {:else}
        No vines on the network yet.
      {/if}
    </p>
  {:else}
    <div class="feed-list" role="list" aria-label="Vine feed">
      {#each filteredVines as vine (vine.id)}
        <div role="listitem">
          <VineCard
            {vine}
            onPlay={openPlayer}
            isViewed={viewedIds.has(vine.id)}
            showFollowButton={activeTab === 'discover'}
            isFollowed={followedAddresses.has(vine.creatorAddress)}
            {onFollow}
            {onUnfollow}
          />
        </div>
      {/each}
    </div>
  {/if}
</div>

{#if activeVine}
  <VinePlayer
    vine={activeVine}
    onClose={closePlayer}
    onNext={activeIndex >= 0 && activeIndex < playerList.length - 1 ? nextVine : undefined}
    onPrevious={activeIndex > 0 ? previousVine : undefined}
    {onReshare}
    {resolveVideo}
  />
{/if}
```

Add the `.tab-bar` and `.tab` styles. Keep existing styles, add:

```css
  .tab-bar {
    display: flex;
    gap: 0;
    padding: 0 16px 4px;
    border-bottom: 1px solid var(--bg-tertiary);
  }

  .tab {
    background: none;
    border: none;
    border-bottom: 2px solid transparent;
    color: var(--text-muted);
    font-size: 0.85rem;
    font-weight: 500;
    padding: 8px 16px;
    cursor: pointer;
    transition: color 0.15s, border-color 0.15s;
  }

  .tab:hover {
    color: var(--text-primary);
  }

  .tab.active {
    color: var(--text-primary);
    font-weight: 600;
    border-bottom-color: var(--accent);
  }
```

- [ ] **Step 4: Update existing VineFeed tests**

The existing tests reference the old `vines` prop. Update them to use the new props. Replace the render calls in existing tests. For tests that use `vines`, pass them as `followedVines` with `activeTab: 'following'`:

In the existing test fixtures, update each `render(VineFeed, { props: { vines, viewedIds: makeViewedIds() } })` to:

```typescript
render(VineFeed, { props: { followedVines: vines, viewedIds: makeViewedIds(), activeTab: 'following', followedAddresses: new Set() } })
```

Apply the same pattern to all existing tests (replace `vines` prop with `followedVines`, add `activeTab: 'following'` and `followedAddresses: new Set()`). For the filter tab tests, update the tab text from `Unviewed` to match the new sub-filter.

- [ ] **Step 5: Run all VineFeed tests**

Run: `npx vitest run src/lib/components/__tests__/VineFeed.test.ts`
Expected: All tests PASS (old updated + new)

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/VineFeed.svelte src/lib/components/__tests__/VineFeed.test.ts
git commit -m "feat: add Following/Discover tab layout to VineFeed"
```

---

## Task 7: App.svelte — wire everything together

**Files:**
- Modify: `src/App.svelte`

- [ ] **Step 1: Update vine state variables**

Replace the vine state block (around lines 61-72) with:

```typescript
const vineService = new VineService();
$effect(() => () => vineService.destroy());

let followedVines = $state([...vineService.followedVines]);
let discoverVines = $state([...vineService.discoverVines]);
let vineViewedIds = $state(new Set(vineService.viewedIds));
let vineTab = $state<'following' | 'discover'>('following');
let followedAddresses = $state(new Set(vineService.followedAddresses));

vineService.onChange = () => {
  followedVines = [...vineService.followedVines];
  discoverVines = [...vineService.discoverVines];
  vineViewedIds = new Set(vineService.viewedIds);
  followedAddresses = new Set(vineService.followedAddresses);
};
vineService.ownDisplayName = myProfile.displayName || 'You';
```

- [ ] **Step 2: Add follow/unfollow handlers**

After the existing vine handlers (after `handleVineReshare`, around line 96), add:

```typescript
async function handleVineFollow(address: string, name: string) {
  try {
    await vineService.follow(address, name);
  } catch (err) {
    console.error('Follow failed', err);
  }
}

async function handleVineUnfollow(address: string) {
  try {
    await vineService.unfollow(address);
  } catch (err) {
    console.error('Unfollow failed', err);
  }
}
```

- [ ] **Step 3: Load follows on Zenoh connect**

In the section where `vineService.connectAdapter(adapter)` is called (around line 182), add `loadFollowed` after the adapter is connected:

```typescript
await vineService.connectAdapter(adapter);
await vineService.loadFollowed();
```

- [ ] **Step 4: Update the vineFeed snippet**

Replace the vine feed snippet rendering (around line 608-613):

```svelte
  {#snippet vineFeed()}
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
      resolveVideo={resolveVideoFn}
    />
    {#if showVinePublish}
      <VinePublishDialog onPublish={handleVinePublish} onClose={() => showVinePublish = false} />
    {/if}
  {/snippet}
```

- [ ] **Step 5: Run the full test suite to verify no breakage**

Run: `npx vitest run`
Expected: All tests PASS

- [ ] **Step 6: Commit**

```bash
git add src/App.svelte
git commit -m "feat: wire follow/discover feeds into App.svelte"
```

---

## Task 8: Startup reconciliation and name updates

**Files:**
- Modify: `src/lib/vine-service.ts`
- Modify: `src/lib/vine-service.test.ts`

- [ ] **Step 1: Write failing test for reconciliation**

Add to `src/lib/vine-service.test.ts`:

```typescript
  it('reconciles misrouted vines after loadFollowed', async () => {
    const { adapter, emit } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'list_followed') {
        return Promise.resolve([
          { address: 'aabb', name: 'Alice', followedAt: 1 },
        ]);
      }
      return Promise.resolve(undefined);
    });
    await svc.connectAdapter(adapter);
    // Vine arrives before loadFollowed — routes to discover
    emit('vine-received', {
      id: 'recon-1', creatorAddress: 'aabb', creatorName: 'Alice',
      createdAt: 1, videoCid: 'cid-r1', source: 'discover',
    });
    expect(svc.discoverVines.find(v => v.id === 'recon-1')).toBeTruthy();
    // Now load follows — should move to followed
    await svc.loadFollowed();
    expect(svc.discoverVines.find(v => v.id === 'recon-1')).toBeFalsy();
    expect(svc.followedVines.find(v => v.id === 'recon-1')).toBeTruthy();
  });
```

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/vine-service.test.ts -- -t "reconciles"`
Expected: FAIL

- [ ] **Step 3: Add reconciliation to loadFollowed**

At the end of the `loadFollowed` method, after populating `followedAddresses`, add:

```typescript
    // Reconcile: move any vines from discover to followed that
    // arrived before the follow list was loaded.
    const toMove = this.discoverVines.filter(v => this.followedAddresses.has(v.creatorAddress));
    if (toMove.length > 0) {
      this.discoverVines = this.discoverVines.filter(v => !this.followedAddresses.has(v.creatorAddress));
      this.followedVines = [...this.followedVines, ...toMove];
      this.onChange?.();
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/vine-service.test.ts`
Expected: All tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/lib/vine-service.ts src/lib/vine-service.test.ts
git commit -m "feat: add startup reconciliation for misrouted vines"
```

---

## Task 9: Final integration test and cleanup

**Files:**
- All modified files

- [ ] **Step 1: Run the full frontend test suite**

Run: `npx vitest run`
Expected: All tests PASS

- [ ] **Step 2: Run Rust tests and check**

Run: `cd src-tauri && cargo test --lib && cargo clippy --all-targets`
Expected: All tests PASS, no clippy errors

- [ ] **Step 3: Remove the deprecated `vines` getter if no remaining references**

Search for remaining `svc.vines` or `.vines` references outside of tests. If the mock-data seeding and any remaining code has been updated, remove the deprecated getter from VineService. If references remain, keep it for now.

Run: `npx vitest run`
Expected: All tests still PASS

- [ ] **Step 4: Final commit**

```bash
git add -A
git commit -m "chore: cleanup and verify vine follow/feed integration"
```

- [ ] **Step 5: Push to remote**

```bash
git push origin main
```
