<script lang="ts">
  import type { Message } from '../types';
  import type { ResolvedCard } from '../member-card-service';
  import { resolveAuthorLabel } from '../mention-render';
  import { formatMessageTimestamp, formatFullTimestamp, formatClockTime } from '../time-format';
  import { dayClock } from '../day-clock';
  import { timeFormatPrefs } from '../time-format-service';
  import Avatar from './Avatar.svelte';
  import PeerName from './PeerName.svelte';

  let { message, onAvatarClick, allMessages = [], onScrollToMessage, isSelf = false, onDelete, resolveNickname, resolveCard, seenAt }: {
    message: Message;
    onAvatarClick?: (address: string, event: MouseEvent) => void;
    allMessages?: Message[];
    onScrollToMessage?: (messageId: string) => void;
    /** ZEB-228 Phase 4: true when the message was sent by the local
     *  user; gates whether the inline delete button is rendered. */
    isSelf?: boolean;
    /** ZEB-228 Phase 4: invoked with the OutboxEntryId (hex) when the
     *  user clicks the inline ⓧ on a stuck/expired self-Message. The
     *  parent is responsible for the ConfirmDialog + delete_outbox_entry
     *  IPC call; this component only surfaces the request. */
    onDelete?: (messageId: string) => void;
    /** ZEB-839: local friend nickname for an owner_id — top rung of the author
     *  ladder. Pure consumer; the parent owns the reactivity seam (App.svelte
     *  bumps a $state version so these reads re-run when a nickname changes). */
    resolveNickname?: (ownerIdHex: string) => string | undefined;
    /** ZEB-839: broadcast profile card for an owner_id — second rung of the
     *  author ladder. Same pure-consumer contract as `resolveNickname`. */
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    /** ZEB-214: when set (ms), render a "Seen HH:MM" line under this message —
     *  the parent sets it only on the newest own-message the DM peer has read. */
    seenAt?: number;
  } = $props();

  // ZEB-228 Phase 4: re-evaluate `canDelete` every 5s so the button
  // appears once a "sending" message crosses the 60s stuck threshold
  // without requiring any external nudge.
  //
  // PR #81 round 4 (Greptile P2 + ZEB-242): only schedule the timer
  // when this Message could actually transition states — that's
  // self-Messages currently in 'sending'. Received messages, terminal
  // self-Messages (delivered/expired/failed), and self-Messages
  // without a messageId never need the tick. In a long DM thread
  // with 50+ scrollback entries, this drops O(N) idle timers to 0.
  let now = $state(Date.now());
  $effect(() => {
    if (!isSelf) return;
    if (message.messageId === undefined) return;
    if (message.deliveryState !== 'sending') return;
    const interval = setInterval(() => { now = Date.now(); }, 5_000);
    return () => clearInterval(interval);
  });

  let canDelete = $derived(
    isSelf
    && message.messageId !== undefined
    && (
      message.deliveryState === 'expired'
      || message.deliveryState === 'failed'
      || (message.deliveryState === 'sending' && now - message.timestamp > 60_000)
    )
  );

  // ZEB-943: date-aware label formatted against the app-wide day clock, so it
  // reclassifies at local midnight without a remount (day-clock.ts). Kept off
  // the delete-state `now` above, which only ticks for still-sending messages.
  let timeStr = $derived(formatMessageTimestamp(message.timestamp, $dayClock, $timeFormatPrefs));

  // ZEB-214: "Seen HH:MM" clock (only rendered when seenAt is set).
  // ZEB-944: format through the same preference-aware clock path as the message
  // timestamp so it honors the 12h/24h choice (time-only — never a date).
  let seenStr = $derived(
    seenAt === undefined ? '' : formatClockTime(seenAt, $timeFormatPrefs)
  );

  let parentMessage = $derived(
    message.replyTo ? allMessages.find(m => m.id === message.replyTo) : undefined
  );

  let parentPreview = $derived(
    parentMessage ? parentMessage.text.slice(0, 50) + (parentMessage.text.length > 50 ? '...' : '') : ''
  );

  // ZEB-839: resolve the author at RENDER time through the shared ladder
  // (nickname ► profile card ► wire name ► short hex), mirroring
  // ChannelMessageFeed.authorLabel. DM senders carry no baked name, so this is
  // the only thing standing between a DM bubble and a raw hex prefix — and
  // because it reads the resolvers on every render, the label fills in live
  // the moment the peer's card arrives.
  let authorName = $derived(resolveAuthorLabel(message.sender, resolveNickname, resolveCard));
  let authorLabel = $derived(authorName.label);
  let parentAuthorLabel = $derived(
    parentMessage ? resolveAuthorLabel(parentMessage.sender, resolveNickname, resolveCard).label : ''
  );

