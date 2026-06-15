# ZEB-461 — Cross-WAN DM via friend-established Reticulum tunnel — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a 1:1 DM between two friends actually deliver bytes end-to-end (co-located harness + cross-WAN) by completing the existing Reticulum-over-iroh-tunnel chain: the friend handshake populates `OwnerDeviceCache` AND registers a tunnel `Contact`, so `try_initiate_tunnel` opens a `tunnel-*` interface, Reticulum announces propagate, and the queued DM drains.

**Architecture:** Two repos. **harmony** (`/Users/zeblith/work/zeblithic/harmony`, dependency): add PQ-key fields to `ContactAddress::Tunnel` and make `try_initiate_tunnel` read them from the contact when the discovery cache misses (Option B — confirmed: PQ keys are read only from `discovery.get_record`, which expires and doesn't route between friends-without-community). **harmony-client** (`/Users/zeblith/work/zeblithic/harmony-client`): carry device-bundle + reachability + PQ keys in the friend handshake, populate the cache via `apply_owner_device_update`, and register a `ContactAddress::Tunnel` (with the peer's PQ keys) via a new contact-registration channel into the event loop. Lands as two PRs; the harmony-client PR bumps the `harmony-runtime` git pin to the harmony commit.

**Tech Stack:** Rust, ciborium (canonical-ish CBOR), iroh QUIC, harmony-runtime/harmony-reticulum/harmony-contacts, tokio, cargo-nextest. Wire structs use single-char `#[serde(rename)]` canonical-CBOR keys.

**Cross-repo order:** harmony Tasks 1–3 land first (PR on harmony). Then harmony-client Task 4 bumps the pin and runs the **feasibility spike** — a checkpoint that proves the transport chain before any handshake wiring. Tasks 5–10 build the handshake automation. **If the Task 4 spike cannot deliver a DM co-located, STOP and escalate to Jake — the approach needs rethinking before more work.**

---

## File structure

**harmony repo:**
- `crates/harmony-contacts/src/contact.rs` — `ContactAddress::Tunnel` gains `peer_dsa_pubkey`/`peer_kem_pubkey`.
- `crates/harmony-runtime/src/runtime.rs` — `try_initiate_tunnel` contact-PQ fallback.
- `crates/harmony-node/src/main.rs` — `--add-tunnel-peer` Contact construction (compile fix + optional PQ from spec).

**harmony-client repo:**
- `src-tauri/Cargo.toml` — bump `harmony-runtime` pin.
- `src-tauri/src/iroh_friend_acceptor.rs` — wire fields on `FriendLinkRequest`/`FriendLinkAccepted`; sig-preimage extension; self-bundle on the accept; cache population + contact-register in `process_friend_request`/acceptor.
- `src-tauri/src/lib.rs` — requester: self-bundle on the request; cache population + contact-register after `apply_friend_update`.
- `src-tauri/src/dm_tunnel_contact.rs` *(new)* — small helper: build a `ContactAddress::Tunnel` from a peer's reachability+PQ, and the contact-registration request type + channel plumbing.
- `src-tauri/tests/wire_format_zeb370_fixtures.rs` — regenerate pinned hex for the extended structs.
- `e2e-harness/tests/e2e_two_node.rs` — S2 hard-assert byte-delivery.

---

## Task 1 (harmony): Add PQ-key fields to `ContactAddress::Tunnel`

**Files:**
- Modify: `/Users/zeblith/work/zeblithic/harmony/crates/harmony-contacts/src/contact.rs:7-16`
- Modify: `/Users/zeblith/work/zeblithic/harmony/crates/harmony-node/src/main.rs` (`--add-tunnel-peer` construction, ~786)
- Test: in `crates/harmony-contacts/src/contact.rs` `#[cfg(test)]`

- [ ] **Step 1: Write the failing test** (append to contact.rs tests mod)

```rust
#[test]
fn tunnel_address_roundtrips_with_pq_keys() {
    let addr = ContactAddress::Tunnel {
        node_id: [7u8; 32],
        relay_url: Some("https://relay.example".into()),
        direct_addrs: vec![],
        peer_dsa_pubkey: Some(vec![1, 2, 3]),
        peer_kem_pubkey: Some(vec![4, 5, 6]),
    };
    let bytes = serde_json::to_vec(&addr).unwrap();
    let back: ContactAddress = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(addr, back);
}

#[test]
fn tunnel_address_deserializes_legacy_without_pq_fields() {
    // A persisted contact written before this change has no PQ fields.
    let legacy = r#"{"Tunnel":{"node_id":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"relay_url":null,"direct_addrs":[]}}"#;
    let addr: ContactAddress = serde_json::from_str(legacy).unwrap();
    match addr {
        ContactAddress::Tunnel { peer_dsa_pubkey, peer_kem_pubkey, .. } => {
            assert!(peer_dsa_pubkey.is_none());
            assert!(peer_kem_pubkey.is_none());
        }
        _ => panic!("expected Tunnel"),
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony && cargo test -p harmony-contacts tunnel_address -- --nocapture`
Expected: FAIL — `ContactAddress::Tunnel` has no `peer_dsa_pubkey`/`peer_kem_pubkey`.

