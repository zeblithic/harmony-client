# ZEB-279 Sub-D Phase 2 — library auto-discovery via announce topic

**Status:** Design (vertical slice, ready for implementation plan)
**Ticket:** [ZEB-279](https://linear.app/zeblith/issue/ZEB-279/zeb-218-sub-d-phase-2-library-auto-discovery-via-announce-topic) (parent: [ZEB-218](https://linear.app/zeblith/issue/ZEB-218/))
**Predecessor:** Phase 1 ([`2026-05-11-zeb-218-sub-d-library-directory-vertical-slice-design.md`](./2026-05-11-zeb-218-sub-d-library-directory-vertical-slice-design.md), PR [#108](https://github.com/zeblithic/harmony-client/pull/108))
**Author:** Codified by Claude during 2026-05-11 brainstorm
**Branch:** `zeb-279-sub-d-phase-2-library-auto-discovery`

---

## 1. Goal

Add a single auto-discovery primitive to Sub-D: **libraries self-announce on `harmony/discovery/library/announce`**, clients subscribe, the existing `LibraryDirectoryBrowser` gains an inline "Discovered libraries" section so users can enroll a library without out-of-band knowledge of its address.

The user must still **explicitly add** each discovered library — auto-add is incompatible with Phase 1's paste-an-address-only trust model and would let any Zenoh participant inject libraries into a user's subscription set.

---

## 2. Why this shape (vertical slice)

Phase 2 as scoped in the parent ZEB-218 design has several latent axes — auto-discovery topic, UI panel, persistent dismiss-list, TTL/freshness, anti-spam quotas. We ship the **core primitive only** this round, mirroring the Phase 1 vertical-slice playbook:

- **Subscribe + ingest + verify + dedupe** in `library_directory.rs` (extends, not creates).
- **One new IPC** (`list_discovered_libraries`) + reuse Phase 1's `add_library` and `library-directory-updated` event.
- **One UI section** added to `LibraryDirectoryBrowser.svelte`.

Deferred to Phase 2.1 follow-ups (see §12): persistent dismiss-list, TTL, per-source-identity anti-spam quotas, strong-consistency CRDT replication of the discovered set.

---

## 3. Architecture overview

```text
  Zenoh topic                              consumer (each device)
  ───────────                              ────────────────────────
  harmony/discovery/library/announce ───► single subscriber (event_loop)
                                              │
                                              ▼
                                     LibraryDirectory::process_announce(bytes)
                                              │
                                  ┌───────────┴───────────┐
                                  ▼                       ▼
                            verify Ed25519 sig    insert into Announces map
                            (sig field zeroed)    (latest-listed_at-wins)
                                              │
                                              ▼
                                     emit `library-directory-updated`
                                              │
                                              ▼
                                  frontend refetches list_discovered_libraries
                                  (filtered: omit already-added)
```

The existing `LibraryDirectory` struct gains a new field `announces: Mutex<Announces>` alongside `aggregation: Mutex<Aggregation>`. Both inhabit the same struct because both serve the same UI surface (the LibraryDirectoryBrowser).

The subscription is **fixed-key, lifetime = app lifetime**, unlike Phase 1's per-library subscriptions that come and go with `add_library`/`remove_library`.

---

## 4. Data model

### 4.1 Wire format — `LibraryAnnounce`

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryAnnounce {
    /// 64-byte identity bundle (X25519_pub(32) || Ed25519_pub(32)).
    /// `OwnerAddr` derives from this via `Identity::from_public_bytes`.
    #[serde(
        rename = "ai",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub library_identity_pub: [u8; 64],

    #[serde(rename = "nm")]
    pub name: String,        // ≤ MAX_NAME_LEN (200) — reuse Phase 1 const

    #[serde(rename = "ds")]
    pub description: String, // ≤ MAX_DESCRIPTION_LEN (2000) — reuse Phase 1 const

    #[serde(rename = "la")]
    pub listed_at: Hlc,

    /// Ed25519 sig over canonical CBOR with `ls` field zeroed.
    /// Signer = library's Ed25519 private key (the second 32 bytes
    /// of the identity bundle).
    #[serde(
        rename = "ls",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub library_signature: [u8; 64],
}
```

**2-char field keys** (`ai`, `nm`, `ds`, `la`, `ls`) match Phase 1 precedent and satisfy `canonical_cbor_encode`'s same-length-keys invariant.

**No `library_addr` field on the wire** — it derives from `library_identity_pub`. Saves 16 bytes per record and removes a possible inconsistency (an entry whose `library_addr` disagrees with `hash(library_identity_pub)`).

**Signature scope**: `canonical_cbor_encode` of the entire `LibraryAnnounce` with `library_signature` zeroed. Same shape as Phase 1's `LibraryDirectoryEntry::community_signature`.

### 4.2 In-memory discovered set — `Announces`

```rust
pub(super) struct Announces {
    /// Discovered libraries, keyed by `library_addr` (derived from
    /// `library_identity_pub` at verify time). Latest-`listed_at`-wins
    /// on dedupe; HLC tie-break on `(wall_ms, logical, device_id)`.
    by_addr: BTreeMap<OwnerAddr, LibraryAnnounce>,
}

const MAX_DISCOVERED_LIBRARIES: usize = 1_000;
```

**Not persisted.** Rebuilt from subscription on every startup. The acceptance criterion's "replicate across bound devices" is satisfied via loose replication: every device subscribes the same global topic, so they converge as fresh announces arrive. Strong-consistency CRDT replication is deferred to §12 row 4.

**Cap = 1,000.** Much smaller than Phase 1's per-library 10,000 cap. This is the **global** count of known libraries (not per-library entries); 1,000 is generous and bounds memory at a few hundred KB.

**Overflow eviction**: on insert when at-cap, drop the oldest-by-`listed_at` entry. Stable tie-break by `OwnerAddr` byte order if two have identical HLCs.

---

## 5. Subscription lifecycle

### 5.1 Subscribe path (event_loop.rs)

A single subscription wired next to the existing `harmony/announce/*` content subscribe at `event_loop.rs:787`:

```rust
dispatch_action(
    RuntimeAction::Subscribe {
        key_expr: "harmony/discovery/library/announce".to_string(),
    },
    &session, &zenoh_tx, &udp, &broadcast_addr, &app, &closing, &own_zid,
)
.await;
```

No add/remove plumbing — the subscription is **fixed for the app lifetime**.

Sample router (extends the existing `harmony/discovery/library/*/communities` routing in `event_loop.rs::on_sample` or equivalent dispatch):

```rust
if key_expr == "harmony/discovery/library/announce" {
    if let Some(dir) = library_directory.as_ref() {
        let result = dir.process_announce(payload);
        if matches!(result.outcome, AnnounceOutcome::Inserted | AnnounceOutcome::Updated) {
            emit_library_directory_updated(&app);
        }
    }
}
```

### 5.2 Receive path

`LibraryDirectory::process_announce(bytes: &[u8]) -> AnnounceProcessResult`:

1. Deserialize CBOR → `LibraryAnnounce`. Decode error → `Dropped(DecodeFailed)`.
2. Bounds check (`name` ≤ 200 bytes, `description` ≤ 2000 bytes). Out-of-bounds → `Dropped(NameTooLong | DescriptionTooLong)`.
3. Parse `library_identity_pub` via `Identity::from_public_bytes`. Fail → `Dropped(InvalidIdentityPub)`.
4. Derive `library_addr: OwnerAddr` from parsed identity.
5. Canonical-CBOR encode with `library_signature` zeroed → bytes to verify.
6. Ed25519 sig verify (`verifying_key.verify_strict`). Fail → `Dropped(SignatureInvalid)`.
7. Acquire `announces` mutex. If existing entry for `library_addr` has newer `listed_at`, drop incoming. Otherwise insert; on at-cap insert, evict oldest-by-`listed_at`.
8. Return `AnnounceOutcome::Inserted` or `AnnounceOutcome::Updated` (the outer dispatch emits the frontend event).

---

## 6. IPC surface

### 6.1 `list_discovered_libraries`

```rust
#[derive(Serialize)]
pub struct DiscoveredLibraryInfo {
    /// Hex-encoded `library_addr` (32 chars, 16 bytes).
    pub library_addr: String,
    pub name: String,
    pub description: String,
    /// Base-10 string of `listed_at.wall_ms` for UI display only.
    /// HLC ordering decisions MUST NOT use this value — the wall_ms
    /// projection drops the logical+device_id tie-break fields. The
    /// frontend formats this for human-readable timestamps.
    pub listed_at: String,
}

#[tauri::command]
async fn list_discovered_libraries(state: State<'_, NodeState>)
    -> Result<Vec<DiscoveredLibraryInfo>, String>;
```

**Filter applied at IPC layer**: omit any `library_addr` that appears in `OwnerState.libraries` as a non-tombstoned entry. The discovered panel only shows libraries the user has **not yet added**.

Sort: newest `listed_at` first (helps users see fresh announces at the top of the panel).

### 6.2 Refetch event

**Reuse** the existing `library-directory-updated` event. Frontend's existing debounced refetch handler will call `listDiscoveredLibraries()` alongside `listLibraries()` and `browseLibrary()`. No new event type.

---

## 7. Frontend

### 7.1 `LibraryDirectoryBrowser.svelte` changes

A new section between "Your libraries" and the bottom add-manual affordance:

```text
+---- Library Directory ---------------+
| Your libraries (2)                   |
|  [chip] LibX (abcd…)  [Remove]       |
|  [chip] LibY (1234…)  [Remove]       |
|                                      |
| > Discovered libraries (3)           |
|   LibZ (8888…) — Pop culture   [Add] |
|   LibQ (9999…) — Math wiki     [Add] |
|   LibR (aaaa…) — Indie games   [Add] |
|                                      |
| [+ Add library manually]             |
+--------------------------------------+
```

- Section header is a click-to-toggle collapsible (`<details>` element or equivalent Svelte 5 idiom).
- **Collapsed by default when N=0.** Auto-expanded when N>0.
- Each row: name (bold), description (subdued single-line truncated to 60 chars), short addr (`abcd…`, 8 chars + ellipsis), `Add` button.
- Click `Add` → calls existing `addLibrary(libraryAddrHex)` → success closes the row (refetch removes it from discovered set via the filter). Error surfaces inline next to the row (mirrors Phase 1 `removeError` placement).

### 7.2 `library-directory-service.ts`

Add one wrapper:

```typescript
export interface DiscoveredLibraryInfo {
  libraryAddr: string;
  name: string;
  description: string;
  listedAt: string;
}

export async function listDiscoveredLibraries(): Promise<DiscoveredLibraryInfo[]> {
  return await invoke('list_discovered_libraries');
}
```

### 7.3 Event handling

Existing `library-directory-updated` listener in `LibraryDirectoryBrowser.svelte` is extended to refetch the discovered list alongside the existing `listLibraries()`. Debounce reused (no new timer).

---

## 8. Add-from-discovered flow

No new IPC. The "Add" button calls existing Phase 1 `addLibrary(libraryAddrHex)`:

1. Phase 1's `add_library` validates hex, mutates `OwnerState.libraries`, spawns the per-library `harmony/discovery/library/{addr}/communities` subscription (Phase 1 path).
2. Emits `library-directory-updated`.
3. Frontend refetches both lists; the IPC filter at §6.1 now excludes this library from `list_discovered_libraries`, so it visibly moves from "Discovered" to "Your libraries".

---

## 9. Error handling

```rust
#[derive(Debug, thiserror::Error)]
pub enum AnnounceVerifyError {
    #[error("CBOR decode failed: {0}")]
    DecodeFailed(String),
    #[error("malformed identity_pub: {0}")]
    InvalidIdentityPub(String),
    #[error("canonical CBOR encode failed: {0}")]
    Encode(#[from] crate::owner_state_crypto::CryptoError),
    #[error("Ed25519 signature verification failed")]
    SignatureInvalid,
    #[error("name exceeds MAX_NAME_LEN bytes")]
    NameTooLong,
    #[error("description exceeds MAX_DESCRIPTION_LEN bytes")]
    DescriptionTooLong,
}
```

Verify fail → warn-log + silent drop. Same shape as Phase 1's `EntryVerifyError`. No frontend surfacing — verify failures are protocol-level and not user-actionable.

---

## 10. Performance / scale

- **Hot path cost**: one Ed25519 verify per announce sample. At an arrival rate of 1/sec (plausible upper bound for a young protocol), this is ~50 µs of CPU — negligible.
- **Memory bound**: 1,000 entries × ~2.4 KB worst-case (200B name + 2000B desc + crypto fields + overhead) ≈ **2.4 MB** worst-case. Realistic: ~few hundred KB.
- **Subscription cost**: one Zenoh subscription, fixed. No fan-out concern at vertical-slice scope.
- **Engineer-for-real-scale guidance**: at hypothetical 1M libraries, this design degrades (every device subscribes the firehose). Future work (Phase 2.1+): regional sharding, Reticulum routing, per-source-identity rate-limiting. Not in scope this round.

---

## 11. Testing

### 11.1 Test fixture (Cargo feature `test-fixtures`)

Extend `src-tauri/tests/common/library_fixtures.rs` with a `mock_library_announce(name, description)` builder that:
- Generates an Ed25519 keypair deterministically (test seed).
- Computes `library_identity_pub` (32-byte X25519 pub + 32-byte Ed25519 pub).
- Builds `LibraryAnnounce` with the supplied name/description and a test HLC.
- Signs canonical CBOR with sig zeroed.
- Returns the serialized bytes + the derived `library_addr`.

Mirrors the existing `mock_library_entry` pattern from Phase 1.

### 11.2 Integration tests — `tests/library_announce_integration.rs` (new)

Named tests:

| Test name | What it covers |
|---|---|
| `subscribe_ingests_and_appears_in_list` | Publish via mock fixture → `process_announce` → `list_discovered_libraries` returns the entry |
| `dedupe_by_library_addr_latest_listed_at_wins` | Same `library_addr` published twice with different `listed_at`; newer wins |
| `dedupe_older_listed_at_dropped` | Same addr; older sample arrives second; silently dropped |
| `invalid_sig_rejected` | Tampered payload bytes → verify fails → not inserted |
| `invalid_identity_pub_rejected` | All-`0x7F` identity_pub → parse fails → not inserted |
| `name_too_long_rejected` | Name = 201 bytes → bounds check → not inserted |
| `already_added_library_filtered_out` | Add via Phase 1 `add_library` → discovered IPC no longer returns this entry |
| `cap_eviction_drops_oldest` | Publish 1,001 distinct announces → oldest-by-`listed_at` is evicted |

All tests drive `LibraryDirectory::process_announce` directly (precedent: Phase 1's `library_directory_integration.rs`).

### 11.3 Unit tests — `library_directory.rs::tests` (extension)

- `library_announce_canonical_cbor_roundtrip` — encode→decode round-trip preserves all fields including signed bytes.
- `library_announce_sig_field_must_be_zeroed_in_canonical_form` — explicit assertion that the sig-zeroed encoding differs from the with-sig encoding only at the `ls` field bytes.

### 11.4 Wire-format pinning — `tests/wire_format_library_announce_fixtures.rs` (new)

Pin exact canonical-CBOR hex bytes for one canonical `LibraryAnnounce` (deterministic-keys fixture). One test enumerates field keys via `ciborium::Value::Map` decode + `BTreeSet` exact equality (mirrors Phase 1 R4 F1 hardening: prevents accidental key-rename/key-add slip-through):

```rust
#[test]
fn field_keys_are_2char() {
    let bytes = canonical_test_announce_bytes();
    let value: ciborium::Value = ciborium::from_reader(&bytes[..]).unwrap();
    let map = value.as_map().expect("must decode as map");
    let keys: BTreeSet<&str> = map.iter()
        .filter_map(|(k, _)| k.as_text())
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from(["ai", "nm", "ds", "la", "ls"]),
    );
}
```

### 11.5 Frontend vitest — `src/lib/components/__tests__/LibraryDirectoryBrowser.test.ts` (extend)

New cases added to the existing test file:

- `discovered_panel_renders_with_n_entries` — mock IPC returns 3 entries; section header reads "Discovered libraries (3)"; rows render with name + description + addr.
- `discovered_section_collapsed_when_empty` — IPC returns []; section header reads "Discovered libraries (0)"; rows not visible.
- `click_add_invokes_addLibrary_with_correct_addr` — click `Add` on a row → spy on service `addLibrary` confirms it was called with the row's `libraryAddr`.
- `add_failure_surfaces_inline` — service throws; row shows inline error text.

---

## 12. Deferred follow-ups

**NOT filed as new Linear tickets this round** — surfaced here for spec traceability. Each is a candidate for a Phase 2.1 follow-up ticket if user demand emerges:

| # | Topic | Notes |
|---|---|---|
| 1 | Persistent dismiss-list | LWW CRDT entry per dismissed-library; replicated through Phase 1 owner-state sync; "X" button on each discovered row. UX motivator: without it, dismissed libraries reappear after restart. |
| 2 | TTL / re-announce-or-evict | Currently announces stay forever (capped at 1,000). With TTL, libraries that stop re-announcing fall off; live libraries stay visible. Requires choosing a re-announce cadence and TTL window. |
| 3 | Per-source-identity anti-spam quotas | Currently any signed record passes per-record bounds. Pathological case: an attacker controls 1,000 OwnerAddrs and publishes from all of them, evicting legitimate libraries. Mitigation: per-identity max-rate, OR allow-list of trusted libraries. |
| 4 | Strong CRDT replication of discovered set | Currently each device independently re-derives from the subscription firehose. If device A is online when announce X arrives but device B is offline past Zenoh's retention window, B may never see X. CRDT replication of the discovered set would close this gap. |

If the user wants any of these surfaced as proper Linear tickets, that's a one-line Linear filing.

---

## 13. Out of scope this round (explicit non-goals)

- **Library hosting itself.** Libraries are a separate role; the publishing-side `harmony-library` codebase (if it exists) is not in this client repo.
- **Curated default libraries pre-populated at install.** Already rejected in Phase 1 brainstorm as anti-polycentric.
- **Reputation / ranking of discovered libraries.** No global moderation, no platform admin.
- **Phase 3 federated republication** (library wrapping signature) — separate ticket [ZEB-280](https://linear.app/zeblith/issue/ZEB-280/).
- **Phase 4 ProfileMembershipBroadcast** — separate ticket [ZEB-281](https://linear.app/zeblith/issue/ZEB-281/).
- **Phase 6 direct-join IPC** — separate ticket [ZEB-252](https://linear.app/zeblith/issue/ZEB-252/).

---

## 14. Acceptance criteria (from ZEB-279)

| Criterion | Satisfied by |
|---|---|
| Auto-discovery topic subscribed at startup | §5.1 — single fixed-key subscribe in `event_loop.rs` |
| Announce records surface in UI | §6.1 + §7.1 — `list_discovered_libraries` IPC + inline panel |
| User must explicitly confirm adding | §7.1 + §8 — `Add` button calls existing `addLibrary`; no auto-add path |
| Discovered libraries replicate across bound devices | §3 + §4.2 — loose replication: every device subscribes the same global topic. Strong CRDT replication deferred (§12 row 4). |

---

## 15. References

- Phase 1 spec: [`2026-05-11-zeb-218-sub-d-library-directory-vertical-slice-design.md`](./2026-05-11-zeb-218-sub-d-library-directory-vertical-slice-design.md)
- Phase 1 PR: [#108](https://github.com/zeblithic/harmony-client/pull/108)
- Parent ticket: [ZEB-218](https://linear.app/zeblith/issue/ZEB-218/) (Sub-D library-federated discovery directory + browse UI)
- This ticket: [ZEB-279](https://linear.app/zeblith/issue/ZEB-279/zeb-218-sub-d-phase-2-library-auto-discovery-via-announce-topic)
- Sibling phases: [ZEB-280](https://linear.app/zeblith/issue/ZEB-280/) (Phase 3), [ZEB-281](https://linear.app/zeblith/issue/ZEB-281/) (Phase 4), [ZEB-252](https://linear.app/zeblith/issue/ZEB-252/) (Phase 6)
- Existing announce-topic precedent in code: `event_loop.rs:787` (`harmony/announce/*` for content-CID announces)
- Existing Phase 1 `LibraryDirectoryEntry`: `src-tauri/src/library_directory.rs:31-67`
- Existing wildcard sample router: `event_loop.rs:2457` (`harmony/announce/` prefix match)
