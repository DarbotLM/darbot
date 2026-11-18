/** Composable, validated sidepane layers and their saved disclosure choices. */

export const SIDEPANE_DISCLOSURE_SCHEMA_VERSION = 1;
export const SIDEPANE_DISCLOSURE_STORAGE_KEY = "darbot:copilot:sidepane";

const MAX_LAYERS = 32;
const MAX_ID_LENGTH = 128;
const MAX_TITLE_LENGTH = 256;
const MAX_DISCLOSURE_ENTRIES = 512;
const MAX_DISCLOSURE_LENGTH = 200_000;
const STABLE_ID = /^[a-z0-9][a-z0-9_-]*$/;

export const SIDEPANE_REGIONS = ["header", "body", "footer"] as const;

export type SidepaneRegion = (typeof SIDEPANE_REGIONS)[number];

export type SidepaneLayer = {
  /** Stable layer identity. Unique within a sidepane. */
  readonly id: string;
  /** Accessible label for the layer. */
  readonly title: string;
  /** Where the layer sits in the column. */
  readonly region: SidepaneRegion;
  /** Ascending placement within the region. */
  readonly order: number;
  /** Whether the layer renders groups that can be expanded and collapsed. */
  readonly collapsible: boolean;
  /** Whether a collapsible group starts expanded when the user has no saved choice. */
  readonly expandedByDefault: boolean;
};

export type SidepaneSpec = {
  readonly id: string;
  readonly label: string;
  layers: readonly SidepaneLayer[];
};

export type SidepaneLayerInput = {
  id: string;
  title: string;
  region: SidepaneRegion;
  order?: number;
  collapsible?: boolean;
  expandedByDefault?: boolean;
};

export type SidepaneSpecInput = {
  id: string;
  label: string;
  layers: readonly SidepaneLayerInput[];
};

/** Persisted, explicit user disclosure choices keyed by group. */
export type SidepaneDisclosure = {
  schemaVersion: typeof SIDEPANE_DISCLOSURE_SCHEMA_VERSION;
  /** Only groups the user has toggled appear here; everything else follows the layer default. */
  groups: Record<string, boolean>;
};

type DisclosureStorage = Pick<Storage, "getItem" | "setItem">;

