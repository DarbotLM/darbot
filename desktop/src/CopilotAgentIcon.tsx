import {
  identityImageSource,
  resolveAgentIdentity,
} from "../../shared/swarm-identity";
import type { CopilotAgentSummary } from "./copilot-types";

const PERSONAL_AGENT_IDENTITIES: Readonly<Record<string, string>> = {
  "dayour-azure": "azure_architect",
  "dayour-entra": "entra_identity",
  "dayour-teams": "teams_collaboration",
  "dayour-pplat": "power_platform_maker",
  "dayour-analyst": "power_bi_analyst",
  "dayour-analytics": "power_bi_analyst",
  "dayour-m365": "m365_strategist",
  "dayour-copilot": "m365_strategist",
  "dayour-studio": "copilot_studio_agent_builder",
  "dayour-agentbuilder": "copilot_studio_agent_builder",
  "dayour-bat": "copilot_studio_agent_builder",
  "dayour-ppt": "powerpoint_storyteller",
  "dayour-word": "word_writer",
  "dayour-sharepoint": "sharepoint_knowledge",
  "dayour-onedrive": "onedrive_sync",
  "dayour-dev": "vscode_developer",
  "dayour-dataverse": "dataverse_modeler",
  "dayour-fabric": "fabric_data_engineer",
  "dayour-data-eng": "fabric_data_engineer",
  "dayour-foundry": "azure_ai_foundry",
  "dayour-integration": "apim_integration",
  "dayour-design": "designer_creator",
  "dayour-swe": "github_engineer",
  "darbot-swe": "github_engineer",
  "dayour-github": "github_engineer",
  "dayour-ado": "devops_release",
  "dayour-pipeline": "devops_release",
  "dayour-gitops": "devops_release",
  "dayour-graph": "graph_connector",
  "dayour-security": "defender_security",
  "dayour-purview": "purview_governance",
  "dayour-intune": "intune_endpoint",
  "dayour-viva": "viva_employee_experience",
  "dayour-pmo": "planner_coordination",
  "dayour-dynamics": "dynamics_customer",
  "dayour-finops": "finance_ops",
  "dayour-researcher": "research_scientist",
  "dayour-architect": "principal_architect",
  "dayour-swarm": "agent_swarm_orchestrator",
  dayswarm: "agent_swarm_orchestrator",
  "dayour-test": "graphite_reviewer",
  "dayour-qa": "graphite_reviewer",
  "dayour-evaluation": "graphite_reviewer",
  "dayour-sre": "observability_sre",
  "dayour-network": "network_edge",
  "dayour-notes": "word_writer",
  "dayour-markdown": "word_writer",
  "dayour-flashcard": "sharepoint_knowledge",
  "darbot-docs-guide": "sharepoint_knowledge",
  "darbot-workspace-guide": "prompt_engineer",
};

const defaultIcon = new URL("./marks/copilot.svg", import.meta.url).href;

const claudeIcon = new URL("./marks/claude-agent-sdk.svg", import.meta.url)
  .href;
const anthropicIcon = new URL("./marks/anthropic.svg", import.meta.url).href;
const openaiIcon = new URL("./marks/openai.svg", import.meta.url).href;

function labelKey(name: string) {
  return name
    .normalize("NFKC")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, " ")
    .trim();
}

export function agentArtworkSeed(id: string): string {
  const alias = labelKey(id).replaceAll(" ", "-");
  const mapped = (key: string) => Object.hasOwn(PERSONAL_AGENT_IDENTITIES, key)
    ? PERSONAL_AGENT_IDENTITIES[key] : undefined;
  return mapped(alias) ??
    (alias.startsWith("darbot-")
      ? mapped(`dayour-${alias.slice("darbot-".length)}`)
      : undefined) ?? id;
}

function AgentImage({
  src, label, className, identityCode, color = "#217346", style,
}: {
  src?: string;
  label: string;
  className?: string;
  identityCode?: string;
  color?: string;
  style?: CSSProperties;
}) {
  const [failedSource, setFailedSource] = useState<string | null>(null);
  const words = label.replace(/^(?:darbot|dayour)[-_ ]+/i, "").trim().split(/[\s_-]+/);
  const glyph = (identityCode?.split("-").at(-1) ??
    words.slice(0, 2).map((word) => Array.from(word)[0] ?? "").join("").toUpperCase()) || "D";
  if (!src || failedSource === src) {
    return <svg className={className} data-swarm-identity={identityCode}
      data-agent-artwork="glyph" viewBox="0 0 32 32" style={style}
      aria-hidden="true" focusable="false">
      <rect x="1" y="1" width="30" height="30" rx="6" fill="#101820" stroke={color} strokeWidth="2" />
      <path d="M3 23h26v3a3 3 0 0 1-3 3H6a3 3 0 0 1-3-3z" fill={color} />
      <text x="16" y="20" fill="#f2f5f4" textAnchor="middle" fontSize="13"
        fontWeight="700" fontFamily="system-ui, sans-serif">{glyph}</text>
    </svg>;
  }
  return <img className={className} data-swarm-identity={identityCode} src={src}
    alt="" aria-hidden="true" style={style}
    onError={() => {
      console.warn("Darbot agent artwork could not be loaded; displaying its local identity glyph.");
      setFailedSource(src);
    }} />;
}

