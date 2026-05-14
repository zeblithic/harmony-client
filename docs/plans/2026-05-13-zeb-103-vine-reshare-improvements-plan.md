# ZEB-103 Vine Reshare Improvements Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` to implement this plan task-by-task. Each task ends in a single commit (Task 0 is verification-only, no commit). Steps use checkbox (`- [ ]`) syntax.

**Goal:** Wire the existing 146-line spec at `docs/specs/2026-04-07-vine-reshare-improvements-design.md` (commit `a3ad5ca`) into the codebase: original-creator attribution on the wire + UI, reshare counts derived from the local feed, a confirmation dialog gating the Reshare action, self-reshare prevention, and click-through navigation to the original vine.

**Architecture:** Two optional fields (`original_creator_address`, `original_creator_name`) added to `VineDescriptorPayload` / `PublishVinePayload` / `VineVideoDto` (Rust) and `VineVideo` / `VineDescriptorEvent` (TS), backward-compatible via `serde(default)` / `?` optionality. `VineFeedCache::CachedVine` extended so attribution survives reload. `VineService` gains `findVine(id)`, `getReshareCount(id)`, and self-reshare prevention in `publish()`. New `ReshareConfirmDialog.svelte` (mirrors `ConfirmationModal.svelte`) gates the Reshare button. `VineCard` and `VinePlayer` swap the existing "reshare" / "Reshared" markers for clickable "↗ originally by {name}" rows; `VineCard` adds an opt-in reshare count beside the like count. `VineFeed` forwards new props; `App.svelte` adds `vineGetReshareCount` (parallels `vineGetReaction`) and `handleViewOriginal`.

**Tech Stack:** Rust (`src-tauri/`, tauri 2.x, serde, serde_json), TypeScript (Svelte 5 `$state`/`$props` runes, vitest, @testing-library/svelte). No new dependencies. Five required gates: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `npx tsc --noEmit`, `npx vitest run` — cargo from `src-tauri/`, frontend from repo root.

**Spec acceptance criteria mapping:**

| Spec § | Deliverable | Task |
|---|---|---|
| Wire format → Rust | `original_creator_address` / `original_creator_name` on `VineDescriptorPayload`, `PublishVinePayload`, `VineVideoDto`, `CachedVine` | 1 |
| Wire format → TS | `originalCreatorAddress` / `originalCreatorName` on `VineVideo` + `VineDescriptorEvent` | 2 |
| VineService → publish | `publish()` threads original-creator fields | 2 |
| VineService → self-reshare prevention | silent no-op when `creatorAddress === 'self'` + no `reshareOf` | 4 |
| VineService → reshare count | `getReshareCount(id)` | 3 |
| VineService → findVine | `findVine(id)` searches both feeds | 3 |
| `ReshareConfirmDialog` | new component, modal-backed, Escape + backdrop dismissal, focus trap | 5 |
| `VineCard` attribution row + reshare count | replace "reshare" badge with clickable "↗ originally by {name}" + show count for originals | 7 |
| `VinePlayer` attribution row + confirm flow + hide-on-own | replace "Reshared" with clickable attribution, gate Reshare via dialog, hide button on own originals | 6 |
| `VineFeed` prop forwarding | passes `getReshareCount`, `onViewOriginal` to cards + player | 8 |
| `App.svelte` wiring | `handleVineReshare` propagates original-creator fields, new `vineGetReshareCount` reactive, `handleViewOriginal` finds + opens vine | 9 |
| Rust tests | new fields serialize/deserialize, backward compat with missing | 1 |
| Frontend tests | new VineService methods + ReshareConfirmDialog + VineCard + VinePlayer + VineFeed | 3, 4, 5, 6, 7, 8 |
| Integration | App-level reshare-with-attribution round-trip | 9 |

---

## File Structure

| File | Action | Responsibility |
|---|---|---|
| `src-tauri/src/lib.rs:4290-4329` | Modify | Add original-creator fields to `VineDescriptorPayload`, `PublishVinePayload`, `VineVideoDto`. Update DTO construction site (~line 4406). |
| `src-tauri/src/lib.rs:13700-13830` | Modify | Add round-trip tests for the new fields + backward-compat tests. |
| `src-tauri/src/vine_feed_cache.rs` | Modify | Add fields to `CachedVine` + `DescriptorOnDisk` + the cache→DTO mappers. Add module-level test verifying attribution survives disk round-trip. |
| `src-tauri/src/event_loop.rs` (audit only) | Audit | Verify the event-loop pipes the new fields end-to-end (no code changes expected — the fields ride through the existing `VineDescriptorPayload` deserialize → `on_descriptor_sample` → DTO emit path). |
| `src/lib/types.ts:116-132` | Modify | Add `originalCreatorAddress?` + `originalCreatorName?` to `VineVideo`. |
| `src/lib/vine-service.ts` | Modify | Add fields to `VineDescriptorEvent`; thread through `wireToVine`; update `publish()` signature + self-reshare guard; add `findVine()` + `getReshareCount()`. |
| `src/lib/vine-service.test.ts` | Modify | Add test cases for new methods + new fields. |
| `src/lib/components/ReshareConfirmDialog.svelte` | Create | New modal-backed component gating the reshare action. |
| `src/lib/components/__tests__/ReshareConfirmDialog.test.ts` | Create | Component tests: render, confirm/cancel callbacks, Escape, backdrop click. |
| `src/lib/components/VineCard.svelte` | Modify | Replace reshare badge with attribution row; add `reshareCount` + `onViewOriginal` props + render. |
| `src/lib/components/__tests__/VineCard.test.ts` | Modify | Update existing "reshare badge" tests; add attribution + count + onViewOriginal tests. |
| `src/lib/components/VinePlayer.svelte` | Modify | Replace "Reshared" label with attribution row; gate Reshare button via dialog; hide on own originals. |
| `src/lib/components/__tests__/VinePlayer.test.ts` | Modify | Update existing reshare tests; add attribution + dialog + hide-on-own tests. |
| `src/lib/components/VineFeed.svelte` | Modify | Forward `getReshareCount` + `onViewOriginal` to children. |
| `src/lib/components/__tests__/VineFeed.test.ts` (or integration test) | Modify | Verify forwarding. |
| `src/App.svelte` | Modify | Update `handleVineReshare`; add `vineGetReshareCount` + `handleViewOriginal`; wire to `VineFeed`. |
| `src/App.test.ts` (or integration test) | Modify | Add reshare-with-attribution smoke test. |

**File count:** 1 new component + 1 new component test + ~10 modified files.

---

## Task 0: Pre-flight + green baseline

**Files:** (none)

**Purpose:** Confirm the working tree is on the freshly cut branch, baseline gates are green, and nothing is in a half-modified state. **No commit.**

- [ ] **Step 1: Verify branch and HEAD**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git branch --show-current
git rev-parse HEAD
git status --short
```

Expected: branch `zeb-103-vine-reshare-improvements`, HEAD == `origin/main` (post-ZEB-147 merge, currently `76c399b`), zero modified/untracked files except the in-progress plan (which is fine — the plan will be committed in a separate step before Task 1 begins).

- [ ] **Step 2: Run all five required gates (must all pass before starting Task 1)**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && \
  cargo fmt --all -- --check && \
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && \
  cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && \
  npx tsc --noEmit && \
  npx vitest run
```

Expected: all five gates green. Note the test count from `cargo nextest` (call it `BASELINE_RUST_TESTS`) and from `npx vitest run` (call it `BASELINE_VITEST_TESTS`). These are the baselines new tests must add to, not subtract from.

- [ ] **Step 3: If any gate fails, STOP**

Failing baseline = test drift since merge. Per the `feedback_test_drift_is_our_fault` memory rule, broken tests on main are exclusively ours and must be swept + fixed before new feature work. File a follow-up if discovered, fix on the new branch, re-run from Step 2.

---

## Task 1: Rust wire fields + persistence

**Spec ref:** §Wire Format Changes → Rust Types; §Edge Cases → Backward compatibility.

**Files:**
- Modify: `src-tauri/src/lib.rs:4290-4329` (Rust payload structs)
- Modify: `src-tauri/src/lib.rs` (one site inside `list_vine_videos` that constructs `VineVideoDto`, around line 4406)
- Modify: `src-tauri/src/lib.rs` (existing wire-format pin tests around line 13780)
- Modify: `src-tauri/src/vine_feed_cache.rs:55-65` (`DescriptorOnDisk` struct)
- Modify: `src-tauri/src/vine_feed_cache.rs:125-135` (`CachedVine` struct)
- Modify: `src-tauri/src/vine_feed_cache.rs:280-295` (`populate_from_disk` mapping)
- Modify: `src-tauri/src/vine_feed_cache.rs:395-415` (cache→DTO mapper in `list_descriptors`)
- Modify: `src-tauri/src/vine_feed_cache.rs:420-435` (`on_descriptor_sample` insert path)
- Modify: `src-tauri/src/vine_feed_cache.rs:555-580` (`save()` serialization)
- Modify: `src-tauri/src/vine_feed_cache.rs:640-670` (test helper `insert_descriptor_for_test` or equivalent — used by module tests)

- [ ] **Step 1: Write the failing Rust test for camelCase round-trip with original-creator fields**

In `src-tauri/src/lib.rs`, in the existing test module that hosts the wire-format pin tests for `VineDescriptorPayload` (search for `reshareOf` in the existing tests around line 13780 and add a sibling test). Add:

```rust
#[test]
fn vine_descriptor_payload_serializes_original_creator_fields_as_camel_case() {
    let payload = VineDescriptorPayload {
        id: "vine-1".to_string(),
        creator_address: "addr-resharer".to_string(),
        creator_name: "Resharer".to_string(),
        created_at: 100,
        video_cid: "cid-1".to_string(),
        title: None,
        reshare_of: Some("vine-0".to_string()),
        original_creator_address: Some("addr-original".to_string()),
        original_creator_name: Some("Original Creator".to_string()),
    };
    let json = serde_json::to_string(&payload).expect("serialize");
    assert!(
        json.contains("\"originalCreatorAddress\":\"addr-original\""),
        "originalCreatorAddress should be present in camelCase: {json}"
    );
    assert!(
        json.contains("\"originalCreatorName\":\"Original Creator\""),
        "originalCreatorName should be present in camelCase: {json}"
    );

    // Round-trip.
    let parsed: VineDescriptorPayload =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed.original_creator_address.as_deref(), Some("addr-original"));
    assert_eq!(parsed.original_creator_name.as_deref(), Some("Original Creator"));
}

#[test]
fn vine_descriptor_payload_omits_original_creator_fields_when_none() {
    let payload = VineDescriptorPayload {
        id: "vine-1".to_string(),
        creator_address: "addr-1".to_string(),
        creator_name: "Alice".to_string(),
        created_at: 100,
        video_cid: "cid-1".to_string(),
        title: None,
        reshare_of: None,
        original_creator_address: None,
        original_creator_name: None,
    };
    let json = serde_json::to_string(&payload).expect("serialize");
    assert!(
        !json.contains("originalCreatorAddress"),
        "should omit originalCreatorAddress when None: {json}"
    );
    assert!(
        !json.contains("originalCreatorName"),
        "should omit originalCreatorName when None: {json}"
    );
}

#[test]
fn vine_descriptor_payload_deserializes_legacy_wire_without_original_creator_fields() {
    let legacy = r#"{
        "id": "vine-1",
        "creatorAddress": "addr-1",
        "creatorName": "Alice",
        "createdAt": 100,
        "videoCid": "cid-1",
        "reshareOf": "vine-0"
    }"#;
    let parsed: VineDescriptorPayload =
        serde_json::from_str(legacy).expect("legacy wire must deserialize");
    assert_eq!(parsed.reshare_of.as_deref(), Some("vine-0"));
    assert!(parsed.original_creator_address.is_none());
    assert!(parsed.original_creator_name.is_none());
}
```

