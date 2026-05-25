# Alpha Validation Playbook

This is the playbook for running v0.1.0-alpha validation: recruiting testers, defining what "tester journey complete" means, and tracking progress toward [ZEB-330](https://linear.app/zeblith/issue/ZEB-330)'s definition-of-done.

Read [`zeblithic-bootstrap.md`](./zeblithic-bootstrap.md) and [`invite-distribution.md`](./invite-distribution.md) first.

## What "tester journey complete" means

A tester has completed the validation flow when all of the following are true:

- [ ] Downloaded harmony-client for their OS (per [`install-macos.md`](./install-macos.md) / [`install-windows.md`](./install-windows.md) / [`install-linux.md`](./install-linux.md))
- [ ] Walked past Gatekeeper / SmartScreen / AppImage permissions using the documented steps
- [ ] Launched the app and saw the Welcome modal
- [ ] Either pasted Jake's `harmony://invite/...` URL into the Welcome modal and clicked "Join", or skipped the Welcome and used the community-sidebar invite-redeem flow
- [ ] Saw the `RedeemInviteDialog` complete (status: `joined`)
- [ ] Landed in Zeblithic and saw the channel list
- [ ] Exchanged at least one message with another tester (or Jake) in any channel
- [ ] Opened the Network Health panel and confirmed their connection state (reachable, RTT < a few hundred ms is fine for alpha)
- [ ] Submitted at least one feedback issue via the `(?)` help menu → `Submit Feedback` flow (even if the "feedback" is "everything worked, no issues" — confirms the flow is reachable end-to-end)

The full list is in the [tester recruit message template](./invite-distribution.md#tester-recruit-message-template) so testers can self-verify as they go.

## Target tester pool

[ZEB-330](https://linear.app/zeblith/issue/ZEB-330) DoD: **at least 2 hand-picked testers complete the full flow.**

Recommended initial cohort: **3–5 testers** drawn from:

| Axis | Why it matters |
|---|---|
| **Trust** | They won't leak the URL (Phase 3 caveat) |
| **OS diversity** | Spans at least 2 of macOS / Windows / Linux — ideally all 3 — so install docs get exercised on each |
| **Network diversity** | Different NAT topologies (residential CGNAT, carrier-grade NAT, IPv6-only if you have access to one) so Network Health and the iroh fallback paths get tested under variety |
| **Patience** | Alpha testers must tolerate friction; they're not consumers |
| **Articulate** | Useful feedback requires written reflection — not just "it broke" but "I clicked X, expected Y, saw Z" |

3-5 is enough to surface real issues without being so many that triage drowns. Add more in subsequent waves if the first wave goes smoothly.

### Recruit script

For each candidate:

1. **Pre-ask** through your normal communication channel: "I'm running a closed alpha for a chat app I've been building. Want to be one of the first 3-5 testers? Setup is ~15 min, ongoing usage is whatever you want. Real bug reports help; mobile isn't supported yet. OK to send you details?"
2. If they accept, follow the [per-tester distribution sequence](./invite-distribution.md#per-tester-distribution-sequence).
3. Log them in your tracker.

If they decline or don't respond within ~3 days, move to the next candidate.

## Cadence

**Week 1 (mint + distribute + kickoff):**
- Mint Zeblithic ([`zeblithic-bootstrap.md`](./zeblithic-bootstrap.md))
- Generate + distribute URLs to first cohort ([`invite-distribution.md`](./invite-distribution.md))
- *Optional:* hold a synchronous kickoff (group video / Discord huddle) for the first cohort. Walk through the install + join flow live. Saves a lot of "I'm stuck" follow-ups.
- Monitor GitHub issues + Signal direct messages

**Week 2 (observation):**
- Testers explore at their own pace
- Daily check on `alpha-feedback`-labeled issues (per [`triage-alpha-feedback.md`](./triage-alpha-feedback.md))
- File follow-up ZEB-NNN tickets for actionable findings — never fold into ZEB-330 itself

**Week 3+ (aggregation):**
- Group similar findings, cluster into theme tickets if the same issue appears 3+ times
- Decide which findings are alpha-cycle fixable vs deferred to v0.1.1 / v0.2
- When the first cohort has reached "complete" (per the [DoD checklist above](#what-tester-journey-complete-means) for ≥2 testers), mark ZEB-330 done

## Findings → tickets

**Linear is the authoritative tracker** for harmony-client work. GitHub Issues with the `alpha-feedback` label are an intake surface only — every actionable finding flows into a Linear ZEB-NNN ticket within the triage SLA, and progress is tracked there. GitHub Issues serve as the public-facing receipt for testers; Linear serves as the engineering source of truth. Do not let GitHub Issues become a parallel work-tracking system.

Per [ZEB-330](https://linear.app/zeblith/issue/ZEB-330) DoD: **validation findings get filed as FOLLOW-UP tickets, NOT folded into ZEB-330.**

For each `alpha-feedback`-labeled GitHub issue, decide:

| Disposition | What to do |
|---|---|
| **Real bug** | File ZEB-NNN under [ZEB-321](https://linear.app/zeblith/issue/ZEB-321) (connectivity) / [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) (alpha) / appropriate parent. Comment on GitHub issue with the ZEB link. Close issue when ticket is resolved. |
| **Feature gap** | Same as bug, but file under the relevant feature epic. |
| **Future-feature request** | File under appropriate post-alpha epic, OR add to a "wishlist" Linear doc. Close issue with "tracked in [link]". |
| **Duplicate** | Close as duplicate of existing issue/ticket. Link both directions. |
| **Won't-fix-this-alpha** | Close with a templated reply (per [`triage-alpha-feedback.md`](./triage-alpha-feedback.md)) referencing the relevant ZEB-327 scope constraint. |
| **Docs gap** | File a docs-followup ZEB-NNN. Update the relevant doc as part of resolving. |
| **User confusion** | If the tester misread something, the docs failed. Treat as docs gap. |

The runbook for each disposition is in [`triage-alpha-feedback.md`](./triage-alpha-feedback.md).

## When is alpha validation done?

ZEB-330 DoD is hit when:

- [ ] At least 2 testers have completed the full journey (above)
- [ ] Each completion is confirmed in your tracker
- [ ] All `alpha-feedback`-labeled GitHub issues are triaged (closed, dispositioned, or have a linked ZEB-NNN)
- [ ] At least one follow-up ZEB-NNN ticket has been filed (the bar for "validation produced actionable signal")
- [ ] Critical-path issues (anything that prevents a new tester from completing the journey) are either fixed or have a workaround documented in [`troubleshooting.md`](./troubleshooting.md)

At that point: mark ZEB-330 done, write a brief retrospective comment on ZEB-327 summarizing what was learned, and decide what the v0.1.1 / next-phase priorities are based on validation findings.

## Pre-mortem: alpha validation failure modes

| Failure | Cause | Recovery |
|---|---|---|
| No tester completes the full journey | Critical bug discovered mid-validation that blocks new testers | Fix the bug (file ZEB-NNN, release v0.1.0-alpha.2), re-recruit |
| All testers complete but no issues filed | Testers are too polite, or the (?) flow is broken | Solicit feedback directly via Signal / email — "what did you find frustrating?" |
| One tester reports nothing for 2+ weeks | They've drifted out of the cohort | Drop quietly. Don't pressure. |
| Validation surfaces a fundamental design issue | e.g. NAT traversal is unreliable for half the cohort | Don't try to fix everything in this alpha. File the issues, decide which are blockers for shipping a v0.1.1, defer the rest to a future phase. |
| GitHub issue volume overwhelms you | Triage falling behind by days | Pause the cohort: don't recruit more, focus on triage. Resume recruiting once backlog is < 1 week old. |

## References

- [Sub-D design spec](./specs/2026-05-25-zeb-330-sub-d-zeblithic-bootstrap-design.md)
- [`zeblithic-bootstrap.md`](./zeblithic-bootstrap.md) — mint Zeblithic
- [`invite-distribution.md`](./invite-distribution.md) — distribute invites + tracker template
- [`triage-alpha-feedback.md`](./triage-alpha-feedback.md) — per-issue runbook
- [`troubleshooting.md`](./troubleshooting.md) — what testers will hit + documented workarounds
- [`feedback.md`](./feedback.md) — Sub-C feedback flow guide
- [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) alpha umbrella + DoD
- [ZEB-330](https://linear.app/zeblith/issue/ZEB-330) Sub-D ticket
