# ZEB-344 — Receive-side avatar byte cap + decode guard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound the avatar receive path the same way ingest is bounded — a new `fetch_avatar` IPC caps the download via a byte limit threaded through `fetch_recursive`, and `AvatarResolver` adds a decoded-dimension guard with identicon fallback.

**Architecture:** `FetchRequest` gains an optional `max_bytes`; `fetch_recursive` aborts once the assembled bytes exceed it (download bounded, not just rejected post-buffer). A thin `fetch_avatar` IPC sets that cap to a shared `AVATAR_MAX_BYTES` (512KB); generic `fetch_content` stays unbounded. On the TS side `AvatarResolver` decodes via `createImageBitmap` + the existing `assertDecodedDimsOk` before building the blob URL.

**Tech Stack:** Rust + Tauri IPC; tokio mpsc/oneshot; TypeScript; vitest + jsdom.

**Spec:** `docs/specs/2026-06-08-zeb-344-avatar-receive-caps-design.md`

**Gate commands:**
- Rust fmt: `cd src-tauri && cargo fmt --all -- --check`
- Rust clippy: `cd src-tauri && cargo clippy --locked -p harmony-app --lib --bins --features test-fixtures --no-deps -- -D warnings`
- Rust test (scoped): `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(fetch_recursive) + test(fetch_avatar)'`
- Frontend: `npx vitest run src/lib/__tests__/avatar-resolver.test.ts` + `npx tsc --noEmit`

**Discipline:** commit BEFORE the heavy gate; 10-min wall-clock kill switch per cargo command; report `DONE_WITH_CONCERNS` rather than hang. iroh/zenoh first-bind flakes are unrelated (no iroh-binding tests added). Use `${pipestatus[1]}` in zsh (not `${PIPESTATUS[0]}`) or avoid piping cargo.

---

### Task 1: Rust — bound `fetch_recursive` via `FetchRequest.max_bytes`

**Files:**
- Modify: `src-tauri/src/event_loop.rs` — `FetchRequest` struct (line 190), `fetch_recursive` (line 5248), the `fetch_rx` arm (line 2529), and 4 test callers (lines 5490, 5518, 5543, 5647).
- Modify: `src-tauri/src/lib.rs` — 3 `FetchRequest { … }` constructors (lines 8817, 11960, 12066) get `max_bytes: None`.
- Modify: `src-tauri/src/mail_sync.rs` — 1 `FetchRequest { … }` constructor (line 429) gets `max_bytes: None`.

- [ ] **Step 1: Write the failing cap tests**

In `src-tauri/src/event_loop.rs`, inside `mod fetch_recursive_tests` (around line 5472, after `missing_leaf_propagates_error`), add:

```rust
    #[tokio::test]
    async fn max_bytes_cap_rejects_oversized_assembly() {
        // a(3)+b(4)+c(5) = 12 bytes assembled, fetched in order a,b,c.
        let a_bytes = b"aaa".to_vec();
        let b_bytes = b"bbbb".to_vec();
        let c_bytes = b"ccccc".to_vec();
        let a = ContentId::for_book(&a_bytes, ContentFlags::default()).unwrap();
        let b = ContentId::for_book(&b_bytes, ContentFlags::default()).unwrap();
        let c = ContentId::for_book(&c_bytes, ContentFlags::default()).unwrap();
        let mut builder = BundleBuilder::new();
        builder.add(a).add(b).add(c);
        let (payload, root) = builder.build_with_flags(ContentFlags::default()).unwrap();
        let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
        store.insert(a, a_bytes);
        store.insert(b, b_bytes);
        store.insert(c, c_bytes);
        store.insert(root, payload);
        let fetcher = move |cid: ContentId| {
            let bytes = store.get(&cid).cloned();
            std::future::ready(bytes.ok_or_else(|| format!("missing cid: {cid:?}")))
        };

        // cap=5 → rejected once a(3)+b(4)=7 > 5.
        let err = fetch_recursive(fetcher.clone(), root, Some(5)).await.unwrap_err();
        assert!(err.contains("exceeds"), "got: {err}");
        // cap=12 (exactly the total) → accepted.
        let got = fetch_recursive(fetcher.clone(), root, Some(12)).await.unwrap();
        assert_eq!(got.len(), 12);
        // None → unbounded, accepted.
        let got = fetch_recursive(fetcher, root, None).await.unwrap();
        assert_eq!(got.len(), 12);
    }
```

