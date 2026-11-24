import { expect, test } from "bun:test";
import {
  AppControlBridge,
  redactControlText,
  requireIdentity,
  requireActiveTurn,
  verifyOutcome,
  type ControlState,
  type ControlAction,
  type ControlEnvelope,
  type ControlHandler,
} from "./app-control";

function state(): ControlState {
  return {
    instanceId: "owned-instance",
    revision: 3,
    sessionId: "owned-session",
    conversationId: "owned-chat",
    activeTurnId: "owned-turn",
    window: { id: "main", widget: false, front: false },
    surface: "workspace",
    dialog: null,
    agentId: "owned-agent",
    dashboardAgentId: null,
    busy: false,
    protected: false,
    captureAllowed: true,
    composer: { text: "local fixture", editable: true, sendable: true },
    transcript: [],
    agents: [],
    conversations: [],
    config: { theme: "dark", autoScroll: true, snapScroll: false },
    controls: [],
    sessionConfig: [],
    truncated: false,
  };
}
function envelope(
  current: ControlState,
  operation: ControlAction,
  expiresAt = Date.now() + 1000,
): ControlEnvelope {
  return {
    ticket: "native-ticket",
    expiresAt,
    request: {
      requestId: "fixture-request",
      operation,
      expected: {
        instanceId: current.instanceId,
        revision: current.revision,
        sessionId: current.sessionId,
        conversationId: current.conversationId,
        activeTurnId: current.activeTurnId,
      },
    },
  };
}
function fixture(handler?: ControlHandler, claim = async () => {}) {
  const current = state();
  const acks: { value: unknown; failure: string | null }[] = [];
  let executions = 0;
  const bridge = new AppControlBridge(
    () => current,
    () =>
      handler ??
      (() => {
        executions++;
      }),
    {
      claim,
      ack: async (_ticket, value, failure) => {
        acks.push({ value, failure });
      },
    },
    (callback) => callback(),
  );
  return { current, bridge, acks, executions: () => executions };
}

test("identity binds the mounted instance, revision, conversation and runtime session", () => {
  const current = state();
  expect(() => requireIdentity(current, current)).not.toThrow();
  for (const changed of [
    { instanceId: "detached" },
    { revision: 2 },
    { sessionId: null },
    { conversationId: "other-chat" },
  ]) {
    expect(() => requireIdentity({ ...current, ...changed }, current)).toThrow(
      "Stale target",
    );
  }
  expect(() => requireIdentity(null, current)).toThrow();
});

test("only cancellation tolerates streaming revisions, never a replaced or inactive turn", () => {
  const current = state();
  const streaming = { ...current, revision: current.revision + 1 };
  expect(() => requireActiveTurn(current, streaming)).not.toThrow();
  expect(() => requireIdentity(current, streaming)).toThrow("Stale target");
  for (const changed of [
    { instanceId: "replaced-instance" },
    { sessionId: "replaced-session" },
    { conversationId: "replaced-chat" },
    { activeTurnId: "replacement-turn" },
    { activeTurnId: null },
  ]) {
    expect(() =>
      requireActiveTurn(current, { ...streaming, ...changed }),
    ).toThrow("Stale target");
  }
  expect(() =>
    requireActiveTurn(
      { ...current, activeTurnId: null },
      { ...current, activeTurnId: null },
    ),
  ).toThrow("Stale target");
});

test("snapshot returns real state without executing an action", async () => {
  const f = fixture();
  await f.bridge.receive(envelope(f.current, { action: "snapshot" }));
  expect(f.executions()).toBe(0);
  expect(f.acks[0]).toEqual({
    value: {
      requestId: "fixture-request",
      outcome: "observed",
      state: f.current,
      captureAllowed: true,
    },
    failure: null,
  });
});

