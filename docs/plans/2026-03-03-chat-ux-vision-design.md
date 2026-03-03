# Harmony Chat UX Vision

Post-MVP UX design for the Harmony client. Extends the baseline MVP design
(`2026-03-03-harmony-client-design.md`) with a dual-panel layout, recursive
navigation, profile extensions, message priority levels, media trust controls,
and threaded conversation support.

All features described here are **client-side UX** — the underlying Harmony
protocol and wire formats remain unchanged. Data structures for profiles,
notification sounds, and trust settings live in client-local storage and are
shared with peers via Zenoh presence announcements.

---

## 1. Dual-Panel Layout

Separate text from media. The left panel is a compact, scannable text feed.
The right panel is a chronological media feed of stacked cards.

### Layout

```
┌──────┬──────────────────────┬──────────────────────┐
│      │                      │                      │
│ Nav  │   Text Feed          │   Media Feed         │
│      │   (compact messages) │   (stacked cards)    │
│      │                      │                      │
│      ├──────────────────────┤                      │
│      │                      │   ← cards from both  │
│      │   Thread View        │     main + thread    │
│      │   (when open)        │     interleave here  │
│      │                      │                      │
└──────┴──────────────────────┴──────────────────────┘
```

### Text Feed (left panel, top)

- Messages display as compact text-only: avatar micro (24px circle), display
  name, timestamp, message text.
- Links, images, and embeds are replaced with a small inline indicator (subtle
  icon + "1 image" or "link: github.com") clickable to scroll to the
  corresponding media card.
- The text feed stays dense and scannable — like IRC with avatars.

### Media Feed (right panel)

- Stacked cards in chronological order matching the text feed.
- Each card has a small header: sender avatar mini, display name, timestamp,
  link-back anchor to the originating message in the text feed.
- Card content: image preview, link OG card, code block, video thumbnail, file
  card, or any other rich media type.
- When a thread is open, thread media cards interleave chronologically with main
  channel cards, visually distinguished (subtle left border color + "in thread"
  tag). Thread cards animate out when the thread closes.

### Responsive behavior

- On narrow windows the media panel collapses and media embeds fall back to
  inline display (Discord-style) — graceful degradation.
- The nav panel can collapse to icons-only mode.

---

## 2. Navigation Hierarchy

Recursive, arbitrarily deep, but **zero indentation**. Depth is expressed
through color banding and bracket markers, never through horizontal offset.
All text and icons remain left-justified at the same position regardless of
nesting depth.

### Depth visualization

- Each nesting level adds a **1–4 px colored border strip** on the left edge.
  The color matches the group.
- Four rotating colors (e.g., green → blue → purple → orange). Each level
  cycles through the three colors that are not the parent's color.
- At 8 levels deep the left border consumes at most 32 px (one profile picture
  width).
- **Bracket markers** on the right edge: rotated parentheses `┌` / `┘` mark
  group open/close. These work without color for colorblind accessibility.

### Example

```
┌─────────────────────────────┐
│ 🔍 Search                   │
├─────────────────────────────┤
│┃ Work ──────────────── ┌  │   green bg, bracket opens
│┃┃ Harmony Dev ──── ┌  │   blue bg, nested bracket
│┃┃ # general         │  │
│┃┃ # crypto          │  │
│┃┃ # transport  ──── ┘  │   blue bracket closes
│┃┃ IPFS Crew ─────── ┌  │   purple bg
│┃┃ # mesh            │  │
│┃┃ # routing ──────── ┘  │   purple bracket closes
│┃ Alice               │  │   green bg (in Work)
│┃ Bob ──────────────── ┘  │   green bracket closes
│ Friends ─────────── ┌  │   orange bg
│  Charlie              │  │
│  Dana ─────────────── ┘  │
│ Eve                       │   top-level, no color band
└─────────────────────────────┘
```

### Structure

- Everything is a **node** in a tree: folders, community hubs, channels, DMs,
  group chats.
- Folders can contain any node type including other folders — unlimited nesting.
- Community hubs are special nodes that map to a shared Zenoh key-expression
  namespace (`harmony/community/{hub_id}/channels/{channel_name}`).
- DMs and group chats can live inside folders or at the top level.

### Display modes (user toggle)

- **Text list:** display name + unread badge, compact rows (~24 px height).
- **Icon grid:** profile picture thumbnails (32×32) in a flowing grid, unread
  dot overlay.
- **Both:** icons on left, name on right.
- Configurable per-folder or globally.

### Ordering behavior (configurable per-folder)

