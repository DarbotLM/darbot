import { useState } from "react";
import {
  MAX_FRAME,
  makeFlashcard,
  numberstring,
  parseFlashcard,
  type Feedback,
  type RowFeatures,
} from "./agent-flashcard";
import type { SessionSummary } from "./history-sources";
import {
  forgetReference,
  keepReference,
  toggleDecision,
  type useHarvest,
} from "./session-harvest";
import { asProblem } from "./Problem";

type Props = { agentKey: string; summary: SessionSummary };
type Memory = ReturnType<typeof useHarvest>;
function problem(e: unknown) {
  return asProblem(e).said;
}

export function ExportPreview({
  agentKey,
  summary,
  row,
}: Props & { row?: RowFeatures }) {
  const [json, setJson] = useState("");
  const [notice, setNotice] = useState("");
  async function preview() {
    try {
      setJson(JSON.stringify(await makeFlashcard(agentKey, summary, row)));
      setNotice("");
    } catch (e) {
      setJson("");
      setNotice(problem(e));
    }
  }
  async function copy(text: string) {
    try {
      await navigator.clipboard.writeText(text);
      setNotice("Copied. Paste into another agent’s local memory to validate.");
    } catch {
      setNotice("Clipboard unavailable. Select and copy the preview manually.");
    }
  }
  const frame = json ? numberstring(json) : "";
  return (
    <div className="flashcard-exchange">
      <button
        type="button"
        className="quiet"
        data-ui-action="inspect_details"
        onClick={() => void preview()}
      >
        {row ? "Preview session flashcard" : "Preview agent flashcard"}
      </button>
      {json && (
        <>
          <p>
            Source snapshot, not instructions. Fingerprints identify the source;
            the glyph is decorative. Recency, source feedback and commit
            evidence are separate. Missing evidence is null, not zero.
          </p>
          <label>
            Flashcard JSON
            <textarea readOnly data-ui-action="inspect_details" value={json} />
          </label>
          <button
            type="button"
            data-ui-action="memory_copy"
            onClick={() => void copy(json)}
          >
            Copy flashcard JSON
          </button>
          <label>
            Darbot numberstring
            <textarea readOnly data-ui-action="inspect_details" value={frame} />
          </label>
          <button
            type="button"
            data-ui-action="memory_copy"
            onClick={() => void copy(frame)}
          >
            Copy numberstring
          </button>
          <small>
            Darbot decimal UTF-8 v1: 01, five length digits, then three digits
            per byte (000–255; max 8192 bytes). A reversible model input string,
            not a trained model weight. Tetrid interoperability is unverified.
          </small>
          <details>
            <summary data-ui-action="inspect_details">
              Format and design references
            </summary>
            <p>
              For example, “A” is 0100001065. Keep leading zeros; decode bytes
              as strict UTF-8 without normalization. A flashcard frames its
              compact v1 JSON. Fingerprints are SHA-256, not secrets or
              authentication; the display glyph uses only the first byte and is
              not an identifier.
            </p>
            <p>
              The local Wednesday bakeoff uses normalized letter runs, a letter
              spine and G3/G4/G5 glyph segments. PERCY_EXPORT also records 8-bit
              word-shape history and versioned reflections. Darbot does not
              implement either codebook or import their histories. The canonical
              Tetrid specification was not verified.
            </p>
          </details>
        </>
      )}
      {notice && <small role="status">{notice}</small>}
    </div>
  );
}

