# Zeblithic Community Bootstrap

This is Jake's playbook for minting the canonical **Zeblithic** community — the alpha-tester bootstrap target for harmony-client v0.1.0-alpha.

This doc covers only the mint itself. For what to do once Zeblithic exists, see [`invite-distribution.md`](./invite-distribution.md) and [`alpha-validation.md`](./alpha-validation.md).

## Pre-conditions

Before you start:

- [ ] harmony-client v0.1.0-alpha or later is installed (per [`install-macos.md`](./install-macos.md) / [`install-windows.md`](./install-windows.md) / [`install-linux.md`](./install-linux.md))
- [ ] Your identity is already minted — you see the main app, not the setup screen
- [ ] Network Health panel (`(?)` menu → Network Health) shows your connection as reachable (see [`cross-wan-validation.md`](./cross-wan-validation.md))
- [ ] You are the canonical zeblith / J Eng — this community's admin identity belongs to you and only you (the device that mints is the only device that can sign as admin until ZEB-173 multi-device binding ships)
- [ ] You are doing this on the device you intend to keep as the canonical Zeblithic admin. Switching devices later is currently not supported.

## Mint sequence

1. **Launch the app.** Confirm you see the main UI with the community sidebar on the left.

2. **Click `+ Create community`** at the bottom of the community sidebar.

3. **In the CreateCommunityDialog:**
   - **Name:** `Zeblithic`
   - **Type:** Open *(Phase 3 only supports Open communities. Invite-only is Phase 4. See [`invite-distribution.md` §Phase 3 caveats](./invite-distribution.md#how-invites-work-in-phase-3) for the implications.)*
   - **Submit.**

4. **Wait for the sidebar to update** with the new community + auto-created `#general` channel. The hex `community_id` is visible by hovering the community in the sidebar.

5. **Verify the mint:**
   - Sidebar shows `Zeblithic` with one channel (`#general`)
   - Network Health still shows you as reachable
   - Open `Settings → Profile` and confirm your handle is correct

## Add the initial channel set

Default channel set for Zeblithic alpha. Revise to taste before you actually click through — these are starting recommendations:

| Channel | `write_power` | Purpose |
|---|---:|---|
| `#general` (auto-created) | 0 | Default chat, open to all |
| `#announcements` | 50 (mod-only write) | Jake-broadcast: releases, alerts, "we're rotating epochs in 1h" |
| `#help` | 0 | Tester help requests; first place to look for "I'm stuck" |
| `#feedback` | 0 | Synchronous impressions; complement to the (?) → Submit Feedback flow |
| `#network-health` | 0 | Tester reports of connectivity issues, Network Health screenshots, "I see relay use, is that normal?" |

`write_power = 0` means anyone can write. `write_power = 50` means only mod-tier members (kick power ≥ 50) can write — currently just you.

### Per-channel sequence

For each non-auto channel:

1. **Select Zeblithic** in the community sidebar.
2. **Click `+`** at the bottom of the channel sub-sidebar.
3. **In the CreateChannelDialog:**
   - **Name:** the channel name without the `#` prefix (e.g. `announcements`)
   - **write_power:** per the table above
   - **Submit.**
4. **Confirm the channel appears** in the sub-sidebar.

Repeat for each channel in the table.

### Skipping or revising

- **Skip `#help`** if you don't want to triage real-time questions in addition to GitHub issues.
- **Add `#off-topic`** or `#lounge` if you want a social channel where testers can chat outside the project scope.
- **Add `#governance`** if you want to make polycentric-governance discussions visible to testers.
- **Drop everything except `#general` + `#feedback`** if you want minimal scope (rationale: less surface to test, fewer empty-channel impressions for early testers).

## Verify the bootstrap

After all channels are added:

- [ ] Owner state CBOR on disk includes the new Space row — verify via `ls ~/Library/Application\ Support/net.zeblith.harmony/owner_state.cbor` (macOS) or equivalent on your OS, and confirm the file was modified at the time you minted
- [ ] Sidebar shows `Zeblithic` with all expected channels
- [ ] Network Health still shows green
- [ ] Open `CommunitySettingsPanel → Members` and confirm you are listed as admin
- [ ] Open `CommunitySettingsPanel → Channels` (if present) and confirm each channel has the right `write_power`

## What can go wrong (mint-time)

| Failure | Likely cause | Recovery |
|---|---|---|
| `Submit` button stays grey | Empty name, or name contains only whitespace | Trim and retry |
| Community appears in sidebar but `#general` is missing | Backend partial-write bug | File a `bug` issue with logs from `~/.cache/net.zeblith.harmony/` (Linux) / equivalent |
| Network Health flips red after mint | Probably coincidence (unrelated network event); minting itself doesn't reconfigure your iroh endpoint | Check [`troubleshooting.md`](./troubleshooting.md) network sections; community mint succeeds locally regardless |
| `CreateChannelDialog` rejects channel name | Name > 32 chars, or contains characters the backend rejects | Pick a shorter / simpler name |
| `CreateChannelDialog` rejects `write_power` | Value > 100 | Use a value 0-100 |

None of these are fatal — Zeblithic is a regular community as far as the protocol is concerned, so any recoverable failure mode that applies to user-created communities applies here.

## What now?

Proceed to [`invite-distribution.md`](./invite-distribution.md) to generate your first invite URL and start distributing to testers.

## References

- [Sub-D design spec](./specs/2026-05-25-zeb-330-sub-d-zeblithic-bootstrap-design.md)
- [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) alpha umbrella
- [ZEB-330](https://linear.app/zeblith/issue/ZEB-330) Sub-D ticket
- `src-tauri/src/lib.rs:14020-14149` — `create_community` IPC
- `src-tauri/src/lib.rs:12335-12465` — `create_channel` IPC
