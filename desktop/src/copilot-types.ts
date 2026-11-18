/**
 * Shared TypeScript types for the Copilot ACP contracts exposed by the Tauri backend.
 *
 * One definition per shape, imported by both ProviderPicker (the pre-connection readiness check)
 * and CopilotWorkspace (the live agent workspace), so the two surfaces that talk to
 * `copilot_status` / `copilot_inventory` never drift into two different ideas of what those calls
 * return. `CopilotStatus` mirrors the shape already relied on before this change; the inventory
 * item shapes below replace the previous `unknown[]` placeholders with the fields the backend
 * actually sends.
 */

import type { SessionSummary } from "./history-sources";

export type CopilotCapabilities = {
  loadSession: boolean;
  resumeSession: boolean;
  listSessions: boolean;
  closeSession: boolean;
  deleteSession: boolean;
  promptImage: boolean;
  promptEmbeddedContext: boolean;
  mcpHttp: boolean;
  mcpSse: boolean;
};

export type CopilotStatus = {
  version: string;
  protocolVersion: number;
  authentication: "ready" | "required" | "unknown";
  capabilities: CopilotCapabilities;
  sessionCount: number;
  hasMoreSessions: boolean;
  warnings: string[];
};

export type CopilotAgentSummary = {
  id: string;
  name: string;
  description?: string | null;
  model?: string | null;
  definition?: {
    file: string;
    modifiedAtUnix?: number | null;
    declaredId: string | null;
    tools: { name: string; field: string; server: string | null }[];
    omittedEntries: number;
  };
  source?: StudioAgentSource | null;
};

export type StudioAgentSource = {
  provider: "copilot-studio";
  tenantId: string;
  environmentId: string;
  environmentName: string;
  environmentUrl: string;
  botId: string;
  schemaName: string;
  declaredDescription?: string | null;
  modifiedAt: string;
  stateCode: number;
  lastSyncedAt: number;
};

export type StudioSourceStatus = {
  tenantId: string;
  environmentId: string;
  environmentName: string;
  environmentUrl: string;
  state: "ready" | "stale" | "unavailable";
  agentCount: number;
  assets?: {
    kind: "workflow" | "canvas-app" | "model-driven-app";
    id: string;
    name: string;
  }[];
  assetWarnings?: string[];
  lastSyncedAt: number | null;
  error: string | null;
};

const GUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

export function isStudioAgentId(id: string): boolean {
  return id.toLowerCase().startsWith("studio:");
}

export function studioAgentId(
  source: Pick<StudioAgentSource, "tenantId" | "environmentId" | "botId">,
): string {
  const parts = [source.tenantId, source.environmentId, source.botId];
  if (!parts.every((part) => GUID.test(part))) {
    throw new Error("Copilot Studio agent identity contains an invalid GUID.");
  }
  return `studio:${parts.join(":")}`;
}

export function studioAgentUrl(agent: CopilotAgentSummary): string {
  const source = agent.source;
  if (
    source?.provider !== "copilot-studio" ||
    studioAgentId(source) !== agent.id
  ) {
    throw new Error("Copilot Studio agent identity does not match its source.");
  }
  return `https://copilotstudio.microsoft.com/environments/${source.environmentId}/bots/${source.botId}/overview?tenantId=${source.tenantId}`;
}

export function agentSourceLabel(agent: CopilotAgentSummary): string {
  return agent.source
    ? `Copilot Studio / ${agent.source.environmentName} / ${agent.source.tenantId}`
    : isStudioAgentId(agent.id)
      ? "Copilot Studio / source unavailable"
      : "Copilot CLI / local";
}

export function requireLocalAgent(id: string): void {
  if (isStudioAgentId(id)) {
    throw new Error(
      "Copilot Studio agents must be opened in their source environment, not the local Copilot CLI.",
    );
  }
}

export type CopilotAgentCatalog = {
  agents: CopilotAgentSummary[];
  warnings: string[];
  sources?: StudioSourceStatus[];
};

export type CopilotSkill = {
  name: string;
  description?: string | null;
  enabled: boolean;
  source?: string | null;
};

