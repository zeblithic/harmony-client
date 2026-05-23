# ZEB-321 Phase 2: cross-WAN first-contact discovery + small-private-group bootstrap

**Linear:** [ZEB-321](https://linear.app/zeblith/issue/ZEB-321) (umbrella; this is Phase 2 of N).
**Branch:** TBD — will create off `origin/main` `589d55d` once this spec lands.
**Status:** Brainstormed 2026-05-23 against Gemini Deep Research; approved 2026-05-23 (Jake).
**Phasing:** Phase 2 of the multi-phase ZEB-321 initiative. Cross-repo: changes land in `harmony` (new `harmony-pkarr` crate) and `harmony-client` (policy modules + IPCs + UX).
**Spec inputs:**
- Phase 1 spec: `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md`
- Research prompt: `docs/research/2026-05-23-zeb-321-phase2-discovery-research-prompt.md` (commit `589d55d`)
- Research report: `docs/research/2026-05-23-zeb-321-phase2-discovery-research-report.md` (commit `589d55d`)

---

## 1. Goal

Make cross-WAN **first-contact** work for three concrete use cases that Phase 1 does not address:

- **A — Invite redemption:** when alice sends bob an invite URL (`harmony://invite/{base64url}`) over an out-of-band channel (Signal, email, QR), bob's harmony-client can find alice's current iroh routing data and deliver the existing `CommunityInviteSigned` packet to her cross-WAN. Today the redemption packet rides Reticulum, which needs a fixed WAN-entry gateway and breaks behind residential CGNAT.
- **B — Opt-in identity-keyed discovery:** alice can flip a per-device toggle to "make me discoverable" so anyone who already knows her harmony identity hash can find her current iroh routing without an invite link. Default off; explicit opt-in.
- **C — In-community reconnection fallback:** when Phase 1's in-community `ReachabilityResolver` has no fresh entry for a peer (e.g., both peers moved networks at the same time so neither's reachability announce reached the other), automatically fall back to a community-secret-keyed pkarr lookup. Repairs Phase 1's structural circularity where the discovery channel runs over the connection it's trying to repair.

All three are built on a single new primitive: **pkarr publish/resolve via HTTP-relay** to the BitTorrent Mainline DHT, with HKDF-derived ephemeral Ed25519 keys differing only in their derivation inputs per case.

## 2. Background

### 2.1 What Phase 1 shipped (recap)

