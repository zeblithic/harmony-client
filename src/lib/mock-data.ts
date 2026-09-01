import type { Profile, Message, NavNode, VineVideo } from './types';

export const peers: Profile[] = [
  { address: 'a1b2c3d4', displayName: 'Alice', statusText: 'Working on transport layer' },
  { address: 'e5f6g7h8', displayName: 'Bob' },
  { address: 'i9j0k1l2', displayName: 'Carol', statusText: 'AFK until tomorrow' },
  { address: 'm3n4o5p6', displayName: 'Dave', statusText: 'Reviewing PRs' },
];

const hour = 3600_000;
const base = Date.now() - 4 * hour;

export const messages: Message[] = [
  {
    id: 'msg-01',
    sender: peers[0],
    text: 'Hey everyone, just pushed the new transport layer changes.',
    timestamp: base,
    priority: 'standard',
  },
  {
    id: 'msg-02',
    sender: peers[1],
    text: 'Nice! Here is the PR for review.',
    timestamp: base + 5 * 60_000,
    priority: 'standard',
  },
  // Thread reply to msg-02 (Bob's PR)
  {
    id: 'msg-02-r1',
    sender: peers[2], // Carol
    text: 'Reviewed — looks good, left a few comments on the error handling.',
    timestamp: base + 8 * 60_000,
    priority: 'standard',
    replyTo: 'msg-02',
  },
  {
    id: 'msg-02-r2',
    sender: peers[0], // Alice
    text: 'Thanks Carol, I will address those today.',
    timestamp: base + 10 * 60_000,
    priority: 'standard',
    replyTo: 'msg-02',
  },
  {
    id: 'msg-03',
    sender: peers[2],
    text: 'Looking at the benchmarks, throughput is up 3x on the routing tier.',
    timestamp: base + 12 * 60_000,
    priority: 'standard',
  },
  {
    id: 'msg-04',
    sender: peers[0],
    text: 'That looks great. The adaptive fuel scaling really helped.',
    timestamp: base + 15 * 60_000,
    priority: 'quiet',
  },
  {
    id: 'msg-05',
    sender: peers[3],
    text: 'Here is the config I used for the starvation test:',
    timestamp: base + 20 * 60_000,
    priority: 'standard',
  },
  {
    id: 'msg-06',
    sender: peers[1],
    text: 'I ran the same test with the W-TinyLFU cache enabled.',
    timestamp: base + 30 * 60_000,
    priority: 'quiet',
  },
  {
    id: 'msg-07',
    sender: peers[2],
    text: 'Check out the cache hit rates — much better with the frequency sketch.',
    timestamp: base + 35 * 60_000,
    priority: 'standard',
  },
  {
    id: 'msg-08',
    sender: peers[0],
    text: 'Has anyone tested the Reticulum interop with the latest packet format changes?',
    timestamp: base + hour,
    priority: 'loud',
  },
  // Thread reply to msg-08 (interop question)
  {
    id: 'msg-08-r1',
    sender: peers[3], // Dave
    text: 'Running the full suite now, will post results shortly.',
    timestamp: base + hour + 2 * 60_000,
    priority: 'standard',
    replyTo: 'msg-08',
  },
  {
    id: 'msg-08-r2',
    sender: peers[2], // Carol
    text: 'I tested the identity path — byte-identical to Python.',
    timestamp: base + hour + 3 * 60_000,
    priority: 'standard',
    replyTo: 'msg-08',
  },
  {
    id: 'msg-08-r3',
    sender: peers[0], // Alice
    text: 'Excellent — that confirms the HKDF path is correct too.',
    timestamp: base + hour + 4 * 60_000,
    priority: 'quiet',
    replyTo: 'msg-08',
  },
  {
    id: 'msg-09',
    sender: peers[3],
    text: 'Yes, all 14 cross-language tests pass. Here is the test output.',
    timestamp: base + hour + 5 * 60_000,
    priority: 'standard',
  },
  {
    id: 'msg-10',
    sender: peers[1],
    text: 'Perfect. The identity derivation path is byte-identical to Python Reticulum now.',
    timestamp: base + hour + 10 * 60_000,
    priority: 'quiet',
  },
  {
    id: 'msg-11',
    sender: peers[2],
    text: 'I documented the address derivation flow:',
    timestamp: base + hour + 20 * 60_000,
    priority: 'standard',
  },
  {
    id: 'msg-12',
    sender: peers[0],
    text: 'Clean. Next up is the Zenoh pub/sub integration for presence.',
    timestamp: base + 2 * hour,
    priority: 'standard',
  },
  {
    id: 'msg-13',
    sender: peers[3],
    text: 'I have a draft of the liveliness token flow.',
    timestamp: base + 2 * hour + 15 * 60_000,
    priority: 'standard',
  },
  {
    id: 'msg-14',
    sender: peers[1],
    text: 'Looks solid. The key expression hierarchy makes sense for our namespace.',
    timestamp: base + 2 * hour + 25 * 60_000,
    priority: 'quiet',
  },
  {
    id: 'msg-15',
    sender: peers[2],
    text: 'Agreed. Let us get this merged and start on the voice engine next.',
    timestamp: base + 3 * hour,
    priority: 'standard',
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
    mentionCount: 0,
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
    mentionCount: 0,
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
    mentionCount: 0,
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
    mentionCount: 0,
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
    mentionCount: 0,
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
    mentionCount: 0,
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
    mentionCount: 0,
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
    mentionCount: 0,
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
    mentionCount: 0,
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
    mentionCount: 0,
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
    mentionCount: 0,
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
    mentionCount: 0,
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
    mentionCount: 0,
    unreadLevel: 'none',
    lastActivity: now - 3 * hour,
    peer: { address: 'q7r8s9t0', displayName: 'Eve' },
  },
];

