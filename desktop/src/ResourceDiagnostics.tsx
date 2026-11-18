import { useId } from "react";
import {
  isStudioAgentId,
  studioAgentId,
  type CopilotAgentSummary,
  type CopilotInventory,
  type CopilotStatus,
  type StudioSourceStatus,
} from "./copilot-types";

const operations = {
  identity: "GET WhoAmI",
  "verify-account": "GET systemusers · verify configured account",
  bots: "GET bots",
  workflows: "GET workflows",
  "canvas-apps": "GET canvasapps",
  "model-driven-apps": "GET appmodules",
  "acp-initialize": "ACP initialize",
  "acp-list-sessions": "ACP session/list",
  "list-plugins": "CLI plugin list",
  "list-skills": "CLI skill list",
  "list-mcp": "CLI mcp list",
  "sdk-start": "SDK startup negotiation",
  "sdk-status": "SDK getStatus",
  "sdk-verify-auth": "SDK getAuthStatus · verify signed-in user",
  "sdk-plugins": "SDK plugins.list",
  "sdk-agents": "SDK agent.list",
  "sdk-tasks": "SDK tasks.list",
  "sdk-remote": "SDK sessions.list · remote",
  "fallback-plugins": "CLI plugin list · SDK compatibility fallback",
} as const;
type Operation = keyof typeof operations;
type Call = {
  atUnixMs: number;
  latencyMs: number;
  succeeded: boolean;
  httpStatus?: number | null;
};
type History = {
  lastCall: Call;
  lastSuccess?: Call | null;
  lastFailure?: Call | null;
};
type Observations = Partial<Record<Operation, History>>;

/** Additive native payload on CopilotInventory, CopilotStatus and each StudioSourceStatus. */
export type WithResourceObservations = { observations?: Observations };
export type ResourceDiagnosticsProps = {
  inventory: CopilotInventory | null;
  agent?: CopilotAgentSummary | null;
  status?: CopilotStatus | null;
};

function observations(
  row?: CopilotInventory | CopilotStatus | StudioSourceStatus | null,
): Observations {
  return (row as WithResourceObservations | null)?.observations ?? {};
}

function Timestamp({ ms }: { ms?: number }) {
  if (!ms || !Number.isSafeInteger(ms) || ms < 0 || ms > 8.64e15)
    return <>Time not recorded</>;
  const iso = new Date(ms).toISOString();
  return (
    <time dateTime={iso}>{iso.replace("T", " ").replace("Z", " UTC")}</time>
  );
}

function outcome(call: Call) {
  const code = call.httpStatus;
  const http =
    typeof code === "number" &&
    Number.isInteger(code) &&
    code >= 100 &&
    code <= 599;
  return `${call.succeeded ? "Succeeded" : "Failed"}${http ? ` · HTTP ${code}` : ""}`;
}

function Attempt({ call }: { call: Call }) {
  const ms = call.latencyMs;
  return (
    <>
      <strong>{outcome(call)}</strong>
      <br />
      <Timestamp ms={call.atUnixMs} />
      <br />
      {Number.isSafeInteger(ms) && ms >= 0
        ? `${ms === 0 ? "<1" : ms} ms`
        : "Latency not recorded"}
    </>
  );
}

