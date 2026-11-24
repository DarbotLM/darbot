import { MAX_DRAFT_LENGTH } from "./copilot-workspace-store";
import {
  COPILOT_WORKSPACE_SIDEPANE,
  SIDEPANE_AGENT_LAYER,
} from "./sidepane-layers";

type StringField = {
  readonly type: "string";
  readonly minLength?: number;
  readonly maxLength?: number;
  readonly enum?: readonly string[];
};
type BooleanField = { readonly type: "boolean" };
type StringListField = {
  readonly type: "array";
  readonly items: StringField;
  readonly uniqueItems?: boolean;
};
type NullableStringField = {
  readonly type: readonly ["string", "null"];
};
type Field = StringField | BooleanField | StringListField | NullableStringField;

export type UiToolInputSchema = {
  readonly type: "object";
  readonly properties: Readonly<Record<string, Field>>;
  readonly required: readonly string[];
  readonly additionalProperties: false;
};

export type UiMcpToolSchema = {
  readonly name: string;
  readonly title: string;
  readonly description: string;
  readonly inputSchema: UiToolInputSchema;
  readonly annotations: {
    readonly readOnlyHint: boolean;
    readonly destructiveHint: boolean;
    readonly openWorldHint: boolean;
  };
};

type Surface =
  | "setup"
  | "imports"
  | "sidebar"
  | "workspace"
  | "canvas"
  | "cli"
  | "settings"
  | "history"
  | "sdk"
  | "menu";
type Effect = "read" | "local" | "write" | "protected";
type Platform = "all" | "macos";

export type UiSemanticAction = {
  readonly surface: Surface;
  readonly effect: Effect;
  readonly platform: Platform;
  /** A description is not permission to automate credentials or grant runtime access. */
  readonly humanOnly: boolean;
  readonly tool: UiMcpToolSchema;
};

function args<const Properties extends Record<string, Field>>(
  properties: Properties,
  required: readonly (keyof Properties & string)[] = [],
): UiToolInputSchema {
  return Object.freeze({
    type: "object",
    properties: Object.freeze(properties),
    required: Object.freeze([...required]),
    additionalProperties: false,
  });
}

const noArgs = args({});
const text = (maxLength?: number, minLength = 1): StringField =>
  Object.freeze({
    type: "string",
    minLength,
    ...(maxLength === undefined ? {} : { maxLength }),
  });
const choice = (options: readonly string[]): StringField =>
  Object.freeze({ type: "string", enum: Object.freeze([...options]) });
const flag = Object.freeze({ type: "boolean" }) satisfies BooleanField;
const ids = (): StringListField =>
  Object.freeze({
    type: "array",
    items: text(),
    uniqueItems: true,
  });

function action(
  id: string,
  surface: Surface,
  title: string,
  description: string,
  inputSchema: UiToolInputSchema = noArgs,
  effect: Effect = "local",
  platform: Platform = "all",
): UiSemanticAction {
  return Object.freeze({
    surface,
    effect,
    platform,
    humanOnly: effect === "write" || effect === "protected",
    tool: Object.freeze({
      name: `darbot_ui_${id}`,
      title,
      description,
      inputSchema,
      annotations: Object.freeze({
        readOnlyHint: effect === "read",
        destructiveHint: effect === "write" || effect === "protected",
        openWorldHint: effect === "write" || effect === "protected",
      }),
    }),
  });
}

export const WORKSPACE_SURFACES = Object.freeze([
  { id: "workspace", label: "Your workspace" },
  { id: "canvas", label: "Your canvas" },
  { id: "cli", label: "Your CLI" },
] as const);

