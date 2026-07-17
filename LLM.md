# partiri CLI — agent guide

This document is shipped inside the `partiri` binary and printed by `partiri llm guide`.
It tells an LLM agent (Claude Code, Codex, custom MCP client, etc.) everything needed to drive
the CLI without reading source.

## 1. Quickstart

```sh
partiri auth set-apikey --key "$PARTIRI_KEY"   # one-time setup (non-interactive)
partiri -j llm doctor                       # confirm environment is sane
partiri -j llm context | jq '.data'         # one call → workspaces, projects, regions, pods, secrets, services
# ... pick UUIDs from the tree, write/edit .partiri.jsonc ...
partiri -j validate --remote                # gate before create
partiri -j -y service create
partiri -j -y service deploy
```

- `-j` switches to JSON output.
- `-y` skips confirmation prompts on destructive operations (`deploy`, `kill`, `pause`, `unpause`).
- `--config <PATH>` (`-c`) points at an alternate config file instead of `./.partiri.jsonc` — pass PATH as a directory to use `.partiri.jsonc` inside it (e.g. `--config staging/`).
- Prompts are auto-skipped when stdin is not a TTY (every Bash-tool invocation, every CI run).
- Humans can run `partiri auth login` for a browser flow that issues a key automatically. Agents must use `auth set-apikey` — `auth login` requires a TTY and a desktop browser, and refuses to run with `--json` or `--no-input`.

## 2. I/O contract

- **stdout**: exactly one structured result per invocation, terminated by `\n`.
- **stderr**: spinners, progress, prompts, warnings — and the error JSON document on failure.
- Every JSON document carries `"schema_version": "1"`.

Envelopes:

```jsonc
// list
{ "schema_version": "1", "data": [ { ... } ] }
// single resource
{ "schema_version": "1", "data": { ... } }
// successful mutation
{ "schema_version": "1", "ok": true, "message": "…", "data": { … } }
// error (stderr, exit 1)
{
  "schema_version": "1",
  "ok": false,
  "error": {
    "code": "401",                            // HTTP status as string OR literal: validation/auth/network/config/cancelled/conflict/missing_dependency
    "message": "Unauthorized",
    "hint": "Your API key may have expired or been revoked. Run 'partiri auth login' to sign in again.",
    "likely_causes": ["…"],
    "suggested_commands": ["partiri auth set-apikey --key <K>", "partiri llm doctor"]
  }
}
```

Exit codes: `0` success, `1` any error, `2` user cancellation (Ctrl-C / inquire abort).

When something goes wrong, **read `error.suggested_commands` first** — that's the next thing to run.

## 3. The `.partiri.jsonc` schema

Read the schema straight from the binary instead of trusting a copy here:

- `partiri llm schema --json` — the full JSON Schema (generated from the CLI's
  own config types, so it never drifts): every field, its type, whether it's
  required, the allowed enum values, and the cross-field rules.
- `partiri llm template [--runtime <r>] [--source repo|registry]` — a filled,
  commented `.partiri.jsonc` ready to write and edit.

Invariants worth knowing up front:

- `service.name` ≤ 16 characters.
- `repository_url` XOR `registry_url` — exactly one.
- Private repo/registry sources require `fk_service_secret` (see §5).
- `disk` is optional and pins the service to a single region (see §5).
- Env vars are never stored in this file — manage them with `partiri service env`.

## 4. Discovery commands

| Need | Single-call shortcut | Per-resource |
|---|---|---|
| Everything | `partiri -j llm context` | — |
| Workspaces | `partiri -j workspaces list` | — |
| Projects | — | `partiri -j projects list --workspace <UUID>` |
| Regions | — | `partiri -j regions list --workspace <UUID>` |
| Pods (with pricing) | — | `partiri -j pods list --workspace <UUID> --region <UUID>` |
| Secrets | — | `partiri -j secrets list --workspace <UUID>` |
| Services | — | `partiri -j service list --project <UUID>` |
| Storage volumes | — | `partiri -j storage list --project <UUID>` |

`partiri llm context` is the single most useful command for any multi-resource decision — it
fans out the per-workspace requests in parallel and returns a fully nested tree.

The context response includes `price_eur_month` per pod (priced against the first region),
`volume_price_per_gb` for the workspace, and `balance_eur` — enough to estimate cost before
creating a service.

## 5. Workflow recipes

### Create a new service from scratch

```sh
partiri auth set-apikey --key "$KEY"
partiri init --template                 # writes .partiri.jsonc with commented examples
# (edit .partiri.jsonc — fill in fk_workspace / fk_project / fk_region / fk_pod and the service.* block)
partiri -j llm context | jq '.data'     # to find UUIDs
partiri -j validate --remote
partiri -j -y service create
partiri -j -y service deploy
```

### Adopt an existing service into a fresh checkout

```sh
partiri auth set-apikey --key "$KEY"
partiri service pull                    # interactive; or write .partiri.jsonc by hand with id set
partiri -j -y service deploy
```

### Deploy across many directories (the "8 services" scenario)

