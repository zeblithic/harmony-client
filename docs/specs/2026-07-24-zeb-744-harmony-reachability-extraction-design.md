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

**Resolver depth (decided 2026-07-24 after reading the full ~900-line resolver).** The resolver is ~80% Harmony-specific policy — the three-source `DurableCrdt`/`PkarrLive`/`FleetSibling` model is structural (woven through `ResolverSlots`, `freshest`, `durable_preferred`, `source_rank`, `freshest_across_owners`, `update_with_source`), and three client subsystems are integrated inside it (reconnect-supervisor kicks, peer-liveness telemetry, the `event_loop` generation counter), plus a full pkarr-refresh policy (`maybe_refresh_stale`: staleness gate, per-owner cooldowns, global semaphore, fleet-exclusion). All of it is concurrency-correct code with documented ZEB-620/621/622/627/643/704 fixes. Only ~150 lines are genuinely generic. **So we extract the record (full) + the clean kernel, and the Harmony resolver stays client-side as a consumer of the kernel** — real substrate lands in core, the hard-won policy code stays put (no rewrite, no regression risk).

**Goals**
- New core crate `harmony-reachability` housing (a) the reachability record (byte-identical wire format) and (b) a generic reachability kernel: the `lww_newer` LWW comparator, a `MultiDeviceMap<Owner, V>` newtype (the `(owner, node_id)` keying with owner-prefix range + reverse-by-node lookup), and the `ReachabilityFallback` async trait.
- Client rewired: the record's ~consumers repointed onto the core record; `ReachabilityResolver` (kept client-side) rebuilt on top of the core kernel; local `reachability_record.rs` deleted, `reachability_resolver.rs` slimmed to the Harmony policy.
- Zero wire-format change: the five golden-hex vectors migrate into core as the acceptance gate and stay green client-side.

**Non-goals**
- Converging onto `harmony-discovery::AnnounceRecord` (wire-incompatible; a separate, network-migration-scale effort if ever wanted).
- Moving into core: the inner-signature scheme, the butler-deposit protocol, the pkarr resolver adapter, the three-source arbitration policy (`ResolverSlots`/`freshest`/`durable_preferred`/`source_rank`), the supervisor/liveness/generation integration, the `maybe_refresh_stale` pkarr-refresh policy, or the fleet/community bindings. All stay client-side.
- Any change to the outer `PkarrRoutingRecord` (already in core `harmony-pkarr`).

## 3. Architecture overview

