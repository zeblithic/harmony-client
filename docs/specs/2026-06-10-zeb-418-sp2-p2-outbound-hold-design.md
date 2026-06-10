# SP2 P2 — Outbound hold: fleet-held outbound DMs + fresh butler-set advertisement

**Status:** Design approved in brainstorm 2026-06-10 (this doc is the write-up; pending Jake's spec review).
**Ticket:** [ZEB-418](https://linear.app/zeblith/issue/ZEB-418) P2 · folds in [ZEB-422](https://linear.app/zeblith/issue/ZEB-422) (closes with this PR) · **Epic:** [ZEB-416](https://linear.app/zeblith/issue/ZEB-416)
**Builds on:** SP2 P1 inbound deposit (PR #221, squash `e39a3339`) · SP1 `FleetSyncEngine` ([ZEB-417](https://linear.app/zeblith/issue/ZEB-417)) · P1 spec `docs/specs/2026-06-09-zeb-418-sp2-butler-design.md` (§4 as amended)
**Reuses:** `butler_deposit.rs` rung + client · `reachability_record.rs` butler-set wire format (unchanged — `pin` bit already carried) · `owner_device_cache` · device admin surface ([ZEB-170](https://linear.app/zeblith/issue/ZEB-170))

## 1. Goal

With P1+P2, the headline epic demo works: **two owners who are never online at the same time exchange DMs.**

A1 (Alice's phone) sends a DM while Bob's whole fleet is offline, then A1 goes offline. The `OutboxEntry` (already fleet-replicated in `OwnerState`) and the new outhold blob reached A2 (Alice's desktop) while A1 and A2 overlapped online. A2's drain — which already iterates all pending entries with no originator filter — keeps retrying. When Bob's desktop comes online and publishes a fresh butler-set, A2's deposit rung fires, deposits, and Bob's fleet ingests via P1. Delivered-state merges back to A1 whenever it returns. If Bob instead comes online while A2 is direct-sending, the direct path also completes: Bob's CidNotify fetch-back hits A2's CAS because the held blob is there.

**What P2 is NOT:** no new retry machinery (drain + `state.outbox` already run fleet-wide), no DM-history migration (P1's D6 stands), no wire-format changes anywhere.

## 2. Decisions (settled in this brainstorm; numbering continues P1's table)

| # | Decision | Choice |
|---|---|---|
| D11 | ZEB-422 | **Folded into P2.** Deposit candidacy extends to sent-but-never-acked pairs (§4); the ticket closes with this PR |
| D12 | Blob availability | **`dm-outhold-v1` content side-table** (message_cid → blob), not read-at-use, not on-demand fetch. ALL retry/delivery state stays in `state.outbox` — the side-table carries content only |
| D13 | Consumer paths | `on_applied` inserts the held blob into each device's **local CAS**. Deposit client, CidNotify fetch-back serving, and future P3 consumers work unmodified (content-addressed: same CID ⇒ same key everywhere) |
| D14 | Hold lifetime | GC mirrors the matching `OutboxEntry`'s terminal states (Complete / Expired / user-deleted): dataset row + sibling CAS copy removed. Only undelivered mail is ever held |
| D15 | Fleet net-info | New tiny `fleet-net-v1` dataset: per-device `{iroh endpoint, home relay, seen_at}` rows + owner-level `pinned` (LWW). A **synchronous snapshot** feeds the sync pkarr blob-builder closure (the P1 blocker) |
| D16 | Advertisement refresh | Periodic re-publish at ~half the 15-min `bs_at` freshness window + 60s-debounced fleet-change re-register. Butler-set stays max 2 entries: ordering pinned-first, then most-recently-seen (recency is the v1 proxy for P1 §3's "longest-uptime") |
| D17 | Pin-a-butler | Backend **and UI** in P2: one pinned device per owner (LWW), `set_butler_pin` IPC, toggle on the device-admin surface |
| D18 | Shape | One PR, one phase (P1's D3 pattern) |

## 3. The `dm-outhold-v1` dataset

A `FleetSyncEngine` dataset, dm-inbox-v1's mirror image (same construction: own `lookup_key_tag`, own Zenoh topic, 2-char canonical-CBOR renames, version byte, plaintext-CBOR behind SP1's encryption):

```text
DmOuthold = { entries: BTreeMap<message_cid, OutholdEntry> }
OutholdEntry = {
  pl: <storage_blob bytes>,     # the CAS blob ([ver][nonce][ct][tag]) — already encrypted
  sp: <space_id>,
  ca: <created_at: Hlc>,
}
```

- **Write site:** `send_dm`, alongside the `OutboxEntry` (the blob bytes are in hand there). Insert-once per CID — a redundant insert from a merge carries identical content (content-addressed), so merge is trivially convergent.
- **Apply site:** `on_applied`, each device inserts `pl` into its local CAS under `message_cid` and records the CID in a GC ledger. If the matching outbox entry is already terminal when the row arrives (race), skip the CAS insert and GC the row immediately.
- **GC:** when an `OutboxEntry` transitions to Complete/Expired or is deleted via `delete_dm_outbox_entry`, remove the dataset row; devices that CAS-inserted via outhold also drop their CAS copy (originator's own CAS copy is untouched — it predates the hold and serves history as today).
- **Bounds:** the hold is bounded by the pending outbox itself (30-day expiry, existing send-side limits). A defensive cap on total held bytes is pinned at plan time.

## 4. Deposit-rung candidacy extension (ZEB-422)

Current behavior (P1): drain phase C's Ok-arm **overwrites** the pair's `AttemptState` to `failure_count = 1` every window, so sent-but-unacked never accumulates and the rung — which requires `Transient && pre_count ≥ 1` — never fires for cached-but-offline recipients (`Ok`-enqueued sends that never ack: the butler's PRIMARY scenario).

P2 changes, on every draining device:

1. **Ok-arm accumulates:** `failure_count` increments (saturating) instead of resetting to 1. Side effect, intentionally accepted: direct-send backoff grows toward the 5-min cap for unresponsive recipients, matching the Err path. An ack still clears the pair via the existing `mark_ack_delivered`/retain path.
2. **Candidacy becomes:** existing `Transient failure && pre_count ≥ 1`, **OR** new: send returned Ok but the pair was already sent-and-unacked for `pre_count ≥ 2` windows (N=2, pinned as a named constant).
3. **Never-worse invariant untouched:** rung outcomes still never write `AttemptState`; at most one deposit attempt per backoff window; `SkippedNoFreshButlerSet` and `Failed` leave the entry exactly as the direct attempt left it.

## 5. `fleet-net-v1` + advertisement refresh + sibling secondary

```text
FleetNet = {
  devices: BTreeMap<device_id, NetRow>,   # NetRow = { ep: <iroh EndpointID>, hr: <home relay URL>, sa: <seen_at: Hlc> }
  pinned:  Option<device_id>,             # LWW (Hlc-stamped)
}
```

- **Row upkeep:** each device upserts its **own** row at startup and on relay change (LWW per row by `sa`). Rows for revoked devices are pruned via the existing device-revocation path; rows staler than the butler-set freshness window are excluded from selection.
- **Synchronous snapshot:** the event loop maintains `Arc<RwLock<FleetNetSnapshot>>`, updated on `on_applied` and on local row writes. This is what the sync pkarr blob-builder closure reads — resolving the P1 deferral comment in the `lib.rs` blob builder.
- **Butler-set build (max 2 entries, fleet-global order):** pinned device first (if its row is fresh), then most-recently-seen. The publishing device includes itself wherever it falls in that order; `vk` comes from `owner_device_cache`; `ep`/`hr` from the snapshot. Both writers still describe the same fleet (P1 §3 invariant).
- **Refresh:** (a) periodic re-publish at ~half of `BUTLER_SET_FRESHNESS_MS` (so `bs_at` never lapses while the device is up — exact constant pinned at plan time); (b) 60s-debounced re-publish when the snapshot's selection-relevant content changes (fleet membership, pin, relay). Both reuse the existing `PkarrPublisher` rebuild-on-publish path (BEP44 `seq+1`).

## 6. Pin-a-butler

- **Semantics:** one pinned device per owner; `pinned` is advisory ordering (a pinned-but-offline device is skipped by freshness/row-staleness, never blocks delivery).
- **IPC:** `set_butler_pin(device_id: Option<…>)` writes the LWW field through the fleet-net engine; current pin + per-device pinned flag surfaced in the existing device-admin listing payload.
- **UI:** a toggle ("act as always-on butler") per device row on the device-admin page (ZEB-170 surface), single-select semantics — pinning one device unpins the previous, no confirmation tier needed (low-risk, reversible).

## 7. Sender chain (updated) + failure modes

The P1 §6 chain is unchanged in shape; P2 widens **who** runs it (any fleet device with the replica — already true mechanically) and **when** rung 2 fires (§4).

| Failure | Behavior |
|---|---|
| Two siblings deposit the same message | Inbox insert-once + DM-store message-id dedupe absorb it (P1, exists); caps exempt duplicates |
| Two siblings direct-send concurrently | Recipient dedupes by message id; both get acks; `mark_ack_delivered` is idempotent; delivered-state merges |
| Outhold row arrives after entry terminal | `on_applied` consults outbox status → skips CAS insert, GCs the row |
| CAS insert fails on apply | Engine dirty-latch retry (P1 persist pattern); row stays until applied |
| Pinned device revoked/offline | Row pruned (revocation) or stale-excluded; ordering falls back to recency |
| Sender's whole fleet offline before any overlap | Nothing replicated — today's single-device behavior exactly (best-effort, framing-approved) |
| Recipient fetch-back races sibling's CAS GC | Entry only completes on ack, and GC fires on Complete — the serving sibling holds the blob at least until the recipient acked |

## 8. Compatibility / invariants

- **No wire-format changes:** routing blob (butler-set + `pin` bit shipped in P1), `DepositFrame`, CidNotify, `OwnerState` all byte-identical. New datasets are additive with their own tags/topics.
- **Sibling-signed packets verify:** a sibling building CidNotify/deposit signs with its own device key and lists itself in `sender_devices`; the butler's step-5 `resolve_sender_device` device→owner binding and the recipient's receive path accept any enrolled device of the sending owner.
- **P1 invariants untouched:** §4 acceptor order, persist-then-ack (D7), atomic caps with duplicate exemption, `covered_at_start` GC deferral, never-regenerate pinned fixtures.

## 9. Testing

- **Unit:** outhold CRDT merge + GC-on-terminal + late-row race; Ok-arm accumulation + new candidacy condition (incl. never-worse: rung outcomes don't touch `AttemptState`); fleet-net row LWW + pin LWW + stale-row exclusion; snapshot ordering (pinned-first, recency, self-position); refresh debounce.
- **Wire pinning:** fixtures for `DmOuthold` and `FleetNet` docs (hex-pin pattern, regeneration env-var gate).
- **Integration (two-engine, P1 harness):** A1 `send_dm` → outhold + outbox replicate to A2 → A1 stops → A2's rung deposits into a P1 acceptor → ingestion observed. Variant: sibling direct-delivery (recipient fetch-back served from A2's CAS).
- **Frontend:** pin-toggle component test (single-select, IPC error extraction per house rule).
- **Manual cross-WAN proof (post-merge, with Jake):** Koya ↔ Ildwyn, recipient-fleet-offline scenario — the headline demo. ZEB-422's trigger is validated by this proof (its ticket requirement).

## 10. Threat-model delta

The secondary butler-set entry advertises a **second** device's endpoint in the public pkarr record — inherent to advertising it as a butler, already bounded by the 2-entry cap and covered by P1 §9's analysis. `fleet-net-v1` itself is owner-internal (SP1-encrypted at rest and in transit; only the owner's devices read it). No third party touches content or metadata; the pin is not externally observable beyond entry ordering.

## 11. Open items deferred to the P2 plan

1. Exact constants: refresh cadence, N (=2) candidacy threshold name, stale-row threshold, defensive held-bytes cap.
2. CAS GC ledger mechanics (how a device records "this CAS entry came from outhold" — refcount vs direct-delete safety with DM-history copies).
3. Dataset tags/topic strings (`dm-outhold-v1`, `fleet-net-v1`) conformance with existing naming.
4. Snapshot wiring points in `event_loop.rs`/`start_node` (where engine handles are available — see the P1 deferral comment in the `lib.rs` blob builder).
5. Whether `send_dm` writes outhold before or after the OutboxEntry within its existing lock/persist sequence (atomicity of the pair).
6. Device-admin payload shape for the pin toggle + UI copy.
7. Verify the recipient fetch-back path serves from sibling CAS via ZEB-343's serve gate for DM blobs end-to-end (expected: encrypted-bit CIDs are served; integration test confirms).
