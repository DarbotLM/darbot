import { invoke } from "@tauri-apps/api/core";
import { useRef, useState } from "react";
import type {
  CopilotAgentSummary,
  CopilotWorkspaceChat,
} from "./copilot-types";
import { fleetIdentity, isStudioAgentId } from "./copilot-types";
import { asProblem } from "./Problem";
import { sessionScope } from "./copilot-workspace-store";
import {
  acpKey,
  type AgentHistory,
  type ArchiveSession,
  type SessionSummary,
} from "./history-sources";
import { HistorySources, type Sources } from "./HistorySources";
import { AgentMemory, HarvestControls } from "./AgentMemory";
import { noFeedback, type ImplementationEvidence } from "./agent-flashcard";
import { draftKey, sessionKey, useHarvest } from "./session-harvest";

type Evidence = {
  status: string;
  commits: string[];
  points: number;
  reason: string;
};
type Result = Evidence | "checking";

export function useSessionEvidence() {
  const cache = useRef(new Map<string, Result>());
  const pending = useRef(0);
  const [, render] = useState(0);
  async function check(chat: CopilotWorkspaceChat, retry = false) {
    if (
      !chat.sessionId ||
      isStudioAgentId(chat.agentId) ||
      chat.agentId === "__unavailable__"
    )
      return;
    const key = sessionScope(chat);
    if (
      pending.current >= 3 ||
      cache.current.get(key) === "checking" ||
      (cache.current.has(key) && !retry)
    )
      return;
    pending.current++;
    cache.current.set(key, "checking");
    render((n) => n + 1);
    try {
      const { sessionId, cwd, agentId } = chat;
      const value = await invoke<Evidence>("copilot_session_evidence", {
        sessionId,
        cwd,
        agentId,
      });
      if (
        !value ||
        typeof value.reason !== "string" ||
        typeof value.status !== "string" ||
        !Array.isArray(value.commits) ||
        !value.commits.every((id) => typeof id === "string") ||
        !Number.isSafeInteger(value.points) ||
        value.points < 0
      ) {
        throw new Error("The runtime returned incomplete evidence.");
      }
      cache.current.set(key, value);
    } catch (error) {
      cache.current.set(key, {
        status: "unknown",
        commits: [],
        points: 0,
        reason: asProblem(error).said,
      });
    } finally {
      pending.current--;
      render((n) => n + 1);
    }
  }
  return { cache: cache.current, check, full: pending.current >= 3 };
}

function points(value?: Result) {
  return value &&
    value !== "checking" &&
    value.status === "verified" &&
    value.commits.length
    ? Math.max(0, value.points)
    : 0;
}

function verified(result?: Result): ImplementationEvidence | null {
  if (!result || result === "checking") return null;
  if (result.status === "none" && !result.commits.length && !result.points)
    return { verifiedPoints: 0, verifiedCommits: 0 };
  if (result.status !== "verified" || !result.commits.length) return null;
  return {
    verifiedPoints: result.points,
    verifiedCommits: result.commits.length,
  };
}
const chatKey = (row: CopilotWorkspaceChat) =>
  row.sessionId === null
    ? draftKey(row.conversationId)
    : sessionKey(acpKey(row.sessionId));

