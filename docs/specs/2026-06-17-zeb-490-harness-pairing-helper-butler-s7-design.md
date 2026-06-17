# ZEB-490 — e2e-harness SAS device-pairing helper + co-located butler `s7` design

**Status:** approved (Jake, 2026-06-17) — Approach A.
**Branch:** `zeb-490-harness-pairing-helper-butler-s7` off `main` `fb31d9d6`.
**Parent:** ZEB-321. **Follow-up source:** ZEB-489 (butler tooling, MERGED `fb31d9d6`).

## 1. Goal

Give the e2e-harness its first **device-pairing** capability and use it to test the
**butler** deposit→recover durability path co-located — the butler counterpart of
ZEB-487's relay `s6_relay_deposit_recover`. Two deliverables:

1. A **reusable** SAS device-pairing helper (`pair_into_fleet`) + thin RPC wrappers,
   so any future scenario can put two local profiles into one fleet.
2. A scenario `s7_butler_deposit_recover` that pins a paired second device as butler,
   sends a DM while the primary is offline, and asserts HELD → RECV → CLEARED.

## 2. Context

ZEB-489 shipped the headless butler control + observability RPCs (`set_butler_pin`,
`get_butler_pin`, `get_butler_held`) but deliberately shipped **no** co-located
harness scenario: exercising the butler rung needs a recipient with a **second
enrolled device in one fleet**, which needs real SAS pairing (ZEB-446), and the
harness has zero pairing helpers today (`e2e-harness/src/driver.rs` = 219 helpers,
none touch pairing). This ticket builds that infrastructure and the first scenario.

## 3. Transport reality (read first — it shapes the whole design)

Pairing **discovery** rides **Zenoh**, not iroh. `ZenohPairingTransport`
(`src-tauri/src/pairing/zenoh_transport.rs`) publishes/subscribes on
`harmony/pairing/v2/lan/**` (`PAIRING_KEY_PREFIX`), pumped through the event loop's
`pairing_in_tx` (`event_loop.rs:3067`). Two co-located harness nodes therefore must
exchange LAN-scoped Zenoh publications for discovery to work — the **same transport
class** as the ZEB-466 co-located routing gap (profile-card broadcasts don't
propagate between co-located peers; s6's relay deposit didn't land co-located).

**Consequence:** s7 may characterize at the **pairing** step (boundary 1) before it
ever reaches the butler deposit. The "full co-located durability proof" is a *best
case*, not a guarantee. This is exactly why the design uses **layered characterize
fallbacks** (§7) — every boundary that establishes becomes a hard assertion; every
boundary that stalls characterizes + files a finding, and we still bank the reusable
helper. s7 also serves as the **empirical answer** to "does co-located Zenoh pairing
establish?" — a question no test answers today.

Note the asymmetry that still makes this worth running: *if* pairing establishes, the
butler **deposit** then dials the recipient's own enrolled device over **iroh**
(reachability from the friend-handshake device directory, ZEB-461) — the iroh-direct
path ZEB-485 proved works co-located — so the deposit boundary is more likely to
succeed than s6's zenoh-relay deposit did.

## 4. The SAS pairing state machine (what the helper drives)

`PairingState` (`src-tauri/src/pairing/types.rs`, serde `tag="kind"`, camelCase):

```text
Idle
Discovering      { role, ephemeralPubkeyHex, sessionId }
Discovered       { peers: [DiscoveredPeer] }
Handshaking      { peerSessionId, sasDigits }   // sasDigits = exactly 6 chars
WaitingPeerConfirm { peerSessionId }
Enrolling
Complete         { deviceIdHex }                 // 32-hex device-identity hash
Failed           { reason }
```

`DiscoveredPeer` (camelCase): `sessionId, role(inviter|joiner), displayName,
ownerIdIfInviter, ephemeralPubkeyHex, joinerEd25519VerifyHex (64-hex), seenAtUnix`.

The six curated RPCs (all `pub(crate) async fn *_inner(&Mutex<NodeState>)`,
registered in `api/rpc.rs:529-573`):

| RPC | Args | Effect |
|---|---|---|
| `start_inviter_pairing` | `DisplayNameArgs` | P loads owner_state + master_seed → `Discovering{role:inviter}` |
| `start_joiner_pairing`  | `DisplayNameArgs` | B2 generates a fresh ed25519 signing key → `Discovering{role:joiner}` |
| `select_pairing_peer`   | `PeerSessionIdArgs{peerSessionId}` | derive session key + SAS → `Handshaking{sasDigits}` |
| `confirm_pairing_sas`   | `EmptyArgs` | exchange encrypted Confirm; Inviter signs EnrollmentCert |
| `get_pairing_state`     | `EmptyArgs` | snapshot the current `PairingState` (watch-channel borrow) |
| `cancel_pairing`        | `EmptyArgs` | abort |

