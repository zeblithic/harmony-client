# ZEB-690 — ZEB-510 dial-seeding test/doc hardening (design)

**Ticket:** [ZEB-690](https://linear.app/zeblith/issue/ZEB-690) (follow-up to [ZEB-510](https://linear.app/zeblith/issue/ZEB-510), PR #469). Priority Low. All items are non-blocking Minors deferred from the dial-seeding branch; none affect shipped behavior.

**Branch:** `zeb-690-hardening` (off `main@f3ded0b7`).

## Goal

Close the deferred Minors from PR #469 with focused tests, one small testability refactor, two doc/cosmetic fixes, and a harness freshness guard that prevents the stale-`harmony-app`-binary trap that invalidated s7 gates mid-branch.

## Scope re-verification against the *merged* code

Reading the merged tree shrank the bundle:

- **Item 5 (owner_device_cache idempotency) — code already fixed.** PR #469's converge fix `fleet_net.rs:463–472` already makes `seed_sibling_device_cache` no-op *only* when the sibling is present *with* `Some(pub)`; if the hash exists with `pub == None` it falls through and re-adds, letting `apply_owner_device_update`'s Some-over-None dedup fill the pub without duplicating. → **Remaining work: a regression test** for the `pub == None` fall-through (the existing test starts from empty state and never hits it).
- **Item 3 (Confirm back-compat) — mechanism already load-bearing.** The new fields carry `#[serde(default, skip_serializing_if = "Option::is_none")]`, so a `None`-Confirm serializes to the same wire shape as an old peer (the iroh keys are skipped entirely); the existing `without` round-trip test already exercises that path. → **Remaining work: a tight hard-fixture** pinning both halves of the contract.

## Work items

### Test additions (existing infra; no new scaffolding)

- **Item 1 — fleet heartbeat re-feed does not re-fire `NewPeer`** (`reachability_resolver.rs`).
   Uses the existing `SupervisorHandle::new()` + `pending_trigger()` spy (same pattern as `first_learn_kicks_new_peer` / `changed_relay_kicks_record_changed`). Steps: feed a `FleetSibling` entry with **no** supervisor installed (no kick recorded); install a fresh `SupervisorHandle`; re-feed a **newer-HLC, identical-addressing** heartbeat via `update_with_source(.., ReachabilitySource::FleetSibling)`; assert `pending_trigger(node_id) == None`. Pins that `was_present` (fleet slot populated) suppresses `NewPeer` and that unchanged `addr_key` keeps `RecordChanged` silent.

- **Item 2 — persist branch coverage** (`fleet_peer_seed_persist.rs`), two tests:
   - *Trailing-bytes rejection:* write `[version] || valid-CBOR || extra byte`; assert `load` errors with the "trailing bytes after fleet-peer-seed value" `CborDecode` (distinct from the existing junk-CBOR quarantine test).
   - *Transient-IO propagation:* drive `load_doc_or_recover` at a path whose read fails with a non-`NotFound`, non-decode error (e.g. a path that is a directory), and assert the error **propagates** (`Err`) rather than quarantining to `default()`.

- **Item 3 — Confirm back-compat hard-fixture** (`pairing/types.rs`).
   Pin both halves of the wire contract that `#[serde(default, skip_serializing_if = "Option::is_none")]` provides: (a) serialize a `Confirm` with `iroh_node_id_hex: None` / `iroh_home_relay: None` and assert the CBOR carries no `irohNodeIdHex`/`irohHomeRelay` key (forward: a `None`-Confirm emits the same key set an old peer does — this is structural/semantic equivalence, not a byte-pin against a captured legacy fixture); (b) decode a hand-built old-style CBOR map (`kind` + `sasDigits` only) and assert it yields a `Confirm` with both iroh fields `None` (backward: old wire decodes). Distinct from the existing `without` round-trip, which only proves current-serializer↔current-deserializer symmetry.

- **Item 5 — partial-pub idempotency regression** (`fleet_net.rs`).
   Construct a self-owner `owner_device_cache` entry that already holds the sibling's device hash with its aligned pub `None`; call `seed_sibling_device_cache`; assert `vk_lookup` (`vk_map_from_device_cache`) now resolves the sibling **and** the device list carries no duplicate hash. Pins the converge fix's fall-through arm.

### Testability refactor + test

- **Item 4 — extract `decode_peer_iroh_endpoint`** (`pairing/state_machine.rs`).
   Lift the inline decode in the Confirm handler (`match hex::decode(&nid_hex) { Ok(bytes) if bytes.len()==32 => Some(..), _ => None }`) into a pure helper `fn decode_peer_iroh_endpoint(node_id_hex: Option<String>, home_relay: Option<String>) -> Option<([u8; 32], String)>`; call it from the handler (behavior unchanged); unit-test 4 cases: valid → `Some`, malformed hex → `None`, wrong length → `None`, absent (`None` hex) → `None`. Makes the defensive branch testable without the transport/channel/ctx rig.

### Docs / cosmetic

- **Item 6 — docstring drift** (`reachability_resolver.rs::resolve_entry_by_node_id`). "across the peer's durable and pkarr slots (ties → durable)" predates ZEB-510's fleet slot, which `freshest()` includes. Correct to name the fleet slot and its precedence.

- **Item 7 — arrow char** (`e2e_two_node.rs:1835`). `.expect()` message uses ASCII `->`; the header comment uses `→`. One-char consistency fix.

### Harness freshness guard

- **Item 8 — stale-binary freshness assert** (`e2e-harness/src/bin_resolver.rs`).

   **Mechanism: mtime-vs-source.** This is the correct primitive: the harness does not depend on the `harmony-app` crate, so a build-stamp handshake would be blind to app-source edits in the common dirty-tree dev loop — only the source files' mtime tracks what matters.

   After resolving the binary path (env override or `target/{release,debug}`), compute the newest mtime among the app's first-party sources: `src-tauri/src/**/*.rs` plus `src-tauri/Cargo.toml`, `src-tauri/Cargo.lock`, and `src-tauri/build.rs`. Exclude `src-tauri/target/` and `src-tauri/vendor/` (vendored deps change rarely and force explicit rebuilds anyway — a documented false-negative gap). All first-party app code is under `src-tauri/` (single crate; the `owner_state_crdt`/`dm_signing`/etc. modules are `crate::`-local), so this scope is complete.

   **On detected staleness (binary mtime < newest source mtime): HARD-FAIL** with an actionable message naming the offending newer source file and the fix (`cd src-tauri && cargo build --bin harmony-app`). Escape hatch: `HARMONY_APP_FRESHNESS=off` bypasses the check. Applies to the env-override path too (a stale `HARMONY_APP_BIN` bit us as well). **Graceful skip** when `src-tauri/src` is not locatable from the manifest dir (installed/packaged contexts) — never a false failure.

   Rationale (Jake, 2026-07-14): failing closed makes it *impossible* to silently test stale code (the trap cost hours). The residual false positive — a `git checkout` rewriting source mtimes newer than a just-built binary — resolves to a 5-second rebuild or the env var, and after a branch switch a rebuild is warranted anyway.

## Testing

Every item ships with its own test except items 6 and 7 (doc/cosmetic — covered by compile + `fmt`). Item 8's guard is exercised by unit tests in `bin_resolver.rs`: stale binary → error; fresh binary → ok; missing source tree → skip; `HARMONY_APP_FRESHNESS=off` → bypass. Full-suite gate: `cargo nextest run --locked --workspace --all-targets --features test-fixtures` from `src-tauri/`, plus the standalone `e2e-harness` crate's own `cargo nextest run --locked` for `bin_resolver` (its e2e scenario tests are opt-in via `--features e2e` and need a built binary, so `--all-targets` is not used for the unit run here). CI-parity `fmt` + `clippy --locked --all-targets` as the final gate.

## Non-goals

- No change to shipped ZEB-510 behavior — this is test/doc/harness hardening only.
- Not the ZEB-689 cross-WAN transport validation (separate ticket; parked).
- Vendored-crate edits are not tracked by the freshness guard (documented gap).
