import { expect, test } from "bun:test";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { isMap, isSeq, parseDocument } from "yaml";

const root = fileURLToPath(new URL("../../", import.meta.url));

function read(...parts: string[]): string {
  return readFileSync(join(root, ...parts), "utf8");
}

function version(...parts: string[]): string {
  const value: unknown = JSON.parse(read(...parts));
  if (
    !value ||
    typeof value !== "object" ||
    !("version" in value) ||
    typeof value.version !== "string"
  ) {
    throw new Error(`${parts.join("\\")} has no valid version.`);
  }
  return value.version;
}

test("release checks call the real local reusable CI workflow", () => {
  const workflow = parseDocument(
    read(".github", "workflows", "publish-release.yml"),
  );
  const ci = parseDocument(read(".github", "workflows", "ci.yml"));
  expect(workflow.errors).toEqual([]);
  expect(ci.errors).toEqual([]);
  expect(workflow.getIn(["jobs", "checks", "uses"])).toBe(
    "./.github/workflows/ci.yml",
  );
  expect(existsSync(join(root, ".github", "workflows", "ci.yml"))).toBe(true);
  expect(ci.hasIn(["on", "workflow_call"])).toBe(true);
});

test("native release versions and chart metadata remain aligned", () => {
  const expected = version("package.json");
  expect(expected).toMatch(/^\d+\.\d+\.\d+$/);
  expect(version("desktop", "package.json")).toBe(expected);
  expect(version("desktop", "src-tauri", "tauri.conf.json")).toBe(expected);
  const cargo = read("desktop", "src-tauri", "Cargo.toml");
  expect(cargo.match(/^\[package\][\s\S]*?\r?\nversion = "([^"]+)"/)?.[1]).toBe(
    expected,
  );
  const lock = read("desktop", "src-tauri", "Cargo.lock");
  expect(
    lock.match(
      /\[\[package\]\]\r?\nname = "darbot-desktop"\r?\nversion = "([^"]+)"/,
    )?.[1],
  ).toBe(expected);
  const chart = parseDocument(read("charts", "darbot", "Chart.yaml"));
  expect(chart.errors).toEqual([]);
  expect(chart.get("appVersion")).toBe(expected);
  expect(read("CHANGELOG.md").split(/\r?\n/)).toContain(`## ${expected}`);
});

test("CI and native packaging retain push triggers and support explicit verification", () => {
  for (const name of ["ci.yml", "desktop.yml"]) {
    const workflow = parseDocument(read(".github", "workflows", name));
    expect(workflow.errors).toEqual([]);
    expect(workflow.hasIn(["on", "workflow_dispatch"])).toBe(true);
    expect(workflow.hasIn(["on", "workflow_call"])).toBe(true);
    expect(workflow.getIn(["on", "push", "branches", 0])).toBe("main");
    expect(workflow.getIn(["permissions", "contents"])).toBe("read");
  }
  const desktop = parseDocument(read(".github", "workflows", "desktop.yml"));
  const steps = desktop.getIn(["jobs", "app", "steps"], true);
  expect(isSeq(steps)).toBe(true);
  if (!isSeq(steps)) throw new Error("Desktop app steps are missing.");
  expect(
    steps.items.some(
      (step) => isMap(step) && step.get("run") === "bun test ./src",
    ),
  ).toBe(true);
});

test("Windows release packaging signs and verifies both deliverables", () => {
  const desktop = parseDocument(read(".github", "workflows", "desktop.yml"));
  expect(desktop.errors).toEqual([]);
  const steps = desktop.getIn(["jobs", "app", "steps"], true);
  expect(isSeq(steps)).toBe(true);
  if (!isSeq(steps)) throw new Error("Desktop app steps are missing.");
  const runs = steps.items
    .filter(isMap)
    .map((step) => `${step.get("name") ?? ""}\n${step.get("run") ?? ""}`)
    .join("\n");
  expect(runs).toContain("WINDOWS_SIGNING_PFX_BASE64");
  expect(runs).toContain("/tr");
  expect(runs).toContain("darbot-desktop.exe");
  expect(runs).toContain("bundle/nsis");
  expect(runs).toContain("Get-AuthenticodeSignature");
  expect(runs).toContain("SHA256SUMS.txt");

  const release = parseDocument(
    read(".github", "workflows", "publish-release.yml"),
  );
  expect(release.errors).toEqual([]);
  expect(release.getIn(["jobs", "desktop", "uses"])).toBe(
    "./.github/workflows/desktop.yml",
  );
  expect(release.getIn(["jobs", "desktop", "secrets"])).toBe("inherit");
  const verificationNeeds = release.getIn(
    ["jobs", "windows-verify", "needs"],
    true,
  );
  expect(isSeq(verificationNeeds)).toBe(true);
  if (!isSeq(verificationNeeds))
    throw new Error("Windows verification needs are missing.");
  expect(
    verificationNeeds.items.some((item) => String(item) === "desktop"),
  ).toBe(true);
  expect(release.getIn(["jobs", "windows-verify", "runs-on"])).toBe(
    "windows-latest",
  );
  const releaseNeeds = release.getIn(["jobs", "github-release", "needs"], true);
  expect(isSeq(releaseNeeds)).toBe(true);
  if (!isSeq(releaseNeeds))
    throw new Error("GitHub release needs are missing.");
  expect(
    releaseNeeds.items.some((item) => String(item) === "windows-verify"),
  ).toBe(true);
});
