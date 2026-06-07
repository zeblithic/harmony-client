# ZEB-395 — Community content serve policy (v1) design

**Status:** Draft for review
**Linear:** ZEB-395 (blocker for ZEB-330 cross-WAN first-contact / ZEB-366)
**Date:** 2026-06-07
**Branch:** `zeb-395-community-content-serve`

## 1. Problem

Cross-machine community sync (Koya ↔ Ildwyn) fails: a joiner redeems an invite, the
session is healthy, but no membership/channels ever propagate (`channels=[]` on the
joiner, both directions).

Root-caused empirically with Koya-side instrumentation during a real redeem
(community `5bbfe67d…`, Ildwyn node `39753b0b…`):

- The iroh invite handshake completes; the zenoh session is bidirectional; state-root
  pub/sub is exchanged; the publisher-membership gate **passes**.
- The **only** failure is the CAS fetch of the encrypted community state-root blob.
  Each side's content-serve queryable returns an empty final reply for the other's
  root CID, and the receiver's `handle_incoming_publish` ends in
  `ErrPreMutation(ContentStore(Io("fetch '…': no successful reply")))`.

The cause is the ZEB-343 CAS content-serve gate. `spawn_content_serve_queryable`
(`src-tauri/src/event_loop.rs`) refuses to serve **any** CID whose `encrypted` flag is
set:

```rust
if cid.flags().encrypted {
    continue; // → empty final reply
}
```

The gate exists so a node does not serve *private* encrypted blobs (DMs, private
profiles) to arbitrary requesters. But the community state-root blob is encrypted with
the community **epoch key** (`encrypt_blob`, `community_state_sync.rs`), so it carries
the encrypted flag too — and the gate refuses it. Community state is
**encrypted-but-shared-among-members**, a class the gate never accounted for. Result:
neither member will serve the community root, so the CRDT never transfers.

### Why tests never caught it

`tests/community_sync_integration.rs` wires both engines to a **shared** CAS
(`spawn_shared_cas()`, `Arc::clone(&cs)` for both registries). `content_store.get(root_cid)`
therefore always resolves locally and never traverses the content-serve queryable, so
the encrypted-serve gate is never exercised in the existing tests. The bug only appears
when the two members have **separate** content stores reachable solely over the serve
queryable — i.e., cross-machine.

## 2. Goal & non-goals

**Goal:** Members can fetch each other's encrypted community state-root blob over CAS,
unblocking community sync — without re-opening serving of private encrypted content.

**Non-goals (v1):**
- Membership-authenticated serve (verify the requester is a member before serving).
  Deferred as a hardening follow-up (see §7).
- Serving encrypted community content *other than* state-root blobs (attachments,
  encrypted avatars). Same mechanism can extend later; not required to unblock sync.
- Pruning/eviction of the allowlist (see §5; not needed for correctness or safety).
- The owner-power bug (ZEB-396) and durability bug (ZEB-393) — separate.

## 3. Approach (chosen: serve-allowlist)

Serve an encrypted CID **only if it is a community state-root blob this node has
published** for a community it belongs to. Maintain a small in-memory allowlist of
those root CIDs; the content-serve handler consults it before refusing an encrypted CID.

### Why this is safe

1. **The blob is epoch-key ciphertext.** Serving it to a non-member yields useless
   bytes; only members holding the epoch key can decrypt it.
2. **The root CID is a capability.** The state-root *publish* is itself epoch-key
   encrypted (`decrypt_root_publish`). A non-member subscribing to the community topic
   receives ciphertext and cannot learn the `root_cid` to request it. In practice, only
   a member who decrypted a publish can name the CID — so serving "by allowlisted CID"
   is *implicitly* member-gated.

The residual exposure is metadata (a requester who already knows a community root CID
learns this node hosts it, plus its size). Accepted for v1; approach 2 (§7) closes it.

## 4. Design

### 4.1 The allowlist type

New type in `src-tauri/src/content_store.rs` (neutral home; both
`community_state_sync` and `event_loop` already import this module):

```rust
/// Set of community state-root CIDs this node is willing to serve even though
/// they carry the `encrypted` flag (ZEB-395). Community roots are epoch-key
/// ciphertext shared among members; serving them is safe (see design §3). Private
/// encrypted blobs (DMs, private profiles) are never inserted, so they stay refused.
#[derive(Clone, Default)]
pub struct CommunityServeAllowlist(std::sync::Arc<std::sync::RwLock<std::collections::HashSet<ContentId>>>);

impl CommunityServeAllowlist {
    pub fn new() -> Self { Self::default() }

    /// Mark a community-root CID serveable. Idempotent.
    pub fn allow(&self, cid: ContentId) {
        if let Ok(mut g) = self.0.write() {
            g.insert(cid);
        }
    }

    /// True if `cid` is an allowlisted community-root CID. Locks internally and
    /// returns a bool — never holds the guard across an `.await`.
    pub fn contains(&self, cid: &ContentId) -> bool {
        self.0.read().map(|g| g.contains(cid)).unwrap_or(false)
    }
}
```

`std::sync::RwLock` (not tokio) is correct: `allow`/`contains` lock, mutate/read, and
drop the guard synchronously — no guard is ever held across an `.await`.

### 4.2 Registration (one hook)

The only production site that writes a community-root blob to CAS is
`publish_root_now` (`community_state_sync.rs`, the `content_store.put(root_cid, …)`
after `encrypt_blob` + `ContentId::for_book`). Immediately after the successful `put`,
register the CID:

```rust
ctx.content_store.put(root_cid, blob_ciphertext).await?;
ctx.serve_allowlist.allow(root_cid); // ZEB-395
```

