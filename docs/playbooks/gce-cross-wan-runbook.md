# GCE cross-WAN test node — runbook (ZEB-635)

Operates the self-driven cross-WAN test node from `scripts/gce-xwan/`. Design and
rationale live in the plan (`docs/plans/2026-07-04-gce-cross-wan-test-node.md`,
ZEB-522); this is the hands-on session guide, format modeled on
`docs/playbooks/e2e-two-agent-suite.md`.

**What this proves that nothing else can:** every fleet host shares one egress IP
(NAT hairpin), so direct-path hole-punch has never been tested across genuinely
different WANs. The GCE VM is a node on Google's network with its own public IP —
the missing far end.

## Prerequisites (once per operator machine)

- `gcloud` authed with access to project `zeblith-minecraft` (`gcloud config list`).
- ssh agent holding a GitHub-authorized key (`ssh-add -l`) — the VM build fetches
  the private `zeblithic/harmony.git` cargo deps through agent forwarding; no
  repo credential rests on the VM.
- `jq`, `rsync`, `shellcheck` (dev) locally.
- A **fresh local release binary** (the local partner node):
  `cd src-tauri && cargo build --locked --release --bin harmony-app`
- ⚠️ The standing fleet node (profile `fleet-koya`) is on iroh 0.98 wire until the
  flag-day rebuild — these scripts never touch it (local partner profile is
  `xwan-local`), and it can NOT pair with the GCE node.

## Session flow

```bash
scripts/gce-xwan/up.sh            # create-or-start harmony-xwan-1 (~40s start, ~1min create)
scripts/gce-xwan/provision.sh     # idempotent: deps + rsync + build + profile (cold build = long pole)
scripts/gce-xwan/mode.sh open     # firewall mode 1: public-host easy case
scripts/gce-xwan/run-tests.sh --mode open --test all       # T1 + T2
scripts/gce-xwan/mode.sh filtered # firewall mode 2: hole-punch must actually work
scripts/gce-xwan/run-tests.sh --mode filtered --test t2
scripts/gce-xwan/mode.sh open     # restore: run-tests.sh asserts the live mode matches --mode
scripts/gce-xwan/run-tests.sh --mode open --test d3     # butler deposit→recover (ZEB-689)
scripts/gce-xwan/down.sh          # stop (disk survives); --delete when the wave ends
```

Re-running `provision.sh` after a local source change is cheap: rsync + incremental
rebuild (minutes). The 50 GB disk holding the target dir is the expensive-to-recreate
asset — hence stop-don't-delete within a wave.

## The two firewall modes (why every T2 result names its mode)

| Mode | Rule `harmony-xwan-udp` | VM behaves like | What a PASS means |
|---|---|---|---|
| `open` | present (udp:1024-65535 ingress) | public-IP host | the pure cross-WAN path works; no filtering variable |
| `filtered` | absent | endpoint behind address/port-dependent filtering | relay-coordinated hole-punch genuinely works |

iroh binds an **ephemeral UDP port** (no pin exists — `iroh_endpoint.rs`,
`Endpoint::builder(presets::N0)…bind()`), which is why open mode allows a range
rather than one port, and why the rule must not outlive a session (`down.sh`
removes it).

Scope honesty: filtered-mode GCE approximates filtering but does no port
rewriting; the consumer-NAT half of the problem lives on the local (home NAT)
side of the pair. Symmetric-NAT-both-ends stays out of scope (needs a real
second non-cloud network).

## Tests

