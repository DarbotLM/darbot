import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  countLabel,
  readHistoryIndex,
  type AgentHistory,
  type ArchiveState,
} from "./history-sources";

export function useHistorySources() {
  const [state, setState] = useState<ArchiveState>({
    index: null,
    failed: false,
    loading: true,
  });
  const generation = useRef(0);
  const load = useCallback(async (refresh: boolean) => {
    const request = ++generation.current;
    setState((previous) => ({ ...previous, loading: true }));
    try {
      const index = readHistoryIndex(
        await invoke("history_sources_index", { refresh }),
      );
      if (request === generation.current)
        setState({ index, failed: false, loading: false });
    } catch {
      if (request === generation.current)
        setState((previous) => ({ ...previous, failed: true, loading: false }));
    }
  }, []);
  useEffect(() => {
    void load(false);
    return () => {
      generation.current++;
    };
  }, [load]);
  return { ...state, refresh: () => void load(true) };
}
export type Sources = ReturnType<typeof useHistorySources>;
export function HistorySources({
  history,
  sources,
  expanded,
}: {
  history: AgentHistory;
  sources: Sources;
  expanded?: boolean;
}) {
  const { summary } = history;
  return (
    <details className="history-sources" open={expanded}>
      <summary data-ui-action="inspect_details">
        <strong>{countLabel(summary)}</strong> · History sources
      </summary>
      <p>
        Coverage: {summary.coverage}. Known ACP session IDs:{" "}
        {summary.acp ?? "unknown"}. Archive-only IDs:{" "}
        {summary.archiveOnly ?? "unknown"}. Drafts excluded; shared and repeated
        IDs count once per agent.
      </p>
      {sources.loading && <p role="status">Reading history sources…</p>}
      {sources.failed && (
        <p role="status">
          Archive scan unavailable.{sources.index && " Showing the last scan."}
        </p>
      )}
      {sources.index && (
        <>
          <p>
            Archive scan:{" "}
            <time dateTime={sources.index.scannedAt}>
              {sources.index.scannedAt}
            </time>
            . Source totals cover all agents; this agent’s matches may overlap
            across sources.
          </p>
          {sources.index.sources.map((s) => (
            <p key={s.id}>
              <strong>{s.label}</strong> · this agent:{" "}
              {history.bySource.get(s.id) ?? "no observed matches"}. Source
              totals: {s.indexed} indexed, {s.duplicates} duplicates,{" "}
              {s.unreadable} unreadable, {s.unattributed} unattributed.{" "}
              {s.complete ? "Scan complete" : "Scan incomplete"} · {s.status}
            </p>
          ))}
        </>
      )}
      <button
        type="button"
        className="quiet"
        disabled={sources.loading}
        data-ui-action="history_refresh_sources"
        onClick={sources.refresh}
      >
        Refresh history sources
      </button>
    </details>
  );
}
