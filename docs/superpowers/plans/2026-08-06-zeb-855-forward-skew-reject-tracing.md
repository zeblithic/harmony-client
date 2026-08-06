# ZEB-855 — Uniform forward-skew reject tracing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Emit one uniform, greppable `tracing::debug` event at every forward-skew ingest/merge reject boundary, via shared `clock_trust` logged wrappers, without changing any reject decision.

**Architecture:** Add a single private emit + three logged sibling wrappers to `clock_trust.rs`; each wrapper calls the plain predicate for the decision (single source of truth) and emits only when it rejects. Then swap ~22 call sites to their logged sibling, passing a `<subsystem>.<register>.<field>` discriminator. No new behaviour; correctness pinned by behaviour-parity unit tests.

**Tech Stack:** Rust, `tracing`, `cargo nextest`. Spec: `docs/superpowers/specs/2026-08-06-zeb-855-forward-skew-reject-tracing-design.md`.

## Global Constraints

- **Observability only** — no reject decision, skew constant, or policy changes; existing subsystem tests must stay green unchanged (they prove no decision drift).
- **Level `debug`, never `warn`** — a skewed peer is expected.
- **No raw peer identity in any emitted event** — `field` discriminator only.
- **All emitted magnitudes in milliseconds** (`skew_ms`, `receiver_now_ms`), uniform across sites.
- **No new test dependency** (no `tracing-test` / capture subscriber) — parity is the contract.
- Gates from `src-tauri/`: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Frontend `npx tsc --noEmit` from repo root (no FE change — run once at the end as a sanity check).
- Constants (verbatim): `MAX_FORWARD_SKEW_MS = 300_000`, `DISPLAY_SKEW_TOLERANCE_MS = 1_800_000`, `DISPLAY_SKEW_TOLERANCE_SECS = 1_800`.

---

## File Structure

- `src/clock_trust.rs` — **Task 1** adds `emit_forward_skew_reject` (private), `secs_reject_fields` (private, testable), and the three public logged wrappers, plus their unit tests. Sole owner of the event format.
- Call-site files — **Tasks 2–4** swap the predicate for its logged sibling. No test files change; the parity tests (Task 1) plus each subsystem's existing tests are the verification.

---

### Task 1: `clock_trust` logged wrappers, emit, and parity tests

**Files:**
- Modify: `src/clock_trust.rs` (add wrappers after `wall_exceeds_forward_skew_secs`, ~line 182; add tests inside the existing `#[cfg(test)] mod tests`).

**Interfaces:**
- Consumes: existing `reject_future`, `wall_exceeds_forward_skew`, `wall_exceeds_forward_skew_secs`, `MAX_FORWARD_SKEW_MS`.
- Produces (later tasks call these):
  - `pub fn wall_exceeds_forward_skew_logged(wall_ms: u64, receiver_now_ms: Option<u64>, field: &str) -> bool`
  - `pub fn reject_future_logged(stamp_ms: u64, now_ms: u64, tolerance_ms: u64, field: &str) -> bool`
  - `pub fn wall_exceeds_forward_skew_secs_logged(wall_ms: u64, now_secs: u64, tolerance_ms: u64, field: &str) -> bool`

- [ ] **Step 1: Write the failing parity tests**

Add inside `mod tests`:

