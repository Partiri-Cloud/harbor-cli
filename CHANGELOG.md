# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.1] — 2026-09-17

### Changed

- The `fk_`-prefixed fields in `.partiri.jsonc` are renamed to what they actually
  hold. `fk_workspace` → `workspace`, `fk_project` → `project`,
  `service.fk_region` → `service.region`, `service.fk_pod` → `service.pod`, and
  `service.fk_service_secret` → `service.service_secret`.

  The prefix was the API's own foreign-key column naming leaking into a file
  people write by hand; it described how the platform stores the value, not what
  the user is choosing. The API still receives the column names — only the
  manifest changed.

  Old manifests keep working. Both spellings are accepted on read, the editor
  raises no warning for the old ones, and the file is rewritten with the new
  names on the next `service pull` or any other command that saves it.

  `partiri validate` reports these checks under the new names too, so the
  `remote.fk_workspace` row is now `remote.workspace`.

### Removed

- `deploy_tag` is no longer part of `.partiri.jsonc`. It was never a setting: the
  platform assigns it on every deploy so it can tag images and scope observability,
  and the API rejected it on writes — the manifest only cached a copy of what the
  server had already decided. Caching it bought nothing, since `service logs` and
  `service metrics` were each fetching the service from the API on every run
  anyway, and it introduced a failure mode of its own: a stale or missing local
  value made `service logs` refuse to run until you remembered to `service pull`.

  Both commands now read the tag from that same API response. `service logs` on a
  service that has never been deployed says so, instead of blaming the config file.

  Existing manifests keep working — the key is ignored on read, no editor warning
  is raised for it, and it is dropped the next time the file is written.

- `service deploy` no longer writes to `.partiri.jsonc` after enqueuing the job.
  It had been doing a best-effort refresh to pick up the new tag; there is nothing
  left to refresh. `partiri llm next` now derives the deployed state purely from
  deploy-job history, so it can no longer report "deployed" from a stale local value.

## [0.5.0] — 2026-09-02

### Added

- `deploy_type: "cronjob"` — batch workloads, configured in `.partiri.jsonc` and
  driven by the same `service create` / `push` / `deploy` commands as everything
  else. There is deliberately no separate command family: a cronjob is a service.

  `scheduler` is the discriminator. Set it and the service renders as a
  recurring Kubernetes CronJob; leave it out and the identical config is a
  one-shot Job that runs once per deploy. The other ten cron fields
  (`cronjob_time_zone`, `cronjob_concurrency_policy`, `cronjob_backoff_limit`,
  `cronjob_ttl_seconds_after_finished`, `cronjob_starting_deadline_seconds`,
  `cronjob_successful_jobs_history_limit`, `cronjob_failed_jobs_history_limit`,
  `cronjob_suspend`, `cronjob_command`, and the required
  `cronjob_active_deadline_seconds`) are written to the config only for a
  cronjob — a block of inert keys in every other service's file would be noise.

  `partiri validate` mirrors the API's own rules so a bad schedule fails before
  a round-trip: five fields, runs at least five minutes apart, a deadline
  between 1 and 3600 seconds, a known concurrency policy, and a `run_command` or
  `registry_url` to actually execute. The schedule check expands the minute
  field and takes the tightest gap between fires; the wrap into the next hour
  counts only when the hour field lets two consecutive hours fire, so
  `0,58 0 * * *` — once a day at 00:00 and 00:58 — is read as the 58-minute gap
  it is rather than a 2-minute one. A field it cannot expand is passed through
  for the API's real cron parser to judge, so it never blocks a schedule the
  server would have accepted.

  `partiri init` offers `cronjob` in the service-type picker and asks only for
  the two fields the API requires, writing the rest as commented examples.

- Per-run cost estimates for a cronjob, which is metered per run on actual
  duration and never billed a flat month. `service create` prints the ceiling a
  single run can reach at the configured timeout and puts it in the JSON
  envelope as `max_cost_per_run_eur`, leaving `monthly_cost_eur` null.