- **T1 — first-contact + community** (ZEB-330's missing evidence): local
  `create_community` + `generate_invite` → GCE `connectivity_redeem_invite_iroh`
  (retries absorb the ~75–90 s pkarr propagation window) → `list_community_members`
  convergence on both sides.
- **T2 — friend + DM + direct path** (the headline): GCE `generate_friend_token` →
  local redeem → DM both directions → **assert some peer reports
  `connectionMode: "direct"`** (serde camelCase value — lowercase) in
  `network_health_snapshot` on BOTH sides. DMs delivering relay-only while the
  direct assert fails is an expected, *reportable* outcome — it means traversal
  failed for that mode.
- **T3 — restart + backfill**: graceful remote `/v1/shutdown` → local posts channel
  messages while the GCE node is down → restart → catch-up assert (cross-WAN
  anti-entropy on a real WAN). Run via `--test t3` (re-runs T1 first for a fresh
  community).
- **D3 — butler deposit→recover** (ZEB-477 Scenario D3 / ZEB-689): the
  authoritative proof of the butler-rung deposit dial, which the co-located
  s7 harness cannot establish. Three nodes: GCE **A** (sender) + two LOCAL
  nodes — **P** (recipient primary) and **B2** (butler, SAS-paired into P's
  fleet; pairing rides Zenoh `harmony/pairing/v2/lan/**` so the fleet half is
  LAN-local by necessity). Flow: pair → friend A↔P → shared community
  (announce replication path) → P persists A's announce (the
  relaunch-replay dial seed) → relaunch both locals → pin B2 → A holds P's
  POST-PIN announce (`announcedAtMs` >= pin floor — the B2-bearing set) →
  SIGKILL P → A sends a DM → **HELD**
  (B2 receives the `harmony/butler-deposit/v1` dial from GCE — the headline)
  → restart P → **RECV** (P recovers the deposit) → **CLEARED** (B2 records
  the ingest). Every boundary hard-fails (no characterization fallbacks — this
  scenario IS the verdict): a HELD timeout means only that **no HELD record
  was observed** — transport failure vs authorization reject is
  indistinguishable at INFO, because the acceptor **rejects without wire
  detail by design** (no oracle). Rerun the butler with
  `RUST_LOG=harmony_app=debug` to classify before escalating (the
  2026-07-16 lesson — dial fine, frame delivered, roster missing, ZEB-702).
  RECV/CLEARED timeouts mean the recover half broke on a real WAN. Local
  profiles `xwan-d3-p`/`xwan-d3-b2` are wiped fresh each run (disposable
  identities). Run via `--test d3` in both firewall modes. **ZEB-702/705
  status:** the ZEB-702 fix (boot-seed dial view + transport up-edge
  republish) merged 2026-07-17 and its mechanisms were PROVEN on the same
  day's live re-run — but HELD stayed red: both roster publishes reached B2
  ~2 s after its relaunch boot and were dropped on an un-retried content-blob
  fetch (queryable-declaration race on the ~1 s-old link); P, the only blob
  holder, was SIGKILLed 3 s later → ZEB-705. That fix adds a bounded fetch
  retry (3×2 s re-injection) plus a **D3 ROSTER barrier** (B2's friend list
  must carry A before P is killed — separates "roster converges while P is
  alive" from "deposit dial + authorization work"). On a HELD or ROSTER
  timeout the script now snapshots B2's `network_health_snapshot` into the
  artifacts: read `butlerDeposits.rejectedUnauthorized` FIRST (counts ONLY
  roster misses — admitted-sender scope failures land in `rejectedOther`). A
  climbing counter means the roster still isn't converging (confirm via the
  butler's debug log — a genuinely unknown sender also lands here); zero
  rejects + no HELD points transport-side, but is not by itself proof — the
  debug log remains the classifier of record.

Node snapshots and logs land in `~/.cache/gce-xwan-logs/<timestamp>/`.

## Identity & credential hygiene

- VM identities are disposable: passphrase file is VM-local random, minted at
  provision; a deleted VM takes its identity with it. Mint **fresh invites after
  any identity rotation** (stale-ghost-key lesson, ZEB-477).
- If the PAT fallback was ever used (agent forwarding broken): `down.sh` deletes
  the file, but **server-side revocation is manual** — GitHub → Settings →
  Developer settings → revoke the fine-grained token. The down-checklist is not
  complete without it.

## Teardown checklist (end of every session)

1. `scripts/gce-xwan/down.sh` (or `--delete` at wave end) — this also removes the
   open-mode firewall rule and prints the final RUNNING-instances check.
2. Confirm the check shows only `harmony-relay` (production pkarr rendezvous —
   always RUNNING, never touched).
3. If a PAT was used: revoke server-side (above).

## Measured results

_Filled from live sessions; each row names the commit driven and the firewall mode._

| Date (UTC) | Commit | Cold build | T1 | T2 open | T2 filtered | T3 | Session cost |
|---|---|---|---|---|---|---|---|
| 2026-07-04 | `9d12a9a4` (main) | **24m20s** (e2-standard-4, peak RSS 7.1 GB) | **PASS** both modes, attempt 1 each | **PASS + `direct` both sides** | **PASS + `direct` both sides** | **PASS** (catch-up ≈60s post-restart) | ≈$0.40 (VM up ~2.5 h) |

**2026-07-17 D3 session — open mode only** (`e1bcb92e` + this branch, VM
recreated from scratch, provision 28m55s; filtered mode deliberately not run —
known-red until ZEB-702, so a filtered result would not be attributable to the
mode): every setup barrier PASSED first-attempt cross-WAN — friend redeem
attempt 1, community join attempt 1, post-pin announce replicated GCE-ward in
38 s (the honest session-re-establishment signal; the earlier existence-only
Phase 4.5 pass could have been satisfied by A's stale pre-relaunch record —
the barrier now requires a strictly-newer `announcedAtMs`). **HELD FAIL,
root-caused live**: the deposit dial A→B2 (GCE → home-NAT,
dial-by-`{endpoint_id, home_relay}`) **established and delivered the frame** —
so `MultipathNotNegotiated` was not the cause of THIS session's failure (that
noise came from deposit dials to the killed P's own butler-set entry; the
co-located establishment gap remains a co-located-only observation). B2's
acceptor rejected every deposit: "sender is not authorized (not an active
friend or co-member)" — a cert-only paired butler never receives
`OwnerState.friend_graph` (no same-owner CRDT channel exists for a
community-less sibling; measured: 300 s bilateral-alive, roster still empty).
Filed as **ZEB-702** (High). Also found: `dm_outbox` pending entries do not
survive a graceful restart. Session cost ≈$0.50 (VM up ~1.5 h).

