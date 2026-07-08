<script lang="ts">
  import { untrack } from 'svelte';
  import Modal from './Modal.svelte';
  import RoleBadge from './governance/RoleBadge.svelte';
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
  // ZEB-608 D6: helper copy keyed to the PREVIEWED role (design frame C1).
  const ROLE_HELP: Record<ReturnType<typeof powerToRole>, string> = {
    member: 'Member can post, vote, propose, invite, delegate and fork.',
    mod: 'Moderator can manage channels & join requests.',
    admin: 'Admin can set roles and change decision rules — under quorum.',
  };
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
    <RoleBadge {role} />
  </div>
  <p class="role-help">{ROLE_HELP[role]}</p>

  <div class="control-row">
    <div class="slider-stack">
      <input type="range" min="0" max={safeMax} step="1" bind:value={power} class="slider" aria-label="Power level slider" />
      <!-- ZEB-608 D6: banded track — widths from POWER_THRESHOLDS. The admin
           band is a fixed end-cap: the admin threshold IS the scale max
           (setPower == max == 100), so its data-width is zero; the cap marks
           "admin sits at the top of the scale" without inventing a range. -->
      <div class="band-track" aria-hidden="true">
        <span class="band band-member" style="flex-grow: {POWER_THRESHOLDS.kick - POWER_THRESHOLDS.invite}"></span>
        <span class="band band-mod" style="flex-grow: {POWER_THRESHOLDS.setPower - POWER_THRESHOLDS.kick}"></span>
        <span class="band band-admin"></span>
      </div>
    </div>
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
  .dialog-subtitle code { font-family: var(--font-mono); }
  .role-preview { text-align: center; margin-bottom: 6px; }
  .role-help {
    text-align: center;
    font-size: 0.72rem;
    color: var(--text-secondary);
    margin: 0 0 12px;
  }
  .control-row { display: flex; align-items: center; gap: 14px; margin-bottom: 6px; }
  .slider-stack {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .slider { width: 100%; }
  .band-track {
    display: flex;
    height: 6px;
    border-radius: 4px;
    overflow: hidden;
  }
  .band { min-height: 100%; }
  .band-member { background: var(--status-drafting-bg); }
  .band-mod { background: var(--gov-clay-soft); }
  .band-admin { flex: 0 0 12px; background: var(--primary-soft); }
  .number-input {
    width: 64px;
    background: var(--input-bg);
    border: 1px solid var(--border);
    border-radius: 5px;
    padding: 6px 8px;
    color: var(--vote-for);
    font-size: 0.9rem;
    font-weight: 600;
    text-align: center;
    font-family: var(--font-mono);
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
  .threshold.mod { color: var(--gov-clay-deep); }
  .threshold.admin { color: var(--vote-for); }
  .threshold .bar { display: block; }
  .dialog-actions { display: flex; justify-content: flex-end; gap: 8px; }
  .cancel-btn {
    background: transparent;
    color: var(--text-secondary);
    border: 1px solid var(--border);
    padding: 8px 16px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.875rem;
  }
  .confirm-btn {
    background: var(--accent);
    color: var(--on-accent);
    border: 1px solid var(--accent);
    padding: 8px 16px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.875rem;
    font-weight: 600;
  }
  .cancel-btn:focus-visible,
  .confirm-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
</style>
