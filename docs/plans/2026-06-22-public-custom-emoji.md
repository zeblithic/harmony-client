# Public Custom Emoji Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make custom reaction emoji public by default (unencrypted, `hash(plaintext)` CID, deduplicated and freely served) while preserving an opt-in encrypted mode for sensitive emoji.

**Architecture:** The ingest, render/fetch, and serve layers *already* support both public and encrypted CAS artifacts (built for ZEB-539 attachments): `ingest_channel_artifact_bytes_impl` takes `encrypt: bool` with a full public branch, `authorize_and_fetch_artifact` already branches on `cid.flags().encrypted` and only decrypts encrypted CIDs while keeping channel-scoping via `find_attachment`, and the serve gate already serves unencrypted CIDs with no allowlist. Custom *emoji* are the only thing forced encrypted — by one guard at the mint boundary (`set_message_reaction_impl`) and one at receipt (`verify_channel_event`). This plan removes those two guards, flips the frontend emoji default to public, surfaces a per-upload "keep private" checkbox, and inverts the two tests that pinned the old encrypted-only invariant.

**Tech Stack:** Rust (Tauri backend, `cargo nextest`), Svelte 5 (frontend, `vitest`), content-addressed storage (`harmony_content::cid`).

**Branch:** `public-custom-emoji` (already created off `origin/main` @ `40cc9446`). Keep ZEB IDs out of branch and commit names.

**Spec:** `docs/specs/2026-06-22-public-custom-emoji-design.md`

---

## Scope note (read first)

The spec listed "five backend seams," but code inspection shows three of them are **already implemented** for the generic artifact path and need **no change**:

- **Ingest** — `ingest_channel_artifact_bytes_impl(..., encrypt: bool)` already has a public branch (`lib.rs:20751-20814`). Emoji just need to pass `encrypt=false`.
- **Render/fetch** — `authorize_and_fetch_artifact` already does `let encrypted = content_id.flags().encrypted` (`lib.rs:20974`), only fetches the epoch key + decrypts when encrypted, and keeps channel-scoping via `find_attachment(&cid_bytes, scope)` (`lib.rs:21008`). `decrypt_and_verify_artifact` passes public bytes through undecrypted (`lib.rs:20876-20883`).
- **Serve** — the gate serves unencrypted CIDs with no allowlist (documented at `lib.rs:21091-21092`).

So the real work is: **Task 1** (relax verify), **Task 2** (relax mint), **Task 3** (frontend default), **Task 4** (frontend checkbox), **Task 5** (dedup proof). Tasks 1–2 each invert one ZEB-541 test (intentional behavior change, not drift).

## Files touched

- `src-tauri/src/community_channel_log.rs` — remove the verify encrypted-gate + its now-unused error variant; invert one test; fix one comment. (Task 1)
- `src-tauri/src/lib.rs` — remove the mint encrypted-gate; invert one test; add a dedup test. (Tasks 2, 5)
- `src/lib/channel-message-service.ts` — `ingestEmojiBytes` gains an `encrypted` param defaulting to public. (Task 3)
- `src/lib/__tests__/channel-message-service.test.ts` — update + add `ingestEmojiBytes` tests. (Task 3)
- `src/lib/components/ChannelMessageFeed.svelte` — "keep private" checkbox in the reaction picker; thread to `ingestEmojiBytes`. (Task 4)
- `src/lib/components/__tests__/ChannelMessageFeed.test.ts` — assert default public + add private-path test. (Task 4)

## Gates (run before each commit that touches the relevant side)

- Rust: `cd src-tauri && cargo fmt --all -- --check`
- Rust: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- Rust: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- Frontend: `npx tsc --noEmit` (from repo root)
- Frontend: `npx vitest run` (from repo root — FULL suite; a shared component is edited)

---

