import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";
import {
  agentSourceLabel,
  type CopilotAgentCatalog,
  type StudioSourceStatus,
} from "./copilot-types";
import { asProblem, InlineFailure, type Problem } from "./Problem";

export function CopilotAgentImport({
  existingIds = [],
  firstRun = false,
  onComplete,
  onBack,
  onCatalogLoaded,
}: {
  existingIds?: string[];
  firstRun?: boolean;
  onComplete: (ids: string[]) => void;
  onBack: () => void;
  onCatalogLoaded?: (catalog: CopilotAgentCatalog) => void;
}) {
  const [catalog, setCatalog] = useState<CopilotAgentCatalog | null>(null);
  const [failure, setFailure] = useState<Problem | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [query, setQuery] = useState("");
  const [attempt, setAttempt] = useState(0);
  const existing = new Set(existingIds);

  // biome-ignore lint/correctness/useExhaustiveDependencies: The retry counter intentionally repeats native discovery.
  useEffect(() => {
    let active = true;
    setFailure(null);
    invoke<CopilotAgentCatalog>("copilot_agents")
      .then((result) => {
        if (active) {
          setCatalog(result);
          onCatalogLoaded?.(result);
        }
      })
      .catch((error) => {
        if (active) setFailure(asProblem(error));
      });
    return () => {
      active = false;
    };
  }, [attempt]);

  const search = query.trim().toLowerCase();
  const visible = (catalog?.agents ?? []).filter(
    (agent) =>
      !search ||
      `${agent.name} ${agent.description ?? ""} ${agentSourceLabel(agent)}`
        .toLowerCase()
        .includes(search),
  );
  const Heading = firstRun ? "h1" : "h2";

  return (
    <section className="copilot-import" aria-labelledby="copilot-import-title">
      <header>
        {firstRun && <p className="steps-of">Step 3 of 3</p>}
        <Heading id="copilot-import-title">Import agents (optional)</Heading>
        <p className="hint">
          Choose local Copilot agents, or inspect configured Copilot Studio
          sources. Local conversations stay with Copilot CLI. Cloud agents are
          indexed by tenant and environment; their definitions and transcripts
          are not imported.
        </p>
      </header>
      <label htmlFor="copilot-import-search">Search agents</label>
      <input
        id="copilot-import-search"
        data-ui-action="imports_search"
        type="search"
        placeholder="Find an agent by name or description"
        value={query}
        onChange={(event) => setQuery(event.target.value)}
      />
      <div className="copilot-import-summary">
        <span role="status">
          {catalog
            ? `${catalog.agents.length} available. ${selected.length} selected.`
            : failure
              ? "Agent discovery failed."
              : "Reading local agents and configured cloud sources..."}
        </span>
        <button
          type="button"
          className="quiet"
          data-ui-action="imports_select_shown"
          disabled={
            !catalog || visible.every((agent) => existing.has(agent.id))
          }
          onClick={() =>
            setSelected((current) => [
              ...new Set([
                ...current,
                ...visible
                  .filter((agent) => !existing.has(agent.id))
                  .map((agent) => agent.id),
              ]),
            ])
          }
        >
          Select shown
        </button>
        <button
          type="button"
          className="quiet"
          data-ui-action="imports_clear"
          disabled={selected.length === 0}
          onClick={() => setSelected([])}
        >
          Clear
        </button>
      </div>
      <StudioSources sources={catalog?.sources ?? []} />
      {failure && (
        <div>
          <InlineFailure problem={failure} />
          <button
            type="button"
            className="quiet"
            data-ui-action="imports_retry"
            onClick={() => setAttempt((value) => value + 1)}
          >
            Retry
          </button>
        </div>
      )}
      <ul className="copilot-import-options" aria-label="Available agents">
        {visible.map((agent) => (
          <li key={agent.id}>
            <label>
              <input
                type="checkbox"
                data-ui-action="imports_toggle_agent"
                checked={existing.has(agent.id) || selected.includes(agent.id)}
                disabled={existing.has(agent.id)}
                onChange={(event) =>
                  setSelected((current) =>
                    event.target.checked
                      ? [...current, agent.id]
                      : current.filter((id) => id !== agent.id),
                  )
                }
              />
              <span>
                <strong>{agent.name}</strong>
                <span>
                  {existing.has(agent.id)
                    ? "Already in this workspace"
                    : agent.description || "Personal Copilot agent"}
                </span>
                <span>{agentSourceLabel(agent)}</span>
              </span>
            </label>
          </li>
        ))}
        {catalog && visible.length === 0 && (
          <li className="copilot-resource-empty">
            No matching agents in the available sources.
          </li>
        )}
      </ul>
      {!!catalog?.warnings.length && (
        <details className="copilot-runtime-warnings">
          <summary data-ui-action="inspect_details">
            Agent discovery warnings
          </summary>
          <ul>
            {catalog.warnings.map((warning) => (
              <li key={warning}>{warning}</li>
            ))}
          </ul>
        </details>
      )}
      <footer className="copilot-import-actions">
        <button
          type="button"
          className="quiet"
          data-ui-action="imports_back"
          onClick={onBack}
        >
          {firstRun ? "Back" : "Cancel"}
        </button>
        <button
          type="button"
          data-ui-action="imports_skip"
          className={selected.length ? "quiet" : undefined}
          onClick={() => onComplete([])}
        >
          {firstRun ? "Import none" : "Done without importing"}
        </button>
        <button
          type="button"
          data-ui-action="imports_confirm"
          disabled={!selected.length}
          onClick={() => onComplete(selected)}
        >
          Import selected ({selected.length})
        </button>
      </footer>
    </section>
  );
}

export function StudioSources({
  sources,
}: {
  sources: readonly StudioSourceStatus[];
}) {
  if (!sources.length) return null;
  return (
    <section
      className="copilot-source-status"
      aria-label="Copilot Studio source coverage"
    >
      <strong>Copilot Studio sources ({sources.length})</strong>
      {sources.map((source) => (
        <p key={`${source.tenantId}:${source.environmentId}`}>
          <strong>{source.environmentName}</strong>: {source.agentCount} agents
          {" / "}
          {source.state}
          {" / "}
          {source.lastSyncedAt === null
            ? "never synced"
            : `last synced ${new Date(source.lastSyncedAt * 1000).toLocaleString()}`}
          <br />
          {source.environmentUrl} / tenant {source.tenantId}
          {source.error && (
            <>
              <br />
              <span role="alert">{source.error}</span>
            </>
          )}
        </p>
      ))}
    </section>
  );
}
