# ZEB-536: Message reactions / acks — design spec

**Status:** approved direction (Jake, 2026-06-21) — multi-reaction set; reactions folded into the message DTO.
**Ticket:** [ZEB-536](https://linear.app/zeblith/issue/ZEB-536) (child of [ZEB-533](https://linear.app/zeblith/issue/ZEB-533) "Harmony fleet collaboration features"; siblings: ZEB-534 mentions ✅, ZEB-535 CAS artifacts ⏳, ZEB-537 presence).
**Branch:** `zeb-536-message-reactions` off `main@1687dd8d` (#309 — 3-member convergence + channel messaging hard-asserted).

> **Scope note.** This spec covers **Spec 1 — backend fundamentals, headless-testable**. Two follow-ups are out of scope here and get their own spec → plan cycles: **Spec 2** (Svelte reaction chips + picker + the Yes/No/Other palette) and **Spec 3** (custom/hosted emoji via the CAS work in ZEB-535). Line/symbol references below are indicative (from a codebase survey); the implementation plan pins exact locations.

---

## Goal

Let a channel member attach a **lightweight reaction** (e.g. 👍 / ✅ / 👀) to a specific message **without posting a full reply**, so the fleet can acknowledge "got it" or signal agreement **without adding chatter to the bus** (ZEB-536's stated motivation, and a natural complement to the mention-hygiene discipline). Reactions must converge across peers the same way messages do — offline, out-of-order, and through backfill — and be fully exercisable over the headless `api` surface so Ildwyn and AVALON can validate cross-WAN before any GUI exists.

### Reaction semantics (settled)

**Multi-reaction set**, keyed by `(messageId, member, emoji)`. Each member may hold **any subset** of emoji on a message and toggle each independently; a message renders each emoji with a count, the set of reactors, and whether the local member reacted.

This generalizes the ticket's shorthand (`(messageId, member) → token`, a single-slot-per-member map) by adding `emoji` to the key — a strict superset. The fleet's ack use-case is simply "one emoji per member"; nothing prevents 👍 + 🎉. Same "small CRDT, surfaced as an event, GUI later" the ticket called for.

---

## Background — how channel messages work today

Channel messages live in a **per-(community, channel) append-only signed log** (`src-tauri/src/community_channel_log.rs`), transported over Zenoh and materialized on read. The pieces a reaction reuses verbatim:

- **`enum SignedChannelEvent`** (~`community_channel_log.rs:154`) — today only the `Post` variant (fields: `id: MessageId`, `community_id`, `channel_id`, `author: OwnerAddr`, `at: Hlc`, `content_kind: u8`, `body: String`, `reply_to: Option<MessageId>`, `sig: [u8;64]`). Wire format uses **2-char CBOR keys** (`id`, `ci`, `ch`, `au`, `at`, `kd`, `bd`, `rt`, `sg`). **The original ZEB-248 design reserved a v3 `React { id→target, ci, ch, au, at, em→emoji, sg }` variant for exactly this feature.**
- **`sign_channel_event()`** (~`:300`) / **`verify_channel_event()`** (~`:656`) — Ed25519 over canonical CBOR of the signed-set; verify enforces misroute defense, a pre-auth replay check, membership-at-`at` (author `Joined`, power ≥ channel `write_power`), channel-existence, and signature against materialized enrolled device keys.
- **`encrypt_channel_packet()` / `decrypt_channel_packet()`** — ChaCha20-Poly1305, AAD `b"harmony-channel-msg-v1"`, `[nonce(12)][ciphertext]`.
- **`ChannelLogEngine`** (`src-tauri/src/community_channel_log_engine.rs`): `publish()` (~`:606`) signs → appends under the log lock → emits locally → fires the packet to the Zenoh publisher; `process_inbound_packet()` (~`:852`) decrypts → replay-checks → verifies → appends → emits. Both paths reserve a monotonic HLC via `dm_outbox::reserve_next_hlc_for_device` and dedup via `ChannelLogReplayTracker` (per `(channel, author, device)` lane).
- **`emit_message_received()`** (~`:788`) projects an event to **`ChannelMessageDto`** and emits the **`channel-message-received`** event (kebab string, `#[serde(rename_all="camelCase")]` payload) to both the `api --events` stream and the Tauri frontend.
- **RPC seam** (`src-tauri/src/api/rpc.rs`): the `rpc!()` macro registers verbs into `build_registry()`; each verb deserializes a camelCase args struct and calls a shared `*_impl` in `lib.rs` that serves **both** the headless CLI and the `#[tauri::command]` GUI invoke. A test (`registry_has_exactly_the_curated_v1_surface`, ~`:847`) pins the exact verb list.

**Reactions are not a bolt-on; they are the next variant of an existing, tested primitive.**

---

## Design overview

A reaction is a **new `SignedChannelEvent::React` variant in the same per-channel log**, flowing through the same sign → encrypt → Zenoh pub/sub → backfill → seal machinery as `Post`. Five components, all in `src-tauri/`:

1. **Wire format** — add the `React` variant + signed-set + sign/verify.
2. **Reaction index** — an incremental in-memory materialization (`(message, emoji, member) → latest (Hlc, add)`) updated as `React` events apply.
3. **DTO projection** — surface materialized reactions on the message DTO through the canonical `event_to_dto()` path.
4. **RPC verb + event** — `set_message_reaction` (add/remove) and a `channel-reaction-received` event on both local and peer paths.
5. **Tests** — TDD across wire format, CRDT convergence, RPC surface, and a two-node engine round-trip.

### Alternatives rejected

- **Parallel reaction log + separate Zenoh topic** (`…/channel/{id}/reactions`). Doubles the transport/backfill/seal code and creates a second sync path to keep consistent with the message log, for no benefit. The reserved-variant approach reuses one log.
- **Reactions as a mutable field on the `Post`.** Breaks the append-only / independently-signed model, can't be authored by a *different* member than the post's author, and loses reactions on out-of-order or backfill delivery.

---

## Component 1 — Wire format: the `React` variant

Add to `enum SignedChannelEvent` (`community_channel_log.rs`), keeping the 2-char canonical-CBOR key convention and an externally-tagged variant tag that does not collide with `Post`:

```rust
/// ZEB-536: a reaction/ack targeting a prior message in this channel.
/// Append-only: un-reacting is a fresh React event with add=false, never a
/// mutation/removal of a prior event. Convergence is last-writer-wins per
/// (target, author, emoji) by HLC — see Component 2.
React {
    #[serde(rename = "id")] target: MessageId,     // message being reacted to
    #[serde(rename = "ci")] community_id: SpaceId,  // misroute defense
    #[serde(rename = "ch")] channel_id: ChannelId,
    #[serde(rename = "au")] author: OwnerAddr,      // who reacted
    #[serde(rename = "at")] at: Hlc,
    #[serde(rename = "em")] emoji: String,          // bounded UTF-8, ≤ MAX_REACTION_EMOJI_BYTES
    #[serde(rename = "ad")] add: bool,              // true = react, false = un-react
    #[serde(rename = "sg")] sig: [u8; 64],
}
```

- **Signed-set** (canonical CBOR fed to Ed25519): `(target, community_id, channel_id, author, at, emoji, add)` — mirrors the `Post` signed-set struct, omitting only the signature.
- **`emoji`** is an opaque bounded string. v1 carries a literal Unicode emoji; a new `MAX_REACTION_EMOJI_BYTES` cap (proposed **32 bytes**) bounds it. The backend does **not** validate that the string is a "real" emoji — that, and the Yes/No/Other palette, are Spec-2 frontend concerns. Custom/hosted emoji (Spec 3) later reuse this same field via a shortcode/CAS-CID convention (e.g. `:zeb:` → content hash); **no wire change required**.
- **Encryption** is unchanged: a `React` is a `SignedChannelEvent` encrypted by the existing `encrypt_channel_packet()` (same AAD, same topic, same packet envelope) and published to the existing channel topic.
- **Sealing/backfill** are unchanged: a `React` is an event in the tail, counts toward the 1024-event seal threshold, seals into segments, and backfills with everything else.

`sign_channel_event()` / `verify_channel_event()` gain a `React` arm.

## Component 2 — Reaction index & CRDT convergence

**Convergence rule.** For each `(target, author, emoji)` triple, the reaction's presence is decided by the **single latest `React` event by HLC** (HLC total order already tie-breaks on `wall_ms`, then `logical`, then `device_id`): present iff that latest event has `add = true`. This makes toggling **idempotent and order-independent** — replaying, reordering, or backfilling events always converges to the same set.

**Materialization.** Maintain an incremental in-memory index alongside the log, updated every time a `React` event is applied (local publish *and* peer inbound):

```rust
// target message -> emoji -> member -> latest (hlc, add)
HashMap<MessageId, HashMap<String, HashMap<OwnerAddr, (Hlc, bool)>>>
```

On apply, update the `(target, emoji, author)` slot only if the new event's HLC is greater than the stored one (LWW guard). Projection for a message reads its entry and emits, per emoji with ≥1 present reactor:

```
ReactionDto { emoji: String, count: u32, mine: bool, reactors: Vec<String /*hex OwnerAddr*/> }
```

`count` = members whose latest is `add=true`; `mine` = the local owner is among them; `reactors` = those members' hex addrs (bounded; tiny for the fleet). Emoji with zero present reactors are omitted.

> An equivalent fold-over-events at list time is correct but O(events) per read; the incremental index is preferred. Rebuilds (process restart, segment reload) replay events through the same apply path, so the index is always reconstructable from the log — no separate persistence.

**Orphan tolerance.** A `React` can arrive **before** its target `Post` (backfill races, out-of-order delivery). We index it regardless; it simply does not surface until the target is present. Hard-rejecting reactions with an unknown target would silently drop valid reactions during normal sync.

## Component 3 — Verification

`verify_channel_event()`'s `React` arm applies the **same authorization gates as `Post`**:

- **Misroute defense** — `community_id` / `channel_id` match the routing.
- **Replay** — pre-auth cheap check, same tracker/lane as posts.
- **Membership at `at`** — author materialized as `Joined` with power ≥ the channel's `write_power` at the reaction's HLC. (Reacting requires the same write capability as posting — no separate "react power" in v1.)
- **Channel existence at `at`** — channel not deleted before the reaction.
- **Signature** — Ed25519 over the signed-set against the author's materialized enrolled device keys.

**Deliberately *not* a gate:** existence of the **target message**. Per Component 2 (orphan tolerance), a well-signed reaction from an authorized member is accepted even if its target hasn't arrived yet. `emoji` length is validated (≤ `MAX_REACTION_EMOJI_BYTES`); over-long fails verification.

## Component 4 — RPC verb, IPC seam, and event

**Verb** (`api/rpc.rs`): register `set_message_reaction` via the `rpc!()` macro with a camelCase args struct, and add it to the curated-v1-surface test:

```rust
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetMessageReactionArgs {
    community_id: String,   // 32 hex
    channel_id: String,     // 32 hex
    message_id: String,     // 32 hex — the target
    emoji: String,
    add: bool,              // true = react, false = un-react
}
```

**Shared seam** (`lib.rs`): one `pub(crate) async fn set_message_reaction_impl(...) -> Result<String, String>` (returns the new reaction event id, hex) plus a thin `#[tauri::command]` wrapper, registered in the Tauri `generate_handler!` list. The `_impl` validates hex ids, looks up the engine for `(community, channel)`, and calls a new `ChannelLogEngine::react(target, emoji, add)` that mirrors `publish()` (reserve HLC → sign → append+index under lock → emit → fire to publisher).

**Reads — folded into the message DTO.** Extend the message DTO with `reactions: Vec<ReactionDto>` (default empty) and populate it in the **canonical `event_to_dto()` projection** so `list_channel_messages` returns reactions inline. ZEB-538 already flags a hand-rolled duplicate projection in `get_pre_fork_snapshot`; we route reactions through the single `event_to_dto()` path rather than adding a third copy (and note `get_pre_fork_snapshot` will under-populate reactions until ZEB-538 consolidates it — acceptable, pre-fork snapshots are a separate surface). No separate `list_message_reactions` verb in v1.

**Event** (`community_channel_log_engine.rs`): a new `emit_reaction_received()` emits **`channel-reaction-received`** on **both** the local-publish and peer-inbound paths, parallel to `emit_message_received()`:

```rust
#[derive(Serialize)] #[serde(rename_all = "camelCase")]
struct ChannelReactionReceivedPayload {
    community_id: String, channel_id: String, message_id: String,
    reactor: String, emoji: String, add: bool, at: Hlc,
}
```

Frontend consumption of this event is Spec 2.

---

## Phasing

| Spec | Contents | Surface |
|---|---|---|
| **1 (this)** | `React` variant + sign/verify; reaction index + LWW convergence; `event_to_dto` reactions; `set_message_reaction` verb + `*_impl`; `channel-reaction-received` event; tests | Headless `api` + GUI invoke (no UI yet) |
| **2** | Svelte reaction chips, hover "react" button, emoji picker, the Yes/No/Other starter palette, live event wiring in `channel-message-service.ts` | Desktop GUI |
| **3** | Custom/hosted emoji via CAS (shortcode/CID in `emoji`), riding ZEB-535 | GUI + backend convention |

---

## Testing (TDD — watched-red first)

Per CLAUDE.md: `cd src-tauri && cargo nextest run --locked --all-targets --features test-fixtures`; scope during dev with `-E 'test(channel)'`/`-p harmony-app`. New + extended tests:

- **Wire format / fixtures** — `React` round-trips through canonical CBOR with stable 2-char keys; add a pinned fixture in `tests/wire_format_channel_log_fixtures.rs` (deterministic-nonce helpers behind `test-fixtures`).
- **Sign/verify** — valid reaction verifies; tampered `emoji`/`add`/`target` fails; non-member / under-powered author rejected; over-long emoji rejected; **orphan** (unknown target) **accepted**.
- **CRDT convergence** — react→unreact→react LWW by HLC; out-of-order apply converges; idempotent replay; multi-member multi-emoji counts; `mine` correctness; HLC tie-break determinism.
- **RPC surface** — `registry_has_exactly_the_curated_v1_surface` updated to include `set_message_reaction`; args deserialize from camelCase; bad-hex → `BadArgs`.
- **Engine round-trip (two-node)** — node A posts, node B reacts; assert A emits `channel-reaction-received` and `list_channel_messages` on A shows `count=1, mine=false`, on B `mine=true`; then un-react converges to `count=0`. Mirror the existing two-node channel-log integration test.

## Fleet validation (cross-WAN, headless)

Because Spec 1 is fully headless, **Ildwyn + AVALON** validate it cross-WAN over `api` before any GUI:

1. AVALON posts a message in a `#reactions-test` (or existing per-effort) channel.
2. Ildwyn `set_message_reaction {…, emoji:"👍", add:true}`; assert AVALON receives `channel-reaction-received` and `list_channel_messages` materializes `👍 count=1`.
3. AVALON reacts `✅`; both nodes converge to `{👍:1, ✅:1}` with correct `mine`.
4. Ildwyn toggles `add:false`; converges to `{✅:1}`.

This doubles as the **AVALON local-dev exercise** (the "less-exercised node" goal): a full `cargo build` + headless serve loop on AVALON. The known ZEB-519 `/STACK` repo-root build wart is avoided by building from `src-tauri/`; any new AVALON-specific speed bumps get filed as they surface.

## Non-goals (v1)

- No GUI (Spec 2). No custom/hosted emoji (Spec 3).
- No separate "react power" — reacting requires channel `write_power`, same as posting.
- No emoji-validity enforcement beyond a length cap.
- No reaction notifications/mentions (orthogonal; ZEB-534 owns targeted wake).
- No per-reaction rate limiting beyond what the existing publish path already imposes.

## Open questions

1. **`MAX_REACTION_EMOJI_BYTES`** — 32 bytes proposed (room for ZWJ sequences + a short shortcode). Confirm during planning.
2. **`reactors` in the DTO** — included for v1 (tiny fleet). If a channel ever has many members this list grows; a future cap/opt-in can trim it. Flagged, not blocking.