### Task 1: Relax verify — accept public custom emoji

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs` (verify block ~1224-1233; error variant ~509-510; comment ~4501-4502)
- Test: `src-tauri/src/community_channel_log.rs` (invert `verify_react_rejects_unencrypted_custom_emoji_cid` ~4545-4572)

- [ ] **Step 1: Invert the verify test to expect acceptance**

Replace the whole `verify_react_rejects_unencrypted_custom_emoji_cid` test (currently at ~4545-4572) with:

```rust
    #[tokio::test]
    async fn verify_react_accepts_public_custom_emoji_cid() {
        // Public custom emoji (foundation): `[0x42; 32]` decodes to mode nibble
        // 0x4 (encrypted bit CLEAR) → a PUBLIC CID. Custom emoji default to
        // public (deduplicated, freely served), so verify must ACCEPT it; the
        // descriptor passes the field/size/image checks.
        let state = fixture_state_with_alice_joined();
        let (signing_key, author, _pub64) = fixture_identity(0xa1);
        let community_id = fixture_community(0xc0);
        let channel_id = fixture_channel(0x01);
        let att = ChannelAttachment {
            cid: [0x42; 32],
            mime: "image/png".to_string(),
            name: String::new(),
            size: 1024,
        };
        let payload = custom_emoji_react_payload(community_id, channel_id, author, Some(att));
        let event = sign_channel_react(&payload, &signing_key).expect("sign react");
        let mut tracker = ChannelLogReplayTracker::new();
        verify_channel_event(&event, &community_id, &channel_id, &state, &mut tracker)
            .await
            .expect("a public custom-emoji react must verify");
    }
```

- [ ] **Step 2: Run it to verify it FAILS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(verify_react_accepts_public_custom_emoji_cid)'`
Expected: FAIL — verify currently returns `CustomEmojiNotEncrypted` for `[0x42; 32]`.

- [ ] **Step 3: Remove the verify encrypted-gate**

In `verify_channel_event`, delete the block (currently ~1224-1233):

```rust
            // A custom emoji is a channel-private (encrypted) CAS blob; the
            // serve/preview gate keys off the CID's encrypted flag. Reject a
            // public CID so an emoji image can't be made world-fetchable
            // (parity with the mint-boundary check). CodeRabbit PR #320.
            if !harmony_content::cid::ContentId::from_bytes(att.cid)
                .flags()
                .encrypted
            {
                return Err(ChannelEventError::CustomEmojiNotEncrypted);
            }
```

The preceding checks in the same `if let Some(att) = emoji_attachment { … }` block (unicode-exclusion, `AttachmentFieldTooLong`, `CustomEmojiTooLarge`, `CustomEmojiNotImage`) stay.

- [ ] **Step 4: Remove the now-unused error variant**

First confirm it has no other references:

Run: `cd src-tauri && grep -rn "CustomEmojiNotEncrypted" src/`
Expected after Step 3: only the enum definition (~509-510) remains.

Then delete the variant from `ChannelEventError` (currently ~509-510):

```rust
    #[error("custom emoji cid must reference an encrypted CAS blob")]
    CustomEmojiNotEncrypted,
```

- [ ] **Step 5: Fix the stale comment in `verify_react_accepts_valid_custom_emoji`**

In `verify_react_accepts_valid_custom_emoji` (~4496), the comment on `[0xB2; 32]` says it "passes the encrypted-CID invariant below." That invariant is gone. Replace the comment (currently ~4501-4502):

```rust
        // `[0xB2; 32]` decodes to mode nibble 0xB (0b1011) → the encrypted flag
        // bit is set, so this passes the encrypted-CID invariant below.
```

with:

```rust
        // `[0xB2; 32]` decodes to mode nibble 0xB (encrypted bit set) — an
        // ENCRYPTED custom emoji. Both encrypted and public custom emoji verify;
        // this case keeps the encrypted path covered.
```

- [ ] **Step 6: Run the verify tests to confirm they PASS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(verify_react)'`
Expected: PASS (including `verify_react_accepts_public_custom_emoji_cid` and `verify_react_accepts_valid_custom_emoji`).