- [ ] **Step 3: Add the fields** (contact.rs, the `Tunnel` variant). Use `#[serde(default)]` so legacy contacts deserialize.

```rust
    Tunnel {
        node_id: [u8; 32],
        #[serde(default)]
        relay_url: Option<String>,
        #[serde(default)]
        direct_addrs: Vec<String>,
        /// ML-DSA-65 public key (1952 bytes) for the tunnel handshake, when
        /// learned out-of-band (e.g. a friend handshake) rather than from a
        /// discovery announce. `try_initiate_tunnel` falls back to this when the
        /// discovery cache has no record. ZEB-461.
        #[serde(default)]
        peer_dsa_pubkey: Option<Vec<u8>>,
        /// ML-KEM-768 public key (1184 bytes), paired with `peer_dsa_pubkey`.
        #[serde(default)]
        peer_kem_pubkey: Option<Vec<u8>>,
    },
```

- [ ] **Step 4: Fix the `--add-tunnel-peer` construction** (main.rs ~786) — add the two fields as `None`:

```rust
        addresses: vec![harmony_contacts::ContactAddress::Tunnel {
            node_id,
            relay_url,
            direct_addrs: vec![],
            peer_dsa_pubkey: None,
            peer_kem_pubkey: None,
        }],
```

- [ ] **Step 5: Build the workspace to find any other exhaustive matches**

Run: `cd /Users/zeblith/work/zeblithic/harmony && cargo build --workspace 2>&1 | grep -E "error|Tunnel" | head`
Expected: any non-`..` match on `ContactAddress::Tunnel` is flagged; `try_initiate_tunnel` and `find_by_tunnel_node_id` already use `..` so are unaffected. Fix any that surface by adding the two fields (read-only sites: ignore via `..`).

- [ ] **Step 6: Run the test to verify it passes**

Run: `cd /Users/zeblith/work/zeblithic/harmony && cargo test -p harmony-contacts tunnel_address`
Expected: PASS (both tests).

- [ ] **Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony
git checkout -b zeb-461-tunnel-contact-pq-keys
git add crates/harmony-contacts/src/contact.rs crates/harmony-node/src/main.rs
git commit -m "feat(zeb-461): carry peer PQ keys on ContactAddress::Tunnel

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2 (harmony): `try_initiate_tunnel` falls back to contact PQ keys

**Files:**
- Modify: `/Users/zeblith/work/zeblithic/harmony/crates/harmony-runtime/src/runtime.rs:1592-1607` (the PQ lookup block inside `try_initiate_tunnel`)
- Test: `crates/harmony-runtime/src/runtime.rs` `#[cfg(test)]`

- [ ] **Step 1: Write the failing test** — a contact carrying tunnel PQ keys, with an EMPTY discovery cache, must still emit `InitiateTunnel`. (Mirror an existing `try_initiate_tunnel`/peer-manager test for setup; find one via `grep -n "try_initiate_tunnel\|InitiateTunnel" crates/harmony-runtime/src/runtime.rs` and copy its harness.)

```rust
#[test]
fn try_initiate_tunnel_uses_contact_pq_keys_when_discovery_empty() {
    let mut rt = test_runtime(); // existing helper; see neighboring tests
    let id = [9u8; 16];
    rt.contact_store_mut().add(harmony_contacts::Contact {
        identity_hash: id,
        display_name: None,
        peering: harmony_contacts::PeeringPolicy { enabled: true, priority: harmony_contacts::PeeringPriority::Normal },
        added_at: 0, last_seen: None, notes: None,
        addresses: vec![harmony_contacts::ContactAddress::Tunnel {
            node_id: [3u8; 32], relay_url: None, direct_addrs: vec![],
            peer_dsa_pubkey: Some(vec![1u8; 1952]),
            peer_kem_pubkey: Some(vec![2u8; 1184]),
        }],
        replication: None,
    }).unwrap();
    rt.push_event(RuntimeEvent::ContactChanged { identity_hash: id });
    let actions = rt.drain_pending_direct_actions(); // existing accessor; match neighbors
    assert!(actions.iter().any(|a| matches!(a,
        RuntimeAction::InitiateTunnel { peer_dsa_pubkey, peer_kem_pubkey, .. }
            if peer_dsa_pubkey.len() == 1952 && peer_kem_pubkey.len() == 1184)));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony && cargo test -p harmony-runtime try_initiate_tunnel_uses_contact_pq`
