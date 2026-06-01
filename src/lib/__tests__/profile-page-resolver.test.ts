import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { ProfilePageResolver, type ProfilePageDto } from '../profile-page-resolver';

const SAMPLE_DTO: ProfilePageDto = {
  bio: 'Builder of meshes.',
  links: [{ label: 'site', url: 'https://example.com' }],
  fields: [{ key: 'pronoun', value: 'they/them' }],
};

/** A controllable fake adapter whose `invoke` resolves/rejects on demand. */
function fakeAdapter(invoke: (cmd: string, args?: Record<string, unknown>) => Promise<unknown>) {
  return {
    invoke: vi.fn(invoke),
    // ProfilePageResolver never calls listen, but TauriAdapter requires it.
    listen: vi.fn(async () => () => {}),
  };
}

describe('ProfilePageResolver', () => {
  beforeEach(() => {
    vi.useRealTimers();
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('returns undefined on first resolve and kicks off fetch_profile_doc', () => {
    const adapter = fakeAdapter(async () => SAMPLE_DTO);
    const r = new ProfilePageResolver();
    r.connectAdapter(adapter as any);

    expect(r.resolve('cid1')).toBeUndefined();
    expect(adapter.invoke).toHaveBeenCalledTimes(1);
    expect(adapter.invoke).toHaveBeenCalledWith('fetch_profile_doc', { cid: 'cid1' });
  });

  it('returns the cached DTO after the fetch resolves and fires onChange', async () => {
    const adapter = fakeAdapter(async () => SAMPLE_DTO);
    const r = new ProfilePageResolver();
    let changes = 0;
    r.onChange = () => { changes++; };
    r.connectAdapter(adapter as any);

    expect(r.resolve('cid1')).toBeUndefined();
    // Let the pending fetch promise settle.
    await vi.waitFor(() => expect(changes).toBe(1));

    expect(r.resolve('cid1')).toEqual(SAMPLE_DTO);
    // A second resolve hit the cache: no extra invoke.
    expect(adapter.invoke).toHaveBeenCalledTimes(1);
  });

  it('does not re-invoke while a fetch is pending', () => {
    let resolveFetch: (v: ProfilePageDto) => void = () => {};
    const adapter = fakeAdapter(() => new Promise<ProfilePageDto>((res) => { resolveFetch = res; }));
    const r = new ProfilePageResolver();
    r.connectAdapter(adapter as any);

    expect(r.resolve('cid1')).toBeUndefined();
    expect(r.resolve('cid1')).toBeUndefined();
    expect(adapter.invoke).toHaveBeenCalledTimes(1);
    resolveFetch(SAMPLE_DTO);
  });

  it('sets a 30s cooldown after a failed fetch (no re-invoke within 30s)', async () => {
    vi.useFakeTimers();
    const adapter = fakeAdapter(async () => { throw new Error('boom'); });
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const r = new ProfilePageResolver();
    r.connectAdapter(adapter as any);

    expect(r.resolve('cid1')).toBeUndefined();
    // Flush the rejected fetch promise so failedAt is recorded.
    await vi.runAllTimersAsync();
    expect(adapter.invoke).toHaveBeenCalledTimes(1);
    expect(warn).toHaveBeenCalled();

    // Within the cooldown window: no re-invoke.
    vi.advanceTimersByTime(29_000);
    expect(r.resolve('cid1')).toBeUndefined();
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    // After the cooldown elapses: a fresh fetch is allowed.
    vi.advanceTimersByTime(2_000);
    expect(r.resolve('cid1')).toBeUndefined();
    expect(adapter.invoke).toHaveBeenCalledTimes(2);
  });

  it('status() transitions loading → resolved over a successful fetch', async () => {
    let resolveFetch: (v: ProfilePageDto) => void = () => {};
    const adapter = fakeAdapter(() => new Promise<ProfilePageDto>((res) => { resolveFetch = res; }));
    const r = new ProfilePageResolver();
    r.connectAdapter(adapter as any);

    // Never attempted yet → treated as loading (resolve() will kick a fetch).
    expect(r.status('cid1')).toBe('loading');
    // resolve() starts the fetch; while pending, status stays 'loading'.
    expect(r.resolve('cid1')).toBeUndefined();
    expect(r.status('cid1')).toBe('loading');

    resolveFetch(SAMPLE_DTO);
    await vi.waitFor(() => expect(r.status('cid1')).toBe('resolved'));
  });

  it('status() reports error after a failed fetch (so the UI stops showing Loading)', async () => {
    vi.useFakeTimers();
    const adapter = fakeAdapter(async () => { throw new Error('boom'); });
    vi.spyOn(console, 'warn').mockImplementation(() => {});
    const r = new ProfilePageResolver();
    r.connectAdapter(adapter as any);

    expect(r.resolve('cid1')).toBeUndefined();
    // Flush the rejected fetch so failedAt is recorded.
    await vi.runAllTimersAsync();
    // resolve() still returns undefined, but status() now distinguishes the
    // failure from an in-flight fetch.
    expect(r.resolve('cid1')).toBeUndefined();
    expect(r.status('cid1')).toBe('error');
  });

  it('status() transitions error → loading once the retry cooldown elapses', async () => {
    vi.useFakeTimers();
    let attempt = 0;
    const adapter = fakeAdapter(async () => {
      attempt++;
      // First attempt fails; a post-cooldown retry would be the second invoke.
      // Keep it pending forever so we can observe the 'loading' status without
      // it racing back to 'error' on a second immediate failure.
      if (attempt === 1) throw new Error('boom');
      return new Promise<ProfilePageDto>(() => {});
    });
    vi.spyOn(console, 'warn').mockImplementation(() => {});
    const r = new ProfilePageResolver();
    r.connectAdapter(adapter as any);

    expect(r.resolve('cid1')).toBeUndefined();
    await vi.runAllTimersAsync();
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    // Still inside the cooldown → error, and resolve() does NOT re-invoke.
    vi.advanceTimersByTime(29_000);
    expect(r.status('cid1')).toBe('error');
    expect(r.resolve('cid1')).toBeUndefined();
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    // Advance past RETRY_COOLDOWN_MS (30s): a retry is now imminent, so status
    // reports 'loading' even before resolve() is called again.
    vi.advanceTimersByTime(2_000);
    expect(r.status('cid1')).toBe('loading');
    // ...and the next resolve() actually kicks the retry.
    expect(r.resolve('cid1')).toBeUndefined();
    expect(adapter.invoke).toHaveBeenCalledTimes(2);
    // While that retry is in flight, status stays 'loading'.
    expect(r.status('cid1')).toBe('loading');
  });

  it('destroy() clears the cache', async () => {
    const adapter = fakeAdapter(async () => SAMPLE_DTO);
    const r = new ProfilePageResolver();
    let changes = 0;
    r.onChange = () => { changes++; };
    r.connectAdapter(adapter as any);

    expect(r.resolve('cid1')).toBeUndefined();
    await vi.waitFor(() => expect(changes).toBe(1));
    expect(r.resolve('cid1')).toEqual(SAMPLE_DTO);

    r.destroy();
    // Cache cleared: a post-destroy resolve no longer returns the DTO.
    expect(r.resolve('cid1')).toBeUndefined();
  });

  it('resolve() after destroy() does not invoke fetch_profile_doc', () => {
    const adapter = fakeAdapter(async () => SAMPLE_DTO);
    const r = new ProfilePageResolver();
    r.connectAdapter(adapter as any);

    r.destroy();
    // A fresh CID never seen before: resolve must short-circuit on destroyed
    // and NOT kick off an IPC fetch.
    expect(r.resolve('never-seen')).toBeUndefined();
    expect(adapter.invoke).not.toHaveBeenCalled();
  });
});
