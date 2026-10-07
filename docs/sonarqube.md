# SonarQube integration

ide integrates [SonarQube Community Edition](https://www.sonarsource.com/products/sonarqube/)
and the SonarLint "SonarQube for IDE" language server for connected-mode static
analysis. One SonarQube server and one SonarLint language server are shared by
**all** IDE windows. A built-in setup assistant (`exthost/builtin/sonarqube/`,
our code, MIT) drives the whole flow from the command palette.

See also `docs/exthost-integration.md` for how the shared extension host works.

## Components

| component | what | license / origin |
| --- | --- | --- |
| SonarQube server | external Docker container `ide-sonarqube` (image `sonarqube:community`, pinned tag), bound to `127.0.0.1:9000` | LGPL-3.0, pulled from Docker Hub, external only |
| SonarLint language server | the `SonarSource.sonarlint-vscode` extension (darwin-arm64, bundles its own JRE + the SonarLint server); runs in the shared extension host, fetched at runtime from Open VSX, pinned in `exthost/test/extensions.lock.json` | LGPL-3.0, unmodified, external only |
| setup assistant | built-in extension `exthost/builtin/sonarqube/` | MIT, our code |

Because the extension runs in the one shared host, a **single** SonarLint Java
language server serves every window.

## Prerequisites

A Docker engine is required to run the server locally. Docker Desktop, Colima,
and OrbStack are all detected (via `docker info`). If no engine is found, the
assistant shows install guidance:

```sh
brew install colima docker && colima start
# or install Docker Desktop
```

"Connect to Existing Server" needs no Docker — only a reachable SonarQube URL
and a token.

## Commands

All commands are in the command palette under the category **SonarQube**.

### SonarQube: Set Up Local Server

End-to-end local setup:

1. Detects a Docker engine (`docker info`). If missing, shows install guidance
   and stops.
2. After an explicit modal confirmation, runs or reuses the singleton container
   `ide-sonarqube` (`image sonarqube:community`, `-p 127.0.0.1:9000:9000`,
   named volumes `sonarqube_data` / `sonarqube_extensions` / `sonarqube_logs`,
   `--restart unless-stopped`). A stopped container is started; a second
   container is never created.
3. Polls `GET /api/system/status` with a progress UI until the server is `UP`
   (SonarQube needs 1-3 min to boot; the assistant polls up to ~5 min).
4. On first run, detects the default `admin`/`admin` credentials and prompts
   (masked) for a new admin password, changing it via the API.
5. Generates a user token via `POST /api/user_tokens/generate` and stores it in
   SecretStorage (never in settings).
6. Writes the connection config and flips the status bar to **connected**.
7. Offers to **Bind Workspace**.

### SonarQube: Connect to Existing Server

Prompts for a server URL and a token (masked), validates them via
`/api/authentication/validate` and `/api/system/status`, stores the token in
SecretStorage, and writes the connection config. Use this to point at a server
you already run.

### SonarQube: Bind Workspace

For each repo among the open folders, derives a default project key from the git
remote / repo name, then lets you pick an existing project or create one
(`POST /api/projects/create`), and binds **all** folders of that repo. Worktrees
of the same repo share **one** binding / project key.

### SonarQube: Open Dashboard

Opens the server / project dashboard in the external browser.

### SonarQube: Stop Local Server

`docker stop ide-sonarqube` (after confirmation). The container and its volumes
are preserved for reuse.

### SonarQube: Show Setup Status

Summarizes Docker presence, container state, server status, connection, and
bindings. This is also what the status bar item runs on click.

## Status bar

A left-aligned status bar item reflects setup state and runs **Show Setup
Status** on click:

- `SonarQube: not configured`
- `SonarQube: starting…`
- `SonarQube: connected`
- `SonarQube: stopped`
- `SonarQube: error`

## Singleton semantics

- **One SonarQube server** — container `ide-sonarqube` on `127.0.0.1:9000` for
  all windows. Reused across restarts; the assistant never starts a second.
- **One SonarLint language server** — a single shared JVM in the extension host
  tree serves every window and worktree.
- Worktrees of a single repo share one binding and project key.

## Connected-mode config and secret storage

Where connected-mode state lives:

- **Connection** — the setting
  `sonarlint.connectedMode.connections.sonarqube` =
  `[{ connectionId: "ide-local", serverUrl, disableNotifications: true }]`,
  at application (global) scope. No token is ever written to settings.
- **Token** — stored via SecretStorage under the SonarLint extension's
  namespace, keyed by the exact server URL, so the SonarLint language server
  fetches it itself. The host exposes `ide.secretState.store` /
  `ide.secretState.delete` bridge commands for this. The assistant never writes
  the token to settings or logs.
- **Per-worktree binding** — folder-scoped
  `sonarlint.connectedMode.project` = `{ projectKey, connectionId }`, persisted
  by the host in `<dataDir>/User/sonarqube-bindings.json` (keyed by folder
  `fsPath`) and projected into folder-scoped configuration. It is applied to
  every window / worktree; worktrees of one repo share one project key.

## Memory

The external SonarQube container runs as a separate process and is **not** part
of ide's 2 GB memory budget. The SonarLint language server JVM *is* inside the
host tree and is counted — but it is a single shared JVM across all windows.

Measure it with:

```sh
cd exthost && node scripts/measure.mjs --sonarlint --workspaces 8 --settle 45 --limit-mb 100000
```

This prints a `SONARLINT_JVM processes=… rss=… MB` line; expect `processes=1`.

Measured on Apple Silicon (macOS, 8 windows, one shared SonarLint language
server): `SONARLINT_JVM processes=1  rss≈1321 MB` — a single shared JVM for all
eight windows (not one per window). The exact figure varies with the JVM's
default heap sizing; the invariant that matters is **processes=1**. For context
the whole host tree in that run was ≈2557 MB (node host ≈385 MB + the shared
SonarLint JVM ≈1321 MB + the per-window isolate heaps); the external SonarQube
server container is separate and excluded from this and from the 2 GB budget.

## Troubleshooting

- **Docker not found / daemon not running** — install Docker Desktop, or
  `brew install colima docker && colima start`.
- **Server slow to boot** — SonarQube needs 1-3 min to come up; the assistant
  polls `/api/system/status` for up to ~5 min.
- **Port 9000 already in use** — stop the conflicting process or the existing
  container before running Set Up Local Server.
- **Token rejected** — re-run **Connect to Existing Server**. The token lives in
  SecretStorage, not settings.
- **No diagnostics appear** — make sure the folder is bound (**Bind
  Workspace**), the connection shows **connected**, and the file's language is
  supported (JS/TS, Python, Java, etc.).
- **Reset** — run **Stop Local Server**, or
  `docker rm -f ide-sonarqube` and remove the `sonarqube_data` /
  `sonarqube_extensions` / `sonarqube_logs` volumes.

## License

SonarQube Community Edition (`sonarqube:community`) and the SonarLint extension
(`SonarSource.sonarlint-vscode`) are both LGPL-3.0. They are used **only** as
external, unmodified, runtime-downloaded dependencies — never vendored into the
repo, bundled into ide binaries, or patched. The container is pulled from Docker
Hub; the extension from Open VSX. This is recorded in `NOTICE` and enforced by
`exthost/scripts/license-audit.mjs`.
