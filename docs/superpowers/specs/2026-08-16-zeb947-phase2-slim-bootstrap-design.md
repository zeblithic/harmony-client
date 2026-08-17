# ZEB-947 Phase 2 — Slim-Bootstrap Community Invites — Design

**Status:** Approved design (sections 1–3 signed off 2026-08-16). Ready for implementation-plan authoring.

**Epic:** [ZEB-947](https://linear.app/zeblith/issue/ZEB-947) — shrink / "dereference" community invites so they fit an out-of-band message.
**Predecessor:** Phase 1 = ZEB-948 (deflate compression), merged in PR #695.
**Feasibility gate:** the membership-gate bootstrap spike (recorded as a ZEB-947 comment, 2026-08-16) — **RESOLVED YES**.

---

## 1. Problem & context

A community invite is `harmony://invite/<base64url(canonical_cbor(CommunityInvitePayload))>`. Its dominant cost is `epoch_snapshot.state_snapshot` — a full `MaterializedCommunityState` (every member + device pubkeys + enrollment sigs, all channels, policies), so the invite grows **O(members)** and, for large communities, exceeds Discord's 2000-char non-Nitro cap.

Phase 1 (deflate) is a one-time ~2× level-shift but does **not** change the slope: ~47% of the payload is the incompressible cryptographic core, and the roster still grows without bound. Phase 2 attacks the growth directly: **stop inlining the roster; let it sync P2P** — which is how Harmony distributes every other piece of community state.

### Why this is safe (the spike verdict)

The receive-side membership-at-HLC gate derives membership **exclusively from the P2P-synced event log** anchored at `admin_addr`; the inlined roster is never consulted by verification. Established facts (full citations in the ZEB-947 spike comment):

- The invariant is already documented: `community_invite.rs:22-23` — *"state_snapshot is a UI bootstrap hint — CRDT replay post-redemption is the source of truth."*
- The gate `community_membership::prior_state_at_hlc(events, at, admin_addr)` (~`community_membership.rs:4065-4098`) takes the synced event log + trust root; it has no snapshot parameter. All call sites feed `state.events()`, never the invite snapshot.
- Inbound events self-authenticate via their **embedded EnrollmentCert** chaining to `admin_addr` (`community_membership.rs:1678-1703`), so a fresh signer verifies with zero pre-loaded roster.
- The one genuinely-needed anchor is the admin's own Join, carried by `admin_bootstrap` (`community_invite.rs:136-148`, inserted at `lib.rs:42155-42156` before ingestion). The slim invite **keeps** this — it is not the roster.
- The roster snapshot's only post-decode consumer is a display seed (`lib.rs:41919-41940` → `seed_bootstrap_hint`), returned by `materialized()` only while `log.is_empty()` (`community_state_crdt.rs:631-636`) and discarded the instant the first real event lands.

**Load-bearing delivery constraint** (empirically confirmed in the spike): the gate is verify-**on-insert**, not retroactive — a member's authored event inserted *before* that member's Join is rejected and dropped. This is safe because (a) a member's Join always has an earlier HLC than anything they author, so a sort-ordered state-root batch merge delivers Join-before-authored, and (b) the engine already **defers-not-drops** unknown invite-only publishers (ZEB-526, `community_state_sync.rs:4961-4975`) and recovers on re-delivery. **Phase 2 must preserve both:** it strips the roster from the invite but the P2P sync must still deliver a coherent Join-inclusive membership log — which it already does.

---

## 2. Approved decisions

| Decision | Choice | Rationale |
| -- | -- | -- |
| Roster policy | **Uniform slim** — always strip the roster | One code path, uniform behavior; simplest. (Adaptive "inline-when-fits" was considered and declined.) |
| Slim payload scope | **Option A — drop everything**: emit `MaterializedCommunityState::default()` | Members, channels, and power-levels all sync in ~1s; no materialize on invite-gen; fully uniform. |
| Wire format | **No change** — reuse the existing field | `state_snapshot`'s maps already serialize empty; no `Option`, no version byte, no struct change. |
| First-sync UX | **Frontend-only `initialSyncing` flag** (Option A) | Mirrors the existing `channelSyncing` idiom; zero backend/IPC surface. |

**Explicitly out of scope** (deferred, not forgotten):
- Phase 3 (token-keyed pkarr-hosted encrypted pointer) — crosses the metadata-leak line; a separate values decision, only if large-community links still hurt after Phase 2.
- Adaptive inline-when-fits policy.
- A backend sync-progress event.
- Slimming `pre_fork_snapshot` on fork invites (§3) — a separate fork-seeding question; the existing oversize-fork fallback remains the safety net.

---

## 3. Architecture — the encoder change

The entire production change is one block. `generate_invite_impl` (`lib.rs:36098`) currently builds the snapshot at `lib.rs:36187-36207`:

```rust
let state_snapshot = {
    let materialized = if engine_state.is_some() {
        let wall_now_ms = /* now */;
        crate::community_membership::materialize_with_now(&events, admin, Some(wall_now_ms))
    } else {
        crate::community_membership::MaterializedMembership::default()
    };
    crate::community_invite::MaterializedCommunityState {
        members: materialized.members,
        channels: materialized.channels,
        power_levels: materialized.power_levels,
    }
};
```

**Slim replacement:** emit an empty snapshot and drop the materialize entirely:

```rust
// ZEB-947 Phase 2: invites no longer inline the roster — members, channels, and
// policies sync P2P after redemption (spike verdict: the snapshot is a UI hint,
// never consulted by the membership-at-HLC gate). O(members) → O(1).
let state_snapshot = crate::community_invite::MaterializedCommunityState::default();
```

### What is kept vs stripped

**Kept** (the credential + orientation — all fixed-size or O(1)):
`community_id`, `epoch_snapshot.epoch`, `sealed_epoch_key` / `sealed_epoch_keys` (targeted invite-only, O(invitee devices)), `admin_addr`, `community_name`, `is_invite_only`, `expires_at`, `invite_token`, `admin_bootstrap`, `admin_identity_pub`, `forked_from` (a 16-byte `SpaceId`), `inviter_signer_certs`, `inviter_enrollment`, `untargeted_decrypt_key`.

**Stripped** (the O(members) bulk): `state_snapshot.members`, `.channels`, `.power_levels` — all now empty. This also removes, for free, the zeroed `ed25519_pub` / PQ placeholder fields that lived per-member inside the roster (the Phase-1-descoped "strip dead placeholders" item — resolved as a side effect).

**Explicitly NOT slimmed by this phase:** `pre_fork_snapshot` — the fork-specific snapshot of the *parent* community, which can itself be O(members). The membership-gate spike cleared only the current community's roster, not fork-seeding semantics (does a forked community need the parent's materialized state to bootstrap, or does that also sync?). Slimming it is a separate question deferred to a follow-up; meanwhile the existing oversize-fork fallback in `generate_invite` (drops `forked_from` + `pre_fork_snapshot` when the invite is too large — extended in ZEB-948 to cover both size errors) remains the safety net, so fork invites are never worse than today. Non-fork invites (the overwhelming majority) carry no `pre_fork_snapshot` and get the full O(1) win.

