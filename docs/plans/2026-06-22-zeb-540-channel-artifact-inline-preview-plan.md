# Channel Artifact Inline Preview — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add click-to-preview for `image/*` and `text/*` channel artifacts (≤ 4 MiB) inline in the feed, via a new in-memory `preview_channel_artifact` IPC that reuses the download authorize-first gate but returns plaintext bytes instead of writing to disk.

**Architecture:** Backend factors two pure helpers out of the shipped download path so the security-critical authorize-first gate lives in one place, then adds a preview command with a tighter cap. Frontend adds a service facade, a pure preview-helpers module, and a per-cid preview state machine in `MessageAttachments.svelte` that renders an image via a blob URL (reusing the avatar decode-bomb guards) or a decoded text head, revoking blob URLs on collapse/unmount.

**Tech Stack:** Rust (Tauri v2 command + `tokio`), Svelte 5 runes, TypeScript, vitest, cargo-nextest.

**Spec:** `docs/specs/2026-06-22-zeb-540-channel-artifact-inline-preview-design.md`

**Conventions (load-bearing):**
- Cargo commands run from `src-tauri/`. Frontend commands run from repo root.
- Tauri IPC: Rust params `snake_case`, JS callers `camelCase`.
- IPC error extraction: `e instanceof Error ? e.message : String(e)`.
- Keep ZEB-NNN out of commit messages (doc/code comments may reference it).
- Commit messages end with `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.

---

## Task 1: Backend — extract `decrypt_and_verify_artifact` + `authorize_and_fetch_artifact` (no behavior change)

**Files:**
- Modify: `src-tauri/src/lib.rs` — `finalize_artifact` (~line 20623) and `download_channel_artifact_impl` (~line 20706).

The shipped `download_channel_artifact_impl` and `finalize_artifact` are the regression net: after refactor, the existing `download_channel_artifact_*` tests must still pass. **This task introduces no new behavior.**

- [ ] **Step 1: Extract the pure decrypt+verify tail from `finalize_artifact`.**

Add this **sync** helper immediately ABOVE `finalize_artifact`:

```rust
/// ZEB-540: pure decrypt + plaintext-size-verify, shared by the download
/// (writes to disk) and preview (returns bytes) paths. Decrypts with the epoch
/// key when `encrypted` (the nonce `encrypt_blob` prepended is read back by
/// `decrypt_blob`), then verifies the plaintext length equals the authoritative
/// `expected_size`. No I/O — unit-testable without a `NodeState`.
fn decrypt_and_verify_artifact(
    bytes: Vec<u8>,
    encrypted: bool,
    epoch_key_opt: Option<crate::owner_state_types::EpochKey>,
    expected_size: u64,
) -> Result<Vec<u8>, String> {
    let plaintext = if encrypted {
        let epoch_key =
            epoch_key_opt.ok_or_else(|| "encrypted artifact requires an epoch key".to_string())?;
        crate::community_state_sync::decrypt_blob(&epoch_key, &bytes)
            .map_err(|e| format!("decrypt: {e:?}"))?
    } else {
        bytes
    };

    // Verify length BEFORE any caller-side write so a mismatch never leaves a file.
    if plaintext.len() as u64 != expected_size {
        return Err(format!(
            "size mismatch: got {} expected {expected_size}",
            plaintext.len()
        ));
    }
    Ok(plaintext)
}
```

Then replace the decrypt+verify head of `finalize_artifact` (the `let plaintext = if encrypted { ... } else { bytes }; if plaintext.len() ... { return Err(...) }` block, ~lines 20630-20645) with a single call, leaving the atomic-write tail unchanged:

```rust
async fn finalize_artifact(
    bytes: Vec<u8>,
    encrypted: bool,
    epoch_key_opt: Option<crate::owner_state_types::EpochKey>,
    expected_size: u64,
    dest_path: &str,
) -> Result<u64, String> {
    let plaintext = decrypt_and_verify_artifact(bytes, encrypted, epoch_key_opt, expected_size)?;

    // Atomic write: temp file in the dest dir, then rename over the target.
    // (unchanged from here down — keep the existing ARTIFACT_TMP_SEQ temp+rename body)
    let dest = std::path::Path::new(dest_path);
    let seq = ARTIFACT_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dest.with_extension(format!("partial.{}.{}", std::process::id(), seq));
    tokio::fs::write(&tmp, &plaintext)
        .await
        .map_err(|e| format!("write tmp: {e}"))?;
    if let Err(e) = tokio::fs::rename(&tmp, dest).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(format!("rename: {e}"));
    }
    Ok(plaintext.len() as u64)
}
```

- [ ] **Step 2: Extract the authorize-first + fetch block into a shared helper.**

Add this struct + helper ABOVE `download_channel_artifact_impl`. The body is the existing lines ~20714-20824 of `download_channel_artifact_impl`, parameterized by `cap` (the caller clamps to its own ceiling) and taking ids by `&str`:

```rust
/// ZEB-540: result of [`authorize_and_fetch_artifact`] — the fetched bytes
/// (ciphertext if `encrypted`, else plaintext) plus the metadata needed to
/// decrypt/verify and (for download) allowlist re-serve.
pub(crate) struct ArtifactFetch {
    pub bytes: Vec<u8>,
    pub encrypted: bool,
    pub epoch_key_opt: Option<crate::owner_state_types::EpochKey>,
    pub expected_size: u64,
    pub content_id: harmony_content::cid::ContentId,
}

