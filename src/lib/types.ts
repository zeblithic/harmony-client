export interface Peer {
  address: string;
  displayName: string;
  avatarUrl?: string;
}

export interface MediaAttachment {
  id: string;
  type: 'image' | 'link' | 'code';
  /** URL for images and links */
  url?: string;
  /** OG title or filename */
  title?: string;
  /** Extracted domain for link indicators (e.g. "github.com") */
  domain?: string;
  /** Source code for code blocks */
  content?: string;
}

export interface Message {
  id: string;
  sender: Peer;
  text: string;
  /** Unix timestamp in milliseconds */
  timestamp: number;
  /** Empty array for text-only messages */
  media: MediaAttachment[];
}

export interface Channel {
  id: string;
  name: string;
  unreadCount: number;
}
