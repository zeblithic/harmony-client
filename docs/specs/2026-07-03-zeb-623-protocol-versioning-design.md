# ZEB-623 — Wire-Protocol Versioning & Evolution: Design

**Status:** blessed design (expands Area G of the ZEB-321 Phase 3 decision record, 2026-07-01)
**Ticket:** [ZEB-623](https://linear.app/zeblith/issue/ZEB-623) (S7 of ZEB-321 Phase 3)
**Depends on / expands:** [`2026-07-02-zeb-321-phase3-decision-record.md`](2026-07-02-zeb-321-phase3-decision-record.md) Area G
**Module:** `src-tauri/src/protocol_versioning.rs`

This slice codifies *how harmony wire protocols evolve* without stranding
peers mid-upgrade. It ships the policy constants, the `TunnelHello`
capability-frame primitive, and the tunnel `/v2` ALPN as the exemplar. It does
**not** yet wire the hello exchange into the live tunnel acceptor/dialer — that
is a follow-up in this bundle; this task establishes the vocabulary every later
task cites.

## 1. Two mechanisms, two rates of change

Harmony has exactly two knobs for protocol change, and they turn at very
different speeds. Conflating them is the failure mode this spec exists to
prevent.

### ALPN generation bump — *wire-incompatible, rare*

Each sub-protocol carries a `/vN` suffix in its ALPN string
(`harmony/tunnel/v1`, `harmony/tunnel/v2`, …). A generation bump means the
**framing itself changed incompatibly** — a `/v1` reader cannot parse a `/v2`
stream at all. This is the heavyweight, rare move.

Because QUIC/TLS ALPN negotiation is *server-picks-from-the-client's-list* but
iroh `connect()` takes exactly **one** ALPN per attempt, cross-generation
interop is handled by an explicit dual posture:

- **Acceptor deprecation window.** The endpoint binds *every* still-supported
  generation at once. Today the bind list registers both
  `alpn::HARMONY_TUNNEL_V1` and `alpn::HARMONY_TUNNEL_V2`
  (`iroh_endpoint.rs`), so a `/v1`-only peer keeps connecting while `/v2` rolls
  out.
- **Dialer newest-first fallback.** A dialer attempts the newest generation it
  knows (`TUNNEL_ALPN_GENERATION`) first, and on a connect-level failure falls
  back to the next-older generation. The cost of a bump is therefore *one extra
  connect round-trip* only for the shrinking set of not-yet-upgraded peers.

The generation the dialer prefers and the oldest it still accepts are pinned in
`protocol_versioning.rs` as `TUNNEL_ALPN_GENERATION` /
`MIN_SUPPORTED_TUNNEL_ALPN_GENERATION`.

### Hello / capabilities frame — *feature evolution, common*

Most protocol change is *additive*: a new optional feature, a new field, a new
behavior a peer may or may not support. That never justifies a new ALPN. Instead
each protocol's **first frame** is a versioned hello:

```rust
pub struct TunnelHello {
    pub protocol_version: u16,
    #[serde(default)]
    pub capabilities: u64,   // additive bitmap; unknown bits ignored
}
```

`protocol_version` advances for feature milestones; `capabilities` is an
additive bitmap where **unknown bits are ignored**. A newer peer can advertise
capability bits an older peer has never heard of, and the older peer simply
doesn't use them — no new ALPN, no extra dial, no compile-time coupling. This is
the common, cheap path.

## 2. Fleet compatibility policy: N / N-1

A node supports the **current and the previous** protocol generation — never
just the current one. The floor is expressed as the `MIN_SUPPORTED_*` constants,
which **live in `protocol_versioning.rs`** so there is exactly one place to read
"what will this build still talk to":

| Constant | Meaning |
|---|---|
| `TUNNEL_ALPN_GENERATION` | newest ALPN generation the dialer prefers |
| `MIN_SUPPORTED_TUNNEL_ALPN_GENERATION` | oldest ALPN generation still on the bind list |
| `TUNNEL_PROTOCOL_VERSION` | hello version this build advertises |
| `MIN_SUPPORTED_TUNNEL_PROTOCOL_VERSION` | oldest hello version this build interoperates with |

The invariant `MIN_SUPPORTED_* <= CURRENT_*` holds while a deprecation window is
open; retiring a generation is the deliberate act of advancing the `MIN` floor
(and only then dropping the old ALPN from the bind list).

**Incompatibility must surface, never fail silently.** When a peer is below the
supported floor, that is *not* a silent connect drop — it is recorded in
`ProtocolCompatRegistry` (which logs loudly via `tracing::warn!`) and is meant to
surface in **Network Health** so an operator sees "peer X is on an unsupported
protocol generation" rather than an unexplained missing peer. A silent drop here
would be indistinguishable from a network outage — exactly the debugging trap
Phase 3 is trying to eliminate.

## 3. Additive payload rule (codifying the de-facto CRDT convention)

The CRDT/DTO layer already evolves this way de-facto; this spec writes it down as
a hard rule for **all** serde-serialized wire payloads, framing included:

1. **New fields are additive and defaulted.** Every field added to a
   wire/serialized struct carries `#[serde(default)]` so an older peer's message
   (which omits it) still decodes. `TunnelHello::capabilities` is the reference
   example.
2. **Never `#[serde(deny_unknown_fields)]` on a wire type.** Unknown-field
   tolerance is what lets a v-next peer add fields an older peer ignores. Denying
   unknown fields turns every additive change into a wire break.
3. **Signed preimages must explicitly thread new fields.** Serde tolerance makes
   *decoding* forward-compatible, but a **signature preimage is a manual, ordered
   byte layout** — adding a field to a signed struct without adding it to the
   preimage builder means the new field is unauthenticated (MITM-malleable). Any
   new signed field MUST be threaded into the preimage explicitly. See
   `friend_request_sig_preimage` (`iroh_friend_acceptor.rs`), whose domain-tagged
   builder was extended for `eph_x25519_pub` (ZEB-371) and `devices_digest`
   (ZEB-461) exactly this way — each new signed field was added to the preimage,
   not just the struct.

## 4. Review checklist (for any PR that touches a wire type or ALPN)

- [ ] **New field has a default?** Every added serde field on a wire/serialized
      type carries `#[serde(default)]` (and the type is not
      `deny_unknown_fields`).
- [ ] **Preimage updated?** If the changed struct is signed, the new field is
      threaded into its `*_sig_preimage` builder — not just the struct.
- [ ] **Is an ALPN bump really needed?** A `/vN` bump is justified *only* by a
      wire-incompatible framing change. If a `#[serde(default)]` field or a
      capability bit would do, do that instead.
- [ ] **Hello version bumped instead?** For a new *feature* (not a reframing),
      bump `TUNNEL_PROTOCOL_VERSION` / set a new capability bit rather than
      minting an ALPN.
- [ ] **Health surfaced?** Any path that rejects a peer for protocol reasons
      records it in `ProtocolCompatRegistry` so it shows in Network Health — no
      silent connect failures.

## 5. Exemplar: tunnel v2 hello

The tunnel protocol is the exemplar for this task. It gains a `/v2` ALPN
generation whose first frame (per direction) is a `TunnelHello`, pipelined so the
hello costs no extra round-trip:

```
Dialer (tunnel/v2)                         Acceptor (binds tunnel/{v2,v1})
  |                                             |
  |  connect ALPN=harmony/tunnel/v2             |
  |-------------------------------------------->|  accept; pick v2 from list
  |                                             |
  |  TunnelHello{ protocol_version, caps } ---->|  (sent immediately, pipelined
  |                                             |   with the first PQ handshake
  |<---- TunnelHello{ protocol_version, caps }  |   bytes — no extra RTT)
  |                                             |
  |     each side: check_hello_compatible()     |
  |       ok  -> intersect capabilities, proceed
  |       err -> note_incompatible(node_id, …)  -> Network Health, close
```

Compatibility rules applied to the received hello:

- `protocol_version < MIN_SUPPORTED_TUNNEL_PROTOCOL_VERSION` →
  `check_hello_compatible` returns `Err(reason)`; the caller calls
  `ProtocolCompatRegistry::note_incompatible` (loud) and closes. **Not** a silent
  drop.
- `protocol_version >= MIN_SUPPORTED` (including a version *newer* than ours) →
  compatible. The two `capabilities` bitmaps are intersected; unknown bits on
  either side are ignored, so each side uses only features both understand.
- A well-formed but oversized frame (`> TUNNEL_HELLO_MAX`) is rejected by
  `decode_hello` before any allocation/parse, bounding first-frame cost against a
  hostile peer.

Wiring this exchange into the live tunnel acceptor and dialer (including the
newest-first ALPN fallback) is the follow-up task; this slice lands the
primitives and the policy they enforce.
