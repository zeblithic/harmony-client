# Rendered screenshots

Thumbnail/representative renders of each design, so you can review without
opening the `.dc.html` files. Canvas designs (Directions, Desktop, Mobile,
Onboarding, Forks, Charter, Dark) hold several frames each — these PNGs show the
title + hero frame; open the corresponding `references/*.dc.html` (next to
`support.js`) and pan for the full set.

| File | Design | Notes |
|---|---|---|
| `01-design-system.png` | Foundations & components | full token/type/component reference |
| `02-directions.png` | Three visual directions | A·Commons (chosen), B·Assembly, C·Chord |
| `03-desktop.png` | Desktop hi-fi | shell · ballot · proposals hub · delegation |
| `04-mobile.png` | Mobile hi-fi | channel · drawer · Assembly · ballot · delegation |
| `05-onboarding.png` | Onboarding & identity | mint → back up → redeem → identity/devices |
| `06-forks-lineage.png` | Forks & lineage | tree · fork dialog · divider · settings |
| `07-charter-settings.png` | Charter & settings | constitution · manage-community · dialogs |
| `08-dark.png` | Dark theme | warm dark: shell · ballot · charter |
| `09-vote-flow.png` | Vote flow (interactive) | the canonical voting interaction spec |
| `10-lineage-prototype.png` | Lineage walk (interactive) | click nodes · enter a fork |
| `11-town-hall.png` | Town Hall & voice | deliberation + motion-from-floor · voice channel · states · mobile |
| `12-vines-files.png` | Vines & Files | one-engine thesis · feed · player+publish · file store |
| `13-vines-feed-interactive.png` | Vines feed (interactive) | endless autoplay · 2nd/3rd-degree Discover · Tune |

For pixel-level values, the README and ADOPTION docs reference exact tokens; the
HTML files are the source of truth for spacing and layout. Dark-theme values for
**every** screen live in `tokens.css` under `:root[data-theme="dark"]`;
`08-dark.png` shows the shell/ballot/charter rendered with them, and the same
swap applies to Town Hall, Vines, and Files (warm dark, never graphite).