const vineBase = Date.now() / 1000 - 3600;

export const vineVideos: VineVideo[] = [
  {
    id: 'vine-01',
    creatorAddress: 'a1b2c3d4',
    creatorName: 'Alice',
    createdAt: vineBase,
    videoCid: 'cid-video-alice-01',
    title: 'Transport layer demo',
    viewed: false,
    // ZEB-671: mock Discover provenance so dev mode exercises the
    // degree chips + via lines + Tune sheet (graph-only Discover).
    degree: 2,
    via: ['e5f6g7h8'],
  },
  {
    id: 'vine-02',
    creatorAddress: 'e5f6g7h8',
    creatorName: 'Bob',
    createdAt: vineBase + 120,
    videoCid: 'cid-video-bob-01',
    title: 'Mesh routing in action',
    viewed: true,
    degree: 2,
    via: ['i9j0k1l2'],
  },
  {
    id: 'vine-03',
    creatorAddress: 'i9j0k1l2',
    creatorName: 'Carol',
    createdAt: vineBase + 300,
    videoCid: 'cid-video-carol-01',
    viewed: false,
    degree: 3,
    via: ['e5f6g7h8', 'a1b2c3d4'],
  },
  {
    id: 'vine-04',
    creatorAddress: 'a1b2c3d4',
    creatorName: 'Alice',
    createdAt: vineBase + 600,
    videoCid: 'cid-video-alice-02',
    title: 'Cache hit rates explained',
    reshareOf: 'vine-02',
    viewed: false,
    degree: 2,
    via: ['e5f6g7h8'],
  },
  {
    id: 'vine-05',
    creatorAddress: 'm3n4o5p6',
    creatorName: 'Dave',
    createdAt: vineBase + 900,
    videoCid: 'cid-video-dave-01',
    title: 'Zenoh key expressions tutorial',
    viewed: true,
    // Deliberately NO degree/via: exercises the graph-only Discover
    // filter (unconnected creators don't render) in dev mode (ZEB-671).
  },
];

export const profileStore = new Map<string, Profile>(
  [...peers, { address: 'q7r8s9t0', displayName: 'Eve', statusText: 'Lurking' } as Profile]
    .map((p) => [p.address, p])
);
