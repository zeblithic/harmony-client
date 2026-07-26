# Headless relay-rung deposit→recover tooling — design

**Date:** 2026-06-16
**Status:** Approved (design); implementation pending
**Author:** Koya (orchestrator instance)
**Ticket:** ZEB-487 (child of ZEB-321)
**Branch:** `zeb-487-headless-relay-rung-deposit-recover`, off `main` `6b2424a4`
**Related:** ZEB-483 (DM-invite deposit durability — MERGED, the durability this makes testable), ZEB-458 (community sealed-relay rung), ZEB-418 (butler rung — the fast-follow), ZEB-447 (two-agent E2E suite + playbook), ZEB-321 (cross-WAN umbrella, parent)

---

## 1. Context & motivation

ZEB-483 shipped the last *code* of the DM-over-iroh durability story: a DM-Space `DmInvite` now rides inside the sealed deposit payload, so a recipient who was **offline when the DM Space was created** can bootstrap the Space from a recovered deposit and the DM delivers (instead of rejecting at `verify_cidnotify_admission: SpaceNotFound`).

That headline scenario — *offline-at-create → deposit → recover* — has **never run**, not even co-located, because the deposit lands on a **rung** that does not exist in a bare two-node setup:

- the recipient's **butler** (ZEB-418): a second always-on device in the recipient's *own* owner-fleet, or
- a **community sealed-relay** (ZEB-458): any co-member of a shared community who has volunteered to relay.

Two things block a headless test today:

1. **No headless control** to stand a node up as a rung (the relay opt-in toggle exists only as a GUI Tauri command, not in the curated `serve` RPC allowlist).
2. **No headless observability** to prove a message travelled *via the deposit* rather than the live tunnel.

This design closes both gaps **relay-first** (Approach A), productizing the relay rung as real headless surface and writing the test that drives it. The butler rung (Approach B) is the fast-follow and reuses the same observability layer plus the existing `set_butler_pin` designation knob.

### What is NOT the problem

The deposit and recovery *machinery already exists and fires automatically* — this design adds **no** deposit/recover/verify logic:

- The **butler acceptor is always-on** for every node with a bound iroh endpoint (no enable flag).
- **Recovery is automatic**: an inbox-ingest sweep runs on startup and after every fleet merge (`dm_inbox_ingest.rs:285-305`), emitting the normal `dm-received`.
- ZEB-483 already wired `apply_deposited_invite` into **both** rung ingest paths (`ProdDmInboxIngestCtx` for butler, `ProdRelayIngestCtx::ingest_recovered` at `community_relay_prod.rs:361-419` for relay).

So the work is **surface + test orchestration**, not new transport.

## 2. Goals / non-goals

**Goals**

1. Expose the community-relay rung as real, headless, **production-quality** surface: opt-in control + held-deposit observability.
2. Make the ZEB-483 *offline-at-create → relay-deposit → recover* scenario **assertable** end-to-end across two real machines (a live Ildwyn ↔ AVALON ↔ Koya run) and in a deterministic single-machine harness test.
3. Prove delivery went **via the deposit** (not the tunnel) **by construction** — no new wire/event fields.

**Non-goals**

- The butler rung (Approach B) — separate fast-follow; this spec only ensures the observability surface generalizes to it.
- Any change to deposit candidacy, fan-out ordering, the sealed-blob crypto, or the recover/verify pipeline.
- An explicit `recovered: true` provenance marker on messages/events (explicitly rejected as YAGNI — see §5.3).
- A relay-operator GUI/admin panel (the new read RPC is the data layer; UI is future).

## 3. Grounding: how the relay rung works today (verified)

