// Module-level reactive "please open the backup wizard" request.
//
// The backup-staleness banner is rendered at the app root, but the backup
// wizard lives inside IdentityPanel — which only mounts when Settings →
// Account is open, and never at all in collapsed/mobile layout (Settings has
// no column there). A fire-and-forget `window` CustomEvent is therefore lossy:
// dispatched while IdentityPanel is unmounted, the request simply vanishes.
//
// This reactive flag instead *survives* until IdentityPanel observes it: if the
// panel is already mounted it reacts immediately; if it mounts later (the user
// opens Settings, or widens a collapsed window) its effect picks up the pending
// request on mount. IdentityPanel consumes (clears) it so it fires exactly once.
//
// Resets implicitly on page reload, which is correct: a reload re-reads the
// persisted identity, so any in-flight request no longer applies.
export const backupExportRequest = $state<{ pending: boolean }>({ pending: false });
