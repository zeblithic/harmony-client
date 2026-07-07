# ZEB-660 Commons H: Valence Pass Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Apply the three "clay vs red" valence calls from the ZEB-657 design session — restrictive-engaged call controls read clay, an untrusted-media card reads as a clay attention-card, and a mic-denied hard blocker reads red.

**Architecture:** Three independent, single-concern CSS/markup restyles across five `.svelte` files, landing as one cohesive "valence pass" PR (one commit). #2 (call bars) adds a `.ctrl.restrictive` clay state and re-points the deafen/mute class bindings; #5 and #10 are token swaps. No new tokens, no logic/behavior change.

**Tech Stack:** Svelte 5 runes, CSS custom-property token layer (`src/app.css`), Vitest + @testing-library/svelte, `style-token-guard` budget test.

## Global Constraints

- **Source of decisions:** `docs/design/commons/h-deferred-decisions.md` §3 (#2, #5, #10). The governing rule: **clay** = a state the user *chose* and the feature still *works*; **red** = an unchosen failure/blocker.
- **Budget-0 color tokens.** No new hex/rgb/hsl/named-color literals. Every color is a `var(--*)` **already defined** in `src/app.css`. `src/style-token-allowlist.json` must stay **byte-identical** (the guard ratchets down only).
- **Tokens used — all verified present in both `:root` (light) and warm-dark blocks:** `--gov-clay`, `--surface-raised`, `--border`, `--shadow-e1`, `--danger-text-muted`, `--text-bright`, `--text-primary`, `--accent`.
- **No behavior/markup-contract change.** All `data-testid`, `aria-label`, `aria-pressed`, and click handlers are preserved exactly. The existing call/flashcard/untrusted-media test suites must stay green (they assert behavior + testids, not `.active` styling).
- **Gates (from repo root):** `npx tsc --noEmit && npx vitest run` clean; `style-token-guard` green; `git diff --stat src/style-token-allowlist.json` empty.
- **No CSS-value tests.** jsdom does not compute `<style>`-block values, so a `border-color: var(--gov-clay)` assertion tests nothing real. Verification = existing suite green + tsc + token-guard + allowlist byte-identical.

**Scope guard:** touch *only* the properties named per task. Do **not** opportunistically fix adjacent radii (e.g. UntrustedMediaCard's 4px buttons) — those are outside the three recorded decisions and stay as-is.

---

### Task 1: Call-control valence — clay for restrictive-engaged (#2)

**Files:**
- Modify: `src/lib/components/CallInProgressBar.svelte` (mute + deafen `class:` bindings ~L75/L94; `<style>` `.ctrl.active` ~L165)
- Modify: `src/lib/components/GroupCallBar.svelte` (mute + deafen `class:` bindings ~L126/L145; `<style>` `.ctrl.active` ~L277)
- Modify: `src/lib/components/VoiceChannelView.svelte` (mute + deafen `class:` bindings ~L267/L289; `<style>` `.ctrl.active` ~L511)
- Verify (no edit): the three `__tests__/{CallInProgressBar,GroupCallBar,VoiceChannelView}.test.ts` stay green.

**Model (identical across all three files):** the mute button already lights `.ctrl.active` (sage) only when *live* — correct, keep it — and additionally reads clay when *muted*. The deafen button currently lights `.ctrl.active` (sage) when *deafened* — the bug — and must instead read clay. PTT-hold / PTT-mode `.active` bindings are untouched (talk-engaged stays sage).

- **Mute button:** keep `class:active={…!muted…}` and **add** `class:restrictive={…muted…}`.
- **Deafen button:** change `class:active={…deafened…}` → `class:restrictive={…deafened…}`.
- **Add** a `.ctrl.restrictive` rule mirroring that file's `.ctrl.active`, swapping `--accent` → `--gov-clay` (keep the same `color:` the file's `.active` uses).

- [ ] **Step 1: Baseline the gate**

Run (from repo root):
```bash
npx vitest run src/lib/components/__tests__/CallInProgressBar.test.ts src/lib/components/__tests__/GroupCallBar.test.ts src/lib/components/__tests__/VoiceChannelView.test.ts
```
Expected: PASS (green baseline to preserve).

- [ ] **Step 2: CallInProgressBar — re-point bindings**

Mute button (~L75): `class:active={!$callState?.muted}` → add the restrictive binding on the same element:
```
        class:active={!$callState?.muted}
        class:restrictive={$callState?.muted}
```
Deafen button (~L94): `class:active={$callState?.deafened}` → `class:restrictive={$callState?.deafened}`.

- [ ] **Step 3: CallInProgressBar — add `.ctrl.restrictive`**

Its `.ctrl.active` is `{ background: var(--accent); border-color: var(--accent); color: var(--text-bright); }`. Add immediately after it:
```css
  .ctrl.restrictive {
    background: var(--gov-clay);
    border-color: var(--gov-clay);
    color: var(--text-bright);
  }
```

- [ ] **Step 4: GroupCallBar — re-point bindings**

Mute button (~L126): `class:active={!$callState.muted}` → add on the same element:
```
        class:active={!$callState.muted}
        class:restrictive={$callState.muted}
```
Deafen button (~L145): `class:active={$callState.deafened}` → `class:restrictive={$callState.deafened}`.

- [ ] **Step 5: GroupCallBar — add `.ctrl.restrictive`**

Read the file's exact `.ctrl.active` block (~L277) and add a `.ctrl.restrictive` mirroring it with `--accent` → `--gov-clay` (preserve whatever `color:` the `.active` rule declares; if it declares none, declare none).

- [ ] **Step 6: VoiceChannelView — re-point bindings**

Mute button (~L267): `class:active={!$voiceState.muted}` → add on the same element:
```
          class:active={!$voiceState.muted}
          class:restrictive={$voiceState.muted}
```
Deafen button (~L289): `class:active={$voiceState.deafened}` → `class:restrictive={$voiceState.deafened}`.

- [ ] **Step 7: VoiceChannelView — add `.ctrl.restrictive`**

Its `.ctrl.active` is `{ background: var(--accent); border-color: var(--accent); color: var(--text-primary); }`. Add immediately after it:
```css
  .ctrl.restrictive {
    background: var(--gov-clay);
    border-color: var(--gov-clay);
    color: var(--text-primary);
  }
```

- [ ] **Step 8: Re-run the three call test files**

```bash
npx vitest run src/lib/components/__tests__/CallInProgressBar.test.ts src/lib/components/__tests__/GroupCallBar.test.ts src/lib/components/__tests__/VoiceChannelView.test.ts
```
Expected: PASS (behavior/testids unchanged; only styling class names moved).

---

### Task 2: UntrustedMediaCard — clay attention-card + clay confirm (#5)

**Files:**
- Modify: `src/lib/components/UntrustedMediaCard.svelte` (`.untrusted-card` ~L95; `.action-btn.confirming:not(:disabled)` ~L185)
- Verify (no edit): `src/lib/components/__tests__/UntrustedMediaCard*.test.ts` (if present) stay green.

- [ ] **Step 1: Reshape `.untrusted-card` to the attention-card recipe**

Replace (~L95–100):
```css
  .untrusted-card {
    background: var(--bg-tertiary);
    border-radius: 8px;
    overflow: hidden;
    scroll-margin-top: 12px;
  }
```
with:
```css
  .untrusted-card {
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-left: 3px solid var(--gov-clay);
    border-radius: 8px;
    box-shadow: var(--shadow-e1);
    overflow: hidden;
    scroll-margin-top: 12px;
  }
```

- [ ] **Step 2: Recolor the enabled "Confirm load" button to clay**

The reveal action is `.action-btn.confirming:not(:disabled)` (~L185–188), currently sage:
```css
  .action-btn.confirming:not(:disabled) {
    border-color: var(--accent);
    color: var(--accent);
  }
```
Change to clay (leave the `.action-btn` base + the disabled cooldown state untouched):
```css
  .action-btn.confirming:not(:disabled) {
    border-color: var(--gov-clay);
    color: var(--gov-clay);
  }
```

- [ ] **Step 3: Verify (no edit) the "Show" button stays neutral**

Confirm `.action-btn` (the first-step "Show" button, no `.confirming`) is unchanged — it stays the neutral `var(--bg-secondary)` button; only the reveal-confirm goes clay.

---

### Task 3: FlashcardView mic-denied → red (#10)

**Files:**
- Modify: `src/lib/components/FlashcardView.svelte` (`.ptt-hint.error` ~L662)

- [ ] **Step 1: Promote the mic-denied hint from clay to red**

Replace (~L662–664):
```css
  .ptt-hint.error {
    color: var(--text-warning);
```
so the color line reads:
```css
  .ptt-hint.error {
    color: var(--danger-text-muted);
```
(Change only the `color` value; leave any other declarations in the rule intact. The `role="alert"` on the markup is preserved.)

---

### Task 4: Full gate + single commit

**Files:** none (verification + commit only).

- [ ] **Step 1: Confirm the allowlist is byte-identical**

```bash
git diff --stat src/style-token-allowlist.json
```
Expected: **no output**. If it changed, a color literal slipped in — revert and use an existing `var(--*)`.

- [ ] **Step 2: Full gate**

```bash
npx tsc --noEmit && npx vitest run
```
Expected: tsc clean; full suite PASS (272 files / 3245+ tests).

- [ ] **Step 3: Commit (one commit for the cohesive valence pass)**

```bash
git add src/lib/components/CallInProgressBar.svelte src/lib/components/GroupCallBar.svelte \
        src/lib/components/VoiceChannelView.svelte src/lib/components/UntrustedMediaCard.svelte \
        src/lib/components/FlashcardView.svelte docs/plans/2026-07-07-zeb-660-valence-pass-plan.md
git commit   # message: ZEB-660: Commons H valence pass — clay for restrictive call controls, clay untrusted-media card, red mic-denied
```

---

## Self-Review

**1. Spec coverage:** All three decision-doc items covered — #2 call-bar valence (Task 1, all 3 files), #5 UntrustedMediaCard attention-card + clay confirm (Task 2), #10 FlashcardView mic-denied → red (Task 3). ✅

**2. Placeholder scan:** No TBD/TODO; every code step shows the exact old→new CSS. The one deferred specific (GroupCallBar's `.ctrl.active` `color:` line) is explicitly "mirror the file's rule," resolved by reading that block at edit time — not a placeholder value. ✅

**3. Type/name consistency:** The new class is `restrictive` in every binding and every `.ctrl.restrictive` rule across all three call files. Store access matches each file (`$callState?.` in CallInProgressBar, `$callState.` in GroupCallBar, `$voiceState.` in VoiceChannelView). Tokens (`--gov-clay`, `--surface-raised`, `--border`, `--shadow-e1`, `--danger-text-muted`) match the Global Constraints list. ✅