- [ ] **Step 7: Rust gates + commit**

```bash
cd src-tauri && cargo fmt --all -- --check \
  && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
git add src/community_channel_log.rs
git commit -m "feat(emoji): accept public custom emoji at verify

Custom emoji default to public (deduplicated, freely served). Remove the
receipt-time encrypted-only gate and its now-unused error variant; invert the
test that pinned the old invariant.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Relax mint — accept public custom emoji

**Files:**
- Modify: `src-tauri/src/lib.rs` (mint guard in `set_message_reaction_impl` ~20462-20473)
- Test: `src-tauri/src/lib.rs` (invert `set_message_reaction_rejects_unencrypted_custom_emoji_cid` ~20360-20383 in the test module, currently at line ~23360)

- [ ] **Step 1: Invert the mint test to expect the public CID passes validation**

Replace the whole `set_message_reaction_rejects_unencrypted_custom_emoji_cid` test (currently ~23360-23383) with:

```rust
    #[tokio::test]
    async fn set_message_reaction_accepts_public_custom_emoji_cid() {
        // Public custom emoji (foundation): `VALID_CID_64` (`00...`) is a PUBLIC
        // CID (encrypted flag clear). Custom emoji default to public, so the mint
        // boundary must NOT reject it for being unencrypted. It now passes the
        // descriptor checks and falls through to the registry lookup (absent on a
        // bare NodeState), proving the encrypted gate is gone.
        let state = reaction_error_path_state();
        let err = set_message_reaction_impl(
            &state,
            VALID_HEX_32.to_string(),
            VALID_HEX_32.to_string(),
            VALID_HEX_32.to_string(),
            String::new(),
            true,
            Some(ReactionEmojiInput {
                cid: VALID_CID_64.to_string(),
                mime: "image/png".to_string(),
                size: 1024,
            }),
        )
        .await
        .expect_err("bare NodeState has no channel_log_registry");
        assert!(
            err.contains("channel_log_registry missing"),
            "public emoji must pass validation and reach the registry lookup, got: {err}"
        );
        assert!(
            !err.contains("encrypted CAS blob"),
            "no encrypted-gate rejection should fire, got: {err}"
        );
    }
```

- [ ] **Step 2: Run it to verify it FAILS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(set_message_reaction_accepts_public_custom_emoji_cid)'`
Expected: FAIL — mint currently returns `"custom emoji cid must reference an encrypted CAS blob"`.

- [ ] **Step 3: Remove the mint encrypted-gate**

In `set_message_reaction_impl`, delete the block (currently ~20462-20473):

```rust
            // A custom emoji is a channel-private (encrypted) CAS blob — the
            // serve/preview gate keys off the CID's encrypted flag. Reject a
            // public CID at the mint boundary so an emoji image can't be made
            // world-fetchable (verify enforces the same on receipt). CodeRabbit
            // PR #320. Checked last so a malformed mime/size surfaces its own
            // (more specific) error first.
            if !harmony_content::cid::ContentId::from_bytes(emoji_cid)
                .flags()
                .encrypted
            {
                return Err("custom emoji cid must reference an encrypted CAS blob".to_string());
            }
```

The preceding checks (cid hex length, mime length, `image/` prefix, `MAX_CUSTOM_EMOJI_BYTES`) and the following `Some(ChannelAttachment { … })` construction stay.

- [ ] **Step 4: Run it to verify it PASSES**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(set_message_reaction_accepts_public_custom_emoji_cid)'`
Expected: PASS.

- [ ] **Step 5: Rust gates + commit**

