# e2e-harness (ZEB-447)

Standalone harness that spawns two real `harmony-app serve` nodes under named
profiles and drives them over the live HTTP/WS API.

## Run

```bash
# 1. Build the binary the harness drives:
cd src-tauri && cargo build --bin harmony-app && cd ..

# 2. Run the scenario suite (slow, real transport):
cd e2e-harness && cargo nextest run --features e2e
```

Set `HARMONY_APP_BIN=/path/to/harmony-app` to override binary discovery.
Set `HARMONY_E2E_KEEP=1` to retain run artifacts on success
(`e2e-harness/target/e2e-runs/<scenario>-<runid>/`).

Run the real-transport scenarios serially (avoid two scenarios contending on
transport/discovery at once):

```bash
cargo nextest run --features e2e --test-threads 1
# or, with plain cargo test:
cargo test --features e2e -- --test-threads 1
```

First contact is racy + relay-dependent (~75–90s pkarr propagation; relays
warm up ~1–2 min after a node boots), so scenarios poll/retry — allow a few
minutes of wall-clock.

## Scenarios

Two-sided scenarios live in `tests/e2e_two_node.rs`. Status on a **single
machine** (two co-located nodes):

| Scenario | What it proves | Status |
|---|---|---|
| `s1_invite_join_roster_convergence` | invite → iroh first-contact join → roster converges both ways | ✅ pass |
| `s2_friend_graph_and_dm_send` | friend-token iroh handshake → friendship `active` both ways (ZEB-431 DM-picker graph) + `send_dm` accepted. (1:1 DM byte-**delivery** is characterized, not asserted here — see the hard-assert row.) | ✅ pass |
| `s2_dm_delivery_over_tunnel_hard_assert` | live 1:1 DM byte-delivery over the PQ tunnel (ZEB-473): Alice→Bob DM MUST fire `dm-received` + land in Bob's thread. | ⏸️ `#[ignore]` — tunnel delivers the signed CidNotify co-located, but the DM Space (random per-owner `SpaceId` + `content_key`) has no cross-owner carrier (DmInvite rode Reticulum, removed in harmony#280; re-wiring onto the tunnel is a later move) → receiver rejects every tunnel DM at `verify_cidnotify_admission: SpaceNotFound`. Un-ignore once the DM-Space invite carrier lands. |
| `s3_offline_channel_reconnect_catchup` | channel created while peer offline → reconnect catch-up (ZEB-434) | ✅ pass |
| `s4_restart_durability` | single-node community survives a restart (ZEB-393) | ✅ pass |
| `s8_channel_multi_member_message_exchange` | two members in one community both post to a shared channel → each reads back the OTHER's remote-authored message (`channel-message-received` WS event + `list_channel_messages`), hard-asserted (ZEB-529) | ✅ pass |
| `s9_three_member_channel_convergence` | THREE members join one invite-only community (iroh first-contact) → all rosters converge to {a,b,c}, both joiners converge on a shared channel, every node reads back all three members' messages, and the 3rd member gets the founder's message in real-time; hard-asserted (ZEB-530). The all-online 3rd-member case converges co-located — narrows ZEB-526 to the offline/3b case. | ✅ pass |
| `s11_recovery_veto_path` | admin-recovery liveness path (ZEB-715): founder configures designates {alice,bob} R=2 → bob initiates recovery of alice → cosign to threshold (timeLocked replicates both ways) → alice vetoes → both replicas resolve `vetoed`-by-alice, membership unchanged. All event-driven — no time control. | ✅ pass |
| `s12_recovery_time_locked_execution` | admin-recovery execution path (ZEB-715): same dance to timeLocked (replay deadlines equal across replicas), then the read-side `nowMs` as-of override on `get_recovery_state` observes derived execution — new_admin's `selfPower` flips to 100, the named lost_admin's read is REFUSED by the Joined-caller gate (the kick, behaviorally), `rotationEligibleAtMs` = deadline + 48h — while real-clock reads stay `timeLocked` (no leak). | ✅ pass |
| `s_vines_publish_feed_view_reshare` | Vines round-trip over the MESH path: a community join (invite-only, iroh first-contact) establishes the peer link vines ride on, then publish → feed → view → reshare (with origin attribution) round-trips A→B→A over real Zenoh wildcard pub/sub. Needs a community preamble because vines carry no peer-acquisition step of their own (ZEB-811) — without one, co-located nodes only "converge" via LAN multicast, a false positive that doesn't hold cross-WAN. | ✅ pass |
| `s_vines_follow_only` | ZEB-811 relay path: two nodes with NO relationship of any kind (no community, no friendship, no LAN scouting) — the exact gap `s_vines_publish_feed_view_reshare` needed a community join to close. Bob follows Alice by address only; Alice's own device is her v1 relay, discovered via her public pkarr `vines` slot. Descriptors and video both arrive over `harmony/vine-relay/v1`, then view + reshare round-trip on Bob's pulled copy (v1 has no reverse channel — Alice never sees the reshare). | ✅ pass |

The single-machine harness validates **first-contact + join/handshake-time state**
(S1, S2), **single-node restart durability** (S4), and **co-located ongoing
community-state sync + offline→restart→catch-up** (S3), plus **multi-member
channel message exchange** (S8) and **3-member convergence** (S9). S3/S4 were previously
`#[ignore]`'d "blocked by ZEB-462", but that was a harness bug — the assertions
checked `c.get("id")` while the DTOs are camelCase (`channelId` / `spaceId`), so
they always timed out and *looked* like a sync failure. With the keys corrected
and the ZEB-462 (B) membership-CRDT durability fix on main (#253), both pass
reliably (S3 proven 3/3). The one remaining co-located gap is **1:1 DM
byte-delivery**: after ZEB-473 (DM-over-iroh Move 1a) the PQ tunnel establishes +
delivers the signed CidNotify packet between two co-located nodes, but the receiver
rejects it at `verify_cidnotify_admission: SpaceNotFound` — the DM Space (random
per-owner `SpaceId` + per-Space `content_key`) currently has no cross-owner carrier
(its DmInvite rode the Reticulum unicast removed in harmony#280; re-wiring it onto
the tunnel is a later move). The hard-assert test
`s2_dm_delivery_over_tunnel_hard_assert` is kept as a real assertion and `#[ignore]`'d
with that exact diagnosis; it flips green for free once the DM-Space carrier lands.
**Cross-WAN** reachability
(two real machines / LANs) is still only exercised by the cross-machine playbook
(`docs/playbooks/e2e-two-agent-suite.md`, ZEB-444) — a co-located pass does not
prove cross-WAN re-peering.

## CI

This suite is its own deliberately-invoked CI job, **not** run on every push (it
spawns real binaries + touches the network, and is excluded from the per-task
`--lib` gate and from harmony-app's `--all-targets`). The job: (1) builds
`harmony-app`, (2) runs `cargo nextest run --features e2e --test-threads 1` from
`e2e-harness/`, (3) uploads `target/e2e-runs/` on failure.
