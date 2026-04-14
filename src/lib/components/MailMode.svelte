<script lang="ts">
  import type { InboxEntry, MailMessage } from '../types';
  import InboxList from './InboxList.svelte';
  import MailDetail from './MailDetail.svelte';

  let {
    entries = [],
    selectedCid = null,
    selectedMessage = null,
    loading = false,
    onSelect,
  }: {
    entries: InboxEntry[];
    selectedCid: string | null;
    selectedMessage: MailMessage | null;
    loading?: boolean;
    onSelect: (cid: string) => void;
  } = $props();
</script>

<div class="mail-mode">
  <div class="inbox-panel">
    <div class="inbox-header">
      <h3>Inbox</h3>
    </div>
    <InboxList {entries} {selectedCid} {onSelect} />
  </div>
  <div class="detail-panel">
    <MailDetail message={selectedMessage} {loading} />
  </div>
</div>

<style>
  .mail-mode {
    display: flex;
    height: 100%;
    overflow: hidden;
  }

  .inbox-panel {
    width: 38%;
    min-width: 280px;
    max-width: 450px;
    display: flex;
    flex-direction: column;
    border-right: 1px solid var(--border, #2a2d31);
    background: var(--bg-secondary, #2b2d31);
  }

  .inbox-header {
    padding: 12px 16px;
    border-bottom: 1px solid var(--border, #2a2d31);
    flex-shrink: 0;
  }

  .inbox-header h3 {
    margin: 0;
    font-size: 15px;
    font-weight: 600;
    color: var(--text-primary, #dbdee1);
  }

  .detail-panel {
    flex: 1;
    background: var(--bg-primary, #313338);
    overflow: hidden;
  }
</style>