NOTE: this requires the existing three tests in this mod (`leaf_only_fetch_returns_single_payload` line 5490, `bundle_fetch_concatenates_children_in_order` line 5518, `missing_leaf_propagates_error` line 5543) to pass `None` as the new 3rd arg — do that in Step 3. The `fetcher` closure here must be `Clone` (it is — it captures a `HashMap` by move and is reused via `.clone()`).

- [ ] **Step 2: Run tests to verify they fail (compile error)**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(max_bytes_cap_rejects_oversized_assembly)'`
Expected: FAIL — `fetch_recursive` takes 2 args, not 3.

- [ ] **Step 3: Add `max_bytes` to `fetch_recursive` + the cap check**

In `src-tauri/src/event_loop.rs`, change `fetch_recursive`'s signature (line 5248) and add the check. Replace:

```rust
pub(crate) async fn fetch_recursive<F, Fut>(
    fetch_one: F,
    root: ContentId,
) -> Result<Vec<u8>, String>
```

with:

```rust
pub(crate) async fn fetch_recursive<F, Fut>(
    fetch_one: F,
    root: ContentId,
    max_bytes: Option<usize>,
) -> Result<Vec<u8>, String>
```

and in the body, replace the leaf branch:

```rust
        } else {
            out.extend_from_slice(&bytes);
        }
```

with:

```rust
        } else {
            out.extend_from_slice(&bytes);
            // ZEB-344: bound the assembled size so an oversized avatar_cid
            // can't force an unbounded download. ≤ cap + one chunk (a single
            // chunk is bounded by ChunkerConfig::DEFAULT). None = unbounded.
            if let Some(cap) = max_bytes {
                if out.len() > cap {
                    return Err(format!(
                        "content exceeds max_bytes cap: {} > {cap}",
                        out.len()
                    ));
                }
            }
        }
```

- [ ] **Step 4: Add the field to `FetchRequest` + update all callers**

In `src-tauri/src/event_loop.rs`, the `FetchRequest` struct (line 190) — add the field:

```rust
pub struct FetchRequest {
    pub cid_hex: String,
    pub reply: oneshot::Sender<Result<Vec<u8>, String>>,
    /// ZEB-344: optional assembled-byte ceiling enforced by `fetch_recursive`.
    /// `None` = unbounded (all callers except `fetch_avatar`).
    pub max_bytes: Option<usize>,
}
```

In the `fetch_rx` arm (line 2529), after `let cid_hex = req.cid_hex;` add `let max_bytes = req.max_bytes;`, and change the `fetch_recursive` call (line 2584) from:

```rust
                    let result = fetch_recursive(fetch_one_with_admit, root).await;
```

to:

```rust
                    let result = fetch_recursive(fetch_one_with_admit, root, max_bytes).await;
```

Update the 3 existing tests in `mod fetch_recursive_tests` and the 1 in `mod fetch_one_wrapper_tests` to pass `None`:
- line 5490: `fetch_recursive(fetcher, leaf, None).await.unwrap();`
- line 5518: `fetch_recursive(fetcher, root, None).await.unwrap();`
- line 5543: `fetch_recursive(fetcher, root, None).await.unwrap_err();`
- line 5647 (in `mod fetch_one_wrapper_tests`): `fetch_recursive(wrapped, root, None).await.unwrap();`

In `src-tauri/src/lib.rs`, add `max_bytes: None,` to the `FetchRequest { … }` constructors at lines 8817, 11960 (`fetch_content`), and 12066 (`fetch_profile_doc`). Each currently reads:

```rust
        .send(event_loop::FetchRequest {
            cid_hex: cid,
            reply: reply_tx,
        })
```

→

```rust
        .send(event_loop::FetchRequest {
            cid_hex: cid,
            reply: reply_tx,
            max_bytes: None,
        })
```

