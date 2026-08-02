# ZEB-849 T-CARD — forward-bound `shared_at` on profile cards Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reject implausibly future-dated `shared_at` stamps on profile cards so a skewed/compromised own-device can no longer pin its identity fields on every peer forever.

**Architecture:** Three bounded changes reusing the `clock_trust` policy module. **L1** an ingest bound in `verify_card` (the single write chokepoint into both caches). **L2** a non-destructive read-view gate on the disk store for poison already resident (which honest cards can never out-HLC). **L3** the same bound on the in-memory membership-broadcast sibling (`on_sample`, finding C10). Design: `docs/superpowers/specs/2026-08-01-zeb-849-t-card-shared-at-forward-bound-design.md`.

**Tech Stack:** Rust (harmony-app crate), `clock_trust` module, `cargo nextest`.

## Global Constraints

- Use `crate::clock_trust::MAX_FORWARD_SKEW_MS` (5 min) — never a new constant.
- **Reject, never clamp** for these replicated newer-wins registers (ZEB-847 lesson).
- **A bad LOCAL clock must never drop honest state.** The unreadable-clock sentinel in this subsystem is `now_secs == 0` (prod passes `iroh_friend_acceptor::wall_now_secs()` = `wall_now_ms()/1000` with `.unwrap_or(0)`); the store/`on_sample` fail-open sentinel is `None`. In every layer, that sentinel ⇒ **apply-all** (do not reject).
- **Never destroy at-rest cards via a load-time write-back** (view-not-store rule). L2 suppresses from the returned view and leaves the map + disk untouched.
- Units: `shared_at.wall_ms` is ms; `now_secs` is seconds — compare `wall_ms` against `now_secs.saturating_mul(1000) + skew`.
- The inclusive-boundary convention of `clock_trust::reject_future` is load-bearing: `stamp == now + skew` is accepted; `+1` is rejected.
- CI gates (run from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.

---

## File Structure

- `src/profile_card_broadcast.rs` — **L1**: add `CardVerifyError::SharedAtTooFarInFuture`; add the bound in `verify_card`; add L1 tests.
- `src/persistent_card_store.rs` — **L2**: add `get_with_now` / `display_names_by_owner_with_now` seams; make `get` / `display_names_by_owner` delegate through `clock_trust::receiver_now_ms()`; add L2 tests.
- `src/profile_broadcast.rs` — **L3**: add `now_secs` param to `on_sample`; add `CacheOnSampleError::FutureSkew`; add the bound; add L3 tests; update existing `on_sample` test call-sites.
- `src/event_loop.rs` — **L3**: pass `crate::iroh_friend_acceptor::wall_now_secs()` at the one `on_sample` call-site (`event_loop.rs:2861`).

---

## Task 1: L1 — ingest bound in `verify_card`

**Files:**
- Modify: `src/profile_card_broadcast.rs` (`CardVerifyError` enum ~186-199; `verify_card` ~210-246; tests module)

**Interfaces:**
- Consumes: `crate::clock_trust::{reject_future, MAX_FORWARD_SKEW_MS}`; `card.shared_at.wall_ms: u64`; the `now_secs: u64` param `verify_card` already receives.
- Produces: `CardVerifyError::SharedAtTooFarInFuture`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` in `src/profile_card_broadcast.rs`:

```rust
#[test]
fn verify_card_rejects_future_dated_shared_at() {
    // C4: a card whose shared_at.wall_ms is beyond now + MAX_FORWARD_SKEW_MS
    // must never verify — otherwise it out-HLCs every honest card forever.
    let owner = crate::community_membership::mint_test_owner(0x71);
    const NOW_S: u64 = 1_700_000_000;
    let now_ms = NOW_S * 1000;
    let one_year_ms = 365 * 86_400 * 1000;
    let poison = sign_card(
        &owner.device_key,
        owner.owner.0,
        "Mallory".into(),
        "".into(),
        None,
        None,
        owner.cert.clone(),
        Hlc { wall_ms: now_ms + one_year_ms, logical: 0, device_id: "d".into() },
    )
    .expect("sign");
    assert!(matches!(
        verify_card(&poison, NOW_S),
        Err(CardVerifyError::SharedAtTooFarInFuture)
    ));
}

#[test]
fn verify_card_accepts_in_range_shared_at_at_the_inclusive_ceiling() {
    let owner = crate::community_membership::mint_test_owner(0x72);
    const NOW_S: u64 = 1_700_000_000;
    let now_ms = NOW_S * 1000;
    // Present, and exactly at the inclusive ceiling, both verify.
    for wall_ms in [now_ms, now_ms + crate::clock_trust::MAX_FORWARD_SKEW_MS] {
        let card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Ann".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc { wall_ms, logical: 0, device_id: "d".into() },
        )
        .expect("sign");
        assert_eq!(verify_card(&card, NOW_S).expect("in-range verifies"), owner.owner.0);
    }
    // One millisecond past the ceiling is rejected.
    let over = sign_card(
        &owner.device_key,
        owner.owner.0,
        "Ann".into(),
        "".into(),
        None,
        None,
        owner.cert.clone(),
        Hlc {
            wall_ms: now_ms + crate::clock_trust::MAX_FORWARD_SKEW_MS + 1,
            logical: 0,
            device_id: "d".into(),
        },
    )
    .expect("sign");
    assert!(matches!(
        verify_card(&over, NOW_S),
        Err(CardVerifyError::SharedAtTooFarInFuture)
    ));
}

