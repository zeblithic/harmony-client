import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import PublishButton from '../PublishButton.svelte';
import type { ContentSensitivity } from '../../types';

function renderPublish(overrides: {
  filename?: string;
  cid?: string;
  sensitivity?: ContentSensitivity;
  confirmationOverrides?: Partial<Record<ContentSensitivity, number>>;
  onPublish?: (cid: string) => void;
  onRelease?: (cid: string) => void;
} = {}) {
  return render(PublishButton, {
    props: {
      filename: overrides.filename ?? 'my-file.txt',
      cid: overrides.cid ?? 'test-cid-123',
      sensitivity: overrides.sensitivity ?? 'private',
      confirmationOverrides: overrides.confirmationOverrides ?? {},
      onPublish: overrides.onPublish ?? vi.fn(),
      onRelease: overrides.onRelease ?? vi.fn(),
    },
  });
}

describe('PublishButton', () => {
  // ── Rendering ────────────────────────────────────────────────────

  it('renders Publish and Release buttons', () => {
    renderPublish();
    expect(screen.getByRole('button', { name: /Publish/i })).toBeTruthy();
    expect(screen.getByRole('button', { name: /Release/i })).toBeTruthy();
  });

  it('does not show any dialog initially', () => {
    renderPublish();
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  // ── Publish flow: private (double confirm) ──────────────────────

  describe('publish flow — private sensitivity (double confirm)', () => {
    it('shows DoubleConfirmDialog when Publish clicked', async () => {
      renderPublish({ sensitivity: 'private' });
      await fireEvent.click(screen.getByRole('button', { name: /Publish/i }));
      expect(screen.getByRole('dialog')).toBeTruthy();
      expect(
        screen.getByText(/Publishing makes this content permanently public/),
      ).toBeTruthy();
    });

    it('completes after double confirm and fires onPublish', async () => {
      const onPublish = vi.fn();
      renderPublish({ sensitivity: 'private', cid: 'cid-abc', onPublish });

      // Click Publish to start flow
      await fireEvent.click(screen.getByRole('button', { name: /Publish/i }));

      // Gate 1: Continue
      await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
      expect(onPublish).not.toHaveBeenCalled();

      // Gate 2: Confirm
      await fireEvent.click(screen.getByRole('button', { name: /Confirm Publish/i }));
      expect(onPublish).toHaveBeenCalledWith('cid-abc');
    });

    it('cancelling at gate 1 closes dialog without calling onPublish', async () => {
      const onPublish = vi.fn();
      renderPublish({ sensitivity: 'private', onPublish });

      await fireEvent.click(screen.getByRole('button', { name: /Publish/i }));
      await fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
      expect(onPublish).not.toHaveBeenCalled();
      expect(screen.queryByRole('dialog')).toBeNull();
    });

    it('shows second message with filename after Continue', async () => {
      renderPublish({ sensitivity: 'private', filename: 'secret.pdf' });
      await fireEvent.click(screen.getByRole('button', { name: /Publish/i }));
      await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
      expect(screen.getByText(/You are about to publish secret\.pdf/)).toBeTruthy();
    });
  });

  // ── Publish flow: public sensitivity (double confirm, same as private)

  describe('publish flow — public sensitivity (double confirm)', () => {
    it('uses double confirm for public sensitivity', async () => {
      const onPublish = vi.fn();
      renderPublish({ sensitivity: 'public', cid: 'cid-pub', onPublish });

      await fireEvent.click(screen.getByRole('button', { name: /Publish/i }));
      // Gate 1
      await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
      // Gate 2
      await fireEvent.click(screen.getByRole('button', { name: /Confirm Publish/i }));
      expect(onPublish).toHaveBeenCalledWith('cid-pub');
    });
  });

  // ── Publish flow: intimate (triple confirm) ─────────────────────

  describe('publish flow — intimate sensitivity (triple confirm)', () => {
    it('shows DoubleConfirmDialog first, then TypeToConfirmDialog', async () => {
      const onPublish = vi.fn();
      renderPublish({
        sensitivity: 'intimate',
        filename: 'love-letter.txt',
        cid: 'cid-intimate',
        onPublish,
      });

      // Click Publish
      await fireEvent.click(screen.getByRole('button', { name: /Publish/i }));
      expect(screen.getByRole('dialog')).toBeTruthy();

      // Double confirm gates
      await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
      await fireEvent.click(screen.getByRole('button', { name: /Confirm Publish/i }));

      // Now TypeToConfirmDialog should appear
      expect(screen.getByRole('dialog')).toBeTruthy();
      expect(screen.getByLabelText('Type to confirm')).toBeTruthy();
      expect(onPublish).not.toHaveBeenCalled();

      // Type the filename
      const input = screen.getByLabelText('Type to confirm');
      await fireEvent.input(input, { target: { value: 'love-letter.txt' } });

      // Confirm
      await fireEvent.click(screen.getByRole('button', { name: /Confirm Publish/i }));
      expect(onPublish).toHaveBeenCalledWith('cid-intimate');
    });

    it('does not allow confirming TypeToConfirm with wrong text', async () => {
      const onPublish = vi.fn();
      renderPublish({
        sensitivity: 'intimate',
        filename: 'love-letter.txt',
        cid: 'cid-intimate',
        onPublish,
      });

      await fireEvent.click(screen.getByRole('button', { name: /Publish/i }));
      await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
      await fireEvent.click(screen.getByRole('button', { name: /Confirm Publish/i }));

      // Type wrong text
      const input = screen.getByLabelText('Type to confirm');
      await fireEvent.input(input, { target: { value: 'wrong-file' } });

      const confirmBtn = screen.getByRole('button', { name: /Confirm Publish/i });
      expect(confirmBtn.hasAttribute('disabled')).toBe(true);
    });
  });

  // ── Publish flow: confidential (triple + OOB stub) ──────────────

  describe('publish flow — confidential sensitivity (triple + OOB)', () => {
    it('walks through all 4 gates: double → type → OOB stub', async () => {
      const onPublish = vi.fn();
      renderPublish({
        sensitivity: 'confidential',
        filename: 'top-secret.doc',
        cid: 'cid-conf',
        onPublish,
      });

      // Click Publish
      await fireEvent.click(screen.getByRole('button', { name: /Publish/i }));

      // Gate 1 (double confirm first message)
      await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));

      // Gate 2 (double confirm second message)
      await fireEvent.click(screen.getByRole('button', { name: /Confirm Publish/i }));

      // Gate 3 (type to confirm)
      const input = screen.getByLabelText('Type to confirm');
      await fireEvent.input(input, { target: { value: 'top-secret.doc' } });
      await fireEvent.click(screen.getByRole('button', { name: /Confirm Publish/i }));

      // Gate 4 (OOB stub)
      expect(screen.getByRole('dialog')).toBeTruthy();
      expect(screen.getByText(/future version/i)).toBeTruthy();
      expect(onPublish).not.toHaveBeenCalled();

      // Confirm OOB stub
      await fireEvent.click(screen.getByRole('button', { name: /Confirm/i }));
      expect(onPublish).toHaveBeenCalledWith('cid-conf');
    });
  });

  // ── Confirmation overrides ratchet UP ────────────────────────────

  describe('confirmationOverrides', () => {
    it('ratchets private up to triple when override is 3', async () => {
      const onPublish = vi.fn();
      renderPublish({
        sensitivity: 'private',
        filename: 'file.txt',
        cid: 'cid-ratchet',
        confirmationOverrides: { private: 3 },
        onPublish,
      });

      // Start flow
      await fireEvent.click(screen.getByRole('button', { name: /Publish/i }));

      // Double confirm
      await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
      await fireEvent.click(screen.getByRole('button', { name: /Confirm Publish/i }));

      // Should now show TypeToConfirm (ratcheted up)
      expect(screen.getByLabelText('Type to confirm')).toBeTruthy();

      const input = screen.getByLabelText('Type to confirm');
      await fireEvent.input(input, { target: { value: 'file.txt' } });
      await fireEvent.click(screen.getByRole('button', { name: /Confirm Publish/i }));
      expect(onPublish).toHaveBeenCalledWith('cid-ratchet');
    });

    it('does not ratchet DOWN: override 2 on intimate keeps triple', async () => {
      const onPublish = vi.fn();
      renderPublish({
        sensitivity: 'intimate',
        filename: 'file.txt',
        cid: 'cid-no-down',
        confirmationOverrides: { intimate: 2 },
        onPublish,
      });

      // Start flow
      await fireEvent.click(screen.getByRole('button', { name: /Publish/i }));

      // Double confirm
      await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
      await fireEvent.click(screen.getByRole('button', { name: /Confirm Publish/i }));

      // Should still show TypeToConfirm (not ratcheted down)
      expect(screen.getByLabelText('Type to confirm')).toBeTruthy();
    });

    it('ratchets private up to 4 (triple+OOB) when override is 4', async () => {
      const onPublish = vi.fn();
      renderPublish({
        sensitivity: 'private',
        filename: 'important.txt',
        cid: 'cid-ratchet4',
        confirmationOverrides: { private: 4 },
        onPublish,
      });

      await fireEvent.click(screen.getByRole('button', { name: /Publish/i }));

      // Double confirm
      await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
      await fireEvent.click(screen.getByRole('button', { name: /Confirm Publish/i }));

      // TypeToConfirm
      const input = screen.getByLabelText('Type to confirm');
      await fireEvent.input(input, { target: { value: 'important.txt' } });
      await fireEvent.click(screen.getByRole('button', { name: /Confirm Publish/i }));

      // OOB stub
      expect(screen.getByText(/future version/i)).toBeTruthy();
      await fireEvent.click(screen.getByRole('button', { name: /Confirm/i }));
      expect(onPublish).toHaveBeenCalledWith('cid-ratchet4');
    });
  });

  // ── Release flow: always double confirm ─────────────────────────

  describe('release flow (always double confirm)', () => {
    it('shows DoubleConfirmDialog with ephemeral messaging', async () => {
      renderPublish({ sensitivity: 'confidential' });
      await fireEvent.click(screen.getByRole('button', { name: /Release/i }));
      expect(screen.getByRole('dialog')).toBeTruthy();
      expect(
        screen.getByText(/publicly available.*persist on the network or fade/i),
      ).toBeTruthy();
    });

    it('completes after double confirm and fires onRelease', async () => {
      const onRelease = vi.fn();
      renderPublish({
        sensitivity: 'confidential',
        cid: 'cid-release',
        filename: 'memo.txt',
        onRelease,
      });

      await fireEvent.click(screen.getByRole('button', { name: /Release/i }));

      // Gate 1
      await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
      expect(onRelease).not.toHaveBeenCalled();

      // Gate 2
      await fireEvent.click(screen.getByRole('button', { name: /Confirm Release/i }));
      expect(onRelease).toHaveBeenCalledWith('cid-release');
    });

    it('shows second message with filename', async () => {
      renderPublish({ filename: 'memo.txt' });
      await fireEvent.click(screen.getByRole('button', { name: /Release/i }));
      await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
      expect(screen.getByText(/You are about to release memo\.txt/)).toBeTruthy();
    });

    it('cancelling release flow at gate 1 closes dialog', async () => {
      const onRelease = vi.fn();
      renderPublish({ onRelease });

      await fireEvent.click(screen.getByRole('button', { name: /Release/i }));
      await fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
      expect(onRelease).not.toHaveBeenCalled();
      expect(screen.queryByRole('dialog')).toBeNull();
    });

    it('release uses double confirm regardless of sensitivity', async () => {
      // Even for public sensitivity, release is double confirm
      const onRelease = vi.fn();
      renderPublish({ sensitivity: 'public', cid: 'cid-r', onRelease });

      await fireEvent.click(screen.getByRole('button', { name: /Release/i }));
      await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
      await fireEvent.click(screen.getByRole('button', { name: /Confirm Release/i }));
      expect(onRelease).toHaveBeenCalledWith('cid-r');
    });
  });

  // ── Accessibility ────────────────────────────────────────────────

  describe('accessibility', () => {
    it('uses native button elements', () => {
      renderPublish();
      const buttons = screen.getAllByRole('button');
      buttons.forEach((btn) => expect(btn.tagName).toBe('BUTTON'));
    });

    it('dialog has role="dialog" and aria-modal', async () => {
      renderPublish();
      await fireEvent.click(screen.getByRole('button', { name: /Publish/i }));
      const dialog = screen.getByRole('dialog');
      expect(dialog.getAttribute('aria-modal')).toBe('true');
    });
  });
});
