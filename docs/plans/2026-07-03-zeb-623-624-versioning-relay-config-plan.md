# ZEB-623 + ZEB-624 Bundle Implementation Plan (S7 protocol versioning + S8 iroh relay config)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Codify harmony's wire-protocol versioning policy with a working exemplar (hello/capabilities frame on a new `harmony/tunnel/v2` ALPN generation, N/N-1 constants, loud Network-Health incompatibility signal), and surface the iroh relay list as persisted runtime-editable config mirroring the pkarr relay pattern.

**Architecture:** S7 adds a `protocol_versioning` module (hello frame codec + version constants + per-peer compat registry), a v2 tunnel generation negotiated dialer-side with v1 fallback, and health plumbing for incompatible peers. S8 extends the connectivity-settings file (renamed `ConnectivitySettings`) with an `iroh_relays` list applied at endpoint build via `RelayMode::custom` and live via `insert_relay`/`remove_relay` diffs, plus 5 IPC verbs and a Settings-panel editor mirroring the pkarr relay manager.

**Tech Stack:** Rust (tokio, iroh 1.0.1, ciborium CBOR, serde), Tauri IPC, Svelte 5 (runes), vitest.

## Global Constraints

- **One PR, one branch** (`zeb-623-624-versioning-relay-config`), one commit per task (rename task = 2 commits). ZEB-623 + ZEB-624 both close via this PR.
- **Gates per task:** `cargo fmt --all` then `cargo fmt --all -- --check`; `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings` (use `--all-targets` when the task adds integration-test code); scoped `cargo nextest run --locked -p harmony-app --features test-fixtures -E '<scope>'`. Full `--all-targets` sweep is Task 7 ONLY. All cargo commands from `src-tauri/`. **ONE cargo invocation at a time** (a fleet node runs from this target dir).
- **Hermetic iroh tests** pay the iroh-endpoint throttle (nextest `iroh-endpoint` group, max 4 threads) and the ~10-30s first-bind global init. Never assert wall-clock budgets equal to timeouts; reuse the existing `warm_up_iroh_global_init()` pattern where a timeout is asserted.
- **Do not break** `default_relay_map_is_stable_non_canary` (`iroh_endpoint.rs:452`) — production default relays remain the n0 stable preset when no custom list is configured.
- **`schema_version` stays 4** in `NetworkHealthSnapshot` — all new health fields are additive with `#[serde(default)]` (that IS the payload rule this slice codifies).
- **Wire DTO keys are camelCase** (`#[serde(rename_all = "camelCase")]`); frontend asserts/reads camelCase keys.
- **No `capabilities/default.json` edits** — app IPC commands are not capability-gated here; register new commands in BOTH `generate_handler!` blocks (production `lib.rs:~51586` region and the `#[cfg(any(test, feature = "test-fixtures"))]` `add_dm_ipc_handlers` block `lib.rs:~51682`).
- **New framing uses big-endian length prefixes** via `tunnel_task`'s `write_length_prefixed`/`read_length_prefixed` (the `iroh_framing` BE convention).
- **Additive payload rule (codified by this slice, obeyed by this slice):** new serde fields on persisted/wire structs take `#[serde(default)]` (or `#[serde(default = "...")]`); never add `deny_unknown_fields`.
- Naming: plain "Greptile" in any prose; never the @-form.

## Deliberate scope notes (deviations an implementer/reviewer must not "fix")

1. **The hello frame ships inside a NEW `harmony/tunnel/v2` ALPN generation, not injected into `/v1`.** The ticket's "no new ALPN, no extra dial" describes steady-state feature evolution AFTER a protocol carries a hello frame. Bootstrapping the hello itself changes frame order — an old (N-1) responder would feed our hello bytes into `TunnelSession::new_responder` and fail the PQ handshake. That is exactly the wire-incompatible-generation case the policy reserves ALPN bumps for. Shipping v2 with v1 kept registered exercises BOTH policy mechanisms once: generation bump + deprecation window (mechanism 1) and hello frame (mechanism 2).
2. **Dialer falls back v2→v1 on ANY connect error, unconditionally.** Classifying `no_application_protocol` TLS alerts vs. unreachable-peer errors is brittle across iroh versions. Both dial attempts stay inside the existing 15s `HANDSHAKE_TIMEOUT` envelope, and tunnel dials already have the deposit-durability fallback behind them. The decision record explicitly accepts "fallback costs an extra dial".
3. **The hello exchange is pipelined — zero added round trips.** Initiator writes hello + `TunnelInit` back-to-back; responder reads hello, validates, reads init, writes its own hello + `TunnelAccept` back-to-back. Nobody waits on a hello-only round trip.
4. **`PkarrSettings` → `ConnectivitySettings` rename** is in scope (Task 4, own commit): the struct already owns friend-accept/presence toggles and now gains iroh relays; the pkarr name is actively misleading. Mechanical, compiler-verified.
5. **Empty persisted `iroh_relays` = "follow iroh preset defaults"** (not an explicit URL list). Unlike pkarr (which persists its 3 defaults), persisting iroh's default URLs would freeze them across iroh upgrades. `reset_iroh_relays` therefore writes `[]`.
6. **No headless (serve/WS rpc.rs) verbs for relay editing** — the pkarr relay verbs are GUI-IPC-only today; mirror that. Headless parity is a separate ticket if wanted.
7. **v1-fallback peers are NOT flagged incompatible** — v1 is the supported N-1 generation during the deprecation window. Only a hello `protocol_version < MIN_SUPPORTED` marks a peer incompatible.

