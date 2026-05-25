# Triage Runbook: `alpha-feedback` Issues

This is the per-issue runbook for processing GitHub issues labeled `alpha-feedback`. These come primarily from Sub-C's [`FeedbackModal`](../src/lib/components/FeedbackModal.svelte) (the in-app `(?)` → Submit Feedback flow), plus any manual reports filed through the [issue template](../.github/ISSUE_TEMPLATE/alpha-feedback.md).

Read [`alpha-validation.md`](./alpha-validation.md) first for the higher-level cadence + DoD context.

## Triage SLA

- **First-pass within 48h** of issue creation. Tester morale drops fast if reports vanish into the void.
- Even a holding response ("looking into this, will follow up by X") counts as first-pass. Don't let the issue sit unacknowledged.
- Aim to fully disposition within 1 week.

## Per-issue decision tree

For each new `alpha-feedback` issue:

```
1. Is the description complete enough to act on?
   ├─ Yes → continue to (2)
   └─ No  → comment requesting specifics ([Template: Need-more-info]), wait for response
           If no response in 7 days → close with [Template: Closed-needs-info]

2. What kind of finding is it?
   ├─ Real bug (something broken that shouldn't be) → see §Bug
   ├─ Feature gap (something missing that was meant to be there) → see §Bug (same treatment)
   ├─ Future-feature request → see §Future-feature
   ├─ Duplicate of existing issue/ticket → see §Duplicate
   ├─ Won't-fix-this-alpha → see §Out-of-scope
   ├─ Docs gap → see §Docs-gap
   └─ User confusion / installation issue → see §Confusion
```

## §Bug (and Feature gap)

