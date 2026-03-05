import type { Peer, Message, NavNode } from './types';

export const peers: Peer[] = [
  { address: 'a1b2c3d4', displayName: 'Alice', avatarUrl: undefined },
  { address: 'e5f6g7h8', displayName: 'Bob', avatarUrl: undefined },
  { address: 'i9j0k1l2', displayName: 'Carol', avatarUrl: undefined },
  { address: 'm3n4o5p6', displayName: 'Dave', avatarUrl: undefined },
];

const hour = 3600_000;
const base = Date.now() - 4 * hour;

export const messages: Message[] = [
  {
    id: 'msg-01',
    sender: peers[0],
    text: 'Hey everyone, just pushed the new transport layer changes.',
    timestamp: base,
    media: [],
  },
  {
    id: 'msg-02',
    sender: peers[1],
    text: 'Nice! Here is the PR for review.',
    timestamp: base + 5 * 60_000,
    media: [
      {
        id: 'media-01',
        type: 'link',
        url: 'https://github.com/zeblithic/harmony/pull/35',
        title: 'feat: durable workflow execution engine',
        domain: 'github.com',
      },
    ],
  },
  {
    id: 'msg-03',
    sender: peers[2],
    text: 'Looking at the benchmarks, throughput is up 3x on the routing tier.',
    timestamp: base + 12 * 60_000,
    media: [
      {
        id: 'media-02',
        type: 'image',
        url: 'https://placehold.co/600x400/313338/f2f3f5?text=Benchmark+Chart',
        title: 'Routing throughput comparison',
      },
    ],
  },
  {
    id: 'msg-04',
    sender: peers[0],
    text: 'That looks great. The adaptive fuel scaling really helped.',
    timestamp: base + 15 * 60_000,
    media: [],
  },
  {
    id: 'msg-05',
    sender: peers[3],
    text: 'Here is the config I used for the starvation test:',
    timestamp: base + 20 * 60_000,
    media: [
      {
        id: 'media-03',
        type: 'code',
        title: 'tier_schedule.toml',
        content: `[tier_schedule]
router_max_per_tick = 10
storage_max_per_tick = 5
starvation_threshold = 8

[adaptive_compute]
high_water = 50
floor_fraction = 0.1`,
      },
    ],
  },
  {
    id: 'msg-06',
    sender: peers[1],
    text: 'I ran the same test with the W-TinyLFU cache enabled.',
    timestamp: base + 30 * 60_000,
    media: [],
  },
  {
    id: 'msg-07',
    sender: peers[2],
    text: 'Check out the cache hit rates — much better with the frequency sketch.',
    timestamp: base + 35 * 60_000,
    media: [
      {
        id: 'media-04',
        type: 'image',
        url: 'https://placehold.co/600x300/313338/f2f3f5?text=Cache+Hit+Rates',
        title: 'W-TinyLFU cache hit rate over time',
      },
    ],
  },
  {
    id: 'msg-08',
    sender: peers[0],
    text: 'Has anyone tested the Reticulum interop with the latest packet format changes?',
    timestamp: base + hour,
    media: [],
  },
  {
    id: 'msg-09',
    sender: peers[3],
    text: 'Yes, all 14 cross-language tests pass. Here is the test output.',
    timestamp: base + hour + 5 * 60_000,
    media: [
      {
        id: 'media-05',
        type: 'link',
        url: 'https://github.com/zeblithic/harmony/actions/runs/123456',
        title: 'CI: All interop tests passing',
        domain: 'github.com',
      },
    ],
  },
  {
    id: 'msg-10',
    sender: peers[1],
    text: 'Perfect. The identity derivation path is byte-identical to Python Reticulum now.',
    timestamp: base + hour + 10 * 60_000,
    media: [],
  },
  {
    id: 'msg-11',
    sender: peers[2],
    text: 'I documented the address derivation flow:',
    timestamp: base + hour + 20 * 60_000,
    media: [
      {
        id: 'media-06',
        type: 'code',
        title: 'address_derivation.rs',
        content: `// Address = SHA256(X25519_pub || Ed25519_pub)[:16]
let mut hasher = Sha256::new();
hasher.update(x25519_public.as_bytes());
hasher.update(ed25519_public.as_bytes());
let hash = hasher.finalize();
let address: [u8; 16] = hash[..16]
    .try_into()
    .expect("SHA256 output is 32 bytes");`,
      },
    ],
  },
  {
    id: 'msg-12',
    sender: peers[0],
    text: 'Clean. Next up is the Zenoh pub/sub integration for presence.',
    timestamp: base + 2 * hour,
    media: [],
  },
  {
    id: 'msg-13',
    sender: peers[3],
    text: 'I have a draft of the liveliness token flow.',
    timestamp: base + 2 * hour + 15 * 60_000,
    media: [
      {
        id: 'media-07',
        type: 'link',
        url: 'https://github.com/zeblithic/harmony/wiki/Liveliness-Tokens',
        title: 'Liveliness Token Design',
        domain: 'github.com',
      },
    ],
  },
  {
    id: 'msg-14',
    sender: peers[1],
    text: 'Looks solid. The key expression hierarchy makes sense for our namespace.',
    timestamp: base + 2 * hour + 25 * 60_000,
    media: [],
  },
  {
    id: 'msg-15',
    sender: peers[2],
    text: 'Agreed. Let us get this merged and start on the voice engine next.',
    timestamp: base + 3 * hour,
    media: [],
  },
];