export function ArchiveTiles({
  identity,
  rows,
  summary,
}: {
  identity: string;
  rows: ArchiveSession[];
  summary: SessionSummary;
}) {
  const [limit, setLimit] = useState(5);
  const memory = useHarvest(identity);
  const decisions = (row: ArchiveSession) =>
    memory.available
      ? (memory.decisions.get(sessionKey(row.id)) ?? noFeedback)
      : null;
  const sorted = [...rows].sort(
    (a, b) => Number(!!decisions(b)?.pinned) - Number(!!decisions(a)?.pinned),
  );
  if (!rows.length) return null;
  return (
    <>
      <h4>Archive-only references</h4>
      <ol
        className="copilot-dashboard-tiles"
        aria-label="Archive-only references"
      >
        {sorted.slice(0, limit).map((row) => (
          <li key={row.id}>
            <strong>{row.id}</strong>
            <small>
              {row.sourceIds.join(", ")} ·{" "}
              {row.updatedAt
                ? new Date(row.updatedAt).toLocaleString()
                : "Date unavailable"}
            </small>
            <small>
              Archive only · not resumable · commit evidence unknown
            </small>
            <HarvestControls
              key={identity}
              agentKey={identity}
              summary={summary}
              busy={memory.busy}
              row={{
                kind: "archive",
                sessionKey: sessionKey(row.id),
                feedback: decisions(row),
                updatedAt: row.updatedAt,
                evidence: null,
              }}
            />
          </li>
        ))}
      </ol>
      {rows.length > limit && (
        <button
          type="button"
          className="quiet"
          data-ui-action="history_load_more"
          onClick={() => setLimit((n) => n + 10)}
        >
          Show more archive references ({rows.length - limit} remaining)
        </button>
      )}
    </>
  );
}

