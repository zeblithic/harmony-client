import { describe, it, expect, vi, beforeEach } from "vitest";

vi.mock("@tauri-apps/plugin-updater", () => ({
  check: vi.fn(),
}));

import { checkForUpdate } from "../updater-adapter";
import { check } from "@tauri-apps/plugin-updater";

describe("checkForUpdate", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    localStorage.clear();
  });

  it("returns the Update object when one is available", async () => {
    const fakeUpdate = {
      version: "0.1.0-alpha.2",
      available: true,
      downloadAndInstall: vi.fn(),
    };
    (check as ReturnType<typeof vi.fn>).mockResolvedValue(fakeUpdate);
    const result = await checkForUpdate();
    expect(result).toBe(fakeUpdate);
  });

  it("returns null when no update is available", async () => {
    (check as ReturnType<typeof vi.fn>).mockResolvedValue({ available: false });
    const result = await checkForUpdate();
    expect(result).toBeNull();
  });

  it("returns null and logs on network failure", async () => {
    (check as ReturnType<typeof vi.fn>).mockRejectedValue(new Error("network"));
    const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
    const result = await checkForUpdate();
    expect(result).toBeNull();
    expect(warnSpy).toHaveBeenCalled();
    warnSpy.mockRestore();
  });

  // ZEB-328 PR #160 R4 (Cursor MED): dismissal is per-EXACT-VERSION
  // (per spec §6.4), not "suppress anything <= this version". Switched
  // from `semverCompare(...) <= 0` to strict equality. Tests below
  // verify the per-version semantic.
  it("suppresses when available version exactly matches dismissed", async () => {
    localStorage.setItem("harmony.updater.dismissed_version", "0.1.0-alpha.5");
    const fakeUpdate = { version: "0.1.0-alpha.5", available: true };
    (check as ReturnType<typeof vi.fn>).mockResolvedValue(fakeUpdate);
    const result = await checkForUpdate();
    expect(result).toBeNull();
  });

  it("does NOT suppress a different version (higher) than dismissed", async () => {
    localStorage.setItem("harmony.updater.dismissed_version", "0.1.0-alpha.2");
    const fakeUpdate = { version: "0.1.0-alpha.5", available: true };
    (check as ReturnType<typeof vi.fn>).mockResolvedValue(fakeUpdate);
    const result = await checkForUpdate();
    expect(result).toBe(fakeUpdate);
  });

  it("does NOT suppress a different version (lower) than dismissed — strict per-version semantic", async () => {
    // Pre-R4, this would have been suppressed (alpha.2 <= alpha.5). With
    // strict-equality dismissal, only the exact alpha.5 is suppressed; if
    // we ever roll the manifest back to alpha.2 (hotfix rollback), users
    // see the toast and can choose to install.
    localStorage.setItem("harmony.updater.dismissed_version", "0.1.0-alpha.5");
    const fakeUpdate = { version: "0.1.0-alpha.2", available: true };
    (check as ReturnType<typeof vi.fn>).mockResolvedValue(fakeUpdate);
    const result = await checkForUpdate();
    expect(result).toBe(fakeUpdate);
  });

  // Cursor MED regression: dismissing a stable does NOT silently suppress
  // any pre-release of the same release (which would happen with `<= 0`
  // because pre-release < stable per SemVer §11).
  it("does NOT suppress pre-release when stable of same release was dismissed", async () => {
    localStorage.setItem("harmony.updater.dismissed_version", "1.0.0");
    const fakeUpdate = { version: "1.0.0-alpha.11", available: true };
    (check as ReturnType<typeof vi.fn>).mockResolvedValue(fakeUpdate);
    const result = await checkForUpdate();
    expect(result).toBe(fakeUpdate);
  });

  // ZEB-328 PR #160 R3 (Greptile) — coverage retained: with strict
  // equality, alpha → stable transition naturally shows because the
  // strings differ. semverCompare's §11 logic isn't load-bearing for
  // dismissal anymore but is correct for any future range-based usage.
  it("shows stable release as 'available' when an earlier pre-release was dismissed", async () => {
    localStorage.setItem("harmony.updater.dismissed_version", "1.0.0-alpha.5");
    const fakeUpdate = { version: "1.0.0", available: true };
    (check as ReturnType<typeof vi.fn>).mockResolvedValue(fakeUpdate);
    const result = await checkForUpdate();
    expect(result).toBe(fakeUpdate);
  });

  // ZEB-328 PR #160 R4 (CodeRabbit nitpick): SemVer §10 — build metadata
  // is ignored in precedence. semverCompare strips `+…` before comparing,
  // so `1.0.0+build1` and `1.0.0+build2` are equal under semverCompare
  // even though their raw strings differ. Dismissal (which uses
  // `semverCompare === 0`) therefore correctly suppresses re-notifications
  // when the only difference is build metadata.
  it("ignores build metadata per SemVer §10 (dismissal suppresses build-metadata variants)", async () => {
    localStorage.setItem("harmony.updater.dismissed_version", "1.0.0+build1");
    const fakeUpdate = { version: "1.0.0+build2", available: true };
    (check as ReturnType<typeof vi.fn>).mockResolvedValue(fakeUpdate);
    const result = await checkForUpdate();
    // Both versions strip to `1.0.0` per §10 → semverCompare returns 0 →
    // dismissal suppresses. This matches §10 intent: build metadata
    // changes are not "different versions" from a precedence standpoint.
    expect(result).toBeNull();
  });
});