**2026-07-17 D3 post-ZEB-702 re-run — open mode** (`999c5f3d`, warm disk,
provision 12m incremental): every setup barrier again passed (friend attempt
1, community attempt 2, post-pin announce replicated in ~3 s). **HELD FAIL,
counter-classified in one log line**: the ZEB-702 epoch-republish DELIVERED
P's roster roots to B2 twice at +2 s after B2's relaunch boot, and the
acceptor's new WARN fired with `rejected_unauthorized=1` — but both publishes
were dropped at the content fetch (`no successful reply` with zero
reply-error warns = the get() raced P's queryable-declaration propagation on
the ~1 s-old LAN link), and P was SIGKILLed 3 s later, leaving no holder of
the blob. Filed as **ZEB-705**; its fix (bounded fetch retry + ROSTER
barrier) makes this window deterministic. Session cost ≈$0.15 (VM up ~30 min,
stop-not-delete). Artifacts: `~/.cache/gce-xwan-logs/20260717-101952/`.

**2026-08-10 — 0.2.5 RC cross-WAN validation** (`c438ca3e`, VM recreated from scratch
after a prior full delete; cold build **28m09s**). Purpose: prove the flows 0.2.4 shipped
broken work cross-WAN before cutting 0.2.5. **T1 PASS** (first-contact attempt 1). **T2:
DMs both directions PASS; the both-sides-`direct` assertion FAILED** — root-caused live as
a **snapshot-timing artifact, not a transport failure**: the assert snapshots ~3 min after
the last DM, by which point the home-NAT side's peer entry has idle-evicted (empty `peers`
array) while GCE still held Koya `direct`. Re-probing the peer table **during active
traffic** showed `direct` in open (8/8) and **filtered (5/8, 0 relay — zero-ingress ⇒
genuine hole-punch)**. Union of runs: each direction achieved `direct`; the connection is
**direct-or-idle, never relay-only when connected**. Real change vs 2026-07-04 (`9d12a9a4`,
which held both sides `direct` simultaneously): **peer entries evict faster when idle** —
filed non-blocking; recommend the T2 assert snapshot during active traffic (or add a
keepalive). Hand-driven V1–V4 (fresh never-toggled local vs live remote) all PASS both
modes (see `docs/playbooks/0.2.5-fleet-validation.md`). Also: IPv6 QADv6 `No route to host`
log spam on IPv4-only Koya (benign; log-cleanliness). VM stopped (disk preserved). Session
cost ≈$0.30.