Phase 1 (merged at `b082e66` on 2026-05-23 via PR #157) shipped:

- `iroh::Endpoint` wrapper with OS-keychain `SecretKey` persistence (`iroh_endpoint.rs`).
- ALPN registry constant `harmony/zenoh/v1` (`iroh_alpn` module).
- Zenoh-over-iroh custom transport (`zenoh_iroh_link.rs`, `zenoh_iroh_transport.rs`) implementing zenoh's `LinkUnicastTrait` + `LinkManagerUnicastTrait`.
- `ReachabilityAnnounce` CRDT event (`community_membership.rs`, kind `kd="rch"`) with verify rules RCH1-RCH5 (outer sig, inner identity sig, actor binding, ±30min skew, current-member).
- LWW projection via HLC → `announced_at_ms` → lex `iroh_node_id` tie-break.
- `ReachabilityResolver` (`reachability_resolver.rs`) — composite-keyed `BTreeMap<(OwnerAddr, [u8;32]), ResolverEntry>` for multi-device users.
- `ReachabilityPublisher` (`reachability_publisher.rs`) — startup + network-change (debounced 2s) + 60-min idle + force-republish.
- 3 Tauri IPCs (`connectivity_get_my_reachability`, `connectivity_list_known_reachability`, `connectivity_force_republish`) + `connectivity-reachability-changed` event.
- Diagnostics panel UI showing current reachability state.

**Net effect:** two devices already in the same harmony community auto-discover each other's iroh routing through the community-state CRDT and successfully open zenoh-over-iroh sessions cross-WAN. Phase 1's two-process integration test exercises this end-to-end through DERP.

### 2.2 The Phase 2 gap (first-contact)

Phase 1 makes within-community routing work. It does **not** answer: *how does the first connection get established?* Specifically:

- The existing `community_invite.rs` module (2240 LOC, shipped under ZEB-217 Sub-C) generates URL-form invites containing the inviter's identity_pub, community state snapshot, and an authorization token. The URL is shared out-of-band (Signal, email, QR). Today the redemption counter-sig flow rides Reticulum, which is mesh-routable but needs a fixed WAN-entry gateway. That works on LAN-bridged Reticulum but fails across most residential WAN paths.
- ZEB-218 (Sub-D, library-federated directory) shipped public-community discovery — users add a library, browse its catalog, request to join. Public communities are solved.
- Private 2-person or small-group bootstrap (alice and bob in different countries) has no path today. Phase 2 is exactly that.

The Phase 1 spec anticipated this: "Cross-community first-contact (pkarr + civic registry), liveness/rebinding protocol, mobile push-wake architecture, and community-operated relays are all out-of-scope for Phase 1 — they ship in later phases." Phase 2 is the "pkarr + civic registry" pillar.

### 2.3 Research conclusions feeding Phase 2

The Gemini Deep Research report (`docs/research/2026-05-23-zeb-321-phase2-discovery-research-report.md`) converged on the **Cipher-Swarm (Ephemeral Pkarr Bootstrap)** pattern as the right primitive for private-group first-contact:

- HKDF-derived rotating Ed25519 keys publish iroh routing data to Mainline DHT via iroh's pkarr subsystem.
- To any external observer or DHT crawler, the records manifest as random cryptographic noise; the social edge does not leak.
- Recipients independently derive the same epoch keys and resolve.
- No new central infrastructure (Mainline DHT is the substrate; HTTP-relays are stateless proxies).

The report also concluded:
- HTTP-relay clients are mandatory for mobile (battery + carrier-NAT constraints); Phase 2 picks HTTP-relay-only for both desktop and mobile (Phase 3+ can add embedded-DHT if needed).
- pkarr write reliability from cellular CGNAT is an empirical unknown (Open Question #1 in the report); Phase 2 ships with one HTTP-relay backend and documents the open question for prototype validation.

Phase 2 extends the report's recommendation with two harmony-specific design moves not in the report:
- Use the case-C HKDF input `EpochKey` (already present in ZEB-249's invite epoch system) so in-community pkarr fallback is keyed by a secret only community members hold. This naturally restricts case C to members-only without any new permission machinery.
- Use the case-A HKDF input `invite_token.sig` (the inviter's pre-existing Ed25519 signature inside every invite URL) so the keying secret is already exchanged with no new URL field.

## 3. Architecture overview

```text
                        ┌───────── BitTorrent Mainline DHT ─────────┐
                        │       (~10M nodes; BEP44 signed records)  │
                        └───────────────────▲──┬─────────────────────┘
                                            │  │
                                       HTTP-relay (i.q8.fyi extension +
                                       community-hosted, civic pattern)
                                            │  │
                ┌───────── alice publishes ──┘  └── bob resolves ─────┐
                │                                                       │
   ┌────────────┴────────────┐                       ┌──────────────────┴───────┐
   │ harmony-client (Alice)  │                       │ harmony-client (Bob)     │
   │                         │                       │                          │
   │ Case A: while invite    │                       │ Case A: redeem_invite    │
   │   pending → publish     │                       │   → derive HKDF key      │
   │   HKDF(invite_sig,      │                       │   → query pkarr-relay    │
   │   epoch) → BEP44(iroh   │                       │   → open iroh conn       │
   │   routing + inner sig)  │                       │   → send Invite-Signed   │
   │                         │                       │                          │
   │ Case B (opt-in): always │                       │ Case B: discover_identity│
   │   publish HKDF(owner_pub│                       │   → derive HKDF key      │
   │   , epoch) → BEP44      │                       │   → query pkarr-relay    │
   │                         │                       │                          │
   │ Case C: per community,  │                       │ Case C: Phase 1 resolver │
   │   publish HKDF(EpochKey │                       │   cache miss → pkarr     │
   │   ‖ own_pub, epoch)     │                       │   fallback → fill map    │
   └────────────┬────────────┘                       └──────────────┬───────────┘
                │                                                    │
                └────────────── iroh QUIC + DERP ─────────────────────┘
                              (Phase 1 transport reused
                               unchanged once peers meet)
```

The pkarr primitive is the only new transport-layer piece. Once it produces a NodeId + routing tuple, everything downstream is unchanged Phase 1 (iroh connect + zenoh session over the existing `harmony/zenoh/v1` ALPN + existing community_invite or sync flows).

## 4. Crate & module placement

### 4.1 New harmony-core crate: `harmony-pkarr`

```
harmony/crates/harmony-pkarr/
├─ Cargo.toml                       # new workspace deps: pkarr, hkdf, sha2, ed25519-dalek
├─ src/
│  ├─ lib.rs                        # public API surface
│  ├─ record.rs                     # PkarrRoutingRecord (BEP44 payload + inner sig)
│  ├─ derive.rs                     # HKDF key derivation per case
│  ├─ publisher.rs                  # PkarrPublisher: publish(key, record) via HTTP relay rotation
│  ├─ resolver.rs                   # PkarrResolver: resolve(key) → Option<PkarrRoutingRecord>, in-memory LRU
│  ├─ relay.rs                      # Relay client (HTTP transport, rotation, cooldown, retry)
│  ├─ epoch.rs                      # Epoch math (week-aligned, ±1 tolerance window)
│  └─ testing.rs                    # #[cfg(any(test, feature = "test-fixtures"))] mock relay server
```

Crate **is transport-agnostic at the value layer.** `PkarrRoutingRecord` carries an opaque `routing_blob: Vec<u8>`. harmony-pkarr does NOT depend on iroh, zenoh, or any harmony-client surface. harmony-client wraps the blob with iroh-specific routing data.

### 4.2 harmony-client policy modules

```
src-tauri/src/
├─ pkarr_invite_publisher.rs        # case A: lifecycle tied to active invites
├─ pkarr_identity_publisher.rs      # case B: lifecycle tied to discoverability toggle
├─ pkarr_community_publisher.rs     # case C: lifecycle tied to community membership
├─ pkarr_resolver_adapter.rs        # wraps harmony_pkarr::PkarrResolver; plugs into Phase 1's
│                                   # ReachabilityResolver as a cache-miss source
└─ pkarr_settings.rs                # persisted preference for case B (file-backed via Tauri AppData)
```

Each policy module owns its trigger logic and registers/unregisters active publications with a shared `harmony_pkarr::PkarrPublisher` instance held in `NodeState`.

### 4.3 Phase 1 surgical change

`src-tauri/src/reachability_resolver.rs` gains one additive field and one method:

```rust
pub struct ReachabilityResolver {
    // ... existing fields ...

    /// Phase 2: cache-miss fallback source. When resolve() finds no
    /// fresh entry in the in-memory map, await this fallback (if set).
    /// Phase 1 default: None (current behavior unchanged).
    fallback_source: Option<Arc<dyn ReachabilityFallback>>,
}

#[async_trait]
pub trait ReachabilityFallback: Send + Sync {
    async fn resolve(
        &self,
        addr: &OwnerAddr,
    ) -> Vec<ReachabilityAnnouncePayload>;
}
```

`PkarrResolverAdapter` (case C policy) implements `ReachabilityFallback`. Wired at boot in `lib.rs`. Phase 1 tests pass unchanged because the default `fallback_source: None` preserves the original behavior; new Phase 2 tests exercise the adapter path.

### 4.4 Cross-repo coordination

Phase 2 ships as two sequenced PRs:

| PR | Repo | Scope | Approx LOC |
|---|---|---|---|
| 1 | `harmony` | New `harmony-pkarr` crate. No changes to existing crates. Adds `pkarr` workspace dep. | 800-1200 |
| 2 | `harmony-client` | Pins `harmony-pkarr` to PR 1's merge commit SHA. Adds 5 new policy modules + Phase 1 resolver extension + 5 new IPCs + 3 events + 3 UX changes. | 1800-2500 |

PR 1 must merge first. PR 2's CI fails until PR 1's commit is reachable from `harmony` main and the cargo lockfile is updated.

## 5. Wire format

### 5.1 PkarrRoutingRecord (BEP44 inner payload)

The opaque payload bytes inside the BEP44 envelope. Target ≤ 1000 bytes (BEP44 hard limit); typical ~280 bytes.

```rust
// crates/harmony-pkarr/src/record.rs
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PkarrRoutingRecord {
    /// Opaque routing blob. harmony-client encodes iroh NodeId + relay URL +
    /// direct addresses here, using its own canonical CBOR.
    /// harmony-pkarr treats this as opaque bytes.
    #[serde(rename = "rd", with = "serde_bytes")]
    pub routing_blob: Vec<u8>,

    /// Harmony identity_pub (64 bytes = X25519_pub(32) ‖ Ed25519_pub(32))
    /// of the OWNER whose routing this is. Bound to routing_blob via
    /// inner_sig so a record can't be re-attributed to a different identity.
    #[serde(rename = "ip", with = "serde_bytes")]
    pub harmony_identity_pub: [u8; 64],

    /// Wall-clock ms at publication. Used by resolver to pick the freshest
    /// of multiple records under the same key (multi-device LWW).
    /// Mirrors Phase 1 ReachabilityAnnouncePayload.announced_at_ms.
    #[serde(rename = "at")]
    pub announced_at_ms: u64,

    /// Ed25519 sig over canonical-CBOR((routing_blob, harmony_identity_pub,
    /// announced_at_ms)), computed with the publisher's harmony identity
    /// Ed25519 key (the last 32 bytes of harmony_identity_pub).
    /// NOT the BEP44 outer sig (which uses the ephemeral pkarr key).
    #[serde(rename = "sg", with = "serde_bytes")]
    pub inner_sig: [u8; 64],
}
```

Field key convention: 2-char keys throughout, mirroring Phase 1 and the existing community CRDT envelope. Same-length-keys invariant preserved at each nesting level.

### 5.2 BEP44 envelope

The `pkarr` crate wraps `PkarrRoutingRecord` (CBOR-encoded) in its standard BEP44 envelope:

- `seq`: monotonically increasing sequence number (we use `announced_at_ms` mod 2^32 to stay within BEP44's u32 limit; ties resolved by `announced_at_ms`).
- `salt`: empty (we don't use BEP44's salt slot; HKDF-derived ephemeral key already provides the discriminator).
- `sig`: Ed25519 sig over `(seq ‖ salt ‖ payload)` using the ephemeral pkarr key.
- `payload`: CBOR-encoded `PkarrRoutingRecord`.

### 5.3 HKDF key derivation

Single function in `harmony-pkarr/src/derive.rs`:

```rust
pub fn derive_ephemeral_key(
    case: PkarrCase, // A / B / C
    ikm: &[u8],      // case-specific input key material
    info: &[u8],     // case-specific binding context (e.g., epoch_id bytes)
) -> ed25519_dalek::SigningKey {
    let salt = match case {
        PkarrCase::Invite     => b"harmony.pkarr.v1.invite",
        PkarrCase::Identity   => b"harmony.pkarr.v1.identity",
        PkarrCase::Community  => b"harmony.pkarr.v1.community",
    };
    let hkdf = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut seed = [0u8; 32];
    hkdf.expand(info, &mut seed)
        .expect("HKDF-SHA256 32-byte output fits");
    let key = ed25519_dalek::SigningKey::from_bytes(&seed);
    seed.zeroize();
    key
}
```

The `harmony.pkarr.v1.*` salts version the entire scheme — a future v2 (e.g., switching to BLAKE3) bumps these without re-deriving v1 records' keys.

Per-case derivation inputs:

| Case | `ikm` (input key material) | `info` (context binding) | Resulting key visibility |
|---|---|---|---|
| **A — invite** | `invite_token.sig` (64 B; already in invite URL) | `epoch_id_bytes` (8 B, big-endian u64) | Only URL holders can derive. |
| **B — identity** | `owner_identity_pub` (64 B = X25519 ‖ Ed25519) | `epoch_id_bytes` (8 B, big-endian u64) | Anyone with `owner_identity_pub` can derive (publication is opt-in). |
| **C — community** | `EpochKey` (32 B; ZEB-249 community per-epoch shared secret) | `member_identity_pub` (64 B) ‖ `epoch_id_bytes` (8 B) | Community members can derive any other member's key. |

### 5.4 Epoch math

```rust
const EPOCH_DURATION_MS: u64 = 7 * 86_400_000; // 1 week

pub fn current_epoch_id(now_ms: u64) -> u64 {
    now_ms / EPOCH_DURATION_MS
}

pub fn epoch_tolerance_window(now_ms: u64) -> [u64; 3] {
    let e = current_epoch_id(now_ms);
    [e.saturating_sub(1), e, e.saturating_add(1)]
}
```

- Publisher writes for `epoch_id` at `epoch_start + 30min` (post-rollover safety margin) and again at `epoch_start + 3.5d` (DHT-TTL refresh).
- Resolver queries all three keys in `epoch_tolerance_window(now)` in parallel, takes the freshest valid record by `announced_at_ms`.

## 6. Publication lifecycles

A single `PkarrPublisher` instance lives in `NodeState`. The three policy modules register/deregister "active publications" with it. Each active publication is `(key: VerifyingKey, record_builder: Fn() -> PkarrRoutingRecord, next_republish_at: Instant)`.

The publisher's background task wakes on the soonest `next_republish_at` across all active publications.

### 6.1 Case A: invite-redemption

```rust
// pkarr_invite_publisher.rs (sketch)
struct ActiveInvitePublication {
    invite_id: SpaceId,
    derived_key: VerifyingKey,
    expires_at_ms: Option<u64>,
}

// Triggered after generate_invite succeeds:
fn register_invite(&self, invite: &CommunityInvitePayload, token_sig: [u8; 64]) {
    let epoch_id = current_epoch_id(now_ms());
    let key = derive_ephemeral_key(
        PkarrCase::Invite,
        &token_sig,
        &epoch_id.to_be_bytes(),
    );
    // ... register with shared PkarrPublisher with appropriate
    //     next_republish_at and record builder ...
}

// Triggered when invite is consumed OR expires OR user revokes:
fn unregister_invite(&self, invite_id: SpaceId) { /* ... */ }
```

Consumption detection: subscribe to `community-state-changed` event; check if any new member's `joined_via_invite_id` matches. On detect, unregister.

### 6.2 Case B: opt-in identity-keyed

```rust
// pkarr_identity_publisher.rs (sketch)
fn on_discoverable_toggled(&self, enabled: bool) {
    if enabled {
        let epoch_id = current_epoch_id(now_ms());
        let key = derive_ephemeral_key(
            PkarrCase::Identity,
            &self.owner_identity_pub,
            &epoch_id.to_be_bytes(),
        );
        self.publisher.register("identity", key, /* ... */);
    } else {
        self.publisher.unregister("identity");
    }
}
```

Persistence: setting written to `AppData/connectivity-settings.json` keyed by `owner_addr`. Re-read on boot; if true, register publication at startup.

### 6.3 Case C: in-community fallback

```rust
// pkarr_community_publisher.rs (sketch)
fn on_community_joined(&self, community_id: SpaceId, epoch_key: EpochKey) {
    let epoch_id = current_epoch_id(now_ms());
    let mut info = Vec::with_capacity(64 + 8);
    info.extend_from_slice(&self.owner_identity_pub);
    info.extend_from_slice(&epoch_id.to_be_bytes());
    let key = derive_ephemeral_key(
        PkarrCase::Community,
        epoch_key.as_bytes(),
        &info,
    );
    self.publisher.register(("community", community_id), key, /* ... */);
}

fn on_community_left_or_kicked(&self, community_id: SpaceId) {
    self.publisher.unregister(("community", community_id));
}
```

Triggered on community-create, community-join (from any source), or boot scan of existing CommunityStates.

### 6.4 IP-hiding for case B

Case B has the strongest exposure: any opt-in publisher signals "I (identity X) am online and reachable on the network." If the pkarr-relay sees the publishing client's IP, the relay operator can build a (identity_pub → IP, timestamp) log.

Mitigation for Phase 2:
- All publishes go via HTTP-relay (cases A and C inherit this).
- For case B specifically, the publisher rotates among ≥ 3 configured relays per publish. This scatters the (identity_pub → publishing_IP) signal across multiple operators rather than concentrating it.
- Resolver does NOT rotate for case B (the seeker has weaker privacy needs — bob looking up alice doesn't expose alice's identity, just bob's interest in alice).

Deferred to Phase 3+ hardening:
- Tor or equivalent transport for case B publishes.
- Decoy publishes/queries.
- Relay-side query batching to prevent timing correlation.

## 7. Resolution lifecycles

### 7.1 PkarrResolver shape

```rust
// crates/harmony-pkarr/src/resolver.rs
pub struct PkarrResolver {
    relay_client: RelayClient,
    cache: Mutex<LruCache<VerifyingKey, CachedResolution>>,
}

struct CachedResolution {
    record: Option<PkarrRoutingRecord>, // None = confirmed-absent
    fetched_at: Instant,
    ttl: Duration,
}

impl PkarrResolver {
    pub async fn resolve(
        &self,
        key: VerifyingKey,
    ) -> Result<Option<PkarrRoutingRecord>, ResolveError>;
}
```

Cache TTL: `min(15 min, epoch_remaining_ms / 4)`. Negative cache (confirmed-absent): 60s.

`resolve()` queries the three `epoch_tolerance_window` keys in parallel, picks the freshest valid record by `announced_at_ms`, verifies BEP44 outer sig + caches.

### 7.2 Case A orchestration

New IPC `connectivity_redeem_invite_iroh(invite_url: String) -> Result<RedemptionOutcome, String>`:

1. Decode URL → `CommunityInvitePayload` (existing `community_invite` decode logic).
2. Verify outer payload sigs (existing logic).
3. Derive case-A key from `invite_token.sig` + current epoch.
4. `PkarrResolver.resolve(key)`. Retry on `None` with backoff `[5s, 10s, 30s]`. After three misses → return `RedemptionOutcome::InviterUnreachable`.
5. Verify inner sig binds to `admin_identity_pub` from the invite payload (defense: hostile relay can't substitute a different routing without breaking the inner sig).
6. Parse `routing_blob` → iroh NodeId + relay + direct addrs.
7. `iroh::Endpoint::connect(node_addr, harmony/zenoh/v1)`.
8. Open zenoh session over that conn; send `CommunityInviteSigned` via the existing zenoh path. `community_invite::handle_unicast` (currently Reticulum-bound) gets a new transport-agnostic sender trait; iroh-borne zenoh session implements it.
9. Await counter-signed response, insert into CRDT — existing flow.

**Reticulum fallback:** if step 4 exhausts retries OR step 7 fails to connect, fall through to the existing Reticulum-bound redeem path. Phase 2 ships both transports in parallel; Phase 3+ may deprecate Reticulum once iroh path is proven.

### 7.3 Case B orchestration

New IPC `connectivity_discover_identity(identity_pub_hex: String) -> Result<Option<DiscoveredRecord>, String>`:

1. Parse `identity_pub_hex` → `[u8; 64]`.
2. Derive case-B key with `owner_identity_pub = parsed` + current epoch.
3. `PkarrResolver.resolve(key)`. Same retry shape as case A.
4. If `Some`: verify inner sig binds to the requested `identity_pub`.
5. Return parsed `DiscoveredRecord { iroh_node_id, relay_url, direct_addrs, announced_at_ms }` or `None`.

Caller (UI or another harmony-client subsystem) decides what to do with it — typically open an iroh conn and initiate a DM or community invite.

### 7.4 Case C orchestration

Automatic inside Phase 1's `ReachabilityResolver.resolve()`:

1. Check in-memory CRDT-sourced map first (unchanged Phase 1 behavior).
2. On miss (no entry OR entry older than 24h): iterate every community this device is in. For each, derive case-C key with the seeker's perspective: `EpochKey` from the community's per-epoch shared state, `member_identity_pub = addr`'s identity_pub.
3. Issue parallel pkarr queries (one per community-context the seeker shares with the target).
4. First valid response wins; insert into Phase 1's map with `provenance: PkarrFallback { community_id, epoch_id }`.
5. Subsequent Phase 1 resolves hit the warm map for `cache_ttl` (15 min); after that, either a CRDT-sourced entry has overwritten (preferred) or the pkarr fallback fires again.

**Privacy property:** the case-C HKDF requires the community's `EpochKey`, so case C only enables discovery among members of a shared community. Strangers cannot use case C to find anyone.

## 8. ALPN registry & connection reuse

No new ALPN. The case-A redemption connection reuses `harmony/zenoh/v1` (Phase 1's ALPN).

Rationale:
- Once alice and bob have a zenoh-over-iroh session open, that same session carries post-join CRDT sync immediately (no second handshake).
- `community_invite::handle_unicast` is abstracted over its sender trait — only needs a `Zenoh::Sender` implementation backed by an iroh-borne zenoh session, which Phase 1 already produces.
- Avoids ALPN registry bloat.

Case B and case C don't change ALPN at all — they just produce iroh NodeIds that the existing zenoh-over-iroh transport dials.

## 9. Verify rules (RPK1-RPK5)

Mirror Phase 1's RCH1-RCH5 with silent-drop discipline. Any rule failure: drop the record, log at WARN with `pkarr_case` + truncated key hex, do not raise an error to the caller.

| Rule | Check | Failure mode |
|---|---|---|
| **RPK1** | BEP44 outer signature verifies under the derived ephemeral pubkey | Silent drop — record was not produced by someone with the HKDF secret. |
| **RPK2** | `inner_sig` verifies over `canonical_cbor((routing_blob, harmony_identity_pub, announced_at_ms))` using the last 32 bytes of `harmony_identity_pub` as the Ed25519 verifying key | Silent drop — record's claimed identity doesn't match its routing data. |
| **RPK3** | When case A or case B: `harmony_identity_pub` matches the caller-supplied expected identity (case A: `admin_identity_pub` from invite; case B: the queried `identity_pub_hex`). When case C: matches the requested resolver target. | Silent drop — substitution attempt. |
| **RPK4** | `announced_at_ms` within ±30 min of `now_ms()` | Silent drop — stale or clock-skewed. |
| **RPK5** | `routing_blob` successfully decodes into harmony-client's iroh routing format (NodeId + relay + addrs) | Silent drop — malformed payload (likely version-mismatch or attacker). |

## 10. Multi-device behavior (ZEB-173 interaction)

Per [ZEB-173](https://linear.app/zeblith/issue/ZEB-173) (Done), an OWNER has a long-lived master identity that UCAN-delegates to per-device identities. `harmony_identity_pub` in Phase 2 records always refers to the OWNER's identity (matching Phase 1's convention). Per-device iroh routing is carried inside the opaque `routing_blob`.

**Phase 2 semantics: LWW across devices.**

| Case | Multi-device collision behavior | Why this is acceptable for Phase 2 |
|---|---|---|
| **A** | None — each device that issues an invite produces its own `invite_token.sig` ⇒ its own derived key ⇒ its own DHT slot. | Per-invite keying is naturally per-device. |
| **B** | Two devices of the same owner derive `HKDF(owner_pub, epoch)` → same DHT key → BEP44 LWW: freshest publisher wins. | First-contact only needs to reach *some* device of alice; once any conn is open, in-community sync routes to her active device. |
| **C** | Two in-community devices of the same owner derive `HKDF(EpochKey ‖ owner_pub, epoch)` → same key → LWW. | Phase 1's in-community CRDT resolver remains the primary path for multi-device discovery (composite-keyed per PR-157 round-5). pkarr fallback is the rare backstop. |

**Phase 3 upgrade path (out of scope):** switch cases B and C to multi-record by extending HKDF to mix in `publishing_device_pub`. Resolver would then need the target's device list (queryable via the ZEB-173 OwnerDeviceCache once published).

## 11. Tauri IPC surface & events

### 11.1 New IPCs

| IPC | Signature | Purpose |
|---|---|---|
| `connectivity_redeem_invite_iroh` | `(invite_url: String) -> Result<RedemptionOutcome, String>` | Case A orchestration (Section 7.2). |
| `connectivity_set_identity_discoverable` | `(enabled: bool) -> Result<(), String>` | Case B toggle. Persists to settings; kicks publisher. |
| `connectivity_get_identity_discoverable` | `() -> Result<bool, String>` | Read current setting. |
| `connectivity_discover_identity` | `(identity_pub_hex: String) -> Result<Option<DiscoveredRecord>, String>` | Case B reverse lookup (Section 7.3). |
| `connectivity_pkarr_publication_status` | `() -> Result<PublicationStatus, String>` | Diagnostics: returns active publications per case + last republish + next republish + expiry. |

All IPCs use `rename_all = "snake_case"` per harmony-client convention.

### 11.2 New events

| Event | Payload (camelCase to JS) | When |
|---|---|---|
| `connectivity-invite-resolution-progress` | `{ inviteId, stage: "resolving" \| "connecting" \| "sending" \| "awaiting_countersig" \| "joined", attemptN }` | During case A redemption — drives the join-flow UI progress bar. |
| `connectivity-identity-discoverable-changed` | `{ enabled: boolean }` | On case B toggle. |
| `connectivity-pkarr-fallback-fired` | `{ peerAddrShort, communityId, hit: boolean }` | Each time case C fallback resolves (or fails). Diagnostics-only. |

### 11.3 Extensions to Phase 1 IPCs

`connectivity_force_republish` (Phase 1) is extended to also force the pkarr publisher to republish all active publications. Cheap; useful for diagnostics + manual recovery.

`connectivity_get_my_reachability` and `connectivity_list_known_reachability` are unchanged (return Phase 1 in-community CRDT data only).

## 12. UX surface

### 12.1 Join Community via Invite Link

Extends the existing `src/lib/components/RedeemInviteDialog.svelte` component (already wired for the LAN-via-Reticulum redeem flow; Phase 2 adds the iroh path alongside):

- Paste-or-scan QR for `harmony://invite/{base64url}`.
- Progress states wired to `connectivity-invite-resolution-progress`:
  - "Looking up inviter…" (stage `resolving`)
  - "Connecting…" (stage `connecting`)
  - "Sending join request…" (stage `sending`)
  - "Waiting for confirmation…" (stage `awaiting_countersig`)
  - "Joined ✓" (stage `joined`)
- On `RedemptionOutcome::InviterUnreachable` (Section 7.2 retry exhaustion): "Couldn't reach the inviter through the network right now. They may be offline; try again later." No fallback button.
- Tauri error extraction: `e instanceof Error ? e.message : String(e)` per CLAUDE.md.

### 12.2 Settings → Privacy → Network Discoverability

New section in the existing Settings panel:

- Single toggle: "Let people who know my harmony identity find my devices on the network."
- Default: **OFF**.
- Helper copy (1-2 sentences plain-language): "When on, anyone who has your identity address can connect to your devices over the internet. When off, you can only be reached through invite links and communities you already share."
- Sibling link: "Learn more" → opens an in-app Privacy doc page (page itself out of scope for Phase 2; link can dead-end with a "coming soon" copy).

### 12.3 Diagnostics Panel additions

Extends `DiagnosticsPanel.svelte` (Phase 1):

- New collapsible section "Network Discovery (pkarr)" under the existing "Iroh" section.
- Shows: # active publications by case (A/B/C counts), last republish time, # of pkarr-fallback hits in the last 24h (driven by `connectivity-pkarr-fallback-fired`).
- Dev-mode-only — gated behind the existing diagnostics feature flag.

No new top-level menu items. No new settings categories.

## 13. Test plan

### 13.1 Unit tests (per-crate)

**`harmony-pkarr`:**
- HKDF derivation reference vector — for each of the 3 cases, a fixed `(ikm, info, salt)` produces a fixed 32-byte seed → fixed Ed25519 pubkey hex. Pinned.
- `PkarrRoutingRecord` canonical CBOR round-trip + wire-format pin fixture (mirroring Phase 1's `wire_format_reachability_announce_fixtures.rs`).
- Inner-sig verify path: 5 silent-drop rules (RPK1-RPK5) each as a discrete test.
- Epoch math: boundary tests for `epoch_id` rollover, `epoch_tolerance_window` selection.
- Relay client: 429 backoff, all-relays-down behavior, cooldown reset after window.

**`harmony-client/src-tauri`:**
- Each policy module's lifecycle: case A start-on-generate / stop-on-consumed-or-expired; case B start-on-toggle-on / stop-on-toggle-off; case C start-on-community-join / stop-on-leave-or-kick.
- `PkarrResolverAdapter` plugged into Phase 1's `ReachabilityResolver`: assert that resolve cache-miss with adapter wired issues ONE pkarr query per community-context, returns the freshest, populates Phase 1's map with correct provenance.
- `pkarr_settings` persistence: write toggle, re-read on simulated boot, publisher state matches.

### 13.2 Integration tests

Each runs end-to-end with in-process iroh endpoints + a mock pkarr-relay HTTP server (axum, in-memory BEP44 store, shipped as `harmony-pkarr::testing` fixture).

- **Case A** (`tests/pkarr_invite_redemption_integration.rs`): alice generates invite (existing IPC); alice's `pkarr_invite_publisher` writes to mock relay. Bob's `connectivity_redeem_invite_iroh(url)` resolves from mock relay, opens iroh conn, sends `CommunityInviteSigned`, receives counter-sig. Assert bob's `CommunityState` reflects new membership. Wall-clock budget < 30s.
- **Case B** (`tests/pkarr_identity_discovery_integration.rs`): alice toggles discoverable, publisher writes. Bob's `connectivity_discover_identity(alice_pub)` returns alice's routing. Variant: substitute a fake pubkey in the record → resolve returns `None` (RPK3).
- **Case C** (`tests/pkarr_community_fallback_integration.rs`): seed Phase 1's resolver with an empty map. Wire `pkarr_resolver_adapter` as fallback. Resolve alice's addr → adapter derives community-keyed pkarr key → mock relay returns alice's record → adapter inserts into Phase 1 map. Subsequent resolves hit warm map.

### 13.3 Wire-format pinning

New `src-tauri/tests/wire_format_pkarr_routing_record_fixtures.rs` — pins canonical CBOR bytes + outer/inner sig hex for one record per case (3 total). Refuses to change without regenerating the fixture, exactly like Phase 1's pin tests.

### 13.4 Cross-repo CI sequencing

PR 1 (harmony) merges → its merge commit SHA is recorded in PR 2's Cargo.lock. PR 2's CI fails until that lockfile update is in. The PR 2 body explicitly documents merge order: "Blocks on harmony#NNN merging first."

## 14. Failure modes & recovery

| Failure | User-visible behavior | Internal recovery |
|---|---|---|
| **DHT cache miss** (bob queries a key that hasn't propagated yet) | "Looking up…" stays visible during retry; final error if all 3 attempts (5s/10s/30s backoff) miss | Resolver retries with backoff. On final miss, surface `InviterUnreachable` (case A) or `None` (case B/C); let user retry manually. |
| **Stale routing** (alice published, then moved networks before bob queried) | Iroh `connect()` times out; UI shows "Inviter unreachable" | Resolver invalidates its 15-min cache for this key + re-queries on next attempt. Alice's next scheduled republish (≤ 30 min if she's online) refreshes the DHT. |
| **All configured pkarr-relays unreachable** | "Discovery service unavailable. Check your connection and try again." | Relay client iterates the configured list per request; per-request timeout 5s. Each request that times out trips a 30s per-relay cooldown. If all relays on cooldown, surface error. |
| **Inner-sig verify fails** (hostile relay returned an attacker's routing) | Silent drop, treat as cache miss | Per RPK2/RPK3, record is discarded as if it never existed. Log to tracing at WARN. No retry of the same record. |
| **Multi-record collision** (LWW between alice's devices, or coincidental key collision) | Resolver picks the one with greatest `announced_at_ms` | Both records' inner sigs verified; freshest wins. If neither binds to the expected identity_pub, drop both, treat as cache miss. |
| **pkarr-relay rate-limit (429)** | Slower publishes; more retry attempts visible in diagnostics | Per-relay 429-backoff. Rotation across configured pool. Phase 3 may add adaptive cadence. |
| **HTTP-relay operator censorship** (relay refuses specific keys) | User sees "Inviter unreachable" with no specific explanation | Try the next relay in rotation. If all configured relays collude, Phase 3 alternative-relay-discovery is needed. Residual trust in relay operators documented. |
| **Inviter offline when invite redemption attempted** | "Inviter unreachable" | Phase 2 retries 3× then falls back to existing Reticulum redeem path (LAN-bound only; will work if both peers are on bridged Reticulum). |

## 15. Within-Phase-2 PR phasing

The spec defines one cohesive design; implementation splits into 2 sequenced PRs:

**PR 1 — `harmony#NNN` — `harmony-pkarr` crate**
- New crate, 5-6 source files (Section 4.1).
- ~800-1200 LOC Rust + tests.
- Unit tests, wire-format pin, mock-relay fixture.
- Adds `pkarr = "<latest>"` to harmony workspace deps.
- No downstream impact.

**PR 2 — `harmony-client#MMM` — policies + IPCs + UX**
- Pins `harmony-pkarr` to PR-1's merge commit SHA.
- 5 new policy modules + Phase 1 resolver extension (Section 4.2, 4.3).
- 5 new Tauri IPCs + 3 new events (Section 11).
- 3 UI changes (Section 12).
- 3 integration tests (Section 13.2).
- ~1800-2500 LOC Rust + TS + Svelte.

**Optional further-split inside PR 2** — if review surface exceeds ~3500 LOC, the writing-plans skill can carve PR 2 into 2a (case A + redemption IPC + join UX) → 2b (case B + discoverable toggle UI) → 2c (case C resolver extension + diagnostics). Each independently mergeable, each depending on PR 1. Default plan: keep as one PR 2.

**Linear ticket creation:** per the `never-invent-IDs` rule, the Phase-2 sub-ticket gets filed after this spec lands. The PR bodies use prose-only references to "the multi-phase ZEB-321 initiative" with NO bare `ZEB-321` identifier strings in the PR title or body (per the just-updated `feedback_linear_pr_auto_close` memory rule — markdown-linked refs still trigger the cascade). Only the new sub-ticket appears as an auto-close trigger.

## 16. Out of scope (deferred to Phase 3+)

- **Mobile push-wake architecture** (Zero-Push gateway, iOS NSE, UnifiedPush): Phase 3.
- **Liveness / rebinding protocol** for already-paired peers across cellular handoff: Phase 3.
- **Multi-record per-device pkarr publication** (per Section 10 upgrade path): Phase 3 or later.
- **Embedded full DHT client** (no relay dependency): Phase 4+ if relay trust ever becomes unacceptable.
- **Decoy queries / publishes for case B IP-hiding**: Phase 3 hardening.
- **Tor or equivalent anonymizing transport for case B publishes**: Phase 3+.
- **Community-operated pkarr relays as civic infrastructure**: Phase 5+ (relay governance pillar).
- **Deprecation of the existing Reticulum-bound invite redeem path**: Phase 3 or later once iroh path is empirically proven.

## 17. Cross-references

- [ZEB-321](https://linear.app/zeblith/issue/ZEB-321) — umbrella, this is Phase 2.
- [ZEB-217](https://linear.app/zeblith/issue/ZEB-217) — Sub-C communities (existing `community_invite.rs` lives under this; URL form `harmony://invite/{base64url}` is its surface).
- [ZEB-218](https://linear.app/zeblith/issue/ZEB-218) — Sub-D library-federated directory (orthogonal — public-community discovery; Phase 2 covers private + reconnection).
- [ZEB-173](https://linear.app/zeblith/issue/ZEB-173) — Multi-device identity binding (Done). Phase 2 takes LWW across devices; upgrade-to-multi-record path noted.
- [ZEB-249](https://linear.app/zeblith/issue/ZEB-249) — Invite epoch system (`EpochKey` reused as case-C HKDF input).
- [ZEB-46](https://linear.app/zeblith/issue/ZEB-46) — EigenTrust dynamic rendezvous (related research track; Phase 2 doesn't depend on it).
- [ZEB-47](https://linear.app/zeblith/issue/ZEB-47) — ZipPIR private discovery (orthogonal research; Phase 2 doesn't use it).
- Phase 1 spec: `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md`.
- Phase 2 research prompt: `docs/research/2026-05-23-zeb-321-phase2-discovery-research-prompt.md`.
- Phase 2 research report: `docs/research/2026-05-23-zeb-321-phase2-discovery-research-report.md`.
