import { describe, it, expect } from "vitest";
import { extractHarmonyInviteUrl } from "../deep-link-router";

describe("extractHarmonyInviteUrl", () => {
  it("returns the URL when it matches harmony://invite/", () => {
    const urls = ["harmony://invite/abc123"];
    expect(extractHarmonyInviteUrl(urls)).toBe("harmony://invite/abc123");
  });

  it("returns the first matching URL when multiple given", () => {
    const urls = [
      "harmony://other/x",
      "harmony://invite/abc123",
      "harmony://invite/def",
    ];
    expect(extractHarmonyInviteUrl(urls)).toBe("harmony://invite/abc123");
  });

  it("returns null when no harmony://invite/ URL", () => {
    expect(extractHarmonyInviteUrl(["harmony://other/x"])).toBeNull();
    expect(extractHarmonyInviteUrl(["https://example.com"])).toBeNull();
    expect(extractHarmonyInviteUrl([])).toBeNull();
  });
});
