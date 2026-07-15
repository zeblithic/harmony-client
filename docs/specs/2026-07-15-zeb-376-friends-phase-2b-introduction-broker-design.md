# Friends Phase 2b — Active Introduction Broker + `PeerIntroPolicy` — Design

**Ticket:** ZEB-376 (child of ZEB-321). **Builds on:** Phase 2a (ZEB-375, merged
PR #192 — referral-catalog awareness / friend-PEX browse) and the arc spec
[`2026-06-03-friends-peer-introductions-design.md`](./2026-06-03-friends-peer-introductions-design.md)
(§5.4 Path C, §6.2 active broker, §7 abuse posture).

**Status:** design approved (reachability-in-envelope + single-PR scope), pending
spec review.

---

## 1. Problem & intent

Phase 2a made a friend's referrable peers **visible**: over a live
`harmony/friend/v1` link you send a `CatalogRequest` to friend F and get back a
signed `ReferralCatalog` of `{peer_owner, display}` entries. Nothing connects; no
reachability is exposed (arc §6.1).

Phase 2b makes the introduction **active**: you ask F to introduce you to one of
those peers X, F relays a signed vouch to X, X decides (per its `PeerIntroPolicy`)
whether to accept, and on accept **X forms a direct friend-link with you** — F
drops out, never party to your↔X relationship. This is the arc's "Path C"
(§5.4/§6.2), the mechanism by which "connectivity islands" merge without
out-of-band invites.

**In scope:** the `IntroduceRequest`/`Introduction` wire protocol + signing;
F's broker relay; X's `PeerIntroPolicy` enforcement (persistence + IPC + the four
policy branches + AskMe staging); the introduction-driven link completion
(reachability-in-envelope so first contact needs no global discoverability); the
frontend request-introduction affordance + policy dropdown; a 3-node e2e.

**Out of scope:** changes to Case-D rendezvous, the referral catalog (2a), the
friend-token path (Path B), or the mutual-key path (Path A) beyond the one
extracted dial helper they share.

---

## 2. The Path-C flow

Three actors: **You** (initiator, an Active friend of F), **F** (broker, mutual
friend), **X** (target, an Active + `referrable` friend of F, behind F).

1. **You browse** F's referral catalog (2a). X appears as
   `ReferralEntry{peer_owner, display}`. You tap **Request introduction**.
2. **You → F** (`IntroduceRequest`): "introduce me to X; here is my
   device-#2 cert and my current reachability." You resolve F via Case-D (F is
   your Active friend) and dial the friend-PEX ALPN, exactly as browse does. You
   **locally record a pending-outbound introduction to X** (owner-keyed, TTL'd) so
   X's inbound link auto-accepts.
3. **F validates + relays.** F checks: your device-#2 signature; X is F's
   **Active + `referrable`** friend; `to_addr == F`. F builds and signs an
   `Introduction{voucher: F, subject: you + cert + reachability, at}`, resolves X
   via Case-D, and dials X's friend-PEX ALPN to deliver it. F acks you.
4. **X enforces `PeerIntroPolicy`.** X verifies F's signature, that F is X's
   Active friend, and your cert. Then:
   - `Open` → proceed automatically.
   - `FriendsOfFriends` (**default**) → proceed iff the voucher (F) is an Active
     friend of X (already established in the verify step).
   - `AskMe` → stage an introduction-offer in X's pending inbox; proceed **only**
     on X's explicit accept.
   - `Closed` → reject; F relays a benign "declined" back to you.
5. **X → you (direct link).** On proceed, X synthesizes a dial target from the
   reachability carried in the `Introduction` (no pkarr lookup, no Case-B), dials
   your `harmony/friend/v1` acceptor, and sends a token-less `FriendLinkRequest`.
   Your acceptor matches X against your pending-outbound record → auto-accepts.
   Both sides write `FriendEntry{status: Active, established_via: Introduction}`
   and begin Case-D publication for each other. **F drops out.**

---

## 3. The reachability decision (the design's central commitment)

The final step (§2.5) is **genuine first contact**: X and you are not yet
friends, so neither publishes a Case-D record for the other. The arc (§5.2/§8)
accepted, "for the alpha," that first contact falls back to **Case-B** (which
requires the *target's* global discoverability) or a Path-B token. But an
introduction has no token, and forcing the introducee to be world-discoverable
contradicts §7's headline promise ("**No global-discoverability requirement**").

**Decision: the reachability rides inside the introduction envelope.** The
`IntroduceRequest` carries the introducee's own current
`ReachabilityAnnouncePayload` (`reachability_record.rs:81`) — the same
self-authenticating routing blob the reachability publisher emits for Case-A/B/C.
F relays it verbatim inside the `Introduction`. X feeds it to the dial path.

Why this is sound and cheap:

- **Self-authenticating.** `ReachabilityAnnouncePayload.identity_signature`
  binds the iroh `node_id` (and seal-targets) to the harmony identity key. X runs
  the **exact** checks the Case-B initiator already runs
  (`connectivity_add_friend_by_key_inner`, lib.rs:54683-54690):
  `verify_inner_sig`, `verify_identity_match`, `verify_freshness`. Same security
  guarantees as a pkarr-resolved record — X simply skips the DHT lookup because
  the record was handed to it.
- **Fresh.** You are online and initiating; the blob you embed is your live
  reachability, and X dials promptly. No staleness window a pkarr TTL wouldn't
  also have.
- **No new dial machinery.** The Case-B path already turns a
  `ReachabilityAnnouncePayload` into an `EndpointAddr` and dials
  `harmony/friend/v1`. We extract "step 3 onward" (synthesize → dial → sign
  `FriendLinkRequest` → apply outcome) into a shared helper; the Case-B path
  resolves the payload, the introduction path receives it in the envelope. See §7.

**Consequence:** the introducee never flips on global discoverability. Case-B
remains available for out-of-band add-by-key, but the introduce flow does not
depend on it.

---

## 4. Data model & wire types

New wire types live in `referral_catalog.rs` (or a sibling `friend_intro.rs`
sharing its codec), mirroring the 2a discipline **exactly**: strict CBOR,
single-/two-char map keys, bounded decode at `PEX_MAX_PACKET_LEN` with
trailing-byte rejection, device-#2 Ed25519 signatures over a domain-separated
CBOR preimage, `verify_enrolled_device` as the auth chokepoint, address-binding
(no nonce) for anti-replay.

### 4.1 `IntroduceRequest` (You → F)

```
IntroduceRequest {
    from_addr:  OwnerAddr,                 // you (requester)          key "a"
    to_addr:    OwnerAddr,                 // F (broker) — re-aim guard key "d"
    target:     OwnerAddr,                 // X (whom you want to meet) key "x"
    reachability: ReachabilityAnnouncePayload,  // your dial target    key "r"
    enrollment: EnrollmentCert,            // your device-#2 cert       key "c"
    sig:        [u8; 64],                  // device-#2 over preimage   key "s"
    signer_certs: Vec<EnrollmentCert>,     // ZEB-677 bundle (empty today) key "b"
}
```

Preimage `introduce_request_sig_preimage(from, to, target, &reachability)`,
domain tag **`"hir1"`**. Binding `to_addr` blocks re-aiming the request at a
different broker; binding `target` blocks swapping whom you asked to meet.

### 4.2 `Introduction` (F → X)

```
Introduction {
    voucher:    OwnerAddr,                 // F (introducer)            key "v"
    to_addr:    OwnerAddr,                 // X (target) — re-aim guard key "d"
    subject:    OwnerAddr,                 // you (introducee)          key "s"
    subject_cert: EnrollmentCert,          // your device-#2 cert       key "c"
    reachability: ReachabilityAnnouncePayload,  // your dial target     key "r"
    at:         Hlc,                       // freshness / dedupe        key "t"
    sig:        [u8; 64],                  // F's device-#2 over preimage key "g"
    voucher_enrollment: EnrollmentCert,    // F's device-#2 cert        key "e"
    signer_certs: Vec<EnrollmentCert>,     // ZEB-677 bundle (empty today) key "b"
}
```

Preimage `introduction_sig_preimage(voucher, to, subject, &subject_cert,
&reachability, &at)`, domain tag **`"hin1"`**. F signs the whole envelope
including your cert and reachability, so X can trust "F vouches that this subject,
reachable here, asked to meet me" without F being able to forge the subject's
identity (your cert is still your Master-issued cert; F only relays it).

### 4.3 Verification order (security-load-bearing, mirrors 2a §262/§318)

**F authenticating an `IntroduceRequest`:**
1. `req.to_addr == self_owner` else `WrongTarget`.
2. `verify_enrolled_device(&req.enrollment, &req.signer_certs, req.from_addr)`
   (binds cert→requester).
3. `verify_strict(req.sig)` over the `"hir1"` preimage.
4. **Authorization:** `req.target` is an `Active` + `referrable` friend in F's
   graph (else `NotReferrable` → benign decline). Reuses the 2a
   `project_referrals` predicate.

**X verifying an `Introduction`:**
1. `intro.to_addr == self_owner` else `WrongTarget`.
2. `verify_enrolled_device(&intro.voucher_enrollment, …, intro.voucher)` (binds
   F's cert→F).
3. `verify_strict(intro.sig)` over the `"hin1"` preimage.
4. `verify_enrolled_device(&intro.subject_cert, …, intro.subject)` (binds your
   cert→you — X will pin this into the resulting `FriendEntry`).
5. Reachability inner checks (§3): `verify_inner_sig` + `verify_identity_match` +
   `verify_freshness` on `intro.reachability`.
6. `PeerIntroPolicy` enforcement (§6) — F being an Active friend is the
   `FriendsOfFriends` predicate.

---

## 5. Transport — extend the friend-PEX ALPN to a frame enum

The friend-PEX ALPN (`harmony/friend-pex/v1`, `iroh_pex_acceptor.rs`) is today a
single-shot `CatalogRequest → ReferralCatalog`. Rather than stand up a new ALPN
(new acceptor + dispatcher arm + accept-loop gate + multiplex entry), we make the
first decoded item a tagged **`PexFrame`**:

```
enum PexFrame {
    CatalogRequest(CatalogRequest),   // existing browse (2a)
    IntroduceRequest(IntroduceRequest), // You → F (2b)
    Introduction(Introduction),        // F → X (2b)
}
```

The acceptor's `serve()` decodes a `PexFrame` and dispatches:
- `CatalogRequest` → existing `serve_catalog_for_request` (unchanged behavior).
- `IntroduceRequest` → validate + authorize (§4.3), then F **spawns** the F→X
  delivery (resolve X via Case-D, dial, send `Introduction`) and acks the
  requester. The spawn keeps F's acceptor single-shot and non-blocking.
- `Introduction` → validate (§4.3) + enforce policy (§6) + (on proceed) trigger
  the introduction-driven link (§7).

**Wire-compat (no flag-day):** a ciborium externally-tagged enum encodes as a
map (`{"CatalogRequest": …}`), which is **not** byte-identical to a bare 2a
`CatalogRequest`. So the friend-PEX **decoder tries `PexFrame` first and falls
back to decoding a bare `CatalogRequest`** — 2a peers (bare) and 2b peers
(`PexFrame::CatalogRequest`) both serve correctly, no coordinated upgrade needed.
The 2a `zeb375_pex_fixtures` bytes stay byte-pinned (unchanged); the fallback is
what keeps them valid. A full flag-day (require `PexFrame` everywhere) is rejected
for 2b — it would break every un-upgraded 2a peer. Both 2b message directions are
new tags, so they never collide with 2a bytes.

---

## 6. `PeerIntroPolicy` — persistence, IPC, enforcement

### 6.1 Persistence (single-user, `connectivity-settings.json`)

`PeerIntroPolicy` already exists as a type (`friend_graph.rs:118`, Open/
FriendsOfFriends(default)/AskMe/Closed, lowercase serde tags) but is unenforced
and unpersisted. Its home is **`ConnectivitySettings`**
(`connectivity_settings.rs:9`), alongside `friend_auto_accept_known` — a
single-user preference, **not** the CRDT (that is per-friend `referrable`).

- New field `peer_intro_policy: PeerIntroPolicy` with
  `#[serde(default = "default_peer_intro_policy")]` (→ `FriendsOfFriends`),
  mirroring `default_friend_auto_accept_known` (`:42`).
- Added to `Default` (`:64`) **and** `fail_closed_defaults()` (`:301`): a
  corrupt/unreadable settings file must not silently widen the policy. The
  fail-closed value is **`Closed`** (most restrictive; a parse failure should
  never auto-introduce a stranger). Note this differs from the *fresh-install*
  default `FriendsOfFriends` — fail-closed is strictly for the
  corrupt-file path, matching how the existing trust-sensitive toggles degrade.
- Atomic save via the existing `save()` (NamedTempFile + fsync + rename).

### 6.2 IPC (get/set, cloned from `friend_auto_accept`)

- `get_peer_intro_policy() -> Result<PeerIntroPolicy>` — clones
  `get_friend_auto_accept` (`lib.rs:54423`): snapshot `connectivity_settings_path`,
  `load_or_default(&path).peer_intro_policy`, spec default when path is `None`.
- `set_peer_intro_policy(policy: PeerIntroPolicy) -> Result<()>` — clones
  `set_friend_auto_accept` (`lib.rs:54373`): the process-global
  `connectivity_settings_write_lock` RMW, load→mutate→save in `spawn_blocking`,
  then `app.emit("connectivity-peer-intro-policy-changed", …)`.
- `PeerIntroPolicy` already `Serialize`/`Deserialize` with lowercase tags, so it
  crosses the IPC boundary as `"open"/"fof"/"ask"/"closed"`.

**Live-apply:** like `friend_auto_accept_known`, the running acceptor captures the
policy at `start_node`. To let a policy change take effect without restart, the
acceptor reads the policy through an interior-mutability handle
(`Arc<ArcSwap<PeerIntroPolicy>>` or an `Arc<AtomicU8>`) that `set_peer_intro_policy`
updates. This is a small addition over the auto-accept precedent and avoids a
"applies on next start" footgun for a security control. (If the plan finds the
handle threading too invasive for 2b, document the restart limitation explicitly —
but the interior-mutability handle is the preferred path.)

### 6.3 Enforcement (`decide_consent` is *not* the seam; a new intro gate is)

`decide_consent` (`iroh_friend_acceptor.rs:833`) governs inbound
**`FriendLinkRequest`** consent (Path A/B). The `PeerIntroPolicy` decision happens
earlier and elsewhere — when X processes an **`Introduction`** (§2.4), before any
link exists. So enforcement is a **new pure function** beside `decide_consent`:

```
enum IntroDecision { Proceed, Stage, Reject }   // Open→Proceed, FoF→Proceed(if voucher active),
                                                //  AskMe→Stage, Closed→Reject
fn decide_introduction(policy: PeerIntroPolicy, voucher_is_active_friend: bool) -> IntroDecision
```

- `Proceed` → X runs the introduction-driven link (§7).
- `Stage` (AskMe) → record an introduction-offer in the pending inbox (§8) and
  emit `friend-request-received`; X's later accept promotes it to `Proceed`.
- `Reject` (Closed, or FoF with a non-active voucher) → drop; F relays a benign
  decline.

Keeping this a pure function beside `decide_consent` matches the codebase's
"pure policy, no I/O" pattern and makes the four branches unit-testable without a
live acceptor.

---

## 7. Link completion — shared dial helper + pending-outbound pre-auth

### 7.1 Extracted dial helper

Factor the post-resolve tail of `connectivity_add_friend_by_key_inner`
(lib.rs:54695 onward — synthesize `EndpointAddr` from a
`ReachabilityAnnouncePayload`, dial `harmony/friend/v1`, sign & send the
`FriendLinkRequest`, apply the `AddFriendOutcome`) into:

```
async fn dial_and_link_friend(
    reachability: ReachabilityAnnouncePayload,   // dial target (resolved OR from envelope)
    target_owner: OwnerAddr,
    origin: FriendOrigin,                         // MutualKey (Case-B) | Introduction (Path C)
    …self identity/keys/crdt handles…
) -> Result<AddFriendOutcome, String>
```

- Case-B path: resolves `reachability` via pkarr, then calls with
  `FriendOrigin::MutualKey`.
- Introduction path: X, on `IntroDecision::Proceed`, calls with the envelope's
  `reachability` and `FriendOrigin::Introduction`. **X is the dialer** and stamps
  `established_via: Introduction` on its own `FriendEntry` (the initiator site
  currently hardcodes `MutualKey` at lib.rs:55021 — parameterized by `origin`).

### 7.2 Pending-outbound introductions (your pre-auth)

When you send the `IntroduceRequest` (§2.2), you know X's `OwnerAddr` (from the
2a catalog). You record it in a new process-local
`PendingOutboundIntroductions` store (mirrors `PendingFriendRequests`,
`friend_requests.rs:48`): `record(target, TTL)`, `take(owner) -> bool`,
TTL-expiry. This is the pre-authorization: you asked to meet X, so X's inbound
link should not prompt you.

### 7.3 Auto-accept on your acceptor

X's inbound `FriendLinkRequest` is token-less and (to your acceptor) from an
unknown owner → today `decide_consent` returns `Pending`. We add the
introduction pre-auth as a new branch, resolved atomically like the existing
`prior_accept` approval (`resolve_consent_consuming_approval`, `:863`):

- Extend `ConsentDecision` with **`AcceptInlineIntroduced`** (stamps
  `established_via: Introduction`, distinct from `AcceptInline`'s `MutualKey`).
- In `resolve_consent_consuming_approval`, if `decide_consent(...) == Pending`
  and `pending_outbound.take(&from)` is true → `AcceptInlineIntroduced`. The
  `take` consumes the one-shot pre-auth atomically (same TOCTOU-closing pattern as
  `take_approved`), so concurrent dials from X yield exactly one inline accept.

The authenticated `from` owner (handshake cert-bound) is what we match against the
pending-outbound set — so only the X you actually asked to meet is auto-accepted;
an unrelated unknown dialer still lands at `Pending`.

---

## 8. AskMe staging — reuse the pending-request inbox

The ticket calls for reusing the Phase-1b `PendingFriendRequests` inbox +
`friend-requests-section` UI. On `IntroDecision::Stage`, X records an
introduction-offer as a pending entry **tagged with the voucher F** (an
entry-kind discriminant on the store: `LinkRequest` vs `IntroductionOffer`).
`list_pending_friend_requests` projects both; the `PendingFriendRequestDto` gains
an optional `introducedBy: Option<display>` so the UI can badge "introduced by F".

Accept/decline branch on the kind:
- `accept_friend_request` on an `IntroductionOffer` → run the §7 introduction
  link (X dials you), **not** the existing approve-and-wait. On a `LinkRequest` →
  existing `store.approve` behavior, unchanged.
- `decline_friend_request` → drop the entry either way.

This reuses the store, the two IPCs, the `friend-request-received` event, and the
inbox UI section; the only new surface is the entry-kind discriminant and the
accept branch.

---

## 9. Trust, consent & abuse posture (arc §7)

- **Authn everywhere.** Every envelope is device-#2 signed and cert-bound through
  `verify_enrolled_device`. F cannot fabricate a subject (your Master-issued cert
  rides inside the `Introduction`); a compromised F can only relay *real* signed
  requests or refuse.
- **X is the consent authority.** F never decides whether you and X connect —
  `PeerIntroPolicy` on X's node does. `Closed`/`AskMe` stop auto-links from a
  spammy or compromised F.
- **Replay / re-aim.** Address-binding (`to_addr`, `target`, `subject`) in both
  preimages (no nonce, matching 2a). The `Introduction.at` HLC gives freshness;
  the pending-outbound TTL bounds the window on your side.
- **Rate-limit / dedupe (DoS hygiene, secondary to policy).**
  - X: dedupe inbound `Introduction`s by `(voucher, subject)` within a freshness
    window (a bounded recent-set), and cap Introductions accepted per voucher per
    window — a compromised F cannot flood X's inbox.
  - F: cap `IntroduceRequest`s relayed per requester per window; only relay for
    `referrable` targets.
  - These are bounded in-memory structures with documented caps (the
    "no silent truncation" rule: log when a cap sheds).
- **F is never party to the secret.** The friend-link (§7) is direct X↔you over
  `harmony/friend/v1`; the pairwise Case-D secret is derived by the two of you. F
  relayed only signed, self-authenticating envelopes and then dropped out.
- **Revocation.** Unchanged from the arc: unfriending X sets a `Revoked`
  tombstone, stops Case-D publication, and drops X from future catalogs — an
  introduced friend is a friend like any other.

---

## 10. Frontend (Svelte/TS)

- `friend-service.ts`: add `getPeerIntroPolicy()`, `setPeerIntroPolicy(policy)`,
  `requestIntroduction(viaFriendOwnerIdHex, targetOwnerIdHex)` — each through the
  single `invoke<T>` wrapper (camelCase params, `Error`/string rejection
  normalization per CLAUDE.md). Subscribe to `connectivity-peer-intro-policy-changed`.
- `FriendsPanel.svelte`:
  - **Request-introduction button** in the referral-item `<li>` else-branch
    (`:888-897`), shown when `!r.alreadyFriend`; calls
    `service.requestIntroduction(browsedFriendOwnerId, r.ownerIdHex)`. Surfaces a
    transient "introduction requested" / "declined" / "unreachable" status.
  - **Policy dropdown** — a new `action-block` beside the auto-accept section
    (`:1138-1154`): a `<select>` of Open/FriendsOfFriends/AskMe/Closed bound to
    `peerIntroPolicy`, with the load/save/loading/error quartet mirrored from
    `autoAccept` (`:157-160`, `loadAutoAccept` `:223`, `handleAutoAcceptToggle`
    `:684`).
  - **AskMe offers** render in the existing `friend-requests-section`
    (`:962-1018`) with an "introduced by F" badge when `introducedBy` is present;
    Accept/Decline reuse the existing handlers.

---

## 11. Testing strategy (TDD)

- **Wire fixtures** (`zeb376_intro_fixtures`, beside 2a's `zeb375_pex_fixtures`):
  byte-pinned canonical CBOR for `IntroduceRequest`, `Introduction`, and
  `PexFrame` variants; a test that the 2a `CatalogRequest` bytes still decode
  under the new frame decoder (wire-compat guard).
- **Sign/verify units:** happy path; each `WrongTarget`/`AuthorMismatch`/`Auth`/
  `SignatureInvalid` branch for both preimages; a forged voucher cert; a subject
  cert swapped for a different owner; a re-aimed request (`to_addr` mismatch); a
  swapped `target`.
- **Reachability-in-envelope:** an `Introduction` carrying a reachability blob
  with a bad `identity_signature` / stale `announced_at_ms` / mismatched identity
  is rejected at X before dialing.
- **Policy:** `decide_introduction` truth table (4 policies × voucher-active
  bool); `fail_closed_defaults()` yields `Closed`; get/set round-trip persists
  (mirror `set_friend_auto_accept_persists_round_trips`, lib.rs:56204); live-apply
  via the interior-mutability handle.
- **Consent:** `AcceptInlineIntroduced` only when the sender is in the
  pending-outbound set; atomic one-shot consumption under concurrent dials; TTL
  expiry drops the pre-auth.
- **AskMe staging:** an offer is recorded + surfaced; accept runs the link,
  decline drops it; the `LinkRequest` path is unchanged.
- **Abuse:** dedupe by `(voucher, subject)`; per-voucher cap sheds with a log.
- **3-node e2e** (headless, `api`/e2e-harness): You—F—X, all three named
  profiles; you browse F, request X, X's policy = `FriendsOfFriends`, assert a
  mutual `FriendEntry{established_via: Introduction}` on both you and X and that F
  holds no you↔X friend edge. A second run with X's policy = `AskMe` asserts the
  offer stages and completes only after an explicit accept; a third with `Closed`
  asserts no link forms. (e2e assertions poll the DTO's camelCase keys.)

---

## 12. Open questions — resolved / deferred

- **Reachability mechanism** — RESOLVED: in-envelope (§3), not Case-B.
- **Transport** — RESOLVED: `PexFrame` enum on the existing friend-PEX ALPN (§5).
- **AskMe UI** — RESOLVED: reuse the pending-request inbox with an entry-kind
  discriminant (§8).
- **Live-apply of policy** — preferred: interior-mutability handle (§6.2);
  fallback (documented) is restart-scoped.
- **Deferred:** epoch/rotation of the `(voucher, subject)` dedupe set beyond a
  simple freshness window; a gossip-mesh liveness layer (arc "Approach 3") — both
  post-2b optimizations that don't change this model.

---

## 13. Scope & execution

**One PR** (ZEB-376), executed via subagent-driven-development, ~14–16
bite-sized tasks along these seams: (A) wire types + sign/verify + fixtures;
(B) `PexFrame` transport + acceptor dispatch; (C) F's broker relay + authorization;
(D) X's `Introduction` verify + `decide_introduction`; (E) `PeerIntroPolicy`
persistence + fail-closed + get/set IPC + live-apply handle; (F) extracted
`dial_and_link_friend` + `FriendOrigin` parameterization; (G) pending-outbound
store + `AcceptInlineIntroduced` consent; (H) AskMe staging discriminant;
(I) `request_introduction` IPC; (J) frontend service + panel; (K) abuse
caps/dedupe; (L) 3-node e2e. Whole-branch review + bot converge before ready.

Sliced into two PRs was considered and rejected: it would ship a live-but-inert
policy dropdown to `main` (worse half-state than a cohesive larger review), and
ZEB-376 is a single ticket.

---

## 14. References

- Arc spec: [`2026-06-03-friends-peer-introductions-design.md`](./2026-06-03-friends-peer-introductions-design.md)
  (§5.4 Path C, §6.2 broker, §7 abuse, §8 cold-start).
- 2a: `src-tauri/src/referral_catalog.rs` (wire discipline, `"hcr1"`/`"hrc1"`),
  `iroh_pex_acceptor.rs` (friend-PEX ALPN).
- Handshake: `iroh_friend_acceptor.rs` (`decide_consent` :833,
  `resolve_consent_consuming_approval` :863, `process_friend_request` origin site
  :1084, `FriendOrigin` `friend_graph.rs:107`).
- Reachability: `reachability_record.rs:81` (`ReachabilityAnnouncePayload`);
  `connectivity_add_friend_by_key_inner` (lib.rs:54607, Case-B resolve + dial).
- Settings: `connectivity_settings.rs` (`ConnectivitySettings`,
  `fail_closed_defaults` :301); IPC template `lib.rs:54373-54440`.
- Frontend: `src/lib/friend-service.ts`, `src/lib/components/FriendsPanel.svelte`.
