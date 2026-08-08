# Discoverability Default-ON + Redeem Copy — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a freshly-minted identity discoverable by default (ZEB-881), add a dismissible onboarding note that says so, and replace the misleading "no relays available" redeem error with an actionable message (ZEB-879a).

**Architecture:** Flip the `ConnectivitySettings` `Default` for `identity_discoverable` (propagates to both fresh-profile load and mint reset via `..Self::default()`); keep `fail_closed_defaults` OFF and never migrate existing persisted files. Add a dismissible callout to the one-time `WelcomeModal` mint gate, and a new variant to the `redeem-invite-errors` mapping table.

**Tech Stack:** Rust (Tauri backend, `cargo nextest`), Svelte 5 + TypeScript (Vitest).

## Global Constraints

- `fail_closed_defaults().identity_discoverable` MUST stay `false` (corrupt/unreadable settings never become discoverable).
- Do NOT migrate existing users: `load_or_default` returns a persisted file verbatim; only fresh profiles and mint-reset get the new ON default.
- Rust IPC params snake_case ↔ JS camelCase. Error extraction: `e instanceof Error ? e.message : String(e)`.
- Gates before PR (from CLAUDE.md, run from `src-tauri/` for cargo): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; `npx tsc --noEmit`; `npx vitest run` (from repo root).
- Branch: `zeblith/zeb-881-879-discoverability-default-on` (already created off origin/main).

---

### Task 1: Flip discoverability default to ON (ZEB-881 backend)

**Files:**
- Modify: `src-tauri/src/connectivity_settings.rs:80` (the `Default` impl)
- Modify: `src-tauri/src/connectivity_settings.rs` tests (`mod tests`)
- Modify: `src-tauri/src/lib.rs:10140-10159` (ZEB-794 OFF-branch log copy)

**Interfaces:**
- Consumes: `ConnectivitySettings::default()`, `::load_or_default(&PathBuf)`, `::fail_closed_defaults()`, `::reset_privacy_posture_for_new_identity(&PathBuf)`.
- Produces: no signature change — only the default *value* of `identity_discoverable` flips from `false` to `true`.

- [ ] **Step 1: Update the Rust tests to expect the ON default (write the failing expectations first)**

In `src-tauri/src/connectivity_settings.rs` `mod tests`, make these edits (flip the Default/mint-path assertions; LEAVE the fail-closed ones):

Rename + flip `defaults_to_not_discoverable`:
```rust
    #[test]
    fn defaults_to_discoverable() {
        // ZEB-881: fresh identities are discoverable by default so first
        // cross-WAN contact works; users opt into privacy, not out of usability.
        let settings = ConnectivitySettings::default();
        assert!(settings.identity_discoverable);
    }
```

In `missing_file_returns_default` (a genuinely-absent file is first-run → product Default):
```rust
        assert!(settings.identity_discoverable);
        assert!(settings.friend_auto_accept_known);
```

In `load_missing_file_returns_default`:
```rust
        let settings = ConnectivitySettings::load_or_default(&path);
        assert!(settings.identity_discoverable);
```

In the reset-on-mint test (the one asserting `"discoverable must reset OFF"`):
```rust
        // ZEB-881: mint resets to the product Default, which is now ON.
        assert!(after.identity_discoverable, "discoverable must reset ON");
```

In `reset_privacy_posture_on_corrupt_file_writes_clean_default` (reset writes product Default over the fail-closed load):
```rust
        assert!(after.identity_discoverable);
```

LEAVE unchanged (fail-closed paths must stay OFF): `parse_error_fails_closed_not_open`, `unreadable_file_fails_closed_not_first_run`, `load_corrupted_file_returns_default`. Add one explicit guard to `parse_error_fails_closed_not_open` right after its existing asserts to pin the invariant:
```rust
        // ZEB-881 guard: the ON default must NOT leak into the fail-closed path.
        assert!(!ConnectivitySettings::fail_closed_defaults().identity_discoverable);
```

- [ ] **Step 2: Run the tests to verify they FAIL**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(connectivity_settings)'`
Expected: FAIL — `defaults_to_discoverable`, `missing_file_returns_default`, `load_missing_file_returns_default`, the reset-on-mint test, and the corrupt-file reset test fail because the default is still `false`.

- [ ] **Step 3: Flip the default**

In `src-tauri/src/connectivity_settings.rs` `impl Default`, change line 80:
```rust
            identity_discoverable: true,
```

- [ ] **Step 4: Update the ZEB-794 OFF-branch boot log (no longer the default)**