export function AgentDashboard({
  agent,
  history,
  sources,
  compact,
  evidence,
  runtime,
  blocked,
  busy,
  loading,
  onContinue,
  onNew,
  onHistory,
}: {
  agent: CopilotAgentSummary;
  history: AgentHistory;
  sources: Sources;
  compact?: boolean;
  evidence: ReturnType<typeof useSessionEvidence>;
  runtime: string | null;
  blocked: string | null;
  busy: boolean;
  loading: boolean;
  onContinue: (row: CopilotWorkspaceChat) => void;
  onNew: () => void;
  onHistory: () => void;
}) {
  const [limit, setLimit] = useState(5);
  const identity = fleetIdentity(agent);
  const memory = useHarvest(identity);
  const decisions = (key: string) =>
    memory.available ? (memory.decisions.get(key) ?? noFeedback) : null;
  const pinned = (key: string) => (decisions(key)?.pinned ? 1 : 0);
  const suggest = (key: string) =>
    memory.available && !decisions(key)?.dismissed && !decisions(key)?.reviewed;
  const ranked = [...history.live].sort(
    (a, b) =>
      pinned(chatKey(b)) - pinned(chatKey(a)) ||
      points(evidence.cache.get(sessionScope(b))) -
        points(evidence.cache.get(sessionScope(a))) ||
      Number(Boolean(b.draft.trim())) - Number(Boolean(a.draft.trim())) ||
      (Date.parse(b.updatedAt ?? "") || 0) -
        (Date.parse(a.updatedAt ?? "") || 0) ||
      a.conversationId.localeCompare(b.conversationId),
  );
  const archives = history.archiveOnly;
  const top = ranked.find((row) => suggest(chatKey(row)));
  const topArchive =
    archives.find(
      (row) => pinned(sessionKey(row.id)) && suggest(sessionKey(row.id)),
    ) ?? archives.find((row) => suggest(sessionKey(row.id)));
  const preferArchive =
    topArchive &&
    (!top || pinned(sessionKey(topArchive.id)) > pinned(chatKey(top)));
  const cloud = isStudioAgentId(agent.id);
  return (
    <section
      className="copilot-agent-dashboard"
      aria-label={`${agent.name} dashboard`}
    >
      <h3>Recorded sessions and drafts</h3>
      <HistorySources history={history} sources={sources} />
      <p className="hint">
        {cloud ? "Copilot Studio" : "Local Copilot CLI"} ·{" "}
        {agent.id || "default agent"}
      </p>
      <p className="hint">
        Pinned first. ACP tiles then rank by verified points, unsent draft and
        activity date. Reviewed or dismissed tiles stay here but are not
        suggested. Feedback never earns points.
      </p>
      {!memory.available && (
        <p role="status">
          {memory.loading
            ? "Reading local feedback..."
            : (memory.problem ??
              "Local feedback unavailable. No decisions were replaced.")}
        </p>
      )}
      <div className="row">
        {!cloud && (
          <>
            <button
              type="button"
              data-ui-action="workspace_new_chat"
              disabled={busy || !!blocked}
              onClick={onNew}
            >
              {compact ? "New chat" : `New chat with ${agent.name}`}
            </button>
            <button
              type="button"
              className="quiet"
              data-ui-action="workspace_view_agent_history"
              onClick={onHistory}
            >
              View agent history
            </button>
          </>
        )}
      </div>
      {blocked && <p>{blocked}</p>}
      {preferArchive ? (
        <p>
          Suggested next step: review archive reference{" "}
          <strong>{topArchive.id}</strong>.
        </p>
      ) : (
        top && (
          <p>
            Suggested next step:{" "}
            {top.draft.trim() ? "finish the unsent draft in" : "review"}{" "}
            <strong>{top.title}</strong>.
          </p>
        )
      )}
      {loading && <p role="status">Reading session history…</p>}
      {!loading && !ranked.length && !archives.length && (
        <p>
          {history.summary.coverage === "complete"
            ? "No recorded sessions or saved drafts for this agent."
            : "No attributed sessions available in the current results."}
        </p>
      )}
      <ArchiveTiles
        identity={identity}
        rows={archives}
        summary={history.summary}
      />
      <ol className="copilot-dashboard-tiles">
        {ranked.slice(0, limit).map((chat) => {
          const result = evidence.cache.get(sessionScope(chat));
          const score = points(result);
          const checked = result && result !== "checking" ? result : null;
          return (
            <li key={chat.conversationId}>
              <strong>{chat.title}</strong>
              <small>
                {chat.cwd} ·{" "}
                {chat.updatedAt
                  ? new Date(chat.updatedAt).toLocaleString()
                  : "Date unavailable"}
              </small>
              <small>
                {chat.draft.trim()
                  ? "Unsent draft"
                  : chat.sessionId
                    ? "Recorded session"
                    : "Local draft"}
                {chat.sessionId &&
                  ` · ${
                    verified(result)?.verifiedCommits
                      ? `${score} verified points (${checked?.commits.length} commits)`
                      : result === "checking"
                        ? "Checking evidence…"
                        : verified(result)
                          ? "No verified commits"
                          : "Commit evidence unknown"
                  }`}
              </small>
              {checked?.reason && <small>{checked.reason}</small>}
              <div className="row">
                <button
                  type="button"
                  data-ui-action="workspace_open_chat"
                  aria-label={`Continue ${chat.title}`}
                  disabled={busy || !!blocked}
                  onClick={() => onContinue(chat)}
                >
                  Continue
                </button>
                {chat.sessionId &&
                  (!result || checked?.status === "unknown") &&
                  chat.agentId !== "__unavailable__" && (
                    <button
                      type="button"
                      className="quiet"
                      data-ui-action="inspect_details"
                      disabled={evidence.full}
                      onClick={() => void evidence.check(chat, !!checked)}
                    >
                      {checked
                        ? "Retry evidence check"
                        : "Check commit evidence"}
                    </button>
                  )}
              </div>
              <HarvestControls
                key={identity}
                agentKey={identity}
                summary={history.summary}
                busy={memory.busy}
                row={{
                  kind: chat.sessionId ? "acp" : "draft",
                  sessionKey: chatKey(chat),
                  updatedAt: chat.updatedAt ?? null,
                  feedback: decisions(chatKey(chat)),
                  evidence: verified(result),
                }}
              />
            </li>
          );
        })}
      </ol>
      {ranked.length > limit && (
        <button
          type="button"
          className="quiet"
          data-ui-action="workspace_view_agent_history"
          onClick={() => setLimit((n) => n + 10)}
        >
          Show more ({ranked.length - limit} remaining)
        </button>
      )}
      <AgentMemory
        key={identity}
        agentKey={identity}
        summary={history.summary}
        memory={memory}
      />
      {!compact && (
        <>
          <h3>Tools and context</h3>
          <p>
            {runtime ??
              "No open session for this agent. Runtime commands and context have not been negotiated."}
          </p>
          <p className="hint">
            Definition only (not confirmed runtime tools):{" "}
            {agent.definition?.tools.map((tool) => tool.name).join(", ") ||
              "none listed"}
            .
          </p>
        </>
      )}
    </section>
  );
}
