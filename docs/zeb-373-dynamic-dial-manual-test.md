# ZEB-373 dynamic mid-session iroh dial — manual cross-machine test playbook

This playbook validates [ZEB-373](https://linear.app/zeblith/issue/ZEB-373): when a node
already running learns a **new** peer's iroh node-id mid-session, it proactively dials that
peer over the iroh transport (Zenoh `connect_peer`) instead of waiting for the next boot's
static-seed pass. It is the manual counterpart to the automated acceptance test
`src-tauri/tests/zeb_373_dynamic_dial_integration.rs`.

Companion surface: the **Network → Dynamic dials** panel (ZEB-377) renders the dial
telemetry this playbook asserts on.

---

## 0. The one gotcha that invalidates a careless test

ZEB-373 only fires on **first-learn** of a `(owner, node_id)` pair. At boot, the resolver
replays *persisted* peer reachability into its cache **before** the dial-hint sender is
installed (`lib.rs` bootstrap replay → `event_loop.rs` sender install). Those replayed peers
become **static Zenoh connect-seeds** and emit **no** `DialHint`. The dynamic dial only
triggers for a peer learned **while already running and never seen before**.

So: a peer the observing node has *ever* persisted will be static-seeded on the next launch,
not dynamically dialed. The resolver map is keyed by `(owner, node_id)` **globally**, not
per-community — a peer known from any prior community counts as "seen". Design the test for a
genuine first-contact, or you will watch a correctly-working feature do nothing.

---

## 1. The direction asymmetry (important)

The two directions are **not** symmetric. Only one of them exercises ZEB-373:

| Direction | What happens | Dynamic dial? |
|---|---|---|
| **A → B** — A up first & idle, B joins later | B's `Join`/`ReachabilityAnnounce` lands in A's community CRDT *mid-session* → `resolver.update()` → first-learn → `DialHint` → dial | **YES — this is the test.** |
| **B → A** — joiner → inviter, during redeem | B obtains A's reachability *before* its event loop is up: invite-only → **pkarr case-A** pre-seed; open community → prior DM cache. Seeded as `was_present` → no `DialHint` | **NO — static seed.** |

Consequences:
- Designate the machine that comes up **first and sits idle** as the **dial observer (Node A)**.
- The community invite carries only `community_id` + `admin_addr` (+ admin bootstrap/token for
  invite-only) — **no routing / node-id** (`community_invite.rs`). B's first contact to A is
  bootstrapped via **pkarr case-A** (keyed off the invite token, `pkarr_invite_publisher.rs`),
  so the two machines need **internet access** (mainline DHT) for the first redeem even on the
  same LAN.

---

## 2. Roles

| | Machine | Node | Job |
|---|---|---|---|
| **A** | Koya (macOS) | up **first**, idle | the guaranteed dial observer |
| **B** | Ildwyn (Windows) | joins **second** | triggers A's first-learn; can read its own telemetry via Playwright |

Same WiFi → both publish private **direct addresses**, so iroh should take the **direct path**
with no relay. This is the easiest-possible case and the right first validation.

**Dev builds are sufficient.** The entire iroh/Zenoh p2p stack runs identically in dev vs
release; only the auto-updater/signing differ. No release binaries are needed.

---

## 3. Prerequisites (once per machine)

- Rust **1.88+**, Node **20+**, then `npm ci` in the repo root.
- **macOS (Koya):** enable Developer Tools for the terminal (see `CLAUDE.md` → "macOS
  XprotectService"), or first cold builds appear to hang. Accept the "allow incoming network
  connections" prompt on first bind.
- **Windows (Ildwyn):** accept the Defender Firewall prompt → **Allow on private networks**.

Launch with dial logging on so the terminal narrates dials:

```bash
# macOS (Koya)
RUST_LOG=harmony_app::iroh_dial_driver=info npm run tauri dev
```
```powershell
# Windows (Ildwyn), PowerShell
$env:RUST_LOG="harmony_app::iroh_dial_driver=info"; npm run tauri dev
```

The frontend serves at `localhost:5173`; the desktop window is what you drive.

---

## 4. Guarantee a clean first-contact

- **First run ever between these two identities:** clean by construction — skip to §5.
- **Re-runs / resets** (pick the lightest):
  - **Easiest — fresh identity on Ildwyn:** delete `%USERPROFILE%\.harmony\identity.key`
    (and `identity.enc`). New identity = new node_id = guaranteed fresh first-learn on Koya.
    B rejoins the community as a new member.
  - **Or wipe Koya's persisted peers** so its replay set is empty:
    ```bash
    rm -rf ~/Library/Application\ Support/net.zeblith.harmony   # forgets peers + membership → rejoin
    ```
    (Keep `~/.harmony` so Koya's own identity is stable.)

---

## 5. Trigger sequence

1. **Koya (A):** launch, unlock identity, **create a community C**, generate an **invite**.
   Open the **Network** panel (left sidebar → "Network") and leave it idle.
2. **Ildwyn (B):** launch, unlock identity, **redeem the invite** to join C. On join, B's
   publisher immediately publishes its `ReachabilityAnnounce` into C's CRDT.
3. That event syncs to Koya over Zenoh; Koya merges it, `resolver.update()` first-learns B,
   and the **dynamic dial** fires.

---

## 6. Observe (three independent channels)

**(a) Koya terminal** — within a few seconds of B joining:
```
ZEB-373: dialed iroh peer <hex>          # success (info)
ZEB-373: dial failed (3 attempts) for …  # failure (warn)
```

**(b) Koya Network → Dynamic dials panel** (ZEB-377) — the panel shows:
- counters: **Attempts / Succeeded / Failed / Skipped (dup)**;
- recent hits (newest first): `✓/✗  <node-id-short>  owner <owner-short>  <age>s ago`.
- Expected after one successful first-learn dial: **Attempts ≥ 1, Succeeded 1**, one `✓` hit.

**(c) Koya dev console (F12)** — raw telemetry, useful for scripting/assertions:
```js
await window.__TAURI__.core.invoke('network_health_snapshot').then(s => console.log(s.dialStatus))
// { attempts: ≥1, succeeded: 1, failed: 0, skippedDuplicate: 0, recent: [{ outcome: "succeeded", … }] }
```

**(d) Ildwyn via Playwright** (automatable; reads B's own snapshot):
```js
const dial = await page.evaluate(() =>
  window.__TAURI__.core.invoke('network_health_snapshot').then(s => s.dialStatus));
```
> Per §1, B→A is a static seed, so B's `dialStatus` may show **no** attempt for A. Treat A's
> panel as the primary assertion.

---

## 7. Prove the connection carries real traffic

`succeeded` only means the dial returned `true`. Confirm the link is real:
- In Koya's **Network** panel, Ildwyn appears as a **peer with a live RTT** and
  `connectionMode: direct` (same-LAN).
- Send a **DM** Koya ↔ Ildwyn and confirm bidirectional delivery (the
  `docs/cross-wan-validation.md` "exchange" check).

---

## 8. Edge / negative checks

1. **Dedup / no re-dial (Phase-2 boundary):** restart **Koya** (warm). Ildwyn is now persisted
   → replayed as a static seed → **no new `DialHint`**, panel attempts do not climb for
   Ildwyn. Confirms dial-once-per-node-id (re-dial deferred to ZEB-321 Phase 3).
2. **Failure path:** fresh first-learn but B unreachable (kill Ildwyn's app as it joins, or
   block its port) → Koya logs `dial failed (3 attempts)`, panel **Failed 1**. Confirms the
   bounded backoff is terminal — no infinite retry.

---

## 9. Resetting for another run

Each `(owner, node_id)` dials at most once for the life of the observer's session/persisted
state. To get another fresh first-learn: give **Ildwyn a new identity** (delete `~/.harmony`)
or **wipe Koya's app-data** (§4). A brand-new community alone is **not** enough — the resolver
key is global, not per-community.
