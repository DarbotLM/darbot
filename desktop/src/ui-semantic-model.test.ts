import { expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import ts from "typescript";
import {
  COPILOT_WORKSPACE_SIDEPANE,
  SIDEPANE_AGENT_LAYER,
} from "./sidepane-layers";
import {
  DESKTOP_SEMANTIC_MODEL,
  DESKTOP_UI_ACTIONS,
} from "./ui-semantic-model";

const INTERACTIVE_TAGS = new Set([
  "a",
  "button",
  "form",
  "input",
  "select",
  "summary",
  "textarea",
]);
const USER_EVENTS = new Set([
  "onBlur",
  "onCancel",
  "onChange",
  "onClick",
  "onClose",
  "onKeyDown",
  "onScroll",
  "onSubmit",
  "onToggle",
]);

test("every desktop UI control points to a declared MCP action schema", () => {
  const missing: string[] = [];
  const unknown: string[] = [];
  const used = new Set<string>();
  const files = readdirSync(new URL(".", import.meta.url)).filter(
    (file) => file.endsWith(".tsx") && !file.endsWith(".test.tsx"),
  );
  for (const file of files) {
    const source = ts.createSourceFile(
      file,
      readFileSync(new URL(file, import.meta.url), "utf8"),
      ts.ScriptTarget.Latest,
      true,
      ts.ScriptKind.TSX,
    );
    function visit(node: ts.Node): void {
      if (ts.isJsxOpeningElement(node) || ts.isJsxSelfClosingElement(node)) {
        const tag = node.tagName.getText(source);
        const attrs = node.attributes.properties.filter(ts.isJsxAttribute);
        const interactive =
          /^[a-z]/.test(tag) &&
          (INTERACTIVE_TAGS.has(tag) ||
            attrs.some(
              (attr) =>
                ts.isIdentifier(attr.name) && USER_EVENTS.has(attr.name.text),
            ));
        if (interactive) {
          const marker = attrs.find(
            (attr) =>
              ts.isIdentifier(attr.name) && attr.name.text === "data-ui-action",
          );
          const line =
            source.getLineAndCharacterOfPosition(node.getStart(source)).line +
            1;
          const location = `${file}:${line} <${tag}>`;
          if (!marker?.initializer || !ts.isStringLiteral(marker.initializer)) {
            missing.push(location);
          } else if (
            !Object.hasOwn(DESKTOP_UI_ACTIONS, marker.initializer.text)
          ) {
            unknown.push(`${location} ${marker.initializer.text}`);
          } else {
            used.add(marker.initializer.text);
          }
        }
      }
      ts.forEachChild(node, visit);
    }
    visit(source);
  }
  expect(missing).toEqual([]);
  expect(unknown).toEqual([]);
  expect(
    Object.keys(DESKTOP_UI_ACTIONS).filter(
      (id) => !id.startsWith("menu_") && !used.has(id),
    ),
  ).toEqual([]);
});

test("semantic model provides unique, serializable, closed MCP tool schemas", () => {
  expect(DESKTOP_SEMANTIC_MODEL.schemaVersion).toBe(1);
  expect(DESKTOP_SEMANTIC_MODEL.sidepane).toBe(COPILOT_WORKSPACE_SIDEPANE);
  expect(DESKTOP_SEMANTIC_MODEL.surfaces.map(({ id }) => id)).toEqual([
    "workspace",
    "canvas",
    "cli",
  ]);
  const tools = DESKTOP_SEMANTIC_MODEL.mcpTools;
  expect(tools).toHaveLength(Object.keys(DESKTOP_UI_ACTIONS).length);
  expect(new Set(tools.map(({ name }) => name)).size).toBe(tools.length);
  for (const [
    id,
    { surface, effect, platform, humanOnly, tool },
  ] of Object.entries(DESKTOP_UI_ACTIONS)) {
    expect(tool.name).toBe(`darbot_ui_${id}`);
    expect(tool.name).toMatch(/^[a-z][a-z0-9_]*$/);
    expect(tool.title.length).toBeGreaterThan(0);
    expect(tool.description.length).toBeGreaterThan(0);
    expect(surface.length).toBeGreaterThan(0);
    expect(["all", "macos"]).toContain(platform);
    expect(tool.inputSchema.type).toBe("object");
    expect(tool.inputSchema.additionalProperties).toBe(false);
    for (const required of tool.inputSchema.required) {
      expect(Object.hasOwn(tool.inputSchema.properties, required)).toBe(true);
    }
    expect(humanOnly).toBe(effect === "write" || effect === "protected");
    expect(JSON.parse(JSON.stringify(tool))).toEqual(tool);
  }
  expect(
    DESKTOP_UI_ACTIONS.sidebar_toggle_chats.tool.inputSchema.properties.layerId,
  ).toEqual({
    type: "string",
    enum: [SIDEPANE_AGENT_LAYER.id],
  });
  expect(
    DESKTOP_UI_ACTIONS.sidebar_toggle_chats.tool.inputSchema.properties.agentId,
  ).toEqual({ type: "string", minLength: 0 });
  expect(
    DESKTOP_UI_ACTIONS.workspace_edit_agent_name.tool.inputSchema.properties
      .name,
  ).toEqual({ type: "string", minLength: 0, maxLength: 64 });
  expect(
    DESKTOP_UI_ACTIONS.workspace_edit_agent_description.tool.inputSchema
      .properties.description,
  ).toEqual({ type: "string", minLength: 0, maxLength: 1000 });
  expect(
    DESKTOP_UI_ACTIONS.workspace_edit_agent_instructions.tool.inputSchema
      .properties.instructions,
  ).toEqual({ type: "string", minLength: 0, maxLength: 30000 });
  expect(
    DESKTOP_UI_ACTIONS.workspace_edit_draft.tool.inputSchema.properties.text,
  ).toEqual({ type: "string", minLength: 0, maxLength: 32000 });
  expect(
    DESKTOP_UI_ACTIONS.sdk_set_task_prompt.tool.inputSchema.properties.prompt,
  ).toEqual({ type: "string", minLength: 0, maxLength: 16000 });
  expect(
    DESKTOP_UI_ACTIONS.imports_confirm.tool.inputSchema.properties.agentIds,
  ).toEqual({
    type: "array",
    items: { type: "string", minLength: 1 },
    uniqueItems: true,
  });
});

test("native tray and window menu actions have schemas, not runnable tools", () => {
  const native = readFileSync(
    new URL("../src-tauri/src/main.rs", import.meta.url),
    "utf8",
  );
  const items = [
    ...native.matchAll(/MenuItem::with_id\(app, "(open|stop|quit)"/g),
  ]
    .map((match) => match[1])
    .filter((id, index, all) => all.indexOf(id) === index);
  expect(items).toEqual(["open", "stop", "quit"]);
  expect(
    items.map(
      (id) =>
        DESKTOP_UI_ACTIONS[`menu_${id}` as keyof typeof DESKTOP_UI_ACTIONS]
          ?.tool.name,
    ),
  ).toEqual(items.map((id) => `darbot_ui_menu_${id}`));
  expect(DESKTOP_UI_ACTIONS.menu_stop.humanOnly).toBe(true);
  expect(DESKTOP_UI_ACTIONS.menu_quit.humanOnly).toBe(true);
  expect(native).toContain("tauri::WindowEvent::CloseRequested");
  expect(native).toContain("window.hide()");
  expect(DESKTOP_UI_ACTIONS.menu_hide.tool.name).toBe("darbot_ui_menu_hide");
  const macItems = [
    ...native.matchAll(
      /PredefinedMenuItem::(undo|redo|cut|copy|paste|select_all)\(/g,
    ),
  ].map((match) => match[1]);
  expect(macItems).toEqual([
    "undo",
    "redo",
    "cut",
    "copy",
    "paste",
    "select_all",
  ]);
  for (const id of macItems) {
    const action =
      DESKTOP_UI_ACTIONS[`menu_${id}` as keyof typeof DESKTOP_UI_ACTIONS];
    expect(action.tool.name).toBe(`darbot_ui_menu_${id}`);
    expect(action.platform).toBe("macos");
  }
});
