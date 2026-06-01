# Voice V1 — Channel Typing (`kind: Text|Voice`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give community channels a `kind` (`Text` | `Voice`) so members can create 🔊 voice channels alongside `#` text channels — additive CRDT wire, dialog/sidebar/routing — shipping a joinable voice-channel *scaffold* (no audio yet).

**Architecture:** Mirror ZEB-345's `profile_page_root` additive-field pattern: a new `ChannelKind` enum serialized as a `u8` CBOR value under a 2-char key `"ck"`, `skip_serializing_if = "ChannelKind::is_text"` + `default`, so a **Text** `ChannelCreate`/`ChannelInfo` stays *byte-identical* to pre-change wire (existing fixtures untouched) and only **Voice** carries the extra map entry. Kind is immutable (ChannelModify can't touch it — already true by construction). Frontend threads `kind` through `community-service` → dialog selector → sidebar glyph → `CommunityView` routing to a new `VoiceChannelView.svelte` scaffold.

**Tech Stack:** Rust (Tauri commands, serde + ciborium canonical CBOR), TypeScript, Svelte 5 (runes), vitest + @testing-library/svelte.

**Spec:** `docs/specs/2026-05-31-voice-comms-design.md` §V1 (committed `cf64a7e`). Epic ZEB-348; this is ZEB-349.

---

## File Structure

**Rust (`src-tauri/`):**
- Modify `src/community_membership.rs` — add `ChannelKind` enum; add `kind` to `ChannelCreate` (variant) + `ChannelInfo` (materialized); set `kind` in the `ChannelCreate` materialize arm.
- Modify `src/lib.rs` — `create_channel` gains `kind: Option<String>` param; `mint_channel_create_event` gains a `ChannelKind` arg; `ChannelInfoDto` gains `kind: String`; `list_channels` maps it.
- Modify `tests/wire_format_community_fixtures.rs` — pin a Voice `ChannelCreate` fixture; keep the existing (Text) fixture byte-identical; add a `ChannelKind` round-trip + immutability unit assertions (the CRDT unit tests live inline in `community_membership.rs`).

**Frontend (`src/`):**
- Modify `lib/community-service.ts` — `ChannelInfo` gains `kind: 'text' | 'voice'`; `createChannel` gains a `kind` arg.
- Modify `lib/components/CreateChannelDialog.svelte` — Text/Voice segmented control.
- Modify `lib/components/ChannelSubSidebar.svelte` — 🔊 glyph branch.
- Create `lib/components/VoiceChannelView.svelte` — V1 scaffold (header + roster placeholder + disabled Join).
- Modify `lib/components/CommunityView.svelte` — route `kind === 'voice'` to `VoiceChannelView`.
- Modify/add tests under `lib/components/__tests__/` and `lib/__tests__/`.

---

## Task 0: Baseline — confirm green before touching anything

**Files:** none (verification only)

- [ ] **Step 1: Run the Rust test suite**

Run (from `src-tauri/`):
```bash
cargo nextest run --locked --features test-fixtures -E 'test(channel)' 2>&1 | tail -20
```
Expected: all channel-related tests PASS (this is the pre-change baseline; the `signed_event_channel_create_wire_bytes_pinned` fixture passes).

- [ ] **Step 2: Run the frontend baseline**

Run (from repo root):
```bash
npx vitest run src/lib/components/__tests__/CreateChannelDialog.test.ts src/lib/components/__tests__/ChannelSubSidebar.test.ts 2>&1 | tail -20
```
Expected: PASS.

- [ ] **Step 3: Confirm tsc baseline**

Run (from repo root):
```bash
npx tsc --noEmit 2>&1 | tail -20
```
Expected: no NEW errors (the pre-existing `src/lib/voice/*` TS errors from ZEB-153 may appear — note them but do NOT fix here; they are out of scope and tracked separately. If `tsc` is clean, even better.)

> If the baseline is not green for channel code, STOP and report — do not build on a broken base.

---

## Task 1: `ChannelKind` enum (serialized as `u8`, Text-default, byte-identity helper)

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (add enum near the other channel types, e.g. just above `ChannelId` at ~line 318, or above the `MembershipEventKind` enum — pick a spot adjacent to the channel types and keep `rustfmt` happy)
- Test: inline `#[cfg(test)]` in the same file

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `community_membership.rs`:

