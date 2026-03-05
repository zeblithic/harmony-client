<script lang="ts">
  import type { NotificationAction, NavNode, Peer, TrustLevel } from '../types';
  import { NotificationService } from '../notification-service';
  import type { TrustService } from '../trust-service';

  let { service, trustService, peers, communities, onClose, onTrustChange }: {
    service: NotificationService;
    trustService?: TrustService;
    peers: Peer[];
    communities: NavNode[];
    onClose?: () => void;
    onTrustChange?: () => void;
  } = $props();

  type Tab = 'global' | 'communities' | 'peers';
  let activeTab = $state<Tab>('global');

  const ACTION_LABELS: Record<NotificationAction, string> = {
    silent: 'Muted',
    dot_only: 'Dot only',
    notify: 'Notification',
    sound: 'Sound',
    break_dnd: 'Break DND',
  };

  const ACTIONS: NotificationAction[] = ['silent', 'dot_only', 'notify', 'sound', 'break_dnd'];

  const PRIORITY_LABELS = {
    quiet: 'Quiet messages',
    standard: 'Standard messages',
    loud: 'Loud messages',
  } as const;

  let version = $state(0);

  function getGlobal() {
    void version;
    return service.settings.global;
  }

  function setGlobalLevel(level: 'quiet' | 'standard' | 'loud', action: NotificationAction) {
    const current = service.settings.global;
    service.setGlobalPolicy({ ...current, [level]: action });
    version++;
  }

  function getPeerPolicy(address: string) {
    void version;
    return service.settings.perPeer.get(address);
  }

  function setPeerLevel(address: string, level: 'quiet' | 'standard' | 'loud', action: NotificationAction) {
    const current = service.settings.perPeer.get(address) ?? {};
    service.setPeerPolicy(address, { ...current, [level]: action });
    version++;
  }

  function clearPeerLevel(address: string, level: 'quiet' | 'standard' | 'loud') {
    const current = service.settings.perPeer.get(address);
    if (!current) return;
    const { [level]: _, ...rest } = current;
    if (Object.keys(rest).length === 0) {
      service.clearPeerPolicy(address);
    } else {
      service.setPeerPolicy(address, rest);
    }
    version++;
  }

  function clearPeer(address: string) {
    service.clearPeerPolicy(address);
    version++;
  }

  function getCommunityPolicy(id: string) {
    void version;
    return service.settings.perCommunity.get(id);
  }

  function setCommunityLevel(id: string, level: 'quiet' | 'standard' | 'loud', action: NotificationAction) {
    const current = service.settings.perCommunity.get(id) ?? {};
    service.setCommunityPolicy(id, { ...current, [level]: action });
    version++;
  }

  function clearCommunityLevel(id: string, level: 'quiet' | 'standard' | 'loud') {
    const current = service.settings.perCommunity.get(id);
    if (!current) return;
    const { [level]: _, ...rest } = current;
    if (Object.keys(rest).length === 0) {
      service.clearCommunityPolicy(id);
    } else {
      service.setCommunityPolicy(id, rest);
    }
    version++;
  }

  function clearCommunity(id: string) {
    service.clearCommunityPolicy(id);
    version++;
  }

  const TRUST_LABELS: Record<TrustLevel, string> = {
    untrusted: 'Untrusted',
    preview: 'Preview (coming soon)',
    trusted: 'Trusted',
  };

  const TRUST_LEVELS: TrustLevel[] = ['untrusted', 'preview', 'trusted'];

  function getGlobalTrust(): TrustLevel {
    void version;
    return trustService?.settings.global ?? 'untrusted';
  }

  function setGlobalTrust(level: TrustLevel) {
    trustService?.setGlobalTrust(level);
    version++;
    onTrustChange?.();
  }

  function getPeerTrust(address: string): TrustLevel | undefined {
    void version;
    return trustService?.settings.perPeer.get(address);
  }

  function setPeerTrust(address: string, level: TrustLevel | '') {
    if (!trustService) return;
    if (level === '') {
      trustService.clearPeerTrust(address);
    } else {
      trustService.setPeerTrust(address, level);
    }
    version++;
    onTrustChange?.();
  }

  function getCommunityTrust(id: string): TrustLevel | undefined {
    void version;
    return trustService?.settings.perCommunity.get(id);
  }

  function setCommunityTrust(id: string, level: TrustLevel | '') {
    if (!trustService) return;
    if (level === '') {
      trustService.clearCommunityTrust(id);
    } else {
      trustService.setCommunityTrust(id, level);
    }
    version++;
    onTrustChange?.();
  }
