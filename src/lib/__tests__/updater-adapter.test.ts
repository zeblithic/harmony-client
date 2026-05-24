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

  it("respects dismissed_version localStorage entry (suppresses when dismissed >= available)", async () => {
    localStorage.setItem("harmony.updater.dismissed_version", "0.1.0-alpha.5");
    const fakeUpdate = { version: "0.1.0-alpha.2", available: true };
    (check as ReturnType<typeof vi.fn>).mockResolvedValue(fakeUpdate);
    const result = await checkForUpdate();
    expect(result).toBeNull();
  });

  it("does NOT suppress when available version is higher than dismissed", async () => {
    localStorage.setItem("harmony.updater.dismissed_version", "0.1.0-alpha.2");
    const fakeUpdate = { version: "0.1.0-alpha.5", available: true };
    (check as ReturnType<typeof vi.fn>).mockResolvedValue(fakeUpdate);
    const result = await checkForUpdate();
    expect(result).toBe(fakeUpdate);
  });
});
