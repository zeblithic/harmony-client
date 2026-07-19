<script lang="ts">
  // ZEB-359 — voice device pickers (microphone + speaker). Backed by the
  // shared AudioDevicePrefs service: choices persist per device and apply
  // live to any active voice channel / call via the sessions' followers.
  import { onMount } from 'svelte';
  import type { AudioDevicePrefs, AudioDeviceInfo } from '../audio-device-prefs';

  let {
    audioDevices,
  }: {
    audioDevices: Pick<
      AudioDevicePrefs,
      | 'getInput'
      | 'getOutput'
      | 'setInput'
      | 'setOutput'
      | 'listDevices'
      | 'supportsOutputSelection'
      | 'subscribe'
    >;
  } = $props();

  let inputs = $state<AudioDeviceInfo[]>([]);
  let outputs = $state<AudioDeviceInfo[]>([]);
  let selectedInput = $state(audioDevices.getInput() ?? '');
  let selectedOutput = $state(audioDevices.getOutput() ?? '');
  const outputSupported = audioDevices.supportsOutputSelection();

  /** Monotonic token so a slow enumeration can't overwrite a newer one
   *  (PR #495 R1, Qodo): refreshes fire on mount AND every prefs/devicechange
   *  notification, and enumerateDevices latency lets them resolve out of
   *  order — only the latest-started refresh may commit. */
  let refreshSeq = 0;

  async function refresh(): Promise<void> {
    const seq = ++refreshSeq;
    const set = await audioDevices.listDevices();
    if (seq !== refreshSeq) return; // superseded by a newer refresh
    inputs = set.inputs;
    outputs = set.outputs;
    selectedInput = audioDevices.getInput() ?? '';
    selectedOutput = audioDevices.getOutput() ?? '';
  }

  onMount(() => {
    void refresh();
    // Re-enumerate on hot-plug (the service rebroadcasts `devicechange`).
    const unsub = audioDevices.subscribe(() => void refresh());
    return unsub;
  });

  function onInputChange(e: Event): void {
    const v = (e.currentTarget as HTMLSelectElement).value;
    audioDevices.setInput(v === '' ? null : v);
    selectedInput = v;
  }

  function onOutputChange(e: Event): void {
    const v = (e.currentTarget as HTMLSelectElement).value;
    audioDevices.setOutput(v === '' ? null : v);
    selectedOutput = v;
  }

  /** The saved device when it isn't currently enumerated (unplugged): keep it
   *  selectable so the choice isn't silently lost — capture falls back to the
   *  system default until it returns. */
  const inputMissing = $derived(
    selectedInput !== '' && !inputs.some((d) => d.deviceId === selectedInput),
  );
  const outputMissing = $derived(
    selectedOutput !== '' && !outputs.some((d) => d.deviceId === selectedOutput),
  );
</script>

<section class="voice-device-settings">
  <h3>Voice devices</h3>

  <div class="setting-row">
    <div class="setting-text">
      <label class="setting-label" for="voice-input-select">Microphone</label>
      <span class="setting-hint">
        Applies immediately, including during a call. Device names may appear
        generic until you first join voice.
      </span>
    </div>
    <select
      id="voice-input-select"
      class="device-select"
      value={selectedInput}
      onchange={onInputChange}
    >
      <option value="">System default</option>
      {#if inputMissing}
        <option value={selectedInput}>Saved device (unavailable — using default)</option>
      {/if}
      {#each inputs as d (d.deviceId)}
        <option value={d.deviceId}>{d.label}</option>
      {/each}
    </select>
  </div>

  <div class="setting-row">
    <div class="setting-text">
      <label class="setting-label" for="voice-output-select">Speaker</label>
      {#if outputSupported}
        <span class="setting-hint">Where voice audio plays.</span>
      {:else}
        <span class="setting-hint">
          Output selection is not supported by this platform's webview; the
          system default output is used.
        </span>
      {/if}
    </div>
    <select
      id="voice-output-select"
      class="device-select"
      value={selectedOutput}
      disabled={!outputSupported}
      onchange={onOutputChange}
    >
      <option value="">System default</option>
      {#if outputSupported && outputMissing}
        <option value={selectedOutput}>Saved device (unavailable — using default)</option>
      {/if}
      {#each outputs as d (d.deviceId)}
        <option value={d.deviceId}>{d.label}</option>
      {/each}
    </select>
  </div>
</section>

<style>
  /* Tokens only (ZEB-604 ratchet); row/typography model mirrors
     AppearanceSettings / NetworkDiscoverabilitySettings. */
  .voice-device-settings {
    padding: 12px 0;
  }

  .voice-device-settings h3 {
    margin: 0 0 8px;
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .setting-row {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 16px;
    padding: 6px 0;
  }

  .setting-text {
    display: flex;
    flex-direction: column;
    gap: 4px;
    flex: 1;
  }

  .setting-label {
    font-size: 13px;
    color: var(--text-primary);
  }

  .setting-hint {
    font-size: 11px;
    color: var(--text-muted);
    line-height: 1.4;
  }

  .device-select {
    max-width: 220px;
    flex-shrink: 0;
    padding: 4px 8px;
    font-size: 0.8rem;
    color: var(--text-primary);
    background: var(--bg-primary);
    border: 1px solid var(--border);
    border-radius: 6px;
  }

  .device-select:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }

  .device-select:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
</style>
