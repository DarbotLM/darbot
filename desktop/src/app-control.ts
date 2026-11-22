import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { flushSync } from "react-dom";

export type ControlIdentity = {
  instanceId: string;
  revision: number;
  conversationId: string | null;
  sessionId: string | null;
  activeTurnId: string | null;
};

export type ControlAction =
  | { action: "snapshot" | "screenshot" | "workspace_close_dialog" | "workspace_cancel_prompt" }
  | { action: "workspace_select_surface"; surface: "workspace" | "canvas" | "cli" }
  | { action: "workspace_open_dialog"; dialog: "settings" | "resources" | "conversations" }
  | { action: "workspace_edit_draft"; text: string }
  | { action: "workspace_new_chat"; agentId: string }
  | { action: "workspace_open_chat"; conversationId: string }
  | { action: "workspace_send_prompt"; text: string; agentId: string };

export type ControlRequest = {
  requestId: string;
  expected?: ControlIdentity | null;
  operation: ControlAction;
};

export type ControlEnvelope = {
  ticket: string;
  expiresAt: number;
  request: ControlRequest;
};

export type ControlState = ControlIdentity & {
  window: { id: "main"; widget: boolean; front: boolean };
  surface: string;
  dialog: string | null;
  agentId: string;
  dashboardAgentId: string | null;
  busy: boolean;
  protected: boolean;
  captureAllowed: boolean;
  composer: {
    text: string; editable: boolean; sendable: boolean;
    attachmentCount?: number; attachmentAccess?: "local-review-only" | "none";
  };
  transcript: { role: string; text: string }[];
  agents: { id: string; name: string }[];
  conversations: { id: string; agentId: string; sessionId: string | null; title: string }[];
  config: { theme: string; autoScroll: boolean; snapScroll: boolean };
  sessionConfig: { id: string; value: string }[];
  controls: { id: string; action: string; enabled: boolean }[];
  truncated: boolean;
};

export type ControlLease = {
  ticket: string;
  signal: AbortSignal;
  check: () => void;
};

export type ControlHandler = (action: ControlAction, lease: ControlLease) => Promise<void> | void;

