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
DM body is hex-encoded on `read_dm_thread`; channel presence is `id` in
`list_channels`. First contact is racy (~75–90s pkarr propagation) — **poll/retry**
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

## Artifacts on failure

Each agent collects, into a run directory it reports back:
- the node's rolling log (`<app-data>/logs/`),
- a transcript of the `api` calls it made (command + JSON result + timestamp),
- the final `get_owner_state` / `list_community_members` / `list_channels` / `list_friends` snapshots,
- the BOOT-PROBE breadcrumbs from stderr (01–10) if a node stalled at boot.

Attach both machines' artifacts to the ZEB-447 issue (or the relevant finding issue).

## Reference

- Single-machine harness + assertions (canonical): `e2e-harness/README.md`, `e2e-harness/tests/e2e_two_node.rs` (S1–S4 passing; S3/S4 un-ignored after the ZEB-462 key-bug fix).
- Spec: `docs/specs/2026-06-13-zeb-447-two-agent-e2e-suite-design.md`.

## Known co-located limitations (what the single-machine harness still can't prove)

- **ZEB-461** — 1:1 DM byte-delivery needs a DM transport (`OwnerDeviceCache`) that the Reticulum-disabled co-located harness can't populate. S2 asserts friendship + `send_dm` acceptance; byte-delivery is characterized, not asserted.
- **Cross-WAN re-peering** — S3 proves co-located restart→reconnect→catch-up, but two co-located nodes share a host; it does NOT prove re-peering between two real LANs/WANs after a restart. That is the cross-machine run's unique job (ZEB-444).

Resolved (no longer co-located limitations):

- **ZEB-462 (A)** "ongoing co-located sync never establishes / no-responder re-peering" — a NON-bug: an artifact of a harness assertion checking the wrong JSON key (`id` vs camelCase `channelId`). With the key fixed, co-located ongoing sync + offline→restart→catch-up pass (S3, proven 3/3).
- **ZEB-462 (B)** "own membership rehydrates as `Left`" — the real two-node admin-as-`Left` publish-gate durability bug was fixed (#253). The single-node "rehydrates as `Left`" symptom was the same wrong-key artifact (`id` vs `spaceId`); S4 passes.
