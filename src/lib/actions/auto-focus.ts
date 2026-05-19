/**
 * Svelte action: focus the node and select its content on mount.
 * Used by the inline rename input (ZEB-299) to put the cursor in the
 * field immediately and select-all so the user can retype the name
 * fast — matches Finder/Explorer behaviour on rename-mode entry.
 */
export function autoFocus(node: HTMLInputElement) {
  node.focus();
  node.select();
}
