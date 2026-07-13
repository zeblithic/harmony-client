# ZEB-678 — Revocation-aware feeds: vine + follow-list signing migration #3 → #2

Status: draft for Jake's review. Design forks were settled interactively
(2026-07-12): feed identity = preserve node-addr feed-id (follow graph
unchanged); publisher tracking = living authority record (LWW snapshot,
remove-wins revocation); feed cardinality = per-device feed, independently
revocable; reactions = #2-signed + self-carried EnrollmentCert with
best-effort revocation. All four confirmed by Jake before this spec.

ZEB-668 §9 follow-up 2 (parent spec:
`docs/specs/2026-07-11-zeb-668-device-management-design.md`). Same family as
the DM-packet migration (ZEB-580) and the storage-buddy migration (ZEB-679).

## §0 Ground truth (as-implemented, main @ eb5fdc3b)

The pre-design survey (four parallel code explorers, 2026-07-12) established:

* **Every vine record is signed with the #3 per-device node key.** The signer
  is `NodeState.owner_private_identity: Option<Arc<harmony_identity::PrivateIdentity>>`
  (`lib.rs:795`) — misleadingly named, but it is the device's #3 node
  identity (loaded from the node-identity file at `lib.rs:3676`;
  `node_addr = hex(address_hash)`). ZEB-668 §0 confirms #3 is per-device and
  "never crosses the pairing wire."
* **Feed identity is self-certifying and per-device.** `vine_signing.rs`
  `verify_signed` (`:156`) hex-decodes the record's own `identity_pub`, builds
  `harmony_identity::Identity::from_public_bytes`, requires
  `hex(address_hash) == claimed_address`, then `verify_strict`. The
  `creator_address` *is* `hash(#3 pubkey)`, and that address *is* the Zenoh
  topic `harmony/vines/{creator_address}`. There is no owner concept in the
  feed at all.
* **No binding exists from a vine's #3 identity to an `owner_id` or #2
  `device_id`.** They are different keyspaces (#3 = `harmony_identity`
  address; #2 = `EnrollmentCert.device_id`, the 16-byte identity-hash of a
  `PubKeyBundle`; the enrolled ed25519 = 32-byte
  `device_pubkeys.classical.ed25519_verify`). No cert, CRDT, or code path
  binds them.
* **The trust CRDT is same-owner-only.** `owner-trust-v1`
  (`harmony_owner::state::OwnerState`, `NodeState.owner_trust_doc` at
  `lib.rs:1378`) replicates only among one owner's own devices; a foreign
  owner's snapshot merges to zero change (pinned by
  `owner_trust_sync.rs::merge_drops_record_for_foreign_owner_without_degrading`).
  The only cross-owner revocation channel that exists is the ZEB-668 S3
  community retire-announce, which is community-membership-scoped and reaches
  only shared-community members. **There is no cross-owner channel a feed
  verifier can reach**, so ZEB-678 is greenfield here.
* **The #2 reference pattern is solid.** `community_membership::sign_event(payload, &SigningKey)`
  (`:630`) signs with the enrolled device key
  (`loaded.device_signing_key`, keychain `device_signing_key` /
  `owner_state.cbor`, wrapped at `lib.rs:4593`); the device's own cert is
  `loaded.state.enrollments.get(&this_device_id_hash)` (`lib.rs:~4600`).
  Verification funnels through the `enrollment_verify` chokepoint:
  `verify_enrollment_any_issuer(cert, signer_certs, expected_owner: Option<&[u8;16]>, now_secs) -> VerifiedEnrollment { device_ed25519, master_ed25519 }`
  (`enrollment_verify.rs:66`) — signature + Master/Quorum issuance + expiry,
  **not** revocation; `verify_revocation_any_issuer(cert, target_enrollment, signer_certs, now_secs)`
  (`:131`). Revocation is enforced *outside* the chokepoint by converging on a
  per-scope tombstone (community `DeviceRetire` →
  `MemberState.revoked_device_keys`, remove-wins).