- A `scheduled-cronjob` entry in `partiri llm examples`, cronjob notes in
  `partiri llm explain service create` and `explain validate`, and a
  cronjob-aware `partiri llm template --deploy-type cronjob`.

### Fixed

- `partiri service pull` now round-trips the cron configuration. The client
  never deserialized `scheduler` or the `cronjob_*` columns, so pulling a
  cronjob wrote a config declaring `deploy_type: "cronjob"` with no schedule and
  no deadline — which `partiri validate` then rejected, and which `service push`
  would have sent back as a job that never fires.

### Changed

- `partiri service push` no longer reports a monthly cost delta for a cronjob,
  which is metered per run and never billed a flat month. It quotes the per-run
  ceiling on each side instead, so changing only the deadline — an edit a
  monthly figure hides entirely — shows up as the cost change it is.

## [0.4.0] — 2026-08-06

### Added

- `partiri db` (aliased `partiri database`) — manage the platform's new managed
  PostgreSQL databases: `create`, `list`, `show`, `deploy`, `pause`, `unpause`,
  `jobs`, and `delete`. A database has no repository, build, or run command, so
  the family is entirely flag-driven and addressed by UUID; nothing about it is
  written to `.partiri.jsonc`.

  `db create` generates a strong password by default and prints it once —
  the API stores it write-only, never returns it, and offers no rotation, so
  `-j` puts it in the JSON envelope as `password` for scripts and agents. It is
  also printed when the create request itself fails, since a timeout or an
  unparseable response can arrive after the server already committed, and that
  password would otherwise be unrecoverable. Pass `--password-stdin` to supply
  your own; there is deliberately no `--password` flag, which would leak into
  shell history and the process list. Every rule the API enforces (identifier
  pattern and reserved names, PostgreSQL version, disk bounds, password length
  and character set) is checked locally first, so a bad value fails without a
  round-trip.

  `deploy`, `pause`, `unpause`, and `delete` confirm before acting, matching the
  `service` commands; `-y` skips the prompt. `db show` renders the connection
  string, which the CLI builds client-side
  because the API exposes no endpoint for it. There is no `db update` or
  `db kill` subcommand: the API makes every `db_*` field immutable after
  creation and rejects `kill` for databases.

- `partiri llm explain db …`, a `managed-postgresql-database` entry in
  `partiri llm examples`, and the `db_*` / `internal_sd_url` fields in
  `partiri llm context`, so agents driving the CLI can discover and connect to a
  database without extra calls.

### Changed

- `partiri service pull` now refuses a database UUID and hides databases from
  its interactive picker. Writing one out produced a `.partiri.jsonc` that every
  later command rejected, since `deploy_type: "database"` and `runtime: "psql"`
  are not valid config values.

- `partiri validate` explains what to do when a config declares
  `deploy_type: "database"` instead of only listing the allowed values.

## [0.3.2] — 2026-08-01

### Fixed

- The compute pod picker in `partiri init` and `partiri service link` listed pods
  in whatever order the API returned them, which has no relation to price, and
  pre-selected the first entry. Pressing Enter therefore accepted an arbitrary
  tier — in practice one of the most expensive ones — and the labels showed CPU
  and RAM but no price, so there was nothing on screen to suggest otherwise. Pods
  are now sorted cheapest-first with the cheapest pre-selected, and each row
  shows its monthly price. Pods with no price row for the region sort last and
  are labelled `price unavailable` rather than being rendered as free. When
  pricing cannot be fetched the picker keeps the previous order, omits prices,
  and warns instead of failing.

- `partiri service pull` refreshed `deploy_tag` and the disk block on an existing
  config but left `fk_pod` alone. A pod size changed in the dashboard was
  therefore invisible to the local config, and the next `partiri service push`
  silently reverted it — re-charging the old size. The pull now adopts the live
  pod and warns on stderr when it replaces a diverging local value.
