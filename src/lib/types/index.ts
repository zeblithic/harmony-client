// Barrel for typed subgroups under `src/lib/types/`. The legacy flat
// `src/lib/types.ts` continues to host the rest of the app's domain
// types; new feature-scoped type groups land in this directory and
// re-export through here so consumers can `import { ... } from
// '$lib/types/index'` or `'../types'` (folder-resolution picks up
// `index.ts`).
export * from './voting';