The 2026-07-04 session is the **first proven cross-WAN direct-path NAT traversal**
for Harmony: `connectionMode: "direct"` on both a home-NAT node and the GCE node,
in open mode AND in filtered mode (relay-coordinated hole-punch with zero ingress
rules). It also proved distinct-WAN first-contact (T1, the ZEB-330 evidence) and
restart backfill over a real WAN (T3). Node snapshots:
`~/.cache/gce-xwan-logs/20260704-{102353,102710,104929}/` on Koya.

## Troubleshooting

- **`provision.sh` build fails fetching `harmony.git`** → `ssh-add -l` locally;
  the agent must hold a key with zeblithic access. Fallback: fine-grained
  read-only PAT for `zeblithic/harmony` on the VM (see plan §2 step 4) — then the
  teardown revocation step is mandatory.
- **`run-tests.sh` dies with "firewall mode is X but --mode Y"** → deliberate;
  run `mode.sh Y` first. Results must be attributable to a mode.
- **T1 redeem loops on `inviter_unreachable`** → normal for the first ~90 s
  (pkarr propagation + relay warm-up). Persisting past 5 min: check the local
  node's pkarr publish (`network_health_snapshot` → `relays[].lastOutcome`).
- **api CLI exit 2 vs 1**: 2 = server not reachable (port/token discovery or
  connection), 1 = server-side error with a real message. `wait_api` in
  `run-tests.sh` keys on exactly this distinction.
- **VM ssh works but `gssh` hangs** → BatchMode is set everywhere; a hang usually
  means gcloud is re-generating ssh keys — run any `gcloud compute ssh` command
  interactively once, then retry.

### Hard-won timing/shape rules (all found live, 2026-07-04 — the scripts encode them)

- **Never fire the first redeem early.** One attempt before the inviter's
  current-process record is on the relay triggers a remote-side dial backoff
  that outlives a 300 s same-URL retry loop. The script gates on a **fresh**
  identity publish (`identityLastPublishMs >= script start` — the value
  PERSISTS across restarts, so a null-check passes on stale data), settles
  90 s, then mints a fresh invite per attempt. Settled first attempts joined
  instantly, every time (5/5 across manual probes + scripted runs).
- **Publish timing:** boot publish ≈15–90 s; a RUNTIME `discoverable` enable
  waits for the publisher tick — 4m49s observed. Prefer booting with
  discoverable already persisted.
- **gcloud ssh stderr blinds jq.** `2>&1` capture interleaves "Existing host
  keys…" chatter with the JSON — a JOINED result was invisible to a bare
  `jq` parse. Extract the JSON line (`grep -E '^\{' | tail -1`) first.
- **Payload shapes** (verified against `e2e-harness/src/driver.rs`): minting
  verbs return **bare JSON strings**; DM/channel payloads go up as **byte
  arrays**; DM reads come back **hex** (`body`), channel reads come back as
  **byte-number arrays** — hex-matching a channel body "would silently never
  match" (the harness's own words).
- **Remote process control over ssh:** `source env; nohup <bin> serve >log
  2>&1 </dev/null & disown; exit 0` — with `&&` instead of `;` the `&`
  backgrounds the whole list and ssh blocks on the serve process; without
  `</dev/null` stdin pins the channel. And never `pkill -f "harmony-app
  serve"` over ssh — the pattern matches the invoking shell's own cmdline;
  use `pkill -f "[h]armony-app serve"`.
