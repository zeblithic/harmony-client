/**
 * ZEB-607 Commons status-pill variant union. Lives in a `.ts` (not the
 * StatusPill.svelte module script) so plain-`tsc` consumers — e.g. the
 * shared `proposal-format.ts` helper — can import the type; the ambient
 * `*.svelte` module shim tsc uses does not expose a component's named
 * exports. StatusPill.svelte re-exports it for `.svelte` importers.
 */
export type StatusPillVariant =
  | 'drafting'
  | 'open'
  | 'passing'
  | 'failing'
  | 'passed'
  | 'failed'
  | 'archived'
  | 'recalled';