export function HarvestControls({
  agentKey,
  summary,
  row,
  busy = false,
}: Props & { row: RowFeatures; busy?: boolean }) {
  const [notice, setNotice] = useState("");
  const [saving, setSaving] = useState(false);
  const decisions = row.feedback;
  const buttons: [keyof Feedback, string, string][] = [
    ["pinned", "Pin", "Unpin"],
    ["reviewed", "Mark reviewed", "Undo reviewed"],
    ["dismissed", "Dismiss suggestion", "Restore suggestion"],
  ];
  return (
    <div>
      <div className="row">
        {buttons.map(([field, off, on]) => (
          <button
            key={field}
            type="button"
            className="quiet"
            data-ui-action="harvest_decide"
            disabled={!decisions || busy || saving}
            aria-pressed={decisions?.[field] ?? false}
            onClick={async () => {
              setSaving(true);
              try {
                await toggleDecision(agentKey, row.sessionKey, field);
                setNotice("");
              } catch (e) {
                setNotice(problem(e));
              } finally {
                setSaving(false);
              }
            }}
          >
            {decisions?.[field] ? on : off}
          </button>
        ))}
      </div>
      <ExportPreview agentKey={agentKey} summary={summary} row={row} />
      {notice && <small role="alert">{notice}</small>}
    </div>
  );
}

export function AgentMemory({
  agentKey,
  summary,
  memory,
}: Props & { memory: Memory }) {
  const [paste, setPaste] = useState("");
  const [validated, setValidated] = useState("");
  const [notice, setNotice] = useState("");
  const [saving, setSaving] = useState(false);
  return (
    <details className="flashcard-exchange">
      <summary data-ui-action="inspect_details">
        Agent flashcards ·{" "}
        {memory.available ? memory.references.length : "unknown"} local
        references
      </summary>
      <p>
        Preview and copy on the source agent. On the destination agent, paste,
        validate, then save. Imported observations are references only: they
        never change local decisions, scores, permissions or prompts.
      </p>
      <ExportPreview agentKey={agentKey} summary={summary} />
      <label>
        Paste a flashcard for this agent
        <textarea
          data-ui-action="memory_paste"
          value={paste}
          maxLength={MAX_FRAME + 1}
          disabled={saving}
          onInput={(event) => {
            setPaste(event.currentTarget.value);
            setValidated("");
            setNotice("");
          }}
        />
      </label>
      <button
        type="button"
        data-ui-action="inspect_details"
        disabled={!paste || saving}
        onClick={() => {
          try {
            setValidated(JSON.stringify(parseFlashcard(paste)));
            setNotice("Validated. Review the source snapshot before saving.");
          } catch {
            setValidated("");
            setNotice("Invalid flashcard. No reference saved.");
          }
        }}
      >
        Validate and preview
      </button>
      {validated && (
        <>
          <pre>{JSON.stringify(JSON.parse(validated), null, 2)}</pre>
          <button
            type="button"
            data-ui-action="memory_store"
            disabled={!memory.available || memory.busy || saving}
            onClick={async () => {
              setSaving(true);
              try {
                await keepReference(agentKey, validated);
                setValidated("");
                setPaste("");
                setNotice("Reference saved locally. Nothing sent.");
              } catch (e) {
                setNotice(problem(e));
              } finally {
                setSaving(false);
              }
            }}
          >
            Save as local reference
          </button>
        </>
      )}
      {(!memory.available || memory.problem) && (
        <small>
          {memory.loading
            ? "Reading stored memory..."
            : (memory.problem ??
              "Stored memory unavailable; existing data will not be overwritten.")}
        </small>
      )}
      {memory.references.map((card) => (
        <details key={JSON.stringify(card)}>
          <summary data-ui-action="inspect_details">
            Imported reference · {card.agent.slice(0, 12)} ·{" "}
            {card.observation?.kind ?? "agent"}
          </summary>
          <pre>{JSON.stringify(card, null, 2)}</pre>
          <button
            type="button"
            data-ui-action="memory_store"
            disabled={!memory.available || memory.busy || saving}
            onClick={async () => {
              setSaving(true);
              try {
                await forgetReference(agentKey, card);
                setNotice("Reference removed.");
              } catch (e) {
                setNotice(problem(e));
              } finally {
                setSaving(false);
              }
            }}
          >
            Remove reference
          </button>
        </details>
      ))}
      {notice && <p role="status">{notice}</p>}
    </details>
  );
}
