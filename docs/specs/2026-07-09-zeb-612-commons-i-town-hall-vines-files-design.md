# ZEB-612 — Commons I: Town Hall (net-new) + Vines/Files restyle — design

**Ticket:** ZEB-612 (last remaining child of the Commons epic ZEB-603; shipping it closes the epic).
**Approved by Jake:** 2026-07-09 (scope decisions + design sections, this session).
**References:** `docs/design/commons/references/Harmony Town Hall.dc.html` (TH), `Harmony Vines Feed.dc.html` (VFI, interactive), `Harmony Vines & Files.dc.html` (VF, static), `docs/design/commons/tokens.css`, `docs/design/commons/ADOPTION.md`.
**Sibling pattern:** ZEB-649 (Commons F), ZEB-650 (Commons G) — ground-truth-first, honest-data-only, sliced PRs.

## §0 Ground truth and scope decisions

Exploration (4 parallel surveys, 2026-07-09) established:

1. **VoiceChannelView** (`src/lib/components/VoiceChannelView.svelte`) already ships every state the design draws: join-muted pane, inline PTT with Space hotkey (`:68-69,103-114,247-263`), mute/Live + deafen + leave bar (`:264-297`), channel-full soft-cap bounce (`VOICE_CHANNEL_SOFT_CAP=64`, `voice-session.ts:22,606-629`), mic-blocked listen-only, reconnecting badge, self-mod-muted/self-kicked notes (`:228-237`), roster grid ≤12 → list (`GRID_MAX=12`), mod mute/kick gated `selfPower>=50 && selfPower>target` (`:75-92`), local speaking indicators. **S1 is a faithful restyle, no behavior change.**
2. **Raise-hand / speaker queue / invite-to-speak / quorum-present detection do not exist.** Seams: `harmony/voice-control/{c}/{ch}` is a production-proven device-signed, power-gated, LWW+TTL directive bus (`voice_moderation.rs`); `VoicePresenceBeacon` has an established additive-optional-field pattern (`left`, `voice_presence.rs:32-34`); Tier-1 polls are channel-scoped, live-tallied, quorum-parameterized, and openable programmatically from any surface (`voting-adapter.ts:617-630`). `votingReady` in App.svelte is adapter-wiring state, **not** a quorum signal. Static quorum config: `getCommunityGovernance().adminQuorum` (`community-service.ts:595-598`).
3. **Vines:** Following/Discover tabs, All/Unviewed + "N new", reactions (hearts), reshare-with-attribution, unviewed-dot/viewed-dim all exist with real data. Viewed-state is persisted in Rust (`vine_feed_cache.rs:171,315,597`) but the frontend never hydrates it (`vine-service.ts` never calls `list_vine_videos`). No duration field, no loop/play counts, no in-feed playback (cards are gray `▶` boxes; blob playback lives only in `VinePlayer.svelte`), no trim, no delete verb. The two design files specify different feed models — interactive (scroll-snap autoplay) vs static (viewed/unviewed list).
4. **Files:** components and TS types all exist, but live data is fabricated: `replicaCount` hardcoded 1 (`file-manager-service.ts:137-160`) — so ReplicationStatus shows fake "under-replicated" warnings today — storage buddies / sharedWith / origin are mock (`:240-296`), quota total is a hardcoded 10 GB (`:180`). CID + size are real end-to-end; CID is never displayed. `replication_tier` is a stored *target*, never an observed count. `StorageBudget` exists in Rust (`lib.rs:9074`) but has no IPC.

**Jake's four scope decisions (2026-07-09):**