test("claim rejection and deadline expiry prevent all queued execution", async () => {
  const f = fixture(undefined, async () => {
    throw new Error("revoked");
  });
  await f.bridge.receive(
    envelope(f.current, { action: "workspace_cancel_prompt" }),
  );
  expect(f.executions()).toBe(0);
  expect(f.acks).toEqual([]);
  const expired = fixture();
  await expired.bridge.receive(
    envelope(
      expired.current,
      { action: "workspace_cancel_prompt" },
      Date.now() - 1,
    ),
  );
  expect(expired.executions()).toBe(0);
  expect(expired.acks).toEqual([]);
});

test("a stale target is rejected after native claim, not clicked in another chat", async () => {
  const f = fixture();
  const request = envelope(f.current, { action: "workspace_cancel_prompt" });
  f.current.conversationId = "human-selected-other-chat";
  await f.bridge.receive(request);
  expect(f.executions()).toBe(0);
  expect(f.acks[0]?.failure).toContain("Stale target");
});

test("human-only surfaces cannot be acted on or captured", async () => {
  const f = fixture();
  f.current.protected = true;
  for (const action of ["workspace_cancel_prompt", "screenshot"] as const) {
    await f.bridge.receive(envelope(f.current, { action }));
    expect(f.acks.at(-1)?.failure).toContain("human-only");
  }
  expect(f.executions()).toBe(0);
});

test("no success is acknowledged when a typed handler silently fails to change state", async () => {
  const f = fixture();
  await f.bridge.receive(
    envelope(f.current, {
      action: "workspace_select_surface",
      surface: "canvas",
    }),
  );
  expect(f.acks[0]?.failure).toBe("Surface did not become visible");
});

test("acknowledgment contains the committed state, not the pre-action snapshot", async () => {
  const f = fixture(() => {
    f.current.surface = "canvas";
    f.current.revision++;
  });
  await f.bridge.receive(
    envelope(f.current, {
      action: "workspace_select_surface",
      surface: "canvas",
    }),
  );
  expect(f.acks[0]?.failure).toBeNull();
  const value = f.acks[0]?.value as { state: ControlState } | undefined;
  expect(value?.state.revision).toBe(4);
});

test("revoke during an awaited action prevents its late continuation and success", async () => {
  let resume!: () => void;
  let laterAction = false;
  const pending = new Promise<void>((resolve) => {
    resume = resolve;
  });
  const f = fixture(async (_action, lease) => {
    await pending;
    lease.check();
    laterAction = true;
  });
  const response = f.bridge.receive(
    envelope(f.current, { action: "workspace_cancel_prompt" }),
  );
  await Promise.resolve();
  f.bridge.cancel("native-ticket");
  resume();
  await response;
  expect(laterAction).toBe(false);
  expect(f.acks).toEqual([]);
});

test("detached instances never execute old requests after a remount", async () => {
  const f = fixture();
  f.bridge.detach();
  await f.bridge.receive(
    envelope(f.current, { action: "workspace_cancel_prompt" }),
  );
  expect(f.executions()).toBe(0);
  expect(f.acks).toEqual([]);
});

test("snapshots redact common credentials before bounding text", () => {
  for (const text of [
    "password=fixture-secret",
    'api_key: "fixture-secret"',
    "Bearer abc.def.ghi",
    "github_pat_syntheticToken123456789",
    "ghp_syntheticToken123456789",
    "-----BEGIN RSA PRIVATE KEY-----\nsynthetic secret\n-----END RSA PRIVATE KEY-----",
  ]) {
    expect(redactControlText(text)).not.toContain("fixture-secret");
    expect(redactControlText(text)).not.toContain("syntheticToken");
    expect(redactControlText(text)).not.toContain("abc.def");
    expect(redactControlText(text)).not.toContain("synthetic secret");
    expect(redactControlText(text)).toContain("redacted");
  }
  expect(redactControlText("x".repeat(9000), 2000)).toHaveLength(2000);
});

test("draft outcome comparison accounts for redaction without exposing credential bytes", () => {
  const current = state();
  const text = "password=synthetic-secret";
  current.composer.text = redactControlText(text, 8000);
  expect(() =>
    verifyOutcome({ action: "workspace_edit_draft", text }, current),
  ).not.toThrow();
});