* **Vines are JSON, not CBOR.** All records `serde_json::to_vec` over Zenoh;
  no canonical-byte fixtures. Signatures are **wire-only** — never persisted
  (`VineFeedDiskV1` / `FollowListOnDisk` omit sig fields; verify-once-at-ingest).
  `VineFeedDiskV1.FILE_VERSION = 1` (`vine_feed_cache.rs:49`); `tombstones`
  and `follow_lists` were added to that v1 envelope as `#[serde(default)]`
  **without** a version bump — the exact additive precedent this migration
  reuses.
* **Signer-authority guards already exist** at every sign site (the embedded
  address must equal `signer_address(identity)` — `lib.rs:14062`, `14363`,
  `15142`).

Honesty rule (ZEB-610 §0) governs everything: render only what the backend
can prove; UI copy states exactly what an action does and does not do.

### §0.1 Current record + topic inventory (the migration surface)

| Record | Wire struct | Domain | Sign site | Ingest verify | Topic |
|---|---|---|---|---|---|
| Descriptor | `VineDescriptorPayload` (`lib.rs:13882`) | `harmony-vine-descriptor-v1` (`vine_signing.rs:39`) | `sign_descriptor` (`:129`), caller `lib.rs:14069` | `vine_feed_cache.rs:562` | `harmony/vines/{N}` |
| Reaction | `VineReactionPayload` (`lib.rs:13976`) | `harmony-vine-reaction-v1` (`:41`) | `sign_reaction` (`:136`), caller `lib.rs:14379` | `vine_feed_cache.rs:756` | `harmony/vines/{C}/reactions/{vine_id}/{R}` |
| Follow list | `VineFollowListPayload` (`lib.rs:14013`) | `harmony-vine-follows-v1` (`:43`) | `sign_follow_list` (`:143`), caller `lib.rs:15180` | `vine_feed_cache.rs:976` | `harmony/vines/{N}/follows` |
| Tombstone | `VineTombstonePayload` (`vine_tombstone.rs:23`) | `harmony-vine-tombstone-v1` (`:20`) | `sign_tombstone` (`:62`), caller `lib.rs:14483` | `vine_feed_cache.rs:842` | `harmony/vines/{N}/tombstones/{vine_id}` |

Throughout this spec, **`N`** = a feed-id = a device's #3 node address (hex) =
the existing feed topic; **`O`** = the owner's 16-byte `owner_id`; **`D`** =
the publishing device's 16-byte `device_id`; **`K`** = D's enrolled #2
ed25519 key (32 bytes).

## §1 Scope (locked) and non-goals

**In scope:** owner-anchor each existing per-device feed without changing the
topic or the follow graph; migrate descriptor / follow-list / tombstone
signing from #3 to #2; migrate reactions to #2 with a self-carried
EnrollmentCert; a per-feed `FeedAuthorityRecord` that binds `N → O → D → K`
and carries revocation; a `revoke_device` hook that cuts off a revoked
device's feed (self- and master-issued); retire the §8 honesty-ledger feed
row.

