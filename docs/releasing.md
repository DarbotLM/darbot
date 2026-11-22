# Releasing

A release is one person choosing a version, and a reviewed pull request doing everything else. No
step involves a terminal, a tag pushed by hand, or an image built on somebody's laptop.

## Cutting one

**Windows desktop packaging:** from a trusted release checkout, run
`.\scripts\toolbox.ps1 Package -Offline` to build the optimized x64 NSIS installer in
a fresh target directory without running regression suites. The script prints the
installer path, version, size and SHA-256. `-Offline` applies to Cargo; Bun and the
bundler may need their caches populated. `Build` also runs the desktop regression
suites; neither mode installs, signs, tags or publishes the package.

On a new Windows x64 node, use the toolbox and installer from the *same trusted
release*. Run `.\toolbox.ps1 Doctor`, then `.\toolbox.ps1 Prerequisites -For Runtime`.
Exit Darbot before using `.\toolbox.ps1 Install -InstallerPath <local-installer> -ExpectedVersion <version> -ExpectedSha256 <trusted-installer-digest> -ExpectedExeSha256 <trusted-installed-executable-digest>`.
Use `.\toolbox.ps1 Launch` from a non-elevated shell. Presence in Doctor is not proof
of provider authentication; use the node user's existing supported sign-in or official
consent flow, never copied tokens.

### EXE and MSIX production packages

Run the existing PowerShell toolbox from a short local checkout path to stay
within Windows linker path limits:

```powershell
.\scripts\toolbox.ps1 Package -Offline -WithMsix -AllowUnsignedMsix
```

`Package` builds the optimized NSIS EXE installer without running regression
suites; `Build` includes them. Both require an absent `desktop\dist` and allocate
a fresh Cargo target. `-Offline` applies to Cargo, not Bun or bundler downloads.

To package an already built executable without rebuilding:

```powershell
.\scripts\toolbox.ps1 Msix -ApplicationPath $builtExe `
  -ExpectedVersion $version -ExpectedExeSha256 $trustedRawExeHash `
  -AllowUnsignedMsix
```

MSIX packaging uses Windows SDK MakeAppx with validation enabled and leaves the
input executable unchanged. It requires WebView2 on the destination machine.
An unsigned MSIX is an artifact for signing, not a generally install-ready
package. Production signing requires an existing CurrentUser code-signing
certificate matching `-MsixPublisher`, supplied with
`-MsixCertificateThumbprint`; the toolbox never creates certificates or changes
trust stores. `Install` continues to install the verified NSIS EXE, not MSIX.

Publish distinct hashes for the installer, raw executable and MSIX. Tauri's
installed NSIS executable may differ from the raw executable only by its standard
bundle marker; verify the installed hash separately. Never overwrite a previously
published binary with another build carrying the same version.

The toolbox requires prior `desktop\dist` output to be moved aside before a build;
it preserves Tauri's normal relative embedded-asset configuration. `BuiltExeSHA256`
identifies the raw build output, not necessarily the installed payload: the NSIS
bundle marker can differ. The release owner must independently verify and publish
both the installer digest and the installed executable digest. A local unsigned
installer is not a signed or attested release.

### Windows fleet rollout and post-install review

Use the same versioned installer and verified digests on every node. Install as
the node's existing non-elevated Windows user, then launch in that user's
interactive desktop session; an SSH session alone is not a visible desktop.
`Launch` activates a receipt-verified, same-user/session tray-hidden instance
rather than starting an unrelated replacement.

After installation:

1. Confirm the compact composer renders and the CLI loads the intended personal
   profile. Do not inherit another application's process-local `COPILOT_HOME`.
2. Open Settings, expand Fleet, and configure a reachable interface IP and fixed
   port. Darbot does not create firewall rules or expose local app-control MCP.
3. Enable the fleet through native consent. Exchange public pairing cards on both
   desktops and compare their certificate fingerprints using a trusted channel.
   Matching hostnames or usernames are not authentication.
4. Approve an existing local repository root, agent and exact available model on
   each executor. Approve a harmless shared task before trying code-changing jobs.
5. Exercise two-way task status/results, round-robin selection, offline recovery,
   restart, cancellation and peer revocation. A reconciliation-required outcome
   must never automatically repeat a potentially side-effecting prompt.
6. Inspect each installed UI and startup logs. Confirm screenshot cancellation
   releases its owned worker, and that an expired native approval cannot permit
   capture. Keep screenshots and task contents out of public release evidence.

Local app control (`darbot_ui_dispatch` / `darbot_fleet_dispatch`) and the
authenticated peer A2A listener are separate services. Installing the release
does not silently approve peers, execution profiles, tasks or credentials.
The separate repository inventory/SMB `repo_toolbox.ps1` is not this desktop
installer; its actions are not automatically run during rollout.

1. Check `## Unreleased` in [CHANGELOG.md](../CHANGELOG.md) reads the way you want it to. It is the
   release notes. Nothing is generated from commit subjects, because a commit subject is written for
   the person reading the diff and these notes are for the person deciding whether to upgrade.
2. Run **Create release PR** from the Actions tab, choosing `patch`, `minor` or `major`. Use
   `dry_run` first if you want to see the version and the notes without opening anything.
3. Review the pull request it opens. It contains exactly three changes: the version in `package.json`,
   the `## Unreleased` heading becoming `## X.Y.Z`, and the Helm chart's `appVersion` moving to the
   same number so a default `helm install` pulls the image this release builds.
4. Merge it. That is the publish.

