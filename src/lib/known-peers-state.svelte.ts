// ZEB-979: module-level reactive handoff of the known-peers index from
// App.svelte (which owns the contacts / friends / card state and rebuilds
// the index when any of them change) to PeerName.svelte (which consults it
// on every render). A module store — same idiom as
// `stq8-calibration-state.svelte.ts` — rather than a per-site prop, so ALL
// ~12 ladder surfaces become collision-aware by passing only `ownerIdHex`
// to PeerName; threading a second index prop through every intermediate
// component would churn each call chain for no behavioral difference.
//
// The `index` property is REPLACED wholesale on rebuild (App's $effect),
// never mutated in place: property reassignment is what Svelte 5 tracks
// here — the Maps inside are deliberately plain (not SvelteMap), so
// in-place mutation would be invisible to consumers.
import { EMPTY_KNOWN_PEERS, type KnownPeersIndex } from './name-collision';

export const knownPeersState = $state<{ index: KnownPeersIndex }>({
  index: EMPTY_KNOWN_PEERS,
});