```rust
#[test]
fn channel_kind_defaults_to_text_and_reports_is_text() {
    assert_eq!(ChannelKind::default(), ChannelKind::Text);
    assert!(ChannelKind::Text.is_text());
    assert!(!ChannelKind::Voice.is_text());
}

#[test]
fn channel_kind_u8_round_trip() {
    assert_eq!(u8::from(ChannelKind::Text), 0);
    assert_eq!(u8::from(ChannelKind::Voice), 1);
    assert_eq!(ChannelKind::try_from(0u8).unwrap(), ChannelKind::Text);
    assert_eq!(ChannelKind::try_from(1u8).unwrap(), ChannelKind::Voice);
    assert!(ChannelKind::try_from(2u8).is_err());
}

#[test]
fn channel_kind_serializes_as_cbor_u8() {
    // Voice encodes as the single CBOR byte 0x01; Text as 0x00.
    let voice = crate::owner_state_crypto::canonical_cbor_encode(&ChannelKind::Voice)
        .expect("encode voice");
    assert_eq!(voice, vec![0x01]);
    let text = crate::owner_state_crypto::canonical_cbor_encode(&ChannelKind::Text)
        .expect("encode text");
    assert_eq!(text, vec![0x00]);
    // Round-trips through ciborium.
    let back: ChannelKind =
        ciborium::de::from_reader(&voice[..]).expect("decode voice");
    assert_eq!(back, ChannelKind::Voice);
}
```

> Note: `canonical_cbor_encode` is the helper already used by the fixtures (see `wire_format_community_fixtures.rs`). If its path differs, match the import the fixtures use (`crate::owner_state_crypto::canonical_cbor_encode`). For decode, `ciborium::de::from_reader` matches the repo's deserialize idiom — if the crate is re-exported elsewhere, follow the existing usage.

- [ ] **Step 2: Run to verify failure**

Run (from `src-tauri/`):
```bash
cargo nextest run --locked --features test-fixtures -E 'test(channel_kind)'
```
Expected: FAIL — `cannot find type ChannelKind`.

- [ ] **Step 3: Implement `ChannelKind`**

Add to `community_membership.rs` (adjacent to the channel types):

```rust
/// The kind of a community channel. Serialized on the wire as a `u8` tag
/// (`Text = 0`, `Voice = 1`). `Text` is the default and is **omitted** from
/// the CBOR map by `skip_serializing_if = "ChannelKind::is_text"`, keeping a
/// Text `ChannelCreate`/`ChannelInfo` byte-identical to pre-ZEB-349 wire.
/// Voice channels are introduced by ZEB-349 (epic ZEB-348); kind is immutable
/// once a channel is created.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize,
)]
#[serde(into = "u8", try_from = "u8")]
pub enum ChannelKind {
    #[default]
    Text,
    Voice,
}

impl ChannelKind {
    /// `skip_serializing_if` / default-omission predicate: Text is the default
    /// and is never written to the CBOR map.
    pub fn is_text(&self) -> bool {
        matches!(self, ChannelKind::Text)
    }
}

impl From<ChannelKind> for u8 {
    fn from(kind: ChannelKind) -> u8 {
        match kind {
            ChannelKind::Text => 0,
            ChannelKind::Voice => 1,
        }
    }
}

impl TryFrom<u8> for ChannelKind {
    type Error = String;

    fn try_from(tag: u8) -> Result<Self, Self::Error> {
        match tag {
            0 => Ok(ChannelKind::Text),
            1 => Ok(ChannelKind::Voice),
            other => Err(format!("invalid ChannelKind tag: {other}")),
        }
    }
}
```

> `#[serde(into = "u8", try_from = "u8")]` requires `ChannelKind: Clone` (have `Copy`) and `TryFrom<u8>` with `Error: Display` (`String: Display` ✓). No new dependency (serde_repr not needed). Ensure `Serialize`/`Deserialize` are in scope (the module already imports `serde::{Serialize, Deserialize}` — confirm).

- [ ] **Step 4: Run to verify pass**

Run (from `src-tauri/`):
```bash
cargo nextest run --locked --features test-fixtures -E 'test(channel_kind)'
```
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
cd src-tauri && cargo fmt --all && cd ..
git add src-tauri/src/community_membership.rs
git commit -m "feat(zeb-349): ChannelKind enum (Text|Voice, u8 wire tag, Text-default)"
```

---

## Task 2: Thread `kind` into `ChannelCreate`, `ChannelInfo`, and materialize

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (`ChannelCreate` variant ~128-136, `ChannelInfo` struct ~1467-1477, `ChannelCreate` materialize arm ~1942-1961)
- Test: inline `#[cfg(test)]` in the same file

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block. (Reuse the existing test helpers in that module for constructing a `SignedMembershipEvent` / materializing — find the existing channel materialize tests, e.g. those exercising `ChannelCreate`, and follow their exact construction idiom. The skeleton below shows intent; adapt the event/materialize calls to the existing helpers.)