/** Full UI declaration catalog, not runtime authority. app_control.rs advertises only its executed subset. */
export const DESKTOP_UI_ACTIONS = Object.freeze({
  settings_desktop_updates: action(
    "settings_desktop_updates",
    "settings",
    "Manage verified update notices",
    "Check authenticated GitHub build metadata, enable periodic checks, dismiss a notice or view the verified build. Does not install or grant remote update authority.",
    noArgs,
    "protected",
  ),
  settings_app_control: action(
    "settings_app_control",
    "settings",
    "Enable or revoke local app control",
    "Human-only native owner approval enables the private loopback service. Stopping revokes connections. Never callable through app control.",
    noArgs,
    "protected",
  ),
  settings_fleet: action(
    "settings_fleet",
    "workspace",
    "Manage authenticated fleet",
    "Review peer fingerprints, approved execution scopes and selected tasks in Settings. Native darbot_fleet_dispatch exposes the closed typed Fleet/1 actions; trust and task mutations require native approval and execution approval is desktop-only.",
    noArgs,
    "protected",
  ),
  history_refresh_sources: action(
    "history_refresh_sources",
    "history",
    "Refresh archive index",
    "Rescan configured local metadata sources. No session is opened.",
  ),
  harvest_decide: action(
    "harvest_decide",
    "history",
    "Update local review",
    "Toggle a human decision on the selected agent and session. Does not verify commits.",
    noArgs,
    "write",
  ),
  memory_copy: action(
    "memory_copy",
    "history",
    "Copy flashcard preview",
    "Copy a previewed, metadata-only reference to the clipboard.",
    noArgs,
    "write",
  ),
  memory_paste: action(
    "memory_paste",
    "history",
    "Paste flashcard",
    "Edit a local preview buffer; this does not dispatch a message.",
    noArgs,
    "write",
  ),
  memory_store: action(
    "memory_store",
    "history",
    "Manage local reference",
    "Save or remove an inert reference for the selected agent. Never execute it.",
    noArgs,
    "write",
  ),
  catalog_refresh: action(
    "catalog_refresh",
    "canvas",
    "Refresh agent catalog",
    "Read every configured Copilot Studio source with its pinned tenant/account, refresh private metadata indexes, and report failures without importing transcripts.",
    noArgs,
    "local",
  ),
  studio_copy_link: action(
    "studio_copy_link",
    "canvas",
    "Copy Copilot Studio link",
    "Copy a source-scoped agent URL. This does not start a local session, authenticate, or publish the cloud agent.",
    args({ agentId: text(1024) }, ["agentId"]),
    "local",
  ),
  window_set_widget: action(
    "window_set_widget",
    "workspace",
    "Change window mode",
    "Switch between the full workspace and a frameless floating widget without restarting the runtime.",
    args({ widget: flag }, ["widget"]),
  ),
  window_set_wide: action(
    "window_set_wide",
    "workspace",
    "Change widget shape",
    "Switch between a portrait widget and a compact 640 by 480 wide widget without remounting the workspace.",
    args({ wide: flag }, ["wide"]),
  ),
  window_set_pinned: action(
    "window_set_pinned",
    "workspace",
    "Pin widget",
    "Keep the widget above other windows or return it to normal stacking.",
    args({ pinned: flag }, ["pinned"]),
  ),
  window_flip: action(
    "window_flip",
    "workspace",
    "Flip widget",
    "Show the live agent tile or the chat workspace while preserving the conversation.",
    args({ front: flag }, ["front"]),
  ),
  window_hide: action(
    "window_hide",
    "workspace",
    "Hide to tray",
    "Hide the desktop window without stopping its agents; restore it from the tray.",
  ),
  window_drag: action(
    "window_drag",
    "workspace",
    "Move desktop window",
    "Drag the custom title bar to move the floating window.",
  ),
  window_resize: action(
    "window_resize",
    "workspace",
    "Resize widget",
    "Drag the corner grip to resize the floating widget.",
  ),
  workspace_inspect_agent: action(
    "workspace_inspect_agent",
    "workspace",
    "Inspect agent settings",
    "Open an agent's actions and conversation settings without selecting it or starting a session.",
    args({ agentId: text(undefined, 0) }, ["agentId"]),
  ),
  setup_begin: action(
    "setup_begin",
    "setup",
    "Begin setup",
    "Open the desktop setup flow.",
  ),
  setup_back: action(
    "setup_back",
    "setup",
    "Go back",
    "Return to the preceding setup screen.",
  ),
  setup_continue: action(
    "setup_continue",
    "setup",
    "Continue setup",
    "Confirm the current framework or provider choice and advance.",
  ),
  setup_framework_options: action(
    "setup_framework_options",
    "setup",
    "Show framework options",
    "Expand or collapse the agent framework choices.",
    args({ expanded: flag }, ["expanded"]),
  ),
  setup_select_framework: action(
    "setup_select_framework",
    "setup",
    "Choose framework",
    "Choose an agent framework and, when needed, its AG-UI endpoint.",
    args({ frameworkId: text(), agentUrl: text(undefined, 0) }, [
      "frameworkId",
    ]),
  ),
  setup_select_provider: action(
    "setup_select_provider",
    "setup",
    "Choose AI provider",
    "Select the provider for a local or Copilot-backed workspace.",
    args({ providerId: text() }, ["providerId"]),
  ),
  setup_select_login: action(
    "setup_select_login",
    "setup",
    "Choose sign-in method",
    "Switch between a provider's supported sign-in methods.",
    args({ method: choice(["plan", "api-key", "endpoint", "copilot"]) }, [
      "method",
    ]),
  ),
  setup_sign_in_provider: action(
    "setup_sign_in_provider",
    "setup",
    "Sign in to provider",
    "Start the provider-owned interactive browser sign-in.",
    args({ providerId: text() }, ["providerId"]),
    "protected",
  ),
  setup_finish_provider_sign_in: action(
    "setup_finish_provider_sign_in",
    "setup",
    "Finish provider sign-in",
    "Submit the browser's one-time code to complete provider sign-in.",
    args({ code: text() }, ["code"]),
    "protected",
  ),
  setup_reuse_provider_sign_in: action(
    "setup_reuse_provider_sign_in",
    "setup",
    "Reuse provider sign-in",
    "Choose an existing provider-owned plan session.",
    args({ providerId: text() }, ["providerId"]),
    "protected",
  ),
  setup_reuse_provider_key: action(
    "setup_reuse_provider_key",
    "setup",
    "Reuse saved provider key",
    "Choose an existing locally saved model credential without reading it into the schema.",
    args({ providerId: text() }, ["providerId"]),
    "protected",
  ),
  setup_drop_endpoint_key: action(
    "setup_drop_endpoint_key",
    "setup",
    "Ignore saved endpoint key",
    "Proceed without the saved key for this compatible endpoint.",
  ),
  setup_provider_key: action(
    "setup_provider_key",
    "setup",
    "Enter provider key",
    "Edit a provider API key in the local credential field; never register this as a remote tool.",
    args({ secret: text(undefined, 0) }, ["secret"]),
    "protected",
  ),
  setup_endpoint_url: action(
    "setup_endpoint_url",
    "setup",
    "Set compatible endpoint",
    "Edit the compatible model endpoint URL.",
    args({ url: text(undefined, 0) }, ["url"]),
  ),
  setup_container_endpoint_url: action(
    "setup_container_endpoint_url",
    "setup",
    "Set container endpoint",
    "Edit the endpoint URL visible from containers.",
    args({ url: text(undefined, 0) }, ["url"]),
  ),
  setup_model_name: action(
    "setup_model_name",
    "setup",
    "Set model name",
    "Edit the selected compatible endpoint model.",
    args({ model: text(undefined, 0) }, ["model"]),
  ),
  setup_intelligence_sign_in: action(
    "setup_intelligence_sign_in",
    "setup",
    "Sign in to Intelligence",
    "Start the managed Intelligence browser sign-in.",
    noArgs,
    "protected",
  ),
  setup_select_project: action(
    "setup_select_project",
    "setup",
    "Choose Intelligence project",
    "Choose the project for the managed Intelligence connection.",
    args({ projectId: text() }, ["projectId"]),
    "protected",
  ),
  setup_reuse_intelligence: action(
    "setup_reuse_intelligence",
    "setup",
    "Reuse Intelligence connection",
    "Use the existing managed Intelligence connection.",
    noArgs,
    "protected",
  ),
  setup_root: action(
    "setup_root",
    "setup",
    "Set deployment folder",
    "Edit the deployment root and reload the saved configuration for it.",
    args({ path: text(undefined, 0) }, ["path"]),
  ),
  setup_intelligence_key: action(
    "setup_intelligence_key",
    "setup",
    "Enter Intelligence key",
    "Edit a self-hosted Intelligence project key locally.",
    args({ secret: text(undefined, 0) }, ["secret"]),
    "protected",
  ),
  setup_intelligence_api: action(
    "setup_intelligence_api",
    "setup",
    "Set Intelligence API",
    "Edit the self-hosted Intelligence API URL.",
    args({ url: text(undefined, 0) }, ["url"]),
  ),
  setup_intelligence_gateway: action(
    "setup_intelligence_gateway",
    "setup",
    "Set Intelligence gateway",
    "Edit the self-hosted WebSocket gateway URL.",
    args({ url: text(undefined, 0) }, ["url"]),
  ),
  setup_change_provider: action(
    "setup_change_provider",
    "setup",
    "Change AI connection",
    "Return to AI provider selection.",
  ),
  setup_start_stack: action(
    "setup_start_stack",
    "setup",
    "Start deployment",
    "Prepare the local engine and start the configured deployment.",
    noArgs,
    "protected",
  ),
  setup_stop_stack: action(
    "setup_stop_stack",
    "setup",
    "Stop deployment",
    "Stop the active container-based deployment.",
    noArgs,
    "protected",
  ),
  setup_show_darbot: action(
    "setup_show_darbot",
    "setup",
    "Show running darbot",
    "Open the running deployment in the desktop window.",
    noArgs,
    "read",
  ),
  setup_ask: action(
    "setup_ask",
    "setup",
    "Verify Bot response",
    "Ask the connected Bot a question before handing over to its UI.",
    args({ question: text() }, ["question"]),
    "protected",
  ),
  setup_change_model: action(
    "setup_change_model",
    "setup",
    "Change model after failure",
    "Stop the unsuccessful deployment and return to model selection.",
    noArgs,
    "protected",
  ),
  inspect_details: action(
    "inspect_details",
    "workspace",
    "Show details",
    "Expand or collapse explanatory details in the current screen.",
    args({ section: text(256), expanded: flag }, ["section", "expanded"]),
    "read",
  ),
  open_external_link: action(
    "open_external_link",
    "setup",
    "Open external page",
    "Follow an explicitly visible help or sign-in link in the system browser.",
    args({ url: text() }, ["url"]),
    "protected",
  ),
  imports_search: action(
    "imports_search",
    "imports",
    "Search personal agents",
    "Filter the agent import catalog by name or description.",
    args({ query: text(undefined, 0) }, ["query"]),
  ),
  imports_select_shown: action(
    "imports_select_shown",
    "imports",
    "Select shown agents",
    "Select the currently visible, not-yet-imported personal agents.",
  ),
  imports_clear: action(
    "imports_clear",
    "imports",
    "Clear selection",
    "Clear pending agent imports.",
  ),
  imports_toggle_agent: action(
    "imports_toggle_agent",
    "imports",
    "Toggle agent selection",
    "Add or remove one personal agent from the import selection.",
    args({ agentId: text(), selected: flag }, ["agentId", "selected"]),
  ),
  imports_retry: action(
    "imports_retry",
    "imports",
    "Retry discovery",
    "Retry local Copilot agent discovery.",
    noArgs,
    "read",
  ),
  imports_back: action(
    "imports_back",
    "imports",
    "Leave import",
    "Return from agent import without saving.",
  ),
  imports_skip: action(
    "imports_skip",
    "imports",
    "Import none",
    "Finish agent import without adding agents.",
  ),
  imports_confirm: action(
    "imports_confirm",
    "imports",
    "Import selected agents",
    "Add selected agent references and link their conversation history.",
    args({ agentIds: ids() }, ["agentIds"]),
    "write",
  ),
  workspace_open_dialog: action(
    "workspace_open_dialog",
    "workspace",
    "Open workspace dialog",
    "Open Resources, History, Settings, Profile, Import, or Create agent.",
    args(
      {
        dialog: choice([
          "resources",
          "conversations",
          "settings",
          "profile",
          "import",
          "create-agent",
        ]),
      },
      ["dialog"],
    ),
  ),
  workspace_close_dialog: action(
    "workspace_close_dialog",
    "workspace",
    "Close workspace dialog",
    "Dismiss the current workspace dialog without applying pending form edits.",
  ),
  sidebar_select_agent: action(
    "sidebar_select_agent",
    "sidebar",
    "Select agent",
    "Select the agent for new conversations.",
    args({ agentId: text(undefined, 0) }, ["agentId"]),
  ),
  sidebar_toggle_chats: action(
    "sidebar_toggle_chats",
    "sidebar",
    "Toggle agent chats",
    "Expand or collapse the agent's conversations in the Layer 1 navigator.",
    args(
      {
        layerId: choice([SIDEPANE_AGENT_LAYER.id]),
        agentId: text(undefined, 0),
        expanded: flag,
      },
      ["layerId", "agentId", "expanded"],
    ),
  ),
  workspace_new_chat: action(
    "workspace_new_chat",
    "workspace",
    "Create chat draft",
    "Create a local draft for the selected agent without allocating a CLI session.",
    args({ agentId: text(undefined, 0) }, ["agentId"]),
  ),
  workspace_open_chat: action(
    "workspace_open_chat",
    "workspace",
    "Open chat",
    "Open a local draft or load its linked Copilot conversation.",
    args({ conversationId: text(128) }, ["conversationId"]),
    "write",
  ),
  workspace_initialize_model: action(
    "workspace_initialize_model",
    "workspace",
    "Discover conversation models",
    "Open the current local CLI conversation to negotiate its available model choices without sending a prompt or changing global defaults.",
    noArgs,
    "write",
  ),
  workspace_view_agent_history: action(
    "workspace_view_agent_history",
    "workspace",
    "View agent history",
    "Open the History dialog filtered to one agent.",
    args({ agentId: text(undefined, 0) }, ["agentId"]),
  ),
  workspace_create_agent: action(
    "workspace_create_agent",
    "workspace",
    "Create personal agent",
    "Write a new personal Copilot agent file without replacing existing files.",
    args(
      { name: text(64), description: text(1000), instructions: text(30000) },
      ["name", "description", "instructions"],
    ),
    "protected",
  ),
  workspace_edit_agent_name: action(
    "workspace_edit_agent_name",
    "workspace",
    "Edit agent name",
    "Edit the new agent's name before creating its personal agent file.",
    args({ name: text(64, 0) }, ["name"]),
  ),
  workspace_edit_agent_description: action(
    "workspace_edit_agent_description",
    "workspace",
    "Edit agent description",
    "Edit the new agent's description before creating its personal agent file.",
    args({ description: text(1000, 0) }, ["description"]),
  ),
  workspace_edit_agent_instructions: action(
    "workspace_edit_agent_instructions",
    "workspace",
    "Edit agent instructions",
    "Edit the new agent's instructions before creating its personal agent file.",
    args({ instructions: text(30000, 0) }, ["instructions"]),
  ),
  workspace_link_history: action(
    "workspace_link_history",
    "workspace",
    "Link agent conversations",
    "Read and link recorded Copilot conversations to imported agents.",
    args({ agentIds: ids() }, ["agentIds"]),
    "write",
  ),
  workspace_select_surface: action(
    "workspace_select_surface",
    "workspace",
    "Select surface",
    "Switch between Workspace, Canvas, and CLI tabs.",
    args({ surface: choice(WORKSPACE_SURFACES.map((entry) => entry.id)) }, [
      "surface",
    ]),
  ),
  workspace_edit_draft: action(
    "workspace_edit_draft",
    "workspace",
    "Edit chat draft",
    "Change the locally persisted message draft.",
    args({ text: text(MAX_DRAFT_LENGTH, 0) }, ["text"]),
  ),
  workspace_attachment_menu: action(
    "workspace_attachment_menu",
    "workspace",
    "Add context",
    "Open local attachment choices. Not remotely executable.",
    noArgs,
    "protected",
  ),
  workspace_upload: action(
    "workspace_upload",
    "workspace",
    "Upload selected files",
    "Choose exact PNG or UTF-8 files using the native picker. Human only.",
    noArgs,
    "protected",
  ),
  workspace_select_context: action(
    "workspace_select_context",
    "workspace",
    "Select context",
    "Review selected text or exact files for this conversation. Human only.",
    noArgs,
    "protected",
  ),
  workspace_capture: action(
    "workspace_capture",
    "workspace",
    "Capture Screen",
    "Select and approve one native window snapshot locally; never sends automatically.",
    noArgs,
    "protected",
  ),
  workspace_share_screen: action(
    "workspace_share_screen",
    "workspace",
    "Share Screen",
    "Start a bounded local preview of an explicitly approved window. Not video transmission.",
    noArgs,
    "protected",
  ),
  workspace_capture_source: action(
    "workspace_capture_source",
    "workspace",
    "Choose capture source",
    "Select a live opaque source and approve native window capture. Human only.",
    noArgs,
    "protected",
  ),
  workspace_capture_stop: action(
    "workspace_capture_stop",
    "workspace",
    "Stop sharing",
    "Revoke screen capture and discard staged screen images.",
    noArgs,
    "protected",
  ),
  workspace_attachment_remove: action(
    "workspace_attachment_remove",
    "workspace",
    "Remove attachment",
    "Remove one scoped native attachment and its grant.",
    noArgs,
    "protected",
  ),
  workspace_attachment_cancel: action(
    "workspace_attachment_cancel",
    "workspace",
    "Cancel context selection",
    "Cancel human context selection without sending a turn.",
    noArgs,
    "protected",
  ),
  workspace_send_prompt: action(
    "workspace_send_prompt",
    "workspace",
    "Send Copilot prompt",
    "Send the current draft to the selected Copilot conversation.",
    args({ text: text(MAX_DRAFT_LENGTH) }, ["text"]),
    "protected",
  ),
  workspace_cancel_prompt: action(
    "workspace_cancel_prompt",
    "workspace",
    "Stop response",
    "Cancel the currently running Copilot turn.",
    noArgs,
    "protected",
  ),
  workspace_jump_latest: action(
    "workspace_jump_latest",
    "workspace",
    "Jump to newest message",
    "Scroll the conversation to its newest message.",
  ),
  workspace_scroll_messages: action(
    "workspace_scroll_messages",
    "workspace",
    "Scroll messages",
    "Scroll the message list and update follow-new-messages state.",
  ),
  workspace_retry_chat: action(
    "workspace_retry_chat",
    "workspace",
    "Retry opening conversation",
    "Retry loading the selected recorded conversation without resubmitting the draft.",
    noArgs,
    "write",
  ),
  workspace_insert_resource: action(
    "workspace_insert_resource",
    "workspace",
    "Insert skill or command",
    "Insert a locally discovered skill or allowed command into the composer.",
    args({ name: text() }, ["name"]),
  ),
  workspace_filter_fleet: action(
    "workspace_filter_fleet",
    "workspace",
    "Find an agent",
    "Filter agent cards by name, ID or environment.",
    args({ query: text(undefined, 0) }, ["query"]),
  ),
  fleet_page: action(
    "fleet_page",
    "canvas",
    "Change fleet page",
    "Page through matching AgentCards.",
  ),
  fleet_copy_json: action(
    "fleet_copy_json",
    "canvas",
    "Copy card JSON",
    "Copy the displayed AgentCard instance or schema metadata.",
    args({}),
    "local",
  ),
  workspace_filter_resources: action(
    "workspace_filter_resources",
    "workspace",
    "Filter resources",
    "Filter locally discovered skills and commands.",
    args({ query: text(undefined, 0) }, ["query"]),
  ),
  settings_theme: action(
    "settings_theme",
    "settings",
    "Set theme",
    "Choose dark, light, or system appearance.",
    args({ theme: choice(["dark", "light", "system"]) }, ["theme"]),
  ),
  settings_auto_scroll: action(
    "settings_auto_scroll",
    "settings",
    "Set automatic scroll",
    "Choose whether new messages automatically scroll into view.",
    args({ enabled: flag }, ["enabled"]),
  ),
  settings_snap_scroll: action(
    "settings_snap_scroll",
    "settings",
    "Set message snapping",
    "Choose whether completed messages snap into view when scrolling.",
    args({ enabled: flag }, ["enabled"]),
  ),
  settings_folder: action(
    "settings_folder",
    "settings",
    "Set working folder",
    "Edit or apply the Copilot working folder.",
    args({ path: text(undefined, 0) }, ["path"]),
    "write",
  ),
  settings_browse_folder: action(
    "settings_browse_folder",
    "settings",
    "Browse working folder",
    "Open the system folder picker for a Copilot working folder.",
    noArgs,
    "protected",
  ),
  settings_reset_folder: action(
    "settings_reset_folder",
    "settings",
    "Use home folder",
    "Reset the Copilot working folder to the home directory.",
    noArgs,
    "write",
  ),
  settings_configure_session: action(
    "settings_configure_session",
    "settings",
    "Configure Copilot session",
    "Change an option advertised by the active Copilot session.",
    args(
      {
        option: choice(["model", "reasoning_effort", "mode"]),
        value: text(),
      },
      ["option", "value"],
    ),
    "write",
  ),
  settings_open_setup: action(
    "settings_open_setup",
    "settings",
    "Open deployment setup",
    "Leave the Copilot workspace for container deployment setup.",
  ),
  history_select_agent: action(
    "history_select_agent",
    "history",
    "Filter history by agent",
    "Filter conversation history to one agent or all agents.",
    args({ agentId: text(undefined, 0) }, ["agentId"]),
  ),
  history_scope_folders: action(
    "history_scope_folders",
    "history",
    "Set history folder scope",
    "Include all working folders or only the current one.",
    args({ allFolders: flag }, ["allFolders"]),
    "read",
  ),
  history_search: action(
    "history_search",
    "history",
    "Search history",
    "Filter conversation titles, folders, and agents.",
    args({ query: text(undefined, 0) }, ["query"]),
  ),
  history_load_more: action(
    "history_load_more",
    "history",
    "Load more history",
    "Reveal the next page of indexed conversations.",
    noArgs,
    "read",
  ),
  workspace_permission_decide: action(
    "workspace_permission_decide",
    "workspace",
    "Answer permission request",
    "Submit an exact Copilot-offered option or reject a pending permission; human-only.",
    args(
      {
        requestId: text(),
        optionId: { type: ["string", "null"] },
      },
      ["requestId", "optionId"],
    ),
    "protected",
  ),
  sdk_memory: action(
    "sdk_memory",
    "sdk",
    "Set runtime memory choice",
    "Choose whether optional Copilot SDK memory starts for the background workspace.",
    args({ enabled: flag }, ["enabled"]),
  ),
  sdk_connect: action(
    "sdk_connect",
    "sdk",
    "Connect SDK workspace",
    "Connect the optional SDK workspace using the current working folder.",
    args({ memoryEnabled: flag }, ["memoryEnabled"]),
    "protected",
  ),
  sdk_refresh: action(
    "sdk_refresh",
    "sdk",
    "Refresh SDK workspace",
    "Read the background agents, tasks, and plugins.",
    noArgs,
    "read",
  ),
  sdk_list_remote: action(
    "sdk_list_remote",
    "sdk",
    "Discover remote sessions",
    "Read remote Copilot session metadata.",
    noArgs,
    "read",
  ),
  sdk_disconnect: action(
    "sdk_disconnect",
    "sdk",
    "Disconnect SDK workspace",
    "End the optional background SDK connection.",
    noArgs,
    "protected",
  ),
  sdk_select_task_agent: action(
    "sdk_select_task_agent",
    "sdk",
    "Choose task agent",
    "Choose the background agent for a new SDK task.",
    args({ agentId: text() }, ["agentId"]),
  ),
  sdk_set_task_name: action(
    "sdk_set_task_name",
    "sdk",
    "Edit task name",
    "Edit the new SDK task name.",
    args({ name: text(128, 0) }, ["name"]),
  ),
  sdk_set_task_prompt: action(
    "sdk_set_task_prompt",
    "sdk",
    "Edit task prompt",
    "Edit the new SDK task prompt.",
    args({ prompt: text(16000, 0) }, ["prompt"]),
  ),
  sdk_start_task: action(
    "sdk_start_task",
    "sdk",
    "Start background task",
    "Start a Copilot SDK task with the chosen agent and prompt.",
    args(
      {
        agentId: text(),
        name: text(128),
        prompt: text(16000),
      },
      ["agentId", "name", "prompt"],
    ),
    "protected",
  ),
  sdk_cancel_task: action(
    "sdk_cancel_task",
    "sdk",
    "Cancel background task",
    "Cancel the selected running SDK task.",
    args({ taskId: text() }, ["taskId"]),
    "protected",
  ),
  sdk_toggle_plugin: action(
    "sdk_toggle_plugin",
    "sdk",
    "Change plugin availability",
    "Enable or disable a toggleable marketplace plugin.",
    args({ pluginId: text(), enabled: flag }, ["pluginId", "enabled"]),
    "protected",
  ),
  menu_open: action(
    "menu_open",
    "menu",
    "Open darbot window",
    "Restore the current desktop view from the native menu.",
    noArgs,
    "read",
  ),
  menu_stop: action(
    "menu_stop",
    "menu",
    "Stop from native menu",
    "Stop the running deployment from the native menu.",
    noArgs,
    "protected",
  ),
  menu_quit: action(
    "menu_quit",
    "menu",
    "Quit desktop",
    "Quit the desktop app, stopping its owned processes.",
    noArgs,
    "protected",
  ),
  menu_hide: action(
    "menu_hide",
    "menu",
    "Hide desktop window",
    "Hide the window when it is closed while leaving the tray app available.",
  ),
  menu_undo: action(
    "menu_undo",
    "menu",
    "Undo edit",
    "Undo the most recent native text edit on macOS.",
    noArgs,
    "local",
    "macos",
  ),
  menu_redo: action(
    "menu_redo",
    "menu",
    "Redo edit",
    "Redo a previously undone native text edit on macOS.",
    noArgs,
    "local",
    "macos",
  ),
  menu_cut: action(
    "menu_cut",
    "menu",
    "Cut selection",
    "Cut selected text to the system clipboard on macOS.",
    noArgs,
    "protected",
    "macos",
  ),
  menu_copy: action(
    "menu_copy",
    "menu",
    "Copy selection",
    "Copy selected text to the system clipboard on macOS.",
    noArgs,
    "protected",
    "macos",
  ),
  menu_paste: action(
    "menu_paste",
    "menu",
    "Paste clipboard",
    "Paste system clipboard text into the active field on macOS.",
    noArgs,
    "protected",
    "macos",
  ),
  menu_select_all: action(
    "menu_select_all",
    "menu",
    "Select all text",
    "Select the active field's text on macOS.",
    noArgs,
    "local",
    "macos",
  ),
} satisfies Record<string, UiSemanticAction>);

export type DesktopUiActionId = keyof typeof DESKTOP_UI_ACTIONS;

export const DESKTOP_SEMANTIC_MODEL = Object.freeze({
  schemaVersion: 1,
  sidepane: COPILOT_WORKSPACE_SIDEPANE,
  surfaces: WORKSPACE_SURFACES,
  actions: DESKTOP_UI_ACTIONS,
  mcpTools: Object.freeze(
    Object.values(DESKTOP_UI_ACTIONS).map(({ tool }) => tool),
  ),
});
