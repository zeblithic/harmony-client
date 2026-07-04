# GCE cross-WAN test node — plan (ZEB-522)

**Goal:** an automatable, fully-controlled test node on a genuinely different WAN (a GCE VM),
driven end-to-end from Koya with no human tester, to close the one test surface every fleet
validation so far has missed: real separate-WAN NAT traversal.

**Scope:** this document is the ZEB-522 deliverable — the plan. Implementation (the scripts +
runbook) is a follow-up ticket once this is reviewed. Provenance: ZEB-522 (Jake's 2026-06-21
call: a few hours of GCE spend is a reasonable trade for a self-driven cross-WAN node).

**Validated against:** `main` @ `b89c765c` (2026-07-04). The `path:line` citations below are
secondary hints anchored to that commit; symbol names are the stable reference.

**Why:** Ildwyn + AVALON share one egress IP (165.162.82.51), so every "cross-WAN" run to date
is NAT-hairpin — direct-path hole-punch reflects through the same NAT and `ValidationFailed`s,
and the relay path masks it. Signed-TTL freshness and store-and-forward durability are proven;
distinct-WAN direct-path traversal is not.

---

## 0. What changed since the ticket was written (2026-06-21 → 2026-07-04)

The ticket's constraints section is three weeks stale in our favor:

1. **The headless gaps are closed.** ZEB-512 and ZEB-520 (both Done 2026-06-23) added the two
   missing verbs to the headless `api` v1 allowlist:
   `connectivity_set_identity_discoverable` / `connectivity_get_identity_discoverable`
   (`src-tauri/src/api/rpc.rs:839`, impl `lib.rs:46790`) and
   `connectivity_get_my_identity_pub_hex` (impl `lib.rs:50910`). The "file-seed + bounce dance"
   workaround the ticket describes is no longer required — **fully unattended runs are
   possible today**. (Pre-seeding `connectivity-settings.json` remains a useful optimization;
   see §2 step 6.)
