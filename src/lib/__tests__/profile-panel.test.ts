import { render, screen, fireEvent, cleanup } from '@testing-library/svelte';
import { describe, it, expect, vi, afterEach } from 'vitest';
import ProfilePanel from '../components/ProfilePanel.svelte';
import type { ProfilePageDto } from '../profile-page-resolver';
import type { ProfilePageResolver } from '../profile-page-resolver';

const OWNER_HEX = 'ab'.repeat(16);
const ROOT_CID = 'cid-root-1';

const SAMPLE_DTO: ProfilePageDto = {
  bio: 'Line one.\nLine two.',
  links: [
    { label: 'My site', url: 'https://example.com' },
    { label: 'Harmony room', url: 'harmony:room/abc' },
  ],
  fields: [
    { key: 'pronoun', value: 'they/them' },
    { key: 'location', value: 'mesh' },
  ],
};

/**
 * A fake resolver returning a fixed DTO for the known root CID and undefined
 * otherwise. Synchronous — mirrors the real resolver's already-cached path so
 * the panel's $derived doc is populated on first render without async wiring
 * (which App.svelte owns via onChange in T10).
 */
function fakeResolver(map: Record<string, ProfilePageDto>): ProfilePageResolver {
  return {
    resolve: (cid: string) => map[cid],
    // A cached CID is 'resolved'; anything else is treated as still 'loading'
    // (mirrors the real resolver, where an unseen CID kicks a fetch).
    status: (cid: string) => (cid in map ? 'resolved' : 'loading'),
  } as unknown as ProfilePageResolver;
}

/**
 * A resolver whose fetch has FAILED for the given root: resolve() returns
 * undefined (as the real one does during the retry cooldown) and status()
 * reports 'error'. Used to assert the panel shows an error state, not an
 * endless "Loading…".
 */
function failedResolver(failedCid: string): ProfilePageResolver {
  return {
    resolve: () => undefined,
    status: (cid: string) => (cid === failedCid ? 'error' : 'loading'),
  } as unknown as ProfilePageResolver;
}

/**
 * A resolver whose fetch is still in flight: resolve() returns undefined and
 * status() reports 'loading' for every CID.
 */
function loadingResolver(): ProfilePageResolver {
  return {
    resolve: () => undefined,
    status: () => 'loading',
  } as unknown as ProfilePageResolver;
}