(The `cid_hex` value differs per site — keep each site's existing field values; only add `max_bytes: None`.)

In `src-tauri/src/mail_sync.rs` line 429, the `FetchRequest { … }` constructor likewise gets `max_bytes: None,`.

- [ ] **Step 5: Run the cap tests + the existing fetch tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(fetch_recursive)'`
Expected: PASS — `max_bytes_cap_rejects_oversized_assembly` + the 3 existing `fetch_recursive_tests` + the `fetch_one_wrapper_tests`.

- [ ] **Step 6: Commit, then gate**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/event_loop.rs src-tauri/src/lib.rs src-tauri/src/mail_sync.rs
git commit -m "feat(zeb-344): thread max_bytes through fetch_recursive/FetchRequest

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked -p harmony-app --lib --bins --features test-fixtures --no-deps -- -D warnings
```
Expected: fmt clean, clippy 0 warnings. (If fmt diffs: `cargo fmt --all` + `git commit --amend --no-edit`.)

---

### Task 2: Rust — `fetch_avatar` IPC + shared `AVATAR_MAX_BYTES`

**Files:**
- Modify: `src-tauri/src/lib.rs` — promote the ingest cap to a module const (near `ingest_avatar_bytes_inner`, ~line 8918); add `fetch_avatar` after `fetch_content` (ends ~line 11970); register in `invoke_handler!` (line 37579, next to `fetch_content,`).

- [ ] **Step 1: Write the failing tests**

In `src-tauri/src/lib.rs`, add a test module near the avatar code (place it right before `ingest_avatar_bytes_inner`, or in an existing nearby `#[cfg(test)]` block). It reuses `mock_app_with_default_node_state()` (the same helper the ZEB-385 / ZEB-321 connectivity IPC tests use — find it in the lib.rs test modules and `use` it if the new mod isn't a sibling):

```rust
#[cfg(test)]
mod zeb_344_avatar_fetch_tests {
    use super::*;

    #[test]
    fn avatar_max_bytes_is_512k() {
        assert_eq!(AVATAR_MAX_BYTES, 512 * 1024);
    }

    #[tokio::test]
    async fn fetch_avatar_rejects_malformed_hex() {
        let app = mock_app_with_default_node_state();
        let state = app.state::<StdMutex<NodeState>>();
        let got = fetch_avatar("nothex!!".to_string(), state).await;
        assert!(
            got.is_err_and(|e| e.contains("invalid CID hex")),
            "expected invalid-hex rejection"
        );
    }
}
```

If `mock_app_with_default_node_state` / `StdMutex` are not reachable via `use super::*` from this location, place the `#[tokio::test]` in the existing test module that already uses them (search for `fn mock_app_with_default_node_state`) and keep `avatar_max_bytes_is_512k` here.

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(avatar_max_bytes_is_512k) + test(fetch_avatar_rejects_malformed_hex)'`
Expected: FAIL — `AVATAR_MAX_BYTES` and `fetch_avatar` don't exist yet.

- [ ] **Step 3: Promote the shared const**

In `src-tauri/src/lib.rs`, immediately ABOVE `pub(crate) async fn ingest_avatar_bytes_inner(` (~line 8924), add:

```rust
/// ZEB-344: shared avatar byte ceiling. Enforced on ingest
/// (`ingest_avatar_bytes_inner`) AND on receive (`fetch_avatar` via
/// `FetchRequest.max_bytes`), so the two cannot drift. 512KB carries ~2× headroom
/// over a realistic 256×256 PNG (≤256KB).
pub(crate) const AVATAR_MAX_BYTES: usize = 512 * 1024;
```

Then inside `ingest_avatar_bytes_inner`, DELETE the local `const MAX_AVATAR_BYTES: usize = 512 * 1024;` line (8929) and replace the two `MAX_AVATAR_BYTES` usages (the `if bytes.len() > MAX_AVATAR_BYTES` guard and the `format!` message) with `AVATAR_MAX_BYTES`:

```rust
    if bytes.len() > AVATAR_MAX_BYTES {
        return Err(format!(
            "avatar too large: {} > {AVATAR_MAX_BYTES}",
            bytes.len()
        ));
    }
```

- [ ] **Step 4: Add the `fetch_avatar` IPC**

In `src-tauri/src/lib.rs`, immediately AFTER `fetch_content`'s closing brace (~line 11970), add:

```rust
/// ZEB-344: avatar-semantic CAS fetch. Identical to `fetch_content` but caps the
/// download at `AVATAR_MAX_BYTES` via `FetchRequest.max_bytes`, so an oversized
/// `avatar_cid` on a peer's signed profile card can't force an unbounded fetch.
/// The decoded-dimension guard lives on the receive side in `AvatarResolver`.
#[tauri::command]
async fn fetch_avatar(
    cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<u8>, String> {
    if cid.is_empty() || !cid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid CID hex: {cid}"));
    }

    let fetch_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .fetch_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    fetch_tx
        .send(event_loop::FetchRequest {
            cid_hex: cid,
            reply: reply_tx,
            max_bytes: Some(AVATAR_MAX_BYTES),
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;

    reply_rx
        .await
        .map_err(|_| "event loop dropped fetch request".to_string())?
}
```

- [ ] **Step 5: Register in `invoke_handler!`**

At line 37579, add `fetch_avatar,` directly below `fetch_content,`:

```rust
            fetch_content,
            fetch_avatar,
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(avatar_max_bytes_is_512k) + test(fetch_avatar_rejects_malformed_hex)'`
Expected: PASS.

- [ ] **Step 7: Commit, then gate**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-344): fetch_avatar IPC caps download at shared AVATAR_MAX_BYTES

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked -p harmony-app --lib --bins --features test-fixtures --no-deps -- -D warnings
```
Expected: fmt clean, clippy 0 warnings.

---

### Task 3: TS — `AvatarResolver` decode guard + switch to `fetch_avatar`

**Files:**
- Modify: `src/lib/avatar-resolver.ts` — import `assertDecodedDimsOk`; in `fetchCid`, target `fetch_avatar` and add the decode guard.
- Create: `src/lib/__tests__/avatar-resolver.test.ts`.

- [ ] **Step 1: Write the failing test**

Create `src/lib/__tests__/avatar-resolver.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { AvatarResolver } from '../avatar-resolver';
import type { TauriAdapter } from '../zenoh-service';

