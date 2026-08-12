# ZEB-898 (re-scoped): pin the headless stash→mint→drain card flow + make `republish_owner_card.statusText` optional

**Ticket:** [ZEB-898](https://linear.app/zeblith/issue/ZEB-898) · **Date:** 2026-08-12 · **Branch:** `zeblith/zeb-898-serve-display-name-is-lost-on-a-fresh-mint-headless-node`

## 1. Context — why the scope changed

The ticket's primary bug ("headless `serve --display-name` card publish fails pre-mint and is never retried post-mint") is **already fixed on main** — verified by live repro on `5d9fccf8` (fresh profile → serve → mint over the API → own-card subscribe returns the display name). The whole machinery landed in one commit, `eb59ca73` (#635, ZEB-882+884, in v0.2.5):

- `republish_owner_card_impl` **stashes** the requested card into `NodeState.pending_card` when the owner runtime isn't wired (instead of dropping it) and returns the not-ready `Err`.
- `start_node_inner`'s success path calls `drain_pending_owner_card` (`lib.rs:15458`) strictly after the atomic commit block sets all five gating components (`dm_outbox`, `dm_self_owner`, `dm_device_id`, `hlc_tracker`, `profile_card_publisher`).
- `mint_owner_identity_inner` Phase 3 restarts via `start_node_inner`, so a fresh-mint headless node drains the boot-time stash.

What remains real (confirmed live 2026-08-12):

1. **No test pins the end-to-end headless flow.** The flow broke silently pre-#635 and was fixed incidentally to the GUI boot-race fix. The individual seams have unit tests (stash-on-not-ready, drain-no-op, drain-leaves-latch-when-not-ready), but nothing pins (a) the latch surviving mint's Phase-1 `stop_inner`, or (b) the full stash → mint → real-restart → published chain. A future refactor of any joint (e.g. a "clear stale state" sweep added to `stop_inner`) would silently reintroduce the field failure.
2. **`republish_owner_card` requires `statusText`** (HTTP 400 when omitted) — needless friction for the headless workaround/agent surface, where callers only care about the display name.

Out of scope (split to its own ticket): the `get_owner_state.ownerDisplayName` device-label-vs-card-name confusion — design-flavored (rename vs. new field, GUI consumers).

## 2. Design

### 2.1 Regression tests (no production behavior change)

**T1 — `stop_inner` preserves the latch** (unit, `pending_owner_card_tests` in `lib.rs`): stash a `PendingCard`, call `crate::stop_inner(&state, None)` (exactly what mint Phase 1 calls), assert the latch is still present with the same display name. Pins latch survival across mint's stop.

**T2 — full headless mint flow publishes the stashed card** (in-crate integration test, real node boot — mirrors the `zeb687` boot-guard test pattern at `lib.rs:85500`):

1. `EnvVarGuard` scoping: tempdir `HOME`/`USERPROFILE`/`XDG_DATA_HOME`/`APPDATA`, `HARMONY_PASSPHRASE`, `HARMONY_DISABLE_KEYCHAIN=1` (ZEB-428 posture; keychain constructor refuses in test builds anyway).
2. Fresh `NodeState::default()` → `republish_owner_card_impl(&state, "Zeb898", "", None, None)` → assert not-ready `Err` + latch stashed (the headless serve boot publish, exactly).
3. Mint via `mint_owner_identity_inner_for_test(&state, restart)` where `restart` performs the **real** `start_node_inner(None, sink, None, &state, None)` — the same closure shape production's `mint_owner_identity_impl` passes. (`warm_up_iroh_global_init().await` first, per ZEB-347, so the one-time global bind isn't paid under the assertion.)
4. Assert: `pending_card` is now `None` (drained) AND `profile_card_publisher.latest_handle()` holds a card whose decoded CBOR (`ProfileCardBroadcast`) carries `display_name == "Zeb898"` — the publisher-side observable the ZEB-884 queryable and 600 s refresher serve to peers.
5. Teardown: `stop_inner(&state, None)` before the env guards drop.

T2 is the true pin of the field flow: if any joint regresses (stash dropped, latch cleared on stop, drain skipped or reordered before the commit block, publisher not wired at drain time), the test fails.

**T3 — RPC args accept omitted `statusText`** (in `api/rpc.rs` tests): dispatch `republish_owner_card` with `{"displayName": "OnlyName"}` against a default `NodeState` registry and assert it fails with the **Command** not-ready error (the existing args-shape test convention at `rpc.rs:2642` — reaching the command error proves args parsed). A companion pure-deserialize assert pins `status_text == ""` on omission.

### 2.2 `statusText` optional (one-line production change)

`RepublishOwnerCardArgs` (`api/rpc.rs:645`): add `#[serde(default)]` to `status_text`. Omitted → `""` — identical to what the headless serve boot publish already passes, and an empty status is the natural "no status" card. The Tauri IPC command keeps requiring both parameters (the GUI always sends both; IPC arg-shape stays untouched, so no frontend change and no parity break — the RPC surface is a strict widening).

Rejected alternative: `Option<String>` + skip-if-absent semantics ("absent = keep previous status"). That would make the card publish read-modify-write against `ProfileCardPublisher.latest`, adding a read dependency and a stale-card race for no caller who wants it (the GUI always sends the full card; agents want "set the name").

## 3. Rotation/compat notes

No wire-format, CRDT, IPC-shape, or behavior change on any existing caller. The RPC widening is backward-compatible (all current callers pass both fields).

## 4. Testing

T1–T3 above, plus the full existing suites as regression backstop. Gates: fmt, clippy `--all-targets -D warnings`, targeted nextest (`pending_owner_card`, `rpc`, mint/card-related), full `--workspace --all-targets` sweep pre-PR.

## 5. Rollout

Single small PR. Linear: ZEB-898 closes with it (re-scoped meaning documented in-ticket); the `ownerDisplayName` DX item gets its own ticket at PR time.
