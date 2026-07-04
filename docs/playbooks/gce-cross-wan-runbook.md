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

```
scripts/gce-xwan/up.sh            # create-or-start harmony-xwan-1 (~40s start, ~1min create)
scripts/gce-xwan/provision.sh     # idempotent: deps + rsync + build + profile (cold build = long pole)
scripts/gce-xwan/mode.sh open     # firewall mode 1: public-host easy case
scripts/gce-xwan/run-tests.sh --mode open --test all       # T1 + T2
scripts/gce-xwan/mode.sh filtered # firewall mode 2: hole-punch must actually work
scripts/gce-xwan/run-tests.sh --mode filtered --test t2
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
| _pending first session_ | | | | | | | |

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