export type CopilotPlugin = {
  name: string;
  marketplace?: string | null;
  version?: string | null;
  enabled: boolean;
  source?: string | null;
  /** Agent plugin 1.0 fields. Optional so an older inventory still parses. */
  protocol?: string | null;
  manifestUrl?: string | null;
  capabilities?: string[] | null;
};

export type CopilotMcpServer = {
  name: string;
  enabled: boolean;
  source?: string | null;
  transport?: string | null;
  toolCount: number;
};

export type CopilotInventory = {
  agents: CopilotAgentSummary[];
  skills: CopilotSkill[];
  plugins: CopilotPlugin[];
  mcpServers: CopilotMcpServer[];
  warnings: string[];
  sources?: StudioSourceStatus[];
};

/** What `copilot_workspace` and `copilot_pick_directory` both resolve to. */
export type CopilotWorkspaceLocation = {
  cwd: string;
  home: string;
};

export type ConfigOptionValue = {
  value: string;
  name: string;
  description?: string | null;
  _meta?: { copilotEnablement?: string | null } | null;
};

export type ConfigOption = {
  id: string;
  name?: string | null;
  currentValue: string;
  options: ConfigOptionValue[];
};

/**
 * A value counts as disabled only when the runtime explicitly said so. Absence of `_meta` or of
 * `copilotEnablement` is not a disabled signal — only a real, non-"enabled" string is.
 */
export function isConfigValueDisabled(value: ConfigOptionValue): boolean {
  const state = value._meta?.copilotEnablement;
  return typeof state === "string" && state !== "enabled";
}

export function findConfigOption(
  options: ConfigOption[] | null | undefined,
  id: string,
): ConfigOption | null {
  return options?.find((option) => option.id === id) ?? null;
}

export type CopilotSession = {
  sessionId: string;
  cwd: string;
  modes?: unknown;
  configOptions?: ConfigOption[];
};

export type CopilotSessionSummary = {
  sessionId: string;
  cwd: string;
  title?: string | null;
  updatedAt?: string | null;
};

export type CopilotSessionsPage = {
  sessions: CopilotSessionSummary[];
  nextCursor?: string | null;
};

export type CopilotHistorySession = CopilotSessionSummary & {
  agentId: string;
  agentName: string;
  provider?: "copilot-cli";
};

export type CopilotWorkspaceChat = Omit<
  CopilotHistorySession,
  "sessionId" | "title"
> & {
  conversationId: string;
  sessionId: string | null;
  title: string;
  draft: string;
};

export type CopilotHistory = {
  sessions: CopilotHistorySession[];
  agents: { id: string; name: string; conversationCount: number }[];
  warnings: string[];
  timings?: {
    connectionMs: number;
    listingMs: number;
    indexingMs: number;
    firstPageMs: number | null;
    totalMs: number;
  };
};

export type CopilotHistoryProgress = {
  requestId: string | null;
  phase: "listing" | "indexing";
  loaded: number;
  sessions?: CopilotHistorySession[];
};

/** Non-repository cards deliberately do not implement urn:repo-toolbox:agentcard:v1. */
export const FLEET_SCHEMA = "urn:darbot:fleet-agentcard:v1";
export const FLEET_TABS = [
  "identity",
  "overview",
  "source",
  "model",
  "tools",
  "relationships",
  "tables",
  "environment",
  "sessions",
  "runtime",
  "facets",
  "freshness",
  "limitations",
] as const;
export type FleetTab = (typeof FLEET_TABS)[number];