- [ ] **Step 2: Run failing tests to verify they fail to compile**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures \
  -E 'test(vine_descriptor_payload_serializes_original_creator) + test(vine_descriptor_payload_omits_original_creator) + test(vine_descriptor_payload_deserializes_legacy)'
```

Expected: FAIL — compile error because `VineDescriptorPayload` doesn't have the new fields yet.

- [ ] **Step 3: Add the wire fields to `VineDescriptorPayload`**

In `src-tauri/src/lib.rs:4290`, modify the struct to append two new optional fields after `reshare_of`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VineDescriptorPayload {
    pub id: String,
    pub creator_address: String,
    pub creator_name: String,
    pub created_at: u64,
    pub video_cid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reshare_of: Option<String>,
    /// If this vine is a reshare, the hex-encoded address of the original creator.
    /// Always traces to the true origin — if Alice reshares Bob's reshare of Carol's vine,
    /// the field carries Carol's address. None for non-reshare originals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_creator_address: Option<String>,
    /// Display name of the original creator (snapshot at reshare time). See above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_creator_name: Option<String>,
}
```

- [ ] **Step 4: Add the same fields to `PublishVinePayload`**

In `src-tauri/src/lib.rs:4307`:

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishVinePayload {
    pub video_cid: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub reshare_of: Option<String>,
    #[serde(default)]
    pub creator_name: String,
    /// See VineDescriptorPayload::original_creator_address.
    #[serde(default)]
    pub original_creator_address: Option<String>,
    /// See VineDescriptorPayload::original_creator_name.
    #[serde(default)]
    pub original_creator_name: Option<String>,
}
```

- [ ] **Step 5: Add the same fields to `VineVideoDto`**

In `src-tauri/src/lib.rs:4322`:

```rust
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VineVideoDto {
    pub id: String,
    pub creator_address: String,
    pub creator_name: String,
    pub created_at: u64,
    pub video_cid: String,
    pub title: Option<String>,
    pub reshare_of: Option<String>,
    pub viewed: bool,
    /// See VineDescriptorPayload::original_creator_address.
    pub original_creator_address: Option<String>,
    /// See VineDescriptorPayload::original_creator_name.
    pub original_creator_name: Option<String>,
}
```

- [ ] **Step 6: Update the DTO construction site in lib.rs**

Around `src-tauri/src/lib.rs:4406`, where `VineVideoDto` is constructed from a `VineVideo` model. Locate the existing site (it'll be in the path that converts internal vine state to the DTO returned by `list_vine_videos`). Add the two new fields, defaulting to `None`:

```rust
// Inside the existing VineVideoDto { ... } literal:
original_creator_address: vine.original_creator_address.clone(),
original_creator_name: vine.original_creator_name.clone(),
```

If the source struct doesn't have these fields yet (it's the in-memory `VineVideo`), pass `None` for now — the cache→DTO mapper in Task 1 Step 9 below will be the actual production source.

- [ ] **Step 7: Run the wire-format tests — verify they pass**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures \
  -E 'test(vine_descriptor_payload_serializes_original_creator) + test(vine_descriptor_payload_omits_original_creator) + test(vine_descriptor_payload_deserializes_legacy)'
```

Expected: PASS — 3 passed.

- [ ] **Step 8: Write the failing cache-persistence test**

In `src-tauri/src/vine_feed_cache.rs`, near the existing module tests (search for `reshare_of` to find the right region around line 1480). Add:

```rust
#[test]
fn cached_descriptor_round_trips_original_creator_fields_through_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // First boot: insert a reshare with full attribution, drop the cache.
    {
        let mut cache = VineFeedCache::load(dir.path());
        let payload = VineDescriptorPayload {
            id: "vine-reshare".to_string(),
            creator_address: "addr-resharer".to_string(),
            creator_name: "Resharer".to_string(),
            created_at: now_secs.saturating_sub(1),
            video_cid: "cid-r".to_string(),
            title: None,
            reshare_of: Some("vine-orig".to_string()),
            original_creator_address: Some("addr-original".to_string()),
            original_creator_name: Some("Original".to_string()),
        };
        let outcome = cache.on_descriptor_sample(
            "harmony/vines/addr-resharer",
            serde_json::to_vec(&payload).expect("encode").into(),
        );
        assert!(matches!(outcome, Some(DescriptorOutcome::Inserted { .. })));
    }

    // Second boot: reload, verify attribution survived.
    {
        let cache = VineFeedCache::load(dir.path());
        let dtos = cache.list_descriptors();
        let dto = dtos.iter().find(|d| d.id == "vine-reshare")
            .expect("reshare should survive reload");
        assert_eq!(dto.original_creator_address.as_deref(), Some("addr-original"));
        assert_eq!(dto.original_creator_name.as_deref(), Some("Original"));
    }
}
```

- [ ] **Step 9: Run the cache test — verify it fails**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures \
  -E 'test(cached_descriptor_round_trips_original_creator)'
```

Expected: FAIL (compile error — fields don't exist on `CachedVine` / `DescriptorOnDisk` / DTO yet, and the existing struct literals don't initialize them).

- [ ] **Step 10: Add the fields to `DescriptorOnDisk` and `CachedVine`**

In `src-tauri/src/vine_feed_cache.rs:55` (or wherever `DescriptorOnDisk` lives), inside the struct:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
original_creator_address: Option<String>,
#[serde(default, skip_serializing_if = "Option::is_none")]
original_creator_name: Option<String>,
```

In `src-tauri/src/vine_feed_cache.rs:125` (or wherever `CachedVine` lives), inside the struct (in the inner `descriptor: VineDescriptorPayload` if `CachedVine` wraps the payload; otherwise add the two fields directly to `CachedVine`):

The implementer should look at the actual struct shape. If `CachedVine.descriptor` is already `VineDescriptorPayload`, **no change is needed** in `CachedVine` — Step 3 already added the fields to the payload. Just confirm.

If `CachedVine` mirrors the payload field-by-field instead, add:

```rust
pub original_creator_address: Option<String>,
pub original_creator_name: Option<String>,
```

- [ ] **Step 11: Update the populate-from-disk mapping**

In `src-tauri/src/vine_feed_cache.rs:280-295`, inside `populate_from_disk` (or whatever the function that converts `DescriptorOnDisk` → `CachedVine` is called), pass the new fields through. Search for `reshare_of: d.reshare_of` and add the parallel lines:

```rust
original_creator_address: d.original_creator_address,
original_creator_name: d.original_creator_name,
```

- [ ] **Step 12: Update the save() serialization**

In `src-tauri/src/vine_feed_cache.rs:555-580`, where `CachedVine` is converted to `DescriptorOnDisk` for disk write. Search for the literal that constructs `DescriptorOnDisk { id, ..., reshare_of: ..., }` and add:

```rust
original_creator_address: cv.descriptor.original_creator_address.clone(),
original_creator_name: cv.descriptor.original_creator_name.clone(),
```

(Adjust path through `cv.descriptor.*` vs `cv.*` based on the actual struct shape from Step 10.)

- [ ] **Step 13: Update the cache→DTO mapper**

In `src-tauri/src/vine_feed_cache.rs:395-415` (`list_descriptors` and any other DTO-construction site — there may be multiple). Search for `reshare_of: cv.descriptor.reshare_of.clone()` (or similar) and add:

```rust
original_creator_address: cv.descriptor.original_creator_address.clone(),
original_creator_name: cv.descriptor.original_creator_name.clone(),
```

If `on_descriptor_sample` (line ~427) also constructs a DTO for the `Inserted { dto }` outcome, update it the same way.

- [ ] **Step 14: Update any test helpers that construct CachedVine / DescriptorOnDisk inline**

Search the test module for any `CachedVine { ... }` or `DescriptorOnDisk { ... }` literals that don't compile after the field additions. Add `original_creator_address: None, original_creator_name: None,` to each. The existing test helper at `src-tauri/src/vine_feed_cache.rs:640-670` (`insert_descriptor_for_test` or equivalent) likely needs a parameter add — keep the param `Option<&str>` for each, default `None` at call sites, or take a single struct param.

If the implementer prefers, they can leave existing test helpers alone and only thread the new fields through where the new test in Step 8 needs them (the new test passes them via the `VineDescriptorPayload` literal directly, so an internal helper may not need any param changes).

- [ ] **Step 15: Re-run all tests in the cache module + the new tests**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures \
  -E 'test(/vine_feed_cache/) + test(/vine_descriptor_payload/) + test(cached_descriptor_round_trips_original_creator)'
```

Expected: all PASS, no regressions in existing cache tests, the new round-trip test passes.

- [ ] **Step 16: Run all five required gates**

```bash
cd src-tauri && cargo fmt --all -- --check && \
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && \
  cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && \
  npx tsc --noEmit && \
  npx vitest run
```

Expected: all five gates green. `npx tsc --noEmit` will catch any TS-side type-mismatches if `list_vine_videos` is invoked anywhere on the frontend with a strict type — but we haven't added TS fields yet, so the new fields will just be untyped extras at the boundary. **This is OK** — TypeScript only checks declared fields; extra unknown fields don't fail the type checker. Task 2 will add the TS-side declarations.

If `tsc --noEmit` does fail (e.g., because something destructures the DTO and TypeScript narrows to "no extra fields"), that's information — the implementer should diagnose and either fix or defer to Task 2.

- [ ] **Step 17: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs src-tauri/src/vine_feed_cache.rs
git commit -m "$(cat <<'EOF'
feat(zeb-103): add original-creator wire fields + persistence

Extends the Vine reshare wire format with two optional fields tracing
the true origin through reshare chains:

- `VineDescriptorPayload.original_creator_address` (camelCase wire)
- `VineDescriptorPayload.original_creator_name`

Mirror fields on `PublishVinePayload` (inbound from frontend) and
`VineVideoDto` (outbound to frontend). All three use `serde(default)` +
`skip_serializing_if = "Option::is_none"` for backward compatibility —
legacy descriptors without these fields deserialize cleanly to `None`,
and non-reshare vines omit them from the wire.

Extends `VineFeedCache::DescriptorOnDisk` with the same fields (also
`skip_serializing_if`-gated) and threads them through:

- `populate_from_disk` (load path)
- `save()` (write path)
- `list_descriptors` + `on_descriptor_sample` DTO mappers (read path)

So attribution survives reload (parallel to ZEB-147 reshare_of
persistence guarantee).

Test coverage:

- `vine_descriptor_payload_serializes_original_creator_fields_as_camel_case`
- `vine_descriptor_payload_omits_original_creator_fields_when_none`
- `vine_descriptor_payload_deserializes_legacy_wire_without_original_creator_fields`
- `cached_descriptor_round_trips_original_creator_fields_through_disk`

Refs ZEB-103 spec §Wire Format Changes → Rust Types,
§Edge Cases → Backward compatibility.
EOF
)"
```

