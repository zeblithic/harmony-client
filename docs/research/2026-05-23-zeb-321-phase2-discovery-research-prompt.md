# Gemini Deep Research Prompt — Cross-WAN P2P discovery & bootstrap for indie federated apps in 2026

Companion to Linear umbrella [ZEB-321](https://linear.app/zeblith/issue/ZEB-321) (harmony-client: cross-WAN peer discovery, reconnection, and NAT traversal). Drafted 2026-05-23 to inform Phase 2 (first-contact across WAN without prior NodeId exchange) and bracket Phases 3-5.

---

## Paste-into-Gemini prompt

> I am architecting the discovery and bootstrap layer of a federated, end-to-end-encrypted, self-sovereign social/collaboration app (think the love-child of Signal, Discord, Matrix, and Briar) built on the iroh QUIC stack with Zenoh as the application-layer pub/sub. I need a comparative, citation-rich 2026 survey of the state of the art for peer discovery, NAT traversal, and reconnection — specifically focused on the case where two devices on residential ISPs in different countries need to establish their *first ever* end-to-end connection without prior knowledge of each other's network address, and the case where one device wakes up after weeks offline and needs to find its known peers again.
>
> ### What is already built (so the report doesn't re-litigate solved problems)
>
> - **Cross-NAT transport: iroh 0.98 (QUIC + n0-hosted DERP relays).** End-to-end encrypted. Hole-punches when possible, relays when not. Self-hosted relay deployed at `i.q8.fyi` on GCP. Wired through the app: a device with a known peer's iroh NodeId can already address it across WAN through DERP.
> - **Application-layer pub/sub: Zenoh 1.x.** Runs *over* iroh as a custom transport so Zenoh's content-routed messages reach the same peers iroh discovered. CRDT events flow over Zenoh-over-iroh.
> - **Within-community reachability gossip:** every device publishes a signed `ReachabilityAnnounce` event (iroh NodeId + DERP home-relay + direct addresses) into its community's CRDT. Members of the same community automatically learn each other's current network address. *This solves discovery for peers who are already in the same community.*
> - **Public-community catalog: a "library-federated directory" pattern.** Users subscribe to "libraries" (federated trust anchors, modeled loosely on public libraries) that publish signed catalogs of communities they vouch for. Users browse a library's catalog and request to join a community. *This solves first-contact for **public** communities.*
>
> ### What is **not** built (the gap this research informs)
>
> - **First-contact for small/private groups across WAN.** Two friends in different countries who decide to start a private 2-person community right now. Neither's iroh NodeId is in any library catalog. Neither has any prior shared address. How do they bootstrap that connection without a centralized server and without a terrible out-of-band UX?
> - **Reconnection after mobility / long offline.** Laptop wakes from a week of sleep at a new coffee shop in a new city; its peers' DERP home-relays may have changed; it can't broadcast its new address to peers it hasn't connected to yet *because the discovery channel itself runs over peer connections*. How is this resolved without N stale-address timeouts?
> - **Re-establishing contact after total network change.** Both devices have moved networks and changed DERP relays since they last spoke. Their last-known address records are mutually stale. They both still have each other's static cryptographic identity. How do they find each other again?
>
> ### Architectural constraints (the report must respect these)
>
> 1. **Polycentric / no global infrastructure.** No platform admin, no single DHT operator, no one company that can be coerced into deanonymizing users. Federated, civically-rooted, or fully P2P only.
> 2. **Self-sovereign cryptographic identity.** Users own their identity keys; identity is *not* re-issuable by any third party. Recovery only via offline artifacts the user holds.
> 3. **Designed for billion-user scale from day one.** The system should not have an obvious analytical scaling cliff at 10⁶, 10⁸, or 10⁹ active devices. Cost-efficient on commodity cloud.
> 4. **Metadata-minimizing.** Discovery lookups should not leak the social graph to any single observer (DHT operator, relay operator, ISP). Acceptable to have a privacy-vs-latency knob.
> 5. **Mobile-respectful.** Must work within iOS/Android push-notification gates and intermittent cellular connectivity; cannot require always-on background sockets.
> 6. **Composable with what's already built.** Solutions that *extend* iroh's pkarr/DERP/relay infrastructure or sit alongside the library-federated directory are preferable to wholesale replacements.
>
> ### Primary research questions (the report should answer these with 2026-current data)
>
> **Q1. Discovery substrates at scale.** Compare these candidate first-contact discovery substrates for the small-private-group bootstrap case and the public-community-at-scale case:
>   1. **DHT-based** (Mainline DHT, Kademlia variants, iroh's pkarr-over-Mainline)
>   2. **Gossip-based overlay** (HyParView + Plumtree, Scuttlebutt)
>   3. **DNS-style signed records** (pkarr standalone, IPNS, IPFS DNSLink)
>   4. **Federated rendezvous servers** (libp2p Rendezvous, Matrix-style HS federation, IRC-style)
>   5. **Library/civic-infrastructure federated catalogs** (the pattern this app already uses for public communities — extend or constrain it?)
>   6. **Out-of-band cryptographic invite tokens** (Signal-style safety numbers, Briar invite codes, Magic Wormhole, QR-based pairing)
>   7. **Hybrid: reputation-gated DHT writes** (EigenTrust on top of Mainline, Trustchain-style)
>   8. **Anything I'm missing in the 2026 landscape.**
>
>   For each, give: how it works in 1 paragraph, scale ceiling with citation, metadata-leak profile, mobile-compatibility, current implementations / production deployments in 2026, known failure modes, and a recommendation for whether it fits the small-private-group case, the public-community case, or both.
>
> **Q2. State-of-the-art NAT traversal in 2026.** Compare iroh 0.98 (QUIC + ICE-like + DERP), libp2p hole-punch v2 / DCUtR, Hyperswarm UDP holepunching, Tailscale's DERP, and MASQUE/CONNECT-UDP for traversal success rate on real residential and mobile NAT topologies. Cite empirical traversal success rates from 2024-2026 measurement studies if any exist. Identify NAT scenarios where iroh's DERP fallback fails in practice and what alternatives do better.
>
> **Q3. Mobile reconnection patterns from production systems.** How do Signal, Matrix (Element X), Briar, Tox, Berty, Session, SimpleX, Earthstar/Willow, and Veilid handle:
>   - Coming back online after hours/days offline (rediscovering peer addresses)
>   - Cellular handoff and address rebinding
>   - iOS background push gate (apps can't run >30s in background without push wake)
>   - Android Doze / battery saver compatibility
>
>   Highlight patterns that work in adversarial mobile conditions (no FCM/APNs available, e.g. degoogled Android) and patterns that explicitly require a push provider.
>
> **Q4. Iroh-specific scale economics.** What is the 2026-current capacity, cost curve, and operational risk of:
>   - The free n0-hosted DERP relay network (capacity, rate limits, latency tail, what happens at 10⁴/10⁶/10⁸ users)
>   - Self-hosted iroh DERP relays on GCP / fly.io / hetzner — cost per million concurrent connections, geographic distribution thresholds
>   - pkarr (iroh's signed-record discovery layer over Mainline DHT) — write/read throughput, censorship resistance, time-to-resolve in 2026
>
> **Q5. Comparison of competing P2P stacks for indie federated apps in 2026.** Compare iroh, libp2p (rust-libp2p), Hyperswarm/Pear, Earthstar+Willow, Veilid, and any other production-credible 2026 stack against the criteria: NAT traversal success, discovery scale, identity model, transport efficiency, mobile fit, ecosystem maturity, license, and what an indie team would have to rebuild themselves. **Which is closest to "production-ready for indie federated apps in 2026" and what are the open gaps?**
>
> **Q6. The bootstrap-cost-vs-UX-vs-privacy frontier.** For the small-private-group first-contact case, what is the *frontier* — i.e., what does the best UX look like at each privacy and cost point? Specifically:
>   - Pure out-of-band: alice texts bob an invite link. UX: friction. Privacy: perfect (no observer learns the social edge). Cost: $0.
>   - Federated rendezvous: alice posts a one-time challenge to a known rendezvous server bob also trusts. UX: smooth. Privacy: rendezvous operator sees the edge. Cost: minimal.
>   - DHT rendezvous: alice publishes a one-time challenge to Mainline DHT. UX: smooth. Privacy: DHT crawlers can see the edge. Cost: $0.
>   - Hybrid: out-of-band initial pairing exchanges a *future* rendezvous secret that is rotated and stored only on each end. UX: smooth after first pairing. Privacy: strong forward secrecy on the discovery channel.
>
>   What are the strongest published designs on each frontier point in 2026? Cite specific papers and production systems.
>
> ### Deliverable format
>
> A written report, ~15-30 pages, structured as:
>
> 1. Executive summary (1 page): top 3 recommendations for the small-private-group bootstrap case and the reconnection case, with the strongest justification.
> 2. Section per primary question (Q1-Q6) with comparative tables, citations, and explicit "fits our constraints / does not fit" judgments.
> 3. **Concrete proposed designs** (2-4 candidate end-to-end designs for Phase 2 that combine choices from Q1-Q6 into something we could brainstorm + build). Each design: 1-page architecture sketch + tradeoffs + estimated build effort.
> 4. Open questions that the report could not resolve and would need a brainstorm or prototype to settle.
>
> ### What to exclude (out of scope)
>
> - Anonymity networks (Tor, I2P, Nym) as primary discovery — useful as composition partners but not the core substrate.
> - Blockchain-based discovery (ENS, Lens, Farcaster hubs) — over-engineered and adds a permanent ledger we don't need.
> - Anything requiring custom hardware (LoRa mesh, etc.) — interesting but not Phase 2 scope.
> - LAN-only discovery (mDNS, etc.) — already solved.
> - Identity / key management — out of scope; assume each user has a stable Ed25519 identity key already.

---

## Notes for Jake (not part of the prompt)

- The output of this report feeds the Phase 2 brainstorm (see task #1516). After the report lands, drop the markdown back into the chat and we'll open the brainstorming skill against it.
- Estimated Deep Research runtime: 30-60 min based on prior reports.
- If you want to narrow the scope before running (cheaper, faster), the **load-bearing questions for Phase 2 specifically are Q1, Q3, Q5, and Q6.** Q2 (NAT traversal SOTA) and Q4 (Iroh economics) are for Phase 3+ and could be deferred to a follow-up report.
- Once the report is back, we'll file the actual ZEB-NNN Phase 2 sub-ticket under [ZEB-321](https://linear.app/zeblith/issue/ZEB-321). Per the `never invent Linear IDs` rule, no number gets minted until the brainstorm settles the scope.
