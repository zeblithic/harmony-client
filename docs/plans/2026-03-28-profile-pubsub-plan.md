# Profile Pub/Sub — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish your profile to the network via Zenoh and discover other peers' profiles. First real content transport in harmony-client.

**Architecture:** When the user saves their profile, the Tauri backend publishes the profile JSON to `harmony/profile/{address}` via Zenoh PUT. The backend subscribes to `harmony/profile/*` and emits Tauri events for discovered peer profiles. The frontend's ZenohService gains a `peerProfiles` map. Profile data is inline JSON (not CAS-backed yet — CAS references can be added later).

**Tech Stack:** Rust (Tauri, zenoh), TypeScript (Svelte 5, vitest)

---

## File Map

| File | Responsibility |
|------|---------------|
| `src-tauri/src/lib.rs` | `publish_profile` command + profile subscriber in connect flow |
| `src/lib/zenoh-service.ts` | `peerProfiles` map, `profile-update` event handler |
| `src/lib/zenoh-service.test.ts` | Tests for profile events |
| `src/NetworkApp.svelte` or `src/App.svelte` | Wire profile publish on save (when connected) |

---

### Task 1: Backend — publish_profile command + profile subscriber

**Files:** Modify `src-tauri/src/lib.rs`

- [ ] **Step 1: Add `publish_profile` Tauri command**

A new async command that PUTs the profile JSON to `harmony/profile/{address}`:

```rust
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePayload {
    pub address: String,
    pub display_name: String,
    pub status_text: Option<String>,
    pub avatar_url: Option<String>,
}

#[tauri::command]
async fn publish_profile(
    profile: ProfilePayload,
    state: tauri::State<'_, Mutex<ZenohState>>,
) -> Result<(), String> {
    let session = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.session.clone().ok_or("not connected")?
    };
    let key = format!("harmony/profile/{}", profile.address);
    let payload = serde_json::to_vec(&profile)
        .map_err(|e| format!("serialize: {e}"))?;
    session.put(&key, payload)
        .await
        .map_err(|e| format!("put failed: {e}"))?;
    Ok(())
}
```

Register in `run()`.

- [ ] **Step 2: Add profile subscriber to connect flow**

In the subscriber task spawned by `connect_zenoh`, add a second subscriber for `harmony/profile/*`. When a profile arrives, emit a `"profile-update"` Tauri event with the parsed ProfilePayload.

Alternatively, spawn a second subscriber alongside the capacity one. The simplest approach: add another `declare_subscriber` + recv loop in the same task, using `tokio::select!`.

Actually simpler: spawn a separate task for the profile subscriber, stored alongside the capacity task. OR combine both subscribers into one task using `tokio::select!`.

The cleanest approach for this first slice: declare both subscribers before the task, then `select!` between them in the loop.

- [ ] **Step 3: Add unit test for profile payload parsing**

Add a test for `ProfilePayload` serde roundtrip.

- [ ] **Step 4: Verify and commit**

```bash
cd src-tauri && cargo check && cargo test
git add src-tauri/src/lib.rs
git commit -m "feat(tauri): add publish_profile command and profile subscriber"
```

---

### Task 2: Frontend — peerProfiles in ZenohService

**Files:** Modify `src/lib/zenoh-service.ts`, `src/lib/zenoh-service.test.ts`

- [ ] **Step 1: Add peerProfiles map and profile-update listener**

Add to ZenohService:
```typescript
peerProfiles: Map<string, ProfilePayload> = new Map();
```

In `init()`, listen for `"profile-update"` events and upsert into `peerProfiles`:
```typescript
const unlistenProfile = await this.adapter.listen(
  'profile-update',
  (event) => {
    if (this.connectionStatus !== 'connected') return;
    const profile = event.payload as ProfilePayload;
    this.peerProfiles.set(profile.address, profile);
    this.onChange?.();
  },
);
this.unlisteners.push(unlistenProfile);
```

Clear `peerProfiles` in `disconnect()`.

Add `ProfilePayload` type:
```typescript
export interface ProfilePayload {
  address: string;
  displayName: string;
  statusText?: string;
  avatarUrl?: string;
}
```

- [ ] **Step 2: Add `publishProfile` method**

```typescript
async publishProfile(profile: ProfilePayload): Promise<void> {
  await this.adapter.invoke('publish_profile', { profile });
}
```

- [ ] **Step 3: Add tests**

- `profile-update` event upserts into peerProfiles
- `disconnect` clears peerProfiles
- `publishProfile` invokes `publish_profile` command

- [ ] **Step 4: Verify and commit**

```bash
npx vitest run
git add src/lib/zenoh-service.ts src/lib/zenoh-service.test.ts
git commit -m "feat: add peerProfiles map and publishProfile to ZenohService"
```

---

### Task 3: Wire profile publish into App

**Files:** Modify `src/App.svelte`

- [ ] **Step 1: Update handleProfileSave to publish when connected**

In `App.svelte`, update `handleProfileSave` to also publish via Zenoh when connected:

```typescript
function handleProfileSave(profile: Profile) {
  saveProfile(profile);
  myProfile = profile;
  // Publish to network if connected
  // (zenohService is on NetworkApp, not App — need to wire through)
}
```

Actually, the profile editor is in `App.svelte` but `ZenohService` is in `NetworkApp.svelte`. The simplest approach: add a `publish_profile` Tauri command call directly from App.svelte using the TauriAdapter pattern, OR lift zenohService to be accessible from App.

For this slice: just call `invoke('publish_profile', { profile: { address, displayName, statusText, avatarUrl } })` directly from App.svelte when Tauri is available. No need to go through ZenohService for the publish direction.

```typescript
async function handleProfileSave(profile: Profile) {
  saveProfile(profile);
  myProfile = profile;
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    await invoke('publish_profile', {
      profile: {
        address: profile.address,
        displayName: profile.displayName,
        statusText: profile.statusText,
        avatarUrl: profile.avatarUrl,
      },
    });
  } catch {
    // Not in Tauri or not connected — profile saved locally only
  }
}
```

- [ ] **Step 2: Verify all tests and build**
- [ ] **Step 3: Commit**

```bash
git add src/App.svelte
git commit -m "feat: publish profile to Zenoh on save when connected"
```