- **Activity-first:** new messages surface the node to the top of its containing
  folder (Discord-like).
- **Pinned:** nodes stay in fixed positions, unread indicators appear but
  position does not change.
- **Alphabetical:** static sort by name.
- Default is activity-first.

### Unread indicators

- **Standard priority:** bold name + count badge.
- **Quiet priority:** no badge; subtle "new activity" dot visible on hover only.
- **Loud priority:** pulsing/highlighted badge — visually prominent, cannot miss.

### Color assignment

- Groups auto-assign from the 4-color palette based on sibling position,
  rotating to avoid the parent's color.
- Users can override any group's color from the palette or from an extended
  palette in visual settings.
- The color set itself is configurable (swap to high-contrast, pastel, etc.).

---

## 3. Profile System & Custom Notification Sounds

Visual identity and sonic identity, both content-addressed.

### Profile pictures

- **Source/master:** 1024×1024 PNG or WebP, stored in harmony-content CAS
  (referenced by CID).
- **Mini:** 256×256 derivative, also CID-referenced. Used in message feeds, nav
  icons, media card headers.
- **Micro:** 24×24 rendered from mini. Used inline in compact text feed.
- Client generates the 256×256 on upload (center-crop + downscale), stores both
  as separate CIDs.
- Other clients fetch the mini by default; fetch the 1024 for profile view or
  high-DPI contexts.
- If no avatar set, generate a deterministic visual from the peer's address hash
  (identicon-style).

### Notification sounds

Small audio files (WAV or OGG, capped at ~500 KB) stored in harmony-content
CAS, referenced by CID in the user's profile.

```
profile.notification_sounds = {
  quiet:    CID("whisper.ogg"),   // plays only for receivers who opt in
  standard: CID("chime.ogg"),
  loud:     CID("siren.ogg"),
}
```

- On first encounter with a peer, the client lazily fetches their notification
  sound CIDs.
- Client caches sounds locally after first fetch — tiny files, fetch once.
- All three slots are optional; `null` falls back to the client's system
  default.

### Receiver override chain (highest priority wins)

```
1. Receiver's per-peer override    ("I want Alice to sound like 'Knock'")
2. Receiver's per-community override ("All Harmony Dev messages use 'Ping'")
3. Sender's profile default        ("Alice set 'Chime' as her standard sound")
4. Client's global default         ("Use system notification sound")
```

If a receiver does not configure anything, they hear whatever sound the sender
chose for themselves. Overrides apply per-priority-level.

### Profile metadata

Shared via Zenoh presence announcement, stored locally:

- Display name (UTF-8, max 64 chars)
- Avatar CIDs (1024 + 256)
- Notification sound CIDs (quiet + standard + loud)
- Status text (optional, max 128 chars)

---

## 4. Message Priority Levels

Three tiers baked into every message, with cascading notification behavior.

### Priority definitions

| Priority     | Sender intent               | Default receiver behavior                   |
|------------- |---------------------------- |-------------------------------------------- |
| **Quiet**    | "No rush, read whenever"    | Unread dot only (subtle), no push, no sound |
| **Standard** | "Normal message"            | Unread badge + count, sound, push           |
| **Loud**     | "Hey, pay attention"        | Prominent badge, distinct sound, breaks DND |

### Sender-side UX

- Default send is always **standard** (Enter key).
- **Quiet** via modifier: Ctrl+Enter, dropdown on the send button, or `/quiet`
  prefix.
- **Loud** via explicit action: dropdown on the send button or `/loud` prefix.
  Slightly more friction than standard — intentionally discourages overuse.
- Priority selector: a small three-state toggle near the send button `🔇 🔔 📢`.

### Receiver-side controls

Per-peer, per-community, or global:

```
notification_policy = {
  quiet:    "silent" | "dot_only" | "sound",              // default: "dot_only"
  standard: "silent" | "notify" | "sound",                // default: "sound"
  loud:     "silent" | "notify" | "sound" | "break_dnd",  // default: "break_dnd"
}
```

- **silent:** message arrives, no indicator at all (effectively muted).
- **dot_only:** subtle unread dot, no sound, no push.
- **notify:** push notification but no sound.
- **sound:** play notification sound + push notification.
- **break_dnd:** push notification + sound even when Do Not Disturb is active.

### DND interaction

- Standard: respects DND (no notification while DND active).
- Loud: breaks through DND **by default** — receiver can downgrade per-peer.
- Total receiver control: if someone spams loud, set them to `loud: "silent"`.