```rust
#[test]
fn wall_exceeds_forward_skew_logged_matches_plain() {
    let now = 1_700_000_000_000u64;
    let cases: [(u64, Option<u64>); 6] = [
        (now, Some(now)),                            // present -> false
        (now - 1, Some(now)),                        // past -> false
        (now + MAX_FORWARD_SKEW_MS, Some(now)),      // boundary inclusive -> false
        (now + MAX_FORWARD_SKEW_MS + 1, Some(now)),  // just over -> true
        (now + 10 * MAX_FORWARD_SKEW_MS, Some(now)), // far future -> true
        (now + MAX_FORWARD_SKEW_MS + 1, None),       // no clock -> apply-all -> false
    ];
    for (wall, rn) in cases {
        assert_eq!(
            wall_exceeds_forward_skew_logged(wall, rn, "test.parity"),
            wall_exceeds_forward_skew(wall, rn),
            "logged must match plain: wall={wall} rn={rn:?}",
        );
    }
}

#[test]
fn reject_future_logged_matches_plain() {
    let now = 1_700_000_000_000u64;
    for tol in [MAX_FORWARD_SKEW_MS, DISPLAY_SKEW_TOLERANCE_MS] {
        for stamp in [now, now - 1, now + tol, now + tol + 1, now + 5 * tol] {
            assert_eq!(
                reject_future_logged(stamp, now, tol, "test.parity"),
                reject_future(stamp, now, tol),
                "logged must match plain: stamp={stamp} now={now} tol={tol}",
            );
        }
    }
}

#[test]
fn wall_exceeds_forward_skew_secs_logged_matches_plain() {
    let now_secs = 1_700_000_000u64;
    let now_ms = now_secs * 1000;
    let cases: [(u64, u64, u64); 5] = [
        (now_ms, now_secs, MAX_FORWARD_SKEW_MS),                       // present
        (now_ms + MAX_FORWARD_SKEW_MS + 2000, now_secs, MAX_FORWARD_SKEW_MS), // over
        (now_ms + DISPLAY_SKEW_TOLERANCE_MS + 2000, now_secs, DISPLAY_SKEW_TOLERANCE_MS), // over (display)
        (now_ms, 0, MAX_FORWARD_SKEW_MS),                             // 0-sentinel -> apply-all
        (u64::MAX, 0, MAX_FORWARD_SKEW_MS),                           // 0-sentinel, huge stamp
    ];
    for (wall_ms, ns, tol) in cases {
        assert_eq!(
            wall_exceeds_forward_skew_secs_logged(wall_ms, ns, tol, "test.parity"),
            wall_exceeds_forward_skew_secs(wall_ms, ns, tol),
            "logged must match plain: wall_ms={wall_ms} now_secs={ns} tol={tol}",
        );
    }
}

#[test]
fn secs_reject_fields_normalizes_to_ms() {
    // now_secs=1000 -> compensated now_ms = 1_000_999
    assert_eq!(secs_reject_fields(2_000_000, 1000), (2_000_000 - 1_000_999, 1_000_999));
    // stamp below compensated now -> saturating skew 0 (never underflows)
    assert_eq!(secs_reject_fields(500, 1000), (0, 1_000_999));
}
```

- [ ] **Step 2: Run tests to verify they fail (do not compile — functions undefined)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(clock_trust)' 2>&1 | tail -20`
Expected: compile error `cannot find function ... _logged` / `secs_reject_fields`.

- [ ] **Step 3: Implement the emit, the pure fields helper, and the three wrappers**

Insert after `wall_exceeds_forward_skew_secs` (after ~line 182, before `#[cfg(test)]`):

