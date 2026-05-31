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
});
