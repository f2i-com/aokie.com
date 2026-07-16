import { parseCallSnapshot } from "../protocol/codec";
import type { CallSnapshot, CommandAck } from "../protocol/types";

export type TransportState = "idle" | "connecting" | "connected" | "reconnecting" | "offline";

export interface CommandMutationFence {
  commandId: string;
  streamNonce: string;
  sequence: number;
}

export const COMMAND_AMBIGUITY_ERROR =
  "The last command may have reached Aokie, but its acknowledgement was not confirmed. To prevent a duplicate action, all call controls remain locked until a newer authoritative call update arrives.";

export interface CompanionCallState {
  transport: TransportState;
  socketConnected: boolean;
  authoritativeSyncReady: boolean;
  streamNonce: string | null;
  streamSequence: number;
  snapshot: CallSnapshot | null;
  pendingCommands: CommandMutationFence[];
  ambiguousCommands: CommandMutationFence[];
  lastError: string | null;
  ignoredSnapshots: number;
  endedCallIds: string[];
}

export type CompanionCallAction =
  | { type: "transport"; value: TransportState }
  | { type: "snapshot"; value: unknown }
  | { type: "sync_ready"; value: { streamNonce: string; sequence: number } }
  | { type: "command_sent"; value: CommandMutationFence }
  | { type: "command_uncertain"; commandId: string }
  | { type: "command_ack"; value: CommandAck }
  | { type: "error"; message: string }
  | { type: "clear_error" }
  | { type: "reset" };

export function createCompanionCallState(): CompanionCallState {
  return {
    transport: "idle",
    socketConnected: false,
    authoritativeSyncReady: false,
    streamNonce: null,
    streamSequence: 0,
    snapshot: null,
    pendingCommands: [],
    ambiguousCommands: [],
    lastError: null,
    ignoredSnapshots: 0,
    endedCallIds: [],
  };
}

