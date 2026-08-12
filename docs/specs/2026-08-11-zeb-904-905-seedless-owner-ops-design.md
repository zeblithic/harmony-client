# ZEB-904/905: seedless owner-ops decoupling + honest fleet-crypto degradation — design

**Status:** approved (Jake, 2026-08-11 — Option A, one PR)
**Tickets:** ZEB-905 (decouple local owner-state from fleet crypto) + ZEB-904 (silent
half-alive boot; honest degradation + observability)
**Related:** ZEB-492 (fleet-KeyTree distribution at pairing), ZEB-668 S5 (pinned tree /
FleetKeySet split), ZEB-836 (repairable-boot recovery surface), ZEB-801 (owner-not-loaded
message classification), ZEB-439 (restore from mnemonic)

## 1. Problem

A device whose owner state loads from disk but that holds **no master seed and no
fleet-KeyTree material** boots "half-alive": the Account panel renders the owner
(that read path goes straight to `owner_state.cbor`), but `start_node` silently
skips the entire owner wiring, so every owner-touching IPC fails with the false
message "no identity is set up on this device yet". Found on Jake's own long-lived
identity during 0.2.5 RC dogfooding.

## 2. Verified root cause (code census, 2026-08-11)

1. **The fleet gate is total.** In `start_node_inner`, the
   `if let Some(ref loaded) = owner_loaded` body consists of *nothing but* the inner
   `if let Some((kt, keys)) = fleet_crypto.clone()` branch (lib.rs ~5615–12467,
   ~6,850 lines). "Owner loaded, no fleet crypto" is therefore behaviorally
   identical to "no owner at all": ~100 pre-declared `*_opt`/`*_for_state`
   variables are only ever assigned inside that branch, so `crdt_state`,
   `community_registry`, `dm_outbox`, `hlc_tracker`, `channel_log_registry`,
   `tunnel_manager`, all pkarr handles, etc. all install as `None`.
2. **The no-material path is silent.** `fleet_crypto` construction warns when
   material exists but is unusable; when `master_seed` and `fleet_keytree` are both
   absent the `.as_ref().and_then(..)` short-circuits and nothing is logged. The
   inner and outer `else` arms of the gate are bare `None` — no logging.
3. **Most of the gated code never touches fleet keys.** Exhaustive census of
   `kt`/`keys` consumption inside the branch:
   - NEEDS-FLEET-KEYS: the fleet *dataset* engines (owner-state SyncEngine's
     publish/ingest, mint, notes, dm-inbox + its grant-unseal ingest ctx,
     community-device-intro, relay-hold, relay-optin, dm-outhold, fleet-net,
     owner-trust, owner-quorum, fleet-keys carrier + install task), and six
     surgical sites in the community/iroh band: reachability Case-D reconcile,
     startup Case-D reconcile, the routing-republish epoch-window closure, the
     iroh friend-handshake acceptor (friend-secret sealing), the friend/intro
     broker's `.with_owner_keytree`, and `NodeState.owner_keytree`
     (friend-token redeem).
   - OWNER-ONLY: everything else — `crdt_state`/tracker/content-store,
     `DmOutbox`, the community registry + per-community engines (their keys are
     per-community epoch keys read from the owner CRDT, **not** the fleet
     KeyTree), channel logs, address-book routing seed, pkarr publishers,
     tunnel manager, relay drivers, profile broadcast.