**Device-id subtlety (load-bearing):** `set_butler_pin` validates its `device_id`
against `NodeState.fleet_net_enrolled`, which is built from
`hex::encode(cert.device_pubkeys.classical.ed25519_verify)` = **64-hex**
(`lib.rs:4618`). That is **not** `Complete.deviceIdHex` (32-hex identity hash). The
64-hex value is the joiner's ed25519 verify key, surfaced as
`DiscoveredPeer.joinerEd25519VerifyHex`. So the helper captures
`joinerEd25519VerifyHex` from the **inviter's** `Discovered.peers` and returns it as
the value to pass to `set_butler_pin`.

## 5. Unit 1 — reusable pairing helper (`e2e-harness/src/driver.rs`)

Thin async wrappers (each one `node.rpc("<cmd>", json!({…}))`):
`start_inviter_pairing(node, display)`, `start_joiner_pairing(node, display)`,
`select_pairing_peer(node, peer_session_id)`, `confirm_pairing_sas(node)`,
`get_pairing_state(node) -> Value`, `cancel_pairing(node)`. Plus butler wrappers:
`set_butler_pin(node, device_id: Option<&str>)`, `get_butler_pin(node) -> Value`,
`get_butler_held(node) -> Vec<Value>`.

Orchestrator:

```rust
pub async fn pair_into_fleet(
    inviter: &NodeHandle,   // P — already-minted owner
    joiner:  &NodeHandle,   // B2 — fresh, unminted
    display: &str,
    deadline: Duration,
) -> Result<String>         // returns the joiner's 64-hex ed25519 verify key (the pin device_id)
```

Drives the state machine with bounded polling (mirrors `poll_join_iroh`):

1. `start_inviter_pairing(P, "{display}-P")`; `start_joiner_pairing(B2, "{display}-B2")`.
2. Poll `get_pairing_state(P)` until `kind == "discovered"` **and** `peers` contains a
   peer with `role == "joiner"`; capture that peer's `sessionId` (B2's session) and
   `joinerEd25519VerifyHex` (the return value). Concurrently poll `get_pairing_state(B2)`
   until it is `discovered` with an `inviter` peer; capture the inviter's `sessionId`.