// PNG magic bytes so the resolver's detectImageMime returns image/png.
const PNG_BYTES = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

function makeAdapter(): TauriAdapter {
  return { invoke: vi.fn().mockResolvedValue(PNG_BYTES) } as unknown as TauriAdapter;
}

let createImageBitmapMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  createImageBitmapMock = vi.fn().mockResolvedValue({ width: 256, height: 256, close: vi.fn() });
  vi.stubGlobal('createImageBitmap', createImageBitmapMock);
  // jsdom lacks URL.createObjectURL / revokeObjectURL.
  vi.stubGlobal('URL', {
    ...URL,
    createObjectURL: vi.fn(() => 'blob:mock-url'),
    revokeObjectURL: vi.fn(),
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('AvatarResolver — receive-side decode guard (ZEB-344)', () => {
  it('fetches via fetch_avatar and caches a URL for an in-bounds image', async () => {
    const resolver = new AvatarResolver();
    const adapter = makeAdapter();
    resolver.connectAdapter(adapter);

    await (resolver as unknown as { fetchCid(cid: string): Promise<void> }).fetchCid('aa');

    expect(adapter.invoke).toHaveBeenCalledWith('fetch_avatar', { cid: 'aa' });
    expect(resolver.resolve('aa')).toBe('blob:mock-url');
  });

  it('rejects an over-dimension (decode-bomb) image and falls back (no cached URL)', async () => {
    createImageBitmapMock.mockResolvedValue({ width: 9000, height: 9000, close: vi.fn() });
    const resolver = new AvatarResolver();
    const adapter = makeAdapter();
    resolver.connectAdapter(adapter);

    await (resolver as unknown as { fetchCid(cid: string): Promise<void> }).fetchCid('bb');

    expect(resolver.resolve('bb')).toBeUndefined();
  });
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `npx vitest run src/lib/__tests__/avatar-resolver.test.ts`
Expected: FAIL — the first test expects `'fetch_avatar'` but the resolver still calls `'fetch_content'` (and/or no decode guard yet).

- [ ] **Step 3: Add the import**

In `src/lib/avatar-resolver.ts`, after line 1 (`import type { TauriAdapter } from './zenoh-service';`), add:

```typescript
import { assertDecodedDimsOk } from './avatar-normalize';
```

- [ ] **Step 4: Switch to `fetch_avatar` + add the decode guard**

In `fetchCid`, replace this block:

```typescript
      const bytes = (await this.adapter.invoke('fetch_content', { cid })) as number[];
      if (this.destroyed) return;
      const mime = detectImageMime(bytes);
      const blob = new Blob([new Uint8Array(bytes)], { type: mime });
      const url = URL.createObjectURL(blob);
      this.cache.set(cid, url);
      this.onChange?.();
```

with:

```typescript
      const bytes = (await this.adapter.invoke('fetch_avatar', { cid })) as number[];
      if (this.destroyed) return;
      const mime = detectImageMime(bytes);
      const blob = new Blob([new Uint8Array(bytes)], { type: mime });
      // ZEB-344: decoded-dimension guard on the RECEIVE path (parity with
      // normalizeAvatar's ingest guard) — reject a decode bomb before its blob
      // URL ever reaches an <img>. A throw here lands in the catch → identicon.
      const bmp = await createImageBitmap(blob);
      try {
        assertDecodedDimsOk(bmp.width, bmp.height);
      } finally {
        bmp.close();
      }
      const url = URL.createObjectURL(blob);
      this.cache.set(cid, url);
      this.onChange?.();
```

(Also update the class doc-comment reference from "`fetch_content` Tauri command" to "`fetch_avatar` Tauri command" on the lines around 7-9.)

- [ ] **Step 5: Run the test to verify it passes**

Run: `npx vitest run src/lib/__tests__/avatar-resolver.test.ts`
Expected: PASS — 2 cases.

- [ ] **Step 6: Type-check, commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
git add src/lib/avatar-resolver.ts src/lib/__tests__/avatar-resolver.test.ts
git commit -m "feat(zeb-344): AvatarResolver decode-dimension guard + fetch_avatar

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```
Expected: `tsc` clean.

---

## Final verification (after all tasks)

```bash
cd src-tauri && cargo fmt --all -- --check \
  && cargo clippy --locked -p harmony-app --lib --bins --features test-fixtures --no-deps -- -D warnings \
  && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(fetch_recursive) + test(fetch_avatar) + test(avatar_max_bytes)'
cd .. && npx tsc --noEmit && npx vitest run src/lib/__tests__/avatar-resolver.test.ts src/lib/__tests__/avatar-normalize.test.ts
```

`--all-targets` is left to CI (relink cost). None of these changes touch integration-test symbols, so the lib-scoped run is sufficient locally.

---

## Self-Review

**1. Spec coverage:**
- Byte cap on the avatar fetch path, bounding the download (not just post-buffer) → Task 1 (`fetch_recursive` cap) + Task 2 (`fetch_avatar` sets it). ✓
- Shared `AVATAR_MAX_BYTES` (512KB), ingest+receive can't drift → Task 2 (promote const, both sites reference it). ✓
- Generic `fetch_content` stays unbounded → Task 1 (passes `None`). ✓
- Decoded-dimension guard on receive, identicon fallback → Task 3. ✓
- Reuse `assertDecodedDimsOk` / `AVATAR_MAX_DECODED_DIM` → Task 3 import. ✓
- Acceptance: over-cap → no unbounded download (Task 1 test); over-decode → identicon (Task 3 test); happy path (None/under-cap Task 1 + unchanged fetch_content). ✓
- Header-only pre-decode parse stays OUT (follow-up filed separately by controller). ✓

**2. Placeholder scan:** No TBD/TODO/"handle edge cases". Every code step has complete code. The one conditional (Task 2 Step 1 test placement if helper not in scope) gives an explicit fallback. ✓

**3. Type consistency:** `max_bytes: Option<usize>` is identical across `FetchRequest`, `fetch_recursive`, and all constructors. `AVATAR_MAX_BYTES: usize` used in both ingest and `fetch_avatar`. IPC name `fetch_avatar` matches across Rust (fn + registry + test) and TS (`invoke('fetch_avatar', …)` + test). `assertDecodedDimsOk(width, height)` signature matches `avatar-normalize.ts`. `fetchCid` private-method access in the test matches the class. ✓
