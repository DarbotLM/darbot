import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";
import type { CopilotAgentSummary } from "./copilot-types";
import { asProblem, InlineFailure, type Problem } from "./Problem";

type ElicitationField = {
  type: "string" | "number" | "integer" | "boolean" | "array";
  title?: string;
  description?: string;
  default?: string | number | boolean | string[];
  enum?: string[];
  enumNames?: string[];
  oneOf?: { const: string; title?: string }[];
  items?: { enum?: string[]; anyOf?: { const: string; title?: string }[] };
  minLength?: number;
  maxLength?: number;
  minimum?: number;
  maximum?: number;
  minItems?: number;
  maxItems?: number;
  format?: string;
};

export type CopilotElicitationRequest = {
  requestId: string;
  sessionId?: string;
  mode: "form" | "url";
  message: string;
  url?: string;
  elicitationId?: string;
  requestedSchema?: {
    type: "object";
    properties: Record<string, ElicitationField>;
    required?: string[];
  };
};

export function ElicitationReview({
  request,
  onRespond,
}: {
  request: CopilotElicitationRequest;
  onRespond: (
    action: "accept" | "decline" | "cancel",
    content?: Record<string, unknown>,
  ) => Promise<void>;
}) {
  const [values, setValues] = useState<Record<string, unknown>>(() =>
    Object.fromEntries(
      Object.entries(request.requestedSchema?.properties ?? {})
        .filter(([, field]) => field.default !== undefined)
        .map(([name, field]) => [name, field.default]),
    ),
  );
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<Problem | null>(null);
  async function respond(action: "accept" | "decline" | "cancel") {
    if (busy) return;
    setBusy(true);
    setProblem(null);
    try {
      await onRespond(
        action,
        action === "accept" && request.mode === "form" ? values : undefined,
      );
    } catch (error) {
      setProblem(asProblem(error));
    } finally {
      setBusy(false);
    }
  }
  const set = (name: string, value: unknown) =>
    setValues((current) => {
      const entries = Object.entries(current).filter(([key]) => key !== name);
      return Object.fromEntries(
        value === undefined ? entries : [...entries, [name, value]],
      );
    });
  let host = "";
  if (request.mode === "url" && request.url) {
    try {
      host = new URL(request.url).hostname;
    } catch {
      host = "Invalid URL";
    }
  }
  return (
    <>
      <div className="copilot-dialog-header">
        <h2 id="copilot-permission-title">Agent information request</h2>
      </div>
      <div className="copilot-dialog-body">
        <p>
          Connected ACP agent
          {request.sessionId ? ` · session ${request.sessionId}` : ""}
        </p>
        <p>{request.message}</p>
        {request.mode === "url" ? (
          <>
            <p>
              External site: <strong>{host}</strong>
            </p>
            <pre>{request.url}</pre>
            <p>
              Open only if you trust this site. It opens in your browser, not in
              the agent. Opening does not confirm that sign-in or the external
              action completed.
            </p>
            {host.includes("xn--") && (
              <p className="caution">
                This hostname contains Punycode. Check the full destination
                carefully.
              </p>
            )}
          </>
        ) : (
          <form
            data-ui-action="workspace_permission_decide"
            id={`elicitation-${request.requestId}`}
            onSubmit={(event) => {
              event.preventDefault();
              void respond("accept");
            }}
          >
            {Object.entries(request.requestedSchema?.properties ?? {}).map(
              ([name, field]) => {
                const value = values[name];
                const required =
                  request.requestedSchema?.required?.includes(name) ?? false;
                const options =
                  field.oneOf ??
                  field.enum?.map((item, index) => ({
                    const: item,
                    title: field.enumNames?.[index],
                  }));
                const arrayOptions =
                  field.items?.anyOf ??
                  field.items?.enum?.map((item) => ({
                    const: item,
                    title: item,
                  }));
                const fieldId = `elicitation-${request.requestId}-${encodeURIComponent(name)}`;
                return (
                  <label key={name} htmlFor={fieldId}>
                    {field.title ?? name}
                    {required ? " (required)" : ""}
                    {field.description && <small>{field.description}</small>}
                    {field.type === "boolean" ? (
                      <select
                        id={fieldId}
                        data-ui-action="inspect_details"
                        disabled={busy}
                        required={required}
                        value={value === undefined ? "" : String(value)}
                        onChange={(event) =>
                          set(
                            name,
                            event.target.value === ""
                              ? undefined
                              : event.target.value === "true",
                          )
                        }
                      >
                        <option value="">Choose</option>
                        <option value="true">Yes</option>
                        <option value="false">No</option>
                      </select>
                    ) : field.type === "array" ? (
                      <select
                        id={fieldId}
                        multiple
                        data-ui-action="inspect_details"
                        disabled={busy}
                        required={required && (field.minItems ?? 0) > 0}
                        value={
                          Array.isArray(value)
                            ? value.filter(
                                (item): item is string =>
                                  typeof item === "string",
                              )
                            : []
                        }
                        onChange={(event) =>
                          set(
                            name,
                            Array.from(
                              event.target.selectedOptions,
                              (option) => option.value,
                            ),
                          )
                        }
                      >
                        {arrayOptions?.map((option) => (
                          <option key={option.const} value={option.const}>
                            {option.title ?? option.const}
                          </option>
                        ))}
                      </select>
                    ) : options ? (
                      <select
                        id={fieldId}
                        data-ui-action="inspect_details"
                        disabled={busy}
                        required={required}
                        value={
                          typeof value === "string" ? JSON.stringify(value) : ""
                        }
                        onChange={(event) =>
                          set(
                            name,
                            event.target.value === ""
                              ? undefined
                              : JSON.parse(event.target.value),
                          )
                        }
                      >
                        <option value="">Choose</option>
                        {options.map((option) => (
                          <option
                            key={option.const}
                            value={JSON.stringify(option.const)}
                          >
                            {option.title ?? option.const}
                          </option>
                        ))}
                      </select>
                    ) : (
                      <input
                        id={fieldId}
                        data-ui-action="inspect_details"
                        disabled={busy}
                        required={
                          required &&
                          (field.type !== "string" ||
                            (field.minLength ?? 0) > 0)
                        }
                        type={
                          field.type === "number" || field.type === "integer"
                            ? "number"
                            : field.format === "email"
                              ? "email"
                              : field.format === "date"
                                ? "date"
                                : "text"
                        }
                        step={field.type === "integer" ? 1 : "any"}
                        min={field.minimum}
                        max={field.maximum}
                        minLength={field.minLength}
                        maxLength={field.maxLength}
                        value={
                          typeof value === "string" || typeof value === "number"
                            ? value
                            : ""
                        }
                        onChange={(event) => {
                          const text = event.target.value;
                          set(
                            name,
                            field.type === "number" || field.type === "integer"
                              ? text === ""
                                ? undefined
                                : Number(text)
                              : text,
                          );
                        }}
                      />
                    )}
                  </label>
                );
              },
            )}
          </form>
        )}
        <div className="row">
          <button
            data-ui-action="workspace_permission_decide"
            disabled={busy}
            type={request.mode === "form" ? "submit" : "button"}
            form={
              request.mode === "form"
                ? `elicitation-${request.requestId}`
                : undefined
            }
            onClick={
              request.mode === "url"
                ? () => {
                    void respond("accept");
                  }
                : undefined
            }
          >
            {request.mode === "url" ? "Open in browser" : "Submit response"}
          </button>
          <button
            type="button"
            className="quiet"
            data-ui-action="workspace_permission_decide"
            disabled={busy}
            onClick={() => {
              void respond("decline");
            }}
          >
            Decline
          </button>
          <button
            type="button"
            className="quiet"
            data-ui-action="workspace_permission_decide"
            disabled={busy}
            onClick={() => {
              void respond("cancel");
            }}
          >
            Cancel request
          </button>
        </div>
        {problem && <InlineFailure problem={problem} />}
      </div>
    </>
  );
}

