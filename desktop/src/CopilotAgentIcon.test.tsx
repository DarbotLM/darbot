import { afterAll, afterEach, beforeAll, expect, spyOn, test } from "bun:test";
import { GlobalRegistrator } from "@happy-dom/global-registrator";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { readFileSync } from "node:fs";
import { AgentAttributeIcon, agentArtworkSeed, CopilotAgentIcon } from "./CopilotAgentIcon";
import { identityImageSource, swarmIdentities } from "../../shared/swarm-identity";

beforeAll(() => GlobalRegistrator.register({ url: "http://tauri.localhost" }));
afterEach(cleanup);
afterAll(() => GlobalRegistrator.unregister());

test("Darbot role artwork matches existing role identity without changing the agent ID", () => {
  for (const role of ["architect", "analyst", "azure", "security", "finops", "researcher", "markdown"]) {
    expect(agentArtworkSeed(`darbot-${role}`)).toBe(agentArtworkSeed(`dayour-${role}`));
  }
  expect(agentArtworkSeed("darbot-swe")).toBe("github_engineer");
  expect(agentArtworkSeed("darbot-docs-guide")).toBe("sharepoint_knowledge");
  expect(agentArtworkSeed("constructor")).toBe("constructor");
  expect(agentArtworkSeed("unmapped-private-role")).toBe("unmapped-private-role");
});

test("unknown custom agents have visible local glyphs without fetching external images", () => {
  const view = render(<CopilotAgentIcon agent={{ id: "unmapped-review", name: "Custom Reviewer" }} />);
  expect(view.container.querySelector("img")).toBeNull();
  expect(view.container.querySelector("svg[data-agent-artwork='glyph'] text")?.textContent).toBe("CR");
});

test("failed role artwork retains its canonical color and identity and recovers on agent change", () => {
  const warning = spyOn(console, "warn").mockImplementation(() => {});
  try {
    const view = render(<CopilotAgentIcon agent={{ id: "darbot-architect", name: "Architect" }} />);
    const image = view.container.querySelector("img");
    if (!image) throw new Error("Known role must select its bundled artwork");
    const code = image.getAttribute("data-swarm-identity");
    const identity = swarmIdentities.find((item) => item.identityCode === code);
    if (!identity) throw new Error("Role artwork must have canonical identity");
    fireEvent.error(image);
    const glyph = view.container.querySelector("svg");
    expect(glyph?.getAttribute("data-swarm-identity")).toBe(code);
    expect(glyph?.querySelector("rect")?.getAttribute("stroke")).toBe(identity.fallbackHex);
    expect(warning).toHaveBeenCalledTimes(1);
    view.rerender(<CopilotAgentIcon agent={{ id: "darbot-security", name: "Security" }} />);
    expect(view.container.querySelector("img")).not.toBeNull();
  } finally {
    warning.mockRestore();
  }
});

test("provider badge image failure produces a readable local glyph", () => {
  const warning = spyOn(console, "warn").mockImplementation(() => {});
  try {
    const view = render(<AgentAttributeIcon name="Claude" />);
    const image = view.container.querySelector("img");
    if (!image) throw new Error("Claude badge must select bundled artwork");
    fireEvent.error(image);
    expect(view.container.querySelector("svg text")?.textContent).toBe("C");
  } finally {
    warning.mockRestore();
  }
});

test("every canonical desktop avatar references a real PNG source asset", () => {
  expect(swarmIdentities.length).toBe(128);
  for (const identity of swarmIdentities) {
    const source = identityImageSource(identity, 32, "avatar");
    expect(source.startsWith("/assets/swarm/")).toBe(true);
    const bytes = readFileSync(new URL(`../../${source.slice(1)}`, import.meta.url));
    expect([...bytes.subarray(0, 8)]).toEqual([137, 80, 78, 71, 13, 10, 26, 10]);
  }
});
