# Profile Editor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a profile editor panel where users can edit their display name, status text, and see their avatar. Profile persists to localStorage.

**Architecture:** New `ProfileEditor.svelte` component shown inside the existing settings panel area. A `profile-service.ts` manages the local profile state and localStorage persistence. The existing `Profile` type is reused.

**Tech Stack:** Svelte 5 (runes), TypeScript, vitest, localStorage

---

## File Map

| File | Responsibility |
|------|---------------|
| `src/lib/profile-service.ts` | Load/save local profile to localStorage |
| `src/lib/profile-service.test.ts` | Tests for persistence |
| `src/lib/components/ProfileEditor.svelte` | Edit form: display name, status text, avatar preview |
| `src/lib/components/__tests__/ProfileEditor.test.ts` | Component tests |
| `src/App.svelte` | Wire profile editor into settings area |

---

### Task 1: Profile Service

- [ ] **Step 1: Create profile-service.ts** — `loadProfile()`, `saveProfile()`, default profile with address `"local"`. Uses localStorage key `"harmony-profile"`.
- [ ] **Step 2: Create tests** — roundtrip, defaults, update fields
- [ ] **Step 3: Verify and commit**

### Task 2: ProfileEditor Component

- [ ] **Step 1: Create ProfileEditor.svelte** — form with display name input, status text input, avatar preview (uses existing `Avatar` + `Identicon`), save button. Calls `onSave(profile)`.
- [ ] **Step 2: Create component tests** — renders inputs, shows avatar, calls onSave
- [ ] **Step 3: Verify and commit**

### Task 3: Wire into App

- [ ] **Step 1: Add profile state + editor to App.svelte** — load profile on mount, show ProfileEditor in the settings panel area (as a section above notification settings)
- [ ] **Step 2: Verify all tests and build**
- [ ] **Step 3: Commit**
