import { expect, mock, test } from "bun:test";
import {
  COPILOT_WORKSPACE_SIDEPANE,
  defineSidepane,
  emptySidepaneDisclosure,
  isSidepaneGroupExpanded,
  readSidepaneDisclosure,
  SIDEPANE_AGENT_LAYER,
  SIDEPANE_DISCLOSURE_STORAGE_KEY,
  setSidepaneGroupExpanded,
  sidepaneGroupKey,
  sidepaneLayer,
  sidepaneRegion,
  validateSidepaneDisclosure,
  writeSidepaneDisclosure,
} from "./sidepane-layers";

function storage(seed: Record<string, string> = {}) {
  const items = new Map(Object.entries(seed));
  return {
    getItem: (key: string) => items.get(key) ?? null,
    setItem: (key: string, value: string) => void items.set(key, value),
    saved: () => items.get(SIDEPANE_DISCLOSURE_STORAGE_KEY) ?? null,
  };
}

test("layers are ordered by region and then declared order", () => {
  const spec = defineSidepane({
    id: "demo",
    label: "Demo",
    layers: [
      { id: "tools", title: "Tools", region: "footer" },
      { id: "second", title: "Second", region: "body", order: 2 },
      { id: "first", title: "First", region: "body", order: 1 },
      { id: "brand", title: "Brand", region: "header" },
    ],
  });
  expect(spec.layers.map((layer) => layer.id)).toEqual([
    "brand",
    "first",
    "second",
    "tools",
  ]);
  expect(sidepaneRegion(spec, "body").map((layer) => layer.id)).toEqual([
    "first",
    "second",
  ]);
  expect(Object.isFrozen(spec)).toBe(true);
  expect(Object.isFrozen(spec.layers)).toBe(true);
  expect(spec.layers.every(Object.isFrozen)).toBe(true);
});

test("invalid sidepane descriptions are rejected", () => {
  expect(() =>
    defineSidepane({ id: " ", label: "Demo", layers: [] }),
  ).toThrow();
  expect(() =>
    defineSidepane({
      id: "demo",
      label: "Demo",
      layers: [
        { id: "a", title: "A", region: "body" },
        { id: "a", title: "A again", region: "body" },
      ],
    }),
  ).toThrow(/declared more than once/);
  expect(() =>
    defineSidepane({
      id: "demo",
      label: "Demo",
      layers: [
        { id: "a", title: "A", region: "body", expandedByDefault: true },
      ],
    }),
  ).toThrow(/without being collapsible/);
  expect(() => sidepaneLayer(COPILOT_WORKSPACE_SIDEPANE, "missing")).toThrow();
});

test("sidepane declarations reject malformed shapes, unsafe keys, and coercion", () => {
  const invalid = (value: unknown) => () =>
    defineSidepane(value as Parameters<typeof defineSidepane>[0]);
  const spec = (layer: unknown) => ({
    id: "demo",
    label: "Demo",
    layers: [layer],
  });
  const layer = { id: "agent", title: "Agent", region: "body" };
  expect(invalid(null)).toThrow(/must be an object/);
  expect(invalid({ ...spec(layer), extra: true })).toThrow(
    /unsupported fields/,
  );
  expect(invalid(spec({ ...layer, other: true }))).toThrow(
    /unsupported fields/,
  );
  expect(invalid(spec(null))).toThrow(/must be an object/);
  expect(invalid({ ...spec(layer), id: "demo:other" })).toThrow(/identifier/);
  expect(invalid(spec({ ...layer, id: " agent" }))).toThrow(/identifier/);
  expect(invalid(spec({ ...layer, collapsible: "false" }))).toThrow(/boolean/);
  expect(invalid(spec({ ...layer, expandedByDefault: "true" }))).toThrow(
    /boolean/,
  );
  expect(invalid(spec({ ...layer, region: "aside" }))).toThrow(/must sit in/);
  expect(invalid(spec({ ...layer, order: -1 }))).toThrow(
    /non-negative integer/,
  );
  expect(invalid(spec({ ...layer, order: 1.5 }))).toThrow(
    /non-negative integer/,
  );
});

test("agent groups collapse by default and open for the active agent", () => {
  const disclosure = emptySidepaneDisclosure();
  expect(
    isSidepaneGroupExpanded(disclosure, SIDEPANE_AGENT_LAYER, "architect"),
  ).toBe(false);
  expect(
    isSidepaneGroupExpanded(
      disclosure,
      SIDEPANE_AGENT_LAYER,
      "architect",
      true,
    ),
  ).toBe(true);
});