```bash
cd src-tauri && cargo fmt --all -- --check \
  && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
git add src/lib.rs
git commit -m "feat(emoji): accept public custom emoji at mint

Remove the mint-boundary encrypted-only gate so a public custom-emoji CID is a
valid reaction descriptor; invert the test that pinned the old invariant.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: Frontend — default emoji ingest to public

**Files:**
- Modify: `src/lib/channel-message-service.ts` (`ingestEmojiBytes` ~564-589)
- Test: `src/lib/__tests__/channel-message-service.test.ts` (`ingestEmojiBytes` test ~690-707)

- [ ] **Step 1: Update + add service tests**

Replace the existing `it('ingestEmojiBytes invokes ingest_channel_artifact_bytes with a number[] body + returns cid/size', …)` body's assertion so `encrypt` is `false`, and add a second test for the explicit private param. The existing test currently asserts `encrypt: true` (~702). Change that line to `encrypt: false`, then add immediately after that test:

```ts
  it('ingestEmojiBytes passes encrypt=true when caller opts into private', async () => {
    const bytes = new Uint8Array([1, 2, 3]);
    const dto = { cid: 'cd'.repeat(32), mime: 'image/png', name: '', size: 256, encrypted: true };
    (adapter.invoke as any).mockResolvedValue(dto);
    await service.ingestEmojiBytes(CID, bytes, true);
    expect(adapter.invoke).toHaveBeenCalledWith('ingest_channel_artifact_bytes', {
      communityId: CID,
      bytes: Array.from(bytes),
      name: '',
      mime: 'image/png',
      encrypt: true,
    });
  });