export function companionCallReducer(
  state: CompanionCallState,
  action: CompanionCallAction,
): CompanionCallState {
  switch (action.type) {
    case "transport":
      if (action.value === "connected") {
        return {
          ...state,
          socketConnected: true,
          transport: state.authoritativeSyncReady
            ? "connected"
            : state.transport === "reconnecting" || state.transport === "offline"
              ? "reconnecting"
              : "connecting",
        };
      }
      return {
        ...state,
        transport: action.value,
        socketConnected: false,
        authoritativeSyncReady: false,
        pendingCommands: [],
        ambiguousCommands: mergeCommandFences(state.ambiguousCommands, state.pendingCommands),
        lastError:
          state.pendingCommands.length || state.ambiguousCommands.length
            ? COMMAND_AMBIGUITY_ERROR
            : state.lastError,
      };
    case "snapshot": {
      let incoming: CallSnapshot;
      try {
        incoming = parseCallSnapshot(action.value);
      } catch (error) {
        const ambiguousCommands = mergeCommandFences(state.ambiguousCommands, state.pendingCommands);
        const message = error instanceof Error ? error.message : "Invalid realtime snapshot";
        return {
          ...state,
          transport: "offline",
          socketConnected: false,
          authoritativeSyncReady: false,
          pendingCommands: [],
          ambiguousCommands,
          lastError: errorWithCommandAmbiguity(message, ambiguousCommands),
          ignoredSnapshots: state.ignoredSnapshots + 1,
        };
      }

      if (state.endedCallIds.includes(incoming.callId) && incoming.telephonyState !== "ended") {
        return { ...state, ignoredSnapshots: state.ignoredSnapshots + 1 };
      }
      if (
        state.streamNonce === incoming.streamNonce &&
        (incoming.sequence < state.streamSequence ||
          (incoming.sequence === state.streamSequence && state.transport === "connected"))
      ) {
        return { ...state, ignoredSnapshots: state.ignoredSnapshots + 1 };
      }
      if (state.streamNonce && state.streamNonce !== incoming.streamNonce && state.transport === "connected") {
        const ambiguousCommands = mergeCommandFences(state.ambiguousCommands, state.pendingCommands);
        return {
          ...state,
          transport: "offline",
          socketConnected: false,
          authoritativeSyncReady: false,
          pendingCommands: [],
          ambiguousCommands,
          lastError: errorWithCommandAmbiguity(
            "The realtime stream changed without a completed resync. Controls are locked.",
            ambiguousCommands,
          ),
          ignoredSnapshots: state.ignoredSnapshots + 1,
        };
      }

      const endedCallIds = incoming.telephonyState === "ended"
        ? [...new Set([...state.endedCallIds, incoming.callId])].slice(-32)
        : state.endedCallIds;
      const pendingCommands = incoming.gatewayReachable ? state.pendingCommands : [];
      const ambiguousCommands = reconcileCommandFences(
        incoming.gatewayReachable
          ? state.ambiguousCommands
          : mergeCommandFences(state.ambiguousCommands, state.pendingCommands),
        incoming.streamNonce,
        incoming.sequence,
      );
      return {
        ...state,
        snapshot: incoming,
        streamNonce: incoming.streamNonce,
        streamSequence: incoming.sequence,
        transport: incoming.gatewayReachable && state.socketConnected ? "connected" : incoming.gatewayReachable ? state.transport : "offline",
        authoritativeSyncReady: incoming.gatewayReachable,
        socketConnected: incoming.gatewayReachable ? state.socketConnected : false,
        lastError: ambiguousCommands.length ? COMMAND_AMBIGUITY_ERROR : null,
        pendingCommands,
        ambiguousCommands,
        endedCallIds,
      };
    }
    case "sync_ready": {
      const validNonce = /^[A-Za-z0-9._:-]{1,200}$/.test(action.value.streamNonce);
      if (!validNonce || !Number.isSafeInteger(action.value.sequence) || action.value.sequence < 0) {
        return {
          ...state,
          transport: "offline",
          socketConnected: false,
          authoritativeSyncReady: false,
          pendingCommands: [],
          ambiguousCommands: mergeCommandFences(state.ambiguousCommands, state.pendingCommands),
          lastError: "The gateway sent an invalid idle sync proof. Controls are locked.",
        };
      }
      if (
        state.streamNonce === action.value.streamNonce &&
        (action.value.sequence < state.streamSequence ||
          (action.value.sequence === state.streamSequence && state.transport === "connected"))
      ) {
        return { ...state, ignoredSnapshots: state.ignoredSnapshots + 1 };
      }
      if (state.streamNonce && state.streamNonce !== action.value.streamNonce && state.transport === "connected") {
        const ambiguousCommands = mergeCommandFences(state.ambiguousCommands, state.pendingCommands);
        return {
          ...state,
          transport: "offline",
          socketConnected: false,
          authoritativeSyncReady: false,
          pendingCommands: [],
          ambiguousCommands,
          lastError: errorWithCommandAmbiguity(
            "The realtime stream restarted without resync. Controls are locked.",
            ambiguousCommands,
          ),
        };
      }
      const ambiguousCommands = reconcileCommandFences(
        mergeCommandFences(state.ambiguousCommands, state.pendingCommands),
        action.value.streamNonce,
        action.value.sequence,
      );
      return {
        ...state,
        snapshot: null,
        endedCallIds: state.snapshot
          ? [...new Set([...state.endedCallIds, state.snapshot.callId])].slice(-32)
          : state.endedCallIds,
        streamNonce: action.value.streamNonce,
        streamSequence: action.value.sequence,
        authoritativeSyncReady: true,
        transport: state.socketConnected ? "connected" : state.transport,
        pendingCommands: [],
        ambiguousCommands,
        lastError: ambiguousCommands.length ? COMMAND_AMBIGUITY_ERROR : null,
      };
    }
    case "command_sent": {
      const validFence =
        /^[A-Za-z0-9._:-]{1,200}$/.test(action.value.commandId) &&
        /^[A-Za-z0-9._:-]{1,200}$/.test(action.value.streamNonce) &&
        Number.isSafeInteger(action.value.sequence) &&
        action.value.sequence >= 0;
      if (
        !validFence ||
        state.streamNonce !== action.value.streamNonce ||
        state.streamSequence !== action.value.sequence ||
        areMutatingControlsLocked(state)
      ) {
        return {
          ...state,
          lastError: "The command could not be bound to the current authoritative state. Controls remain locked.",
        };
      }
      return { ...state, pendingCommands: [...state.pendingCommands, action.value] };
    }
    case "command_uncertain": {
      const pending = state.pendingCommands.find((command) => command.commandId === action.commandId);
      if (!pending) return state;
      const ambiguousCommands = isFenceReconciled(
        pending,
        state.streamNonce,
        state.streamSequence,
      )
        ? state.ambiguousCommands
        : mergeCommandFences(state.ambiguousCommands, [pending]);
      return {
        ...state,
        pendingCommands: state.pendingCommands.filter((command) => command.commandId !== action.commandId),
        ambiguousCommands,
        lastError: ambiguousCommands.length ? COMMAND_AMBIGUITY_ERROR : null,
      };
    }
    case "command_ack":
      if (!state.pendingCommands.some((command) => command.commandId === action.value.commandId)) return state;
      return {
        ...state,
        pendingCommands: state.pendingCommands.filter((command) => command.commandId !== action.value.commandId),
        lastError: action.value.accepted
          ? null
          : action.value.error?.message ?? "The command was not accepted",
      };
    case "error":
      return {
        ...state,
        lastError: errorWithCommandAmbiguity(action.message, state.ambiguousCommands),
      };
    case "clear_error":
      return {
        ...state,
        lastError: state.ambiguousCommands.length ? COMMAND_AMBIGUITY_ERROR : null,
      };
    case "reset":
      return createCompanionCallState();
    default:
      return state;
  }
}

