# ZEB-654 — Commons H: Mail cluster restyle

**Parent epic:** ZEB-603 (Commons design system). **Source:** ZEB-611 gap-fill audit
(`docs/design/commons/gap-fill-audit.md`, §Mail). **Branch:** `zeb-654-commons-h-mail-restyle`.

Three surfaces — `MailInbox.svelte`, `MailReader.svelte`, `MailCompose.svelte` — share one
systemic gap: **rem-at-14px-root spacing**. The app root is `font-size: 14px` (app.css:165), so
every `rem` padding/margin/gap resolves to a fractional pixel off the Commons grid
(`0.5rem` → 7px, `0.75rem` → 10.5px). The governance exemplars use raw integer px; rem-at-14px is
the legacy outlier. Fractional px also blur borders/text under fractional zoom.

## Scope (from the ticket)

1. Convert **rem spacing** (padding / margin / gap) → integer px on the Commons grid, all three files.
2. `MailReader .subject` (h2 panel headline) → `var(--font-display)`.
3. `MailCompose .compose-header h3` → `var(--font-display)`.
4. `MailInbox .unread-badge` bespoke pill → decide chip idiom.
5. Button / input radii off scale → normalize.

Already fixed in the ZEB-611 sweep (do **not** re-touch): `MailCompose` `#e55` → `var(--mail-error-text)`,
and its input radius 4px → 5px.

## Decisions

### D1 — rem→px spacing mapping (14px root; snap to nearest grid step, ≤1.5px shift)

Only **spacing** (padding / margin / gap) converts. **font-size stays in rem** (type scale — matches
the governance exemplars: `CountChip` uses px spacing + rem font-size). **Pure dimensions**
(`min-height`, `max-width`, `grid-template-columns`) stay in rem — they are intentionally scalable
sizing, not spacing, and out of this ticket's scope; converting them would be gratuitous visual churn.

| rem | px@14 | → | | rem | px@14 | → |
|-----|-------|---|-|-----|-------|---|
| 0.125 | 1.75  | 2  | | 0.625 | 8.75  | 8  |
| 0.25  | 3.5   | 4  | | 0.75  | 10.5  | 12 |
| 0.375 | 5.25  | 6  | | 1     | 14    | 16 |
| 0.4   | 5.6   | 6  | | 1.5   | 21    | 24 |
| 0.5   | 7     | 8  | | 2     | 28    | 32 |

Nearest-grid, uniform. `6`/`2` are the compact-control sub-grid the exemplars already use
(`CountChip` pads `6px 10px`). Every shift ≤1.5px — imperceptible tightening onto the grid.

### D2 — `.unread-badge` idiom: match the app's existing notification badge (NOT CountChip)

`CountChip` is a stacked *label-over-mono-value* data box (`QUORUM / 3 of 5`) — the wrong shape for
an inline count beside a folder-tab label. The app **already** has the right idiom: `NavNodeRow
.unread-badge` (accent pill, `text-bright`, `font-weight: 700`, `padding: 1px 6px`, `border-radius:
8px`) renders the identical concept — an unread count next to a nav label. `MailInbox`'s badge adopts
that anatomy (DRY, already-blessed). `.folder-tab` becomes `inline-flex` with a `4px` gap so the badge
sits beside the label without a bespoke `margin-left`/`min-width`. font-size stays the file's `0.6875rem`.

### D3 — radii: panel controls/buttons/inputs → 5px

Restyled sibling **panels** (`FriendsPanel`, `PendingJoinsPanel`) put every button, control, and input
at **5px** (`.unfriend-btn`/`.primary-btn`/`.secondary-btn`/`.accept-btn`/`.reject-btn` = 5px). The
**7px** button radius is scoped to confirm-*dialog* action buttons (`GovConfirmModal`/`ForkConfirmDialog`)
only. Mail is panel chrome → all its `4px` button/tab/control radii → `5px`. Cards/pills unchanged
(the notification badge keeps its 8px pill per D2). No 3px chips exist in these files.

## Edits

**MailInbox** — `.mail-toolbar` `8px 12px`; `.folder-tabs` gap `4px`; `.folder-tab` `4px 8px` / radius
`5px` / `inline-flex`+gap `4px`; `.unread-badge` → nav idiom (D2); `.compose-btn` `6px 12px` / radius
`5px`; `.empty-state` `32px`; `.mail-row` gap `8px` / pad `8px 12px`; `.mail-actions` gap `4px`;
`.action-btn` `0 4px`; `.sync-controls` gap `4px` / margin-left `8px`; `.sync-refresh-btn` `2px 6px` /
radius `5px`. (`grid-template-columns: 8rem …` stays — dimension.)

**MailReader** — `.mail-reader` `16px`; `.reader-toolbar` gap `8px` / mb `16px`; `.back-btn,.reply-btn`
`6px 12px` / radius `5px`; `.reader-header` mb `16px` / pb `12px`; `.subject` mb `8px` **+ font-display**;
`.recipients` mt `4px`; `.attachments` mt `16px` / pt `12px`; `.attachments h4` `0 0 8px`;
`.attachment-item` `4px 0`; `.reader-loading,.reader-error` gap `8px` / pad `16px`; `.error-msg` pad
`8px` / radius `5px`. (`max-width: 40rem` stays — dimension.)

**MailCompose** — `.mail-compose` `16px`; `.compose-header` mb `16px`; `.compose-header h3` **+ font-display**;
`.cancel-btn` `4px`; `.compose-form` gap `12px`; `.field` gap `4px`; `.field input,textarea` pad `8px`
(radius `5px` already); `.compose-actions` pt `8px`; `.send-btn` `8px 24px` / radius `5px`.
(`min-height: 12rem` stays — dimension.)

## Gate

No color literals touched → allowlist byte-identical (run the guard to confirm removal-only/no-op).
`npx tsc --noEmit && npx vitest run` from repo root. Rendered DOM unchanged (CSS-only) → existing
component tests are the regression net.
