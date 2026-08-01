# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