function mergeCommandFences(
  existing: CommandMutationFence[],
  incoming: CommandMutationFence[],
): CommandMutationFence[] {
  const merged = new Map(existing.map((fence) => [fence.commandId, fence]));
  for (const fence of incoming) merged.set(fence.commandId, fence);
  return [...merged.values()];
}

function isFenceReconciled(
  fence: CommandMutationFence,
  streamNonce: string | null,
  sequence: number,
): boolean {
  return fence.streamNonce === streamNonce && sequence > fence.sequence;
}

function reconcileCommandFences(
  fences: CommandMutationFence[],
  streamNonce: string,
  sequence: number,
): CommandMutationFence[] {
  return fences.filter((fence) => !isFenceReconciled(fence, streamNonce, sequence));
}

function errorWithCommandAmbiguity(
  message: string,
  ambiguousCommands: CommandMutationFence[],
): string {
  return ambiguousCommands.length ? `${message} ${COMMAND_AMBIGUITY_ERROR}` : message;
}

export function areMutatingControlsLocked(state: CompanionCallState): boolean {
  return state.pendingCommands.length > 0 || state.ambiguousCommands.length > 0;
}

export function canMonitor(snapshot: CallSnapshot | null): boolean {
  return Boolean(
    snapshot?.gatewayReachable &&
      snapshot.capabilities.monitorAudio &&
      (!snapshot.disclosure.required || snapshot.disclosure.verified) &&
      snapshot.telephonyState === "active",
  );
}

export function canTakeOver(snapshot: CallSnapshot | null): boolean {
  return Boolean(
    snapshot?.gatewayReachable &&
      snapshot.capabilities.takeover &&
      snapshot.capabilities.softwareHold &&
      snapshot.serviceMode === "aokie_active" &&
      snapshot.telephonyState === "active" &&
      snapshot.mediaState !== "failed",
  );
}