```rust
/// ZEB-855: the (skew_ms, compensated receiver_now_ms) pair a seconds-domain
/// reject reports, given an ms stamp and a seconds receiver-now. Pure (no
/// `tracing`), so the ms-normalization is unit-testable without a subscriber.
/// Mirrors the `now_secs*1000 + 999` floored-seconds compensation in
/// [`wall_exceeds_forward_skew_secs`].
#[inline]
fn secs_reject_fields(wall_ms: u64, now_secs: u64) -> (u64, u64) {
    let now_ms = now_secs.saturating_mul(1000).saturating_add(999);
    (wall_ms.saturating_sub(now_ms), now_ms)
}

/// ZEB-855: the single home for the forward-skew reject event format. `debug`,
/// never `warn` (a skewed peer is expected); no raw peer identity (`field` is a
/// static `<subsystem>.<register>.<stamp_field>` discriminator). `tier` is
/// derived from the budget so events are greppable by tier without memorising
/// the numeric constants.
fn emit_forward_skew_reject(field: &str, skew_ms: u64, receiver_now_ms: u64, tolerance_ms: u64) {
    let tier = if tolerance_ms <= MAX_FORWARD_SKEW_MS {
        "control"
    } else {
        "display"
    };
    tracing::debug!(
        target: "clock_trust::forward_skew",
        field,
        skew_ms,
        receiver_now_ms,
        tolerance_ms,
        tier,
        "forward-skew reject: peer stamp beyond receiver clock tolerance",
    );
}

/// ZEB-855: logged sibling of [`wall_exceeds_forward_skew`] (control tier, ms
/// stamp, `Option` receiver-now). Identical reject decision; emits one
/// `debug` event on reject. `field` is a static discriminator, never a peer id.
#[inline]
pub fn wall_exceeds_forward_skew_logged(
    wall_ms: u64,
    receiver_now_ms: Option<u64>,
    field: &str,
) -> bool {
    match receiver_now_ms {
        Some(now) if reject_future(wall_ms, now, MAX_FORWARD_SKEW_MS) => {
            emit_forward_skew_reject(field, wall_ms.saturating_sub(now), now, MAX_FORWARD_SKEW_MS);
            true
        }
        _ => false,
    }
}

/// ZEB-855: logged sibling of [`reject_future`] for **millisecond** callers.
/// Identical reject decision; emits one `debug` event on reject.
#[inline]
pub fn reject_future_logged(stamp_ms: u64, now_ms: u64, tolerance_ms: u64, field: &str) -> bool {
    if reject_future(stamp_ms, now_ms, tolerance_ms) {
        emit_forward_skew_reject(field, stamp_ms.saturating_sub(now_ms), now_ms, tolerance_ms);
        true
    } else {
        false
    }
}

/// ZEB-855: logged sibling of [`wall_exceeds_forward_skew_secs`] (ms stamp,
/// epoch-**seconds** receiver-now, explicit tolerance). Identical reject
/// decision; emits one `debug` event on reject, magnitudes normalized to ms.
#[inline]
pub fn wall_exceeds_forward_skew_secs_logged(
    wall_ms: u64,
    now_secs: u64,
    tolerance_ms: u64,
    field: &str,
) -> bool {
    if wall_exceeds_forward_skew_secs(wall_ms, now_secs, tolerance_ms) {
        let (skew_ms, now_ms) = secs_reject_fields(wall_ms, now_secs);
        emit_forward_skew_reject(field, skew_ms, now_ms, tolerance_ms);
        true
    } else {
        false
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(clock_trust)'`
Expected: all `clock_trust` tests PASS (the 4 new + the pre-existing boundary tests).

- [ ] **Step 5: fmt + clippy the module, then commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
git add src/clock_trust.rs
git commit -m "ZEB-855: clock_trust logged forward-skew wrappers + parity tests"
```

---

### Task 2: Swap the `wall_exceeds_forward_skew` control-tier sites

**Files (all Modify):** `src/owner_state_sync.rs`, `src/notes_crdt.rs`, `src/fleet_net.rs`, `src/voice_moderation.rs`.

**Interfaces:**
- Consumes: `wall_exceeds_forward_skew_logged` (Task 1).

Each swap is `wall_exceeds_forward_skew(X, RN)` → `wall_exceeds_forward_skew_logged(X, RN, "<field>")`, decision-identical.

> **Disambiguation note:** `owner_state_sync.rs` has two guards with the *identical* text `crate::clock_trust::wall_exceeds_forward_skew(entry.learned_at.wall_ms, receiver_now)` — the owner-device loop (~357) and the friend loop (~424). When editing, include the preceding line/comment so each `old_string` is unique (owner-device is preceded by the `learned_at` DEV comment; friend is preceded by the `FAIL-OPEN: blocked party` comment).

- [ ] **Step 1: owner_state_sync.rs — the 8 guards (10 predicate calls)**

Apply, matching each guard's existing text (add the discriminator as the new 3rd arg; for the OR blocks, log each term):

| Existing arg | New discriminator |
|---|---|
| `space.updated_at.wall_ms, receiver_now` | `"owner_state.space.updated_at"` |
| `marker.last_read_at.wall_ms, receiver_now` | `"owner_state.read_marker.last_read_at"` |
| `entry.learned_at.wall_ms, receiver_now` (owner-device loop) | `"owner_state.owner_device.learned_at"` |
| `remote_entry.added_at.wall_ms, receiver_now` | `"owner_state.library.added_at"` |
| `rm.wall_ms, receiver_now` (library `removed_at` OR term) | `"owner_state.library.removed_at"` |
| `entry.learned_at.wall_ms, receiver_now` (friend loop) | `"owner_state.friend.learned_at"` |
| `g.granted_at, receiver_now` | `"owner_state.grant.granted_at"` |
| `g.revoked_at, receiver_now` | `"owner_state.grant.revoked_at"` |
| `dismissed_at, receiver_now` | `"owner_state.dismissed_grant.dismissed_at"` |
| `grant.received_at, receiver_now` | `"owner_state.received_grant.received_at"` |

- [ ] **Step 2: notes_crdt.rs:99, fleet_net.rs:163 & :219, voice_moderation.rs:444**

| Site | Existing arg | New discriminator |
|---|---|---|
| `notes_crdt.rs` | `r.updated_at.wall_ms, receiver_now` | `"notes.note.updated_at"` |
| `fleet_net.rs` (seen_at) | `remote_row.seen_at.wall_ms, receiver_now` | `"fleet_net.device.seen_at"` |
| `fleet_net.rs` (petname) | `remote_pn.set_at.wall_ms, receiver_now` | `"fleet_net.petname.set_at"` |
| `voice_moderation.rs` | `d.issued_hlc.wall_ms, now_wall_ms` | `"voice_moderation.directive.issued_hlc"` |

> Note: `fleet_net.rs:198` (`!wall_exceeds_forward_skew(...)` pin accept-guard) is **excluded** — negated, no reject branch. Do NOT touch it.

- [ ] **Step 3: Build + scoped tests**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(owner_state) or test(notes) or test(fleet) or test(voice_moderation)'`
Expected: PASS unchanged (decision-identical swaps).