Expected: FAIL — discovery cache miss returns early, no action emitted.

- [ ] **Step 3: Implement the fallback** — replace the PQ lookup block (runtime.rs:1592-1607) so a discovery miss falls back to the contact's tunnel PQ fields. Re-read the contact for the PQ fields (the earlier `tunnel_addr` find already borrowed it; capture the PQ keys in that same `find_map`).

Change the tunnel-address `find_map` to also capture PQ keys:

```rust
    let tunnel_addr = match self.contact_store.get(&identity_hash) {
        Some(contact) => contact.addresses.iter().find_map(|addr| {
            if let ContactAddress::Tunnel { node_id, relay_url, peer_dsa_pubkey, peer_kem_pubkey, .. } = addr {
                Some((*node_id, relay_url.clone(), peer_dsa_pubkey.clone(), peer_kem_pubkey.clone()))
            } else { None }
        }),
        None => return,
    };
    let (node_id, relay_url, contact_dsa, contact_kem) = match tunnel_addr {
        Some(t) => t,
        None => return,
    };
```

Then the PQ resolution prefers discovery, falls back to the contact:

```rust
    let (peer_dsa_pubkey, peer_kem_pubkey) = match self
        .discovery
        .get_record(&identity_hash, self.last_unix_now)
    {
        Some(record) if !record.public_key.is_empty() && !record.encryption_key.is_empty() => {
            (record.public_key.clone(), record.encryption_key.clone())
        }
        _ => match (contact_dsa, contact_kem) {
            (Some(d), Some(k)) if !d.is_empty() && !k.is_empty() => (d, k),
            _ => {
                tracing::info!(
                    identity = %hex::encode(&identity_hash[..4]),
                    "contact has tunnel address but PQ keys not in discovery cache or contact — \
                     tunnel dial deferred until announce arrives"
                );
                return;
            }
        },
    };
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cd /Users/zeblith/work/zeblithic/harmony && cargo test -p harmony-runtime try_initiate_tunnel`
Expected: PASS (new test + existing try_initiate_tunnel tests still green).

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony
git add crates/harmony-runtime/src/runtime.rs
git commit -m "feat(zeb-461): try_initiate_tunnel falls back to contact PQ keys

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3 (harmony): gate + push + open PR; record the rev

- [ ] **Step 1: Full gates**

Run: `cd /Users/zeblith/work/zeblithic/harmony && cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo nextest run -p harmony-contacts -p harmony-runtime -p harmony-node`
Expected: all green.

- [ ] **Step 2: Push + PR**

```bash
cd /Users/zeblith/work/zeblithic/harmony
git push -u origin zeb-461-tunnel-contact-pq-keys
gh pr create --repo zeblithic/harmony --title "ZEB-461: carry peer PQ keys on tunnel contacts" --body "Adds peer_dsa_pubkey/peer_kem_pubkey to ContactAddress::Tunnel and makes try_initiate_tunnel fall back to them when the discovery cache misses. Enables friend-handshake-established tunnels (friends-without-community have no discovery route). Paired with harmony-client ZEB-461 PR.

🤖 Generated with [Claude Code](https://claude.com/claude-code)"
```

- [ ] **Step 3:** Record the branch HEAD sha (`git rev-parse HEAD`) — needed for the harmony-client Cargo pin in Task 4. Do NOT self-merge; this PR rides the same review/merge gate. The harmony-client work can pin the branch HEAD during development and re-pin to the merge commit before the client PR merges.

---

## Task 4 (harmony-client): FEASIBILITY SPIKE — prove co-located DM delivery (CHECKPOINT)

**Goal:** Before any handshake wiring, prove that with the cache + tunnel-contact manually populated, a DM delivers between two co-located engines. This validates the whole transport chain (tunnel handshake co-located, announce propagation, path-table route, DM drain). **If this cannot pass, STOP and escalate — the approach is wrong.**

**Files:**
- Modify: `src-tauri/Cargo.toml:91` (pin bump)
- Test: `e2e-harness/tests/e2e_two_node.rs` (new ignored-by-default test `spike_manual_tunnel_dm_delivers`) OR a focused two-engine integration test under `src-tauri/tests/`.

- [ ] **Step 1: Bump the harmony-runtime pin** to the Task 3 branch HEAD:

`src-tauri/Cargo.toml:91`
```toml
harmony-runtime = { git = "https://github.com/zeblithic/harmony.git", rev = "<TASK3_HEAD_SHA>" }
```
Run: `cd src-tauri && cargo update -p harmony-runtime --precise <TASK3_HEAD_SHA> 2>/dev/null; cargo build --locked 2>&1 | tail -5`
Expected: builds against the new harmony rev (the new `ContactAddress::Tunnel` fields available).

