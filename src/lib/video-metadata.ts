/**
 * Read a video blob URL's duration from container metadata (ZEB-612 S2).
 *
 * Used by the VinePublishDialog's honest ≤6s gate and injectable there as
 * the `probeDuration` prop — jsdom implements no media pipeline, so tests
 * stub the prop rather than this DOM glue.
 */
export function probeVideoDuration(url: string): Promise<number> {
  return new Promise((resolve, reject) => {
    const video = document.createElement('video');
    video.preload = 'metadata';
    const cleanup = () => {
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
      resolve(d);
    };
    video.onerror = () => {
      cleanup();
      reject(new Error('could not read video metadata'));
    };
    video.src = url;
  });
}
