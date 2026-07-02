<script lang="ts">
  import { untrack } from 'svelte';
  import Modal from './Modal.svelte';
  import { powerToRole, POWER_THRESHOLDS } from '../types';

  let {
    targetName,
    targetAddress,
    currentPower,
    actorMaxPower = POWER_THRESHOLDS.max,
    onSubmit,
    onCancel,
  }: {
    targetName: string;
    targetAddress: string;
    currentPower: number;
    /** Hard ceiling — admins can match (but never exceed) their own
     *  power. Backend permits any level 0..=POWER_THRESHOLDS.max for
     *  any admin, but the conventional UX is "promote up to but not
     *  above yourself", so CommunitySettingsPanel passes myPower. */
    actorMaxPower?: number;
    onSubmit: (power: number) => void;
    onCancel: () => void;
  } = $props();

  let power = $state(untrack(() => currentPower));
  let role = $derived(powerToRole(power));
  let safeMax = $derived(Math.max(0, Math.min(POWER_THRESHOLDS.max, actorMaxPower)));
  let canSubmit = $derived(Number.isFinite(power) && power >= 0 && power <= safeMax);
  const titleId = `set-power-title-${Math.random().toString(36).slice(2)}`;

  function clampOnBlur() {
    if (Number.isNaN(power) || !Number.isFinite(power)) power = 0;
    if (power < 0) power = 0;
    if (power > safeMax) power = safeMax;
  }

  function handleSubmit() {
    clampOnBlur();
    if (!canSubmit) return;
    onSubmit(Math.trunc(power));
  }
</script>

<Modal {onCancel} ariaLabelledby={titleId}>
  <h3 class="dialog-title" id={titleId}>Set {targetName}'s role</h3>
  <p class="dialog-subtitle"><code>{targetAddress}</code> · currently {powerToRole(currentPower)} (power {currentPower})</p>

  <div class="role-preview">
    <span class="role-badge" data-role={role}>{role.toUpperCase()}</span>
  </div>

  <div class="control-row">
    <input type="range" min="0" max={safeMax} step="1" bind:value={power} class="slider" aria-label="Power level slider" />
    <input
      type="number"
      min="0"
      max={safeMax}
      step="1"
      bind:value={power}
      onblur={clampOnBlur}
      class="number-input"
      aria-label="Power level"
    />
  </div>

  <div class="thresholds">
    <span class="threshold member"><span class="bar">|</span>0<br/>Member</span>
    <span class="threshold mod"><span class="bar">|</span>50<br/>Mod</span>
    <span class="threshold admin"><span class="bar">|</span>100<br/>Admin</span>
  </div>

  <div class="dialog-actions">
    <button class="cancel-btn" onclick={onCancel}>Cancel</button>
    <button class="confirm-btn" onclick={handleSubmit} disabled={!canSubmit}>Set role</button>
  </div>
</Modal>

<style>
  .dialog-title { color: var(--text-primary); font-size: 1.05rem; margin: 0 0 4px; }
  .dialog-subtitle { color: var(--text-secondary); font-size: 0.8rem; margin: 0 0 16px; }
  .dialog-subtitle code { font-family: monospace; }
  .role-preview { text-align: center; margin-bottom: 12px; }
  .role-badge { padding: 3px 14px; border-radius: 12px; font-size: 0.75rem; font-weight: bold; }
  .role-badge[data-role="member"] { background: var(--bg-tertiary); color: var(--text-secondary); }
  .role-badge[data-role="mod"] { background: var(--role-mod); color: var(--text-inverse-dark); }
  .role-badge[data-role="admin"] { background: var(--accent); color: var(--text-primary); }
  .control-row { display: flex; align-items: center; gap: 14px; margin-bottom: 6px; }
  .slider { flex: 1; }
  .number-input {
    width: 64px;
    background: var(--bg-tertiary);
    border: 1px solid var(--accent);
    border-radius: 4px;
    padding: 6px 8px;
    color: var(--text-primary);
    font-size: 0.9rem;
    text-align: center;
    font-family: monospace;
  }
  .number-input:focus { outline: 2px solid var(--accent); outline-offset: -1px; }
  .thresholds {
    display: flex;
    justify-content: space-between;
    padding: 0 4px;
    margin-bottom: 24px;
    font-size: 0.7rem;
  }
  .threshold { text-align: center; }
  .threshold.member { color: var(--text-secondary); }
  .threshold.mod { color: var(--role-mod); }
  .threshold.admin { color: var(--accent); }
  .threshold .bar { display: block; }
  .dialog-actions { display: flex; justify-content: flex-end; gap: 8px; }
  .cancel-btn {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    border: none;
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }
  .confirm-btn {
    background: var(--accent);
    color: var(--text-primary);
    border: none;
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }
  .cancel-btn:focus-visible,
  .confirm-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
</style>
