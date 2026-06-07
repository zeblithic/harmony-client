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

### 4.2 Registration (one hook, via the ContentStore trait)

`publish_root_now` (`community_state_sync.rs`) is the only production site that
writes a community-root blob to CAS — `content_store.put(root_cid, …)` after
`encrypt_blob` + `ContentId::for_book`. The fix changes that single call to a new
trait method, `put_serveable`:

```rust
// community_state_sync.rs, publish_root_now (replaces the bare `.put`):
ctx.content_store
    .put_serveable(root_cid, blob_ciphertext)
    .await?;
```

`put_serveable` is added to the `ContentStore` trait with a **default
implementation identical to `put`**, so `InMemoryStub` and every test store
inherit it unchanged and only the production store overrides it:

```rust
// content_store.rs, trait ContentStore:
/// Like `put`, but also marks `cid` serveable to peers even though it carries
/// the `encrypted` flag (ZEB-395 community-root sharing). Default == `put`;
/// only RuntimeContentStore registers the CID in its shared allowlist.
async fn put_serveable(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
    self.put(cid, blob).await
}
```

`RuntimeContentStore` (the production impl) overrides it — normal `put`, then on
success register the CID in its (optional) shared allowlist handle:

```rust
async fn put_serveable(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
    self.put(cid, blob).await?;
    if let Some(allowlist) = &self.serve_allowlist {
        allowlist.allow(cid); // synchronous insert into the shared set
    }
    Ok(())
}
```

This covers both lifecycles:
- **On change:** every publish registers the new root.
- **On boot/restart:** each engine performs an initial publish at spawn (observed in
  the instrumented logs), which re-registers the current root — so a freshly-booted node
  can serve its existing community root without special boot-load handling.

**No race:** the allowlist insert completes inside `put_serveable`, which returns
*before* `publish_root_now` builds and broadcasts the state-root envelope that
announces `root_cid`. A peer can only learn the CID from that later publish, so the
CID is always allowlisted before any peer can request it.

(The test-only `build_signed_publish` helper at `community_state_sync.rs:~4664` is not a
production put and needs no hook. `owner_state_sync`'s owner-root put stays on plain
`put` — owner multi-device sync over CAS is out of scope for ZEB-395; see §7.)

### 4.3 Threading the handle (Arc-shared, no config changes)

`CommunityServeAllowlist` wraps an `Arc`, so a single instance is shared by clone
between the two production sites that need it — with **no new fields on
`CommunityRegistryConfig` / `CommunitySyncEngineConfig` / `InternalCtx`** and **no
change to the `CasOp` enum**. (Config-threading was the original sketch; it was
dropped because it would force ~43 mechanical `serve_allowlist: Default::default(),`
edits across the community test suite, and extending `CasOp::PutLocal` would break
~25 exhaustive match sites — a large, merge-conflict-prone diff for a one-line
behavior change. The Arc-shared approach is behavior-identical and touches far
fewer files.)

The instance is created once in `lib.rs::start_node`, where both the production
`RuntimeContentStore` and `event_loop::run` are already constructed:

```rust
let serve_allowlist = crate::content_store::CommunityServeAllowlist::new();

let content_store: Arc<dyn ContentStore> = Arc::new(
    RuntimeContentStore::new(cas_op_tx.clone(), fetch_timeout)
        .with_serve_allowlist(serve_allowlist.clone()), // registration side
);
// ... later, passed as a new trailing argument:
event_loop::run(/* … */, serve_allowlist.clone()).await;  // serve side
```

- `RuntimeContentStore::new` is **unchanged**; a new chained builder
  `with_serve_allowlist(self, allowlist) -> Self` sets the optional handle, so the
  ~10 `RuntimeContentStore::new(...)` test call sites need no edits.
- `event_loop::run` gains **one** trailing parameter; it has exactly one
  production call site and no test call sites, so this is a single-line edit.
- `event_loop::run` passes the handle into `spawn_content_serve_queryable` (new
  trailing parameter; its 5 test call sites pass an empty `CommunityServeAllowlist`).

### 4.4 Serve-handler gate change

`spawn_content_serve_queryable` (`event_loop.rs`) gains a `serve_allowlist:
CommunityServeAllowlist` parameter, and the encrypted refusal is relaxed to consult it:

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

1. **Unit — `CommunityServeAllowlist` (`content_store.rs`):** `allow` then `contains`
   returns true; `contains` on an un-added CID returns false; `Clone` shares state
   (allow on one clone is visible via another). Cheap, no async.

2. **Unit — `put_serveable` registration (`content_store.rs`):** a `RuntimeContentStore`
   built `.with_serve_allowlist(a)` over a stub `cas_op` receiver, after
   `put_serveable(cid, blob)`, leaves `a.contains(&cid) == true`; plain `put(cid2, …)`
   leaves `a.contains(&cid2) == false`. The default trait impl (`InMemoryStub`) routes
   `put_serveable` to `put` with no panic.

3. **Unit — serve gate (`event_loop` or a focused unit):**
   - encrypted CID **in** allowlist → served (bytes returned).
   - encrypted CID **not** in allowlist → refused (empty final), as today.
   - unencrypted CID → served (unchanged behavior).

4. **Integration — cross-store serve (regression; model on
   `tests/cas_serve_two_node_integration.rs`):** two nodes with **separate** content
   stores wired through `spawn_content_serve_queryable`. Node A’s queryable is passed a
   `CommunityServeAllowlist` containing one **encrypted** CID; node B fetches that CID
   and succeeds. A second encrypted CID that is *not* in the allowlist must still fail
   (use a public control CID for the liveness proof, exactly like the existing
   `does_not_serve_encrypted_cid` test). This is the test that would have caught the bug
   (the existing community-sync test shares a CAS and cannot).

5. **Integration — community sync over separate stores (end-to-end): DEFERRED to the
   §8 live re-test.** With the Arc-shared design, exercising the full
   membership/channel path over separate stores requires standing up
   `RuntimeContentStore` + a live `event_loop::run` per node — far heavier than the
   `spawn_shared_cas` harness supports, and not worth a bespoke test fixture. The §6.4
   focused cross-store serve regression proves the byte-fetch fix; the §8 two-machine
   live re-test proves the full membership/channel propagation. (A future
   separate-store community-sync harness is noted as a §7 follow-up.)

6. **Gates:** `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets
   --features test-fixtures -- -D warnings`, `cargo nextest run --locked -p harmony-app`
   (lib + the touched integration tests). Frontend untouched. Note: the full
   `--all-targets` clippy is load-bearing — `--lib` clippy does NOT compile `#[cfg(test)]`
   modules, so a lint inside a unit-test helper only surfaces under `--all-targets`.

## 7. Follow-ups (out of scope for v1)

- **Approach 2 — membership-gated serve (hardening):** requester attaches a signed
  membership proof to the content GET; the server verifies it against materialized
  membership before serving. Closes the metadata exposure in §3. Needs new plumbing on
  both fetch and serve sides (requester identity is not currently carried on content
  GETs). File as a separate hardening ticket.
- **Allowlist bounding** (§5) if needed.
- **Serving other encrypted community content** (attachments, encrypted avatars) by
  registering their CIDs through the same mechanism (call `put_serveable` instead of
  `put` at those publish sites).
- **Separate-store community-sync test harness** — the §6.5 end-to-end test, if a
  reusable `RuntimeContentStore`-backed two-node fixture is built later.
- **Owner-state multi-device sync over CAS** — `owner_state_sync`'s encrypted owner
  root currently uses plain `put` (not serveable). If cross-device owner-state fetch
  over CAS is ever needed, switch that site to `put_serveable` under its own ticket.

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
