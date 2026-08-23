import { describe, it, expect, vi, beforeEach } from 'vitest';
import {
  ContactsService,
  petnameMapFromContacts,
  type ContactView,
} from './contacts-service';
import type { TauriAdapter } from './zenoh-service';

function makeAdapter(): TauriAdapter & { listeners: Map<string, Function> } {
  const listeners = new Map<string, Function>();
  return {
    listeners,
    invoke: vi.fn(),
    listen: vi.fn(async (event: string, handler: Function) => {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    }),
  } as any;
}

function view(overrides: Partial<ContactView> = {}): ContactView {
  return {
    ownerIdHex: 'aa'.repeat(16),
    petname: 'Koya',
    firstSeenMs: 100,
    updatedMs: 200,
    ...overrides,
  };
}

describe('ContactsService', () => {
  let service: ContactsService;
  let adapter: ReturnType<typeof makeAdapter>;

  beforeEach(() => {
    service = new ContactsService();
    adapter = makeAdapter();
  });

  it('connectAdapter installs the contacts-changed listener', async () => {
    await service.connectAdapter(adapter);
    expect(adapter.listeners.has('contacts-changed')).toBe(true);
  });

  it('contacts-changed notifies subscribers; unsubscribe removes only that listener', async () => {
    await service.connectAdapter(adapter);
    const a = vi.fn();
    const b = vi.fn();
    const offA = service.onContactsChanged(a);
    service.onContactsChanged(b);
    adapter.listeners.get('contacts-changed')!();
    expect(a).toHaveBeenCalledTimes(1);
    expect(b).toHaveBeenCalledTimes(1);
    offA();
    adapter.listeners.get('contacts-changed')!();
    expect(a).toHaveBeenCalledTimes(1);
    expect(b).toHaveBeenCalledTimes(2);
  });

  it('list invokes contacts_list', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue([view()]);
    const rows = await service.list();
    expect(adapter.invoke).toHaveBeenCalledWith('contacts_list', {});
    expect(rows).toHaveLength(1);
    expect(rows[0].petname).toBe('Koya');
  });

  it('setPetname invokes set_contact_petname with camelCase args (null clears)', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(view());
    await service.setPetname('aabb', 'Koya');
    expect(adapter.invoke).toHaveBeenCalledWith('set_contact_petname', {
      ownerIdHex: 'aabb',
      petname: 'Koya',
    });
    (adapter.invoke as any).mockResolvedValue(null);
    const cleared = await service.setPetname('aabb', null);
    expect(adapter.invoke).toHaveBeenLastCalledWith('set_contact_petname', {
      ownerIdHex: 'aabb',
      petname: null,
    });
    expect(cleared).toBeNull();
  });

  it('setNotes invokes set_contact_notes with camelCase args', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(view({ notes: 'gardener' }));
    const out = await service.setNotes('aabb', 'gardener');
    expect(adapter.invoke).toHaveBeenCalledWith('set_contact_notes', {
      ownerIdHex: 'aabb',
      notes: 'gardener',
    });
    expect(out?.notes).toBe('gardener');
  });

  it('normalizes string rejections into Error (Tauri IPC error extraction)', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockRejectedValue('petname too long (max 64 characters)');
    await expect(service.setPetname('aabb', 'x'.repeat(65))).rejects.toThrow(
      'petname too long (max 64 characters)',
    );
  });

  it('rejects before connectAdapter with a named-command error', async () => {
    await expect(service.list()).rejects.toThrow('ContactsService.contacts_list');
  });
});

describe('petnameMapFromContacts', () => {
  it('keys by lowercased owner hex and keeps only non-blank petnames', () => {
    const m = petnameMapFromContacts([
      view({ ownerIdHex: 'AABB', petname: 'Koya' }),
      view({ ownerIdHex: 'ccdd', petname: '   ' }),
      view({ ownerIdHex: 'eeff', petname: null }),
      view({ ownerIdHex: '0011', petname: undefined }),
    ]);
    expect(m.get('aabb')).toBe('Koya');
    expect(m.size).toBe(1);
  });
});
