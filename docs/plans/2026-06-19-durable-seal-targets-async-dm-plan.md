# Durable seal-targets for async DM delivery — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.
>
> **Repo rule:** Do NOT use markdown `- [ ]` checkbox TODO tracking. Steps are plain **bold numbered** items. Track progress in TodoWrite, not in this file.
>
> **Linear hygiene:** Keep `ZEB-NNN` out of the PR title/body/branch/commit messages (Linear auto-closes every ZEB-NNN in a merged PR body, incl. parents). Cross-reference issues in PR *comments*. This plan file may reference them freely.

**Goal:** Make a recipient's deposit seal-targets durably resolvable while the recipient is fully offline, for both the butler rung and the community-relay rung, by carrying the butler-set in the durable community CRDT (inner-sig-covered) and adding an enrolled-device-vk fallback for the relay rung.

**Architecture:** Two parts. **Part 1 (butler rung):** the production CRDT `ReachabilityAnnounce` publisher now builds + carries the owner's butler-set (vk + endpoint), the inner identity signature covers it, and a per-source freshness policy exempts durable-CRDT butler-sets from the 15-min pkarr window. **Part 2 (relay rung):** the relay deposit client falls back to ≤2 enrolled-device ed25519 vks from durable community membership when no butler-set exists, so a butler-less recipient is servable. Closes ZEB-488 (relay) + ZEB-493 (butler).

**Tech Stack:** Rust (src-tauri Tauri app), `ciborium` CBOR, `ed25519-dalek`, `curve25519-dalek` (birational vk→x25519), `cargo-nextest`, e2e-harness (`e2e_two_node.rs`).

**Spec:** `docs/specs/2026-06-19-durable-seal-targets-async-dm-design.md` (Approved).

**Branch:** `durable-seal-targets-async-dm` (on latest `main` `ad5fdbec`, carries the spec).

**Full gate (run from `src-tauri/`):**
```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

---

## File structure

| File | Responsibility | Change |
|---|---|---|
| `src-tauri/src/reachability_record.rs` | `ReachabilityAnnouncePayload`, inner-sig, butler-set readers | Extend inner-sig preimage to cover `butler_set`+`bs_at`; add `butler_set`/`bs_at` params to the two builders; add `durable_butler_set` (window-exempt reader); extend tests |
| `src-tauri/src/lib.rs` (`start_node_inner`, `publish_fn` ~6404) | Production CRDT reachability publisher | Build + carry the durable butler-set per publish |
| `src-tauri/src/reachability_resolver.rs` | Resolver cache + resolve | Internal `ReachabilitySource` tag; `update_with_source`; `resolve(_async)_with_source`; tag pkarr-cache-back `PkarrLive` |
| `src-tauri/src/butler_deposit.rs` | Butler deposit client + freshness selectors | Add `freshest_butler_set_by_source`; rewire `IrohButlerDepositClient::deposit` to the source-aware path |
| `src-tauri/src/community_relay_prod.rs` | Relay deposit client + butler-set resolve | Rewire `ReachabilityButlerSetResolve` to source-aware; add enrolled-device fallback in `ProdCommunityRelayDepositClient::deposit` |
| `src-tauri/tests/wire_format/reachability_announce_fixtures.rs` | Pinned wire fixtures | Add a signed-butler-set round-trip fixture (existing struct pins unchanged) |
| `e2e-harness/tests/e2e_two_node.rs` | s6/s7 scenarios | Publish+sync reachability while online before kill; promote HELD/RECV/CLEARED to hard asserts |

---

## Task 1: Inner identity signature covers `butler_set` + `bs_at`

**Files:**
- Modify: `src-tauri/src/reachability_record.rs:178-212` (`inner_signed_bytes`), `:217-244` (`build_signed_payload`), `:254-282` (`build_signed_payload_with_key`), `:287-306` (`verify_inner_signature`)
- Modify (call sites): `src-tauri/src/lib.rs:6468` (prod — pass empty for now), `src-tauri/src/lib.rs:~50889` (`make_signed_announce` test helper)
- Test: `src-tauri/src/reachability_record.rs` (mutation + round-trip tests), `src-tauri/tests/wire_format/reachability_announce_fixtures.rs`

Background: the pkarr routing blob zero-fills `identity_signature` (it never calls `inner_signed_bytes`), so this preimage change affects ONLY the durable CRDT `ReachabilityAnnounce` records. Existing struct-encoding wire fixtures use a hardcoded sig and stay byte-identical. This is the spec's accepted alpha flag-day (a CRDT record signed before this change won't verify after).

**Step 1: Write the failing test — tampering the butler-set breaks the inner sig.**

Add to the `tests` module in `src-tauri/src/reachability_record.rs` (near `inner_sig_rejects_tampered_node_id`, ~line 589):

```rust
    #[test]
    fn inner_sig_covers_butler_set_and_rejects_tamper() {
        let identity = PrivateIdentity::from_seed(&[0xAA; 32]);
        let public = identity.public_identity();
        let actor = OwnerAddr(public.address_hash);
        let hlc = fixture_hlc();
        let butler_set = vec![fixture_butler_entry(0x10)];
        let bs_at = 1_700_000_000_000u64;
        let p = build_signed_payload(
            [0xAB; 32],
            "https://derp.example/".into(),
            vec![],
            1_700_000_000_000,
            &actor,
            &hlc,
            butler_set.clone(),
            bs_at,
            &identity,
        )
        .expect("sign");
        // Valid as signed.
        verify_inner_signature(&p, &actor, &hlc, &public.verifying_key).expect("verify");
        // Tamper a butler-set entry's vk -> inner sig must now fail.
        let mut tampered = p.clone();
        tampered.butler_set[0].device_ed25519_verify[0] ^= 0xFF;
        assert_eq!(
            verify_inner_signature(&tampered, &actor, &hlc, &public.verifying_key),
            Err(InnerSigError::Invalid)
        );
        // Strip the butler-set -> inner sig must fail (can't strip seal-targets).
        let mut stripped = p.clone();
        stripped.butler_set.clear();
        stripped.bs_at = 0;
        assert_eq!(
            verify_inner_signature(&stripped, &actor, &hlc, &public.verifying_key),
            Err(InnerSigError::Invalid)
        );
    }