- [ ] **Step 2: Write the spike test** — start two harness `serve` nodes (S2 setup, Reticulum off), then for EACH node: (a) call `apply_owner_device_update` for the peer with the peer's single device hash+pub, (b) register a `ContactAddress::Tunnel` for the peer (peer iroh node_id from the node's discovery/status, peer PQ keys from the node's published identity) via the same path Task 8 will add, (c) send a DM, (d) assert byte-delivery within a generous budget. Use the harness's existing S2 helpers (`friend handshake`, `send_dm`, `read_dm_plaintext_any`) and add a manual cache+contact bootstrap. Mark `#[ignore]` so it's opt-in.

Concretely, reuse the harness API surface: each node exposes RPCs; if no RPC exists yet to inject a contact, add a **temporary** debug RPC `__spike_register_tunnel_peer { identity_hash, node_id, relay_url, dsa, kem }` guarded behind `#[cfg(feature = "e2e")]` that calls `contact_store_mut().add() + push_event(ContactChanged)`, plus `__spike_register_device { owner, device_hash, identity_pub }` calling `apply_owner_device_update`. These become the basis for Task 8/9's real wiring.

- [ ] **Step 3: Run the spike**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && cargo test -p e2e-harness --features e2e -- --ignored spike_manual_tunnel_dm_delivers --nocapture`
Expected: the DM is delivered both directions. Capture node stderr (`HARMONY_E2E_KEEP=1`, `RUST_LOG=harmony_app=debug,harmony_runtime=debug,harmony_node=debug`) to confirm: tunnel handshake completes (`TunnelHandshakeComplete`), an announce arrives over `tunnel-*`, the path table learns the route, the DM drains.

- [ ] **Step 4: CHECKPOINT decision.** If green → proceed to Task 5; the rest is "make the handshake do automatically what the spike did manually." If red → STOP, capture the failing layer from the logs, and escalate to Jake with the evidence (do not build handshake wiring on a broken chain).

- [ ] **Step 5: Commit the spike** (kept as a regression artifact; the temp debug RPCs stay `#[cfg(feature = "e2e")]`):

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/Cargo.toml src-tauri/Cargo.lock e2e-harness/ src-tauri/src/
git commit -m "test(zeb-461): co-located DM-over-tunnel feasibility spike + harmony pin

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5 (harmony-client): extend friend handshake wire + bind in signature

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs:90-193` (the two structs), `:226-244` + the `sig_preimage` builder (`:250-265`)
- Modify: `src-tauri/tests/wire_format_zeb370_fixtures.rs` (regen pinned hex)
- Test: same fixtures file + a new round-trip test in `iroh_friend_acceptor.rs`

- [ ] **Step 1: Write the failing round-trip test** (iroh_friend_acceptor.rs tests mod)

```rust
#[test]
fn friend_request_roundtrips_with_device_bundle_and_reachability() {
    let req = FriendLinkRequest {
        from_addr: OwnerAddr([1u8; 16]),
        display: None,
        token_sig: None,
        eph_x25519_pub: [2u8; 32],
        enrollment: sample_enrollment_cert(), // existing test helper
        sig: [3u8; 64],
        sender_devices: vec![DeviceIdentityHash([4u8; 16])],
        device_identity_pubs: vec![Some([5u8; 64])],
        iroh_node_id: [6u8; 32],
        home_relay_url: Some("https://relay.example".into()),
        pq_dsa_pubkey: vec![7u8; 8],
        pq_kem_pubkey: vec![8u8; 8],
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&req, &mut buf).unwrap();
    let back: FriendLinkRequest = ciborium::from_reader(&buf[..]).unwrap();
    assert_eq!(req, back);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(friend_request_roundtrips_with_device_bundle)'`
Expected: FAIL — fields don't exist.

- [ ] **Step 3: Add fields to both structs.** Use single-char keys not already taken (`a,n,t,e,c,s` used). Use `d` (devices), `p` (pubs), `i` (iroh id), `r` (relay), `k` (kem), and `q` (dsa). Variable-length byte vecs use `serde_bytes`. Add to BOTH `FriendLinkRequest` and `FriendLinkAccepted` (each sends its own bundle):

```rust
    /// ZEB-461: the sender's bound device hashes (their OwnerDeviceCache bundle).
    #[serde(rename = "d", default)]
    pub sender_devices: Vec<DeviceIdentityHash>,
    /// ZEB-461: parallel 64-byte X25519||Ed25519 identity pubs (Some at known indices).
    #[serde(rename = "p", default)]
    pub device_identity_pubs: Vec<Option<[u8; 64]>>,
    /// ZEB-461: sender's iroh EndpointId, for the tunnel Contact.
    #[serde(rename = "i", default, serialize_with = "serialize_bytes_as_bstr", deserialize_with = "deserialize_bytes_from_bstr")]
    pub iroh_node_id: [u8; 32],
    /// ZEB-461: sender's home DERP relay, if any.
    #[serde(rename = "r", default)]
    pub home_relay_url: Option<String>,
    /// ZEB-461: sender's ML-DSA-65 public key (for the tunnel handshake).
    #[serde(rename = "q", default, with = "serde_bytes")]
    pub pq_dsa_pubkey: Vec<u8>,
    /// ZEB-461: sender's ML-KEM-768 public key.
    #[serde(rename = "k", default, with = "serde_bytes")]
    pub pq_kem_pubkey: Vec<u8>,
```

Note `device_identity_pubs: Vec<Option<[u8;64]>>` needs a serde helper for the inner `Option<[u8;64]>` — reuse the existing `opt_bstr64` module pattern as a `Vec` wrapper, or store pubs as `Vec<serde_bytes::ByteBuf>` and convert. Implement a small `vec_opt_bstr64` module mirroring `opt_bstr64` if the derive doesn't satisfy CBOR-bstr encoding.

- [ ] **Step 4: Bind the device bundle into the signature.** A MITM on the iroh stream must not swap the device list. Extend `sig_preimage` (and both `friend_request_sig_preimage`/`friend_accept_sig_preimage`) to include the device bundle. Add a `devices_digest: &[u8;32]` argument = `sha256(canonical_cbor((sender_devices, device_identity_pubs)))`, and include it in the `Preimage` struct. Update both preimage fns + every call site (`process_friend_request:639`, the requester build site, and tests). (Reachability + PQ keys are routing hints — a wrong value yields a failed tunnel, not a compromise — so they need not be signed; document that choice in a comment.)

- [ ] **Step 5: Run the round-trip test**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(friend_request_roundtrips_with_device_bundle)'`
Expected: PASS.

- [ ] **Step 6: Regenerate the pinned wire fixtures.** `tests/wire_format_zeb370_fixtures.rs` pins exact hex for these structs; the new fields change the hex. Re-read the file, regenerate the expected hex from the deterministic fixtures (the test prints actual-vs-expected on failure — run it, copy the new canonical hex into the expected constants, keeping the empty-default cases asserting back-compat).

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(wire_format_zeb370)'`
Expected: PASS after updating the pinned constants.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/iroh_friend_acceptor.rs src-tauri/tests/wire_format_zeb370_fixtures.rs
git commit -m "feat(zeb-461): carry device bundle + reachability + PQ keys in friend handshake wire

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6 (harmony-client): populate self's bundle when building request + accept

**Files:**
- Modify: `src-tauri/src/lib.rs` (`connectivity_link_friend_iroh_inner` request-build site, ~39815) + `FriendAcceptorConfig`/`process_friend_request` accept-build site.
- Create: `src-tauri/src/dm_tunnel_contact.rs` — `pub fn self_device_bundle(identity_pub_64: [u8;64]) -> (Vec<DeviceIdentityHash>, Vec<Option<[u8;64]>>)` returning the single-device bundle.

- [ ] **Step 1: Write the failing unit test** (dm_tunnel_contact.rs)

```rust
#[test]
fn self_device_bundle_is_single_device_with_matching_pub() {
    let pub64 = [1u8; 64];
    let (devices, pubs) = self_device_bundle(pub64);
    assert_eq!(devices.len(), 1);
    assert_eq!(pubs, vec![Some(pub64)]);
    // hash must equal derive_device_hash_from_identity_pub(&pub64)
    let expected = crate::dm_signing::derive_device_hash_from_identity_pub(&pub64).unwrap();
    assert_eq!(devices[0].0, expected);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(self_device_bundle_is_single_device)'`
Expected: FAIL — function not defined.

- [ ] **Step 3: Implement `self_device_bundle`** (dm_tunnel_contact.rs):

```rust
use crate::owner_state_types::DeviceIdentityHash;

/// The local node's own single-device bundle for the friend handshake.
/// (Multi-device enumeration is a future refinement; alpha nodes are single-device.)
pub fn self_device_bundle(identity_pub_64: [u8; 64]) -> (Vec<DeviceIdentityHash>, Vec<Option<[u8; 64]>>) {
    let hash = crate::dm_signing::derive_device_hash_from_identity_pub(&identity_pub_64)
        .expect("our own identity pub must derive a device hash");
    (vec![DeviceIdentityHash(hash)], vec![Some(identity_pub_64)])
}
```
Add `mod dm_tunnel_contact;` to lib.rs.

- [ ] **Step 4: Thread self's identity pub + PQ keys + iroh identity to the build sites.** The values are computed at node construction (lib.rs:3044-3055: `local_dsa_pubkey`, `local_kem_pubkey`, `identity_pub_64`). Store them on `NodeState` (the struct already holds many such snapshots) so the requester IPC reads `g.identity_pub_64 / g.local_dsa_pubkey / g.local_kem_pubkey`, and pass them into `FriendAcceptorConfig` for the accept side. Self iroh id/relay: `iroh_endpoint.node_id()` (as bytes) + `iroh_endpoint.home_relay()`.

- [ ] **Step 5: Populate the fields when building the request** (lib.rs request-build, ~39815) and the accept (`process_friend_request`, before building `FriendLinkAccepted`). Both: `let (sender_devices, device_identity_pubs) = self_device_bundle(self_identity_pub_64);` then set `iroh_node_id`, `home_relay_url`, `pq_dsa_pubkey`, `pq_kem_pubkey`.

- [ ] **Step 6: Run + commit**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(self_device_bundle)'` (PASS), then:
```bash
git add src-tauri/src/dm_tunnel_contact.rs src-tauri/src/lib.rs src-tauri/src/iroh_friend_acceptor.rs
git commit -m "feat(zeb-461): populate self device bundle + reachability + PQ in friend handshake

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 7 (harmony-client): populate `OwnerDeviceCache` on handshake receipt

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs` `process_friend_request` (after `apply_friend_update`, ~line 690) — populate from `req`.
- Modify: `src-tauri/src/lib.rs` `connectivity_link_friend_iroh_inner` (after `apply_friend_update`, ~39992) — populate from `accepted`.
- Test: `iroh_friend_acceptor.rs` tests mod.

- [ ] **Step 1: Write the failing test** — `process_friend_request` populates the cache for the requester.

```rust
#[test]
fn process_friend_request_populates_owner_device_cache() {
    let mut state = test_owner_state(); // existing helper
    let req = sample_friend_request_with_devices(); // builds req with sender_devices+pubs
    let _ = process_friend_request(&mut state, hlc(1), &req, /* self params */ ..).unwrap();
    let entry = state.owner_device_cache.devices.get(&req.from_addr).expect("cache entry");
    assert_eq!(entry.devices, req.sender_devices);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(process_friend_request_populates_owner_device_cache)'`
Expected: FAIL — cache empty.

- [ ] **Step 3: Implement** — in `process_friend_request`, after the `apply_friend_update`, mirror `handle_invite` (dm_outbox.rs:1538-1551). Reuse the `learned_at: Hlc` parameter already passed in — it is the caller's local `next_hlc()` (NOT the peer's claimed time), so it satisfies the `handle_invite` anti-forgery rule (the LWW HLC must record when WE learned the devices):