```

(`CID` is the test's existing community-id constant used by the adjacent emoji tests; reuse it verbatim.)

- [ ] **Step 2: Run frontend tests to verify the default test FAILS**

Run: `npx vitest run src/lib/__tests__/channel-message-service.test.ts`
Expected: FAIL — `ingestEmojiBytes` still sends `encrypt: true` by default.

- [ ] **Step 3: Flip the default in `ingestEmojiBytes`**

Replace the `ingestEmojiBytes` doc comment + signature + invoke (currently ~564-583):

```ts
  /**
   * Ingest already-normalized PNG bytes (from {@link normalizeEmoji}) into CAS
   * for use as a custom reaction emoji. Public by default — a custom emoji is
   * `hash(plaintext)`-addressed so the same image is one CID network-wide
   * (deduplicated and freely served, never expiring). Pass `encrypted = true` to
   * keep this emoji private to the community (access-controlled, but permanent
   * caveats of the encrypted path apply). Returns the minted CID (hex) +
   * plaintext size to pass to {@link reactToMessage} as the `customEmoji`
   * descriptor. The backend enforces a 256 KiB cap; an over-cap input rejects.
   */
  async ingestEmojiBytes(
    communityId: string,
    bytes: Uint8Array,
    encrypted: boolean = false,
  ): Promise<{ cid: string; size: number }> {
    if (!this.adapter) throw new Error('ChannelMessageService.ingestEmojiBytes: adapter not connected');
    try {
      const dto = await this.adapter.invoke('ingest_channel_artifact_bytes', {
        communityId,
        bytes: Array.from(bytes),
        name: '',
        mime: 'image/png',
        encrypt: encrypted,
      }) as ChannelAttachmentDto;
```

(The `return { cid: dto.cid, size: dto.size };` and `catch` tail are unchanged.)

- [ ] **Step 4: Run frontend tests to verify they PASS**

Run: `npx vitest run src/lib/__tests__/channel-message-service.test.ts`
Expected: PASS (both the default-public and explicit-private tests).

- [ ] **Step 5: Type-check + commit**

```bash
npx tsc --noEmit
git add src/lib/channel-message-service.ts src/lib/__tests__/channel-message-service.test.ts
git commit -m "feat(emoji): default custom-emoji ingest to public

ingestEmojiBytes now defaults encrypt=false (public, deduplicated) and takes an
optional encrypted flag for the private path.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: Frontend — "keep private" checkbox in the reaction picker

**Files:**
- Modify: `src/lib/components/ChannelMessageFeed.svelte` (state ~461-463; `handleCustomEmojiPick` ~482-511; picker markup ~733-739)
- Test: `src/lib/components/__tests__/ChannelMessageFeed.test.ts` (happy-path emoji test ~1118-1123; add a private-path test after it)

- [ ] **Step 1: Add the default-public assertion + a private-path test**

In the existing custom-emoji happy-path test, tighten the `ingest_channel_artifact_bytes` assertion (currently `expect.objectContaining({ communityId: 'aa'.repeat(16), mime: 'image/png' })` ~1122) to include `encrypt: false`:

```ts
      expect(ctx.adapter.invoke).toHaveBeenCalledWith(
        'ingest_channel_artifact_bytes',
        expect.objectContaining({ communityId: 'aa'.repeat(16), mime: 'image/png', encrypt: false }),
      );
```

Then add a new test immediately after the happy-path test (`the picker custom button runs normalize → ingest → react`, ~1080). It mirrors that test's exact setup (`setup()`, the same `ingest_channel_artifact_bytes` mock, the same `channel-message-received` handler payload, the same `.picker-toggle` → `.picker-custom` flow) and inserts one step: checking the private checkbox **while the picker is open**, before clicking `.picker-custom` (which closes the picker). The `customEmojiPrivate` state persists after the picker closes, so `handleCustomEmojiPick` reads `true`:

```ts
  it('custom-emoji pick with "keep private" checked ingests encrypted', async () => {
    const ctx = await setup();
    (ctx.adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      if (cmd === 'ingest_channel_artifact_bytes') {
        return Promise.resolve({ cid: 'newcid', mime: 'image/png', name: '', size: 321, encrypted: true });
      }
      return Promise.resolve(undefined);
    });
    const handler = ctx.adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'ee'.repeat(20),
          at: { wallMs: 1000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('hi')),
        },
      },
    });
    await waitFor(() => expect(ctx.container.querySelector('.channel-message')).toBeTruthy());

    // Open the picker, check "keep private" WHILE it is open, THEN click the
    // custom (+) affordance (which closes the picker but leaves the flag set).
    await fireEvent.click(ctx.container.querySelector('.picker-toggle') as HTMLButtonElement);
    const priv = await waitFor(() => {
      const el = ctx.container.querySelector(
        '[aria-label="Keep custom emoji private to this community"]',
      );
      if (!el) throw new Error('private checkbox not rendered');
      return el as HTMLInputElement;
    });
    await fireEvent.click(priv);
    await fireEvent.click(ctx.container.querySelector('.picker-custom') as HTMLButtonElement);

    // Choose a file: set files on the hidden input and fire change.
    const input = ctx.container.querySelector('.custom-emoji-input') as HTMLInputElement;
    const file = new File([new Uint8Array([1, 2, 3])], 'pepe.png', { type: 'image/png' });
    Object.defineProperty(input, 'files', { value: [file], configurable: true });
    await fireEvent.change(input);

    await waitFor(() => {
      expect(ctx.adapter.invoke).toHaveBeenCalledWith(
        'ingest_channel_artifact_bytes',
        expect.objectContaining({ encrypt: true }),
      );
    });
  });
```

- [ ] **Step 2: Run the feed tests to verify the new test FAILS**

Run: `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts`
Expected: FAIL — there is no private checkbox yet, and the default ingest is not yet wired to `encrypt: false` from the component (the component calls `ingestEmojiBytes(commId, bytes)` with no third arg, which after Task 3 defaults to public, so the default assertion may already pass; the private-path test fails for lack of the checkbox).

- [ ] **Step 3: Add component state for the private toggle**

In `ChannelMessageFeed.svelte`, alongside the existing custom-emoji state (currently ~461-463):

```svelte
  let customEmojiInput: HTMLInputElement | undefined = $state();
  let customEmojiFor: string | null = null;
  let reactionError = $state<string | null>(null);
  // Foundation: custom emoji are PUBLIC by default (deduplicated, freely served).
  // This per-upload toggle opts a single emoji into the encrypted/private path.
  // Reset to public after each pick so the safe default never silently sticks.
  let customEmojiPrivate = $state(false);
```

- [ ] **Step 4: Thread the toggle through `handleCustomEmojiPick`**

In `handleCustomEmojiPick`, capture + reset the flag next to the existing `messageId` capture/reset, and pass it to `ingestEmojiBytes`. Change the capture/reset region (currently ~485-489):

```svelte
    const input = e.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    const messageId = customEmojiFor;
    // Capture the per-upload privacy choice, then reset to the public default.
    const makePrivate = customEmojiPrivate;
    customEmojiPrivate = false;
    // Reset the input + the target so a repeated pick of the same file re-fires
    // change, and a stale target can't leak into a later pick.
    input.value = '';
    customEmojiFor = null;
```

and the ingest call (currently ~500):

```svelte
      const { cid: emojiCid, size } = await channelMessageService.ingestEmojiBytes(commId, bytes, makePrivate);
```

- [ ] **Step 5: Add the checkbox to the reaction picker popover**

In the `reaction-picker` popover, after the custom-emoji `<button class="picker-custom">…</button>` (currently ends ~739), add:

```svelte
                <label class="picker-private" title="Public emoji can be cached and re-shared by anyone and can't be deleted later. Check this to keep this emoji private to the community.">
                  <input
                    type="checkbox"
                    bind:checked={customEmojiPrivate}
                    aria-label="Keep custom emoji private to this community"
                  />
                  <span>Keep private</span>
                </label>
                <span class="picker-private-hint">Public emoji can't be deleted later.</span>
```

Add minimal styling near the other picker styles (e.g. after `.reaction-picker { … }`), so the checkbox row wraps onto its own line and the hint is small/muted:

```css
  .picker-private {
    display: inline-flex;
    align-items: center;
    gap: 0.25rem;
    flex-basis: 100%;
    font-size: 0.75rem;
  }
  .picker-private-hint {
    flex-basis: 100%;
    font-size: 0.7rem;
    opacity: 0.6;
  }
```

(If `.reaction-picker` is not a flex/wrap container, add `flex-wrap: wrap;` to its rule so `flex-basis: 100%` forces the checkbox + hint onto their own rows below the emoji palette.)

- [ ] **Step 6: Run the FULL frontend suite to verify everything PASSES**

Run: `npx vitest run` (from repo root — full suite, because `ChannelMessageFeed` is rendered by other tests)
Expected: PASS.

- [ ] **Step 7: Type-check + commit**

```bash
npx tsc --noEmit
git add src/lib/components/ChannelMessageFeed.svelte src/lib/components/__tests__/ChannelMessageFeed.test.ts
git commit -m "feat(emoji): per-upload keep-private toggle in reaction picker

Custom emoji upload defaults to public; a checkbox in the reaction picker opts a
single emoji into the encrypted/private path, with a permanence warning.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 5: Dedup proof — public CID is content-deterministic across communities

**Files:**
- Test: `src-tauri/src/lib.rs` (add next to `ingest_channel_artifact_bytes_public_returns_dto` ~51370-51406)

- [ ] **Step 1: Add the dedup characterization test**

This locks the "superpower" claim from the spec: a public ingest is `hash(plaintext)`-addressed, so the same bytes ingested in two *different* communities produce the identical CID (one shared network-wide copy). It documents existing behavior, so it passes immediately — there is no production change to make.

Add after `ingest_channel_artifact_bytes_public_returns_dto`:

```rust
    /// Dedup proof: a PUBLIC ingest CIDs by hash(plaintext), so the same bytes
    /// ingested in two DIFFERENT communities yield the IDENTICAL CID — the
    /// network hosts one shared copy. (An encrypted ingest would differ per
    /// epoch key; public does not. This is the content-addressing superpower the
    /// public-emoji model unlocks.)
    #[tokio::test]
    async fn ingest_channel_artifact_bytes_public_is_deterministic_across_communities() {
        let (tx, _log) = spawn_test_ingest_handler();
        let state = StdMutex::new(NodeState {
            ingest_tx: Some(tx),
            ..NodeState::default()
        });
        let bytes: Vec<u8> = (0u8..200).collect();
        let a = ingest_channel_artifact_bytes_impl(
            &state,
            "00".repeat(16),
            bytes.clone(),
            String::new(),
            "image/png".into(),
            false,
        )
        .await
        .expect("public ingest a");
        let b = ingest_channel_artifact_bytes_impl(
            &state,
            "11".repeat(16),
            bytes.clone(),
            String::new(),
            "image/png".into(),
            false,
        )
        .await
        .expect("public ingest b");
        assert_eq!(a.cid, b.cid, "same plaintext → same public CID (cross-community dedup)");
        assert!(!a.encrypted && !b.encrypted, "both public");
        drop(state);
    }
```

- [ ] **Step 2: Run it to verify it PASSES**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(ingest_channel_artifact_bytes_public_is_deterministic_across_communities)'`
Expected: PASS (characterizes existing public-ingest behavior).

- [ ] **Step 3: Commit**

```bash
cd src-tauri && cargo fmt --all -- --check
git add src/lib.rs
git commit -m "test(emoji): pin public-ingest cross-community dedup

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Final verification (after all tasks)

- [ ] **Full Rust gate sweep:**

```bash
cd src-tauri && cargo fmt --all -- --check \
  && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
  && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: fmt clean, clippy 0 warnings, all tests pass.

- [ ] **Full frontend gate sweep (from repo root):**

```bash
npx tsc --noEmit && npx vitest run
```
Expected: type-check clean, all tests pass.

- [ ] **Manual smoke (optional, if a dev build is handy):** upload a custom emoji reaction (leave "keep private" unchecked) → it reacts and renders for peers; the minted CID is unencrypted (verify via the public flag). Check "keep private" on a second upload → it takes the encrypted path.

---

## Notes for the implementer

- **The two test inversions (Tasks 1 & 2) are intentional behavior changes**, not test drift. The old tests asserted the encrypted-only invariant we are deliberately removing.
- **Run `--all-targets` for clippy/nextest** — emoji guards live in code reached by integration tests; a `--lib`-only run would miss breakage (CLAUDE.md).
- **Run the FULL `vitest` suite** before pushing — `ChannelMessageFeed` is rendered by other component tests; a scoped run can miss cross-file drift (prior incident on the merged emoji PR).
- **Keep ZEB IDs out of branch and commit messages** (Linear auto-close cascade).
- After removing `CustomEmojiNotEncrypted`, confirm `grep -rn CustomEmojiNotEncrypted src/` returns nothing before the clippy run, or `-D warnings` will fail on a dangling reference.
- No new IPC, no protocol/wire change, no migration: existing encrypted emoji keep rendering via the unchanged encrypted branch.
- **Scope call on the spec's "two-engine integration test" (no silent cap):** the spec listed a two-engine test proving a public emoji reacted in community A is fetchable by a peer. This plan deliberately does **not** add a dedicated two-engine *emoji* test, because the render/fetch path (`authorize_and_fetch_artifact` → `decrypt_and_verify_artifact`) is byte-identical for public artifacts and is already exercised cross-engine by the existing public-*artifact* integration coverage; public emoji travel that same unchanged path. The dedup determinism (Task 5) plus the public accept-at-mint/verify tests (Tasks 1–2) cover what is new. If a reviewer wants belt-and-suspenders, extend the nearest existing two-engine emoji test (e.g. the `set_message_reaction_happy_path_custom_emoji_surfaces_cid` lineage) with a public CID — but it is not required for correctness here.
```