const text = { type: "string" };
const maybeText = { type: ["string", "null"] };
const maybeNumber = { type: ["number", "null"] };
const list = (items: object) => ({ type: "array", items });
const fields = (properties: Record<string, object>) => ({
  type: "object",
  properties,
  additionalProperties: false,
  required: Object.keys(properties),
});
const nullable = (schema: object) => ({ anyOf: [schema, { type: "null" }] });
const tabSchemas = {
  identity: fields({
    sourceKey: text,
    agentId: text,
    declaredId: maybeText,
    displayName: text,
  }),
  overview: fields({ name: text, description: maybeText, evidence: text }),
  source: fields({
    provider: text,
    definitionFile: maybeText,
    studio: nullable(
      fields({
        tenantId: text,
        environmentId: text,
        environmentName: text,
        botId: text,
        schemaName: text,
        stateCode: { type: "number" },
      }),
    ),
  }),
  model: fields({ declared: maybeText, status: text }),
  tools: fields({
    status: text,
    declarations: nullable(
      list(fields({ name: text, field: text, server: maybeText })),
    ),
    withheldCount: maybeNumber,
  }),
  relationships: fields({
    edges: list(
      fields({
        relation: text,
        targetType: text,
        source: text,
        target: text,
        evidence: text,
      }),
    ),
  }),
  tables: fields({
    catalogRows: list(
      fields({
        entitySet: text,
        primaryKey: text,
        rowId: text,
        identity: text,
      }),
    ),
    applicationTableUsage: text,
  }),
  environment: fields({
    scope: text,
    tenantId: maybeText,
    environmentId: maybeText,
    status: text,
    sourceState: maybeText,
    warningCount: { type: "integer" },
    assets: nullable(
      list(
        fields({
          kind: { enum: ["workflow", "canvas-app", "model-driven-app"] },
          id: text,
          name: text,
        }),
      ),
    ),
  }),
  sessions: fields({
    distinctSessions: nullable(
      fields({
        known: maybeNumber,
        acp: maybeNumber,
        archiveOnly: maybeNumber,
        coverage: text,
      }),
    ),
    indexStatus: text,
    indexScope: maybeText,
    indexedCount: maybeNumber,
    linkedCount: maybeNumber,
    drafts: maybeNumber,
    observedSessionIds: list(text),
    indexed: list(fields({ sessionId: text, cwd: text, updatedAt: maybeText })),
  }),
  runtime: fields({
    provider: maybeText,
    activeSessionId: maybeText,
    observedModel: maybeText,
    terminal: maybeText,
  }),
  facets: fields({
    rubric: text,
    entries: list(
      fields({ facet: text, weight: { enum: [1, 2, 3] }, basis: text }),
    ),
  }),
  freshness: fields({
    modifiedAt: maybeText,
    definitionModifiedAtUnix: maybeNumber,
    lastSyncedAt: maybeNumber,
    sourceState: maybeText,
  }),
  limitations: fields({ notes: list(text) }),
} satisfies Record<FleetTab, object>;

export const fleetCardSchema = {
  $schema: "http://json-schema.org/draft-07/schema#",
  $id: FLEET_SCHEMA,
  type: "object",
  additionalProperties: false,
  required: [
    "schema",
    "version",
    "kind",
    "sourceKey",
    "agentcard_ID",
    "name",
    "tabs",
    "limitations",
  ],
  properties: {
    schema: { const: FLEET_SCHEMA },
    version: { const: "1.0" },
    kind: { const: "AgentCard" },
    sourceKey: { type: "string", minLength: 1 },
    agentcard_ID: { type: "string", pattern: "^#[0-9A-F]{6}$" },
    name: { type: "string" },
    tabs: {
      type: "object",
      additionalProperties: false,
      required: FLEET_TABS,
      properties: tabSchemas,
    },
    limitations: { type: "array", items: { type: "string" } },
  },
} as const;

export function fleetIdentity(agent: CopilotAgentSummary) {
  if (isStudioAgentId(agent.id)) return agent.id;
  return agent.definition
    ? `copilot-cli:definition:${encodeURIComponent(agent.definition.file)}`
    : `copilot-cli:runtime:${encodeURIComponent(agent.id)}`;
}

export function fleetDisplayId(identity: string) {
  let hash = 2166136261;
  for (let i = 0; i < identity.length; i++)
    hash = Math.imul(hash ^ identity.charCodeAt(i), 16777619);
  return `#${(hash & 0xffffff).toString(16).toUpperCase().padStart(6, "0")}`;
}

type FleetEdge = {
  relation:
    | "local-counterpart"
    | "member-of"
    | "represented-by"
    | "linked-session";
  targetType: "agent" | "environment" | "dataverse-row" | "local-session";
  source: string;
  target: string;
  evidence: string;
};