3. `select_pairing_peer(P, b2_session_id)` **and** `select_pairing_peer(B2, p_session_id)`.
   (Both sides select; the state machine is symmetric — selecting establishes the
   pairwise session. Selecting from both sides is the headless analogue of each user
   clicking the other's row.)
4. Poll both until `kind == "handshaking"`; read `sasDigits` from each. **Assert the two
   `sasDigits` are equal** (the real SAS security property). If they differ → hard
   failure (a genuine bug, not a characterize case).
5. `confirm_pairing_sas(P)` and `confirm_pairing_sas(B2)`.
6. Poll both until `kind == "complete"` (B2 now enrolled). Return the captured
   `joinerEd25519VerifyHex`.

Every poll is deadline-bounded; on timeout return `Err` (the caller decides whether to
characterize). The helper never panics on timeout — it returns `Err` so the scenario's
boundary-1 fallback can fire.

## 6. Unit 2 — scenario `s7_butler_deposit_recover` (`e2e-harness/tests/e2e_two_node.rs`)

Setup: spawn **A** (minted sender) + **P** (minted primary) + **B2** (fresh, unminted —
B2 must NOT be independently minted; it acquires identity by enrolling into P's fleet).
A new `s7`-specific spawn helper (or a reuse of the existing per-node spawn) provides
the three handles; A and P are minted via the existing mint path, B2 is spawned only.

Flow:

1. `let b2_device = pair_into_fleet(&P, &B2, "s7", deadline)?;`
   — **Boundary 1.** On `Err` → `S7 FINDING: pairing did not establish co-located`;
   `run.mark_success(); return;`.
2. `set_butler_pin(&P, Some(&b2_device)).await?` — must succeed (B2 is genuinely
   enrolled, so the `fleet_net_enrolled` gate at `set_butler_pin_inner` lib.rs:43799
   accepts it). Assert `get_butler_pin(&P)` returns `pinnedDeviceId == b2_device`.
   **This is a hard assertion** — it proves pairing + enrolled-set + pin end to end,
   the guaranteed-value core even if later boundaries characterize.
3. A↔P friend handshake while both online (reuse s6's friend helpers) so A's device
   directory learns P's fleet devices incl. B2's reachability.
4. `P.kill()` (hard offline). `add_dm_space(&A, &p_owner)` → `a_space`;
   `send_dm(&A, &a_space, b"butler-durable-hello", "text/plain")`. Deposit fans out
   after `DEPOSIT_NOACK_WINDOWS = 2` (`butler_deposit.rs:105`).
5. **HELD — Boundary 2.** Poll `get_butler_held(&B2)` until an entry with
   `senderOwnerHex == a_owner`. On timeout →
   `S7 FINDING: butler deposit never landed on B2 co-located`; `mark_success; return;`.
   On success capture `spaceIdHex` / `messageCidHex` for the cleared check.
6. `P = P.relaunch()` — rehydrates, fleet-merges with B2, `apply_deposited_invite`
   bootstraps the DM Space (`dm_outbox.rs:2227`).
7. **RECV — Boundary 3.** Poll `read_dm_plaintext_any(&P, &[a_space])` until
   `"butler-durable-hello"` appears. On timeout →
   `S7 FINDING: recovery did not complete co-located`; `mark_success; return;`.
8. **CLEARED.** Poll `get_butler_held(&B2)` until the HELD entry's `ingestedByDevices`
   contains P's device id (the grow-only recovered/cleared signal). Hard assertion
   once RECV passed.

## 7. Error handling — layered characterize fallbacks

Three racy boundaries, each `eprintln!("S7 FINDING: …"); run.mark_success(); return;`
(the s6 pattern, `e2e_two_node.rs:1312`):

| Boundary | Stalls when | Fallback | If it passes |
|---|---|---|---|
| 1 — pairing | co-located Zenoh `lan/**` doesn't propagate (ZEB-466 class) | characterize | hard-assert pin + every later boundary |
| 2 — deposit | sender can't reach B2 / `DEPOSIT_NOACK_WINDOWS` exceeds the poll window | characterize | hard-assert RECV + CLEARED |
| 3 — recovery | relaunch ingest doesn't complete in the window | characterize | hard-assert CLEARED |

Worst case: banks the reusable helper + the boundary-1 finding. Mid case: helper +
pairing/pin proof + a deposit finding. Best case: the full HELD→RECV→CLEARED chain —
first co-located proof of the offline-at-create→deposit→recover durability path.
**Any characterized finding gets a Linear follow-up ticket**, exactly like s6→ZEB-488.

## 8. Testing & gating

- The harness is **not in CI** (too slow/racy), so this gates nothing — it is a
  local/manual proof tool. Run: build `harmony-app` (`cd src-tauri && cargo build
  --bin harmony-app`), then `cargo nextest run --locked --features e2e --release`
  (the `.config/nextest.toml` 120s slow-timeout already covers the multi-minute run).
- **Unit coverage** on the helper's pure SAS-match assertion logic where factorable
  (a small free fn `assert_sas_match(a: &str, b: &str) -> Result<()>` is unit-tested:
  equal → Ok, differ → Err).
- The new driver helpers are public API on the `e2e-harness` crate, so the standard
  `src-tauri` gates still run for hygiene (fmt / clippy `-p harmony-app --lib` /
  nextest `-p harmony-app --lib`), but the scenario body is `--features e2e` only.

## 9. File-touch map

| File | Change |
|---|---|
| `e2e-harness/src/driver.rs` | + 6 pairing wrappers, + 3 butler wrappers, + `pair_into_fleet`, + `assert_sas_match` (+ unit test) |
| `e2e-harness/tests/e2e_two_node.rs` | + `s7_butler_deposit_recover` + any `s7`-specific spawn helper for the unminted joiner |
| `docs/playbooks/e2e-two-agent-suite.md` | no change (Scenario D3 already added in ZEB-489; this is the co-located sibling, not cross-WAN) |

No `src-tauri/src` production code changes — all three pairing/butler RPCs already
exist in the curated surface. This is pure harness/test code.

## 10. Risks

1. **Co-located Zenoh pairing may not establish (boundary 1).** Mitigated by the
   fallback; the helper + empirical finding are guaranteed value. (§3)
2. **Both-sides-select symmetry.** If the state machine expects exactly one selector,
   selecting from both sides may double-handshake. Mitigation: the plan verifies the
   single- vs both-select contract against `state_machine.rs` before finalizing step 3
   of `pair_into_fleet`; fall back to inviter-only select if that is the contract.
3. **Unminted-joiner spawn.** The harness mint helpers mint every node; B2 must spawn
   *without* minting. Mitigation: the plan confirms a spawn-without-mint path exists
   (the joiner generates its key inside `start_joiner_pairing`, so no pre-mint identity
   is needed) and adds a minimal `spawn_unminted`-style helper if absent.
4. **`DEPOSIT_NOACK_WINDOWS` timing.** The deposit only fires after 2 backoff windows;
   boundary-2's poll window must exceed that. Mitigation: size the HELD poll deadline
   generously (≥ s6's 60s) and document it.

## 11. Scope boundary

Helper + one scenario. **Not** in scope: cross-WAN D3 live run (needs AVALON, on
hold), group-DM butler fan-out / multi-butler scenario (separate follow-up), wiring
the harness into CI, any production `src-tauri/src` change.