2. **ZEB-504 and ZEB-521 are closed.** DM delivery now works cross-machine via the
   tunnel-relay + deposit double-cover, and friend contacts carry `home_relay_url` (PR #352).
   What those tickets *left* unproven — and what this node exists to prove — is the
   **direct path**: real distinct-WAN hole-punch producing a validated non-relay connection.
3. **iroh 0.98→1.0 flag-day interplay.** A GCE node built from `main` speaks iroh 1.0 wire and
   cannot interop with the legacy 0.98 fleet nodes until the coordinated rebuild. Its partner
   is therefore a **fresh local build on Koya**, not the standing fleet profiles. Upside: the
   GCE node doubles as the off-LAN validation peer for the flag-day rebuild itself.
4. **We already have GCP footprint.** Project `zeblith-minecraft`, gcloud SDK authed as
   zeblith on Koya, and the `harmony-relay` VM (e2-micro, us-west1-b, 34.168.78.132) serving
   `pkarr.q8.fyi`. The test node is a **separate VM** — `harmony-relay` is production
   rendezvous infrastructure and must not be touched. Useful precedents carry over: the
   firewall-rule pattern (`harmony-relay-pkarr-dht`), and the lesson that e2-micro (1 GB)
   cannot build Rust — pkarr-relay needed a throwaway e2-medium; harmony-app needs more.

---

## 1. VM shape & cost

- **Instance:** `e2-standard-4` (4 vCPU / 16 GB), on-demand, **us-west1**. One shape for both
  build and run — harmony-app links the full Tauri/WebKit stack (§2), which rules out the
  micro/small tiers for building, and a 16 GB headroom makes `cargo build` + link reliable.
  Runtime alone would fit e2-small, but a two-shape scheme (build big, run small) adds image
  plumbing for pennies of savings at our duty cycle. Region co-located with `pkarr.q8.fyi` is
  fine: the property we need is a distinct WAN from Koya's LAN, not distance from our relay.
- **Disk:** 50 GB pd-balanced. The cargo target dir for this workspace is tens of GB; 50 GB
  gives one-build headroom. The disk is the expensive-to-recreate asset (cold build), which
  drives the teardown discipline below.
- **Cost (approximate, verify at impl time):** e2-standard-4 on-demand ≈ $0.13–0.15/hr;
  50 GB pd-balanced ≈ $5/mo (≈ $0.007/hr); a 4-hour validation session ≈ **$0.60–0.70**.
  Spot (~70% off) is tempting but preemption mid-choreography wastes more attention than it
  saves — use on-demand for test sessions; Spot is acceptable for the first cold build only.
- **Ephemeral external IP** (default): each `start` gets a fresh public IP via GCE's 1:1 NAT —
  the VM sees its internal 10.x address; the world sees the external IP. This is a genuinely
  different public IP and network path from the fleet's shared egress → the hairpin problem
  cannot occur. No static-IP reservation needed (costs money while stopped; freshness of IP is
  even mildly useful for pkarr-record-update realism).
- **Teardown discipline:** *stop, don't delete* between sessions within an active validation
  wave (restart ≈ 40 s, cost drops to disk-only ≈ $5/mo); **delete VM + disk when the wave
  ends**. Backstops: a GCP budget alert on the project, and the runbook's down-checklist ends
  with `gcloud compute instances list` to confirm nothing is left running.

**Confirmation the topology is right:** GCE external IP is a 1:1 static binding (not a shared
port-rewriting NAT), so with the firewall in "open mode" (§3) the VM behaves like a public-IP
host — the clean far end for validating the path itself. The consumer-NAT hard cases live on
*Koya's* side of the pair, which is exactly the side we care about exercising.

---

## 2. Provisioning (scripted bring-up, driver = Koya)

Implementation follow-up delivers these as `scripts/gce-xwan/{up,provision,run,down}.sh`; the
plan fixes the shape:

1. **Create:** `gcloud compute instances create harmony-xwan-1` — `ubuntu-2404-lts-amd64`,
   `e2-standard-4`, 50 GB pd-balanced, us-west1-b, default VPC. Name deliberately distinct
   from `harmony-relay`.
2. **System deps:** `build-essential curl pkg-config` plus the Tauri Linux set that CI already
   pins (`.github/actions/install-linux-tauri-deps/action.yml:47-53`): `libgtk-3-dev`,
   `libwebkit2gtk-4.1-dev`, `libayatana-appindicator3-dev`, `librsvg2-dev`, `libssl-dev`,
   `libsoup-3.0-dev`, `libjavascriptcoregtk-4.1-dev`. **There is no headless-only build** —
   `harmony-app` is a single binary with no GUI-off feature flag, so the WebKit/GTK libs must
   be present to build and link even though `serve` never opens a window (no xvfb needed at
   runtime). ZEB-519's `/STACK` linker-arg hazard is Windows-only — N/A on Linux, noted per
   the ticket.
3. **Toolchain:** rustup with default toolchain none; the repo's `rust-toolchain.toml` pins
   1.94.1 and auto-installs on first cargo invocation. `cargo-nextest` not required (we run
   the node, not the test suite).
4. **Source transfer — no long-lived repo credentials on the VM.** Rsync the working tree from
   Koya over `gcloud compute ssh` (excluding `target/`, `node_modules/`; the vendored
   `vendor/zenoh-link` + `vendor/netdev` patches ride along — the netdev patch is
   macOS-cfg'd, inert on Linux). The **private git deps** (`zeblithic/harmony.git`, nine
   pinned-rev crates in `src-tauri/Cargo.toml:103-120`) are fetched by cargo at build time:
   run the build inside `gcloud compute ssh --ssh-flag="-A -o BatchMode=yes"` (agent
   forwarding) with cargo's `net.git-fetch-with-cli = true`. Precisely stated: keys never
   touch VM disk, but the forwarded agent socket IS usable by the VM for signing while a
   session is live — acceptable for a short-lived single-purpose VM we own, and gone the
   moment the session closes. Fallback if forwarding misbehaves: a fine-grained **read-only**
   PAT scoped to `zeblithic/harmony` only — a deliberate credential-at-rest deviation from
   the above: stored 0600, revoked *and* deleted at teardown (the down-checklist owns this).
5. **Build:** `cd src-tauri && cargo build --locked --release --bin harmony-app`. First cold build is
   the big unknown (est. 30–60 min on 4 vCPU — measure on first bring-up; if painful, do the
   one-time build on a temporary e2-standard-8 and keep the disk). Incremental rebuilds after
   a source rsync are minutes — this is why the disk survives between sessions.
6. **Profile + vault (headless Linux hard requirements):**
   - `HARMONY_PROFILE=xwan` (named profile → file-vault **only**; `serve` fails fast without a
     passphrase env, `lib.rs:19122-19137`).
   - `HARMONY_PASSPHRASE_FILE=/home/<user>/.harmony-xwan-pass` (0600, random, generated at
     provision; disposable with the node). Server Ubuntu has no secret-service daemon, so
     without this the node hard-fails at boot (`identity.rs:2789`). Set
     `HARMONY_DISABLE_KEYCHAIN=1` for belt-and-braces determinism — safe *only because* the
     passphrase is mandatory here: the encrypted-file vault covers the iroh transport key too
     (ZEB-449, `docs/headless-install.md` "Keychain-backed vault" caveat), whereas disabling
     the keychain **without** a passphrase boots a node that can't network.
   - Pre-seed `<data-dir>/connectivity-settings.json` with
     `{"identity_discoverable": true}` before first boot **or** call
     `connectivity_set_identity_discoverable` after boot (ZEB-512 verb). If pre-seeding:
     valid JSON, readable perms — a corrupt/unreadable file fails **closed** to
     `discoverable=false` + `presence_invisible=true` (`connectivity_settings.rs:294-350`),
     which silently breaks first-contact.
   - Identities are disposable: mint fresh per validation wave (`mint_owner_identity`), and
     mint **fresh invites after any identity rotation** (the ZEB-477 stale-ghost-key lesson).
7. **Run:** `harmony-app --profile xwan serve` — tmux for attended sessions; the systemd unit
   template in `docs/headless-install.md` if we want boot-persistent. The control API binds
   **127.0.0.1 only** (`api/mod.rs:219`), default port 7420, bearer token at
   `<data-dir>/api/token` — nothing to firewall, nothing exposed.

---

## 3. Firewall / NAT — the crux, as a two-mode design

**Fact that shapes everything:** the production iroh endpoint binds an **OS-assigned ephemeral
UDP port** — `Endpoint::builder(presets::N0)…bind()` with no `bind_addr`
(`src-tauri/src/iroh_endpoint.rs:194-235`), and no env/config override exists. So "open UDP
port N" is not available; the design must work with an unpinned port.

- **Egress:** default VPC allows all egress. That covers pkarr HTTPS
  (`pkarr.q8.fyi` → `relay.pkarr.org` → `pkarr.pubky.app`, `connectivity_settings.rs:56-62`),
  iroh relay HTTPS (n0 stable preset; `iroh_relays` empty = preset default), and QUIC egress.
  Nothing to configure.
- **Ingress — run the choreography in BOTH of these modes** (one firewall rule toggles them):
  1. **Open mode** — rule `harmony-xwan-udp`: allow `udp:1024-65535` from `0.0.0.0/0` to the
     VM's tag. Unsolicited inbound reaches the ephemeral port → the VM behaves like a
     public-IP host. This isolates the pure cross-WAN path: if direct doesn't establish here,
     the failure is not NAT filtering.
  2. **Filtered mode** — no ingress rule. GCE's VPC firewall is stateful: outbound UDP opens
     5-tuple return paths, so inbound succeeds only for flows the VM has probed — an
     approximation of address/port-dependent filtering, i.e. hole-punch must actually work
     (simultaneous open via relay-coordinated probing), not just "server is reachable."
- **Sequencing:** open mode first (does the direct path work at all across real WANs), then
  filtered mode (does hole-punching work under filtering). Mode flip =
  `gcloud compute firewall-rules create/delete harmony-xwan-udp` between runs.
- **Honest scope limit:** filtered-mode GCE is not a consumer NAT — no port rewriting happens
  on the VM side. The port-mangling half of the problem is supplied by *Koya's home NAT* on
  the other end of the pair. The hardest class (symmetric NAT both ends) remains untested and
  out of scope; that would need a second non-cloud network, which is exactly the "real
  external tester" case the ticket reserves for when we genuinely need it.
- **SSH:** default-allow-ssh / IAP as-is for now; impl may restrict source ranges.

**Success signal for the crux test:** `network_health_snapshot` on **both** sides reports the
peer with `connectionMode: "Direct"` (camelCase DTO keys — assert the exact key), corroborated
by iroh path logs (`path_remote=Direct(...)`). Relay-only connectivity delivers messages but
fails the test's purpose and must be reported as such, per mode.

---

## 4. Driving it headlessly

- **Transport for verbs:** the `api` CLI is hardcoded to `127.0.0.1` + local token discovery
  (`api/cli.rs:21,64`) — it cannot target a remote host, and the control server is
  loopback-only. So the driver executes verbs **on** each node:
  - local node: `harmony-app --profile xwan-local api <verb> '<json>'`
  - GCE node: `gcloud compute ssh harmony-xwan-1 --ssh-flag="-o BatchMode=yes" -- <path>/harmony-app --profile xwan api <verb> '<json>'`
  - event stream: `gcloud compute ssh harmony-xwan-1 --ssh-flag="-o BatchMode=yes" -- … api --events`
    piped to a local log (the fleet-playbook watcher idiom). For programmatic use an `-L`
    tunnel + `curl` with the bearer token against `/v1/rpc/{command}` works identically.
  No new client code required; the driver is a shell script of ssh-exec'd verbs. Every
  unattended ssh invocation carries `-o BatchMode=yes` (compliance rule: automation must
  fail fast, never hang on an interactive prompt).
- **Fully unattended is now real** (§0.1): every step below is an allowlisted v1 verb — no GUI,
  no file-dance, no human at either end.
- **Choreography** — retarget the `docs/playbooks/e2e-two-agent-suite.md` scenarios to the pair
  {fresh local Koya node ↔ GCE node}:
  - **T1 — first-contact + community (ZEB-330's missing evidence):** local `create_community`
    + `generate_invite` → GCE `connectivity_redeem_invite_iroh` (poll to `{"status":"joined"}`;
    `inviter_unreachable` retries expected during pkarr warm-up) → `list_community_members`
    convergence both sides.
  - **T2 — friend + DM + direct path (the headline):** GCE `generate_friend_token` → local
    `redeem_friend_token` → `accept_friend_request` if pending → `send_dm` both directions →
    `read_dm_thread` both sides → **assert `connectionMode: "Direct"` per §3**. Run in open
    mode, then filtered mode.
  - **T3 — reconnect/backfill:** stop GCE `serve`, post channel messages locally, restart GCE,
    assert catch-up (cross-WAN anti-entropy/backfill — the ZEB-599 machinery on a real WAN).
  - Timing realities baked into the polls: pkarr propagation ≈ 75–90 s; relay state warms 1–2
    min after boot; invite redeems are prompt-window (±30 min skew — GCE NTP makes this moot).
- **What can't drive this:** the single-machine `e2e-harness/` crate spawns local child
  processes only (`e2e-harness/src/node.rs:116`) — it is not the vehicle here. If we later
  want these runs in CI, teaching the harness a "remote node" handle is its own ticket.

---

## 5. What it unlocks

1. **ZEB-330** (In Progress): true distinct-WAN first-contact — the reopened DoD's validation
   step, self-served instead of waiting on external testers.
2. **Direct-path NAT traversal evidence** — the residue ZEB-504/ZEB-521 explicitly left
   unproven when they closed ("true cross-WAN NAT traversal remains a separate, unproven
   item").
3. **iroh 1.0 flag-day validation partner** — an off-LAN 1.0-wire peer to green the coordinated
   fleet rebuild before/while the standing profiles flip.
4. **ZEB-573 close gate** — open-join cross-WAN revalidation currently queued on AVALON
   availability can use the GCE node as the remote end.
5. **Recurring capability** — any future "does X survive a real WAN" question becomes an
   afternoon self-service session instead of a favor from a human on another network.

---

## 6. Implementation follow-up (next ticket, after this plan is reviewed)

- `scripts/gce-xwan/`: `up.sh` (create or start), `provision.sh` (idempotent deps + rsync +
  build), `run-tests.sh` (T1/T2/T3 with per-mode firewall toggling and camelCase-key
  assertions), `down.sh` (stop vs delete, ends with the nothing-left-running check).
- A short runbook in `docs/playbooks/` mirroring the two-agent suite's format.
- Acceptance for the impl ticket: from zero → T1 + T2 (both firewall modes) green in a single
  attended afternoon session, total GCE spend under ~$2.

## 7. Risks / open questions

- **Cold-build time** on 4 vCPU is unmeasured (est. 30–60 min). Mitigations: disk persistence
  across sessions; one-time build on e2-standard-8 if needed.
- **GCE filtered mode ≉ consumer NAT** (no VM-side port rewriting) — §3 scope limit; symmetric
  NAT both-ends stays out of scope by design.
- **Ephemeral-port firewall breadth:** open mode allows `udp:1024-65535` because the port
  can't be pinned. Acceptable for a short-lived test VM with nothing else listening; the rule
  exists only while a session runs.
- **Flag-day coupling:** until the fleet rebuild, only fresh local builds can pair with the
  GCE node. Not a blocker (the driver builds its own local node) but worth stating so nobody
  points a legacy fleet profile at it and files a ghost bug.
- **Cost drift:** shapes/prices quoted are estimates; impl pins exact figures and adds the
  budget alert.
