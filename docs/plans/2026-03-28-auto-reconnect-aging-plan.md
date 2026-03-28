# Auto-Reconnect and Node Aging — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Auto-reconnect on unexpected Zenoh session loss, and age out discovered nodes not seen recently.

**Architecture:** ZenohService gains auto-reconnect (stores last endpoint, retries on error with exponential backoff) and node age filtering. Both are opt-in via constructor config.

**Tech Stack:** TypeScript, Svelte 5

---

## File Map

| File | Responsibility |
|------|---------------|
| `src/lib/zenoh-service.ts` | Auto-reconnect logic + lastEndpoint tracking |
| `src/lib/zenoh-service.test.ts` | Tests for reconnect + aging |
| `src/lib/zenoh-utils.ts` | `filterStaleNodes()` utility |
| `src/lib/zenoh-utils.test.ts` | Tests for stale filtering |
| `src/NetworkApp.svelte` | Wire aging into mergeNodes, pass config |

---

### Task 1: Node Aging

- [ ] **Step 1: Add `filterStaleNodes` to zenoh-utils.ts**
- [ ] **Step 2: Add tests for stale filtering**
- [ ] **Step 3: Wire into mergeNodes in NetworkApp.svelte**
- [ ] **Step 4: Verify and commit**

### Task 2: Auto-Reconnect

- [ ] **Step 1: Add lastEndpoint, reconnect timer, and exponential backoff to ZenohService**
- [ ] **Step 2: Add tests for reconnect behavior**
- [ ] **Step 3: Wire reconnect status into ConnectionBar**
- [ ] **Step 4: Verify and commit**
