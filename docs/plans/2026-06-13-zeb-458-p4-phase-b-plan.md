# ZEB-458 P4 Phase B — Community Sealed Relay: Production Wiring — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the Phase A community-sealed-relay *mechanism* into the live node so a co-community volunteer actually advertises, accepts opaque deposits, serves pulls, and a sender's outbox falls through to a relay when both the direct and first-party-butler paths fail — proven end-to-end across three engines.

**Architecture:** Phase A shipped the standalone mechanism (wire types, `RelayHoldDoc` CRDT, three core admission/pull handlers, `open_and_ingest`) behind three context traits with only test mocks and zero `start_node` wiring. Phase B adds the production glue: a `CommunityRelayAnnounce` membership-CRDT event (mirroring `ReachabilityAnnounce` exactly — payload module + resolver + publisher + no-op materialize), a fleet-replicated opt-in doc, production implementations of the three `*Ctx` traits, the two iroh ALPN acceptor shells, a background pull driver, a last-resort sender rung in the outbox drain, the `start_node` install + opt-in IPCs, and a three-engine E2E test. The relay never opens the blob — that invariant is preserved structurally (it implements no decrypt seam).

**Tech Stack:** Rust, Tokio, iroh (ALPN connections), zenoh, ciborium (canonical CBOR), ed25519-dalek / x25519, Tauri IPC, `FleetSyncEngine<Doc>` CRDT replication, `cargo nextest`.

**Spec:** `docs/specs/2026-06-13-zeb-458-community-sealed-relay-design.md` (decisions D35–D45; Phase B = D37 publish/resolve + D39 driver + D40 sender rung + D43 lifecycle + the full E2E test).

**Scope / PR shape:** ONE PR (`zeb-458-p4-phase-b`), per the bundling rule (one PR per repo in flight; Phase B is one cohesive feature whose security argument only closes when the E2E test passes). **Fallback split point** if the diff proves unreviewable at PR-open time: T1–T8 (the relay-*serving* side + install of it) is a natural first sequential PR, T9–T12 (sender rung + recipient pull-driver + E2E) the second — decide at PR-open, default is one PR.

**Phase A surfaces this plan builds on (already on `main@fab0bb19`, do NOT re-create):**
- `src/community_relay.rs` — all wire types (`RelayDepositFrame`, `RelayDepositAck`, `RelayPullQuery`, `RelayHeldBlob`, `RelayPullResponse`, `RelayPullAck`) + `encode_/decode_` pairs + sig-payload builders (`relay_deposit_sig_payload`, `relay_pull_sig_payload`, `relay_pull_ack_sig_payload`) + `build_relay_deposit_frame` + `both_joined_members` + the ALPN/seal/sig-domain/cap/TTL constants.
- `src/community_relay_hold_crdt.rs` — `RelayHoldDoc` / `RelayHoldEntry`, `RelayHoldDoc::key`, `merge_from`, `count_for_sender`, `live_count`, `gc`.
- `src/iroh_community_relay_acceptor.rs` — `RelayDepositCtx`, `RelayPullCtx` traits (test mocks only); `handle_relay_deposit_core`, `handle_relay_pull_query`, `handle_relay_pull_ack` (fully implemented + tested); `RelayDepositReject`, `RelayPullReject`, `RelayPersistVerdict`.
- `src/community_relay_pull.rs` — `RelayIngestCtx` trait (test mock only) + `open_and_ingest`.
- `tests/wire_format_community_relay_fixtures.rs` — byte-pins for `RelayDepositFrame` / `RelayPullQuery` / `RelayPullResponse`.

**Reference patterns to MIRROR (read before implementing the relevant task):**
- `src/reachability_record.rs` — payload module shape: 2-char serde keys, `inner_signed_bytes` (binds `data ‖ actor ‖ hlc` so a payload can't be replayed under a different actor/HLC), `build_signed_payload_with_key`, `verify_inner_signature`, `InnerSigError`, the `fresh_butler_set` freshness reader, and the in-module CBOR pin test. **This is the exact template for T1.**
- `src/reachability_resolver.rs` — in-memory resolver fed by an event-loop hook (`update(actor, payload, hlc)`, `resolve(actor)`). Template for T3.
- `src/reachability_publisher.rs` — periodic signed-announce publisher (`tokio::spawn` loop, `Notify` + `interval`, no locks held across await). Template for T10.
- `src/community_membership.rs` — `MembershipEventKind::ReachabilityAnnounce { payload }` (enum at line 83+; variant ~line 305), the verify arm (`verify_event`, the `ReachabilityAnnounce` match ~line 3228: RCH1 outer sig already done, RCH2 inner sig via `reachability_record::verify_inner_signature`, RCH4 ±30min skew `REACHABILITY_TIMESTAMP_SKEW_MAX_MS`, RCH5 `is_joined_member`), and the **no-op materialize arm** (`~line 2461`). Template for T2.
- `src/fleet_net.rs` — `FleetNetDoc` LWW-by-stamp CRDT + `merge_from`. Template for T4 (`RelayOptInDoc`).
- `src/lib.rs` — butler-deposit acceptor install (`~6259–6293`), butler-deposit client inject (`~6307–6339`), `NodeState` fleet-net handles (`~989–1015`), `set_butler_pin_inner`/`set_butler_pin` IPC (`~42193`, `~42224`), `invoke_handler` list (`~42740`). Templates for T7/T8/T11.
- `src/dm_outbox.rs` — `drain_phase_c` (`~1040`), butler-rung candidacy (Ok-arm `~1110`, Err-arm `~1150`), `drain_lifted` spawned-task butler rung (`~2109–2188`), `set_butler_deposit_client` (`~540`). Template for T6.
- `src/zenoh_iroh_transport.rs` — ALPN dispatch match in `spawn_accept_loop` (`~337–443`), `install_butler_deposit_acceptor` seam (`~203–213`). Template for T8.
- `src/iroh_endpoint.rs` — `alpn` module (`~46–68`) + the `.alpns(vec![...])` builder lists (`~106–113`, `~358–363`). Template for T8.

**House rules (every task):**
- No worktrees — work directly in `/Users/zeblith/work/zeblithic/harmony-client` on branch `zeb-458-p4-phase-b`.
- `--locked` on every cargo invocation. `set -o pipefail` before any piped gate. Commit BEFORE running gates (so a hung gate never loses work). 10-minute wall-clock kill switch on every cargo command (`timeout` param on the Bash tool — macOS has no `timeout` binary).
- **Per-task gate (lib-scoped, fast):**
  ```bash
  set -o pipefail
  cd src-tauri
  cargo fmt --all -- --check
  cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
  cargo nextest run --locked -p harmony-app --lib --features test-fixtures 2>&1 | tail -30
  ```
- **Integration-test tasks (T12, and any task that adds/touches a `tests/*.rs` file)** additionally run the specific test binary: `cargo nextest run --locked -p harmony-app --features test-fixtures --test community_relay_integration 2>&1 | tail -40` and `--test wire_format_community_relay_fixtures`.
- **Final task (T12) only:** the full `--all-targets` sweep (reserved for last; relinks ~97 integration binaries, ~13 min+ — run in BACKGROUND if it would exceed the 10-min foreground budget, with a ScheduleWakeup/heartbeat safety net):
  ```bash
  set -o pipefail
  cd src-tauri
  cargo fmt --all -- --check
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
  cargo nextest run --locked --workspace --all-targets --features test-fixtures
  ```
- Subagent statuses: DONE / DONE_WITH_CONCERNS / NEEDS_CONTEXT / BLOCKED. Implementers do NOT push.

---

## File Structure

**New modules (`src/`):**
- `community_relay_announce.rs` — `CommunityRelayEntry`, `CommunityRelayAnnouncePayload`, inner-sig (`inner_signed_bytes` / `build_signed_community_relay_announce` / `verify_inner_signature`), `fresh_relay_entry` reader, freshness/cap constants. (T1)
- `community_relay_resolver.rs` — `CommunityRelayResolver` in-memory store (`update` + `relays_for_community`). (T3)
- `community_relay_optin.rs` — `RelayOptInDoc` fleet-replicated per-community opt-in CRDT. (T4)
- `community_relay_prod.rs` — `ProdRelayDepositCtx`, `ProdRelayPullCtx` (T7); `ProdRelayIngestCtx` (T9); `ProdCommunityRelayDepositClient` (T9). Production `*Ctx` impls holding live handles.
- `community_relay_pull_driver.rs` — `CommunityRelayPullDriver` background loop. (T9)
- `community_relay_publisher.rs` — `CommunityRelayPublisher` background advertise loop. (T10)

**Modified (`src/`):**
- `community_relay.rs` — `find_shared_communities` + `CommunityRelayDepositClient` trait (T5).
- `community_membership.rs` — `CommunityRelayAnnounce` variant + verify arm + no-op materialize arm (T2).
- `dm_outbox.rs` — `community_relay_deposit_client` field + setter + drain rung (T6).
- `iroh_community_relay_acceptor.rs` — `IrohCommunityRelayDepositAcceptor` + `IrohCommunityRelayPullAcceptor` shells + `RelayPullAckFrame` envelope (T8).
- `iroh_endpoint.rs` — register both ALPNs (T8).
- `zenoh_iroh_transport.rs` — ALPN dispatch arms + install seams (T8).
- `event_loop.rs` — feed applied `CommunityRelayAnnounce` events into the resolver; relay republish timer + pull-driver poke (T10).
- `lib.rs` — `NodeState` fields, `start_node` install, opt-in IPCs + `invoke_handler` (T11).

**Modified (`tests/`):**
- `wire_format_community_relay_fixtures.rs` — pin `RelayPullAckFrame` (T8). (`CommunityRelayAnnouncePayload` pin lives in its own module, T1, mirroring `reachability_record.rs`.)
- `community_relay_integration.rs` — third "relay" engine + E2E (T12).

---

## Task 1: `community_relay_announce` payload module (D37 data model)

**Files:**
- Create: `src-tauri/src/community_relay_announce.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod community_relay_announce;` next to the other `pub mod community_relay*` declarations)