```rust
    // ZEB-461: learn the requester's devices so we can route DMs to them.
    // `learned_at` is this node's local HLC (caller's next_hlc) — safe per the
    // handle_invite anti-forgery comment (never the peer's claimed timestamp).
    let outcome = state.apply_owner_device_update(
        req.from_addr,
        req.sender_devices.clone(),
        req.device_identity_pubs.clone(),
        learned_at.clone(),
    );
    if let crate::owner_state_crdt::ApplyOutcome::Rejected(reason) = outcome {
        return Err(FriendHandshakeError::ApplyRejected(format!("device cache: {reason:?}")));
    }
```

In `connectivity_link_friend_iroh_inner` (lib.rs, inside the same lock as `apply_friend_update`, ~39992), do the same using `accepted.sender_devices`/`accepted.device_identity_pubs` and `payload.inviter_addr`.

- [ ] **Step 4: Run the test + the requester-side equivalent**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(populates_owner_device_cache)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/iroh_friend_acceptor.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-461): populate OwnerDeviceCache from the friend handshake bundle

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 8 (harmony-client): contact-registration channel into the event loop

**Files:**
- Modify: `src-tauri/src/dm_tunnel_contact.rs` — request type + a `tokio::sync::mpsc` sender wrapper.
- Modify: `src-tauri/src/lib.rs` — create the channel at node start (mirror `unicast_send_tx`, lib.rs:2652), drain it in the event loop, and on each request call `runtime.contact_store_mut().add()`/`get_mut(..)` + `runtime.push_event(RuntimeEvent::ContactChanged { identity_hash })`.
- Test: `src-tauri/tests/` integration or a `dm_tunnel_contact.rs` unit test for the builder.

