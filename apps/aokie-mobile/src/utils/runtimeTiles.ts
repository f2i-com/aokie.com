import type { RuntimeCapabilities } from "../bridge";

export interface RuntimeTileState {
  ready: boolean;
  /** The words under the tile's name. */
  status: string;
}

/**
 * The "Native calls" tile. Android reports why its call surface is not usable in `callInfrastructure`: the
 * usual reason is that notifications are not allowed, which the user can fix in a moment, and that is not
 * the same as a component that is missing. "Not installed" stays for what is really absent.
 */
export function nativeCallsTile(
  runtime: Pick<RuntimeCapabilities, "platform" | "nativeCallUi" | "callInfrastructure">,
): RuntimeTileState {
  if (runtime.nativeCallUi) return { ready: true, status: "Ready" };
  if (runtime.platform === "android") {
    if (runtime.callInfrastructure === "notification_permission_required") {
      return { ready: false, status: "Allow notifications" };
    }
    if (runtime.callInfrastructure === "android_8_required") {
      return { ready: false, status: "Needs Android 8 or newer" };
    }
  }
  return { ready: false, status: "Not installed" };
}

export interface NotificationPrompt {
  kind: "request" | "open_settings";
  sentence: string;
  actionLabel: string;
}

/**
 * What the setup screen says and does about denied notifications. While Android will still show its dialog the
 * button asks for the permission. Once Android has stopped showing it (the question was declined twice) a request
 * would do nothing, so the button opens the app's notification settings and the sentence says why.
 */
export function notificationPrompt(
  runtime: Pick<RuntimeCapabilities, "platform" | "notificationPermission" | "notificationPermissionBlocked">,
): NotificationPrompt | null {
  if (
    runtime.platform !== "android" ||
    runtime.notificationPermission === "granted" ||
    runtime.notificationPermission === "not_required"
  ) {
    return null;
  }
  if (runtime.notificationPermissionBlocked) {
    return {
      kind: "open_settings",
      sentence:
        "Android will not ask for notifications again because the question was declined twice. Open this app's notification settings and turn notifications on: genuine voice offers need them to ring. Microphone access remains a separate, later prompt used only after an active talk lease.",
      actionLabel: "Open notification settings",
    };
  }
  return {
    kind: "request",
    sentence:
      "Android notifications are currently denied. Genuine voice offers cannot start Core-Telecom or a foreground call surface until you allow them. Microphone access remains a separate, later prompt used only after an active talk lease.",
    actionLabel: "Allow call notifications",
  };
}