**Read first:** `src/reachability_record.rs` in full — this task mirrors it. Keep the same conventions: 2-char serde keys at each nesting level, `serialize_bytes_as_bstr`/`deserialize_bytes_from_bstr` for byte arrays, `inner_signed_bytes` binding `data ‖ actor ‖ hlc`, `CanonicalPayload`/`CanonicalPayloadSealed` impls, an in-module CBOR pin test.

- [ ] **Step 1: Write the failing tests** (`#[cfg(test)] mod tests` in the new file)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::{Hlc, OwnerAddr};

    fn fixture_hlc() -> Hlc {
        Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: "fix".into() }
    }
    fn fixture_entry() -> CommunityRelayEntry {
        CommunityRelayEntry {
            relay_device_id: [0x11; 16],
            iroh_endpoint_id: [0x22; 32],
            relay_device_ed25519_verify: [0x33; 32],
            home_relay: "https://derp.example/".into(),
        }
    }

    #[test]
    fn payload_round_trips_canonical_cbor() {
        let p = CommunityRelayAnnouncePayload {
            relay: fixture_entry(),
            ad_at: 1_700_000_000_000,
            identity_signature: [0xCD; 64],
        };
        let bytes = canonical_payload_bytes(&p).expect("encode");
        let back: CommunityRelayAnnouncePayload =
            ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(back, p);
    }

    #[test]
    fn payload_top_level_keys_are_2_chars() {
        let p = CommunityRelayAnnouncePayload {
            relay: fixture_entry(), ad_at: 1, identity_signature: [0; 64],
        };
        let bytes = canonical_payload_bytes(&p).expect("encode");
        let val: ciborium::Value = ciborium::de::from_reader(&bytes[..]).expect("decode");
        for (k, _) in val.as_map().expect("map") {
            assert_eq!(k.as_text().expect("text").chars().count(), 2);
        }
    }

    #[test]
    fn inner_sig_round_trips_and_rejects_actor_or_hlc_mutation() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]);
        let vk = signing_key.verifying_key();
        let actor = OwnerAddr([0xAA; 16]);
        let hlc = fixture_hlc();
        let p = build_signed_community_relay_announce(
            fixture_entry(), 1_700_000_000_000, &actor, &hlc, &signing_key,
        ).expect("build");
        verify_inner_signature(&p, &actor, &hlc, &vk).expect("verify");
        assert!(verify_inner_signature(&p, &OwnerAddr([0xBB; 16]), &hlc, &vk).is_err());
        let wrong_hlc = Hlc { wall_ms: hlc.wall_ms + 1, ..hlc.clone() };
        assert!(verify_inner_signature(&p, &actor, &wrong_hlc, &vk).is_err());
    }

    #[test]
    fn inner_sig_rejects_tampered_relay_entry() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]);
        let vk = signing_key.verifying_key();
        let actor = OwnerAddr([0xAA; 16]);
        let hlc = fixture_hlc();
        let mut p = build_signed_community_relay_announce(
            fixture_entry(), 1_700_000_000_000, &actor, &hlc, &signing_key,
        ).expect("build");
        p.relay.iroh_endpoint_id[0] ^= 0xFF;
        assert!(verify_inner_signature(&p, &actor, &hlc, &vk).is_err());
    }

    #[test]
    fn fresh_relay_entry_filters_stale_zero_and_future() {
        let p = CommunityRelayAnnouncePayload {
            relay: fixture_entry(), ad_at: 1_700_000_000_000, identity_signature: [0; 64],
        };
        let w = COMMUNITY_RELAY_AD_FRESHNESS_MS;
        let t = p.ad_at;
        assert!(fresh_relay_entry(&p, t).is_some());
        assert!(fresh_relay_entry(&p, t + w).is_some());        // window edge
        assert!(fresh_relay_entry(&p, t - w).is_some());        // forward-skew edge
        assert!(fresh_relay_entry(&p, t + w + 1).is_none());    // stale
        assert!(fresh_relay_entry(&p, t - w - 1).is_none());    // too-far-future stamp
        let mut zero = p.clone();
        zero.ad_at = 0;
        assert!(fresh_relay_entry(&zero, 1).is_none());         // missing stamp
    }

    /// Byte-pin the canonical CBOR of a deterministic payload. Capture on first
    /// run via the eprintln!, then hardcode and keep. A failure = deliberate
    /// re-pin only.
    #[test]
    fn payload_wire_bytes_pinned() {
        let p = CommunityRelayAnnouncePayload {
            relay: CommunityRelayEntry {
                relay_device_id: [0x11; 16],
                iroh_endpoint_id: [0x22; 32],
                relay_device_ed25519_verify: [0x33; 32],
                home_relay: "https://derp.example/".into(),
            },
            ad_at: 1_700_000_000_000,
            identity_signature: [0xCD; 64],
        };
        let hex = hex::encode(canonical_payload_bytes(&p).expect("encode"));
        eprintln!("community_relay_announce payload hex: {hex}");
        assert_eq!(hex, "<<CAPTURE_ON_FIRST_RUN>>",
            "CommunityRelayAnnouncePayload wire format changed — re-pin deliberately");
    }
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(community_relay_announce)'` → FAIL (module/types not defined).

- [ ] **Step 3: Implement the module.** Mirror `reachability_record.rs`:

```rust
//! ZEB-458 P4 Phase B: CommunityRelayAnnounce CRDT event payload.
//!
//! A community member opts in to volunteer as a sealed relay; opting in
//! publishes this signed advertisement into the community-state membership
//! CRDT, mirroring `ReachabilityAnnounce` (see `reachability_record.rs`).
//! Consumers read the fresh, capped advertiser set via `CommunityRelayResolver`.

use ed25519_dalek::Signer;
use serde::{Deserialize, Serialize};

use crate::owner_state_crypto::{
    canonical_cbor_encode, sealed::CanonicalPayloadSealed, CanonicalPayload, CryptoError,
};
use crate::owner_state_types::{
    deserialize_bytes_from_bstr, serialize_bytes_as_bstr, Hlc, OwnerAddr,
};
use crate::reachability_record::InnerSigError;

/// Advertisement refresh cadence (~7.5 min) — reuse the butler-set cadence.
pub const COMMUNITY_RELAY_AD_REFRESH_MS: u64 = crate::butler_deposit::BUTLER_SET_REFRESH_MS;
/// Freshness window (~15 min) — reuse the butler-set freshness window.
pub const COMMUNITY_RELAY_AD_FRESHNESS_MS: u64 = crate::butler_deposit::BUTLER_SET_FRESHNESS_MS;
/// Max advertisers a consumer reads per community (bounds fan-out, D37).
pub const COMMUNITY_RELAY_ADVERTISERS_MAX: usize = 4;

/// The relay device's dialing coordinates (one advertised volunteer device).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityRelayEntry {
    #[serde(rename = "rd", serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr")]
    pub relay_device_id: [u8; 16],
    #[serde(rename = "ep", serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr")]
    pub iroh_endpoint_id: [u8; 32],
    #[serde(rename = "vk", serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr")]
    pub relay_device_ed25519_verify: [u8; 32],
    #[serde(rename = "hr")]
    pub home_relay: String,
}

/// Payload of a `MembershipEventKind::CommunityRelayAnnounce`. Top-level keys
/// are all 2 chars (`rl`/`aa`/`sg`); `rl` nests `CommunityRelayEntry`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityRelayAnnouncePayload {
    #[serde(rename = "rl")]
    pub relay: CommunityRelayEntry,
    /// Wall-clock ms freshness stamp.
    #[serde(rename = "aa")]
    pub ad_at: u64,
    /// Inner ed25519 sig by the advertiser's enrolled device key over
    /// `inner_signed_bytes(relay, ad_at, actor, hlc)`.
    #[serde(rename = "sg", serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr")]
    pub identity_signature: [u8; 64],
}

impl CanonicalPayloadSealed for CommunityRelayAnnouncePayload {}
impl CanonicalPayload for CommunityRelayAnnouncePayload {}

pub fn canonical_payload_bytes(p: &CommunityRelayAnnouncePayload) -> Result<Vec<u8>, CryptoError> {
    canonical_cbor_encode(p)
}

/// Deterministic bytes the inner sig covers: CBOR of (rd, ep, vk, hr, aa, ac, hl).
/// Binds the relay entry + stamp to the surrounding actor + HLC so a payload
/// can't be replayed under a different actor/HLC (mirrors
/// `reachability_record::inner_signed_bytes`).
pub fn inner_signed_bytes(
    relay: &CommunityRelayEntry,
    ad_at: u64,
    actor: &OwnerAddr,
    hlc: &Hlc,
) -> Result<Vec<u8>, CryptoError> {
    #[derive(Serialize)]
    struct InnerSigInput<'a> {
        #[serde(rename = "rd", serialize_with = "serialize_bytes_as_bstr")]
        rd: &'a [u8; 16],
        #[serde(rename = "ep", serialize_with = "serialize_bytes_as_bstr")]
        ep: &'a [u8; 32],
        #[serde(rename = "vk", serialize_with = "serialize_bytes_as_bstr")]
        vk: &'a [u8; 32],
        #[serde(rename = "hr")]
        hr: &'a str,
        #[serde(rename = "aa")]
        aa: u64,
        #[serde(rename = "ac")]
        ac: &'a OwnerAddr,
        #[serde(rename = "hl")]
        hl: &'a Hlc,
    }
    let input = InnerSigInput {
        rd: &relay.relay_device_id,
        ep: &relay.iroh_endpoint_id,
        vk: &relay.relay_device_ed25519_verify,
        hr: &relay.home_relay,
        aa: ad_at,
        ac: actor,
        hl: hlc,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&input, &mut buf).map_err(|e| CryptoError::CborEncode(format!("{e}")))?;
    Ok(buf)
}

/// Sign a fresh advertisement with the advertiser's ENROLLED device key.
pub fn build_signed_community_relay_announce(
    relay: CommunityRelayEntry,
    ad_at: u64,
    actor: &OwnerAddr,
    hlc: &Hlc,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<CommunityRelayAnnouncePayload, CryptoError> {
    let inner = inner_signed_bytes(&relay, ad_at, actor, hlc)?;
    let sig = signing_key.sign(&inner).to_bytes();
    Ok(CommunityRelayAnnouncePayload { relay, ad_at, identity_signature: sig })
}

/// Verify the inner sig against the advertiser's enrolled device verifying key.
pub fn verify_inner_signature(
    p: &CommunityRelayAnnouncePayload,
    actor: &OwnerAddr,
    hlc: &Hlc,
    actor_ed25519_pub: &ed25519_dalek::VerifyingKey,
) -> Result<(), InnerSigError> {
    let bytes = inner_signed_bytes(&p.relay, p.ad_at, actor, hlc).map_err(|_| InnerSigError::Encode)?;
    let sig = ed25519_dalek::Signature::from_bytes(&p.identity_signature);
    actor_ed25519_pub.verify_strict(&bytes, &sig).map_err(|_| InnerSigError::Invalid)
}

/// Reader-side freshness gate (mirrors `fresh_butler_set`): the entry iff
/// `ad_at` is present and within `COMMUNITY_RELAY_AD_FRESHNESS_MS` of `now_ms`,
/// tolerating one window of forward skew. Zero / stale / too-far-future → None.
pub fn fresh_relay_entry(
    p: &CommunityRelayAnnouncePayload,
    now_ms: u64,
) -> Option<CommunityRelayEntry> {
    if p.ad_at == 0
        || p.ad_at > now_ms.saturating_add(COMMUNITY_RELAY_AD_FRESHNESS_MS)
        || now_ms.saturating_sub(p.ad_at) > COMMUNITY_RELAY_AD_FRESHNESS_MS
    {
        return None;
    }
    Some(p.relay.clone())
}
```

  Add `pub mod community_relay_announce;` to `lib.rs` (alongside the other `community_relay*` module declarations).

- [ ] **Step 4: Capture the pin.** Run the suite; copy the `eprintln!` hex from `payload_wire_bytes_pinned` into the `assert_eq!`, re-run → PASS. Run the full per-task gate.

- [ ] **Step 5: Commit** — `git add -A && git commit -m "feat(zeb-458-p4b): CommunityRelayAnnounce payload module + inner-sig + freshness reader"`

---

## Task 2: `CommunityRelayAnnounce` membership event (variant + verify + no-op materialize) (D37)

**Files:**
- Modify: `src-tauri/src/community_membership.rs` — `MembershipEventKind` enum (~line 83), `verify_event` (the per-variant match arm; `ReachabilityAnnounce` arm at ~3228 is the model), the materialize loop (no-op arm next to `ReachabilityAnnounce` at ~2461), and any exhaustive `match MembershipEventKind` the compiler flags (e.g. ~2943).

**Read first:** the `ReachabilityAnnounce` variant + its verify arm + its no-op materialize arm. The new arm reuses the SAME RCH-style checks against `community_relay_announce::verify_inner_signature`.

- [ ] **Step 1: Write the failing test** (in `community_membership.rs` `#[cfg(test)]`, beside the existing reachability verify tests):

```rust
#[test]
fn community_relay_announce_verifies_when_actor_joined_and_inner_sig_valid() {
    // Build a community with a joined member M; M signs a CommunityRelayAnnounce
    // with M's enrolled device key; verify_event must accept. Mutating the inner
    // sig (wrong device key) must yield ReachabilityInnerSigInvalid-equivalent;
    // a non-member actor must yield ReachabilityActorNotMember-equivalent;
    // announced_at skew > 30min must yield the skew error.
    // (Mirror zeb_321_reachability_verify_tests construction exactly, swapping
    //  ReachabilityAnnounce for CommunityRelayAnnounce.)
}
```

  Find the existing `ReachabilityAnnounce` verify test (search `ReachabilityActorNotMember` in tests) and clone its harness verbatim, substituting the new variant + `community_relay_announce::build_signed_community_relay_announce`. Write the four assertions (accept; bad inner sig; non-member; skew) with the real error variants the new arm returns.

- [ ] **Step 2: Run to verify it fails** — `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(community_relay_announce_verifies)'` → FAIL.

- [ ] **Step 3: Implement.**
  1. Add the enum variant (mirror `ReachabilityAnnounce`):
     ```rust
     /// ZEB-458 P4 Phase B: a member's opt-in relay advertisement. No
     /// membership-state effect; resolved by `CommunityRelayResolver` in the
     /// event loop (mirrors ReachabilityAnnounce).
     CommunityRelayAnnounce {
         #[serde(rename = "vl")]
         payload: crate::community_relay_announce::CommunityRelayAnnouncePayload,
     },
     ```
     Use the same serde field-key convention the `ReachabilityAnnounce` variant uses for its `payload` (match the existing `#[serde(rename = ...)]`).
  2. Add the verify arm next to `ReachabilityAnnounce` (~3228). Reuse RCH1 (outer sig, already done above the match), RCH2 (inner sig via `community_relay_announce::verify_inner_signature` with the resolved `signer_vk`), RCH4 (skew: `payload.ad_at.abs_diff(event.at.wall_ms) > REACHABILITY_TIMESTAMP_SKEW_MAX_MS` → the skew error), RCH5 (`is_joined_member(prior_state, &event.actor)` → not-member error). Reuse the existing `VerifyError::Reachability*` variants (they are generic "inner sig / skew / actor-not-member" errors; no new error variants needed — keep DRY).
  3. Add the no-op materialize arm next to `ReachabilityAnnounce` (~2461):
     ```rust
     MembershipEventKind::CommunityRelayAnnounce { .. } => {
         // No membership-state effect; resolved by CommunityRelayResolver.
     }
     ```
  4. Fix every other exhaustive `match` the compiler flags (e.g. the `~2943` arm) with a `CommunityRelayAnnounce { .. } => { /* parallel to ReachabilityAnnounce */ }` arm matching whatever the sibling does.

- [ ] **Step 4: Run to verify it passes + full per-task gate.**

- [ ] **Step 5: Commit** — `git commit -am "feat(zeb-458-p4b): CommunityRelayAnnounce membership event — verify (RCH2/4/5) + no-op materialize"`

---

## Task 3: `CommunityRelayResolver` (D37 read path)

**Files:**
- Create: `src-tauri/src/community_relay_resolver.rs`
- Modify: `src-tauri/src/lib.rs` (`pub mod community_relay_resolver;`)

**Read first:** `src/reachability_resolver.rs` (`update` / `resolve` shape, interior-mutability via `Mutex`/`RwLock`).

