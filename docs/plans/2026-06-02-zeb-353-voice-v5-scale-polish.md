# ZEB-353 Voice V5 — Scale + Polish Implementation Plan

> **For agentic workers:** Use superpowers:subagent-driven-development to implement task-by-task. Steps use `- [ ]` checkboxes.

**Goal:** Harden community voice channels + DM calls to the 64-participant target: a 64 soft-cap, transport-blip reconnect, mic-device-error surfacing (listen-only fallback), a DM Deafen control, leave-on-app-close, and an N-publisher scale test.

**Architecture:** Additive on the merged V1–V4 stack. Frontend `VoiceSession`/`CallSession` controllers gain `reconnecting`/`micBlocked` state + a soft-cap post-join check + leave-on-close; the event-loop media subscribers gain a retry-on-drop loop emitting a `voice-transport-lost` event; one new N-publisher presence integration test. Deafen + speaking-rings already exist for channels (V3) — only the DM bar Deafen button is missing.

**Tech stack:** Rust (Tauri/Zenoh event loop, tokio), Svelte 5 runes frontend, vitest + cargo-nextest.

**Branch:** `zeb-353-voice-v5-scale-polish` (off `origin/main` @ `3cf1b688`).

**Gate discipline (per task):** frontend → `npx tsc --noEmit` + scoped `npx vitest run <files>`; backend lib → `cargo fmt --all` + `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings` + scoped `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E '<filter>'`; integration-test tasks add `--test <name>`. Reserve full `--all-targets` for the final sweep (T8). Known 6 iroh/zenoh loopback flakes fail locally / pass on CI — never block on them.

---

### Task 0: Baseline

**Files:** none (verification only).

- [ ] **Step 1:** Confirm branch is `zeb-353-voice-v5-scale-polish` on `origin/main` lineage.
- [ ] **Step 2:** Frontend baseline green: `npx tsc --noEmit` (0) + `npx vitest run src/lib/voice-session.test.ts src/lib/call-session.test.ts src/lib/components/__tests__/VoiceChannelView.test.ts src/lib/components/__tests__/CallInProgressBar.test.ts` (all pass).
- [ ] **Step 3:** Backend lib baseline: `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings` (0) + `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(voice)'` (pass).
- [ ] **Step 4:** Record current test counts (voice-session 13, call-session 18, VoiceChannelView 11, CallInProgressBar 8) for later regression comparison.

---

### Task 1: DM Deafen control (`CallInProgressBar`)

Deafen is fully implemented in `CallSession.setDeafened` + `VoiceMixer.setDeafened` (mute all inbound, implies self-mute) — only the DM bar lacks a button. Channel `VoiceChannelView` already has one.

**Files:**
- Modify: `src/lib/components/CallInProgressBar.svelte`
- Test: `src/lib/components/__tests__/CallInProgressBar.test.ts`

- [ ] **Step 1 (test first):** Add a test: rendering with an active call, clicking the `data-testid="deafen"` button calls `session.setDeafened(true)`; `aria-pressed` reflects `$callState.deafened`; label toggles `🔈 Deafen` ↔ `🔕 Deafened`.
- [ ] **Step 2:** Add a `toggleDeafen` handler (`if (session && $callState) swallow(session.setDeafened(!$callState.deafened))`) mirroring `toggleMute`, and a Deafen button between PTT and End with `data-testid="deafen"`, `class:active={$callState?.deafened}`, `aria-pressed={$callState?.deafened}`, `aria-label={$callState?.deafened ? 'Undeafen' : 'Deafen'}`.
- [ ] **Step 3:** Gate (tsc + the 2 component test files). Commit `feat(zeb-353): Deafen control on the DM in-call bar`.

---

### Task 2: 64 soft-cap (best-effort post-join refusal)

**Design:** The roster is decentralized (a joiner only learns the count after subscribing to presence), so the cap is a **soft, best-effort** check on the joining client. After the backend join resolves, `VoiceSession` watches the first roster snapshot(s) for a short grace window; if the channel roster (excluding self) is `≥ 64`, it leaves and surfaces "voice channel full." Solo/first-joiner (empty roster) proceeds with no added latency. Document the best-effort nature (concurrent joiners may briefly exceed before bouncing).

**Files:**
- Modify: `src/lib/voice-session.ts` (add `VOICE_CHANNEL_SOFT_CAP = 64`; post-join roster check; `phase: 'idle'|'joining'|'connected'|'leaving'` unchanged — surface via thrown error)
- Modify: `src/lib/components/VoiceChannelView.svelte` (already catches + displays join errors — verify the "voice channel full" string renders in the `voice-error` alert)
- Test: `src/lib/voice-session.test.ts`