```

**Step 2: Run it to confirm it fails to COMPILE** (the builder signatures don't take `butler_set`/`bs_at` yet):

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(inner_sig_covers_butler_set_and_rejects_tamper)'
```
Expected: compile error — `build_signed_payload` takes 7 args, not 9.

**Step 3: Extend `inner_signed_bytes` to cover `butler_set` + `bs_at`.**

In `src-tauri/src/reachability_record.rs`, change the signature and `InnerSigInput` (lines 178-212). Add two 2-char-keyed fields `bs` (butler_set) and `ba` (bs_at):

```rust
pub fn inner_signed_bytes(
    iroh_node_id: &[u8; 32],
    home_relay_url: &str,
    direct_addresses: &[SocketAddr],
    announced_at_ms: u64,
    actor: &OwnerAddr,
    hlc: &Hlc,
    butler_set: &[ButlerSetEntry],
    bs_at: u64,
) -> Result<Vec<u8>, CryptoError> {
    #[derive(Serialize)]
    struct InnerSigInput<'a> {
        #[serde(rename = "nd", serialize_with = "serialize_bytes_as_bstr")]
        nd: &'a [u8; 32],
        #[serde(rename = "rl")]
        rl: &'a str,
        #[serde(rename = "da")]
        da: &'a [SocketAddr],
        #[serde(rename = "ts")]
        ts: u64,
        #[serde(rename = "ac")]
        ac: &'a OwnerAddr,
        #[serde(rename = "hl")]
        hl: &'a Hlc,
        // ZEB-493 D4: bind the recipient's own seal-targets so a co-member or
        // relay cannot forge or strip them. Always present in the preimage
        // (empty slice / 0 when no butler-set) — flag-day vs pre-change records.
        #[serde(rename = "bs")]
        bs: &'a [ButlerSetEntry],
        #[serde(rename = "ba")]
        ba: u64,
    }
    let input = InnerSigInput {
        nd: iroh_node_id,
        rl: home_relay_url,
        da: direct_addresses,
        ts: announced_at_ms,
        ac: actor,
        hl: hlc,
        bs: butler_set,
        ba: bs_at,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&input, &mut buf).map_err(|e| CryptoError::CborEncode(format!("{e}")))?;
    Ok(buf)
}
```

(`ButlerSetEntry` derives `Serialize` in declaration order, so the preimage is deterministic — same property the existing fields rely on.)

**Step 4: Thread `butler_set` + `bs_at` through the two builders.**

`build_signed_payload` (lines 217-244) — add params before `identity`, pass `&butler_set` to `inner_signed_bytes`, move into the payload:

```rust
pub fn build_signed_payload(
    iroh_node_id: [u8; 32],
    home_relay_url: String,
    direct_addresses: Vec<SocketAddr>,
    announced_at_ms: u64,
    actor: &OwnerAddr,
    hlc: &Hlc,
    butler_set: Vec<ButlerSetEntry>,
    bs_at: u64,
    identity: &harmony_identity::PrivateIdentity,
) -> Result<ReachabilityAnnouncePayload, CryptoError> {
    let inner = inner_signed_bytes(
        &iroh_node_id,
        &home_relay_url,
        &direct_addresses,
        announced_at_ms,
        actor,
        hlc,
        &butler_set,
        bs_at,
    )?;
    let sig = identity.sign(&inner);
    Ok(ReachabilityAnnouncePayload {
        iroh_node_id,
        home_relay_url,
        direct_addresses,
        announced_at_ms,
        identity_signature: sig,
        butler_set,
        bs_at,
    })
}
```

`build_signed_payload_with_key` (lines 254-282) — identical param additions, signing via `signing_key.sign(&inner).to_bytes()`:

```rust
pub fn build_signed_payload_with_key(
    iroh_node_id: [u8; 32],
    home_relay_url: String,
    direct_addresses: Vec<SocketAddr>,
    announced_at_ms: u64,
    actor: &OwnerAddr,
    hlc: &Hlc,
    butler_set: Vec<ButlerSetEntry>,
    bs_at: u64,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<ReachabilityAnnouncePayload, CryptoError> {
    use ed25519_dalek::Signer;
    let inner = inner_signed_bytes(
        &iroh_node_id,
        &home_relay_url,
        &direct_addresses,
        announced_at_ms,
        actor,
        hlc,
        &butler_set,
        bs_at,
    )?;
    let sig = signing_key.sign(&inner).to_bytes();
    Ok(ReachabilityAnnouncePayload {
        iroh_node_id,
        home_relay_url,
        direct_addresses,
        announced_at_ms,
        identity_signature: sig,
        butler_set,
        bs_at,
    })
}
```

**Step 5: Update `verify_inner_signature` to pass the payload's butler-set into the preimage.**