// Old catalogs and live ACP metadata may not have passed the native projection.
const sensitiveMetadata =
  /:\/\/|bearer |basic |token|secret|password|passwd|credential|api[ _-]?key|private key|gh[pousr]_|github_pat_|xox|a(?:kia|sia)[A-Z0-9]{16}|\bsk-|eyj|[a-z\d]{32}/i;
function publicSymbol(value?: string | null) {
  return value &&
    /^(?:\*|[a-z\d][\w./:*-]{0,127})$/i.test(value) &&
    !sensitiveMetadata.test(value)
    ? value
    : null;
}

export function buildFleetCard(
  agent: CopilotAgentSummary,
  fleet: readonly CopilotAgentSummary[],
  chats: readonly CopilotWorkspaceChat[] | null,
  history: CopilotHistory | null,
  historyScope: string,
  historyComplete: boolean,
  sourceStatuses: readonly StudioSourceStatus[],
  active?: { agentId: string; sessionId: string; model?: string | null },
  distinctSessions: SessionSummary | null = null,
) {
  const identity = fleetIdentity(agent);
  const cloud = isStudioAgentId(agent.id);
  const rawSource = agent.source;
  const source =
    rawSource &&
    [rawSource.tenantId, rawSource.environmentId, rawSource.botId].every((v) =>
      GUID.test(v),
    ) &&
    studioAgentId(rawSource) === agent.id
      ? rawSource
      : null;
  const definition = !cloud ? agent.definition : undefined;
  const scopedSource = source;
  const coverage =
    scopedSource &&
    sourceStatuses.find(
      (s) =>
        s.tenantId === scopedSource.tenantId &&
        s.environmentId === scopedSource.environmentId,
    );
  const edges: FleetEdge[] = [];
  const add = (
    relation: FleetEdge["relation"],
    targetType: FleetEdge["targetType"],
    target: string,
    evidence: string,
  ) => edges.push({ relation, targetType, source: identity, target, evidence });
  const tables = [];
  if (source) {
    const environment = `studio:${source.tenantId}:${source.environmentId}`;
    const row = `${environment}:bots:${source.botId}`;
    add(
      "member-of",
      "environment",
      environment,
      "Cached tenantId and environmentId",
    );
    add(
      "represented-by",
      "dataverse-row",
      row,
      "Dataverse bots.botid in the scoped catalog",
    );
    tables.push({
      entitySet: "bots",
      primaryKey: "botid",
      rowId: source.botId,
      identity: row,
    });
  }
  const uniqueLocal =
    !cloud &&
    agent.id !== "__unavailable__" &&
    fleet.filter((a) => a.id === agent.id).length === 1;
  const localId = definition?.declaredId ?? agent.id;
  const match = /^(dayour|darbot)-(.+)$/.exec(localId);
  if (uniqueLocal && definition && match) {
    const otherId = `${match[1] === "dayour" ? "darbot" : "dayour"}-${match[2]}`;
    const byId = (id: string) =>
      fleet.filter(
        (a) =>
          a.definition &&
          !isStudioAgentId(a.id) &&
          (a.definition.declaredId ?? a.id) === id,
      );
    const matches = byId(otherId);
    if (matches.length === 1 && byId(localId).length === 1)
      add(
        "local-counterpart",
        "agent",
        fleetIdentity(matches[0]),
        `Independent local definitions with observed IDs ${localId} and ${otherId}; no shared implementation implied.`,
      );
  }
  const linked = uniqueLocal
    ? (chats?.filter((c) => c.agentId === agent.id) ?? [])
    : [];
  const indexed = uniqueLocal
    ? (history?.sessions.filter((s) => s.agentId === agent.id) ?? [])
    : [];
  const key = (s: { cwd: string; sessionId: string | null }) =>
    JSON.stringify([agent.id, s.cwd, s.sessionId]);
  const linkedIds = new Set(
    linked.flatMap((c) => (c.sessionId ? [key(c)] : [])),
  );
  const indexRows = new Map(
    indexed.map((s) => [
      key(s),
      { sessionId: s.sessionId, cwd: s.cwd, updatedAt: s.updatedAt ?? null },
    ]),
  );
  const refs = new Set([...linkedIds, ...indexRows.keys()]);
  for (const id of refs)
    add(
      "linked-session",
      "local-session",
      id,
      [
        linkedIds.has(id) && "Workspace link",
        indexRows.has(id) && "CLI history index",
      ]
        .filter(Boolean)
        .join(" + "),
    );
  const runtime = uniqueLocal && active?.agentId === agent.id ? active : null;
  const prose = cloud ? scopedSource?.declaredDescription : agent.description;
  const description =
    prose &&
    prose.length <= 1000 &&
    !/[^\p{L}\p{N} .,;:!?()'’"/–—&-]/u.test(prose) &&
    !sensitiveMetadata.test(prose)
      ? prose.trim() || null
      : null;
  const evidence = cloud
    ? "Dataverse bots row"
    : definition
      ? "Local definition frontmatter"
      : "Local agent listing";
  const entries: { facet: string; weight: 1 | 2 | 3; basis: string }[] = [];
  const configOrigin = definition?.file ?? scopedSource?.schemaName;
  if (configOrigin !== undefined)
    entries.push({
      facet: "Configuration origin",
      weight: 3,
      basis: configOrigin,
    });
  const declarations =
    definition?.tools
      .filter(
        (t) =>
          publicSymbol(t.name) &&
          (!t.server || publicSymbol(t.server)) &&
          /^(?:mcps|mcp-servers|mcpServers|tools(?:\.(?:mcp\.)?(?:required|optional))?)$/.test(
            t.field,
          ),
      )
      .map(({ name, server, field }) => ({ name, server, field })) ?? null;
  const declaredModel = cloud ? null : publicSymbol(agent.model);
  if (description)
    entries.push({ facet: "Declared purpose", weight: 2, basis: description });
  if (declarations?.length)
    entries.push({
      facet: "Declared integrations",
      weight: 2,
      basis: [
        ...new Set(
          declarations.map((t) =>
            t.server ? `${t.server}/${t.name}` : t.name,
          ),
        ),
      ].join(", "),
    });
  if (declaredModel)
    entries.push({ facet: "Declared model", weight: 2, basis: declaredModel });
  if (refs.size || runtime)
    entries.push({
      facet: "Runtime context",
      weight: 1,
      basis: runtime ? "Active ACP session" : "Agent-linked session reference",
    });
  const limitations = [
    "No transcripts, prompts, commands, credentials or runtime tool configuration.",
    "Tool declarations do not establish permissions or actual use. No application data-table usage inferred.",
    ...(cloud
      ? [
          "Studio catalog only: models, capabilities and cloud sessions are not collected. Environment assets are not agent dependencies.",
        ]
      : []),
    ...(!cloud && !definition
      ? ["No local definition metadata available."]
      : []),
    ...(definition?.omittedEntries
      ? ["Non-symbolic tool declarations withheld."]
      : []),
    ...(!cloud && !uniqueLocal
      ? [
          "Ambiguous local runtime ID; sessions and counterparts not attributed.",
        ]
      : []),
  ];
  const status =
    !uniqueLocal || !history
      ? "not-collected"
      : historyComplete && !history.warnings.length
        ? "collected"
        : "partial";
  const tabs = {
    identity: {
      sourceKey: identity,
      agentId: agent.id,
      declaredId: definition?.declaredId ?? null,
      displayName: agent.name,
    },
    overview: {
      name: agent.name,
      description,
      evidence: description ? evidence : "Not reported or withheld",
    },
    source: {
      provider: cloud ? "copilot-studio" : "copilot-cli",
      definitionFile: definition?.file ?? null,
      studio: source
        ? {
            tenantId: source.tenantId,
            environmentId: source.environmentId,
            environmentName: source.environmentName,
            botId: source.botId,
            schemaName: source.schemaName,
            stateCode: source.stateCode,
          }
        : null,
    },
    model: {
      declared: declaredModel,
      status: cloud
        ? "not-collected"
        : declaredModel
          ? "declared"
          : "not-reported",
    },
    tools: {
      status: definition ? "inspected" : "not-collected",
      declarations,
      withheldCount:
        definition && declarations
          ? definition.omittedEntries +
            definition.tools.length -
            declarations.length
          : null,
    },
    relationships: { edges },
    tables: { catalogRows: tables, applicationTableUsage: "not-collected" },
    environment: {
      scope: "ENVIRONMENT inventory — not confirmed agent dependencies",
      tenantId: scopedSource?.tenantId ?? null,
      environmentId: scopedSource?.environmentId ?? null,
      sourceState: coverage?.state ?? null,
      warningCount: coverage?.assetWarnings?.length ?? 0,
      status: !cloud
        ? "not-applicable"
        : !coverage?.assets
          ? "not-collected"
          : coverage.assetWarnings?.length
            ? "partial"
            : "collected",
      assets:
        coverage?.assets?.map(({ kind, id, name }) => ({ kind, id, name })) ??
        null,
    },
    sessions: {
      distinctSessions,
      indexStatus: status,
      indexScope: uniqueLocal && history ? historyScope : null,
      indexedCount:
        uniqueLocal && history
          ? new Set(indexed.map((s) => s.sessionId)).size ||
            (status === "collected" ? 0 : null)
          : null,
      linkedCount:
        uniqueLocal && chats
          ? new Set(linked.flatMap((c) => (c.sessionId ? [c.sessionId] : [])))
              .size
          : null,
      drafts:
        uniqueLocal && chats ? linked.filter((c) => !c.sessionId).length : null,
      observedSessionIds: [
        ...new Set([
          ...linked.flatMap((c) => (c.sessionId ? [c.sessionId] : [])),
          ...indexed.map((s) => s.sessionId),
        ]),
      ],
      indexed: [...indexRows.values()],
    },
    runtime: {
      provider: runtime || indexed.length ? "copilot-cli" : null,
      activeSessionId: runtime?.sessionId ?? null,
      observedModel: publicSymbol(runtime?.model),
      terminal: null,
    },
    facets: {
      rubric:
        "Provenance weights, once per supported facet: 3 = scoped configuration identity; 2 = explicit role, integration or model declaration; 1 = observed session context. No evidence, no facet. Not a performance score.",
      entries,
    },
    freshness: {
      modifiedAt: scopedSource?.modifiedAt ?? null,
      definitionModifiedAtUnix: definition?.modifiedAtUnix ?? null,
      lastSyncedAt: scopedSource?.lastSyncedAt ?? null,
      sourceState: coverage?.state ?? null,
    },
    limitations: { notes: limitations },
  } satisfies Record<FleetTab, object>;
  return {
    schema: FLEET_SCHEMA,
    version: "1.0",
    kind: "AgentCard",
    sourceKey: identity,
    agentcard_ID: fleetDisplayId(identity),
    name: agent.name,
    tabs,
    limitations,
  };
}

export type CopilotPermissionOption = {
  optionId: string;
  name: string;
  kind: string;
};

export type CopilotPermissionRequest = {
  requestId: string;
  sessionId: string;
  origin?: "conversation" | "background";
  toolCall: unknown;
  options: CopilotPermissionOption[];
};

export type CopilotSessionUpdate = {
  sessionId: string;
  update: Record<string, unknown>;
};

export type CopilotAvailableCommand = {
  name: string;
  description?: string | null;
  input?: { hint?: string | null } | null;
};

export type CopilotPromptResult = {
  stopReason: string;
};

export type CopilotToolActivity = {
  title: string;
  status?: string;
  input?: unknown;
  output?: unknown;
};

export function mergeToolActivity(
  previous: CopilotToolActivity | undefined,
  update: Partial<CopilotToolActivity>,
): CopilotToolActivity {
  return {
    title: update.title?.trim() || previous?.title || "Tool call",
    status: update.status ?? previous?.status,
    input: update.input ?? previous?.input,
    output: update.output ?? previous?.output,
  };
}