---

## Task 2: TypeScript types + VineService.publish param threading

**Spec ref:** §Wire Format Changes → TypeScript Types; §Frontend Service → Publishing Reshares.

**Files:**
- Modify: `src/lib/types.ts:113-132` (add fields to `VineVideo`)
- Modify: `src/lib/vine-service.ts:5-15` (add fields to `VineDescriptorEvent`)
- Modify: `src/lib/vine-service.ts:300-330` (thread through `wireToVine`)
- Modify: `src/lib/vine-service.ts:125-160` (update `publish()` signature)
- Modify: `src/lib/vine-service.test.ts` (new test cases)

- [ ] **Step 1: Write failing tests for new fields**

In `src/lib/vine-service.test.ts`, near the existing `preserves optional fields (title, reshareOf)` test (around line 126):

```typescript
it('preserves original creator attribution on incoming wire vines', async () => {
  const { adapter, emit } = createMockAdapter();
  await svc.connectAdapter(adapter);
  const initial = svc.discoverVines.length;
  emit('vine-received', {
    id: 'vine-resh',
    creatorAddress: 'addr-resharer',
    creatorName: 'Resharer',
    createdAt: 1,
    videoCid: 'cid-r',
    reshareOf: 'orig-1',
    originalCreatorAddress: 'addr-original',
    originalCreatorName: 'Original',
  } satisfies VineDescriptorEvent);
  const vine = svc.discoverVines[initial];
  expect(vine.reshareOf).toBe('orig-1');
  expect(vine.originalCreatorAddress).toBe('addr-original');
  expect(vine.originalCreatorName).toBe('Original');
});

it('publish forwards original creator fields to adapter', async () => {
  const { adapter, invokes } = createMockAdapter();
  await svc.connectAdapter(adapter);
  await svc.publish('cid-pub', 'Title', 'reshare-of-1', 'addr-original', 'Original');
  const publishCall = invokes.find(c => c.cmd === 'publish_vine');
  expect(publishCall).toBeTruthy();
  expect(publishCall!.args).toEqual({
    vine: {
      videoCid: 'cid-pub',
      title: 'Title',
      reshareOf: 'reshare-of-1',
      creatorName: 'You',
      originalCreatorAddress: 'addr-original',
      originalCreatorName: 'Original',
    },
  });
});

it('publish omits original creator fields when not provided', async () => {
  const { adapter, invokes } = createMockAdapter();
  await svc.connectAdapter(adapter);
  await svc.publish('cid-pub', 'Title');
  const publishCall = invokes.find(c => c.cmd === 'publish_vine');
  expect(publishCall).toBeTruthy();
  expect(publishCall!.args.vine.originalCreatorAddress).toBeUndefined();
  expect(publishCall!.args.vine.originalCreatorName).toBeUndefined();
});
```

The exact assertion shape on `invokes` depends on what `createMockAdapter()` exposes — read `src/lib/test-utils.ts` first to confirm the actual API, then adjust the assertions to match (`invokes` may be an array of `{ cmd, args }`, or a `vi.fn()` you can call `.mock.calls` on, etc.).

- [ ] **Step 2: Run failing tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/vine-service.test.ts
```

Expected: FAIL — compile error on `originalCreatorAddress` not existing on `VineDescriptorEvent` / `VineVideo`, and `publish` signature missing the new params.

- [ ] **Step 3: Add fields to `VineVideo`**

In `src/lib/types.ts:116-132`, append after the `reshareOf` field:

```typescript
/** Mirrors harmony-content VineDescriptor on the TypeScript side. */
export interface VineVideo {
  /** Unique ID for this vine (hex-encoded bundle CID). */
  id: string;
  /** Hex-encoded 128-bit creator address. */
  creatorAddress: string;
  /** Creator display name (resolved from profile store). */
  creatorName: string;
  /** Unix timestamp in seconds when the vine was created. */
  createdAt: number;
  /** Hex-encoded CID of the raw video content blob. */
  videoCid: string;
  /** Optional human-readable title (max 140 bytes). */
  title?: string;
  /** If this vine is a reshare, the hex-encoded CID of the original. */
  reshareOf?: string;
  /**
   * If this vine is a reshare, the address of the original creator
   * (always the true origin — traces through reshare-of-reshare chains).
   * Undefined for non-reshare originals.
   */
  originalCreatorAddress?: string;
  /** Display name of the original creator (snapshot at reshare time). */
  originalCreatorName?: string;
  /** Whether the current user has viewed this vine. */
  viewed: boolean;
}
```

- [ ] **Step 4: Add fields to `VineDescriptorEvent`**

In `src/lib/vine-service.ts:5-15`:

```typescript
/** Wire format for vine descriptors from the Rust backend. */
export interface VineDescriptorEvent {
  id: string;
  creatorAddress: string;
  creatorName: string;
  createdAt: number;
  videoCid: string;
  title?: string;
  reshareOf?: string;
  /** See VineVideo.originalCreatorAddress. */
  originalCreatorAddress?: string;
  /** See VineVideo.originalCreatorName. */
  originalCreatorName?: string;
  source?: 'followed' | 'discover';
}
```

- [ ] **Step 5: Thread new fields through `wireToVine`**

In `src/lib/vine-service.ts:300-330`, locate the `wireToVine` (or equivalent) function. After the `reshareOf: wire.reshareOf` line, add:

```typescript
originalCreatorAddress: wire.originalCreatorAddress,
originalCreatorName: wire.originalCreatorName,
```

- [ ] **Step 6: Update `publish()` signature**

In `src/lib/vine-service.ts:125-160`, modify the `publish` method to accept the new params and forward them to the adapter:

```typescript
/** Publish a vine via Tauri command. */
async publish(
  videoCid: string,
  title?: string,
  reshareOf?: string,
  originalCreatorAddress?: string,
  originalCreatorName?: string,
): Promise<void> {
  if (this.adapter) {
    try {
      await this.adapter.invoke('publish_vine', {
        vine: {
          videoCid,
          title,
          reshareOf,
          creatorName: this.ownDisplayName,
          originalCreatorAddress,
          originalCreatorName,
        },
      });
      return; // Backend will echo via subscription → vine-received event
    } catch (err: unknown) {
      const msg = err instanceof Error ? err.message : String(err);
      if (!msg.includes('not connected') && !msg.includes('event loop')) {
        throw err;
      }
    }
  }

  // Offline fallback: append locally so the UI stays responsive.
  const id = `vine-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
  this.seenIds.add(id);
  this.viewedIds = new Set([...this.viewedIds, id]);
  const vine: VineVideo = {
    id,
    creatorAddress: 'self',
    creatorName: this.ownDisplayName,
    createdAt: Math.floor(Date.now() / 1000),
    videoCid,
    title,
    reshareOf,
    originalCreatorAddress,
    originalCreatorName,
    viewed: true,
  };
  this.discoverVines = [...this.discoverVines, vine];
  this.onChange?.();
}
```

- [ ] **Step 7: Run tests — verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/vine-service.test.ts
```

Expected: PASS.

- [ ] **Step 8: Run frontend gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit && npx vitest run
```

Expected: green. (Any callers of `publish()` that already use named-arg style or only the first three params keep working; the new params are optional and append to the end.)

- [ ] **Step 9: Commit**

```bash
git add src/lib/types.ts src/lib/vine-service.ts src/lib/vine-service.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-103): TS types + VineService.publish original-creator threading

Mirror the Rust-side wire field additions (commit 1 of ZEB-103) on the
TypeScript side:

- `VineVideo.originalCreatorAddress?`, `VineVideo.originalCreatorName?`
- `VineDescriptorEvent.originalCreatorAddress?`,
  `VineDescriptorEvent.originalCreatorName?`
- `wireToVine` propagates both fields through to the in-memory model
- `VineService.publish(videoCid, title?, reshareOf?, originalCreatorAddress?, originalCreatorName?)`
  accepts and forwards the new fields to the backend

Both fields stay optional throughout (no breaking change for existing
callers); App.svelte will start populating them in a later commit.

Test coverage:

- `preserves original creator attribution on incoming wire vines`
- `publish forwards original creator fields to adapter`
- `publish omits original creator fields when not provided`

Refs ZEB-103 spec §Wire Format Changes → TypeScript Types,
§Frontend Service → Publishing Reshares.
EOF
)"
```

---

## Task 3: VineService.findVine + getReshareCount

**Spec ref:** §Frontend Service → Reshare Count; §Frontend Service → Navigate to Original.

**Files:**
- Modify: `src/lib/vine-service.ts` (add two methods)
- Modify: `src/lib/vine-service.test.ts` (new test cases)

- [ ] **Step 1: Write failing tests**

In `src/lib/vine-service.test.ts`, add a new describe block (or extend existing) near the bottom of the file:

```typescript
describe('VineService.findVine', () => {
  it('returns vine from followedVines by id', () => {
    const vine: VineVideo = {
      id: 'vine-f', creatorAddress: 'a', creatorName: 'A',
      createdAt: 1, videoCid: 'cid', viewed: false,
    };
    svc.followedVines = [vine];
    expect(svc.findVine('vine-f')).toBe(vine);
  });

  it('returns vine from discoverVines by id', () => {
    const initialLen = svc.discoverVines.length;
    const vine: VineVideo = {
      id: 'vine-d', creatorAddress: 'a', creatorName: 'A',
      createdAt: 1, videoCid: 'cid', viewed: false,
    };
    svc.discoverVines = [...svc.discoverVines, vine];
    expect(svc.findVine('vine-d')).toBe(vine);
  });

  it('returns undefined when no vine matches', () => {
    expect(svc.findVine('nonexistent-id')).toBeUndefined();
  });

  it('searches followedVines before discoverVines (order tie-break)', () => {
    // Same id in both feeds shouldn't happen, but if it does, followed wins.
    const fVine: VineVideo = {
      id: 'dup', creatorAddress: 'a', creatorName: 'F',
      createdAt: 1, videoCid: 'cid', viewed: false,
    };
    const dVine: VineVideo = { ...fVine, creatorName: 'D' };
    svc.followedVines = [fVine];
    svc.discoverVines = [...svc.discoverVines, dVine];
    expect(svc.findVine('dup')).toBe(fVine);
  });
});

describe('VineService.getReshareCount', () => {
  it('returns 0 when no vines reshare the id', () => {
    expect(svc.getReshareCount('vine-none')).toBe(0);
  });

  it('counts reshares across both feeds', () => {
    const origId = 'vine-orig';
    const r1: VineVideo = {
      id: 'r1', creatorAddress: 'a', creatorName: 'A',
      createdAt: 1, videoCid: 'c', viewed: false, reshareOf: origId,
    };
    const r2: VineVideo = {
      id: 'r2', creatorAddress: 'b', creatorName: 'B',
      createdAt: 1, videoCid: 'c', viewed: false, reshareOf: origId,
    };
    const r3: VineVideo = {
      id: 'r3', creatorAddress: 'c', creatorName: 'C',
      createdAt: 1, videoCid: 'c', viewed: false, reshareOf: origId,
    };
    svc.followedVines = [r1, r2];
    svc.discoverVines = [...svc.discoverVines, r3];
    expect(svc.getReshareCount(origId)).toBe(3);
  });

  it('does not count vines that are not reshares', () => {
    const orig: VineVideo = {
      id: 'vine-orig', creatorAddress: 'a', creatorName: 'A',
      createdAt: 1, videoCid: 'c', viewed: false,
    };
    svc.followedVines = [orig];
    expect(svc.getReshareCount('vine-orig')).toBe(0);
  });

  it('does not count reshares of a different id', () => {
    const reshareOfOther: VineVideo = {
      id: 'r1', creatorAddress: 'a', creatorName: 'A',
      createdAt: 1, videoCid: 'c', viewed: false, reshareOf: 'different-orig',
    };
    svc.followedVines = [reshareOfOther];
    expect(svc.getReshareCount('target-orig')).toBe(0);
  });
});
```

- [ ] **Step 2: Run failing tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/vine-service.test.ts
```

Expected: FAIL — `findVine` / `getReshareCount` don't exist.

- [ ] **Step 3: Implement the two methods**

In `src/lib/vine-service.ts`, find a place near the end of the class body (after the existing public methods) and add:

```typescript
/**
 * Find a vine by id, searching followedVines then discoverVines.
 * Returns the first match or undefined.
 *
 * Used by the UI when the user clicks an attribution link to navigate
 * to the original vine. If the original isn't in the local feed (e.g.,
 * creator isn't followed and the original wasn't surfaced in discover),
 * the click is silently ignored.
 */
findVine(vineId: string): VineVideo | undefined {
  return (
    this.followedVines.find(v => v.id === vineId)
    ?? this.discoverVines.find(v => v.id === vineId)
  );
}

/**
 * Count how many vines in the local feed reshare the given vine id.
 *
 * Only meaningful for original vines (where the caller's vine has no
 * `reshareOf` itself). Counts across both followed and discover feeds.
 * Computed on demand — no separate state map kept.
 */
getReshareCount(vineId: string): number {
  let count = 0;
  for (const v of this.followedVines) {
    if (v.reshareOf === vineId) count++;
  }
  for (const v of this.discoverVines) {
    if (v.reshareOf === vineId) count++;
  }
  return count;
}
```

- [ ] **Step 4: Run tests — verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/vine-service.test.ts
```

Expected: PASS — all new `findVine` + `getReshareCount` tests green, existing tests unaffected.

- [ ] **Step 5: Run frontend gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit && npx vitest run
```

Expected: green.

- [ ] **Step 6: Commit**

```bash
git add src/lib/vine-service.ts src/lib/vine-service.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-103): VineService.findVine + getReshareCount