`src-tauri/src/reachability_record.rs:287-306` — add the two args from `p`:

```rust
    let bytes = inner_signed_bytes(
        &p.iroh_node_id,
        &p.home_relay_url,
        &p.direct_addresses,
        p.announced_at_ms,
        actor,
        hlc,
        &p.butler_set,
        p.bs_at,
    )
    .map_err(|_| InnerSigError::Encode)?;
```

**Step 6: Fix the non-prod call sites to pass empty/0 (prod is wired in Task 2).**

- `src-tauri/src/reachability_record.rs` test `inner_sig_roundtrip_with_real_identity` (~513) and `inner_sig_rejects_tampered_node_id` (~589): add `Vec::new(), 0,` before the `identity` arg.
- `src-tauri/src/reachability_record.rs` test `build_signed_payload_with_key_verifies_and_rejects_mutation` (~540): add `Vec::new(), 0,` before `signing_key`.
- `src-tauri/src/lib.rs:~50889` (`make_signed_announce`): add `Vec::new(), 0,` before `identity`.
- `src-tauri/src/lib.rs:6468` (production `build_signed_payload_with_key`): pass `Vec::new(), 0,` before `community_signing_key.as_ref()` **for now** — Task 2 replaces these with the real butler-set.

**Step 7: Run the new + existing reachability_record tests — all pass.**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(reachability) + test(inner_sig)'
```
Expected: PASS, including `inner_sig_covers_butler_set_and_rejects_tamper`, `build_signed_payload_with_key_verifies_and_rejects_mutation`, `inner_sig_roundtrip_with_real_identity`, `routing_blob_without_butler_set_is_wire_identical_to_legacy` (still green — pkarr/fixture sig is hardcoded, struct encoding unchanged).

**Step 8: Add a signed-butler-set wire fixture (locks the new signed shape).**

In `src-tauri/tests/wire_format/reachability_announce_fixtures.rs`, add a test that builds a record via `build_signed_payload` with a non-empty butler-set and asserts it round-trips encode→decode→`verify_inner_signature` (do NOT pin a computed-sig hex — ed25519 over the new preimage is deterministic per key, but pin only the struct round-trip + verify, mirroring the existing fixture style). Reuse `fixture_butler_entry`-style entries inline:

```rust
#[test]
fn signed_reachability_with_butler_set_round_trips_and_verifies() {
    use harmony_identity::PrivateIdentity;
    let identity = PrivateIdentity::from_seed(&[0x5A; 32]);
    let public = identity.public_identity();
    let actor = harmony_app::owner_state_types::OwnerAddr(public.address_hash);
    let hlc = harmony_app::owner_state_types::Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: "d".into() };
    let butler = harmony_app::reachability_record::ButlerSetEntry {
        device_id: [0x10; 16],
        iroh_endpoint_id: [0x11; 32],
        device_ed25519_verify: [0x12; 32],
        home_relay: "https://use1-1.relay.iroh.network./".into(),
        pinned: true,
    };
    let p = harmony_app::reachability_record::build_signed_payload(
        [0xAB; 32], "https://derp.example/".into(), vec![], 1_700_000_000_000,
        &actor, &hlc, vec![butler.clone()], 1_700_000_000_000, &identity,
    ).expect("sign");
    let bytes = harmony_app::owner_state_crypto::canonical_cbor_encode(&p).expect("encode");
    let decoded: harmony_app::reachability_record::ReachabilityAnnouncePayload =
        ciborium::from_reader(&bytes[..]).expect("decode");
    assert_eq!(decoded.butler_set, vec![butler]);
    assert_eq!(decoded.bs_at, 1_700_000_000_000);
    harmony_app::reachability_record::verify_inner_signature(&decoded, &actor, &hlc, &public.verifying_key)
        .expect("verify after round-trip");
}
```
(Adjust `harmony_app::` paths/visibility to match the crate — if `build_signed_payload`/`ButlerSetEntry`/`canonical_cbor_encode` aren't `pub` enough for an integration test, mirror the test inside `reachability_record.rs`'s unit `tests` module instead.)

**Step 9: Run the wire-format fixtures.**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(reachability_announce) + test(signed_reachability_with_butler_set)'
```
Expected: PASS. The two pre-existing pinned-hex fixtures stay byte-identical.

**Step 10: Commit.**

```bash
git add -A && git commit -m "feat(reachability): inner identity signature covers the butler-set"
```

---

## Task 2: CRDT reachability publisher carries the durable butler-set

**Files:**
- Modify: `src-tauri/src/lib.rs` — `publish_fn` closure captures (~6386-6421) + body (~6443-6487)
- Reference (mirror, do not change): pkarr blob builder `src-tauri/src/lib.rs:6700-6816`, `src-tauri/src/fleet_net.rs:210` (`build_butler_set`), `vk_map_from_device_cache` (`src-tauri/src/fleet_net.rs`)

Background: the production CRDT publisher (`publish_fn`, async, captures `crdt_state`) currently calls `build_signed_payload_with_key(... Vec::new(), 0, ...)`. It must instead build the owner's butler-set once per publish and pass it. Unlike the sync pkarr blob builder (which reads a `fleet_vk_map` `RwLock` view because it cannot lock the async mutex), `publish_fn` is `async` and already captures `crdt_state`, so it can lock and build the vk-map inline. Build the butler-set ONCE before the per-community loop (it's identical across communities) and clone per payload.

**Step 1: Add captures to the `publish_fn` setup (~lines 6386-6403).**

Alongside the existing `let crdt_state_for_cb = std::sync::Arc::clone(&crdt_state);` add:

```rust
                        let fleet_net_snapshot_for_pub = std::sync::Arc::clone(&fleet_net_snapshot);
                        // Per-device constants (Copy) for the butler self-entry +
                        // vk-map seed — same values the pkarr blob builder uses.
                        let butler_self_device_id_hash_for_pub = this_device_id_hash;
                        let butler_self_device_vk_for_pub =
                            loaded.device_signing_key.verifying_key().to_bytes();
```

(`fleet_net_snapshot` is defined at `lib.rs:4743` — in scope here. `this_device_id_hash` and `loaded` are in scope; confirm by the pkarr builder using them at `6704-6706`.)

**Step 2: Add per-invocation clones inside the closure (~lines 6406-6421).**

```rust
                                let fleet_net_snapshot = std::sync::Arc::clone(&fleet_net_snapshot_for_pub);
                                let butler_self_device_id_hash = butler_self_device_id_hash_for_pub;
                                let butler_self_device_vk = butler_self_device_vk_for_pub;
```

**Step 3: Build the butler-set once, before the `for community_id` loop (~after line 6448).**

Insert after `announced_at_ms` is computed and before/around the community loop:

```rust
                                    // ZEB-493 Part 1: build the owner's durable
                                    // butler-set once (identical across communities;
                                    // cloned per payload). publish_fn is async, so it
                                    // locks crdt_state directly instead of the sync
                                    // RwLock vk-map view the pkarr blob builder uses.
                                    let (self_butler_set, self_bs_at) = {
                                        let st = crdt_state.lock().await;
                                        let vk_map = crate::fleet_net::vk_map_from_device_cache(
                                            &st.owner_device_cache,
                                            &actor,
                                            &device_id,
                                            butler_self_device_vk,
                                        );
                                        let snap = fleet_net_snapshot
                                            .read()
                                            .unwrap_or_else(|p| p.into_inner());
                                        let self_entry =
                                            crate::reachability_record::ButlerSetEntry {
                                                device_id: butler_self_device_id_hash,
                                                iroh_endpoint_id: node_id_bytes,
                                                device_ed25519_verify: butler_self_device_vk,
                                                home_relay: home_relay.clone(),
                                                pinned: false,
                                            };
                                        let vk_lookup = |dev_id: &str| -> Option<[u8; 32]> {
                                            vk_map.get(dev_id).copied()
                                        };
                                        let set = crate::fleet_net::build_butler_set(
                                            &snap,
                                            &device_id,
                                            self_entry,
                                            &vk_lookup,
                                            announced_at_ms.saturating_sub(
                                                crate::butler_deposit::BUTLER_SET_FRESHNESS_MS,
                                            ),
                                        );
                                        let bs_at = if set.is_empty() { 0 } else { announced_at_ms };
                                        (set, bs_at)
                                    };
```

**Step 4: Pass the butler-set into the per-community builder (line 6468).**

Replace the `Vec::new(), 0,` placeholder from Task 1 Step 6 with clones:

```rust
                                            match crate::reachability_record::build_signed_payload_with_key(
                                                node_id_bytes,
                                                home_relay.clone(),
                                                direct_addrs.clone(),
                                                announced_at_ms,
                                                &actor,
                                                &hlc,
                                                self_butler_set.clone(),
                                                self_bs_at,
                                                community_signing_key.as_ref(),
                                            ) {
```

**Step 5: Compile + run the reachability publisher's reachable tests.**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(reachability) + test(zeb_321)'
```
Expected: PASS (no regression). The behavioral effect is exercised end-to-end by Task 6.

**Step 6: Verify no borrow/lock issues** — confirm `crdt_state.lock().await` here does not deadlock against the event loop. The publish_fn already locks `crdt_state` for the ZEB-371 friend reconcile later in the same closure, so this lock acquisition follows an established safe pattern (publish_fn runs on its own publisher cadence, not inline in the `select!`).

**Step 7: Commit.**

```bash
git add -A && git commit -m "feat(reachability): carry the durable butler-set in the CRDT publisher"
```

---

## Task 3: Per-source freshness policy in the resolver

**Files:**
- Modify: `src-tauri/src/reachability_resolver.rs` (`ResolverEntry` ~45-49, `update` ~110, `resolve` ~146, `resolve_async` ~270-298, pkarr-cache-back ~295)
- Modify: `src-tauri/src/reachability_record.rs` (add `durable_butler_set`)
- Modify: `src-tauri/src/butler_deposit.rs` (add `freshest_butler_set_by_source`)
- Test: `src-tauri/src/reachability_resolver.rs`, `src-tauri/src/butler_deposit.rs`

Background: `resolve_async` returns payloads with no source tag, and the in-memory cache mixes CRDT-projected entries (via `update`) with pkarr-cache-back entries (the single site at `resolve_async:295`). Tag at that asymmetry: `update` defaults to `DurableCrdt`; only the pkarr-cache-back tags `PkarrLive`.

**Step 1: Write the failing unit test (source-aware freshness).**

Add to `src-tauri/src/butler_deposit.rs` tests:

```rust
    #[test]
    fn durable_source_butler_set_survives_past_freshness_window() {
        use crate::reachability_resolver::ReachabilitySource;
        let now = 100 * BUTLER_SET_FRESHNESS_MS; // far past the window
        let stale = ReachabilityAnnouncePayload {
            iroh_node_id: [1; 32],
            home_relay_url: "r".into(),
            direct_addresses: vec![],
            announced_at_ms: 1,
            identity_signature: [0; 64],
            butler_set: vec![crate::reachability_record::ButlerSetEntry {
                device_id: [9; 16],
                iroh_endpoint_id: [8; 32],
                device_ed25519_verify: [7; 32],
                home_relay: "r".into(),
                pinned: false,
            }],
            bs_at: 1, // ancient stamp
        };
        // DurableCrdt: window-exempt -> returns the set.
        let durable = freshest_butler_set_by_source(
            &[(stale.clone(), ReachabilitySource::DurableCrdt)],
            now,
        );
        assert_eq!(durable.len(), 1, "durable butler-set must survive the window");
        // PkarrLive: windowed -> filtered to empty.
        let live = freshest_butler_set_by_source(
            &[(stale, ReachabilitySource::PkarrLive)],
            now,
        );
        assert!(live.is_empty(), "pkarr butler-set past the window must be empty");
    }
