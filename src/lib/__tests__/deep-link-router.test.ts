import { describe, it, expect, beforeEach } from "vitest";
import {
  extractHarmonyInviteUrl,
  queueInviteForPostMint,
  consumeQueuedInvite,
} from "../deep-link-router";

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

describe("post-mint invite queue", () => {
  beforeEach(() => {
    // Drain any residual queued value so tests don't bleed into each other.
    consumeQueuedInvite();
  });

  it("queueInviteForPostMint stores the url", () => {
    queueInviteForPostMint("harmony://invite/v1?x=1");
    expect(consumeQueuedInvite()).toBe("harmony://invite/v1?x=1");
  });

  it("consumeQueuedInvite returns and clears", () => {
    queueInviteForPostMint("harmony://invite/v1?x=2");
    expect(consumeQueuedInvite()).toBe("harmony://invite/v1?x=2");
    expect(consumeQueuedInvite()).toBeNull();
  });

  it("consumeQueuedInvite returns null when empty", () => {
    expect(consumeQueuedInvite()).toBeNull();
  });

  it("consumeQueuedInvite is idempotent on double call", () => {
    queueInviteForPostMint("harmony://invite/v1?x=3");
    consumeQueuedInvite();
    expect(consumeQueuedInvite()).toBeNull();
  });

  it("latest queue write wins", () => {
    queueInviteForPostMint("harmony://invite/v1?x=4");
    queueInviteForPostMint("harmony://invite/v1?x=5");
    expect(consumeQueuedInvite()).toBe("harmony://invite/v1?x=5");
  });
});