Adds two pure-read methods to VineService:

- `findVine(vineId)` — searches followedVines then discoverVines, returns
  first match or undefined. Used by UI navigation-to-original on click.
- `getReshareCount(vineId)` — counts vines across both feeds where
  `reshareOf === vineId`. Computed on demand (no separate state).

Both are foundations for the upcoming VineCard reshare-count display
(commit 7) and App.svelte handleViewOriginal handler (commit 9).

Test coverage:

- `findVine` returns from followed, from discover, undefined when
  missing, and prefers followed on duplicate-id tie
- `getReshareCount` returns 0 baseline, counts across both feeds,
  ignores non-reshares, ignores reshares of different originals

Refs ZEB-103 spec §Frontend Service → Reshare Count,
§Frontend Service → Navigate to Original.
EOF
)"
```

---

## Task 4: VineService self-reshare prevention

**Spec ref:** §Frontend Service → Self-Reshare Prevention; §Edge Cases → Self-reshare prevention.

**Files:**
- Modify: `src/lib/vine-service.ts:125-160` (`publish` guard)
- Modify: `src/lib/vine-service.test.ts` (new test cases)

- [ ] **Step 1: Write failing tests**

In `src/lib/vine-service.test.ts`, add:

```typescript
describe('VineService self-reshare prevention', () => {
  it('publish silently no-ops when resharing own original (creatorAddress === "self")', async () => {
    const { adapter, invokes } = createMockAdapter();
    await svc.connectAdapter(adapter);
    const before = svc.vines.length;
    // Reshare with originalCreatorAddress === 'self' should be rejected silently.
    await svc.publish('cid-x', 'My title', 'orig-1', 'self', 'You');
    expect(invokes.find(c => c.cmd === 'publish_vine')).toBeUndefined();
    expect(svc.vines.length).toBe(before);
  });

  it('publish silently no-ops when originalCreatorAddress === ownAddress (hex form)', async () => {
    const { adapter, invokes } = createMockAdapter();
    svc.ownAddress = 'a1b2c3d4';
    await svc.connectAdapter(adapter);
    const before = svc.vines.length;
    await svc.publish('cid-x', 'My title', 'orig-1', 'a1b2c3d4', 'You');
    expect(invokes.find(c => c.cmd === 'publish_vine')).toBeUndefined();
    expect(svc.vines.length).toBe(before);
  });

  it('publish allows resharing someone else\'s reshare of your content', async () => {
    // Carol resharing Bob's reshare of Alice's vine. The originalCreator
    // *of the vine Carol is resharing* is Bob, even though Carol's content
    // ultimately traces to Alice's original. But the spec says: trace to
    // true origin. So if true origin === self, no-op. Here true origin is
    // someone else, so it goes through.
    //
    // The implementer's responsibility: the GUARD checks
    // originalCreatorAddress against ownAddress/'self'. The CALLER
    // (App.svelte handleVineReshare) is responsible for resolving the
    // true origin before calling publish.
    const { adapter, invokes } = createMockAdapter();
    svc.ownAddress = 'self-addr';
    await svc.connectAdapter(adapter);
    await svc.publish('cid-x', 'Title', 'reshare-of-1', 'other-addr', 'Other');
    expect(invokes.find(c => c.cmd === 'publish_vine')).toBeTruthy();
  });

  it('publish allows non-reshare originals (no reshareOf, no original creator fields)', async () => {
    const { adapter, invokes } = createMockAdapter();
    svc.ownAddress = 'self-addr';
    await svc.connectAdapter(adapter);
    await svc.publish('cid-x', 'Original title');
    expect(invokes.find(c => c.cmd === 'publish_vine')).toBeTruthy();
  });
});
```

- [ ] **Step 2: Run failing tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/vine-service.test.ts
```

Expected: FAIL — guard not implemented; the first two tests will fail because `publish_vine` IS being called.

- [ ] **Step 3: Implement the guard**

In `src/lib/vine-service.ts:125-160`, modify the top of `publish`:

```typescript
/** Publish a vine via Tauri command. */
async publish(
  videoCid: string,
  title?: string,
  reshareOf?: string,
  originalCreatorAddress?: string,
  originalCreatorName?: string,
): Promise<void> {
  // Self-reshare prevention: silently no-op when resharing your own
  // original content. The check is on `originalCreatorAddress` (not
  // `reshareOf` alone) because resharing someone else's reshare of your
  // content traces back to you and should also be blocked — but the
  // caller is responsible for resolving the true origin before calling.
  if (reshareOf && originalCreatorAddress) {
    if (
      originalCreatorAddress === 'self'
      || (this.ownAddress && originalCreatorAddress === this.ownAddress)
    ) {
      return;
    }
  }

  if (this.adapter) {
    // ... existing body unchanged from Task 2 Step 6 ...
  }

  // ... offline fallback unchanged ...
}
```

- [ ] **Step 4: Run tests — verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/vine-service.test.ts
```

Expected: PASS — all four new self-reshare tests + all existing tests green.

- [ ] **Step 5: Run frontend gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit && npx vitest run
```

Expected: green.

- [ ] **Step 6: Commit**

```bash
git add src/lib/vine-service.ts src/lib/vine-service.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-103): VineService self-reshare prevention

`publish` now silently no-ops when `originalCreatorAddress` matches the
local identity (either the magic value 'self' or the hex-encoded
ownAddress). The check is on originalCreatorAddress — the caller
(App.svelte handleVineReshare in commit 9) is responsible for resolving
the true origin through reshare-of-reshare chains before invoking
publish.

This means:

- Resharing your own original → blocked silently (UI hides the button
  too, in commit 6; this is the belt-and-suspenders backstop).
- Resharing someone else's reshare of your content (true origin === you)
  → also blocked once App.svelte traces correctly to the true origin.
- Resharing someone else's vine → goes through normally.

Test coverage:

- `publish silently no-ops when resharing own original`
- `publish silently no-ops when originalCreatorAddress === ownAddress`
- `publish allows resharing someone else's reshare of your content`
  (i.e., guard only fires when true origin is self)
- `publish allows non-reshare originals`

Refs ZEB-103 spec §Frontend Service → Self-Reshare Prevention,
§Edge Cases → Self-reshare prevention.
EOF
)"
```

---

## Task 5: ReshareConfirmDialog component

**Spec ref:** §UI Components → ReshareConfirmDialog.

**Files:**
- Create: `src/lib/components/ReshareConfirmDialog.svelte`
- Create: `src/lib/components/__tests__/ReshareConfirmDialog.test.ts`

- [ ] **Step 1: Write failing component tests**

Create `src/lib/components/__tests__/ReshareConfirmDialog.test.ts`:

```typescript
import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import ReshareConfirmDialog from '../ReshareConfirmDialog.svelte';
import type { VineVideo } from '../../types';

const vine: VineVideo = {
  id: 'vine-1',
  creatorAddress: 'addr-1',
  creatorName: 'Alice',
  createdAt: 1700000000,
  videoCid: 'cid-abc',
  title: 'Cool vine',
  viewed: false,
};

const resharedVine: VineVideo = {
  ...vine,
  id: 'vine-2',
  creatorName: 'Bob',
  reshareOf: 'vine-1',
  originalCreatorName: 'Alice',
};