```

**Step 2: Run it — fails to compile** (`ReachabilitySource`, `freshest_butler_set_by_source` don't exist):

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(durable_source_butler_set_survives_past_freshness_window)'
```
Expected: compile error.

**Step 3: Add the `ReachabilitySource` enum + tag `ResolverEntry`.**

In `src-tauri/src/reachability_resolver.rs`, add near the top:

```rust
/// Provenance of a resolved reachability payload, used to apply a per-source
/// butler-set freshness policy (ZEB-493 Decision 3): durable replicated CRDT
/// records carry seal-targets that stay valid even when the recipient's primary
/// is long-offline, so they are exempt from the live pkarr freshness window;
/// pkarr-sourced records keep the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReachabilitySource {
    /// Projected from a durable community-membership ReachabilityAnnounce CRDT
    /// event (persisted, replicated, boot-replayed).
    DurableCrdt,
    /// Fetched live from the recipient's pkarr routing blob.
    PkarrLive,
}
```

Extend `ResolverEntry` (lines 45-49):

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverEntry {
    pub payload: ReachabilityAnnouncePayload,
    pub hlc: Hlc,
    pub source: ReachabilitySource,
}
```

**Step 4: Split `update` into `update` (DurableCrdt default) + `update_with_source`.**

Replace the existing `update` (line 110) so every current caller keeps working unchanged:

```rust
    /// CRDT-projection update (the default, durable source). All
    /// membership-apply call sites use this unchanged.
    pub fn update(&self, actor: OwnerAddr, payload: ReachabilityAnnouncePayload, hlc: Hlc) {
        self.update_with_source(actor, payload, hlc, ReachabilitySource::DurableCrdt);
    }

    /// Source-tagged update. Only the pkarr-cache-back path passes `PkarrLive`.
    pub fn update_with_source(
        &self,
        actor: OwnerAddr,
        payload: ReachabilityAnnouncePayload,
        hlc: Hlc,
        source: ReachabilitySource,
    ) {
        // ... existing LWW body, but store `source` in the ResolverEntry it
        // inserts/replaces (ResolverEntry { payload, hlc, source }).
    }
```

Update the existing LWW body to construct `ResolverEntry { payload, hlc, source }` (carry the new entry's source on replace).

**Step 5: Tag the pkarr-cache-back as `PkarrLive` (the ONE site, line 295).**

In `resolve_async`, change:

```rust
        for payload in &payloads {
            let hlc = Hlc { wall_ms: payload.announced_at_ms, logical: 0, device_id: String::new() };
            self.update_with_source(*addr, payload.clone(), hlc, ReachabilitySource::PkarrLive);
        }
```

**Step 6: Add source-exposing resolve methods.**

```rust
    /// Like `resolve`, but carries each entry's source for the freshness policy.
    pub fn resolve_with_source(
        &self,
        actor: &OwnerAddr,
    ) -> Vec<(ReachabilityAnnouncePayload, ReachabilitySource)> {
        // mirror `resolve`, returning (entry.payload.clone(), entry.source)
    }

    /// Like `resolve_async`, but carries source. Cache hits keep their stored
    /// source; pkarr fallback results are `PkarrLive`.
    pub async fn resolve_async_with_source(
        &self,
        addr: &OwnerAddr,
    ) -> Vec<(ReachabilityAnnouncePayload, ReachabilitySource)> {
        let cached = self.resolve_with_source(addr);
        if !cached.is_empty() {
            return cached;
        }
        let fb = { self.fallback_source.read().expect("fallback_source poisoned").clone() };
        let Some(fb) = fb else { return Vec::new() };
        let payloads = fb.resolve(addr).await;
        for payload in &payloads {
            let hlc = Hlc { wall_ms: payload.announced_at_ms, logical: 0, device_id: String::new() };
            self.update_with_source(*addr, payload.clone(), hlc, ReachabilitySource::PkarrLive);
        }
        payloads.into_iter().map(|p| (p, ReachabilitySource::PkarrLive)).collect()
    }