</script>

<div class="text-message" class:loud={message.priority === 'loud'} id="msg-{message.id}">
  <Avatar
    address={message.sender.address}
    displayName={authorLabel}
    avatarUrl={message.sender.avatarUrl}
    size={24}
    onclick={onAvatarClick ? (e) => onAvatarClick(message.sender.address, e) : undefined}
  />
  <div class="message-content">
    <div class="message-header">
      <span class="sender-name"><PeerName name={authorName} /></span>
      <time class="timestamp" datetime={new Date(message.timestamp).toISOString()} title={formatFullTimestamp(message.timestamp, $timeFormatPrefs)}>{timeStr}</time>
      {#if canDelete}
        <button
          type="button"
          class="delete-btn"
          aria-label="Delete this message"
          onclick={() => { if (message.messageId) onDelete?.(message.messageId); }}
        >
          &#9447;
        </button>
      {/if}
    </div>
    {#if parentMessage}
      <button
        class="reply-to-header"
        onclick={() => onScrollToMessage?.(parentMessage.id)}
        aria-label="In reply to {parentAuthorLabel}: {parentPreview}"
      >
        <span class="reply-to-icon">↩</span>
        <span class="reply-to-sender">{parentAuthorLabel}</span>
        <span class="reply-to-text">{parentPreview}</span>
      </button>
    {/if}
    <div class="message-text">{message.text}</div>
    {#if seenAt !== undefined}
      <!-- ZEB-214: peer read up to (at least) this message. -->
      <div class="seen-indicator" data-testid="seen-indicator">Seen {seenStr}</div>
    {/if}
  </div>
</div>

<style>
  .text-message {
    display: flex;
    gap: 12px;
    padding: 4px 16px;
    scroll-margin-top: 8px;
  }

  .text-message:hover {
    background: var(--bg-secondary);
  }

  .message-content {
    flex: 1;
    min-width: 0;
  }

  .message-header {
    display: flex;
    align-items: baseline;
    gap: 8px;
  }

  .sender-name {
    font-weight: 600;
    font-size: 14px;
    color: var(--text-primary);
  }

  .timestamp {
    font-size: 11px;
    color: var(--text-muted);
  }

  .delete-btn {
    background: transparent;
    border: none;
    padding: 0 4px;
    margin-left: 4px;
    color: var(--text-muted);
    font-size: 12px;
    cursor: pointer;
    line-height: 1;
  }

  .delete-btn:hover {
    color: var(--danger-muted);
  }

  .delete-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .message-text {
    color: var(--text-secondary);
    font-size: 14px;
    word-wrap: break-word;
  }

  /* ZEB-214: subtle right-aligned "Seen HH:MM" under the last read own-message. */
  .seen-indicator {
    margin-top: 2px;
    font-size: 11px;
    color: var(--text-muted, var(--text-secondary));
    text-align: right;
    opacity: 0.8;
  }

  .reply-to-header {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 2px 0;
    margin-bottom: 2px;
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 11px;
    cursor: pointer;
    text-align: left;
  }

  .reply-to-header:hover {
    color: var(--accent);
  }

  .reply-to-icon {
    font-size: 12px;
  }

  .reply-to-sender {
    font-weight: 600;
  }

  .reply-to-text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 200px;
  }

  .text-message.loud {
    border-left: 2px solid var(--accent);
    padding-left: 14px;
  }

  .text-message.loud .sender-name {
    font-weight: 700;
  }
</style>
