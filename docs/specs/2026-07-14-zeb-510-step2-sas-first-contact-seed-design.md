# ZEB-510 Step 2 — SAS first-contact endpoint seed

**Status:** DRAFT for Jake's review (Koya, 2026-07-14). Design gate — no implementation until approved.
**Ticket:** [ZEB-510](https://linear.app/zeblith/issue/ZEB-510) (parent [ZEB-451](https://linear.app/zeblith/issue/ZEB-451)).
**Predecessor:** Step 1 (FleetNetDoc→resolver wiring) — DONE, reviewed-Approved, full-sweep 4589/4589 green, on branch `zeb-510-fleet-sibling-dial-seeding`. **Ships together with step 2 as one PR once s7 goes green** (Jake's decision, 2026-07-14).
**Step-1 design:** `docs/specs/2026-07-14-zeb-510-fleet-sibling-dial-seeding-design.md`.

## Why step 2 (the empirical gate result)

Step 1 promoted the e2e `s7_butler_deposit_recover` `HELD` boundary to a hard assert; it **timed out at 120s co-located** (run 2026-07-14, artifacts `e2e-runs/s7-1784048221-15218`). Every prior boundary passed — PAIRED, COMMUNITY, `DEVICES total=2, peers=1` (P *does* have a B2 device row), PIN, B2-BUTLER-READY, REACHABILITY (A observed P's durable B2 butler-set) — but the deposit never reached B2.

This is the **step-1-vs-step-2 gate firing exactly as predicted.** Step 1's boot-replay seeds the resolver *from* `FleetNetDoc`, but `FleetNetDoc` only gains B2's **real** iroh endpoint via B2's inbound fleet-net publish, which requires P↔B2 to have **already peered** on the `fleet-net-v1` zenoh topic — and nothing bootstraps that first peering co-located. **The pairing ceremony is LAN-only Zenoh broadcast + SAS-encrypt; it establishes no iroh transport connection and neither device learns the other's iroh endpoint** (recon §E, confirmed: no iroh dial anywhere in the pairing flow today). So P never learns B2's dialing coordinates, its published butler-set carries the wrong endpoint for B2, and A dials the wrong place.

Step 2 breaks the circular bootstrap: **each device observes the other's iroh endpoint first-hand during the SAS handshake** and persists it as a dial seed, feeding it into the resolver at boot as a `FleetSibling` entry — so P can dial B2 the very first time, establishing the fleet-net peering that then keeps everything converged.

## The mechanism (one paragraph)

The SAS `Confirm` message is **bidirectional** — both roles send it and each verifies the peer's SAS digits (`state_machine.rs:745` send, `:1525` receive; the `peer_confirmed`/`maybe_advance_to_enroll` mutual-confirmation gate). It rides the session-encrypted, SAS-authenticated channel. We piggyback each device's **own** iroh endpoint (node_id + home_relay) onto its `Confirm`. On receiving the peer's `Confirm`, each side stashes the peer's endpoint; when enrollment completes, that observation is carried to the persistence layer and written to a new dedicated **`fleet_peer_seed.cbor`** store (plaintext CBOR, `fleet_net_persist.rs` idiom). At boot, `start_node` loads the seed store and feeds each row into the `ReachabilityResolver` as a `FleetSibling` entry — the exact same source/slot/mapper step 1 already built. Once P dials B2 and fleet-net converges, B2's **real** self-stamped `FleetNetDoc` row (fresher HLC) supersedes the seed via the resolver's existing per-source LWW.

## Architecture

### 1. Wire: carry the endpoint on `EncryptedPayload::Confirm`

`src-tauri/src/pairing/types.rs:117-120`. Extend the `Confirm` variant with two back-compatible optional fields:

```rust
    Confirm {
        sas_digits: String,
        /// ZEB-510 step 2: the sender's iroh transport endpoint, observed
        /// first-hand over the SAS-authenticated channel so each device can
        /// seed a dial route to its fleet sibling before fleet-net converges.
        /// `#[serde(default)]` keeps pre-step-2 peers decodable (they omit it;
        /// we tolerate `None` and simply write no seed).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        iroh_node_id_hex: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        iroh_home_relay: Option<String>,
    },
```

Rationale for `Confirm` over the design doc's original `*EnrollResult` suggestion: `Confirm` is the one **bidirectional wire** message (both roles send it), so a single field pair + single handler branch seeds **both** directions (P learns B2, B2 learns P). `InviterEnrollResult`/`JoinerEnrollResult` (`state_machine.rs:73-113`) are *local* persistence-handoff structs, not wire messages — they still get a field (step 4) to carry the observed peer endpoint to disk, but they are not the transport.

### 2. Thread the local iroh endpoint into the state machine

The pairing SM has **no** access to the local iroh endpoint today (recon §B: zero hits for `iroh_endpoint` across `pairing/`). Thread it the same way `fleet_current_epoch` is threaded onto `PairingCommand::StartInviter`/`StartJoiner` (`state_machine.rs:~58`):

- In `pairing_commands.rs`, at the `StartInviter`/`StartJoiner` construction sites (`:60-67`, `:122-129`, `:154-157`), read `NodeState.iroh_endpoint` (`lib.rs:1449`, set at `:11091`) under the existing `state.lock()` — the same lock already read for `owner_quorum_doc`/`owner_trust_doc` (`pairing_commands.rs:100-114`). Extract `(node_id: [u8;32], home_relay: String)` via `ep.node_id().as_bytes()` / `ep.home_relay()` (the shape used at `lib.rs:5627-5631`). `Option` — a headless/no-transport node passes `None` and simply seeds nothing.
- Add a `local_iroh_endpoint: Option<(/*node_id*/ [u8;32], /*home_relay*/ String)>` field to both `PairingCommand::StartInviter` and `StartJoiner`, carried into the session `ctx` (`state_machine.rs:~445`, alongside `sas_digits`).

### 3. Send side: attach on `Confirm`

`send_confirm` (`state_machine.rs:745`): populate `iroh_node_id_hex`/`iroh_home_relay` from `ctx.local_iroh_endpoint` (hex-encode the node_id). No other change to the encrypt/publish path.

### 4. Receive side + persistence handoff

- Confirm handler (`state_machine.rs:1525`): after the SAS-match check, if the peer's `Confirm` carries an endpoint, stash it in `ctx.peer_iroh_endpoint: Option<(/*node_id*/[u8;32], /*home_relay*/String)>`.
- When the SM builds the enroll result on success — `InviterEnrollResult` (`state_machine.rs:100-113`, built `:1268`) and `JoinerEnrollResult` (`:73-82`, built `:1754`) — add a field `peer_iroh_endpoint: Option<(...)>` set from `ctx.peer_iroh_endpoint`.
- The drainer in `start_node` (`lib.rs:11932-11983`) already routes these into `pairing::persist::install_joiner_state` / `install_inviter_state` (`pairing/persist.rs:29,128`). Extend those (`_inner` seams) to also persist the observed peer endpoint into the new seed store, keyed by the **peer's device id** already known from the enrollment (the joiner's `our_device_id`/the inviter learns it from the cert it signs). The seed row records `(iroh_node_id, home_relay, observed_at_ms)`.

### 5. New `fleet_peer_seed` store

Mirror the `fleet_net_persist.rs` idiom **1:1** (recon §C), minus the CRDT/replay machinery (the seed is a one-shot local write, not a synced CRDT):

- `src-tauri/src/fleet_peer_seed.rs`: the doc type.
  ```rust
  #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
  pub struct FleetPeerSeedDoc {
      /// Keyed by the peer's iroh node_id (hex). Both sides learn the peer's
      /// node_id directly from the received CONFIRM (no device-id lookup — the
      /// joiner side has no clean way to identify *which* enrollment is the
      /// inviter's device anyway), and the resolver key is `(self_owner,
      /// iroh_node_id)` regardless, so the seed and the eventual real
      /// FleetNetDoc row converge on the SAME resolver slot for that node.
      #[serde(rename = "sd")]
      pub seeds: std::collections::BTreeMap<String, FleetPeerSeedRow>,
  }
  #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
  pub struct FleetPeerSeedRow {
      #[serde(rename = "ep")]
      pub iroh_node_id: [u8; 32],
      #[serde(rename = "hr")]
      pub home_relay: String,
      /// Pairing-time wall-clock ms — used as the resolver entry's announce
      /// time so a real FleetNetDoc self-row (re-stamped every boot with a
      /// later clock) always supersedes the seed via LWW.
      #[serde(rename = "oa")]
      pub observed_at_ms: u64,
  }
  ```
- `src-tauri/src/fleet_peer_seed_persist.rs`: `FLEET_PEER_SEED_FILENAME = "fleet_peer_seed.cbor"`, `FLEET_PEER_SEED_SCHEMA_V1: u8 = 1`, and `save`/`load`/`load_doc_or_recover` copied from `fleet_net_persist.rs:41-179` (version byte + CBOR + `atomic_write` via `owner_state_persist::save_atomically`; corrupt-quarantine on `CborDecode`, propagate transient I/O). Skip the replay-tracker and `FleetPersist` trait parts — not applicable to a one-shot store.

### 6. Boot-feed hook (composes with step 1)

In `start_node`, immediately after the step-1 boot-replay hook (`lib.rs:5668-5689`), load the seed store and feed each row into the resolver as `FleetSibling`, excluding the self device id:

```rust
    // ZEB-510 step 2: feed SAS first-contact seeds into the resolver so P can
    // dial a freshly-paired sibling BEFORE fleet-net has ever converged. A real
    // FleetNetDoc row for the same device (fed by the step-1 hook above) wins
    // via LWW once it exists — the seed only fills the pre-convergence gap.
    {
        let self_node_id = iroh_endpoint_arc.as_ref().map(|ep| *ep.node_id().as_bytes());
        let seed_path = identity_dir.join(crate::fleet_peer_seed_persist::FLEET_PEER_SEED_FILENAME);
        let seed_doc = crate::fleet_peer_seed_persist::load_doc_or_recover(&seed_path)
            .map_err(|e| format!("load fleet-peer-seed doc: {e}"))?;
        for row in seed_doc.seeds.values() {
            if Some(row.iroh_node_id) == self_node_id { continue; } // never seed ourselves
            reachability_resolver.update_with_source(
                self_owner,
                crate::fleet_peer_seed::seed_reachability_payload(row),
                // Synthetic HLC: empty device_id, pairing-time wall clock. A real
                // FleetNetDoc self-row for this node (fed by the step-1 hook) wins
                // once B2's HLC advances past pairing time — but correctness does
                // not depend on it: the seed and the real row carry the SAME stable
                // node_id, so P dials B2 correctly whichever holds the slot.
                crate::owner_state_types::Hlc { wall_ms: row.observed_at_ms, logical: 0, device_id: String::new() },
                crate::reachability_resolver::ReachabilitySource::FleetSibling,
            );
        }
    }
```

`seed_reachability_payload(row)` mirrors `fleet_net::sibling_reachability_payload` (zero signature, empty butler_set, `announced_at_ms = row.observed_at_ms`). Both step-1 and step-2 feeds target the **same** `fleet` resolver slot keyed by `(self_owner, iroh_node_id)`; the resolver's per-source LWW (`reachability_resolver.rs` `lww_newer`, recon §D) resolves them order-independently, and B2's real self-stamped row — re-stamped every boot with a current wall clock — carries a later `wall_ms` than the pairing-time seed, so it always wins once fleet-net converges. **Supersession confirmed mechanically** (recon §D).

## Design decisions (resolved — flagged for your sign-off)

1. **Directionality: bidirectional via `Confirm` (both P and B2 seed each other).** The s7-critical direction is P learning B2 (P publishes the butler-set and must dial B2), but `Confirm` is symmetric so we get both for free. ✅ recommend.
2. **Seed store: a NEW dedicated `fleet_peer_seed.cbor`, NOT injected into `FleetNetDoc`** (decision (b)=B2 from the step-1 design, upheld). Injecting a foreign-authored B2 row into the CRDT-synced `FleetNetDoc` would publish P-authored B2 data into the fleet and fight the self-stamped-by-subject convention. The seed is purely **local** (P's private dial hint), never published, feeds only P's resolver. ✅ recommend.
3. **At-rest: plaintext CBOR** (matches `fleet_net_persist` policy). The endpoint is captured over the SAS-authenticated channel (integrity assured), and a node_id + home_relay is dialing-coordinate metadata, not a secret — the same class already stored plaintext in `fleet_net.cbor` and published in the pkarr routing record. ✅ recommend. *(Recon open-q1: it is briefly the only copy pre-convergence — this does not change the sensitivity class; flagging for your awareness.)*
4. **Staleness / TTL: none (YAGNI).** iroh node_id is **stable across reboots** (persisted `iroh_sk.enc`), so the seed's node_id stays dialable even if B2 restarts; home_relay drift is tolerated by node_id-based dialing (holepunch/relay rediscovery); and the seed is superseded by the real row the moment fleet-net converges (which the seed itself bootstraps). A stale seed that never converges self-heals via the supervisor's normal retry. ✅ recommend no TTL.
5. **Seed HLC stamp:** synthetic `Hlc { wall_ms = observed_at_ms (pairing time), logical = 0, device_id = "" }` (mirrors the pkarr-refresh path's synthetic HLC). Supersession by B2's real self-row is **best-effort, not correctness-critical**: the seed and the real row carry the SAME stable iroh node_id (persisted `iroh_sk`), so P dials B2 correctly whichever holds the resolver slot. Once B2's HLC advances past pairing time (next boot re-stamp), the real row wins the slot and keeps `home_relay` fresh. ✅ recommend.

## Validation

- **Primary:** the promoted s7 `HELD` hard assert (already on the branch) should now **PASS** co-located: P observes B2's endpoint at pairing → seeds it → dials B2 → bootstraps fleet-net → publishes B2's correct endpoint → A's deposit lands on B2. If it does, also try promoting `RECV`/`CLEARED` (the recover half) to hard asserts (the step-1 design left them soft with a residual note).
- **Unit/integration (deterministic, in the src-tauri gate):**
  - `EncryptedPayload::Confirm` round-trips the endpoint fields and stays decodable when they're absent (back-compat).
  - `fleet_peer_seed_persist` save→load round-trip + corrupt-quarantine (mirror the `fleet_net_persist` tests).
  - A seed row fed at boot produces a `FleetSibling` resolver entry keyed `(self_owner, node_id)`; a real `FleetNetDoc` row with a later `wall_ms` supersedes it (LWW), and self is excluded.
  - The pairing SM stashes the peer's endpoint from a `Confirm` and carries it into the enroll result (mock transport).
- **Full gates:** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast`.

## File-touch map (step 2)

- `src-tauri/src/pairing/types.rs` — `EncryptedPayload::Confirm` endpoint fields.
- `src-tauri/src/pairing/state_machine.rs` — thread `local_iroh_endpoint` onto `PairingCommand::Start{Inviter,Joiner}` + `ctx`; attach in `send_confirm` (:745); stash peer endpoint in the `Confirm` handler (:1525); add `peer_iroh_endpoint` to `Inviter/JoinerEnrollResult` (:73-113) at the build sites (:1268/:1754).
- `src-tauri/src/pairing_commands.rs` — read `NodeState.iroh_endpoint` at the `Start*` construction sites (:60-157).
- `src-tauri/src/pairing/persist.rs` — `install_{inviter,joiner}_state_inner` write the seed row.
- `src-tauri/src/fleet_peer_seed.rs` — NEW: doc/row types + `seed_reachability_payload` mapper.
- `src-tauri/src/fleet_peer_seed_persist.rs` — NEW: persistence (fleet_net_persist idiom).
- `src-tauri/src/lib.rs` — register the two new modules; boot-feed hook after :5689.
- `e2e-harness/tests/e2e_two_node.rs` — s7 already promoted (step 1); optionally promote RECV/CLEARED if they pass.

## Risks

- **Pairing wire-format change.** Additive `#[serde(default)]` optional fields keep old↔new pairing compatible (the `fleet_keytree_cbor_hex` precedent at types.rs:129 is the same back-compat pattern). Wire-format pinning tests (`wire_format_*`) may need a fixture update — expected, not a regression.
- **Endpoint observed but never used.** If B2 never comes online again, the seed sits unused and is eventually superseded-or-idle; the supervisor's retry/liveness handles a dead route. No worse than any stale record.
- **Two feeds, one slot.** Step-1 and step-2 both write the `fleet` slot; LWW makes this order-independent and idempotent (recon §D). Already the intended design.

## Open questions for you (all have a recommendation above — just want your sign-off)

1. **Plaintext seed store** (decision 3) — OK, or do you want it sealed like the keytree? (I recommend plaintext; it's dialing metadata, not a secret, and matches `fleet_net.cbor`.)
2. **No TTL on seeds** (decision 4) — OK to rely on LWW-supersession + node_id stability, or do you want an expiry? (I recommend no TTL — YAGNI.)
3. **Bidirectional seeding** (decision 1) — seeding both directions via `Confirm` is free; any reason to make it P-learns-B2 only? (I recommend bidirectional.)