```

(Keep the existing `resolve`/`resolve_async` as-is for non-deposit callers.)

**Step 7: Add `durable_butler_set` (window-exempt reader) to `reachability_record.rs`.**

Next to `fresh_butler_set` (line 133):

```rust
/// Like [`fresh_butler_set`] but WITHOUT the freshness window — for butler-sets
/// sourced from the durable community CRDT (ZEB-493 D3). The seal-target vk is
/// durable even when the advertised endpoint has drifted (a stale endpoint just
/// fails the dial and falls through; the vk stays a valid seal target). A
/// zero/missing `bs_at` still means "no butler-set".
pub fn durable_butler_set(blob: &ReachabilityAnnouncePayload) -> Vec<ButlerSetEntry> {
    if blob.bs_at == 0 {
        return Vec::new();
    }
    blob.butler_set
        .iter()
        .take(crate::butler_deposit::BUTLER_SET_MAX_ENTRIES)
        .cloned()
        .collect()
}
```

**Step 8: Add `freshest_butler_set_by_source` to `butler_deposit.rs`.**

Next to `freshest_butler_set` (line 415):

```rust
/// Source-aware variant of [`freshest_butler_set`]: durable-CRDT entries are
/// window-exempt ([`crate::reachability_record::durable_butler_set`]); pkarr-live
/// entries keep the 15-min window ([`crate::reachability_record::fresh_butler_set`]).
/// Picks the entry with the newest `bs_at` among those that yield a non-empty set.
pub(crate) fn freshest_butler_set_by_source(
    tagged: &[(ReachabilityAnnouncePayload, crate::reachability_resolver::ReachabilitySource)],
    now_ms: u64,
) -> Vec<ButlerSetEntry> {
    use crate::reachability_resolver::ReachabilitySource;
    tagged
        .iter()
        .map(|(b, src)| {
            let set = match src {
                ReachabilitySource::DurableCrdt => {
                    crate::reachability_record::durable_butler_set(b)
                }
                ReachabilitySource::PkarrLive => {
                    crate::reachability_record::fresh_butler_set(b, now_ms)
                }
            };
            (b.bs_at, set)
        })
        .filter(|(_, set)| !set.is_empty())
        .max_by_key(|(bs_at, _)| *bs_at)
        .map(|(_, set)| set)
        .unwrap_or_default()
}
```

**Step 9: Run the new test + the resolver tests.**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(reachability_resolver) + test(durable_source_butler_set_survives_past_freshness_window) + test(butler_set)'
```
Expected: PASS. (Resolver tests that pattern-match `ResolverEntry` may need the new `source` field — update them to `ResolverEntry { payload, hlc, source: ReachabilitySource::DurableCrdt }`.)

**Step 10: Commit.**

```bash
git add -A && git commit -m "feat(reachability): per-source butler-set freshness (durable exempt, pkarr windowed)"
```

---

## Task 4: Wire both deposit rungs to the source-aware selector

**Files:**
- Modify: `src-tauri/src/butler_deposit.rs:513-521` (`IrohButlerDepositClient::deposit`)
- Modify: `src-tauri/src/community_relay_prod.rs:559-566` (`ReachabilityButlerSetResolve::resolve_targets`)
- Test: `src-tauri/src/butler_deposit.rs`

**Step 1: Write the failing test — butler deposit resolves a durable (past-window) butler-set.**

Add to `src-tauri/src/butler_deposit.rs` tests a unit test that builds a `ReachabilityResolver`, `update`s it (DurableCrdt) with a payload whose `bs_at` is far in the past but `butler_set` non-empty, wires it as the resolver of an `IrohButlerDepositClient` (or directly tests the resolve+select helper path the deposit uses), and asserts the resolved entries are non-empty at a `now_ms` far past the window. If a full client is heavy, assert at the seam: `resolver.resolve_async_with_source(addr).await` + `freshest_butler_set_by_source(...)` returns the set.

```rust
    #[tokio::test]
    async fn butler_deposit_resolves_durable_butler_set_past_window() {
        use crate::reachability_resolver::{ReachabilityResolver, ReachabilitySource};
        let resolver = ReachabilityResolver::new();
        let addr = OwnerAddr([0x5; 16]);
        let payload = /* ReachabilityAnnouncePayload with non-empty butler_set, bs_at = 1 */;
        resolver.update(addr, payload, Hlc { wall_ms: 1, logical: 0, device_id: "x".into() });
        let now = 100 * BUTLER_SET_FRESHNESS_MS;
        let tagged = resolver.resolve_async_with_source(&addr).await;
        assert!(matches!(tagged[0].1, ReachabilitySource::DurableCrdt));
        let set = freshest_butler_set_by_source(&tagged, now);
        assert_eq!(set.len(), 1, "durable butler-set resolves past the window");
    }
```