#[test]
fn verify_card_zero_now_is_apply_all_for_shared_at() {
    // now_secs == 0 is the unreadable-local-clock sentinel (wall_now_secs()
    // .unwrap_or(0)); a bad LOCAL clock must never reject an honest card, so the
    // bound disables itself. (This is also why every legacy verify_card(&card, 0)
    // test keeps passing.)
    let owner = crate::community_membership::mint_test_owner(0x73);
    let far_future = sign_card(
        &owner.device_key,
        owner.owner.0,
        "Ann".into(),
        "".into(),
        None,
        None,
        owner.cert.clone(),
        Hlc { wall_ms: u64::MAX / 2, logical: 0, device_id: "d".into() },
    )
    .expect("sign");
    assert_eq!(verify_card(&far_future, 0).expect("apply-all"), owner.owner.0);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(verify_card_rejects_future_dated_shared_at) + test(verify_card_accepts_in_range_shared_at_at_the_inclusive_ceiling) + test(verify_card_zero_now_is_apply_all_for_shared_at)'`
Expected: FAIL — `SharedAtTooFarInFuture` variant does not exist yet (compile error), or the poison verifies.

- [ ] **Step 3: Add the error variant**

In the `CardVerifyError` enum (~line 186), add:

```rust
    #[error("shared_at.wall_ms is implausibly far in the receiver's future")]
    SharedAtTooFarInFuture,
```

- [ ] **Step 4: Add the bound in `verify_card`**

In `verify_card`, immediately after the two length checks (after the `StatusTextTooLong` check, before the `verify_enrollment_any_issuer` call ~line 220), insert:

```rust
    // ZEB-849 (C4): reject an implausibly future-dated shared_at before it can
    // out-HLC every honest card in both the live cache and the disk store.
    // now_secs == 0 ⇒ unreadable local clock (wall_now_secs().unwrap_or(0)) ⇒
    // apply-all: a bad LOCAL clock must never drop honest state.
    if now_secs != 0
        && crate::clock_trust::reject_future(
            card.shared_at.wall_ms,
            now_secs.saturating_mul(1000),
            crate::clock_trust::MAX_FORWARD_SKEW_MS,
        )
    {
        return Err(CardVerifyError::SharedAtTooFarInFuture);
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(verify_card)'`
Expected: PASS — the three new tests plus every existing `verify_card_*` test (they pass `now_secs=0` or small fixtures, so the bound never trips them).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/profile_card_broadcast.rs
git commit -m "fix(zeb-849): L1 forward-bound shared_at in verify_card (C4)"
```

---

## Task 2: L2 — non-destructive read-view gate on the disk store

**Files:**
- Modify: `src/persistent_card_store.rs` (`get` ~240; `display_names_by_owner` ~247; tests module)

**Interfaces:**
- Consumes: `crate::clock_trust::{receiver_now_ms, wall_exceeds_forward_skew}`; `PersistedCard::shared_at.wall_ms`.
- Produces: `PersistentCardStore::get_with_now(&self, owner_id: &[u8; 16], now_ms: Option<u64>) -> Option<PersistedCard>` and `display_names_by_owner_with_now(&self, now_ms: Option<u64>) -> HashMap<[u8; 16], String>` (private to the module; the public `get` / `display_names_by_owner` delegate through them).

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` in `src/persistent_card_store.rs`:

```rust
#[test]
fn get_with_now_suppresses_future_dated_entry_non_destructively() {
    // A poison entry (wall_ms far in the future) is omitted from the read view
    // when the local clock is readable, but is NOT removed from the map/disk —
    // suppress-from-view, never delete-on-load (slow-clock-purge safe).
    let dir = tempfile::tempdir().unwrap();
    let now_ms = 1_700_000_000_000u64;
    let future = now_ms + 500 * 86_400 * 1000; // ~1.4 yr ahead
    let store = PersistentCardStore::from_cards(
        dir.path().join("profile_cards.x.cbor"),
        DEFAULT_MAX_ENTRIES,
        vec![card(0xEE, "Mallory", future), card(0x01, "Alice", now_ms)],
    );
    // Readable clock: poison suppressed, in-range shown.
    assert!(store.get_with_now(&[0xEE; 16], Some(now_ms)).is_none());
    assert_eq!(
        store.get_with_now(&[0x01; 16], Some(now_ms)).unwrap().display_name,
        "Alice"
    );
    // Non-destructive: the entry is still resident and still returned when the
    // clock is unreadable (apply-all).
    assert_eq!(store.len(), 2);
    assert_eq!(
        store.get_with_now(&[0xEE; 16], None).unwrap().display_name,
        "Mallory"
    );
}

#[test]
fn display_names_by_owner_with_now_omits_future_dated_entries() {
    let dir = tempfile::tempdir().unwrap();
    let now_ms = 1_700_000_000_000u64;
    let future = now_ms + 500 * 86_400 * 1000;
    let store = PersistentCardStore::from_cards(
        dir.path().join("profile_cards.x.cbor"),
        DEFAULT_MAX_ENTRIES,
        vec![card(0xEE, "Mallory", future), card(0x01, "Alice", now_ms)],
    );
    let names = store.display_names_by_owner_with_now(Some(now_ms));
    assert!(!names.contains_key(&[0xEE; 16]), "poison omitted from the view");
    assert_eq!(names.get(&[0x01; 16]).map(String::as_str), Some("Alice"));
    // Apply-all when the clock is unreadable.
    let all = store.display_names_by_owner_with_now(None);
    assert!(all.contains_key(&[0xEE; 16]));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(get_with_now_suppresses_future_dated_entry_non_destructively) + test(display_names_by_owner_with_now_omits_future_dated_entries)'`
Expected: FAIL — `get_with_now` / `display_names_by_owner_with_now` do not exist (compile error).

- [ ] **Step 3: Add the seams and delegate the public methods**

Replace the existing `get` and `display_names_by_owner` (~240-254) with:

```rust
    /// Last-known card for an owner, if any and not implausibly future-dated.
    pub fn get(&self, owner_id: &[u8; 16]) -> Option<PersistedCard> {
        self.get_with_now(owner_id, crate::clock_trust::receiver_now_ms())
    }

    /// ZEB-849 (C4) L2 seam: `get`, but with the receiver clock injected. A
    /// resident entry whose `shared_at.wall_ms` is beyond `now_ms + skew` is
    /// suppressed from the view (never deleted — the map/disk are untouched;
    /// `now_ms == None` ⇒ apply-all). Poison that predates the L1 ingest bound
    /// can never be out-HLC'd by an honest card, so gating the read is the only
    /// non-destructive remedy.
    fn get_with_now(&self, owner_id: &[u8; 16], now_ms: Option<u64>) -> Option<PersistedCard> {
        let inner = self.inner.lock().expect("card store poisoned");
        let card = inner.map.get(owner_id).map(|e| e.card.clone())?;
        if crate::clock_trust::wall_exceeds_forward_skew(card.shared_at.wall_ms, now_ms) {
            return None;
        }
        Some(card)
    }

    /// `owner_id` → last-known display name, for bulk roster / network-health
    /// enrichment fallback (mirrors the live cache's `display_names_by_owner`).
    pub fn display_names_by_owner(&self) -> HashMap<[u8; 16], String> {
        self.display_names_by_owner_with_now(crate::clock_trust::receiver_now_ms())
    }

    /// ZEB-849 (C4) L2 seam: `display_names_by_owner` with the receiver clock
    /// injected; future-dated entries are omitted from the view (`None` ⇒
    /// apply-all).
    fn display_names_by_owner_with_now(&self, now_ms: Option<u64>) -> HashMap<[u8; 16], String> {
        let inner = self.inner.lock().expect("card store poisoned");
        inner
            .map
            .iter()
            .filter(|(_, e)| {
                !crate::clock_trust::wall_exceeds_forward_skew(e.card.shared_at.wall_ms, now_ms)
            })
            .map(|(owner, e)| (*owner, e.card.display_name.clone()))
            .collect()
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(persistent_card_store) + test(get_with_now) + test(display_names_by_owner_with_now)'`
Expected: PASS — the two new tests plus every existing store test (their fixtures use small `at` values that never exceed a real `now`; the ones that use no injected clock go through `receiver_now_ms()` and are far in the past, so are never suppressed).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/persistent_card_store.rs
git commit -m "fix(zeb-849): L2 non-destructive read-view gate for at-rest poison (C4)"
```

---

## Task 3: L3 — C10 sibling bound in `on_sample`

**Files:**
- Modify: `src/profile_broadcast.rs` (`CacheOnSampleError` enum; `on_sample` ~581; tests module — new tests + update existing `on_sample(...)` call-sites)
- Modify: `src/event_loop.rs` (~2861: the one prod `on_sample` call-site)

**Interfaces:**
- Consumes: `crate::clock_trust::{reject_future, MAX_FORWARD_SKEW_MS}`; `broadcast.shared_at.wall_ms`; a new `now_secs: u64` param.
- Produces: `CacheOnSampleError::FutureSkew`; `on_sample(&self, sub, broadcast, now_secs)`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` in `src/profile_broadcast.rs`:

```rust
#[tokio::test]
async fn on_sample_rejects_future_dated_shared_at() {
    // C10: a future-dated shared_at must not be cached, or it out-HLCs every
    // honest in-memory sample for the session.
    let (signer, identity_pub) = build_identity([201u8; 32]);
    let peer_addr = OwnerAddr(
        harmony_identity::Identity::from_public_bytes(&identity_pub)
            .unwrap()
            .address_hash,
    );
    let cache = ProfileBroadcastCache::default();
    cache.register(7, peer_addr).await;

    const NOW_S: u64 = 1_700_000_000;
    let now_ms = NOW_S * 1000;
    let poison = sign_broadcast(
        &signer,
        identity_pub,
        vec![fixture_space_id(1)],
        fixture_hlc(now_ms + 365 * 86_400 * 1000),
    )
    .unwrap();
    assert!(matches!(
        cache.on_sample(7, poison.clone(), NOW_S).await,
        Err(CacheOnSampleError::FutureSkew)
    ));
    assert!(cache.get_cached(7).await.is_none(), "poison never cached");

    // now_secs == 0 ⇒ apply-all (unreadable local clock): the same broadcast is
    // accepted.
    assert_eq!(
        cache.on_sample(7, poison, 0).await.unwrap(),
        CacheOnSampleOutcome::InsertedFirst
    );
}

#[tokio::test]
async fn on_sample_in_range_newer_still_wins() {
    // The bound must not over-reject: an in-range newer sample still replaces an
    // in-range older one.
    let (signer, identity_pub) = build_identity([202u8; 32]);
    let peer_addr = OwnerAddr(
        harmony_identity::Identity::from_public_bytes(&identity_pub)
            .unwrap()
            .address_hash,
    );
    let cache = ProfileBroadcastCache::default();
    cache.register(8, peer_addr).await;

    const NOW_S: u64 = 1_700_000_000;
    let now_ms = NOW_S * 1000;
    let older = sign_broadcast(
        &signer,
        identity_pub,
        vec![fixture_space_id(1)],
        fixture_hlc(now_ms - 10_000),
    )
    .unwrap();
    let newer = sign_broadcast(
        &signer,
        identity_pub,
        vec![fixture_space_id(2)],
        fixture_hlc(now_ms),
    )
    .unwrap();
    assert_eq!(
        cache.on_sample(8, older, NOW_S).await.unwrap(),
        CacheOnSampleOutcome::InsertedFirst
    );
    assert_eq!(
        cache.on_sample(8, newer, NOW_S).await.unwrap(),
        CacheOnSampleOutcome::Replaced
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(on_sample_rejects_future_dated_shared_at) + test(on_sample_in_range_newer_still_wins)'`
Expected: FAIL — `on_sample` takes 2 args / `FutureSkew` does not exist (compile error).

- [ ] **Step 3: Add the error variant**

In the `CacheOnSampleError` enum (`#[derive(Debug, thiserror::Error)]`, ~line 539), add after the `Replay` variant:

```rust
    #[error("shared_at.wall_ms is implausibly far in the receiver's future")]
    FutureSkew,
```

- [ ] **Step 4: Add the `now_secs` param + the bound**

Change the `on_sample` signature to:

```rust
    pub async fn on_sample(
        &self,
        sub: SubscriptionId,
        broadcast: ProfileMembershipBroadcast,
        now_secs: u64,
    ) -> Result<CacheOnSampleOutcome, CacheOnSampleError> {
```

Immediately after the `verify_broadcast(&broadcast)?` call (step (1), ~line 587), before taking the map lock, insert:

```rust
        // ZEB-849 (C10): reject a future-dated shared_at before newer-wins can
        // pin it. now_secs == 0 ⇒ unreadable local clock ⇒ apply-all.
        if now_secs != 0
            && crate::clock_trust::reject_future(
                broadcast.shared_at.wall_ms,
                now_secs.saturating_mul(1000),
                crate::clock_trust::MAX_FORWARD_SKEW_MS,
            )
        {
            return Err(CacheOnSampleError::FutureSkew);
        }
```

- [ ] **Step 5: Update the existing `on_sample` test call-sites**

The pre-existing `on_sample` calls in this test module (currently `cache.on_sample(sub, b).await`) must pass a `now_secs`. Use `0` (apply-all) so their in-range/replay assertions stay behavior-identical:

- `cache.on_sample(1, newer.clone(), 0)`, `cache.on_sample(1, older, 0)`
- `cache.on_sample(2, older, 0)`, `cache.on_sample(2, newer, 0)`
- and any other existing `on_sample(` call in the module.

- [ ] **Step 6: Update the prod call-site**

In `src/event_loop.rs` (~line 2861), pass the receiver clock:

```rust
                                            match cache_for_task
                                                .on_sample(
                                                    subscription_id,
                                                    broadcast,
                                                    crate::iroh_friend_acceptor::wall_now_secs(),
                                                )
                                                .await
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(on_sample) + test(state_replay) + test(profile_broadcast)'`
Expected: PASS — the two new tests plus all existing `on_sample`/replay tests.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/profile_broadcast.rs src-tauri/src/event_loop.rs
git commit -m "fix(zeb-849): L3 forward-bound shared_at in on_sample (C10)"
```

---

## Final gate (after all tasks)

- [ ] **Full CI-parity sweep** (never the `-p harmony-app` scoped form for the final gate):

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: fmt clean, clippy clean (`-D warnings`), all tests pass. Note any new integration-test files with `git add` before gating (untracked files are invisible to the run).