## File structure

| File | Role |
|---|---|
| `src-tauri/src/protocol_versioning.rs` (NEW) | TunnelHello codec, version/generation constants, `ProtocolCompatRegistry` |
| `docs/specs/2026-07-03-zeb-623-protocol-versioning-design.md` (NEW) | The policy spec (deliverable #1 of ZEB-623) |
| `docs/specs/2026-07-03-zeb-624-iroh-relay-config-design.md` (NEW) | Short S8 design note incl. deterministic-overlap pattern |
| `src-tauri/src/iroh_endpoint.rs` | `HARMONY_TUNNEL_V2` constant; bind list; `new_with_secret_and_relays`; `apply_relay_urls`; `relay_map_urls` |
| `src-tauri/src/tunnel_task.rs` | v2/v1 negotiation in `initiator_handshake` + `responder_handshake`, hello exchange |
| `src-tauri/src/tunnel_manager.rs` | holds `Arc<ProtocolCompatRegistry>`, `note_peer_incompatible` passthrough |
| `src-tauri/src/zenoh_iroh_transport.rs` | accept-loop dispatch branch for `HARMONY_TUNNEL_V2` |
| `src-tauri/src/network_health.rs` | `protocol_incompat_reason` on `ResolverPeerRecord` + `PeerHealth`, registry join |
| `src/lib/types/network-health.ts` | `protocolIncompatReason` on `PeerHealth` |
| `src/lib/components/NetworkHealthView.svelte` | incompatible-peer badge + title |
| `src-tauri/src/connectivity_settings.rs` (RENAME of `pkarr_settings.rs`) | `ConnectivitySettings` + `iroh_relays` + validators + atomic save |
| `src-tauri/src/lib.rs` | rename fallout; iroh-relay IPC verbs + apply helper + boot wiring + reconcile; registry construction |
| `src/lib/connectivity-adapter.ts` | 5 iroh-relay adapter fns |
| `src/lib/components/IrohRelaySettings.svelte` (NEW) | relay editor UI |
| `src/lib/components/SettingsPanel.svelte` | mount point |

---

### Task 1: `protocol_versioning` module + versioning policy spec + v2 ALPN registration

**Files:**
- Create: `src-tauri/src/protocol_versioning.rs`
- Create: `docs/specs/2026-07-03-zeb-623-protocol-versioning-design.md`
- Modify: `src-tauri/src/iroh_endpoint.rs:94` (add `HARMONY_TUNNEL_V2` to the `alpn` module), `:134-144` (bind list)
- Modify: `src-tauri/src/lib.rs` (add `pub mod protocol_versioning;` near the other module decls)

**Interfaces (Produces):**
```rust
pub const TUNNEL_ALPN_GENERATION: u16 = 2;
pub const MIN_SUPPORTED_TUNNEL_ALPN_GENERATION: u16 = 1;
pub const TUNNEL_PROTOCOL_VERSION: u16 = 1;
pub const MIN_SUPPORTED_TUNNEL_PROTOCOL_VERSION: u16 = 1;
pub const TUNNEL_HELLO_MAX: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TunnelHello {
    pub protocol_version: u16,
    #[serde(default)]
    pub capabilities: u64,
}
impl TunnelHello { pub fn current() -> Self /* {TUNNEL_PROTOCOL_VERSION, 0} */ }

pub fn encode_hello(h: &TunnelHello) -> Result<Vec<u8>, String>;   // ciborium::into_writer
pub fn decode_hello(bytes: &[u8]) -> Result<TunnelHello, String>;  // ciborium::from_reader
/// Err(reason string) when hello.protocol_version < MIN_SUPPORTED_TUNNEL_PROTOCOL_VERSION.
pub fn check_hello_compatible(h: &TunnelHello) -> Result<(), String>;

#[derive(Default)]
pub struct ProtocolCompatRegistry { /* Mutex<HashMap<[u8;32], String>> */ }
impl ProtocolCompatRegistry {
    pub fn note_incompatible(&self, node_id: [u8; 32], reason: String); // tracing::warn! here (the LOUD log)
    pub fn note_compatible(&self, node_id: [u8; 32]);                   // clears any entry
    pub fn incompat_reason(&self, node_id: &[u8; 32]) -> Option<String>;
}
```

- [ ] **Step 1: Write failing unit tests** in `protocol_versioning.rs` `#[cfg(test)]`:

```rust
#[test]
fn hello_roundtrips_via_cbor() {
    let h = TunnelHello { protocol_version: 1, capabilities: 0b101 };
    let bytes = encode_hello(&h).unwrap();
    assert!(bytes.len() < TUNNEL_HELLO_MAX);
    assert_eq!(decode_hello(&bytes).unwrap(), h);
}

#[test]
fn hello_decode_tolerates_unknown_fields_and_missing_capabilities() {
    // Future-proofing: a v-next hello with extra fields decodes; capabilities defaults.
    let mut extended = std::collections::BTreeMap::new();
    extended.insert("protocol_version".to_string(), ciborium::Value::Integer(7.into()));
    extended.insert("some_future_field".to_string(), ciborium::Value::Text("x".into()));
    let mut bytes = Vec::new();
    ciborium::into_writer(&ciborium::Value::Map(
        extended.into_iter().map(|(k, v)| (ciborium::Value::Text(k), v)).collect(),
    ), &mut bytes).unwrap();
    let h = decode_hello(&bytes).unwrap();
    assert_eq!(h.protocol_version, 7);
    assert_eq!(h.capabilities, 0);
}

#[test]
fn check_hello_rejects_below_min_supported() {
    assert!(check_hello_compatible(&TunnelHello { protocol_version: 0, capabilities: 0 }).is_err());
    assert!(check_hello_compatible(&TunnelHello::current()).is_ok());
    // A NEWER version than ours is compatible (unknown capability bits ignored).
    assert!(check_hello_compatible(&TunnelHello { protocol_version: u16::MAX, capabilities: u64::MAX }).is_ok());
}

#[test]
fn registry_note_and_clear() {
    let r = ProtocolCompatRegistry::default();
    let id = [7u8; 32];
    assert_eq!(r.incompat_reason(&id), None);
    r.note_incompatible(id, "tunnel hello v0 < min 1".into());
    assert_eq!(r.incompat_reason(&id).as_deref(), Some("tunnel hello v0 < min 1"));
    r.note_compatible(id);
    assert_eq!(r.incompat_reason(&id), None);
}

#[test]
fn tunnel_alpn_generations_cover_n_minus_1() {
    // N/N-1 pin: both generations remain registered while MIN < CURRENT.
    assert_eq!(crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V1, b"harmony/tunnel/v1");
    assert_eq!(crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V2, b"harmony/tunnel/v2");
    assert!(MIN_SUPPORTED_TUNNEL_ALPN_GENERATION <= TUNNEL_ALPN_GENERATION);
}
```

- [ ] **Step 2: Run to verify failure.** `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(protocol_versioning)'` — expect compile failure (module absent).
- [ ] **Step 3: Implement the module** per the Produces block. `decode_hello` enforces `bytes.len() <= TUNNEL_HELLO_MAX` before parsing. Module doc: one paragraph pointing at the spec doc. Add `HARMONY_TUNNEL_V2` to `iroh_endpoint.rs` `alpn` module with a doc comment naming ZEB-623 + the deprecation-window rule, and append `alpn::HARMONY_TUNNEL_V2.to_vec()` to the bind list at `iroh_endpoint.rs:134-144`.
- [ ] **Step 4: Write the spec doc** `docs/specs/2026-07-03-zeb-623-protocol-versioning-design.md`, sections: (1) Two mechanisms, two rates of change (ALPN generation bump = wire-incompatible only, with acceptor deprecation window + dialer newest-first fallback; hello frame = feature evolution, unknown capability bits ignored); (2) N/N-1 fleet rule + `MIN_SUPPORTED_*` constants live in `protocol_versioning.rs`, incompatibility must surface in Network Health, never a silent connect failure; (3) Additive payload rule: new serde fields carry `#[serde(default)]`, never `deny_unknown_fields`, signed preimages must explicitly thread new fields (cite `friend_request_sig_preimage`); (4) Review checklist (5 bullets: new field has default? preimage updated? ALPN bump really needed? hello version bumped instead? health surfaced?); (5) Exemplar: tunnel v2 hello, frame diagram of the pipelined exchange.
- [ ] **Step 5: Gates + commit.** fmt, clippy (`--lib`), the Step-2 nextest filter now green. `git add -A && git commit -m "feat(zeb-623): protocol_versioning module, tunnel/v2 ALPN, versioning policy spec"`

### Task 2: tunnel v2 negotiation — hello exchange, dialer fallback, accept dispatch

**Files:**
- Modify: `src-tauri/src/tunnel_task.rs` (`initiator_handshake` :217-268, `responder_handshake` :114-154, test-endpoint ALPN list ~:661)
- Modify: `src-tauri/src/tunnel_manager.rs` (registry field + `spawn_dial`/`register_inbound` unchanged; add `pub(crate) fn compat_registry(&self) -> &Arc<ProtocolCompatRegistry>` and constructor threading)
- Modify: `src-tauri/src/zenoh_iroh_transport.rs:773-790` (v2 dispatch branch)
- Modify: `src-tauri/src/lib.rs` (construct `Arc<ProtocolCompatRegistry>` where `TunnelManager` is built; store a clone on `NodeState` as `protocol_compat: Arc<ProtocolCompatRegistry>` for Task 3)

**Interfaces:**
- Consumes: Task 1's `TunnelHello`, `encode_hello`/`decode_hello`/`check_hello_compatible`, `ProtocolCompatRegistry`, `alpn::HARMONY_TUNNEL_V2`.
- Produces: `TunnelManager::new(...)` gains a `compat: Arc<ProtocolCompatRegistry>` parameter (update all constructor call sites incl. tests); handshake failure taxonomy below.

**Design (exact):**

`tunnel_task.rs` — replace the `String` error of both handshake fns with:

```rust
pub(crate) enum HandshakeFailure {
    /// Peer spoke a hello below MIN_SUPPORTED — record in the compat registry.
    Incompatible { reason: String },
    Other(String),
}
```

`initiator_handshake` (single seam — connect lives here):

```rust
// Try v2 first; fall back to v1 on ANY connect error (see plan scope note 2).
let (conn, gen2) = match endpoint
    .inner()
    .connect(addr.clone(), crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V2)
    .await
{
    Ok(c) => (c, true),
    Err(e2) => {
        tracing::debug!(err = %e2, "tunnel v2 connect failed; falling back to v1");
        match endpoint.inner().connect(addr, crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V1).await {
            Ok(c) => (c, false),
            Err(e1) => return Err(HandshakeFailure::Other(format!("connect v2: {e2}; v1: {e1}"))),
        }
    }
};
let (mut send_stream, mut recv_stream) = conn.open_bi().await.map_err(...)?;
if gen2 {
    // Pipelined: hello then TunnelInit back-to-back; peer hello read AFTER, with TunnelAccept.
    let hello = crate::protocol_versioning::encode_hello(&TunnelHello::current())...;
    write_length_prefixed(&mut send_stream, &hello).await...;
}
// ... existing TunnelInit write ...
if gen2 {
    let peer_hello_bytes = read_length_prefixed(&mut recv_stream, crate::protocol_versioning::TUNNEL_HELLO_MAX).await...;
    let peer_hello = decode_hello(&peer_hello_bytes)...;
    check_hello_compatible(&peer_hello)
        .map_err(|reason| HandshakeFailure::Incompatible { reason })?;
}
// ... existing TunnelAccept read + Active check ...
```

`responder_handshake`: determine generation from `conn.alpn()` (compare to `alpn::HARMONY_TUNNEL_V2` — same accessor the accept loop's `alpn_used` comes from). If v2: read hello (cap `TUNNEL_HELLO_MAX`) → `check_hello_compatible` (on Err: return `Incompatible`, and the conn is closed by the caller path as today) → read `TunnelInit` → build session → write own hello THEN `TunnelAccept` (order matters: initiator reads hello first).

`run_tunnel_initiator` / `run_tunnel_responder`: on `HandshakeFailure::Incompatible { reason }` call `mgr.compat_registry().note_incompatible(peer_node_id, reason)` (initiator knows `peer_node_id` param; responder only learns the peer id from a COMPLETED handshake, so on the responder side an incompatible hello is logged via the registry's warn path only when the id is known — for v2 responder rejects, log `tracing::warn!` directly with the remote endpoint id from `conn.remote_id()` if available, else skip registry). On successful v2 handshake call `note_compatible(peer_node_id)`.

`zenoh_iroh_transport.rs`: add `else if alpn_used == alpn::HARMONY_TUNNEL_V2 { /* identical body to the V1 branch — same acceptor */ }` (copy the existing 15-line branch at :773; do not factor prematurely).

- [ ] **Step 1: Write failing hermetic tests** in `tunnel_task.rs` `#[cfg(test)]` (mirror the existing hermetic tunnel test setup at ~:640-700, endpoints bind BOTH v1+v2 tunnel ALPNs now; these run in the `iroh-endpoint` nextest group):

```rust
#[tokio::test]
async fn v2_dialer_to_v2_acceptor_exchanges_hello_and_reaches_active() { /* both endpoints register v2+v1; assert session Active AND registry has NO entry for peer */ }

#[tokio::test]
async fn v2_dialer_falls_back_to_v1_only_acceptor() { /* acceptor endpoint binds ONLY v1 tunnel ALPN; dialer still reaches Active via fallback; no registry entry */ }

#[tokio::test]
async fn incompatible_hello_is_rejected_and_recorded() { /* dialer writes a hand-crafted hello {protocol_version: 0} over a raw v2 connection to the responder; responder handshake returns Incompatible; OR: patch MIN via a #[cfg(test)] check seam — simplest honest version: call responder_handshake against a scripted peer stream. Use the same raw-connection scripting the existing handshake tests use; assert the handshake errors and (initiator variant) registry.incompat_reason(peer).is_some() */ }
```

Write the initiator variant of the incompatible test (script a fake acceptor that replies with hello `{protocol_version: 0}` + valid TunnelAccept bytes are unnecessary — initiator checks hello before needing accept, so the fake acceptor just sends the low hello and the initiator must fail `Incompatible` and record it).

- [ ] **Step 2: Run to verify failure.** `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(tunnel_task) & test(hello or falls_back or incompatible)'` — expect fail/compile-error.
- [ ] **Step 3: Implement** per the design block above. Update every `TunnelManager` constructor call site (lib.rs + tunnel_manager tests) to pass `Arc<ProtocolCompatRegistry>`; store the same Arc on `NodeState.protocol_compat` and ensure it is built ONCE per node start (alongside the TunnelManager build in `start_node`), NOT recreated per dial. `clear_iroh_handles` does NOT clear it (peer incompatibility is knowledge, not a connection handle) — but identity switch rebuilds NodeState anyway.
- [ ] **Step 4: Run the new tests + existing tunnel suite.** `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(tunnel)'` — all green (existing v1 tests must still pass untouched).
- [ ] **Step 5: Gates + commit.** fmt; clippy `--lib`; `git commit -m "feat(zeb-623): tunnel/v2 hello negotiation with v1 fallback + compat registry wiring"`

### Task 3: incompatibility surfaces in Network Health (backend + frontend)

**Files:**
- Modify: `src-tauri/src/network_health.rs` (`ResolverPeerRecord` :578-588, `PeerHealth` :76-85, construction :552-560, snapshot assembly where `ResolverPeerRecord`s are built)
- Modify: `src-tauri/src/lib.rs` (pass `Arc<ProtocolCompatRegistry>` into `NetworkHealthService` construction)
- Modify: `src/lib/types/network-health.ts:27-35`
- Modify: `src/lib/components/NetworkHealthView.svelte` (peer row :268-285, `peerStatusIcon` :173-179, `peerStatusTitle` :185-188)
- Test: `network_health.rs` `#[cfg(test)]`, `src/lib/components/__tests__/NetworkHealthView.test.ts`

**Interfaces:**
- Consumes: `ProtocolCompatRegistry::incompat_reason(&[u8;32])`, `NodeState.protocol_compat`.
- Produces: `PeerHealth.protocol_incompat_reason: Option<String>` (`#[serde(default)]`, camelCase `protocolIncompatReason`); same field on `ResolverPeerRecord`.

- [ ] **Step 1: Failing backend test** (place beside the existing `filter_peers_by_shared_membership` / snapshot tests):

```rust
#[test]
fn incompatible_peer_reason_flows_to_peer_health() {
    // Build a ResolverPeerRecord with protocol_incompat_reason = Some("tunnel hello v0 < min 1")
    // through the existing test constructor pattern, run it through
    // filter_peers_by_shared_membership (peer shares a community), and assert
    // the resulting PeerHealth.protocol_incompat_reason carries the string.
}

#[test]
fn peer_health_serializes_protocol_incompat_reason_camel_case() {
    // serde_json::to_value(peer_health) — assert v["protocolIncompatReason"] == "…"
    // and that a None serializes as null (field present, additive-with-default).
}
```

- [ ] **Step 2: Run to verify failure** (`-E 'test(incompatible_peer or protocol_incompat)'`).
- [ ] **Step 3: Implement backend.** Add the field to both structs (`#[serde(default)]`). Where snapshot assembly builds each `ResolverPeerRecord` (it already carries `iroh_node_id`), join: `protocol_incompat_reason: compat.incompat_reason(&record_node_id)`. `NetworkHealthService` gains the `Arc<ProtocolCompatRegistry>` at construction (mirror how it already holds the resolver/liveness handles); update its `lib.rs` construction site. `schema_version` stays 4 (assert-tests untouched).
- [ ] **Step 4: Frontend.** `network-health.ts` `PeerHealth` gains `protocolIncompatReason: string | null;`. `NetworkHealthView.svelte`: in the peer `<li>`, when set, render `<span class="peer-incompat" role="alert" title={p.protocolIncompatReason}>⚠ incompatible</span>` next to the connection mode (copy the styling approach of the existing transport-disabled `role="alert"` section :217-237 scaled down; a red/amber inline badge). `peerStatusTitle` prepends the reason when present. Vitest: extend the existing peer-row test data with one incompatible peer; assert the badge text and `role="alert"` render, and that peers without the field render no badge.
- [ ] **Step 5: Gates + commit.** fmt/clippy `--lib`; scoped nextest `-E 'test(network_health)'`; `npx vitest run src/lib/components/__tests__/NetworkHealthView.test.ts`; `npx tsc --noEmit`. `git commit -m "feat(zeb-623): surface per-peer protocol incompatibility in Network Health"`

### Task 4: settings layer — rename to `ConnectivitySettings`, add `iroh_relays`, atomic save

**Files:**
- Rename: `src-tauri/src/pkarr_settings.rs` → `src-tauri/src/connectivity_settings.rs` (`git mv`)
- Modify: `src-tauri/src/lib.rs` + any other `pkarr_settings::` referencers (`rg 'pkarr_settings|PkarrSettings'`)
- Test: unit tests inside `connectivity_settings.rs`

**Two commits.** Commit A = pure rename, zero behavior change. Commit B = additions.

- [ ] **Step 1 (Commit A): mechanical rename.** `git mv src-tauri/src/pkarr_settings.rs src-tauri/src/connectivity_settings.rs`. Then: module decl `pub mod pkarr_settings` → `pub mod connectivity_settings`; `PkarrSettings` → `ConnectivitySettings`; `default_relays` → `default_pkarr_relays`; `NodeState.pkarr_settings_path` → `connectivity_settings_path` (field + all uses); update every `use`/path. **Do NOT rename** the persisted file name (`connectivity-settings.json` — already right), the IPC command names, `PKARR_RELAY_WRITE_LOCK`, or the `relays` JSON key (wire/disk compat). `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings` proves completeness; run `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(connectivity_settings or pkarr)'`. fmt. `git commit -m "refactor(zeb-624): rename PkarrSettings -> ConnectivitySettings (owns non-pkarr settings)"`
- [ ] **Step 2 (Commit B): failing tests for the additions:**

```rust
#[test]
fn iroh_relays_default_empty_and_roundtrip() {
    let s = ConnectivitySettings::default();
    assert!(s.iroh_relays.is_empty()); // empty = follow iroh preset defaults
    // old file without the key parses with empty vec (serde default)
    let old = r#"{"identity_discoverable":false,"friend_auto_accept_known":true,"relays":["https://pkarr.q8.fyi"],"presence_invisible":false}"#;
    let parsed: ConnectivitySettings = serde_json::from_str(old).unwrap();
    assert!(parsed.iroh_relays.is_empty());
}

#[test]
fn validate_iroh_relay_urls_rules() {
    // https accepted + normalized (trailing slash stripped)
    assert_eq!(
        validate_iroh_relay_urls(vec!["https://use1-1.relay.n0.iroh.link/".into()]).unwrap(),
        vec!["https://use1-1.relay.n0.iroh.link".to_string()]
    );
    // must also parse as an iroh RelayUrl
    assert!(validate_iroh_relay_urls(vec!["https://relay example".into()]).is_err());
    // empty list rejected (use reset for defaults)
    assert!(validate_iroh_relay_urls(vec![]).is_err());
    // http only for local hosts; dedup; cap MAX_IROH_RELAYS=8 — mirror the pkarr test matrix
    assert!(validate_iroh_relay_urls(vec!["http://127.0.0.1:3340".into()]).is_ok());
    assert!(validate_iroh_relay_urls(vec!["http://relay.evil.example".into()]).is_err());
}

#[test]
fn save_is_atomic_tmp_rename() {
    // save() writes <name>.tmp then renames: after save, no .tmp sibling remains
    // and the file parses. (Direct observable: write, assert parse + no tmp file.)
}
```

- [ ] **Step 3: Implement.** Add to `ConnectivitySettings`: `#[serde(default)] pub iroh_relays: Vec<String>,` (doc: "ZEB-624: custom iroh relay URL list. EMPTY = follow the iroh preset's default relay map (n0 stable). Applied at endpoint build and live via insert/remove diff."). `fail_closed_defaults` sets `iroh_relays: Vec::new()` (empty = defaults; relays are operational infra). `pub const MAX_IROH_RELAYS: usize = 8;` `validate_iroh_relay_urls` = per-entry `validate_single_relay` (reuse) **plus** `iroh::RelayUrl::from_str(&normalized).map_err(...)`, empty-list reject, dedup, cap — factor the shared loop with a per-entry closure rather than duplicating `validate_relay_urls`. `sanitize_iroh_relay_urls` mirrors `sanitize_relay_urls` (lenient, may return empty). `effective_iroh_relays(&ConnectivitySettings) -> Option<Vec<String>>`: sanitize; empty → `None` (= use defaults). Atomic `save`: write to `path.with_extension("json.tmp")` then `std::fs::rename` (same dir → atomic on macOS/Linux; document Windows best-effort).
- [ ] **Step 4: Run tests green** (`-E 'test(connectivity_settings)'`), fmt, clippy `--lib`. `git commit -m "feat(zeb-624): iroh_relays setting + validators + atomic settings save"`

### Task 5: endpoint plumbing + IPC verbs + boot wiring

**Files:**
- Modify: `src-tauri/src/iroh_endpoint.rs` (builder + new methods; keep `default_relay_map_is_stable_non_canary` green)
- Modify: `src-tauri/src/lib.rs` (boot call sites — `rg 'new_with_secret\('`; IPC verbs near the pkarr relay block :46978-47103; both `generate_handler!` blocks; boot reconcile mirroring :10184-10222)
- Create: `docs/specs/2026-07-03-zeb-624-iroh-relay-config-design.md`
- Test: `iroh_endpoint.rs` + `lib.rs` unit tests (pure pieces only — no live-endpoint IPC test)

**Interfaces:**
- Consumes: Task 4's `effective_iroh_relays`, `validate_iroh_relay_urls`, `ConnectivitySettings`.
- Produces:

```rust
// iroh_endpoint.rs
impl IrohEndpoint {
    /// Build with an optional custom relay list. None/empty => presets::N0 defaults.
    pub async fn new_with_secret_and_relays(
        secret_key: SecretKey,
        custom_relays: Option<Vec<RelayUrl>>,
    ) -> Result<Self, IrohEndpointError>;      // new_with_secret delegates with None
    /// Current relay map URLs (normalized strings, sorted).
    pub fn relay_map_urls(&self) -> Vec<String>;
    /// Diff-apply: target = the given list. Returns (inserted, removed) counts.
    pub async fn apply_relay_urls(&self, target: &[RelayUrl]) -> (usize, usize);
}

// lib.rs IPC (all return IrohRelayWire, mirror pkarr verb structure + write lock):
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IrohRelayWire { pub relays: Vec<String>, pub custom: bool }
// get_iroh_relays / set_iroh_relays(relays: Vec<String>) / add_iroh_relay(url: String)
// / remove_iroh_relay(url: String) / reset_iroh_relays
```

- [ ] **Step 1: Failing endpoint tests** (in `iroh_endpoint.rs` test module; the custom-bind test joins the `iroh-endpoint` nextest group and uses `warm_up_iroh_global_init()` if it asserts any timeout):

```rust
#[tokio::test]
async fn custom_relay_list_overrides_default_map() {
    // new_with_secret_and_relays with one custom RelayUrl ("https://relay.example.com")
    // → relay_map_urls() == ["https://relay.example.com./"]-normalized form (assert via
    // RelayUrl::from_str round-trip equality, not raw string literals)
}

#[tokio::test]
async fn apply_relay_urls_diffs_insert_and_remove() {
    // start custom [A]; apply [B] → (1 inserted, 1 removed); relay_map_urls() == [B]
    // then apply [B] again → (0, 0)  (idempotent)
}
```

- [ ] **Step 2: Verify failure**, then implement `iroh_endpoint.rs`:
  - builder: `let builder = Endpoint::builder(presets::N0).secret_key(...).alpns(...);` then `let builder = match custom_relays { Some(urls) if !urls.is_empty() => builder.relay_mode(iroh::endpoint::RelayMode::custom(urls)), _ => builder };` `.bind()`.
  - `apply_relay_urls`: read `self.inner.relay_map()`, collect current `urls()` into a set; for each target url not present → `self.inner.insert_relay(url.clone(), std::sync::Arc::new(iroh::RelayConfig::from(url.clone()))).await`; for each current not in target → `self.inner.remove_relay(&url).await`. Count both.
- [ ] **Step 3: lib.rs wiring.**
  - Boot: at each `new_with_secret(` call site, load `ConnectivitySettings` (the path helper already exists), compute `effective_iroh_relays`, parse to `Vec<RelayUrl>` (drop unparseable with `warn!` — sanitize already filtered), call `new_with_secret_and_relays`.
  - IPC verbs: mirror the pkarr five exactly (validate → persist under a new `static IROH_RELAY_WRITE_LOCK: tokio::sync::Mutex<()>` → live-apply → emit). `apply_iroh_relays(app, state, validated: Vec<String>) -> Result<IrohRelayWire, String>` helper: persist `iroh_relays = validated` (empty for reset), compute target = custom or `iroh::endpoint::default_relay_mode().relay_map()` urls, if a live endpoint handle exists → `apply_relay_urls`, emit `app.emit("iroh-relays-changed", ())`, return wire `{relays: <effective target>, custom: !persisted.is_empty()}`. `get_iroh_relays`: prefer live `relay_map_urls()`, else effective target from persisted; `custom` from persisted. `remove_iroh_relay` rejects removing the last relay of a CUSTOM list (tell user to reset instead); `add_iroh_relay`/`remove_iroh_relay` on a defaults-following node operate on the materialized default list (add → defaults+new becomes custom; remove of a default → remaining defaults become custom).
  - Register all 5 in BOTH `generate_handler!` blocks.
  - Boot reconcile: after the endpoint handle is stored, under `IROH_RELAY_WRITE_LOCK`, compare `relay_map_urls()` vs effective target and `apply_relay_urls` if differing (mirrors pkarr :10184-10222).
  - Pure-logic tests beside the pkarr IPC tests (`lib.rs` test module): `iroh_relay_set_persist_roundtrip` (tempdir settings file: set → reload → effective list), `iroh_relay_reset_clears_to_defaults_sentinel` (reset writes `[]`, effective = None).
- [ ] **Step 4: Write the S8 design note** (`docs/specs/2026-07-03-zeb-624-iroh-relay-config-design.md`, ~40 lines): decision (config-not-code, empty-=-defaults sentinel, live diff-apply), the **deterministic-overlap pattern** paragraph (when custom relays enter a fleet, share a primary so dialer/acceptor relay sets overlap — the pkarr.q8.fyi/ZEB-513 lesson), and the explicit non-goals (no self-hosted relay, no headless verbs, governance Phase 5+).
- [ ] **Step 5: Gates + commit.** fmt; clippy `--lib`; `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(iroh_relay or iroh_endpoint or connectivity_settings)'`; keep `default_relay_map_is_stable_non_canary` green. `git commit -m "feat(zeb-624): iroh relay list as persisted runtime-editable config (5 IPC verbs, live diff-apply)"`

### Task 6: frontend — adapter + IrohRelaySettings component

**Files:**
- Modify: `src/lib/connectivity-adapter.ts` (after the pkarr relay fns :362-436), `src/lib/types/network-health.ts` (add `IrohRelayInfo`)
- Create: `src/lib/components/IrohRelaySettings.svelte`
- Modify: `src/lib/components/SettingsPanel.svelte` (import + mount directly below `<NetworkDiscoverabilitySettings />` :175)
- Test: `src/lib/connectivity-adapter.test.ts`, `src/lib/components/__tests__/IrohRelaySettings.test.ts`

**Interfaces:**
- Consumes: IPC verbs from Task 5 (`get_iroh_relays` etc., returning `{relays: string[], custom: boolean}` camelCase), Tauri event `iroh-relays-changed`.
- Produces: `export interface IrohRelayInfo { relays: string[]; custom: boolean }`; adapter fns `getIrohRelays(): Promise<IrohRelayInfo>`, `setIrohRelays(relays: string[])`, `addIrohRelay(url: string)`, `removeIrohRelay(url: string)`, `resetIrohRelays()` — each returns `Promise<IrohRelayInfo>`.

- [ ] **Step 1: Failing adapter tests** — mirror `connectivity-adapter.test.ts:183-292` pkarr-relay block: mock `invoke`, assert command name + args (`{ relays }` / `{ url }`), assert error extraction uses `e instanceof Error ? e.message : String(e)`.
- [ ] **Step 2: Implement adapter fns** (copy the pkarr fn bodies, swap command names/types). Run vitest on the adapter file — green.
- [ ] **Step 3: Failing component test** `IrohRelaySettings.test.ts`: renders relay list from `getIrohRelays` mock; shows "Using recommended relays" note when `custom === false` and a "Custom relay set" note when true; add-input calls `addIrohRelay` and renders returned list; remove button per row calls `removeIrohRelay`; "Restore recommended" calls `resetIrohRelays`; an invoke rejection surfaces the message in the error region (`role="alert"`); subscribes to `iroh-relays-changed` on mount and refetches.
- [ ] **Step 4: Implement `IrohRelaySettings.svelte`** — structurally copy `NetworkDiscoverabilitySettings.svelte`'s relay-manager section (state :49-66, fetch-with-seq-guard :84-106, authoritative-apply :113-118, handlers :120-176, mount/teardown :210-232), minus per-relay health labels (iroh wire has no health field; the row shows the URL + remove button). Section heading: "Transport relays (iroh)"; helper copy: "Relays carry traffic when a direct connection isn't possible. Leave on the recommended set unless you run your own relay." Mount in `SettingsPanel.svelte` below `<NetworkDiscoverabilitySettings />`.
- [ ] **Step 5: Gates + commit.** `npx vitest run` (both new test files + existing), `npx tsc --noEmit`. `git commit -m "feat(zeb-624): iroh relay settings UI + adapter"`

### Task 7: full sweep + ledger

- [ ] **Step 1:** `cargo fmt --all -- --check`
- [ ] **Step 2:** `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] **Step 3:** `cargo nextest run --locked --workspace --all-targets --features test-fixtures` — the full sweep (~55-90 min under the iroh-endpoint throttle; run foregrounded with a generous timeout or supervised background per the standing supervision rule; machine-sleep SIGTERM ⇒ prove completion by pass-set union across runs on identical code).
- [ ] **Step 4:** `npx tsc --noEmit && npx vitest run` (repo root).
- [ ] **Step 5:** Fix any fallout (unrelated pre-existing failures get a ticket, not a fold-in). Commit fixes if any: `git commit -m "test(zeb-623/624): full-sweep fallout"`.

---

## Self-review notes (spec coverage)

- ZEB-623 deliverable "short versioning policy spec in docs/specs/" → Task 1 Step 4. "hello-frame exemplar + tests" → Tasks 1-2. "N/N-1 constants" → Task 1 (`MIN_SUPPORTED_*`, generation pin test). "health-surface incompatibility signal" → Task 3. "payload rule codified" → spec §3 + checklist (Task 1).
- ZEB-624 "persists/validates/round-trips like pkarr relays" → Tasks 4-5. "defaults = stable map" → empty-sentinel design (scope note 5) + `default_relay_map_is_stable_non_canary` stays green. "Network panel shows the active home relay" → already shipped (`nh-relay` row, `NetworkHealthView.svelte:254`); Task 5's `get_iroh_relays` gives Settings the configured set. "docs note deterministic-overlap" → Task 5 Step 4.
- Acceptance tests named per ticket: hello roundtrip/tolerance (T1), v2↔v2 + fallback + incompatible-reject (T2), health flow + camelCase (T3), validator matrix + atomic save (T4), custom-map override + idempotent diff (T5), UI flows (T6).
