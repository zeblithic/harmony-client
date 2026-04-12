import type { MailEntry, MailCounts } from './types';

const now = Date.now();
const hour = 3600_000;
const day = 86400_000;

export const mockMailEntries: MailEntry[] = [
  {
    messageCid: 'a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2',
    messageId: 'msg001msg001msg0',
    senderAddress: 'a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6',
    timestamp: Math.floor((now - 2 * hour) / 1000),
    subjectSnippet: 'Welcome to Harmony Mail',
    read: false,
  },
  {
    messageCid: 'b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3',
    messageId: 'msg002msg002msg0',
    senderAddress: 'f1e2d3c4b5a6f1e2d3c4b5a6f1e2d3c4',
    timestamp: Math.floor((now - 1 * day) / 1000),
    subjectSnippet: 'Meeting notes from Tuesday',
    read: false,
  },
  {
    messageCid: 'c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4',
    messageId: 'msg003msg003msg0',
    senderAddress: 'a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6',
    timestamp: Math.floor((now - 3 * day) / 1000),
    subjectSnippet: 'Re: Distributed systems architecture',
    read: true,
  },
  {
    messageCid: 'd4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5',
    messageId: 'msg004msg004msg0',
    senderAddress: 'deadbeefcafebabe1234567890abcdef',
    timestamp: Math.floor((now - 5 * day) / 1000),
    subjectSnippet: 'Zenoh mesh sync results',
    read: true,
  },
  {
    messageCid: 'e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6',
    messageId: 'msg005msg005msg0',
    senderAddress: 'f1e2d3c4b5a6f1e2d3c4b5a6f1e2d3c4',
    timestamp: Math.floor((now - 7 * day) / 1000),
    subjectSnippet: 'Invitation: Harmony dev sync',
    read: true,
  },
];

export const mockMailCounts: MailCounts = {
  inbox: { total: 5, unread: 2 },
  sent: { total: 3, unread: 0 },
  drafts: { total: 0, unread: 0 },
  trash: { total: 1, unread: 0 },
};
