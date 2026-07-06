/**
 * ZEB-607 — shared hex-address abbreviation. Two forms:
 *   shortAddr: first 8 + '…' + last 4 (roster/proposer rows — the
 *              SortitionRevealView/DraftingPanel convention)
 *   shortId:   first 8 + '…' (ID pills, author chips)
 *
 * NOTE: `voting-toast-wiring.ts` keeps its own local shortAddr — its
 * message format is locked by ZEB-298 Task 10. Do not migrate it here.
 */
export function shortAddr(hex: string): string {
  return hex.length > 16 ? `${hex.slice(0, 8)}…${hex.slice(-4)}` : hex;
}

export function shortId(hex: string): string {
  return hex.length > 8 ? `${hex.slice(0, 8)}…` : hex;
}
