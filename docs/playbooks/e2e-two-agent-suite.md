# Two-agent E2E scenario suite — cross-machine run playbook (ZEB-447)

A protocol two Claude Code agents (one per machine, e.g. Ildwyn + AVALON) follow to run end-to-end Harmony scenarios **across two physical machines**, with no human driving either half. This is the cross-WAN counterpart to the single-machine `e2e-harness` crate.

## When to use this vs the single-machine harness

- **`e2e-harness/` (single machine, automated):** the day-to-day dev substrate. `cd e2e-harness && cargo nextest run --features e2e`. Spawns two real `serve` nodes on one box. It reliably validates **first-contact + join/handshake-time state** — `s1_invite_join_roster_convergence` and `s2_friend_graph_and_dm_send` pass. It does **not** validate ongoing community-state sync, 1:1 DM byte-delivery, or restart catch-up — those need two real machines (see "Known co-located limitations" below).
- **This playbook (two machines, agent-driven):** the cross-WAN proof. Run it when AVALON is up (ZEB-444) to validate the scenarios the single-machine harness can't, and to satisfy the ZEB-447 DoD ("≥3 scenarios proven in a live Ildwyn↔AVALON run with artifacts").

## Roles

- **Agent A** — machine 1 (e.g. Ildwyn). Runs one `harmony-app serve` node; drives it with `harmony-app api`.
- **Agent B** — machine 2 (e.g. AVALON). Same.

Each agent operates ONLY its own node. They coordinate turn-taking + hand off artifacts (invite URLs, friend tokens, owner ids) through the **coordination channel** below.

## Node bring-up (each agent, once)

Each machine runs a dev node under a named profile (keychain-safe file vault):

```bash
# Pick a unique profile per machine; set a passphrase file for the file vault.
export HARMONY_PASSPHRASE_FILE=$HOME/.harmony-e2e-pass   # contains any passphrase
harmony-app --profile e2e serve --api-port 7420
```

The node auto-starts and writes discovery files under its app-data dir:
`<app-data>/api/port` and `<app-data>/api/token`. The `harmony-app api` CLI reads
them automatically (scoped to `--profile e2e`).

Drive the node (single-shot RPC; camelCase JSON args; same error strings as the GUI):

```bash
harmony-app --profile e2e api mint_owner_identity                  # first boot only
harmony-app --profile e2e api get_owner_state                      # -> {ownerId, ...}
harmony-app --profile e2e api create_community '{"name":"x","isInviteOnly":true}'
harmony-app --profile e2e api --events                             # stream WS events (one JSON/line)
```

**Offline = a real process kill.** To take a node offline, kill the `serve` PID
(`kill <pid>` / `Stop-Process -Force`) — do NOT just close a window (ZEB-433).
Back online = relaunch the same `--profile e2e serve`.

## Coordination channel (dogfood the coordination instance)

Both agents are members of a shared **coordination community** on the pinned
`serve --profile coord` instance that already runs on each machine (ZEB-446). Use
it to relay artifacts + turn-taking signals as channel messages:

```bash
# Post a signal/artifact to the coord channel:
harmony-app --profile coord api post_channel_message \
  '{"communityId":"<coord-community-id>","channelId":"<coord-chan-id>","body":<bytes>}'
# Read what the other agent posted:
harmony-app --profile coord api list_channel_messages \
  '{"communityId":"<coord-community-id>","channelId":"<coord-chan-id>","limit":100}'
```

(`body` is a JSON array of UTF-8 bytes, e.g. `"READY S1"` → `[82,69,...]`.) Signals
follow a simple convention: `READY <scenario>`, `INVITE <url>`, `TOKEN <url>`,
`OWNER <ownerIdHex>`, `JOINED`, `DONE <scenario> PASS|FAIL`.

**Fallback (manual relay):** if the coord instance is unavailable, the controlling
human/agent copies each artifact between the two machines' transcripts per the
per-scenario steps. The protocol is identical; only the transport differs.

## Scenarios

Assert with the SAME predicates the Rust harness uses (so results are comparable):
roster membership status is `"joined"` (lowercase); friend status is `"active"`;
DM body is hex-encoded on `read_dm_thread`; channel presence is `channelId` in
`list_channels` (the DTO is camelCase — `id` is always absent, see the harness key
fix). First contact is racy (~75–90s pkarr propagation) — **poll/retry**
the cross-node redeem until it reports success; transient
`pkarr resolve: no relays available` is retryable (relay warm-up), not a failure.

### Scenario 1 — invite → cross-node join → roster convergence

