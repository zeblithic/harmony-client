# ZEB-744 — Extract reachability record + multi-device LWW resolver → `harmony-reachability`

**Status:** Design (approved shape 2026-07-24; Decisions 1 & 2 approved by Jake)
**Ticket:** ZEB-744 (child of ZEB-571 item 7)
**Repos:** harmony (core) + harmony-client
**Scope choice:** Full extract (record + resolver), over resolver-only / re-scope-as-satisfied.

---

## 1. Context & motivation

ZEB-571's seam audit flagged the client's iroh-reachability primitive as net-new platform substrate that belongs in core. Premise-verification against the current trees (client `main` d4f79b51, core `main` cb05de9) by three read-only agents refined that:

- **A partial record twin already exists in core** — `harmony-discovery::AnnounceRecord` + `RoutingHint::Tunnel`. It carries the same node-id/relay/addrs quad as serialized forms, is identity-signed, and depends on neither iroh nor pkarr. But it is **wire-incompatible** with the client's record (postcard vs CBOR; extra `encryption_key`/`nonce`/`expires_at`; different signature), and the client's record is frozen on the wire (published to the pkarr DHT, signed over by the outer `PkarrRoutingRecord`). **Convergence is a network break and is out of scope.** The two records coexist in core, serving different consumers (harmony-node discovery vs the client's community-reachability system).
- **The genuinely net-new substrate is the resolver's multi-device dimension.** Core's `DiscoveryManager` is single-slot keyed by `IdentityHash`; the client's `ReachabilityResolver` is `(OwnerAddr, node_id)`-keyed, HLC-LWW, with per-source slots — no core analog.
- **Crate placement is de-risked.** The record stores serialized forms (no iroh types); the resolver dependency-inverts pkarr behind an async trait. The extracted crate needs **neither an iroh nor a pkarr dependency** → the two-pkarr type-unification trap (which forced item 1 to be a pkarr module) does not apply here.
- **Core has no `Hlc` and no `OwnerAddr`.** So the resolver is generic over the owner-key and clock types, and the inner-signature helpers (whose preimage binds the membership envelope's `actor`/`hlc`) stay client-side.

**Goal:** land the client's mature reachability record + a generic multi-device LWW resolver skeleton in a new core crate `harmony-reachability`, byte-preserving the wire format, with Harmony's app-specific policy injected from the client.

## 2. Goals / non-goals

**Goals**
- New core crate `harmony-reachability` housing the reachability record (byte-identical wire format) and a generic multi-device LWW resolver + fallback trait.
- Client rewired onto the core types; local `reachability_record.rs` / `reachability_resolver.rs` deleted; ~17 consumers repointed.
- Zero wire-format change: the five golden-hex vectors migrate into core as the acceptance gate and stay green client-side.

**Non-goals**
- Converging onto `harmony-discovery::AnnounceRecord` (wire-incompatible; a separate, network-migration-scale effort if ever wanted).
- Moving the inner-signature scheme, the butler-deposit protocol, the pkarr resolver adapter, the three-source arbitration policy, or the fleet/community bindings into core.
- Any change to the outer `PkarrRoutingRecord` (already in core `harmony-pkarr`).

## 3. Architecture overview

```
harmony-reachability (new core crate)
├── record        ReachabilityAnnouncePayload (byte-preserving move) + DelegateEndpoint
├── store         ReachabilityStore<Owner, Clock, Src>  — generic multi-device LWW map
│                 + async cache-then-fallback resolve, cooldowns, bounded refresh,
│                   injectable clock, stale-refresh, reverse-lookup, list queries
├── fallback      ReachabilityFallback async trait (pkarr inverted behind it)
└── hooks         SourcePolicy trait (rank / dial-vs-durable set) + optional
                  RefreshHook / LivenessHook trait seams (supervisor+liveness inverted)

harmony-client (rewired)
├── keeps         inner sign/verify, PkarrResolverAdapter (impl ReachabilityFallback),
│                 ReachabilitySource enum + SourcePolicy impl, butler-deposit protocol,
│                 community-CRDT binding, fleet/SAS paths, supervisor+liveness impls
└── ReachabilityResolver becomes a thin client wrapper over ReachabilityStore<OwnerAddr, HlcOrd, ReachabilitySource>
```

## 4. The record — pure byte-preserving move

`ReachabilityAnnouncePayload` moves verbatim into `harmony-reachability::record`. **Every byte-affecting property is preserved:**

- Field declaration order `nd, rl, da, ts, sg, bs, ba` (CBOR struct-as-map serializes in declaration order; ciborium 0.2 has no canonical writer).
- The `#[serde(rename)]` 2-char keys and the `serialize_bytes_as_bstr` / `deserialize_bytes_from_bstr` byte-string encoding for the `[u8;32]` / `[u8;64]` fields (a `serialize_bytes` → default-array flip is a silent wire break).
- `#[serde(default, skip_serializing_if = ...)]` elision on `bs` / `ba` — the *only* versioning mechanism; a butler-less blob must stay byte-identical to the pre-ZEB-418 legacy encoding.
- The `CanonicalPayload` sealed-trait impl and `canonical_payload_bytes` helper (used for signing/hashing).

The `serialize_bytes_as_bstr` / `deserialize_bytes_from_bstr` helpers and the `is_zero_u64` predicate move with the type (they live in `owner_state_types.rs` today; the crate gets its own copy, or a tiny shared serde-helpers module — decided at plan time to avoid pulling client crypto into core).

### Decision 1 — butler fields as a generic delegate concept

`bs` (`butler_set: Vec<ButlerSetEntry>`) and `ba` (`bs_at: u64`) are app-specific but are (a) wire fields and (b) folded into the inner-signature preimage. To keep byte-identity **and** keep the butler *protocol* client-side, core carries the fields concretely under a **generically-named type**:

- `ButlerSetEntry` → `DelegateEndpoint` (a reachable delegate a peer can be reached through), with the identical serde shape: `device_id: [u8;16]`, `iroh_endpoint_id: [u8;32]`, `device_ed25519_verify: [u8;32]`, `home_relay: String`, `pinned: bool` (same rename attrs → identical bytes).
- The record keeps `butler_set: Vec<DelegateEndpoint>` and `bs_at: u64` as fields (renamed field identifiers are fine; only the `#[serde(rename="bs"/"ba")]` wire keys matter).
- **What stays client-side:** everything that *does* something with delegates — the `IrohButlerDepositClient`, the `harmony/butler-deposit/v1` dial, seal-target selection, `freshest_butler_set_by_source`, the `ButlerSetResolve` wrappers.

Rejected alternatives: a generic `Ext` type-parameter on the record (over-engineered for one consumer; risks byte drift across instantiations) and a client-side wrapper that re-emits the flat map (fragile byte-identity, duplicates the codec).

### Inner signature stays client-side

`inner_signed_bytes` / `build_signed_payload` / `build_signed_payload_with_key` / `verify_inner_signature` remain in the client. Rationale: the preimage is the 8-field tuple `(nd, rl, da, ts, ac, hl, bs, ba)` where `ac` (actor `OwnerAddr`) and `hl` (`Hlc`) come from the membership envelope — both are client-only types with a byte-identity constraint, and core has no `Hlc`/`OwnerAddr`. The client's preimage builder reads the (now core-typed) record's fields; byte-identity is preserved because the field values and their encoding are unchanged. The inner sig is zero-filled on the pkarr path and only meaningful on the durable-CRDT membership path (fully client-side).

## 5. The resolver — generic skeleton, policy injected

Core `ReachabilityStore<Owner, Clock, Src>`:

- `Owner: Ord + Clone` — the owner key (client: `OwnerAddr`).
- `Clock: Ord + Clone` — the LWW ordering key. The client wraps `Hlc` in a newtype `HlcOrd(Hlc)` whose `Ord` matches `Hlc::is_strictly_newer_than` (tuple `(wall_ms, logical, device_id)`), since `Hlc` deliberately does not derive `Ord`.
- `Src` — the source discriminant (client: `ReachabilitySource { DurableCrdt, PkarrLive, FleetSibling }`), used to key the per-source cells.

**What the store owns (generic):**
- The `BTreeMap<(Owner, node_id), PerSourceCells<Src, Clock, Record>>` — the multi-device dimension (different devices of one owner coexist; only same `(owner, node_id, src)` updates compete via LWW).
- The `lww_newer` comparator: primary `Clock` order → tie-break greater `announced_at_ms` → tie-break lexicographically greater `node_id`; full equality is a no-op (byte-identical replay). **The ZEB-621 future-skew clamp** (`effective_announced_at_ms = min(announced_at_ms, now + FUTURE_SKEW_TOLERANCE_MS)`) is part of merge behavior and moves with the comparator.
- Cache-then-fallback async resolve (`ReachabilityFallback` on miss), per-owner refresh cooldowns, a bounded (`Semaphore`) background refresh fan-out, an injectable now-ms clock, stale-refresh, reverse `resolve_by_node_id`, and the `list_*` queries.

**What the client injects (policy — Decision 2):**
- `SourcePolicy`: `rank(&Src) -> u8` (durable > pkarr > fleet) and the dial-vs-durable set (`freshest` counts all sources; `durable_preferred` = durable-then-pkarr, excludes fleet). The `freshest` / `durable_preferred` selection logic is generic over this policy; the concrete ranks and the three variants are client-side.
- `ReachabilityFallback` concrete impl: `PkarrResolverAdapter` (queries pkarr relays, verifies the outer `PkarrRoutingRecord`, decodes the blob back to a record) — stays client-side, injected at boot.
- Refresh/liveness/supervisor hooks: `SupervisorHandle` (reconnect kicks) and `LivenessHandle` (telemetry) become trait seams (`RefreshHook`/`LivenessHook`) implemented client-side and injected, mirroring how `ReachabilityFallback` is already inverted.

The client's `ReachabilityResolver` becomes a thin wrapper: it holds a `ReachabilityStore<OwnerAddr, HlcOrd, ReachabilitySource>`, wires the injected policy + fallback + hooks, and exposes the same public methods the ~17 consumers already call (so consumer call sites change only their import path, not their call shape, wherever possible).

## 6. Dependency edges / crate manifest

`harmony-reachability/Cargo.toml` dependencies: `serde` (derive, alloc), `ciborium`, `serde_bytes`, `async-trait`, `tokio` (rt/sync/time — for `Semaphore`, `Instant`, `RwLock`), and `futures` if the fallback returns streams. **No `iroh`, no `pkarr`, no `harmony-owner`, no `harmony-identity`, no `harmony-crypto` concrete-type deps.** (If a generic `verify` helper over a caller-supplied verifying key lands, it pulls `ed25519-dalek`; default is to leave all signing client-side and keep the crate crypto-free.)

Because the crate takes no pkarr/iroh dep and no client-only concrete types, it rides the lockstep rev with zero pin-trap exposure. Added to the core workspace members + the client's lockstep pin block on the rewire PR.

## 7. Sequencing (two PRs, same as items 2–5)

1. **Core PR (harmony):** add the `harmony-reachability` crate — record (byte-preserving) + `DelegateEndpoint` + the generic `ReachabilityStore` + `ReachabilityFallback` + `SourcePolicy`/hook traits. Migrate the wire-format golden vectors as in-crate tests (the acceptance gate). Merge.
2. **Client PR (harmony-client):** bump the lockstep rev to the new core head; delete `reachability_record.rs` + `reachability_resolver.rs`; re-export/rewire the ~17 consumers onto the core record + the thin `ReachabilityResolver` wrapper; keep inner-sign/verify, `PkarrResolverAdapter`, the `ReachabilitySource`/`SourcePolicy`, the butler-deposit protocol, fleet/community bindings, and supervisor/liveness impls client-side; keep the client-side wire-format fixture tests green.

## 8. Byte-preservation invariants (acceptance gate)

The single non-negotiable invariant: `canonical_cbor_encode(ReachabilityAnnouncePayload)` stays byte-identical to the pinned golden hex (the `EXPECTED_LEGACY_HEX` constant at `reachability_record.rs:460`, and its integration-level twins). These tests migrate into `harmony-reachability` and must be green there, and the client-side copies must stay green after the rewire:

- `reachability_record.rs:458` `routing_blob_without_butler_set_is_wire_identical_to_legacy` (marked DO NOT REGENERATE).
- `tests/wire_format/reachability_announce_fixtures.rs` — `reachability_announce_payload_wire_bytes_pinned`, `..._with_butler_set_wire_bytes_pinned` (guards a `ts`↔`ba` swap), `signed_event_reachability_announce_wire_bytes_pinned`.
- `tests/wire_format/pkarr_routing_record_fixtures.rs` — `routing_blob_canonical_cbor_pinned` (asserts `ciborium::into_writer` == `canonical_cbor_encode`).

Plus the behavioral LWW anchors (must be preserved identically): higher-HLC-wins-per-device, lower-HLC-ignored, announced_at tie-break, node_id tie-break, same-source-keeps-LWW, pkarr-upgraded-by-older-durable (cross-slot), order-independent convergence, multi-device coexistence, the dial-vs-butler view precedence trio, and the `reverse_lookup_*_zeb704` cross-owner tie-break set.

**Suggested hardening (plan-time):** add a golden byte-vector for the inner-signature *preimage* (`inner_signed_bytes` output) before the move — today only sign/verify-symmetry + tamper tests exist; the preimage builder crosses the repo boundary (record fields in core, `ac`/`hl` client-side), so a preimage vector locks it.

## 9. Risks & mitigations

| Risk | Mitigation |
| --- | --- |
| Wire drift on the move (field reorder, rename, bstr→array flip, dropped skip predicate) | Golden vectors are the gate; they move first and must be green in the new crate before consumers rewire. HIGH→LOW iff pure move. |
| Inner-sig preimage drift (record fields now core-typed, `ac`/`hl` client-side) | Keep the preimage builder client-side reading core fields; add a preimage golden vector (§8 hardening). |
| Generic bloat / over-abstraction (YAGNI) | Two params + `Src` + one `SourcePolicy` trait + `ReachabilityFallback` — no speculative generality; concrete ranks/sources/fallback stay client-side. |
| ~17 consumer rewire churn | Keep the thin `ReachabilityResolver` wrapper's public method shapes identical so call sites change imports, not logic; subagent-driven task-per-cluster. |
| `serialize_bytes_as_bstr` helper duplication into core | Small self-contained serde helpers; copy into the crate (no client-crypto dep) — confirmed at plan time. |

## 10. Test strategy

- **Core:** the migrated wire-format golden vectors (record + butler variant + pkarr-blob equivalence), the LWW behavioral suite (generic over the test's chosen `Owner`/`Clock`/`Src`), fallback-on-miss, cooldown, reverse-lookup, stale-refresh — ported from `reachability_resolver.rs` tests. `no_std`-not-required (tokio dep); crate builds clean under `clippy --all-targets -D warnings`.
- **Client:** the existing wire-format fixture tests stay in place and green post-rewire (the strongest cross-repo byte-identity proof); the inner-sign/verify + butler-deposit + three-source policy tests stay client-side and green; the integration round-trips (`pkarr_community_fallback`, `iroh_zenoh_registration`, `two_engines_exchange_via_iroh_zenoh`) exercise the wrapper end-to-end.
- Both repos: fmt, `clippy --all-targets -D warnings`, scoped nextest per the harmony-app relink guidance (avoid the full ~97-binary relink per iteration).