/** Never serializes raw SDK events, tool arguments, auth forms, permissions or elicitation values. */
export function redactControlText(text: string, limit = 4000): string {
  return text
    .replace(/-----BEGIN [^-]*PRIVATE KEY-----[\s\S]*?(?:-----END [^-]*PRIVATE KEY-----|$)/g, "[redacted private key]")
    .replace(/\b(?:Bearer\s+|(?:github_pat_|gh[pousr]_|sk-)[A-Za-z0-9_-]*)[A-Za-z0-9._~+/-]+/gi, "[redacted credential]")
    .replace(/\b(?:password|passwd|secret|api[_-]?key|access[_-]?token|refresh[_-]?token|authorization)\s*[:=]\s*(?:"[^"]*"|'[^']*'|[^\s,;]+)/gi, "[redacted credential]")
    .slice(0, limit);
}

export function requireIdentity(expected: ControlIdentity | null | undefined, current: ControlIdentity): void {
  if (!expected || expected.instanceId !== current.instanceId ||
      expected.revision !== current.revision || expected.sessionId !== current.sessionId ||
      expected.conversationId !== current.conversationId || expected.activeTurnId !== current.activeTurnId) {
    throw new Error("Stale target: obtain a new snapshot before acting");
  }
}

export function requireActiveTurn(expected: ControlIdentity | null | undefined, current: ControlIdentity): void {
  if (!expected || !expected.activeTurnId || !expected.sessionId || !expected.conversationId ||
      expected.instanceId !== current.instanceId || expected.sessionId !== current.sessionId ||
      expected.conversationId !== current.conversationId || expected.activeTurnId !== current.activeTurnId) {
    throw new Error("Stale target: obtain a new snapshot of the active turn before cancelling");
  }
}

export function verifyOutcome(action: ControlAction, state: ControlState): void {
  switch (action.action) {
    case "workspace_select_surface":
      if (state.surface !== action.surface || state.window.front) throw new Error("Surface did not become visible");
      break;
    case "workspace_open_dialog":
      if (state.dialog !== action.dialog) throw new Error("Dialog did not open");
      break;
    case "workspace_close_dialog":
      if (state.dialog !== null) throw new Error("Dialog did not close");
      break;
    case "workspace_edit_draft":
      if (state.composer.text !== redactControlText(action.text, 8000)) throw new Error("Draft was not applied");
      break;
    case "workspace_new_chat":
      if (state.agentId !== action.agentId) throw new Error("Agent was not selected");
      break;
    case "workspace_open_chat":
      if (state.conversationId !== action.conversationId) throw new Error("Conversation was not selected");
      break;
  }
}

type BridgeIo = {
  claim: (ticket: string) => Promise<void>;
  ack: (ticket: string, value: unknown, failure: string | null) => Promise<void>;
};

/** Ticket lifetime and revision checks sit beside handlers, not DOM clicks or synthetic events. */
export class AppControlBridge {
  private active = new Map<string, AbortController>();
  private detached = false;
  constructor(
    private readonly state: () => ControlState,
    private readonly handler: () => ControlHandler,
    private readonly io: BridgeIo,
    private readonly commit: (callback: () => void) => void = (callback) => flushSync(callback),
  ) {}

  cancel(ticket: string): void { this.active.get(ticket)?.abort(); }
  detach(): void {
    this.detached = true;
    for (const controller of this.active.values()) controller.abort();
    this.active.clear();
  }

  async receive(envelope: ControlEnvelope): Promise<void> {
    if (this.detached || this.active.has(envelope.ticket)) return;
    const controller = new AbortController();
    this.active.set(envelope.ticket, controller);
    const lease: ControlLease = {
      ticket: envelope.ticket,
      signal: controller.signal,
      check: () => {
        if (this.detached || controller.signal.aborted || Date.now() >= envelope.expiresAt) {
          controller.abort();
          throw new Error("App-control request expired or disconnected");
        }
      },
    };
    const timer = setTimeout(() => controller.abort(), Math.max(0, envelope.expiresAt - Date.now()));
    let claimed = false;
    try {
      lease.check();
      await this.io.claim(envelope.ticket);
      claimed = true;
      lease.check();
      const { operation, expected } = envelope.request;
      if (operation.action === "workspace_cancel_prompt") requireActiveTurn(expected, this.state());
      else if (operation.action !== "snapshot") requireIdentity(expected, this.state());
      if (operation.action !== "snapshot" && this.state().protected) {
        throw new Error("A human-only surface is active");
      }
      if (operation.action === "screenshot" && !this.state().captureAllowed) {
        throw new Error("Screenshot unavailable on this surface");
      }
      if (operation.action !== "snapshot" && operation.action !== "screenshot") {
        let task: Promise<void> | void = undefined;
        this.commit(() => {
          lease.check();
          task = this.handler()(operation, lease);
        });
        await task;
      }
      lease.check();
      this.commit(() => {});
      const state = this.state();
      verifyOutcome(operation, state);
      const outcome = operation.action === "workspace_cancel_prompt" ? "cancel_requested" :
        operation.action === "workspace_send_prompt" ? "turn_completed" :
        operation.action === "snapshot" || operation.action === "screenshot" ? "observed" : "applied";
      await this.io.ack(envelope.ticket, {
        requestId: envelope.request.requestId, outcome, state, captureAllowed: state.captureAllowed,
      }, null);
    } catch (failure) {
      if (claimed && !controller.signal.aborted && !this.detached) {
        // Only our bounded errors cross the boundary, never arbitrary SDK or credential errors.
        const message = failure instanceof Error ? redactControlText(failure.message, 240) : "App operation failed";
        await this.io.ack(envelope.ticket, null, message).catch(() => {});
      }
    } finally {
      clearTimeout(timer);
      this.active.delete(envelope.ticket);
    }
  }
}

export type ControlStatus = { enabled: boolean; endpoint: string | null; discoveryFile: string | null };
export type StateInput = Omit<ControlState, "revision" | "instanceId" | "controls">;

export function useAppControl(input: StateInput, handler: ControlHandler, revisionKey = "") {
  const [instanceId] = useState(() => crypto.randomUUID());
  const [status, setStatus] = useState<ControlStatus>({ enabled: false, endpoint: null, discoveryFile: null });
  const [problem, setProblem] = useState("");
  const [changing, setChanging] = useState(false);
  const [, forceCommit] = useState(0);
  const current = useRef<ControlState>({ ...input, instanceId, revision: 0, controls: [] });
  const last = useRef("");
  const handlerRef = useRef(handler);
  useLayoutEffect(() => {
    handlerRef.current = handler;
    const fingerprint = `${JSON.stringify(input)}:${revisionKey}`;
    if (fingerprint !== last.current) {
      last.current = fingerprint;
      const supported = [
        ["workspace_select_surface", !input.protected],
        ["workspace_open_dialog", !input.protected],
        ["workspace_close_dialog", !input.protected && input.dialog !== null],
        ["workspace_edit_draft", input.composer.editable],
        ["workspace_send_prompt", input.composer.sendable],
        ["workspace_cancel_prompt", !input.protected && input.activeTurnId !== null && input.sessionId !== null],
        ["workspace_new_chat", !input.busy && !input.protected],
        ["workspace_open_chat", !input.busy && !input.protected],
      ] as const;
      current.current = { ...input, instanceId, revision: current.current.revision + 1,
        controls: supported.map(([action, enabled]) => ({ id: `${instanceId}:${action}`, action, enabled })),
      };
    }
  });
  useEffect(() => {
    let active = true;
    const unlisten: (() => void)[] = [];
    const bridge = new AppControlBridge(() => current.current, () => handlerRef.current, {
      claim: (ticket) => invoke("app_control_claim", { ticket }),
      ack: (ticket, value, failure) => invoke("app_control_ack", { ticket, value, failure }),
    }, (callback) => flushSync(() => { callback(); forceCommit((n) => n + 1); }));
    const register = async () => {
      for (const [event, callback] of [
        ["app-control:request", (payload: unknown) => { void bridge.receive(payload as ControlEnvelope); }],
        ["app-control:cancel", (payload: unknown) => bridge.cancel(payload as string)],
        ["app-control:problem", () => { if (active) setProblem("The expired app-control turn could not be cancelled. Check the conversation."); }],
      ] as const) {
        const stop = await listen<unknown>(event, ({ payload }) => callback(payload));
        if (!active) stop(); else unlisten.push(stop);
      }
    };
    void register().catch(() => {
      if (active) setProblem("The app-control renderer connection is unavailable.");
    });
    const refresh = () => {
      void invoke("app_control_heartbeat").catch(() => {
        if (active) setProblem("The native app-control connection was lost.");
      });
      void invoke<ControlStatus>("app_control_status").then((next) => {
        if (active && next) setStatus(next);
      }).catch(() => {
        if (active) setProblem("App-control status could not be read.");
      });
    };
    refresh();
    const timer = setInterval(refresh, 5000);
    return () => {
      active = false;
      clearInterval(timer);
      bridge.detach();
      unlisten.forEach((stop) => stop());
      // Detaching never changes the conversation, but revokes every app-control capability.
      void invoke("app_control_stop").catch(() => {});
    };
  }, []);

  async function toggle() {
    if (changing) return;
    setChanging(true);
    setProblem("");
    try {
      if (status.enabled) {
        await invoke("app_control_stop");
        setStatus({ enabled: false, endpoint: null, discoveryFile: null });
      } else {
        setStatus(await invoke<ControlStatus>("app_control_start"));
      }
    } catch {
      setProblem("App control was not changed. Native approval may have been denied, or the private local service is unavailable.");
    } finally { setChanging(false); }
  }
  return { status, problem, changing, toggle };
}