- [ ] **Step 1: Write the failing test for the contact builder**

```rust
#[test]
fn build_tunnel_contact_carries_pq_keys() {
    let c = build_tunnel_contact([1u8;16], [2u8;32], Some("r".into()), vec![3u8;1952], vec![4u8;1184], 100);
    match &c.addresses[0] {
        harmony_contacts::ContactAddress::Tunnel { node_id, peer_dsa_pubkey, peer_kem_pubkey, .. } => {
            assert_eq!(*node_id, [2u8;32]);
            assert_eq!(peer_dsa_pubkey.as_deref(), Some(&[3u8;1952][..]));
            assert_eq!(peer_kem_pubkey.as_deref(), Some(&[4u8;1184][..]));
        }
        _ => panic!("expected Tunnel"),
    }
    assert!(c.peering.enabled);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(build_tunnel_contact_carries_pq_keys)'`
Expected: FAIL — function not defined.

- [ ] **Step 3: Implement the builder + request type** (dm_tunnel_contact.rs):

```rust
pub struct RegisterTunnelPeer {
    pub identity_hash: [u8; 16],
    pub node_id: [u8; 32],
    pub relay_url: Option<String>,
    pub dsa: Vec<u8>,
    pub kem: Vec<u8>,
}

pub fn build_tunnel_contact(
    identity_hash: [u8; 16], node_id: [u8; 32], relay_url: Option<String>,
    dsa: Vec<u8>, kem: Vec<u8>, added_at: u64,
) -> harmony_contacts::Contact {
    harmony_contacts::Contact {
        identity_hash,
        display_name: None,
        peering: harmony_contacts::PeeringPolicy { enabled: true, priority: harmony_contacts::PeeringPriority::Normal },
        added_at, last_seen: None, notes: None,
        addresses: vec![harmony_contacts::ContactAddress::Tunnel {
            node_id, relay_url, direct_addrs: vec![],
            peer_dsa_pubkey: Some(dsa), peer_kem_pubkey: Some(kem),
        }],
        replication: None,
    }
}
```

