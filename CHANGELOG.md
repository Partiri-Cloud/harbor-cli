# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