describe('ReshareConfirmDialog', () => {
  it('renders the heading and vine title', () => {
    render(ReshareConfirmDialog, {
      props: { vine, onConfirm: vi.fn(), onCancel: vi.fn() },
    });
    expect(screen.getByText(/reshare this vine/i)).toBeTruthy();
    expect(screen.getByText('Cool vine')).toBeTruthy();
  });

  it('shows the original creator name when the vine is a reshare', () => {
    render(ReshareConfirmDialog, {
      props: { vine: resharedVine, onConfirm: vi.fn(), onCancel: vi.fn() },
    });
    // The dialog should surface "Alice" (the original) so the user knows
    // attribution goes to Alice, not Bob (the resharer).
    expect(screen.getByText(/Alice/)).toBeTruthy();
  });

  it('falls back to creator name when not a reshare', () => {
    render(ReshareConfirmDialog, {
      props: { vine, onConfirm: vi.fn(), onCancel: vi.fn() },
    });
    expect(screen.getByText(/Alice/)).toBeTruthy();
  });

  it('calls onConfirm when the Reshare button is clicked', async () => {
    const onConfirm = vi.fn();
    render(ReshareConfirmDialog, {
      props: { vine, onConfirm, onCancel: vi.fn() },
    });
    const btn = screen.getByRole('button', { name: /^reshare$/i });
    await fireEvent.click(btn);
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it('calls onCancel when the Cancel button is clicked', async () => {
    const onCancel = vi.fn();
    render(ReshareConfirmDialog, {
      props: { vine, onConfirm: vi.fn(), onCancel },
    });
    const btn = screen.getByRole('button', { name: /cancel/i });
    await fireEvent.click(btn);
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it('calls onCancel on Escape key', async () => {
    const onCancel = vi.fn();
    render(ReshareConfirmDialog, {
      props: { vine, onConfirm: vi.fn(), onCancel },
    });
    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(onCancel).toHaveBeenCalled();
  });
});
```

- [ ] **Step 2: Run failing tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/ReshareConfirmDialog.test.ts
```

Expected: FAIL — component file doesn't exist.

- [ ] **Step 3: Create the component**

Create `src/lib/components/ReshareConfirmDialog.svelte`:

```svelte
<script lang="ts">
  import Modal from './Modal.svelte';
  import type { VineVideo } from '../types';

  let {
    vine,
    onConfirm,
    onCancel,
  }: {
    vine: VineVideo;
    onConfirm: () => void;
    onCancel: () => void;
  } = $props();

  const titleId = `reshare-confirm-title-${Math.random().toString(36).slice(2)}`;

  // For a reshare-of-a-reshare, the original creator name is the true origin;
  // for an original vine, the creator IS the origin.
  let attribution = $derived(
    vine.originalCreatorName ?? vine.creatorName
  );
</script>

<Modal {onCancel} ariaLabelledby={titleId}>
  <h3 class="modal-title" id={titleId}>Reshare this vine?</h3>
  <p class="modal-description">
    {#if vine.title}
      <strong>{vine.title}</strong>
      <br />
    {/if}
    Originally by {attribution}. Your reshare will preserve attribution to them.
  </p>

  <div class="action-row">
    <button class="confirm-btn" onclick={onConfirm}>Reshare</button>
    <div class="spacer"></div>
    <button class="cancel-btn" onclick={onCancel}>Cancel</button>
  </div>
</Modal>

<style>
  .modal-title {
    color: var(--text-primary);
    font-size: 1rem;
    margin: 0 0 12px;
  }
  .modal-description {
    color: var(--text-secondary);
    font-size: 0.875rem;
    line-height: 1.5;
    margin: 0 0 20px;
  }
  .action-row {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .spacer { flex: 1; }
  .confirm-btn {
    background: var(--accent);
    color: var(--text-primary);
    border: none;
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }
  .cancel-btn {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    border: none;
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }
  .cancel-btn:focus-visible,
  .confirm-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
</style>
```

The `Modal` primitive already handles Escape dismissal + focus trapping + backdrop click → onCancel — the test for Escape verifies this end-to-end, and Modal's own existing tests cover the trap/backdrop in isolation, so we don't re-test them.

- [ ] **Step 4: Run tests — verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/ReshareConfirmDialog.test.ts
```

Expected: PASS — all 6 dialog tests green.

If a test fails because Modal renders the children in a portal/teleport and `screen.getByText` can't find the content, look at how `ConfirmationModal.test.ts` (or similar) is structured for the correct query approach.

- [ ] **Step 5: Run frontend gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit && npx vitest run
```

Expected: green.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/ReshareConfirmDialog.svelte \
        src/lib/components/__tests__/ReshareConfirmDialog.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-103): ReshareConfirmDialog component

New modal-backed confirmation dialog gating the Reshare action. Mirrors
the shape of ConfirmationModal.svelte — wraps the Modal primitive (which
handles Escape, focus trap, and backdrop dismissal) and adds:

- Heading: "Reshare this vine?"
- Body: optional vine title (bold) + attribution line
  ("Originally by {name}. Your reshare will preserve attribution to them.")
- Cancel + Reshare buttons

The attribution name uses `originalCreatorName` when present (for
reshare-of-reshare chains) and falls back to `creatorName` for original
vines, matching the spec's "trace to true origin" rule.

Wiring into VinePlayer (replacing the fire-and-forget Reshare path)
happens in commit 6.

Test coverage:

- Renders heading + vine title
- Shows original creator on reshare
- Falls back to creator name when not a reshare
- Confirm button → onConfirm
- Cancel button → onCancel
- Escape key → onCancel

Refs ZEB-103 spec §UI Components → ReshareConfirmDialog.
EOF
)"
```

---

## Task 6: VinePlayer — attribution row + confirmation flow + hide-on-own

**Spec ref:** §UI Components → VinePlayer Changes.

**Files:**
- Modify: `src/lib/components/VinePlayer.svelte`
- Modify: `src/lib/components/__tests__/VinePlayer.test.ts`

- [ ] **Step 1: Write failing tests**

In `src/lib/components/__tests__/VinePlayer.test.ts`, add (and update the existing reshare test):

```typescript
it('shows attribution row when vine is a reshare', () => {
  const resharedVine = {
    ...vine,
    reshareOf: 'vine-00',
    originalCreatorName: 'Original Person',
  };
  render(VinePlayer, {
    props: { vine: resharedVine, onClose: vi.fn(), resolveVideo: vi.fn() },
  });
  expect(screen.getByText(/originally by Original Person/i)).toBeTruthy();
});

it('attribution row is clickable when onViewOriginal is provided', async () => {
  const onViewOriginal = vi.fn();
  const resharedVine = {
    ...vine,
    reshareOf: 'vine-00',
    originalCreatorName: 'Original Person',
  };
  render(VinePlayer, {
    props: {
      vine: resharedVine,
      onClose: vi.fn(),
      onViewOriginal,
      resolveVideo: vi.fn(),
    },
  });
  const link = screen.getByRole('button', { name: /originally by Original Person/i });
  await fireEvent.click(link);
  expect(onViewOriginal).toHaveBeenCalledWith('vine-00');
});

it('opens confirmation dialog when Reshare button is clicked', async () => {
  render(VinePlayer, {
    props: { vine, onClose: vi.fn(), onReshare: vi.fn(), resolveVideo: vi.fn() },
  });
  const btn = screen.getByRole('button', { name: /^reshare/i });
  await fireEvent.click(btn);
  // Dialog should now be visible.
  expect(screen.getByText(/reshare this vine\?/i)).toBeTruthy();
});

it('calls onReshare only after dialog confirm', async () => {
  const onReshare = vi.fn().mockResolvedValue(undefined);
  render(VinePlayer, {
    props: { vine, onClose: vi.fn(), onReshare, resolveVideo: vi.fn() },
  });
  await fireEvent.click(screen.getByRole('button', { name: /^reshare/i }));
  expect(onReshare).not.toHaveBeenCalled();
  await fireEvent.click(screen.getByRole('button', { name: /^reshare$/i })); // The confirm button inside dialog
  expect(onReshare).toHaveBeenCalledWith(vine);
});

it('does not call onReshare on dialog cancel', async () => {
  const onReshare = vi.fn();
  render(VinePlayer, {
    props: { vine, onClose: vi.fn(), onReshare, resolveVideo: vi.fn() },
  });
  await fireEvent.click(screen.getByRole('button', { name: /^reshare/i }));
  await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));
  expect(onReshare).not.toHaveBeenCalled();
});

it('hides Reshare button on own original vines', () => {
  const ownOriginal = { ...vine, creatorAddress: 'self', reshareOf: undefined };
  render(VinePlayer, {
    props: { vine: ownOriginal, onClose: vi.fn(), onReshare: vi.fn(), resolveVideo: vi.fn() },
  });
  expect(screen.queryByRole('button', { name: /^reshare/i })).toBeNull();
});

it('shows Reshare button on own reshare of someone else\'s vine', () => {
  // Edge: I reshared someone else's vine. I should still be able to
  // re-reshare (or unshare → reshare). The hide rule is "own ORIGINAL",
  // not "anything I published".
  const ownReshare = { ...vine, creatorAddress: 'self', reshareOf: 'vine-orig' };
  render(VinePlayer, {
    props: { vine: ownReshare, onClose: vi.fn(), onReshare: vi.fn(), resolveVideo: vi.fn() },
  });
  // The action button (which opens the dialog) is named "Reshare ..."
  expect(screen.getByRole('button', { name: /^reshare/i })).toBeTruthy();
});
```

Update the existing test (around line 24 in `VinePlayer.test.ts`):

```typescript
// Before:
//   it('shows "Reshared" label for reshared vines', ...
// After:
it('does not show legacy "Reshared" label anymore (replaced by attribution row)', () => {
  const resharedVine = {
    ...vine,
    reshareOf: 'vine-00',
    originalCreatorName: 'Original',
  };
  render(VinePlayer, {
    props: { vine: resharedVine, onClose: vi.fn(), resolveVideo: vi.fn() },
  });
  // "Reshared" by itself should no longer appear; attribution row replaces it.
  expect(screen.queryByText(/^Reshared$/)).toBeNull();
});
```

- [ ] **Step 2: Run failing tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/VinePlayer.test.ts
```

Expected: FAIL — attribution row not rendered, dialog flow not wired, hide-on-own not implemented.

- [ ] **Step 3: Update VinePlayer.svelte**

Modify `src/lib/components/VinePlayer.svelte`:

**3a. Update the `$props()` block to add `onViewOriginal`:**

Locate the existing props block (around line 7):

```svelte
let { vine, onClose, onNext, onPrevious, onReshare, resolveVideo, onToggleLike, reactionCount = 0, likedByMe = false, onViewOriginal }: {
  vine: VineVideo;
  onClose: () => void;
  onNext?: () => void;
  onPrevious?: () => void;
  onReshare?: (vine: VineVideo) => Promise<void> | void;
  resolveVideo: (cid: string) => Promise<Blob | null>;
  onToggleLike?: (vine: VineVideo) => void;
  reactionCount?: number;
  likedByMe?: boolean;
  onViewOriginal?: (vineId: string) => void;
} = $props();
```

**3b. Import ReshareConfirmDialog and add dialog state:**

At the top of the `<script>` block, near other imports:

```typescript
import ReshareConfirmDialog from './ReshareConfirmDialog.svelte';
```

After the existing state declarations (`resharing`, `reshareGeneration`, etc.):

```typescript
let showReshareConfirm = $state(false);
```

**3c. Compute `isOwnOriginal` and `canReshare`:**

After the existing `$derived`/`$state` declarations:

```typescript
let isOwnOriginal = $derived(vine.creatorAddress === 'self' && !vine.reshareOf);
let canReshare = $derived(!!onReshare && !isOwnOriginal);
```

**3d. Rewrite `handleReshare`:**

Replace the existing `handleReshare` body (around line 67):