test("an explicit choice overrides the active-agent default in both directions", () => {
  const collapsed = setSidepaneGroupExpanded(
    emptySidepaneDisclosure(),
    SIDEPANE_AGENT_LAYER,
    "architect",
    false,
  );
  expect(
    isSidepaneGroupExpanded(collapsed, SIDEPANE_AGENT_LAYER, "architect", true),
  ).toBe(false);
  const expanded = setSidepaneGroupExpanded(
    collapsed,
    SIDEPANE_AGENT_LAYER,
    "architect",
    true,
  );
  expect(
    isSidepaneGroupExpanded(expanded, SIDEPANE_AGENT_LAYER, "architect"),
  ).toBe(true);
  expect(Object.keys(expanded.groups)).toEqual([
    sidepaneGroupKey("agents", "architect"),
  ]);
});

test("non-collapsible layers stay open and never record state", () => {
  const tools = sidepaneLayer(COPILOT_WORKSPACE_SIDEPANE, "tools");
  const disclosure = setSidepaneGroupExpanded(
    emptySidepaneDisclosure(),
    tools,
    "anything",
    false,
  );
  expect(disclosure.groups).toEqual({});
  expect(isSidepaneGroupExpanded(disclosure, tools, "anything")).toBe(true);
});

test("disclosure state round-trips through storage", () => {
  const store = storage();
  const disclosure = setSidepaneGroupExpanded(
    emptySidepaneDisclosure(),
    SIDEPANE_AGENT_LAYER,
    "architect",
    true,
  );
  writeSidepaneDisclosure(disclosure, store);
  expect(readSidepaneDisclosure(store)).toEqual(disclosure);
  expect(
    readSidepaneDisclosure(
      storage({
        [SIDEPANE_DISCLOSURE_STORAGE_KEY]:
          '{"schemaVersion":1,"groups":{"agents:architect":true}}',
      }),
    ),
  ).toEqual(disclosure);
});

test("unusual agent IDs remain usable without corrupting saved choices", () => {
  const store = storage();
  let disclosure = setSidepaneGroupExpanded(
    emptySidepaneDisclosure(),
    SIDEPANE_AGENT_LAYER,
    "architect",
    true,
  );
  disclosure = setSidepaneGroupExpanded(
    disclosure,
    SIDEPANE_AGENT_LAYER,
    " agent with unsupported metadata ",
    true,
  );
  expect(
    isSidepaneGroupExpanded(
      disclosure,
      SIDEPANE_AGENT_LAYER,
      " agent with unsupported metadata ",
    ),
  ).toBe(true);
  const warn = console.warn;
  const warnings = mock(() => {});
  console.warn = warnings;
  try {
    writeSidepaneDisclosure(disclosure, store);
    expect(warnings).toHaveBeenCalledTimes(1);
  } finally {
    console.warn = warn;
  }
  expect(readSidepaneDisclosure(store)).toEqual(
    setSidepaneGroupExpanded(
      emptySidepaneDisclosure(),
      SIDEPANE_AGENT_LAYER,
      "architect",
      true,
    ),
  );
});

test("unreadable saved layouts fall back to defaults instead of failing", () => {
  const warn = console.warn;
  const warnings = mock(() => {});
  console.warn = warnings;
  try {
    expect(
      readSidepaneDisclosure(
        storage({ [SIDEPANE_DISCLOSURE_STORAGE_KEY]: "{not json" }),
      ),
    ).toEqual(emptySidepaneDisclosure());
    expect(
      readSidepaneDisclosure(
        storage({
          [SIDEPANE_DISCLOSURE_STORAGE_KEY]: JSON.stringify({
            schemaVersion: 99,
            groups: {},
          }),
        }),
      ),
    ).toEqual(emptySidepaneDisclosure());
    expect(warnings).toHaveBeenCalledTimes(2);
  } finally {
    console.warn = warn;
  }
  expect(() =>
    validateSidepaneDisclosure({
      schemaVersion: 1,
      groups: { "agents:architect": "yes" },
    }),
  ).toThrow(/true or false/);
  expect(() =>
    validateSidepaneDisclosure({ schemaVersion: 1, groups: {}, extra: 1 }),
  ).toThrow(/unsupported fields/);
  expect(() =>
    validateSidepaneDisclosure({
      schemaVersion: 1,
      groups: { "agents:": true },
    }),
  ).toThrow(/group keys are invalid/);
  expect(() =>
    validateSidepaneDisclosure({
      schemaVersion: 1,
      groups: { "agents: agent ": true },
    }),
  ).toThrow(/group keys are invalid/);
  expect(() =>
    validateSidepaneDisclosure({
      schemaVersion: 1,
      groups: { "agents:architect": true, "agents:architect:mobile": false },
    }),
  ).not.toThrow();
  expect(() => sidepaneGroupKey("agents:other", "architect")).toThrow(
    /identifier/,
  );
});

test("the workspace sidepane composes the expected layers", () => {
  expect(COPILOT_WORKSPACE_SIDEPANE.layers.map((layer) => layer.id)).toEqual([
    "brand",
    "compose",
    "agents",
    "tools",
  ]);
  expect(SIDEPANE_AGENT_LAYER.collapsible).toBe(true);
  expect(SIDEPANE_AGENT_LAYER.expandedByDefault).toBe(false);
});