- [ ] **Step 1 (test):** Two tests: (a) join into a channel whose first roster has ≥64 non-self members → `join()` rejects with `/voice channel full/`, `leave_voice_channel` is invoked, phase returns to `idle`; (b) join into a roster with <64 (or empty) → connects normally (no false refusal).
- [ ] **Step 2:** In `_doJoin`, after the backend `join_voice_channel` IPC resolves, set `this.joinedAtMs = <now>`. In the presence-changed handler (`refreshRoster`/`onPresenceChanged`), when within a grace window (~3 s of `joinedAtMs`) and the non-self roster length `≥ VOICE_CHANNEL_SOFT_CAP`, call `this.leave()` and set a one-shot rejection path so the in-flight `join()` rejects with `"voice channel full"`. (Use a stored `pendingJoinReject` deferred resolved/rejected by the grace-window check or on successful connect.) Keep the common path latency-free.
- [ ] **Step 3:** Verify `VoiceChannelView` surfaces the error (the existing `try/catch` around `session.join` → `error` alert). No new UI needed beyond confirming the string shows.
- [ ] **Step 4:** Gate (tsc + voice-session + VoiceChannelView tests). Commit `feat(zeb-353): 64-participant soft cap on voice-channel join (best-effort)`.

**NOTE for implementer:** keep the deferred-reject design simple and fully torn down on both success and failure (no dangling timers/promises). This is the highest-design-risk task — call out any ambiguity before coding.

---

### Task 3: Transport-blip reconnect (backend retry + frontend `reconnecting` state)

Media subscribers (channel `event_loop.rs:~2715`, DM `~2971`) currently exit on `recv_async` error with only a warning — no retry. Mirror the voice-signal subscriber's retry/backoff loop, emit a `voice-transport-lost`/`voice-transport-restored` event, and add a `reconnecting` state + "Reconnecting…" UI.

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (channel + DM media subscriber tasks: wrap inner `while let Ok(sample)` in an outer retry loop that, on non-`closing` error, emits `voice-transport-lost {communityId?,channelId?|callId}`, re-declares the subscriber with backoff, and emits `voice-transport-restored` on success)
- Modify: `src/lib/voice-session.ts` + `src/lib/call-session.ts` (add `reconnecting: boolean` to state; listen for the two events; set/clear `reconnecting`)
- Modify: `src/lib/components/VoiceChannelView.svelte` + `CallInProgressBar.svelte` (show a "Reconnecting…" indicator when `reconnecting`)
- Test: `voice-session.test.ts`, `call-session.test.ts`, component tests

- [ ] **Step 1 (backend):** Refactor the channel media subscriber spawn to capture the needed key/ids and loop: on `Err`, if `!closing`, `tracing::warn!`, emit `voice-transport-lost`, sleep backoff (5s→cap, mirror signal sub), re-`declare_subscriber`, emit `voice-transport-restored`; on `closing`, break. Do the same for the DM media subscriber (emit with `callId`). Keep payloads camelCase.
- [ ] **Step 2 (backend gate):** `cargo clippy --lib` + `cargo nextest --lib -E 'test(voice)'`. Commit backend part.
- [ ] **Step 3 (frontend):** Add `reconnecting` to `VoiceSessionState`/`CallSessionState` (default false). Wire `listen('voice-transport-lost'/'voice-transport-restored')` filtered to the active channel/call → patch `reconnecting`. Render a subtle "Reconnecting…" badge in `VoiceChannelView` control bar + `CallInProgressBar` (next to the timer).
- [ ] **Step 4 (test):** session tests: a `voice-transport-lost` event for the active channel sets `reconnecting=true`; `-restored` clears it; events for a different channel/call are ignored. Component test: badge shows when `reconnecting`.
- [ ] **Step 5:** Gate (tsc + vitest). Commit `feat(zeb-353): reconnect media subscribers on transport drop + Reconnecting… UI`.

---

### Task 4: Mic-device-error surfacing + listen-only fallback

Mic permission/device errors currently propagate the raw browser string and roll back the entire join. V5: classify the error, and if it's a permission/device error, **join listen-only** (no sender) with a persistent "mic blocked — listening only" note instead of failing.

**Files:**
- Modify: `src/lib/voice-session.ts` (`_doJoin`: wrap `sender.start()`; on `NotAllowedError`/`NotFoundError`/`PermissionDenied*`, set `this.sender = null`, add `micBlocked: boolean` to state, continue with mixer+receiver only; re-throw on non-mic errors)
- Modify: `src/lib/components/VoiceChannelView.svelte` (persistent "Mic blocked — listening only" note when `$voiceState.micBlocked`, distinct from the transient join `error`)
- Test: `voice-session.test.ts`, `VoiceChannelView.test.ts`

- [ ] **Step 1 (test):** join where `sender.start()` rejects with `new DOMException('...', 'NotAllowedError')` (or an Error whose name/message matches) → session reaches `connected` with `micBlocked=true`, `sender` null, mixer+receiver still initialized, gate transmits nothing. A non-mic error (e.g. generic) still rolls back to idle + rejects (preserve existing transactional behavior).
- [ ] **Step 2:** Add a `classifyMicError(e): 'blocked'|'notfound'|null` helper (matches `NotAllowedError`/`PermissionDenied`, `NotFoundError`/`DevicesNotFound`). In `_doJoin`, isolate `sender.start()` in its own try; on a classified mic error set `micBlocked` + skip the sender (drain/receiver still run); otherwise propagate to the existing rollback.
- [ ] **Step 3:** Render the persistent note in `VoiceChannelView` when `micBlocked`.
- [ ] **Step 4:** Gate. Commit `feat(zeb-353): mic-error → listen-only join with "mic blocked" note`.

