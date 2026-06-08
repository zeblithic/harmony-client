# ZEB-388 — "Share my key" affordance (expose local identity pub hex)

**Status:** Approved 2026-06-08
**Issue:** [ZEB-388](https://linear.app/zeblith/issue/ZEB-388) (Medium, harmony-client)
**Surfaced by:** ZEB-330 / ZEB-366 cross-WAN first-contact testing (sibling finding to ZEB-385)

## Problem

The connectivity surface has commands that **consume** a peer's 64-byte transport
identity pub hex — `connectivity_discover_identity { identityPubHex }` and
`add_friend_by_key { identityPubHex }` — but **no command that returns the local
node's own identity pub hex**. The value exists internally as
`NodeState.dm_identity_pub_64` (`X25519_pub(32) ‖ Ed25519_pub(32)`, set in
`start_node`) but is never surfaced.

Consequences:

- The cross-WAN playbook's Stage-B raw-key identity discovery is **not runnable** —
  neither machine can obtain the hex to hand to its peer.
- `FriendsPanel`'s "Add friend by key" can only ever be the *receiving* side; there
  is no affordance to display/copy your own key for someone else to paste. (The only
  self-identity share today is the `generate_friend_token` URL flow.)

The closest existing getters are all the wrong value: `current_identity_hash` (32-hex
identity **hash**), `connectivity_get_my_reachability_record` (iroh **node_id**),
`get_owner_state` (16-byte master id).

## Goal

Expose the local node's 64-byte transport identity pub so a user can copy it and a
peer can paste it into "Add friend by key" — making the cross-WAN playbook's raw-key
discovery runnable and giving FriendsPanel a *send* side.

## Architecture

A read-only vertical slice with **no new state**: the value already lives in
`NodeState.dm_identity_pub_64`. One Rust getter IPC → one TS wrapper → one
FriendsPanel affordance. Three small, independently testable units.

## Components

### 1. Rust IPC — `connectivity_get_my_identity_pub_hex`

```rust
/// Returns the local node's 64-byte transport identity pub as 128 lowercase
/// hex chars — exactly the value `add_friend_by_key` / `connectivity_discover_identity`
/// consume. `Ok(None)` when the node isn't started / no owner identity is loaded.
#[tauri::command(rename_all = "snake_case")]
async fn connectivity_get_my_identity_pub_hex(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Option<String>, String> {
    let g = state.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
    Ok(my_identity_pub_hex(g.dm_identity_pub_64))
}
```

The encoding logic is extracted into a pure helper so it is deterministically
unit-testable without constructing a live `NodeState` (the same pattern ZEB-385 used
for `log_dir_in`; the connectivity getters have no IPC-level unit tests today
precisely because `NodeState` is expensive to build):

```rust
/// Pure: map the optional 64-byte identity pub to its lowercase hex encoding.
fn my_identity_pub_hex(pub64: Option<[u8; 64]>) -> Option<String> {
    pub64.map(hex::encode)
}
```

Mirrors `connectivity_get_my_reachability_record` (lock → read one field →
`Ok(None)` when absent). Registered in the `invoke_handler!` registry.

### 2. TS wrapper — `FriendService.getMyIdentityPubHex`

```typescript
/** Returns the local node's 64-byte transport identity pub as hex, or null
 *  when the node isn't started. Arg naming N/A (no args). */
async getMyIdentityPubHex(): Promise<string | null> {
  return this.invoke<string | null>('connectivity_get_my_identity_pub_hex', {});
}
```

Mirrors the existing `addByKey` / `generateFriendToken` wrappers in
`friend-service.ts`.

### 3. FriendsPanel affordance — "My key"

A new `action-block` paired with "Add friend by key" (produce ↔ consume symmetry),
fetched on mount into `myKeyHex: string | null`:

- **key present:** a readonly display of the full hex (selectable, like the existing
  friend-URL input) + a **Copy** button that reuses the exact
  `navigator.clipboard.writeText` try/catch from the existing `handleCopy`.
- **null (node not started / owner not loaded):** a muted
  "Start your node to view your key" line — neutral state, **not** an error.

## Data flow & error handling

Panel mount → `getMyIdentityPubHex()` → render. The IPC returns `Ok(None)` (never
`Err`) when the node isn't up, so the UI shows the neutral state rather than an error
toast. Clipboard failure stays silent (the value remains selectable in the readonly
field), matching the existing copy handler.

## Privacy

The transport identity **pub** is public key material designed to be shared for
discovery — peers already consume it via `discover_identity` / `add_friend_by_key`.
This is **not** the master/owner secret. The IPC is a read-only getter with zero
write surface.

## Testing

- **Rust:** unit-test `my_identity_pub_hex` — `None → None`; a known fixture
  (`[0xAB; 64]`) → exact 128-char lowercase hex string (asserts encoding correctness
  and length).
- **Frontend wrapper:** `friend-service.test.ts` — asserts the command name
  (`connectivity_get_my_identity_pub_hex`), empty `{}` args, and `string | null`
  passthrough (mirrors the existing `addByKey` test).
- **Component:** net-new `FriendsPanel.test.ts` (the `@testing-library/svelte`
  harness already exists — `Layout.test.ts` is the template) — inject a mock service:
  (a) returns a key → My-key block renders + clicking Copy calls
  `clipboard.writeText` with the **full** hex; (b) returns null → muted message, no
  Copy button.

## Out of scope (YAGNI)

- QR codes, chunked/grouped hex formatting, or a "friendly" short form — the raw
  128-char hex is what the paste-side consumes, and the `generate_friend_token` URL
  flow already covers the friendly-share path.
- Auto-refresh of the key display when the node starts after the panel mounts — the
  user can reopen the panel; not worth a reactive subscription.
- Surfacing the key anywhere besides FriendsPanel (e.g. a Settings/Profile pane).