</script>

<div class="settings-panel">
  <div class="settings-header">
    <h3>Notification Settings</h3>
    <button class="close-btn" aria-label="Close settings" onclick={() => onClose?.()}>&#x2715;</button>
  </div>

  <div class="tabs">
    <button class="tab {activeTab === 'global' ? 'active' : ''}" onclick={() => { activeTab = 'global'; }}>Global</button>
    <button class="tab {activeTab === 'communities' ? 'active' : ''}" onclick={() => { activeTab = 'communities'; }}>Communities</button>
    <button class="tab {activeTab === 'peers' ? 'active' : ''}" onclick={() => { activeTab = 'peers'; }}>Peers</button>
  </div>

  <div class="tab-content">
    {#if activeTab === 'global'}
      <div class="policy-rows">
        {#each (['quiet', 'standard', 'loud'] as const) as level}
          <div class="policy-row">
            <span class="policy-label">{PRIORITY_LABELS[level]}</span>
            <select
              value={getGlobal()[level]}
              onchange={(e) => setGlobalLevel(level, (e.target as HTMLSelectElement).value as NotificationAction)}
            >
              {#each ACTIONS as action}
                <option value={action}>{ACTION_LABELS[action]}</option>
              {/each}
            </select>
          </div>
        {/each}
      </div>

      {#if trustService}
        <div class="trust-section">
          <div class="section-label">Media Trust</div>
          <div class="policy-row">
            <span class="policy-label">Default trust level</span>
            <select
              value={getGlobalTrust()}
              onchange={(e) => setGlobalTrust((e.target as HTMLSelectElement).value as TrustLevel)}
            >
              {#each TRUST_LEVELS as level}
                <option value={level} disabled={level === 'preview'}>{TRUST_LABELS[level]}</option>
              {/each}
            </select>
          </div>
        </div>
      {/if}

    {:else if activeTab === 'communities'}
      {#each communities as comm (comm.id)}
        <div class="override-section">
          <div class="override-header">
            <span class="override-name">{comm.name}</span>
            {#if getCommunityPolicy(comm.id)}
              <button class="reset-btn" onclick={() => clearCommunity(comm.id)}>Reset</button>
            {/if}
          </div>
          <div class="policy-rows">
            {#each (['quiet', 'standard', 'loud'] as const) as level}
              <div class="policy-row">
                <span class="policy-label">{PRIORITY_LABELS[level]}</span>
                <select
                  value={getCommunityPolicy(comm.id)?.[level] ?? ''}
                  onchange={(e) => {
                    const val = (e.target as HTMLSelectElement).value;
                    if (val) {
                      setCommunityLevel(comm.id, level, val as NotificationAction);
                    } else {
                      clearCommunityLevel(comm.id, level);
                    }
                  }}
                >
                  <option value="">Using global default</option>
                  {#each ACTIONS as action}
                    <option value={action}>{ACTION_LABELS[action]}</option>
                  {/each}
                </select>
              </div>
            {/each}
          </div>
          <div class="sound-slots">
            <div class="slots-label">Custom sounds</div>
            {#each (['quiet', 'standard', 'loud'] as const) as slot}
              <div class="sound-slot-row">
                <span class="slot-name">{PRIORITY_LABELS[slot]}</span>
                <span class="slot-value">System default</span>
              </div>
            {/each}
          </div>
          {#if trustService}
            <div class="trust-section">
              <div class="section-label">Media Trust</div>
              <div class="policy-row">
                <span class="policy-label">Trust level</span>
                <select
                  value={getCommunityTrust(comm.id) ?? ''}
                  onchange={(e) => setCommunityTrust(comm.id, (e.target as HTMLSelectElement).value as TrustLevel | '')}
                >
                  <option value="">Use default</option>
                  {#each TRUST_LEVELS as level}
                    <option value={level} disabled={level === 'preview'}>{TRUST_LABELS[level]}</option>
                  {/each}
                </select>
              </div>
            </div>
          {/if}
        </div>
      {/each}

    {:else if activeTab === 'peers'}
      {#each peers as peer (peer.address)}
        <div class="override-section">
          <div class="override-header">
            <span class="override-name">{peer.displayName}</span>
            {#if getPeerPolicy(peer.address)}
              <button class="reset-btn" onclick={() => clearPeer(peer.address)}>Reset</button>
            {/if}
          </div>
          <div class="policy-rows">
            {#each (['quiet', 'standard', 'loud'] as const) as level}
              <div class="policy-row">
                <span class="policy-label">{PRIORITY_LABELS[level]}</span>
                <select
                  value={getPeerPolicy(peer.address)?.[level] ?? ''}
                  onchange={(e) => {
                    const val = (e.target as HTMLSelectElement).value;
                    if (val) {
                      setPeerLevel(peer.address, level, val as NotificationAction);
                    } else {
                      clearPeerLevel(peer.address, level);
                    }
                  }}
                >
                  <option value="">Using community/global default</option>
                  {#each ACTIONS as action}
                    <option value={action}>{ACTION_LABELS[action]}</option>
                  {/each}
                </select>
              </div>
            {/each}
          </div>
          <div class="sound-slots">
            <div class="slots-label">Custom sounds</div>
            {#each (['quiet', 'standard', 'loud'] as const) as slot}
              <div class="sound-slot-row">
                <span class="slot-name">{PRIORITY_LABELS[slot]}</span>
                <span class="slot-value">System default</span>
              </div>
            {/each}
          </div>
          {#if trustService}
            <div class="trust-section">
              <div class="section-label">Media Trust</div>
              <div class="policy-row">
                <span class="policy-label">Trust level</span>
                <select
                  value={getPeerTrust(peer.address) ?? ''}
                  onchange={(e) => setPeerTrust(peer.address, (e.target as HTMLSelectElement).value as TrustLevel | '')}
                >
                  <option value="">Use default</option>
                  {#each TRUST_LEVELS as level}
                    <option value={level} disabled={level === 'preview'}>{TRUST_LABELS[level]}</option>
                  {/each}
                </select>
              </div>
            </div>
          {/if}
        </div>
      {/each}
    {/if}
  </div>
</div>

<style>
  .settings-panel {
    display: flex;
    flex-direction: column;
    height: 100%;
    background: var(--bg-secondary);
  }

  .settings-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
  }

  .settings-header h3 {
    margin: 0;
    font-size: 14px;
    font-weight: 600;
    color: var(--text-primary);
  }

  .close-btn {
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 16px;
    cursor: pointer;
    padding: 4px;
  }

  .close-btn:hover {
    color: var(--text-primary);
  }

  .tabs {
    display: flex;
    border-bottom: 1px solid var(--border);
  }

  .tab {
    flex: 1;
    padding: 8px 12px;
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 13px;
    cursor: pointer;
    border-bottom: 2px solid transparent;
  }

  .tab.active {
    color: var(--text-primary);
    border-bottom-color: var(--accent);
  }

  .tab:hover {
    color: var(--text-secondary);
  }

  .tab-content {
    flex: 1;
    overflow-y: auto;
    padding: 12px 16px;
  }

  .policy-rows {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .policy-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
  }

  .policy-label {
    font-size: 13px;
    color: var(--text-secondary);
  }

  .policy-row select {
    padding: 4px 8px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-primary);
    color: var(--text-primary);
    font-size: 12px;
  }

  .override-section {
    margin-bottom: 16px;
    padding-bottom: 16px;
    border-bottom: 1px solid var(--border);
  }

  .override-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 8px;
  }

  .override-name {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
  }

  .reset-btn {
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 11px;
    cursor: pointer;
    padding: 2px 6px;
  }

  .reset-btn:hover {
    color: var(--accent);
  }

  .sound-slots {
    margin-top: 8px;
    padding-top: 8px;
    border-top: 1px solid var(--border);
  }

  .slots-label {
    font-size: 11px;
    font-weight: 600;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin-bottom: 4px;
  }

  .sound-slot-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 2px 0;
  }

  .slot-name {
    font-size: 12px;
    color: var(--text-secondary);
  }

  .slot-value {
    font-size: 12px;
    color: var(--text-muted);
    font-style: italic;
  }

  .trust-section {
    margin-top: 12px;
    padding-top: 8px;
    border-top: 1px solid var(--border);
  }

  .section-label {
    font-size: 11px;
    font-weight: 600;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin-bottom: 6px;
  }
</style>
