import { describe, expect, it } from "vitest";
import { nativeCallsTile } from "./runtimeTiles";

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
