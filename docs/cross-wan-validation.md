# Cross-WAN validation playbook

> Goal: prove that two real Harmony machines on different networks
> can find each other and exchange messages end-to-end. This is the
> hands-on counterpart to the in-app Network Health panel.

## What you need

- Two machines on different networks (home Wi-Fi + coffee-shop Wi-Fi,
  two friends, two ISPs)
- Both running the same Harmony version (v0.1.2 or later)
- One out-of-band channel to share a `harmony://invite/...` URL
  (Signal, SMS, email)

This playbook takes ~10–15 minutes end-to-end.

## Step 1: Baseline (single-machine sanity)

On EACH machine independently:

1. Launch Harmony.
2. Open the **Network** panel (sidebar → Network).
3. Enable **Settings → Make me discoverable** (publishes your identity to
   pkarr so the pkarr self-test steps can verify discovery). Leave it on for
   the whole test.
4. Wait until the "Reachable" status appears (typically <30 seconds).
5. Click **Run self-test**. With discoverability on and a healthy network,
   all four steps show ✓:
   - **endpoint** — your iroh endpoint is bound.
   - **relay** — round-trip to the pkarr relay (shows the real RTT in ms; a
     slow but non-zero RTT is fine, not a failure).
   - **pkarr_publish** — your identity publication is active.
   - **pkarr_resolve** — your identity resolved back from pkarr (real RTT).

   A neutral **⊘** on `pkarr_publish`/`pkarr_resolve` means discoverability is
   off (turn it on in step 3) — it is **not** a failure. A red **✗** is a real
   problem; the reason next to it says what.
6. Screenshot the panel for your records.

**If a machine fails Step 1**: the cross-WAN test can't proceed.
Click **Submit diagnostics**, save the export, and attach it to a
tester-feedback issue with the title "Cross-WAN Step 1 failure on
\<your-OS\>".

## Step 2: First contact

On **machine A**:

1. Create a community ("test-cross-wan-YYYYMMDD" or similar throwaway name).
2. From the community settings, generate an invite URL.
3. Paste the URL into your out-of-band channel.

On **machine B**:

1. Click the `harmony://...` URL from machine A.
2. Confirm the join dialog.
3. After the "Joined" toast appears, return to the Network panel.
4. Within 60 seconds, **peer A** should appear in the peer list.

If peer A does NOT appear within 60s, both machines should run
self-test again and capture diagnostics.

## Step 3: Exchange

> To exchange raw identity keys directly (instead of going through a community
> invite), each tester opens **Friends → My key**, clicks **Copy**, and sends the
> 128-char hex to the other out-of-band; the receiver pastes it into **Add friend
> by key**. ("My key" shows "Start your node to view your key" until the node is up.)

1. On **machine A**: send a DM to machine B's identity ("hello from A").
2. On **machine B**: confirm receipt.
3. Reverse: B → A.
4. The Network panel on both machines should now show the other peer with:
   - **last_seen** within seconds
   - either **direct** or **relay** mode (note which)
   - measured **RTT**

## Step 4: Export

On both machines:

1. Click **Submit diagnostics**.
2. Review the redacted markdown.
3. Save as `.txt` (or copy and paste into your feedback issue).
4. Attach both diagnostics to a tester-feedback issue along with:
   - "Successful Step 3 cross-WAN exchange" OR
   - "Got stuck at Step N because Y"
   - Network conditions (which ISP, residential/business, behind VPN, etc.)

## Troubleshooting cheatsheet

| Symptom | Likely cause | Next step |
|---|---|---|
| Stuck on "starting up…" for >60s | Relay unreachable (firewall blocks UDP 443 outbound) | Test on a different network (mobile hotspot, coffee shop) |
| Self-test relay step ✗ | Same as above | Same as above |
| Peer never appears after URL click | Discovery (pkarr) failure or invite expired | Re-generate invite; if persistent, attach both machines' diagnostics |
| Peer appears but mode is "noConnection" | Reachability record received but no live connection negotiated | Try sending a DM (forces connection); if still failing, attach diagnostics |
| RTT >2s on relay mode | Distant relay or congested path | Note location; this is expected for some geographies in the alpha |
| One direction works, reverse doesn't | Asymmetric NAT | File a tester-feedback issue noting NAT type from both machines' panels |

## What "success" looks like

A Step 3 success means: bidirectional message exchange between two
machines on different networks, with both machines' Network panels
showing the other peer reachable. This is the empirical evidence that
[ZEB-321](https://linear.app/zeblith/issue/ZEB-321) Phase 2's cross-WAN discovery + handshake stack works as
designed.

A Step 3 failure on a specific symptom is also valuable data — file
the tester-feedback issue with both diagnostics and any other context
you can share.