Merging is the trigger, so a release is always a reviewed commit on `main`.

## What merging does

`publish-release.yml` runs on every push to `main` and starts by deciding whether the commit is a
release at all. It asks the API for the pull request that produced the commit, and requires that the
head branch matches `release/publish/vX.Y.Z`, that the branch is in this repository, and that the
pull request carries the `release` label. A fork can name a branch anything; it cannot add a label.

Then, in order:

- the version in the tree is checked against the branch that is publishing it, and the changelog is
  checked for a section with that number
- one image is built and pushed to `ghcr.io/darbotlm/darbot`, tagged with the version, the commit
  and `latest`
- the services `docker-compose.yml` can build are published too, one image each, at
  `ghcr.io/darbotlm/darbot-<service>`. Those are `linux/amd64` and `linux/arm64`, built on native
  runners of each architecture and joined into one manifest list, because the machines pulling them
  are laptops as well as servers. `.github/published-images.json` is the list, and CI fails if it
  stops matching the Dockerfiles in the tree
- a build provenance attestation is signed with the workflow's OIDC identity and pushed alongside
  every one of them
- the commit is tagged and a GitHub Release is created, carrying the changelog section as its notes
  and `container-images.json` as an asset

## Deploying a release

`container-images.json` pins a digest for every image the release published. Deploy those, not tags:

```sh
gh release download v0.1.0 --pattern container-images.json
docker run -p 3001:3001 --env-file .env \
  "$(jq -r .images.darbot.reference container-images.json)"
```

The same file is how a machine runs the stack without building any of it. Each key under `images`
is a service, so the references can be read straight out of it:

```sh
export COMPUTER_IMAGE="$(jq -r '.images["agent-computer"].reference' container-images.json)"
export SUPERVISOR_IMAGE="$(jq -r .images.supervisor.reference container-images.json)"
export BOT_IMAGE="$(jq -r '.images["agent-bot"].reference' container-images.json)"
export LANGGRAPH_IMAGE="$(jq -r '.images["agent-langgraph"].reference' container-images.json)"
export SERVER_IMAGE="$(jq -r .images.server.reference container-images.json)"
export IMAGE_PULL_POLICY=missing
docker compose up -d
```

`IMAGE_PULL_POLICY=missing` is what turns those names into pulls; without it every one of those
services builds from source, because a service with a `build` section builds by default whatever
its image is called. A pull that fails still falls back to building, so a machine with no toolchain
wants the `build` sections overridden away rather than this variable alone.

A tag can be moved to point at a different image; a digest cannot. The same digest that CI smoke
tested is the one that runs, and rolling back is the same command with an earlier version.

Before deploying, you can check an image is the one this repository built. Every published image
carries its own attestation:

```sh
gh attestation verify oci://ghcr.io/darbotlm/darbot:v0.1.0 -R darbotlm/darbot
gh attestation verify oci://ghcr.io/darbotlm/darbot-supervisor:v0.1.0 -R darbotlm/darbot
```

## What has to be green

Branch protection should require one check, `verify`, which fails unless every other job succeeded.
A job added to `ci.yml` is covered by it without anybody updating a list.

| check | what it would catch |
| --- | --- |
| `format, lint, types` | the ordinary things, across every workspace including `agent-computer` and the supervisor |
| `tests` | a decision made wrongly, in isolation |
| `chart` | a Helm values file that renders a server which cannot start, across the EKS, GKE, AKS and self-hosted targets |
| `python harness regressions` | a provider-boundary regression in the Python Bot harnesses |
| `build` | the app not compiling |
| `migrations` | a schema change with no migration, or a snapshot that has drifted |
| `image` | an image that builds but does not boot, or a supervised service that respawns |
| `component dockerfiles` | a Dockerfile a release would publish that no longer builds, or one the publish list has stopped covering |

`image` matters more than its position suggests. Everything above it can pass on a tree whose image
never starts, because nothing else here runs the thing it ships. It builds the container, boots it
with embedded PostgreSQL, waits for `/api/capabilities`, and fails if a supervised service is
respawning.

These checks run again, against the release commit, when the release PR is merged. They gate the
publish rather than the proposal, which is why the release PR arriving without its own checks does
not matter: a pull request opened by a workflow does not trigger them.

**No secrets are required.** Every workflow here uses only the built-in `GITHUB_TOKEN`.

## The one thing CI cannot do

The smoke journey in `tests/smoke` is the only check that proves the parts are wired to each other:
the server reaches the supervisor, the supervisor builds a computer, the gateway decides before the
browser acts, and the trail records it. It cannot run in CI, and this is not a gap to be closed
later.

darbot only runs in Intelligence mode, and `loadConfig` refuses to start without the project's
Intelligence values, which a hosted runner has no business holding. The `image` check gets around
this with placeholder values, because nothing is contacted at start-up, but the journey asserts
`licenseStatus` is `valid`, and no placeholder can make that true.

So it is a step a person takes, on a machine with real Intelligence credentials, before merging
the release PR:

```sh
bash scripts/start.sh
export darbot_SMOKE_COOKIE='better-auth.session_token=...'   # from a signed-in browser
bun run test:smoke
```

The session is the second half of "a machine with real credentials": the routes the journey proves
are behind `requireUser`, so a run without one answers 401 three times and says nothing about the
release. See [development.md](development.md#quality-checks) for where the cookie comes from.

The release PR asks for the result in a comment. That is deliberately a person rather than a robot:
it is the one gate that cannot be automated, so it is the one gate worth naming.