**Non-goals (v1):** unifying an owner's multiple devices into one shared feed
(feed continuity across device replacement — a legitimate future feature that
can build on this spec's owner-binding); re-keying the follow graph or the
ZEB-671 Discover transitive graph to owner addresses; any change to Zenoh
feed topics or to how follows are stored; cert expiry (certs are
`expires_at: None` today — out of scope, unchanged); the DM (#3) migration
(ZEB-580) and storage-buddy (#3) migration (ZEB-679).

## §2 Feed identity model (the core)

Each existing per-device feed becomes **owner-anchored in place**:

* The feed topic stays `harmony/vines/{N}` and the follow graph is untouched
  — `N` remains the stable feed-id. Because records become **#2-signed**, the
  #3 node key is needed exactly **once**: to sign the `N → O/D/K` binding
  inside the feed's authority record. After that, all content records on `N`
  are signed by `K` and verified against the cached authority.
* A **`FeedAuthorityRecord`** (§3) is the single source of truth per feed: it
  binds `N` to owner `O`, device `D`, and publisher key `K`, and carries an
  optional `RevocationCert`. Followers cache the latest by LWW; revocation is
  remove-wins sticky.
* **Migration marker (per feed):** a follower that has cached an authority
  record for `N` treats `N` as migrated → requires a valid #2 `device_sig` on
  content records and rejects #3-only records. A follower with no cached
  authority for `N` verifies content the legacy self-certifying #3 way,
  unchanged. This is what prevents a revoked device from bypassing the scheme
  by continuing to #3-sign: once the feed is migrated, #3 records are rejected.

Per-device consequence (the §1 non-goal, stated honestly): an owner with two
devices D1 (feed N1) and D2 (feed N2) has two independent feeds. Revoking D2
cuts off N2 (followers of N2 reject D2's post-revocation records); N1 is
unaffected. A device's feed does not survive replacement — a new device is a
new feed-id. Unifying feeds is a future follow-up.

## §3 Data structures & wire (JSON, additive — no file-version bump)

### §3.1 New: `FeedAuthorityRecord`

New JSON record on new sub-topic `harmony/vines/{N}/authority`
(`#[serde(rename_all = "camelCase")]`):

```rust
struct FeedAuthorityRecord {
    feed_id: String,          // N (hex node address) — also the topic segment
    owner_id: String,         // hex 16-byte harmony-owner owner_id (O)
    device_id: String,        // hex 16-byte EnrollmentCert.device_id (D)
    publisher_key: String,    // hex 32-byte enrolled #2 ed25519 (K)
    n_identity_pub: String,   // hex 64-byte #3 pubkey whose hash == feed_id
    enrollment: EnrollmentCert,             // proves K ↔ D ↔ O
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    signer_certs: Vec<EnrollmentCert>,      // quorum bundle (empty ⇒ master-issued)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revocation: Option<RevocationCert>,     // present ⇒ D revoked (self- or master-signed)
    updated_at: u64,          // LWW clock (HLC wall_ms)
    n_sig: String,            // hex 64-byte #3 signature over the binding bytes
}
```

* **Binding bytes** signed by `n_sig` (domain-separated, length-prefixed like
  the existing `vine_signing` builders): `"harmony-vine-authority-v1"` ‖
  `feed_id` ‖ `owner_id` ‖ `device_id` ‖ `publisher_key`. Only the holder of
  N's #3 key can produce `n_sig`; that key hashes to `feed_id`, closing the
  loop.
* The active binding's authenticity rests on `n_sig` + `enrollment`. The
  revoked state's authenticity rests on the embedded `RevocationCert`
  (self-proving), so any device or forwarding follower can carry a revoked
  authority.

### §3.2 Changed: steady-state content records (authority-backed)

`VineDescriptorPayload`, `VineFollowListPayload`, `VineTombstonePayload` each
gain one additive optional field:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub device_sig: Option<String>,   // hex 64-byte K signature over the -v2 canonical bytes
```

No per-record cert — the follower already holds `K` from the feed's authority
record. The legacy `identity_pub`/`sig` (#3) fields remain for backward
verify. (`VineTombstonePayload`'s existing sig fields are required `String`;
`device_sig` is added as the additive optional and a tombstone is "migrated"
when `device_sig` is present.)

### §3.3 Changed: reactions (self-introducing — cross-actor)

Reactions are signed by the reactor R but delivered under the creator C's
topic to C's followers, who usually do **not** hold R's authority record. So a
reaction carries its own owner-anchoring proof and is verified standalone:

```rust
// added to VineReactionPayload, all additive optional:
pub owner_id: Option<String>,               // R's owner_id
pub enrollment: Option<EnrollmentCert>,     // proves the reactor's #2 key ↔ owner
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub signer_certs: Vec<EnrollmentCert>,      // quorum bundle
pub device_sig: Option<String>,             // hex 64-byte #2 sig over -v2 reaction bytes
```

Revocation for reactions is **best-effort**: rejected only when the follower
already holds R's authority record (`feed_id == reactor_address`) marking R's
device revoked; otherwise accepted on valid enrollment. (Recorded in §8.)

### §3.4 Signed-bytes domain: `-v2` for the #2 variant

The #2 signature covers the **same canonical field set** as today under a
bumped domain constant (`harmony-vine-descriptor-v2`, `-reaction-v2`,
`-follows-v2`, `-tombstone-v2`), giving clean protocol separation between
"#3-signed" and "#2-signed" bytes (defense-in-depth beyond the distinct
`device_sig` field). `feed_id`/`creator_address` is already inside those bytes,
so a `device_sig` cannot be replayed onto another feed.

### §3.5 Active-binding registry for master-revoke (fleet-net, additive)

Because #3 never crosses the pairing wire, a seed-holder can neither learn a
sibling's feed-id `N` nor produce its `n_sig` (only N's #3 key can). So a
seed-holder cannot synthesize a revoked authority record from scratch — it can
only *re-publish an existing active binding with a `RevocationCert` appended*.
To make that possible, when a device migrates its feed (§5) it **self-stamps
its full active binding** (the `FeedAuthorityRecord` sans `revocation`) into
its fleet-net row: `FleetNetRow` gains an additive optional
`feed_binding: Option<FeedAuthorityRecord>` (`#[serde(default)]`,
self-authored, riding the existing ZEB-668 S4 per-row LWW). On master-revoke
the seed-holder reads the target row's `feed_binding`, appends the master
`RevocationCert`, bumps `updated_at`, and re-publishes the complete record to
`harmony/vines/{N}/authority` (§6) — so even a brand-new follower fetches one
complete, fully-verifiable record. A device that never migrated a feed has no
`feed_binding` and nothing to cut (honest residual, §8).

## §4 Verification & migration marker

**Authority record, on receipt** (once per LWW update):
1. `hex(hash(n_identity_pub)) == feed_id`, then `n_sig` verifies the binding
   bytes under `n_identity_pub`.
2. `verify_enrollment_any_issuer(enrollment, signer_certs, Some(owner_id_bytes), now_secs)`
   → `VerifiedEnrollment { device_ed25519, .. }`; require
   `device_ed25519 == publisher_key` and `enrollment.device_id == device_id`.
   `now_secs` is **verifier-controlled** — the ingest boundary supplies the
   real wall clock. It is deliberately *not* derived from `updated_at`, which
   is excluded from `n_sig` (unauthenticated); deriving the expiry clock from
   it would let a peer backdate `updated_at` to revive an expired/backdated
   enrollment. The revocation check below keeps `revocation.issued_at`, which
   *is* authenticated inside the `RevocationCert`.
3. If `revocation` present:
   `verify_revocation_any_issuer(revocation, target_enrollment = enrollment, signer_certs, revocation.issued_at)`
   and require `revocation.target == device_id`. (Issued-at-time semantics so a
   retire works even for an already-expired cert, mirroring
   `verify_device_retire_certs`.)
4. Cache for `feed_id` with two different disciplines:
   * **Active binding** (`device_id`, `publisher_key`, `n_identity_pub`) is
     **first-write-wins**: pinned on first valid sight and never re-bound.
     This blocks a re-binding evasion — a compromised device (which *does*
     hold N's #3) cannot re-point its feed to a different `device_id`/key to
     dodge a revocation aimed at its own device. Later records that disagree
     with the pinned binding are dropped.
   * **`revoked`** is **sticky / monotonic-true**: set by *any* record
     carrying a valid `RevocationCert` whose `target` equals the pinned
     `device_id`, and never cleared — independent of `updated_at`, so a newer
     `revoked:false` record can never un-revoke. `updated_at` LWW governs only
     benign refreshes that agree on the pinned binding.

**Content record for feed `N`, at ingest:**
* **authority cached for `N`** → require a valid `device_sig` against
  `authority.publisher_key` **and** `!authority.revoked`; reject #3-only
  records.
* **no authority cached** → verify the legacy self-certifying #3 way
  (unchanged).

**Reaction, at ingest:** if `device_sig` + `enrollment` present → verify
enrollment via the chokepoint against `owner_id`, then `device_sig` against
the recovered `device_ed25519`; best-effort revocation per §3.3. Else legacy
#3.

**Failure handling:** an unverifiable authority record is dropped with a warn
(never trust-degrading, mirroring the trust-merge posture). A content record
that fails its #2 check on a migrated feed is rejected at ingest exactly as an
unsigned record is today.

**Ordering / bootstrap:** a content record arriving before the feed's
authority record is treated as "no authority cached" and takes the legacy
path; once the authority record is cached, subsequent records require #2.
Because a migrating publisher publishes its authority record before (or with)
its first #2-signed content, and followers subscribe to `N`, steady state
converges quickly. Newcomers fetch the single latest authority record.

## §5 Signing migration (S2)

* Add a #2 signer that takes the enrolled device key: extend `vine_signing`
  with `sign_*_v2(signing_key: &ed25519_dalek::SigningKey, ..)` producing
  `device_sig` over the `-v2` bytes, and `verify_*_v2` checking a `device_sig`
  against a supplied `publisher_key`. The production callers
  (`publish_vine_descriptor` `lib.rs:14041`, reaction publish `lib.rs:14342`,
  `build_signed_follow_list_with` `lib.rs:15133`, `delete_vine_impl`
  `lib.rs:14430`) switch to the #2 key (`community_signing_key`) and stop
  #3-signing new records.
* On a device's **first migrated publish**, publish (and thereafter maintain)
  its `FeedAuthorityRecord` for its own feed `N` (active binding, no
  revocation), and self-stamp `feed_id = N` into its fleet-net row (§3.5).
* Reactions additionally attach `owner_id` + `enrollment` (+ `signer_certs`)
  per §3.3.
* Dual-path verifier + per-feed migration marker per §4.

## §6 Revocation flow (S3 — hooks the existing S2 `revoke_device`)

`revoke_device` (`owner_commands.rs`) today: sign `RevocationCert` →
`add_revocation` → persist → `notify_dirty` (trust engine) → emit
`owner-devices-updated` → hand to S4 retire-announce. ZEB-678 adds a feed
cut-off step:

* **Self-revoke** (D removes itself): D signs a final `FeedAuthorityRecord`
  for its own `N` with `revocation` = its self-signed `RevocationCert`, and
  **publishes + flushes it before** entering the removed terminal state
  (mirrors ZEB-668 S2 self-revoke ordering — otherwise no follower learns).
* **Master-revoke** (seed-holder removes D, incl. the lost/compromised case):
  the seed-holder reads D's `feed_binding` from D's fleet-net row (§3.5) —
  the complete active binding with its `n_sig` — appends the master-signed
  `RevocationCert` for `D`, bumps `updated_at`, and publishes the complete
  `FeedAuthorityRecord` to `harmony/vines/{N}/authority`. Proof is the
  embedded `RevocationCert`, so followers honor it regardless of publisher;
  first-write-wins on the binding + sticky `revoked` (§4 step 4) make it
  permanent even if a compromised D fights back with a newer `revoked:false`
  record or a re-binding attempt. If D never migrated a feed (no
  `feed_binding`), there is no feed to cut (§8).

## §7 UI / honesty copy (S3)

The ZEB-668 revoke confirm-dialog copy (`DevicesPanel`, §3 of the parent
spec) currently says "existing direct-message and feed publishing from that
device is not blocked yet." ZEB-678 updates the **feed** half: for a migrated
feed, removal now blocks the device's feed publishing (followers reject its
post-revocation records). The DM half stays (ZEB-580). Copy states the honest
residual: a device that never published a vine has no feed to cut, and
reaction revocation is best-effort (§8).

## §8 Honesty ledger (delta from ZEB-668 §8)

| Claim the UI might imply | Reality after ZEB-678 | Handling |
|---|---|---|
| "Remove device" blocks its feed publishing | YES for a migrated feed (authority record cached ⇒ #2 required ⇒ revoked device's records rejected). A feed that never migrated (device never published a vine) has no authority record and cannot be cut. | Confirm-dialog copy; §2 migration marker |
| …its reactions too | Best-effort — rejected only where the follower already holds the reactor's authority record; else accepted on valid enrollment (cross-actor: the follower usually lacks the reactor's authority) | §3.3; copy notes best-effort |
| The revoked device can't fight back | Correct — `revoked` is sticky/monotonic and honored only when backed by a valid `RevocationCert`; the active binding is first-write-wins, so a compromised device (holding N's #3) can neither un-revoke itself nor re-bind its feed to a different device to dodge the revocation | §4 step 4 |
| A revoked device can't keep #3-publishing | Correct once the feed is migrated (authority cached ⇒ #3 rejected). Before migration, #3 is still accepted (legacy) — but a pre-migration feed has no owner binding to revoke against anyway | §2 marker; alpha/pre-RC posture |

## §9 Compatibility & gates

* JSON tolerates unknown keys; all new fields are
  `#[serde(default, skip_serializing_if = …)]`, so old builds ignore them and
  the default-omitted encoding stays byte-identical. Extends the existing
  `vine_signing.rs` omission tests (`serde_sig_fields_absent_when_none…`
  `:420`, `follow_list_serde_camel_case_pin` `:527`,
  `legacy_json_without_sig_fields_parses` `:558`) with `device_sig`/authority
  analogues. No `FILE_VERSION` bump (the `tombstones`/`follow_lists`
  precedent); signatures are still never persisted.
* Keychain-touching code only via the `*_inner` seams (ZEB-428) — the #2 key
  is already loaded at boot (`community_signing_key`); no new keychain access.
* Gates per PR: `scripts/test-select --context task|round` iteratively (paste
  the `round=…/bucket=…` summary line), `cargo fmt --all -- --check`, clippy
  `--all-targets`, vitest + tsc for the UI slice; full
  `--workspace --all-targets --features test-fixtures` nextest sweep before
  each PR opens.

## §10 Slice / PR map (sequential, one open PR at a time)

* **S1 — Authority record foundation.** `FeedAuthorityRecord` type + binding
  bytes + `n_sig` + chokepoint-backed verify (§4 steps 1-3) + LWW/sticky
  cache (§4 step 4). Data + verify only, no publish wiring. Tests: authority
  round-trip + default-omitted; chokepoint accept/reject (master + quorum +
  expired signer); revoked-sticky / rollback (a newer `revoked:false` cannot
  clear an established revocation); owner-mismatch rejection.
* **S2 — Signing migration + migration marker.** `sign_*_v2`/`verify_*_v2`;
  publish callers switch to #2; publish + maintain the active authority
  record on first migrated publish; self-stamp `feed_id` into fleet-net;
  reactions carry cert; dual-path ingest verifier + per-feed marker. Tests:
  #2 sign/verify round-trip; legacy #3 accepted pre-authority and rejected
  post-authority; reaction self-verify (master + quorum); bootstrap ordering.
* **S3 — Revocation wiring + honesty copy.** `revoke_device` hook publishes
  the revoked authority (self-revoke ordering; master-revoke via fleet-net
  feed-id lookup); followers converge sticky; retire the §8 feed honesty row
  + `DevicesPanel` copy. Tests: self- vs master-revoke authority publish +
  ordering; follower rejects post-revocation content; compromised-device
  fight-back (newer `revoked:false` ignored, re-binding to a different
  `device_id` rejected by first-write-wins); never-migrated device has
  nothing to cut.

## §11 Follow-up tickets (file at implementation end, use assigned IDs)

1. Unify an owner's per-device feeds into one canonical owner feed (multi-device
   publish + feed continuity across device replacement) — builds on this
   spec's owner-binding.
2. Strengthen reaction revocation beyond best-effort (e.g. piggyback the
   reactor's revocation onto the reaction delivery path, or a follower-side
   authority prefetch for observed reactors).
