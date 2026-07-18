# ZEB-682 + ZEB-683 — Feed-authority: quorum self-publish + rotation maintenance

**Branch:** `zeb-682-683-feed-authority-quorum-and-rotation` off `main@6c5461ab`.
**Tickets:** ZEB-682 (thread the signer bundle so quorum-issued devices migrate), ZEB-683 (maintain/refresh the authority record across rotation). Bundled: the rotation re-publish path needs the bundle threading anyway.
**Parent context:** ZEB-678 (design spec `docs/specs/2026-07-12-zeb-678-vine-follow-revocation-design.md`, S1/S2/S3 plans), ZEB-677 (quorum wiring, S1–S5 merged).

## 1. Verified current state (all claims re-checked first-hand on this branch)

| # | Fact | Where |
|---|------|-------|
| V1 | `build_active_authority` hardcodes `signer_certs_cbor_hex: String::new()` — quorum-issued self-publish cannot verify | `feed_authority.rs:135` |
| V2 | The **reaction wire** also hardcodes `wire.signer_certs_cbor_hex = String::new()`; receiver-side `verify_reaction_v2` runs whenever `device_sig` is present and its failure **rejects the reaction** — so a quorum-issued device's reactions are dropped by every receiver today | `lib.rs:15485`, `vine_feed_cache.rs:818-833` |
| V3 | `own_cert_bundle(state, cert)` (Master → `[]`, Quorum → signer certs from `state.enrollments`) exists but has **zero production callers** (tests only) | `enrollment_verify.rs:162-173` |
| V4 | The trust doc is reachable at runtime: `NodeState.owner_trust_doc: Option<Arc<tokio::sync::Mutex<harmony_owner::state::OwnerState>>>` | `lib.rs:1404-1405` |
| V5 | **Premise correction (ZEB-683):** a fleet/KeyTree epoch bump does NOT rotate the `#2` device key — it rotates the fleet *data-encryption* KeyTree only (`plan_fleet_epoch_bump*` never touches `DmOutbox.community_signing_key`/`enrollment_cert`). The `#2` key/cert changes only via (re-)enrollment | `owner_commands.rs:194-320`, `lib.rs:6272-6404` |
| V6 | `EnrollmentCert.issued_at: u64` (Unix s) is **inside the master/quorum-signed payload** — an authenticated, only-issuer-mintable ordering key | `harmony/crates/harmony-owner/src/certs/enrollment.rs:18,48` |
| V7 | `FeedAuthorityCache` is in-memory only (rebuilt from live samples each boot; not in `VineFeedDiskV1`) — pin-shape changes need **no disk migration** | `vine_feed_cache.rs:332,362` |
| V8 | Publish gates are once-per-boot bools `vine_authority_published` / `vine_feed_binding_stamped`; publish is a plain Zenoh PUT (no queryable, no re-offer) — a follower that subscribes later never receives the record until the next boot's first vine publish | `lib.rs:887-893,14952-15045` |
| V9 | The cache pin (`PinnedAuthority`) stores no cert metadata — nothing orderable for supersession | `feed_authority.rs:347-353` |
| V10 | `verify_authority` decodes the enrollment cert and already verifies populated bundles (ingest side is ready) | `feed_authority.rs:295-342` |

So the **real** ZEB-683 staleness triggers are: (a) cert renewal (same binding, embedded cert approaches/passes expiry — late followers reject the old record at ingest), (b) genuine `#2` change for the same device via re-enrollment (binding change — needs authenticated supersession), (c) distribution loss (V8 — no re-offer). Epoch bumps per se are NOT a trigger (V5); "rotation" below means re-enrollment.

## 2. Design

### A. ZEB-682 — thread the signer bundle (publish side)

1. `build_active_authority` gains a `signer_certs: &[EnrollmentCert]` parameter; sets `signer_certs_cbor_hex = encode_certs(signer_certs)?` (empty slice ⇒ `""` ⇒ serde-omitted, wire-compatible).
2. New `lib.rs` helper `own_signer_bundle(state, cert) -> Vec<EnrollmentCert>`: Master-issued → `vec![]` without locking; Quorum-issued → async-lock `owner_trust_doc` and call `own_cert_bundle`. Missing trust doc ⇒ empty bundle (the self-check below then keeps the feed on legacy, honestly).
3. `publish_feed_authority_if_needed` builds with the bundle, then **self-verifies** `verify_authority(&rec, now_ms/1000)` before publishing; on `Err` → warn + release the gate (never publish a record receivers will drop — also catches an expired own cert and a short bundle).
4. Reaction path: fill `wire.signer_certs_cbor_hex` from the same bundle (fixes V2), and validate the enrollment + bundle publisher-side first (R1, CodeRabbit): a quorum device that cannot present a verifying bundle publishes the LEGACY `#3`-only reaction (which every receiver accepts, §3.3 dual-path) instead of a v2 reaction receivers silently drop.

Out of scope: the friend-handshake self-bundle (`iroh_friend_acceptor.rs:1568`) — separate documented S4 deferral, different seam.

### B. ZEB-683 — publish-side maintenance: fingerprint gates

Replace the two boot-bools with fingerprints in a shared `VineAuthorityGates` cell:

