# ZEB-679 — Storage-buddy record signing migration #3 → #2 (revocation-aware storage)

**Branch:** `zeb-679-storage-buddy-signing-migration` off `main@62ce2b2e` (post-#488).
**Ticket:** ZEB-679 (ZEB-668 §9 follow-up 3). **Family:** ZEB-580 (DM packets), ZEB-678 (vines/follow lists — the direct template), ZEB-682/683 (#488 — bundle threading + the rotation-unrepresentability finding this plan leans on).
**Specs:** `docs/specs/2026-07-11-zeb-668-device-management-design.md` §9, `docs/specs/2026-07-11-zeb-669-storage-buddies-design.md` §3 (record posture).

## 1. Verified current state (all first-hand on this branch)

| # | Fact | Where |
|---|------|-------|
| V1 | Exactly THREE production sign sites, all `lib.rs`: `build_signed_pledge_list` (sync, NodeState std-lock held), `build_signed_backup_set` (sync, **re-signs inside the wire-cap shrink loop**), `build_signed_hosting_report_with` (sync fn run inside the spawned 30 s hosting task). The event_loop/buddy_pin_planner `sign_*` hits are `#[cfg(test)]` fixtures | `lib.rs:16721,16781,16867` |
| V2 | Sites A/B reach `guard.dm_outbox` (tokio mutex) and can mirror the follow-list `try_lock` dual-sign posture (contended → `#3`-only, self-heals next publish). Site C receives only `&PrivateIdentity` — the `#2` material must be **newly threaded** through `spawn_hosting_report_publisher` and its spawn call | `lib.rs:16562-16584` (precedent), `lib.rs:16924,12279` |
| V3 | Ingest is `StorageRecordStore::on_{pledge_list,backup_set,hosting_report}_sample` — verify-first (cap → parse → `#3` sig+address binding → topic shape → caps → LWW); `Rejected` = zero state effect; the store holds **no identity/trust dependency**; router `note_storage_record_sample` has only store+key+payload+now_ms | `storage_records.rs:225-359`, `event_loop.rs:7075-7101` |
| V4 | Legacy canonical bytes are frozen by golden-pin tests — the migration adds `-v2` domains, never mutates v1 bytes | `storage_signing.rs:451-516` |
| V5 | Every revocation store is keyed `(master owner_id, #2 ed25519)`. The enforcement query is `RevokedDeviceProjection::is_revoked(&OwnerAddr, &[u8;32])` — sticky, threaded by handle, fed from community materialized views + DM store + friend-link retire-announce carry; already consumed by friend/PEX/intro/relay verifiers (deliberately not DM-specific) | `revoked_device_projection.rs:46-49`, `lib.rs:3317-3339,4472` |
| V6 | `verify_enrollment_any_issuer` checks cert validity/owner-binding/expiry ONLY; revocation is always layered outside by callers (friend-acceptor precedent: chokepoint first, then `is_revoked`) | `enrollment_verify.rs:66-123`, `iroh_friend_acceptor.rs:970-1006` |
| V7 | **No map exists from a `#3` storage signer to `(owner_id, #2 key)`** — storage records are keyed by the per-device `#3` `owner_address`; the vine `FeedAuthorityRecord` is the only binding precedent and is vine-topic-specific. Storage ingest therefore cannot name the revocation principal today | recon (both agents), `storage_records.rs:112-119` |
| V8 | `DmOutbox` carries the `#2` material (`community_signing_key`, `enrollment_cert`, construction-asserted consistent); `own_signer_bundle`/`signer_bundle_from_doc` (#488) assemble the quorum bundle (Master → `[]` no-lock fast path) | `dm_outbox.rs:640-723`, `lib.rs` (#488) |
| V9 | Same-device `#2` rotation is UNREPRESENTABLE (`device_id == hash(device_pubkeys)`, both issuer arms) — a pinned `(owner_id, device key)` for an address can never legitimately change; cert renewal keeps the binding | `harmony/crates/harmony-owner/src/certs/enrollment.rs:132-134` (#488 finding) |
| V10 | Pledges/backup-sets persist to `storage_records.json` (versioned; corrupt/foreign → empty store); hosting reports are in-memory + staleness-swept. Boot republishes pledge/backup records; hosting publishes every 30 s | `storage_records.rs:125-183,356,403-406`, `lib.rs:12267-12268` |

## 2. Design

### A. Wire: additive v2 fields on all three payloads

On `PledgeListPayload` / `BackupSetPayload` / `HostingReportPayload` (all `#[serde(default, skip_serializing_if …)]`, camelCase — wire-compatible with old receivers):

- `owner_id: Option<String>` (32 hex = 16-byte master owner id)
- `enrollment_cbor_hex: Option<String>` — the signer's `EnrollmentCert`
- `signer_certs_cbor_hex: String` (empty ⇒ omitted) — quorum bundle
- `device_sig: Option<String>` — `#2` signature over the `-v2` canonical bytes (same field set, bumped domains `harmony-storage-{pledges,backup-set,hosting}-v2`)
- `binding_sig: Option<String>` — **`#3` signature** over `STORAGE_BINDING_DOMAIN ‖ owner_address ‖ owner_id ‖ device_ed25519_pub` (`harmony-storage-binding-v1`)

**Why `binding_sig` (the load-bearing piece):** storage records have no authority record (vines) and no session handshake (DMs) — v2 material is self-carried. Without a `#3` countersignature an attacker can take a victim's legacy-signed record and attach their OWN enrollment + device_sig (both verify — they're the attacker's real credentials), hijacking attribution and, worse, squatting the victim's first-write-wins pin (§C). `binding_sig` is the per-record inlining of `FeedAuthorityRecord.n_sig`: only the `#3` holder can bind their address to an `(owner_id, #2 key)` pair. Records stay dual-signed always: legacy `#3` `sig` proves address ownership of the CONTENT, `binding_sig` proves address↔owner binding, enrollment proves the device is the owner's, `device_sig` proves the enrolled device produced the content.

### B. Sign side (all three sites dual-sign; legacy path untouched)

1. Legacy `#3` sign exactly as today (golden pins intact; old receivers unaffected).
2. `#2` material: Sites A/B — `dm_outbox.try_lock()`; **contention falls back to the last-known-good cached material** (`NodeState.storage_v2_cache`, R1 CodeRabbit — a pinned receiver rejects legacy, so contention must not downgrade a migrated publisher; the cache is warmed by every uncontended snapshot and by Site C's per-tick refresh, and RESET on stop/stale-cleanup so material never crosses an owner switch). Cold-cache contention publishes honestly-legacy (documented residual; heals at the next publish/tick). Site C — thread the `dm_outbox` Arc through `spawn_hosting_report_publisher`; the task snapshots `(sk, cert, bundle)` EVERY tick with real async locks.
3. Bundle: Master-issued → `[]` (no lock); Quorum-issued → trust-doc bundle (Sites A/B `try_lock` best-effort → cached-material fallback on contention; Site C async lock).
4. **Publisher-side self-check (#488 lesson, + R1):** before attaching v2 fields, require the signing key's verifying key to EQUAL the enrollment's device key (cert validity alone doesn't prove the pair matches — R1, CodeRabbit + Qodo converged), then `verify_enrollment_any_issuer(cert, bundle, None, now)`; failure → publish legacy-only (a delivered legacy record beats a v2 record every receiver drops). A validation failure never falls back to cached material — fresh material is authoritative.
5. Backup-set shrink loop re-signs BOTH signatures per round (v2 canonical covers entries).
6. `binding_sig` minted with the same `#3` identity already at hand.

### C. Verify side: self-anchored v2 + first-write-wins signer pin + revocation

`storage_signing.rs` gains a shared `verify_record_v2(...) -> Result<VerifiedStorageSigner { owner_id: [u8;16], device_ed25519: [u8;32] }, String>`: bounded hex decodes (Qodo posture) → `verify_enrollment_any_issuer` → `binding_sig` verified against the record's legacy `identity_pub` (v2 REQUIRES the legacy fields — dual-signed always) → `device_sig` over `-v2` canonical bytes against the enrollment's device key.

`StorageRecordStore` ingest, after the existing legacy verify + topic check:

- **v2 material present:** `verify_record_v2` (any failure → `Rejected`; present-but-invalid never falls back — vine posture). Then `projection.is_revoked(owner_id, device_key)` → `Rejected("signer device revoked")`. Then the pin: unpinned address → pin `(owner_id, device_ed25519)` (first-write-wins); pinned → must match exactly (V9: the binding can never legitimately change; mismatch → `Rejected` rebind attempt).
- **v2 absent:** pinned address → `Rejected("legacy record from migrated owner")` (anti-downgrade ratchet); unpinned → legacy accept (bootstrap/compat).

**Pin persistence (deliberate divergence from the vine in-memory cache):** `signer_pins: HashMap<owner_address, {owner_id_hex, device_ed25519_hex}>` persisted in `storage_records.json` (additive, serde-default — old files load with no pins). Vines could stay in-memory because the wire-durable revoked-authority record re-arms after restart; storage has no authority record, so an in-memory pin would let a revoked device (which holds its `#3` forever and will never publish v2 again) publish accepted legacy records after every receiver restart — permanently. The persisted pin is the only durable revocation anchor.

**Pin cap (`MAX_SIGNER_PINS = 4096` > 3-family live-owner union):** eviction takes dead pins (no live record) first, and among them the **NEWEST `pinned_at_ms` first** (R1, CodeRabbit — a flood of attacker-minted throwaway pins evicts its OWN just-minted pins; a long-established migrated address's ratchet pin is never the newest and survives; legit fresh pins are live-backed at mint time). A pin backing a live record is never forced out.

**Retroactive revocation (R1, Qodo):** `purge_revoked(&RevokedDeviceProjection)` runs each auto-pin engine tick — records admitted BEFORE the projection learned their signer's revocation are dropped across all three families (the revoked device has no reason to republish), while their pins stay (sticky ratchet + revoked-at-ingest block any re-admission).

**Threading:** `note_storage_record_sample` gains `&RevokedDeviceProjection` + `now_secs` params; the event loop receives a projection clone (same handle pattern as friend/PEX acceptors). `StorageRecordStore` itself stays dependency-free — the projection is passed per-call.

### D. Honesty ledger (documented residuals)

- Never-migrated (`#3`-only) publishers keep working and carry no revocation semantics — retires organically as devices republish post-upgrade (boot republish + 30 s hosting tick re-pin fast).
- A receiver that never learned a revocation (no shared community, no friend-link carry) admits the revoked device's v2 records — inherent to replicated trust, same residual as the DM cutoff.
- Sites A/B under lock contention publish legacy-only for that round (existing follow-list posture; self-heals).

## 3. Test plan (red-first)

1. `storage_signing`: v2 sign/verify round-trips ×3; **swap-attack test** (attacker's valid enrollment+device_sig on victim's record → `binding_sig` failure); binding forged by non-`#3`-holder fails; tampered v2 fields fail; golden v1 pins UNCHANGED; serde pins for new fields; v2 domains distinct.
2. `storage_records` ingest: valid v2 → accepted + pinned; revoked signer (seeded projection) → rejected; legacy-after-pin → rejected (ratchet); rebind (same address, different owner/key) → rejected; pin survives disk reload; v2-present-but-invalid → rejected, zero state effect; unpinned legacy → accepted.
3. `lib.rs` publish: dual-sign present when outbox material available (extend `signed_state()` with outbox fixture); self-check fallback → legacy-only (quorum cert, no doc); backup-set shrink loop keeps both sigs valid (extend the wire-cap test with verification of the final payload).
4. `event_loop`: router threads projection; revoked sample dropped end-to-end.

## 4. Out of scope / follow-ups

- Verifier-side use of the vine `FeedAuthorityCache` as a second binding source (buddies don't reliably subscribe to vine topics).
- Hosting-ledger attribution changes — the ledger consumes post-admission records; no change.
- ZEB-700-style rate limiting of storage ingest (separate ticket family).
