import { describe, expect, it } from "vitest";
import { demoSnapshot } from "../features/demo/snapshot";
import {
  areMutatingControlsLocked,
  canMonitor,
  canTakeOver,
  companionCallReducer,
  createCompanionCallState,
} from "./callReducer";

describe("companionCallReducer", () => {
  const connectedCallState = () => {
    let state = companionCallReducer(createCompanionCallState(), {
      type: "transport",
      value: "connecting",
    });
    state = companionCallReducer(state, { type: "transport", value: "connected" });
    return companionCallReducer(state, { type: "snapshot", value: demoSnapshot });
  };

  const timedOutCommandState = () => {
    let state = connectedCallState();
    state = companionCallReducer(state, {
      type: "command_sent",
      value: {
        commandId: "cmd_timed_out",
        streamNonce: demoSnapshot.streamNonce,
        sequence: demoSnapshot.sequence,
      },
    });
    return companionCallReducer(state, {
      type: "command_uncertain",
      commandId: "cmd_timed_out",
    });
  };

  it("requires both socket readiness and a fresh authoritative snapshot regardless of event order", () => {
    const connecting = companionCallReducer(createCompanionCallState(), {
      type: "transport",
      value: "connecting",
    });
    const socketFirst = companionCallReducer(connecting, { type: "transport", value: "connected" });
    expect(socketFirst.transport).toBe("connecting");
    const socketThenSnapshot = companionCallReducer(socketFirst, { type: "snapshot", value: demoSnapshot });
    expect(socketThenSnapshot.transport).toBe("connected");

    const snapshotFirst = companionCallReducer(connecting, { type: "snapshot", value: demoSnapshot });
    expect(snapshotFirst.transport).toBe("connecting");
    const snapshotThenSocket = companionCallReducer(snapshotFirst, { type: "transport", value: "connected" });
    expect(snapshotThenSocket.transport).toBe("connected");
  });

  it("can complete an authoritative idle sync without inventing a call", () => {
    const connecting = companionCallReducer(createCompanionCallState(), {
      type: "transport",
      value: "connecting",
    });
    const socket = companionCallReducer(connecting, { type: "transport", value: "connected" });
    const idle = companionCallReducer(socket, {
      type: "sync_ready",
      value: { streamNonce: "stream_idle", sequence: 0 },
    });
    expect(idle.transport).toBe("connected");
    expect(idle.authoritativeSyncReady).toBe(true);
    expect(idle.snapshot).toBeNull();
  });

  it("treats an authoritative idle sync as final for the previous call", () => {
    let state = companionCallReducer(createCompanionCallState(), {
      type: "transport",
      value: "connecting",
    });
    state = companionCallReducer(state, { type: "transport", value: "connected" });
    state = companionCallReducer(state, { type: "snapshot", value: demoSnapshot });

    const idle = companionCallReducer(state, {
      type: "sync_ready",
      value: { streamNonce: demoSnapshot.streamNonce, sequence: demoSnapshot.sequence + 1 },
    });
    expect(idle.transport).toBe("connected");
    expect(idle.snapshot).toBeNull();
    expect(idle.endedCallIds).toContain(demoSnapshot.callId);

    const late = companionCallReducer(idle, {
      type: "snapshot",
      value: { ...demoSnapshot, sequence: demoSnapshot.sequence + 2 },
    });
    expect(late.snapshot).toBeNull();
    expect(late.ignoredSnapshots).toBe(idle.ignoredSnapshots + 1);
  });

  it("accepts a fresh snapshot and ignores an older sequence", () => {
    const accepted = companionCallReducer(createCompanionCallState(), {
      type: "snapshot",
      value: demoSnapshot,
    });
    const ignored = companionCallReducer(accepted, {
      type: "snapshot",
      value: { ...demoSnapshot, sequence: demoSnapshot.sequence - 1 },
    });
    expect(ignored.snapshot).toEqual(demoSnapshot);
    expect(ignored.ignoredSnapshots).toBe(1);
  });

  it("orders the entire stream globally and accepts a reset only during resync with a new nonce", () => {
    const connected = companionCallReducer(
      { ...createCompanionCallState(), transport: "connected" },
      { type: "snapshot", value: demoSnapshot },
    );
    const otherCallOldSequence = companionCallReducer(connected, {
      type: "snapshot",
      value: { ...demoSnapshot, callId: "call_other", sequence: demoSnapshot.sequence - 1 },
    });
    expect(otherCallOldSequence.snapshot?.callId).toBe(demoSnapshot.callId);

    const reconnecting = companionCallReducer(connected, { type: "transport", value: "reconnecting" });
    const reset = companionCallReducer(reconnecting, {
      type: "snapshot",
      value: { ...demoSnapshot, streamNonce: "stream_demo_02", sequence: 1 },
    });
    expect(reset.snapshot?.streamNonce).toBe("stream_demo_02");
    expect(reset.transport).toBe("reconnecting");
  });

  it("never resurrects an ended call from a late snapshot", () => {
    const ended = companionCallReducer(createCompanionCallState(), {
      type: "snapshot",
      value: {
        ...demoSnapshot,
        sequence: 43,
        telephonyState: "ended",
        serviceMode: "ended",
        talkOwner: { kind: "none" },
        mediaState: "none",
      },
    });
    const late = companionCallReducer(ended, {
      type: "snapshot",
      value: { ...demoSnapshot, sequence: 44 },
    });
    expect(late.snapshot?.telephonyState).toBe("ended");
    expect(late.ignoredSnapshots).toBe(1);
  });

  it("requires verified disclosure before receive-only monitoring", () => {
    expect(canMonitor(demoSnapshot)).toBe(true);
    expect(canMonitor({
      ...demoSnapshot,
      disclosure: { ...demoSnapshot.disclosure, verified: false },
    })).toBe(false);
  });

  it("gates takeover on software hold, endpoint capability, and active Aokie ownership", () => {
    expect(canTakeOver(demoSnapshot)).toBe(true);
    expect(canTakeOver({
      ...demoSnapshot,
      capabilities: { ...demoSnapshot.capabilities, softwareHold: false },
    })).toBe(false);
    expect(canTakeOver({ ...demoSnapshot, serviceMode: "recovering" })).toBe(false);
  });

  it("keeps controls locked after a timeout when only the same authoritative sequence is available", () => {
    const timedOut = timedOutCommandState();
    expect(timedOut.pendingCommands).toEqual([]);
    expect(timedOut.ambiguousCommands).toEqual([{
      commandId: "cmd_timed_out",
      streamNonce: demoSnapshot.streamNonce,
      sequence: demoSnapshot.sequence,
    }]);
    expect(areMutatingControlsLocked(timedOut)).toBe(true);

    const duplicate = companionCallReducer(timedOut, {
      type: "snapshot",
      value: { ...demoSnapshot, callEpoch: demoSnapshot.callEpoch + 1 },
    });
    expect(areMutatingControlsLocked(duplicate)).toBe(true);
    expect(duplicate.ambiguousCommands).toEqual(timedOut.ambiguousCommands);
  });

  it("clears a timeout fence only after a strictly newer authoritative sequence", () => {
    const timedOut = timedOutCommandState();
    const reconciled = companionCallReducer(timedOut, {
      type: "snapshot",
      value: { ...demoSnapshot, sequence: demoSnapshot.sequence + 1 },
    });
    expect(reconciled.ambiguousCommands).toEqual([]);
    expect(areMutatingControlsLocked(reconciled)).toBe(false);
    expect(reconciled.lastError).toBeNull();
  });

  it("does not let an unknown or late acknowledgement clear a timeout fence", () => {
    const timedOut = timedOutCommandState();
    const unknownAck = companionCallReducer(timedOut, {
      type: "command_ack",
      value: { commandId: "cmd_unknown", accepted: true },
    });
    const lateAck = companionCallReducer(unknownAck, {
      type: "command_ack",
      value: { commandId: "cmd_timed_out", accepted: true },
    });
    expect(lateAck.ambiguousCommands).toEqual(timedOut.ambiguousCommands);
    expect(areMutatingControlsLocked(lateAck)).toBe(true);
  });

  it("keeps an ambiguity fence across reconnect and a replacement stream", () => {
    let state = timedOutCommandState();
    state = companionCallReducer(state, { type: "transport", value: "reconnecting" });
    state = companionCallReducer(state, { type: "transport", value: "connected" });
    state = companionCallReducer(state, {
      type: "snapshot",
      value: {
        ...demoSnapshot,
        streamNonce: "stream_after_reconnect",
        sequence: 1,
        callEpoch: demoSnapshot.callEpoch + 1,
        ownerEpoch: demoSnapshot.ownerEpoch + 1,
      },
    });
    expect(state.transport).toBe("connected");
    expect(state.ambiguousCommands).toHaveLength(1);
    expect(areMutatingControlsLocked(state)).toBe(true);
  });

  it("fences in-flight mutations when transport is lost", () => {
    const current = connectedCallState();
    const pending = companionCallReducer(
      current,
      {
        type: "command_sent",
        value: {
          commandId: "cmd_known",
          streamNonce: demoSnapshot.streamNonce,
          sequence: demoSnapshot.sequence,
        },
      },
    );
    const offline = companionCallReducer(pending, { type: "transport", value: "offline" });
    expect(offline.pendingCommands).toEqual([]);
    expect(offline.ambiguousCommands.map((command) => command.commandId)).toEqual(["cmd_known"]);
    expect(offline.lastError).toContain("duplicate action");
  });
});