* **Town Hall: full honest build** — new backend primitives on the proven seams (not a restyle-only or no-backend subset).
* **Vines: interactive full-bleed feed** (the ticket's own spec), merging the static design's *real* data elements; Discover degree-chips/Tune sheet stay out (transitive-follows is a separate product model → spin-off ticket).
* **Files: real replication + gated buddies** — build observed-holders counter + quota IPC; remove/gate every mock-backed surface; buddies domain spun off.
* **Town Hall identity: new `townhall` channel kind** in the community CRDT. One channelId scopes voice presence/control topics, the backchannel message log, and Tier-1 motions.

## §1 Slicing — five sequential PRs

| Slice | Content | Nature |
|---|---|---|
| S1 | VoiceChannelView faithful restyle | frontend-only |
| S2 | Vines: interactive feed + publish dialog + viewed-rehydrate gap-fix | frontend + adapter wiring |
| S3 | Files: observed-holders counter + quota IPC + restyle + de-mocking | small backend + frontend |
| S4 | Town Hall backend: `townhall` kind, `hand` beacon field, invite-to-speak directive, IPC | backend + wire fixtures |
| S5 | TownHallView frontend | frontend over S4 |

One PR per repo at a time; each converges (CI + bots) before the next opens. S1–S3 deliver standalone value. The spec (this file) and each slice's plan ride the slice's own PR.

## §2 S1 — VoiceChannelView restyle

Faithful Commons treatment of the shipped component; **zero behavior change**. Anatomy and copy from TH frames B/C:

* **Join pane (idle):** 🔊 glyph, channel name, accent **"Join Voice"** button, copy *"You'll join muted — unmute when you're ready."*
* **PTT held:** full-width accent button *"🎙 Transmitting… (hold Space)"* with accent glow ring; helper copy *"Release to go quiet. Replaces the mute toggle while PTT mode is on."*
* **Mod-silenced note:** clay-danger surface (`--status-recalled-bg` family), 🛡, *"You've been muted by a moderator. Your talk controls are disabled until they unmute you."*
* **Channel-full bounce:** `--gov-clay-soft` surface, *"Voice channel full — try again later."*
* **Roster tiles:** speaking ring = double box-shadow in `--accent`; 🔇 "muted" badge; 🛡 "mod-muted" badge on `--status-recalled-bg`; hover reveals mod Mute/Remove per existing power gating.
* **Control bar:** 🎙 Live / PTT / 🎧 Deafen / Leave; Leave in `--danger`.
* Type/idiom: uppercase `--faint` section labels, mono (`--font-mono`) for counts; radii — badge/chip **pills** fully rounded (20px+/999px), **buttons/inputs** 5px, **cards/banners** 8px.

Existing tests pin behavior; restyle updates class/DOM assertions only where selectors move. style-token-guard budget-0 (existing `var(--*)` only).

## §3 S2 — Vines

### Feed (`VineFeed.svelte` rework + `VineCard` full-bleed variant)

* **Layout:** vertical scroll-snap (`scroll-snap-type: y mandatory`; cards `snap-align: start; snap-stop: always`), full-bleed dark cards per VFI.
* **Autoplay:** rAF-throttled center-distance handler selects the single card nearest viewport center → `playingId`; that card plays (muted-loop `<video>` via existing `resolveVideo(cid)` blob URLs), all others pause and dim (`brightness(.82)` + ❚❚ glyph). Lazy-load only playing card ± neighbors; revoke blob URLs beyond that window. No endless-cycling (`maybeAppend` in VFI cycles mock data); real feed ends in the existing "all caught up" state.
* **Real-data merge (from the static design):** Following/Discover tabs (Discover stays today's flat list); All/Unviewed filter + clay "N new" pill on Following; clay unviewed dot; viewed cards dim additionally when paused; reshare attribution *"↻ {name} reshared · view original by {orig}"* (real `reshareOf`/`originalCreator*` via `vine-utils`); ♥ + like count (real reactionMap); ↻ + reshare count (real `reshareCountMap`).
* **Viewed semantics:** a card is marked viewed (`mark_vine_viewed`) when it becomes the playing card.
* **Duration badge:** mono pill "↻ 0:06" read from the video element's `loadedmetadata` at render — honest, no backend.
* **`playTarget` / view-original:** scrolls the feed to the target vine (feed is now the player). `VinePlayer.svelte` overlay is retired from the feed flow; component deleted only if nothing else references it (verify at plan time).
* **Gap-fix:** `VineService` hydrates from `list_vine_videos` at startup so the Rust-persisted viewed-set survives restart (today `viewedIds` is session-only).

### Publish dialog (`VinePublishDialog.svelte` restyle)

* Header *"Share a vine"*, subtitle *"≤ 6 seconds · loops forever"*; Commons drop-zone/caption-field anatomy (VF 172-191); caption stays ≤140 with counter; "max 100 MB" hint stays; advanced paste-a-CID disclosure stays.
* **Honest ≤6s gate (client-side, honest-client posture — same as voice moderation):** after ingest returns a CID, resolve the blob and read metadata duration. >6.0s blocks publish: *"This clip is {X.X}s — vines are 6 seconds or less. Trim it and re-ingest."* ≤6.0s shows *"{filename} · {X.X}s ✓ · ingested to content store"*. **No fake trim UI** (the reference's trim bar is a static indicator; no trimming exists).
* **Sovereign note reworded to true claims only:** 🔑 *"Publishes to your sovereign identity and replicates peer-to-peer. There's no central server to take it down."* The "only you can delete it" line returns when a delete verb exists (spin-off §7.2).

## §4 S3 — Files

### Backend (new, well-seamed)

* **Observed-holders counter:** Rust-side per-CID set of distinct announcing peers (fed from the content-announced flow that already exists), staleness-pruned, self counted. Exposed as `replica_count` on `ContentItemWire` (`list_content`/`list_root`). This is an *observed lower bound* — copy must say "copies **seen** across your peers".
* **Quota IPC:** expose the existing `StorageBudget` (`harmony_content::storage_tier`) totals via a new query command; `getQuotaStatus` uses the real total (used-bytes already real).

### Frontend

* `wireToContentItem` stops fabricating: real `replica_count`; hardcoded `stalenessScore`/`accessCount`/`lastAccessed` removed **along with their renderers** (StalenessIndicator, access-count/last-accessed rows) until real data exists.
* **CID surfaced:** truncated mono `cid:bafy…7f3a` in rows; full CID (word-break) + **⧉ Copy CID** in the detail panel.
* **Replication chips:** dot + "×N healthy" (`--accent`) / "×N at risk" (`--gov-clay`) against the real `tierTarget` map (expendable 1 / light 2 / default 3 / high 5 / ultra 9, `file-utils.ts:13-19`); the `underReplicated` filter becomes truthful. Detail panel replication box (sage): "×N · copies seen across your peers · Above/Below the ×{target} target…".
* **"Used by N vines":** computed client-side from real vine→`videoCid` references.
* **De-mocked:** StorageBuddyList + contribution meter, ShareList (`sharedWith`), and origin row are removed from the panel (mock-only today) → spin-off §7.1. `getContentDetail` returns only real fields.
* Browser chrome per VF 195-255: Storage nav (All files / Videos / Images / Documents / Pinned by me), toolbar search *"Search files or paste a CID…"*, "⤓ Add files", grid/list toggle, Name/Size/Replication columns.

## §5 S4 — Town Hall backend

* **Channel kind:** community CRDT channel-kind gains `townhall` (additive). Creation UI offers Text / Voice / Town Hall. Stale-client fallback for an unknown kind follows the existing deserialization posture — verify at plan time and pin a safe behavior (must not crash; degrading to a joinable voice room or an inert row are both acceptable).
* **Raise-hand:** `VoicePresenceBeacon` gains `hand: Option<u64>` (wall-clock ms when raised; absent = lowered), `#[serde(default, skip_serializing_if = "Option::is_none")]` — exactly the `left` pattern (`voice_presence.rs:32-34`). Republished by the ≤4s heartbeat; surfaced through `RosterEntry` → `RosterMember.handRaisedAt`. New IPC `set_voice_hand(raised: bool)`.
* **Speaker queue is derived, not stored:** roster members with `hand` set, ordered by raise timestamp (tiebreak: owner hex). Deterministic on every client; no new store, no sync protocol.
* **Invite-to-speak:** new directive variant on the voice-control topic, reusing the signed-directive machinery. Authority: actor power ≥ 50 (the `actor>target` clause is punitive-action logic and does **not** apply to the benign invite). TTL-boxed (an unclaimed invite expires, ~2 min; exact constant at plan time alongside the existing TTL discipline constants). Surfaced via the moderation overlay event (`invitedOwners`, `selfInvited`). The target's own client lowers the hand on accept (unmute) or dismiss — the hand is a target-owned presence field; the invite never mutates another owner's beacon.
* **Wire fixtures:** `tests/wire_format/voice_fixtures.rs` extended both directions (old beacon decodes; new beacon + invite directive round-trip).
* **Quorum: no new backend.** "Present" = distinct owners in the live roster; "required" = `adminQuorum`.

## §6 S5 — TownHallView frontend

New `src/lib/components/TownHallView.svelte`, rendered by `CommunityView` for `kind === 'townhall'`; reuses `VoiceSession` (join/PTT/mute/deafen/mod) plus S4's hand/invite state. Anatomy per TH frame A:

* **Header:** room name · LIVE dot when roster > 0 · present count. The design's agenda line renders the channel topic if the channel model carries one (verify at plan time), else is omitted. The design's global meeting timer ("⏱ 32:14") is omitted — no room-start record exists (§8 ledger).
* **Spotlight ("On the floor"):** dominant speaker from the existing local speaking inference (fallback: most recent speaker; empty state "No one has the floor"). Double-ring avatar, name, MOD pill when power ≥ 50 (real powers map), "🎙 speaking", **elapsed-only** local speaking timer (the design's "/ 3:00" cap implies a speak-limit policy that doesn't exist). Waveform: decorative bar cluster animated from speaking state, `prefers-reduced-motion` honored. **No transcript quote** — no transcription exists.
* **"In the room · N" grid:** speaking rings, ✋ raised-hand badge (`--gov-clay`, real beacon data), 🔇 muted badges, mono "+N more" overflow tile (18 visible).
* **Right rail ("Floor"), top to bottom:** speaker queue → motion card → backchannel.
  * **Speaker queue:** numbered rows (index 1 clay, rest faint), 26px avatar, name, *"wants to speak ✋"*; **Invite** button (accent-filled #1, outline rest) — visible to all, actionable at power ≥ 50; invited entries dim with "invited".
  * **Motion card:** idle → "⚖ Call this to a motion" affordance (title input) for members meeting the existing Tier-1 poll-creation gate. On call: if present ≥ quorum → creates a 5-minute-window (300 s) Tier-1 poll on this channelId and renders it live in-card (TallyBar + CountChip + vote buttons — this specifies the live-vote variant the reference leaves as prose); else → the drawn DRAFT card: `--gov-clay` DRAFT badge, "live · N present" meta, provenance line, quorum bar (`--tally-track` + `--gov-clay` fill, mono "N / M"), copy *"Not enough present for a live vote — open it as a 48-hour async proposal instead."* → creates the standard async proposal and links to the proposals view.
  * **Backchannel:** the existing `ChannelMessageFeed` on this same channelId, compact, composer placeholder *"Message the room…"*.
* **Self affordances:** ✋ Raise hand toggle in the control bar; when invited: banner *"You've been invited to speak — Unmute?"* with accept (unmute + lower hand) / dismiss (lower hand).
* **Not-joined state:** join pane variant — *"Join the assembly — you'll join muted."*

## §7 Spin-off tickets (file during this work; use assigned IDs, never invented ones)

1. **Storage-buddies domain** — real hosting accounting (who hosts what for whom, contributed bytes), contribution meter, sharing model (`sharedWith`) — restores the gated Files surfaces.
2. **`delete_vine` creator tombstone** — signed retract on the creator topic + cache eviction; restores the "only you can delete it" sovereign copy.
3. **Discover = transitive follows** — 2nd/3rd-degree graph, degree chips, provenance paths, Tune sheet (the ticket explicitly requires filing this).

## §8 Honesty ledger (deviations from the drawn references)

| Reference element | Treatment | Why |
|---|---|---|
| Spotlight transcript quote | omitted | no transcription exists |
| Speaking timer "2:40 **/ 3:00**" | elapsed-only, no cap | no speak-limit policy exists |
| Header meeting timer "⏱ 32:14" | omitted | no room-start record |
| "▶ N loops" count | omitted | nothing counts plays |
| Duration badge | client metadata at render | not stored; honest source available |
| Trim UI / "Trimmed to 6s ✓" | honest-client ≤6s publish gate; no trim chrome | no trimming exists |
| "only you can delete it" | reworded until delete verb ships | no delete verb (§7.2) |
| Storage buddies + meter, ShareList, origin, staleness, access counts | removed/gated | mock/fabricated today (§7.1) |
| Replication "×N healthy" | real observed lower bound, "copies **seen**" | counter observes announcements, not global truth |
| Discover degree chips / Tune / provenance paths | out of scope | separate product model (§7.3) |
| Town Hall "23 present" | live voice roster (distinct owners) | the only honest present-now signal |

## §9 Testing & gates

Per slice: `npx tsc --noEmit` + `npx vitest run`; style-token-guard budget-0; `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; targeted `cargo nextest` per task; wire-format fixture coverage for every beacon/directive change; final full sweep `cargo nextest run --locked --workspace --all-targets --features test-fixtures` before each PR. Warm-dark falls out of the token system (no per-surface dark work). Component tests follow the established dialog/feed idioms (mock `@tauri-apps/api/core`, camelCase DTO keys, real timers + `waitFor` for debounce/observer behavior).