- [ ] **Step 4: Wire the channel** — at node start (lib.rs ~2652, beside `unicast_send_tx`), create `let (tunnel_peer_tx, mut tunnel_peer_rx) = tokio::sync::mpsc::channel::<RegisterTunnelPeer>(64);`, stash `tunnel_peer_tx` on `NodeState` (cleared in `stop_node` like `unicast_send_tx`), and in the event loop add a `recv` arm that, on each `RegisterTunnelPeer`, does (idempotent — use `get_mut` to merge if the contact exists, else `add`):

```rust
Some(req) = tunnel_peer_rx.recv() => {
    let contact = crate::dm_tunnel_contact::build_tunnel_contact(
        req.identity_hash, req.node_id, req.relay_url, req.dsa, req.kem, wall_now_secs());
    if runtime.contact_store().get(&req.identity_hash).is_none() {
        let _ = runtime.contact_store_mut().add(contact);
    } else if let Some(existing) = runtime.contact_store_mut().get_mut(&req.identity_hash) {
        // merge: ensure a Tunnel address with PQ keys is present
        existing.addresses.retain(|a| !matches!(a, harmony_contacts::ContactAddress::Tunnel { .. }));
        existing.addresses.extend(contact.addresses);
    }
    runtime.push_event(crate::runtime::RuntimeEvent::ContactChanged { identity_hash: req.identity_hash });
}
```

- [ ] **Step 5: Run + commit**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(build_tunnel_contact)'` (PASS), then:
```bash
git add src-tauri/src/dm_tunnel_contact.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-461): contact-registration channel for tunnel peers

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 9 (harmony-client): register the tunnel contact from both handshake sites

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs` — add a `tunnel_peer_tx: Option<tokio::sync::mpsc::Sender<RegisterTunnelPeer>>` to `IrohFriendHandshakeAcceptor`/`FriendAcceptorConfig`; after `emit_friend_added` (lines 1113/1143), send a `RegisterTunnelPeer` built from `req`'s reachability+PQ.
- Modify: `src-tauri/src/lib.rs` — after the requester's cache population, send a `RegisterTunnelPeer` built from `accepted`'s reachability+PQ via the `NodeState`'s `tunnel_peer_tx`.
- Test: integration test that drives a handshake and asserts a `ContactChanged`/contact appears (or extends the Task 4 spike to run via the real handshake).

- [ ] **Step 1: Write the failing integration test** — drive the acceptor with a request carrying reachability+PQ; assert a `RegisterTunnelPeer` is sent on the channel.

