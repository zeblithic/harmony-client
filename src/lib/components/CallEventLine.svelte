<script lang="ts">
  // ZEB-357 — system line for a call-event DM message (caller-authored call
  // outcome record). Rendered by TextFeed in place of a TextMessage bubble.
  // The label is derived from the parsed payload + viewer side rather than
  // Message.text so the direction can never go stale against the payload.
  import type { Message } from '../types';
  import { describeCallEvent, isMissedCallEvent } from '../call-log';
  import { formatMessageTimestamp, formatFullTimestamp } from '../time-format';

  let { message, isSelf = false }: { message: Message; isSelf?: boolean } = $props();

  let direction = $derived(isSelf ? ('author' as const) : ('recipient' as const));
  let label = $derived(
    message.callEvent ? describeCallEvent(message.callEvent, direction) : message.text,
  );
  let missed = $derived(
    message.callEvent ? isMissedCallEvent(message.callEvent, direction) : false,
  );
  // ZEB-943: mount-time reference instant (no per-message timer — see ZEB-242).
  const now = Date.now();
  let timeStr = $derived(formatMessageTimestamp(message.timestamp, now));
</script>

<div class="call-event-line" class:missed data-testid="call-event-line" role="note">
  <span class="rule" aria-hidden="true"></span>
  <span class="glyph" aria-hidden="true">📞</span>
  <span class="label">{label}</span>
  {#if message.deliveryState === 'failed'}
    <!-- The record never reached the outbox — the callee will not see this
         call. Without the marker a failed send is invisible to the caller. -->
    <span class="failed">not delivered</span>
  {/if}
  <span class="time" title={formatFullTimestamp(message.timestamp)}>{timeStr}</span>
  <span class="rule" aria-hidden="true"></span>
</div>

<style>
  .call-event-line {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 4px 16px;
    font-size: 0.8rem;
    color: var(--text-secondary);
  }
  .rule {
    flex: 1;
    height: 1px;
    background: var(--border);
  }
  .glyph {
    flex-shrink: 0;
  }
  .label {
    flex-shrink: 0;
    white-space: nowrap;
  }
  .time {
    flex-shrink: 0;
    font-size: 0.75rem;
    opacity: 0.7;
  }
  .failed {
    flex-shrink: 0;
    font-size: 0.75rem;
    color: var(--danger);
  }
  .call-event-line.missed .label {
    color: var(--accent);
    font-weight: 600;
  }
</style>