In `src-tauri/src/lib.rs`, the `else` branch (~10140-10159), replace the message so it no longer calls OFF "the default" and reframes it as a user opt-out:
```rust
                        // ZEB-794 / ZEB-881: OFF is now an explicit opt-out, not
                        // the default. Still logged unconditionally so an operator
                        // reading a `serve` log can see the node is undiscoverable.
                        tracing::info!(
                            "ZEB-794: identity discoverability OFF (opted out) — case-B \
                             not published, so add_friend_by_key against this node \
                             returns `unreachable`. Enable with \
                             `connectivity_set_identity_discoverable {{\"enabled\": true}}`, \
                             or use the friend-token path, which does not need it."
                        );
```
(Leave the ON-branch message at ~10136 unchanged.)

- [ ] **Step 5: Run the tests to verify they PASS + no other test regressed**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(connectivity_settings)'`
Expected: PASS.
Then confirm nothing else workspace-wide depended on the old default:
Run: `grep -rnE "identity_discoverable" src-tauri --include='*.rs' | grep -iE "assert.*!.*discoverable|false"` — every remaining `!discoverable`/`false` assertion must be a fail-closed or explicit-persisted-file case. (Expected: only `parse_error_fails_closed_not_open`, `unreadable_file_fails_closed_not_first_run`, `load_corrupted_file_returns_default`, and the `fail_closed_defaults` guard.)

- [ ] **Step 6: fmt + clippy for the touched crate**

Run: `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/connectivity_settings.rs src-tauri/src/lib.rs
git commit -m "feat(connectivity): default identity discoverability ON (ZEB-881)"
```

---

### Task 2: Actionable redeem error for the relay warm-up case (ZEB-879a)

**Files:**
- Modify: `src/lib/redeem-invite-errors.ts` (add a `VARIANT_PATTERNS` entry before `FALLBACK`)
- Modify: `src/lib/__tests__/redeem-invite-errors.test.ts`

**Interfaces:**
- Consumes: `mapRedeemInviteError(raw: string): RedeemInviteUserError` (`{ summary, hint, tag, raw }`), the `VARIANT_PATTERNS` array, `FALLBACK`.
- Produces: a new variant with `tag: 'relays_warming_up'`.

- [ ] **Step 1: Write the failing test**

In `src/lib/__tests__/redeem-invite-errors.test.ts`, add:
```ts
  it('maps the pkarr relay warm-up error to an actionable, non-misleading message', () => {
    const r = mapRedeemInviteError('no relays available (all on cooldown or unreachable)');
    expect(r.tag).toBe('relays_warming_up');
    expect(r.summary).toBe('The network is still warming up.');
    expect(r.hint).toMatch(/try again/i);
    // must NOT fall through to the generic network-failure fallback
    expect(r.tag).not.toBe('network_failure');
  });
```

- [ ] **Step 2: Run the test to verify it FAILS**

Run: `npx vitest run src/lib/__tests__/redeem-invite-errors.test.ts`
Expected: FAIL — currently `mapRedeemInviteError` returns the `network_failure` FALLBACK for this string.

- [ ] **Step 3: Add the variant**

In `src/lib/redeem-invite-errors.ts`, add this entry to the `VARIANT_PATTERNS` array (place it just before the array closes, ahead of `FALLBACK`):
```ts
  {
    match: /no relays available/i,
    summary: 'The network is still warming up.',
    hint: 'Discovery relays warm up for about a minute after launch — try again shortly. If it keeps failing, the inviter may not be discoverable yet.',
    tag: 'relays_warming_up',
  },
```

- [ ] **Step 4: Run the test to verify it PASSES**

Run: `npx vitest run src/lib/__tests__/redeem-invite-errors.test.ts`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib/redeem-invite-errors.ts src/lib/__tests__/redeem-invite-errors.test.ts
git commit -m "fix(redeem): actionable copy for relay warm-up instead of raw 'no relays available' (ZEB-879)"
```

---

### Task 3: Onboarding privacy note in WelcomeModal (ZEB-881 Option A)

**Files:**
- Modify: `src/lib/components/WelcomeModal.svelte` (explain-stage content, ~lines 257-283)
- Modify: `src/lib/components/__tests__/WelcomeModal.test.ts`

**Interfaces:**
- Consumes: the existing `stage === 'explain'` render branch and Svelte 5 runes (`$state`).
- Produces: a dismissible note gated by a local `noteDismissed` `$state<boolean>` (no persistence — the mint gate is one-time, so cross-session persistence adds nothing).

- [ ] **Step 1: Write the failing test**

In `src/lib/components/__tests__/WelcomeModal.test.ts`, add a test that the note renders in the explain stage and can be dismissed. Match the file's existing render/setup helpers (render `WelcomeModal` with `open: true`); assert on a stable `data-testid`:
```ts
  it('shows a dismissible discoverability privacy note on the welcome stage', async () => {
    const { getByTestId, queryByTestId } = renderWelcome({ open: true });
    const note = getByTestId('welcome-discoverability-note');
    expect(note.textContent).toMatch(/discoverable/i);
    expect(note.textContent).toMatch(/Settings/i);
    await fireEvent.click(getByTestId('welcome-discoverability-note-dismiss'));
    expect(queryByTestId('welcome-discoverability-note')).toBeNull();
  });
```
(If the test file's existing helper is named differently than `renderWelcome`, use that helper and `fireEvent`/`screen` as the file already imports them.)

- [ ] **Step 2: Run the test to verify it FAILS**

Run: `npx vitest run src/lib/components/__tests__/WelcomeModal.test.ts`
Expected: FAIL — no `welcome-discoverability-note` element exists yet.

- [ ] **Step 3: Add the dismiss state + note markup**

In `WelcomeModal.svelte` `<script>`, add near the other `$state` declarations:
```ts
  // ZEB-881: one-time reassurance that the new identity is discoverable, with a
  // pointer to go private. Local-only dismiss — the mint gate itself is one-time.
  let noteDismissed = $state(false);
```
In the explain-stage content (inside `{#if stage === 'explain' || stage === 'minting'}`, after the existing explanatory `<p>` blocks around line 274), add:
```svelte
        {#if !noteDismissed}
          <div class="discoverability-note" data-testid="welcome-discoverability-note" role="note">
            <p>
              You’ll be <strong>discoverable</strong>, so people can reach you with an invite.
              You can go private anytime in <strong>Settings → Network</strong>.
            </p>
            <button
              type="button"
              class="note-dismiss"
              data-testid="welcome-discoverability-note-dismiss"
              aria-label="Dismiss discoverability note"
              onclick={() => (noteDismissed = true)}
            >Got it</button>
          </div>
        {/if}
```
Add minimal styling in the component `<style>` (mirror the existing `.muted`/callout look — a subtle bordered box):
```css
  .discoverability-note {
    margin-top: 0.75rem;
    padding: 0.6rem 0.75rem;
    border: 1px solid var(--border-subtle, rgba(128, 128, 128, 0.3));
    border-radius: 6px;
    font-size: 0.9em;
  }
  .discoverability-note .note-dismiss {
    margin-top: 0.4rem;
  }
```
(If the component already defines a `--border-subtle` or callout class, prefer it over the fallback.)

- [ ] **Step 4: Run the test to verify it PASSES**

Run: `npx vitest run src/lib/components/__tests__/WelcomeModal.test.ts`
Expected: PASS.

- [ ] **Step 5: tsc + commit**

Run: `npx tsc --noEmit`
Expected: clean.
```bash
git add src/lib/components/WelcomeModal.svelte src/lib/components/__tests__/WelcomeModal.test.ts
git commit -m "feat(onboarding): dismissible discoverability privacy note in WelcomeModal (ZEB-881)"
```

---

### Task 4: Full CI-parity gate + open PR

**Files:** none (verification + PR).

- [ ] **Step 1: Run the full gate to completion (no early exit)**

From `src-tauri/`: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
From repo root: `npx tsc --noEmit`, `npx vitest run`.
Expected: all green. Confirm `git status` is clean (no uncommitted changes) before declaring green.

- [ ] **Step 2: Push the branch**

```bash
git push -u origin zeblith/zeb-881-879-discoverability-default-on
```

- [ ] **Step 3: Open the PR** (`gh pr create --repo zeblithic/harmony-client`), body linking ZEB-881 + ZEB-879, noting: default ON (fresh identities only; fail-closed + existing users unchanged), onboarding note, redeem copy; and that ZEB-879b (runtime enable→republish stall) is deferred.

- [ ] **Step 4: Trigger one CodeRabbit review** (`@coderabbitai review`) once, then converge on CI + bot findings without auto-merging.

## Self-Review

- **Spec coverage:** A (default flip) → Task 1; B (onboarding note) → Task 3; C (redeem copy) → Task 2; testing + gate → each task + Task 4. Deferred 879b explicitly out of scope. ✓
- **Placeholders:** none — every step has concrete code/paths.
- **Type consistency:** `tag: 'relays_warming_up'` used in both the test and the variant; `noteDismissed`/`welcome-discoverability-note(-dismiss)` testids consistent across test and markup.
- **Deviation from spec:** the note uses a local (non-persisted) dismiss instead of an owner-scoped localStorage flag, because the WelcomeModal mint gate is inherently one-time and no ownerId exists pre-mint. Flagged in the PR body.
