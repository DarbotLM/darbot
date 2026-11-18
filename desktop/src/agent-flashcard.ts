import type { SessionSummary } from "./history-sources";

/** Darbot Decimal UTF-8 v1: "01" | five decimal digits of byte length | three digits per byte.
 * The entire frame is a string (including leading zeros), never a JavaScript Number.
 * Both local Tetrid v3 bakeoffs use a normalized letter spine and G3/G4/G5 glyphs;
 * PERCY_EXPORT additionally tracks 8-bit shapes, projection history and versioned reflections.
 * Neither file establishes a wire format for Darbot or canonical Tetrid compatibility.
 */
export const MAX_BYTES = 8192;
export const MAX_FRAME = 7 + 3 * MAX_BYTES;
const utf8 = new TextEncoder();
const unicode = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });

export function numberstring(text: string): string {
  if (text.length > MAX_BYTES)
    throw new Error("Input exceeds the frame limit.");
  const bytes = utf8.encode(text);
  if (bytes.length > MAX_BYTES || unicode.decode(bytes) !== text)
    throw new Error("Invalid or oversized Unicode input.");
  return `01${String(bytes.length).padStart(5, "0")}${Array.from(bytes, (b) => String(b).padStart(3, "0")).join("")}`;
}

export function fromNumberstring(frame: string): string {
  if (
    frame.length < 7 ||
    frame.length > MAX_FRAME ||
    !frame.startsWith("01") ||
    /\D/.test(frame)
  )
    throw new Error("Invalid Darbot decimal UTF-8 frame.");
  const size = Number(frame.slice(2, 7));
  if (size > MAX_BYTES || frame.length !== 7 + size * 3)
    throw new Error("Frame length mismatch.");
  const bytes = new Uint8Array(size);
  for (let i = 0; i < size; i++) {
    const b = Number(frame.slice(7 + i * 3, 10 + i * 3));
    if (b > 255) throw new Error("Invalid byte.");
    bytes[i] = b;
  }
  return unicode.decode(bytes);
}

export type Feedback = {
  pinned: boolean;
  reviewed: boolean;
  dismissed: boolean;
};
export const noFeedback: Feedback = Object.freeze({
  pinned: false,
  reviewed: false,
  dismissed: false,
});
export type ImplementationEvidence = {
  verifiedPoints: number;
  verifiedCommits: number;
};
export type RowFeatures = {
  kind: "acp" | "archive" | "draft";
  sessionKey: string;
  updatedAt: string | null;
  feedback: Feedback | null;
  evidence: ImplementationEvidence | null;
};
export type Flashcard = {
  schema: "darbot.agent-flashcard";
  version: 1;
  agent: string;
  glyph: string;
  sessions: SessionSummary;
  observation: {
    kind: RowFeatures["kind"];
    session: string;
    updatedAt: string | null;
    sourceFeedback: Feedback | null;
    sourceEvidence: ImplementationEvidence | null;
  } | null;
};
const sha = async (keys: string[]) =>
  Array.from(
    new Uint8Array(
      await crypto.subtle.digest("SHA-256", utf8.encode(JSON.stringify(keys))),
    ),
    (b) => b.toString(16).padStart(2, "0"),
  ).join("");
const glyphFor = (id: string) =>
  String.fromCharCode(0x2800 + parseInt(id.slice(0, 2), 16));

export async function makeFlashcard(
  agentKey: string,
  sessions: SessionSummary,
  row?: RowFeatures,
): Promise<Flashcard> {
  const agent = await sha(["darbot-agent-v1", agentKey]);
  return validateCard({
    schema: "darbot.agent-flashcard",
    version: 1,
    agent,
    glyph: glyphFor(agent),
    sessions,
    observation: row
      ? {
          kind: row.kind,
          session: await sha(["darbot-row-v1", agentKey, row.sessionKey]),
          updatedAt:
            row.updatedAt && Number.isFinite(Date.parse(row.updatedAt))
              ? new Date(row.updatedAt).toISOString()
              : null,
          sourceFeedback: row.feedback,
          sourceEvidence: row.evidence,
        }
      : null,
  });
}

function invalid(): never {
  throw new Error("Invalid flashcard fields, bounds or version.");
}
function fields(v: unknown, keys: string): Record<string, unknown> {
  if (!v || typeof v !== "object" || Array.isArray(v)) return invalid();
  const names = keys.split(" ");
  if (
    Object.keys(v).length !== names.length ||
    !names.every((key) => Object.hasOwn(v, key))
  )
    return invalid();
  return v as Record<string, unknown>;
}
const hash = (v: unknown) =>
  typeof v === "string" && v.length === 64 && !/[^0-9a-f]/.test(v)
    ? v
    : invalid();