describe('ProfilePanel', () => {
  afterEach(() => cleanup());

  it('renders the header (name, status, owner id) regardless of page content', () => {
    render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Alice', statusText: 'building', profilePageRoot: ROOT_CID },
        resolver: fakeResolver({ [ROOT_CID]: SAMPLE_DTO }),
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('building')).toBeTruthy();
    expect(screen.getByText(OWNER_HEX)).toBeTruthy();
  });

  it('renders the bio (escaped, newlines preserved via pre-wrap)', () => {
    render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Alice', profilePageRoot: ROOT_CID },
        resolver: fakeResolver({ [ROOT_CID]: SAMPLE_DTO }),
        onClose: vi.fn(),
      },
    });
    // textContent preserves the raw newline; the element is text-interpolated
    // ({doc.bio}), never {@html}, so no HTML is injected.
    const bio = screen.getByText((_, el) => el?.textContent === 'Line one.\nLine two.');
    expect(bio).toBeTruthy();
  });

  it('escapes bio content (no {@html} injection: <script> + <img onerror>)', () => {
    const xssDto: ProfilePageDto = {
      bio: '<script>alert(1)</script><img src=x onerror=alert(1)>',
      links: [],
      fields: [],
    };
    const { container } = render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Eve', profilePageRoot: ROOT_CID },
        resolver: fakeResolver({ [ROOT_CID]: xssDto }),
        onClose: vi.fn(),
      },
    });
    // The bio must be rendered as TEXT, not parsed into <script>/<img> elements.
    expect(container.querySelector('img')).toBeNull();
    expect(container.querySelector('script')).toBeNull();
    expect(
      screen.getByText('<script>alert(1)</script><img src=x onerror=alert(1)>'),
    ).toBeTruthy();
  });

  it('renders an https: link as an anchor with its href; a harmony: link as a hrefless button', () => {
    render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Alice', profilePageRoot: ROOT_CID },
        resolver: fakeResolver({ [ROOT_CID]: SAMPLE_DTO }),
        onClose: vi.fn(),
      },
    });
    const site = screen.getByText('My site') as HTMLAnchorElement;
    expect(site.tagName).toBe('A');
    expect(site.getAttribute('href')).toBe('https://example.com');
    // The harmony: link is a <button> with NO href (the live-href context-menu
    // bypass surface is removed) — navigation is dispatched via onHarmonyLink.
    const room = screen.getByText('Harmony room');
    expect(room.tagName).toBe('BUTTON');
    expect(room.getAttribute('href')).toBeNull();
  });

  // ── T11: render safety — scheme allowlist + dispatch ─────────────────────
  it('renders an https: link as <a target="_blank" rel="noopener noreferrer">', () => {
    const dto: ProfilePageDto = {
      bio: '',
      links: [{ label: 'My site', url: 'https://example.com' }],
      fields: [],
    };
    render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Alice', profilePageRoot: ROOT_CID },
        resolver: fakeResolver({ [ROOT_CID]: dto }),
        onClose: vi.fn(),
      },
    });
    const site = screen.getByText('My site') as HTMLAnchorElement;
    expect(site.tagName).toBe('A');
    expect(site.getAttribute('href')).toBe('https://example.com');
    expect(site.getAttribute('target')).toBe('_blank');
    expect(site.getAttribute('rel')).toBe('noopener noreferrer');
  });

  it('dispatches a harmony: link click via onHarmonyLink (button, no live href)', async () => {
    const onHarmonyLink = vi.fn();
    const dto: ProfilePageDto = {
      bio: '',
      links: [{ label: 'Harmony room', url: 'harmony:room/abc' }],
      fields: [],
    };
    const { container } = render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Alice', profilePageRoot: ROOT_CID },
        resolver: fakeResolver({ [ROOT_CID]: dto }),
        onClose: vi.fn(),
        onHarmonyLink,
      },
    });
    const room = screen.getByText('Harmony room');
    // The harmony: link renders as a <button> — NO anchor, NO href — so there's
    // no live-href surface for the WebView right-click "Open in new tab" to
    // dispatch a raw OS deep-link past the in-app router.
    expect(room.tagName).toBe('BUTTON');
    expect(room.getAttribute('href')).toBeNull();
    expect(container.querySelector('a')).toBeNull();

    // Clicking it routes the url through onHarmonyLink.
    room.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }));
    expect(onHarmonyLink).toHaveBeenCalledTimes(1);
    expect(onHarmonyLink).toHaveBeenCalledWith('harmony:room/abc');
  });

  it('renders a disallowed-scheme link INERT (no anchor, not clickable)', () => {
    const onHarmonyLink = vi.fn();
    const dto: ProfilePageDto = {
      bio: '',
      links: [
        { label: 'javascript bomb', url: 'javascript:alert(1)' },
        { label: 'plain http', url: 'http://insecure.example' },
        { label: 'data uri', url: 'data:text/html,<script>1</script>' },
      ],
      fields: [],
    };
    const { container } = render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Eve', profilePageRoot: ROOT_CID },
        resolver: fakeResolver({ [ROOT_CID]: dto }),
        onClose: vi.fn(),
        onHarmonyLink,
      },
    });
    // None of the disallowed links produced an <a> at all.
    const anchors = Array.from(container.querySelectorAll('a'));
    expect(anchors.length).toBe(0);
    // The labels still appear as inert text.
    const inert = screen.getByText('javascript bomb');
    expect(inert.tagName).not.toBe('A');
    expect(inert.getAttribute('href')).toBeNull();
    expect(screen.getByText('plain http').tagName).not.toBe('A');
    expect(screen.getByText('data uri').tagName).not.toBe('A');
    // Clicking the inert label does nothing.
    inert.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }));
    expect(onHarmonyLink).not.toHaveBeenCalled();
  });

  it('renders harmony-scheme lookalikes INERT, but a real harmony: path as a button (Fix 3)', () => {
    // Mirror the backend `link_scheme_allowed` `:` guard: a harmony: link with an
    // embedded scheme separator (or an empty path) is a lookalike and must render
    // inert — never a dispatching <button> that could route a hostile URL.
    const onHarmonyLink = vi.fn();
    const dto: ProfilePageDto = {
      bio: '',
      links: [
        { label: 'harmony js', url: 'harmony:javascript:alert(1)' },
        { label: 'harmony data', url: 'harmony:data:text/html,1' },
        { label: 'bare harmony', url: 'harmony:' },
        { label: 'real harmony', url: 'harmony:room/abc' },
        { label: 'https with port', url: 'https://x.com:8443/p' },
      ],
      fields: [],
    };
    const { container } = render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Eve', profilePageRoot: ROOT_CID },
        resolver: fakeResolver({ [ROOT_CID]: dto }),
        onClose: vi.fn(),
        onHarmonyLink,
      },
    });

    // Lookalikes: inert <span>, no <button>, no <a>.
    for (const label of ['harmony js', 'harmony data', 'bare harmony']) {
      const el = screen.getByText(label);
      expect(el.tagName).not.toBe('BUTTON');
      expect(el.tagName).not.toBe('A');
      expect(el.getAttribute('href')).toBeNull();
      el.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }));
    }
    expect(onHarmonyLink).not.toHaveBeenCalled();

    // A real harmony: path still renders a dispatching <button>.
    const real = screen.getByText('real harmony');
    expect(real.tagName).toBe('BUTTON');
    expect(real.getAttribute('href')).toBeNull();
    real.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }));
    expect(onHarmonyLink).toHaveBeenCalledTimes(1);
    expect(onHarmonyLink).toHaveBeenCalledWith('harmony:room/abc');

    // A https: URL with a port still renders a real <a> (the port colon is in
    // the authority, not a forbidden embedded scheme — only harmony: forbids ':').
    const https = screen.getByText('https with port') as HTMLAnchorElement;
    expect(https.tagName).toBe('A');
    expect(https.getAttribute('href')).toBe('https://x.com:8443/p');

    // Exactly two anchors-or-buttons among the actionable links, no stray <a>
    // from a lookalike: only the real harmony button + the https anchor.
    expect(container.querySelectorAll('a').length).toBe(1);
    expect(container.querySelectorAll('button.profile-link').length).toBe(1);
  });

  it('renders field rows as key/value pairs', () => {
    render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Alice', profilePageRoot: ROOT_CID },
        resolver: fakeResolver({ [ROOT_CID]: SAMPLE_DTO }),
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('pronoun')).toBeTruthy();
    expect(screen.getByText('they/them')).toBeTruthy();
    expect(screen.getByText('location')).toBeTruthy();
    expect(screen.getByText('mesh')).toBeTruthy();
  });

  it('renders header-only when the card has no profilePageRoot', () => {
    render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Bob', statusText: 'offline' },
        resolver: fakeResolver({ [ROOT_CID]: SAMPLE_DTO }),
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('Bob')).toBeTruthy();
    // No About / Links / Details sections.
    expect(screen.queryByText('About')).toBeNull();
    expect(screen.queryByText('Links')).toBeNull();
    expect(screen.queryByText('Details')).toBeNull();
    expect(screen.queryByText('My site')).toBeNull();
    expect(screen.getByText('No page content.')).toBeTruthy();
  });

  it('shows a Loading placeholder when a root is set but the doc is still unresolved', () => {
    render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        // profilePageRoot set, but the resolver has nothing cached for it yet.
        card: { displayName: 'Carol', profilePageRoot: 'not-yet-cached' },
        resolver: fakeResolver({ [ROOT_CID]: SAMPLE_DTO }),
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('Carol')).toBeTruthy();
    expect(screen.queryByText('About')).toBeNull();
    // A root is present → loading, NOT the "no content" empty state.
    expect(screen.getByText('Loading profile…')).toBeTruthy();
    expect(screen.queryByText('No page content.')).toBeNull();
  });

  it('shows an error state (role=alert) when the fetch FAILED, not endless Loading', () => {
    const failedCid = 'cid-that-failed';
    render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Frank', profilePageRoot: failedCid },
        resolver: failedResolver(failedCid),
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('Frank')).toBeTruthy();
    // A failed fetch must NOT keep showing "Loading…": it surfaces the error.
    const alert = screen.getByText("Couldn't load this profile page.");
    expect(alert).toBeTruthy();
    expect(alert.getAttribute('role')).toBe('alert');
    expect(screen.queryByText('Loading profile…')).toBeNull();
    expect(screen.queryByText('No page content.')).toBeNull();
    expect(screen.queryByText('About')).toBeNull();
  });

  it('still shows the loading state (role=status) while the fetch is in flight', () => {
    render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Grace', profilePageRoot: 'cid-loading' },
        resolver: loadingResolver(),
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('Grace')).toBeTruthy();
    const loading = screen.getByText('Loading profile…');
    expect(loading).toBeTruthy();
    expect(loading.getAttribute('role')).toBe('status');
    expect(screen.queryByText("Couldn't load this profile page.")).toBeNull();
    expect(screen.queryByText('No page content.')).toBeNull();
  });

  it('omits the Links section when the doc has no links', () => {
    const noLinks: ProfilePageDto = { bio: 'just a bio', links: [], fields: [] };
    render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Dan', profilePageRoot: ROOT_CID },
        resolver: fakeResolver({ [ROOT_CID]: noLinks }),
        onClose: vi.fn(),
      },
    });
    expect(screen.getByText('About')).toBeTruthy();
    expect(screen.getByText('just a bio')).toBeTruthy();
    expect(screen.queryByText('Links')).toBeNull();
    expect(screen.queryByText('Details')).toBeNull();
  });

  it('calls onClose when the close button is clicked', async () => {
    const onClose = vi.fn();
    render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Alice', profilePageRoot: ROOT_CID },
        resolver: fakeResolver({ [ROOT_CID]: SAMPLE_DTO }),
        onClose,
      },
    });
    await fireEvent.click(screen.getByLabelText('Close profile'));
    expect(onClose).toHaveBeenCalled();
  });
});