This single hook covers both lifecycles:
- **On change:** every publish registers the new root.
- **On boot/restart:** each engine performs an initial publish at spawn (observed in
  the instrumented logs), which re-registers the current root — so a freshly-booted node
  can serve its existing community root without special boot-load handling.

(The test-only `build_signed_publish` helper at `community_state_sync.rs:~4664` is not a
production put and needs no hook.)

### 4.3 Threading the handle

`CommunityServeAllowlist` is created once in `event_loop::run` (where both the community
registry and the content-serve queryable are set up) and shared by clone:

- Add `serve_allowlist: CommunityServeAllowlist` to **`CommunityRegistryConfig`**
  (`community_state_sync.rs:~3584`). The registry clones it into each spawned engine's
  **`CommunitySyncEngineConfig`** (`:~777`), which carries it into the engine's
  **`InternalCtx`** (`:~1715`, alongside `content_store`) so `publish_root_now` can call
  `ctx.serve_allowlist.allow(...)`.
- Pass the same clone to `spawn_content_serve_queryable` as a new parameter.

### 4.4 Serve-handler gate change

In `spawn_content_serve_queryable` (`event_loop.rs`), relax the encrypted refusal:

```rust
// Before:
// if cid.flags().encrypted { continue; }

// After (ZEB-395): refuse encrypted CIDs UNLESS they are allowlisted community roots.
if cid.flags().encrypted && !serve_allowlist.contains(&cid) {
    continue; // private encrypted content stays unservable
}
```

Everything downstream is unchanged: the local `lookup` still returns the stored
ciphertext, `cid.verify_hash(&bytes)` still holds (the CID is `for_book` over the
ciphertext), and `query.reply` returns it. The fetching member decrypts with the epoch
key it already holds.

### 4.5 Data flow (post-fix, happy path)

1. Koya mints → `publish_root_now` puts root `K`, `allow(K)`.
2. Ildwyn redeems (bootstrap from invite), subscribes, publishes its root `I`, `allow(I)`.
3. Koya receives publish `I`; fetches `I`'s blob from Ildwyn → Ildwyn's serve queryable
   serves it (allowlisted) → Koya decrypts + merges → Koya re-publishes `K2`, `allow(K2)`.
4. Ildwyn receives `K2`; fetches from Koya → served (allowlisted) → decrypts + merges →
   **channels + members appear.** Convergence proceeds, each member serving its own roots.

## 5. Allowlist lifecycle / bounding

v1 is **insert-only**; no eviction. Justification:
- A `ContentId` is small; community roots change rarely (membership/channel edits), so
  per-session growth is negligible.
- Serving an old, already-published community-root ciphertext is harmless — it is
  ciphertext we already chose to publish, decryptable only with the corresponding epoch
  key, which the requester would need anyway.

A future bound (per-community last-N, or prune on `leave_community`) is a cheap
follow-up if a long-lived, high-churn session ever warrants it. Noted, not done.

## 6. Testing

1. **Unit — serve gate (`event_loop` or a focused unit):**
   - encrypted CID **in** allowlist → served (bytes returned).
   - encrypted CID **not** in allowlist → refused (empty final), as today.
   - unencrypted CID → served (unchanged behavior).

2. **Integration — cross-store serve (regression; model on
   `tests/cas_serve_two_node_integration.rs`):** two nodes with **separate** content
   stores wired through `spawn_content_serve_queryable`. Node A puts an **encrypted**
   blob and `allow()`s its CID; node B fetches it and succeeds. A second encrypted CID
   that is *not* allowlisted must still fail. This is the test that would have caught the
   bug (the existing community-sync test shares a CAS and cannot).

3. **Integration — community sync over separate stores (end-to-end):** extend/add a
   two-engine community-sync test that uses **separate** CAS instances routed through the
   serve queryable (not `spawn_shared_cas`), publish a channel-config change on A, assert
   B materializes it. Confirms the full membership/channel path, not just the byte fetch.
   (If wiring a full second store through the registry is too heavy for one test, the
   §6.2 focused regression plus the live re-test in §8 cover the gap; decide during
   planning.)

4. **Gates:** `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets
   --features test-fixtures -- -D warnings`, `cargo nextest run --locked -p harmony-app`
   (lib + the touched integration tests). Frontend untouched.

## 7. Follow-ups (out of scope for v1)

- **Approach 2 — membership-gated serve (hardening):** requester attaches a signed
  membership proof to the content GET; the server verifies it against materialized
  membership before serving. Closes the metadata exposure in §3. Needs new plumbing on
  both fetch and serve sides (requester identity is not currently carried on content
  GETs). File as a separate hardening ticket.
- **Allowlist bounding** (§5) if needed.
- **Serving other encrypted community content** (attachments, encrypted avatars) by
  registering their CIDs through the same mechanism.

## 8. Re-test / rollout

After implementation lands on `zeb-395-community-content-serve`:
1. Re-apply the saved diagnostic instrumentation
   (`/tmp/zeb-395-diagnostic-instrumentation.patch`) on top for the live session.
2. Launch Koya, mint a fresh invite-only community, have Ildwyn redeem.
3. Expect, on Koya's log: `content-serve HIT — replied with bytes` for the root CID and
   `outcome=…Applied…` (not `ErrPreMutation`). On Ildwyn: `channels=[#general]`,
   `members=2`, messages propagate. This closes ZEB-366 / ZEB-330 DoD#3.
4. Strip the diagnostic instrumentation before finalizing the PR (it is never committed).

## 9. Risks

- **Convergence still depends on publishes being exchanged.** Confirmed already
  happening in the captured run (joiner's publish reaches the inviter, which merges and
  re-publishes), so serving the content is the only missing piece. Low risk.
- **The fetcher must hold the epoch key** to decrypt the served blob. Already true: both
  sides decrypt each other's 245-byte publishes today (same epoch key), so blob decrypt
  will succeed. Low risk.