function nameBadge(name: string) {
  const key = labelKey(name);
  if (key.replaceAll(" ", "").includes("studio")) return "Copilot Studio";
  if (
    /\bclaude(?:agent|code|sdk|sonnet|opus|haiku|\d|\b)/.test(key) ||
    /\bclaude\b/.test(labelKey(name.replace(/([a-z])([A-Z])/g, "$1 $2")))
  )
    return "Claude";
  return null;
}

function attributeMark(name: string): {
  label: string;
  glyph?: string;
  src?: string;
} {
  const badge = nameBadge(name);
  if (badge === "Copilot Studio") {
    const identity = resolveAgentIdentity({
      id: "copilot_studio_agent_builder",
      name: "Copilot Studio Agent Builder",
      avatarSeed: "copilot_studio_agent_builder",
    });
    return identity
      ? { label: badge, src: identityImageSource(identity, 32, "avatar") }
      : { label: badge, glyph: "CS" };
  }
  if (badge === "Claude") return { label: badge, src: claudeIcon };
  const key = labelKey(name);
  if (/\banthropic\b/.test(key))
    return { label: "Anthropic", src: anthropicIcon };
  if (/\bopen ?ai\b/.test(key)) return { label: "OpenAI", src: openaiIcon };
  if (/\b(?:github ?copilot|copilot ?cli)\b/.test(key))
    return { label: "GitHub Copilot", src: defaultIcon };
  switch (key) {
    case "workflow":
      return { label: name, glyph: "↳" };
    case "canvas app":
      return { label: name, glyph: "CA" };
    case "model driven app":
      return { label: name, glyph: "MD" };
    case "tools":
    case "tool":
      return { label: name, glyph: "{}" };
    case "chats":
    case "chat":
      return { label: name, glyph: "…" };
    default:
      return { label: name.trim() || "Attribute", glyph: "#" };
  }
}

// The caller renders the actual attribute text; don't announce it twice.
export function AgentAttributeIcon({
  name,
  className,
}: {
  name: string;
  className?: string;
}) {
  const { label, src, glyph } = attributeMark(name);
  return (
    <span
      className={
        className ? `agent-attribute-icon ${className}` : "agent-attribute-icon"
      }
      aria-hidden="true"
      title={label}
      style={{
        display: "inline-grid",
        placeItems: "center",
        verticalAlign: "middle",
        width: "1.5em",
        height: "1.5em",
        flex: "none",
        lineHeight: 1,
        fontWeight: 700,
        borderRadius: "0.25em",
        backgroundColor: src ? "#fff" : "var(--sunk)",
        color: "var(--ink)",
      }}
    >
      {src ? (
        <AgentImage src={src} label={label} style={{ width: "1.15em", height: "1.15em" }} />
      ) : (
        glyph
      )}
    </span>
  );
}

export function CopilotAgentIcon({ agent }: { agent: CopilotAgentSummary }) {
  const identity = resolveAgentIdentity({
    id: agent.id,
    name: agent.name,
    avatarSeed: agentArtworkSeed(agent.id),
  });
  const badge =
    nameBadge(agent.name) ??
    nameBadge(agent.id) ??
    (agent.source?.provider === "copilot-studio" ? "Copilot Studio" : null);
  const avatar = (
    <AgentImage
      label={agent.name || "Copilot"}
      className={`copilot-agent-icon${!identity && agent.id === "" ? " copilot-agent-icon-default" : ""}`}
      identityCode={identity?.identityCode}
      color={identity?.fallbackHex}
      src={
        identity
          ? identityImageSource(identity, 32, "avatar")
          : agent.id === ""
            ? defaultIcon
            : undefined
      }
    />
  );
  if (!badge) return avatar;
  return (
    <span
      className="copilot-agent-icon"
      aria-hidden="true"
      style={{
        position: "relative",
        display: "inline-grid",
        placeItems: "center",
      }}
    >
      {identity && avatar}
      <span
        style={
          identity
            ? {
                position: "absolute",
                bottom: -2,
                right: -2,
                fontSize: "0.62rem",
              }
            : undefined
        }
      >
        <AgentAttributeIcon name={badge} />
      </span>
    </span>
  );
}
import { useState, type CSSProperties } from "react";
