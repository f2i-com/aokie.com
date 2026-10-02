import { describe, expect, it } from "vitest";
import { nativeCallsTile, notificationPrompt } from "./runtimeTiles";

describe("nativeCallsTile", () => {
  it("says Ready when the native call surface works", () => {
    expect(nativeCallsTile({ platform: "android", nativeCallUi: true, callInfrastructure: "ready_for_authoritative_offers" }))
      .toEqual({ ready: true, status: "Ready" });
  });

  it("names denied notifications instead of calling the component missing", () => {
    expect(nativeCallsTile({ platform: "android", nativeCallUi: false, callInfrastructure: "notification_permission_required" }))
      .toEqual({ ready: false, status: "Allow notifications" });
  });

  it("says when Android is too old", () => {
    expect(nativeCallsTile({ platform: "android", nativeCallUi: false, callInfrastructure: "android_8_required" }))
      .toEqual({ ready: false, status: "Needs Android 8 or newer" });
  });

  it("keeps Not installed for a component that is really absent", () => {
    // Android reports it can take offers, yet the call surface is not there: the manifest lacks the permission.
    expect(nativeCallsTile({ platform: "android", nativeCallUi: false, callInfrastructure: "ready_for_authoritative_offers" }))
      .toEqual({ ready: false, status: "Not installed" });
    // Outside Android there is no native call surface.
    expect(nativeCallsTile({ platform: "windows", nativeCallUi: false, callInfrastructure: "not_applicable" }))
      .toEqual({ ready: false, status: "Not installed" });
    // The words about notifications belong to Android only.
    expect(nativeCallsTile({ platform: "windows", nativeCallUi: false, callInfrastructure: "notification_permission_required" }))
      .toEqual({ ready: false, status: "Not installed" });
  });
});

describe("notificationPrompt", () => {
  const denied = { platform: "android", notificationPermission: "denied", notificationPermissionBlocked: false };

  it("asks for the permission while Android will still show its dialog", () => {
    const prompt = notificationPrompt(denied);
    expect(prompt?.kind).toBe("request");
    expect(prompt?.actionLabel).toBe("Allow call notifications");
  });

  it("opens the notification settings, and says why, once Android will not ask again", () => {
    const prompt = notificationPrompt({ ...denied, notificationPermissionBlocked: true });
    expect(prompt?.kind).toBe("open_settings");
    expect(prompt?.actionLabel).toBe("Open notification settings");
    expect(prompt?.sentence).toMatch(/will not ask for notifications again/);
  });

  it("offers nothing when notifications are allowed, not needed, or the platform is not Android", () => {
    expect(notificationPrompt({ ...denied, notificationPermission: "granted" })).toBeNull();
    expect(notificationPrompt({ ...denied, notificationPermission: "not_required" })).toBeNull();
    expect(notificationPrompt({ ...denied, platform: "windows" })).toBeNull();
    // A stale blocked flag never shows the settings button for allowed notifications.
    expect(notificationPrompt({ ...denied, notificationPermission: "granted", notificationPermissionBlocked: true })).toBeNull();
  });
});
