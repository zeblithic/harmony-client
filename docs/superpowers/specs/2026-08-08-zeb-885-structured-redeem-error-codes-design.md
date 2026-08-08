# ZEB-885 — Structured redeem-invite error codes (design)

**Ticket:** ZEB-885 (Low). Surfaced by Qodo during PR #634 (ZEB-881/ZEB-879a).
**Branch:** `zeblith/zeb-885-structured-redeem-error-codes`
**Scope decision (Jake, 2026-08-08):** widest cut — unify **both** redeem paths under one structured error contract.

## Goal

Make the invite-redeem UI route its error copy off a **structured, typed error code** returned by the backend, instead of regex-matching raw English prose. The raw message stays for display/bug-reports; the *routing* decision keys off the code.

## Problem (reframed — it's a latent bug, not just brittleness)

The exploration turned up that the current mapping is **largely dead against real output**, so this ticket also repairs a live UX defect:

- `redeem_invite` returns `Result<RedeemInviteResultDto, String>` and surfaces `RedeemBootstrapVerifyError` via `.to_string()` (Display) at `lib.rs:40791`. The Display strings are *prose* — e.g. `BootstrapActorMismatch` renders `"redeem_invite: admin_bootstrap.actor != admin_addr"`, which contains **no** `BootstrapActorMismatch` substring. So the frontend's `/BootstrapActorMismatch/i` (and every `Bootstrap*` pattern) **never matches** → all bootstrap failures fall through to the generic `network_failure` fallback ("Couldn't reach the network") instead of their specific "ask the inviter to regenerate" copy.
- Three frontend tags (`bootstrap_invalid_pubkey`, `bootstrap_address_mismatch`, `bootstrap_insert_failed`) have **no backend producer at all** (the pubkey/address checks were removed in ZEB-339). `already_member` and the `malformed_url` patterns are likewise dead. `relays_warming_up` (`no relays available`) only flows through the *iroh* path's separate `irohError` banner, never through `mapRedeemInviteError`.
- The frontend mapper tests pass only because they feed **synthetic** inputs (fabricated strings containing the CamelCase names) that don't resemble real backend output — false confidence.
- Meanwhile `RedeemBootstrapVerifyError::reason_tag()` (`community_invite.rs:1481`) **already returns exactly the frontend's tag strings** — the backend computes the right code and throws it away.

## Two paths, two starting shapes

**LAN path** — `redeem_invite` IPC (`lib.rs:41517`) → `redeem_invite_impl` (`41528`, shared IPC/RPC seam) → `redeem_invite_inner_with_overrides` (`40382`). Returns `Result<Dto, String>`. Consumed in `App.svelte` (`~4711` catch → `redeemError: string` → `mapRedeemInviteError(raw)` → banner). **Pure prose-matching. This is where the bug lives.**

**iroh path** — `connectivity_redeem_invite_iroh` (`lib.rs:61237`) → `_impl` (`61257`). Returns `Result<RedemptionOutcome, String>` where `RedemptionOutcome { status: String, community_id: Option<String> }` and `status` is **already a code** (`joined` / `inviter_unreachable` / `join_failed` / `missing_admin_identity_pub` / `pkarr_resolved_no_handshake`). Consumed in `RedeemInviteDialog.svelte::handleIrohRedeem` (`129`), which **already switches on `status`** and renders context-rich per-status copy (community-id hint, fallback-button logic, joined-dismiss timer). The only un-coded surface here is the hard-error `catch` branch: `irohError = e instanceof Error ? e.message : String(e)` (`206`) — raw prose shown, never matched.

**Consequence for "fold in the iroh path":** the iroh Ok-side is already code-driven and its copy is *better* than the LAN table — do **not** retype its ~25 `status: "…"` sites into an enum. Instead: (a) draw the iroh status vocabulary and the LAN codes from **one shared code list** so the taxonomy can't drift, and (b) give the iroh hard-error branch a real code so it stops dumping raw prose.

## The taxonomy — `RedeemInviteErrorCode`

One Rust enum, `#[derive(Serialize)] #[serde(rename_all = "snake_case")]`, serialized as stable strings. Grouped by the user-facing remediation:

| Code | Source | Meaning / copy family |
|---|---|---|
| `bootstrap_missing` | `RedeemBootstrapVerifyError::BootstrapMissing` (reason_tag) | Invite link incomplete — regenerate |
| `bootstrap_actor_mismatch` | `::BootstrapActorMismatch` | Invite link malformed — regenerate |
| `bootstrap_community_mismatch` | `::BootstrapCommunityMismatch` | Link points to a different community |
| `bootstrap_signature_invalid` | `::BootstrapSignatureInvalid` | Signature invalid — regenerate via another channel |
| `bootstrap_kind_invalid` | `::BootstrapKindInvalid` | Wrong event type — regenerate |
| `invite_url_malformed` | `InviteUrlError` (all variants; `lib.rs:40428`) | URL isn't a valid Harmony invite |
| `inviter_enrollment_invalid` | `CommunityInviteVerifyError` (`lib.rs:40438`) | Inviter's enrollment failed to verify |
| `invite_token_missing` | `lib.rs:40776` | Invite-only payload missing token |
| `missing_admin_identity_pub` | iroh status | Open invite; no admin binding |
| `inviter_unreachable` | iroh status; pkarr/dial/handshake fail | Inviter offline — retry later |
| `relays_warming_up` | pkarr transient (`resolve_error_is_transient_unreachable`, `lib.rs:67538`) | Discovery relays cold — retry shortly |
| `node_not_ready` | NodeState snapshot guards (owner_not_loaded, missing handles; `lib.rs:41552`, iroh `61313`) | Identity still loading — retry |
| `generation_changed` | generation fence (`lib.rs:41609`) | Node reconfigured mid-redeem — retry |
| `engine_insert_failed` | LAN engine insert (`lib.rs:40820`) | Local insert failed — transient, retry |
| `join_failed` | iroh status; inviter reached, local insert failed | Reached inviter, local join failed — restart & retry |
| `internal` | poisoned, oneshot closed, other ad-hoc internal (`lib.rs:41104`) | Unexpected internal error |
| `unknown` | frontend fallback for any unmapped code | Generic "couldn't complete redeem" |

`reason_tag()` on `RedeemBootstrapVerifyError` already yields the first five strings verbatim — the `From<RedeemBootstrapVerifyError>` impl is a trivial variant match, and we keep `reason_tag()` as the single source for those five.

## Backend design

1. **New types** (in `community_invite.rs` next to the existing error enums, or a small `redeem_error.rs`):
   ```rust
   #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
   #[serde(rename_all = "snake_case")]
   pub enum RedeemInviteErrorCode { /* the 17 variants above */ }

   #[derive(Debug, Clone, serde::Serialize)]
   pub struct RedeemInviteError { pub code: RedeemInviteErrorCode, pub message: String }
   ```
   - `impl std::fmt::Display for RedeemInviteError` → writes `message` (so existing `format!("{e}")`/logging stays readable).
   - `impl std::error::Error`.
   - `RedeemInviteError::new(code, impl Into<String>)` + convenience constructors.
   - `From<RedeemBootstrapVerifyError>` (match → code, `message = err.to_string()`), `From<InviteUrlError>` (→ `invite_url_malformed`), `From<CommunityInviteVerifyError>` (→ `inviter_enrollment_invalid`).
   - A `From<String>`/`From<&str>` → `{ code: internal, message }` catch-all so ad-hoc `?` sites still compile; sites that deserve a *specific* code get an explicit constructor instead of relying on the catch-all.

2. **LAN seam** — change `redeem_invite_inner_with_overrides` (and `redeem_invite_inner`, `redeem_invite_impl`, `redeem_invite`) return type `String → RedeemInviteError`. Each error site assigns a code (bootstrap/url/enrollment via `?`+`From`; snapshot guards → `node_not_ready`; generation fence → `generation_changed`; engine insert → `engine_insert_failed`; oneshot closed / poisoned → `internal`).

3. **iroh seam** — `connectivity_redeem_invite_iroh{,_impl}` hard-error `String → RedeemInviteError` (poisoned/internal sites → `internal`; the not-fully-booted `Ok(inviter_unreachable)` path is unchanged). The Ok-side `RedemptionOutcome.status` **string constants are re-sourced** from `RedeemInviteErrorCode::….serialize`-equivalent `&'static str` values (a `const`/`as_str()` on the enum) so `status` and `code` share one vocabulary and can't drift. `RedemptionOutcome` itself stays a `String`-status struct (no 25-site retype).