- [ ] **Step 4: fmt + clippy + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
git add src/owner_state_sync.rs src/notes_crdt.rs src/fleet_net.rs src/voice_moderation.rs
git commit -m "ZEB-855: route wall_exceeds_forward_skew control sites through logged wrapper"
```

---

### Task 3: Swap the `reject_future` sites

**Files (all Modify):** `src/community_membership.rs`, `src/community_channel_log.rs`, `src/community_invite.rs`, `src/open_join_admit.rs`, `src/community_voting_log_engine.rs`, `src/library_directory.rs`, `src/vine_feed_cache.rs`.

**Interfaces:**
- Consumes: `reject_future_logged` (Task 1).

Each control site: `reject_future(STAMP, NOW, MAX_FORWARD_SKEW_MS)` → `reject_future_logged(STAMP, NOW, MAX_FORWARD_SKEW_MS, "<field>")`.

> **Disambiguation note:** `community_voting_log_engine.rs` has two guards with identical text `reject_future(event.hlc.wall_ms, receiver_now_ms, ...)` — the `process_inbound` path (~2924) and the backfill-pull path (~3082). Include the preceding distinguishing comment (`process_inbound` block vs the "backfilled voting event" block) so each `old_string` is unique, and give each its own discriminator.

- [ ] **Step 1: The five ms control sites**

| Site | Existing stamp/now args | New discriminator |
|---|---|---|
| `community_membership.rs:4072` | `event.at.wall_ms, now,` | `"community_membership.event.at"` |
| `community_channel_log.rs:1413` | `at.wall_ms, now,` | `"channel_log.event.at"` |
| `community_invite.rs:1871` | `signed.join_event.at.wall_ms, now,` | `"community_invite.join_event.at"` |
| `open_join_admit.rs:396` | `req.join_event.at.wall_ms, wall_now_ms,` | `"open_join_admit.join_event.at"` |
| `community_voting_log_engine.rs:2924` (inbound) | `event.hlc.wall_ms, receiver_now_ms,` | `"voting_log.inbound.event.hlc"` |
| `community_voting_log_engine.rs:3082` (backfill) | `event.hlc.wall_ms, receiver_now_ms,` | `"voting_log.backfill.event.hlc"` |

The new 4th arg goes after the tolerance line, e.g.:
```rust
if crate::clock_trust::reject_future_logged(
    event.at.wall_ms,
    now,
    crate::clock_trust::MAX_FORWARD_SKEW_MS,
    "community_membership.event.at",
) {
```

- [ ] **Step 2: library_directory.rs:481 (display, ms)**

`reject_future(announce.listed_at.wall_ms, now, DISPLAY_SKEW_TOLERANCE_MS)` →
```rust
if crate::clock_trust::reject_future_logged(
    announce.listed_at.wall_ms,
    now,
    crate::clock_trust::DISPLAY_SKEW_TOLERANCE_MS,
    "library_directory.announce.listed_at",
) {
```

- [ ] **Step 3: vine_feed_cache.rs:729 (display, seconds → ms rescale)**

Replace the seconds-domain call with the ms-rescaled logged call (exact behaviour-preserving `×1000` rescale):
```rust
if crate::clock_trust::reject_future_logged(
    descriptor.created_at.saturating_mul(1000),
    now_secs.saturating_mul(1000),
    crate::clock_trust::DISPLAY_SKEW_TOLERANCE_MS,
    "vine_feed.descriptor.created_at",
) {
```

> Note: `vine_feed_cache.rs:813` (`!reject_future(...)`) is **excluded** — negated. Do NOT touch it.

- [ ] **Step 4: Build + scoped tests**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_membership) or test(channel_log) or test(community_invite) or test(open_join) or test(voting_log) or test(library_directory) or test(vine)'`
Expected: PASS unchanged.

- [ ] **Step 5: fmt + clippy + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
git add src/community_membership.rs src/community_channel_log.rs src/community_invite.rs src/open_join_admit.rs src/community_voting_log_engine.rs src/library_directory.rs src/vine_feed_cache.rs
git commit -m "ZEB-855: route reject_future ingest sites through logged wrapper"
```

---

### Task 4: Swap the `wall_exceeds_forward_skew_secs` sites

**Files (all Modify):** `src/profile_broadcast.rs`, `src/profile_card_broadcast.rs`.

**Interfaces:**
- Consumes: `wall_exceeds_forward_skew_secs_logged` (Task 1).

- [ ] **Step 1: Apply both swaps**

| Site | Existing args | New discriminator |
|---|---|---|
| `profile_broadcast.rs:596` | `broadcast.shared_at.wall_ms, now_secs, DISPLAY_SKEW_TOLERANCE_MS` | `"profile_broadcast.shared_at"` |
| `profile_card_broadcast.rs:225` | `card.shared_at.wall_ms, now_secs, MAX_FORWARD_SKEW_MS` | `"profile_card.shared_at"` |

e.g. `profile_card_broadcast.rs`:
```rust
if crate::clock_trust::wall_exceeds_forward_skew_secs_logged(
    card.shared_at.wall_ms,
    now_secs,
    crate::clock_trust::MAX_FORWARD_SKEW_MS,
    "profile_card.shared_at",
) {
```

- [ ] **Step 2: Build + scoped tests**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(profile_broadcast) or test(profile_card)'`
Expected: PASS unchanged.

- [ ] **Step 3: fmt + clippy + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
git add src/profile_broadcast.rs src/profile_card_broadcast.rs
git commit -m "ZEB-855: route wall_exceeds_forward_skew_secs sites through logged wrapper"
```

---

### Final verification (before PR)

- [ ] **Full CI-parity gate** from `src-tauri/`:
  - `cargo fmt --all -- --check`
  - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- [ ] **Frontend sanity** from repo root: `npx tsc --noEmit` (no FE change expected).
- [ ] **Grep audit** — confirm no in-scope reject site still calls a plain predicate, and no excluded site was touched:
  - `grep -rn 'clock_trust::\(reject_future\|wall_exceeds_forward_skew\)(' src` — remaining plain calls should be ONLY: the excluded sites (`community_state_crdt.rs:634`, `persistent_card_store.rs`, negated guards `fleet_net:198`/`vine_feed_cache:813`/`community_membership:2336`), the `owner_trust_sync.rs` `secs_exceeds_forward_skew` site, and the wrappers' own internal calls in `clock_trust.rs`.

---

## Notes for the executor

- These are decision-preserving swaps: the parity tests (Task 1) guarantee the logged wrapper returns the same bool, and each subsystem's existing tests prove the reject behaviour is unchanged. There are no new per-site tests by design.
- If any `old_string` is non-unique (the two flagged duplicates), widen it with the adjacent comment rather than editing by line number — line numbers shift as edits land.
- Do not "improve" the excluded sites; their exclusion is a documented design decision (see spec).
