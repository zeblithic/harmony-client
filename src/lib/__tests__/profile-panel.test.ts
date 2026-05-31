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

  it('renders link rows with correct labels and hrefs', () => {
    render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Alice', profilePageRoot: ROOT_CID },
        resolver: fakeResolver({ [ROOT_CID]: SAMPLE_DTO }),
        onClose: vi.fn(),
      },
    });
    const site = screen.getByText('My site') as HTMLAnchorElement;
    expect(site.getAttribute('href')).toBe('https://example.com');
    const room = screen.getByText('Harmony room') as HTMLAnchorElement;
    expect(room.getAttribute('href')).toBe('harmony:room/abc');
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

  it('dispatches a harmony: link click via onHarmonyLink and prevents raw navigation', async () => {
    const onHarmonyLink = vi.fn();
    const dto: ProfilePageDto = {
      bio: '',
      links: [{ label: 'Harmony room', url: 'harmony:room/abc' }],
      fields: [],
    };
    render(ProfilePanel, {
      props: {
        ownerIdHex: OWNER_HEX,
        card: { displayName: 'Alice', profilePageRoot: ROOT_CID },
        resolver: fakeResolver({ [ROOT_CID]: dto }),
        onClose: vi.fn(),
        onHarmonyLink,
      },
    });
    const room = screen.getByText('Harmony room') as HTMLAnchorElement;
    // The harmony: link still renders as a real <a> (allowlisted scheme) so the
    // href is inspectable / right-clickable, but the click is intercepted.
    expect(room.tagName).toBe('A');
    expect(room.getAttribute('href')).toBe('harmony:room/abc');

    // Dispatch a real click and confirm the default (navigation) was prevented.
    const evt = new MouseEvent('click', { bubbles: true, cancelable: true });
    room.dispatchEvent(evt);
    expect(onHarmonyLink).toHaveBeenCalledTimes(1);
    expect(onHarmonyLink).toHaveBeenCalledWith('harmony:room/abc');
    expect(evt.defaultPrevented).toBe(true);
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

  it('renders header-only when the doc is still unresolved (resolve returns undefined)', () => {
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
    expect(screen.getByText('No page content.')).toBeTruthy();
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