4. **RPC/serve surface (integration point to verify):** `redeem_invite_impl` is the shared IPC/**RPC** seam (ZEB-445). Changing its error type changes what the headless `serve`/`api` WS surface serializes for a redeem failure (today a JSON string; after, a `{code,message}` object). This is an *improvement* but a wire change for our own `api` CLI. Implementation must grep the `api`/serve redeem consumers and update any that parse the error as a bare string. (Same for `connectivity_redeem_invite_iroh_impl` via its serve seam.)

## Frontend design

1. **`src/lib/redeem-invite-errors.ts`** — delete `VARIANT_PATTERNS`. Replace with a `switch (code)` copy table:
   ```ts
   export type RedeemInviteErrorCode = 'bootstrap_missing' | … | 'unknown';
   export interface RedeemInviteError { code: RedeemInviteErrorCode; message: string }
   export interface RedeemInviteUserError { summary: string; hint: string; code: RedeemInviteErrorCode; raw: string }
   export function redeemInviteCopy(code: RedeemInviteErrorCode): { summary: string; hint: string } { /* switch */ }
   export function mapRedeemInviteError(err: RedeemInviteError): RedeemInviteUserError;
   ```
   Keep `tag` in the UI disclosure but rename its source to `code` (the disclosure already shows "Telemetry tag: {tag}" — becomes the code). Unknown/absent code → the `unknown` fallback copy.

2. **`App.svelte`** — the `redeem_invite` rejection is now a serialized `{code, message}` object, not a string. Update the catch to extract it structurally, tolerating the test/`Error` shape:
   ```ts
   catch (e) {
     redeemError = toRedeemInviteError(e); // { code, message }: object → as-is; Error/string → { code:'unknown', message }
   }
   ```
   `redeemError` becomes `RedeemInviteError | null`; the dialog `error` prop type changes `string → RedeemInviteError | null`.

3. **`RedeemInviteDialog.svelte`** — `mapped = error ? mapRedeemInviteError(error) : null`. The iroh `handleIrohRedeem` keeps its lifecycle branching (joined-dismiss, fallback button, community hint) but its **failure copy** is sourced from `redeemInviteCopy(code)` for the shared codes (map each failure `status` to its code, and the `catch` error already carries a code). Richer per-status context (community-id hint, "restart & retry") is layered on top of the shared base copy rather than hardcoded prose. The two banners stay distinct in the DOM (LAN `mapped` vs `iroh-error-banner`) but draw from one copy source.

4. **`community-service.ts`** — `redeemInvite`/`redeemInviteIroh` let the structured rejection propagate unchanged (they already don't transform it); update return/throw typing only.

## Testing

**Backend (Rust):**
- Unit-test the `From` impls: each `RedeemBootstrapVerifyError` variant → its code (pins the prose↔code contract that has **zero** coverage today); `InviteUrlError`/`CommunityInviteVerifyError` → their codes.
- Boundary tests on `redeem_invite_inner_with_overrides` (extend the existing `#[cfg(test)] mod redeem_invite_inner_tests`, `lib.rs:41973`): drive each reachable error site and assert `err.code == Expected` (not the message). Cover node-not-ready, generation-changed, engine-insert, token-missing, internal.
- `RedeemInviteErrorCode` serde round-trips to the expected snake_case string (pins the wire contract the frontend depends on).

**Frontend (vitest):**
- Rewrite `redeem-invite-errors.test.ts`: feed **codes** (not synthetic prose); assert each code → its summary/hint; unknown code → fallback; `raw` passthrough.
- `App.svelte`/dialog: rejection with `{code:'bootstrap_signature_invalid', message}` renders the specific summary + disclosure shows the code; a legacy `Error`/string rejection degrades to `unknown` copy (back-compat for any un-migrated throw).
- Keep/adjust `RedeemInviteDialog.test.ts` iroh-banner cases to assert the code-sourced copy.

## Migration / compatibility

- **Tauri IPC is internal** (frontend+backend ship as one binary) — no cross-version skew on the GUI path; both ends change in this PR.
- **Serve/api RPC is a real wire surface** for our fleet tooling — the error shape change is structured-strictly-better but must land with matching `api`-CLI updates in the same PR (see backend step 4).
- The `unknown` fallback guarantees forward-compat: a code the frontend hasn't learned yet renders generic copy, never a crash.

## Non-goals

- Localization itself (the codes *enable* it; no i18n framework added here).
- Retyping `RedemptionOutcome.status` into an enum (deliberately kept a String to bound the diff; vocabulary is shared via `RedeemInviteErrorCode::as_str()`).
- Reworking the iroh lifecycle UX (joined-dismiss, fallback button) beyond re-sourcing failure copy.

## Follow-ups (file, don't fold)

- If, after this lands, we want `RedemptionOutcome.status` fully typed (enum on the wire), that's a separate mechanical ticket.
