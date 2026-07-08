# ZEB-662 — Mention notifications (MVP slice) — design

**Status:** approved 2026-07-08 (Jake). Branch: `zeb-662-mention-notifications-mvp`.
**Ticket:** [ZEB-662](https://linear.app/zeblith/issue/ZEB-662) — spun out of the shipped [ZEB-588](https://linear.app/zeblith/issue/ZEB-588) wire-format/resolution core.

## Goal

When an incoming **channel** message @-mentions the viewer, actually *notify* them — a nav mention indicator plus a focus-aware toast / OS-notification — instead of only the passive in-feed row highlight that already exists. Honor the existing per-scope notification policy, and make notification settings survive a restart.

## Why this is a small slice

The epic is ~60% pre-built (verified 2026-07-08). What already exists and is reused unchanged:

- **Policy engine** — `NotificationService.resolve(priority, peerAddress, communityId) → NotificationAction` (`src/lib/notification-service.ts`), precedence peer → community → global. Actions: `silent | dot_only | notify | sound | break_dnd`. Priorities: `quiet | standard | loud`. Default global policy: `{quiet: dot_only, standard: sound, loud: break_dnd}`.
- **Settings UI** — `NotificationSettingsPanel.svelte`, mounted via `SettingsPanel.svelte:176` (Global / Communities / Peers tabs).
- **Delivery primitives** — the `Toast` / `ToastHost` / `stores/toast.ts` system; `tauri-plugin-notification` (Cargo + `notification:default` capability + `@tauri-apps/plugin-notification` JS dep); the **focus-aware escalation** pattern in `src/lib/incoming-call-alert.ts` (toast when focused, OS-notification when the window is unfocused, via injected `isFocused()` / `sendNotification()`).
- **Detection primitive** — the `<@([0-9a-f]{32})>` tokenizer in `src/lib/mention-render.ts` (`tokenizeBody`).

The three real gaps this slice closes:

1. **Nothing classifies a mention** — `message-service.ts` hardcodes every message to `priority: 'standard'`. A self-mention is never elevated to `loud`.
2. **The resolver's output is never acted on** — `.resolve()` has zero delivery callers; the engine is wired to the settings UI but not to incoming messages.
3. **Settings don't persist** — `NotificationService.settings` are in-memory `Map`s (no load/save); every configured policy resets on restart. There is also no nav mention indicator (`NavNode.unreadCount` exists but is never incremented).

## Key decisions (approved)

- **Viewer-relative priority.** Mention priority is computed **receiver-side** (does this message's body contain *my* `<@ownerId>`?), never from the sender-set wire `priority` field — a message mentioning Alice is `loud` for Alice, `standard` for Bob.
- **Policy authority = the frontend `NotificationService`.** The backend CRDT field `Space.notification_pref { All, Mentions, Muted }` (`owner_state_types.rs:1831`) is **left untouched** this slice; unifying the two models for cross-device settings sync is a later slice.
- **Scope = channel mentions only.** No general per-message unread, no DM notifications this slice.
- **Persistence = owner-scoped localStorage** (settings survive restart), mirroring the ZEB-586 owner-scoping pattern. Mention **counts** are **session-ephemeral** (clear-on-view, reset on restart); durable mention history is the future "Mentions inbox" slice.
- **`sound`/`break_dnd` ≡ `notify` delivery** this slice — there is no in-app notification-sound primitive in the codebase, so those actions deliver the same toast/OS-notification as `notify` (the OS notification carries the system sound when unfocused). An explicit in-app chime, custom per-sender CAS sounds (`resolveSoundCid`), and a real DND state for `break_dnd` to break are all deferred.
- **Frontend-only slice** — no Rust/CRDT changes (`mentionCount` is client-derived).

## Architecture

A new dep-injected, focus-aware `MentionAlertService` subscribes to incoming channel messages, detects viewer self-mentions, classifies them `loud`, resolves the action via the existing `NotificationService`, and drives delivery across the existing rails plus a new nav mention indicator. It mirrors `incoming-call-alert.ts`: all side-effecting capabilities (`isFocused`, `sendNotification`, toast push, `resolve`, `getActiveChannel`, nav mutators) are injected, so the service is deterministic and unit-testable with no real OS, timers, or window.

### Data flow

`ChannelMessageService` already receives `channel-message-received` (payload `ChannelMessageReceivedPayload { communityId, channelId, message }`) and fans it into `ingest(...)`. We add a `onChannelMessage(communityId, channelId, message)` callback hook on that service (mirroring its existing `onBackfillProgress`) — a **single** event source, no parallel `adapter.listen`. `MentionAlertService.onMessage(communityId, channelId, message)` runs:

1. If `!bodyMentionsOwner(message.body, myOwnerId)` → return (not a self-mention).
2. If `channelId === getActiveChannel() && isFocused()` → treat as seen; return (the viewer is looking at it).
3. `action = resolve('loud', message.author, communityId)`. If `action === 'silent'` → return.
4. Always (i.e. `dot_only` and above): `nav.incMention(channelId)` (increments the channel node and bubbles a count to its community node).
5. If `action ∈ {notify, sound, break_dnd}`: `isFocused()` → push an in-app toast (routes to the channel on click); else → `sendNotification({ title, body })` (OS — carries the system's default notification sound).

There is intentionally no separate step for `sound`/`break_dnd`: with no in-app notification-sound primitive in the codebase (the only audio is real-time voice), those actions map to the **same delivery as `notify`** for this slice. The OS notification's own system sound covers the unfocused-audible case; an explicit in-app chime + custom CAS sounds are a later slice.

**Clear-on-view:** `handleNodeSelect(channelId)` (`App.svelte:2763`) calls `nav.clearMention(channelId)` and recomputes the community bubble.

### Components / files

| File | Change |
|---|---|
| `src/lib/mention-detect.ts` | **New.** `bodyMentionsOwner(body: string, myOwnerIdHex: string): boolean` — reuses the `<@hex>` tokenizer (extract a shared matcher from `mention-render.ts` to avoid a second regex). Pure, no deps. |
| `src/lib/mention-alert.ts` | **New.** `MentionAlertService` — the classify → gate → deliver logic above, all capabilities injected. Pure logic + injected effects. |
| `src/lib/notification-service.ts` | Add `serialize(): string` and `load(raw: string): void` (round-trips `global` + the four `Map`s); call a save hook from every setter. |
| `src/lib/notification-settings-persistence.ts` | **New (small).** Owner-scoped localStorage load/save: key `harmony:notif-settings:<ownerIdHex>`; load-on-boot, save-on-change. Mirrors the ZEB-586 owner-scoping. |
| `src/lib/types.ts` | `NavNode.mentionCount: number` (distinct from `unreadCount`). |
| `src/lib/nav-service.ts` | Initialize `mentionCount: 0`; preserve across rebuilds (like `unreadCount` at lines 214/238/307/327); add `incMention(channelId)` (bubbles to community) + `clearMention(channelId)`. |
| `src/lib/channel-message-service.ts` | Add an `onChannelMessage?(communityId, channelId, message)` callback (mirroring `onBackfillProgress`), invoked from the existing `channel-message-received` handler after `ingest`. Single event source for the alert service. |
| `src/lib/components/NavPanel.svelte` (+ channel/community row markup) | Render a mention badge/dot when `mentionCount > 0` (Commons idiom: `CountChip` where a count fits, dot where space is tight). |
| `src/App.svelte` | Instantiate `MentionAlertService` wired to `channelMessageService.onChannelMessage`, `activeChannel`, the focus deps (same source `incoming-call-alert` uses), `notificationService`, and the nav mention mutators; load notification settings on boot; call `clearMention` in `handleNodeSelect`. |

### Error handling

- **Missing owner identity** (pre-login / race): `bodyMentionsOwner` returns `false` for an empty/absent `myOwnerId` → no misfires.
- **OS-notification unavailable / permission denied**: `sendNotification` is wrapped in try/catch (as in `incoming-call-alert.ts:116`); a failure degrades to the nav dot + (if focused) toast, never throws into the message pipeline.
- **Corrupt persisted settings**: `load(raw)` is defensive — a parse/shape failure logs and falls back to `DEFAULT_POLICY`, never wedging boot.
- **Focus query failure**: mirror `isFocusedSafe()` — default to treating the window as focused (safe: prefer an in-app toast over an OS push on uncertainty).

## Testing

Vitest units (frontend-only slice; Rust untouched):

- `mention-detect.test.ts` — mentions me / mentions someone else / no tokens / empty body / empty ownerId / multiple tokens incl. me.
- `mention-alert.test.ts` — the full matrix: not-a-mention → no-op; active+focused channel → suppressed; each resolved action (`silent` → nothing; `dot_only` → nav inc only; `notify`/`sound`/`break_dnd` → nav inc + (toast when focused / OS-notif when unfocused)) → asserts exactly which of {nav inc, toast, OS-notif} fire; per-peer/community override beats global; OS-notif throw is swallowed.
- `notification-service` persistence — `serialize()`/`load()` round-trip incl. all four Maps; corrupt input → defaults.
- `notification-settings-persistence.test.ts` — owner-scoped key; load-on-boot; save-on-setter; owner switch does not leak the other owner's settings.
- `nav-service` — `incMention` bubbles to community; `clearMention` zeroes + recomputes bubble; `mentionCount` preserved across rebuild.

**Gates:** `npx tsc --noEmit` + `npx vitest run` + `style-token-guard` (new nav badge must use `var(--*)` tokens only). No `cargo` gate needed unless a backend field is added (it is not).

## Out of scope / follow-up slices

1. **CRDT-backed cross-device settings sync** — unify `NotificationService` policy with `Space.notification_pref`; settings follow the owner across devices.
2. **DND / quiet-hours** — a real DND state for `break_dnd` to break.
3. **Cross-community Mentions / Activity inbox** — durable, cross-restart mention history (needs a per-space read cursor / HLC watermark).
4. **General per-message unread** — wire `NavNode.unreadCount` for all messages (distinct from mentions).
5. **DM notifications** — route DMs through the same delivery rails.