### Anti-abuse

- No rate limiting on quiet (silent anyway).
- Loud messages show a client-side courtesy cooldown indicator if sent
  frequently ("you've sent 3 loud messages in 5 minutes") — not protocol
  enforcement.
- Receivers always have full override control; abuse is self-correcting.

---

## 5. Media Trust & Auto-Preview

Security-first rich content. Nothing renders automatically unless the user has
opted in.

### Default state

Every link, image, or embed shows a **placeholder card** in the media panel:

```
┌──────────────────────────────┐
│ 🔒 Media from Alice          │
│   [link: github.com/...]     │
│   Click to preview           │
│   ☐ Always preview from Alice│
└──────────────────────────────┘
```

No HTTP requests, no images fetched, no OG metadata loaded — zero network
activity until the user clicks. Prevents tracking pixels, IP leaks, malicious
payloads, and bandwidth surprises.

### Trust levels

| Level                    | Behavior                                          | How to set                                  |
|------------------------- |-------------------------------------------------- |-------------------------------------------- |
| **Untrusted** (default)  | Placeholder card, click to load one-time          | Default for all peers                       |
| **Preview**              | Auto-fetch OG metadata + thumbnail; full media requires click | Checkbox on media card or in settings |
| **Trusted**              | Full auto-preview — images render, links expand   | Explicit toggle in settings                 |

### Trust resolution

```
1. Per-peer override exists?     → use it
2. Per-community setting exists? → use it
3. Global default                → untrusted
```

A new peer joining a trusted community starts at the community's trust level.
Per-peer overrides always win.

### What goes through the trust gate

- External URLs (HTTP/HTTPS links)
- Images (could be fetched from a malicious source)
- Embeds (OG cards, video thumbnails, rich previews)
- File attachments with preview capability (PDFs, code blocks)

### What bypasses the trust gate

- Plain text (always renders)
- Sender's profile picture (CID-referenced from CAS, verified by hash)
- Notification sounds (CID-referenced from CAS, verified by hash)
- Any CID-referenced content (content-addressed = integrity-verified before
  rendering)

---

## 6. Thread View & Media Interleaving

How threads interact with both panels.

### Opening a thread

The left panel splits horizontally with a draggable divider:

```
┌──────┬──────────────────────┬──────────────────────┐
│      │ Main Chat Feed       │                      │
│ Nav  │                      │   Media Feed         │
│      │ Alice: check this out│   ┌────────────────┐ │
│      │   💬 3 replies       │   │ Alice           │ │
│      │ Bob: sounds good     │   │ [github PR #42]│ │
│      ├─── drag handle ──────┤   ├────────────────┤ │
│      │ Thread: "check this" │   │ 🧵 Carol       │ │
│      │                      │   │ [screenshot]   │ │
│      │ Alice: here's the PR │   ├────────────────┤ │
│      │ Carol: screenshot    │   │ Dave            │ │
│      │ Alice: thoughts?     │   │ [OG: article]  │ │
│      │                      │   └────────────────┘ │
│      │ [reply box]          │                      │
└──────┴──────────────────────┴──────────────────────┘
```

### Thread behavior

- Parent message is highlighted/pinned in the main feed.
- Thread replies appear in the bottom split with the same compact text format.
- Thread has its own reply input box.
- Main feed remains scrollable above the divider.

### Media interleaving

- Thread media cards **interleave chronologically** with main feed cards.
- Thread cards are visually distinguished: subtle left border in the thread's
  parent color + "🧵" tag.
- When the thread closes, thread media cards **animate out** (fade or slide) and
  the feed re-flows to show only main channel media.

### Thread discovery

- Messages with replies show a compact indicator: `💬 3 replies` with mini
  avatar stack of participants.
- Clicking the indicator opens the thread in the bottom split.
- Unread thread replies show the indicator in bold with a count badge.

### Keyboard navigation

- `Esc` closes the thread and restores full-height main feed.
- `Tab` toggles focus between main feed and thread view.
- `Enter` (when thread is focused) opens the reply box.

---

## Implementation Beads

Each section maps to an independent work item. Suggested priority order:

1. **Dual-panel layout** — foundational; all other features build on this.
2. **Navigation hierarchy** — required for any multi-conversation UX.
3. **Profile system** — avatar + notification sounds; dependency for priorities.
4. **Message priority levels** — requires profile sounds infrastructure.
5. **Thread view** — extends the dual-panel layout.
6. **Media trust** — can be added last; graceful default (everything untrusted).
