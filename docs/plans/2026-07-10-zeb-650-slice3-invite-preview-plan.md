# ZEB-650 slice 3 — Invite Preview Implementation Plan

> **For agentic workers:** Execute task-by-task; each task carries its own
> test cycle and ends in a commit. Steps use checkbox (`- [ ]`) syntax.

**Goal:** A pure-local `preview_invite` Tauri command (decode + token-signature
verify + expiry evaluation; mints nothing, joins nothing, no network) and a
debounced preview card in `RedeemInviteDialog` — spec
`docs/specs/2026-07-09-zeb-650-commons-g-deferred-data-design.md` §4.

**Architecture:** The Rust side composes three existing primitives with zero
new crypto: `community_invite::decode_invite_url` (URL → `CommunityInvitePayload`,
`src/community_invite.rs:1152`), `community_invite::verify_inviter_enrollment`
(cert-validity + Master-issuer + owner-binding + token-sig chain,
`src/community_invite.rs:1980`), and the wall-clock-ms expiry comparison used by
`verify_packet_pure` / `orphan_dir_adoption_eligible` (`now_ms >= exp`). The
frontend adds a `previewInvite` wrapper to `connectivity-adapter.ts` (the
dialog's existing IPC layer) and a debounced (300 ms) card in
`RedeemInviteDialog.svelte`.

**Tech Stack:** Rust (single-crate `harmony-app`), Tauri v2 sync command,
Svelte 5 runes, vitest + @testing-library/svelte.

## Global Constraints

- Honesty rule (ZEB-610 §0.4, pinned by an existing test): the card renders
  **no member/channel counts** — the token signature does not commit to the
  payload's name/snapshot; the ✓ is scoped to inviter authorization.
- The two-IPC TOCTOU rule does NOT apply (spec §4.1): preview and redeem both
  derive deterministically from the same immutable pasted URL string.
- `inviterDisplayName` is **always `None` in this slice**: local name
  resolution (friend nicknames / ZEB-281 profiles) is engine-async today; the
  DTO field exists so the wire shape is stable when resolution lands. The
  card's spec'd fallback (display name, else fingerprint) makes this honest.
- New DTO `#[serde(rename_all = "camelCase")]`; Rust params snake_case, TS
  caller camelCase (`{ url }`).
- Svelte 5 runes; color tokens only (`commons-hex-guard` stays empty); all
  existing testids/aria/copy pins preserved byte-identical.
- Existing inline `disabled`/guard expressions in RedeemInviteDialog are
  extended surgically (append `|| previewExpired`), not refactored.
- Frontend gates: `npx tsc --noEmit && npx vitest run`. Rust gates:
  `cargo fmt --all -- --check` + `cargo clippy --locked --all-targets
  --features test-fixtures --no-deps -- -D warnings` + targeted nextest per
  task. Final full sweep (explicit command): `cargo nextest run --locked
  --workspace --all-targets --features test-fixtures`.
- Commit per task; branch `zeb-650-slice3-invite-preview`; no worktrees.

## Ground truth (from exploration — verified at HEAD `51472b6d`)

- URL shape: `harmony://invite/` + base64url-no-pad of canonical CBOR;
  `decode_invite_url` trims, length-caps, decodes, runs shape guards
  (invite-only requires token + inviter_enrollment + admin_bootstrap +
  92-byte sealed key + untargeted key when untargeted; open requires 32-byte
  sealed key). It does NOT verify signatures.
- `InviteToken.expires_at: Option<u64>` wall-clock **ms**; `None` = no expiry;
  expired ⇔ `now_ms >= exp`. The outer `payload.expires_at: Option<Hlc>` is
  `None` in practice (generate_invite) but spec §4.1 says evaluate both.
- `token.inviter: OwnerAddr(pub [u8; 16])`. Fingerprint idiom
  `format_fingerprint(&[u8;16]) -> "xxxx·xxxx"` exists PRIVATE at
  `src/owner_commands.rs:140` — widen to `pub(crate)`.
- `verify_inviter_enrollment` returns `Ok(())` unconditionally for
  `!is_invite_only` payloads — so `inviter_verified` must be gated
  `payload.is_invite_only && verify(...)`, never derived from the bare call.
- Fixtures: `community_membership::mint_test_owner(seed)` (pub fields
  `owner`, `device_key`, `cert`; cert has no expiry so any `now_secs`
  verifies); `invite_mint::mint_invite_token(inviter, hint, minted_at,
  expires_at, &device_key)` signs canonical bytes for real.
  `make_*_payload_correct` helpers are PRIVATE to community_invite's test
  mod — lib.rs tests build payload struct literals (existing precedent:
  `build_open_invite_payload_round_trips_via_url`, lib.rs:54936).
- Registration list: `redeem_invite,` at lib.rs:52961 inside the primary
  `tauri::generate_handler![` (:52859). The secondary test-builder block
  (:53148) gets nothing.
- Frontend: `canSubmit = url.trim().startsWith('harmony://invite/') &&
  !pending && !irohPending` (line 33); the same format check is inlined at
  lines 74, 223, 232. Adapter idiom: `try { return await invoke<T>('cmd',
  {args}) } catch (e) { throw new Error(`cmd: ${msg}`) }`
  (connectivity-adapter.ts:118). Debounce idiom: module-scoped
  `let timer` + clear/reset + onDestroy clear (LibraryDirectoryBrowser:139);
  debounce tests use real timers + `waitFor`.
- Dialog tests mock `@tauri-apps/api/core` globally
  (RedeemInviteDialog.test.ts:7) — the adapter chain needs no extra mock.
  Unknown commands resolve `null` in old tests → `preview` stays null → no
  card → old tests unaffected (verify; if a bare `mockInvoke` call-count
  assertion breaks, scope it per-command).

---

### Task 1: Rust — `InvitePreviewDto` + `preview_invite` command + tests

**Files:**
- Modify: `src-tauri/src/owner_commands.rs:140` (visibility only)
- Modify: `src-tauri/src/lib.rs` (insert after the `RedeemInviteResultDto`
  block ~:27290; register at :52961)
- Test: inline `#[cfg(test)] mod preview_invite_tests` next to the new code

**Interfaces:**
- Consumes: `decode_invite_url`, `verify_inviter_enrollment`,
  `format_fingerprint`, `mint_test_owner`, `mint_invite_token`,
  `encode_invite_url` (via `build_open_invite_url` for open; direct for
  invite-only).
- Produces: `preview_invite` IPC returning
  `{ communityName, isInviteOnly, inviterVerified, inviterFingerprint,
  inviterDisplayName, expired }` (camelCase), errors as `String`.

- [ ] **Step 1: widen `format_fingerprint`**

In `src-tauri/src/owner_commands.rs:140`: `fn format_fingerprint` →
`pub(crate) fn format_fingerprint` (doc comment unchanged).

- [ ] **Step 2: write the failing tests**

Insert after the `RedeemInviteResultDto` struct block in `src-tauri/src/lib.rs`
(production code in Step 4 goes between the struct and this test mod; write
tests first, `cargo nextest list` fails on missing symbols = red):

```rust
#[cfg(test)]
mod preview_invite_tests {
    use super::*;
    use crate::community_invite::{
        encode_invite_url, CommunityInvitePayload, InviteEpochSnapshot,
        MaterializedCommunityState,
    };
    use crate::community_membership::{
        mint_test_owner, MembershipEventKind, SignedMembershipEvent, TestOwner,
    };
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

    /// Fixed "now" inside mint_test_owner's cert validity window.
    const NOW_MS: u64 = 1_700_000_000_000;

    fn open_payload() -> CommunityInvitePayload {
        CommunityInvitePayload {
            community_id: SpaceId([7; 16]),
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: vec![0u8; 32],
                sealed_epoch_keys: Vec::new(),
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr: OwnerAddr([0x11; 16]),
            community_name: "DoorClub".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: None,
            pre_fork_snapshot: None,
            inviter_enrollment: None,
            untargeted_decrypt_key: None,
        }
    }

    /// Invite-only payload with a REAL signature chain: token signed by the
    /// inviter's device key, inviter_enrollment = the inviter's Master cert.
    /// Decode-guard fields (bootstrap / 92-byte key / untargeted key) are
    /// shape-valid stubs — `verify_inviter_enrollment` doesn't check them.
    fn invite_only_payload(
        token_expires_at: Option<u64>,
    ) -> (CommunityInvitePayload, TestOwner) {
        let inviter = mint_test_owner(0x42);
        let token = crate::invite_mint::mint_invite_token(
            inviter.owner,
            None,
            Hlc {
                wall_ms: NOW_MS - 1_000,
                logical: 0,
                device_id: "inv-dev".into(),
            },
            token_expires_at,
            &inviter.device_key,
        )
        .expect("mint token");
        let admin_bootstrap = SignedMembershipEvent {
            id: [0u8; 16],
            community_id: SpaceId([9; 16]),
            kind: MembershipEventKind::Join,
            actor: inviter.owner,
            at: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "t".into(),
            },
            sig: [0u8; 64],
            countersig: None,
            enrollment: Some(mint_test_owner(0x6E).cert),
        };
        let payload = CommunityInvitePayload {
            community_id: SpaceId([9; 16]),
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: vec![0u8; 92],
                sealed_epoch_keys: Vec::new(),
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr: inviter.owner,
            community_name: "Cascadia Commons".into(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(token),
            admin_bootstrap: Some(admin_bootstrap),
            admin_identity_pub: Some([0u8; 64]),
            forked_from: None,
            pre_fork_snapshot: None,
            inviter_enrollment: Some(inviter.cert.clone()),
            untargeted_decrypt_key: Some([0x99; 32]),
        };
        (payload, inviter)
    }

    fn expected_fingerprint(owner: &TestOwner) -> String {
        let hex = hex::encode(owner.owner.0);
        format!("{}·{}", &hex[..4], &hex[4..8])
    }

    #[test]
    fn open_invite_previews_unverified_and_unexpired() {
        let url = build_open_invite_url(&open_payload()).expect("url");
        let dto = preview_invite_impl(&url, NOW_MS).expect("preview");
        assert_eq!(dto.community_name, "DoorClub");
        assert!(!dto.is_invite_only);
        assert!(!dto.inviter_verified, "open invites carry no token chain");
        assert_eq!(dto.inviter_fingerprint, None);
        assert_eq!(dto.inviter_display_name, None);
        assert!(!dto.expired);
    }

    #[test]
    fn invite_only_valid_chain_previews_verified_with_fingerprint() {
        let (payload, inviter) = invite_only_payload(Some(NOW_MS + 60_000));
        let url = encode_invite_url(&payload).expect("url");
        let dto = preview_invite_impl(&url, NOW_MS).expect("preview");
        assert_eq!(dto.community_name, "Cascadia Commons");
        assert!(dto.is_invite_only);
        assert!(dto.inviter_verified);
        assert_eq!(
            dto.inviter_fingerprint.as_deref(),
            Some(expected_fingerprint(&inviter).as_str())
        );
        assert_eq!(dto.inviter_display_name, None);
        assert!(!dto.expired);
    }

    #[test]
    fn tampered_token_sig_previews_unverified_not_error() {
        let (mut payload, inviter) = invite_only_payload(Some(NOW_MS + 60_000));
        payload.invite_token.as_mut().expect("token").sig[0] ^= 0x01;
        let url = encode_invite_url(&payload).expect("url");
        let dto = preview_invite_impl(&url, NOW_MS).expect("preview is not an error");
        assert!(!dto.inviter_verified, "forged sig must not verify");
        // The card still shows name + fingerprint honestly.
        assert_eq!(dto.community_name, "Cascadia Commons");
        assert_eq!(
            dto.inviter_fingerprint.as_deref(),
            Some(expected_fingerprint(&inviter).as_str())
        );
        assert!(!dto.expired);
    }

    #[test]
    fn expired_token_flags_expired() {
        let (payload, _) = invite_only_payload(Some(NOW_MS - 1));
        let url = encode_invite_url(&payload).expect("url");
        let dto = preview_invite_impl(&url, NOW_MS).expect("preview");
        assert!(dto.expired);
        assert!(dto.inviter_verified, "expiry and sig validity are independent");
    }

    #[test]
    fn boundary_now_equal_expiry_is_expired() {
        let (payload, _) = invite_only_payload(Some(NOW_MS));
        let url = encode_invite_url(&payload).expect("url");
        assert!(preview_invite_impl(&url, NOW_MS).expect("preview").expired);
    }

    #[test]
    fn payload_level_expiry_flags_expired() {
        let mut payload = open_payload();
        payload.expires_at = Some(Hlc {
            wall_ms: NOW_MS - 1,
            logical: 0,
            device_id: "t".into(),
        });
        let url = build_open_invite_url(&payload).expect("url");
        assert!(preview_invite_impl(&url, NOW_MS).expect("preview").expired);
    }

    #[test]
    fn malformed_urls_err() {
        assert!(preview_invite_impl("https://not-an-invite", NOW_MS).is_err());
        assert!(preview_invite_impl("harmony://invite/%%not-base64%%", NOW_MS).is_err());
    }

    /// Purity is by-construction — `preview_invite_impl` takes only the URL
    /// string and a clock, no NodeState/registry/paths — so "mints nothing,
    /// mutates nothing" cannot regress without a signature change. This test
    /// pins determinism: identical inputs, identical outputs.
    #[test]
    fn preview_is_deterministic() {
        let (payload, _) = invite_only_payload(Some(NOW_MS + 60_000));
        let url = encode_invite_url(&payload).expect("url");
        let a = preview_invite_impl(&url, NOW_MS).expect("a");
        let b = preview_invite_impl(&url, NOW_MS).expect("b");
        assert_eq!(a, b);
    }

    #[test]
    fn dto_serializes_camel_case() {
        let dto = InvitePreviewDto {
            community_name: "X".into(),
            is_invite_only: true,
            inviter_verified: true,
            inviter_fingerprint: Some("ab12·cd34".into()),
            inviter_display_name: None,
            expired: false,
        };
        let v = serde_json::to_value(&dto).expect("json");
        for key in [
            "communityName",
            "isInviteOnly",
            "inviterVerified",
            "inviterFingerprint",
            "inviterDisplayName",
            "expired",
        ] {
            assert!(v.get(key).is_some(), "missing camelCase key {key}");
        }
    }
}
```

- [ ] **Step 3: red** — `cd src-tauri && cargo nextest list --locked
  --features test-fixtures -E 'test(preview_invite)'` fails to compile
  (missing `InvitePreviewDto` / `preview_invite_impl`).

- [ ] **Step 4: implement**

Insert between the `RedeemInviteResultDto` block and the new test mod:

```rust
/// ZEB-650 slice 3 (spec §4.1): pure-local invite preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InvitePreviewDto {
    pub community_name: String,
    pub is_invite_only: bool,
    /// True iff the payload is invite-only AND the full inviter chain
    /// verifies (`verify_inviter_enrollment`: Master cert validity +
    /// owner binding + token sig over canonical bytes). The ✓ is scoped
    /// to inviter authorization — it does NOT authenticate the payload's
    /// name/snapshot, which is why no member/channel counts are exposed.
    pub inviter_verified: bool,
    /// `xxxx·xxxx` short fingerprint of `token.inviter`; None when the
    /// invite carries no token (open communities).
    pub inviter_fingerprint: Option<String>,
    /// Always None in this slice: local name resolution (friend
    /// nicknames / ZEB-281 profiles) is engine-async today. Field kept so
    /// the wire shape is stable when resolution lands; the frontend falls
    /// back to the fingerprint.
    pub inviter_display_name: Option<String>,
    pub expired: bool,
}

/// Decode + verify + expiry-evaluate an invite URL. Pure local
/// computation: mints nothing, joins nothing, no network, no NodeState —
/// the only inputs are the pasted URL string and the clock.
pub(crate) fn preview_invite_impl(
    url: &str,
    now_ms: u64,
) -> Result<InvitePreviewDto, String> {
    let payload =
        crate::community_invite::decode_invite_url(url).map_err(|e| e.to_string())?;
    // Spec §4.1: evaluate both the (in-practice unused) payload-level Hlc
    // expiry and the token's wall-clock-ms expiry. `now >= exp` matches
    // verify_packet_pure / orphan_dir_adoption_eligible.
    let payload_expired = payload
        .expires_at
        .as_ref()
        .is_some_and(|hlc| now_ms >= hlc.wall_ms);
    let token_expired = payload
        .invite_token
        .as_ref()
        .and_then(|t| t.expires_at)
        .is_some_and(|exp| now_ms >= exp);
    // verify_inviter_enrollment is Ok(()) unconditionally for open
    // payloads, so gate on is_invite_only — open invites carry no
    // verifiable chain and must never show the ✓.
    let inviter_verified = payload.is_invite_only
        && crate::community_invite::verify_inviter_enrollment(&payload, now_ms / 1000)
            .is_ok();
    let inviter_fingerprint = payload
        .invite_token
        .as_ref()
        .map(|t| crate::owner_commands::format_fingerprint(&t.inviter.0));
    Ok(InvitePreviewDto {
        community_name: payload.community_name,
        is_invite_only: payload.is_invite_only,
        inviter_verified,
        inviter_fingerprint,
        inviter_display_name: None,
        expired: payload_expired || token_expired,
    })
}

/// ZEB-650 slice 3: preview a pasted invite URL without redeeming it.
/// Sync on purpose — bounded CPU work (base64 + CBOR decode + one ed25519
/// verify), no awaits, no locks.
#[tauri::command]
fn preview_invite(url: String) -> Result<InvitePreviewDto, String> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    preview_invite_impl(&url, now_ms)
}
```

Register in the primary handler list (lib.rs:52961 area):

```
            redeem_invite,
            preview_invite,
```

- [ ] **Step 5: green** — `cd src-tauri && cargo nextest run --locked
  --features test-fixtures -E 'test(preview_invite)'` → 9/9 PASS.

- [ ] **Step 6: gates** — `cargo fmt --all`; `cargo clippy --locked
  --all-targets --features test-fixtures --no-deps -- -D warnings`.

- [ ] **Step 7: commit** — `feat(ZEB-650): preview_invite command — pure-local
  invite decode/verify/expiry (slice 3)`

### Task 2: Frontend — adapter + RedeemInviteDialog preview card

**Files:**
- Modify: `src/lib/types/connectivity.ts` (add `InvitePreviewDto`)
- Modify: `src/lib/connectivity-adapter.ts` (add `previewInvite`)
- Modify: `src/lib/components/RedeemInviteDialog.svelte`
- Test: `src/lib/components/__tests__/RedeemInviteDialog.test.ts`

**Interfaces:**
- Consumes: `preview_invite` IPC (Task 1); camelCase arg `{ url }`.
- Produces: `previewInvite(url: string): Promise<InvitePreviewDto>`; testids
  `redeem-preview-card`, `preview-invite-only-chip`, `preview-verified`,
  `preview-unverified`, `preview-expired`, `redeem-preview-invalid`.

- [ ] **Step 1: types + adapter**

`src/lib/types/connectivity.ts` — append:

```ts
/** ZEB-650 slice 3: pure-local invite preview (mirror of Rust InvitePreviewDto). */
export interface InvitePreviewDto {
  communityName: string;
  isInviteOnly: boolean;
  inviterVerified: boolean;
  inviterFingerprint: string | null;
  inviterDisplayName: string | null;
  expired: boolean;
}
```

`src/lib/connectivity-adapter.ts` — extend the type import with
`InvitePreviewDto` and append next to `redeemInviteIroh` (same idiom):

```ts
/**
 * ZEB-650 slice 3: pure-local preview of a pasted invite URL — decode +
 * token-signature verify + expiry. Mints nothing, joins nothing, no network.
 */
export async function previewInvite(url: string): Promise<InvitePreviewDto> {
  try {
    return await invoke<InvitePreviewDto>('preview_invite', { url });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`preview_invite: ${msg}`);
  }
}
```

- [ ] **Step 2: write the failing tests**

Append a new describe to `RedeemInviteDialog.test.ts` (own mock wiring per
existing file conventions; real timers + `waitFor` per the
LibraryDirectoryBrowser debounce-test idiom):

```ts
describe('RedeemInviteDialog invite preview (ZEB-650 slice 3)', () => {
  const VALID_URL = 'harmony://invite/v1?ci=abc';
  const PREVIEW: InvitePreviewDto = {
    communityName: 'Cascadia Commons',
    isInviteOnly: true,
    inviterVerified: true,
    inviterFingerprint: 'ab12·cd34',
    inviterDisplayName: null,
    expired: false,
  };

  function mockPreview(dto: Partial<InvitePreviewDto> | Error) {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'preview_invite') {
        return dto instanceof Error
          ? Promise.reject(dto)
          : Promise.resolve({ ...PREVIEW, ...dto });
      }
      return Promise.resolve(null);
    });
  }

  function typeUrl(value: string) {
    const input = screen.getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    return fireEvent.input(input, { target: { value } });
  }

  it('debounces: no preview IPC before settle, exactly one after', async () => {
    mockPreview({});
    render(RedeemInviteDialog, { props: { onSubmit: vi.fn(), onCancel: vi.fn() } });
    await typeUrl('harmony://invite/v1?ci=a');
    await typeUrl('harmony://invite/v1?ci=ab');
    await typeUrl(VALID_URL);
    expect(mockInvoke).not.toHaveBeenCalledWith('preview_invite', expect.anything());
    await waitFor(() => {
      expect(mockInvoke).toHaveBeenCalledWith('preview_invite', { url: VALID_URL });
    });
    const previewCalls = mockInvoke.mock.calls.filter(([c]) => c === 'preview_invite');
    expect(previewCalls.length).toBe(1);
  });

  it('renders the card: name, invite-only chip, verified line with fingerprint fallback', async () => {
    mockPreview({});
    render(RedeemInviteDialog, { props: { onSubmit: vi.fn(), onCancel: vi.fn() } });
    await typeUrl(VALID_URL);
    const card = await screen.findByTestId('redeem-preview-card');
    expect(card.textContent).toContain('Cascadia Commons');
    expect(screen.getByTestId('preview-invite-only-chip')).toBeTruthy();
    expect(screen.getByTestId('preview-verified').textContent).toContain('ab12·cd34');
    expect(screen.getByTestId('preview-verified').textContent).toMatch(/invite signature verified/i);
  });

  it('prefers the display name over the fingerprint when present', async () => {
    mockPreview({ inviterDisplayName: 'Mara Okafor' });
    render(RedeemInviteDialog, { props: { onSubmit: vi.fn(), onCancel: vi.fn() } });
    await typeUrl(VALID_URL);
    const line = await screen.findByTestId('preview-verified');
    expect(line.textContent).toContain('Mara Okafor');
    expect(line.textContent).not.toContain('ab12·cd34');
  });

  it('unverified invite shows the neutral line, not an error', async () => {
    mockPreview({ inviterVerified: false, isInviteOnly: false });
    render(RedeemInviteDialog, { props: { onSubmit: vi.fn(), onCancel: vi.fn() } });
    await typeUrl(VALID_URL);
    const line = await screen.findByTestId('preview-unverified');
    expect(line.textContent).toMatch(/signature not verifiable/i);
    expect(screen.queryByTestId('preview-verified')).toBeNull();
  });

  it('expired invite shows the notice and disables Redeem', async () => {
    mockPreview({ expired: true });
    render(RedeemInviteDialog, { props: { onSubmit: vi.fn(), onCancel: vi.fn() } });
    await typeUrl(VALID_URL);
    await screen.findByTestId('preview-expired');
    const redeem = screen.getByTestId('iroh-redeem-btn') as HTMLButtonElement;
    expect(redeem.disabled).toBe(true);
  });

  it('preview failure shows only the invalid-link line and keeps the dialog usable', async () => {
    mockPreview(new Error('decode: bad'));
    render(RedeemInviteDialog, { props: { onSubmit: vi.fn(), onCancel: vi.fn() } });
    await typeUrl(VALID_URL);
    const invalid = await screen.findByTestId('redeem-preview-invalid');
    expect(invalid.textContent).toMatch(/looks invalid/i);
    expect(screen.queryByTestId('redeem-preview-card')).toBeNull();
    const redeem = screen.getByTestId('iroh-redeem-btn') as HTMLButtonElement;
    expect(redeem.disabled).toBe(false);
    const cancel = screen.getByText('Cancel') as HTMLButtonElement;
    expect(cancel.disabled).toBe(false);
  });

  it('clearing to an invalid format removes the card', async () => {
    mockPreview({});
    render(RedeemInviteDialog, { props: { onSubmit: vi.fn(), onCancel: vi.fn() } });
    await typeUrl(VALID_URL);
    await screen.findByTestId('redeem-preview-card');
    await typeUrl('nonsense');
    expect(screen.queryByTestId('redeem-preview-card')).toBeNull();
  });

  it('renders no member or channel counts on the preview card', async () => {
    mockPreview({});
    render(RedeemInviteDialog, { props: { onSubmit: vi.fn(), onCancel: vi.fn() } });
    await typeUrl(VALID_URL);
    await screen.findByTestId('redeem-preview-card');
    expect(screen.queryByText(/\d+\s+members/i)).toBeNull();
    expect(screen.queryByText(/\d+\s+channels/i)).toBeNull();
  });
});
```

(Adjust to the file's actual render-helper conventions — it may destructure
`getByTestId` from `render` rather than use `screen`; match whichever the
existing tests use. Import `InvitePreviewDto` from `../../types/connectivity`
and `waitFor` from `@testing-library/svelte` if not already imported.)

- [ ] **Step 3: red** — `npx vitest run src/lib/components/__tests__/RedeemInviteDialog.test.ts`
  → new describe fails (no card, no IPC).

- [ ] **Step 4: implement the dialog changes**

Script block:

```ts
// extend the existing adapter import
import {
  redeemInviteIroh,
  onResolutionProgress,
  previewInvite,
} from '../connectivity-adapter';
import type { RedemptionStage, InvitePreviewDto } from '../types/connectivity';
```

State (next to the iroh-path state):

```ts
// ── ZEB-650 slice 3: debounced pure-local invite preview ────────────────
let preview = $state<InvitePreviewDto | null>(null);
let previewInvalid = $state(false);
let previewExpired = $derived(preview?.expired === true);
let previewTimer: ReturnType<typeof setTimeout> | null = null;
/** Monotonic guard: a resolution whose seq no longer matches is stale
 *  (URL changed or dialog torn down mid-flight) and must be discarded. */
let previewSeq = 0;
const PREVIEW_DEBOUNCE_MS = 300;

$effect(() => {
  const trimmed = url.trim();
  if (previewTimer !== null) clearTimeout(previewTimer);
  previewTimer = null;
  previewSeq += 1;
  const seq = previewSeq;
  if (!trimmed.startsWith('harmony://invite/')) {
    preview = null;
    previewInvalid = false;
    return;
  }
  previewTimer = setTimeout(() => {
    previewTimer = null;
    void (async () => {
      try {
        const dto = await previewInvite(trimmed);
        if (seq !== previewSeq) return;
        preview = dto ?? null;
        previewInvalid = false;
      } catch {
        if (seq !== previewSeq) return;
        preview = null;
        previewInvalid = true;
      }
    })();
  }, PREVIEW_DEBOUNCE_MS);
});
```

`canSubmit` (line 33) gains the expired gate:

```ts
let canSubmit = $derived(
  url.trim().startsWith('harmony://invite/') && !pending && !irohPending && !previewExpired,
);
```

`handleIrohRedeem` guard (line 74) — append the same gate:

```ts
if (!trimmed.startsWith('harmony://invite/') || irohPending || pending || previewExpired) return;
```

Both buttons' inline `disabled` expressions (lines 223, 232) — append
`|| previewExpired`. `onDestroy` — add after the joinedDismissTimer block:

```ts
if (previewTimer !== null) {
  clearTimeout(previewTimer);
  previewTimer = null;
}
previewSeq += 1; // discard any in-flight preview resolution
```

Template — insert between the `{#if pending}` block and `.dialog-actions`
("card above the actions", spec §4.2):

```svelte
{#if preview !== null}
  <div class="preview-card" data-testid="redeem-preview-card">
    <div class="preview-headline">
      <span class="preview-name">{preview.communityName}</span>
      {#if preview.isInviteOnly}
        <span class="invite-only-chip" data-testid="preview-invite-only-chip">🔒 Invite-only</span>
      {/if}
    </div>
    {#if preview.inviterVerified}
      <div class="preview-verified" data-testid="preview-verified">
        ✓ invite signature verified{#if preview.inviterDisplayName ?? preview.inviterFingerprint}&nbsp;— from {preview.inviterDisplayName ?? preview.inviterFingerprint}{/if}
      </div>
    {:else}
      <div class="preview-unverified" data-testid="preview-unverified">signature not verifiable</div>
    {/if}
    {#if preview.expired}
      <div class="preview-expired" data-testid="preview-expired">This invite has expired.</div>
    {/if}
  </div>
{:else if previewInvalid}
  <div class="preview-card" data-testid="redeem-preview-invalid">
    <span class="preview-unverified">This invite link looks invalid</span>
  </div>
{/if}
```

Styles (token-only; card mirrors `.error-banner` anatomy, chip mirrors the
TrustBadge pill, verified line mirrors IdentityChip's mono status line):

```css
.preview-card {
  background: var(--bg-tertiary);
  border: 1px solid var(--border-default);
  border-radius: 6px;
  padding: 10px 12px;
  margin-bottom: 12px;
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.preview-headline {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
.preview-name {
  color: var(--text-primary);
  font-weight: 600;
  font-size: 0.95rem;
}
.invite-only-chip {
  padding: 2px 10px;
  border-radius: 20px;
  font-size: 11px;
  font-weight: 600;
  color: var(--text-secondary);
  background: color-mix(in srgb, currentColor 16%, transparent);
  white-space: nowrap;
}
.preview-verified {
  color: var(--success);
  font-family: var(--font-mono);
  font-size: 0.75rem;
}
.preview-unverified {
  color: var(--text-secondary);
  font-size: 0.8rem;
}
.preview-expired {
  color: var(--danger-muted);
  font-size: 0.8rem;
}
```

- [ ] **Step 5: green** — `npx vitest run
  src/lib/components/__tests__/RedeemInviteDialog.test.ts` → all pass,
  including every pre-existing test (watch for bare `mockInvoke` call-count
  assertions now also seeing `preview_invite` calls; scope per-command if any
  break).

- [ ] **Step 6: gates** — `npx tsc --noEmit && npx vitest run`.

- [ ] **Step 7: commit** — `feat(ZEB-650): RedeemInviteDialog debounced invite
  preview card (slice 3)`

### Task 3: Final gates + PR

- [ ] `npx tsc --noEmit && npx vitest run` (full)
- [ ] `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked
  --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] Full Rust sweep: `cargo nextest run --locked --workspace --all-targets
  --features test-fixtures` (background with a supervision net)
- [ ] Open PR (`Part of ZEB-650`), fire `@coderabbitai review` ONCE, update
  Linear, converge bot/CI feedback.

## Self-review notes

- Spec coverage: §4.1 command ✓ (DTO field-for-field; `inviter_display_name`
  deviation documented as a Global Constraint), §4.2 card ✓ (debounce, name,
  verified/neutral lines, chip, expired-disables-submit, invalid copy,
  cancel-never-blocked), §5 slice-3 tests ✓ (round-trip, tampered sig,
  expired, purity-by-construction + determinism pin; TS debounce/fields/
  expired/invalid).
- `verify_inviter_enrollment`'s open-payload `Ok(())` short-circuit is the
  one sharp edge — handled by the `is_invite_only &&` gate and pinned by
  `open_invite_previews_unverified_and_unexpired`.
- Old-test compatibility: unknown-command mocks resolve `null`; the effect
  assigns `preview = dto ?? null`, so old tests render no card.