1. **A:** `create_community` → capture `communityId`; `generate_invite '{"communityId":"…"}'` → capture invite URL; post `INVITE <url>` + `OWNER <A ownerId>`.
2. **B:** poll `connectivity_redeem_invite_iroh '{"url":"<url>"}'` until `{"status":"joined"}` (retry on `inviter_unreachable` / transient pkarr errors, ~120s). Post `JOINED` + `OWNER <B ownerId>`.
3. **A:** poll `list_community_members '{"communityId":"…"}'` until B's `ownerId` appears with `status:"joined"`.
4. **B:** poll `list_community_members` until A appears with `status:"joined"`.
5. PASS when both rosters converged. Post `DONE S1 PASS`.

### Scenario 2 — friend-add → DM picker → DM exchange

1. **A:** `generate_friend_token` → capture URL; post `TOKEN <url>` + `OWNER <A>`.
2. **B:** poll `redeem_friend_token '{"url":"<url>"}'` until it returns Ok (friend iroh handshake; ~75–90s). Post `OWNER <B>`.
3. **Both:** poll `list_friends` until the peer appears with `status:"active"`. (If A sees a pending request, `accept_friend_request '{"ownerIdHex":"<B>"}'`.) This is the ZEB-431 DM-picker friend graph.
4. **A:** `add_space '{"kind":"dm","name":"e2e","members":["<B ownerId>"]}'` → capture `spaceId`; **B:** same with A's owner. (Ids may differ until the space-invite propagates; read tolerant of either.)
5. **A:** `send_dm '{"spaceId":"…","content":<bytes>,"mimeType":"text/plain"}'`. **B:** poll `read_dm_thread '{"spaceId":"…","limit":100}'` until A's message arrives (hex-decode `body`). Reverse direction. PASS on both round-trips. Post `DONE S2 PASS`.

   > NOTE: 1:1 DM byte-delivery requires a working DM transport between the two owners (ZEB-461). It works across two real machines (Reticulum binds per-host / pkarr reachability) but NOT between two co-located nodes — which is exactly why this leg is a cross-machine scenario.

### Scenario 3 — channel created while peer offline → reconnect catch-up (ZEB-434)

1. **A + B:** both in a community (run Scenario 1 first).
2. **B:** kill the `serve` PID (hard offline). Post `OFFLINE`.
3. **A:** `create_channel '{"communityId":"…","name":"created-offline","writePower":0}'` → capture `channelId`. Post `CHANNEL <channelId>`.
4. **B:** relaunch `serve --profile e2e` (same profile/data-dir → rehydrates).
5. **B:** poll `list_channels '{"communityId":"…"}'` until `channelId` appears. PASS = B caught up the offline-created channel. Post `DONE S3 PASS`.

   > NOTE: this exercises **ongoing community-state sync** + **reconnect**. Both now work co-located — the automated `s3_offline_channel_reconnect_catchup` passes 3/3 after the ZEB-462 (B) durability fix + a harness key fix. The cross-machine run additionally proves **cross-WAN** re-peering, which the co-located harness cannot (ZEB-444).

### Scenario 5 — profile-card propagation (ZEB-341 / ZEB-464)

Proves a member's **signed profile card** (display name + status) published on one node resolves on the other — the headless counterpart to the GUI's member-card resolution, and the cross-WAN proof behind ZEB-432 (community member cards rendering as truncated hex). Card verbs were headless-exposed by ZEB-464; cards ride a **Zenoh broadcast topic keyed by owner_id**, so a subscription needs only the peer's `ownerIdHex`.

1. **A + B:** both in a community (run Scenario 1 first — this establishes the transport path; two never-met nodes peer via iroh first-contact, not ambient scouting).
2. **A:** `republish_owner_card '{"displayName":"<A-name>","statusText":"<A-status>"}'` (poll until it returns Ok — the card publisher is wired post-connect, so it can briefly return `owner card runtime not ready`). **B:** same with B's name.
3. **A:** `subscribe_member_card '{"ownerIdHex":"<B ownerId>"}'` → capture `subscriptionId`. **B:** same with A's ownerId. (Subscribe *before* the convergence puts — a Zenoh put isn't retained for a late subscriber; re-publish on each poll tick as belt-and-braces.)
4. **A:** poll `get_cached_member_card '{"subscriptionId":<A-sub>}'` until it returns a card whose `displayName` == `<B-name>`. **B:** poll until A's `<A-name>` resolves.
5. PASS when both sides resolve the peer's signed card name. `unsubscribe_member_card '{"subscriptionId":…}'` to clean up. Post `DONE S5 PASS`.

   > NOTE (co-located gap — ZEB-466): the **single-machine** harness's `s5_profile_card_propagation` does NOT converge co-located — owner-global card topics don't route between two peers connected only via a community (the community roster syncs, but card topics don't; `verify_card` is self-contained so it's a transport gap, not crypto). So the co-located harness only **characterizes** propagation; **this cross-machine run is the actual test of whether card topics route cross-WAN** (via pkarr/relay). It is also the likely substrate of ZEB-432 — if cards don't traverse here either, that bug is at the transport layer, not the frontend. If step 4 times out, capture both nodes' `RUST_LOG=...profile_card_broadcast=debug` logs for ZEB-466.

   > NOTE: the avatar (`avatarCid` on the card) resolves over the **public CAS content-fetch path** (ZEB-343/344/409/408), a separate layer — S5 asserts the signed name/status, not avatar bytes. The peer-profile broadcast verbs (`subscribe_peer_profile` / `get_cached_peer_profile` / `unsubscribe_peer_profile`, ZEB-281) are also headless-exposed by ZEB-464 for the fuller Reticulum profile, but are not asserted by this scenario.