```sh
partiri -j llm context | jq '.data' > /tmp/ctx.json
for dir in examples/*; do
  ( cd "$dir" \
    && partiri init --template \
    && jq -r '...' /tmp/ctx.json | apply_to .partiri.jsonc \
    && partiri -j validate --remote \
    && partiri -j -y service create \
    && partiri -j -y service deploy )
done
```

### Move a service to a different region/pod

```sh
partiri -j service link --region <UUID> --pod <UUID>
partiri -j -y service push
partiri -j -y service deploy
```

### Create and wire up a credential for a private repo

```sh
# Step 1: create the repository secret in the workspace
# --provider: github | gitlab | bitbucket | codeberg
# --username is only required for Bitbucket basic auth (maps to data.key); omit for other providers
partiri -j secrets create-repository \
  --workspace <UUID> \
  --name "github-token" \
  --provider github \
  --token-stdin <<< "$GITHUB_TOKEN"
# → prints { "id": "<SECRET_UUID>", "kind": "repository" }

# Bitbucket example (requires --username):
# partiri -j secrets create-repository \
#   --workspace <UUID> \
#   --name "bitbucket-cred" \
#   --provider bitbucket \
#   --username "$BB_USER" \
#   --token-stdin <<< "$BB_APP_PASSWORD"

# Step 2: link it to the service config
partiri -j service token --secret <SECRET_UUID>

# Step 3: push the new fk_service_secret to Partiri
partiri -j service push

# Step 4: confirm reachability
partiri -j validate --remote      # remote.repository_url should now pass
```

### Create and wire up a credential for a private registry

```sh
# Step 1: create the registry secret in the workspace
# --provider: github | gitlab | bitbucket | docker | google | aws
partiri -j secrets create-registry \
  --workspace <UUID> \
  --name "ghcr-token" \
  --provider github \
  --username "$GHCR_USER" \
  --password-stdin <<< "$GHCR_TOKEN"
# → prints { "id": "<SECRET_UUID>", "kind": "registry" }

# Step 2: link it to the service config
partiri -j service token --secret <SECRET_UUID>

# Step 3: push the new fk_service_secret to Partiri
partiri -j service push

# Step 4: confirm reachability
partiri -j validate --remote      # remote.registry_url should now pass
```

### List and delete secrets

```sh
partiri -j secrets list --workspace <UUID>
# → { "registry": [...], "repository": [...] }
```

### Attach persistent storage to a service

Add a `disk` block to `.partiri.jsonc` before calling `service create`, or add it
later and reconcile with `service push`:

```jsonc
// .partiri.jsonc
"service": {
  // ...
  "disk": {
    "mount_path": "/app/data",
    "size": 5              // GB, 1–10
  }
}
```

```sh
partiri -j -y service create    # creates the service + volume; auto-attaches on provision
# — or, for an existing service —
partiri -j -y service push      # reconciles: creates, resizes, or removes the volume
```

**Disk reconcile outcomes on `service push`:**

| `.partiri.jsonc` `disk` | Live volume | Result |
|---|---|---|
| Same `mount_path` + `size` | exists | no-op |
| Added | none | creates and attaches the volume |
| `mount_path` or `size` changed | exists | **confirms then: detach → delete → recreate** (data lost) |
| Removed / set to `null` | exists | detaches only — data preserved; prints a hint to `partiri storage delete <id>` |
| Absent | none | no-op |

A disk change (resize or remount) **destroys all data** and **prompts for confirmation**. In
CI or non-TTY environments pass `-y` to auto-confirm, or the command errors out.

Removing the `disk` block detaches the volume but does **not** delete it. The volume
remains billable until you explicitly run:

```sh
partiri storage delete <VOLUME_UUID>
```

**Storage constraints**: a service with a disk is pinned to the region where the volume was
created. Multi-region deployment is not possible while a disk is attached (enforced by the API).

### Manage storage volumes directly

```sh
partiri -j storage list --project <UUID>    # list all volumes in a project
partiri -j storage show <VOLUME_UUID>       # show details for one volume
partiri -j -y storage detach <VOLUME_UUID>  # detach (service must be paused, no active jobs)
partiri -j -y storage delete <VOLUME_UUID>  # delete (must be detached first)
```

`storage detach` requires the service to be paused and have no active job. The CLI surfaces the
API's 400 with a clear hint rather than checking job state itself.

### Set runtime environment variables

Env vars are never stored in `.partiri.jsonc`. Manage them with `partiri service env`:

```sh
partiri -j service env                       # print current env vars on the service
partiri -j service env --save                # write current env vars to .env.partiri
partiri -j service env --path .env           # replace env vars from a dotenv file
```

`--path` replaces the service's env in full (no merge). `--save` is the inverse —
fetches the service's env and writes it to `.env.partiri` in the current
directory (overwrites). Format: standard `KEY=value` per line, `#` comments and
blank lines skipped, surrounding `"..."`/`'...'` quotes stripped on read; values
containing special characters are quoted on write. Multi-line values are
rejected on parse. `.env.partiri` contains secrets — gitignore it.

The read-modify-write pattern for adding a variable without discarding others:

```sh
partiri service env --save                   # writes .env.partiri
echo "NEW_VAR=value" >> .env.partiri
partiri service env --path .env.partiri      # full-replace upload
```

