import { expect, test } from "bun:test";
import {
  agentSourceLabel,
  buildFleetCard,
  fleetIdentity,
  type CopilotAgentSummary,
  mergeToolActivity,
  mergeCompaction,
  terminalDisplayText,
  requireLocalAgent,
  studioAgentId,
  studioAgentUrl,
  type StudioAgentSource,
} from "./copilot-types";

test("fleet cards distinguish verified counterparts, scoped zeros and uncollected history", () => {
  const local = (id: string): CopilotAgentSummary => ({
    id,
    name: "Shared display name",
    definition: {
      file: `${id}.md`,
      declaredId: id,
      omittedEntries: 0,
      tools: [],
    },
  });
  const a = local("dayour-swe");
  const b = local("darbot-swe");
  const card = buildFleetCard(a, [a, b], [], null, "all", false, []);
  expect(card.tabs.sessions.indexedCount).toBeNull();
  expect(card.tabs.sessions.linkedCount).toBe(0);
  expect(card.tabs.relationships.edges[0].target).toBe(fleetIdentity(b));
  expect(card.agentcard_ID).toMatch(/^#[0-9A-F]{6}$/);
  const scanned = buildFleetCard(
    a,
    [a],
    [],
    { sessions: [], agents: [], warnings: [] },
    "one folder",
    true,
    [],
  );
  expect(scanned.tabs.sessions.indexedCount).toBe(0);
  expect(scanned.tabs.sessions.indexScope).toBe("one folder");
  expect(scanned.tabs.relationships.edges).toEqual([]);
});

test("card references retain folder scope and partial coverage without copying chat content", () => {
  const agent = {
    id: "review",
    name: "Review",
    model: "model-a",
    description: "Reviews proposals.",
    definition: {
      file: "review.md",
      declaredId: "review",
      tools: [{ name: "calendar", field: "mcps", server: null }],
      omittedEntries: 0,
    },
  };
  const row = {
    agentId: "review",
    agentName: "Review",
    cwd: "A",
    sessionId: "same",
    updatedAt: "2026-11-17T00:00:00Z",
    title: "Not card metadata",
  };
  const card = buildFleetCard(
    agent,
    [agent],
    [],
    {
      sessions: [row, { ...row, cwd: "B" }, { ...row, agentId: "other" }],
      agents: [],
      warnings: ["partial index"],
    },
    "all local folders",
    true,
    [],
    { agentId: "other", sessionId: "foreign-session" },
  );
  expect(card.tabs.sessions).toMatchObject({
    indexedCount: 1,
    indexStatus: "partial",
    indexScope: "all local folders",
  });
  expect(card.tabs.sessions.indexed.map((r) => r.cwd)).toEqual(["A", "B"]);
  expect(card.tabs.runtime.activeSessionId).toBeNull();
  expect(card.tabs.facets.entries.map((f) => f.weight)).toEqual([
    3, 2, 2, 2, 1,
  ]);
  expect(JSON.stringify(card)).not.toContain(row.title);
  expect(JSON.stringify(card)).not.toContain("foreign-session");
});
test("a completion update preserves the real tool title and requested input", () => {
  const input = { command: "Write-Output 'verified'" };
  const pending = mergeToolActivity(undefined, {
    title: "Print verification marker",
    status: "pending",
    input,
  });
  const output = [{ type: "text", text: "verified" }];
  const completed = mergeToolActivity(pending, { status: "completed", output });
  expect(completed).toEqual({
    title: "Print verification marker",
    status: "completed",
    input,
    output,
  });
  expect(pending.status).toBe("pending");
});

test("status-only updates keep the latest tool output", () => {
  const working = mergeToolActivity(undefined, {
    title: "Read process status",
    status: "in_progress",
    output: "Still running",
  });
  expect(mergeToolActivity(working, { status: "failed" })).toEqual({
    ...working,
    status: "failed",
  });
});

test("explicit new output replaces the previous output without losing metadata", () => {
  const previous = mergeToolActivity(undefined, {
    title: "Read process status",
    status: "in_progress",
    output: "Partial output",
  });
  expect(mergeToolActivity(previous, { output: [] })).toEqual({
    ...previous,
    output: [],
  });
});

test("missing initial tool metadata is described without inventing a tool name", () => {
  expect(mergeToolActivity(undefined, { status: "completed" }).title).toBe(
    "Tool call",
  );
});

test("blank update titles do not erase a known tool identity", () => {
  const previous = mergeToolActivity(undefined, {
    title: "Create review artifact",
  });
  expect(
    mergeToolActivity(previous, { title: "  ", status: "completed" }).title,
  ).toBe("Create review artifact");
});

test("ACP compactions preserve receive-order patches, summary replacement and explicit clearing", () => {
  const initial = mergeCompaction(undefined, {
    sessionUpdate: "compaction_summary_chunk",
    compactionId: "c1",
    content: { type: "text", text: "partial" },
  });
  expect(initial.status).toBe("in_progress");
  const patched = mergeCompaction(initial, {
    sessionUpdate: "compaction_update",
    compactionId: "c1",
    status: "completed",
    error: null,
  });
  expect(patched.summary).toEqual(initial.summary);
  const replaced = mergeCompaction(patched, {
    sessionUpdate: "compaction_update",
    compactionId: "c1",
    status: "custom-status",
    summary: [],
    _meta: { trace: "one" },
  });
  expect(replaced.summary).toEqual([]);
  expect(replaced.status).toBe("custom-status");
  const cleared = mergeCompaction(replaced, {
    sessionUpdate: "compaction_update",
    compactionId: "c1",
    status: "cancelled",
    summary: null,
    _meta: null,
  });
  expect(cleared.summary).toBeUndefined();
  expect(cleared._meta).toBeUndefined();
  expect(() =>
    mergeCompaction(initial, { compactionId: "other", status: "completed" }),
  ).toThrow();
});

test("terminal output remains plain text with Unicode and no actionable terminal controls", () => {
  const esc = String.fromCharCode(27);
  const bell = String.fromCharCode(7);
  const input = `before ${esc}[31m红色✓${esc}[0m\n${esc}]52;c;clipboard${bell}after\tend`;
  expect(terminalDisplayText(input)).toBe("before 红色✓\nafter\tend");
  expect(
    terminalDisplayText(
      `${esc}]8;;https://example.com${esc}\\link${esc}]8;;${esc}\\`,
    ),
  ).toBe("link");
});

test("tool updates retain terminal snapshots, kind and file locations after completion", () => {
  const terminals = [
    {
      sessionId: "session-a",
      terminalId: "terminal-a",
      output: "complete",
      truncated: false,
      exitStatus: { exitCode: 0, signal: null },
    },
  ];
  const active = mergeToolActivity(undefined, {
    title: "Build project",
    kind: "execute",
    locations: [{ path: "/project/file" }],
    terminals,
  });
  expect(mergeToolActivity(active, { status: "completed" })).toEqual({
    ...active,
    status: "completed",
  });
  expect(active.terminals).toEqual(terminals);
});

const source: StudioAgentSource = {
  provider: "copilot-studio",
  tenantId: "11111111-1111-1111-1111-111111111111",
  environmentId: "22222222-2222-2222-2222-222222222222",
  environmentName: "Catalog test",
  environmentUrl: "https://example.crm.dynamics.com/",
  botId: "33333333-3333-3333-3333-333333333333",
  schemaName: "test_agent",
  modifiedAt: "2026-11-17T00:00:00Z",
  stateCode: 0,
  lastSyncedAt: 1794873600,
};

test("cloud agent identities are scoped to tenant and environment, never their names", () => {
  const first = studioAgentId(source);
  expect(studioAgentId({ ...source, tenantId: source.environmentId })).not.toBe(
    first,
  );
  expect(studioAgentId({ ...source, environmentId: source.tenantId })).not.toBe(
    first,
  );
  expect(() => requireLocalAgent(first)).toThrow("Copilot Studio");
  expect(() => requireLocalAgent(first.toUpperCase())).toThrow(
    "Copilot Studio",
  );
  expect(() => requireLocalAgent("architect")).not.toThrow();
});

test("asset inventory distinguishes a collected empty list from an uncollected or foreign environment", () => {
  const agent = { id: studioAgentId(source), name: "Test", source };
  const status = {
    ...source,
    agentCount: 1,
    state: "ready" as const,
    error: null,
    assets: [],
  };
  const card = (sources: Parameters<typeof buildFleetCard>[6]) =>
    buildFleetCard(agent, [agent], [], null, "local", false, sources).tabs
      .environment;
  expect(card([status])).toMatchObject({ status: "collected", assets: [] });
  expect(card([{ ...status, tenantId: source.botId }])).toMatchObject({
    status: "not-collected",
    assets: null,
  });
  expect(
    card([{ ...status, assets: undefined, assetWarnings: ["Unavailable"] }]),
  ).toMatchObject({
    status: "not-collected",
    assets: null,
    warningCount: 1,
  });
});

test("cloud purpose requires a matching scoped public declaration, including for legacy caches", () => {
  const base = {
    id: studioAgentId(source),
    name: "Catalog",
    description: "Copilot Studio · Catalog test",
  };
  const render = (s: StudioAgentSource) =>
    buildFleetCard({ ...base, source: s }, [base], [], null, "all", false, []);
  expect(render(source).tabs.overview.description).toBeNull();
  expect(
    render({ ...source, declaredDescription: "Reviews orders." }).tabs.overview
      .description,
  ).toBe("Reviews orders.");
  for (const patch of [
    { declaredDescription: "Bearer private-value" },
    { environmentId: source.botId, declaredDescription: "Wrong tenant row" },
  ]) {
    const card = render({ ...source, ...patch });
    expect(card.tabs.overview.description).toBeNull();
    expect(JSON.stringify(card)).not.toContain(patch.declaredDescription);
  }
});

test("cloud navigation is constructed only from matching valid scoped identities", () => {
  const agent = { id: studioAgentId(source), name: "Test", source };
  expect(studioAgentUrl(agent)).toBe(
    `https://copilotstudio.microsoft.com/environments/${source.environmentId}/bots/${source.botId}/overview?tenantId=${source.tenantId}`,
  );
  expect(() => studioAgentUrl({ ...agent, id: "architect" })).toThrow(
    "does not match",
  );
  expect(() => studioAgentId({ ...source, botId: "../other" })).toThrow(
    "invalid GUID",
  );
  expect(agentSourceLabel(agent)).toContain(source.environmentName);
});
