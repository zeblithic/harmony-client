# ZEB-839 — Durable last-known peer-profile cache + resolve peer identity across all surfaces

- **Ticket:** [ZEB-839](https://linear.app/zeblith/issue/ZEB-839)
- **Date:** 2026-07-31
- **Status:** Design — awaiting review
- **Parent design:** [`docs/specs/2026-05-30-zeb-341-profile-cards-design.md`](../../specs/2026-05-30-zeb-341-profile-cards-design.md) (the profile-card system this extends)
- **Related:** ZEB-774 (live-resolution latency), ZEB-777 (roster DTO displayName), ZEB-432 (community-surface resolution), ZEB-568 (eager re-broadcast), ZEB-586 (own-profile cache, cross-identity leak lesson)

## 1. Problem

Once we've seen a peer's profile (at least their display name), that knowledge is **discarded** and the UI reverts to a raw truncated `owner_id` hex prefix (e.g. `2e9a2151`). It hurts most in two situations the user actually hits:

- **The peer is offline.** Names arrive only via a *live* profile-card broadcast; an offline peer never re-broadcasts, so we have nothing to show.
- **After a restart.** The cache is in-memory only and starts empty on every launch, so previously-known names vanish until each peer happens to re-broadcast while we're subscribed.

A profile system exists (ZEB-341): a user's name/status is signed into a cert-bound `ProfileCardBroadcast`, published over Zenoh, verified by subscribers, and memoized in `ProfileCardCache`. Community message authors and the member roster resolve through a proper ladder — **friend nickname → profile-card name → truncated hex** (ZEB-432/777). It works *while everyone is online and freshly subscribed*, and forgets everything otherwise.

### 1.1 The symptom decomposes into three independent mechanisms

Investigation (two code-explorer passes, 2026-07-31) found the one symptom has three distinct causes on different surfaces:

1. **No durability (the core, untracked gap).** `ProfileCardCache` is `Mutex<HashMap<SubscriptionId, CardSlot>>` (`profile_card_broadcast.rs:286`) — in-memory *by deliberate original design* (ZEB-341 listed "disk-persisted card cache" under Out of scope). It is rebuilt fresh on every `start_node`, wiped to `None` on `stop_node`/identity-switch, and never touches any persistence layer. The Zenoh subscription is **live-only / non-retained** — you only receive samples published *after* you subscribe. Two in-code comments already state this: `profile_card_broadcast.rs:409` *"Name/status are NOT persisted backend-side"*, and the ZEB-777 note at `lib.rs:29296` — *"on a freshly-restarted node nothing re-publishes them (ZEB-774) — measured 2026-07-26, this cache named 0 of 3 peers on a live node."*
2. **DMs never resolve at all.** `message-service.ts` hardcodes the DM sender to `from.slice(0,8)` on every message (`:208-213` dm-received, `:481-483` scrollback), forever — it never consults the card cache or friend nicknames. DM bubbles show hex even when the name is known elsewhere in the app. Missing wiring, independent of durability.
3. **Nav row / DM header name is actively clobbered.** In `nav-service.ts`, `addOrUpdateNavSpace` (`:210-376`) overwrites `NavNode.name` unconditionally from every `nav-updated` payload, discarding a good name a prior `profile-update` (`:145-176`) had set — with no re-fill for an offline peer. The code even computes a laddered `peer.displayName` (`:310-317`) but stores it on `node.peer.displayName` (used only by the Avatar), not `node.name` (the rendered text).

**Why all three:** a durable cache (fix #1) is necessary but not sufficient. If the "reverts to hex" moment is in a DM or the sidebar, #2 and #3 still bite with a perfect cache in place, because those paths don't read the cache (#2) or overwrite its result (#3).

## 2. Goal & non-goals

**Goal:** once we have verified a peer's profile card, that peer renders by their **last-known display name (and status)** on every surface — message authors, member roster, DM bubbles, DM header, nav rows — **even while the peer is offline and across app restarts**. When the peer is online and re-broadcasts, the live value transparently refreshes the stored one.

**Non-goals (v1):**
- Durably caching avatar **image bytes** for offline rendering — fast-follow (§7). We persist the avatar **CID** now (free), but rendering a picture offline needs a separate CAS blob cache.
- Proactive backend re-subscribe-to-all-known-peers on boot — not needed; serve-stale plus the existing frontend `subscribeVisible` covers the symptom.
- Reducing live-propagation latency for *new* peers — that is ZEB-774/568's domain, orthogonal to durability.
- Re-verifying certs or handling revocation on load (§6).

## 3. Design overview

Three coordinated changes, with a new backend store as the foundation:

```
                          ┌─────────────────────────────────────────┐
   Zenoh card sample ───► │ event_loop: verify_card + attribution    │
                          └───────────────┬─────────────────────────┘
                                          │ insert_verified(sub, card)
                          ┌───────────────▼─────────────────────────┐
                          │ ProfileCardCache (live, sub-keyed)       │
                          │   write-through ▼                        │
                          │ PersistentCardStore (owner_id-keyed) ◄───┼── loaded at start_node
                          └───────────────┬─────────────────────────┘
                                          │ reads: live slot ?? store[owner]
             ┌────────────────────────────┼───────────────────────────┐
             ▼                            ▼                            ▼
   get_cached_member_card        display_names_by_owner        (DM/nav resolution
   (author labels, roster)       (roster enrich, net-health)    consume these via IPC)
```

- **Component 1 — `PersistentCardStore`:** a backend, standalone, `owner_id`-keyed durable store, write-through from `insert_verified`, loaded at boot, read as a fallback under the live cache. This is the new capability.
- **Component 2 — DM author resolution:** render the DM author through the existing reactive ladder instead of baking hex.
- **Component 3 — nav clobber fix:** stop overwriting a known-good `NavNode.name`.

## 4. Component 1 — `PersistentCardStore` (backend)

### 4.1 Data model

Reuse the existing `CachedCard` snapshot (`profile_card_broadcast.rs:263-272`), which already carries exactly the right fields:

```rust
struct CachedCard {
    owner_id: [u8; 16],
    display_name: String,
    status_text: String,
    avatar_cid: Option<[u8; 32]>,
    profile_page_root: Option<[u8; 32]>,
    shared_at: Hlc,
}
```

The store is `owner_id → CachedCard` (keyed by owner, unlike the live cache's ephemeral `SubscriptionId` key). Add `Serialize`/`Deserialize` to `CachedCard` (and `Hlc` if not already), or define a parallel `PersistedCard` if we want to decouple the on-disk schema from the in-memory struct. **Recommendation:** reuse `CachedCard` with serde derives — it *is* the snapshot — and gate the on-disk format with a schema-version byte for future migration.

### 4.2 Storage

Mirror `owner_state_persist.rs`: a single atomic CBOR file (write temp + rename), schema-versioned, under the **per-identity data dir**. Per-identity scoping is load-bearing — ZEB-586 was a real cross-identity leak from an owner-agnostic key; the profile data dir is already per-identity/profile, so a file under it is naturally scoped. `friend_nicknames.rs` (atomic JSON, `load_or_default`/`save`) is the alternative template; CBOR is preferred here for consistency with the card wire format and clean handling of the binary `owner_id`/CID fields.

On-disk shape (conceptual): `{ schema: u8, cards: Vec<CachedCard> }` (a `Vec` for compact CBOR; loaded into a `HashMap<[u8;16], CachedCard>`).

### 4.3 Write-through

The store is a **write-through layer on the live cache**. Give `ProfileCardCache` an optional `Arc<PersistentCardStore>` handle; inside `insert_verified` (`:311`), after the newer-HLC-wins slot update, upsert the same `CachedCard` into the store under `card.owner_id` with the identical newer-HLC-wins rule. Persist is **verified-only by construction** (the caller already ran `verify_card` + attribution), so a stored name is one we cryptographically verified was cert-bound to that `owner_id`.

Disk I/O: cards are infrequent (publish cadence is 600s + a boot burst, ZEB-568), so a synchronous write per `insert_verified` is acceptable; if measurement shows churn, debounce with a dirty-flag + periodic flush. Writes must not block the event loop — use the same off-thread/atomic-write path `owner_state_persist` uses.

### 4.4 Boot load

At `start_node` (`lib.rs:~4598-4620`), after constructing the fresh `ProfileCardCache`, construct the `PersistentCardStore` by loading the file (`load_or_default`) and wire it as the cache's write-through handle. The store is now populated with every previously-known peer *before any subscription exists*.

### 4.5 Fallback reads (owner-keyed)

The live cache is `SubscriptionId`-keyed; the store is `owner_id`-keyed. Reads consult **live slot first, store second**:

- `get_cached(sub)` (`:335`): today returns `None` when `slot.latest` is `None` (a registered-but-empty slot — exactly the offline-peer case, since `subscribeVisible` subscribes to all joined members regardless of presence). Change: when `slot.latest` is `None`, fall back to `store.get(slot.expected_owner)`. Result: an offline peer with a live subscription now resolves from disk.
- `display_names_by_owner()` (`:352`): union the store into the result — for any `owner_id` not present (or empty) in the live slots, use the stored name. This instantly fixes `list_community_members` roster enrichment (`lib.rs:~29296`) and the network-health snapshot for offline/post-restart peers.
- Add a pure owner-keyed getter `store.get(owner_id) -> Option<CachedCard>` for surfaces that resolve by owner without a subscription (roster enrich, and any owner-keyed IPC the DM path needs — see §8 open question).

**Merge invariant:** live-vs-store conflicts resolve by the same `Hlc::is_strictly_newer_than` rule already used everywhere. Because live data is written through to the store, "live wins" and "newest-HLC wins" coincide; there is no separate precedence rule to get wrong.

### 4.6 Bounds

Keep last-known with a generous soft cap (LRU ~10k owners; each entry ≤~200B, so ~2 MB worst case). Evict least-recently-updated on overflow. **No TTL** — a TTL would re-introduce the exact "name vanishes after a while" symptom we are removing.

## 5. Component 2 — DM author resolution (frontend)

`message-service.ts` bakes `displayName: from.slice(0,8)` into the `Message` once at arrival and never revisits it. Fix: render the DM author through the **same reactive ladder** `ChannelMessageFeed` uses — `resolveMentionLabel(id, resolveNickname, resolveCard)` (`mention-render.ts:43`: nickname → card → hex) — rather than a baked string.

- Thread `resolveCard` (from the app-lifetime `MemberCardService`) and `resolveNickname` (friend nicknames) into `TextFeed` → `TextMessage` (today `App.svelte:~4101-4131` passes neither).
- Stop hardcoding the name in `message-service.ts`; carry the raw `owner_id`/address on the message and resolve at render time, so the label updates reactively as cards arrive and benefits from the durable store for free.
- The DM peer must be subscribed for the card path to fill: ensure the DM open path calls `subscribe_member_card(peerOwnerId)` (mirrors `subscribeVisible`). With the durable store, that subscription resolves from disk immediately even if the peer is offline.

## 6. Component 3 — nav clobber fix (frontend + possible backend)

`nav-service.ts addOrUpdateNavSpace` (`:210-376`) overwrites `NavNode.name` unconditionally from the `nav-updated` payload on every `'modified'` (`:354-367`) and duplicate `'added'` (`:338-353`). Fix:

- Write the **laddered** peer name to `node.name` (the field actually rendered at `NavNodeRow.svelte:297` and the DM header `TextFeed.svelte:186/202`), not to `node.peer.displayName` (Avatar-only). The ladder result is already computed at `:310-317`; route it to the right field.
- **Do not clobber** a known-good `node.name` with a non-resolved payload name: if the incoming payload name is a raw address/placeholder and we already hold a resolved name, keep the resolved one (defense-in-depth against the re-sync race). The `profile-update` handler (`:145-176`) stays as the live-refresh path.
- **Backend check (planning):** determine what `name` the backend puts in a `nav-updated 'modified'` payload for `dm`/`group-dm` spaces. If it is a raw address, resolve it via `PersistentCardStore` at the source so the payload is correct; the frontend guard is then belt-and-suspenders.

## 7. Fast-follow (explicitly not v1)

Durably caching avatar **image bytes** so a known peer's picture renders while offline: a small CAS blob cache keyed by CID, populated when we fetch an avatar, read as a fallback when the peer's content store is unreachable. The card's `avatar_cid` is already persisted by v1, so this is purely additive and can land as a sibling ticket. Verify avatars are wired live (ZEB-343) before scheduling.

## 8. Security, privacy, correctness

- **Verified-only persistence.** Only cards that passed `verify_card` + attribution reach `insert_verified`, hence the store. A stored name was cryptographically verified as cert-bound to its `owner_id` when received.
- **No re-verification on load.** We display last-known names without re-running cert verification at boot (the cert/HLC is what we verified; re-verifying stale certs adds cost without a threat model here). Local-disk tampering is out of scope (an attacker with local disk write can do worse). **Revocation** of a device/identity is a separate concern and not handled by this cache.
- **Per-identity isolation.** The file lives under the per-identity data dir; switching identities uses a different store. Explicitly regression-tested (the ZEB-586 lesson).
- **Newest-HLC-wins preserved** end to end; replay-safe (the existing `insert_verified` guard).

## 9. Testing strategy

Backend (`cargo nextest`, `--features test-fixtures`):
- `PersistentCardStore` unit: upsert newer-HLC-wins, older-ignored, load_or_default on missing/corrupt file, atomic-write survives a simulated crash mid-write, schema-version round-trip, LRU cap eviction.
- Cache fallback: `get_cached` returns stored card when `slot.latest` is `None`; `display_names_by_owner` unions store for owners with no live slot; live sample overwrites store (write-through).
- Per-identity isolation: two identities → two stores, no bleed (pins the ZEB-586 fix).
- Boot: `start_node` loads a pre-seeded file; an offline peer's name is served with no subscription sample.
- Wire/format: schema-version fixture so the on-disk format is pinned.

Frontend (`vitest`):
- DM author renders card name / nickname / hex via the ladder; updates reactively when a card arrives; falls to hex only when truly unknown (mirror existing `ChannelMessageFeed` tests).
- `nav-service`: a `nav-updated 'modified'` with a raw-address `name` does **not** clobber an already-resolved `node.name`; `profile-update` still refreshes it; the laddered name lands on `node.name`.

End-to-end (existing two-node harness, not in CI): restart a node and confirm a previously-seen, now-offline peer renders by name in roster, authors, and DM — the direct repro of the report.

## 10. Open questions to resolve during planning (not blockers)

1. Exact signature of `get_cached_member_card` / `subscribe_member_card` and whether the frontend can request a card by `owner_id` without an active subscription (may need a small owner-keyed IPC for the DM/nav paths, or reuse subscribe + fallback).
2. What `name` the backend emits in `nav-updated 'modified'` for dm/group-dm spaces (decides frontend-only vs. fix-at-source for #3).
3. Whether `MemberCardService`'s two instances (`main` + `friendCardService`) should share one store-backed source or each resolve independently (today they hold separate Maps).
4. Debounce policy for the write-through (per-insert vs. dirty-flag flush) — decide from a quick churn measurement.

## 11. References

- Backend: `src-tauri/src/profile_card_broadcast.rs` (cache `:286`, `CachedCard` `:263`, `insert_verified` `:311`, `get_cached` `:335`, `display_names_by_owner` `:352`, "NOT persisted" `:409`), `event_loop.rs:2931-3118` (live subscriber), `lib.rs` (NodeState ~1096-1119, start_node ~4598-4620, shutdown wipes ~2690-2714/~3805-3834, IPC ~43046-43273, roster enrich ~29222-29329), `owner_state_persist.rs` / `friend_nicknames.rs` (persistence templates).
- Frontend: `src/lib/member-card-service.ts`, `mention-render.ts`, `message-service.ts:208-213/481-483/540-544`, `nav-service.ts:145-176/210-376`, `components/ChannelMessageFeed.svelte`, `MemberRow.svelte`, `TextFeed.svelte`, `TextMessage.svelte`.