### Decode / redeem — unchanged

The decoder and `redeem_invite` path already tolerate an empty snapshot (spike-verified end to end; the simnet `SimCommunity` harness redeems `MaterializedCommunityState::default()` invites today). The joiner seeds nothing, mints its own Join, inserts `admin_bootstrap`, and syncs the rest. Phase-1 deflate still wraps the encoded bytes, so a slim invite is a few hundred chars regardless of community size.

### Rollout / version-skew

- **Wire level:** a slim invite deserializes cleanly on older clients — every field is present or serde-defaulted. No parse breakage.
- **Behavior level:** an older client redeeming a slim invite shows empty-then-syncing **iff** its historical redeem path has no hard dependency on a non-empty roster. The "CRDT replay is the source of truth" contract has held since ZEB-249, so it was designed not to.
- **Implementation Task 0 (verification gate):** before flipping the encoder, confirm the redeem path (`redeem_invite_inner` / `seed_bootstrap_hint` consumers) has no hard roster dependency. If — unexpectedly — it does, the fallback is a one-line version-tag gate on emission. New↔new (peers on the same release, the common case) is unconditionally fine.

---

## 4. Frontend — graceful initial-sync UX

**The regression to prevent.** With the roster no longer inlined, a freshly-joined community renders empty for ~1s until sync lands. Two spots currently mishandle that window:
- **Member panel** (`ChannelMembersPanel.svelte:126-168`) renders an empty `<ul>` with no placeholder → looks like a near-empty community.
- **Channel area** (`CommunityView.svelte:547-553`) shows the static *"No channels in this community yet"* → actively misleading.

Both already update reactively — content arrives fine; only the pre-arrival display is wrong:
- Members: `community-members-changed` → `community-service.ts:227-234` → `App.svelte:2067-2076` → `refreshCommunityMembers`.
- Channels: `channel-config-updated` → `App.svelte:2406-2407` (nav) + `CommunityView.svelte:245-255` (feed).

**The fix — extend the existing "syncing" idiom.** `ChannelMessageFeed.svelte:827-833` already renders *"This channel is still syncing — messages will appear shortly"* off `channelSyncing`; the nav has a ⏳ pending badge (`NavNodeRow.svelte`). Add a community-level analogue:

- A per-community **`initialSyncing`** signal (freshly joined, awaiting first roster/channel sync).
  - **Set** in the redeem handler (`App.svelte:4722-4765`), on the just-joined community id.
  - **Cleared** on first real content (first non-self member OR first channel arriving via the reactive listeners), or by a **~10s timeout safety-valve** — whichever first. Clear-on-*content* (not first-event) avoids an empty→populate flicker.
  - Transient / in-memory: on app restart a still-empty community shows the honest empty state, not perpetual "syncing."