**Step 2: Run it — fails** (deposit/relay still use the windowed `resolve_async`/`freshest_butler_set`; the assertion on the helper path should pass, but the deposit-client path won't yet use it — keep the test at the helper seam to make it green in Step 4).

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(butler_deposit_resolves_durable_butler_set_past_window)'
```

**Step 3: Rewire `IrohButlerDepositClient::deposit` (butler_deposit.rs:517-521).**

```rust
        let tagged = self.resolver.resolve_async_with_source(&req.recipient_owner).await;
        let entries = freshest_butler_set_by_source(&tagged, req.now_ms);
        if entries.is_empty() {
            return DepositRungOutcome::SkippedNoFreshButlerSet;
        }
```

**Step 4: Rewire `ReachabilityButlerSetResolve::resolve_targets` (community_relay_prod.rs:564-565).**

```rust
        let tagged = self.0.resolve_async_with_source(recipient_owner).await;
        crate::butler_deposit::freshest_butler_set_by_source(&tagged, now_ms)
```

(Make `freshest_butler_set_by_source` visible to `community_relay_prod.rs` — `pub(crate)` is sufficient since both are in the same crate.)

**Step 5: Run butler + relay deposit tests.**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(butler_deposit) + test(community_relay)'
```
Expected: PASS.

**Step 6: Commit.**

```bash
git add -A && git commit -m "feat(dm): deposit rungs resolve durable butler-sets via the source-aware selector"
```

---

## Task 5: Relay rung enrolled-device fallback (butler-less recipient)

**Files:**
- Modify: `src-tauri/src/community_relay_prod.rs:744-753` (`ProdCommunityRelayDepositClient::deposit`) + a new helper method on the client
- Reference: `src-tauri/src/owner_state_types.rs:612-682` (`OwnerDeviceEntry`), `src-tauri/src/dm_signing.rs:334-335` (ed25519 = `identity_pub[32..64]`)
- Test: `src-tauri/src/community_relay_prod.rs`

Background: the relay rung only seals to a vk (the relay holds; the recipient pulls — no endpoint dial to the recipient), so a vk-only fallback is sufficient. `build_relay_deposit_frame` uses ONLY `target.device_ed25519_verify`. The client already holds `crdt_state: Arc<Mutex<OwnerState>>`.

**Step 1: Write the failing test — relay deposit seals to enrolled-device vks when no butler-set.**

Add to `src-tauri/src/community_relay_prod.rs` tests: construct a `ProdCommunityRelayDepositClient` whose `butler_resolver` returns empty, whose `crdt_state` has an `owner_device_cache` entry for the recipient with 2 `device_identity_pubs` (each `[x25519(32) || ed25519(32)]`), a shared joined community, and a stub `dial` that records the targets it was asked to deposit. Assert `deposit` returns `true` and the dialed frames sealed to the recipient's enrolled ed25519 vks (≤2).

(Reuse existing relay-prod test fixtures/stubs in this module for `dial`, `membership`, `relay_resolver`, `cas`.)

**Step 2: Run it — fails** (deposit returns `false` on empty butler-set):

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(relay_deposit_enrolled_device_fallback)'
```

**Step 3: Add the fallback helper on `ProdCommunityRelayDepositClient`.**

```rust
    /// ZEB-488: when the recipient advertises no (durable or live) butler-set,
    /// fall back to ≤2 of their ENROLLED device ed25519 vks from durable
    /// community membership (`OwnerDeviceCache`). The relay only seals to the vk
    /// (it holds; the recipient pulls), so endpoint/relay fields are unused —
    /// left zero/empty. D35 preserved (seal to device keys, bounded fan-out).
    async fn enrolled_device_fallback_targets(
        &self,
        recipient_owner: &crate::owner_state_types::OwnerAddr,
    ) -> Vec<crate::reachability_record::ButlerSetEntry> {
        let st = self.crdt_state.lock().await;
        let Some(entry) = st.owner_device_cache.devices.get(recipient_owner) else {
            return Vec::new();
        };
        entry
            .device_identity_pubs
            .iter()
            .filter_map(|p| p.as_ref())
            .take(crate::butler_deposit::BUTLER_SET_MAX_ENTRIES)
            .map(|identity_pub| {
                let ed: [u8; 32] = identity_pub[32..64].try_into().expect("64 - 32 == 32");
                crate::reachability_record::ButlerSetEntry {
                    device_id: harmony_owner::pubkey_bundle::PubKeyBundle::classical_identity_hash(&ed),
                    iroh_endpoint_id: [0u8; 32],
                    device_ed25519_verify: ed,
                    home_relay: String::new(),
                    pinned: false,
                }
            })
            .collect()
    }
```

(Confirm the `PubKeyBundle::classical_identity_hash` import path matches `fleet_net.rs`'s usage. If unavailable, `device_id: [0u8; 16]` is acceptable — `device_id` is unused by `build_relay_deposit_frame`.)

**Step 4: Use the fallback in `deposit` (community_relay_prod.rs:745-753).**

```rust
        let mut targets = self
            .butler_resolver
            .resolve_targets(&req.recipient_owner, req.now_ms)
            .await;
        if targets.is_empty() {
            // ZEB-488: no butler-set (durable or live) -> seal to enrolled
            // device vks from durable community membership.
            targets = self.enrolled_device_fallback_targets(&req.recipient_owner).await;
        }
        if targets.is_empty() {
            return false;
        }
```

**Step 5: Run the relay-prod tests.**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_relay_prod) + test(relay_deposit_enrolled_device_fallback)'
```
Expected: PASS.

**Step 6: Commit.**

```bash
git add -A && git commit -m "feat(relay): seal to enrolled device vks when the recipient has no butler-set"
```

---

## Task 6: Promote e2e s6/s7 to hard asserts

**Files:**
- Modify: `e2e-harness/tests/e2e_two_node.rs` — `s6_relay_deposit_recover` (1307-1466), `s7_butler_deposit_recover` (1496-1833)
- Reference: `e2e-harness/src/driver.rs` (`poll_until`, `get_relay_held`, `get_butler_held`, `read_dm_plaintext_any`)

Background: both scenarios currently SIGKILL the recipient BEFORE its reachability (with the durable butler-set) has replicated to the sender/relay, then characterize `held=false`. With Parts 1-5, the durable butler-set replicates via community membership while the recipient is online — so the post-kill resolve succeeds from the durable cache. Restructure to **publish + sync reachability while online, then kill, then send**, and turn the fallbacks into hard asserts.

**Step 1 (s6): Add a "reachability synced" barrier before `b.kill()` (~line 1381).**

After the friendship dance and before `b.kill()`, poll until `a` can resolve `b`'s reachability WITH a non-empty durable butler-set (or, minimally, until `b`'s ReachabilityAnnounce for the shared community has been observed by `a`/`r`). Use a new driver helper if needed (e.g. `poll_reachability_has_butler_set(&a, &b_owner, Duration::from_secs(120))`) or assert via an existing introspection RPC. The barrier guarantees the durable record replicated before the kill.