- [ ] **Step 1: Write the failing tests:**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_relay_announce::{CommunityRelayAnnouncePayload, CommunityRelayEntry};
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

    fn hlc(ms: u64) -> Hlc { Hlc { wall_ms: ms, logical: 0, device_id: "d".into() } }
    fn payload(seed: u8, ad_at: u64) -> CommunityRelayAnnouncePayload {
        CommunityRelayAnnouncePayload {
            relay: CommunityRelayEntry {
                relay_device_id: [seed; 16],
                iroh_endpoint_id: [seed; 32],
                relay_device_ed25519_verify: [seed; 32],
                home_relay: "https://r/".into(),
            },
            ad_at,
            identity_signature: [0; 64],
        }
    }

    #[test]
    fn update_then_read_returns_fresh_entries() {
        let r = CommunityRelayResolver::new();
        let c = SpaceId([0xCC; 16]);
        let now = 1_700_000_000_000;
        r.update(c, OwnerAddr([0x01; 16]), payload(0x01, now), hlc(now));
        let got = r.relays_for_community(&c, now);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].relay_device_id, [0x01; 16]);
    }

    #[test]
    fn newer_ad_at_replaces_older_for_same_advertiser() {
        let r = CommunityRelayResolver::new();
        let c = SpaceId([0xCC; 16]);
        let a = OwnerAddr([0x01; 16]);
        let now = 1_700_000_000_000;
        r.update(c, a, payload(0x01, now), hlc(now));
        r.update(c, a, payload(0x02, now + 1), hlc(now + 1));   // newer
        r.update(c, a, payload(0x03, now - 1), hlc(now - 1));   // older — ignored
        let got = r.relays_for_community(&c, now + 1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].relay_device_id, [0x02; 16]);
    }

    #[test]
    fn stale_entries_filtered_on_read() {
        let r = CommunityRelayResolver::new();
        let c = SpaceId([0xCC; 16]);
        let old = 1_000_000_000_000;
        r.update(c, OwnerAddr([0x01; 16]), payload(0x01, old), hlc(old));
        let way_later = old + crate::community_relay_announce::COMMUNITY_RELAY_AD_FRESHNESS_MS + 1;
        assert!(r.relays_for_community(&c, way_later).is_empty());
    }

    #[test]
    fn read_caps_to_max_advertisers_keeping_freshest() {
        let r = CommunityRelayResolver::new();
        let c = SpaceId([0xCC; 16]);
        let now = 1_700_000_000_000;
        for i in 0..(crate::community_relay_announce::COMMUNITY_RELAY_ADVERTISERS_MAX as u8 + 2) {
            r.update(c, OwnerAddr([i; 16]), payload(i, now + i as u64), hlc(now + i as u64));
        }
        let got = r.relays_for_community(&c, now + 100);
        assert_eq!(got.len(), crate::community_relay_announce::COMMUNITY_RELAY_ADVERTISERS_MAX);
    }
}
```

- [ ] **Step 2: Run to verify they fail.**

- [ ] **Step 3: Implement.**

```rust
//! ZEB-458 P4 Phase B: in-memory resolver for CommunityRelayAnnounce ads.
//! Fed by an event-loop hook on applied CommunityRelayAnnounce events
//! (mirrors `ReachabilityResolver`); read by senders (D40) and the pull
//! driver (D39). Freshness + per-community cap applied on READ.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::community_relay_announce::{
    fresh_relay_entry, CommunityRelayAnnouncePayload, CommunityRelayEntry,
    COMMUNITY_RELAY_ADVERTISERS_MAX,
};
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

#[derive(Default)]
pub struct CommunityRelayResolver {
    // (community, advertiser) -> latest payload (LWW by ad_at).
    inner: Mutex<BTreeMap<(SpaceId, OwnerAddr), CommunityRelayAnnouncePayload>>,
}

impl CommunityRelayResolver {
    pub fn new() -> Self {
        Self { inner: Mutex::new(BTreeMap::new()) }
    }

    /// LWW by `ad_at`: a newer (or equal) stamp replaces; older is ignored.
    /// `_hlc` accepted for signature-parity with the reachability hook.
    pub fn update(
        &self,
        community_id: SpaceId,
        advertiser: OwnerAddr,
        payload: CommunityRelayAnnouncePayload,
        _hlc: Hlc,
    ) {
        let mut g = self.inner.lock().unwrap();
        let k = (community_id, advertiser);
        match g.get(&k) {
            Some(existing) if existing.ad_at >= payload.ad_at => {}
            _ => {
                g.insert(k, payload);
            }
        }
    }

    /// Fresh, capped advertiser entries for a community (freshest first).
    pub fn relays_for_community(&self, community_id: &SpaceId, now_ms: u64) -> Vec<CommunityRelayEntry> {
        let g = self.inner.lock().unwrap();
        let mut fresh: Vec<(u64, CommunityRelayEntry)> = g
            .iter()
            .filter(|((c, _), _)| c == community_id)
            .filter_map(|(_, p)| fresh_relay_entry(p, now_ms).map(|e| (p.ad_at, e)))
            .collect();
        fresh.sort_by(|a, b| b.0.cmp(&a.0)); // freshest first
        fresh.truncate(COMMUNITY_RELAY_ADVERTISERS_MAX);
        fresh.into_iter().map(|(_, e)| e).collect()
    }

    /// Drop all ads from an advertiser (opt-out / leave). Returns count removed.
    pub fn remove_advertiser(&self, community_id: &SpaceId, advertiser: &OwnerAddr) -> usize {
        let mut g = self.inner.lock().unwrap();
        let before = g.len();
        g.retain(|(c, a), _| !(c == community_id && a == advertiser));
        before - g.len()
    }
}
```

- [ ] **Step 4: Run to verify they pass + full per-task gate.**

- [ ] **Step 5: Commit** — `git commit -am "feat(zeb-458-p4b): CommunityRelayResolver (fresh+capped advertiser read)"`

---

## Task 4: `RelayOptInDoc` fleet-replicated opt-in CRDT (D43 storage)

**Files:**
- Create: `src-tauri/src/community_relay_optin.rs`
- Modify: `src-tauri/src/lib.rs` (`pub mod community_relay_optin;`)

**Read first:** `src/fleet_net.rs` `FleetNetDoc::merge_from` (LWW-by-stamp + `MergeOutcome`). `RelayOptInDoc` is the same LWW shape, keyed per community.

- [ ] **Step 1: Write the failing tests:**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::{Hlc, SpaceId};

    fn hlc(ms: u64, dev: &str) -> Hlc { Hlc { wall_ms: ms, logical: 0, device_id: dev.into() } }

    #[test]
    fn set_then_query_round_trips() {
        let mut d = RelayOptInDoc::default();
        let c = SpaceId([0xCC; 16]);
        assert!(!d.is_opted_in(&c));
        d.set(c, true, hlc(100, "a"));
        assert!(d.is_opted_in(&c));
        assert_eq!(d.opted_in_communities(), vec![c]);
    }

    #[test]
    fn later_stamp_wins_lww() {
        let mut d = RelayOptInDoc::default();
        let c = SpaceId([0xCC; 16]);
        d.set(c, true, hlc(100, "a"));
        d.set(c, false, hlc(50, "a"));   // older — ignored
        assert!(d.is_opted_in(&c));
        d.set(c, false, hlc(200, "a"));  // newer — wins
        assert!(!d.is_opted_in(&c));
    }

    #[test]
    fn merge_from_is_lww_and_reports_change() {
        let mut local = RelayOptInDoc::default();
        let c = SpaceId([0xCC; 16]);
        local.set(c, true, hlc(100, "a"));
        let mut remote = RelayOptInDoc::default();
        remote.set(c, false, hlc(200, "b")); // newer opt-out from sibling
        let out = local.merge_from(remote);
        assert!(out.changed);
        assert!(!local.is_opted_in(&c));
    }
}
```

- [ ] **Step 2: Run to verify they fail.**