```rust
#[test]
fn materialize_channel_create_records_kind() {
    let mut m = Membership::default(); // or the existing test constructor
    let ch = ChannelId([0x42; 16]);

    // Voice channel create → ChannelInfo.kind == Voice
    apply_test_event(
        &mut m,
        MembershipEventKind::ChannelCreate {
            channel_id: ch,
            name: "hangout".to_string(),
            write_power: 0,
            kind: ChannelKind::Voice,
        },
    );
    assert_eq!(m.channels.get(&ch).unwrap().kind, ChannelKind::Voice);

    // A text channel defaults to Text
    let ch_text = ChannelId([0x43; 16]);
    apply_test_event(
        &mut m,
        MembershipEventKind::ChannelCreate {
            channel_id: ch_text,
            name: "general".to_string(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
    );
    assert_eq!(m.channels.get(&ch_text).unwrap().kind, ChannelKind::Text);
}

#[test]
fn channel_modify_cannot_change_kind() {
    // Invariant: kind is immutable. ChannelModify has no kind field, so a
    // modify on a Voice channel leaves kind == Voice.
    let mut m = Membership::default();
    let ch = ChannelId([0x42; 16]);
    apply_test_event(
        &mut m,
        MembershipEventKind::ChannelCreate {
            channel_id: ch,
            name: "hangout".to_string(),
            write_power: 0,
            kind: ChannelKind::Voice,
        },
    );
    apply_test_event(
        &mut m,
        MembershipEventKind::ChannelModify {
            channel_id: ch,
            name: Some("renamed".to_string()),
            write_power: Some(50),
        },
    );
    let info = m.channels.get(&ch).unwrap();
    assert_eq!(info.name, "renamed");
    assert_eq!(info.write_power, 50);
    assert_eq!(info.kind, ChannelKind::Voice, "kind must be immutable");
}
```

> `apply_test_event` / `Membership::default()` are placeholders for **whatever helper the existing channel materialize tests already use** in this module. Before writing, read the existing `ChannelCreate`/`ChannelModify` materialize tests in `community_membership.rs` and copy their exact setup (event construction, `materialize`/fold call, accessor). Do NOT invent a new harness.

- [ ] **Step 2: Run to verify failure**

Run (from `src-tauri/`):
```bash
cargo nextest run --locked --features test-fixtures -E 'test(materialize_channel_create_records_kind) + test(channel_modify_cannot_change_kind)'
```
Expected: FAIL — `ChannelCreate` has no field `kind` / `ChannelInfo` has no field `kind`.

- [ ] **Step 3: Add `kind` to the `ChannelCreate` variant**

In `MembershipEventKind::ChannelCreate` (~lines 128-136), add the field after `write_power`:

```rust
    #[serde(rename = "c")]
    ChannelCreate {
        #[serde(rename = "ch")]
        channel_id: ChannelId,
        #[serde(rename = "nm")]
        name: String,
        #[serde(rename = "wp")]
        write_power: u8,
        #[serde(rename = "ck", default, skip_serializing_if = "ChannelKind::is_text")]
        kind: ChannelKind,
    },
```

- [ ] **Step 4: Add `kind` to `ChannelInfo`**