/// ZEB-540: the security-critical authorize-FIRST gate + bounded fetch, shared by
/// `download_channel_artifact_impl` and `preview_channel_artifact_impl`. The
/// signed channel log is the source of truth: an unmatched CID is rejected
/// (`"unknown or unauthorized attachment"`) and the authoritative `expected_size`
/// comes from the signed attachment, not the caller. The over-`cap` rejection
/// happens BEFORE any byte is fetched. The fetch is issued `serveable: false`
/// (no re-serve allowlisting during assembly — download grants that
/// post-validation; preview never does).
pub(crate) async fn authorize_and_fetch_artifact(
    state: &std::sync::Mutex<NodeState>,
    community_id: &str,
    channel_id: &str,
    cid: &str,
    cap: u64,
) -> Result<ArtifactFetch, String> {
    use harmony_content::cid::ContentId;

    // Bound the hex length BEFORE decoding (a multi-MB hex string would allocate
    // ~half its length before the [u8; 32] conversion fails — IPC-boundary DoS).
    if cid.len() != 64 {
        return Err("invalid cid hex".to_string());
    }
    let cid_bytes: [u8; 32] = hex::decode(cid)
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .ok_or_else(|| "invalid cid hex".to_string())?;
    let content_id = ContentId::from_bytes(cid_bytes);
    let encrypted = content_id.flags().encrypted;

    if community_id.len() != 32 {
        return Err("community_id must be 16 bytes (32 hex chars)".to_string());
    }
    if channel_id.len() != 32 {
        return Err("channel_id must be 16 bytes (32 hex chars)".to_string());
    }
    let cid_bytes16: [u8; 16] = hex::decode(community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .try_into()
        .map_err(|_| "community_id length wrong".to_string())?;
    let chid_bytes16: [u8; 16] = hex::decode(channel_id)
        .map_err(|e| format!("invalid channel_id hex: {e}"))?
        .try_into()
        .map_err(|_| "channel_id length wrong".to_string())?;
    let space = crate::owner_state_types::SpaceId(cid_bytes16);
    let chid = crate::community_membership::ChannelId(chid_bytes16);

    let registry = {
        let guard = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        guard
            .channel_log_registry
            .as_ref()
            .ok_or_else(|| "channel_log_registry missing — node not running".to_string())?
            .clone()
    };
    let engine = registry
        .engine(&space, &chid)
        .await
        .ok_or_else(|| format!("no engine for {community_id}/{channel_id}"))?;

    let att = engine
        .find_attachment(&cid_bytes)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "unknown or unauthorized attachment".to_string())?;
    let expected_size = att.size; // AUTHORITATIVE — from the signed channel event.

    if expected_size > cap {
        return Err(format!("artifact size {expected_size} exceeds cap {cap}"));
    }

    let epoch_key_opt = if encrypted {
        Some(current_epoch_key_for(state, &space).await?)
    } else {
        None
    };

    let fetch_tx = {
        let g = state.lock().map_err(|e| format!("lock: {e}"))?;
        g.fetch_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };
    // Fetch `max_bytes` bounds the ASSEMBLED bytes — ciphertext for an encrypted
    // CID, so widen by the AEAD nonce+tag overhead; plaintext is still bounded by
    // the size check in `decrypt_and_verify_artifact`.
    let fetch_cap = if encrypted {
        expected_size.saturating_add(crate::community_state_sync::BLOB_ENCRYPTION_OVERHEAD as u64)
    } else {
        expected_size
    };

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    fetch_tx
        .send(event_loop::FetchRequest {
            cid_hex: cid.to_string(),
            reply: reply_tx,
            max_bytes: Some(fetch_cap as usize),
            serveable: false,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    let bytes = reply_rx
        .await
        .map_err(|_| "event loop dropped fetch request".to_string())??;

    Ok(ArtifactFetch {
        bytes,
        encrypted,
        epoch_key_opt,
        expected_size,
        content_id,
    })
}
```

- [ ] **Step 3: Rewrite `download_channel_artifact_impl` to use the helpers.**

Keep its signature + doc comment. Replace the body with:

```rust
pub(crate) async fn download_channel_artifact_impl(
    state: &std::sync::Mutex<NodeState>,
    community_id: String,
    channel_id: String,
    cid: String,
    dest_path: String,
    max_bytes: Option<u64>,
) -> Result<u64, String> {
    // Clamp the client ceiling to the download cap; the helper does the
    // authoritative `expected_size > cap` check.
    let cap = max_bytes
        .map(|m| m.min(MAX_ARTIFACT_BYTES))
        .unwrap_or(MAX_ARTIFACT_BYTES);
    let ArtifactFetch {
        bytes,
        encrypted,
        epoch_key_opt,
        expected_size,
        content_id,
    } = authorize_and_fetch_artifact(state, &community_id, &channel_id, &cid, cap).await?;

    let written =
        finalize_artifact(bytes, encrypted, epoch_key_opt, expected_size, &dest_path).await?;

    // Post-validation re-serve allowlist (ZEB-539) — encrypted only; non-fatal.
    if encrypted {
        let cid_bytes = content_id.to_bytes();
        let content_store = match state.lock() {
            Ok(g) => g.content_store.clone(),
            Err(e) => {
                tracing::warn!(
                    cid = %hex::encode(cid_bytes),
                    error = %e,
                    "ZEB-539: lock poisoned during re-serve allowlisting (non-fatal; file written)"
                );
                None
            }
        };
        if let Some(store) = content_store {
            match store.allow_serve_subtree(content_id).await {
                Ok(n) => tracing::debug!(
                    cid = %hex::encode(cid_bytes),
                    allowlisted = n,
                    "ZEB-539: allowlisted downloaded artifact subtree for re-serve"
                ),
                Err(e) => tracing::warn!(
                    cid = %hex::encode(cid_bytes),
                    error = %e,
                    "ZEB-539: failed to allowlist artifact subtree for re-serve (non-fatal; file written)"
                ),
            }
        }
    }

    Ok(written)
}
```

- [ ] **Step 4: Verify no behavior change.**

Run (from `src-tauri/`):
```bash
cargo nextest run --locked --features test-fixtures -E 'test(download_channel_artifact) + test(finalize_artifact) + test(ingest_channel_artifact)'
```
Expected: all existing download/finalize/ingest artifact tests PASS. Then:
```bash
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```
Expected: 0 warnings.

- [ ] **Step 5: Commit.**

```bash
git add src-tauri/src/lib.rs
git commit -m "refactor: extract shared authorize_and_fetch + decrypt_verify artifact helpers

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Backend — `preview_channel_artifact` command + cap + tests

**Files:**
- Modify: `src-tauri/src/lib.rs` — add `MAX_PREVIEW_BYTES`, `preview_channel_artifact_impl`, `#[tauri::command] preview_channel_artifact`, register in `invoke_handler!`, add tests.

- [ ] **Step 1: Write failing Rust tests.**

Find the existing `download_channel_artifact_rejects_unauthorized_cid` test (~line 22767) and `download_channel_artifact_rejects_overlong_cid_hex` (~line 49921) to mirror their setup. Add these tests in the same `#[cfg(test)] mod` (use the same imports/helpers the download tests use):

```rust
#[tokio::test]
async fn decrypt_and_verify_artifact_public_passthrough_and_size_check() {
    // Public (unencrypted): bytes returned verbatim when length matches.
    let bytes = b"hello world".to_vec();
    let got = decrypt_and_verify_artifact(bytes.clone(), false, None, bytes.len() as u64)
        .expect("public passthrough");
    assert_eq!(got, bytes);

    // Size mismatch → Err, no panic.
    let err = decrypt_and_verify_artifact(b"short".to_vec(), false, None, 999)
        .expect_err("size mismatch must error");
    assert!(err.contains("size mismatch"), "got: {err}");

    // Encrypted but no key → Err.
    let err = decrypt_and_verify_artifact(b"x".to_vec(), true, None, 1)
        .expect_err("encrypted needs key");
    assert!(err.contains("epoch key"), "got: {err}");
}

#[tokio::test]
async fn preview_channel_artifact_rejects_overlong_cid_hex() {
    // Mirror download_channel_artifact_rejects_overlong_cid_hex: a >64-char cid is
    // rejected at the boundary before any state work. Build the same minimal
    // NodeState the download test uses (copy its construction).
    let state = /* COPY: the std::sync::Mutex<NodeState> setup from
                   download_channel_artifact_rejects_overlong_cid_hex */;
    let long_cid = "ab".repeat(64); // 128 hex chars
    let err = preview_channel_artifact_impl(
        &state,
        "00".repeat(16),
        "11".repeat(16),
        long_cid,
        None,
    )
    .await
    .expect_err("overlong cid must be rejected");
    assert!(err.contains("invalid cid hex"), "got: {err}");
}

#[tokio::test]
async fn preview_channel_artifact_rejects_unauthorized_cid() {
    // Mirror download_channel_artifact_rejects_unauthorized_cid EXACTLY, but call
    // preview_channel_artifact_impl (no dest_path) and expect the same
    // "unknown or unauthorized attachment" rejection. Copy that test's engine /
    // channel-log registry setup verbatim; the only change is the call:
    let state = /* COPY: the engine+registry NodeState setup from
                   download_channel_artifact_rejects_unauthorized_cid */;
    let (community_id, channel_id, bogus_cid) = /* COPY from that test */;
    let err = preview_channel_artifact_impl(&state, community_id, channel_id, bogus_cid, None)
        .await
        .expect_err("unauthorized cid must be rejected");
    assert!(
        err.contains("unknown or unauthorized attachment")
            || err.contains("no engine")
            || err.contains("channel_log_registry"),
        "got: {err}"
    );
}
```

> NOTE to implementer: the two `_rejects_*` tests must reuse the EXACT NodeState/engine construction from the corresponding `download_channel_artifact_rejects_*` tests (read them first). Do not invent a new harness. If the download "unauthorized" test asserts a specific error string, match it.

Run (expected FAIL — `preview_channel_artifact_impl` / `MAX_PREVIEW_BYTES` not defined yet):
```bash
cargo nextest run --locked --features test-fixtures -E 'test(preview_channel_artifact) + test(decrypt_and_verify_artifact)'
```

- [ ] **Step 2: Add the cap constant.**

Next to `pub(crate) const MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;` (~line 20370):

```rust
/// ZEB-540: in-memory preview cap (4 MiB). Far below the 1 GiB download cap
/// (`MAX_ARTIFACT_BYTES`) because a preview decrypts the whole artifact into
/// memory (~2× at decrypt) and ships it over the IPC boundary. Artifacts larger
/// than this are download-only — the frontend hides the Preview affordance and
/// this command also rejects defensively.
pub(crate) const MAX_PREVIEW_BYTES: u64 = 4 * 1024 * 1024;
```

- [ ] **Step 3: Add the impl + command.**

Place beside `download_channel_artifact` (the `#[tauri::command]` delegate ~line 20101 and impl ~line 20706):

```rust
/// ZEB-540: fetch an in-channel CAS artifact into memory for inline preview.
///
/// Like [`download_channel_artifact`] but capped at [`MAX_PREVIEW_BYTES`] and
/// returns the decrypted plaintext bytes instead of writing to disk. Shares the
/// authorize-first gate ([`authorize_and_fetch_artifact`]) — an unauthorized CID
/// or one whose signed size exceeds the cap is rejected before any fetch. Does
/// NOT allowlist the artifact for re-serve (preview is a lightweight read).
#[tauri::command]
async fn preview_channel_artifact(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    channel_id: String,
    cid: String,
    max_bytes: Option<u64>,
) -> Result<Vec<u8>, String> {
    preview_channel_artifact_impl(state_lock.inner(), community_id, channel_id, cid, max_bytes).await
}

/// ZEB-445 shared IPC/RPC seam for [`preview_channel_artifact`].
pub(crate) async fn preview_channel_artifact_impl(
    state: &std::sync::Mutex<NodeState>,
    community_id: String,
    channel_id: String,
    cid: String,
    max_bytes: Option<u64>,
) -> Result<Vec<u8>, String> {
    let cap = max_bytes
        .map(|m| m.min(MAX_PREVIEW_BYTES))
        .unwrap_or(MAX_PREVIEW_BYTES);
    let ArtifactFetch {
        bytes,
        encrypted,
        epoch_key_opt,
        expected_size,
        ..
    } = authorize_and_fetch_artifact(state, &community_id, &channel_id, &cid, cap).await?;
    decrypt_and_verify_artifact(bytes, encrypted, epoch_key_opt, expected_size)
}
```

- [ ] **Step 4: Register the command.** In the `invoke_handler!` / `generate_handler!` list where `download_channel_artifact,` and `ingest_channel_artifact,` appear (~line 47035), add `preview_channel_artifact,` next to them.

- [ ] **Step 5: Run the tests + clippy.**

```bash
cargo nextest run --locked --features test-fixtures -E 'test(preview_channel_artifact) + test(decrypt_and_verify_artifact) + test(download_channel_artifact)'
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo fmt --all -- --check
```
Expected: all PASS, 0 clippy warnings, fmt clean.

- [ ] **Step 6: Commit.**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat: preview_channel_artifact IPC (in-memory CAS fetch, 4 MiB cap)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Frontend — `artifact-preview.ts` pure helpers + test

**Files:**
- Create: `src/lib/artifact-preview.ts`
- Create: `src/lib/__tests__/artifact-preview.test.ts`

- [ ] **Step 1: Write the failing test.**

```typescript
import { describe, it, expect } from 'vitest';
import {
  PREVIEW_MAX_BYTES,
  isPreviewable,
  isImage,
  isText,
  decodeTextHead,
} from '../artifact-preview';
import type { ChannelAttachmentDto } from '../channel-message-service';

function att(p: Partial<ChannelAttachmentDto>): ChannelAttachmentDto {
  return { cid: 'aa', mime: 'text/plain', name: 'f', size: 10, encrypted: false, ...p };
}

describe('artifact-preview', () => {
  it('PREVIEW_MAX_BYTES is 4 MiB', () => {
    expect(PREVIEW_MAX_BYTES).toBe(4 * 1024 * 1024);
  });

  it('isPreviewable: image/text under cap true; over cap / other mime / empty false', () => {
    expect(isPreviewable(att({ mime: 'image/png', size: 1000 }))).toBe(true);
    expect(isPreviewable(att({ mime: 'text/plain', size: 1000 }))).toBe(true);
    expect(isPreviewable(att({ mime: 'image/png', size: PREVIEW_MAX_BYTES + 1 }))).toBe(false);
    expect(isPreviewable(att({ mime: 'application/zip', size: 10 }))).toBe(false);
    expect(isPreviewable(att({ mime: 'image/png', size: 0 }))).toBe(false);
    expect(isPreviewable(att({ mime: 'image/png', size: PREVIEW_MAX_BYTES }))).toBe(true); // boundary
  });

  it('isImage / isText classify by mime prefix', () => {
    expect(isImage(att({ mime: 'image/jpeg' }))).toBe(true);
    expect(isImage(att({ mime: 'text/plain' }))).toBe(false);
    expect(isText(att({ mime: 'text/markdown' }))).toBe(true);
    expect(isText(att({ mime: 'image/png' }))).toBe(false);
  });

  it('decodeTextHead returns head + full + truncated flag', () => {
    const lines = Array.from({ length: 100 }, (_, i) => `line ${i}`).join('\n');
    const bytes = new TextEncoder().encode(lines);
    const r = decodeTextHead(bytes, 40, 100000);
    expect(r.truncated).toBe(true);
    expect(r.head.split('\n').length).toBe(40);
    expect(r.full).toBe(lines);

    const small = new TextEncoder().encode('a\nb\nc');
    const r2 = decodeTextHead(small, 40, 100000);
    expect(r2.truncated).toBe(false);
    expect(r2.head).toBe('a\nb\nc');
  });

  it('decodeTextHead truncates on maxChars even within line budget', () => {
    const bytes = new TextEncoder().encode('x'.repeat(5000));
    const r = decodeTextHead(bytes, 40, 4000);
    expect(r.truncated).toBe(true);
    expect(r.head.length).toBe(4000);
  });
});
```

Run (expected FAIL — module missing): `npx vitest run src/lib/__tests__/artifact-preview.test.ts`

- [ ] **Step 2: Implement `src/lib/artifact-preview.ts`.**

```typescript
import type { ChannelAttachmentDto } from './channel-message-service';

/**
 * Frontend mirror of the backend `MAX_PREVIEW_BYTES` (src-tauri/src/lib.rs).
 * Used only to decide whether to OFFER a preview — the backend enforces the cap
 * authoritatively. Keep the two in sync (4 MiB).
 */
export const PREVIEW_MAX_BYTES = 4 * 1024 * 1024;

export function isImage(att: ChannelAttachmentDto): boolean {
  return att.mime.toLowerCase().startsWith('image/');
}

export function isText(att: ChannelAttachmentDto): boolean {
  return att.mime.toLowerCase().startsWith('text/');
}

/** True iff we can render an inline preview: an image or text artifact whose
 *  signed size is in (0, PREVIEW_MAX_BYTES]. Everything else is download-only. */
export function isPreviewable(att: ChannelAttachmentDto): boolean {
  return att.size > 0 && att.size <= PREVIEW_MAX_BYTES && (isImage(att) || isText(att));
}

export interface TextHead {
  head: string;
  full: string;
  truncated: boolean;
}

/** Decode UTF-8 bytes and return the first `maxLines` lines capped at `maxChars`.
 *  `truncated` is true if either bound clipped the text. `full` is the entire
 *  decoded string (we already hold all the bytes), used by the "show more" toggle. */
export function decodeTextHead(
  bytes: Uint8Array,
  maxLines = 40,
  maxChars = 4000,
): TextHead {
  const full = new TextDecoder().decode(bytes);
  const lines = full.split('\n');
  let head = lines.slice(0, maxLines).join('\n');
  let truncated = lines.length > maxLines;
  if (head.length > maxChars) {
    head = head.slice(0, maxChars);
    truncated = true;
  }
  return { head, full, truncated };
}
```

Run (expected PASS): `npx vitest run src/lib/__tests__/artifact-preview.test.ts`

- [ ] **Step 3: Commit.**

```bash
git add src/lib/artifact-preview.ts src/lib/__tests__/artifact-preview.test.ts
git commit -m "feat: artifact-preview pure helpers (previewable gate + text head)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: Frontend — `previewArtifact` service facade + test

**Files:**
- Modify: `src/lib/channel-message-service.ts` — add `previewArtifact` after `downloadArtifact` (~line 209).
- Modify: `src/lib/__tests__/channel-message-service.test.ts` — add tests (read the file first to match its existing adapter-mock harness; if it does not exist, create it mirroring the `downloadArtifact` test in the suite).

- [ ] **Step 1: Write the failing test** (adapt to the existing harness in the file):

```typescript
it('previewArtifact invokes preview_channel_artifact and returns Uint8Array', async () => {
  const svc = new ChannelMessageService();
  const adapter = { invoke: vi.fn().mockResolvedValue([1, 2, 3]), listen: vi.fn() } as unknown as TauriAdapter;
  await svc.connectAdapter(adapter);
  const att = { cid: 'cc', mime: 'image/png', name: 'p.png', size: 3, encrypted: true };
  const bytes = await svc.previewArtifact('00', '11', att, 4096);
  expect(adapter.invoke).toHaveBeenCalledWith('preview_channel_artifact', {
    communityId: '00', channelId: '11', cid: 'cc', maxBytes: 4096,
  });
  expect(bytes).toBeInstanceOf(Uint8Array);
  expect(Array.from(bytes)).toEqual([1, 2, 3]);
});

it('previewArtifact normalizes a rejection to an Error with the message', async () => {
  const svc = new ChannelMessageService();
  const adapter = { invoke: vi.fn().mockRejectedValue('peer offline'), listen: vi.fn() } as unknown as TauriAdapter;
  await svc.connectAdapter(adapter);
  const att = { cid: 'cc', mime: 'image/png', name: 'p.png', size: 3, encrypted: true };
  await expect(svc.previewArtifact('00', '11', att)).rejects.toThrow('peer offline');
});
```

> The existing suite already imports `ChannelMessageService`, `vi`, and `TauriAdapter` and connects an adapter for other tests — reuse that exact pattern (e.g. a `connectAdapter` mock with `listen: vi.fn().mockResolvedValue(() => {})`).

Run (expected FAIL): `npx vitest run src/lib/__tests__/channel-message-service.test.ts`

- [ ] **Step 2: Implement the facade** (after `downloadArtifact`):

```typescript
  /** Fetch a channel artifact into memory for inline preview (image/text).
   *  Returns the decrypted plaintext bytes. The backend authorizes the CID
   *  against the channel's signed log and rejects anything over the preview cap
   *  (default 4 MiB), so callers should only preview `isPreviewable` attachments.
   *  `maxBytes` further lowers the cap (clamped to the backend ceiling). */
  async previewArtifact(
    communityId: string,
    channelId: string,
    attachment: ChannelAttachmentDto,
    maxBytes?: number,
  ): Promise<Uint8Array> {
    if (!this.adapter) throw new Error('ChannelMessageService.previewArtifact: adapter not connected');
    try {
      const bytes = await this.adapter.invoke('preview_channel_artifact', {
        communityId,
        channelId,
        cid: attachment.cid,
        maxBytes,
      }) as number[];
      return new Uint8Array(bytes);
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(msg);
    }
  }
```

Run (expected PASS): `npx vitest run src/lib/__tests__/channel-message-service.test.ts`

- [ ] **Step 3: Commit.**

```bash
git add src/lib/channel-message-service.ts src/lib/__tests__/channel-message-service.test.ts
git commit -m "feat: ChannelMessageService.previewArtifact in-memory fetch facade

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: Frontend — preview UI in `MessageAttachments.svelte` + tests

**Files:**
- Modify: `src/lib/components/MessageAttachments.svelte`
- Modify: `src/lib/components/__tests__/MessageAttachments.test.ts`

Read both files first. The component currently renders a per-cid download state machine (`states`/`errors`, `uniqueAttachments` $derived, the `.attachment-chip` markup). Add a parallel **preview** state machine + render alongside the existing download chip. Do NOT remove the dedup or download behavior.

- [ ] **Step 1: Write failing component tests** (extend the existing test file; mirror its render/query harness + the avatar-resolver global stubs):

```typescript
// at top of the test file's setup (or in a beforeEach), stub the DOM globals the
// image-preview path needs (jsdom lacks them) — mirror avatar-resolver.test.ts:
beforeEach(() => {
  vi.stubGlobal('createImageBitmap', vi.fn().mockResolvedValue({ width: 800, height: 600, close: vi.fn() }));
  vi.stubGlobal('URL', { ...URL, createObjectURL: vi.fn(() => 'blob:mock'), revokeObjectURL: vi.fn() });
});
afterEach(() => vi.unstubAllGlobals());
```

Tests (use the file's existing render helper + a `channelMessageService` mock exposing `previewArtifact`):

```typescript
it('shows a Preview button only for previewable (image/text ≤ cap) attachments', async () => {
  // image attachment → Preview button present; a 'application/zip' attachment → absent.
});

it('previews an image: click fetches, creates a blob URL, renders <img>', async () => {
  // previewArtifact resolves a PNG byte array; click Preview → expect URL.createObjectURL called,
  // an <img> with src 'blob:mock' and alt = att.name visible.
});

it('previews text: click renders the decoded head in a <pre>', async () => {
  // previewArtifact resolves TextEncoder().encode('hello\nworld'); click → <pre> contains 'hello'.
});

it('collapse revokes the image blob URL and hides the <img>', async () => {
  // click Preview (shown) then click again → URL.revokeObjectURL called, no <img>.
});

it('surfaces a preview error with a retry', async () => {
  // previewArtifact rejects → error text shown; Retry re-invokes previewArtifact.
});

it('rejects an over-dimension image (decode bomb) without rendering <img>', async () => {
  // createImageBitmap resolves { width: 9000, height: 9000 } → assertDecodedDimsOk throws →
  // error state, no <img>. (8192 is the limit.)
});

it('revokes blob URLs on unmount', async () => {
  // render, preview an image, unmount the component → URL.revokeObjectURL called.
});
```

> Implementer: fill these in concretely against the file's actual render/query utilities (Testing Library `render` + `getByRole`/`getByText`, or the suite's existing pattern). Each test must FAIL before Step 2.

Run (expected FAIL): `npx vitest run src/lib/components/__tests__/MessageAttachments.test.ts`

- [ ] **Step 2: Implement the preview UI** in `MessageAttachments.svelte`.

Add to the `<script>` (alongside existing download state):

```svelte
  import { isPreviewable, isImage, isText, decodeTextHead, type TextHead } from '../artifact-preview';
  import { assertHeaderDimsOk, assertDecodedDimsOk } from '../avatar-normalize';

  type PreviewState = 'idle' | 'loading' | 'shown' | 'error';
  let previewStates = $state<Record<string, PreviewState>>({});
  let previewUrls = $state<Record<string, string>>({});   // image blob URLs
  let previewTexts = $state<Record<string, TextHead>>({});
  let previewExpanded = $state<Record<string, boolean>>({}); // text "show more"
  let previewErrors = $state<Record<string, string>>({});

  function previewStateOf(cid: string): PreviewState {
    return previewStates[cid] ?? 'idle';
  }

  function revokeUrl(cid: string) {
    const url = previewUrls[cid];
    if (url) {
      URL.revokeObjectURL(url);
      const { [cid]: _drop, ...rest } = previewUrls;
      previewUrls = rest;
    }
  }

  async function togglePreview(att: ChannelAttachmentDto) {
    const st = previewStateOf(att.cid);
    if (st === 'loading') return;
    if (st === 'shown') {
      // collapse + free
      revokeUrl(att.cid);
      const { [att.cid]: _t, ...t } = previewTexts; previewTexts = t;
      previewStates = { ...previewStates, [att.cid]: 'idle' };
      return;
    }
    previewStates = { ...previewStates, [att.cid]: 'loading' };
    previewErrors = { ...previewErrors, [att.cid]: '' };
    try {
      const bytes = await channelMessageService.previewArtifact(communityId, channelId, att);
      if (isImage(att)) {
        // Decode-bomb guards mirror avatar-resolver: header dims BEFORE decode,
        // decoded dims AFTER (8192px limit). A throw lands in catch → error.
        assertHeaderDimsOk(bytes);
        const blob = new Blob([bytes], { type: att.mime });
        const bmp = await createImageBitmap(blob);
        try {
          assertDecodedDimsOk(bmp.width, bmp.height);
        } finally {
          bmp.close();
        }
        previewUrls = { ...previewUrls, [att.cid]: URL.createObjectURL(blob) };
      } else {
        previewTexts = { ...previewTexts, [att.cid]: decodeTextHead(bytes) };
      }
      previewStates = { ...previewStates, [att.cid]: 'shown' };
    } catch (e) {
      previewStates = { ...previewStates, [att.cid]: 'error' };
      previewErrors = { ...previewErrors, [att.cid]: e instanceof Error ? e.message : String(e) };
    }
  }

  // Leak safety net: revoke every blob URL when this component unmounts (the feed
  // unmounts these children on channel switch / message churn). Mirrors
  // AvatarResolver.destroy().
  $effect(() => {
    return () => {
      for (const url of Object.values(previewUrls)) URL.revokeObjectURL(url);
    };
  });
```

Add to the chip markup — a Preview button when `isPreviewable(att)`, placed BEFORE the Download button:

```svelte
      {#if isPreviewable(att)}
        <button
          type="button"
          class="att-preview-btn"
          onclick={() => togglePreview(att)}
          disabled={previewStateOf(att.cid) === 'loading'}
          aria-label={previewStateOf(att.cid) === 'shown' ? `Hide preview ${att.name}` : `Preview ${att.name}`}
        >
          {#if previewStateOf(att.cid) === 'loading'}&#x2026;
          {:else if previewStateOf(att.cid) === 'shown'}&#x2715;
          {:else if previewStateOf(att.cid) === 'error'}&#x21BB;
          {:else}&#x1F441;{/if}
        </button>
      {/if}
```

Add the preview render block AFTER the chip `</div>` (and after the existing download `.att-error` block), inside the `{#each}`:

```svelte
    {#if previewStateOf(att.cid) === 'shown'}
      {#if isImage(att) && previewUrls[att.cid]}
        <img class="att-preview-img" src={previewUrls[att.cid]} alt={att.name} />
      {:else if isText(att) && previewTexts[att.cid]}
        <pre class="att-preview-text">{previewExpanded[att.cid] ? previewTexts[att.cid].full : previewTexts[att.cid].head}</pre>
        {#if previewTexts[att.cid].truncated}
          <button
            type="button"
            class="att-preview-more"
            onclick={() => (previewExpanded = { ...previewExpanded, [att.cid]: !previewExpanded[att.cid] })}
          >{previewExpanded[att.cid] ? 'Show less' : 'Show more'}</button>
        {/if}
      {/if}
    {/if}
    {#if previewStateOf(att.cid) === 'error'}
      <div class="att-error" role="alert">{previewErrors[att.cid]}</div>
    {/if}
```

Add CSS (bound the image + style the text block):

```svelte
  .att-preview-btn {
    flex: 0 0 auto;
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    cursor: pointer;
    padding: 2px 8px;
    font: inherit;
  }
  .att-preview-btn:hover:not(:disabled) { background: rgba(255, 255, 255, 0.06); }
  .att-preview-btn:disabled { opacity: 0.6; cursor: default; }
  .att-preview-img {
    display: block;
    max-width: 420px;
    max-height: 320px;
    margin-top: 4px;
    border: 1px solid var(--border);
    border-radius: 6px;
    object-fit: contain;
  }
  .att-preview-text {
    max-width: 420px;
    max-height: 320px;
    overflow: auto;
    margin-top: 4px;
    padding: 8px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 6px;
    font-size: 0.75rem;
    white-space: pre-wrap;
    word-break: break-word;
  }
  .att-preview-more {
    background: transparent; border: none; color: var(--text-secondary);
    cursor: pointer; font: inherit; font-size: 0.72rem; padding: 2px 0;
  }
```

> NOTE: there are now two possible `.att-error` blocks per cid (download error + preview error). That is fine — they are mutually exclusive in practice (a cid is being downloaded OR previewed). Keep both; do not merge the error state machines.

Run (expected PASS): `npx vitest run src/lib/components/__tests__/MessageAttachments.test.ts`

- [ ] **Step 3: Commit.**

```bash
git add src/lib/components/MessageAttachments.svelte src/lib/components/__tests__/MessageAttachments.test.ts
git commit -m "feat: inline click-to-preview for image/text channel artifacts

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: Final full gate

**Files:** none (verification only).

- [ ] **Step 1: Frontend gates** (repo root):
```bash
npx tsc --noEmit
npx vitest run
```
Expected: tsc clean; all vitest files pass.

- [ ] **Step 2: Rust gates** (`src-tauri/`):
```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: fmt clean; 0 clippy warnings; all tests pass (the 6 known iroh/zenoh transport orphan-flakes are non-blocking — re-run isolated if they appear).

- [ ] **Step 3:** If all green, the branch is ready for PR (handled by the controller, not this plan).

---

## Self-review checklist (controller, before dispatch)

- **Spec coverage:** Task 1-2 = backend IPC + cap + authorize gate; Task 3 = isPreviewable/decodeTextHead; Task 4 = facade; Task 5 = UI + blob lifecycle + decode-bomb guard; Task 6 = gate. All spec sections mapped. ✓
- **Type consistency:** `previewArtifact` returns `Uint8Array` (service) consumed by `togglePreview`; `decodeTextHead` returns `TextHead` used in render; `ArtifactFetch` fields destructured identically in download + preview. ✓
- **No placeholders:** the two Rust `_rejects_*` tests intentionally say "COPY from the download test" because they must reuse that exact NodeState harness — the implementer reads the sibling test. Everything else is complete code. ✓