- [ ] **Step 3: Implement.** (Reuse the project's `MergeOutcome` type — the same one `RelayHoldDoc::merge_from` / `FleetNetDoc::merge_from` return; import it from wherever they do.)

```rust
//! ZEB-458 P4 Phase B: per-community relay opt-in, fleet-replicated so every
//! online device of a volunteering owner advertises + serves (D43). LWW per
//! community by HLC, mirroring FleetNetDoc.

use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};

use crate::owner_state_types::{Hlc, SpaceId};
use crate::fleet_sync::MergeOutcome; // same MergeOutcome RelayHoldDoc uses — adjust import to the real path

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayOptInState {
    #[serde(rename = "oi")]
    pub opted_in: bool,
    #[serde(rename = "st")]
    pub stamp: Hlc,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayOptInDoc {
    #[serde(rename = "co")]
    pub communities: BTreeMap<SpaceId, RelayOptInState>,
}

impl RelayOptInDoc {
    pub fn set(&mut self, community_id: SpaceId, opted_in: bool, stamp: Hlc) {
        match self.communities.get(&community_id) {
            Some(s) if s.stamp.is_strictly_newer_than(&stamp) || s.stamp == stamp => {}
            _ => { self.communities.insert(community_id, RelayOptInState { opted_in, stamp }); }
        }
    }
    pub fn is_opted_in(&self, community_id: &SpaceId) -> bool {
        self.communities.get(community_id).map(|s| s.opted_in).unwrap_or(false)
    }
    pub fn opted_in_communities(&self) -> Vec<SpaceId> {
        self.communities.iter().filter(|(_, s)| s.opted_in).map(|(c, _)| *c).collect()
    }
    /// LWW merge by per-community stamp. Returns `changed=true` if any entry moved.
    pub fn merge_from(&mut self, remote: RelayOptInDoc) -> MergeOutcome {
        let mut changed = false;
        for (c, rs) in remote.communities {
            match self.communities.get(&c) {
                Some(ls) if ls.stamp.is_strictly_newer_than(&rs.stamp) || ls.stamp == rs.stamp => {}
                _ => { self.communities.insert(c, rs); changed = true; }
            }
        }
        MergeOutcome { changed }
    }
}
```

  Note for implementer: confirm `MergeOutcome`'s real constructor/fields (it may need additional fields beyond `changed` — match `RelayHoldDoc::merge_from`'s return exactly) and `Hlc::is_strictly_newer_than` (used by `community_relay_hold_crdt.rs`). Adjust the `use` path accordingly.

- [ ] **Step 4: Run to verify they pass + full per-task gate.**

- [ ] **Step 5: Commit** — `git commit -am "feat(zeb-458-p4b): RelayOptInDoc — fleet-replicated per-community opt-in (LWW)"`

---

## Task 5: `find_shared_communities` + `CommunityRelayDepositClient` trait (D40 sender-side logic)

**Files:**
- Modify: `src-tauri/src/community_relay.rs` (add the pure fn + trait at the end, before `#[cfg(test)]`)

- [ ] **Step 1: Write the failing test** (in `community_relay.rs` tests):

```rust
#[test]
fn find_shared_communities_includes_only_mutually_joined_community_spaces() {
    use crate::owner_state_types::{OwnerAddr, SpaceId, SpaceKind};
    let self_o = OwnerAddr([0x01; 16]);
    let r = OwnerAddr([0x02; 16]);
    let c_both = SpaceId([0xC1; 16]);     // both joined -> included
    let c_self_only = SpaceId([0xC2; 16]); // only self joined -> excluded
    let dm = SpaceId([0xD0; 16]);          // not a community -> excluded

    // Build a minimal OwnerState with three spaces; helper closures report
    // kind + joined-ness. (Construct OwnerState via the existing test helper
    // used elsewhere in this file's tests for spaces; if none, build the
    // SpaceKind map inline.)
    let kinds = |s: &SpaceId| -> SpaceKind {
        if *s == dm { SpaceKind::Dm } else { SpaceKind::Community }
    };
    let joined = |s: &SpaceId, who: &OwnerAddr| -> bool {
        match (*s, *who) {
            (x, _) if x == c_both => true,
            (x, w) if x == c_self_only => w == self_o,
            _ => false,
        }
    };
    let space_ids = vec![c_both, c_self_only, dm];
    let got = find_shared_communities(&space_ids, &self_o, &r, kinds, joined);
    assert_eq!(got, vec![c_both]);
}
```

- [ ] **Step 2: Run to verify it fails.**

- [ ] **Step 3: Implement.** Keep `find_shared_communities` PURE (closures abstract the registry lookup, so it is lib-testable; the prod client supplies real closures in T9):

```rust
use crate::owner_state_types::SpaceKind;

/// Communities where BOTH `self_owner` and `recipient` are Joined members
/// (D40 gate). Pure: `kind_of` and `is_joined` abstract the community-state
/// registry so this is unit-testable. The caller passes the sender's own
/// space-id list (`OwnerState.spaces` keys).
pub fn find_shared_communities(
    space_ids: &[SpaceId],
    self_owner: &OwnerAddr,
    recipient: &OwnerAddr,
    kind_of: impl Fn(&SpaceId) -> SpaceKind,
    is_joined: impl Fn(&SpaceId, &OwnerAddr) -> bool,
) -> Vec<SpaceId> {
    space_ids
        .iter()
        .filter(|s| kind_of(s) == SpaceKind::Community)
        .filter(|s| is_joined(s, self_owner) && is_joined(s, recipient))
        .copied()
        .collect()
}

/// Sender-side last-resort rung (D40). Mirrors the butler deposit client: given
/// the same outbox candidate the butler rung uses, seal the DepositPayload to
/// R's advertised butler-set device(s) and deposit to a relay in a shared
/// community. Returns true iff at least one relay acked. Never touches
/// AttemptState (the caller treats an acked candidate as delivered-pending-pull).
#[async_trait::async_trait]
pub trait CommunityRelayDepositClient: Send + Sync {
    async fn deposit(&self, req: &crate::butler_deposit::ButlerDepositRequest) -> bool;
}
```

