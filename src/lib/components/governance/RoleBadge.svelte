<script lang="ts">
  /**
   * ZEB-608 — Commons role badge (spec D2). Membership-tier badge for
   * member/mod/admin rows. Deliberately NOT a StatusPill variant: role
   * badges are mono, smaller, and membership-semantic (not
   * governance-lifecycle). Token pairs per the design: member = drafting,
   * mod = open, admin = passed.
   *
   * The label is uppercased in MARKUP (not CSS) — consumer tests pin the
   * literal text 'MEMBER' / 'MOD' / 'ADMIN' via getByText.
   */
  import type { PowerRole } from '../../types';

  let { role }: { role: PowerRole } = $props();
</script>

<span class="role-badge {role}">{role.toUpperCase()}</span>

<style>
  .role-badge {
    display: inline-block;
    font-family: var(--font-mono);
    font-weight: 600;
    font-size: 10px;
    line-height: 1.4;
    letter-spacing: 0.04em;
    padding: 2px 8px;
    border-radius: 20px;
    white-space: nowrap;
  }
  .member {
    color: var(--status-drafting-fg);
    background: var(--status-drafting-bg);
  }
  .mod {
    color: var(--status-open-fg);
    background: var(--status-open-bg);
  }
  .admin {
    color: var(--status-passed-fg);
    background: var(--status-passed-bg);
  }
</style>