- **Deposit candidacy** is built in `push_deposit_candidate` (`dm_outbox.rs:1433-1478`): a `ButlerDepositRequest` carrying the rebuilt CidNotify (`build_cidnotify_packet_bytes`), the optional ZEB-483 invite (`build_invite_packet_bytes`, fail-closed), and the storage blob.
- **Fan-out is butler-first, relay-as-last-resort** (`dm_outbox.rs:2506-2568`): the relay rung is attempted only if the butler did not ack/was skipped. **A recipient with no butler routes straight to the relay** — this is how the test forces the relay path with no tricks.
- **Deposit only fires after the recipient looks unreachable** — `DEPOSIT_NOACK_WINDOWS = 2` (`butler_deposit.rs:105`). Tests must poll, not expect an instant deposit.
- **Relay hosting is per-community opt-in** via the fleet-replicated `RelayOptInDoc` (`community_relay_optin.rs`); opting in wakes the announce publisher so co-members can resolve this node as a relay (`set_community_relay_opt_in`, `lib.rs:43946`).
- **Held deposits** live in a fleet-replicated `RelayHoldDoc` (`community_relay_hold_crdt.rs`; entries `RelayHeldBlob`, `community_relay.rs:214`), held behind `Arc<Mutex<RelayHoldDoc>>` (the same Arc the relay-hold `FleetSyncEngine` owns, `community_relay_prod.rs:146`). A `held_for(recipient, …)` reader already exists (`community_relay_prod.rs:281`).
- **Recovery on reconnect**: the returning recipient pulls held blobs for communities it belongs to (`iroh_community_relay_acceptor.rs::handle_relay_pull_query`), ingests via `ProdRelayIngestCtx::ingest_recovered` (CAS-put → verify sender binding from `OwnerDeviceCache` → `apply_deposited_invite` → apply inbox → emit `dm-received` → ack), and the relay GCs the entry once all the recipient's enrolled devices ack.
- **Existing GUI IPCs** (to be promoted / extended): `set_community_relay_opt_in` (+ `_inner` `lib.rs:43922`), `get_community_relay_status → bool` (`lib.rs:44005`), `set_butler_pin` (+ `_inner` `lib.rs:43795`, the butler-designation knob for B).
- **Curated headless allowlist**: `api/rpc.rs` (~line 769, 42 commands); DM verbs present are `send_dm` (`:409`) and `read_dm_thread` (`:413`). No relay/butler/deposit verbs.

## 4. Design — the headless surface

Three verbs are added to the curated allowlist in `api/rpc.rs`. Each is backed by an extracted `*_impl(node_state, args) -> Result<…, String>` that **both** the existing Tauri command and the new headless RPC call — the established `connectivity_redeem_invite_iroh_impl` pattern (ZEB-447), so GUI and headless stay in lockstep and the `_inner` pure cores remain unit-testable.

| RPC (snake_case) | Args (camelCase JSON) | Returns | Backing |
|---|---|---|---|
| `set_community_relay_opt_in` | `{ communityIdHex: string, optedIn: bool }` | `null` (ok) | **promote**: extract `set_community_relay_opt_in_impl` from the existing command body (`lib.rs:43946`); it snapshots NodeState handles, calls `set_community_relay_opt_in_inner`, notifies/flushes the relay-optin sync engine, wakes the announce publisher. |
| `get_community_relay_status` | `{ communityIdHex: string }` | `bool` | **promote** as-is (opt-in check). Return type unchanged (`bool`) to avoid breaking the existing GUI caller. |
| `get_relay_held` | `{ communityIdHex?: string }` | `{ held: HeldEntryDto[] }` | **new** read-only over NodeState's relay-hold doc Arc; optional `communityIdHex` filters, absent = all. |

### 4.1 `HeldEntryDto`

```jsonc
// serde(rename_all = "camelCase") — keys MUST be camelCase (e2e key lesson)
{
  "senderOwnerHex":    "…",  // 32 hex (owner addr of the depositing sender)
  "recipientOwnerHex": "…",  // 32 hex (intended offline recipient)
  "communityIdHex":    "…",  // 32 hex (the community this relay holds it under)
  "contentIdHex":      "…",  // 64 hex — ContentId(sealed_blob); the relay's per-blob key
  "heldAtMs":          0,     // held_at.wall_ms — when the relay accepted the deposit
  "heldByDevice":      "…"    // 64-hex relay device id that accepted the deposit
}
```

