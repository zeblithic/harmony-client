# Invite Distribution Playbook

This is Jake's playbook for generating, distributing, tracking, and rotating Zeblithic invite URLs. Read [`zeblithic-bootstrap.md`](./zeblithic-bootstrap.md) first if Zeblithic doesn't exist yet.

## How invites work in Phase 3

Before you generate the first URL, you must understand the Phase 3 invite model:

- **Open-community invite URLs are infinitely reusable.** The URL embeds the raw 32-byte EpochKey. Anyone with the URL can join the community.
- **There is no per-URL revocation.** The only way to invalidate an outstanding URL is to trigger an epoch rotation, which invalidates ALL outstanding URLs at once.
- **Epoch rotation happens automatically on Kick.** When you (or any mod-tier member) kicks a community member, the community advances to a new epoch with a new EpochKey, and all old URLs become invalid.

Treat every distributed URL as a bearer secret. A leaked URL grants community membership forever, until you trigger a rotation.

Phase 4 ships per-invitee `InviteToken { expires_at, invitee_hint, sig }` with selective revocation. Until then, you live with the Phase 3 model and use rotation as the revocation lever.

## Generating a URL

1. Open the app.
2. Select **Zeblithic** in the community sidebar.
3. Open `CommunitySettingsPanel → Invites` (or the `InviteLinkManager` widget).
4. Click **Generate invite link**.
5. The URL appears. Copy it to the clipboard.
6. *(Optional)* Note the last 8 chars of the URL fragment in your tracker (see [tracking](#per-tester-tracking) below) — useful for after-the-fact correlation if a URL turns up where it shouldn't.

The same URL works for every tester until rotation. Generating multiple URLs from `InviteLinkManager` produces the same EpochKey embedded in each, so they are interchangeable. The point of generating per-tester URLs is purely for tracking — there's no per-URL revocation.

## Distribution channels — Jake's options

Pick the channel that matches your trust model with each tester:

| Channel | Pros | Cons | Recommended for |
|---|---|---|---|
| **Signal direct message** | E2E encrypted, you control recipients, message can be deleted (locally) | Receiver must type or click the URL on their actual harmony-client machine (paste hand-off if Signal is on phone) | **Default — recommended for all testers** |
| **GPG-encrypted email** | E2E encrypted, audit trail in your sent folder, no platform dependency | Requires GPG setup on both ends; clunky | Testers with existing GPG comfort |
| **In-person QR code** | No transit — you show the URL on your screen, they scan with their device | Requires meeting; hard to add new testers later | First 1-2 testers if you can meet them |
| **Phone-to-phone NFC / AirDrop** | High control, low transit risk | Same as in-person | Testers in your physical vicinity |
| **Private GitHub gist** | Persistent, you can revoke gist | Anyone with the gist URL gets the invite — just adds one layer of indirection; not a real privacy win | Backup channel only |
| **Discord / Slack DM** | Familiar to most testers | Platform stores the message, you don't control retention; gets indexed by search | **Not recommended for the first cohort** |
| **Plaintext email / SMS** | Universal | Plaintext in transit AND at rest on every relay between you and the recipient | **Not recommended** |

Default for first cohort: **Signal direct message** for everyone except the 1-2 testers you can meet in person (use QR for them).

## Per-tester distribution sequence

For each tester:

1. **Generate a fresh URL** via `InviteLinkManager → Generate invite link`. (Functionally identical to the previous URL, but generating per-tester makes [tracking](#per-tester-tracking) cleaner.)
2. **Copy to clipboard.**
3. **Open the distribution channel** (Signal thread, GPG-mail draft, etc.).
4. **Compose the message** using the [tester recruit template](#tester-recruit-message-template) below.
5. **Paste the URL** into the message body, where the template indicates.
6. **Send.**
7. **Log the send** in your [tracker](#per-tester-tracking) immediately, so you don't forget which testers have outstanding URLs.

## Tester recruit message template

Adapt to your voice — this is a starting point, not a script.

```text
Hey [name],

You're getting one of the first invites to test Harmony, the self-sovereign
federated chat I've been building. It's at the v0.1.0-alpha milestone — rough
edges expected.

The full setup takes ~15 minutes:

1. Download the build for your OS from:
   https://github.com/zeblithic/harmony-client/releases/latest

2. Walk past the macOS Gatekeeper / Windows SmartScreen warning using the
   per-OS install doc (linked from the README, or in the release notes).
   The binaries aren't signed for alpha — this is intentional and documented.

3. Launch the app. You'll see a Welcome modal explaining what Harmony is.
   Read it, then click "Skip" or paste the URL below into the invite field
   and click "Join".

   If you skipped the Welcome, you can paste the URL into the app's
   "Redeem invite" field (community sidebar → + → Redeem invite).

   The invite URL is:

   [INVITE_URL_GOES_HERE]

4. After joining, say hi in any channel — I'll be there.

5. The (?) menu in the top-right has a "Submit feedback" item — use it
   anytime to file structured issues with optional network diagnostics
   attached. Or just message me directly.

A few constraints to be aware of:
- Desktop only for this alpha (macOS / Windows / Linux). Mobile is later.
- The invite URL above is a bearer secret in this milestone — don't share
  it. If it leaks, let me know and I'll rotate.
- No telemetry — feedback only flows when you click "Submit feedback" or
  message me directly.

Thanks for being part of this. Reply with any questions.

Jake
```

If you have specific testers you want to give specific instructions to (e.g., "please test the Network Health panel under your double-NAT setup"), customize the message — don't ask testers to read your mind.

## Per-tester tracking

Keep a private tracker. **Don't commit it to git** — it contains tester identifiers and is privacy-sensitive.

Suggested format (CSV in your password manager, Signal note-to-self, or local text file):

```csv
tester,channel,url_fragment_last8,sent_at,redeemed_at,notes
Alice,Signal,a3b9c1d2,2026-05-26,2026-05-26,"Joined immediately, asked about Network Health"
Bob,email-GPG,e8f7g6h5,2026-05-26,,"Reminded 2026-05-28; says he'll get to it weekend"
Charlie,QR-in-person,c4d5e6f7,2026-05-27,2026-05-27,"On Linux Ubuntu 24.04, installed cleanly"
```

What to track:

| Field | Why |
|---|---|
| Tester name | So you know who you've reached out to |
| Channel | So you know how reliable a re-ping is |
| URL fragment (last 8 chars of base64 payload) | So you can correlate "Bob redeemed" with a specific send if you ever need to |
| Sent timestamp | So you know when to remind |
| Redeemed timestamp | Confirm in `CommunitySettingsPanel → Members` (a new member appearing is a tester redemption) |
| Notes | Anything useful: OS, NAT type, follow-up needed, feedback submitted |

A redemption is visible to you as a new member appearing in Zeblithic. Cross-reference the new member's join time with your tracker's `sent_at` to figure out who it was — Phase 3 doesn't include invitee identity in the URL, so the only correlation is timing.

## Reminders

If a tester hasn't redeemed within a few days:

- **Day +3**: gentle reminder, "checking in — let me know if you hit any snags getting it installed."
- **Day +7**: more direct, "still want to take this for a spin? If it's not the right time, no worries — just let me know."
- **Day +14**: assume they've passed, drop from the cohort.

Don't pressure. Alpha testers are doing you a favor.

## Rotation — revoking URLs

Phase 3 has no per-URL revoke. To invalidate ALL outstanding URLs:

1. **Kick any member from Zeblithic** via `CommunitySettingsPanel → Members → [member] → Kick`. **Important:** Kick acts as a ban in v0.1.0-alpha — the kicked member CANNOT rejoin (their next invite-redeem attempt is rejected with `InviteTargetBanned`, and a direct rejoin attempt is rejected with `BannedActorJoin`) until you explicitly unban them.
2. The Kick triggers an `EpochRotation`: a new EpochKey is generated, the community advances to the next epoch.
3. **All prior invite URLs become invalid.** Old URLs embed the old EpochKey; they no longer decrypt the current epoch.
4. **Generate a new URL** via `InviteLinkManager → Generate invite link`. It embeds the new EpochKey.
5. **Unban the kicked tester** via `CommunitySettingsPanel → Members → [member] → Unban` (or however the CommunityMembersPanel surfaces the action — check the member row's action menu). This must happen BEFORE you re-invite them, or their redeem will fail.
6. **Re-distribute** the new URL to anyone you want to keep — including the unbanned friendly tester. They re-redeem and rejoin.

**Two ways to handle the rotation tester:**

- **(a) Use a friendly tester** as above. Coordinate with them in advance ("I'm rotating in 5 minutes, you'll be kicked + need to redeem a new URL after I unban you"). Requires the Unban step.
- **(b) Use a disposable identity you control** — spin up a second harmony-client install on a second device or VM, redeem an invite as a "dummy" account, and kick that account when you need to rotate. No coordination overhead. Still requires Unban if you want to reuse the same dummy account, OR just create a fresh dummy each rotation.

Rotation is a heavy hammer. It invalidates EVERYONE'S invite, not just the leaked one. For Phase 3 this is the only option. Use it when:

- A URL leaks (publicly, or via a tester's compromised device)
- You want to formally "close" the alpha cohort (rotate, don't generate a new URL)
- You suspect (but can't confirm) a leak — the cost is one round of friendly re-distribution + Unban

## Pre-mortem: things that go wrong

| Failure | Likely cause | Recovery |
|---|---|---|
| Tester lost the URL | They closed Signal, lost the email, etc. | Generate + send a fresh URL. The fresh one is functionally identical until rotation, so no harm. |
| URL leaked publicly (e.g. screenshot posted to social media) | Tester forgot what "bearer secret" means | Rotate immediately (see [Rotation](#rotation--revoking-urls)). Re-distribute to remaining testers. |
| Tester can't redeem — RedeemInviteDialog stuck on "Resolving…" | pkarr resolution failed; their iroh endpoint or the relay is unreachable | Direct them to [`troubleshooting.md`](./troubleshooting.md). Suggest the "Try via local network" fallback button. |
| Tester redeems but never appears in your members list | iroh handshake completed locally on their side but the membership-event CRDT didn't replicate to you | Check Network Health (`(?)` menu → Network Health) on both ends. If their side is reachable, the issue is replication latency — wait 1-2 min and refresh. |
| Two testers report the same problem | A real bug, not user error | File ONE GitHub issue, link both reports as `relatedTo` in Linear |
| Tester downloads but never opens | They're busy / lost interest | Follow your reminder cadence (above), then drop from cohort |

## Aggregating feedback into tickets

This belongs in [`triage-alpha-feedback.md`](./triage-alpha-feedback.md). See that doc for the per-issue runbook.

## References

- [Sub-D design spec](./specs/2026-05-25-zeb-330-sub-d-zeblithic-bootstrap-design.md)
- [`zeblithic-bootstrap.md`](./zeblithic-bootstrap.md) — mint Zeblithic before generating invites
- [`alpha-validation.md`](./alpha-validation.md) — what tester journey completion looks like
- [`triage-alpha-feedback.md`](./triage-alpha-feedback.md) — what to do with the issues that come back
- `src-tauri/src/community_invite.rs` — invite wire format (Phase 3 = open, Phase 4 = invite-only with InviteToken)
- `src-tauri/src/lib.rs:13294-13551` — `generate_invite` IPC