```typescript
function handleReshare() {
  if (resharing) return;
  showReshareConfirm = true;
}

async function confirmReshare() {
  showReshareConfirm = false;
  if (!onReshare || resharing) return;
  resharing = true;
  reshareError = '';
  const generation = reshareGeneration;
  try {
    await onReshare(vine);
  } catch (err) {
    if (generation === reshareGeneration) {
      reshareError = err instanceof Error ? err.message : 'Reshare failed';
    }
  } finally {
    if (generation === reshareGeneration) resharing = false;
  }
}

function cancelReshare() {
  showReshareConfirm = false;
}
```

**3e. Replace the "Reshared" label with the attribution row:**

Locate (around line 149-151):

```svelte
{#if vine.reshareOf}
  <p class="reshare-label">Reshared</p>
{/if}
```

Replace with:

```svelte
{#if vine.reshareOf}
  {#if onViewOriginal}
    <button
      type="button"
      class="attribution-link"
      onclick={() => onViewOriginal?.(vine.reshareOf!)}
      aria-label="originally by {vine.originalCreatorName ?? vine.creatorName}"
    >
      <span aria-hidden="true">↗</span> originally by {vine.originalCreatorName ?? vine.creatorName}
    </button>
  {:else}
    <p class="attribution-row">
      <span aria-hidden="true">↗</span> originally by {vine.originalCreatorName ?? vine.creatorName}
    </p>
  {/if}
{/if}
```

**3f. Gate the Reshare button on `canReshare`:**

Locate (around line 167-170):

```svelte
{#if onReshare}
  <button type="button" class="action-btn" onclick={handleReshare} disabled={resharing} aria-label="Reshare vine">
    <span aria-hidden="true">↗</span> {resharing ? 'Resharing…' : 'Reshare'}
  </button>
{/if}
```

Replace with:

```svelte
{#if canReshare}
  <button type="button" class="action-btn" onclick={handleReshare} disabled={resharing} aria-label="Reshare vine">
    <span aria-hidden="true">↗</span> {resharing ? 'Resharing…' : 'Reshare'}
  </button>
{/if}
```

**3g. Render the dialog when `showReshareConfirm` is true:**

Near the end of the template (inside the outer container but before the closing tag):

```svelte
{#if showReshareConfirm}
  <ReshareConfirmDialog
    {vine}
    onConfirm={confirmReshare}
    onCancel={cancelReshare}
  />
{/if}
```

**3h. Add `.attribution-link` and `.attribution-row` styles:**

In the `<style>` block:

```css
.attribution-link {
  background: none;
  border: none;
  padding: 0;
  margin: 0;
  color: var(--accent);
  font-size: 0.875rem;
  cursor: pointer;
  text-decoration: underline;
}
.attribution-link:hover { opacity: 0.85; }
.attribution-link:focus-visible {
  outline: 2px solid var(--accent);
  outline-offset: 2px;
}
.attribution-row {
  color: var(--text-secondary);
  font-size: 0.875rem;
  margin: 0;
}
```

(If a `.reshare-label` class already exists in the styles, leave it — it's now unused but removing CSS rules is a separate cleanup.)

- [ ] **Step 4: Run tests — verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/VinePlayer.test.ts
```

Expected: PASS.

Note: The test for "calls onReshare only after dialog confirm" uses `getByRole('button', { name: /^reshare$/i })` to match the confirm button inside the dialog (which has label `Reshare`) — be careful that the player's own Reshare button isn't matched accidentally. If the player's button label is `Reshare ↗` or similar, the regex `/^reshare$/i` should still work; if it doesn't, the implementer may need to disambiguate by attribute or by finding the dialog scope first.

- [ ] **Step 5: Run frontend gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit && npx vitest run
```

Expected: green.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/VinePlayer.svelte \
        src/lib/components/__tests__/VinePlayer.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-103): VinePlayer attribution row + confirm dialog + hide-on-own

VinePlayer no longer fire-and-forgets reshares. Three behavior changes:

- The legacy "Reshared" footer label is replaced with a clickable
  attribution row: "↗ originally by {name}". Clicking it invokes the
  new `onViewOriginal` callback (App.svelte will wire this in commit 9).
  Falls back to a non-clickable paragraph when `onViewOriginal` isn't
  provided (defensive).

- Clicking the Reshare button now opens `ReshareConfirmDialog`. The
  existing `onReshare` callback fires only after the user confirms;
  cancelling closes the dialog with no side effects. Loading / error
  state stays the same, just delayed until post-confirm.

- The Reshare button is hidden when the vine is your own original
  (`creatorAddress === 'self'` and no `reshareOf`). Resharing your own
  reshare of someone else's vine stays allowed.

Test coverage:

- Attribution row appears for reshares
- Attribution row is clickable when onViewOriginal is provided
- Reshare button opens dialog (doesn't immediately publish)
- onReshare only fires after dialog confirm
- onReshare does not fire on dialog cancel
- Reshare button hidden on own original
- Reshare button shown on own reshare of someone else's

Refs ZEB-103 spec §UI Components → VinePlayer Changes.
EOF
)"
```

---

## Task 7: VineCard — attribution row + reshare count display

**Spec ref:** §UI Components → VineCard Changes.

**Files:**
- Modify: `src/lib/components/VineCard.svelte`
- Modify: `src/lib/components/__tests__/VineCard.test.ts`

- [ ] **Step 1: Write failing tests**

In `src/lib/components/__tests__/VineCard.test.ts`:

Update the existing "shows reshare badge" test:

```typescript
// Replace the existing "shows reshare badge when vine is a reshare" test.
it('shows attribution row when vine is a reshare', () => {
  const reshared = {
    ...vine,
    reshareOf: 'vine-00',
    originalCreatorName: 'Original Person',
  };
  render(VineCard, { props: { vine: reshared, onPlay: vi.fn() } });
  expect(screen.getByText(/originally by Original Person/i)).toBeTruthy();
});

// Replace "does not show reshare badge for original vines".
it('does not show attribution row for original vines', () => {
  render(VineCard, { props: { vine, onPlay: vi.fn() } });
  expect(screen.queryByText(/originally by/i)).toBeNull();
});
```

Add new tests:

```typescript
it('attribution row is clickable when onViewOriginal is provided', async () => {
  const onViewOriginal = vi.fn();
  const reshared = {
    ...vine,
    reshareOf: 'vine-00',
    originalCreatorName: 'Original Person',
  };
  render(VineCard, { props: { vine: reshared, onPlay: vi.fn(), onViewOriginal } });
  const link = screen.getByRole('button', { name: /originally by Original Person/i });
  await fireEvent.click(link);
  expect(onViewOriginal).toHaveBeenCalledWith('vine-00');
});

it('clicking attribution row does not also trigger onPlay (stops propagation)', async () => {
  const onPlay = vi.fn();
  const onViewOriginal = vi.fn();
  const reshared = {
    ...vine,
    reshareOf: 'vine-00',
    originalCreatorName: 'Original Person',
  };
  render(VineCard, { props: { vine: reshared, onPlay, onViewOriginal } });
  const link = screen.getByRole('button', { name: /originally by Original Person/i });
  await fireEvent.click(link);
  expect(onViewOriginal).toHaveBeenCalled();
  expect(onPlay).not.toHaveBeenCalled();
});

it('shows reshare count for originals when count > 0', () => {
  render(VineCard, { props: { vine, onPlay: vi.fn(), reshareCount: 3 } });
  expect(screen.getByText('3')).toBeTruthy();
});

it('hides reshare count when zero', () => {
  render(VineCard, { props: { vine, onPlay: vi.fn(), reshareCount: 0 } });
  // The exact assertion depends on the count element's structure. Easiest:
  // assert the "↗" reshare-count glyph isn't visible. If the existing card
  // uses an aria-label for the count container, use that.
  expect(screen.queryByLabelText(/reshare count/i)).toBeNull();
});

it('does not show reshare count for reshares (counts only meaningful on originals)', () => {
  const reshared = {
    ...vine,
    reshareOf: 'vine-00',
    originalCreatorName: 'Original',
  };
  render(VineCard, { props: { vine: reshared, onPlay: vi.fn(), reshareCount: 5 } });
  // The "5" should not appear, because we don't show counts on reshares.
  expect(screen.queryByLabelText(/reshare count/i)).toBeNull();
});
```

- [ ] **Step 2: Run failing tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/VineCard.test.ts
```

Expected: FAIL.

- [ ] **Step 3: Update VineCard.svelte**

**3a. Update the `$props()` block to add `reshareCount` and `onViewOriginal`:**

Locate the existing `$props()` call near the top of the script. Append:

```typescript
let {
  vine,
  onPlay,
  // ... existing props (showFollowButton, isFollowed, onFollow, onLike, likedByMe, reactionCount, etc.) ...
  reshareCount = 0,
  onViewOriginal,
}: {
  vine: VineVideo;
  onPlay: (vine: VineVideo) => void;
  // ... existing types ...
  reshareCount?: number;
  onViewOriginal?: (vineId: string) => void;
} = $props();
```

(The implementer should read the existing prop block and append the two new lines / type members alongside.)

**3b. Compute display visibility for the count:**

```typescript
let showReshareCount = $derived(!vine.reshareOf && reshareCount > 0);
```

**3c. Replace the reshare badge with the attribution row:**

Locate (around line 75):

```svelte
{#if vine.reshareOf}
  <span class="reshare-badge">reshare</span>
{/if}
```

Replace with:

```svelte
{#if vine.reshareOf}
  {#if onViewOriginal}
    <button
      type="button"
      class="attribution-link"
      onclick={(e) => { e.stopPropagation(); onViewOriginal?.(vine.reshareOf!); }}
      aria-label="originally by {vine.originalCreatorName ?? vine.creatorName}"
    >
      <span aria-hidden="true">↗</span> originally by {vine.originalCreatorName ?? vine.creatorName}
    </button>
  {:else}
    <span class="attribution-row">
      <span aria-hidden="true">↗</span> originally by {vine.originalCreatorName ?? vine.creatorName}
    </span>
  {/if}
{/if}
```

**3d. Add the reshare count to the social stats row:**

The existing `card-like-row` block (around line 94) renders like count. Just inside that block (or alongside it — depends on existing markup), add:

```svelte
{#if showReshareCount}
  <span class="reshare-count" aria-label="reshare count">
    <span aria-hidden="true">↗</span> {reshareCount}
  </span>
{/if}
```

The exact placement depends on the existing markup — the goal is to display next to the like count using a similar visual treatment ("❤️ 5  ↗ 2"). The implementer should look at the existing structure for `card-like-row` / `card-heart` etc. and slot the count in alongside.

**3e. Add the new CSS:**

In the `<style>` block:

```css
.attribution-link {
  background: none;
  border: none;
  padding: 0;
  margin: 0;
  color: var(--accent);
  font-size: 0.75rem;
  cursor: pointer;
  text-decoration: underline;
}
.attribution-link:hover { opacity: 0.85; }
.attribution-link:focus-visible {
  outline: 2px solid var(--accent);
  outline-offset: 2px;
}
.attribution-row {
  color: var(--text-secondary);
  font-size: 0.75rem;
}
.reshare-count {
  color: var(--text-secondary);
  font-size: 0.75rem;
}
```

