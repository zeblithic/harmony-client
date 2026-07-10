/**
 * Read a video blob URL's duration from container metadata (ZEB-612 S2).
 *
 * Used by the VinePublishDialog's honest ≤6s gate and injectable there as
 * the `probeDuration` prop — jsdom implements no media pipeline, so tests
 * stub the prop rather than this DOM glue.
 *
 * Rejection = "could not measure": callers fail open (the gate is an
 * honesty courtesy, not security). That includes non-finite durations —
 * WebM/MediaRecorder blobs can report `Infinity`/`NaN`, which is "no
 * metadata", not a measurement — and a stalled read that never fires
 * either media event (timeout below), so an awaiting caller can't hang.
 */

/** How long to wait for `loadedmetadata` before treating the read as stalled. */
export const PROBE_TIMEOUT_MS = 5000;

export function probeVideoDuration(url: string): Promise<number> {
  return new Promise((resolve, reject) => {
    const video = document.createElement('video');
    video.preload = 'metadata';
    const timeoutId = setTimeout(() => {
      cleanup();
      reject(new Error('timed out reading video metadata'));
    }, PROBE_TIMEOUT_MS);
    const cleanup = () => {
      clearTimeout(timeoutId);
      video.onloadedmetadata = null;
      video.onerror = null;
      video.removeAttribute('src');
      try {
        video.load();
      } catch {
        // jsdom: HTMLMediaElement.load is not implemented.
      }
    };
    video.onloadedmetadata = () => {
      const d = video.duration;
      cleanup();
      if (Number.isFinite(d) && d >= 0) {
        resolve(d);
      } else {
        reject(new Error('video metadata reports no finite duration'));
      }
    };
    video.onerror = () => {
      cleanup();
      reject(new Error('could not read video metadata'));
    };
    video.src = url;
  });
}
