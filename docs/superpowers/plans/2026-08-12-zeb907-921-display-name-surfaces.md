# ZEB-907 + ZEB-921 Display-Name Surfaces Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Surface the published owner-card display name as `get_owner_state.cardDisplayName` (ZEB-921) and make the Manage-community members list resolve names through the shared 4-rung ladder so the self row stops rendering hex (ZEB-907).

**Architecture:** ZEB-921 adds a decode-only helper next to the card wire type, an additive `#[serde(default)]` view field, and threads a snapshot of `ProfileCardPublisher.latest_handle()` through `build_owner_state_view`. ZEB-907 passes the already-in-scope `resolveCard`/`resolveNickname` from `CommunityView` into `CommunitySettingsPanel` and computes one label per row via `resolveMentionLabel` (the documented shared encoding of MemberRow's ladder).

**Tech Stack:** Rust (Tauri command layer, ciborium CBOR), Svelte 5, vitest + @testing-library/svelte.

**Spec:** `docs/superpowers/specs/2026-08-12-zeb907-921-display-name-surfaces-design.md`

## Global Constraints

- Cargo commands run from `src-tauri/`; frontend commands from repo root; `scripts/test-select` from repo root.
- Always `--locked --features test-fixtures`; clippy `--all-targets --no-deps -- -D warnings`; `cargo fmt --all -- --check`.
- Frontend gates: `npx tsc --noEmit` + `npx vitest run`.
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D`.
- `ownerDisplayName` semantics unchanged; new field is additive (`#[serde(default)]` / optional TS field).

---

### Task 1: ZEB-921 — `cardDisplayName` observable (Rust + TS type)

**Files:**
- Modify: `src-tauri/src/profile_card_broadcast.rs` (helper after `verify_card`, ~line 260; tests in existing `mod tests`)
- Modify: `src-tauri/src/owner_state.rs:18` (field) + `:2485` literal + camelCase assertions
- Modify: `src-tauri/src/owner_commands.rs` (`build_owner_state_view` :399/:590; `get_owner_state_inner` :715-736/:777/:825; mint :1629; test call sites :2947/:2992/:3048/:3062 + new threading test)
- Modify: `src/lib/owner-service.ts:5` area (TS field)

**Interfaces:**
- Produces: `pub fn decode_card_display_name(bytes: &[u8]) -> Option<String>` (profile_card_broadcast); `OwnerStateView.card_display_name: Option<String>` (wire `cardDisplayName`); `build_owner_state_view(loaded, this_device_name, card_display_name: Option<String>, fleet, quorum)`.

- [ ] **Step 1: Write the failing Rust tests**

In `profile_card_broadcast.rs` `mod tests` (crib the `sign_card` usage from `card_publisher_publishes_now_and_refreshes`):

```rust
    /// ZEB-921: the owner-state observable decodes the display name from the
    /// exact bytes the publisher caches (and the ZEB-884 queryable serves).
    #[test]
    fn decode_card_display_name_roundtrips_signed_bytes() {
        let owner = crate::community_membership::mint_test_owner(0x73);
        let card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Zeb921Probe".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        let bytes = canonical_cbor_encode(&card).unwrap();
        assert_eq!(
            decode_card_display_name(&bytes).as_deref(),
            Some("Zeb921Probe")
        );
    }

    #[test]
    fn decode_card_display_name_garbage_is_none() {
        assert_eq!(decode_card_display_name(b"not cbor at all"), None);
        assert_eq!(decode_card_display_name(&[]), None);
    }
```

In `owner_commands.rs` tests, next to the existing `build_owner_state_view` tests (reuse the exact `loaded` fixture construction the `:3048` test uses):

```rust
    /// ZEB-921: the card-name snapshot is threaded verbatim into the view.
    #[test]
    fn view_threads_card_display_name() {
        // Construct `loaded` identically to the sibling test at :3048.
        let view = build_owner_state_view(
            &loaded,
            "this device".into(),
            Some("Zeb921Probe".into()),
            FleetJoin::default(),
            QuorumJoin::default(),
        );
        assert_eq!(view.card_display_name.as_deref(), Some("Zeb921Probe"));
    }
```

In `owner_state.rs` `types_serialize_with_camelcase` (:2485): add `card_display_name: Some("Zeb921Card".into()),` after `owner_display_name` in the literal, and with the file's assertion style:

```rust
        // ZEB-921: non-default value so the rename is pinned.
        assert!(
            json.contains("\"cardDisplayName\":\"Zeb921Card\""),
            "expected cardDisplayName:Zeb921Card, got {json}"
        );
```

- [ ] **Step 2: Run to verify failure**

Run (from `src-tauri/`): `cargo nextest run --locked --features test-fixtures -E 'test(decode_card_display_name) or test(view_threads_card_display_name) or test(types_serialize_with_camelcase)'`
Expected: compile FAIL — E0425 (`decode_card_display_name` not found), E0560/E0609 (`card_display_name` unknown field), E0061 (arity).

- [ ] **Step 3: Implement**

`profile_card_broadcast.rs`, after `verify_card`:

```rust
/// ZEB-921: display name from cached self-card wire bytes (`CardWire.1`).
/// Decode-only — the publisher cache is written exclusively by our own
/// publish path with bytes we just signed (`publish_now`), so signature /
/// cert verification would add plumbing without a new guarantee. `None`
/// on decode failure (defensive; self-produced bytes always decode).
pub fn decode_card_display_name(bytes: &[u8]) -> Option<String> {
    ciborium::de::from_reader::<ProfileCardBroadcast, _>(bytes)
        .ok()
        .map(|c| c.display_name)
}
```

`owner_state.rs`, after `owner_display_name` (:18):

```rust
    /// ZEB-921: display name of the currently-cached published owner card —
    /// what peers can actually query from this node right now (the same
    /// publisher cache backs the periodic refresh and the ZEB-884
    /// queryable). `None` when nothing is served this run (node down, never
    /// published, or the pre-boot-publish window). Distinct from
    /// `owner_display_name`, which is the local DEVICE label.
    #[serde(default)]
    pub card_display_name: Option<String>,
```

`owner_commands.rs`:
1. `build_owner_state_view` signature — insert `card_display_name: Option<String>,` after `this_device_name: String,` (:401); view literal (:590) gains `card_display_name,` after `owner_display_name: this_device_name,`.
2. `get_owner_state_inner` — extend the `:715` lock block tuple to also clone the publisher:

```rust
    let (trust_resident, quorum_doc_arc, card_publisher) = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        let trust = match (g.owner_trust_doc.clone(), g.owner_trust_sync.clone()) {
            (Some(doc), Some(engine)) => Some((doc, engine)),
            _ => None,
        };
        (trust, g.owner_quorum_doc.clone(), g.profile_card_publisher.clone())
    };
```

3. After the `quorum` join (:736), before `let identity_dir`:

```rust
    // ZEB-921: snapshot the cached published-card name (async context — the
    // cache is a tokio Mutex). The same handle backs the periodic refresh
    // and the ZEB-884 queryable, so this reports exactly what a peer could
    // query from us right now; `None` = nothing served this run.
    let card_display_name = match card_publisher {
        Some(p) => p
            .latest_handle()
            .lock()
            .await
            .clone()
            .and_then(|(_topic, bytes)| {
                crate::profile_card_broadcast::decode_card_display_name(&bytes)
            }),
        None => None,
    };
```

4. Resident call site (:777): pass `card_display_name` as the new third arg. Blocking tail (:825): same — the `Option<String>` moves into the `run_blocking` closure.
5. Mint site (:1629): pass `None` with comment `// ZEB-921: a just-minted identity has published no card yet.`
6. Test call sites (:2947/:2992/:3048/:3062): add `None` as the third arg.

`owner-service.ts`, after `ownerDisplayName: string;`:

```typescript
  /**
   * ZEB-921: display name of the currently-published owner card — what
   * peers actually resolve (`ownerDisplayName` is the local device label, a
   * different notion). `null`/absent when nothing is being served this run
   * (node down, never published, or the pre-boot-publish window). Optional:
   * a stale backend omits it.
   */
  cardDisplayName?: string | null;
```

- [ ] **Step 4: Run to verify pass**

Run: the Step 2 command. Expected: PASS (all named tests).
Then: `cargo fmt --all` and `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`.
Then (repo root): `npx tsc --noEmit && npx vitest run src/lib/owner-service.test.ts src/lib/components/__tests__/DevicesPanel.test.ts`.
Then (repo root): `scripts/test-select --context task` — paste the `round=… bucket=…` line into the commit body notes if running under a task report.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/profile_card_broadcast.rs src-tauri/src/owner_state.rs src-tauri/src/owner_commands.rs src/lib/owner-service.ts
git commit -m "ZEB-921: surface published-card display name as get_owner_state.cardDisplayName"
```

---

### Task 2: ZEB-907 — settings-panel resolver parity

**Files:**
- Modify: `src/lib/components/CommunitySettingsPanel.svelte` (imports; props :35-65; filter :369-383; rows :558-564)
- Modify: `src/lib/components/CommunityView.svelte:573` mount
- Test: `src/lib/components/__tests__/CommunitySettingsPanel.test.ts`

**Interfaces:**
- Consumes: `resolveMentionLabel(ownerId, resolveNickname?, resolveCard?, resolveRosterName?)` from `../mention-render`; `ResolvedCard` from `../member-card-service`.
- Produces: optional props `resolveCard` / `resolveNickname` on `CommunitySettingsPanel` (same contracts as `CommunityMembersPanel:32-35`).

- [ ] **Step 1: Write the failing tests**

Append to `CommunitySettingsPanel.test.ts`:

```typescript
// ZEB-907: the members list resolves names through the shared 4-rung ladder
// (nickname → live card → roster displayName → hex) — the self row's roster
// displayName is always null (you never receive your own card), so it must
// read the local card instead of falling to hex.
describe('member display-name resolution (ZEB-907)', () => {
  const selfMember: CommunityMember = {
    address: 'ac7f7d42', displayName: null, power: 100, status: 'joined',
  };
  const rosterOther: CommunityMember = {
    address: 'b1c4', displayName: 'RosterBob', power: 0, status: 'joined',
  };
  const selfCard = (id: string) =>
    id === selfMember.address ? { displayName: 'Jake (on Koya)', statusText: '' } : undefined;

  it('self row renders the resolved card name instead of hex', () => {
    const { getByText, queryByText } = render(CommunitySettingsPanel, {
      props: {
        ...baseProps,
        members: [selfMember, rosterOther],
        myAddress: selfMember.address,
        resolveCard: selfCard,
      },
    });
    expect(getByText(/Jake \(on Koya\) \(you\)/)).toBeTruthy();
    expect(queryByText(/ac7f7d42 \(you\)/)).toBeNull();
  });

  it('nickname rung beats the card rung', () => {
    const { getByText } = render(CommunitySettingsPanel, {
      props: {
        ...baseProps,
        members: [selfMember],
        myAddress: selfMember.address,
        resolveCard: selfCard,
        resolveNickname: (id: string) =>
          id === selfMember.address ? 'my-nick' : undefined,
      },
    });
    expect(getByText(/my-nick \(you\)/)).toBeTruthy();
  });

  it('live card name beats a stale roster displayName', () => {
    const { getByText, queryByText } = render(CommunitySettingsPanel, {
      props: {
        ...baseProps,
        members: [rosterOther],
        myAddress: 'ffff',
        resolveCard: (id: string) =>
          id === rosterOther.address ? { displayName: 'FreshBob', statusText: '' } : undefined,
      },
    });
    expect(getByText('FreshBob')).toBeTruthy();
    expect(queryByText('RosterBob')).toBeNull();
  });

  it('without resolvers the self row keeps the hex fallback (pre-fix pin)', () => {
    const { getByText } = render(CommunitySettingsPanel, {
      props: { ...baseProps, members: [selfMember], myAddress: selfMember.address },
    });
    expect(getByText(/ac7f7d42 \(you\)/)).toBeTruthy();
  });

  it('search matches the resolved name, not just roster/hex', async () => {
    const { getByLabelText, getByText, queryByText } = render(CommunitySettingsPanel, {
      props: {
        ...baseProps,
        members: [selfMember, rosterOther],
        myAddress: selfMember.address,
        resolveCard: selfCard,
      },
    });
    await fireEvent.input(getByLabelText('Search members'), { target: { value: 'koya' } });
    expect(getByText(/Jake \(on Koya\) \(you\)/)).toBeTruthy();
    expect(queryByText('RosterBob')).toBeNull();
  });
});
```

- [ ] **Step 2: Run to verify failure**

Run (repo root): `npx vitest run src/lib/components/__tests__/CommunitySettingsPanel.test.ts`
Expected: the new tests FAIL (self row renders `ac7f7d42`; search finds nothing); pre-existing tests still pass.

- [ ] **Step 3: Implement**

`CommunitySettingsPanel.svelte`:
1. Imports: add `import { resolveMentionLabel } from '../mention-render';` and `import type { ResolvedCard } from '../member-card-service';`.
2. Props destructure: add `resolveCard,` and `resolveNickname,`; types block:

```typescript
    /** ZEB-907: optional resolvers (same contracts as CommunityMembersPanel).
     *  Rows resolve through the shared 4-rung ladder (nickname → live card →
     *  roster displayName → hex) so the self row — whose roster displayName
     *  is always null (you never receive your own card) — renders the local
     *  card name instead of hex. */
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    resolveNickname?: (ownerIdHex: string) => string | undefined;
```

3. Script helper (near `filteredMembers`):

```typescript
  /** ZEB-907: ONE label per row via the shared ladder, used by the render
   *  AND the search filter so a rendered name is always findable. */
  function memberLabel(m: CommunityMember): string {
    return resolveMentionLabel(m.address, resolveNickname, resolveCard, () => m.displayName ?? undefined);
  }
```

4. Filter predicate (:377-381) — prepend the label rung:

```typescript
          return joinedMembers.filter(
            (m) =>
              memberLabel(m).toLowerCase().includes(q) ||
              (m.displayName?.toLowerCase().includes(q) ?? false) ||
              m.address.toLowerCase().includes(q)
          );
```

5. Rows (:558-563):

```svelte
        {#each filteredMembers as m (m.address)}
          {@const label = memberLabel(m)}
          <div class="member-row">
            <div class="avatar">{label.slice(0, 1).toUpperCase()}</div>
            <div class="member-name">
              <div class="name">{label}{m.address === myAddress ? ' (you)' : ''}</div>
              <div class="addr">{m.address}</div>
```

(The hex rung returns `address.slice(0, 8)`, whose first character equals today's `m.address` first character — no-resolver rendering is byte-identical.)

`CommunityView.svelte` mount (:573 block): add `{resolveCard}` and `{resolveNickname}` lines after `{members}`.

- [ ] **Step 4: Run to verify pass**

Run: `npx vitest run src/lib/components/__tests__/CommunitySettingsPanel.test.ts` → all PASS.
Then: `npx tsc --noEmit && npx vitest run` (full frontend suite).

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/CommunitySettingsPanel.svelte src/lib/components/CommunityView.svelte src/lib/components/__tests__/CommunitySettingsPanel.test.ts
git commit -m "ZEB-907: resolve Manage-community member rows through the shared name ladder"
```

---

### Pre-PR gate (full sweep)

- [ ] `git status --short` clean; from `src-tauri/`: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; repo root: `npx tsc --noEmit && npx vitest run`.
- [ ] Push branch, open PR (`Closes ZEB-907`, `Closes ZEB-921`), fire `@coderabbitai review` ONCE, converge.
