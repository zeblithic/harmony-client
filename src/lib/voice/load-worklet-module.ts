// src/lib/voice/load-worklet-module.ts
//
// ZEB-575 — robust AudioWorklet module loading inside the Tauri webview.
//
// `audioWorklet.addModule(url)` enforces same-origin on the module URL. In the
// Tauri webview the document is served from a custom origin (WebView2:
// http://tauri.localhost) while a Vite-served/emitted worklet asset lives at a
// different origin (dev: http://localhost:5173). Handing `addModule` that URL
// fails with a cross-origin SecurityError — the bug behind voice-channel join
// dying before the mic ever engages. Worse, `new URL('./x.ts', import.meta.url)`
// for a worklet isn't emitted by Vite at all in a production bundle, so it 404s
// there.
//
// We sidestep both by baking the worklet SOURCE into the bundle as a string
// (callers pass a Vite `?raw` import of the plain-JS processor) and loading it
// from a same-origin `blob:` URL. A blob: URL is same-origin to whatever
// document created it, so `addModule` accepts it in dev and prod alike, with no
// network fetch and no asset-emission to go wrong.

/**
 * Load an AudioWorklet processor module into `ctx` from its source text via a
 * same-origin blob URL.
 *
 * @param ctx    the AudioContext whose `audioWorklet` receives the module
 * @param source the worklet's JavaScript source (e.g. a Vite `?raw` import of a
 *               plain-`.js` processor that calls `registerProcessor(...)`)
 * @throws if `addModule` rejects (e.g. the source fails to parse/register).
 */
export async function loadWorkletModule(ctx: AudioContext, source: string): Promise<void> {
  const blobUrl = URL.createObjectURL(new Blob([source], { type: 'text/javascript' }));
  try {
    await ctx.audioWorklet.addModule(blobUrl);
  } finally {
    // The module is parsed synchronously by addModule; release the blob whether
    // or not registration succeeded (no leak on the error path).
    URL.revokeObjectURL(blobUrl);
  }
}