4. **This state is manufactured by live flows, not just legacy.**
   `install_joiner_state_inner` (pairing/persist.rs) always persists the joiner
   with `master_seed = None`, and when the inviter delivers no fleet material it
   *clears* the fleet-keytree slot rather than aborting. Any pairing from an
   inviter that sends no material produces exactly this state today. (There is no
   user-facing "wipe seed" action; the other sources are legacy pre-ZEB-492
   identities — Jake's case — and manual file deletion.)
5. **Persistence is already key-free.** The generic `FleetSyncEngine`'s
   `persist_direct` path (notify_dirty → debounced persist of
   `owner_state_crdt.cbor` + `state_root_replay.cbor`) never reads keys; the CRDT
   is plaintext canonical CBOR in the identity dir. Keys are consumed at exactly
   one encode choke point (`encode_root_wire`, shared by debounce publish,
   flush, shutdown publish, and the ZEB-707 root-serve reply) and one decode
   site (`handle_incoming_publish`).

**Threat-model confirmation (ZEB-905's open question):** local owner-state does
not require fleet keys. The at-rest CRDT is already plaintext in the identity
dir; fleet keys encrypt only the *replicated* wire blobs. The one genuinely
key-bound domain is the friend-secret domain (blobs sealed once under the pinned
epoch-0 tree) — that stays key-gated because the cryptography demands it, and it
defines the honest degradation line below.

## 3. Design

### 3.1 Capability model (the honest line)

With owner material but no fleet crypto, the device runs in **local-only mode**:

| Works fully | Honestly disabled (needs fleet KeyTree) |
|---|---|
| Owner-state CRDT ops (create/join community, settings, profile) | Fleet replication (all fleet dataset engines) |
| Communities: membership, channels, voting (per-community epoch keys) | Friend features: handshake accept, friend-token redeem, friend-secret unsealing, DM grant unseal |
| Local persistence + restart durability | Case-D friend-slot pkarr reconcile; epoch-window close/prune |
| Address book, tunnels, relays, profile broadcast | Enroll/backup (already seed-gated; unchanged) |

### 3.2 Keyless engine mode (`fleet_sync.rs`)

`FleetSyncConfig.keys` and `Ctx.keys` become `Option<FleetKeySet>`:

- `encode_root_wire`: `None` → return a typed "no keys" skip. Its four callers
  (debounce publish, `flush_now`, shutdown publish, root-serve reply) log at
  debug and continue — persist still runs unconditionally after the publish
  attempt, so `notify_dirty` durability is unchanged.
- `handle_incoming_publish`: `None` → drop inbound frame at debug level.
- All non-owner engine constructions pass `Some(keys)` — they are only built
  when fleet crypto exists, so their behavior is unchanged.
- `owner_state_sync::SyncEngine::new` passes the `Option` through.

The owner-state engine therefore **always runs** when an owner is loaded: same
handles, same `fence_owner_state_flush` semantics (flush = no-op publish +
real persist), no half-wired states.

### 3.3 Boot restructure (`lib.rs start_node_inner`)

Re-layer the branch without reordering code:

1. **Owner band (hoisted):** everything from the current gate entry through the
   owner-state SyncEngine + `sync_handles_opt` (device identifiers, signing
   keys, enrollment cert, CRDT/replay load, `crdt_state`, tracker, adopt floor,
   content store, DmOutbox, transport, SyncEngine-with-`Option`-keys) moves to
   the outer `if let Some(ref loaded) = owner_loaded` scope. `sync_handles_opt`
   is wired unconditionally — inbound frames drop in the engine, outbound never
   produces.
2. **Fleet band (stays gated):** the contiguous BOOT-PROBE 02→08 stretch (mint
   through fleet-keys carrier/install) wraps in
   `if let Some((kt, keys)) = fleet_crypto.clone()`. An `info!` breadcrumb
   ("skipping fleet engines: no fleet crypto") replaces the band when absent.
3. **Community/iroh band (hoisted, six kt-sites Option-threaded):** the
   registry, per-community spawn loop, healing pass, address-book seed, pkarr,
   tunnel/acceptor block, and drivers move under the owner gate. The six
   key-consuming sites take `fleet_crypto.as_ref()` and skip gracefully when
   absent (friend acceptor and broker keytree simply aren't wired; Case-D and
   epoch-window steps no-op at debug).
4. Cross-band data edges (e.g. the fleet-net snapshot refresh task consuming the
   fleet-net doc) are resolved by the compiler during the move: any layer-3
   consumer of a layer-2 output either already reads a pre-declared `Option`
   or gets an explicit `if let` skip. No brace-count refactoring — every move is
   anchored at use-sites and validated by scoped builds.

Behavior when fleet crypto **is** present must be byte-identical; the full test
suite is the regression net.

### 3.4 Observability + honest surface (ZEB-904)

1. **Boot `warn!`** in `fleet_crypto` construction when owner is present but
   both seed and material are absent: "owner present but no master seed and no
   fleet KeyTree material; fleet sync + friend features disabled for this
   session (restore the recovery phrase or re-pair to re-enable)".
2. **`StartNodeResponse.fleet_crypto_missing: bool`** (`fleetCryptoMissing`),
   true iff owner loaded && fleet crypto None. Serialization pin tests updated
   (key-count + camelCase).
3. **Frontend banner** (pattern: BackupReminderBanner): non-blocking,
   shown when `fleetCryptoMissing`; copy explains device sync + friends are
   paused, communities work normally, and points at restore-from-phrase
   (ZEB-439 flow already shipped in StartupRecoveryOptions). No change to
   `classifyOwnerIdentity` — the owner IS present and operational; this is not
   a new blocking identity state.
4. **Honest per-op errors:** keyless friend ops (`redeem_friend_token`, any
   NodeState.owner_keytree consumer) return "friend features need this
   device's sync keys — restore your recovery phrase to re-enable" instead of
   owner-not-loaded copy. Audit all `owner_keytree` consumers during
   implementation.

### 3.5 Non-goals

- No change to pairing's material delivery (ZEB-492 owns that); no new guard on
  the joiner seed-clear (it is the designed cert-only model).
- No fix for the 19 hard-fail `?` sidecar loads inside the boot path (noted for
  a follow-up ticket).
- No blocking modal: with the decoupling, "no fleet crypto" is not "can't come
  online".
- The `"hlc_tracker missing"`-style raw guard strings (create_community_impl)
  become unreachable for this scenario and are left as-is.

## 4. Invariants preserved

- **notify_dirty durability:** unchanged — persist path is key-free and still
  runs on every flush cycle (memory: owner-state mutations MUST persist via
  notify_dirty).
- **Friend-secret domain:** still sealed under the pinned tree; no plaintext
  fallback, no synthetic keys. A keyless device cannot mint or unseal friend
  secrets — enforced by construction, surfaced honestly.
- **Fleet epoch discipline (ZEB-668 S5):** untouched — the FleetKeySet
  install/prune machinery only exists when material exists.
- **Seeded-boot behavior:** byte-identical wiring; the restructure only changes
  *which scope* constructs each subsystem, not construction order or inputs.
- **ZEB-836/ZEB-801 surfaces:** untouched; `fleetCryptoMissing` is additive.

## 5. Testing

1. `fleet_sync.rs` unit: keys=None engine — notify_dirty persists both sidecars;
   publish/flush/shutdown produce no error and no wire bytes; inbound frame
   dropped; root-serve request answered gracefully (no panic, no reply).
2. Boot integration (seedless + no-material profile): `start_node` succeeds,
   `fleetCryptoMissing: true`, `crdt_state`/`community_registry`/`dm_outbox`
   wired; `create_community` succeeds; community + channel survive a
   stop/start cycle (persistence proof).
3. Seeded regression: full workspace suite (5,900+) — the restructure must not
   move a single existing test.
4. Serialization pins: StartNodeResponse key-count + camelCase name.
5. Frontend: vitest for banner render logic + DTO typing; tsc.
6. Honest-copy: keyless `redeem_friend_token` returns the friend-specific
   message, not owner-not-loaded.