```
harmony-reachability (new core crate)
├── record        ReachabilityAnnouncePayload (byte-preserving move) + DelegateEndpoint
│                 + is_zero_u64, serde byte-string helpers, canonical CBOR encode
├── kernel        lww_newer<Clock, Rec>  — the LWW comparator (clock tuple →
│                   announced_at → node_id tie-breaks, generic over a clock + a
│                   ReachabilityRecord trait exposing node_id()/announced_at_ms())
│                 MultiDeviceMap<Owner, V>  — (owner, node_id) keying newtype:
│                   owner-prefix range query + reverse-by-node-id lookup
└── fallback      ReachabilityFallback async trait (pkarr inverted behind it)

harmony-client (rewired; ReachabilityResolver STAYS here)
├── record users  ~consumers repointed onto harmony_reachability::ReachabilityAnnouncePayload
├── keeps         inner sign/verify, butler accessors (fresh/durable_butler_set),
│                 reachability_freshness_check, PkarrResolverAdapter (impl the core
│                 ReachabilityFallback), ReachabilitySource + the 3-source arbitration
│                 (ResolverSlots/freshest/durable_preferred/source_rank), supervisor +
│                 liveness + generation, maybe_refresh_stale, community/fleet bindings
└── ReachabilityResolver  rebuilt on the core kernel: its BTreeMap becomes a
                  MultiDeviceMap<OwnerAddr, ResolverSlots>; same-source slot LWW calls
                  the core lww_newer; all policy above stays byte-for-byte as today
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

## 5. The resolver — extract the kernel, keep the policy client-side

Reading the full resolver (see §2, resolver depth) showed it is ~80% Harmony-specific policy. We extract only the genuinely-generic kernel; the `ReachabilityResolver` itself stays in the client and is rebuilt on top of the kernel.

**Kernel that moves to core (`harmony_reachability::kernel`):**

- `trait ReachabilityRecord { fn node_id(&self) -> [u8; 32]; fn announced_at_ms(&self) -> u64; }` — implemented by the core record (and by anything else that wants the kernel). Lets the comparator and map stay record-agnostic.
- `fn lww_newer<C: Ord, R: ReachabilityRecord>(prev_clock: &C, prev_rec: &R, next_clock: &C, next_rec: &R) -> bool` — the same-source LWW comparator: primary `C` order → tie-break greater `announced_at_ms()` → tie-break lexicographically greater `node_id()`; full equality is a no-op (byte-identical replay is not a change). Generic over the clock so the client passes its `Hlc` comparison tuple `(wall_ms, logical, device_id)` (via a thin `Ord` wrapper, since `Hlc` deliberately does not derive `Ord`).
- `struct MultiDeviceMap<Owner: Ord + Copy, V>` — a newtype over `BTreeMap<(Owner, [u8; 32]), V>` capturing the multi-device keying: `entry`/`get`/`remove` plus the two non-trivial helpers the resolver relies on — `range_owner(&self, owner) -> impl Iterator` (the `(owner, [0u8;32])..=(owner, [0xFF;32])` prefix scan) and `find_by_node_id(&self, &[u8;32]) -> impl Iterator` (the reverse scan). This is the reusable essence of "keyed by (owner, device) so a peer's devices coexist."
- `trait ReachabilityFallback` — moves verbatim (async, `resolve(&self, &Owner) -> Vec<Record>`); the concrete `PkarrResolverAdapter` stays client-side and is injected as today.

**What stays client-side (unchanged Harmony policy, on top of the kernel):**

- `ReachabilityResolver` and everything in it: the three-source `ResolverSlots{durable, pkarr, fleet}`, `ReachabilitySource`, `freshest`/`durable_preferred`/`source_rank`/`freshest_across_owners`, the future-skew clamp + `effective_announced_at_ms`, the supervisor kicks (`addr_key` before/after, `NewPeer`/`RecordChanged`), the liveness handle, the generation counter, `maybe_refresh_stale` (cooldowns + semaphore + fleet-exclusion), `seed_from_pkarr`, `resolve_async*`, and every `list_*`/`resolve*` method.
- The only mechanical change to `ReachabilityResolver`: its `inner: Arc<RwLock<BTreeMap<ResolverKey, ResolverSlots>>>` becomes `Arc<RwLock<MultiDeviceMap<OwnerAddr, ResolverSlots>>>`, its owner-range and reverse-lookup scans call the `MultiDeviceMap` helpers, and its per-slot `lww_newer` call delegates to the core `lww_newer`. All behavior — and all the ZEB-620/621/622/627/643/704 correctness invariants — stays byte-for-byte as today, so the existing resolver test suite is the regression gate.

This keeps the concurrency-correct policy code exactly where it is (no generic rewrite of code with documented TOCTOU/cooldown/generation/skew fixes) while still landing the reusable record + LWW/multi-device kernel + fallback trait in core.

## 6. Dependency edges / crate manifest

`harmony-reachability/Cargo.toml` dependencies: `serde` (derive, alloc), `ciborium`, `serde_bytes`, `async-trait` (for the `ReachabilityFallback` trait). **No `tokio`** (the `Semaphore`/`Instant`/`RwLock` machinery stays with the resolver, which stays client-side). **No `iroh`, no `pkarr`, no `harmony-owner`, no `harmony-identity`, no `harmony-crypto` concrete-type deps** (all signing stays client-side, keeping the crate crypto-free).

Because the crate takes no pkarr/iroh dep and no client-only concrete types, it rides the lockstep rev with zero pin-trap exposure. Added to the core workspace members + the client's lockstep pin block on the rewire PR.

## 7. Sequencing (two PRs, same as items 2–5)

1. **Core PR (harmony):** add the `harmony-reachability` crate — record (byte-preserving) + `DelegateEndpoint` + serde helpers + canonical encode; the kernel (`ReachabilityRecord` trait, `lww_newer`, `MultiDeviceMap`, `ReachabilityFallback`). Migrate the wire-format golden vectors + add kernel unit tests (comparator + map range/reverse). Merge.
2. **Client PR (harmony-client):** bump the lockstep rev to the new core head; **delete** `reachability_record.rs` and re-export/rewire its consumers onto `harmony_reachability::ReachabilityAnnouncePayload` (keeping the client-only helpers — inner-sign/verify, butler accessors, `reachability_freshness_check` — in a thin client module that re-exports the core record); **slim** `reachability_resolver.rs` — swap its `BTreeMap<ResolverKey, ResolverSlots>` for `MultiDeviceMap<OwnerAddr, ResolverSlots>`, delegate same-source LWW to the core `lww_newer`, keep all three-source/supervisor/liveness/generation/refresh policy; keep every client-side wire-format fixture test and the full resolver test suite green (the regression gate).

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
| Concurrency-correctness regression in the resolver | The resolver policy code is NOT rewritten — it stays client-side; only the backing map type + the `lww_newer` call site change. The full existing resolver test suite (LWW, dual-slot, TOCTOU, cooldown, generation, skew) stays as the regression gate. |
| Consumer rewire churn | Far smaller than a full extract: the resolver stays client-side so its ~8 dial/reconnect/telemetry consumers are UNTOUCHED. Only the record-type imports repoint (re-export shim keeps most call sites stable) + the resolver's internal map/comparator swap. |
| `serialize_bytes_as_bstr` helper duplication into core | Small self-contained serde helpers; copy into the crate (no client-crypto dep) — confirmed at plan time. |
| Kernel too thin to be worth a crate | Accepted trade (Jake, 2026-07-24): the record is the substantial reusable piece; the kernel (comparator + multi-device map + fallback trait) is modest but is the honest generic line. The alternative (heavy generic resolver) was rejected for correctness risk. |

## 10. Test strategy

- **Core:** the migrated wire-format golden vectors (record + butler variant + pkarr-blob equivalence) as the byte-identity acceptance gate; new kernel unit tests — `lww_newer` (clock order + `announced_at`/`node_id` tie-breaks + equality-no-op) over a tiny test record, and `MultiDeviceMap` (owner-prefix range returns only that owner's devices; reverse-by-node-id finds across owners; multi-device coexistence). Crate builds clean under `clippy --all-targets -D warnings`.
- **Client:** the existing wire-format fixture tests stay in place and green post-rewire (the strongest cross-repo byte-identity proof); **the full `reachability_resolver.rs` test suite stays client-side and green** — it is the regression gate proving the map/comparator swap preserved every LWW / dual-slot / TOCTOU / cooldown / generation / future-skew invariant; the inner-sign/verify + butler-deposit tests stay client-side; the integration round-trips (`pkarr_community_fallback`, `iroh_zenoh_registration`, `two_engines_exchange_via_iroh_zenoh`) exercise the whole path end-to-end.
- Both repos: fmt, `clippy --all-targets -D warnings`, scoped nextest per the harmony-app relink guidance (avoid the full ~97-binary relink per iteration).
