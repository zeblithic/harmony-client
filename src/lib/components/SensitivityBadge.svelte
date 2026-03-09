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
    color: #43b581;
    background: rgba(67, 181, 129, 0.12);
  }

  .sensitivity-badge.private {
    color: var(--text-secondary, #b5bac1);
    background: rgba(181, 186, 193, 0.1);
  }

  .sensitivity-badge.intimate {
    color: #e67e22;
    background: rgba(230, 126, 34, 0.12);
  }

  .sensitivity-badge.confidential {
    color: #f1c40f;
    background: rgba(241, 196, 15, 0.12);
  }
</style>