```rust
#[tokio::test]
async fn accepting_a_friend_emits_register_tunnel_peer() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let acceptor = test_acceptor_with_tunnel_tx(tx); // helper: wires tunnel_peer_tx
    let req = sample_friend_request_with_devices_and_reachability([6u8;32], vec![7u8;1952], vec![8u8;1184]);
    acceptor.handle_request(req).await.unwrap();
    let got = rx.try_recv().expect("RegisterTunnelPeer sent");
    assert_eq!(got.node_id, [6u8;32]);
    assert_eq!(got.dsa.len(), 1952);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(accepting_a_friend_emits_register_tunnel_peer)'`
Expected: FAIL — acceptor has no `tunnel_peer_tx`.

- [ ] **Step 3: Implement** — add the field + send. Acceptor (after `self.emit_friend_added(&req)` at both 1113 and 1143):

```rust
    if let Some(tx) = self.tunnel_peer_tx.as_ref() {
        let _ = tx.try_send(crate::dm_tunnel_contact::RegisterTunnelPeer {
            identity_hash: req.from_addr.0,
            node_id: req.iroh_node_id,
            relay_url: req.home_relay_url.clone(),
            dsa: req.pq_dsa_pubkey.clone(),
            kem: req.pq_kem_pubkey.clone(),
        });
    }
```
Requester (lib.rs, after cache population): same, from `accepted` + `payload.inviter_addr.0`, via `g.tunnel_peer_tx`.

- [ ] **Step 4: Run + commit**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(emits_register_tunnel_peer)'` (PASS), then:
```bash
git add src-tauri/src/iroh_friend_acceptor.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-461): register a tunnel contact on friend-handshake completion

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 10 (harmony-client): e2e-harness S2 hard-asserts DM byte-delivery

**Files:**
- Modify: `e2e-harness/tests/e2e_two_node.rs` (`s2_friend_dm_exchange`, ~264-394) — replace the characterize-only block (lines ~335-341) with hard asserts; remove the temporary spike RPCs from Task 4 (the real handshake now does the work).

- [ ] **Step 1: Flip the assertion** — replace the `eprintln!`/`.is_ok()` characterization with:

```rust
    let delivered_a_to_b = poll_until(Duration::from_secs(60), || async {
        let msgs = read_dm_plaintext_any(&bob, &candidates).await?;
        Ok(msgs.iter().any(|(_, body)| body == b"hello-from-alice").then_some(()))
    }).await;
    assert!(delivered_a_to_b.is_ok(), "S2: alice→bob DM must deliver over the friend-established tunnel");
    // symmetric assert for bob→alice
```

- [ ] **Step 2: Run S2**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && cargo test -p e2e-harness --features e2e -- s2_friend_dm_exchange --nocapture`
Expected: PASS — DM delivers both directions via the friend handshake alone (no manual bootstrap).

- [ ] **Step 3: Commit**

```bash
git add e2e-harness/tests/e2e_two_node.rs src-tauri/src/
git commit -m "test(zeb-461): S2 hard-asserts friend DM byte-delivery over the established tunnel

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 11: full gates + cross-repo PRs

- [ ] **Step 1: Local gates** (from `src-tauri/`):

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --features test-fixtures -E 'test(friend) + test(tunnel) + test(device_cache) + test(zeb370)'
```
Expected: green. (Reserve full `--all-targets` nextest for CI per the relink-cost rule; run the friend/tunnel-scoped tests + the touched integration tests locally.)

- [ ] **Step 2: Re-pin to the merged harmony commit** once the harmony PR (Task 3) is approved+merged: set `src-tauri/Cargo.toml:91` `rev` to the harmony merge commit, `cargo update -p harmony-runtime --precise <merge_sha>`, rebuild, commit.

- [ ] **Step 3: Push + open the harmony-client PR** (do NOT self-merge either PR):

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git push -u origin zeb-461-dm-device-cache
gh pr create --repo zeblithic/harmony-client --title "ZEB-461: cross-WAN DM via friend-established Reticulum tunnel" --body "<summary: the two-blocker finding, the tunnel-chain approach, S2 now hard-asserts byte-delivery; pairs with harmony PR for the ContactAddress::Tunnel PQ fields>

🤖 Generated with [Claude Code](https://claude.com/claude-code)"
```

- [ ] **Step 4:** Run the bot loop (Qodo+CodeAnt → address → one CodeRabbit pass → check Greptile at convergence) on the harmony-client PR; ensure the harmony PR is reviewed too. Keep parent epics out of PR bodies. Pushover Jake once at ready-to-merge.

---

## Out of scope (tracked separately)

- Multi-device device-bundle enumeration (alpha is single-device; `self_device_bundle` covers one device).
- Community-co-member (non-friend) DM bootstrap.
- Butler / sealed-relay store-and-forward (ZEB-418/458).
- Owner-global Zenoh topic routing (ZEB-466, Ildwyn) — coordinate via ZEB-470.