function record(value: unknown, label: string): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${label} must be an object.`);
  }
  return value as Record<string, unknown>;
}

function onlyFields(
  value: Record<string, unknown>,
  fields: readonly string[],
  label: string,
): void {
  if (Object.keys(value).some((key) => !fields.includes(key))) {
    throw new Error(`${label} contains unsupported fields.`);
  }
}

function identifier(value: unknown, label: string, max: number): string {
  if (typeof value !== "string") {
    throw new Error(`${label} must be a string.`);
  }
  const trimmed = value.trim();
  if (!trimmed || trimmed.length > max) {
    throw new Error(`${label} must be between 1 and ${max} characters.`);
  }
  return trimmed;
}

function stableId(value: unknown, label: string): string {
  const id = identifier(value, label, MAX_ID_LENGTH);
  if (id !== value || !STABLE_ID.test(id)) {
    throw new Error(`${label} must be a lowercase, unspaced identifier.`);
  }
  return id;
}

function isSidepaneRegion(value: unknown): value is SidepaneRegion {
  return SIDEPANE_REGIONS.some((region) => region === value);
}

function persistableGroupKey(key: string): boolean {
  const separator = key.indexOf(":");
  const layerId = key.slice(0, separator);
  const groupId = key.slice(separator + 1);
  return (
    separator > 0 &&
    key.length <= MAX_ID_LENGTH * 2 + 1 &&
    layerId.length <= MAX_ID_LENGTH &&
    STABLE_ID.test(layerId) &&
    !!groupId &&
    groupId.length <= MAX_ID_LENGTH &&
    groupId === groupId.trim() &&
    !/\p{Cc}/u.test(groupId)
  );
}

export function emptySidepaneDisclosure(): SidepaneDisclosure {
  return { schemaVersion: SIDEPANE_DISCLOSURE_SCHEMA_VERSION, groups: {} };
}

/**
 * Validates a sidepane description and returns it with layers ordered by region
 * and then by explicit order, so callers render a stable column.
 */
export function defineSidepane(spec: SidepaneSpecInput): SidepaneSpec {
  const description = record(spec, "Sidepane");
  onlyFields(description, ["id", "label", "layers"], "Sidepane");
  const id = stableId(description.id, "Sidepane ID");
  const label = identifier(
    description.label,
    "Sidepane label",
    MAX_TITLE_LENGTH,
  );
  const inputLayers = description.layers;
  if (
    !Array.isArray(inputLayers) ||
    !inputLayers.length ||
    inputLayers.length > MAX_LAYERS
  ) {
    throw new Error(
      `Sidepane must declare between 1 and ${MAX_LAYERS} layers.`,
    );
  }
  const seen = new Set<string>();
  const layers = inputLayers.map((inputLayer, index) => {
    const layer = record(inputLayer, "Sidepane layer");
    onlyFields(
      layer,
      ["id", "title", "region", "order", "collapsible", "expandedByDefault"],
      "Sidepane layer",
    );
    const layerId = stableId(layer.id, "Sidepane layer ID");
    if (seen.has(layerId)) {
      throw new Error(
        `Sidepane layer "${layerId}" is declared more than once.`,
      );
    }
    seen.add(layerId);
    if (!isSidepaneRegion(layer.region)) {
      throw new Error(
        `Sidepane layer "${layerId}" must sit in ${SIDEPANE_REGIONS.join(", ")}.`,
      );
    }
    const order = layer.order ?? index;
    if (
      typeof order !== "number" ||
      !Number.isSafeInteger(order) ||
      order < 0
    ) {
      throw new Error(
        `Sidepane layer "${layerId}" needs a non-negative integer order.`,
      );
    }
    if (
      layer.collapsible !== undefined &&
      typeof layer.collapsible !== "boolean"
    ) {
      throw new Error(
        `Sidepane layer "${layerId}" needs a boolean collapsible flag.`,
      );
    }
    if (
      layer.expandedByDefault !== undefined &&
      typeof layer.expandedByDefault !== "boolean"
    ) {
      throw new Error(
        `Sidepane layer "${layerId}" needs a boolean expandedByDefault flag.`,
      );
    }
    const collapsible = layer.collapsible ?? false;
    const expandedByDefault = layer.expandedByDefault ?? false;
    if (expandedByDefault && !collapsible) {
      throw new Error(
        `Sidepane layer "${layerId}" cannot default to expanded without being collapsible.`,
      );
    }
    return Object.freeze({
      id: layerId,
      title: identifier(layer.title, "Sidepane layer title", MAX_TITLE_LENGTH),
      region: layer.region,
      order,
      collapsible,
      expandedByDefault,
    } satisfies SidepaneLayer);
  });
  layers.sort(
    (left, right) =>
      SIDEPANE_REGIONS.indexOf(left.region) -
        SIDEPANE_REGIONS.indexOf(right.region) || left.order - right.order,
  );
  return Object.freeze({ id, label, layers: Object.freeze(layers) });
}

export function sidepaneRegion(
  spec: SidepaneSpec,
  region: SidepaneRegion,
): SidepaneLayer[] {
  return spec.layers.filter((layer) => layer.region === region);
}

export function sidepaneLayer(
  spec: SidepaneSpec,
  layerId: string,
): SidepaneLayer {
  const layer = spec.layers.find((candidate) => candidate.id === layerId);
  if (!layer) {
    throw new Error(`Sidepane "${spec.id}" has no layer "${layerId}".`);
  }
  return layer;
}

/** Namespaces a group key so two layers never share disclosure state. */
export function sidepaneGroupKey(layerId: string, groupId: string): string {
  if (typeof groupId !== "string" || !groupId) {
    throw new Error("Sidepane group ID must be a nonempty string.");
  }
  return `${stableId(layerId, "Sidepane layer ID")}:${groupId}`;
}

/**
 * Resolves whether a group is open. An explicit user choice always wins;
 * otherwise the group follows the layer default, and an active group is opened
 * so the current selection is never hidden behind a collapsed arrow.
 */
export function isSidepaneGroupExpanded(
  disclosure: SidepaneDisclosure,
  layer: SidepaneLayer,
  groupId: string,
  active = false,
): boolean {
  if (!layer.collapsible) return true;
  const stored = disclosure.groups[sidepaneGroupKey(layer.id, groupId)];
  if (typeof stored === "boolean") return stored;
  return layer.expandedByDefault || active;
}

/** Records an explicit choice, dropping the oldest entries past the bound. */
export function setSidepaneGroupExpanded(
  disclosure: SidepaneDisclosure,
  layer: SidepaneLayer,
  groupId: string,
  expanded: boolean,
): SidepaneDisclosure {
  if (!layer.collapsible) return disclosure;
  if (typeof expanded !== "boolean") {
    throw new Error("Sidepane group expansion must be true or false.");
  }
  const key = sidepaneGroupKey(layer.id, groupId);
  const entries = Object.entries(disclosure.groups).filter(
    ([candidate]) => candidate !== key,
  );
  entries.push([key, expanded]);
  return {
    schemaVersion: SIDEPANE_DISCLOSURE_SCHEMA_VERSION,
    groups: Object.fromEntries(entries.slice(-MAX_DISCLOSURE_ENTRIES)),
  };
}

export function validateSidepaneDisclosure(value: unknown): SidepaneDisclosure {
  const data = record(value, "Saved sidepane layout");
  onlyFields(data, ["schemaVersion", "groups"], "Saved sidepane layout");
  if (data.schemaVersion !== SIDEPANE_DISCLOSURE_SCHEMA_VERSION) {
    throw new Error(
      "Unsupported sidepane layout version. Use the Darbot version that saved it.",
    );
  }
  const entries = Object.entries(record(data.groups, "Saved sidepane groups"));
  if (entries.length > MAX_DISCLOSURE_ENTRIES) {
    throw new Error(
      `Saved sidepane layout must contain at most ${MAX_DISCLOSURE_ENTRIES} groups.`,
    );
  }
  const groups: [string, boolean][] = [];
  for (const [key, expanded] of entries) {
    if (!persistableGroupKey(key)) {
      throw new Error("Saved sidepane group keys are invalid.");
    }
    if (typeof expanded !== "boolean") {
      throw new Error(`Saved sidepane group "${key}" must be true or false.`);
    }
    groups.push([key, expanded]);
  }
  return {
    schemaVersion: SIDEPANE_DISCLOSURE_SCHEMA_VERSION,
    groups: Object.fromEntries(groups),
  };
}

/** Presentation state is optional, but corrupt storage is reported before using defaults. */
export function readSidepaneDisclosure(
  storage?: DisclosureStorage,
): SidepaneDisclosure {
  try {
    const raw = (storage ?? window.localStorage).getItem(
      SIDEPANE_DISCLOSURE_STORAGE_KEY,
    );
    if (raw === null) return emptySidepaneDisclosure();
    if (raw.length > MAX_DISCLOSURE_LENGTH) {
      throw new Error(
        "Saved sidepane layout exceeds the supported storage size.",
      );
    }
    return validateSidepaneDisclosure(JSON.parse(raw));
  } catch (error) {
    console.warn(
      "Saved sidepane layout could not be read; using defaults.",
      error,
    );
    return emptySidepaneDisclosure();
  }
}

export function writeSidepaneDisclosure(
  disclosure: SidepaneDisclosure,
  storage?: DisclosureStorage,
): void {
  const entries = Object.entries(disclosure.groups);
  const persistable = entries.filter(([key]) => persistableGroupKey(key));
  if (persistable.length !== entries.length) {
    console.warn(
      "Some agent IDs cannot be saved as sidepane disclosure keys; their choices remain available until the window closes.",
    );
  }
  const serialized = JSON.stringify(
    validateSidepaneDisclosure({
      ...disclosure,
      groups: Object.fromEntries(persistable),
    }),
  );
  if (serialized.length > MAX_DISCLOSURE_LENGTH) {
    throw new Error("The sidepane layout exceeds the supported storage size.");
  }
  (storage ?? window.localStorage).setItem(
    SIDEPANE_DISCLOSURE_STORAGE_KEY,
    serialized,
  );
}

/** The sidepane composed by the Copilot workspace. */
export const COPILOT_WORKSPACE_SIDEPANE = defineSidepane({
  id: "copilot-workspace",
  label: "Agents and chats",
  layers: [
    { id: "brand", title: "Darbot", region: "header", order: 0 },
    { id: "compose", title: "Start work", region: "header", order: 1 },
    {
      id: "agents",
      title: "Agents",
      region: "body",
      order: 0,
      collapsible: true,
      expandedByDefault: false,
    },
    { id: "tools", title: "Workspace tools", region: "footer", order: 0 },
  ],
});

export const SIDEPANE_AGENT_LAYER = sidepaneLayer(
  COPILOT_WORKSPACE_SIDEPANE,
  "agents",
);