- [ ] **Step 4: Run to verify it passes + full per-task gate.** (If `OwnerState`/`SpaceKind` construction in the test needs an existing helper, use the one the file's other tests use; otherwise the closures keep the test free of `OwnerState`.)

- [ ] **Step 5: Commit** — `git commit -am "feat(zeb-458-p4b): find_shared_communities + CommunityRelayDepositClient trait (D40)"`

---

## Task 6: Sender rung in the outbox drain (D40 integration)

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` — add `community_relay_deposit_client: Option<Arc<dyn CommunityRelayDepositClient>>` field on the outbox struct (beside `butler_deposit_client`), a `set_community_relay_deposit_client` setter (beside `set_butler_deposit_client` ~540), and the rung in the `drain_lifted` spawned task (after the butler rung ~2109–2188).

**Read first:** the butler rung block in `drain_lifted` and the `ButlerDepositClient` trait + its `deposit` return type in `butler_deposit.rs`. The relay rung fires ONLY when the butler deposit did not produce an ack for that candidate. If `ButlerDepositClient::deposit` currently returns `()`, change it (and the relay trait) to return a `bool` (or a small `enum DepositOutcome { Acked, NoAck }`) so the fallthrough is explicit; update the butler client impl + its tests accordingly (keep the change minimal and symmetric).

- [ ] **Step 1: Write the failing test** (in `dm_outbox.rs` tests; use mock clients):

```rust
// Mocks recording calls.
struct MockButler { acked: bool, calls: std::sync::Arc<std::sync::Mutex<Vec<OutboxEntryId>>> }
#[async_trait::async_trait]
impl crate::butler_deposit::ButlerDepositClient for MockButler {
    async fn deposit(&self, req: &crate::butler_deposit::ButlerDepositRequest) -> bool {
        self.calls.lock().unwrap().push(req.entry_id.clone());
        self.acked
    }
}
struct MockRelay { calls: std::sync::Arc<std::sync::Mutex<Vec<OutboxEntryId>>> }
#[async_trait::async_trait]
impl crate::community_relay::CommunityRelayDepositClient for MockRelay {
    async fn deposit(&self, req: &crate::butler_deposit::ButlerDepositRequest) -> bool {
        self.calls.lock().unwrap().push(req.entry_id.clone());
        true
    }
}

#[tokio::test]
async fn relay_rung_fires_only_after_butler_no_ack() {
    // Arrange an outbox + state where a candidate has reached
    // DEPOSIT_NOACK_WINDOWS (so it becomes a deposit candidate). Inject a
    // MockButler{acked:false} and a MockRelay. Drive the drain. Assert:
    //   - butler.deposit called once for the candidate,
    //   - relay.deposit called once for the SAME candidate (butler no-ack),
    //   - AttemptState for the entry is unchanged by either rung.
}

#[tokio::test]
async fn relay_rung_skipped_when_butler_acks() {
    // Same setup, MockButler{acked:true}. Assert relay.deposit NOT called.
}
```

  Model the arrange/drive on the existing butler-rung test in `dm_outbox.rs` (search `push_deposit_candidate` / `butler_deposit_client` in tests) — clone its harness and add the relay mock + assertions.

- [ ] **Step 2: Run to verify they fail.**

- [ ] **Step 3: Implement.**
  - Add the field + `set_community_relay_deposit_client`.
  - In the `drain_lifted` spawned task, after the butler deposit loop, change the loop so that for each candidate: `let acked = butler_client.deposit(&c).await; if !acked { if let Some(relay) = &community_relay_client { relay.deposit(&c).await; } }`. Capture `community_relay_client` into the spawned task the same way `butler_deposit_client` is captured. Preserve the rung order direct → butler → relay, and never mutate `AttemptState` from the relay rung (the relay outcome is informational, exactly like the butler rung).

- [ ] **Step 4: Run to verify they pass + full per-task gate.**

- [ ] **Step 5: Commit** — `git commit -am "feat(zeb-458-p4b): outbox relay rung — last-resort after butler no-ack (D40)"`

---

## Task 7: `ProdRelayDepositCtx` + `ProdRelayPullCtx` (D36/D39 production ctxs)

**Files:**
- Create: `src-tauri/src/community_relay_prod.rs`
- Modify: `src-tauri/src/lib.rs` (`pub mod community_relay_prod;`)

**Read first:** the `RelayDepositCtx` + `RelayPullCtx` trait method lists in `iroh_community_relay_acceptor.rs` (lines ~118–161, ~348–378), `both_joined_members` in `community_relay.rs`, `CommunitySyncRegistry::state_for` + `CommunityState::materialized(admin_addr)` (for the joined-member lookup), and `RelayHoldDoc` (`key`, `count_for_sender`, `live_count`, `merge_from`) + the `FleetSyncEngine<Doc>` pattern used by `DmInboxDoc` (for `persist_hold` / `held_for` / `mark_pulled`).

The two prod ctxs implement the traits against live handles: the relay's `Arc<Mutex<RelayHoldDoc>>` + its `FleetSyncEngine<RelayHoldDoc>` (for flush), the `CommunitySyncRegistry` (membership), the `RelayOptInDoc` (serves-community gate), and a clock. `persist_hold` enforces the caps atomically inside the doc-lock critical section (reuse the `RelayHoldDoc::count_for_sender`/`live_count` + the dm-inbox cap pattern), then flushes; `mark_pulled` unions `pulled_by` + flushes (GC is the separate sweep, NOT inline — per the Phase A contract).

- [ ] **Step 1: Write the failing tests** (in `community_relay_prod.rs` tests, using real `RelayHoldDoc` + a constructed `CommunityState` with two joined members):

```rust
#[tokio::test]
async fn deposit_ctx_serves_only_opted_in_joined_communities() { /* serves_community true iff opted-in AND relay is Joined; both_co_members true iff both Joined */ }

#[tokio::test]
async fn deposit_ctx_persist_enforces_per_sender_and_global_caps() { /* fill to RELAY_HOLD_PER_SENDER_CAP -> next rejected; occupied key bypasses caps (idempotent) */ }

#[tokio::test]
async fn pull_ctx_held_for_returns_only_recipient_entries_and_mark_pulled_unions() { /* held_for(R) returns R's entries; mark_pulled unions pulled_by, does NOT gc inline */ }
```

  Build the membership via the same `MaterializedMembership`/`MemberState` test helpers `community_relay.rs` tests use (`mint_test_owner`, `joined_member_state`). For the registry, either use a real `CommunitySyncRegistry` seeded with one community, or have `ProdRelay*Ctx` take a small `MembershipLookup` closure/trait so the ctx is unit-testable without a full registry — choose the closure/trait seam (cleaner + testable) and have T11 supply the real registry-backed closure.

- [ ] **Step 2: Run to verify they fail.**

- [ ] **Step 3: Implement** `ProdRelayDepositCtx` (impl `RelayDepositCtx`) and `ProdRelayPullCtx` (impl `RelayPullCtx`). Method-by-method against the trait signatures from Phase A:
  - `relay_device_id` → the relay's own device id string.
  - `serves_community(c)` → `optin.lock().contains(c)` AND relay self is `Joined` in `c`.
  - `both_co_members(c, s, r)` → load `c`'s `MaterializedMembership`, call `both_joined_members(&m, &OwnerAddr(*s), &OwnerAddr(*r))`.
  - `now_secs` / `mint_hlc` → clock + the HLC source used elsewhere.
  - `persist_hold(key, entry)` → lock the hold doc; if key occupied return `AlreadyHeld` verdict; else enforce `count_for_sender(community, sender) < RELAY_HOLD_PER_SENDER_CAP` and `live_count() < RELAY_HOLD_GLOBAL_CAP`, insert, drop lock, flush via the engine; return the verdict variant Phase A defines.
  - `is_joined_member(c, owner)` / `held_for(r)` / `mark_pulled(keys, dev)` for the pull ctx — `held_for` filters the hold doc by `recipient_owner == r` returning `(key, RelayHeldBlob{sender_owner, sealed_blob})`; `mark_pulled` unions `pulled_by += dev` for each key + flushes (no inline gc).

  Use a `MembershipLookup` trait/closure seam so tests don't need a full registry; T11 supplies the registry-backed impl.

- [ ] **Step 4: Run to verify they pass + full per-task gate.**

- [ ] **Step 5: Commit** — `git commit -am "feat(zeb-458-p4b): ProdRelayDepositCtx + ProdRelayPullCtx (caps, co-membership, recipient-scoped pull)"`

---

## Task 8: iroh acceptor shells + ALPN registration + `RelayPullAckFrame` envelope (D36/D39 transport)

**Files:**
- Modify: `src-tauri/src/iroh_community_relay_acceptor.rs` — add `IrohCommunityRelayDepositAcceptor` + `IrohCommunityRelayPullAcceptor` connection shells; add the self-contained `RelayPullAckFrame` envelope handling.
- Modify: `src-tauri/src/community_relay.rs` — add `RelayPullAckFrame { recipient_owner:[u8;16], community_id:SpaceId, requester_enrollment_cert:Vec<u8>, content_ids:Vec<[u8;32]>, sig:Vec<u8> }` + `encode_/decode_relay_pull_ack_frame` (symmetric with `RelayPullQuery`; the query already embeds cert+sig, so the ack mirrors it — D39 "Wire-envelope shape" decision: make the ack self-contained so `handle_relay_pull_ack`'s explicit params come straight off the frame).
- Modify: `src-tauri/src/iroh_endpoint.rs` — register both ALPNs in the `alpn` module (~46–68) and both `.alpns(vec![...])` lists (~106–113, ~358–363).
- Modify: `src-tauri/src/zenoh_iroh_transport.rs` — ALPN dispatch arms in `spawn_accept_loop` (after the butler-deposit arm ~424) + `install_community_relay_deposit_acceptor` / `install_community_relay_pull_acceptor` `OnceLock` seams (mirror `install_butler_deposit_acceptor` ~203–213).

**Read first:** the butler-deposit acceptor shell (`iroh_butler_acceptor.rs`) for the exact connection lifecycle (accept uni/bi stream, read length-delimited frame, decode, call core handler, encode response, write, close) and the `install_butler_deposit_acceptor` seam + its ALPN dispatch arm.

- [ ] **Step 1: Write the failing tests.** Wire-pin the new ack frame + round-trip (in `tests/wire_format_community_relay_fixtures.rs`):

```rust
// add to wire_format_community_relay_fixtures.rs
use harmony_app::community_relay::RelayPullAckFrame;
fn fixture_relay_pull_ack_frame() -> RelayPullAckFrame {
    RelayPullAckFrame {
        recipient_owner: [0x11; 16],
        community_id: SpaceId([0xCC; 16]),
        requester_enrollment_cert: vec![0xA1, 0xA2, 0xA3],
        content_ids: vec![[0x01; 32], [0x02; 32]],
        sig: vec![0x07; 64],
    }
}
#[test]
fn relay_pull_ack_frame_wire_bytes_pinned() {
    let f = fixture_relay_pull_ack_frame();
    let hex = hex::encode(harmony_app::owner_state_crypto::canonical_cbor_encode(&f).expect("encode"));
    eprintln!("relay_pull_ack_frame hex: {hex}");
    assert_eq!(hex, "<<CAPTURE_ON_FIRST_RUN>>", "RelayPullAckFrame wire changed — re-pin deliberately");
}
#[test]
fn relay_pull_ack_frame_round_trips() {
    use harmony_app::community_relay::{decode_relay_pull_ack_frame, encode_relay_pull_ack_frame};
    let f = fixture_relay_pull_ack_frame();
    assert_eq!(decode_relay_pull_ack_frame(&encode_relay_pull_ack_frame(&f).expect("e")).expect("d"), f);
}
```

  For the shells themselves, add a focused unit test in `iroh_community_relay_acceptor.rs` that drives the deposit-shell's frame-handling over an in-memory duplex if the butler acceptor has such a harness; otherwise the shells are exercised by the T12 E2E test (note this explicitly in the task's DONE report).

- [ ] **Step 2: Run to verify they fail.**

- [ ] **Step 3: Implement.**
  - Add `RelayPullAckFrame` + encode/decode + `CanonicalPayload` impls in `community_relay.rs` (mirror `RelayPullQuery`). Capture the pin.
  - Add the two acceptor shells mirroring the butler acceptor: each holds an `Arc<dyn RelayDepositCtx>` / `Arc<dyn RelayPullCtx>`, accepts the iroh connection, reads the frame (`decode_relay_deposit_frame` / `decode_relay_pull_query` / `decode_relay_pull_ack_frame`), calls the Phase A core handler (`handle_relay_deposit_core` / `handle_relay_pull_query` / `handle_relay_pull_ack` — passing the ack frame's `recipient_owner`/`community_id`/`requester_enrollment_cert`/`sig` as the explicit params), encodes the response (`RelayDepositAck` / `RelayPullResponse`; ack needs no response body beyond close/ok), writes, closes. The pull shell handles BOTH the query (returns blobs) and a subsequent ack on the same ALPN (recipient pulls then acks) — match on which frame type arrived, or use one round-trip per stream as the butler acceptor does.
  - Register ALPNs in `iroh_endpoint.rs` (both `alpn` consts + both builder lists).
  - Add dispatch arms in `zenoh_iroh_transport.rs` after the butler-deposit arm and the `OnceLock` install seams.

- [ ] **Step 4: Capture the ack-frame pin; run the wire-fixtures test binary + lib gate:**
  ```bash
  cargo nextest run --locked -p harmony-app --features test-fixtures --test wire_format_community_relay_fixtures 2>&1 | tail -20
  ```
  then the full per-task lib gate.

- [ ] **Step 5: Commit** — `git commit -am "feat(zeb-458-p4b): iroh relay acceptor shells + ALPN registration + self-contained pull-ack frame"`

---

## Task 9: `ProdRelayIngestCtx` + background pull driver (D39 retrieval)

**Files:**
- Modify: `src-tauri/src/community_relay_prod.rs` — add `ProdRelayIngestCtx` (impl `RelayIngestCtx`) and `ProdCommunityRelayDepositClient` (impl `CommunityRelayDepositClient`).
- Create: `src-tauri/src/community_relay_pull_driver.rs` — `CommunityRelayPullDriver`.
- Modify: `src-tauri/src/lib.rs` (`pub mod community_relay_pull_driver;`)

**Read first:** `RelayIngestCtx` (`device_x25519_privs` + `ingest_recovered`) + `open_and_ingest` in `community_relay_pull.rs`; the normal receive path (`verify_cidnotify_admission` / `apply_inbox` / `emit_dm_received`) the butler P1 ingest uses; `reachability_publisher.rs` for the spawn-loop shape. The D39 ack-after-durable-persist requirement: the driver MUST persist the ingested entry durably BEFORE sending the ack (so a crash between ack and persist cannot both GC the relay entry and lose the deposit).

- [ ] **Step 1: Write the failing tests:**

```rust
// ProdRelayIngestCtx: ingest_recovered routes a recovered DepositPayload through
// the same path the butler P1 ingest uses, and surfaces Err on failure (so the
// content id is NOT acked).
#[tokio::test]
async fn ingest_ctx_acks_only_after_successful_ingest() { /* a payload that ingests OK -> open_and_ingest returns its content_id; a payload that fails verification -> content_id absent from the returned acks */ }

// Driver: given a resolver with one fresh relay and a mock pull transport that
// returns one blob R can open, the driver opens+ingests+acks, and only acks
// after ingest succeeded.
#[tokio::test]
async fn pull_driver_queries_fresh_relays_ingests_and_acks_after_persist() { /* assert ack sent with the ingested content_id; assert NOT sent for an un-openable blob */ }
```

  Use a mock pull-transport trait (so the driver is testable without real iroh): `trait RelayPullTransport { async fn pull(&self, relay:&CommunityRelayEntry, query:&RelayPullQuery) -> Option<RelayPullResponse>; async fn ack(&self, relay:&CommunityRelayEntry, ack:&RelayPullAckFrame) -> bool; }`. T11 supplies the iroh-backed impl.

- [ ] **Step 2: Run to verify they fail.**

- [ ] **Step 3: Implement.**
  - `ProdRelayIngestCtx::device_x25519_privs` → the owner's enrolled device x25519 privs (`ed25519_priv_to_x25519` of each enrolled device signing key the node holds); `ingest_recovered(payload)` → run the SAME path the butler P1 deposit ingest uses (`verify_cidnotify_admission` + `apply_inbox` + `emit_dm_received`), returning `Err` on any verification/persist failure so the content id is not acked. The persist inside this path is the durable write; the driver acks only the ids `open_and_ingest` returns (which are exactly those that ingested OK).
  - `CommunityRelayPullDriver`: a `tokio::spawn` loop (mirror `reachability_publisher`): woken by a `Notify` (on coming online / new relay ad) + a periodic floor `interval`. Each pass: for each Joined community, `resolver.relays_for_community(c, now)`; for each fresh relay build a signed `RelayPullQuery` (cert + `relay_pull_sig_payload` sig), `transport.pull` → `open_and_ingest(&resp.entries, &ingest_ctx)` → if any content ids returned, build a signed `RelayPullAckFrame` (`relay_pull_ack_sig_payload`) and `transport.ack`. No locks held across awaits.
  - `ProdCommunityRelayDepositClient::deposit(req)` (the T5 trait): resolve R's fresh butler-set (the reachability resolver — same set the butler rung used); if empty, return false (no seal target — accepted gap). Compute `find_shared_communities` (T5 pure fn fed registry-backed closures); for each shared community resolve its relay-set; build the `DepositPayload` (cidnotify + storage blob from `req`); `build_relay_deposit_frame` sealed to each butler-set device vk; open an iroh deposit connection to each relay until one returns a `RelayDepositAck`; return true on first ack.

- [ ] **Step 4: Run to verify they pass + full per-task gate.**

- [ ] **Step 5: Commit** — `git commit -am "feat(zeb-458-p4b): ProdRelayIngestCtx + pull driver (ack-after-persist) + prod relay deposit client"`

---

## Task 10: Advertisement publisher + event-loop resolver hook + timers (D37 publish)

**Files:**
- Create: `src-tauri/src/community_relay_publisher.rs` — `CommunityRelayPublisher`.
- Modify: `src-tauri/src/lib.rs` (`pub mod community_relay_publisher;`)
- Modify: `src-tauri/src/event_loop.rs` — (a) feed freshly-applied `CommunityRelayAnnounce` events into `CommunityRelayResolver` (mirror the reachability hook ~405–411); (b) add a relay republish counter on the existing 250ms tick (mirror `ROUTING_REPUBLISH_TICKS`) that pokes the publisher; optionally poke the pull driver's `Notify` on community-state sync.

**Read first:** `reachability_publisher.rs` (the periodic signed-announce loop) and the reachability event-loop hook + `ROUTING_REPUBLISH_TICKS` timer arm (`event_loop.rs` ~2829, ~3150).

- [ ] **Step 1: Write the failing test** (publisher unit test):

```rust
#[tokio::test]
async fn publisher_emits_fresh_announce_for_each_opted_in_joined_community() {
    // Given an opt-in doc with community C opted-in and the node Joined in C,
    // one publish pass signs a CommunityRelayAnnounce (advertiser = self,
    // ad_at = now) and inserts it as a membership event into C's state. Assert
    // exactly one CommunityRelayAnnounce event was inserted with this node's
    // relay entry. Opting out (or leaving) emits no further ads (and the
    // resolver drops them on the freshness window).
}
```

  Drive the publisher with a mock "insert membership event" sink + a controllable clock, mirroring how `reachability_publisher` tests inject the insert path.

- [ ] **Step 2: Run to verify it fails.**

- [ ] **Step 3: Implement.**
  - `CommunityRelayPublisher`: a spawn-loop (mirror `reachability_publisher`) that on each tick reads `optin.opted_in_communities()`, filters to those the node is still `Joined` in, and for each signs a fresh `CommunityRelayAnnouncePayload` (`build_signed_community_relay_announce` with the node's enrolled device key, `ad_at = now`, the node's own `CommunityRelayEntry`) and inserts a `MembershipEventKind::CommunityRelayAnnounce` event into that community's state via the same insert path `ReachabilityAnnounce` uses. On opt-out / leave: stop advertising (let the ad go stale; the freshness window + `remove_advertiser` handle retraction).
  - event_loop hook: where applied membership events are dispatched, add the `CommunityRelayAnnounce { payload }` case feeding `resolver.update(community_id, event.actor, payload, event.at)` (mirror the reachability `.update` hook).
  - Timer: add a `RELAY_REPUBLISH_TICKS = COMMUNITY_RELAY_AD_REFRESH_MS / 250` counter on the 250ms tick that pokes the publisher's `Notify` (mirror `ROUTING_REPUBLISH_TICKS`).

- [ ] **Step 4: Run to verify it passes + full per-task gate.**

- [ ] **Step 5: Commit** — `git commit -am "feat(zeb-458-p4b): relay-announce publisher + event-loop resolver hook + republish timer"`

---

## Task 11: `start_node` install + opt-in IPCs (D43 lifecycle wiring)

**Files:**
- Modify: `src-tauri/src/lib.rs` — `NodeState` fields; `start_node_inner` install; `set_community_relay_opt_in` / `get_community_relay_status` IPCs; `invoke_handler` registration.

**Read first:** the butler-deposit acceptor install (`~6259–6293`) + client inject (`~6307–6339`), `NodeState` fleet-net handles (`~989–1015`), `set_butler_pin_inner`/`set_butler_pin` (`~42193`/`~42224`) + its `notify_dirty()`/`flush_now()`/`routing_republish` pattern, and the `invoke_handler` list (`~42740`). The `FleetSyncEngine<DmInboxDoc>` creation in `start_node` is the model for the two new engines. **ZEB-426 hazard:** do all wiring as Arc-clones + `tokio::spawn`; NEVER inline-await anything that routes through an event-loop-serviced channel (CAS bridge, crdt_state) in `start_node` — the pull driver + publisher MUST be spawned, with their handles installed (no awaits) before the event loop starts.

- [ ] **Step 1: Write the failing test** (IPC core, testable seam like `set_butler_pin_inner`):

```rust
#[tokio::test]
async fn set_community_relay_opt_in_inner_persists_and_round_trips() {
    // set_community_relay_opt_in_inner(doc, community, true, now) flips the
    // RelayOptInDoc; get_community_relay_status_inner reflects it; a later
    // opt-out with a newer stamp flips it back (LWW). Mirror
    // set_butler_pin_inner's test.
}
```

- [ ] **Step 2: Run to verify it fails.**

- [ ] **Step 3: Implement.**
  - `NodeState` fields (mirror the fleet-net handle cluster): `relay_hold_doc: Option<Arc<Mutex<RelayHoldDoc>>>`, `relay_hold_sync: Option<Arc<FleetSyncEngine<RelayHoldDoc>>>`, `relay_optin_doc: Option<Arc<Mutex<RelayOptInDoc>>>`, `relay_optin_sync: Option<Arc<FleetSyncEngine<RelayOptInDoc>>>`, `community_relay_resolver: Option<Arc<CommunityRelayResolver>>`.
  - In `start_node_inner` (owner-loaded block): create the two `FleetSyncEngine`s (dataset names `relay-hold-v1` / `relay-optin-v1`, mirroring `dm-inbox-v1`), load persisted docs, build the registry-backed `MembershipLookup` closure, construct `ProdRelayDepositCtx` / `ProdRelayPullCtx` / `ProdRelayIngestCtx`, wrap deposit+pull ctxs in the two iroh acceptor shells, install via the `OnceLock` seams, build the iroh-backed `RelayPullTransport` + `ProdCommunityRelayDepositClient`, inject the client into the outbox (`set_community_relay_deposit_client`), and `tokio::spawn` the publisher + pull driver (store their `Notify` handles for the event-loop pokes). Store all handles on `NodeState`. All of this is Arc-clones + spawns — no inline awaits on event-loop channels.
  - IPCs: `set_community_relay_opt_in_inner(doc, community_id, opted_in, now_ms)` mutates the `RelayOptInDoc` (LWW) → wrapper snapshots handles, calls inner, `notify_dirty()` + `flush_now()` on the optin engine, and pokes the publisher `Notify` (immediate advertise on opt-in / nothing further on opt-out). `get_community_relay_status_inner(doc, community_id) -> bool`. Register both `#[tauri::command]`s in the `invoke_handler` list.

- [ ] **Step 4: Run to verify it passes + full per-task lib gate.** (The app must compile fully here — this is the big integration task. If clippy flags unused handles, wire them rather than `#[allow]`.)

- [ ] **Step 5: Commit** — `git commit -am "feat(zeb-458-p4b): start_node install (acceptors/driver/publisher/client) + opt-in IPCs"`

---

## Task 12: Three-engine E2E integration test + final sweep (D45)

**Files:**
- Modify: `src-tauri/tests/community_relay_integration.rs` — add a third "relay" engine to the harness; the three E2E scenarios.

**Read first:** the existing two-engine butler harness this test (and the butler integration tests) use, and the Phase A `community_relay_integration.rs` engine-backed test, for how to spin engines + drive the deposit/pull paths.

- [ ] **Step 1: Write the failing E2E tests:**

```rust
#[tokio::test]
async fn e2e_relay_happy_path_sender_to_offline_recipient_via_relay() {
    // S, R, and a Relay (all co-members of community C; R's butler-set
    // advertised but unreachable). S sends a DM to R; the direct + first-party
    // butler paths fail; the outbox relay rung deposits the sealed blob to the
    // Relay (admitted, held OPAQUE). Bring R's pull driver online: it queries
    // the Relay, opens its copy, ingests via the normal receive path, acks.
    // Assert: R received exactly the message; the Relay's RelayHoldDoc never
    // exposed plaintext (assert the held sealed_blob does NOT decode as a
    // DepositPayload with the relay's keys); after ack+sweep the entry is GC'd.
}

#[tokio::test]
async fn e2e_relay_rejects_non_member_sender() {
    // A sender who is NOT a Joined member of C deposits -> relay rejects,
    // RelayHoldDoc stays empty.
}

#[tokio::test]
async fn e2e_held_blob_survives_relay_restart_and_is_still_pullable() {
    // Deposit; drop+reload the relay engine (persisted RelayHoldDoc); R pulls
    // successfully after the restart.
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo nextest run --locked -p harmony-app --features test-fixtures --test community_relay_integration 2>&1 | tail -40`.

- [ ] **Step 3: Implement** the harness extension + scenarios (drive the real acceptor shells + pull driver + outbox rung end-to-end across three engines).

- [ ] **Step 4: Run the integration binary, then the FULL sweep** (background if > 10 min foreground; heartbeat safety net):
  ```bash
  set -o pipefail
  cd src-tauri
  cargo fmt --all -- --check
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
  cargo nextest run --locked --workspace --all-targets --features test-fixtures
  ```
  Expected: all green (known-unrelated flakes — `serve_core_drives_full_flow_over_http_and_ws` api_server contention = ZEB-374; transport orphan-flakes — are NOT in this PR's blast radius; re-run the specific job if only those fail).

- [ ] **Step 5: Commit** — `git commit -am "test(zeb-458-p4b): three-engine E2E — happy path + non-member reject + relay-restart survival"`

---

## Self-Review

**Spec coverage (D35–D45):**
- D35 (seal to R's butler-set device key) — reused from Phase A `build_relay_deposit_frame`; the prod deposit client (T9) seals to the butler-set device vks. ✓
- D36 (admission gate) — `ProdRelayDepositCtx` (T7) + acceptor shell (T8) drive the Phase A `handle_relay_deposit_core`. ✓
- D37 (discovery: announce + resolver) — payload module (T1) + membership event (T2) + resolver (T3) + publisher (T10). ✓
- D38 (holding store caps/TTL/GC) — Phase A `RelayHoldDoc`; prod persist enforces caps (T7). ✓
- D39 (pull retrieval + ack-after-persist) — ingest ctx + pull driver (T9) + pull acceptor shell + self-contained ack frame (T8). ✓
- D40 (sender rung) — `find_shared_communities` + client trait (T5) + outbox rung (T6) + prod client (T9). ✓
- D41 (wire) — Phase A pinned deposit/query/response; T8 adds `RelayPullAckFrame` pin; T1 pins the announce payload. ✓
- D42 (DM/group-DM scope) — the rung reuses the `DepositPayload` shape; no channel relaying added. ✓
- D43 (opt-in lifecycle + trust scoping) — `RelayOptInDoc` (T4) + IPCs/install (T11) + `serves_community` gate (T7). ✓
- D44 (deferred hardening) — out of scope; file the follow-up ticket at PR time (note below). ✓ (tracked, not built)
- D45 (test plan) — unit tests across T1–T11; the three integration scenarios + wire pins in T12. ✓

**Placeholder scan:** the only intentional `<<CAPTURE_ON_FIRST_RUN>>` tokens are the two byte-pin literals (T1 announce payload, T8 ack frame), captured on first run per the documented re-pin procedure — that is the established fixture pattern, not a gap. The fuzzy integration points (T6 butler-client return type, T7 membership-lookup seam, T11 handle wiring) carry explicit "read first" + intent + complete tests rather than fabricated line-exact code, because they integrate against mapped-but-large existing code the implementer reads live.

**Type consistency:** `CommunityRelayEntry` / `CommunityRelayAnnouncePayload` (T1) are consumed by the resolver (T3), publisher (T10), and event hook (T10); `RelayOptInDoc` (T4) by ctxs (T7), IPCs (T11), publisher (T10); `CommunityRelayDepositClient` (T5) implemented in T9, consumed in T6; `RelayPullAckFrame` (T8) produced by the driver (T9), consumed by the pull acceptor (T8). `MergeOutcome` / `Hlc::is_strictly_newer_than` flagged in T4 for the implementer to bind to the real types. Names are consistent across tasks.

**D44 follow-up:** at PR-open, file the Linear follow-up "P4 hardening: unlinkable community relay (sealed-sender + UCAN + PoW + timing padding)" related-to ZEB-458 (do NOT invent the ID; let Linear assign).
