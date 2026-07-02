<script lang="ts">
  import type { ContentSensitivity } from '../types';
  import { sensitivityIcon } from '../file-utils';

  let {
    sensitivity,
  }: {
    sensitivity: ContentSensitivity;
  } = $props();

  const LABELS: Record<ContentSensitivity, string> = {
    public: 'Public',
    private: 'Private',
    intimate: 'Intimate',
    confidential: 'Confidential',
  };

  let icon = $derived(sensitivityIcon(sensitivity));
  let label = $derived(LABELS[sensitivity]);
</script>

<span class="sensitivity-badge {sensitivity}">
  <span class="sensitivity-icon" aria-hidden="true">{icon}</span>
  {label}
</span>

<style>
  .sensitivity-badge {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    padding: 2px 8px;
    border-radius: 4px;
    font-size: 0.8rem;
    font-weight: 500;
  }

  .sensitivity-icon {
    font-size: 0.85rem;
  }

  .sensitivity-badge.public {
    color: var(--success-deep);
    background: color-mix(in srgb, var(--success-deep) 12%, transparent);
  }

  .sensitivity-badge.private {
    color: var(--text-secondary);
    background: color-mix(in srgb, var(--text-secondary) 10%, transparent);
  }

  .sensitivity-badge.intimate {
    color: var(--cat-orange);
    background: color-mix(in srgb, var(--cat-orange) 12%, transparent);
  }

  .sensitivity-badge.confidential {
    color: var(--cat-yellow);
    background: color-mix(in srgb, var(--cat-yellow) 12%, transparent);
  }
</style>