### Scenario D2 — offline-at-create → relay deposit → recover (ZEB-487 / ZEB-483 durability)

Proves the headline DM durability: a DM created while the recipient is offline is
deposited on a community relay and delivered when the recipient returns, bootstrapping
the DM Space from the deposited invite. Three nodes: **A = sender, B = recipient
(goes offline), R = relay host** (a distinct owner; only needs to be a community
co-member). Drive with `harmony-app api`; signals on the ZEB-477 thread.

**Setup (all online):**

1. **A:** `create_community '{"name":"s6","isInviteOnly":true}'` → `communityId`; `generate_invite '{"communityId":"…"}'` → invite. Post `INVITE <url>` + `OWNER-A <hex>`.
2. **B and R:** each poll `connectivity_redeem_invite_iroh '{"url":"<url>"}'` until `{"status":"joined"}`. Post `JOINED-B` / `JOINED-R` + their `OWNER-*`.
3. **A:** `generate_friend_token` → post `FTOKEN <url>`. **B:** poll `redeem_friend_token '{"url":"<url>"}'` until Ok; both poll `list_friends` until `status:"active"` (A `accept_friend_request '{"ownerIdHex":"<B>"}'` if pending). This populates B's device cache with A. **Do NOT send a DM yet** — the DM Space must not exist on B.
4. **R:** `set_community_relay_opt_in '{"communityIdHex":"<communityId>","optedIn":true}'`; confirm `get_community_relay_status '{"communityIdHex":"<communityId>"}'` → `true`. Post `RELAY-READY`.

**Run:**

5. **B:** kill the `serve` PID (real offline). Post `OFFLINE`.
6. **A:** `add_space '{"kind":"dm","name":"s6-dm","members":["<B owner>"]}'` → `spaceId`; `send_dm '{"spaceId":"…","content":<bytes>,"mimeType":"text/plain"}'`. Post `SENT`.
7. **R:** poll `get_relay_held '{"communityIdHex":"<communityId>"}'` until an entry shows `senderOwnerHex == A` and `recipientOwnerHex == B`. (Deposit fires only after ~2 no-ack windows — be patient.) Post `HELD`.
8. **B:** relaunch the same `--profile` (rehydrates → auto-pulls + recovers).
9. **B:** poll `read_dm_thread '{"spaceId":"<A spaceId>","limit":100}'` until A's plaintext appears (hex-decode `body`). Post `RECV`.
10. **R:** poll `get_relay_held '{"communityIdHex":"<communityId>"}'` until B's entry is gone. Post `CLEARED`.
11. **PASS** = `HELD` (while B offline) ∧ `RECV` (after reconnect) ∧ `CLEARED`. Post `DONE D2 PASS`.

**Provenance is by construction:** B is killed before the send, so the live tunnel
cannot carry the message; HELD-while-offline + RECV-after-reconnect + CLEARED proves
it travelled via the relay deposit. If `get_relay_held` never shows the entry, capture
R's `<app-data>/profiles/xwan/logs/` + A's outbox logs and file a finding (likely a
relay resolve/dial issue cross-NAT) — do not call it PASS.

### Scenario D3 — offline-at-create → butler deposit → recover (ZEB-489 / ZEB-483 durability)