- **Member panel:** `initialSyncing && members ≤ self` → *"Syncing members…"* instead of the bare empty list. (`membersLoading` at `App.svelte:1277, 1449-1483` stays as-is — it covers only the IPC round-trip, not the CRDT sync window, so `initialSyncing` is a distinct signal.)
- **Channel area:** `initialSyncing && channels empty` → *"Syncing channels…"* instead of *"No channels yet."*

**Kept distinct from** the ZEB-254 `pending` (invite-only awaiting-countersign) affordance — different semantic (awaiting admission vs awaiting sync); they render independently and may co-occur.

---

## 5. Testing & validation

1. **Regression (Rust):** promote the two spike tests to permanent coverage near the invite/CRDT tests:
   - `slim_bootstrap_joiner_verifies_full_community_from_synced_log_alone` — a fresh empty-log joiner accepts admin + member + a member-authored gated event and materializes the full roster from the synced log alone.
   - `gate_is_on_insert_out_of_order_member_event_rejected_then_recovers` — pins verify-on-insert + delivery-order recovery.
2. **Encoder test:** `generate_invite_impl`, run against a community that *has* members and channels, now emits an invite whose `state_snapshot` is empty (all three maps). Fails if roster inlining is re-introduced.
3. **Size validation (codec-level):** build a `MaterializedCommunityState` with N synthetic members (N = 1, 50, 500); encode the **slim** invite → assert URL < 2000 chars for every N and roughly constant across N (O(1) proof); encode the **old full-snapshot** invite for N = 500 → assert it exceeds 2000 chars (quantifies the win and guards the regression).
4. **Round-trip / rollout guard:** slim invite encode→decode→redeem round-trip; the old-decoder tolerance check is implementation Task 0 (§3).
5. **Frontend (vitest):** `initialSyncing` logic — member panel shows *"Syncing members…"* when `initialSyncing && members ≤ self`; channel area shows *"Syncing channels…"* not *"No channels yet"*; both clear on first content and on timeout.
6. **Optional stretch (flagged, not gated):** an engine-layer e2e in `SimCommunity` where an asymmetrically-seeded joiner (admin-only, not cross-seeded) converges to the full roster via bus sync.

**Gates (CLAUDE.md):** `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; clippy `--locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo fmt --all -- --check`; frontend `npx tsc --noEmit` + `npx vitest run`.

---

## 6. File-touch map (for the plan)

| File | Change |
| -- | -- |
| `src-tauri/src/lib.rs` (`generate_invite_impl`, ~`36187-36207`) | Replace the snapshot-building block with `MaterializedCommunityState::default()`; drop the now-dead materialize. |
| `src-tauri/src/community_invite.rs` (or a sibling test module) | Regression tests (spike tests promoted) + encoder empty-snapshot test + size-validation test. |
| `src/App.svelte` | `initialSyncing` signal: set in redeem handler (`4722-4765`); clear-on-content wiring via `onMembersChanged` (`2067-2076`) + channel listeners; timeout safety-valve. |
| `src/lib/components/ChannelMembersPanel.svelte` | *"Syncing members…"* state (`~126-168`). |
| `src/lib/components/CommunityView.svelte` | Sync-aware channel empty-state (`547-553`). |
| `src/lib/community-service.ts` | If a shared `initialSyncing` store/flag is cleaner than App-local state, house it here alongside the existing reactive listeners. |
| Frontend tests (vitest) | `initialSyncing` behavior. |

**Note:** `src/lib/deep-link-router.ts` only pattern-matches the `harmony://invite/` prefix and hands the opaque string to Rust — no change.

---

## 7. Success criteria (DoD)

- New invites carry an empty `state_snapshot`; invite size is O(1) — a 500-member community's invite fits a plain Discord message (< 2000 chars), proven by the size test.
- Existing invites still redeem; the redeem path has no hard roster dependency (Task 0 verified).
- A freshly-joined community shows *"Syncing members… / Syncing channels…"* during the sync window (no misleading empty states), and the lists fill in reactively.
- The two membership-gate regression tests are permanent.
- All Rust + frontend gates green.

---

## 8. Risks & open items

- **Old-client behavioral tolerance** (§3 Task 0) — expected clean given the ZEB-249 contract; verified before encoder flip.
- **Timeout tuning** — the ~10s `initialSyncing` safety-valve is a starting value; a genuinely-partitioned joiner will see the honest empty state after it, which is correct.
- **Self-only communities** — a 2-member community (admin + joiner) with 0 channels correctly resolves to the honest empty state after sync/timeout.
- **No genuinely large real community exists** to field-test scaling — the N=500 synthetic fixture stands in; this is the epic's noted limitation and is acceptable for a codec-level size guarantee.
