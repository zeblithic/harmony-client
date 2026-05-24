import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, fireEvent } from "@testing-library/svelte";
import UpdateAvailableToast from "../UpdateAvailableToast.svelte";

const baseUpdate = {
  version: "0.1.0-alpha.2",
  downloadAndInstall: vi.fn().mockResolvedValue(undefined),
};

describe("UpdateAvailableToast", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    localStorage.clear();
  });

  it("renders the available version", () => {
    const { getByText } = render(UpdateAvailableToast, {
      props: { update: baseUpdate, onDismiss: () => {} },
    });
    expect(getByText(/0\.1\.0-alpha\.2/)).toBeInTheDocument();
  });

  it("calls update.downloadAndInstall on Restart click", async () => {
    const { getByText } = render(UpdateAvailableToast, {
      props: { update: baseUpdate, onDismiss: () => {} },
    });
    await fireEvent.click(getByText(/restart to update/i));
    expect(baseUpdate.downloadAndInstall).toHaveBeenCalledOnce();
  });

  it("calls onDismiss on Later click", async () => {
    const onDismiss = vi.fn();
    const { getByText } = render(UpdateAvailableToast, {
      props: { update: baseUpdate, onDismiss },
    });
    await fireEvent.click(getByText(/later/i));
    expect(onDismiss).toHaveBeenCalledOnce();
  });

  it("persists dismissed_version on Skip click", async () => {
    const onDismiss = vi.fn();
    const { getByText } = render(UpdateAvailableToast, {
      props: { update: baseUpdate, onDismiss },
    });
    await fireEvent.click(getByText(/skip this version/i));
    expect(localStorage.getItem("harmony.updater.dismissed_version")).toBe(
      "0.1.0-alpha.2",
    );
    expect(onDismiss).toHaveBeenCalledOnce();
  });
});