Same durability proof as D2, but the deposit lands on the recipient's **own butler**
device (ZEB-418) instead of a community sealed-relay. **Roles:** A = **Ildwyn**
(sender, 1 device). R = **AVALON** (recipient), running **two local profiles** in one
fleet — primary `P` + butler `B2`. (Baseline keeps both recipient devices on AVALON via
local pairing, sidestepping cross-WAN pairing. Optional 3-machine variant: run `B2` on
Koya.) HELD ∧ RECV ∧ CLEARED by construction: P is killed before the send, so the tunnel
cannot carry it; the deposit lands on B2, and P recovers it on reconnect.

**Setup (AVALON):**
1. Mint `P`: `harmony-app --profile p serve --api-port 7421` then `... api mint_owner_identity`; `get_owner_state` → `OWNER-P`.
2. Pair `B2` into P's fleet (second profile, second `serve`): drive the ZEB-446 pairing RPCs — `start_inviter_pairing` on P / `start_joiner_pairing` on B2, `select_pairing_peer`, `confirm_pairing_sas` (SAS match), poll `get_pairing_state` until both report enrolled. `B2` is now a second enrolled device under P's owner.
3. On P: `set_butler_pin '{"deviceId":"<B2 device id>"}'` (B2's 64-hex enrolled key, from P's device view). `get_butler_pin` → confirms `pinnedDeviceId == <B2>`.

**Run:**
4. **A (Ildwyn):** friend P (`generate_friend_token` → P `redeem_friend_token`; both `list_friends` → active). `add_space` (DM) with P → `SPACE`.
5. **Kill P** (real PID kill — `kill <pid>` / `Stop-Process -Force`, never just close a window).
6. **A:** `send_dm` to P while P is offline → the deposit rung fires after `DEPOSIT_NOACK_WINDOWS=2`; it lands on **B2** (P's online butler).
7. **B2:** poll `get_butler_held` until the entry appears — **HELD** (`senderOwnerHex == OWNER-A`, `spaceIdHex`/`messageCidHex` present, `ingestedByDevices` does NOT yet contain P).
8. **Relaunch P** (same `--profile p serve`). P auto-recovers (startup inbox sweep + fleet merge → `apply_deposited_invite` bootstrap).
9. **B2:** `get_butler_held` now shows `ingestedByDevices` containing P's device id (or the entry GC'd) — **CLEARED**. **P:** `read_dm_thread` shows A's plaintext — **RECV**. Post `DONE D3 PASS`.

**PASS** = `HELD` (on B2 while P offline) ∧ `RECV` (on P after reconnect) ∧ `CLEARED` (on B2 after recovery). If `get_butler_held` never shows the entry, capture both AVALON profiles' logs + A's outbox logs and file a finding under ZEB-489 / ZEB-321 — do not call it PASS. Bring-up/discovery run, not a regression gate.

## Artifacts on failure

Each agent collects, into a run directory it reports back:
- the node's rolling log (`<app-data>/logs/`),
- a transcript of the `api` calls it made (command + JSON result + timestamp),
- the final `get_owner_state` / `list_community_members` / `list_channels` / `list_friends` snapshots,
- the BOOT-PROBE breadcrumbs from stderr (01–10) if a node stalled at boot.

Attach both machines' artifacts to the ZEB-447 issue (or the relevant finding issue).

## Reference

- Single-machine harness + assertions (canonical): `e2e-harness/README.md`, `e2e-harness/tests/e2e_two_node.rs` (S1–S4 passing; S3/S4 un-ignored after the ZEB-462 key-bug fix; S5 card propagation added by ZEB-464).
- Spec: `docs/specs/2026-06-13-zeb-447-two-agent-e2e-suite-design.md`.

## Known co-located limitations (what the single-machine harness still can't prove)

- **ZEB-461** — 1:1 DM byte-delivery needs a DM transport (`OwnerDeviceCache`) that the Reticulum-disabled co-located harness can't populate. S2 asserts friendship + `send_dm` acceptance; byte-delivery is characterized, not asserted.
- **Cross-WAN re-peering** — S3 proves co-located restart→reconnect→catch-up, but two co-located nodes share a host; it does NOT prove re-peering between two real LANs/WANs after a restart. That is the cross-machine run's unique job (ZEB-444).

Resolved (no longer co-located limitations):

- **ZEB-462 (A)** "ongoing co-located sync never establishes / no-responder re-peering" — a NON-bug: an artifact of a harness assertion checking the wrong JSON key (`id` vs camelCase `channelId`). With the key fixed, co-located ongoing sync + offline→restart→catch-up pass (S3, proven 3/3).
- **ZEB-462 (B)** "own membership rehydrates as `Left`" — the real two-node admin-as-`Left` publish-gate durability bug was fixed (#253). The single-node "rehydrates as `Left`" symptom was the same wrong-key artifact (`id` vs `spaceId`); S4 passes.