`get_relay_held` snapshots `Arc<Mutex<RelayHoldDoc>>` (already a NodeState field — `relay_hold_doc`, `lib.rs:1043`, no new wiring) and maps each `RelayHoldEntry` (`community_relay_hold_crdt.rs:23`) into a `HeldEntryDto`.

**Correction vs. the original draft:** the DTO **cannot** carry `spaceIdHex` or a decrypted `messageCidHex` — the held blob is **sealed to the recipient's device key and is opaque to the relay**, and `RelayHoldEntry` stores no DM `space_id`. The relay sees only routing metadata: `sender_owner`, `recipient_owner`, `community_id`, `held_at`, `held_by`, and the sealed-blob content id (the entry's map key is `"{recipientOwnerHex}:{contentIdHex}"`). `contentIdHex` is the recipient's CAS content id for the held blob and uniquely identifies the entry. This does **not** weaken the HELD∧RECV∧CLEARED proof (§4.3): the relay-side assertions key on `(senderOwner, recipientOwner, contentId)` appear/disappear, and the recipient side reads the plaintext (with its `spaceId`) via `read_dm_thread`.

### 4.2 Headless NodeState access

The `*_impl` functions take the headless node-state accessor used by `serve` (the `NodeStateAccess`/managed-`NodeState` seam from ZEB-452), not `tauri::State`. The Tauri commands become thin wrappers that resolve their `tauri::State<Mutex<NodeState>>` and delegate to the same `_impl`. Error strings are preserved verbatim (the curated surface mirrors GUI error text).

### 4.3 Provenance by construction (no new fields)

The assertion that a delivered DM travelled **via the relay deposit** is established by the test *topology + observation sequence*, not a marker:

1. the recipient is **process-killed** before the send → the live tunnel physically cannot carry the message;
2. `get_relay_held` on the relay host shows the entry **while the recipient is offline** (deposit landed);
3. after the recipient relaunches, its `read_dm_thread` shows the plaintext (delivered on reconnect);
4. `get_relay_held` on the relay host then shows the entry **gone** (acked + GC'd — consumed via the pull/recover path).

This triple is airtight and needs zero changes to the message or `dm-received` payloads. An explicit `recovered` flag is therefore **out of scope**.

## 5. Design — the test scenario

### 5.1 Cross-WAN Stage-2 (live, agent-driven — the headline)

Three nodes on `xwan`-style throwaway profiles (the proven env recipe: `HARMONY_DISABLE_KEYCHAIN=1`, `HARMONY_PASSPHRASE=…`, `HARMONY_ZENOH_DISABLE_MULTICAST=1`, `HARMONY_RETICULUM_PORT=0`):

> **[Update, ZEB-809 / PR #558, 2026-07-26]** `HARMONY_ZENOH_DISABLE_MULTICAST` no longer exists. LAN scouting (multicast **and** gossip) is now off by default, so the dial-only behavior this recipe wanted needs no env var at all; setting the retired var is a harmless no-op. The inverse knob is `HARMONY_ZENOH_ENABLE_LAN_SCOUTING=1` (value must be exactly `1`), which re-enables stock zenoh LAN discovery and is intentionally risky.

- **Agent A = Ildwyn** — sender
- **Agent B = AVALON** — recipient (goes offline)
- **Agent R = Koya** — relay host (a distinct owner; only needs to be a community co-member)

**Setup (all online):**

1. A creates community **C**; B and R both join C (cross-WAN iroh first-contact).
2. A and B become **friends** (`generate_friend_token` → `redeem_friend_token` → both `status:"active"`). This populates **B's `OwnerDeviceCache` with A** — required so B can verify the recovered CidNotify sender binding. **A and B must NOT exchange a DM yet** — the DM Space must be absent on B so the deposited invite is what bootstraps it.
3. R opts in: `set_community_relay_opt_in {communityIdHex: C, optedIn: true}`; confirm `get_community_relay_status {C} == true`.

**Run:**

4. **B** kills its `serve` PID (real offline). → signal `OFFLINE`.
5. **A** `add_space {kind:"dm", name:"xwan-dm", members:[B]}` → `send_dm {spaceId, content, mimeType}`. The tunnel can't reach B; after the 2 no-ack windows the deposit fan-out skips the (absent) butler and **deposits to R's relay** for community C.
6. **R** polls `get_relay_held {C}` until an entry appears with `senderOwnerHex == A`, `recipientOwnerHex == B`. → signal `HELD` *(deposit-landed)*.
7. **B** relaunches the same `--profile` (rehydrates). On startup it pulls held blobs for C from R, ingests, `apply_deposited_invite` bootstraps the DM Space, the message applies, `dm-received` fires.
8. **B** polls `read_dm_thread {spaceId}` until A's plaintext appears (hex-decode `body`). → signal `RECV`.
9. **R** polls `get_relay_held {C}` until the entry is **gone**. → signal `CLEARED`.
10. **PASS** = `HELD` (while B offline) ∧ `RECV` (after reconnect) ∧ `CLEARED`. → `DONE Stage2 PASS`.

This is added to `docs/playbooks/e2e-two-agent-suite.md` as **Scenario D2** (the cross-WAN DM durability scenario), with the signal vocabulary above. The online cross-WAN DM (Stage 1, already posted to the coordination thread) is its prerequisite confidence-builder but is independent.

### 5.2 Single-machine harness test (deterministic regression)

In `e2e-harness`, same topology with three co-located `NodeHandle`s (sender, recipient, relay host — three distinct owners, three temp homes). Friendship via `redeem_friend_token` (populates `OwnerDeviceCache` co-located per ZEB-461); community join via the existing `poll_join_iroh` helper; relay host drives the new `set_community_relay_opt_in` RPC. Recipient is taken offline by `NodeHandle::kill()` (SIGKILL) and brought back by relaunching the same config/profile.

New harness helpers:

- spawn a **third** node (the existing `node.rs` spawn already supports N nodes — just construct three configs);
- a **kill → relaunch-same-profile** offline/online helper on `NodeHandle` (kill exists; add a `relaunch()` that reuses the same temp home + profile + port-rediscovery walk);
- a **`get_relay_held` poll helper** in `driver.rs` (mirrors `poll_until`).

Asserted form (`s6_relay_deposit_recover` or similar): HELD-while-offline → RECV-after-relaunch → CLEARED, the same triple.

**Fallback (characterize-not-assert):** co-located relay resolve/dial is unproven and *may* hit the ZEB-466-class "owner-global/community topic doesn't route between co-located peers" gap (relay resolve rides the community registry + iroh dial, so it likely works, but it is a risk). If the deposit never lands or the pull never establishes co-located, the test **downgrades to characterization** — log `held=…/recovered=…` and `#[ignore]` with a FINDING block (a new finding ticket), exactly the posture S2/S5 already take. The cross-WAN live run (§5.1) remains the real proof regardless.

## 6. Error handling & edge cases

- **Node not started / engine absent:** the promoted RPCs surface the existing `_impl` errors verbatim (e.g. `"set_community_relay_opt_in: relay-optin not running (node not started)"`). `get_relay_held` returns the same shape of error if the relay-hold doc handle is absent.
- **Opt-in before join:** opting a node in to relay for a community it has not joined is a user/test error; the announce simply won't resolve to any co-member. The scenario joins C before opting in. No new guard added (matches current GUI behavior).
- **Deposit timing:** `get_relay_held` will return empty until the 2 no-ack windows elapse and the relay deposit completes — tests **poll with a generous budget** (≥ the no-ack window × backoff), never a fixed sleep.
- **Friendship-before-offline ordering:** if B goes offline before the friend handshake completes, recovery fails closed at sender-binding verification (B has no cached A) and the message stays pending — the test sequences friendship first and asserts `active` both ways before step 4.
- **Idempotent recovery:** the held entry is keyed `(sender_owner, space_id, message_cid)`; a duplicated pull is a no-op. `get_relay_held` may briefly show an entry post-recovery until the GC sweep + all-device-ack completes — step 9 **polls** for clearance rather than asserting immediacy.
- **`get_relay_held` is read-only and never decrypts** — it cannot leak plaintext; it exposes only routing metadata already in the held entry. (A real relay operator is explicitly an untrusted store-and-forward party.)

## 7. Scope boundary & the Approach-B fast-follow

This spec delivers **Approach A (relay rung)** only. **Approach B (butler rung)** is the next ticket and is deliberately enabled by this design:

- B reuses the **same observability pattern** — a `get_butler_held` read over the butler inbox doc, mirroring `get_relay_held`.
- B reuses the existing **`set_butler_pin`** command (`lib.rs:43826`) as its designation knob (promoted to headless the same way), so the recipient's second always-on device advertises as the butler in the pkarr routing record.
- B's scenario needs the recipient to run a **second paired device** (its butler) via the ZEB-446 pairing RPCs — heavier topology, hence the fast-follow.

No code in this spec is throwaway with respect to B.

## 8. File-touch map

- `src-tauri/src/api/rpc.rs` — add the three verbs to the registry + allowlist; define `HeldEntryDto`; wire each to its `*_impl`.
- `src-tauri/src/lib.rs` — extract `set_community_relay_opt_in_impl` and `get_community_relay_status_impl` from the existing commands (taking `&Mutex<NodeState>`, the `connectivity_redeem_invite_iroh_impl` shape); thin the Tauri commands to wrappers; add `get_relay_held_impl` reading the existing `relay_hold_doc` NodeState field (`lib.rs:1043`, already present — no new handle needed).
- The `RelayHoldEntry → HeldEntryDto` pure mapper lives next to the DTO (in `api/rpc.rs` or a small `relay_held_dto` module) so it is unit-testable without a live `NodeState`.
- `e2e-harness/src/node.rs` — `relaunch()` (offline→online same profile).
- `e2e-harness/src/driver.rs` — `get_relay_held` poll helper.
- `e2e-harness/tests/e2e_two_node.rs` (or a new `e2e_three_node.rs`) — the `s6_relay_deposit_recover` test (asserted, with the characterize fallback).
- `docs/playbooks/e2e-two-agent-suite.md` — Scenario D2 (cross-WAN DM durability).

## 9. Testing strategy

- **Unit:** `set_community_relay_opt_in_inner` already has LWW tests (`community_relay_optin.rs`); add `*_impl` tests for the NodeState snapshot + the `HeldEntryDto` mapping (a `RelayHoldDoc` with two held blobs → two DTOs with correct hex + `heldAtMs`; empty doc → empty list; `communityIdHex` filter).
- **Integration (harness):** `s6_relay_deposit_recover` per §5.2 (asserted; characterize fallback on co-located routing failure).
- **Live cross-WAN:** Scenario D2 per §5.1, run by the agent trio, artifacts attached to the tracking ticket / ZEB-321.
- **Gates:** `cargo fmt --all`; `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked -p harmony-app --lib --features test-fixtures`; harness via `cd e2e-harness && cargo nextest run --features e2e`. CI runs `--all-targets`.

## 10. Out of scope / future

- Approach B (butler rung) — fast-follow ticket.
- Relay-operator GUI/admin surface over `get_relay_held`.
- Any `recovered`-provenance field (rejected, §4.3).
- Relay capacity/economics, multi-region relays (ZEB-321 Phase 5 governance).
