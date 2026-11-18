import {
  fleetIdentity,
  isStudioAgentId,
  studioAgentId,
  type CopilotAgentSummary,
  type CopilotWorkspaceChat,
} from "./copilot-types";

export type HistorySource = {
  id: string;
  label: string;
  indexed: number;
  duplicates: number;
  unreadable: number;
  unattributed: number;
  complete: boolean;
  status: string;
};
export type ArchiveSession = {
  id: string;
  provider: string;
  nativeId: string;
  agentIds: string[];
  sourceIds: string[];
  updatedAt: string | null;
};
export type HistoryIndex = {
  schemaVersion: 1;
  scannedAt: string;
  sources: HistorySource[];
  sessions: ArchiveSession[];
};
export type ArchiveState = {
  index: HistoryIndex | null;
  loading: boolean;
  failed: boolean;
};
export type Coverage = "complete" | "partial" | "uncollected";
export type SessionSummary = {
  known: number | null;
  acp: number | null;
  archiveOnly: number | null;
  coverage: Coverage;
};
export type AgentHistory = {
  summary: SessionSummary;
  live: CopilotWorkspaceChat[];
  archiveOnly: ArchiveSession[];
  bySource: Map<string, number>;
};

const record = (v: unknown): v is Record<string, unknown> =>
  !!v && typeof v === "object" && !Array.isArray(v);
const text = (v: unknown): v is string => typeof v === "string" && v.length > 0;
const date = (v: unknown): v is string =>
  text(v) && Number.isFinite(Date.parse(v));
const strings = (v: unknown): v is string[] =>
  Array.isArray(v) && v.every((x) => typeof x === "string");
const count = (v: unknown) =>
  typeof v === "number" && Number.isSafeInteger(v) && v >= 0;
const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
export const acpKey = (id: string) =>
  `copilot-cli:${uuid.test(id) ? id.toLowerCase() : id}`;

export function readHistoryIndex(value: unknown): HistoryIndex {
  const invalid = () => {
    throw new Error("Unsupported or incomplete history index.");
  };
  if (
    !record(value) ||
    value.schemaVersion !== 1 ||
    !date(value.scannedAt) ||
    !Array.isArray(value.sources) ||
    !Array.isArray(value.sessions)
  )
    throw new Error("Unsupported or incomplete history index.");
  const sourceIds = new Set<string>();
  const sources = value.sources.map((s: unknown) => {
    if (
      !record(s) ||
      !text(s.id) ||
      sourceIds.has(s.id) ||
      !text(s.label) ||
      typeof s.complete !== "boolean" ||
      !text(s.status) ||
      ![s.indexed, s.duplicates, s.unreadable, s.unattributed].every(count)
    )
      return invalid();
    sourceIds.add(s.id);
    return s as HistorySource;
  });
  const ids = new Set<string>();
  const sessions = value.sessions.map((s: unknown) => {
    if (
      !record(s) ||
      !text(s.id) ||
      !text(s.provider) ||
      !text(s.nativeId) ||
      !s.id.startsWith(`${s.provider}:`) ||
      (s.provider === "copilot-cli" && s.id !== `copilot-cli:${s.nativeId}`) ||
      ids.has(s.id) ||
      !strings(s.agentIds) ||
      !strings(s.sourceIds) ||
      !s.sourceIds.length ||
      !s.sourceIds.every((id) => sourceIds.has(id)) ||
      !(s.updatedAt === null || date(s.updatedAt))
    )
      return invalid();
    ids.add(s.id);
    return s as ArchiveSession;
  });
  return { schemaVersion: 1, scannedAt: value.scannedAt, sources, sessions };
}

export function groupArchive(index: HistoryIndex | null) {
  const byAgent = new Map<string, ArchiveSession[]>();
  const byId = new Map<string, ArchiveSession>();
  for (const row of index?.sessions ?? []) {
    byId.set(row.id, row);
    for (const id of new Set(row.agentIds)) {
      const group = byAgent.get(id);
      if (group) group.push(row);
      else byAgent.set(id, [row]);
    }
  }
  return { byAgent, byId };
}

export function projectAgentHistory(
  agent: CopilotAgentSummary,
  ambiguous: boolean,
  rows: readonly CopilotWorkspaceChat[],
  activeId: string | null,
  archive: ReturnType<typeof groupArchive>,
  state: ArchiveState,
  acpComplete: boolean,
): AgentHistory {
  const cloud = isStudioAgentId(agent.id);
  let scoped = !cloud;
  if (cloud && agent.source) {
    try {
      scoped = studioAgentId(agent.source) === agent.id;
    } catch {
      /* malformed catalog scope */
    }
  }
  const attributable = scoped && agent.id !== "__unavailable__" && !ambiguous;
  const acpIds = new Set<string>();
  const live: CopilotWorkspaceChat[] = [];
  const archived = new Map<string, ArchiveSession>();
  if (attributable && !cloud) {
    for (const row of rows) {
      if (row.agentId !== agent.id) continue;
      if (row.sessionId !== null) {
        const key = acpKey(row.sessionId);
        if (acpIds.has(key)) continue;
        acpIds.add(key);
      }
      live.push(row);
    }
    if (activeId) acpIds.add(acpKey(activeId));
    for (const id of acpIds) {
      const found = archive.byId.get(id);
      if (found) archived.set(id, found);
    }
  }
  if (attributable) {
    for (const id of new Set([agent.id, fleetIdentity(agent)]))
      for (const row of archive.byAgent.get(id) ?? [])
        archived.set(row.id, row);
  }
  const bySource = new Map<string, number>();
  const archiveOnly: ArchiveSession[] = [];
  for (const row of archived.values()) {
    if (!acpIds.has(row.id)) archiveOnly.push(row);
    for (const id of new Set(row.sourceIds))
      bySource.set(id, (bySource.get(id) ?? 0) + 1);
  }
  archiveOnly.sort(
    (a, b) =>
      (Date.parse(b.updatedAt ?? "") || 0) -
        (Date.parse(a.updatedAt ?? "") || 0) || a.id.localeCompare(b.id),
  );
  const known = acpIds.size + archiveOnly.length;
  const archiveComplete =
    !!state.index?.sources.length &&
    !state.failed &&
    !state.loading &&
    state.index.sources.every(
      (s) => s.complete && !s.unreadable && !s.unattributed,
    );
  const complete = attributable && !cloud && archiveComplete && acpComplete;
  const observed = (n: number, done: boolean) => n || (done ? 0 : null);
  return {
    summary: {
      known: observed(known, complete),
      acp: observed(acpIds.size, attributable && !cloud && acpComplete),
      archiveOnly: observed(archiveOnly.length, complete),
      coverage: complete
        ? "complete"
        : !attributable ||
            (cloud && !known) ||
            (!state.index && !acpComplete && !known)
          ? "uncollected"
          : "partial",
    },
    live,
    archiveOnly,
    bySource,
  };
}

export function countLabel(summary: SessionSummary) {
  if (summary.known === null)
    return summary.coverage === "uncollected"
      ? "Sessions not collected"
      : "Session count unknown";
  return `${summary.coverage === "complete" ? "" : "At least "}${summary.known.toLocaleString()} session${summary.known === 1 ? "" : "s"}`;
}