1. Reproduce locally if possible. If not reproducible, ask the reporter for additional info ([Template: Need-more-info]).
2. **File a ZEB-NNN ticket** under the appropriate parent epic ([ZEB-321](https://linear.app/zeblith/issue/ZEB-321) connectivity, [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) alpha umbrella, or other) with:
   - Title: `<short description>`
   - Description: includes the GitHub issue link
   - Priority: per severity (block-alpha → Urgent; degraded → High; minor → Medium)
3. **Comment on the GitHub issue** with the Linear URL: "Tracking in [ZEB-NNN](url) — will close this when resolved."
4. When the ticket is resolved + the release is published:
   - Close the GitHub issue with: "Fixed in [vX.Y.Z](release-url). Please update and re-test."
   - If the reporter confirms the fix, leave it closed. If they report a regression, reopen + investigate.

## §Future-feature

1. Decide if it fits the v0.1.0-alpha scope. Per [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) constraints, **NO** for:
   - Mobile builds (post-alpha)
   - Code signing / notarization (post-alpha)
   - Crash telemetry / phone-home (out of scope)
   - Public beta / open registration (post-alpha milestone)
   - Multi-device admin (ZEB-173 separate track)
2. If out of scope, see [§Out-of-scope](#out-of-scope).
3. If in scope but not for this alpha, file under appropriate post-alpha epic OR add to a "wishlist" Linear doc.
4. Comment on GitHub: "Tracked for the [next-phase / wishlist]. Closing this; will surface when we plan the work."
5. Close.

## §Duplicate

1. Find the canonical issue or ticket.
2. Comment on the new issue: "Duplicate of #N — please follow there for updates."
3. On the canonical: "Additional report — see #M for context." Link both directions.
4. Close the duplicate.

## §Out-of-scope

Use [Template: Won't-fix-alpha] referencing the specific [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) constraint.

## §Docs-gap

1. File a ZEB-NNN docs-followup ticket. Lower priority than bugs.
2. **Update the relevant doc** as part of resolving the ticket. Don't leave docs stale.
3. Common docs gaps in alpha:
   - Install steps that don't match what the OS actually shows (Gatekeeper UI changed, etc.)
   - Network Health interpretations (red ≠ broken, what RTT range is healthy, etc.)
   - Confused about Phase 3 invite reusability — escalate to [`invite-distribution.md`](./invite-distribution.md) prose

## §Confusion (user error or unclear UI)

If a tester misread something or got confused by the UI, **the docs or UI failed, not the user**. Treat as either:
- Docs gap (see [§Docs-gap](#docs-gap))
- UI bug (see [§Bug](#bug-and-feature-gap)) if the UI should make the right behavior obvious

Resist the urge to close with "user error" — every confusion is feedback on the product.

## Response templates

Copy-paste these. Personalize the [bracketed] parts.

### [Template: Need-more-info]

```markdown
Thanks for the report! To help me reproduce this:

- [What were you trying to do? What did you click?]
- [What did you expect to happen?]
- [What actually happened? Any error messages?]
- [OS + version (e.g. macOS 15.0 / Windows 11 22H2 / Ubuntu 24.04)]
- [If applicable: did the (?) → Submit Feedback flow include network diagnostics? Could you paste them here?]

Will follow up once I can reproduce, or within [N days] either way.
```

### [Template: Tracking-in-linear]

```markdown
Thanks for the report! Tracking this in [ZEB-NNN](https://linear.app/zeblith/issue/ZEB-NNN). I'll close this issue when the fix ships — should be in v0.1.0-alpha.[X] / v0.[Y].
```

### [Template: Fixed-please-retest]

```markdown
This should be fixed in [v0.1.0-alpha.X](https://github.com/zeblithic/harmony-client/releases/tag/v0.1.0-alpha.X). The auto-updater should prompt you the next time you launch the app — please update and let me know if you still see the issue.

Closing for now. Reopen if it recurs after updating.
```

### [Template: Won't-fix-alpha]

```markdown
Thanks for the report. This falls outside the v0.1.0-alpha scope:

> [Cite specific ZEB-327 constraint, e.g. "Desktop only — mobile is post-alpha", or "Code signing is deferred to public beta"]

[If applicable: tracked for [next-phase / wishlist] at [link].]

Closing this — but the constraint is documented in [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) if you'd like the rationale.
```

### [Template: Duplicate]

```markdown
This looks like a duplicate of [#N / ZEB-NNN](url) — please follow there for updates. Closing this one to consolidate; appreciate the report!
```

### [Template: Closed-needs-info]

```markdown
Closing for now since I don't have enough context to reproduce. Please reopen with the additional info from [#comment-N](url) if you'd like to revisit — happy to take another look anytime.
```

## Weekly aggregation

End of each week:

1. Review all open `alpha-feedback`-labeled issues
2. Identify patterns — same issue from 2+ testers, recurring confusion in the same UI area
3. Cluster into theme tickets where useful (e.g., "Network Health interpretations" if 3 testers ask the same questions)
4. Update the [tester tracker](./invite-distribution.md#per-tester-tracking) with any tester-specific notes
5. If any tester has gone silent for 2+ weeks, drop quietly

## When to escalate vs absorb

Some findings warrant a tighter response than the standard SLA:

- **Critical-path bug** (no new tester can complete the journey): pause recruitment, focus on the fix, release a hotfix as v0.1.0-alpha.X+1
- **Data-loss / identity bug**: same as critical-path, plus a heads-up message to existing testers via Signal / Zeblithic announcements channel
- **Security vulnerability** (e.g. leak of a private key, signature bypass): escalate to private channel immediately, do NOT discuss in the public GitHub issue, file privately, ship a fix, then disclose

The default for everything else is the standard SLA above.

## References

- [Sub-D design spec](./specs/2026-05-25-zeb-330-sub-d-zeblithic-bootstrap-design.md)
- [`alpha-validation.md`](./alpha-validation.md) — higher-level cadence + DoD
- [`invite-distribution.md`](./invite-distribution.md) — tester tracking format
- [Issue template](../.github/ISSUE_TEMPLATE/alpha-feedback.md) — the structure new issues arrive in
- [Sub-C feedback flow source](../src/lib/components/FeedbackModal.svelte)
- [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) alpha umbrella + scope constraints
- [ZEB-330](https://linear.app/zeblith/issue/ZEB-330) Sub-D ticket