- `published_fp` / `stamped_fp: Option<String>`; `fp = {publisher_key_hex}:{blake3(cert_cbor ‖ bundle_cbor)}` — the FULL published material (R1, CodeRabbit + Qodo): a same-second cert re-issue with different content and a bundle-only change (a signer's cert renewed in the trust doc) each re-arm; `issued_at` alone was collision-prone.
- Gate = `stored != Some(current_fp)`. The atomic reserve/release pattern is preserved (release restores the *previous* value, not `None`). The re-offer task's completion re-arm is a compare-and-set against the fingerprint it observed at tick start, so a stale offer never overwrites a newer reservation (R1); a lost CAS is benign — the newer material's publish path owns the gate.
- Gates are RESET in `stop_inner` / the start-path stale-cleanup (R1, Qodo): boot-time peers don't bump the transport epoch, so a gate persisted across an in-process restart would leave the fresh session with no publish and no re-offer trigger until a peer churns.

This re-publishes exactly when the material changes: first publish (None), cert renewal / re-issue, bundle change — and stays once-per-boot in the steady state. If the node ever re-enrolls as a new device (new `device_id` + keys, same `#3`), the fingerprint also moves and the new binding publishes; followers holding the old pin drop it (first-write-wins, §C), fresh followers pin it.

### C. ZEB-683 — follower-side repin: **FINDING — unrepresentable; no cache change ships**

The originally-designed authenticated supersession (strictly-newer `EnrollmentCert.issued_at` for the same `device_id` authorizes a `publisher_key` repin) was implemented, and its red-first test refuted the *premise*: **`EnrollmentCert::verify` enforces `device_id == hash(device_pubkeys)`** (`harmony/crates/harmony-owner/src/certs/enrollment.rs:132-134`, Master arm; the Quorum arm runs the same device-id consistency check). A record pairing the pinned `device_id` with a different `#2` key can never pass `verify_authority` — the supersession arm is unreachable dead code, and shipping unreachable security-sensitive machinery is worse than shipping none.

**Consequences (now documented in `PinnedAuthority`'s doc + pinned by tests):**
- A `#2` key change mints a **new device identity** — that is device *replacement*, and a replaced device's feed does not survive by design (spec §2). Feed continuity across a key change remains the §11 "canonical owner feed" follow-up, exactly where the design spec already put it.
- What CAN change for the same `device_id` is cert **metadata only**: renewal (`issued_at`/`expires_at`) and Master↔Quorum re-issue over the same keys. Both keep the binding, so they verify and land as `BenignRefresh` — the existing cache is already correct, and each record re-verifies its own embedded cert.
- The cache therefore ships **unchanged** (first-write-wins absolute, sticky-revoked): the anti-rebind evasion property is enforced twice over (cert hash invariant + first-write-wins), and the ticket's sketched "n_sig proves same owner → repin" is doubly closed.

The real ZEB-683 staleness problem is entirely **publish-side**: an aging record with an expiring embedded cert (late followers reject at ingest) and distribution loss (V8). Sections B and D solve those.

### D. ZEB-683 — distribution re-offer (cadence)

Spawn a `run_vine_authority_reoffer` task subscribed to the **transport-epoch watch** (peer up-edges — the exact moment new receivers appear; same trigger semantics as `run_epoch_republish` for the dataset engines). On each bump: if `vine_authority_published_fp` is `Some` (this device has migrated), rebuild the record (fresh `updated_at`) and re-publish + re-stamp best-effort. If `None`, do nothing — the first vine publish remains the migration trigger (no force-migrating feeds that never published). No blind interval timer: dual-signing keeps a missing authority record safe (legacy path), so up-edge-driven re-offer is the right cost/benefit. Spawned (never inlined) per the start_node inline-await hazard.

## 3. Test plan (red-first)

1. `build_active_authority` + quorum world (`quorum_fixtures`): with bundle → `verify_authority` OK; empty bundle for a quorum cert → verify fails (pins V1's fix).
2. Publish self-check: a non-verifying build (quorum cert, empty bundle) is NOT published and releases the gate.
3. Cache (post-finding): (a) a same-device/new-key record is rejected at verification (`cache_rejects_same_device_key_change_record_zeb683` — pins the invariant §C rests on); (b) a cert renewal (same keys, newer `issued_at`) verifies and lands as `BenignRefresh` with the binding unchanged (`cache_cert_renewal_same_binding_is_benign_refresh_zeb683`). Cross-device rebind + sticky-revoked remain covered by the existing S1 tests.
4. Fingerprint gate: same material twice → one publish; changed cert (new issued_at) → second publish (mpsc-captured `PublishRequest`s).
5. Reaction bundle: quorum device's reaction wire carries the bundle → `verify_reaction_v2` passes (red today).
6. Re-offer: watch-bump → a fresh authority publish lands on the mpsc; fp `None` → no publish.

## 4. Out of scope / follow-ups

- Friend-handshake self-bundle threading (documented S4 deferral, `iroh_friend_acceptor.rs:1568`).
- Canonical owner-feed unification across devices (design §11 item 1).
- Authority-cache persistence (wire-only by design intent; dual-signing is the bootstrap story).