const now = Date.now();

export const navNodes: NavNode[] = [
  // Top-level: Work folder
  {
    id: 'work',
    parentId: null,
    type: 'folder',
    name: 'Work',
    expanded: true,
    sortOrder: 'activity',
    unreadCount: 0,
    unreadLevel: 'none',
    lastActivity: now - 10 * 60_000,
  },
  // Work > Harmony Dev folder
  {
    id: 'harmony-dev',
    parentId: 'work',
    type: 'folder',
    name: 'Harmony Dev',
    expanded: true,
    sortOrder: 'activity',
    unreadCount: 0,
    unreadLevel: 'none',
    lastActivity: now - 10 * 60_000,
  },
  // Work > Harmony Dev > #general
  {
    id: 'general',
    parentId: 'harmony-dev',
    type: 'channel',
    name: 'general',
    expanded: false,
    unreadCount: 3,
    unreadLevel: 'standard',
    lastActivity: now - 5 * 60_000,
  },
  // Work > Harmony Dev > #crypto
  {
    id: 'crypto',
    parentId: 'harmony-dev',
    type: 'channel',
    name: 'crypto',
    expanded: false,
    unreadCount: 0,
    unreadLevel: 'none',
    lastActivity: now - 2 * hour,
  },
  // Work > Harmony Dev > #transport
  {
    id: 'transport',
    parentId: 'harmony-dev',
    type: 'channel',
    name: 'transport',
    expanded: false,
    unreadCount: 1,
    unreadLevel: 'quiet',
    lastActivity: now - 30 * 60_000,
  },
  // Work > IPFS Crew folder
  {
    id: 'ipfs-crew',
    parentId: 'work',
    type: 'folder',
    name: 'IPFS Crew',
    expanded: true,
    sortOrder: 'alphabetical',
    unreadCount: 0,
    unreadLevel: 'none',
    lastActivity: now - 45 * 60_000,
  },
  // Work > IPFS Crew > #mesh
  {
    id: 'mesh',
    parentId: 'ipfs-crew',
    type: 'channel',
    name: 'mesh',
    expanded: false,
    unreadCount: 0,
    unreadLevel: 'none',
    lastActivity: now - 3 * hour,
  },
  // Work > IPFS Crew > #routing
  {
    id: 'routing',
    parentId: 'ipfs-crew',
    type: 'channel',
    name: 'routing',
    expanded: false,
    unreadCount: 2,
    unreadLevel: 'loud',
    lastActivity: now - 15 * 60_000,
  },
  // Work > Alice (DM)
  {
    id: 'alice-dm',
    parentId: 'work',
    type: 'dm',
    name: 'Alice',
    expanded: false,
    unreadCount: 0,
    unreadLevel: 'none',
    lastActivity: now - hour,
    peer: peers[0],
  },
  // Top-level: Friends folder
  {
    id: 'friends',
    parentId: null,
    type: 'folder',
    name: 'Friends',
    expanded: true,
    sortOrder: 'pinned',
    unreadCount: 0,
    unreadLevel: 'none',
    lastActivity: now - 20 * 60_000,
  },
  // Friends > Bob (DM)
  {
    id: 'bob-dm',
    parentId: 'friends',
    type: 'dm',
    name: 'Bob',
    expanded: false,
    unreadCount: 0,
    unreadLevel: 'none',
    lastActivity: now - 2 * hour,
    peer: peers[1],
  },
  // Friends > Carol (DM)
  {
    id: 'carol-dm',
    parentId: 'friends',
    type: 'dm',
    name: 'Carol',
    expanded: false,
    unreadCount: 1,
    unreadLevel: 'standard',
    lastActivity: now - 10 * 60_000,
    peer: peers[2],
  },
  // Top-level: Eve (DM)
  {
    id: 'eve-dm',
    parentId: null,
    type: 'dm',
    name: 'Eve',
    expanded: false,
    unreadCount: 0,
    unreadLevel: 'none',
    lastActivity: now - 3 * hour,
    peer: { address: 'q7r8s9t0', displayName: 'Eve' },
  },
];