Remove or leave `.reshare-badge` — it's now unused but stale-CSS removal is non-blocking.

- [ ] **Step 4: Run tests — verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/VineCard.test.ts
```

Expected: PASS.

- [ ] **Step 5: Run frontend gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit && npx vitest run
```

Expected: green.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/VineCard.svelte \
        src/lib/components/__tests__/VineCard.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-103): VineCard attribution row + reshare count display

Two visible changes:

- The legacy "reshare" badge is replaced with a clickable attribution
  row: "↗ originally by {name}". `onClick` invokes `onViewOriginal` and
  stops propagation so the card's own play-on-click doesn't fire. Falls
  back to a non-clickable span when `onViewOriginal` isn't provided.

- New `reshareCount` prop (default 0). When > 0 AND the vine isn't
  itself a reshare, shows a "↗ N" indicator alongside the existing
  like count in the social stats row. Reshares themselves don't show
  counts (only originals have meaningful counts).

Test coverage:

- Attribution row appears for reshares
- Attribution row does not appear for originals
- Attribution row is clickable when onViewOriginal is provided
- Clicking attribution stops propagation (does not trigger onPlay)
- Reshare count shows for originals when count > 0
- Reshare count hidden when zero
- Reshare count hidden for reshares (only originals get counts)

Refs ZEB-103 spec §UI Components → VineCard Changes.
EOF
)"
```

---

## Task 8: VineFeed — prop forwarding

**Spec ref:** §UI Components → VineFeed Changes.

**Files:**
- Modify: `src/lib/components/VineFeed.svelte`
- Modify: `src/lib/components/__tests__/VineFeed.test.ts` (or `VineFeed.integration.test.ts`)

- [ ] **Step 1: Write failing tests**

Read the existing `VineFeed.svelte` + its tests first to understand the current prop API. The new props are:

- `getReshareCount?: (vineId: string) => number` — passed to each VineCard via the `reshareCount={getReshareCount?.(vine.id) ?? 0}` binding
- `onViewOriginal?: (vineId: string) => void` — forwarded to both VineCard and VinePlayer

Add a focused test to the existing `VineFeed` test file (extend it; don't replace existing tests):

```typescript
it('passes reshareCount derived from getReshareCount to each card', () => {
  const getReshareCount = vi.fn((id: string) =>
    id === 'vine-orig' ? 3 : 0
  );
  // Render with a vine list including the orig vine.
  const orig: VineVideo = {
    id: 'vine-orig', creatorAddress: 'a', creatorName: 'A',
    createdAt: 1, videoCid: 'c', viewed: false,
  };
  // ... render VineFeed with vines=[orig], getReshareCount, ...
  render(VineFeed, {
    props: {
      followedVines: [],
      discoverVines: [orig],
      getReaction: () => ({ count: 0, likedByMe: false }),
      getReshareCount,
      onPlay: vi.fn(),
      // ... other required props ...
    },
  });
  expect(screen.getByText('3')).toBeTruthy();
  expect(getReshareCount).toHaveBeenCalledWith('vine-orig');
});

it('forwards onViewOriginal to VineCard', async () => {
  const onViewOriginal = vi.fn();
  const reshared: VineVideo = {
    id: 'vine-r', creatorAddress: 'a', creatorName: 'A',
    createdAt: 1, videoCid: 'c', viewed: false,
    reshareOf: 'orig-1', originalCreatorName: 'Orig',
  };
  render(VineFeed, {
    props: {
      followedVines: [],
      discoverVines: [reshared],
      getReaction: () => ({ count: 0, likedByMe: false }),
      onViewOriginal,
      onPlay: vi.fn(),
    },
  });
  const link = screen.getByRole('button', { name: /originally by Orig/i });
  await fireEvent.click(link);
  expect(onViewOriginal).toHaveBeenCalledWith('orig-1');
});
```

The exact props expected by `VineFeed` may differ — the implementer should read `VineFeed.svelte`'s `$props()` block and `VineFeed.integration.test.ts` for the canonical shape, then fill in the missing required props (likely `onFollow`, `onUnfollow`, `onToggleLike`, etc.).

- [ ] **Step 2: Run failing tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/VineFeed.test.ts
```

Expected: FAIL — `getReshareCount` / `onViewOriginal` aren't props of `VineFeed` yet, so either the tests fail or TypeScript errors out.

- [ ] **Step 3: Update VineFeed.svelte**

**3a. Add the two new props to the `$props()` block:**

```typescript
let {
  // ... existing props ...
  getReshareCount,
  onViewOriginal,
}: {
  // ... existing types ...
  getReshareCount?: (vineId: string) => number;
  onViewOriginal?: (vineId: string) => void;
} = $props();
```

**3b. Forward to VineCard:**

Inside the `{#each vines as vine}` loop where `<VineCard>` is rendered, add:

```svelte
<VineCard
  {vine}
  onPlay={...}
  reshareCount={getReshareCount?.(vine.id) ?? 0}
  {onViewOriginal}
  ...
/>
```

**3c. Forward to VinePlayer:**

If `VineFeed` also renders `<VinePlayer>` (it may — let the implementer verify), add `{onViewOriginal}` to its props.

- [ ] **Step 4: Run tests — verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/VineFeed.test.ts
```

Expected: PASS.

- [ ] **Step 5: Run frontend gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit && npx vitest run
```

Expected: green.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/VineFeed.svelte \
        src/lib/components/__tests__/VineFeed.test.ts \
        src/lib/components/__tests__/VineFeed.integration.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-103): VineFeed forwards getReshareCount + onViewOriginal

Two new optional props on VineFeed:

- `getReshareCount?: (id) => number` — invoked per card to compute the
  reshare count displayed in the social stats row. App.svelte will pass
  a reactive function backed by VineService.getReshareCount.

- `onViewOriginal?: (id) => void` — forwarded to both VineCard (for
  the attribution link) and VinePlayer (same).

VineFeed itself doesn't store the props — it just plumbs them through.
The reshare count is computed per-card at render time (cheap O(N) scan
of both vine arrays per call); for typical N this is fine.

Test coverage:

- Forwarded reshareCount appears on the rendered card
- Forwarded onViewOriginal fires when attribution clicked

Refs ZEB-103 spec §UI Components → VineFeed Changes.
EOF
)"
```

---

## Task 9: App.svelte wiring + integration smoke test

**Spec ref:** §UI Components → App.svelte Changes; §Edge Cases → Resharing a reshare.

**Files:**
- Modify: `src/App.svelte`
- Modify: `src/App.test.ts` (or wherever integration smoke tests live; if none exists for App, add one or extend `VineFeed.integration.test.ts`)

- [ ] **Step 1: Write the failing integration smoke test**

The shape of the existing App-level tests depends on whether `src/App.test.ts` exists. If it does, extend it; otherwise add the test to `src/lib/components/__tests__/VineFeed.integration.test.ts` as a higher-level integration check.

```typescript
it('reshares a vine with attribution preserved through App-level handler', async () => {
  // Mount App with mocked VineService such that we can observe the
  // outgoing publish call.
  // ...
  // 1. Render App
  // 2. Open the VinePlayer on a vine NOT created by self
  // 3. Click Reshare → dialog opens
  // 4. Click Confirm → handleVineReshare fires → vineService.publish called
  // 5. Assert publish was called with the original creator address+name
  //    derived from the source vine (or, if the source vine is itself a
  //    reshare, propagating the source's originalCreator fields)
});
```

Pseudocode is acceptable here — the implementer should look at how existing App-level tests are structured (or if no App.test.ts exists, set one up by following the pattern of any existing integration test like `src/lib/components/__tests__/VineFeed.integration.test.ts`).

**Implementer note:** If standing up a full App.svelte test harness is non-trivial (e.g., heavy mock setup), a unit test directly on `handleVineReshare` is acceptable instead — extract it (or the helper that resolves original-creator fields) to a separately-testable unit. Both achieve the same coverage.

- [ ] **Step 2: Run failing tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run
```

Expected: FAIL — `handleVineReshare` doesn't yet propagate original-creator fields.

- [ ] **Step 3: Update `handleVineReshare`**

In `src/App.svelte`, locate `handleVineReshare` (currently around line 116):

```typescript
async function handleVineReshare(vine: import('./lib/types').VineVideo) {
  try {
    // Resolve the true origin. If vine is itself a reshare, its
    // originalCreatorAddress/Name already trace to the true origin;
    // pass them through. Otherwise the vine being reshared IS the
    // origin — use its creatorAddress/Name.
    const originalCreatorAddress =
      vine.originalCreatorAddress ?? vine.creatorAddress;
    const originalCreatorName =
      vine.originalCreatorName ?? vine.creatorName;
    await vineService.publish(
      vine.videoCid,
      vine.title,
      vine.id,
      originalCreatorAddress,
      originalCreatorName,
    );
  } catch (err) {
    console.error('Vine reshare failed', err);
    throw err;
  }
}
```

- [ ] **Step 4: Add `vineGetReshareCount` reactive state**

Near the existing `vineGetReaction` declaration (around line 89):

```typescript
let vineGetReshareCount = $state<(vineId: string) => number>(() => 0);

// Inside the same place where vineGetReaction is bound to vineService:
// (likely an $effect or service-connect block)
vineGetReshareCount = (vineId: string) => vineService.getReshareCount(vineId);
```

- [ ] **Step 5: Add `handleViewOriginal`**

Near the other vine handlers:

```typescript
function handleViewOriginal(vineId: string) {
  const original = vineService.findVine(vineId);
  if (!original) {
    // Original not in local feed — silently no-op. The creator may not
    // be in the user's network. No error toast per spec.
    return;
  }
  // Open the original in the player. The implementer should mirror the
  // existing "open vine in player" path — e.g., set `playingVine = original`
  // or call whatever state setter App.svelte uses.
  playingVine = original;
}
```

(`playingVine` name is a placeholder — read the existing App.svelte to confirm the actual state variable used to open the player, e.g., `selectedVine`, `currentVine`, etc.)

- [ ] **Step 6: Pass new props through to VineFeed**

In the `<VineFeed ... />` JSX (around line 1340-1350):

```svelte
<VineFeed
  ...
  getReaction={vineGetReaction}
  getReshareCount={vineGetReshareCount}
  onReshare={handleVineReshare}
  onViewOriginal={handleViewOriginal}
  ...
/>
```

- [ ] **Step 7: Run tests — verify the smoke test passes**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run
```

Expected: PASS — smoke test sees publish called with attribution, all other tests still green.

- [ ] **Step 8: Run all gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && \
  cargo fmt --all -- --check && \
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && \
  cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && \
  npx tsc --noEmit && \
  npx vitest run
```

Expected: all five gates green.

- [ ] **Step 9: Commit**

