# ZEB-727 — received-grant GC via a convergent dismiss tombstone (design)

**Status:** settled 2026-07-21. Follow-on to ZEB-722 (owner-state map GC) in the
encrypted-file-sharing arc (ZEB-674 → 723 → 724 → 725 → 726, PRs #512–#516).

## Problem

`OwnerState.received_file_grants: BTreeMap<[u8; 32], ReceivedFileGrant>`
(owner_state_crdt.rs, serde key `"rg"`) holds grants this owner **received** —
encrypted files OTHERS shared with them, keyed by the shared file's root
ContentId. It is written only by `ingest_grant_push` (file_sharing.rs) on the
share path and projected straight to the "shared with me" rows (ZEB-723). Like
the ZEB-674 owner-side maps, it is **grow-only** and **fully replicated across
the owner's bound devices** (Flow A), so it only ever grows — a grantee has no
way to make a "shared with me" entry go away, and the map is an unbounded,
replicated liability at the scale this project targets.

ZEB-722 GC'd the two burn-triggered maps (`file_deks`, `file_grants`) but
**explicitly deferred** `received_file_grants` (ZEB-722 design §Scope → Deferred)
because it has a **different trigger**: it does not flow through `burn_content`
(no local sidecar entry), so the burn hook never reaches it.

## Two triggers — and why only one is reachable today

The ticket names two shrink triggers:

1. **Grantee dismisses** a "shared with me" entry — a grantee-local action.
2. **Owner revokes** the grant and that revocation **propagates to the grantee**.

A wire trace (file_sharing.rs `ingest_grant_push`; butler_deposit.rs
`DepositPayload`; lib.rs `revoke_read_impl`) shows trigger #2 has **no existing
signal to observe**:

- `ingest_grant_push` is the *only* writer of `received_file_grants`, and it runs
  only on the **share** path, driven by `DepositPayload.grant_push` (sealed-DEK
  blobs built at share time by `build_grant_push`).
- `DepositPayload` carries exactly one file-sharing field — `grant_push`
  (share only). There is **no revoke/inactive variant on the deposit wire.**
- The owner's `revoke_read_impl` only tombstones the owner-local `GrantEntry`
  (ZEB-725 LWW `revoked_at`), `notify_dirty`s to the **owner's own** devices, and
  emits the **owner's** UI event. It sends **nothing to the grantee** — no
  deposit, no frame. (It also, by design, leaves the DEK and the serve allowlist
  intact — an already-granted grantee keeps read access; crypto withdrawal needs
  rotation, which is out of scope everywhere in this lineage.)

So a grantee **cannot** detect an owner-side revoke today. Trigger #2 requires a
**new revoke-push wire** (a `DepositPayload` revoke variant + owner-side emission
+ grantee-side ingest) — a cross-cutting deposit-protocol feature, not a
state-hygiene change, and exactly the kind of wire-format work this lineage
splits into its own reviewed ticket (as ZEB-725 was split from ZEB-674).

**This design delivers trigger #1** (grantee dismiss) and the **convergent
tombstone substrate** that trigger #2 will later plug into. Trigger #2 →
new follow-up ticket (see §Scope).

## Why a plain `remove` is wrong here

`received_file_grants` merges as a **grow-only union with a deterministic
tie-break** in `owner_state_sync::merge_remote_into_local` (keep the
lexicographically smaller `sealed_dek`, then smaller `received_at`, so sibling
devices that re-sealed with fresh nonces converge byte-identically). Under
add-wins union, an entry removed on device A is **resurrected** the next time A
merges a stale sibling B that still holds it. Convergent removal therefore
requires a **tombstone that propagates the removal** — the same idiom ZEB-722,
ZEB-725, and the `SpaceId` tombstone set already use.

## Why NOT a permanent tombstone (the crux)

ZEB-722's `burned_content` is a **permanent, HLC-free** set, safe **only because
burn is terminal**: encrypted ingest mints a random DEK and ZEB-726 derives the
nonce from it, so a burned CID is cryptographically unreproducible and can never
legitimately return.

**Dismiss is not terminal.** The `received_file_grants` key is the shared file's
**root ContentId**, which is stable across re-shares of the same file. The owner
can revoke-and-re-share, or simply re-share, the *same file* — producing a new
`grant_push` for the **same CID**. A permanent dismiss-tombstone on that CID
would silently sweep every future re-share of that file to that grantee: a real
correctness bug (the grantee could never receive a file they once dismissed).

So the dismiss tombstone must be **LWW-timestamped**, mirroring ZEB-725's revoke
(`granted_at > revoked_at`), not ZEB-722's permanent set:

> a received grant is **active iff `received_at > dismissed_at`.**

A dismiss stamps `dismissed_at = now`; because the dismissed grant was received
in the past (`received_at < now`), it goes inactive. A later re-share arrives
through `ingest_grant_push`, which stamps a **fresh** `received_at = now_epoch_ms()`
(ingest-local wall clock, never wire-supplied) — now `received_at > dismissed_at`,
so the re-share is active again. Same wall-clock-timestamp assumption ZEB-725
already relies on.

## Design — an LWW dismiss tombstone

Add one field to `OwnerState`, next to `burned_content`
(owner_state_crdt.rs). Proposed canonical key `"dg"` (verify no collision with
existing 2-char keys at implementation; respect the CanonicalPayload
equal-length-key self-check):

```rust
/// ZEB-727: received-grant dismiss tombstones — `cid -> dismissed_at_ms`. A
/// grantee-local "hide this shared-with-me entry" that GCs the grow-only
/// `received_file_grants` entry AND keeps a stale sibling device from
/// resurrecting it on the add-wins union merge.
///
/// LWW-timestamped, NOT a permanent set (contrast `burned_content`): the CID is
/// the shared file's stable root ContentId, so the owner can legitimately
/// re-share the same file. A grant is ACTIVE iff `received_at > dismissed_at`,
/// so a re-share (fresh ingest `received_at`) reactivates over an older
/// dismissal — exactly ZEB-725's `granted_at > revoked_at` idiom. Absent on the
/// wire when empty (`skip_serializing_if` + `default`) so pre-ZEB-727 snapshots
/// load empty.
#[serde(rename = "dg", skip_serializing_if = "BTreeMap::is_empty", default)]
pub dismissed_received_grants: BTreeMap<[u8; 32], u64>,
```

### Dismiss (grantee-local)

New pure seam `file_sharing::dismiss_received_grant_inner(state, cid, now_ms) -> bool`:

```rust
pub fn dismiss_received_grant_inner(state: &mut OwnerState, cid: [u8; 32], now_ms: u64) -> bool {
    let removed = state.received_file_grants.remove(&cid).is_some();
    let slot = state.dismissed_received_grants.entry(cid).or_insert(0);
    let prev = *slot;
    *slot = (*slot).max(now_ms);          // LWW max-join, monotonic
    removed || *slot != prev              // changed → caller notify_dirty + emit
}
```

Recording the tombstone **even when the local entry is already gone** is
intentional: a sibling device may still hold the grant (not yet merged), and the
tombstone is what suppresses it there. Dismissing a never-received CID records a
harmless single-entry tombstone (the frontend only dismisses CIDs it got from
`list_received_grants`).

New backend IPC command `dismiss_received_grant(cid)` (net-new; ZEB-723 added
only the read-only `list_received_grants`), modelled on `revoke_read_impl`
(lib.rs): lock `crdt_state`, call the inner, and **iff it returns `true`**:
`sync_engine.notify_dirty()` + emit `"shared-with-me-updated"`. `notify_dirty`
is load-bearing — `received_file_grants` has **no deposit-rung re-delivery
backstop** (dm_inbox_ingest.rs), so persistence and Flow-A replication depend
entirely on it (ZEB-709). Register in `generate_handler!`.

**The UI dismiss button is deferred** (keeps this PR non-UI, off the pending
v0.2.0 UI pass): the command is the backend contract; wiring a button to
`invoke('dismiss_received_grant', { cid })` is a trivial follow-up.

### Projection (active filter)

`list_received_grants_inner` (file_sharing.rs) filters to active grants —
belt-and-suspenders with the merge sweep, and the same active-filter discipline
ZEB-725 applied in `list_grants_inner`:

```rust
.filter(|(cid, g)| {
    state.dismissed_received_grants.get(*cid).map_or(true, |&d| g.received_at > d)
})
```

### Merge convergence

In `merge_remote_into_local`, LWW-max-join the tombstone map, then **sweep**
`received_file_grants` against it **after** the existing union loop
(owner_state_sync.rs), structurally identical to the `burned_content` block but
with the `received_at > dismissed_at` predicate rather than set membership:

```rust
for (cid, dismissed_at) in dismissed_received_grants {          // LWW max-join
    let slot = local.dismissed_received_grants.entry(cid).or_insert(0);
    *slot = (*slot).max(dismissed_at);
}
local.received_file_grants.retain(|cid, g| {                   // disjoint-field
    local.dismissed_received_grants.get(cid).map_or(true, |&d| g.received_at > d)
});
```

(The disjoint-field borrow — `&mut received_file_grants` in `retain`,
`&dismissed_received_grants` in the closure — compiles for the same reason the
shipped `burned_content` sweep does.)

**Convergence sketch.** `dismissed_received_grants` is a join-semilattice under
per-key `max` (commutative, associative, idempotent). The sweep is a pure
function of the merged tombstone map and the merged grants map, so the post-merge
state is merge-order-independent. Device A dismissing while B still holds the
grant converge to `{dismissed_at present, entry absent}` on both. A re-share
(fresh `received_at > dismissed_at`) survives the sweep on both devices —
reactivation converges too.

## Scope

**In:**
- `dismissed_received_grants` field (owner_state_crdt.rs) + persisted mirror in
  `CrdtFileV2` and both `From` impls (owner_state_persist.rs), absent-loads-empty.
- `dismiss_received_grant_inner` + the `dismiss_received_grant` IPC command
  (file_sharing.rs, lib.rs); `notify_dirty` + `"shared-with-me-updated"` emit.
- Active filter in `list_received_grants_inner` (file_sharing.rs).
- Merge LWW-join + sweep (owner_state_sync.rs).

**Deferred (new follow-up ticket):**
- **Trigger #2 — owner-revoke → grantee prune.** Needs a new revoke-push wire: a
  `DepositPayload` revoke variant, owner-side emission from `revoke_read_impl`,
  and grantee-side ingest that stamps `dismissed_received_grants[cid]` (plugging
  into *this* tombstone). A deposit-protocol change with its own review surface;
  split out exactly as ZEB-725 was split from ZEB-674. The tombstone substrate
  this PR ships is what that ticket builds on.

**Not in scope (unchanged across this lineage):**
- Cryptographic access withdrawal — an already-delivered DEK cannot be withdrawn
  without rotation + re-encrypt. This GC is **state hygiene, not access
  revocation** (same caveat as ZEB-725).

## Testing

Keychain-free, wall-clock-free; mirror the ZEB-722 merge/persist tests.

1. **dismiss GCs the received grant** — seed `received_file_grants[cid]` →
   `dismiss_received_grant_inner(now)` → assert entry gone, `dismissed_received_grants[cid] == now`,
   and the return is `true`.
2. **merge converges across devices** — A dismisses; B still holds the grant;
   merge both directions → both reach `{tombstone, no entry}` (order-independent,
   mirror `merge_sweeps_burned_cid_and_is_order_independent`).
3. **re-share reactivates** — after dismiss (`dismissed_at = d`), a fresh grant
   with `received_at > d` survives both the projection filter and the merge sweep
   on both devices (this is the test that would FAIL under a permanent tombstone —
   the regression guard for the design crux).
4. **stale re-share stays suppressed** — a grant with `received_at <= dismissed_at`
   is filtered/swept (no resurrection).
5. **projection hides dismissed** — `list_received_grants_inner` omits a grant
   whose `dismissed_at >= received_at`.
6. **persistence round-trip** — `dismissed_received_grants` survives
   `save_crdt`/`load_crdt`; a pre-ZEB-727 snapshot (field absent) loads empty.
7. **IPC** — `dismiss_received_grant_impl` mutates owner-state and (on change)
   notify_dirty + emits `"shared-with-me-updated"` (mirror the ZEB-723 emit test).

Full `nextest --workspace --all-targets --features test-fixtures` + CI-exact
clippy + fmt green locally before PR.

## Non-goals

- Bounding `dismissed_received_grants` itself (~40 bytes per distinct dismissed
  file, permanent like `burned_content` / the `SpaceId` tombstone set; re-dismiss
  reuses the same key via max-join, so it's bounded by distinct dismissed CIDs —
  negligible, YAGNI now).
- Replay hardening of `grant_push` (a replayed *original* share re-ingests with a
  fresh `received_at` and would reactivate a dismissed grant; deposit-layer replay
  protection is a separate concern, and functionally the owner's grant is still
  active so re-surfacing is not a correctness violation — the grantee re-dismisses).