**Step 2 (s6): Replace the HELD characterize-fallback (lines 1406-1419) with a hard assert.**

```rust
    let held = poll_until(Duration::from_secs(90), || async {
        let entries = get_relay_held(&r, Some(&community_id)).await?;
        Ok(entries.into_iter().find(|e| {
            e.get("senderOwnerHex").and_then(Value::as_str) == Some(a_owner.as_str())
                && e.get("recipientOwnerHex").and_then(Value::as_str) == Some(b_owner.as_str())
        }))
    })
    .await;
    assert!(held.is_ok(), "HELD: r holds a's deposit for offline b (durable seal-targets)");
```

Keep the existing RECV (1421-1448) and CLEARED (1450-1463) asserts (already hard).

**Step 3 (s7): Add the same reachability-synced barrier before `p.kill()` (~line 1713).**

After the A↔P friendship + the P/B2 relaunches, poll until `a` resolves `p`'s reachability with a non-empty durable butler-set (P's butler-set advertises B2). Only then `p.kill()`.

**Step 4 (s7): Replace the HELD characterize-fallback (lines 1734-1764) with a hard assert.**

```rust
    let held_entry = poll_until(Duration::from_secs(120), || async {
        let entries = get_butler_held(&b2).await?;
        Ok(entries.into_iter().find(|e| {
            e.get("senderOwnerHex").and_then(Value::as_str) == Some(a_owner.as_str())
        }))
    })
    .await
    .expect("HELD: B2 holds A's deposit for offline P (durable butler-set)");
```

Keep the RECV (1781-1791 → make hard assert) and CLEARED (1806-1830) asserts. For RECV, convert the `if recovered.is_err() { ... return }` fallback (1793-1803) into `assert!(recovered.is_ok(), "RECV: P recovered the butler-deposited DM")`.

**Step 5 (s7): Leave boundary 1 (SAS pairing) + boundary 0 (ZEB-491 inviter-persist) characterize-fallbacks AS-IS.** Those gate on unrelated pre-conditions (pairing transport, the ZEB-491 inviter-enrollment-persist gap) — out of scope for this change. Add a code comment noting they remain characterized pending their own tickets.

**Step 6: Run s6 + s7 (single-threaded, long budget).**

```bash
cd src-tauri && cd ../e2e-harness && cargo nextest run --locked -E 'test(s6_relay_deposit_recover) + test(s7_butler_deposit_recover)' --no-capture
```
Expected: both PASS with HELD/RECV/CLEARED asserting (s7 boundary 0/1 may still characterize if pairing/inviter-persist don't establish co-located — acceptable; the durable-seal-target boundaries must now pass). If HELD still fails, the reachability-sync barrier (Steps 1/3) did not guarantee replication before kill — investigate the barrier, not the deposit.

**Step 7: Commit.**

```bash
git add -A && git commit -m "test(e2e): assert s6/s7 deposit→recover with durable seal-targets"
```

---

## Task 7: Full gate + open PR

**Step 1: Run the full gate from `src-tauri/`.**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: 0 fmt diffs, 0 clippy warnings, all tests pass. (Background long-running runs with a wall-clock safety net per repo practice.)

**Step 2: Open the PR (no ZEB-NNN in title/body/branch).**

```bash
git push -u origin durable-seal-targets-async-dm
gh pr create --repo zeblithic/harmony-client \
  --title "feat(dm): durable seal-targets for async DM delivery" \
  --body "<summary of Parts 1+2, no ZEB-NNN>"
```

**Step 3: Post a cascade-safe cross-ref PR COMMENT** naming the issues it closes (ZEB-488 relay rung, ZEB-493 butler rung) — in a comment, not the body.

**Step 4: Drive the bot loop** (Qodo + CodeAnt first pass → address → one CodeRabbit review) per the standing autonomous cadence; never Greptile; no self-merge; pushover at ready-to-merge.

---

## Self-review notes

- **Spec coverage:** Part 1 §"butler rung" → Tasks 1+2+3+4. Part 2 §"relay rung" → Task 5 (enrolled-device fallback) + Task 4 (durable butler-set path). Decision 3 (freshness exemption) → Task 3. Decision 4 (inner sig covers butler_set+bs_at) → Task 1. Wire-format/flag-day → Task 1 (preimage) + Step 8 fixture. Testing §unit/wire/e2e → Tasks 1,3,4,5 (unit) + Task 1 Step 8 (wire) + Task 6 (e2e). ZEB-488 finding-text correction → PR comment / Task 5 commit message.
- **Flag-day scope clarified vs spec:** the preimage change affects ONLY durable CRDT records (pkarr zero-fills the inner sig), so the existing pinned struct fixtures stay byte-identical — the fixture task is additive (Task 1 Step 8), not a regen.
- **Type consistency:** `ReachabilitySource` (resolver) used identically in `freshest_butler_set_by_source` (butler_deposit) and both consumers. `build_signed_payload`/`build_signed_payload_with_key` take `(butler_set: Vec<ButlerSetEntry>, bs_at: u64)` in the same position (before the key/identity arg) everywhere.
- **Open risk to verify during impl:** confirm `loaded` and `this_device_id_hash` are in scope at the `publish_fn` setup (`lib.rs:~6386`); if not, snapshot the self device id-hash + vk earlier. Confirm `fleet_net_snapshot` inner type is `RwLock<FleetNetDoc>` (`.read()` used).