---

### Task 5: Leave-on-app-close

Nothing tears down an active voice channel/DM call on app/window close → stale roster (until 12s TTL) + lingering mic. Add Svelte unmount cleanup + a Tauri window-close handler that leaves/ends first.

**Files:**
- Modify: `src/App.svelte`
- Test: (App.svelte has no unit harness; rely on tsc + manual note in the PR test plan. Optionally a small unit test of a extracted `teardownVoiceOnClose()` helper if cleanly factorable.)

- [ ] **Step 1:** Add a Svelte `$effect(() => () => { void voiceSession?.leave().catch(() => {}); void callSession?.end().catch(() => {}); })` near the `voiceSession`/`callSession` state declarations, for SPA-unmount/hot-reload.
- [ ] **Step 2:** In the Tauri-init block, register `getCurrentWebviewWindow().onCloseRequested(async (e) => { e.preventDefault(); await voiceSession?.leave().catch(()=>{}); await callSession?.end().catch(()=>{}); await win.destroy(); })`, and register the returned unlisten via `fileManagerService.addUnlisten(...)`. Import `getCurrentWebviewWindow` from `@tauri-apps/api/webviewWindow`.
- [ ] **Step 3:** Gate (`tsc --noEmit` + full `vitest run` to confirm no App.svelte fallout). Commit `feat(zeb-353): leave voice / end call on app close`.

---

### Task 6: N-publisher scale validation test

Mirror `src-tauri/tests/voice_presence_two_engine_integration.rs`: spawn N (e.g. 64) presence publishers on one session + one subscriber session; assert all N roster entries converge; assert the roster sweep handles the N-load. Carry the iroh/zenoh loopback-flake disclaimer comment.

**Files:**
- Create: `src-tauri/tests/voice_presence_scale_integration.rs`

- [ ] **Step 1:** Copy the harness shape from `voice_presence_two_engine_integration.rs` (seeded registry, `spawn_voice_presence_publisher`/`subscriber`, `wait_until`). Seed N owner/device pairs into the registry.
- [ ] **Step 2:** Spawn N publishers (each own `SigningKey` + beacon), one subscriber; `wait_until` all N appear in the subscriber roster within a generous timeout; then assert TTL sweep evicts all N. Add the flake-disclaimer comment block (mirror `voice_dm_two_engine_integration.rs:7-14`).
- [ ] **Step 3:** Gate: `cargo nextest run --locked --features test-fixtures --test voice_presence_scale_integration` (note: may be loopback-flaky locally; must pass on CI). Commit `test(zeb-353): N-publisher presence scale integration test`.

---

### Task 7: Final sweep + push + PR

**Files:** none (verification + PR).

- [ ] **Step 1:** Full gate sweep from `src-tauri`: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --all-targets --features test-fixtures` (only the 6 known iroh/zenoh loopback flakes may fail locally — verify each failure is on that list). Frontend: `npx tsc --noEmit` + `npx vitest run`.
- [ ] **Step 2:** Push `git push -u origin zeb-353-voice-v5-scale-polish`.
- [ ] **Step 3:** Open PR (base `main`, title `ZEB-353 Voice V5: scale + polish (64 soft-cap, reconnect, mic-error, deafen, leave-on-close, scale test)`), body covering spec §V5, each deliverable + approach (esp. the best-effort soft-cap), new events (`voice-transport-lost/-restored`), test plan checklist, and the known-flake list.
- [ ] **Step 4:** Enter the autonomous bot-review loop (CodeRabbit/Cursor/CodeAnt/Qodo + 5 CI jobs; scan all three comment buckets; never Greptile). Converge to CI-green + no actionable findings. **Standard merge gate — do NOT self-merge V5; pushover Jake at ready-to-merge.**

---

## Self-review (spec §V5 coverage)

- 64 soft-cap → **T2** (+ error surfacing T2.3). ✅
- Scale validation → **T6**. ✅
- Speaking rings → already shipped in V3 (`VoiceChannelView` tiles/list); DM peer-speaking is optional/out-of-scope (1:1 has no roster). ✅ (noted, no task)
- Deafen → channel done in V3; DM bar button **T1**. ✅
- Reconnect on transport blips + "reconnecting…" → **T3**. ✅
- Persistent in-call bar across navigation → already mounted at App root (V4); no nav teardown exists → covered incidentally; leave-on-close is **T5**. ✅
- Mic-device-error surfacing (join muted, listen-only) → **T4**. ✅
- Leave-on-app-close → **T5**. ✅
- All gates green → **T7**. ✅

Ordering: T1 (smallest, warm-up) → T2 (soft-cap) → T3 (reconnect, backend+frontend) → T4 (mic-error) → T5 (leave-on-close) → T6 (scale test) → T7 (sweep/PR). T1–T6 are largely independent; T3 is the biggest.