## 6. Cost and balance

### Where costs appear

| Command | What is shown |
|---|---|
| `partiri service create` | Estimated monthly cost (pod + disk) for the new service |
| `partiri service push` | Signed monthly cost-delta: desired cost − current cost (e.g. `+€5.0000` or `-€2.0000`) |
| `partiri pods list --region <UUID>` | Monthly price column (`€/month`) per pod |
| `partiri llm context` | `price_eur_month` per pod and `volume_price_per_gb` for the workspace; `balance_eur` for each workspace |

All amounts are in EUR. Cost estimates are non-fatal: if pricing is unavailable for a region the
field is `null` and the command still succeeds.

### Balance preflight

`partiri validate --remote` checks the workspace balance and emits a **warn** row
(`remote.balance`) if the balance is ≤ €0.00. This is **warn-only — it never blocks** the
validate command and does not prevent `service create` or `service push` from running. The
hard backstop is the API's 402 response (see §8). Top up at
`https://partiri.cloud/settings/billing`.

## 7. API key permissions

The API key used by the CLI must hold the following permissions for each operation:

| Operation | Required permission |
|---|---|
| `secrets list` | `workspace:r` |
| `secrets create-repository` | `repository_secret:rw` |
| `secrets create-registry` | `registry_secret:rw` |
| Balance read (`validate --remote`, `llm context`) | `billing:r` |
| Service create / push / deploy | Admin or Management workspace role |

A key with the **User** role can only trigger deploys on existing services — it cannot
create or update service configuration. Use an Admin or Management key for the full
`service create` / `service push` workflow.

## 8. Error codes

The full catalog — every code with its meaning, likely causes, and
`suggested_commands` — is emitted by `partiri llm errors --json`, and it's the
same data the CLI returns in the `error` envelope on failure. So you rarely need
it ahead of time: when a command fails, **read `error.suggested_commands` first**
(see §2).

Codes are either an HTTP status rendered as a string (`"400"`, `"401"`, `"402"`,
`"403"`, `"404"`, `"409"`, `"422"`, `"429"`, `5xx`) or one of the CLI-literal
codes: `auth`, `validation`, `network`, `config`, `cancelled`,
`missing_dependency`.

## 9. Common pitfalls

- **Your service MUST listen on the port given by the `PORT` environment variable.** Partiri injects `PORT` at runtime. If your process binds to a hardcoded port instead, the health check will fail and the deploy will be marked unhealthy. Use `process.env.PORT`, `std::env::var("PORT")`, or the equivalent in your runtime.
- **`service.name` must be ≤16 characters.** Validated locally; the API also rejects longer names.
- **`fk_region` and `fk_pod` must come from the same workspace.** Cross-workspace UUIDs return 404.
- **`repository_url` XOR `registry_url`.** Setting both errors out at `validate`.
- **Private repositories and registries require `fk_service_secret`.** Without it, `validate --remote` fails on the source-reachability check. Create the secret with `partiri secrets create-repository` or `partiri secrets create-registry`, then link it with `partiri service token --secret <UUID>` and push.
- **`health_check_path` accepts either a path or an absolute URL.** Only absolute URLs are probed by `validate --remote`; relative paths are deferred to runtime.
- **`deploy_tag` is set by the deploy job once it succeeds — not synchronously by `service deploy`.** The deploy is async, so the tag may still be empty right after the POST returns. `service deploy` does a best-effort refresh; if the job is still in progress, run `partiri llm next` (which inspects job status) or `partiri service pull` to refresh later. Required for `partiri service logs` and metrics.
- **`init --template` refuses to overwrite an existing `.partiri.jsonc`.** Delete the file manually first (or pull the existing service).
- **Changing `disk.mount_path` or `disk.size` destroys all volume data.** The `service push` disk-changed path runs detach → delete → recreate. Always prompt for confirmation; pass `-y` in CI.
- **Removing the `disk` block only detaches — the volume still exists and accrues cost.** Run `partiri storage delete <VOLUME_UUID>` to remove it entirely.
- **A service with a disk cannot be multi-region.** The API enforces single-region placement when a persistent volume is attached.
- **`storage detach` requires the service to be paused.** Pause first with `partiri service pause`, then detach, then optionally delete.
- **Balance warnings from `validate --remote` are non-blocking.** The `remote.balance` row is a warn, not a fail. The API itself returns 402 when the balance is insufficient — that is the hard backstop.

## 10. Glossary

- **Workspace** — billing/ownership boundary. A user belongs to one or more workspaces.
- **Project** — a logical grouping of services within a workspace, with an environment label (`dev`/`staging`/`prod`).
- **Service** — the deployable unit. One service per `.partiri.jsonc` per directory.
- **Region** — geographic location. Pods live in regions.
- **Pod** — a sized compute slot (CPU + RAM + replicas). Pick a pod that matches your service's needs.
- **deploy_tag** — the immutable tag of the most recent successful deploy. Used to fetch logs/metrics for that exact build.
- **fk_*** — foreign-key fields in `.partiri.jsonc` pointing at other resources by UUID.