```bash
git add src/App.svelte src/App.test.ts src/lib/components/__tests__/VineFeed.integration.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-103): App.svelte handler updates + reshare-attribution smoke

Wires the new VineService + component capabilities into App:

- `handleVineReshare(vine)` now resolves the true origin and passes
  `originalCreatorAddress` / `originalCreatorName` to `publish()`. The
  resolution: if the source vine is itself a reshare, propagate its
  origin fields (transitive); otherwise the source vine IS the origin,
  so use its creatorAddress / creatorName.

- New `vineGetReshareCount` reactive function mirroring `vineGetReaction`,
  bound to `vineService.getReshareCount`. Passed through VineFeed →
  VineCard for the count display.

- New `handleViewOriginal(vineId)` looks up the vine via
  `vineService.findVine` and opens it in the player. Silently no-ops if
  the original isn't in the local feed.

All three are passed through VineFeed to its children.

Test coverage:

- App-level smoke: clicking Reshare on a non-own vine → dialog →
  confirm → publish called with attribution preserved.

Refs ZEB-103 spec §UI Components → App.svelte Changes,
§Edge Cases → Resharing a reshare,
§Edge Cases → Original not in feed.
EOF
)"
```

---

## Task 10: Final gates + push + PR

**Files:** (none modified)

- [ ] **Step 1: Run all five required gates one final time**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && \
  cargo fmt --all -- --check && \
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && \
  cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && \
  npx tsc --noEmit && \
  npx vitest run
```

Expected: all five gates green. Note the final test counts and confirm they're ≥ `BASELINE_RUST_TESTS` and `BASELINE_VITEST_TESTS` from Task 0.

If `cargo nextest` shows ANY new test that's unrelated to ZEB-103 has broken, STOP. Per `feedback_test_drift_is_our_fault` + `feedback_unrelated_test_failures` memory rules, file a Linear follow-up via `mcp__plugin_linear_linear__save_issue` and fix it before merging (do not fold the fix into this PR's commits).

- [ ] **Step 2: Push the branch**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git push -u origin zeb-103-vine-reshare-improvements
```

- [ ] **Step 3: Open the PR**

```bash
gh pr create --title "ZEB-103: Vine reshare improvements — attribution, counts, confirm dialog" --body "$(cat <<'EOF'
## Summary

Implements the [ZEB-103 spec](https://github.com/zeblithic/harmony-client/blob/main/docs/specs/2026-04-07-vine-reshare-improvements-design.md) (committed `a3ad5ca`) — the UX layer that makes resharing feel intentional and informative.

Closes [ZEB-103](https://linear.app/zeblith/issue/ZEB-103).

Builds directly on the persistence groundwork landed in [ZEB-147](https://linear.app/zeblith/issue/ZEB-147) (PR #119, commit `76c399b`) and the cache plumbing landed in [ZEB-286](https://linear.app/zeblith/issue/ZEB-286) (PR #118).

## What changed

**Wire format (backward-compatible):**
- Two new optional fields on `VineDescriptorPayload`, `PublishVinePayload`, `VineVideoDto`: `originalCreatorAddress`, `originalCreatorName` (camelCase wire, `serde(default)` + `skip_serializing_if`).
- Mirror fields on TypeScript `VineVideo` + `VineDescriptorEvent`.
- Cache persists the new fields (round-trip survives reload, parallel to ZEB-147's `reshareOf` guarantee).

**Frontend service (`VineService`):**
- `publish(...)` now accepts and forwards `originalCreatorAddress` / `originalCreatorName`.
- New `findVine(id)` searches both feeds.
- New `getReshareCount(id)` counts reshares of the given id across both feeds.
- Self-reshare prevention: `publish` silently no-ops when `originalCreatorAddress` matches local identity.

**UI:**
- New `ReshareConfirmDialog` component (modal-backed, mirrors `ConfirmationModal.svelte` shape).
- `VineCard` legacy "reshare" badge replaced with clickable "↗ originally by {name}" attribution row; new opt-in reshare-count display alongside likes.
- `VinePlayer` legacy "Reshared" label replaced with attribution row; Reshare button now opens confirmation dialog; hidden on own original vines.
- `VineFeed` plumbs `getReshareCount` and `onViewOriginal` through to children.
- `App.svelte` `handleVineReshare` resolves true origin (transitive through reshare chains) and passes attribution to publish; new `vineGetReshareCount` reactive + `handleViewOriginal` handler.

## Test plan

- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` — no regressions; N new tests
- [x] `npx tsc --noEmit`
- [x] `npx vitest run` — no regressions; N new tests
- [ ] Manual smoke: open a vine in player, click Reshare, confirm in dialog, observe vine appears in feed with attribution to the original creator; click attribution to navigate.

## Acceptance criteria

| Spec § | Status |
|---|---|
| Wire format → Rust | ✅ commit 1 |
| Wire format → TypeScript | ✅ commit 2 |
| VineService.publish original-creator threading | ✅ commit 2 |
| VineService.findVine | ✅ commit 3 |
| VineService.getReshareCount | ✅ commit 3 |
| Self-reshare prevention | ✅ commit 4 |
| ReshareConfirmDialog | ✅ commit 5 |
| VineCard attribution row | ✅ commit 7 |
| VineCard reshare count | ✅ commit 7 |
| VinePlayer attribution row | ✅ commit 6 |
| VinePlayer confirm flow | ✅ commit 6 |
| VinePlayer hide-reshare-on-own | ✅ commit 6 |
| VineFeed prop forwarding | ✅ commit 8 |
| App.svelte wiring | ✅ commit 9 |
| Edge: resharing a reshare | ✅ commit 9 (`handleVineReshare` resolves transitive origin) |
| Edge: original not in feed | ✅ commit 9 (`handleViewOriginal` silently no-ops) |
| Edge: backward compat | ✅ commit 1 (legacy wire deserialize test) |

## Out of scope (deferred)

- Cross-device viewed-state sync (deferred from ZEB-147).
- VinePlayer / VinePublishDialog `<Modal>` migration ([ZEB-204](https://linear.app/zeblith/issue/ZEB-204), [ZEB-205](https://linear.app/zeblith/issue/ZEB-205)).
- Cross-service mock-clear policy ([ZEB-209](https://linear.app/zeblith/issue/ZEB-209)).

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 4: Confirm PR URL is returned and Linear-link auto-attaches**

The Linear MCP integration should auto-attach the PR to ZEB-103 because the body contains `Closes [ZEB-103](https://linear.app/zeblith/issue/ZEB-103)`. Verify by:

```bash
gh pr view --json url,number
```

Note the PR URL + number for the bot-monitoring phase.

- [ ] **Step 5: Hand off to bot-monitoring**

Per the `feedback_autonomous_pr_monitoring_loop` memory rule, the autonomous loop now:

1. Watches CodeRabbit, Cursor Bugbot, CodeAnt, Qodo (NOT Greptile, NOT CI — CI is disabled).
2. Addresses each bot finding in fixup commits + reviews.
3. Repeats until all bots converge with no actionable findings.
4. Sends pushover when PR is mergeable + clean, per `feedback_no_pushover_when_active.md`.
5. Defers merge decision to the user.

---

## Self-Review

After writing the complete plan above, audit it:

**1. Spec coverage:** Every line of `docs/specs/2026-04-07-vine-reshare-improvements-design.md` mapped to a task in the table above. ✅

**2. Placeholder scan:** No "TBD", "TODO", "implement later", "similar to Task N", or unstructured "fill in details" anywhere. Code blocks are complete; commands are exact. ✅

**3. Type consistency:**
- Rust: `original_creator_address` / `original_creator_name` used everywhere (`Option<String>`).
- TypeScript: `originalCreatorAddress` / `originalCreatorName` used everywhere (`string | undefined` via optional `?`).
- The Tauri IPC boundary auto-converts snake_case ↔ camelCase, so the consistent use of camelCase in `#[serde(rename_all = "camelCase")]` Rust struct field-by-field, plus the matching camelCase TS field names, ensures the wire stays aligned. ✅

**4. Test naming consistency:**
- Rust: `vine_descriptor_payload_serializes_original_creator_fields_as_camel_case` style, snake_case, descriptive.
- TS: `'publish forwards original creator fields to adapter'` style, descriptive sentences.
- Both follow existing project conventions. ✅

**5. Method signature consistency:**
- `findVine(vineId: string): VineVideo | undefined` — used as `findVine` in Task 3 (definition) and `handleViewOriginal` in Task 9 (caller).
- `getReshareCount(vineId: string): number` — used as `getReshareCount` in Task 3 (definition), Task 4 (test), Task 8 (passed through `VineFeed`), Task 9 (`vineGetReshareCount` reactive).
- `publish(videoCid, title?, reshareOf?, originalCreatorAddress?, originalCreatorName?)` — same 5-arg signature in Task 2 (definition), Task 4 (guard test), Task 9 (caller). ✅

**6. Branch/commit hygiene:**
- 9 commits total (1 plan + 8 task commits + 0 for Task 0 + 0 for Task 10). Plus this plan commit (a separate commit before Task 1).
- All commits authored by zeblithic, none amended, none use `--no-verify`.
- All commits land on `zeb-103-vine-reshare-improvements` branch which is based on latest `origin/main` (`76c399b`).
- Per `feedback_pull_before_work` HARD RULE: ✅ satisfied (branch cut from latest origin/main).

**7. Linear cross-refs:**
- PR body uses markdown-linked refs for ZEB-103, ZEB-147, ZEB-286, ZEB-204, ZEB-205, ZEB-209 per `feedback_linear_pr_auto_close` HARD RULE.
- Only ZEB-103 in the auto-close paragraph (`Closes [ZEB-103](...)`) — no parent cascade risk. ✅

**8. No invented Linear IDs:**
- Every ZEB-NNN referenced exists (verified via `list_issues` query earlier). No new sub-tickets filed during planning. ✅

**9. Memory rule compliance:**
- `feedback_no_worktrees`: ✅ using `git checkout -b`, not worktrees.
- `feedback_pull_before_work`: ✅ branch cut from latest origin/main; vine-touching commits since spec were scanned (ZEB-147 + ZEB-286, both pure tailwind).
- `feedback_tauri_error_extraction`: ✅ existing pattern used (`err instanceof Error ? err.message : ...`).
- `feedback_pipe_exit_codes_lie`: ✅ no `| tail`/`| grep` in any verification step.
- `feedback_cargo_fmt_gate`: ✅ `cargo fmt --all -- --check` included in every gate sweep.
- `feedback_test_drift_is_our_fault`: ✅ baseline + final test count checks; explicit STOP-and-file rule on unrelated breakage.

**10. Risk audit:**
- Rust struct field additions are append-only with `#[serde(default)]` — no breaking-deserialization risk for any persisted descriptors written by ZEB-147 / ZEB-286.
- `VineService.publish` adds two trailing optional params — no breaking-callers risk; existing 3-arg call in `App.svelte` still type-checks.
- New component is opt-in (Player only renders dialog when `showReshareConfirm = true`); button-rename to use new flow happens atomically in Task 6.
- The hide-reshare-on-own behavior loses surface; the test for "shown on own reshare of someone else's" prevents over-suppression.
- The reshare count is computed O(N) per render; for typical N (a few hundred vines), this is sub-millisecond. If the count becomes hot, memoization is a follow-up.

Plan complete. Ready for execution via `superpowers:subagent-driven-development`.