function CallTable({
  title,
  ops,
  rows,
}: {
  title: string;
  ops: Operation[];
  rows: Observations;
}) {
  return (
    <table>
      <caption>{title}</caption>
      <thead>
        <tr>
          <th scope="col">Operation</th>
          <th scope="col">Last attempt · result, time, latency</th>
          <th scope="col">Last success for this operation</th>
        </tr>
      </thead>
      <tbody>
        {ops.map((op) => {
          const history = rows[op];
          const last = history?.lastCall;
          const success = history?.lastSuccess;
          return (
            <tr key={op}>
              <th scope="row">{operations[op]}</th>
              <td>
                {last ? <Attempt call={last} /> : "No attempt recorded"}
                {last?.succeeded && history?.lastFailure && (
                  <>
                    <br />
                    Prior failure: {outcome(history.lastFailure)} ·{" "}
                    <Timestamp ms={history.lastFailure.atUnixMs} />
                  </>
                )}
              </td>
              <td>
                {success?.succeeded ? (
                  <Timestamp ms={success.atUnixMs} />
                ) : (
                  "No success recorded"
                )}
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}

// Show public names, never arbitrary command output, source URLs, prose or configuration.
function label(value: string) {
  return typeof value === "string" &&
    /^[\p{L}\d*][\p{L}\d ._*/:-]{0,127}$/u.test(value) &&
    !/(?:\/\/|\.\.|^[a-z]:|bearer |basic |token|secret|password|credential|api[ _-]?key|gh[pousr]_|github_pat_|xox|akia|eyj|\bsk-|[a-z\d]{32})/i.test(
      value,
    )
    ? value
    : "Name withheld";
}

function uniqueRows<T>(items: readonly T[], identity: (item: T) => string) {
  const rows = new Map<string, T>();
  for (const item of items) {
    const key = identity(item);
    if (!rows.has(key)) rows.set(key, item);
  }
  return [...rows];
}

const assetKinds = [
  ["workflow", "workflows"],
  ["canvas-app", "canvas-apps"],
  ["model-driven-app", "model-driven-apps"],
] as const;
const legacyReasons = new Set([
  "HTTP 403",
  "HTTP 401",
  "HTTP 404",
  "HTTP 429 or 5xx",
  "unexpected HTTP status",
  "network failure",
  "collection limit exceeded",
  "invalid pagination",
  "invalid metadata",
]);

function StudioReads({
  source,
  agentOnly = false,
}: {
  source: StudioSourceStatus;
  agentOnly?: boolean;
}) {
  const rows = observations(source);
  const account = rows["verify-account"]?.lastCall;
  const state = { ready: "Ready", stale: "Stale", unavailable: "Unavailable" }[
    source.state
  ];
  // Old snapshots sometimes know the failure, but never when it happened or its latency.
  const code = /^Dataverse returned (HTTP (?:401|403|404|429 or 5xx))\./.exec(
    source.error ?? "",
  )?.[1];
  return (
    <section aria-label={`Dataverse catalog: ${label(source.environmentName)}`}>
      <h4>{label(source.environmentName)} · Dataverse</h4>
      <p>
        Catalog: {state ?? "Unknown"}. Last complete bot catalog snapshot:{" "}
        {source.lastSyncedAt != null ? (
          <Timestamp ms={source.lastSyncedAt * 1000} />
        ) : (
          "Not recorded"
        )}
        .
      </p>
      {code && (
        <p>
          Catalog reported {code}. The catalog warning does not identify a call
          time or operation.
        </p>
      )}
      <p>
        Environment authentication:{" "}
        {account ? (
          <>
            {account.succeeded
              ? "configured account matched"
              : "configured account not verified"}{" "}
            at the recorded account check (<Timestamp ms={account.atUnixMs} />)
          </>
        ) : (
          "no account check recorded"
        )}
        .
      </p>
      <p className="hint">
        These reads belong to the environment catalog, not to an agent. A
        successful page read does not establish a complete sync or access to
        other tables.
      </p>
      <CallTable
        title="Dataverse Web API · environment reads"
        rows={rows}
        ops={
          agentOnly
            ? ["bots"]
            : [
                "identity",
                "verify-account",
                "bots",
                ...assetKinds.map(([, op]) => op),
              ]
        }
      />
      {!agentOnly &&
        assetKinds.map(([kind, op]) => {
          const reason = source.assetWarnings
            ?.find((w) => w.startsWith(`${kind}: `))
            ?.slice(kind.length + 2);
          return reason && legacyReasons.has(reason) && !rows[op] ? (
            <p key={kind}>
              Cached {operations[op]} warning: {reason}. Call time, latency and
              last success not recorded.
            </p>
          ) : null;
        })}
    </section>
  );
}

function AgentResources({
  agent,
  inventory,
}: {
  agent: CopilotAgentSummary;
  inventory: CopilotInventory | null;
}) {
  if (agent.source || isStudioAgentId(agent.id)) {
    let source: StudioSourceStatus | undefined;
    if (agent.source?.provider === "copilot-studio") {
      const origin = agent.source;
      try {
        if (studioAgentId(origin) === agent.id) {
          source = inventory?.sources?.find(
            (s) =>
              s.tenantId === origin.tenantId &&
              s.environmentId === origin.environmentId &&
              s.environmentUrl === origin.environmentUrl,
          );
        }
      } catch {
        /* Older catalogs may lack a verifiable scoped identity. */
      }
    }
    return (
      <>
        <p>
          Studio catalog entry. This agent’s own connector calls, authentication
          and tool execution are not instrumented.
        </p>
        {source ? (
          <StudioReads source={source} agentOnly />
        ) : (
          <p>No matching environment catalog is available.</p>
        )}
      </>
    );
  }
  const definition = agent.definition;
  if (!definition)
    return (
      <p>
        No tool declaration available for this agent. Its resource calls are not
        instrumented.
      </p>
    );
  const rows = observations(inventory);
  const declarations = definition.tools.filter((t) =>
    /^(mcps|mcp-servers|mcpServers|tools(\.(mcp\.)?(required|optional))?)$/.test(
      t.field,
    ),
  );
  const serverName = (t: (typeof declarations)[number]) =>
    t.server ??
    (["mcps", "mcp-servers", "mcpServers"].includes(t.field) ? t.name : null);
  return (
    <>
      <p>
        Local agent definition. A declaration is not an installation, permission
        grant or successful call.
      </p>
      {declarations.length ? (
        <ul className="copilot-resource-list copilot-resource-list-plain">
          {uniqueRows(declarations, (tool) =>
            JSON.stringify([tool.field, tool.server, tool.name]),
          ).map(([key, tool]) => {
            const name = serverName(tool);
            const listed =
              name != null &&
              inventory?.mcpServers.some((s) => s.name === name);
            return (
              <li key={key}>
                <strong>
                  {tool.server && `${label(tool.server)} / `}
                  {label(tool.name)}
                </strong>
                {" · "}Declared in <code>{tool.field}</code>.{" "}
                {listed
                  ? "Server listed in this workspace’s CLI configuration."
                  : name && rows["list-mcp"]?.lastCall.succeeded
                    ? "Server not listed in this CLI inventory."
                    : "Installation not established."}{" "}
                Authentication unknown; execution not instrumented.
              </li>
            );
          })}
        </ul>
      ) : (
        <p>No public tool declarations listed.</p>
      )}
      {definition.omittedEntries > 0 && (
        <p>Some non-symbolic declarations were withheld.</p>
      )}
      {declarations.some((t) => serverName(t) != null) && rows["list-mcp"] && (
        <CallTable
          title="Workspace inventory read · not an agent call"
          ops={["list-mcp"]}
          rows={rows}
        />
      )}
    </>
  );
}

function WorkspaceResources({ inventory, status }: ResourceDiagnosticsProps) {
  const rows = { ...observations(inventory) };
  // Status and inventory are independent snapshots. Do not mask a newer failed status check.
  for (const op of ["acp-initialize", "acp-list-sessions"] as const) {
    const fromStatus = observations(status)[op];
    const lastInventoryCall = rows[op]?.lastCall;
    if (
      fromStatus &&
      (!lastInventoryCall ||
        fromStatus.lastCall.atUnixMs >= lastInventoryCall.atUnixMs)
    )
      rows[op] = fromStatus;
  }
  const sdkOps: Operation[] = [
    "sdk-start",
    "sdk-status",
    "sdk-verify-auth",
    "sdk-plugins",
    "sdk-agents",
    "sdk-tasks",
    "sdk-remote",
    "fallback-plugins",
  ];
  const sdkSeen = sdkOps.filter((op) => rows[op]);
  return (
    <>
      <h4>Copilot CLI and SDK</h4>
      <p>
        CLI authentication at the supplied ACP status check:{" "}
        <strong>{status?.authentication ?? "unknown"}</strong>. This does not
        establish plugin, Microsoft 365 or Studio access.
      </p>
      <CallTable
        title="ACP · profile status checks"
        ops={["acp-initialize", "acp-list-sessions"]}
        rows={rows}
      />
      <CallTable
        title="CLI · workspace inventory reads"
        ops={["list-plugins", "list-skills", "list-mcp"]}
        rows={rows}
      />
      {sdkSeen.length ? (
        <CallTable
          title="SDK background workspace · observed operations"
          ops={sdkSeen}
          rows={rows}
        />
      ) : (
        <p>No SDK operation recorded for this workspace.</p>
      )}
      <h4>Installed and configured resources</h4>
      <p className="hint">
        CLI inventory is not tool execution. Per-resource authentication, calls
        and last success are not instrumented; diagnostics did not exercise
        these resources.
      </p>
      {inventory ? (
        <ul className="copilot-resource-list copilot-resource-list-plain">
          {uniqueRows(inventory.plugins, (p) =>
            JSON.stringify(["plugin", p.name, p.marketplace, p.source]),
          ).map(([key, p]) => (
            <li key={key}>
              <strong>{label(p.name)}</strong> ·{" "}
              {p.source === "builtin" ? "Built-in plugin" : "Plugin installed"}{" "}
              · {p.enabled ? "enabled" : "disabled"}
            </li>
          ))}
          {uniqueRows(inventory.skills, (s) =>
            JSON.stringify(["skill", s.name, s.source]),
          ).map(([key, s]) => (
            <li key={key}>
              <strong>{label(s.name)}</strong> · Skill listed ·{" "}
              {s.enabled ? "enabled" : "disabled"}
            </li>
          ))}
          {uniqueRows(inventory.mcpServers, (s) =>
            JSON.stringify(["mcp", s.name, s.source]),
          ).map(([key, s]) => (
            <li key={key}>
              <strong>{label(s.name)}</strong> · MCP server configured;
              connection not verified
            </li>
          ))}
          {!inventory.plugins.length &&
            !inventory.skills.length &&
            !inventory.mcpServers.length && (
              <li>
                No resource entries in the supplied inventory. See list results
                above.
              </li>
            )}
        </ul>
      ) : (
        <p>Workspace inventory unavailable.</p>
      )}
      <h4>Copilot Studio</h4>
      {inventory?.sources?.length ? (
        inventory.sources.map((s) => (
          <StudioReads key={`${s.tenantId}:${s.environmentId}`} source={s} />
        ))
      ) : (
        <p>No Studio source in the supplied inventory.</p>
      )}
      <h4>Microsoft 365</h4>
      <ul className="copilot-resource-list copilot-resource-list-plain">
        {["Email", "Calendar", "Teams", "OneDrive"].map((service) => (
          <li key={service}>
            <strong>{service}</strong> · Not instrumented; no call or user
            access verified.
          </li>
        ))}
      </ul>
    </>
  );
}

/** Passive projection only: opening this panel never probes a service or executes a plugin. */
export function ResourceDiagnostics({
  inventory,
  agent,
  status,
}: ResourceDiagnosticsProps) {
  const heading = useId();
  return (
    <section className="resource-diagnostics" aria-labelledby={heading}>
      <h3 id={heading}>
        {agent
          ? `Resource diagnostics · ${label(agent.name)}`
          : "Resource diagnostics"}
      </h3>
      <p className="hint">
        Only recorded operations are shown as calls. Times are completion times;
        a logo, declaration or listing is not evidence of use.
      </p>
      {agent ? (
        <AgentResources agent={agent} inventory={inventory} />
      ) : (
        <WorkspaceResources inventory={inventory} status={status} />
      )}
    </section>
  );
}