type Feature<T> =
  | { state: "available"; data: T }
  | { state: "unavailable"; problem: Problem };

type ExtensionPlugin = {
  id: string;
  name: string;
  enabled: boolean;
  version?: string | null;
  canToggle: boolean;
  origin: string;
};

type ExtensionTask = {
  id: string;
  kind: string;
  status: string;
  title: string;
  output?: string | null;
  outputTruncated: boolean;
};

type RemoteSession = {
  id: string;
  title: string;
  modifiedAt: string;
  state?: string | null;
};

type ExtensionSnapshot = {
  schemaVersion: number;
  sdkVersion: string;
  runtimeVersion: string;
  protocolVersion: number;
  connectionId: string;
  sessionId: string;
  cwd: string;
  memoryEnabled: boolean;
  agents: Feature<CopilotAgentSummary[]>;
  tasks: Feature<ExtensionTask[]>;
  plugins: Feature<ExtensionPlugin[]>;
  remoteSessions?: Feature<RemoteSession[]> | null;
};

type ExtensionAction = Record<string, unknown>;

const unavailable = (feature: Feature<unknown>): string | null =>
  feature.state === "unavailable" ? feature.problem.said : null;

export function CopilotExtensionsPanel({ cwd }: { cwd: string }) {
  const [snapshot, setSnapshot] = useState<ExtensionSnapshot | null>(null);
  const [memoryEnabled, setMemoryEnabled] = useState(false);
  const [taskAgent, setTaskAgent] = useState("");
  const [taskName, setTaskName] = useState("");
  const [taskPrompt, setTaskPrompt] = useState("");
  const [problem, setProblem] = useState<Problem | null>(null);
  const [busy, setBusy] = useState(false);
  const mismatchedFolder = snapshot !== null && snapshot.cwd !== cwd;

  useEffect(() => {
    let active = true;
    void invoke<ExtensionSnapshot | null>("copilot_extensions_status")
      .then((value) => {
        if (active && value) {
          setSnapshot(value);
          setMemoryEnabled(value.memoryEnabled);
        }
      })
      .catch((error) => {
        if (active) setProblem(asProblem(error));
      });
    return () => {
      active = false;
    };
  }, []);

  async function run(action: ExtensionAction, keepOpen = true) {
    if (busy) return false;
    if (
      mismatchedFolder &&
      ["start-task", "start-fleet", "set-plugin-enabled"].includes(
        String(action.kind),
      )
    ) {
      setProblem({
        said: "The background workspace belongs to a different folder. Disconnect it and connect the selected folder before starting new work.",
      });
      return false;
    }
    setBusy(true);
    setProblem(null);
    try {
      const value = await invoke<ExtensionSnapshot | null>(
        "copilot_extensions_action",
        { request: { schemaVersion: 1, action } },
      );
      if (!keepOpen) {
        setSnapshot(null);
        return true;
      }
      if (!value) {
        setSnapshot(null);
        setProblem({ said: "The SDK workspace is no longer connected." });
        return false;
      }
      setSnapshot(value);
      setMemoryEnabled(value.memoryEnabled);
      const agents =
        value.agents.state === "available" ? value.agents.data : [];
      if (!taskAgent && agents[0]) setTaskAgent(agents[0].id);
      return true;
    } catch (error) {
      setProblem(asProblem(error));
      // Some failed operations retire the owned SDK client. Don't leave a
      // disconnected workspace showing live controls.
      if (snapshot) {
        try {
          setSnapshot(
            await invoke<ExtensionSnapshot | null>("copilot_extensions_status"),
          );
        } catch (statusError) {
          setSnapshot(null);
          setProblem({
            said: asProblem(error).said,
            detail: `The workspace status could not be refreshed: ${asProblem(statusError).said}`,
          });
        }
      }
      return false;
    } finally {
      setBusy(false);
    }
  }

  function connect() {
    void run({ kind: "connect", cwd, memoryEnabled });
  }

  const agents =
    snapshot?.agents.state === "available" ? snapshot.agents.data : [];
  const tasks =
    snapshot?.tasks.state === "available" ? snapshot.tasks.data : [];
  const plugins =
    snapshot?.plugins.state === "available" ? snapshot.plugins.data : [];
  const remoteSessions =
    snapshot?.remoteSessions?.state === "available"
      ? snapshot.remoteSessions.data
      : [];
  const connected = snapshot !== null;

  return (
    <section
      className="copilot-extensions"
      aria-labelledby="copilot-extensions-title"
    >
      <div className="copilot-extensions-header">
        <div>
          <h2 id="copilot-extensions-title">Background Copilot workspace</h2>
          <p className="hint">
            Optional SDK features run separately from this conversation and use
            the signed-in Copilot CLI.
          </p>
        </div>
        <span className="copilot-runtime-state">
          {connected ? "Connected" : "Not connected"}
        </span>
      </div>
      {!connected ? (
        <div className="copilot-extensions-connect">
          <label className="copilot-checkbox-row">
            <input
              type="checkbox"
              data-ui-action="sdk_memory"
              checked={memoryEnabled}
              onChange={(event) => setMemoryEnabled(event.target.checked)}
              disabled={busy}
            />
            <span>
              Enable Copilot runtime memory for this background workspace
              <small>Off by default. This is not Darbot lavamem.</small>
            </span>
          </label>
          <button
            type="button"
            data-ui-action="sdk_connect"
            onClick={connect}
            disabled={busy}
          >
            {busy ? "Connecting..." : "Connect SDK workspace"}
          </button>
        </div>
      ) : (
        <>
          <div className="copilot-extensions-toolbar">
            <span title={snapshot.cwd}>
              {snapshot.runtimeVersion} / protocol {snapshot.protocolVersion}
            </span>
            <button
              type="button"
              className="quiet"
              data-ui-action="sdk_refresh"
              onClick={() =>
                void run({
                  kind: "refresh",
                  connectionId: snapshot.connectionId,
                })
              }
              disabled={busy}
            >
              Refresh
            </button>
            <button
              type="button"
              className="quiet"
              data-ui-action="sdk_list_remote"
              onClick={() =>
                void run({
                  kind: "list-remote",
                  connectionId: snapshot.connectionId,
                })
              }
              disabled={busy}
            >
              Discover remote sessions
            </button>
            <button
              type="button"
              className="quiet"
              data-ui-action="sdk_disconnect"
              onClick={() =>
                void run(
                  { kind: "disconnect", connectionId: snapshot.connectionId },
                  false,
                )
              }
              disabled={busy}
            >
              Disconnect
            </button>
          </div>
          <div className="copilot-extension-grid">
            <div>
              <h3>Agents and tasks</h3>
              {unavailable(snapshot.agents) ? (
                <p className="caution">{unavailable(snapshot.agents)}</p>
              ) : (
                <>
                  <div className="copilot-extension-form">
                    <select
                      data-ui-action="sdk_select_task_agent"
                      value={taskAgent}
                      onChange={(event) => setTaskAgent(event.target.value)}
                      disabled={busy || mismatchedFolder || agents.length === 0}
                      aria-label="Background agent"
                    >
                      <option value="">Choose an agent</option>
                      {agents.map((agent) => (
                        <option key={agent.id} value={agent.id}>
                          {agent.name}
                        </option>
                      ))}
                    </select>
                    <input
                      data-ui-action="sdk_set_task_name"
                      value={taskName}
                      disabled={busy}
                      onChange={(event) => setTaskName(event.target.value)}
                      placeholder="Task name"
                      maxLength={128}
                    />
                    <textarea
                      data-ui-action="sdk_set_task_prompt"
                      value={taskPrompt}
                      disabled={busy}
                      onChange={(event) => setTaskPrompt(event.target.value)}
                      placeholder="Task prompt"
                      maxLength={16000}
                      rows={2}
                    />
                    <button
                      type="button"
                      data-ui-action="sdk_start_task"
                      onClick={async () => {
                        const started = await run({
                          kind: "start-task",
                          connectionId: snapshot.connectionId,
                          agentId: taskAgent,
                          name: taskName,
                          prompt: taskPrompt,
                        });
                        if (started) {
                          setTaskName("");
                          setTaskPrompt("");
                        }
                      }}
                      disabled={
                        busy ||
                        mismatchedFolder ||
                        !taskAgent ||
                        !taskName.trim() ||
                        !taskPrompt.trim()
                      }
                    >
                      Start background task
                    </button>
                  </div>
                  {tasks.length === 0 ? (
                    <p className="hint">No background tasks.</p>
                  ) : (
                    <ul className="copilot-extension-list">
                      {tasks.map((task) => (
                        <li key={task.id}>
                          <span>
                            <strong>{task.title}</strong>
                            <small>{task.status}</small>
                          </span>
                          <button
                            type="button"
                            className="quiet"
                            data-ui-action="sdk_cancel_task"
                            onClick={() =>
                              void run({
                                kind: "cancel-task",
                                connectionId: snapshot.connectionId,
                                taskId: task.id,
                              })
                            }
                            disabled={
                              busy ||
                              ["completed", "failed", "cancelled"].includes(
                                task.status,
                              )
                            }
                          >
                            Cancel
                          </button>
                        </li>
                      ))}
                    </ul>
                  )}
                </>
              )}
            </div>
            <div>
              <h3>Plugins</h3>
              <p className="hint">
                This CLI profile only. Enabled means configured for new
                sessions; inventory does not run plugins or verify their service
                sign-in.
              </p>
              {unavailable(snapshot.plugins) ? (
                <p className="caution">{unavailable(snapshot.plugins)}</p>
              ) : plugins.length === 0 ? (
                <p className="hint">
                  The CLI reported no installed plugins in this profile.
                </p>
              ) : (
                <ul className="copilot-extension-list">
                  {plugins.map((plugin) => (
                    <li key={plugin.id}>
                      <span>
                        <strong>{plugin.name}</strong>
                        <small>
                          {plugin.origin} ·{" "}
                          {plugin.enabled ? "Enabled" : "Disabled"}
                          {plugin.version ? ` · ${plugin.version}` : ""}
                          {!plugin.canToggle
                            ? " · Not toggleable through this SDK"
                            : ""}
                        </small>
                      </span>
                      <button
                        type="button"
                        className="quiet"
                        data-ui-action="sdk_toggle_plugin"
                        onClick={() =>
                          void run({
                            kind: "set-plugin-enabled",
                            connectionId: snapshot.connectionId,
                            pluginId: plugin.id,
                            enabled: !plugin.enabled,
                          })
                        }
                        disabled={busy || mismatchedFolder || !plugin.canToggle}
                      >
                        {plugin.enabled ? "Disable" : "Enable"}
                      </button>
                    </li>
                  ))}
                </ul>
              )}
            </div>
          </div>
          {remoteSessions.length > 0 && (
            <details className="copilot-runtime-warnings" open>
              <summary data-ui-action="inspect_details">
                Remote sessions discovered
              </summary>
              <ul>
                {remoteSessions.map((remote) => (
                  <li key={remote.id}>
                    {remote.title} · {remote.state ?? "unknown"}
                  </li>
                ))}
              </ul>
            </details>
          )}
        </>
      )}
      {(problem || mismatchedFolder) && (
        <InlineFailure
          problem={
            problem ?? {
              said: "The background workspace belongs to a different folder. Disconnect it and connect the selected folder before starting new work.",
            }
          }
        />
      )}
    </section>
  );
}
