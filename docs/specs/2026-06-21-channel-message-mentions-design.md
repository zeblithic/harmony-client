# Channel-message mentions (ZEB-534) — design

**Goal:** let a community channel message carry a structured list of mentioned
members (owner-ids), so a recipient can tell it was addressed. This is the
first-class, `@`-free successor to the interim `@Name` body convention
(fleet-coordination-over-harmony protocol, Track 1): senders pass a
machine-readable `mentions` list; recipients derive "mentions me" locally and a
listener (or GUI) can wake/notify selectively instead of on every message.

Parent epic: ZEB-533. Builds on the real-time event-driven listening layer.

## Design

Add an **optional** `mentions` field to the signed channel-message event,
mirroring the existing optional `reply_to` field exactly. Mentions are
owner-ids (durable, name-independent); the *sender* resolves `@Name` → owner-id
at post time (via member cards / the known fleet map), and the message body may
still contain the `@Name` text for human display.

### Type: `SignedChannelEvent::Post` (`community_channel_log.rs:154-192`)

Add, with the 2-char CBOR key `"mn"` (RFC 8949 canonical order places it
between `kd` and `rt`):

```rust
#[serde(rename = "mn", skip_serializing_if = "Option::is_none", default)]
mentions: Option<Vec<OwnerAddr>>,
```

`OwnerAddr` (`owner_state_types.rs:365`) is the 16-byte address already used for
`author`. Same change to the pre-signature `ChannelPostPayload`
(`community_channel_log.rs:197`) and the signed-set `ChannelPostSignedSet`
(`community_channel_log.rs:226`, same key/position) so mentions are **inside the
signature** (tamper-evident, consistent with `kind`/`reply_to`).

### Wire compatibility — NOT a hard flag-day

Because `mentions` is `skip_serializing_if = Option::is_none` in the signed set:

- **`mentions: None` → the `mn` field is omitted → canonical CBOR is
  byte-identical to a pre-feature message → identical signature.** A mention-less
  message produced by the new code verifies on *both* old and new clients. The
  existing wire-format pin (`tests/wire_format/channel_log_fixtures.rs`,
  `signed_channel_event_post_wire_bytes_pinned`) should stay unchanged — only the
  struct *construction* needs `mentions: None` added (compile), not the expected
  hex. **Verify this empirically** when implementing (re-run the pin; it must
  still pass).
- **`mentions: Some([..])` → `mn` is present → new signed bytes.** An *old*
  client reconstructs the signed set without `mn` and so rejects a mention-bearing
  message (signature mismatch). New clients verify it fine.

**Rollout requirement:** ship the new binary to all fleet nodes + the GUI
*before* anyone sends a message with mentions. Since we control all participants
(3 fleet nodes + GUI) and upgrade together, there is no real-world break; until
everyone's upgraded, just don't populate `mentions`. Add a new wire-fixture pin
for a *mention-bearing* `Post` (new hex) so the populated path is also pinned.

### Materialize → DTO → event

- `message_dto_for_event()` (`community_channel_log_engine.rs:809`): extract
  `mentions` from the event, hex-encode each `OwnerAddr`.
- `ChannelMessageDto` (`community_channel_log_engine.rs:128`): add
  `#[serde(skip_serializing_if = "Option::is_none")] pub mentions: Option<Vec<String>>`
  (camelCase `mentions` on the wire; hex owner-addr strings).
- The `channel-message-received` event payload
  (`ChannelMessageReceivedPayload`, `…:191`) carries the full DTO, so `mentions`
  rides through automatically. **No server-side "mentionsMe" flag** in v1 — the
  list is enough; listeners and the GUI derive "mentions me" locally
  (`self ∈ mentions`). (Koya's Monitor already does the analogous check on the
  `@Koya` text; it switches to `self ∈ mentions` once this lands.)

### Post path (RPC → engine)

- `PostChannelMessageArgs` (`api/rpc.rs:145`): add
  `mentions: Option<Vec<String>>` (hex owner-addrs).
- Tauri `post_channel_message` command (`lib.rs:19948`) + `post_channel_message_impl`
  (`lib.rs:19960`): add the `mentions` param; parse each hex string → `OwnerAddr`
  (reject malformed), validate bounds, pass through.
- `ChannelLogEngine::publish()` (`community_channel_log_engine.rs:606`): accept
  `mentions: Option<Vec<OwnerAddr>>`, validate, thread into `ChannelPostPayload`.
- Frontend `ChannelMessageDto` (`src/lib/channel-message-service.ts:9`): add
  `mentions?: string[];`.

### Validation / bounds

- New const `MAX_MENTIONS` (start at 64) next to `MAX_BODY_BYTES`
  (`community_channel_log_engine.rs:326`).
- New error `ChannelLogEngineError::TooManyMentions { count, max }`
  (alongside `BodyTooLarge`, `…:58`); validate in `publish()` before building the
  payload, and parse/shape-validate hex in `post_channel_message_impl`.
- **Membership-gating** (mentions must be community members) is **out of scope
  for v1** — recipients simply check whether they're in the list; gating is a
  later refinement.

## Sender-side `@Name` resolution

- **Agents:** construct `mentions` explicitly from owner-ids they already know
  (the fleet map / member cards); the body keeps the readable `@Name` text.
- **GUI (later):** parse `@Name` tokens in the compose box → resolve to owner-ids
  via cached member cards → send `body` + `mentions`. GUI render/notify is a
  follow-up, not part of this spec.

## Test plan

1. **Unit:** sign + verify a `Post` with `mentions: Some([..])` round-trips and
   verifies; a `mentions: None` post verifies and is byte-identical to the
   pre-feature encoding (guards the no-flag-day claim).
2. **Wire fixtures:** the existing mention-less pin still passes unchanged; add a
   new pin for a mention-bearing `Post`.
3. **DTO round-trip:** event → `message_dto_for_event` → DTO carries hex mentions;
   camelCase `mentions` serialization.
4. **Bounds:** `> MAX_MENTIONS` → `TooManyMentions`; malformed hex → clean error.
5. **(Optional) e2e:** post with `mentions` cross-node; recipient's
   `list_channel_messages` + `channel-message-received` carry the list; recipient
   derives `mentionsMe`.

## Out of scope (v1)

- `@everyone` / `@here` broadcast mentions.
- GUI rendering + desktop/notification affordance.
- Membership-gating of mention targets.
- A server-computed `mentionsMe` flag (derived client-side instead).

These are natural follow-ups once the structured field + event surface land.
