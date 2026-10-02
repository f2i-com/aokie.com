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