In the `ChannelInfo` struct (~lines 1467-1477), add the field (place it after `write_power` to keep the struct readable; canonical CBOR sorts keys so declaration order does not affect wire bytes):

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelInfo {
    #[serde(rename = "nm")]
    pub name: String,
    #[serde(rename = "wp")]
    pub write_power: u8,
    #[serde(rename = "ck", default, skip_serializing_if = "ChannelKind::is_text")]
    pub kind: ChannelKind,
    #[serde(rename = "ca")]
    pub created_at: Hlc,
    #[serde(rename = "da", skip_serializing_if = "Option::is_none", default)]
    pub deleted_at: Option<Hlc>,
}
```

- [ ] **Step 5: Set `kind` in the `ChannelCreate` materialize arm**

In the `ChannelCreate` materialize arm (~lines 1942-1961), destructure `kind` and pass it into the constructed `ChannelInfo`:

```rust
MembershipEventKind::ChannelCreate {
    channel_id,
    name,
    write_power,
    kind,
} => {
    m.channels
        .entry(*channel_id)
        .or_insert_with(|| ChannelInfo {
            name: name.clone(),
            write_power: *write_power,
            kind: *kind,
            created_at: event.at.clone(),
            deleted_at: None,
        });
}
```

> The `ChannelModify` and `ChannelDelete` arms need **no change** — they don't touch `kind`, which is exactly why kind is immutable.

- [ ] **Step 6: Fix any other construction sites**

Compile to find every place that constructs a `ChannelCreate` literal or a `ChannelInfo` literal (the `mint_channel_create_event` in `lib.rs` and any test fixtures). They'll fail with "missing field `kind`". For now, in non-test production constructors add `kind: ChannelKind::Text` as a temporary default (Task 4 wires the real value through `create_channel`); in fixtures add the explicit kind the fixture intends. Run:

```bash
cargo build --locked --features test-fixtures 2>&1 | grep -A3 'missing field' | head -40
```
Address each. (Expect: `mint_channel_create_event` in `lib.rs`, and the fixture constructors in `wire_format_community_fixtures.rs` — Task 3 & 4 finalize those, but the tree must compile here.)

- [ ] **Step 7: Run to verify pass**

Run (from `src-tauri/`):
```bash
cargo nextest run --locked --features test-fixtures -E 'test(materialize_channel_create_records_kind) + test(channel_modify_cannot_change_kind) + test(channel)'
```
Expected: the two new tests PASS; **all existing channel tests still PASS** (including `signed_event_channel_create_wire_bytes_pinned` — proving Text wire is byte-identical, since the existing fixture builds a Text `ChannelCreate` and `kind` is skipped).

- [ ] **Step 8: Commit**

```bash
cd src-tauri && cargo fmt --all && cd ..
git add -A
git commit -m "feat(zeb-349): kind on ChannelCreate + ChannelInfo + materialize (immutable, Text byte-identical)"
```

---

## Task 3: Wire-format fixtures — pin Voice bytes; prove Text byte-identical

**Files:**
- Modify: `src-tauri/tests/wire_format_community_fixtures.rs` (channel fixtures ~lines 420-503)

- [ ] **Step 1: Confirm the existing Text fixture is unchanged**

Run (from `src-tauri/`):
```bash
cargo nextest run --locked --features test-fixtures -E 'test(signed_event_channel_create_wire_bytes_pinned)'
```
Expected: PASS with the **original** pinned hex (the Text `ChannelCreate` fixture must still match — this is the byte-identity proof; do NOT edit its hex). If it FAILS, the additive pattern is broken — STOP and fix Task 2 (likely `skip_serializing_if`/`default` missing).

- [ ] **Step 2: Add a Voice fixture test (assertion intentionally wrong, to capture real bytes)**

Add next to the existing channel fixtures (after `signed_event_channel_create_wire_bytes_pinned`):

```rust
#[test]
fn signed_event_channel_create_voice_wire_bytes_pinned() {
    let ch_id = ChannelId([0x42; 16]);
    let event = fixture_signed_event(MembershipEventKind::ChannelCreate {
        channel_id: ch_id,
        name: "general".to_string(),
        write_power: 0,
        kind: ChannelKind::Voice,
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    assert_eq!(
        hex,
        "REPLACE_WITH_ACTUAL_HEX",
        "Voice ChannelCreate wire format changed"
    );
    // Sanity: a Voice create is exactly the Text create plus one `ck`->0x01 map
    // entry, so its map header is one greater than the Text fixture's.
    assert!(bytes.len() > 0);
}
```

> `fixture_signed_event`, `canonical_cbor_encode`, `ChannelId`, `MembershipEventKind` are already imported in this file. Add `ChannelKind` to the existing `use community_membership::...` (or `harmony_app::...`) import line — match how `ChannelId`/`MembershipEventKind` are imported.

- [ ] **Step 3: Run to capture the real hex**

Run (from `src-tauri/`):
```bash
cargo nextest run --locked --features test-fixtures -E 'test(signed_event_channel_create_voice_wire_bytes_pinned)' 2>&1 | grep -A2 'assertion' | head
```
Expected: FAIL — the panic prints `left: "<actual hex>"`. Copy that exact hex string.

- [ ] **Step 4: Pin the captured hex**

Replace `REPLACE_WITH_ACTUAL_HEX` with the captured hex from Step 3.

- [ ] **Step 5: Verify the Voice fixture passes and Text is still byte-identical**

Run (from `src-tauri/`):
```bash
cargo nextest run --locked --features test-fixtures -E 'test(signed_event_channel_create)'
```
Expected: BOTH `signed_event_channel_create_wire_bytes_pinned` (Text, original hex) and `signed_event_channel_create_voice_wire_bytes_pinned` (Voice, new hex) PASS. Confirm the two hex strings differ by exactly the inserted `ck`/`0x01` entry and the bumped map header byte.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/tests/wire_format_community_fixtures.rs
git commit -m "test(zeb-349): pin Voice ChannelCreate wire bytes; Text stays byte-identical"
```

---

## Task 4: IPC — `create_channel` `kind` param + `ChannelInfoDto.kind` + `list_channels`

**Files:**
- Modify: `src-tauri/src/lib.rs` — `mint_channel_create_event` (~13122-13150), `create_channel` (~13176), `ChannelInfoDto` (~22447-22456), `list_channels` (~13771) mapping
- Test: extend the existing channel-IPC integration test (find it — likely in `src-tauri/tests/` exercising `create_channel`/`list_channels`, or an inline test). Follow the existing idiom.

- [ ] **Step 1: Write the failing test**

Locate the existing test that creates a channel and lists it (search `tests/` for `create_channel` / `list_channels`; the ZEB-248 Phase-1 integration test is the model). Add a case (adapt to the existing harness — same NodeState/engine setup the existing channel test uses):

```rust
#[tokio::test]
async fn create_voice_channel_surfaces_kind_in_list() {
    // ... existing setup that yields a started node + community_id ...
    let id = create_channel(state.clone(), community_id.clone(), "hangout".into(), 0, Some("voice".into()))
        .await
        .expect("create voice channel");
    let listed = list_channels(state.clone(), community_id.clone()).await.expect("list");
    let ch = listed.iter().find(|c| c.channel_id == id).expect("found");
    assert_eq!(ch.kind, "voice");

    // Default (None) → text
    let id_t = create_channel(state.clone(), community_id.clone(), "general".into(), 0, None)
        .await
        .expect("create text channel");
    let ch_t = list_channels(state, community_id).await.expect("list")
        .into_iter().find(|c| c.channel_id == id_t).expect("found");
    assert_eq!(ch_t.kind, "text");
}

#[tokio::test]
async fn create_channel_rejects_unknown_kind() {
    // ... existing setup ...
    let err = create_channel(state, community_id, "bad".into(), 0, Some("video".into()))
        .await
        .expect_err("must reject unknown kind");
    assert!(err.contains("kind"), "error should mention kind: {err}");
}
```

> If `create_channel` is only reachable as a `#[tauri::command]` and the existing tests call an inner helper, mirror that. The signature change (added trailing `kind` param) is the load-bearing part; match the existing test's invocation style.

- [ ] **Step 2: Run to verify failure**

```bash
cargo nextest run --locked --features test-fixtures -E 'test(create_voice_channel_surfaces_kind_in_list) + test(create_channel_rejects_unknown_kind)'
```
Expected: FAIL — arity mismatch on `create_channel` / `ChannelInfoDto` has no field `kind`.

- [ ] **Step 3: Add `kind` to `ChannelInfoDto`**

In `lib.rs` (~22447-22456):

```rust
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelInfoDto {
    pub channel_id: String,
    pub name: String,
    pub write_power: u8,
    /// "text" | "voice" — always emitted (DTO is IPC-only, no pinned wire fixture).
    pub kind: String,
    pub created_at: crate::owner_state_types::Hlc,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<crate::owner_state_types::Hlc>,
}
```

- [ ] **Step 4: Map `kind` where `ChannelInfoDto` is built**

Find every `ChannelInfoDto { ... }` construction (in `list_channels` and anywhere else). Add:

```rust
kind: match info.kind {
    crate::community_membership::ChannelKind::Text => "text".to_string(),
    crate::community_membership::ChannelKind::Voice => "voice".to_string(),
},
```
(Use the import path that file already uses for `ChannelInfo`.)

- [ ] **Step 5: Thread `kind` through `mint_channel_create_event` + `create_channel`**

In `mint_channel_create_event` (~13122-13150), add a `kind: ChannelKind` parameter and set it in the `MembershipEventKind::ChannelCreate { .. }` it builds:

```rust
fn mint_channel_create_event(/* existing args */, kind: ChannelKind) -> /* ... */ {
    // ...
    kind: MembershipEventKind::ChannelCreate {
        channel_id,
        name,
        write_power,
        kind,
    },
    // ...
}
```

In `create_channel` (~13176), add the param and parse it:

```rust
#[tauri::command]
async fn create_channel(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    name: String,
    write_power: u8,
    kind: Option<String>,
) -> Result<String, String> {
    let channel_kind = match kind.as_deref() {
        None | Some("text") => crate::community_membership::ChannelKind::Text,
        Some("voice") => crate::community_membership::ChannelKind::Voice,
        Some(other) => return Err(format!("invalid channel kind: {other}")),
    };
    // ... existing validation (name, write_power) unchanged ...
    // pass channel_kind into mint_channel_create_event(...)
}
```

> Keep `kind` as the **last** param so positional callers in tests are explicit. The JS boundary sends camelCase `kind` (a plain string) → arrives as the `kind: Option<String>` param.

- [ ] **Step 6: Run to verify pass**

```bash
cargo nextest run --locked --features test-fixtures -E 'test(create_voice_channel_surfaces_kind_in_list) + test(create_channel_rejects_unknown_kind) + test(channel)'
```
Expected: new tests PASS; all existing channel tests still PASS.

- [ ] **Step 7: Full clippy + fmt gate**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20 && cd ..
```
Expected: 0 warnings.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat(zeb-349): create_channel kind param + ChannelInfoDto.kind + list_channels mapping"
```

---

## Task 5: Frontend — `ChannelInfo` type + `createChannel(kind)`

**Files:**
- Modify: `src/lib/community-service.ts` (`ChannelInfo` ~36-42, `createChannel` ~272-282)
- Test: `src/lib/__tests__/community-service.test.ts` (if it exists; else add a focused test file). Find the existing community-service test idiom first.

- [ ] **Step 1: Write the failing test**

Find the existing `community-service` test (search `src/lib/__tests__/` for `community-service`). Add (adapt to the existing mock-adapter helper):

```typescript
it('createChannel passes kind through to the IPC', async () => {
  const invoke = vi.fn().mockResolvedValue('ab'.repeat(16));
  const svc = makeServiceWithInvoke(invoke); // existing helper pattern
  await svc.createChannel('comm-1', 'hangout', 0, 'voice');
  expect(invoke).toHaveBeenCalledWith('create_channel', {
    communityId: 'comm-1',
    name: 'hangout',
    writePower: 0,
    kind: 'voice',
  });
});

it('createChannel defaults kind to text when omitted', async () => {
  const invoke = vi.fn().mockResolvedValue('cd'.repeat(16));
  const svc = makeServiceWithInvoke(invoke);
  await svc.createChannel('comm-1', 'general', 0);
  expect(invoke).toHaveBeenCalledWith('create_channel', {
    communityId: 'comm-1',
    name: 'general',
    writePower: 0,
    kind: 'text',
  });
});
```

> `makeServiceWithInvoke` is a placeholder for the existing test's construction of a `CommunityService` over a mock adapter. Mirror it exactly.

- [ ] **Step 2: Run to verify failure**

```bash
npx vitest run src/lib/__tests__/community-service.test.ts 2>&1 | tail -20
```
Expected: FAIL — `createChannel` ignores the 4th arg / sends no `kind`.

- [ ] **Step 3: Update the `ChannelInfo` type**

In `community-service.ts` (~36-42):

```typescript
export interface ChannelInfo {
  channelId: string;
  name: string;
  writePower: number;
  kind: 'text' | 'voice';
  createdAt: { wallMs: number; logical: number; deviceId: string };
  deletedAt?: { wallMs: number; logical: number; deviceId: string };
}
```

- [ ] **Step 4: Update `createChannel`**

In `community-service.ts` (~272-282):

```typescript
async createChannel(
  communityId: string,
  name: string,
  writePower: number,
  kind: 'text' | 'voice' = 'text',
): Promise<string> {
  return this.invoke<string>('create_channel', {
    communityId,
    name,
    writePower,
    kind,
  });
}
```

- [ ] **Step 5: Run to verify pass**

```bash
npx vitest run src/lib/__tests__/community-service.test.ts 2>&1 | tail -20
```
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/lib/community-service.ts src/lib/__tests__/community-service.test.ts
git commit -m "feat(zeb-349): community-service ChannelInfo.kind + createChannel(kind)"
```

---

## Task 6: `CreateChannelDialog` — Text/Voice segmented control

**Files:**
- Modify: `src/lib/components/CreateChannelDialog.svelte`
- Test: `src/lib/components/__tests__/CreateChannelDialog.test.ts`

- [ ] **Step 1: Write the failing test**

Add (mirror the file's existing `setupDialog()` + assertion idiom):

```typescript
it('creates a voice channel when Voice is selected', async () => {
  const { createChannel } = setupDialog(); // existing helper returns the spy/service
  const voiceBtn = screen.getByRole('button', { name: /voice/i });
  await fireEvent.click(voiceBtn);
  await fireEvent.input(screen.getByPlaceholderText(/channel name/i), {
    target: { value: 'hangout' },
  });
  await fireEvent.click(screen.getByRole('button', { name: /^create$/i }));
  await waitFor(() =>
    expect(createChannel).toHaveBeenCalledWith(expect.any(String), 'hangout', 0, 'voice'),
  );
});

it('defaults to a text channel', async () => {
  const { createChannel } = setupDialog();
  await fireEvent.input(screen.getByPlaceholderText(/channel name/i), {
    target: { value: 'general' },
  });
  await fireEvent.click(screen.getByRole('button', { name: /^create$/i }));
  await waitFor(() =>
    expect(createChannel).toHaveBeenCalledWith(expect.any(String), 'general', 0, 'text'),
  );
});
```

> Match the existing helper names/placeholders in `CreateChannelDialog.test.ts` (the placeholder text, the create-button label). Read the file first; adjust the matchers to the real DOM.

- [ ] **Step 2: Run to verify failure**

```bash
npx vitest run src/lib/components/__tests__/CreateChannelDialog.test.ts 2>&1 | tail -20
```
Expected: FAIL — no Voice button / `createChannel` called with 3 args.

- [ ] **Step 3: Add the selector + thread `kind`**

In `CreateChannelDialog.svelte` script, add state:

```svelte
let kind = $state<'text' | 'voice'>('text');
```

In the submit handler, pass `kind`:

```svelte
await communityService.createChannel(communityId, trimmed, writePower, kind);
```

In the markup, add a segmented control **before the name input** (match the existing class/style conventions in the file; this is a sketch):

```svelte
<div class="kind-selector" role="group" aria-label="Channel type">
  <button
    type="button"
    class="kind-option"
    class:selected={kind === 'text'}
    aria-pressed={kind === 'text'}
    onclick={() => (kind = 'text')}
  >
    # Text
  </button>
  <button
    type="button"
    class="kind-option"
    class:selected={kind === 'voice'}
    aria-pressed={kind === 'voice'}
    onclick={() => (kind = 'voice')}
  >
    🔊 Voice
  </button>
</div>
```

Add minimal styles consistent with the dialog's existing CSS (segmented look: two buttons, selected state highlighted). Reset `kind = 'text'` wherever the dialog resets `name` on close/open (match the existing reset).

- [ ] **Step 4: Run to verify pass**

```bash
npx vitest run src/lib/components/__tests__/CreateChannelDialog.test.ts 2>&1 | tail -20
```
Expected: PASS (incl. existing tests).

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/CreateChannelDialog.svelte src/lib/components/__tests__/CreateChannelDialog.test.ts
git commit -m "feat(zeb-349): CreateChannelDialog Text/Voice selector"
```

---

## Task 7: `ChannelSubSidebar` — 🔊 glyph for voice channels

**Files:**
- Modify: `src/lib/components/ChannelSubSidebar.svelte` (glyph render ~line 98)
- Test: `src/lib/components/__tests__/ChannelSubSidebar.test.ts`

- [ ] **Step 1: Write the failing test**

Add (the existing `ChannelInfo` fixture in this test now needs `kind` — update fixtures to include `kind: 'text'`, and add a voice one):

```typescript
it('renders a speaker glyph for voice channels and # for text', async () => {
  const text: ChannelInfo = {
    channelId: 'aa'.repeat(16), name: 'general', writePower: 0, kind: 'text',
    createdAt: { wallMs: 100, logical: 0, deviceId: 'd1' },
  };
  const voice: ChannelInfo = {
    channelId: 'bb'.repeat(16), name: 'hangout', writePower: 0, kind: 'voice',
    createdAt: { wallMs: 100, logical: 0, deviceId: 'd1' },
  };
  render(ChannelSubSidebar, { /* existing required props */ channels: [text, voice] });
  // Voice row shows 🔊; text row shows #
  expect(screen.getByText('hangout').closest('button')).toHaveTextContent('🔊');
  expect(screen.getByText('general').closest('button')).toHaveTextContent('#');
});
```

> Update ALL existing `ChannelInfo` fixtures in this test file to add `kind: 'text'` (TS will now require it). Match the component's real prop names (`channels`, plus whatever else it requires).

- [ ] **Step 2: Run to verify failure**

```bash
npx vitest run src/lib/components/__tests__/ChannelSubSidebar.test.ts 2>&1 | tail -20
```
Expected: FAIL — voice row renders `#`, not `🔊` (and TS errors on missing `kind` until fixtures updated).

- [ ] **Step 3: Branch the glyph**

In `ChannelSubSidebar.svelte` (~line 98), replace the hardcoded hash span:

```svelte
{#if channel.kind === 'voice'}
  <span class="channel-glyph" aria-hidden="true">🔊</span>
{:else}
  <span class="channel-hash" aria-hidden="true">#</span>
{/if}
```

(Keep `channel-hash` styling; add `channel-glyph` with matching layout so alignment is preserved.)

- [ ] **Step 4: Run to verify pass**

```bash
npx vitest run src/lib/components/__tests__/ChannelSubSidebar.test.ts 2>&1 | tail -20
```
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/ChannelSubSidebar.svelte src/lib/components/__tests__/ChannelSubSidebar.test.ts
git commit -m "feat(zeb-349): ChannelSubSidebar speaker glyph for voice channels"
```

---

## Task 8: `VoiceChannelView` scaffold + `CommunityView` routing

**Files:**
- Create: `src/lib/components/VoiceChannelView.svelte` (V1 scaffold; V3 fleshes it out)
- Modify: `src/lib/components/CommunityView.svelte` (main-area routing ~399-421)
- Test: `src/lib/components/__tests__/VoiceChannelView.test.ts` (new); optionally extend a CommunityView test if one exists.

- [ ] **Step 1: Write the failing test for the scaffold**

Create `src/lib/components/__tests__/VoiceChannelView.test.ts`:

```typescript
import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import VoiceChannelView from '../VoiceChannelView.svelte';

describe('VoiceChannelView (V1 scaffold)', () => {
  it('renders the channel name with a speaker header and a disabled Join with a coming-soon note', () => {
    render(VoiceChannelView, { channelName: 'hangout' });
    expect(screen.getByText(/hangout/)).toBeTruthy();
    const join = screen.getByRole('button', { name: /join/i });
    expect((join as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText(/coming soon|not yet|soon/i)).toBeTruthy();
  });
});
```

- [ ] **Step 2: Run to verify failure**

```bash
npx vitest run src/lib/components/__tests__/VoiceChannelView.test.ts 2>&1 | tail -20
```
Expected: FAIL — module does not exist.

- [ ] **Step 3: Create the scaffold**

Create `src/lib/components/VoiceChannelView.svelte`:

```svelte
<script lang="ts">
  // V1 scaffold (ZEB-349). Audio + live roster arrive in V2/V3 (ZEB-350/351).
  let { channelName }: { channelName: string } = $props();
</script>

<section class="voice-channel" aria-label="Voice channel">
  <header class="voice-header">
    <span class="voice-glyph" aria-hidden="true">🔊</span>
    <h2 class="voice-title">{channelName}</h2>
  </header>

  <div class="voice-body">
    <p class="voice-roster-placeholder">No one is here yet.</p>
    <button type="button" class="voice-join" disabled>Join</button>
    <p class="voice-note">Voice chat is coming soon.</p>
  </div>
</section>

<style>
  .voice-channel { display: flex; flex-direction: column; height: 100%; }
  .voice-header { display: flex; align-items: center; gap: 0.5rem; padding: 0.75rem 1rem; border-bottom: 1px solid var(--border, #2a2d36); }
  .voice-title { font-size: 1rem; margin: 0; }
  .voice-body { flex: 1; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 0.75rem; color: var(--text-dim, #8a909a); }
  .voice-join:disabled { opacity: 0.5; cursor: not-allowed; }
  .voice-note { font-size: 0.85rem; }
</style>
```

> Use the project's real CSS variables if they differ; the fallbacks keep it self-contained. Keep it minimal — V3 replaces the body with the real grid/roster.

- [ ] **Step 4: Run to verify the scaffold passes**

```bash
npx vitest run src/lib/components/__tests__/VoiceChannelView.test.ts 2>&1 | tail -20
```
Expected: PASS.

- [ ] **Step 5: Route voice channels in `CommunityView`**

In `CommunityView.svelte`, import the scaffold and branch the main-area render (~399-421). Replace the `{:else if activeChannel}` block so a voice channel routes to `VoiceChannelView` and a text channel keeps `ChannelMessageFeed`:

```svelte
{:else if activeChannel}
  {#if activeChannel.kind === 'voice'}
    <VoiceChannelView channelName={activeChannel.name} />
  {:else}
    <ChannelMessageFeed
      {communityId}
      channelId={activeChannel.channelId}
      channelName={activeChannel.name}
      {channelMessageService}
      {votingAdapter}
      {ownAddress}
      {myPower}
      snapshotMessages={preForkSnapshot?.channelLog?.[activeChannel.channelId] ?? []}
      originalCommunityName={preForkSnapshot?.originalCommunityName ?? ''}
      forkedAtMs={preForkSnapshot?.forkedAtMs ?? 0}
      {resolveCard}
      {onOpenCard}
    />
  {/if}
{:else}
```

Add the import near the other component imports:
```svelte
import VoiceChannelView from './VoiceChannelView.svelte';
```

- [ ] **Step 6: Run the full FE suite + tsc**

```bash
npx vitest run 2>&1 | tail -25
npx tsc --noEmit 2>&1 | tail -25
```
Expected: vitest PASS; tsc shows no NEW errors (pre-existing ZEB-153 `src/lib/voice/*` errors, if present, are unchanged — do not fix here).

- [ ] **Step 7: Commit**

```bash
git add src/lib/components/VoiceChannelView.svelte src/lib/components/__tests__/VoiceChannelView.test.ts src/lib/components/CommunityView.svelte
git commit -m "feat(zeb-349): VoiceChannelView scaffold + CommunityView voice routing"
```

---

## Task 9: Final full-gate sweep

**Files:** none (verification + any fmt fixups)

- [ ] **Step 1: Rust gates**

```bash
cd src-tauri \
  && cargo fmt --all -- --check \
  && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
  && cargo nextest run --locked --workspace --all-targets --features test-fixtures \
  && cd ..
```
Expected: fmt clean, 0 clippy warnings, all tests pass. (Watch for the known transport/port-flake orphan tests — those are unrelated; if any flake, re-run that one. They are NOT caused by this diff.)

- [ ] **Step 2: Frontend gates**

```bash
npx tsc --noEmit 2>&1 | tail -25
npx vitest run 2>&1 | tail -25
```
Expected: tsc no new errors; vitest all pass.

- [ ] **Step 3: MSRV check**

```bash
cd src-tauri && cargo check --locked --all-targets --features test-fixtures && cd ..
```
Expected: clean.

- [ ] **Step 4: Confirm Text byte-identity one more time (the headline invariant)**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(signed_event_channel_create_wire_bytes_pinned)' && cd ..
```
Expected: PASS with the ORIGINAL hex (Text `ChannelCreate` unchanged on the wire).

- [ ] **Step 5: Commit any fmt fixups**

```bash
git add -A && git commit -m "chore(zeb-349): final gate sweep" || echo "nothing to commit"
```

---

## Acceptance criteria (from spec §V1)

- [x] `ChannelCreate` gains `kind: ChannelKind` (serde `"ck"`, u8 tag, skip-when-text) — Task 2.
- [x] Materialized `ChannelInfo` gains `kind`; kind immutable (ChannelModify can't change it) — Task 2.
- [x] Wire fixture pins a Voice `ChannelCreate`; Text `ChannelCreate` byte-identical to pre-change — Task 3.
- [x] `ChannelKind` round-trips — Task 1.
- [x] `create_channel` IPC gains `kind` param (default `text`, camelCase boundary) — Task 4.
- [x] FE `ChannelInfo.kind`; `CreateChannelDialog` Text/Voice control; `ChannelSubSidebar` 🔊 glyph; `CommunityView` routes voice → scaffold — Tasks 5-8.
- [x] All gates green (fmt/clippy/nextest/large-tests/MSRV/frontend) — Task 9.

## Out of scope (later slices)

- Audio capture/relay, presence/roster, sealing under `ChannelKey` → V2 (ZEB-350).
- Real `VoiceChannelView` (grid/roster/controls), session controller, VAD/mute/PTT → V3 (ZEB-351).
- DM calls → V4 (ZEB-352). 64 cap + scale → V5 (ZEB-353).
- The pre-existing `src/lib/voice/*` TS errors (ZEB-153) — do not fix here.
