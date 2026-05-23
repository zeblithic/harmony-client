# Architecting Decentralized Peer Discovery and Reconnection at Scale: A 2026 Analysis

> **Source:** Gemini Deep Research report, commissioned 2026-05-23 against the prompt at [`2026-05-23-zeb-321-phase2-discovery-research-prompt.md`](./2026-05-23-zeb-321-phase2-discovery-research-prompt.md).
> **Companion to:** Linear umbrella [ZEB-321](https://linear.app/zeblith/issue/ZEB-321). Resolves several of its Phase-0 "Open research questions". Feeds the Phase 2 brainstorm.
> **Verbatim text** from Gemini, lightly formatted for readability (headings, tables, code escapes). No content altered.

---

## Executive Summary

The architectural engineering of a federated, end-to-end-encrypted, self-sovereign collaboration application requires navigating profound trade-offs between network reliability, cryptographic privacy, and the hostile realities of mobile operating systems. The objective of this research is to define the optimal state-of-the-art strategies for peer discovery, Network Address Translation (NAT) traversal, and asynchronous mobile reconnection as of mid-2026. The target architecture leverages the iroh QUIC stack for transport and Zenoh for application-layer Conflict-free Replicated Data Type (CRDT) publish/subscribe messaging. Within this polycentric, globally scalable framework, two primary operational gaps remain: bootstrapping the very first connection between users in small, private groups across Wide Area Networks (WANs) without relying on centralized directories, and gracefully reconnecting highly mobile devices after prolonged periods of offline sleep.

Based on an exhaustive analysis of empirical network measurements, recent protocol deployments, and mobile operating system constraints, the following foundational recommendations are established to guide the next phase of architectural design.

First, to resolve the small-private-group bootstrap challenge without leaking the social graph to global observers, the architecture must adopt an ephemeral cryptographic rendezvous pattern over the BitTorrent Mainline Distributed Hash Table (DHT). Relying purely on out-of-band communication for network addressing introduces unacceptable user friction, while federated rendezvous servers inevitably leak connection metadata. By exchanging a high-entropy cryptographic seed out-of-band — such as through a QR code or an initial secure message — devices can deterministically derive a sequence of time-boxed, ephemeral Ed25519 keypairs. The initiating device can subsequently publish its current iroh NodeId and its designated DERP home-relay to the Mainline DHT using iroh's pkarr infrastructure under these ephemeral keys. To any external observer or DHT crawler, these records manifest as random cryptographic noise, perfectly preserving the anonymity of the social edge while allowing the designated peer to flawlessly resolve the routing information.

Second, the assumption that peer-to-peer applications can maintain persistent background sockets on modern mobile operating systems must be entirely discarded. iOS enforces stringent background execution limits, typically suspending applications within thirty seconds, while Android's Doze mode aggressively restricts network access to preserve battery life. To achieve reliable reconnection after hours or days offline, the architecture requires a decoupled, "Zero-Push" notification gateway. Modeled on patterns successfully deployed by systems like Berty, this untrusted intermediary maps highly ephemeral, rotating routing tokens to Apple Push Notification service (APNs) and Firebase Cloud Messaging (FCM) device tokens. The gateway wakes the dormant device without ever learning the user's identity, the sender's identity, or the cryptographic contents of the notification. For privacy-hardened environments lacking Google Play Services, the application must natively integrate with the UnifiedPush specification, allowing battery-efficient multiplexing of background connections through user-controlled distributors.

Third, while decentralized NAT traversal has matured significantly, empirical evidence from 2026 demonstrates that pure peer-to-peer hole punching success rates consistently plateau around seventy percent in permissionless environments. Iroh's integration of Tailscale-inspired QUIC mechanics fundamentally improves upon this baseline, but strict symmetric NATs and Endpoint-Dependent Mapping (EDM) topologies still impose hard limits on direct connectivity. Therefore, Designated Encrypted Relay for Packets (DERP) fallback is an absolute requirement to achieve the requisite 99.9% connectivity baseline. However, public relay networks are deliberately rate-limited and unsuitable for high-bandwidth CRDT synchronization. The system must deploy a highly distributed, self-hosted fleet of DERP relays positioned at edge compute nodes to minimize latency tails, ensuring that the Zenoh synchronization layer remains robust even when direct peer-to-peer paths inevitably fail.

---

## Q1. Discovery Substrates at Scale

The challenge of peer discovery demands a mechanism that allows nodes to locate each other across dynamic IP environments. The constraints of the architecture require evaluating these substrates against the dual paradigms of the small-private-group bootstrap (where zero metadata leakage is the primary objective) and the public-community scale (where high availability and low latency are prioritized).

### Distributed Hash Tables (Mainline DHT, Kademlia, iroh pkarr)

Distributed Hash Tables, specifically the BitTorrent Mainline DHT, operate by mapping keys to network nodes using the Kademlia XOR metric, allowing for highly efficient, decentralized data retrieval. Iroh's pkarr (Public-Key Addressable Resource Records) subsystem leverages the BEP44 extension to publish mutable, signed Domain Name System (DNS) packets directly into the Mainline DHT. The payload of a BEP44 record is strictly limited to 1000 bytes, which is optimally sized to contain an iroh NodeId and a DERP relay URL, but entirely precludes the storage of heavy application data. The scale ceiling of the Mainline DHT is peerless in the decentralized landscape, boasting an estimated 10 million active nodes globally. Analytically, Kademlia networks scale to billions of devices due to their $\mathcal{O}(\log n)$ routing complexity, ensuring that the system will not experience a performance cliff as user adoption grows.

However, the metadata-leak profile of a standard DHT implementation is inherently high if static, long-lived cryptographic identities are used. Malicious actors and academic researchers continuously deploy DHT crawlers that log the IP addresses associated with specific public keys over time, effectively mapping the geographic and temporal movements of the user. Furthermore, direct mobile compatibility is exceedingly poor; mobile IP churn, intermittent cellular connectivity, and battery constraints prevent mobile devices from acting as reliable routing nodes within the DHT overlay. Mobile clients are thus required to use stateless HTTP relays to read and write to the DHT network. Known failure modes include propagation delays, as aggressive caching can prevent immediate record updates, and the ephemeral nature of the records themselves, which the network drops after several hours unless actively republished. This substrate is highly recommended for the small-private-group case, but only if the application employs ephemeral, rotating keys to completely obfuscate the identity of the participants from global observers. It is less suited for public communities due to the inherent resolution latency compared to dedicated federated systems.

### Gossip-Based Overlays (HyParView + Plumtree, Scuttlebutt)

Gossip protocols manage peer discovery and state dissemination through epidemic routing. Modern structured gossip implementations, such as the combination of HyParView and Plumtree, maintain highly resilient network topologies by dividing peer knowledge into two sets. HyParView ensures the overlay graph remains connected even under massive node churn by maintaining a small, heavily verified active view of peers, alongside a much larger passive view used for recovery. Plumtree then operates atop this resilient mesh to construct a broadcast tree, significantly reducing the redundant message flooding that plagues naive gossip protocols. The scale ceiling for discrete communities using these protocols typically reaches into the tens of thousands of concurrent nodes before broadcast latency and active-view maintenance overhead become prohibitive.

The metadata-leak profile is moderate; nodes within a community actively observe the IP addresses of their immediate active and passive peers, meaning the internal social graph is locally visible, though insulated from global, non-participating observers. The critical flaw for this architecture lies in mobile compatibility. Maintaining the rigorous heartbeat mechanisms required by HyParView's active view causes continuous radio wake-ups, leading to catastrophic battery drain on cellular devices. While protocols like Secure Scuttlebutt (SSB) have historically utilized similar epidemic mechanisms, they rely heavily on always-on "pub" servers to bridge intermittent mobile clients. Ultimately, gossip overlays are phenomenally powerful for intra-community CRDT state propagation — a role already fulfilled by the Zenoh protocol within this stack — but they are fundamentally incapable of solving the first-contact bootstrap problem, as a node cannot gossip with a network it has not yet discovered.

### DNS-Style Signed Records (pkarr standalone, IPFS DNSLink)

This paradigm anchors decentralized identity resolution to traditional, hierarchical DNS infrastructure. Iroh utilizes this approach by default via the `_iroh.<z32-endpoint-id>.` TXT record format. When an iroh endpoint attempts to connect to an unknown peer, it performs a standard DNS query against a configured origin domain (such as `dns.iroh.link`), which returns the peer's designated home relay URL and directly observed IP addresses. The scale ceiling of this approach is effectively infinite, as it inherits the highly optimized, globally distributed caching architecture of the legacy DNS system.

Conversely, the metadata-leak profile is unacceptably high for private communications. DNS queries are frequently transmitted unencrypted (unless DNS-over-HTTPS or DNS-over-TLS is strictly enforced at the OS level), and authoritative DNS operators possess complete visibility into the social graph, logging exactly which IP addresses are requesting the routing information of specific NodeIds. Mobile compatibility is flawless, as standard DNS resolution leverages highly optimized native operating system APIs with minimal battery impact. The primary failure mode is the reliance on central authoritative domains, which violates the polycentric architectural constraint if deployed as the sole discovery mechanism. This substrate is strongly recommended for the public-community case where metadata privacy is entirely deprioritized in favor of instantaneous, frictionless resolution, but it is entirely inappropriate for small-private-group bootstrapping.

### Federated Rendezvous Servers (libp2p Rendezvous, Matrix-style)

Federated rendezvous architectures require nodes to explicitly register their current network presence with a mutually agreed-upon, highly available server. The Matrix protocol exemplifies this pattern, utilizing homeservers to track user presence and bridge communications across a federated network of independent operators. Similarly, the libp2p Rendezvous protocol provides a lightweight mechanism for peers to discover each other within specific, negotiated namespaces without requiring a full DHT traversal. The scale ceiling is tightly constrained by the vertical compute capacity of the individual rendezvous servers; while the Matrix network scales to millions of users globally, heavily populated rendezvous points frequently experience severe state-resolution lag and database bottlenecking.

The metadata-leak profile is exceptionally high. The rendezvous server operator explicitly logs the IP addresses of both the initiator and the recipient, gaining cryptographic proof that the two entities are communicating at a specific time. Mobile compatibility is excellent, as clients can utilize standard HTTP polling or push-notification triggers to interact with the highly available server. The primary failure mode is the vulnerability of the server operator to coercion, DDoS attacks, or simple infrastructure failure, creating a localized single point of failure for any community relying on that specific node. This pattern effectively mirrors the "library-federated directory" approach already present in the architecture and should be reserved exclusively for public communities willing to trust a specific library operator.

### Out-of-Band (OOB) Cryptographic Invite Tokens

To achieve perfect zero-knowledge initial discovery, users must rely on out-of-band communication. This involves Alice and Bob utilizing a pre-existing secure channel — such as Signal, a physical QR code scan, or a Magic Wormhole transfer — to exchange a high-entropy token. This token contains the precise cryptographic material, initial routing addresses, or deterministic seeds necessary to execute the network handshake. The scale ceiling is inapplicable, as the exchange occurs entirely outside the system's infrastructure.

The metadata-leak profile is perfect; no external observer, relay operator, or DHT crawler learns that the social edge exists or that a connection is being attempted. Mobile compatibility is excellent because the exchange is fundamentally user-driven, thereby circumventing operating system background execution limits entirely. This pattern is actively deployed in high-security environments like Briar and for safety number verification in Signal. The distinct failure mode is user friction. The process requires simultaneous or near-simultaneous coordination, and if the embedded routing information (such as a temporary IP or ephemeral relay) expires before the recipient actuates the token, the bootstrap process fails permanently, requiring a new token exchange. Out-of-band tokens are absolutely essential for the small-private-group case, but they must be structurally hybridized with a robust network substrate to prevent the routing data from becoming stale.

### Hybrid: Reputation-Gated DHT Writes (EigenTrust, Trustchain)

In an effort to mitigate the inherent vulnerabilities of permissionless networks, such as Sybil attacks and data pollution, hybrid models implement reputation gating over DHT writes. Protocols leveraging EigenTrust or Trustchain mechanics require nodes to calculate a global or localized trust score for their peers based on historical interactions and transitive trust graphs. Nodes will only store, route, or serve discovery records for peers that maintain a satisfactory reputation score. While analytically capable of scaling to large networks, the mathematical overhead of converging the principal eigenvectors of the trust matrix across a highly dynamic peer-to-peer network is computationally intense.

Crucially, the metadata-leak profile is extreme. For Trustchain models and localized EigenTrust to function, the social graph and interaction history must be rendered largely public or explicitly verifiable by routing nodes, directly violating the core privacy constraints of the application. Mobile compatibility is exceedingly poor due to the continuous computational overhead of verifying trust proofs and maintaining the graph state. This substrate does not fit the architectural requirements and is profoundly over-engineered for the simple requirement of address resolution.

### Delay-Tolerant Local-First Discovery (Earthstar, Willow)

An emerging paradigm in 2026 focuses on absolute network fragility, assuming that wide-area connectivity is a luxury rather than a guarantee. Protocols like Earthstar and the Willow protocol are engineered around local-first CRDT synchronization, where devices discover each other opportunistically over Local Area Networks (LAN), Bluetooth Low Energy (BLE), or ad-hoc physical proximity. The Willow protocol notably utilizes Private Set Intersection (PSI) cryptography to allow peers to discover shared namespaces without revealing their overall subscriptions to untrusted nodes.

While the metadata privacy of PSI is exceptional, the scale ceiling for wide-area discovery is effectively zero, as these protocols intentionally eschew global routing infrastructure. They are highly resilient to total internet outages but fail to address the fundamental requirement of bootstrapping a connection between two residential ISPs on opposite sides of the globe. This approach is highly recommended as a fallback transport mechanism for physically proximate users but cannot serve as the primary WAN discovery substrate.

### Discovery Substrates Comparison Table

| Substrate Methodology | Fits Public Community? | Fits Small Private Group? | Metadata Leakage Profile | Mobile Execution Fit | Empirical Scale Ceiling |
|---|---|---|---|---|---|
| Mainline DHT (iroh pkarr) | Poor (Resolution Latency) | Strong (Requires Ephemeral Keys) | High (if static identities are used) | Weak (Requires HTTP proxy relays) | >10,000,000 active nodes |
| DNS-Style Signed Records | Strong | Poor (Authoritative Logging) | High (DNS operator logs requests) | Strong (Native OS resolution) | Bound by global DNS cache |
| Federated Rendezvous | Strong | Poor (Server Operator Logging) | High (Explicit social edge logging) | Strong (Standard HTTP polling/push) | Bound by server vertical scale |
| Out-of-Band Invite Tokens | Poor (UX Friction) | Strong | Zero (Perfect forward secrecy) | Strong (User-actuated foreground) | N/A (Outside system bounds) |
| Gossip Overlays (Plumtree) | Poor (Cannot bootstrap) | Poor (Cannot bootstrap) | Moderate (Local mesh visibility) | Weak (Battery drain from heartbeats) | ~10k–50k nodes per mesh |
| Reputation-Gated DHT | Poor (Computational overhead) | Poor (Requires public social graph) | Extreme (Trust graph verification) | Weak (Continuous proof validation) | Mathematically constrained |
| Delay-Tolerant (Willow) | Poor (No WAN routing) | Poor (Requires physical proximity) | Low (Private Set Intersection) | Strong (Opportunistic sync) | Local network bounds |

---

## Q2. State-of-the-Art NAT Traversal in 2026

The promise of decentralized, self-sovereign networks is fundamentally gated by the realities of Network Address Translation (NAT) traversal. The exhaustion of the IPv4 address space has led Internet Service Providers (ISPs) and mobile carriers to aggressively deploy Carrier-Grade NAT (CGNAT) and highly restrictive Endpoint-Dependent Mapping (EDM) firewalls. Establishing direct peer-to-peer connectivity across these barriers requires sophisticated hole-punching choreographies. The 2026 empirical landscape provides definitive clarity on the success rates and failure modes of modern traversal protocols.

### The Empirical Reality of Decentralized Hole Punching

Historically, the peer-to-peer engineering community operated under the "tribal knowledge" that User Datagram Protocol (UDP) was vastly superior to Transmission Control Protocol (TCP) for traversing NATs, owing to the connectionless nature of UDP and the strict state-machine enforcement of TCP firewalls. However, a landmark longitudinal measurement study published in late 2025 detailing the performance of the libp2p Direct Connection Upgrade through Relay (DCUtR) protocol fundamentally challenged this assumption.

Operating across the production InterPlanetary File System (IPFS) network, the study analyzed over 4.4 million traversal attempts originating from more than 85,000 distinct networks globally. The empirical data established a contemporary, conditional success rate of exactly $70\% \pm 7.1\%$ for decentralized hole-punching in permissionless environments. Critically, the study demonstrated that when protocols utilize high-precision, Round-Trip Time (RTT) based synchronization to coordinate simultaneous dial attempts, the success rates for TCP and QUIC (UDP) are statistically indistinguishable, both hovering at the ~70% baseline. The efficiency of modern protocols is incredibly high; when DCUtR successfully negotiates a connection, 97.6% of those connections are established on the very first coordinated attempt.

### Traversal Protocol Comparisons

The state of the art in 2026 is defined by several competing architectures, each optimizing for different points on the latency-versus-reliability spectrum.

**libp2p DCUtR (Direct Connection Upgrade through Relay):** This protocol relies on a globally distributed network of decentralized relays. When two NATted peers wish to connect, they establish a low-bandwidth connection through a shared relay. They then exchange their perceived public addresses and coordinate a precise, synchronized simultaneous open. While highly decentralized and requiring no central coordination server, its success rate is strictly capped around the 70% empirical baseline. Furthermore, the protocol is notoriously brittle when encountering symmetric NATs on both sides of the connection, as it currently lacks advanced port-prediction algorithms required to guess the randomized port allocations of the opposing firewall.

**Hyperswarm UDP Holepunching:** Older frameworks relying predominantly on uncoordinated UDP hole punching historically claimed success rates ranging from 82% to 95% under ideal conditions. However, as symmetric NATs and strict EDM mappings have proliferated across mobile networks, uncoordinated UDP approaches have severely degraded. Hyperswarm and similar architectures often struggle profoundly with the "hairpinning problem," which occurs when two peers situated behind the exact same NAT attempt to connect via their public IP addresses. Without explicit local network discovery protocols (which are out of scope for WAN routing), the NAT router frequently drops the packets, failing to translate the destination back to the internal network.

**Tailscale DERP and iroh 0.98 QUIC:** The industry standard for absolute reliability is defined by Tailscale's architecture, which seamlessly integrates aggressive UDP STUN-based hole punching with automatic fallback to Designated Encrypted Relay for Packets (DERP). Iroh 0.98 adopts this architecture wholesale but translates it into a pure QUIC environment. Iroh leverages the native multiplexing capabilities of QUIC to probe multiple potential paths concurrently without head-of-line blocking. Tailscale's internal metrics indicate that with 1,024 random probes, direct traversal succeeds 98% of the time, scaling to 99.9% with 2,048 probes. However, iroh makes a calculated architectural trade-off: it accepts a minor degree of centralization (reliance on the DERP relay fleet) to guarantee near 100% effective connectivity. If a direct path cannot be punched, iroh seamlessly encapsulates the QUIC packets and streams them over HTTPS to the DERP server, ensuring the application layer (Zenoh) remains entirely agnostic to the transport transition.

**MASQUE / CONNECT-UDP:** Standardized under RFC 9298, the MASQUE protocol allows native UDP packets to be proxied over HTTP/3. While this provides unparalleled reliability for circumventing hostile enterprise proxies and strict network censorship, it is fundamentally a proxying mechanism rather than a direct hole-punching technique. Relying on MASQUE implies that 100% of the traffic must flow through the HTTP/3 proxy, dramatically inflating bandwidth costs and defeating the core peer-to-peer mandate of the application.

### The Failure Modes of iroh's DERP Fallback

While iroh's deterministic NAT traversal is profoundly reliable, empirical testing reveals two specific scenarios where the DERP fallback architecture fails in practice:

**The "Hard Symmetric" Deadlock:** When both devices are situated behind highly restrictive Endpoint-Dependent Mapping (EDM) firewalls that employ aggressive blacklist mechanisms or drop packets from unknown addresses, the simultaneous QUIC probes are treated as packet flooding or DoS attacks. The NATs actively block the incoming packets and randomize the outbound port mapping for every subsequent probe. Iroh correctly recognizes this failure and transitions to DERP. However, if the public n0 relay network is utilized, the traffic is hard-capped at a 4KiB/s steady-stream limit. For a Zenoh application attempting to synchronize massive CRDT event logs after weeks offline, this severe rate limit causes the connection to stall, effectively resulting in an application-layer failure despite the transport layer remaining technically "connected."

**Asymmetric Relay Isolation:** Iroh requires that peers either share a designated "home relay" or maintain connectivity to a heavily interconnected mesh of relays. If Alice's restrictive corporate network only permits outbound HTTPS traffic to an iroh DERP relay in North America, and Bob's mobile carrier only permits traffic to a relay in Europe, and those two relays are not explicitly federated to forward traffic between each other, the fallback mechanism completely disintegrates. The connection fails entirely because there is no mutually reachable rendezvous point.

---

## Q3. Mobile Reconnection Patterns from Production Systems

Deploying persistent peer-to-peer networking on modern mobile operating systems represents a fundamentally hostile engineering environment. Both iOS and Android prioritize battery conservation and aggressive memory management over background network availability. iOS famously enforces a draconian background execution limit, categorically suspending applications and severing all TCP and UDP sockets within approximately thirty seconds of the application transitioning out of the foreground. Android's Doze mode similarly restricts background CPU cycles and network access, meaning that long-standing WebRTC or QUIC connections are inevitably terminated. Consequently, the requirement for a mobile device to reconnect and discover peer addresses after days offline simply cannot rely on the discovery channel itself running over persistent peer connections.

### Analysis of Production Mobile Architectures

**1. Centralized APNs/FCM Proxies (Signal, Matrix, Session)**
The dominant pattern in secure messaging relies entirely on the proprietary push infrastructures of Apple (APNs) and Google (FCM). In applications like Signal and the Matrix Element X client, the application server or homeserver maintains the actual connection state. When a message is routed to the server, the server sends a tiny cryptographic payload to APNs or FCM. The proprietary service wakes the mobile device, which then initiates a brief foreground network request to synchronize state with the server. Matrix specifically utilizes a dedicated, highly configurable push gateway called Sygnal to decouple the homeserver logic from the management of Apple and Google certificates.

While highly reliable, this architecture profoundly leaks metadata. Apple and Google possess granular visibility into exactly when a device receives a message, the volume of traffic, and the IP address of the receiving device. The Session messenger attempts to mitigate this by routing push requests through its decentralized onion-routing network (Oxen), but ultimately, the final hop still delivers the device identifier and IP address directly to Apple's servers.

**2. Zero-Identifier Failures (SimpleX Chat)**
SimpleX Chat represents the extreme vanguard of privacy, operating an architecture with absolutely no persistent user identifiers — relying solely on unidirectional message queues (SMP protocol) over decentralized relays. Because the relay holding the encrypted message has no concept of the user's identity, it cannot natively map the message to an APNs push token. To circumvent this, SimpleX deploys separate, dedicated Notification Servers. However, this highly decoupled architecture has experienced catastrophic reliability issues in production. Throughout 2024 and 2025, iOS routinely throttled or completely dropped push notifications for SimpleX because the OS's internal heuristics penalized the app for receiving frequent background "wake up" signals that did not immediately translate to user-facing alerts. This provides a stark warning: attempting to bypass Apple's intended push notification UX flows severely compromises application reliability.

**3. The Decoupled "Zero-Push" Intermediary (Berty, Veilid)**
To prevent the central infrastructure from compiling a social graph, peer-to-peer applications must utilize an untrusted intermediary push mechanism. The Berty messenger achieves this via a component called "Zero-Push". In this architecture, push tokens are mathematically decoupled from Berty account IDs. When Alice wishes to wake Bob's device, she encrypts a wake-up payload and sends it to the Zero-Push server alongside Bob's anonymous push token. The Zero-Push server forwards the token to APNs/FCM but possesses zero ability to decrypt the content or ascertain the identities of the participants. Similarly, the Veilid network utilizes a "WTF server" to handle push notifications. Devices transmit an anonymous token to the WTF server without disclosing their IP addresses, completely obscuring the link between the user's Veilid node ID and their physical device token.

**4. Decentralized Multiplexing (UnifiedPush)**
For adversarial environments, particularly de-Googled Android devices running custom ROMs (e.g., GrapheneOS) that lack FCM, falling back to constant background polling drains the battery in a matter of hours. The 2026 solution to this is UnifiedPush, a set of open specifications that allows the user to choose their own push notification distributor. Applications like Tox (via the ToxProxy and TRIfA companion apps), Matrix, and FluffyChat support UnifiedPush natively. The crucial advantage is multiplexing: instead of ten applications maintaining ten separate background connections, a single, highly optimized distributor app (such as ntfy) maintains one persistent socket to a self-hosted server. When a message arrives, the distributor wakes the specific application locally via an Android broadcast intent.

**5. Delay-Tolerant Apathy (Earthstar, Willow)**
Local-first protocols like Earthstar and Willow entirely bypass the mobile push-gate by simply accepting extreme latency. They do not attempt to maintain persistent connections or wake sleeping devices over the WAN. Instead, they rely on the user to manually bring the application to the foreground, at which point the CRDTs opportunistically discover peers over the local network or internet and synchronize the missing state. While excellent for offline resilience, this is unviable for a real-time collaboration application.

### Architectural Recommendations for Mobile Reconnection

For the proposed iroh/Zenoh stack, relying purely on the discovery channel running over peer connections will absolutely fail on mobile devices. The architecture must explicitly require a push provider to bridge the offline gap. The optimal solution is a tripartite approach:

**For iOS:** Deploy a stateless, untrusted Zero-Push gateway. When Bob sends a message, it is routed through the Zero-Push gateway to APNs. Crucially, the app must implement an iOS Notification Service Extension (NSE). The NSE receives the encrypted payload in the background, decrypts it using a pre-shared keychain, and generates a native OS notification. The heavy iroh/Zenoh Rust stack is not initialized until the user physically taps the notification, perfectly satisfying Apple's memory and CPU constraints.

**For Googled Android:** Utilize FCM via the same untrusted Zero-Push gateway mechanism, executing the decryption via a background worker.

**For De-Googled Android:** Natively integrate the UnifiedPush API. The iroh application registers as a UnifiedPush receiver, allowing users to multiplex the wake-up signals through their distributor of choice (e.g., a self-hosted ntfy server), preserving battery life while circumventing Google's proprietary infrastructure.

---

## Q4. Iroh-Specific Scale Economics

Selecting iroh as the foundational transport layer shifts the economic burden of the application from maintaining global routing tables (as in DHTs) or heavy state resolution databases (as in Matrix) to provisioning sheer bandwidth capacity for the DERP relay fallback mechanism. Understanding the cost curve and operational limits of this infrastructure is paramount for designing a system capable of reaching billion-user scale.

### The Operational Risks of the n0-Hosted Public DERP Network

By default, iroh endpoints seamlessly fall back to the public relay network operated by n0.computer when direct hole punching fails. While invaluable for prototyping and development, relying on this free public infrastructure for a production federated application introduces severe operational risks. As of iroh version 0.29, the network enforces strict rate limits for incoming client connections: sustained throughput is throttled to 4KiB/s, with a maximum burst capacity of 16MiB.

At a scale of $10^4$ users, the application would likely function with minor latency spikes, assuming the majority of connections successfully hole-punch. However, as the user base scales to $10^6$ or $10^8$ devices, the law of large numbers dictates that hundreds of thousands of concurrent users will inevitably fall behind strict symmetric NATs, forcing their traffic onto the public relays. The continuous gossiping of Zenoh CRDT payloads over WAN would rapidly exhaust the 4KiB/s limit. The immediate consequences would be aggressive packet dropping, massive latency tails resulting in desynchronized collaborative state, and ultimately, transport-layer timeouts that sever the connection entirely. The public n0 network cannot support the application layer at scale.

### Self-Hosted iroh DERP Relays: Capacity and Cost Curves

To achieve production reliability, the architecture must deploy its own fleet of dedicated, self-hosted iroh relays. Because iroh relay servers are architecturally stateless — requiring no database synchronization, no complex state migration, and no inter-node replication — they scale horizontally with extreme efficiency. If a relay crashes, clients gracefully and automatically transition to another relay in their configured map without data loss.

The cost curve for self-hosting this infrastructure relies entirely on compute efficiency and outbound bandwidth pricing.

- **Hetzner:** Provides the absolute most cost-efficient baseline for deployments centered in Europe and the US East Coast. Robust micro-Virtual Machines capable of handling thousands of concurrent multiplexed QUIC connections start at approximately \$5 to \$6 per month, with exceptionally generous bandwidth allocations.
- **Fly.io:** Optimizes for global edge deployment, which is critical for minimizing the geographic latency tails that degrade real-time collaboration UX. Fly.io utilizes a pay-as-you-go model, charging for the base micro-VM, roughly \$5 per month per additional gigabyte of RAM, and metering outbound bandwidth.
- **Scale Projections:** Assume a highly active community of $10^6$ concurrent users. Empirical NAT data suggests that a maximum of 10% (100,000 users) will strictly require DERP fallback. Assuming a Zenoh CRDT keep-alive and event propagation baseline of 1 kbps per user, the relay fleet must sustain approximately 100 Mbps of continuous, steady-state throughput. A heavily decentralized fleet of 15-20 edge VMs deployed globally on Fly.io, or a denser cluster of 10 Hetzner servers, could effortlessly sustain this capacity for less than \$500 per month. The cost per million concurrent connections is remarkably trivial compared to the massive database scaling costs associated with federated architectures like Matrix.

### pkarr over Mainline DHT: Throughput and Resolution Dynamics

Iroh integrates discovery directly into the BitTorrent Mainline DHT via the pkarr module. pkarr utilizes the BEP44 standard to write mutable, signed records to the DHT, addressing the payload directly to an Ed25519 public key.

- **Write/Read Throughput:** The absolute limitation of the BEP44 standard is a hard limit of 1000 bytes per record. This constraint is immutably enforced by the 10-million node network to prevent storage abuse. While perfectly sufficient to encode a cryptographic signature, an iroh NodeId, and a DERP home-relay URL, it strictly prevents developers from embedding larger community catalogs, avatars, or rich metadata directly into the discovery channel.
- **Time-to-Resolve:** The performance of pkarr is highly bifurcated. Due to aggressive caching by clients and HTTP relays, resolution of a heavily queried key often requires only a few milliseconds. However, in the event of a cache miss — which is guaranteed to occur when a device wakes up after weeks offline and queries a completely new, ephemeral key — traversing the global Kademlia structure to locate the nodes responsible for that specific key space can take several seconds.
- **Censorship Resistance:** The sheer size of the Mainline DHT (10M+ nodes) makes targeted censorship or eclipse attacks virtually impossible for any single entity. However, this persistence is ephemeral; BEP44 records are systematically dropped by the network after a few hours to manage storage capacity. Devices must proactively wake up and republish their routing information periodically, a factor that must be deeply integrated into the mobile battery management strategy.

---

## Q5. Comparison of Competing P2P Stacks for Indie Apps (2026)

Evaluating the landscape of peer-to-peer networking stacks for indie, federated applications reveals starkly different engineering philosophies. The choice of stack dictates not only the technical capabilities of the application but the sheer volume of supporting infrastructure the engineering team must rebuild themselves.

| Evaluation Criterion | iroh 0.98 (n0) | libp2p (Protocol Labs) | Veilid (Cult of the Dead Cow) | Earthstar / Willow | Hyperswarm / Pear |
|---|---|---|---|---|---|
| NAT Traversal Success | ~99.9% via QUIC + deterministic DERP fallback. Near-instant path migration. | ~70% pure P2P via DCUtR. High failure rates in symmetric topologies. | Moderate. Supports UDP/TCP/WS, reverse connections, but lacks native global relay fallback. | N/A. Explicitly local-first. Manual IP/relay entry required for WAN. | High for UDP, but fails violently on strict EDM firewalls and hairpinning. |
| Discovery Scale | Piggybacks on 10M+ node BitTorrent DHT via pkarr. | Global Kademlia DHT, highly active but prone to lookup latency. | Internal routing table algorithms; resilient but insular. | Private Set Intersection (PSI) for local discovery. | Centralized bootstrap nodes for Kademlia DHT. |
| Identity Model | Clean Ed25519 NodeId mapped to routing data. | Complex Multihash PeerIds; heavy cryptographic abstraction. | Highly typed, anonymous, ephemeral routing identities. | Ed25519 capabilities; strict separation of read/write access. | Simple Ed25519 keys. |
| Mobile Fit | Heavy Rust binary. Requires external push-gateway architecture. | Massive battery drain from constant DHT participation; very poor. | Built-in "WTF" push-notification server logic for mobile sleep. | Excellent. Designed specifically for intermittent, delay-tolerant syncing. | Poor. NodeJS/JS runtime overhead is prohibitive for background execution. |
| Ecosystem Maturity | Rapid enterprise adoption (e.g., Nous distributed AI training). Production ready. | The absolute standard of Web3. Massive tooling, but bloated and complex. | Niche, heavily focused on anonymity rather than general collaboration tooling. | Highly academic, experimental phase. Specifications still finalizing. | Mature within the Holepunch ecosystem, but isolated from broader standards. |
| License | Apache 2.0 / MIT | MIT / Dual Licenses | Mozilla Public License (MPL) | GPL / MIT | MIT |

**Verdict:** For an indie team in 2026, iroh is unequivocally the closest to "production-ready." The pragmatic decision to prioritize 99% connectivity via minor centralization (stateless DERP relays) over ideological, 70%-reliable pure decentralization (libp2p DCUtR) is exactly what allows modern P2P applications to compete with the UX of centralized platforms like Discord or Signal.

**The Open Gaps (The Rebuild Burden):** If an indie team selects iroh, they are securing flawless transport and discovery, but they are entirely on their own for mobile lifecycle management. Iroh currently lacks native WebRTC support for browser environments without custom coordination, and crucially, it offers no built-in primitives for push notifications. The indie team must completely design, build, and host the Zero-Push Gateway infrastructure and the iOS Notification Service Extension bridging logic from scratch.

---

## Q6. The Bootstrap-Cost-vs-UX-vs-Privacy Frontier

Establishing the first ever connection for a small, private group across a massive Wide Area Network represents the most difficult trilemma in decentralized engineering. The architecture must perfectly balance User Experience (frictionless connection), Metadata Privacy (preventing observers from mapping the social graph), and Infrastructure Cost (minimizing hosted server requirements). The 2026 frontier presents four distinct points along this spectrum.

**Pure Out-of-Band (OOB) Addressing:**
- *Mechanism:* Alice utilizes a secondary secure channel (such as a Signal text message) to send Bob a complete iroh connection string, explicitly detailing her current IP address, NodeId, and designated DERP relay.
- *UX:* Unacceptably high friction. Network addresses are highly volatile; if Alice's device switches from Wi-Fi to cellular before Bob clicks the link, the IP address becomes stale, and the connection fails permanently.
- *Privacy:* Perfect. Because the routing data never touches a public directory or DHT, no external observer can ever learn that the social edge exists.
- *Cost:* \$0.

**Federated Rendezvous (The Matrix Model):**
- *Mechanism:* Alice and Bob agree out-of-band to utilize a specific, highly available rendezvous server. Alice posts a one-time cryptographic challenge and her routing data to the server. Bob queries the server, answers the challenge, and retrieves the routing data.
- *UX:* Flawless. The server provides a stable, highly available endpoint that completely absorbs network volatility.
- *Privacy:* Extremely poor. The operator of the rendezvous server possesses explicit logs showing Alice's IP and Bob's IP accessing the exact same cryptographic challenge at the same time, permanently recording the social edge.
- *Cost:* Minimal, but strictly non-zero, requiring the permanent hosting and maintenance of the rendezvous infrastructure.

**Static DHT Rendezvous:**
- *Mechanism:* Alice uses her permanent, static Ed25519 identity key to publish her routing information (home relay and IP) to the Mainline DHT via pkarr. Bob, knowing Alice's static key, queries the DHT.
- *UX:* Very smooth. The DHT provides a massive, globally available key-value store.
- *Privacy:* Unacceptable for high-threat models. Academic researchers and malicious DHT crawlers systematically record the IP addresses publishing to specific public keys. An observer can easily map Alice's geographic movements and definitively prove she is operating on the network.
- *Cost:* \$0, leveraging the free BitTorrent network.

**The 2026 Frontier: Ephemeral DHT Rendezvous (Hybrid OOB):**
- *Mechanism:* Alice and Bob execute an initial out-of-band pairing (e.g., scanning a QR code) that exchanges a high-entropy, 256-bit cryptographic seed ($S$). Using a Cryptographically Secure Pseudorandom Number Generator (CSPRNG) and a Hash-based Key Derivation Function (HKDF), both devices derive a deterministic sequence of ephemeral, time-boxed Ed25519 keypairs (e.g., $K_{eph} = \text{HKDF}(S, \text{epoch\_week\_42})$). Alice uses iroh pkarr to publish her current routing data to the DHT entirely under the guise of this temporary $K_{eph}$. Bob calculates $K_{eph}$ for the current week and queries the DHT.
- *UX:* Flawless after the initial QR scan. Discovery is entirely automatic in the background, even if both devices completely change networks and relays months later.
- *Privacy:* Exceptionally strong. To any global DHT observer or crawler, the published BEP44 records appear as random, disconnected cryptographic noise. The social edge is never leaked, and the continuous rotation of the epoch provides forward secrecy on the discovery channel.
- *Cost:* \$0, heavily utilizing existing pkarr infrastructure.

---

## Concrete Proposed Designs (Phase 2 Architectures)

To systematically bridge the gaps between the core iroh/Zenoh transport stack and the strict constraints of mobile deployment and private-group bootstrapping, the following end-to-end architectures are proposed for Phase 2 implementation.

### Design A: The Cipher-Swarm (Ephemeral Pkarr Bootstrap)

**Target Objective:** Solving the first-contact bootstrap for small/private groups across WAN and ensuring long-term offline reconnection without central servers.

**Architecture Sketch:**

1. **Initial Seed Exchange:** Alice generates a secure 256-bit seed phrase and shares it with Bob via an out-of-band QR code scan or an encrypted Signal message.
2. **Deterministic Derivation:** Both devices feed the seed into an HKDF, mathematically deriving a synchronized sequence of ephemeral Ed25519 public/private keypairs, mapped to specific temporal epochs (e.g., rotating every 7 days).
3. **Anonymized Publication:** When Alice wakes from a long offline period, her device utilizes iroh's pkarr subsystem. It generates a BEP44 record containing her true, permanent iroh NodeId and her newly selected self-hosted DERP relay URL. She signs and publishes this record to the Mainline DHT under the identity of the current week's ephemeral public key.
4. **Blind Resolution:** Bob's device wakes up, independently calculates the current week's ephemeral public key, and queries the Mainline DHT. He retrieves the BEP44 record, parses Alice's true NodeId and routing data, and initiates an iroh QUIC connection.
5. **State Synchronization:** The iroh connection stabilizes, and Zenoh pub/sub events automatically resume synchronization.

**Trade-offs:**
- *Pros:* Achieves perfect zero-metadata leakage to central servers. Inherits the massive, infinite horizontal scale of the 10-million node BitTorrent DHT. Effortlessly survives total network changes on both sides of the connection.
- *Cons:* DHT resolution on a cache miss can incur several seconds of latency. Vulnerable to ephemeral record drop-off; because BEP44 records expire after a few hours, Alice must actively republish her data if Bob is delayed in querying the network.

**Estimated Build Effort:** Moderate (3 to 4 weeks of dedicated engineering). The design requires custom cryptographic wrapping around the existing iroh-pkarr Rust implementation but leverages entirely existing network primitives.

### Design B: The Zero-Push Fleet (Decoupled Notification Gateway)

**Target Objective:** Solving mobile reconnection (hours to days offline) within the draconian background execution limits of iOS and Android.

**Architecture Sketch:**

1. **Infrastructure Provisioning:** Deploy a highly available, untrusted, stateless "Zero-Push" gateway cluster alongside the self-hosted DERP relay fleet.
2. **Anonymous Registration:** When Alice's mobile application transitions to the background, it requests a standard device token from Apple APNs or a UnifiedPush distributor. The app generates a temporary, highly random UUID and registers a mapping of `UUID -> DeviceToken` strictly with the Zero-Push gateway.
3. **In-Band Signaling:** Before her device enters deep sleep, Alice transmits this UUID and an ephemeral symmetric encryption key to Bob over their existing, active iroh/Zenoh channel.
4. **Encrypted Wake-Up:** Days later, when Bob wishes to send a message, he cannot reach Alice via P2P. He contacts the Zero-Push gateway via a standard HTTP request, providing the UUID and a ciphertext payload (encrypted with the ephemeral symmetric key) indicating that a Zenoh synchronization is waiting.
5. **Local Decryption:** The gateway forwards the opaque ciphertext to APNs. Alice's device receives the push. Crucially, an iOS Notification Service Extension (NSE) intercepts the payload in the background, decrypts it locally using the shared keychain, and triggers a localized UI alert. Only when Alice physically taps the alert does the heavy iroh/Zenoh Rust stack initialize and establish the QUIC connection via the DERP relay to pull the pending state.

**Trade-offs:**
- *Pros:* Perfectly conforms to aggressive mobile OS battery policies and memory limits. Provides the frictionless, real-time UX expected of modern chat applications. Mathematically decouples the user's cryptographic identity from their physical push tokens.
- *Cons:* Strictly requires the provisioning, funding, and maintenance of the push gateway infrastructure. Introduces an unavoidable reliance on Apple and Google's centralized infrastructure (unless the UnifiedPush pathway is explicitly utilized on de-Googled Android).

**Estimated Build Effort:** High (6 to 8 weeks). This design requires deep, platform-specific engineering, including native iOS (Swift) and Android (Kotlin) Notification Service Extension modules, alongside the deployment of a lightweight, highly concurrent Golang or Rust push gateway server.

### Design C: The Delay-Tolerant Mailbox (Zenoh Overlay)

**Target Objective:** Re-establishing contact for highly asynchronous, highly mobile users operating under privacy-extremist threat models where centralized push notifications are strictly prohibited.

**Architecture Sketch:**

1. **Detection of Isolation:** Bob attempts to connect to Alice but determines she is entirely unreachable (direct hole punching fails, DERP relay probes time out, and no Push gateway UUID is available).
2. **Encrypted Stash:** Bob generates a "wake-up / routing update" packet containing his latest IP address and relay topology, encrypts it heavily against Alice's public key, and uploads it to a highly available, untrusted "Mailbox" server operating within their community using Zenoh over HTTP.
3. **Opportunistic Retrieval:** A week later, Alice connects her laptop to a public Wi-Fi network in a new city. Upon gaining connectivity, her iroh instance connects to the Mailbox server, downloads the pending encrypted updates, decrypts them to discover Bob's new routing architecture, and initiates a direct iroh connection.

**Trade-offs:**
- *Pros:* The ultimate solution for deep-offline mobility and absolute privacy. Entirely bypasses all mobile push-notification gates and completely severs reliance on Apple/Google.
- *Cons:* Fundamentally destroys real-time UX; messages are only delivered when the recipient actively decides to check the network. Requires the community to host a highly available storage server, introducing persistent disk storage costs that stateless DERP relays avoid.

**Estimated Build Effort:** Low to Moderate (2 to 3 weeks). This architecture can be rapidly prototyped entirely as a data store overlay utilizing the existing Zenoh CRDT structures.

---

## Open Questions for Prototyping

To fully validate the proposed Phase 2 architectures, several critical assumptions require immediate empirical prototyping:

1. **Mainline DHT Write Reliability from Hostile Mobile NATs:** The BEP44 publication process via pkarr heavily relies on HTTP proxy relays because mobile operating systems and strict cellular firewalls frequently restrict the raw, sustained UDP sockets required for direct DHT participation. Prototyping is urgently required to measure the true latency, failure rate, and battery consumption of attempting to publish an ephemeral discovery record from an Android device operating natively on a heavily congested 5G Carrier-Grade NAT.
2. **iOS Notification Service Extension (NSE) Execution Windows:** While the iOS NSE architecture theoretically allows for the local decryption of push payloads without waking the primary application, can it be engineered to reliably initiate a brief, headless iroh connection? The objective would be to pull lightweight Zenoh state updates directly in the background before the operating system terminates the extension. Apple enforces severe, largely undocumented memory limits for NSEs (historically capping around 12MB). This tight constraint may violently conflict with the memory overhead required to boot the full iroh QUIC and Zenoh Rust stack in a headless context.
3. **Asymmetric DERP Failover Latency:** The theoretical routing matrix requires rigorous testing. If Alice defaults to Self-Hosted Relay A (located in Frankfurt), and Bob defaults to Self-Hosted Relay B (located in Singapore), iroh's fallback mechanisms require one peer to successfully cross over to the other's designated relay. Prototyping must definitively determine the exact timeout thresholds, packet loss probabilities, and overall latency penalties incurred when negotiating these highly asymmetric, transcontinental relay paths under heavily simulated network congestion.