const integer = (v: unknown) =>
  typeof v === "number" && Number.isSafeInteger(v) && v >= 0 ? v : invalid();
const maybeCount = (v: unknown) => (v === null ? null : integer(v));
export function feedback(v: unknown): Feedback {
  const { pinned, reviewed, dismissed } = fields(
    v,
    "pinned reviewed dismissed",
  );
  if (
    typeof pinned !== "boolean" ||
    typeof reviewed !== "boolean" ||
    typeof dismissed !== "boolean"
  )
    return invalid();
  return { pinned, reviewed, dismissed };
}
function validateCard(v: unknown): Flashcard {
  const card = fields(v, "schema version agent glyph sessions observation");
  const agent = hash(card.agent),
    glyph = glyphFor(agent);
  if (
    card.schema !== "darbot.agent-flashcard" ||
    card.version !== 1 ||
    card.glyph !== glyph
  )
    return invalid();
  const summary = fields(card.sessions, "known acp archiveOnly coverage");
  const known = maybeCount(summary.known),
    acp = maybeCount(summary.acp),
    archiveOnly = maybeCount(summary.archiveOnly);
  const coverage = summary.coverage;
  if (
    coverage !== "complete" &&
    coverage !== "partial" &&
    coverage !== "uncollected"
  )
    return invalid();
  if (
    (coverage === "complete" && [known, acp, archiveOnly].includes(null)) ||
    (known ?? 0) !== (acp ?? 0) + (archiveOnly ?? 0) ||
    (coverage !== "complete" && known === 0) ||
    (coverage === "uncollected" && known !== null)
  )
    return invalid();
  let observation: Flashcard["observation"] = null;
  if (card.observation !== null) {
    const o = fields(
      card.observation,
      "kind session updatedAt sourceFeedback sourceEvidence",
    );
    const kind = o.kind,
      at = o.updatedAt;
    if (kind !== "acp" && kind !== "archive" && kind !== "draft")
      return invalid();
    if (
      at !== null &&
      (typeof at !== "string" ||
        at.length !== 24 ||
        !Number.isFinite(Date.parse(at)) ||
        new Date(at).toISOString() !== at)
    )
      return invalid();
    const e =
      o.sourceEvidence === null
        ? null
        : fields(o.sourceEvidence, "verifiedPoints verifiedCommits");
    const sourceEvidence = e
      ? {
          verifiedPoints: integer(e.verifiedPoints),
          verifiedCommits: integer(e.verifiedCommits),
        }
      : null;
    if (
      sourceEvidence &&
      (kind !== "acp" ||
        (!sourceEvidence.verifiedCommits && sourceEvidence.verifiedPoints))
    )
      return invalid();
    observation = {
      kind,
      session: hash(o.session),
      updatedAt: at,
      sourceFeedback:
        o.sourceFeedback === null ? null : feedback(o.sourceFeedback),
      sourceEvidence,
    };
  }
  return {
    schema: "darbot.agent-flashcard",
    version: 1,
    agent,
    glyph,
    sessions: { known, acp, archiveOnly, coverage },
    observation,
  };
}

/** Exchange is intentionally closed and canonical: no free text, unknown keys, duplicate keys,
 * unsafe/rounded integers, transcript, endpoint, instruction or executable capability slots. */
export function parseFlashcard(input: string): Flashcard {
  if (input.length > MAX_FRAME)
    throw new Error("Flashcard exceeds the size limit.");
  const isJson = input.trimStart().startsWith("{");
  const raw = isJson ? input : fromNumberstring(input);
  if (raw.length > MAX_BYTES) return invalid();
  const parsed = JSON.parse(raw);
  const card = validateCard(parsed);
  // None of the accepted string fields contain whitespace. This also rejects duplicate
  // keys and lossy numeric literals without rejecting pretty-printed or reordered JSON.
  if (
    JSON.stringify(parsed) !== raw.replace(/[ \t\r\n]/g, "") ||
    (!isJson && JSON.stringify(card) !== raw)
  )
    throw new Error(
      "Paste lossless Darbot v1 JSON or its canonical numberstring.",
    );
  return card;
}
