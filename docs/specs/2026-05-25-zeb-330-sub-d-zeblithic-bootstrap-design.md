# ZEB-330 Sub-D: Zeblithic Bootstrap + Invite Distribution + Alpha Validation — Design

**Status:** Draft 2026-05-25 — autonomous design pass while Jake is offline. Open questions for Jake are flagged inline with **[JAKE-DECISION]**; defaults chosen are noted with rationale.

**Parent:** [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) v0.1.0-alpha umbrella.
**This ticket:** [ZEB-330](https://linear.app/zeblith/issue/ZEB-330) — Sub-project D.
**Predecessors (DONE on `origin/main`):**
- Sub-A [ZEB-328](https://linear.app/zeblith/issue/ZEB-328) #160 — auto-updater + `harmony://` deep-link + per-OS install docs
- Sub-B [ZEB-329](https://linear.app/zeblith/issue/ZEB-329) #161 — Network Health panel + DiagnosticExportModal + cross-WAN validation playbook
- Sub-C [ZEB-331](https://linear.app/zeblith/issue/ZEB-331) #162 — first-run onboarding UX + FeedbackModal + HelpMenuButton + troubleshooting docs

## 1. Overview

Sub-D is the user-facing capstone of the v0.1.0-alpha umbrella. It bridges the gap between "the software exists and works" (Subs A–C) and "real testers have used it end-to-end" (the ZEB-327 DoD).

The deliverables split into three layers:

| Layer | What | Owner |
|---|---|---|
| Hands-on actions | Mint Zeblithic, generate invite URLs, distribute to testers, recruit testers, react to validation findings | Jake (zeblith), out-of-band, on his machine |
| Documented playbooks | Step-by-step click-by-click procedures for each hands-on action so it's reproducible (and so a future contributor can mint a sibling community) | This PR |
| Validation tracking | Issue templates + label conventions + follow-up-filing process so feedback flows into actionable tickets | This PR |

**No code changes.** The wire format (`src-tauri/src/community_invite.rs`), mint IPC (`create_community`), channel IPC (`create_channel`), invite generator (`generate_invite`), redeem flow (`redeem_invite` / `connectivity_redeem_invite_iroh`), and Sub-C's FeedbackModal feedback flow are all already shipped. Sub-D is pure documentation + GitHub-config metadata.

## 2. Goals + non-goals

### Goals

1. **Zeblithic mint playbook**: a doc that walks Jake through every click to mint the canonical Zeblithic community with the right initial channel set + governance config + first reusable invite URL.
2. **Invite distribution playbook**: a doc that walks Jake through generating, distributing, tracking, and rotating invite URLs — including the prominent caveat that open-community URLs in Phase 3 are infinitely reusable and require Kick-triggered epoch rotation to revoke.
3. **Alpha validation playbook**: a doc that defines what "tester journey complete" looks like end-to-end, the tester-recruit message template, the per-tester tracking format, and the cadence for converting feedback into follow-up tickets.
4. **Feedback issue template**: `.github/ISSUE_TEMPLATE/alpha-feedback.md` matched to Sub-C's `buildGitHubIssueUrl()` output so the auto-applied `alpha-feedback` label tags every report from the in-app flow.
5. **Triage runbook**: a brief guide on how Jake processes new `alpha-feedback` issues — what becomes a ZEB-NNN ticket, what gets bundled, what gets closed as won't-fix-this-alpha.

### Non-goals

- **Actual minting**: requires Jake's identity (his Ed25519 device key). Sub-D documents how, does not do.
- **Actual distribution**: requires Jake's social capital (he picks testers, sends them URLs through trusted channels).
- **Phase 4 invite-only flow**: shipped wire format supports invite-only with per-invitee `InviteToken { expires_at, invitee_hint, sig }`, but the `generate_invite` IPC returns `Err("Phase 3 supports OPEN communities only…")` (`src-tauri/src/lib.rs:13351-13356`). Sub-D works around this with the rotation lever.
- **Multi-device admin**: ZEB-173 binding lets Jake's identity span devices, but the device that mints Zeblithic is the only device that can sign as admin until ZEB-173 phases ship. Out of scope.
- **Telemetry / phone-home**: per ZEB-327 constraint, no automated reporting. Findings flow through GitHub issues only.

## 3. Architecture

### 3.1 What ships

```text
docs/
├── zeblithic-bootstrap.md          NEW — mint playbook + first-invite generation
├── invite-distribution.md          NEW — distribution channels + tracking + rotation
├── alpha-validation.md             NEW — tester journey + recruit template + cadence
└── triage-alpha-feedback.md        NEW — Jake's runbook for processing new feedback issues

.github/
└── ISSUE_TEMPLATE/
    ├── alpha-feedback.md           NEW — auto-labeled template matched to Sub-C flow
    └── config.yml                  NEW (if absent) — allows blank issues (to preserve in-app prefilled URLs), links to other contacts
```

5 new docs + 1-2 GitHub-config files. ~600-1200 lines total. No source changes.

### 3.2 What does NOT ship (recorded for clarity)

- No new IPCs, no schema changes, no migrations, no test fixtures.
- No changes to Sub-C's `FeedbackModal.svelte` or `onboarding-env.ts` — the issue template just matches what they emit.
- No changes to `community_invite.rs` or `lib.rs:create_community`.
- No automated minting scripts. The minting flow is UI-driven; documenting commands would only drift from the UI.

### 3.3 Why doc-only

Per the pre-spec exploration (notes from the subagent that mapped `community_invite.rs` + `lib.rs` IPCs before spec drafting; findings condensed into §§3-7 below):
- `create_community` is fully exposed as an IPC, invoked from `CreateCommunityDialog` (`src/App.svelte:1854-1889`).
- `create_channel` is fully exposed and invoked from `CreateChannelDialog`.
- `generate_invite` is exposed and invoked from `InviteLinkManager`.
- No code path requires CLI tools or rust-side scripts to mint a community + populate channels + generate an invite URL.

Adding scripts or helper code would create maintenance burden without value; the existing UI is the source of truth.

## 4. Zeblithic mint playbook (`docs/zeblithic-bootstrap.md`)

### 4.1 Outline

```markdown
# Zeblithic Community Bootstrap

## Pre-conditions
- harmony-client v0.1.0-alpha or later installed
- Identity already minted (you see the main app, not a setup screen)
- Network Health panel shows green (per docs/cross-wan-validation.md)
- You are the canonical zeblith / J Eng — this community's identity belongs to you

## Mint sequence

1. Open the app.
2. Click `+ Create community` in the left community sidebar.
3. In the CreateCommunityDialog:
   - Name: `Zeblithic`
   - Type: Open  (Phase 3 only supports Open. Invite-only is Phase 4.)
   - Submit.
4. Wait for the sidebar to update with the new community + auto-created #general channel.
   The hex `community_id` is visible in the URL bar of the channel sub-sidebar.

## Initial channel set

Default channels for Zeblithic (Jake's call to revise — see [JAKE-DECISION 1]):

| Channel | write_power | Purpose |
|---|---|---|
| #general (auto) | 0 | Default chat (auto-created by mint) |
| #announcements | 50 (mod-only) | Jake's broadcast channel |
| #help | 0 | Tester help requests |
| #feedback | 0 | Tester impressions / synchronous feedback |
| #network-health | 0 | Tester reports of connectivity issues / Network Health screenshots |

Per channel: select Zeblithic in sidebar → click `+` on the channel sub-sidebar → CreateChannelDialog → enter name + write_power. Repeat.

## Verifying the mint

After all channels are added:
1. owner_state.cbor at ~/<app-data>/harmony.client/ contains the new Space row
2. Sidebar shows Zeblithic with 5 channels
3. Network Health shows you as connected
4. CommunitySettingsPanel → Members tab shows you as admin

## What now?

Proceed to docs/invite-distribution.md to generate + distribute your first invite URL.
```

### 4.2 Open questions for Jake (defaults applied; revise inline)

**[JAKE-DECISION 1] Initial channel set.** The above table reflects my best guess. Variations to consider:
- Drop #help if you don't want to triage real-time questions
- Add #off-topic / #lounge if you want a social channel
- Add #governance if you want to make polycentric-governance discussions visible
- Drop everything except #general + #feedback (minimal scope)

The doc draft uses the 5-channel set; you'll edit to taste during your actual mint pass.

**[JAKE-DECISION 2] write_power for #announcements.** I chose 50 (mod-tier write) to keep it Jake-broadcast. If you'd prefer admin-only (100), say so.

## 5. Invite distribution playbook (`docs/invite-distribution.md`)

### 5.1 Outline

```markdown
# Invite Distribution Playbook

## How invites work in Phase 3

The first thing you must understand: in Phase 3, open-community invite URLs
are **infinitely reusable** and **cannot be revoked individually**. The URL
embeds the raw 32-byte EpochKey. Anyone with the URL can join the community
forever, until you trigger an epoch rotation by Kicking a member.

This is intentional Phase 3 scope. Phase 4 ships per-invitee InviteTokens
with expires_at and per-URL revocation. Until then, distribute URLs as if
they are bearer secrets.

## Distribution channels (Jake's options)

| Channel | Pros | Cons |
|---|---|---|
| **Signal direct message** | E2E encrypted, you control recipients | Receiver must type/click URL on their target machine |
| **Personal email** | Universal, easy to forward | Plaintext in transit unless GPG, easily forwarded |
| **In-person QR code** | No transit | Requires meeting; harder to add new testers |
| **Out-of-band hand-off** (slip of paper, phone-to-phone NFC) | High control, low risk | Same as in-person; doesn't scale |
| **Private GitHub gist** | Persistent, can revoke gist | Anyone with gist URL gets the invite — same problem, one indirection deep |
| **Discord/Slack DM** | Familiar to tester | Platform stores message, you don't control retention |

Default recommendation: **Signal direct message**. Trades convenience for the strongest end-to-end story for an alpha cohort.

## Per-tester distribution sequence

1. From Zeblithic's CommunitySettingsPanel, click "Generate invite link".
2. URL appears; copy to clipboard.
3. Open a fresh Signal DM thread with the tester.
4. Send the message template (see §below).
5. Track in your private tracker (see §tracking).
6. If you want a fresh URL per tester (recommended; see §rotation), generate a new one for the next tester — currently all URLs work the same way, but tracking who-got-what makes rotation cheaper later.

## Message template

[See full template in docs/invite-distribution.md final draft]

## Per-tester tracking format

Suggested private tracker — a Signal note-to-self or a local CSV. Don't commit this to git (privacy).

| Tester | Channel | URL fragment (last 8 chars) | Sent at | Redeemed at | Notes |
|---|---|---|---|---|---|
| Alice  | Signal  | …a3b9c1d2 | 2026-05-26 | 2026-05-26 | Joined immediately, asked about Network Health |
| Bob    | email   | …e8f7g6h5 | 2026-05-26 | (pending)  | Reminded 2026-05-28 |

## Rotation (revoking URLs)

Phase 3 has no per-URL revoke. To invalidate ALL outstanding URLs:
1. Kick any member from Zeblithic (CommunitySettingsPanel → Members → Kick).
2. The Kick triggers an EpochRotation: a new EpochKey is generated, the community advances to the next epoch.
3. ALL prior invite URLs (which embed the OLD EpochKey) become invalid.
4. Generate a new URL via InviteLinkManager; it embeds the NEW EpochKey.
5. Re-distribute to anyone you want to keep.

This is a heavy hammer. It invalidates EVERYONE'S invite, not just the leaked one. For Phase 3 this is the only option. Phase 4 ships per-URL InviteTokens with selective revocation.

## Pre-mortem: things that go wrong

- **Tester loses URL**: regenerate one for that tester only (the URL still works for everyone, but you can track "Tester sent a fresh URL").
- **URL leaked publicly**: trigger rotation immediately (Kick a friendly tester, ask them to redeem the new URL).
- **Tester can't redeem**: usually a Network Health issue — direct them to docs/troubleshooting.md.
- **Tester redeems but never appears in members**: check Network Health on both ends; the iroh handshake may have hit a NAT issue. Try the LAN fallback ("Try via local network" button in RedeemInviteDialog).
- **Multiple testers report the same issue**: file ONE GitHub issue, link the duplicates as `relatedTo` in Linear.
```

### 5.2 Open questions for Jake

**[JAKE-DECISION 3] Default distribution channel.** I picked Signal. If you'd prefer GPG email, in-person, or another, the doc's recommendation paragraph changes.

**[JAKE-DECISION 4] Tester tracking format.** I sketched a CSV. If you prefer Linear or a Signal note-to-self, the §tracking section changes.

## 6. Alpha validation playbook (`docs/alpha-validation.md`)

### 6.1 Outline

```markdown
# Alpha Validation Playbook

## What "tester journey complete" means

A tester has completed the validation flow when ALL of the following are true:

- [ ] Downloaded harmony-client for their OS (per docs/install-macos.md / install-windows.md / install-linux.md)
- [ ] Walked past Gatekeeper / SmartScreen using the documented steps
- [ ] Launched the app and saw the Welcome modal
- [ ] Clicked the `harmony://invite/...` URL Jake sent them
- [ ] Saw the RedeemInviteDialog confirm Zeblithic join
- [ ] Landed in Zeblithic and saw the channel list
- [ ] Exchanged at least one message with another tester (or Jake) in any channel
- [ ] Opened the Network Health panel and confirmed reachability
- [ ] Submitted at least one feedback issue via the (?) help menu

The full list lives in the tester recruit message (next section) so they
self-verify as they go.

## Tester recruit message template

[See full template in final draft]

## Target tester pool

ZEB-330 DoD: at least 2 hand-picked testers complete the full flow.

Recommended initial pool: 3-5 testers, drawn from:
- People Jake trusts not to leak URLs (per the Phase 3 caveat)
- People who span 2+ OSes (macOS + Windows + Linux) so install-docs get validated everywhere
- People with realistic-but-different network conditions (carrier NAT, double-NAT, IPv6-only) so Network Health gets validated under variety

## Cadence

Week 1: distribute URLs, hold a launch sync (group video or Discord huddle) to walk testers through the journey.
Week 2: testers explore at their own pace; Jake monitors GitHub issues.
Week 3+: aggregate feedback into follow-up tickets, file as ZEB-NNN under ZEB-327 OR as standalone if scope warrants.

## Findings → tickets

Per ZEB-330 DoD: validation findings get filed as FOLLOW-UP tickets, NOT folded into ZEB-330.

Each finding → one of:
- New ZEB-NNN ticket if it's a real bug / feature gap to fix
- Comment on existing related ticket if it duplicates a known issue
- Close as won't-fix-this-alpha if out-of-scope (e.g., mobile request)

See docs/triage-alpha-feedback.md for the per-issue runbook.
```

### 6.2 Open questions for Jake

**[JAKE-DECISION 5] Tester pool size.** I sketched 3-5. If you want more (broader coverage) or fewer (tighter feedback loop), adjust.

**[JAKE-DECISION 6] Launch cadence.** I sketched a launch sync + 2-3 week observation window. If you want async-only or a longer window, adjust.

## 7. Feedback issue template (`.github/ISSUE_TEMPLATE/alpha-feedback.md`)

### 7.1 Why this matters

Sub-C's `buildGitHubIssueUrl()` (`src/lib/onboarding-env.ts`) opens issues with:
- Title: `[alpha-feedback] <first 50 chars of description>`
- Body: structured `## Description` + `## Environment` + optional `## Network diagnostics`

Without an issue template that matches, GitHub's "new issue" form would override the title prefix and lose structure. The template solves this by:
1. Auto-applying `alpha-feedback` label
2. Providing the same sectional structure as fallback for non-Sub-C-flow reports

### 7.2 Template content

```markdown
---
name: Alpha feedback
about: Report issues, suggestions, or observations from harmony-client v0.1.0-alpha testing
title: '[alpha-feedback] '
labels: ['alpha-feedback']
assignees: ['jenglund']
---

## Description

<!-- What happened? What did you expect? What did you see? -->

## Environment

- App version: <!-- auto-filled from app or paste here -->
- Platform: <!-- macos / windows / linux + arch -->
- OS version: <!-- e.g. macOS 15.0 / Windows 11 22H2 / Ubuntu 24.04 -->
- Submitted: <!-- timestamp -->

## Network diagnostics (optional)

<!-- If the (?) → Submit Feedback flow attached diagnostics, they appear here.
     If reporting manually, you can paste the Network Health → Export panel
     output (it auto-redacts identifiers). -->
```

### 7.3 Repository-level config (`.github/ISSUE_TEMPLATE/config.yml`)

```yaml
# blank_issues_enabled MUST be true. Sub-C's buildGitHubIssueUrl builds
# /issues/new?title=...&body=... URLs without &template=alpha-feedback.md,
# and `false` would redirect those to the chooser and strip the prefilled
# body. Follow-up: add &template=alpha-feedback.md so we can flip this to
# false AND get auto-applied labels/assignees on in-app submissions.
blank_issues_enabled: true
contact_links:
  - name: Troubleshooting docs
    url: https://github.com/zeblithic/harmony-client/blob/main/docs/troubleshooting.md
    about: Common install / network issues with documented fixes.
  - name: Feedback flow guide
    url: https://github.com/zeblithic/harmony-client/blob/main/docs/feedback.md
    about: How to submit feedback through the in-app flow.
```

> **Label gap for in-app submissions.** Because `blank_issues_enabled: true` and `buildGitHubIssueUrl` does not include `&template=alpha-feedback.md`, in-app submissions arrive as blank-form-prefill issues and do NOT carry the `alpha-feedback` label or the `jenglund` assignee automatically. They DO carry the `[alpha-feedback]` title prefix. Until the URL-builder is updated, the triage runbook recommends filtering by title prefix as a fallback (see [`triage-alpha-feedback.md` § Filter notes](../triage-alpha-feedback.md#filter-notes)).

### 7.4 Open questions for Jake

**[JAKE-DECISION 7] Default assignee.** I set `assignees: ['jenglund']`. If you'd rather route through a triage label only, remove the assignees field.

**[JAKE-DECISION 8] Existing templates.** If harmony-client already has issue templates (bug_report.md, feature_request.md, etc.), Sub-D's template lives alongside them. If not, we may want to add basic bug/feature templates too. I'll check during implementation and adjust.

## 8. Triage runbook (`docs/triage-alpha-feedback.md`)

### 8.1 Outline

```markdown
# Triage Runbook: alpha-feedback issues

When a new `alpha-feedback`-labeled issue arrives:

## Decision tree

1. **Real bug** (something is broken that shouldn't be) → file ZEB-NNN ticket, link issue in description, comment on the issue with the Linear URL. Close issue as fixed when the ticket is resolved.
2. **Feature gap** (something missing that was meant to be there) → same as bug.
3. **Future-feature request** (something not on the v0.1.0-alpha roadmap) → file under appropriate post-alpha epic OR add to a "backlog/wishlist" Linear doc. Close issue with "tracked in [link]".
4. **Duplicate of existing issue/ticket** → close as duplicate, link both directions.
5. **Won't-fix-this-alpha** (mobile request, signing request, out-of-scope) → close with template explanation referencing the relevant ZEB-327 constraint.
6. **User confusion / docs gap** → file as docs-followup ticket, comment with link.

## Standard responses (copy-paste)

[Templates for each branch — concise, kind, useful]

## Cadence

Aim for first-pass triage within 48h of issue creation. Tester morale drops fast if reports vanish into the void.

## Aggregation

End of each week: review all open `alpha-feedback` issues. Identify patterns (multiple testers hitting the same thing). Cluster into theme tickets where useful.
```

## 9. Validation flow end-to-end

```text
Jake (zeblith)                    Tester                       Repo
─────────────                     ──────                       ────
1. mints Zeblithic
2. generates URL
3. signal-msg URL ────────────────→
                                  4. clicks URL
                                  5. RedeemInviteDialog
                                  6. lands in Zeblithic
                                  7. messages other tester
                                  8. opens Network Health
                                  9. clicks (?) Submit Feedback
                                                                10. issue filed
                                                                    label=alpha-feedback
11. triages issue ←─────────────────────────────────────────────────── (within 48h)
12. files ZEB-NNN
13. links issue ↔ ticket
14. resolves / closes
```

Loop continues until ZEB-330 DoD is hit: ≥2 testers complete the journey end-to-end and submit feedback.

## 10. Out of scope (cross-referenced)

- Phase 4 invite-only flow + per-invitee `InviteToken` (separate epic when Phase 4 lands)
- ZEB-173 multi-device admin (separate epic, post-alpha)
- ZEB-202 identity recovery flow (separate track)
- ZEB-321 Phase 3+ liveness / rebinding / mobile push (separate epic)
- Public beta / open registration (separate post-alpha milestone)
- Code signing + notarization (separate post-alpha milestone)
- Mobile builds (separate post-alpha milestone)

## 11. References

- [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) — alpha umbrella
- [ZEB-330](https://linear.app/zeblith/issue/ZEB-330) — this ticket
- [ZEB-331](https://linear.app/zeblith/issue/ZEB-331) — Sub-C predecessor (FeedbackModal → GitHub-issue flow)
- [ZEB-329](https://linear.app/zeblith/issue/ZEB-329) — Sub-B predecessor (Network Health panel + DiagnosticExportModal)
- [ZEB-328](https://linear.app/zeblith/issue/ZEB-328) — Sub-A predecessor (deep-link handler + install docs)
- `src-tauri/src/community_invite.rs` — invite wire format
- `src-tauri/src/lib.rs:14020-14149` — `create_community` IPC
- `src-tauri/src/lib.rs:12335-12465` — `create_channel` IPC
- `src-tauri/src/lib.rs:13294-13551` — `generate_invite` IPC
- `src/lib/onboarding-env.ts` — `buildGitHubIssueUrl()` (target of the issue template)

## 12. Open questions summary (for Jake's PR review)

Consolidated decision list to skim before the PR merges:

1. [JAKE-DECISION 1] Initial channel set for Zeblithic (default: #general + #announcements + #help + #feedback + #network-health)
2. [JAKE-DECISION 2] #announcements write_power (default: 50 = mod-only)
3. [JAKE-DECISION 3] Default distribution channel (default: Signal direct message)
4. [JAKE-DECISION 4] Per-tester tracking format (default: CSV / Signal note-to-self)
5. [JAKE-DECISION 5] Initial tester pool size (default: 3-5)
6. [JAKE-DECISION 6] Launch cadence (default: kickoff sync + 2-3 week observation)
7. [JAKE-DECISION 7] Issue template default assignee (default: jenglund)
8. [JAKE-DECISION 8] Other GitHub issue templates (bug / feature) — defer or include?

All defaults are non-blocking — Jake can revise any of them inline in the docs after merge, or pre-merge via PR comment. The docs land with reasonable starting values so the PR is mergeable without a synchronous design session.
