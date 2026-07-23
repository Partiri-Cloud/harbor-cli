# partiri CLI

The official command-line interface for [Partiri Cloud](https://partiri.cloud). Deploy and manage services directly from your terminal.

Built in Rust.

---

## Installation

### Cargo (crates.io)

```bash
cargo install partiri-cli
```

### npm

Installs a prebuilt binary for your platform (Linux and macOS, x64 and arm64):

```bash
npm install -g @partiri/cli
```

### Build from source

```bash
cargo build --release
# Binary at: target/release/partiri
```

---

## Authentication

```bash
partiri auth login
```

Opens your browser, signs you in at partiri.cloud, and writes the resulting API key to `~/.config/partiri/key` (mode 0600). The CLI binds a one-shot listener on `127.0.0.1` to receive the callback, then shuts down.

### Non-interactive (CI / scripts / containers without a browser)

```bash
partiri auth set-apikey --key "$PARTIRI_KEY"
echo "$PARTIRI_KEY" | partiri auth set-apikey --key-stdin
```

**Environment variables:**

| Variable           | Description                                                   |
|--------------------|---------------------------------------------------------------|
| `PARTIRI_API_URL`  | Override the default API base URL                             |
| `PARTIRI_WEB_URL`  | Override the partiri.cloud URL the login flow opens (staging) |
| `PARTIRI_TIMEOUT`  | HTTP request timeout in seconds (default: 30)                 |

---

## Quick Start

```bash
# 1. Authenticate (browser flow; use `partiri auth set-apikey` for non-interactive setups)
partiri auth login

# 2. Initialize config in your project directory
partiri init

# 3. Register the service on Partiri Cloud
partiri service create

# 4. Deploy
partiri service deploy
```

### Adopt an existing service

If the service already exists on Partiri Cloud (created via the web frontend or by a teammate), skip `init` and `create` — `pull` is the only service subcommand that does **not** require a local `.partiri.jsonc` and will generate one for you:

```bash
# 1. Authenticate (browser flow; use `partiri auth set-apikey` for non-interactive setups)
partiri auth login

# 2. In the project directory, pull the existing service
partiri service pull
```

You will be prompted to pick the workspace, project, and service; the CLI writes a ready-to-use `.partiri.jsonc` in the current directory.

---

## Commands

### Global flags

These flags work on every command:

| Flag              | Description                                                                          |
|-------------------|--------------------------------------------------------------------------------------|
| `-j`, `--json`    | Emit machine-readable JSON to stdout (errors as JSON to stderr).                      |
| `-y`, `--yes`     | Skip confirmation prompts on destructive operations (`deploy`, `kill`, `pause`, …).   |
| `--no-input`      | Never prompt; error if a required value is missing. Auto-enabled when stdin is not a TTY. |
| `-c`, `--config <PATH>` | Use `PATH` as the config file instead of `./.partiri.jsonc`. If `PATH` is an existing directory, `.partiri.jsonc` inside it is used. |

### Top-level

| Command             | Description                                                                 |
|---------------------|-----------------------------------------------------------------------------|
| `partiri auth`      | Sign in (`auth login`) or paste a key (`auth set-apikey`)                   |
| `partiri init`      | Create `.partiri.jsonc` — interactive wizard by default; `--template` writes a non-interactive commented scaffold |
| `partiri validate`  | Validate the local `.partiri.jsonc`; `--remote` also runs live API checks (UUIDs exist, region/pod pairing, repo/registry reachability, health-check probe) |

### `partiri service <subcommand>`

| Subcommand                   | Description                                                        |
|------------------------------|--------------------------------------------------------------------|
| `partiri service create`     | Register the service on Partiri and write its ID to the config     |
| `partiri service pull`       | Pull an existing service from Partiri and save as `.partiri.jsonc` |
| `partiri service list`       | List services in a project (discovery; no config file needed)      |
| `partiri service push`       | Push local config changes to the existing service                  |
| `partiri service metrics`    | Show current service metrics and recent jobs                       |
| `partiri service logs`       | Show the last 35 log lines from the past hour                      |
| `partiri service jobs`       | List jobs for this service                                         |
| `partiri service deploy`     | Trigger a deploy job; `--service <UUID>` deploys by UUID and bypasses `.partiri.jsonc` entirely |
| `partiri service pause`      | Pause the service                                                  |
| `partiri service unpause`    | Resume a paused service                                            |
| `partiri service kill`       | Kill the service (requires confirmation)                           |
| `partiri service link`       | Fill in workspace, project, region and pod UUIDs interactively     |
| `partiri service env`        | Print, upload, or save runtime environment variables               |
| `partiri service token`      | Link an auth token to this service (private repos / registries)    |

### `partiri projects <subcommand>`

| Subcommand                | Description                          |
|---------------------------|--------------------------------------|
| `partiri projects list`   | List all projects in a workspace     |
| `partiri projects create` | Create a new project in a workspace  |

### `partiri secrets <subcommand>`

Workspace-scoped credentials for private repositories and container registries. The UUID returned on creation is what you pass to `partiri service token --secret <UUID>` (or set as `fk_service_secret` in `.partiri.jsonc`) to grant a service access to a private source.

| Subcommand                             | Description                                                              |
|----------------------------------------|--------------------------------------------------------------------------|
| `partiri secrets create-registry`       | Store image-pull credentials for a private container registry             |
| `partiri secrets create-repository`     | Store a Git access token for a private repository                         |
| `partiri secrets list`                  | List all secrets in a workspace                                           |

**`partiri secrets create-registry`** flags:

| Flag | Required | Description |
|------|----------|-------------|
| `--workspace <UUID>` | No | Workspace UUID (prompted if omitted) |
| `--name <NAME>` | Yes | Human-readable label for the secret |
| `--provider <PROVIDER>` | Yes | One of: `github`, `gitlab`, `bitbucket`, `docker`, `google`, `aws` |
| `--username <USER>` | Yes | Registry username |
| `--password <PASS>` | Either | Registry password or token (avoid — stays in shell history) |
| `--password-stdin` | Either | Read password from stdin (recommended) |

```bash
echo "$REGISTRY_TOKEN" | partiri secrets create-registry \
  --workspace <UUID> --name ghcr-prod \
  --provider github --username myorg \
  --password-stdin
```

**`partiri secrets create-repository`** flags:

| Flag | Required | Description |
|------|----------|-------------|
| `--workspace <UUID>` | No | Workspace UUID (prompted if omitted) |
| `--name <NAME>` | Yes | Human-readable label for the secret |
| `--provider <PROVIDER>` | Yes | One of: `github`, `gitlab`, `bitbucket`, `codeberg` |
| `--token <TOKEN>` | Either | Git access token (avoid — stays in shell history) |
| `--token-stdin` | Either | Read token from stdin (recommended) |
| `--username <USER>` | No | Username for Bitbucket basic auth only |

```bash
echo "$GH_TOKEN" | partiri secrets create-repository \
  --workspace <UUID> --name github-private \
  --provider github \
  --token-stdin
```

**`partiri secrets list`** flags:

| Flag | Required | Description |
|------|----------|-------------|
| `--workspace <UUID>` | No | Workspace UUID (prompted if omitted) |

### `partiri storage <subcommand>`

Provision and manage persistent volumes for a service. The `disk` block in `.partiri.jsonc` is declarative config only — `service create` and `service push` never touch storage. `partiri storage create` provisions the volume from that block, and `partiri storage update` applies changes to it. See the [disk field reference](#field-reference) for how to declare a volume in config.

| Subcommand                       | Description                                                   |
|----------------------------------|---------------------------------------------------------------|
| `partiri storage create`         | Provision the volume declared in the `service.disk` block and attach it |
| `partiri storage update`         | Apply the `service.disk` block to the existing volume (grow / remount)  |
| `partiri storage list`           | List all volumes in a project                                 |
| `partiri storage show <UUID>`    | Show full details for a specific volume                       |
| `partiri storage detach <UUID>`  | Detach a volume from its service (pause the service first)    |
| `partiri storage delete <UUID>`  | Delete a volume (must be detached first)                      |

**`partiri storage create`** and **`partiri storage update`** take no arguments — they read `service.disk` and the service `id` from the local `.partiri.jsonc`. `create` fails if the service already has a volume; `update` grows the size (prorated charge) and/or changes the mount path, and rejects a size decrease (a PVC cannot shrink).

**`partiri storage list`** flags:

| Flag | Required | Description |
|------|----------|-------------|
| `--project <UUID>` | No | Project UUID (prompted if omitted) |
| `--workspace <UUID>` | No | Workspace UUID — scopes the project picker |

**`partiri storage show`**, **`partiri storage detach`**, and **`partiri storage delete`** each take a single positional volume UUID:

```bash
partiri storage create                 # provision the volume from the local disk block
partiri storage update                 # apply disk-block changes to it (grow / remount)
partiri storage list --project <UUID>
partiri storage show <VOLUME-UUID>
partiri storage detach <VOLUME-UUID>   # service must be paused first
partiri storage delete <VOLUME-UUID>   # volume must be detached first
```

### Discovery

These commands list resources by UUID — useful for filling in a `.partiri.jsonc` by hand:

| Command                                          | Description                                  |
|--------------------------------------------------|----------------------------------------------|
| `partiri workspaces list`                        | List all workspaces accessible with your API key |
| `partiri projects list --workspace <UUID>`       | List all projects in a workspace             |
| `partiri service list --project <UUID>`          | List all services in a project               |
| `partiri regions list --workspace <UUID>`        | List the regions available in a workspace    |
| `partiri pods list --workspace <UUID>`           | List the compute pods available in a workspace; add `--region <UUID>` to include a monthly price column |

### `partiri llm <subcommand>`

Machine-readable helpers for AI agents driving the CLI (pair them with `-j` for JSON output).
The full agent guide is also shipped in [`LLM.md`](LLM.md).

| Subcommand                       | Description                                                      |
|----------------------------------|------------------------------------------------------------------|
| `partiri llm guide`              | Print the embedded agent guide (`LLM.md`)                        |
| `partiri llm schema`             | JSON schema of `.partiri.jsonc`                                  |
| `partiri llm template`           | Print a pre-filled `.partiri.jsonc` template (does not write it) |
| `partiri llm examples`           | Worked examples for common deployment shapes                     |
| `partiri llm capabilities`       | Emit the entire CLI command tree as JSON                         |
| `partiri llm errors`             | Catalog of every error code the CLI emits                        |
| `partiri llm explain <command>`  | Deep help for one command (args, subcommands, pitfalls)          |
| `partiri llm whoami`             | Auth state and identity                                          |
| `partiri llm doctor`             | Environment diagnostic (credentials, API reachability, config)   |
| `partiri llm context`            | Full nested workspace tree (projects/regions/pods/secrets/services) in one call |
| `partiri llm next`               | Suggest the next command for the current `.partiri.jsonc` state  |

### `partiri mcp <subcommand>`

Install or remove the Partiri MCP server in AI tools. Valid `--client` slugs: `claude-desktop`, `claude-code`, `cursor`, `vscode`, `copilot-cli`, `windsurf`. Omit `--client` to pick interactively.

| Subcommand                               | Description                                      |
|------------------------------------------|--------------------------------------------------|
| `partiri mcp install [--client <slug>]`  | Install the Partiri MCP server into an AI tool   |
| `partiri mcp uninstall [--client <slug>]`| Remove the Partiri MCP server from an AI tool    |

---

## Config File — `.partiri.jsonc`

`partiri init` creates a `.partiri.jsonc` in your project root. The file uses JSON5 syntax (supports `//` comments and trailing commas).

```jsonc
{
  // Assigned by Partiri after running 'partiri service create'. Leave null until then.
  "id": null,

  "fk_workspace": "<uuid>",
  "fk_project": "<uuid>",

  "service": {
    "name": "my-service",

    // "webservice" | "static" | "private-service" | "worker"
    "deploy_type": "webservice",

    // "node" | "deno" | "rust" | "python" | "go" | "ruby" | "elixir" | "php" | "jvm" | "dotnet" | "cpp" | "static" | "registry"
    "runtime": "node",

    "root_path": ".",

    // Git repository source
    "repository_url": "https://github.com/org/repo",
    "repository_branch": "main",

    // Or registry image source (not compatible with deploy_type "static")
    // "registry_url": "ghcr.io/owner/image:tag",  // full image reference; API splits host, repo, tag

    // Authentication token for private repos/registries — set via 'partiri service token'
    // "fk_service_secret": "<uuid>",

    "build_command": "npm run build",
    // "build_path": "dist",
    // "pre_deploy_command": "npm run migrate",
    "run_command": "npm start",

    "fk_region": "<region-uuid>",
    "fk_pod": "<pod-uuid>",

    "health_check_path": "/health",

    "maintenance_mode": false,
    "active": true,

    // Persistent disk (optional). Declarative only — provision it with
    // 'partiri storage create' and change it with 'partiri storage update'.
    // 'service create'/'service push' never touch storage.
    // "disk": { "mount_path": "/app/data", "size": 1 }

    // Environment variables are managed via 'partiri service env --path <.env>'.
    // They are never stored in this file.
  }
}
```

### Field reference

| Field                             | Required  | Description                                                                 |
|------------------------------------|-----------|-----------------------------------------------------------------------------|
| `id`                               | Auto      | Service UUID. Set by `service create`; leave `null` initially.              |
| `deploy_tag`                       | Auto      | Most recent deploy tag. Set by the deploy job; refresh with `service pull`. Needed for `logs` / `metrics`. |
| `fk_workspace`                     | Yes       | UUID of the target workspace.                                               |
| `fk_project`                       | Yes       | UUID of the target project. Must belong to `fk_workspace`.                  |
| `service.name`                     | Yes       | Service name (≤16 chars), unique within the project.                        |
| `service.deploy_type`              | Yes       | `webservice`, `static`, `private-service`, or `worker`.                     |
| `service.runtime`                  | Yes       | `node`, `deno`, `rust`, `python`, `go`, `ruby`, `elixir`, `php`, `jvm`, `dotnet`, `cpp`, `static`, or `registry`. |
| `service.root_path`                | Yes       | Path to the app root within the repository.                                 |
| `service.repository_url`           | *Either*  | Git repository URL. Mutually exclusive with `registry_url`.                 |
| `service.repository_branch`        | Cond.     | Branch to deploy. Required when `repository_url` is set.                    |
| `service.registry_url`             | *Either*  | Full container image reference (e.g. `ghcr.io/owner/image:tag`). The API splits host, repository, and tag server-side. Mutually exclusive with `repository_url`; not supported for `static`. |
| `service.fk_service_secret`        | Cond.     | Secret UUID for private repo/registry access. Set via `partiri service token`. |
| `service.build_command`            | Cond.     | Build command. Required for repository sources on non-static runtimes.      |
| `service.build_path`               | No        | Build output directory (e.g. `dist`).                                       |
| `service.pre_deploy_command`       | No        | Command run before each deploy (e.g. DB migrations).                        |
| `service.run_command`              | Cond.     | Start command. Required for `webservice`, `private-service`, and source-built `worker`. |
| `service.fk_region`                | Yes       | Region UUID. List via `partiri regions list --workspace <UUID>`.            |
| `service.fk_pod`                   | Yes       | Compute pod UUID (CPU/RAM tier). List via `partiri pods list --workspace <UUID>`. |
| `service.health_check_path`        | No        | Health-check path or absolute URL. `null` disables the check.               |
| `service.disk`                     | No        | Persistent volume: `{ "mount_path": "/app/data", "size": <1–10 GB> }`. Declarative config only — provision the volume with `partiri storage create` and change it with `partiri storage update`. `service create`/`push` never touch storage; `service pull` adopts the live volume's mount path and size into this block when a volume exists (warning if it replaces a diverging local edit), and preserves a block declared for a not-yet-run `storage create`. |
| `service.maintenance_mode`         | No        | Serve a maintenance page instead of the app.                                |
| `service.active`                   | No        | Whether the service is active.                                              |

> **Env vars** are managed exclusively via `partiri service env` and are never written to `.partiri.jsonc`.
> - `partiri service env` — print env vars currently on the service.
> - `partiri service env --save` — write the service's env vars to `.env.partiri` in the current directory (gitignore it).
> - `partiri service env --path <file>` — replace the service's env vars from a dotenv file (full replace).

---

## Project Detection

`partiri init` auto-detects your project type and pre-fills defaults:

| Detected file                                 | Runtime   | Default build command                          |
|------------------------------------------------|-----------|------------------------------------------------|
| `package.json`                                 | `node`    | `npm run build` / `npm start`                  |
| `Cargo.toml`                                   | `rust`    | `cargo build --release`                        |
| `requirements.txt` / `pyproject.toml`          | `python`  | `pip install -r requirements.txt`              |
| `go.mod`                                       | `go`      | `go build -o app .`                            |
| `Gemfile`                                      | `ruby`    | `bundle install`                               |
| `mix.exs`                                      | `elixir`  | `mix deps.get && mix compile`                  |
| `composer.json`                                | `php`     | `composer install`                             |
| `pom.xml`                                      | `jvm`     | `mvn package -DskipTests`                      |
| `build.gradle` / `build.gradle.kts`            | `jvm`     | `./gradlew build`                              |
| `*.csproj` / `*.fsproj` / `*.sln`              | `dotnet`  | `dotnet publish -c Release`                    |
| `CMakeLists.txt`                               | `cpp`     | `cmake -B build -DCMAKE_BUILD_TYPE=Release && cmake --build build` |

It also reads the git `origin` remote to pre-fill `repository_url`.

---

## Typical Workflow

```
your-project/
├── .partiri.jsonc   ← managed by the CLI
├── src/
└── ...
```

```bash
# First time
partiri auth login           # browser sign-in (or `auth set-apikey --key <KEY>` for CI)
partiri init                 # interactive wizard → .partiri.jsonc
partiri service create       # register on Partiri → writes id to config
partiri service deploy       # trigger first deploy

# Ongoing
partiri service push         # push local config changes (env vars, commands, etc.)
partiri service deploy       # redeploy
partiri service metrics      # check live state
partiri service jobs         # review deploy history

# Maintenance
partiri service pause        # pause the service
partiri service unpause      # resume
partiri service kill         # permanently stop (with confirmation prompt)
```

---

## Development

```bash
# Run from source
cargo run -- <command>

# Example
cargo run -- service metrics
```

Requires Rust 1.77+ (edition 2021). No system OpenSSL dependency — TLS is handled by `rustls`.
